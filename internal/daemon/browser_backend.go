package daemon

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"math"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/topology"
)

// browserStatePollInterval bounds how long a paired browser waits to learn
// that durable state moved. There is no commit-time push today; the watcher
// reads the durable head on a bounded poll.
const browserStatePollInterval = 100 * time.Millisecond

// browserBackend is the direct browser-to-Store adapter. It deliberately
// keeps no durable projection or capability cache: every authenticated
// operation reloads the exact browser client while holding its ephemeral
// operation gate.
type browserBackend struct {
	owner *Daemon
	store *kernel.Store
	now   func() time.Time
	boot  kernel.BootID

	randomMu sync.Mutex
	random   io.Reader

	clientGates *browserClientGates

	inviteMu    sync.Mutex
	inviteMints [4]time.Time
	inviteNext  int

	subMu   sync.Mutex
	closing bool
	subs    map[*browserStateWatch]struct{}
}

type browserClientGate struct {
	users int
	slot  chan struct{}
}

// browserClientGates serializes only operations for one exact durable browser
// client. Store reads and writes remain the authority; this small ephemeral
// gate merely closes the authenticate/effect-versus-revoke race.
type browserClientGates struct {
	mu    sync.Mutex
	gates map[kernel.BrowserClientID]*browserClientGate
}

func newBrowserBackend(store *kernel.Store, now func() time.Time, random io.Reader) (*browserBackend, error) {
	if store == nil || now == nil || random == nil {
		return nil, fmt.Errorf("%w: invalid browser backend", kernel.ErrInvalidValue)
	}
	backend := &browserBackend{
		store: store, now: now, random: random,
		clientGates: &browserClientGates{gates: make(map[kernel.BrowserClientID]*browserClientGate)},
		subs:        make(map[*browserStateWatch]struct{}),
	}
	raw, err := backend.randomIdentifier()
	if err != nil {
		return nil, err
	}
	backend.boot, err = kernel.BootIDFromBytes(raw[:])
	if err != nil {
		return nil, fmt.Errorf("browser boot identity: %w", err)
	}
	return backend, nil
}

func newProductionBrowserBackend(daemon *Daemon) (*browserBackend, error) {
	if daemon == nil || daemon.store == nil || daemon.now == nil || daemon.browserClientGates == nil {
		return nil, fmt.Errorf("%w: invalid daemon browser backend", kernel.ErrInvalidValue)
	}
	backend, err := newBrowserBackend(daemon.store, daemon.now, rand.Reader)
	if err != nil {
		return nil, err
	}
	backend.clientGates = daemon.browserClientGates
	backend.owner = daemon
	return backend, nil
}

func (backend *browserBackend) Identity(ctx context.Context) (browser.Identity, error) {
	if backend == nil || backend.store == nil {
		return browser.Identity{}, browser.ErrUnauthorized
	}
	state, err := backend.store.Factory(ctx)
	if err != nil {
		return browser.Identity{}, mapBrowserError(err)
	}
	var result browser.Identity
	copy(result.DaemonID[:], state.DaemonID.Bytes())
	copy(result.BootID[:], backend.boot.Bytes())
	return result, nil
}

func (backend *browserBackend) Pair(ctx context.Context, request browser.PairRequest) (browser.Authentication, error) {
	identity, err := backend.Identity(ctx)
	if err != nil || request.Identity != identity {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	transcript, err := browserprotocol.BuildPairTranscript(browserprotocol.PairTranscript{
		DaemonID: request.DaemonID[:], BootID: request.BootID[:], ConnectionNonce: request.ConnectionNonce[:],
		Challenge: request.Challenge[:], PublicKeySEC1: request.PublicKeySEC1[:],
		ValidatedHost: request.Host, ValidatedOrigin: request.Origin,
	})
	if err != nil || browserprotocol.VerifySignature(request.PublicKeySEC1[:], request.Signature[:], transcript) != nil {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	rawID, err := backend.randomIdentifier()
	if err != nil {
		return browser.Authentication{}, err
	}
	clientID, err := kernel.BrowserClientIDFromBytes(rawID[:])
	if err != nil {
		return browser.Authentication{}, err
	}
	at, err := backend.timestamp()
	if err != nil {
		return browser.Authentication{}, err
	}
	client, err := backend.store.RedeemBrowserPairingChallenge(ctx, kernel.HashBrowserChallenge(request.Challenge[:]), backend.boot, request.Origin, clientID, request.PublicKeySEC1[:], at)
	if err != nil {
		return browser.Authentication{}, mapBrowserError(err)
	}
	return projectBrowserAuthentication(client)
}

func (backend *browserBackend) Authenticate(ctx context.Context, request browser.AuthRequest) (browser.Authentication, error) {
	identity, err := backend.Identity(ctx)
	if err != nil || request.Identity != identity {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	clientID, release, client, err := backend.authorize(ctx, request.ClientID, kernel.BrowserCapabilityObserve)
	if err != nil {
		return browser.Authentication{}, err
	}
	defer release()
	if client.ID != clientID {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	transcript, err := browserprotocol.BuildAuthTranscript(browserprotocol.AuthTranscript{
		DaemonID: request.DaemonID[:], BootID: request.BootID[:], ConnectionNonce: request.ConnectionNonce[:],
		ClientID: request.ClientID[:], ValidatedHost: request.Host, ValidatedOrigin: request.Origin,
	})
	if err != nil || browserprotocol.VerifySignature(client.PublicKey, request.Signature[:], transcript) != nil {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	return projectBrowserAuthentication(client)
}

func (backend *browserBackend) StateSnapshot(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte) (browserprotocol.StateSnapshot, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityObserve)
	if err != nil {
		return browserprotocol.StateSnapshot{}, err
	}
	defer release()
	snapshot, err := backend.store.ReadPublicSnapshot(ctx)
	if err != nil {
		return browserprotocol.StateSnapshot{}, mapBrowserError(err)
	}
	return projectPublicSnapshot(snapshot)
}

func (backend *browserBackend) HumanRequestDetail(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.HumanRequestDetailGet) (browserprotocol.HumanRequestDetail, error) {
	clientID, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityPrivateHumanRequestDetail)
	if err != nil {
		return browserprotocol.HumanRequestDetail{}, err
	}
	defer release()
	rawRequest, err := parseID(request.RequestID)
	if err != nil {
		return browserprotocol.HumanRequestDetail{}, browser.ErrStale
	}
	requestID, err := kernel.HumanRequestIDFromBytes(rawRequest)
	if err != nil || uint64(request.ExpectedRevision) > math.MaxInt64 {
		return browserprotocol.HumanRequestDetail{}, browser.ErrStale
	}
	revision, err := kernel.NewRevision(int64(request.ExpectedRevision))
	if err != nil {
		return browserprotocol.HumanRequestDetail{}, browser.ErrStale
	}
	detail, err := backend.store.HumanRequestDetail(ctx, clientID, requestID, revision)
	if err != nil {
		return browserprotocol.HumanRequestDetail{}, mapBrowserError(err)
	}
	result := browserprotocol.HumanRequestDetail{
		RequestID: detail.ID.String(), Revision: decimalRevision(detail.Revision), Question: detail.QuestionText,
		CanReply: browserprotocol.Bool(detail.CanReply), ReplyMaxBytes: uint16(detail.ReplyMaxBytes),
	}
	if detail.TerminalTarget != nil {
		result.TerminalTarget = &browserprotocol.TerminalTargetDescriptor{
			RunID: detail.TerminalTarget.RunID().String(), SessionID: detail.TerminalTarget.SessionID().String(),
			RunRevision: decimalRevision(detail.TerminalTarget.RunRevision()), SessionRevision: decimalRevision(detail.TerminalTarget.SessionRevision()),
		}
	}
	if detail.CancelRun != nil {
		result.CancelRun = &browserprotocol.HumanRequestCancelRunDescriptor{
			ExpectedRequestRevision: decimalRevision(detail.CancelRun.ExpectedRequestRevision()),
			ExpectedRunRevision:     decimalRevision(detail.CancelRun.ExpectedRunRevision()),
		}
	}
	return result, nil
}

func (backend *browserBackend) TerminalTarget(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TerminalTargetGet) (browserprotocol.TerminalTarget, error) {
	clientID, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityObserve)
	if err != nil {
		return browserprotocol.TerminalTarget{}, err
	}
	defer release()
	agentID, err := browserID(request.AgentID, kernel.AgentIDFromBytes)
	if err != nil {
		return browserprotocol.TerminalTarget{}, browser.ErrStale
	}
	expectedAgent, err := browserDecimal(request.ExpectedAgentRevision)
	if err != nil {
		return browserprotocol.TerminalTarget{}, browser.ErrStale
	}
	if request.ExpectedHead > math.MaxInt64 {
		return browserprotocol.TerminalTarget{}, browser.ErrStale
	}
	expectedHead, err := kernel.NewEventSequence(int64(request.ExpectedHead))
	if err != nil {
		return browserprotocol.TerminalTarget{}, browser.ErrStale
	}
	target, available, err := backend.store.ResolveAgentTerminalTarget(ctx, clientID, agentID, expectedAgent, expectedHead)
	if err != nil {
		return browserprotocol.TerminalTarget{}, mapBrowserError(err)
	}
	result := browserprotocol.TerminalTarget{AgentID: request.AgentID, AgentRevision: request.ExpectedAgentRevision, Head: request.ExpectedHead}
	if available {
		result.Target = &browserprotocol.TerminalTargetDescriptor{
			RunID: target.RunID().String(), SessionID: target.SessionID().String(),
			RunRevision: decimalRevision(target.RunRevision()), SessionRevision: decimalRevision(target.SessionRevision()),
		}
	}
	return result, nil
}

// PairLink is the pair page's mint: the one-shot launch link OpenBrowser
// returns.
func (backend *browserBackend) PairLink(ctx context.Context) (string, error) {
	if backend == nil || backend.owner == nil {
		return "", browser.ErrUnauthorized
	}
	return backend.owner.OpenBrowser(ctx)
}

func (backend *browserBackend) EnqueueTask(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskEnqueue) (browserprotocol.TaskEnqueueResult, error) {
	clientID, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, err
	}
	defer release()
	taskID, err := browserID(request.TaskID, kernel.TaskIDFromBytes)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, browser.ErrStale
	}
	incarnationID, err := browserID(request.IncarnationID, kernel.IncarnationIDFromBytes)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, browser.ErrStale
	}
	agentID, err := browserID(request.AgentID, kernel.AgentIDFromBytes)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, browser.ErrStale
	}
	expectedAgentRevision, err := browserDecimal(request.ExpectedAgentRevision)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, browser.ErrStale
	}
	at, err := backend.timestamp()
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, mapBrowserError(err)
	}
	result, err := backend.store.EnqueueTaskForBrowserAgent(ctx, clientID, taskID, incarnationID, agentID, expectedAgentRevision, request.Instruction, at)
	if err != nil {
		return browserprotocol.TaskEnqueueResult{}, mapBrowserError(err)
	}
	if backend.owner != nil {
		backend.owner.notifyScheduler()
	}
	return browserprotocol.TaskEnqueueResult{TaskID: result.Task.ID.String(), Revision: decimalRevision(result.Task.Revision), AgentRevision: decimalRevision(result.AgentRevision)}, nil
}

// The transport discovers the console half by type assertion, so a signature
// that drifts would silently answer unauthorized instead of failing to build.
var _ browser.ConsoleBackend = (*browserBackend)(nil)

func (backend *browserBackend) UpdateAgent(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.AgentUpdate) (browserprotocol.AgentUpdateResult, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return browserprotocol.AgentUpdateResult{}, err
	}
	defer release()
	agentID, err := browserID(request.AgentID, kernel.AgentIDFromBytes)
	if err != nil {
		return browserprotocol.AgentUpdateResult{}, browser.ErrStale
	}
	expected, err := browserDecimal(request.ExpectedRevision)
	if err != nil {
		return browserprotocol.AgentUpdateResult{}, browser.ErrStale
	}
	at, err := backend.timestamp()
	if err != nil {
		return browserprotocol.AgentUpdateResult{}, mapBrowserError(err)
	}
	patch := kernel.AgentPatch{Model: request.Model, ReasoningEffort: request.ReasoningEffort}
	if request.Paused != nil {
		paused := bool(*request.Paused)
		patch.Paused = &paused
	}
	agent, err := backend.store.UpdateAgent(ctx, agentID, expected, patch, at)
	if err != nil {
		return browserprotocol.AgentUpdateResult{}, consoleUpdateError(err)
	}
	if backend.owner != nil {
		backend.owner.notifyScheduler()
	}
	return browserprotocol.AgentUpdateResult{AgentID: agent.ID.String(), Revision: decimalRevision(agent.Revision)}, nil
}

func (backend *browserBackend) UpdateTask(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskUpdate) (browserprotocol.TaskUpdateResult, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return browserprotocol.TaskUpdateResult{}, err
	}
	defer release()
	taskID, err := browserID(request.TaskID, kernel.TaskIDFromBytes)
	if err != nil {
		return browserprotocol.TaskUpdateResult{}, browser.ErrStale
	}
	expected, err := browserDecimal(request.ExpectedRevision)
	if err != nil {
		return browserprotocol.TaskUpdateResult{}, browser.ErrStale
	}
	patch := kernel.TaskPatch{Title: request.Title, Priority: request.Priority, Cancel: request.Status != nil}
	if request.AssignedAgentID != nil {
		assigned, err := browserID(*request.AssignedAgentID, kernel.AgentIDFromBytes)
		if err != nil {
			return browserprotocol.TaskUpdateResult{}, browser.ErrStale
		}
		patch.AssignedAgentID = &assigned
	}
	at, err := backend.timestamp()
	if err != nil {
		return browserprotocol.TaskUpdateResult{}, mapBrowserError(err)
	}
	task, err := backend.store.UpdateTask(ctx, taskID, expected, patch, at)
	if err != nil {
		return browserprotocol.TaskUpdateResult{}, consoleUpdateError(err)
	}
	if backend.owner != nil {
		backend.owner.notifyScheduler()
	}
	return browserprotocol.TaskUpdateResult{TaskID: task.ID.String(), Revision: decimalRevision(task.Revision)}, nil
}

// Topology serves the regenerable project structure the daemon already caches
// on disk. Nodes only: containment is implied by parent, so v1 has no edges.
func (backend *browserBackend) Topology(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte, request browserprotocol.TopologyGet) (browserprotocol.Topology, error) {
	_, release, _, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityObserve)
	if err != nil {
		return browserprotocol.Topology{}, err
	}
	defer release()
	if backend.owner == nil {
		return browserprotocol.Topology{}, browser.ErrNotFound
	}
	projectID, err := browserID(request.ProjectID, kernel.ProjectIDFromBytes)
	if err != nil {
		return browserprotocol.Topology{}, browser.ErrStale
	}
	snapshot, err := backend.owner.ProjectTopology(ctx, projectID)
	if err != nil {
		return browserprotocol.Topology{}, mapBrowserError(err)
	}
	return projectTopology(request.ProjectID, snapshot), nil
}

// projectTopology is the one conversion from the derived graph to the wire.
// The tree is a filesystem and bounds nothing; the wire bounds every node's
// text. A node past a bound is dropped with everything under it, so one
// long directory name costs its subtree instead of making the whole project
// unserveable. Truncating instead would invent a label and could silently
// merge two siblings that differ only past the cut.
func projectTopology(projectID string, snapshot topology.Snapshot) browserprotocol.Topology {
	result := browserprotocol.Topology{
		ProjectID: projectID, Digest: snapshot.Digest, SourceRevision: snapshot.SourceRevision,
		Nodes: make([]browserprotocol.TopologyNode, 0, len(snapshot.Nodes)),
	}
	// Nodes are ordered by path and kind, which is not ancestry order: a root
	// module sorts before the repository that contains it. So each node is
	// decided by walking its own ancestor chain, never by slice position.
	byID := make(map[string]topology.Node, len(snapshot.Nodes))
	for _, node := range snapshot.Nodes {
		byID[node.ID] = node
	}
	decided := make(map[string]bool, len(snapshot.Nodes))
	var servable func(string) bool
	servable = func(id string) bool {
		if result, ok := decided[id]; ok {
			return result
		}
		node, ok := byID[id]
		if !ok {
			return false
		}
		// Provisionally false, so a chain that loops back on corrupt input
		// terminates and fails closed rather than recursing forever.
		decided[id] = false
		result := servableTopologyText(node) && (node.ParentID == "" || servable(node.ParentID))
		decided[id] = result
		return result
	}
	for _, node := range snapshot.Nodes {
		if !servable(node.ID) {
			continue
		}
		result.Nodes = append(result.Nodes, browserprotocol.TopologyNode{
			ID: node.ID, ParentID: node.ParentID, Kind: string(node.Kind), Path: node.RelativePath,
			Label: node.Label, Language: node.Language, SizeBucket: node.SizeBucket,
		})
	}
	return result
}

// consoleUpdateError separates a member the domain refuses from a lost race.
// Both are the operator's to resolve, but only one is resolved by refetching:
// a reasoning effort no provider accepts is still refused after a refresh.
func consoleUpdateError(err error) error {
	if errors.Is(err, kernel.ErrInvalidValue) {
		return browser.ErrInvalidRequest
	}
	return mapBrowserError(err)
}

func servableTopologyText(node topology.Node) bool {
	return validTopologyText(node.RelativePath, 1, browserprotocol.MaxTaskTitleBytes) &&
		validTopologyText(node.Label, 1, browserprotocol.MaxAgentNameBytes) &&
		validTopologyText(node.Language, 0, browserprotocol.MaxAgentNameBytes)
}

func validTopologyText(value string, minimum, maximum int) bool {
	return len(value) >= minimum && len(value) <= maximum && utf8.ValidString(value)
}

// RemoteInvite mints one remote pairing invitation for a paired operator, plus
// its scannable code. The mint is never retried; a failure is reported and the
// operator asks again.
func (backend *browserBackend) RemoteInvite(ctx context.Context, rawClient [browserprotocol.ClientIDSize]byte) (browserprotocol.RemoteInviteResult, error) {
	_, release, client, err := backend.authorize(ctx, rawClient, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return browserprotocol.RemoteInviteResult{}, err
	}
	defer release()
	// terminal_input is the one bit a remote grant never carries, so requiring
	// it means only a client paired on this machine's own loopback can invite a
	// phone: a remote controller cannot propagate its own pairing.
	if !client.CapabilityMask.Has(kernel.BrowserCapabilityTerminalInput) {
		return browserprotocol.RemoteInviteResult{}, browser.ErrUnauthorized
	}
	if backend.owner == nil {
		return browserprotocol.RemoteInviteResult{}, browser.ErrUnauthorized
	}
	if !backend.admitRemoteInvite() {
		return browserprotocol.RemoteInviteResult{}, browser.ErrRateLimited
	}
	invitation, err := backend.owner.RemotePair(ctx)
	if err != nil {
		return browserprotocol.RemoteInviteResult{}, mapBrowserError(err)
	}
	// The challenge is already committed. A render that fails here leaves one
	// unredeemed challenge, which nobody can reach and which expires on its own.
	code, err := qrSVG(invitation.Link)
	if err != nil {
		return browserprotocol.RemoteInviteResult{}, mapBrowserError(err)
	}
	return browserprotocol.RemoteInviteResult{Link: invitation.Link, ExpiresAtMS: browserprotocol.Decimal(invitation.Expires * 1000), SVG: code}, nil
}

// admitRemoteInvite bounds minting to four invitations per challenge TTL. The
// page asks; it does not choose this bound. Four per TTL means even a console
// looping REMOTE_INVITE holds at most four of the 32 live challenge slots, so
// the loopback /pair page can always still mint one, while an operator
// re-minting after a failed scan never reaches the limit. An admitted attempt
// spends its slot whether or not the mint that follows succeeds.
func (backend *browserBackend) admitRemoteInvite() bool {
	backend.inviteMu.Lock()
	defer backend.inviteMu.Unlock()
	now := backend.now()
	// The slot about to be overwritten holds the oldest of the four.
	if oldest := backend.inviteMints[backend.inviteNext]; !oldest.IsZero() && now.Sub(oldest) < webChallengeTTL {
		return false
	}
	backend.inviteMints[backend.inviteNext] = now
	backend.inviteNext = (backend.inviteNext + 1) % len(backend.inviteMints)
	return true
}

func (backend *browserBackend) authorize(ctx context.Context, rawID [browserprotocol.ClientIDSize]byte, capability kernel.BrowserCapabilityMask) (kernel.BrowserClientID, func(), kernel.BrowserClient, error) {
	clientID, err := kernel.BrowserClientIDFromBytes(rawID[:])
	if err != nil {
		return kernel.BrowserClientID{}, nil, kernel.BrowserClient{}, browser.ErrUnauthorized
	}
	release, err := backend.acquireClient(ctx, clientID)
	if err != nil {
		return kernel.BrowserClientID{}, nil, kernel.BrowserClient{}, mapBrowserError(err)
	}
	client, found, err := backend.store.BrowserClient(ctx, clientID)
	if err != nil || !found || client.RevokedAt != nil || !client.CapabilityMask.Has(capability) {
		release()
		if err != nil {
			return kernel.BrowserClientID{}, nil, kernel.BrowserClient{}, mapBrowserError(err)
		}
		return kernel.BrowserClientID{}, nil, kernel.BrowserClient{}, browser.ErrUnauthorized
	}
	return clientID, release, client, nil
}

func (backend *browserBackend) acquireClient(ctx context.Context, clientID kernel.BrowserClientID) (func(), error) {
	if backend == nil || backend.clientGates == nil {
		return nil, browser.ErrUnauthorized
	}
	return backend.clientGates.acquire(ctx, clientID)
}

func (gates *browserClientGates) acquire(ctx context.Context, clientID kernel.BrowserClientID) (func(), error) {
	if gates == nil {
		return nil, browser.ErrUnauthorized
	}
	gates.mu.Lock()
	if gates.gates == nil {
		gates.gates = make(map[kernel.BrowserClientID]*browserClientGate)
	}
	gate := gates.gates[clientID]
	if gate == nil {
		gate = &browserClientGate{slot: make(chan struct{}, 1)}
		gate.slot <- struct{}{}
		gates.gates[clientID] = gate
	}
	gate.users++
	gates.mu.Unlock()
	select {
	case <-gate.slot:
		return func() {
			gate.slot <- struct{}{}
			gates.release(clientID, gate)
		}, nil
	case <-ctx.Done():
		gates.release(clientID, gate)
		return nil, ctx.Err()
	}
}

func (gates *browserClientGates) release(clientID kernel.BrowserClientID, gate *browserClientGate) {
	gates.mu.Lock()
	gate.users--
	if gate.users == 0 && gates.gates[clientID] == gate {
		delete(gates.gates, clientID)
	}
	gates.mu.Unlock()
}

func (backend *browserBackend) randomIdentifier() ([kernel.IDBytes]byte, error) {
	backend.randomMu.Lock()
	defer backend.randomMu.Unlock()
	var result [kernel.IDBytes]byte
	if _, err := io.ReadFull(backend.random, result[:]); err != nil {
		return result, fmt.Errorf("browser random identity: %w", err)
	}
	for _, value := range result {
		if value != 0 {
			return result, nil
		}
	}
	return result, fmt.Errorf("browser random identity is zero")
}

func (backend *browserBackend) timestamp() (kernel.UnixMillis, error) {
	return kernel.NewUnixMillis(backend.now().UnixMilli())
}

func projectBrowserAuthentication(client kernel.BrowserClient) (browser.Authentication, error) {
	capabilities := browserprotocol.CapabilityObserve
	if client.CapabilityMask&^kernel.BrowserCapabilityKnownMask != 0 || !client.CapabilityMask.Has(kernel.BrowserCapabilityObserve) || client.RevokedAt != nil {
		return browser.Authentication{}, browser.ErrUnauthorized
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityPrivateHumanRequestDetail) {
		capabilities |= browserprotocol.CapabilityPrivateHumanRequestDetail
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityHumanActions) {
		capabilities |= browserprotocol.CapabilityHumanActions
	}
	if client.CapabilityMask.Has(kernel.BrowserCapabilityTerminalInput) {
		capabilities |= browserprotocol.CapabilityTerminalInput
	}
	var principal browser.Principal
	copy(principal.ClientID[:], client.ID.Bytes())
	return browser.Authentication{Principal: principal, Capabilities: capabilities}, nil
}

// projectPublicSnapshot is the one positive-allowlist conversion from the
// kernel public snapshot to the wire. Nothing private is reachable from here.
func projectPublicSnapshot(snapshot kernel.PublicSnapshot) (browserprotocol.StateSnapshot, error) {
	result := browserprotocol.StateSnapshot{
		Head:          decimalSequence(snapshot.Head),
		Factory:       projectFactory(snapshot.Factory),
		Projects:      make([]browserprotocol.ProjectItem, 0, len(snapshot.Projects)),
		Agents:        make([]browserprotocol.AgentItem, 0, len(snapshot.Agents)),
		Tasks:         make([]browserprotocol.TaskItem, 0, len(snapshot.Tasks)),
		HumanRequests: make([]browserprotocol.HumanRequestItem, 0, len(snapshot.HumanRequests)),
	}
	for _, item := range snapshot.Projects {
		result.Projects = append(result.Projects, projectProject(item))
	}
	for _, item := range snapshot.Agents {
		result.Agents = append(result.Agents, projectAgent(item))
	}
	for _, item := range snapshot.Tasks {
		result.Tasks = append(result.Tasks, projectTask(item))
	}
	for _, item := range snapshot.HumanRequests {
		projected, err := projectHumanRequest(item)
		if err != nil {
			return browserprotocol.StateSnapshot{}, err
		}
		result.HumanRequests = append(result.HumanRequests, projected)
	}
	return result, nil
}

func projectFactory(item kernel.FactorySummary) browserprotocol.FactoryItem {
	return browserprotocol.FactoryItem{DispatchEnabled: browserprotocol.Bool(item.DispatchEnabled), Capacity: item.Capacity, ActiveRuns: item.ActiveRuns, Revision: decimalRevision(item.Revision)}
}

func projectProject(item kernel.ProjectSummary) browserprotocol.ProjectItem {
	return browserprotocol.ProjectItem{ID: item.ID.String(), Name: item.Name, Revision: decimalRevision(item.Revision)}
}

func projectAgent(item kernel.AgentSummary) browserprotocol.AgentItem {
	return browserprotocol.AgentItem{ID: item.ID.String(), ProjectID: item.ProjectID.String(), Name: item.Name, Role: item.Role, Provider: item.Provider, Paused: browserprotocol.Bool(item.Paused), Model: item.Model, ReasoningEffort: item.ReasoningEffort, Revision: decimalRevision(item.Revision)}
}

func projectTask(item kernel.TaskSummary) browserprotocol.TaskItem {
	return browserprotocol.TaskItem{ID: item.ID.String(), ProjectID: item.ProjectID.String(), AssignedAgentID: item.AssignedAgentID.String(), Title: item.Title, Status: item.Status, Priority: item.Priority, Revision: decimalRevision(item.Revision)}
}

func projectHumanRequest(item kernel.HumanRequestProjection) (browserprotocol.HumanRequestItem, error) {
	if item.ReplyMaxBytes > math.MaxUint16 {
		return browserprotocol.HumanRequestItem{}, fmt.Errorf("human-request reply bound exceeds wire")
	}
	return browserprotocol.HumanRequestItem{
		ID: item.ID.String(), ProjectID: item.ProjectID.String(), AgentID: item.AgentID.String(), TaskID: item.TaskID.String(),
		CreatedAt: decimalMillis(item.CreatedAt), UpdatedAt: decimalMillis(item.UpdatedAt), Revision: decimalRevision(item.Revision),
		Kind: item.Kind.String(), Status: item.Status.String(), ReplyMaxBytes: uint16(item.ReplyMaxBytes),
		CanReply: browserprotocol.Bool(item.CanReply),
	}, nil
}

func decimalSequence(value kernel.EventSequence) browserprotocol.Decimal {
	return browserprotocol.Decimal(value.Int64())
}
func decimalRevision(value kernel.Revision) browserprotocol.Decimal {
	return browserprotocol.Decimal(value.Int64())
}
func decimalMillis(value kernel.UnixMillis) browserprotocol.Decimal {
	return browserprotocol.Decimal(value.Int64())
}

func mapBrowserError(err error) error {
	if err == nil {
		return nil
	}
	switch {
	case errors.Is(err, context.Canceled), errors.Is(err, context.DeadlineExceeded):
		// An owner-side effect that already reached a verdict keeps it. Its
		// cause often carries the deadline that produced it, but "the effect
		// may already have landed" is never retryable busyness: retrying would
		// attempt it a second time. remoteErrorCode fences OutcomeUnknownError
		// ahead of its own context arm for the same reason.
		if terminalEffectVerdict(err) {
			return err
		}
		// Otherwise the caller gave up, or its budget expired before anything
		// was attempted. That is retryable busyness, not a fault: the same
		// request converges when it is made again with a budget it fits in.
		return browser.ErrRateLimited
	case errors.Is(err, kernel.ErrUnauthorized):
		return browser.ErrUnauthorized
	case errors.Is(err, kernel.ErrNotFound):
		return browser.ErrNotFound
	case errors.Is(err, kernel.ErrRevisionConflict), errors.Is(err, kernel.ErrConflict), errors.Is(err, kernel.ErrInvalidValue):
		return browser.ErrStale
	case errors.Is(err, kernel.ErrSnapshotTooLarge):
		return browser.ErrTooLarge
	case errors.Is(err, kernel.ErrBusy):
		return browser.ErrRateLimited
	case errors.Is(err, ErrTerminalNotReady):
		// The owner has not yet consumed the runner's ready frame for a session
		// the Store already shows active. Retryable busyness, not an internal
		// failure: the client retries the same target and converges.
		return browser.ErrRateLimited
	default:
		return err
	}
}
