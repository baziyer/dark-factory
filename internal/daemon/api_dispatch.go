package daemon

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// Daemon is the concrete composition root for the local API. It owns the
// durable Store and live attempt owners. It does not own an accept loop; the
// caller accepts and hands one connection to HandleConnection.
type Daemon struct {
	store *kernel.Store
	now   func() time.Time

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

	// operationMu is the single linearization gate for live-attempt operations
	// that combine durable state with an owner-side action. It is deliberately
	// concrete and global: the local operator has no throughput requirement,
	// and one gate avoids a second digest-to-owner authority index.
	operationMu sync.Mutex

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
	if store == nil {
		return nil, fmt.Errorf("%w: nil kernel store", kernel.ErrInvalidValue)
	}
	return &Daemon{store: store, now: time.Now, browsers: make(map[*BrowserRuntime]struct{}), browserClientGates: &browserClientGates{}, attempts: make(map[kernel.RunID]*liveAttempt), supervisors: make(map[*supervisorRegistration]struct{}), schedulerWake: make(chan struct{}, 1)}, nil
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
	at, err := daemon.timestamp()
	if err != nil {
		return newErrorReply(api.RemoteInternal)
	}
	agent, err := daemon.store.CreateAgent(ctx, kernel.NewAgent{
		ID: id, ProjectID: projectID, Name: input.Name, Role: role,
		Provider:        provider,
		Model:           input.Model,
		ReasoningEffort: input.ReasoningEffort,
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
