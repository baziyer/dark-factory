package browser

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

const (
	backendCallLimit       = 3 * time.Second
	subscriptionCloseLimit = time.Second
)

// requestWindow is how long a request id is remembered. The budget is
// maxRequests per window rather than per connection, so a console left open
// for days keeps its socket; a repeated id inside the window is still refused.
const requestWindow = time.Minute

var (
	errBackendResult             = errors.New("browser: invalid backend result")
	errInvalidTerminalAttachment = errors.New("browser: invalid terminal attachment")
)

type incoming struct {
	kind websocket.MessageType
	data []byte
	err  error
}

type connection struct {
	server *Server
	ws     *websocket.Conn
	host   string
	origin string
	ctx    context.Context
	cancel context.CancelFunc
	frames chan incoming
	done   chan struct{}

	stopOnce   sync.Once
	cleanupErr error

	authenticated       bool
	principal           Principal
	pairing             bool
	authenticating      bool
	authenticatingID    [browserprotocol.ClientIDSize]byte
	seen                map[string]struct{}
	recent              []request
	walks               chan browserprotocol.ControlFrame
	walked              chan struct{}
	subscription        StateSubscription
	updates             <-chan StateUpdate
	subscriptionID      string
	subscriptionHead    browserprotocol.Decimal
	subscriptionHeadSet bool

	attachment        TerminalAttachment
	terminalEvents    <-chan TerminalEvent
	terminalAttachID  string
	terminalAttach    browserprotocol.TerminalAttach
	terminalAnnounced bool
	terminalAck       uint64
	terminalSent      uint64
	terminalPending   *TerminalEvent
	terminalAckTimer  *time.Timer
	terminalAckExpiry time.Time
}

type request struct {
	id string
	at time.Time
}

// admit spends one slot of the sliding window on id, forgetting ids older
// than the window first. Only the serve goroutine calls it.
func (current *connection) admit(id string) error {
	now := current.server.now()
	for len(current.recent) > 0 && now.Sub(current.recent[0].at) >= requestWindow {
		delete(current.seen, current.recent[0].id)
		current.recent = current.recent[1:]
	}
	if _, duplicate := current.seen[id]; duplicate {
		return ErrInvalidRequest
	}
	if len(current.recent) >= maxRequests {
		return ErrRateLimited
	}
	current.seen[id] = struct{}{}
	current.recent = append(current.recent, request{id: id, at: now})
	return nil
}

func (current *connection) stop() {
	if current == nil {
		return
	}
	current.stopOnce.Do(func() {
		current.cancel()
		_ = current.ws.CloseNow()
	})
}

func (current *connection) run() {
	readerDone := make(chan struct{})
	readerStarted := false
	defer func() {
		if err := current.closeTerminal(); err != nil {
			current.recordCleanup(err)
		}
		if err := current.closeSubscription(); err != nil {
			current.recordCleanup(err)
		}
		current.stop()
		if current.walked != nil {
			<-current.walked
		}
		if readerStarted {
			<-readerDone
		}
		current.server.unregister(current)
		close(current.done)
	}()

	identity, err := current.server.backend.Identity(current.ctx)
	if err != nil || !nonzero(identity.DaemonID[:]) || !nonzero(identity.BootID[:]) {
		return
	}
	nonce, err := randomNonce()
	if err != nil {
		return
	}
	hello, err := browserprotocol.EncodeHello(browserprotocol.Hello{
		DaemonID:        hex.EncodeToString(identity.DaemonID[:]),
		BootID:          hex.EncodeToString(identity.BootID[:]),
		ConnectionNonce: hex.EncodeToString(nonce[:]),
	})
	if err != nil || current.write(hello) != nil {
		return
	}
	readerStarted = true
	go current.read(readerDone)
	if !current.authenticate(identity, nonce) {
		return
	}
	current.serve()
}

func (current *connection) read(done chan<- struct{}) {
	defer close(done)
	for {
		kind, data, err := current.ws.Read(current.ctx)
		message := incoming{kind: kind, data: data, err: err}
		select {
		case current.frames <- message:
		case <-current.ctx.Done():
			return
		}
		if err != nil {
			return
		}
	}
}

func (current *connection) authenticate(identity Identity, nonce [browserprotocol.NonceSize]byte) bool {
	authContext, cancel := context.WithTimeout(current.ctx, authenticationLimit)
	defer cancel()
	select {
	case <-authContext.Done():
		current.sendError("", browserprotocol.ErrorUnauthorized, false)
		return false
	case <-current.ctx.Done():
		return false
	case message := <-current.frames:
		if message.err != nil || message.kind != websocket.MessageText {
			current.sendError("", browserprotocol.ErrorInvalidRequest, false)
			return false
		}
		frame, err := browserprotocol.DecodeClientControl(message.data)
		if err != nil {
			current.sendError("", browserprotocol.ErrorInvalidRequest, false)
			return false
		}
		if frame.Type != browserprotocol.TypePairProve && frame.Type != browserprotocol.TypeAuthProve {
			current.sendError("", browserprotocol.ErrorUnauthorized, false)
			return false
		}
		current.seen = map[string]struct{}{}
		_ = current.admit(frame.ID) // the first id of an empty window is always admitted
		result, requested, tracked, err := current.prove(authContext, identity, nonce, frame)
		accept := err == nil && authContext.Err() == nil && validateAuthentication(result) == nil
		if accept {
			result.Principal.ConnectionID, err = newConnectionID()
			accept = err == nil
		}
		registered := false
		if requested != nil {
			if tracked {
				registered = current.server.finishAuthentication(current, *requested, result, accept)
			}
		} else if tracked {
			registered = current.server.registerPair(current, result, accept)
		}
		if !registered {
			current.sendError(frame.ID, browserprotocol.ErrorUnauthorized, false)
			return false
		}
		var response []byte
		if frame.Type == browserprotocol.TypePairProve {
			response, err = browserprotocol.EncodePairResult(frame.ID, browserprotocol.PairResult{
				ClientID: hex.EncodeToString(result.Principal.ClientID[:]), Capabilities: result.Capabilities,
			})
		} else {
			response, err = browserprotocol.EncodeAuthResult(frame.ID, browserprotocol.AuthResult{
				ClientID: hex.EncodeToString(result.Principal.ClientID[:]), Capabilities: result.Capabilities,
			})
		}
		return err == nil && current.write(response) == nil
	}
}

func (current *connection) prove(ctx context.Context, identity Identity, nonce [browserprotocol.NonceSize]byte, frame browserprotocol.ControlFrame) (Authentication, *[browserprotocol.ClientIDSize]byte, bool, error) {
	switch body := frame.Body.(type) {
	case browserprotocol.PairProve:
		challenge, err := fixedBytes(body.Challenge, browserprotocol.ChallengeSize)
		if err != nil {
			return Authentication{}, nil, false, err
		}
		publicKey, err := fixedBytes(body.PublicKeySEC1, browserprotocol.PublicKeySize)
		if err != nil {
			return Authentication{}, nil, false, err
		}
		signature, err := fixedBytes(body.Signature, browserprotocol.SignatureSize)
		if err != nil {
			return Authentication{}, nil, false, err
		}
		var request PairRequest
		request.Identity = identity
		request.ConnectionNonce = nonce
		copy(request.Challenge[:], challenge)
		copy(request.PublicKeySEC1[:], publicKey)
		copy(request.Signature[:], signature)
		request.Host, request.Origin = current.host, current.origin
		if !current.server.beginPairing(current) {
			return Authentication{}, nil, false, ErrUnauthorized
		}
		result, err := current.server.backend.Pair(ctx, request)
		return result, nil, true, err
	case browserprotocol.AuthProve:
		clientID, err := fixedBytes(body.ClientID, browserprotocol.ClientIDSize)
		if err != nil {
			return Authentication{}, nil, false, err
		}
		signature, err := fixedBytes(body.Signature, browserprotocol.SignatureSize)
		if err != nil {
			return Authentication{}, nil, false, err
		}
		var request AuthRequest
		request.Identity = identity
		request.ConnectionNonce = nonce
		copy(request.ClientID[:], clientID)
		copy(request.Signature[:], signature)
		request.Host, request.Origin = current.host, current.origin
		if !current.server.beginAuthentication(current, request.ClientID) {
			return Authentication{}, &request.ClientID, false, ErrUnauthorized
		}
		result, err := current.server.backend.Authenticate(ctx, request)
		return result, &request.ClientID, true, err
	default:
		return Authentication{}, nil, false, ErrUnauthorized
	}
}

func (current *connection) serve() {
	for {
		var updates <-chan StateUpdate
		if current.subscription != nil {
			updates = current.updates
		}
		terminalEvents := current.terminalEvents
		var ackTimeout <-chan time.Time
		if current.terminalAckTimer != nil {
			ackTimeout = current.terminalAckTimer.C
		}
		select {
		case <-current.ctx.Done():
			return
		case update, ok := <-updates:
			subscriptionID := current.subscriptionID
			if !ok || current.sendUpdate(update) != nil {
				current.sendError(subscriptionID, browserprotocol.ErrorInternal, false)
				return
			}
		case <-ackTimeout:
			return
		case event, ok := <-terminalEvents:
			if !ok {
				if current.attachment != nil && current.attachment.ResetRequired() {
					// The daemon deliberately bounded this observer. Resume from the
					// browser-confirmed cursor; the runner remains authoritative and
					// will return its own reset if that cursor was pruned.
					if !current.sendTerminalEvent(TerminalEvent{Kind: TerminalEventReset, Floor: current.terminalAck, Head: current.terminalAck}) {
						return
					}
					continue
				}
				if err := current.closeTerminal(); err != nil {
					current.recordCleanup(err)
					return
				}
				continue
			}
			if !current.sendTerminalEvent(event) {
				return
			}
		case message := <-current.frames:
			if message.err != nil {
				return
			}
			if message.kind == websocket.MessageBinary {
				if !current.dispatchBinary(message.data) {
					return
				}
				continue
			}
			if message.kind != websocket.MessageText {
				current.sendError("", browserprotocol.ErrorInvalidRequest, false)
				return
			}
			frame, err := browserprotocol.DecodeClientControl(message.data)
			if err != nil && !errors.Is(err, browserprotocol.ErrUnsupported) {
				current.sendError("", browserprotocol.ErrorInvalidRequest, false)
				return
			}
			if frame.Type == browserprotocol.TypePairProve || frame.Type == browserprotocol.TypeAuthProve || frame.Type == browserprotocol.TypeError {
				current.sendError(frame.ID, browserprotocol.ErrorInvalidRequest, false)
				return
			}
			if frame.Type == browserprotocol.TypeTerminalAck {
				if !current.handleTerminalAck(frame.Body.(browserprotocol.TerminalAck)) {
					return
				}
				continue
			}
			if err := current.admit(frame.ID); err != nil {
				mapped := errorFrame(err)
				current.sendError(frame.ID, mapped.Code, mapped.Retryable)
				if errors.Is(err, ErrRateLimited) {
					// The window admits the client again shortly; only a
					// repeated id is a violation that ends the connection.
					continue
				}
				return
			}
			if !current.dispatch(frame) {
				return
			}
		}
	}
}

func (current *connection) dispatch(frame browserprotocol.ControlFrame) bool {
	ctx, cancel := context.WithTimeout(current.ctx, backendCallLimit)
	defer cancel()
	var payload []byte
	var err error
	switch body := frame.Body.(type) {
	case browserprotocol.StateGet:
		snapshot, snapshotErr := current.server.backend.StateSnapshot(ctx, current.principal.ClientID)
		if ctx.Err() != nil {
			err = ctx.Err()
			break
		}
		if snapshotErr != nil {
			err = snapshotErr
			break
		}
		encoded, encodeErr := browserprotocol.EncodeStateSnapshot(frame.ID, snapshot)
		if encodeErr != nil {
			// An oversized encoded snapshot is a finite too_large answer, not
			// a truncated one and not an internal fault.
			if errors.Is(encodeErr, browserprotocol.ErrOversized) {
				err = ErrTooLarge
				break
			}
			err = encodeErr
			break
		}
		if current.writeSnapshot(encoded) != nil {
			return false
		}
		return true
	case browserprotocol.HumanRequestDetailGet:
		detail, backendErr := current.server.backend.HumanRequestDetail(ctx, current.principal.ClientID, body)
		if ctx.Err() != nil {
			err = ctx.Err()
			break
		}
		if backendErr != nil {
			err = backendErr
			break
		}
		if detail.RequestID != body.RequestID || detail.Revision != body.ExpectedRevision {
			err = fmt.Errorf("backend returned mismatched human request detail")
			break
		}
		payload, err = browserprotocol.EncodeHumanRequestDetail(frame.ID, detail)
	case browserprotocol.TerminalTargetGet:
		target, backendErr := current.server.backend.TerminalTarget(ctx, current.principal.ClientID, body)
		if ctx.Err() != nil {
			err = ctx.Err()
			break
		}
		if backendErr != nil {
			err = backendErr
			break
		}
		if correlationErr := validateTerminalTargetCorrelation(body, target); correlationErr != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTerminalTarget(frame.ID, target)
		if err != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
	case browserprotocol.TaskHistoryGet:
		backend, ok := current.server.backend.(AgentControlBackend)
		if !ok {
			err = ErrUnauthorized
			break
		}
		result, backendErr := backend.TaskHistory(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.TaskID != body.TaskID {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTaskHistory(frame.ID, result)
	case browserprotocol.TaskListGet:
		backend, ok := current.server.backend.(TaskListBackend)
		if !ok {
			err = ErrUnauthorized
			break
		}
		result, backendErr := backend.TaskList(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.AgentID != body.AgentID {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTaskList(frame.ID, result)
	case browserprotocol.TaskDetailGet:
		backend, ok := current.server.backend.(AgentControlBackend)
		if !ok {
			err = ErrUnauthorized
			break
		}
		result, backendErr := backend.TaskDetail(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.TaskID != body.TaskID || result.Revision != body.ExpectedRevision {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTaskDetail(frame.ID, result)
	case browserprotocol.TaskEnqueue:
		if current.server.taskBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.taskBackend.EnqueueTask(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.TaskID != body.TaskID || result.AgentRevision != body.ExpectedAgentRevision || result.Revision == 0 {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTaskEnqueueResult(frame.ID, result)
	case browserprotocol.AgentUpdate:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.UpdateAgent(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.AgentID != body.AgentID || result.Revision <= body.ExpectedRevision {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeAgentUpdateResult(frame.ID, result)
	case browserprotocol.ProjectLimits:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.SetProjectLimits(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.ProjectID != body.ProjectID || result.Revision <= body.ExpectedRevision {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeProjectLimitsResult(frame.ID, result)
	case browserprotocol.TaskUpdate:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.UpdateTask(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.TaskID != body.TaskID || result.Revision <= body.ExpectedRevision {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTaskUpdateResult(frame.ID, result)
	case browserprotocol.TopologyGet, browserprotocol.RunPathsGet, browserprotocol.AgentControl, browserprotocol.HumanRequestReply, browserprotocol.HumanRequestCancelRun:
		// These may use their whole call budget. That must not hold up state
		// and terminal frames, so one worker answers them in arrival order,
		// each under its own budget from the moment it starts; the websocket
		// permits concurrent writes. The queue holds a window's budget, so only
		// a client the window would refuse anyway finds it full and retries.
		if current.walks == nil {
			current.walks = make(chan browserprotocol.ControlFrame, maxRequests)
			current.walked = make(chan struct{})
			go current.walk()
		}
		select {
		case current.walks <- frame:
		default:
			current.sendError(frame.ID, browserprotocol.ErrorRateLimited, true)
		}
		return true
	case browserprotocol.AccountsDiscover:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.DiscoverAccounts(ctx, current.principal.ClientID)
		if backendErr != nil {
			err = backendErr
			break
		}
		payload, err = browserprotocol.EncodeAccounts(frame.ID, result)
	case browserprotocol.AccountLink:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.LinkAccount(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		payload, err = browserprotocol.EncodeAccountLinkResult(frame.ID, result)
	case browserprotocol.AccountUpdate:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.consoleBackend.UpdateAccount(ctx, current.principal.ClientID, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.AccountID != body.AccountID || result.Revision != body.ExpectedRevision+1 {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeAccountUpdateResult(frame.ID, result)
	case browserprotocol.RemoteInvite:
		if current.server.taskBackend == nil {
			err = ErrUnauthorized
			break
		}
		invitation, backendErr := current.server.taskBackend.RemoteInvite(ctx, current.principal.ClientID)
		if backendErr != nil {
			err = backendErr
			break
		}
		payload, err = browserprotocol.EncodeRemoteInviteResult(frame.ID, invitation)
	case browserprotocol.TerminalAttach:
		if current.attachment != nil || current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		attachment, backendErr := current.server.terminalBackend.AttachTerminal(ctx, TerminalAttachRequest{Principal: current.principal, Request: body})
		if backendErr != nil {
			err = backendErr
			break
		}
		if attachment == nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		events := attachment.Events()
		if events == nil {
			invalidErr := errInvalidTerminalAttachment
			if closeErr := attachment.Close(); closeErr != nil {
				err = errors.Join(invalidErr, closeErr)
				current.recordCleanup(err)
			} else {
				err = invalidErr
			}
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		current.attachment, current.terminalEvents, current.terminalAttachID, current.terminalAttach = attachment, events, frame.ID, body
		current.terminalAck = uint64(body.AfterSequence)
		current.terminalSent = current.terminalAck
		return true
	case browserprotocol.TerminalLeaseAcquire:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.terminalBackend.AcquireTerminalLease(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if validationErr := validateLeaseResult("acquired", body.RunID, body.SessionID, 0, body.ExpectedRunRevision, body.ExpectedSessionRevision, result); validationErr != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTerminalLeaseResult(frame.ID, browserprotocol.TerminalLeaseResult(result))
	case browserprotocol.TerminalLeaseRenew:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.terminalBackend.RenewTerminalLease(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if validationErr := validateLeaseResult("renewed", body.RunID, body.SessionID, body.Generation, body.ExpectedRunRevision, body.ExpectedSessionRevision, result); validationErr != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTerminalLeaseResult(frame.ID, browserprotocol.TerminalLeaseResult(result))
	case browserprotocol.TerminalLeaseRelease:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.terminalBackend.ReleaseTerminalLease(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if validationErr := validateLeaseResult("released", body.RunID, body.SessionID, body.Generation, body.ExpectedRunRevision, body.ExpectedSessionRevision, result); validationErr != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInternal, false)
			return false
		}
		payload, err = browserprotocol.EncodeTerminalLeaseResult(frame.ID, browserprotocol.TerminalLeaseResult(result))
	case browserprotocol.TerminalResize:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		err = current.server.terminalBackend.ResizeTerminal(ctx, TerminalResizeRequest{Principal: current.principal, Request: body})
		if err == nil {
			payload, err = browserprotocol.EncodeTerminalResized(frame.ID, browserprotocol.TerminalResized{SessionID: body.SessionID, Generation: body.Generation, Rows: body.Rows, Cols: body.Cols})
		}
	case browserprotocol.TerminalDetach:
		if current.attachment == nil || body.SessionID != current.terminalAttach.SessionID {
			err = ErrStale
			break
		}
		err = current.closeTerminal()
		if err == nil {
			payload, err = browserprotocol.EncodeTerminalDetached(frame.ID, browserprotocol.TerminalDetached{SessionID: body.SessionID})
		}
	case browserprotocol.StateWatch:
		if current.subscription != nil {
			current.sendError(frame.ID, browserprotocol.ErrorInvalidRequest, false)
			return false
		}
		subscription, backendErr := current.server.backend.WatchState(ctx, current.principal.ClientID, body.AfterHead)
		if ctx.Err() != nil {
			err = current.discardSubscription(subscription, ctx.Err())
			break
		}
		if backendErr != nil {
			err = current.discardSubscription(subscription, backendErr)
			break
		}
		if subscription == nil {
			err = fmt.Errorf("invalid subscription")
			break
		}
		updates := subscription.Updates()
		if updates == nil {
			err = current.discardSubscription(subscription, fmt.Errorf("invalid subscription"))
			break
		}
		current.subscription = subscription
		current.updates = updates
		current.subscriptionID = frame.ID
		current.subscriptionHead = body.AfterHead
		current.subscriptionHeadSet = true
		return true
	case nil:
		// An unknown type still spends a request id and the budget, and is
		// refused by that id; the console degrades that feature alone.
		current.sendError(frame.ID, browserprotocol.ErrorUnsupported, false)
		return true
	default:
		current.sendError(frame.ID, browserprotocol.ErrorInvalidRequest, false)
		return false
	}
	if err != nil {
		mapped := errorFrame(err)
		current.sendError(frame.ID, mapped.Code, mapped.Retryable)
		return !errors.Is(err, ErrUnauthorized) && !errors.Is(err, ErrSubscriptionUnresolved)
	}
	if current.write(payload) != nil {
		return false
	}
	return true
}

// sendUpdate forwards one head-only invalidation. Heads are strictly
// increasing for the life of one subscription; anything else is a backend
// fault, not a recoverable client condition.
func (current *connection) sendUpdate(update StateUpdate) error {
	if update.Head == 0 || current.subscriptionHeadSet && update.Head <= current.subscriptionHead {
		return fmt.Errorf("invalid state change head")
	}
	payload, err := browserprotocol.EncodeStateChanged(current.subscriptionID, browserprotocol.StateChanged{Head: update.Head})
	if err == nil {
		err = current.write(payload)
	}
	if err == nil {
		current.subscriptionHead = update.Head
		current.subscriptionHeadSet = true
	}
	return err
}

func validateHumanReplyResult(request browserprotocol.HumanRequestReply, result browserprotocol.HumanRequestReplyResult) error {
	if result.RequestID != request.RequestID || (result.Status != "resolved" && result.Status != "delivery_unknown") {
		return fmt.Errorf("%w: human reply identity or status", errBackendResult)
	}
	if uint64(request.ExpectedRevision) > browserprotocol.MaxSQLiteInteger-2 || result.Revision != request.ExpectedRevision+2 {
		return fmt.Errorf("%w: human reply revision", errBackendResult)
	}
	return nil
}

func validateHumanCancelRunResult(request browserprotocol.HumanRequestCancelRun, result browserprotocol.HumanRequestCancelRunResult) error {
	if result.RequestID != request.RequestID {
		return fmt.Errorf("%w: human cancellation identity", errBackendResult)
	}
	if uint64(request.ExpectedRunRevision) > browserprotocol.MaxSQLiteInteger-1 || result.RunRevision != request.ExpectedRunRevision+1 {
		return fmt.Errorf("%w: human cancellation run revision", errBackendResult)
	}
	if uint64(request.ExpectedRequestRevision) > browserprotocol.MaxSQLiteInteger-1 || result.RequestRevision != request.ExpectedRequestRevision+1 {
		return fmt.Errorf("%w: human cancellation request revision", errBackendResult)
	}
	return nil
}

func validateTerminalTargetCorrelation(request browserprotocol.TerminalTargetGet, result browserprotocol.TerminalTarget) error {
	if result.AgentID != request.AgentID || result.AgentRevision != request.ExpectedAgentRevision || result.Head != request.ExpectedHead {
		return fmt.Errorf("%w: terminal target identity or observation", errBackendResult)
	}
	return nil
}

func validateLeaseResult(operation, runID, sessionID string, generation, expectedRun, expectedSession browserprotocol.Decimal, result TerminalLeaseResult) error {
	if result.Operation != operation || result.RunID != runID || result.SessionID != sessionID || result.Generation == 0 || uint64(result.Generation) > browserprotocol.MaxSQLiteInteger || uint64(result.LastInputSequence) > browserprotocol.MaxSQLiteInteger || uint64(result.RunRevision) > browserprotocol.MaxSQLiteInteger || uint64(result.SessionRevision) > browserprotocol.MaxSQLiteInteger || result.RunRevision != expectedRun || result.SessionRevision != expectedSession {
		return fmt.Errorf("%w: terminal lease identity or revision", errBackendResult)
	}
	switch operation {
	case "acquired":
		if result.LastInputSequence != 0 {
			return fmt.Errorf("%w: acquired terminal lease sequence", errBackendResult)
		}
	case "renewed":
		if generation == 0 || result.Generation != generation {
			return fmt.Errorf("%w: terminal lease generation", errBackendResult)
		}
	case "released":
		// Store clears the lease and increments its generation in one
		// transaction. The post-release generation is therefore the exact
		// fence for all effects issued under the released generation.
		if generation == 0 || uint64(generation) >= browserprotocol.MaxSQLiteInteger || result.Generation != generation+1 {
			return fmt.Errorf("%w: terminal lease generation", errBackendResult)
		}
		if result.LastInputSequence != 0 {
			return fmt.Errorf("%w: released terminal lease sequence", errBackendResult)
		}
	default:
		return fmt.Errorf("%w: terminal lease operation", errBackendResult)
	}
	if operation == "released" {
		if result.ExpiresAtMS != nil {
			return fmt.Errorf("%w: released terminal lease expiry", errBackendResult)
		}
	} else if result.ExpiresAtMS == nil || *result.ExpiresAtMS == 0 || uint64(*result.ExpiresAtMS) > browserprotocol.MaxSQLiteInteger {
		return fmt.Errorf("%w: active terminal lease expiry", errBackendResult)
	}
	return nil
}

func (current *connection) closeSubscription() error {
	if current.subscription == nil {
		return nil
	}
	subscription := current.subscription
	current.subscription = nil
	current.updates = nil
	current.subscriptionID = ""
	current.subscriptionHead = 0
	current.subscriptionHeadSet = false
	return stopSubscription(subscription)
}

func (current *connection) closeTerminal() error {
	if current.attachment == nil {
		return nil
	}
	attachment := current.attachment
	if err := attachment.Close(); err != nil {
		return err
	}
	current.clearTerminal()
	return nil
}

func (current *connection) clearTerminal() {
	if current.terminalAckTimer != nil {
		current.terminalAckTimer.Stop()
		current.terminalAckTimer = nil
	}
	current.terminalAckExpiry = time.Time{}
	current.attachment = nil
	current.terminalEvents = nil
	current.terminalAttachID = ""
	current.terminalAttach = browserprotocol.TerminalAttach{}
	current.terminalAnnounced = false
	current.terminalPending = nil
	current.terminalAck, current.terminalSent = 0, 0
}

func (current *connection) armTerminalAckTimer() {
	if current.terminalAckTimer == nil {
		current.terminalAckTimer = time.NewTimer(current.server.terminalAckTimeout)
		current.terminalAckExpiry = time.Now().Add(current.server.terminalAckTimeout)
	}
}

func (current *connection) handleTerminalAck(ack browserprotocol.TerminalAck) bool {
	if current.attachment == nil || ack.SessionID != current.terminalAttach.SessionID || uint64(ack.NextSequence) <= current.terminalAck || uint64(ack.NextSequence) > current.terminalSent {
		return false
	}
	if current.terminalAckTimer != nil && !time.Now().Before(current.terminalAckExpiry) {
		return false
	}
	current.terminalAck = uint64(ack.NextSequence)
	if current.terminalAck == current.terminalSent && current.terminalAckTimer != nil {
		current.terminalAckTimer.Stop()
		current.terminalAckTimer = nil
		current.terminalAckExpiry = time.Time{}
	} else if current.terminalAckTimer != nil {
		current.terminalAckTimer.Reset(current.server.terminalAckTimeout)
		current.terminalAckExpiry = time.Now().Add(current.server.terminalAckTimeout)
	}
	if current.terminalPending != nil && current.terminalPendingReady(*current.terminalPending) {
		pending := *current.terminalPending
		current.terminalPending = nil
		current.terminalEvents = current.attachment.Events()
		return current.sendTerminalEvent(pending)
	}
	if current.terminalPending == nil {
		current.terminalEvents = current.attachment.Events()
	}
	return true
}

func (current *connection) terminalPendingReady(event TerminalEvent) bool {
	switch event.Kind {
	case TerminalEventOutput:
		return event.End >= current.terminalAck && event.End-current.terminalAck <= browserprotocol.MaxTerminalUnackedBytes
	case TerminalEventReset, TerminalEventExit:
		return current.terminalAck == current.terminalSent
	default:
		return false
	}
}

func (current *connection) holdTerminalEvent(event TerminalEvent) bool {
	if current.terminalPending != nil {
		return false
	}
	current.terminalPending = &event
	current.terminalEvents = nil
	return true
}

func (current *connection) sendTerminalEvent(event TerminalEvent) bool {
	if current.attachment == nil {
		return false
	}
	switch event.Kind {
	case TerminalEventAttached:
		// The runner echoes the cursor it actually accepted. Use that echoed
		// sequence as the credit origin; request bytes are not authority after
		// the daemon/runner attach gate has run.
		current.terminalAck = event.Sequence
		current.terminalSent = event.Sequence
		if !event.Accepted {
			payload, err := browserprotocol.EncodeTerminalReset(current.terminalAttachID, browserprotocol.TerminalReset{SessionID: current.terminalAttach.SessionID, Floor: browserprotocol.Decimal(event.Floor), Head: browserprotocol.Decimal(event.Head)})
			if err != nil || current.write(payload) != nil {
				return false
			}
			return current.closeTerminal() == nil
		}
		payload, err := browserprotocol.EncodeTerminalAttached(current.terminalAttachID, browserprotocol.TerminalAttached{SessionID: current.terminalAttach.SessionID, Floor: browserprotocol.Decimal(event.Floor), Head: browserprotocol.Decimal(event.Head), AcknowledgedSequence: browserprotocol.Decimal(event.Sequence), MaxUnackedBytes: browserprotocol.MaxTerminalUnackedBytes})
		if err != nil || current.write(payload) != nil {
			return false
		}
		current.terminalAnnounced = true
		return true
	case TerminalEventOutput:
		if !current.terminalAnnounced {
			return false
		}
		if event.End <= event.Start || event.End-event.Start != uint64(len(event.Payload)) {
			return false
		}
		if event.End <= current.terminalAck {
			return true
		}
		if event.Start != current.terminalSent {
			return false
		}
		if event.End-current.terminalAck > browserprotocol.MaxTerminalUnackedBytes {
			return current.holdTerminalEvent(event)
		}
		payload, err := browserprotocol.EncodeTerminalOutput(fixedID(current.terminalAttach.SessionID), event.Start, event.Payload)
		if err != nil || current.writeBinary(payload) != nil {
			return false
		}
		current.terminalSent = event.End
		current.armTerminalAckTimer()
		return true
	case TerminalEventReset:
		if current.terminalAck != current.terminalSent {
			return current.holdTerminalEvent(event)
		}
		payload, err := browserprotocol.EncodeTerminalReset(current.terminalAttachID, browserprotocol.TerminalReset{SessionID: current.terminalAttach.SessionID, Floor: browserprotocol.Decimal(event.Floor), Head: browserprotocol.Decimal(event.Head)})
		if err != nil || current.write(payload) != nil {
			return false
		}
		return current.closeTerminal() == nil
	case TerminalEventPTYEOF:
		if !current.terminalAnnounced {
			return false
		}
		payload, err := browserprotocol.EncodeTerminalEOF(current.terminalAttachID, browserprotocol.TerminalEOF{SessionID: current.terminalAttach.SessionID})
		return err == nil && current.write(payload) == nil
	case TerminalEventExit:
		if current.terminalAck != current.terminalSent {
			return current.holdTerminalEvent(event)
		}
		if !current.terminalAnnounced {
			return false
		}
		payload, err := browserprotocol.EncodeTerminalExit(current.terminalAttachID, browserprotocol.TerminalExit{SessionID: current.terminalAttach.SessionID, ExitCode: int64(event.ExitCode), ExitSignal: int64(event.ExitSignal), Aborted: event.Aborted})
		if err != nil || current.write(payload) != nil {
			return false
		}
		return current.closeTerminal() == nil
	default:
		return false
	}
}

func (current *connection) dispatchBinary(data []byte) bool {
	frame, err := browserprotocol.DecodeTerminalFrame(data)
	if err != nil || frame.Opcode != browserprotocol.TerminalInputOpcode || current.attachment == nil || frame.SessionID != fixedID(current.terminalAttach.SessionID) {
		current.sendError("", browserprotocol.ErrorInvalidRequest, false)
		return false
	}
	if current.server.terminalBackend == nil {
		return false
	}
	ctx, cancel := context.WithTimeout(current.ctx, backendCallLimit)
	count, effectErr := current.server.terminalBackend.InputTerminal(ctx, TerminalInputRequest{Principal: current.principal, RunID: current.terminalAttach.RunID, SessionID: current.terminalAttach.SessionID, RunRevision: current.terminalAttach.ExpectedRunRevision, SessionRevision: current.terminalAttach.ExpectedSessionRevision, Frame: frame})
	cancel()
	status := "accepted"
	if effectErr != nil {
		status = "rejected"
		if errors.Is(effectErr, ErrTerminalPartial) {
			status = "partial"
		} else if errors.Is(effectErr, ErrTerminalUncertain) {
			status = "uncertain"
		}
	}
	if count > uint32(len(frame.Payload)) {
		return false
	}
	payload, encodeErr := browserprotocol.EncodeTerminalInputResult(current.terminalAttachID, browserprotocol.TerminalInputResult{SessionID: current.terminalAttach.SessionID, Generation: browserprotocol.Decimal(frame.LeaseGeneration), Sequence: browserprotocol.Decimal(frame.Sequence), Status: status, AcceptedBytes: browserprotocol.Decimal(count)})
	if encodeErr != nil || current.write(payload) != nil {
		return false
	}
	return true
}

func fixedID(encoded string) [16]byte {
	var result [16]byte
	raw, _ := hex.DecodeString(encoded)
	copy(result[:], raw)
	return result
}

func (current *connection) discardSubscription(subscription StateSubscription, cause error) error {
	if subscription == nil {
		return cause
	}
	if err := stopSubscription(subscription); err != nil {
		current.recordCleanup(err)
		return errors.Join(cause, err)
	}
	return cause
}

func (current *connection) recordCleanup(err error) {
	current.cleanupErr = errors.Join(current.cleanupErr, err)
	current.server.recordCleanup(err)
}

func stopSubscription(subscription StateSubscription) error {
	if subscription == nil {
		return ErrSubscriptionUnresolved
	}
	subscription.Cancel()
	done := subscription.Done()
	if done == nil {
		return ErrSubscriptionUnresolved
	}
	timer := time.NewTimer(subscriptionCloseLimit)
	defer timer.Stop()
	select {
	case <-done:
		if err := subscription.Err(); err != nil {
			return fmt.Errorf("%w: %v", ErrSubscriptionUnresolved, err)
		}
		return nil
	case <-timer.C:
		return ErrSubscriptionUnresolved
	}
}

// walk answers the serialized slow request queue until the connection ends;
// cleanup in run waits for it, so a request in flight is joined, never leaked.
func (current *connection) walk() {
	defer close(current.walked)
	for current.ctx.Err() == nil {
		select {
		case <-current.ctx.Done():
		case frame := <-current.walks:
			current.observe(frame)
		}
	}
}

// observe answers the slow request queue off the serve goroutine. It reads
// only members that are fixed once authenticated, and it ends the connection
// the same way dispatch would: on an unauthorized refusal, a backend result
// that does not match the request, or a failed write.
func (current *connection) observe(frame browserprotocol.ControlFrame) {
	ctx, cancel := context.WithTimeout(current.ctx, backendCallLimit)
	defer cancel()
	var payload []byte
	var err error
	write := current.write
	switch body := frame.Body.(type) {
	case browserprotocol.TopologyGet:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		var result browserprotocol.Topology
		if result, err = current.server.consoleBackend.Topology(ctx, current.principal.ClientID, body); err != nil {
			break
		}
		if result.ProjectID != body.ProjectID {
			err = errBackendResult
			break
		}
		// Topology shares the snapshot byte bound, so an oversized one is the
		// same finite too_large answer a snapshot gives.
		write = current.writeSnapshot
		if payload, err = browserprotocol.EncodeTopology(frame.ID, result); errors.Is(err, browserprotocol.ErrOversized) {
			err = ErrTooLarge
		}
	case browserprotocol.RunPathsGet:
		if current.server.consoleBackend == nil {
			err = ErrUnauthorized
			break
		}
		var result browserprotocol.RunPaths
		if result, err = current.server.consoleBackend.RunPaths(ctx, current.principal.ClientID, body); err != nil {
			break
		}
		if result.AgentID != body.AgentID {
			err = errBackendResult
			break
		}
		payload, err = browserprotocol.EncodeRunPaths(frame.ID, result)
	case browserprotocol.AgentControl:
		backend, ok := current.server.backend.(AgentControlBackend)
		if !ok {
			err = ErrUnauthorized
			break
		}
		result, backendErr := backend.ControlAgent(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if result.OperationID != body.OperationID || result.TaskID != body.TaskID || result.RunID != body.RunID || result.SuccessorTaskID != body.SuccessorTaskID {
			err = errBackendResult
			break
		}
		payload, err = browserprotocol.EncodeAgentControlResult(frame.ID, result)
	case browserprotocol.HumanRequestReply:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.terminalBackend.ReplyHumanRequest(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if validationErr := validateHumanReplyResult(body, result); validationErr != nil {
			err = validationErr
			break
		}
		payload, err = browserprotocol.EncodeHumanRequestReplyResult(frame.ID, result)
	case browserprotocol.HumanRequestCancelRun:
		if current.server.terminalBackend == nil {
			err = ErrUnauthorized
			break
		}
		result, backendErr := current.server.terminalBackend.CancelHumanRequestRun(ctx, current.principal, body)
		if backendErr != nil {
			err = backendErr
			break
		}
		if validationErr := validateHumanCancelRunResult(body, result); validationErr != nil {
			err = validationErr
			break
		}
		payload, err = browserprotocol.EncodeHumanRequestCancelRunResult(frame.ID, result)
	}
	if err != nil {
		mapped := errorFrame(err)
		current.sendError(frame.ID, mapped.Code, mapped.Retryable)
		if errors.Is(err, ErrUnauthorized) || errors.Is(err, errBackendResult) {
			current.stop()
		}
		return
	}
	if write(payload) != nil {
		current.stop()
	}
}

func (current *connection) sendError(id string, code browserprotocol.ErrorCode, retryable bool) {
	payload, err := browserprotocol.EncodeError(id, browserprotocol.Error{Code: code, Retryable: retryable})
	if err == nil {
		_ = current.write(payload)
	}
}

func (current *connection) write(payload []byte) error {
	if len(payload) == 0 || len(payload) > browserprotocol.MaxControlBytes {
		return fmt.Errorf("invalid outbound control frame")
	}
	ctx, cancel := context.WithTimeout(current.ctx, writeLimit)
	defer cancel()
	return current.ws.Write(ctx, websocket.MessageText, payload)
}

// writeSnapshot is the only outbound path allowed past MaxControlBytes, and
// STATE_SNAPSHOT and TOPOLOGY are its only frames. Every other frame in either
// direction stays inside the 64 KiB control bound.
func (current *connection) writeSnapshot(payload []byte) error {
	if len(payload) == 0 || len(payload) > browserprotocol.MaxSnapshotBytes {
		return fmt.Errorf("invalid outbound snapshot frame")
	}
	ctx, cancel := context.WithTimeout(current.ctx, writeLimit)
	defer cancel()
	return current.ws.Write(ctx, websocket.MessageText, payload)
}

func (current *connection) writeBinary(payload []byte) error {
	if len(payload) < browserprotocol.TerminalHeaderSize || len(payload) > browserprotocol.TerminalHeaderSize+browserprotocol.MaxTerminalPayload {
		return fmt.Errorf("invalid outbound terminal frame")
	}
	ctx, cancel := context.WithTimeout(current.ctx, writeLimit)
	defer cancel()
	return current.ws.Write(ctx, websocket.MessageBinary, payload)
}

func fixedBytes(encoded string, size int) ([]byte, error) {
	decoded, err := hex.DecodeString(encoded)
	if err != nil || len(decoded) != size {
		return nil, fmt.Errorf("invalid fixed hex")
	}
	return decoded, nil
}
