package browser

import (
	"context"
	"errors"
	"fmt"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

var (
	ErrUnauthorized = errors.New("browser: unauthorized")
	ErrNotFound     = errors.New("browser: not found")
	ErrStale        = errors.New("browser: stale")
	// ErrInvalidRequest is a member the domain refuses. Unlike ErrStale it is
	// not fixed by refetching: the client must send a different value.
	ErrInvalidRequest         = errors.New("browser: invalid request")
	ErrTooLarge               = errors.New("browser: too large")
	ErrRateLimited            = errors.New("browser: rate limited")
	ErrSubscriptionUnresolved = errors.New("browser: subscription cleanup unresolved")
	ErrTerminalPartial        = errors.New("browser: terminal input was partial")
	ErrTerminalUncertain      = errors.New("browser: terminal input outcome is uncertain")
)

// Identity is the daemon identity sent in HELLO. The boot ID changes on every
// daemon start; the daemon ID remains stable for one fresh Go runtime home.
type Identity struct {
	DaemonID [browserprotocol.DaemonIDSize]byte
	BootID   [browserprotocol.BootIDSize]byte
}

// ConnectionID is one transport-minted WebSocket generation. Its bytes stay
// private to this package: callers may retain and compare the value but cannot
// construct a nonzero identity or put it on the wire.
type ConnectionID struct {
	value [browserprotocol.ClientIDSize]byte
}

func (id ConnectionID) zero() bool { return id == (ConnectionID{}) }

const connectionIDRedaction = "[private browser connection]"

// Format ignores every verb, flag, width and precision so no diagnostic form
// can fall through to the private numeric byte representation.
func (ConnectionID) Format(state fmt.State, _ rune) {
	_, _ = state.Write([]byte(connectionIDRedaction))
}

// Principal contains durable daemon-minted client authority plus one private,
// transport-minted connection identity. Backend authentication results must
// leave ConnectionID zero; the transport installs it after proof succeeds and
// before any operation can run.
type Principal struct {
	ClientID     [browserprotocol.ClientIDSize]byte
	ConnectionID ConnectionID `json:"-"`
}

// Authentication is used only for the daemon-minted handshake result.
// Capabilities are negotiated once and are deliberately not cached on the
// connection as later authorization; Backend reloads authority per operation.
type Authentication struct {
	Principal    Principal
	Capabilities browserprotocol.Capabilities
}

type PairRequest struct {
	Identity
	ConnectionNonce [browserprotocol.NonceSize]byte
	Challenge       [browserprotocol.ChallengeSize]byte
	PublicKeySEC1   [browserprotocol.PublicKeySize]byte
	Signature       [browserprotocol.SignatureSize]byte
	Host            string
	Origin          string
}

type AuthRequest struct {
	Identity
	ConnectionNonce [browserprotocol.NonceSize]byte
	ClientID        [browserprotocol.ClientIDSize]byte
	Signature       [browserprotocol.SignatureSize]byte
	Host            string
	Origin          string
}

// StateUpdate is exactly one head-only invalidation. It carries no entity
// data: the client refetches a whole snapshot when it wants current state.
// Closing Updates is treated as an internal failure and forces reconnect.
type StateUpdate struct {
	Head browserprotocol.Decimal
}

// TerminalEvent is the small daemon-to-transport projection. It contains no
// runner identity or descriptor and is delivered from the daemon's existing
// bounded attachment queue.
type TerminalEvent struct {
	Kind       TerminalEventKind
	Accepted   bool
	Sequence   uint64
	Start      uint64
	End        uint64
	Floor      uint64
	Head       uint64
	ExitCode   int
	ExitSignal int
	Aborted    bool
	Payload    []byte
}

type TerminalEventKind uint8

const (
	TerminalEventAttached TerminalEventKind = iota + 1
	TerminalEventOutput
	TerminalEventReset
	TerminalEventPTYEOF
	TerminalEventExit
)

// TerminalAttachment exposes the daemon's receive-only queue directly. Close
// is synchronous: it does not return until the owner has joined the detach.
type TerminalAttachment interface {
	Events() <-chan TerminalEvent
	Close() error
}

type TerminalAttachRequest struct {
	Principal Principal
	Request   browserprotocol.TerminalAttach
}
type TerminalResizeRequest struct {
	Principal Principal
	Request   browserprotocol.TerminalResize
}
type TerminalInputRequest struct {
	Principal                    Principal
	RunID                        string
	SessionID                    string
	RunRevision, SessionRevision browserprotocol.Decimal
	Frame                        browserprotocol.TerminalFrame
}
type TerminalLeaseResult struct {
	Operation         string
	RunID             string
	SessionID         string
	Generation        browserprotocol.Decimal
	ExpiresAtMS       *browserprotocol.Decimal
	LastInputSequence browserprotocol.Decimal
	RunRevision       browserprotocol.Decimal
	SessionRevision   browserprotocol.Decimal
}

// StateSubscription is owned by one WebSocket connection. Cancel must be
// nonblocking. Updates, Done and Err are nonblocking accessors. Done must close
// after the producer exits, after which Err is stable. This lets the connection
// bound shutdown without abandoning a goroutine inside an uncooperative
// synchronous Close call. A subscription must reread the durable head after
// installing its watcher so a change landing in the snapshot-to-watch gap is
// still delivered.
type StateSubscription interface {
	Updates() <-chan StateUpdate
	Cancel()
	Done() <-chan struct{}
	Err() error
}

// Backend is the complete daemon boundary for this state-only slice. Every
// operation after authentication receives only the daemon-minted client ID;
// the implementation revalidates durable authority and canonical state.
// Every method must return promptly when its context is cancelled. The
// transport never wraps a noncooperative call in an orphanable goroutine.
type Backend interface {
	Identity(context.Context) (Identity, error)
	Pair(context.Context, PairRequest) (Authentication, error)
	Authenticate(context.Context, AuthRequest) (Authentication, error)
	StateSnapshot(context.Context, [browserprotocol.ClientIDSize]byte) (browserprotocol.StateSnapshot, error)
	HumanRequestDetail(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.HumanRequestDetailGet) (browserprotocol.HumanRequestDetail, error)
	TerminalTarget(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.TerminalTargetGet) (browserprotocol.TerminalTarget, error)
	WatchState(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.Decimal) (StateSubscription, error)
}

// PairBackend serves the first-party pair page: PairLink mints the page's
// one-shot launch link, or fails plainly.
type PairBackend interface {
	Backend
	PairLink(context.Context) (string, error)
}

// TaskBackend is the optional bounded operator-mutation half of browser v1.
// Keeping it separate preserves the state-only backend seam used by tests and
// non-production adapters.
type TaskBackend interface {
	Backend
	EnqueueTask(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.TaskEnqueue) (browserprotocol.TaskEnqueueResult, error)
	RemoteInvite(context.Context, [browserprotocol.ClientIDSize]byte) (browserprotocol.RemoteInviteResult, error)
}

// ConsoleBackend is the optional console half of browser v1: the bounded
// configuration and queue edits plus the on-demand project topology. It is a
// separate seam for the same reason TaskBackend is: the state-only backend
// used by bootstrap and tests must stay small.
type ConsoleBackend interface {
	Backend
	UpdateAgent(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.AgentUpdate) (browserprotocol.AgentUpdateResult, error)
	UpdateTask(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.TaskUpdate) (browserprotocol.TaskUpdateResult, error)
	Topology(context.Context, [browserprotocol.ClientIDSize]byte, browserprotocol.TopologyGet) (browserprotocol.Topology, error)
}

// TerminalBackend is the optional effect half of browser v1. Keeping it
// separate preserves the small state backend seam used by bootstrap/tests;
// production daemon backends implement both interfaces.
type TerminalBackend interface {
	Backend
	AttachTerminal(context.Context, TerminalAttachRequest) (TerminalAttachment, error)
	AcquireTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseAcquire) (TerminalLeaseResult, error)
	RenewTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseRenew) (TerminalLeaseResult, error)
	ReleaseTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseRelease) (TerminalLeaseResult, error)
	ResizeTerminal(context.Context, TerminalResizeRequest) error
	InputTerminal(context.Context, TerminalInputRequest) (uint32, error)
	ReplyHumanRequest(context.Context, Principal, browserprotocol.HumanRequestReply) (browserprotocol.HumanRequestReplyResult, error)
	CancelHumanRequestRun(context.Context, Principal, browserprotocol.HumanRequestCancelRun) (browserprotocol.HumanRequestCancelRunResult, error)
}
