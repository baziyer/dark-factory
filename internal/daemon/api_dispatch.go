package daemon

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"sync"
	"sync/atomic"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
)

// Daemon is the concrete composition root for the local API. It owns the
// durable Store and live attempt owners. It does not own an accept loop; the
// caller accepts and hands one connection to HandleConnection.
type Daemon struct {
	store *kernel.Store
	now   func() time.Time

	// settleRetained is a package-test-only seam for an inspection that races a
	// durable update. Production settlement always uses retainedSettlement.
	settleRetained func(context.Context, string, kernel.Change) (kernel.ChangeSettlement, error)
	// scheduledRun is a package-test-only seam for the scheduler's terminal
	// completion reread. Production reads from the concrete Store.
	scheduledRun func(context.Context, kernel.RunID) (kernel.Run, bool, error)

	browserMu          sync.Mutex
	browserLifecycleMu sync.Mutex
	browsers           map[*BrowserRuntime]struct{}
	browserClosing     bool
	// relay is the optional outbound relay connector. It is a client of the
	// browser listener above, not a second authority, so it shares that
	// listener's lifecycle gate.
	relay              *RelayRuntime
	browserClientGates *browserClientGates

	// topologies holds the last regenerable topology per project for a short
	// window. It is a cost guard, not state: losing it only costs one walk.
	topologyMu sync.Mutex
	topologies map[kernel.ProjectID]topologySnapshot

	// providerDefaultCache holds the last read of each provider account's own
	// configured model for a short window, on the same terms as topologies:
	// a cost guard over a file read, never state.
	providerDefaultMu    sync.Mutex
	providerDefaultCache map[providerAccount]providerDefault

	// operationMu is the single linearization gate for live-attempt operations
	// that combine durable state with an owner-side action. It is deliberately
	// concrete and global: the local operator has no throughput requirement,
	// and one gate avoids a second digest-to-owner authority index.
	operationMu sync.Mutex

	// changeParent is the changes root the supervisor was given, published
	// without a lock so RunNext never waits on a console read. accountHome is
	// the account every run launches under, published the same way and for the
	// same reason. runPathsMu guards only the map of bounded directory walks,
	// never a walk itself.
	changeParent atomic.Pointer[string]
	accountHome  atomic.Pointer[string]
	runPathsMu   sync.Mutex
	runPaths     map[kernel.RunID]runPathsResult

	attemptMu sync.Mutex
	attempts  map[kernel.RunID]*liveAttempt
	closing   bool
	closeDone chan struct{}
	closeErr  error

	// schedulerWake is a bounded hint channel. SQLite remains the only
	// admission and capacity authority; losing or coalescing a hint is safe
	// because the scheduler also polls. schedulerRunning prevents two process
	// owners from creating competing unobserved admission probes.
	schedulerMu      sync.Mutex
	schedulerWake    chan struct{}
	schedulerRunning bool
	// supervisors contains every synchronous RunNext owner, including the
	// phase before a live attempt can register. Registration and closing share
	// attemptMu, so Close can cancel a real owner rather than passively waiting
	// for a caller that may still be blocked in pre-release setup.
	supervisors map[*supervisorRegistration]struct{}
}

type supervisorRegistration struct {
	ctx    context.Context
	cancel context.CancelFunc
	done   chan struct{}

	mu     sync.Mutex
	result error
}

// NewDaemon creates an API composition root using the wall clock for durable
// timestamps. The Store is never replaced or wrapped by the daemon.
func NewDaemon(store *kernel.Store) (*Daemon, error) {
	return newDaemon(store, time.Now)
}

func newDaemon(store *kernel.Store, now func() time.Time) (*Daemon, error) {
	if store == nil || now == nil {
		return nil, fmt.Errorf("%w: invalid daemon", kernel.ErrInvalidValue)
	}
	return &Daemon{store: store, now: now, browsers: make(map[*BrowserRuntime]struct{}), browserClientGates: &browserClientGates{}, attempts: make(map[kernel.RunID]*liveAttempt), supervisors: make(map[*supervisorRegistration]struct{}), schedulerWake: make(chan struct{}, 1)}, nil
}

// HandleConnection synchronously consumes exactly one authenticated request,
// dispatches it, writes exactly one response, and closes the connection. The
// API transport has already authenticated the domain and credential before a
// Call is returned; this method does not add an alternate auth path.
func (daemon *Daemon) HandleConnection(ctx context.Context, connection *api.Connection) error {
	if daemon == nil || daemon.store == nil || connection == nil {
		return fmt.Errorf("%w: invalid daemon connection", kernel.ErrInvalidValue)
	}
	defer connection.Close()
	call, err := connection.Receive(ctx)
	if err != nil {
		return err
	}
	if attemptOutcomeCall(call.Kind()) {
		var attempt *liveAttempt
		reply, dispatchErr := connection.Dispatch(func(call api.Call) api.Reply {
			var outcome api.Reply
			outcome, attempt = daemon.proposeOutcome(ctx, call)
			return outcome
		})
		if dispatchErr != nil {
			daemon.clearOutcomeReceipt(attempt)
			return dispatchErr
		}
		responseErr := connection.Respond(reply)
		if responseErr == nil {
			responseErr = connection.AwaitOutcomeReceipt(ctx)
		}
		daemon.clearOutcomeReceipt(attempt)
		return responseErr
	}
	reply, err := connection.Dispatch(func(call api.Call) api.Reply { return daemon.dispatch(ctx, call) })
	if err != nil {
		return err
	}
	return connection.Respond(reply)
}

// dispatch is intentionally one closed switch. API validation belongs to the
// transport and domain validation belongs to kernel constructors/Store
// methods; there is no forwarding service layer here.
func (daemon *Daemon) dispatch(ctx context.Context, call api.Call) api.Reply {
	switch call.Kind() {
	case api.CallHealth:
		return daemon.health(ctx)
	case api.CallSnapshot:
		return daemon.snapshot(ctx)
	case api.CallCreateProject:
		return daemon.createProject(ctx, call)
	case api.CallCreateAgent:
		return daemon.createAgent(ctx, call)
	case api.CallEnqueueTask:
		return daemon.enqueueTask(ctx, call)
	case api.CallSetDispatch:
		return daemon.setDispatch(ctx, call)
	case api.CallAttemptTask:
		return daemon.attemptTask(ctx, call)
	case api.CallRequestHuman:
		return daemon.requestHuman(ctx, call)
	case api.CallPeerStatus:
		return daemon.peerStatus(ctx, call)
	case api.CallPeerAsk:
		return daemon.peerAsk(ctx, call)
	case api.CallPeerAnswer:
		return daemon.peerAnswer(ctx, call)
	case api.CallSendBack, api.CallSendBackTask:
		return daemon.sendBack(ctx, call)
	case api.CallOverseerSnapshot:
		return daemon.overseerSnapshot(ctx, call)
	case api.CallOverseerEnqueueTask:
		return daemon.overseerEnqueueTask(ctx, call)
	case api.CallOverseerUpdateTask:
		return daemon.overseerUpdateTask(ctx, call)
	case api.CallOverseerUpdateAgent:
		return daemon.overseerUpdateAgent(ctx, call)
	case api.CallOverseerStopRun:
		return daemon.overseerStopRun(ctx, call)
	case api.CallOverseerReplaceRun:
		return daemon.overseerReplaceRun(ctx, call)
	case api.CallOverseerMessageWorker:
		return daemon.overseerMessageWorker(ctx, call)
	case api.CallOverseerInterruptWorker:
		return daemon.overseerInterruptWorker(ctx, call)
	case api.CallOverseerReplyHuman:
		return daemon.overseerReplyHuman(ctx, call)
	case api.CallWebStatus:
		status, err := daemon.WebStatus(ctx)
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
		reply, err := api.NewWebStatusReply(status)
		if err != nil {
			return newErrorReply(api.RemoteInternal)
		}
		return reply
	case api.CallWebListClients:
		after, ok := call.WebListAfter()
		if !ok {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		page, err := daemon.WebListClients(ctx, after)
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
		reply, err := api.NewWebClientsReply(page)
		if err != nil {
			return newErrorReply(api.RemoteInternal)
		}
		return reply
	case api.CallRemoteStatus:
		status, err := daemon.RemoteStatus()
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
		reply, err := api.NewRemoteStatusReply(status)
		if err != nil {
			return newErrorReply(api.RemoteInternal)
		}
		return reply
	case api.CallWebRevokeClient:
		input, ok := call.WebClientRevocationInput()
		if !ok {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		result, err := daemon.WebRevokeClient(ctx, input)
		if err != nil {
			if errors.Is(err, ErrBrowserClientCleanup) {
				return newErrorReply(api.RemoteCleanupUnresolved)
			}
			return newErrorReply(remoteErrorCode(err))
		}
		reply, err := api.NewWebRevokeReply(result)
		if err != nil {
			return newErrorReply(api.RemoteInternal)
		}
		return reply
	default:
		return newErrorReply(api.RemoteInvalidRequest)
	}
}

func (daemon *Daemon) attemptTask(ctx context.Context, call api.Call) api.Reply {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	authority, err := daemon.store.AuthenticateAttempt(ctx, kDigest)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	reply, err := api.NewAttemptTaskReply(api.AttemptTask{Task: authority.Task()})
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) peerStatus(ctx context.Context, call api.Call) api.Reply {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	authority, err := daemon.store.AuthenticateAttempt(ctx, kDigest)
	if err != nil || authority.Role != kernel.RoleWorker {
		if err == nil {
			err = kernel.ErrUnauthorized
		}
		return newErrorReply(remoteErrorCode(err))
	}
	offset, targetOffset, expectedHead, ok := call.PeerStatusPage()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	head, err := kernel.NewEventSequence(int64(expectedHead))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	items, nextOffset, head, err := daemon.store.PeerQuestionsForTask(ctx, authority.TaskID, offset, head)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	targets, nextTargetOffset, err := daemon.store.PeerTargetsForAttempt(ctx, kDigest, targetOffset, head)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	status := api.PeerStatus{Head: uint64(head.Int64()), Targets: make([]api.PeerTarget, 0, len(targets)), Questions: make([]api.PeerQuestion, 0, len(items)), NextOffset: nextOffset, NextTargetOffset: nextTargetOffset}
	for _, target := range targets {
		status.Targets = append(status.Targets, api.PeerTarget{TaskID: target.TaskID.String(), AgentID: target.AgentID.String(), Name: target.Name, Title: target.Title, Status: target.Status.String(), Revision: uint64(target.Revision.Int64())})
	}
	for _, item := range items {
		status.Questions = append(status.Questions, daemon.projectPeerQuestion(ctx, item))
	}
	state, err := daemon.store.Factory(ctx)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if state.Head != head {
		return newErrorReply(api.RemoteRevisionConflict)
	}
	reply, err := api.NewPeerStatusReply(status)
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) projectPeerQuestion(ctx context.Context, item kernel.PeerQuestion) api.PeerQuestion {
	target, targetFound, _ := daemon.store.Task(ctx, item.TargetTaskID)
	source, sourceFound, _ := daemon.store.Task(ctx, item.SourceTaskID)
	availability := func(task kernel.Task, found bool) string {
		if !found || (task.Status != kernel.TaskQueued && task.Status != kernel.TaskRunning) {
			return "stale"
		}
		if task.Status == kernel.TaskRunning {
			return "available"
		}
		return "pending"
	}
	return api.PeerQuestion{ID: item.ID.String(), SourceTaskID: item.SourceTaskID.String(), TargetTaskID: item.TargetTaskID.String(), Question: item.Question, Answer: item.Answer, RecipientDeliveryState: item.RecipientDeliveryState.String(), AnswerDeliveryState: item.AnswerDeliveryState.String(), RecipientAvailability: availability(target, targetFound), AnswerAvailability: availability(source, sourceFound), Revision: uint64(item.Revision.Int64())}
}

func (daemon *Daemon) peerAsk(ctx context.Context, call api.Call) api.Reply {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	input, ok := call.PeerQuestionInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	target, err := parseTaskID(input.TargetTaskID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	raw, err := parseID(input.IdempotencyKey)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	var key [kernel.IDBytes]byte
	copy(key[:], raw)
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	question, err := daemon.store.CreatePeerQuestionForAttempt(ctx, kDigest, kernel.NewPeerQuestion{TargetTaskID: target, IdempotencyKey: key, Question: input.Question}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if err := daemon.notifyPeerDelivery(ctx, question); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.mutation(ctx, question.Revision)
}

func (daemon *Daemon) peerAnswer(ctx context.Context, call api.Call) api.Reply {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	input, ok := call.PeerAnswerInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	rawID, err := parseID(input.QuestionID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := kernel.PeerQuestionIDFromBytes(rawID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	raw, err := parseID(input.IdempotencyKey)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	var key [kernel.IDBytes]byte
	copy(key[:], raw)
	revision, err := kernel.NewRevision(int64(input.ExpectedRevision))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	question, err := daemon.store.AnswerPeerQuestionForAttempt(ctx, kDigest, kernel.PeerAnswer{QuestionID: id, Expected: revision, IdempotencyKey: key, Answer: input.Answer}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if err := daemon.notifyPeerDelivery(ctx, question); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.mutation(ctx, question.Revision)
}

func (daemon *Daemon) notifyPeerDelivery(ctx context.Context, question kernel.PeerQuestion) error {
	if !terminalEffectsSupported {
		return nil
	}
	if question.AnswerIdempotencyKey == nil && question.RecipientDeliveryID != nil || question.AnswerIdempotencyKey != nil && question.AnswerDeliveryID != nil {
		return nil
	}
	run, found, err := daemon.store.RunningPeerTarget(ctx, question)
	if err != nil || !found {
		return err
	}
	var raw [kernel.IDBytes]byte
	if _, err := rand.Read(raw[:]); err != nil || raw == ([kernel.IDBytes]byte{}) {
		if err == nil {
			err = kernel.ErrCorruptState
		}
		return err
	}
	deliveryID, err := kernel.PeerDeliveryIDFromBytes(raw[:])
	if err != nil {
		return err
	}
	answer := question.AnswerIdempotencyKey != nil
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	delivery, newly, err := daemon.store.ReservePeerDelivery(ctx, question.ID, run.ID, deliveryID, answer, at)
	if err != nil || !newly {
		return err
	}
	attempt, err := daemon.liveTerminalAttempt(run.ID, kernel.TerminalSessionID{})
	if err != nil {
		return nil
	}
	result := attempt.submitEffect(ctx, terminalEffect{kind: terminalEffectHumanReply, payload: delivery.Payload, submit: true})
	if result.effectError(len(delivery.Payload)) != nil {
		return nil
	}
	ackAt, err := daemon.timestamp()
	if err != nil {
		return err
	}
	_, err = daemon.store.AcknowledgePeerDelivery(ctx, question.ID, delivery.DeliveryID, answer, delivery.Revision, ackAt)
	return err
}

func (daemon *Daemon) health(ctx context.Context) api.Reply {
	if _, err := daemon.store.Factory(ctx); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return api.NewHealthReply(api.HealthStatus{Ready: true})
}

func (daemon *Daemon) snapshot(ctx context.Context) api.Reply {
	snapshot, err := daemon.store.Snapshot(ctx)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	public := projectSnapshot(snapshot)
	reply, err := api.NewSnapshotReply(public)
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) createProject(ctx context.Context, call api.Call) api.Reply {
	input, ok := call.CreateProjectInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseProjectID(input.ID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	project, err := daemon.store.CreateProject(ctx, kernel.NewProject{
		ID: id, Name: input.Name, Root: input.Root, VerificationPolicy: kernel.VerificationNone,
	}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.mutation(ctx, project.Revision)
}

func (daemon *Daemon) createAgent(ctx context.Context, call api.Call) api.Reply {
	input, ok := call.CreateAgentInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseAgentID(input.ID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	projectID, err := parseProjectID(input.ProjectID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	role, err := parseAgentRole(input.Role)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	provider, err := kernel.ParseProvider(input.Provider)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	var accountID kernel.AccountID
	if input.AccountID != "" {
		decoded, decodeErr := parseID(input.AccountID)
		if decodeErr != nil {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		if accountID, err = kernel.AccountIDFromBytes(decoded); err != nil {
			return newErrorReply(api.RemoteInvalidRequest)
		}
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	agent, err := daemon.store.CreateAgent(ctx, kernel.NewAgent{
		ID: id, ProjectID: projectID, Name: input.Name, Role: role,
		Provider:        provider,
		Model:           input.Model,
		ReasoningEffort: input.ReasoningEffort,
		AccountID:       accountID,
		ToolBudgetLimit: input.ToolBudgetLimit,
	}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.mutation(ctx, agent.Revision)
}

func (daemon *Daemon) enqueueTask(ctx context.Context, call api.Call) api.Reply {
	input, ok := call.EnqueueTaskInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseTaskID(input.ID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	projectID, err := parseProjectID(input.ProjectID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	agentID, err := parseAgentID(input.AssignedAgentID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	incarnationID, err := parseIncarnationID(input.IncarnationID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	task, err := daemon.store.EnqueueTask(ctx, kernel.NewTask{
		ID: id, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnationID,
		Title: input.Title, Body: input.Body, Priority: input.Priority,
	}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.mutation(ctx, task.Revision)
}

func (daemon *Daemon) setDispatch(ctx context.Context, call api.Call) api.Reply {
	expected, enabled, ok := call.Dispatch()
	if !ok || expected > uint64(^uint64(0)>>1) {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	revision, err := kernel.NewRevision(int64(expected))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	state, err := daemon.store.SetDispatch(ctx, revision, enabled, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if enabled {
		daemon.notifyScheduler()
	}
	return mutationReply(state.Head, state.Revision)
}

func (daemon *Daemon) proposeOutcome(ctx context.Context, call api.Call) (api.Reply, *liveAttempt) {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest), nil
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest), nil
	}
	proposal, err := proposalForCall(call)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest), nil
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal), nil
	}
	daemon.operationMu.Lock()
	// This durable transition and the owner-side attach check share one
	// linearization gate. Whichever operation acquires it first owns the
	// running/finalizing boundary; notification carries no authority.
	operationCtx, cancel := context.WithTimeout(ctx, liveAttemptStoreTimeout)
	run, err := daemon.store.ProposeAttemptOutcome(operationCtx, kDigest, proposal, at)
	cancel()
	var attempt *liveAttempt
	if err == nil {
		daemon.attemptMu.Lock()
		attempt = daemon.attempts[run.ID]
		if attempt != nil {
			attempt.outcomeReceiptPending = true
		}
		daemon.attemptMu.Unlock()
		// The commit completed before ProposeAttemptOutcome returned. The owner
		// notification is deliberately best-effort and carries no authority or
		// state payload; it only shortens the next durable Store poll.
		daemon.notifyRun(run.ID)
	}
	daemon.operationMu.Unlock()
	if err != nil {
		return newErrorReply(remoteErrorCode(err)), nil
	}
	return daemon.mutation(ctx, run.Revision), attempt
}

func (daemon *Daemon) clearOutcomeReceipt(attempt *liveAttempt) {
	if daemon == nil || attempt == nil {
		return
	}
	daemon.operationMu.Lock()
	attempt.outcomeReceiptPending = false
	daemon.operationMu.Unlock()
	attempt.notify()
}

func attemptOutcomeCall(kind api.CallKind) bool {
	return kind == api.CallSucceed || kind == api.CallBlock || kind == api.CallFail
}

// sendBack returns a finished task to its queue: through an orchestrator's
// attempt credential, bound to its project, or as the operator, who names any
// task at its current revision.
func (daemon *Daemon) sendBack(ctx context.Context, call api.Call) api.Reply {
	input, ok := call.SendBackInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	taskID, err := parseTaskID(input.TaskID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	// An attempt is authenticated before anything is read on its behalf, so a
	// credential the kernel would refuse learns nothing about any task.
	var digest kernel.AttemptDigest
	var authority kernel.AttemptAuthority
	raw, attempt := call.AttemptDigest()
	if attempt {
		kDigest, err := attemptDigest(raw)
		if err != nil {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		authority, err = daemon.store.AuthenticateAttempt(ctx, kDigest)
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
		if authority.Role != kernel.RoleOrchestrator {
			return newErrorReply(api.RemoteUnauthorized)
		}
		digest = kDigest
	}
	// The body a send-back leaves is the provider's whole task, so it must fit
	// the provider of the agent that will run it, or the retry would be queued
	// only to fail at launch. The kernel edge authorizes again and writes.
	current, found, err := daemon.store.Task(ctx, taskID)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if !found {
		return newErrorReply(api.RemoteNotFound)
	}
	// Another project's task is not this orchestrator's to read further,
	// let alone measure against its provider.
	if attempt && current.ProjectID != authority.ProjectID {
		return newErrorReply(api.RemoteUnauthorized)
	}
	agent, found, err := daemon.store.Agent(ctx, current.AssignedAgentID)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if !found {
		return newErrorReply(api.RemoteConflict)
	}
	if _, _, err := provider.PrepareTask(agent.Provider, []byte(kernel.SentBackBody(current, input.Note))); err != nil {
		return newErrorReply(api.RemoteTooLarge)
	}
	var task kernel.Task
	if attempt {
		task, err = daemon.store.SendBackTaskForAttempt(ctx, digest, taskID, input.Note, at)
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
	} else {
		task, err = daemon.store.SendBackTask(ctx, taskID, current.Revision, input.Note, at)
		if err != nil {
			return newErrorReply(remoteErrorCode(err))
		}
	}
	// The task is queued again; the scheduler should not wait for its tick.
	daemon.notifyScheduler()
	return daemon.mutation(ctx, task.Revision)
}

func (daemon *Daemon) requestHuman(ctx context.Context, call api.Call) api.Reply {
	digest, ok := call.AttemptDigest()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	kDigest, err := attemptDigest(digest)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	input, ok := call.HumanQuestionInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	rawKey, err := parseID(input.IdempotencyKey)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	var key [kernel.IDBytes]byte
	copy(key[:], rawKey)
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	request, err := daemon.store.CreateHumanQuestionForAttempt(ctx, kDigest, kernel.NewHumanQuestion{
		IdempotencyKey: key,
		QuestionText:   input.Question,
	}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.mutation(ctx, request.Revision)
}

func (daemon *Daemon) overseerDigest(ctx context.Context, call api.Call) (kernel.AttemptAuthority, kernel.AttemptDigest, *api.Reply) {
	raw, ok := call.AttemptDigest()
	if !ok {
		failure := newErrorReply(api.RemoteInvalidRequest)
		return kernel.AttemptAuthority{}, kernel.AttemptDigest{}, &failure
	}
	digest, err := attemptDigest(raw)
	if err != nil {
		failure := newErrorReply(api.RemoteInvalidRequest)
		return kernel.AttemptAuthority{}, kernel.AttemptDigest{}, &failure
	}
	authority, err := daemon.store.AuthenticateAttempt(ctx, digest)
	if err != nil || authority.Role != kernel.RoleOrchestrator {
		if err == nil {
			err = kernel.ErrUnauthorized
		}
		failure := newErrorReply(remoteErrorCode(err))
		return kernel.AttemptAuthority{}, kernel.AttemptDigest{}, &failure
	}
	return authority, digest, nil
}

func (daemon *Daemon) overseerSnapshot(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerSnapshotInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request := kernel.OverseerSnapshotRequest{Offset: input.Offset, TextOffset: input.TextOffset}
	expectedHead, err := kernel.NewEventSequence(int64(input.ExpectedHead))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request.ExpectedHead = expectedHead
	if input.TaskID != "" {
		id, err := parseTaskID(input.TaskID)
		if err != nil {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		request.TaskID = &id
	}
	snapshot, err := daemon.store.OverseerSnapshotForAttempt(ctx, digest, request)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	reply, err := api.NewOverseerSnapshotReply(projectOverseerSnapshot(snapshot))
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) overseerEnqueueTask(ctx context.Context, call api.Call) api.Reply {
	authority, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerTaskCreateInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseTaskID(input.ID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	agentID, err := parseAgentID(input.AssignedAgentID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	incarnationID, err := parseIncarnationID(input.IncarnationID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	task, err := daemon.store.EnqueueTaskForOverseer(ctx, digest, kernel.NewTask{ID: id, ProjectID: authority.ProjectID, AssignedAgentID: agentID, IncarnationID: incarnationID, Title: input.Title, Body: input.Body, Priority: input.Priority}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.mutation(ctx, task.Revision)
}

func (daemon *Daemon) overseerUpdateTask(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerTaskUpdateInput()
	if !ok || input.ExpectedRevision > uint64(^uint64(0)>>1) {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseTaskID(input.TaskID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	expected, err := kernel.NewRevision(int64(input.ExpectedRevision))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	var assignedAgentID *kernel.AgentID
	if input.AssignedAgentID != nil {
		agentID, err := parseAgentID(*input.AssignedAgentID)
		if err != nil {
			return newErrorReply(api.RemoteInvalidRequest)
		}
		assignedAgentID = &agentID
	}
	patch := kernel.TaskPatch{Title: input.Title, Body: input.Body, Priority: input.Priority, AssignedAgentID: assignedAgentID, Cancel: input.Cancel}
	if err := daemon.store.AuthorizeWorkerTaskForOverseer(ctx, digest, id); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if err := prepareQueuedTaskPatch(ctx, daemon.store, id, expected, patch); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	task, err := daemon.store.UpdateTaskForOverseer(ctx, digest, id, expected, patch, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.mutation(ctx, task.Revision)
}

func (daemon *Daemon) overseerUpdateAgent(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerAgentUpdateInput()
	if !ok || input.ExpectedRevision > uint64(^uint64(0)>>1) {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	id, err := parseAgentID(input.AgentID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	expected, err := kernel.NewRevision(int64(input.ExpectedRevision))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	paused := input.Paused
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	agent, err := daemon.store.UpdateAgentForOverseer(ctx, digest, id, expected, paused, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.mutation(ctx, agent.Revision)
}

func (daemon *Daemon) overseerStopRun(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerRunStopInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request, err := overseerInterventionRequest(input.OperationID, input.TaskID, input.ExpectedTaskRevision, input.RunID, input.ExpectedRunRevision, kernel.TaskInterventionStop, "")
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	receipt, err := daemon.store.StopRunForAttempt(ctx, digest, request, nil, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.interventionMutation(ctx, receipt)
}

func (daemon *Daemon) overseerReplaceRun(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerRunReplaceInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request, err := overseerInterventionRequest(input.OperationID, input.TaskID, input.ExpectedTaskRevision, input.RunID, input.ExpectedRunRevision, kernel.TaskInterventionReplace, "")
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	successorID, err := parseTaskID(input.SuccessorTaskID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	incarnationID, err := parseIncarnationID(input.SuccessorIncarnationID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	if err := daemon.prepareOverseerReplacement(ctx, request.TaskID, input.Instruction); err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	receipt, err := daemon.store.StopRunForAttempt(ctx, digest, request, &kernel.NewTask{ID: successorID, IncarnationID: incarnationID, Body: input.Instruction}, at)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	daemon.notifyScheduler()
	return daemon.interventionMutation(ctx, receipt)
}

func (daemon *Daemon) overseerMessageWorker(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerWorkerMessageInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request, err := overseerInterventionRequest(input.OperationID, input.TaskID, input.ExpectedTaskRevision, input.RunID, input.ExpectedRunRevision, kernel.TaskInterventionMessage, input.Message)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	receipt, err := daemon.overseerIntervention(ctx, digest, request)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.interventionMutation(ctx, receipt)
}

func (daemon *Daemon) overseerInterruptWorker(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerWorkerInterruptInput()
	if !ok {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	request, err := overseerInterventionRequest(input.OperationID, input.TaskID, input.ExpectedTaskRevision, input.RunID, input.ExpectedRunRevision, kernel.TaskInterventionInterrupt, "")
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	receipt, err := daemon.overseerIntervention(ctx, digest, request)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.interventionMutation(ctx, receipt)
}

func (daemon *Daemon) overseerReplyHuman(ctx context.Context, call api.Call) api.Reply {
	_, digest, failure := daemon.overseerDigest(ctx, call)
	if failure != nil {
		return *failure
	}
	input, ok := call.OverseerHumanReplyInput()
	if !ok || input.ExpectedRevision > uint64(^uint64(0)>>1) {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	requestID, err := parseHumanRequestID(input.RequestID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	deliveryID, err := parseHumanDeliveryID(input.OperationID)
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	expected, err := kernel.NewRevision(int64(input.ExpectedRevision))
	if err != nil {
		return newErrorReply(api.RemoteInvalidRequest)
	}
	if !terminalEffectsSupported {
		return newErrorReply(api.RemoteUnavailable)
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	delivery, err := daemon.store.BeginHumanReplyForAttempt(ctx, digest, requestID, expected, deliveryID, input.Reply, at)
	if err != nil {
		if terminalStoreOutcomeUnknown(err) {
			deliveryRevision, revisionErr := kernel.NewRevision(expected.Int64() + 1)
			var unknownErr error
			if revisionErr == nil {
				unknownErr = daemon.markHumanReplyUnknown(requestID, deliveryID, deliveryRevision)
			}
			return newErrorReply(remoteErrorCode(errors.Join(err, revisionErr, unknownErr)))
		}
		return newErrorReply(remoteErrorCode(err))
	}
	if delivery.RequestID != requestID || delivery.DeliveryID != deliveryID {
		return newErrorReply(api.RemoteInternal)
	}
	_, effectErr := daemon.deliverHumanReply(ctx, delivery)
	projection, err := daemon.humanReplyOutcome(requestID, effectErr)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return daemon.overseerHumanReplyMutation(projection)
}

func (daemon *Daemon) overseerHumanReplyMutation(projection kernel.HumanRequestProjection) api.Reply {
	readCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	defer cancel()
	state, err := daemon.store.Factory(readCtx)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	reply, err := api.NewMutationReply(api.MutationResult{Head: uint64(state.Head.Int64()), Revision: uint64(projection.Revision.Int64()), HumanReply: &api.OverseerHumanReplyResult{RequestID: projection.ID.String(), State: projection.Status.String()}})
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func overseerInterventionRequest(operationText, taskText string, taskRevision uint64, runText string, runRevision uint64, kind kernel.TaskInterventionKind, payload string) (kernel.TaskInterventionRequest, error) {
	operation, err := parseTaskInterventionID(operationText)
	if err != nil {
		return kernel.TaskInterventionRequest{}, err
	}
	task, err := parseTaskID(taskText)
	if err != nil {
		return kernel.TaskInterventionRequest{}, err
	}
	run, err := parseRunID(runText)
	if err != nil || taskRevision > uint64(^uint64(0)>>1) || runRevision > uint64(^uint64(0)>>1) {
		return kernel.TaskInterventionRequest{}, kernel.ErrInvalidValue
	}
	expectedTask, err := kernel.NewRevision(int64(taskRevision))
	if err != nil {
		return kernel.TaskInterventionRequest{}, err
	}
	expectedRun, err := kernel.NewRevision(int64(runRevision))
	if err != nil {
		return kernel.TaskInterventionRequest{}, err
	}
	return kernel.TaskInterventionRequest{OperationID: operation, TaskID: task, RunID: run, ExpectedTaskRevision: expectedTask, ExpectedRunRevision: expectedRun, Kind: kind, Payload: payload}, nil
}

func (daemon *Daemon) interventionMutation(_ context.Context, receipt kernel.TaskIntervention) api.Reply {
	readCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	defer cancel()
	task, found, err := daemon.store.Task(readCtx, receipt.TaskID)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	if !found {
		return newErrorReply(api.RemoteInternal)
	}
	state, err := daemon.store.Factory(readCtx)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	detail := ""
	if receipt.ResultDetail != nil {
		detail = *receipt.ResultDetail
	}
	reply, err := api.NewMutationReply(api.MutationResult{Head: uint64(state.Head.Int64()), Revision: uint64(task.Revision.Int64()), Intervention: &api.OverseerInterventionResult{OperationID: receipt.OperationID.String(), State: receipt.State.String(), Detail: detail}})
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) prepareOverseerReplacement(ctx context.Context, taskID kernel.TaskID, instruction string) error {
	task, found, err := daemon.store.Task(ctx, taskID)
	if err != nil {
		return err
	}
	if !found {
		return kernel.ErrNotFound
	}
	agent, found, err := daemon.store.Agent(ctx, task.AssignedAgentID)
	if err != nil {
		return err
	}
	if !found {
		return kernel.ErrCorruptState
	}
	_, _, err = provider.PrepareTask(agent.Provider, []byte(instruction))
	return err
}

func proposalForCall(call api.Call) (kernel.Proposal, error) {
	switch call.Kind() {
	case api.CallSucceed:
		result, ok := call.Result()
		if !ok {
			return kernel.Proposal{}, fmt.Errorf("%w: missing success result", kernel.ErrInvalidValue)
		}
		return kernel.NewSuccessProposal(result)
	case api.CallBlock:
		detail, ok := call.Detail()
		if !ok {
			return kernel.Proposal{}, fmt.Errorf("%w: missing block detail", kernel.ErrInvalidValue)
		}
		return kernel.NewBlockedProposal(detail)
	case api.CallFail:
		detail, ok := call.Detail()
		if !ok {
			return kernel.Proposal{}, fmt.Errorf("%w: missing failure detail", kernel.ErrInvalidValue)
		}
		return kernel.NewFailureProposal(kernel.FailureAttempt, detail)
	default:
		return kernel.Proposal{}, fmt.Errorf("%w: non-outcome call", kernel.ErrInvalidValue)
	}
}

func (daemon *Daemon) mutation(ctx context.Context, revision kernel.Revision) api.Reply {
	state, err := daemon.store.Factory(ctx)
	if err != nil {
		return newErrorReply(remoteErrorCode(err))
	}
	return mutationReply(state.Head, revision)
}

func mutationReply(head kernel.EventSequence, revision kernel.Revision) api.Reply {
	reply, err := api.NewMutationReply(api.MutationResult{Head: uint64(head.Int64()), Revision: uint64(revision.Int64())})
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	return reply
}

func (daemon *Daemon) timestamp() (kernel.UnixMillis, error) {
	if daemon == nil || daemon.now == nil {
		return kernel.UnixMillis{}, fmt.Errorf("%w: missing daemon clock", kernel.ErrInvalidValue)
	}
	return kernel.NewUnixMillis(daemon.now().UnixMilli())
}

func (daemon *Daemon) notifyRun(runID kernel.RunID) {
	if daemon == nil {
		return
	}
	daemon.attemptMu.Lock()
	attempt := daemon.attempts[runID]
	daemon.attemptMu.Unlock()
	if attempt != nil {
		attempt.notify()
	}
}

func (daemon *Daemon) registerSupervisor(parent context.Context) (*supervisorRegistration, error) {
	if daemon == nil || parent == nil {
		return nil, fmt.Errorf("%w: invalid supervisor", kernel.ErrInvalidValue)
	}
	ctx, cancel := context.WithCancel(parent)
	registration := &supervisorRegistration{ctx: ctx, cancel: cancel, done: make(chan struct{})}
	daemon.attemptMu.Lock()
	defer daemon.attemptMu.Unlock()
	if daemon.closing {
		cancel()
		return nil, ErrTerminalClosed
	}
	if daemon.supervisors == nil {
		daemon.supervisors = make(map[*supervisorRegistration]struct{})
	}
	daemon.supervisors[registration] = struct{}{}
	return registration, nil
}

func (daemon *Daemon) endSupervisor(registration *supervisorRegistration, result error) {
	if daemon == nil || registration == nil {
		return
	}
	registration.mu.Lock()
	registration.result = result
	close(registration.done)
	registration.mu.Unlock()
	registration.cancel()
	daemon.attemptMu.Lock()
	delete(daemon.supervisors, registration)
	daemon.attemptMu.Unlock()
}

func (registration *supervisorRegistration) wait() error {
	if registration == nil {
		return nil
	}
	<-registration.done
	registration.mu.Lock()
	defer registration.mu.Unlock()
	return registration.result
}

// Close stops accepting live attempts and synchronously joins every owner.
// The API listener and Store are owned by their callers and are intentionally
// not closed here.
func (daemon *Daemon) Close() error {
	if daemon == nil {
		return nil
	}
	return errors.Join(daemon.closeBrowsers(), daemon.closeLiveAttempts())
}

func newErrorReply(code api.RemoteErrorCode) api.Reply {
	reply, err := api.NewErrorReply(code)
	if err != nil {
		return api.Reply{}
	}
	return reply
}

func remoteErrorCode(err error) api.RemoteErrorCode {
	if err == nil {
		return api.RemoteInternal
	}
	var unknown *kernel.OutcomeUnknownError
	if errors.As(err, &unknown) {
		return api.RemoteInternal
	}
	switch {
	case errors.Is(err, context.Canceled), errors.Is(err, context.DeadlineExceeded):
		return api.RemoteUnavailable
	case errors.Is(err, kernel.ErrInvalidValue):
		return api.RemoteInvalidRequest
	case errors.Is(err, kernel.ErrUnauthorized):
		return api.RemoteUnauthorized
	case errors.Is(err, kernel.ErrNotFound):
		return api.RemoteNotFound
	case errors.Is(err, kernel.ErrRevisionConflict):
		return api.RemoteRevisionConflict
	case errors.Is(err, kernel.ErrConflict):
		return api.RemoteConflict
	case errors.Is(err, kernel.ErrSnapshotTooLarge):
		return api.RemoteTooLarge
	case errors.Is(err, kernel.ErrBusy), errors.Is(err, kernel.ErrStoreClosed):
		return api.RemoteUnavailable
	default:
		return api.RemoteInternal
	}
}
