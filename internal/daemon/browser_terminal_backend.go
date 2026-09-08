package daemon

import (
	"context"
	"errors"
	"math"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

// ErrHumanRequestOwnerFence reports that the durable cancel committed but the
// live owner did not positively acknowledge the follow-up input fence. The
// cancellation is intentionally not retryable: its durable request/run transition
// remains authoritative while the owner is converged through its close path.
var ErrHumanRequestOwnerFence = errors.New("daemon: human request owner fence uncertain")

func (backend *browserBackend) AttachTerminal(ctx context.Context, request browser.TerminalAttachRequest) (browser.TerminalAttachment, error) {
	_, release, err := backend.authorizePrincipal(ctx, request.Principal, kernel.BrowserCapabilityObserve)
	if err != nil {
		return nil, err
	}
	defer release()
	runID, err := browserID(request.Request.RunID, kernel.RunIDFromBytes)
	if err != nil {
		return nil, browser.ErrStale
	}
	sessionID, err := browserID(request.Request.SessionID, kernel.TerminalSessionIDFromBytes)
	if err != nil {
		return nil, browser.ErrStale
	}
	after, err := browserSequence(request.Request.AfterSequence)
	if err != nil {
		return nil, browser.ErrStale
	}
	expectedRun, err := browserDecimal(request.Request.ExpectedRunRevision)
	if err != nil {
		return nil, browser.ErrStale
	}
	expectedSession, err := browserDecimal(request.Request.ExpectedSessionRevision)
	if err != nil {
		return nil, browser.ErrStale
	}
	attachment, err := backend.owner.AttachTerminal(ctx, runID, sessionID, expectedRun, expectedSession, after)
	if err != nil {
		return nil, mapBrowserError(err)
	}
	return attachment, nil
}

func (backend *browserBackend) AcquireTerminalLease(ctx context.Context, principal browser.Principal, request browserprotocol.TerminalLeaseAcquire) (browser.TerminalLeaseResult, error) {
	runID, sessionID, runRevision, sessionRevision, err := leaseRequest(request.RunID, request.SessionID, request.ExpectedRunRevision, request.ExpectedSessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, err
	}
	lease, err := backend.owner.terminalLeaseAcquire(ctx, principal, runID, sessionID, runRevision, sessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, mapBrowserError(err)
	}
	return projectLease("acquired", lease), nil
}

func (backend *browserBackend) RenewTerminalLease(ctx context.Context, principal browser.Principal, request browserprotocol.TerminalLeaseRenew) (browser.TerminalLeaseResult, error) {
	runID, sessionID, runRevision, sessionRevision, err := leaseRequest(request.RunID, request.SessionID, request.ExpectedRunRevision, request.ExpectedSessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, err
	}
	if request.Generation > math.MaxInt64 {
		return browser.TerminalLeaseResult{}, browser.ErrStale
	}
	lease, err := backend.owner.terminalLeaseRenew(ctx, principal, runID, sessionID, uint64(request.Generation), runRevision, sessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, mapBrowserError(err)
	}
	return projectLease("renewed", lease), nil
}

func (backend *browserBackend) ReleaseTerminalLease(ctx context.Context, principal browser.Principal, request browserprotocol.TerminalLeaseRelease) (browser.TerminalLeaseResult, error) {
	runID, sessionID, runRevision, sessionRevision, err := leaseRequest(request.RunID, request.SessionID, request.ExpectedRunRevision, request.ExpectedSessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, err
	}
	if request.Generation > math.MaxInt64 {
		return browser.TerminalLeaseResult{}, browser.ErrStale
	}
	lease, err := backend.owner.terminalLeaseRelease(ctx, principal, runID, sessionID, uint64(request.Generation), runRevision, sessionRevision)
	if err != nil {
		return browser.TerminalLeaseResult{}, mapBrowserError(err)
	}
	return projectLease("released", lease), nil
}

func (backend *browserBackend) ResizeTerminal(ctx context.Context, request browser.TerminalResizeRequest) error {
	runID, sessionID, runRevision, sessionRevision, err := leaseRequest(request.Request.RunID, request.Request.SessionID, request.Request.ExpectedRunRevision, request.Request.ExpectedSessionRevision)
	if err != nil {
		return err
	}
	if request.Request.Generation > math.MaxInt64 {
		return browser.ErrStale
	}
	err = backend.owner.terminalResize(ctx, request.Principal, runID, sessionID, uint64(request.Request.Generation), runRevision, sessionRevision, request.Request.Rows, request.Request.Cols)
	return mapBrowserError(err)
}

func (backend *browserBackend) InputTerminal(ctx context.Context, request browser.TerminalInputRequest) (uint32, error) {
	runID, err := browserID(request.RunID, kernel.RunIDFromBytes)
	if err != nil {
		return 0, browser.ErrStale
	}
	sessionID, err := browserID(request.SessionID, kernel.TerminalSessionIDFromBytes)
	if err != nil || request.Frame.SessionID != sessionIDBytes(sessionID) || request.Frame.LeaseGeneration == 0 || request.Frame.Sequence == 0 || request.Frame.Sequence > math.MaxInt64 {
		return 0, browser.ErrStale
	}
	runRevision, err := browserDecimal(request.RunRevision)
	if err != nil {
		return 0, browser.ErrStale
	}
	sessionRevision, err := browserDecimal(request.SessionRevision)
	if err != nil {
		return 0, browser.ErrStale
	}
	count, err := backend.owner.terminalInput(ctx, request.Principal, runID, sessionID, request.Frame.LeaseGeneration, request.Frame.Sequence, runRevision, sessionRevision, request.Frame.Payload)
	if err != nil {
		if errors.Is(err, ErrTerminalEffectPartial) {
			return count, errors.Join(browser.ErrTerminalPartial, err)
		}
		if errors.Is(err, ErrTerminalEffectUncertain) {
			return count, errors.Join(browser.ErrTerminalUncertain, err)
		}
	}
	return count, mapBrowserError(err)
}

func (backend *browserBackend) ReplyHumanRequest(ctx context.Context, principal browser.Principal, request browserprotocol.HumanRequestReply) (browserprotocol.HumanRequestReplyResult, error) {
	requestID, err := browserID(request.RequestID, kernel.HumanRequestIDFromBytes)
	if err != nil {
		return browserprotocol.HumanRequestReplyResult{}, browser.ErrStale
	}
	expected, err := browserDecimal(request.ExpectedRevision)
	if err != nil {
		return browserprotocol.HumanRequestReplyResult{}, browser.ErrStale
	}
	_, effectErr := backend.owner.humanReply(ctx, principal, requestID, expected, request.Reply)
	projection, err := backend.owner.humanReplyOutcome(requestID, effectErr)
	if err != nil {
		return browserprotocol.HumanRequestReplyResult{}, mapBrowserError(err)
	}
	status := "resolved"
	if projection.Status == kernel.HumanRequestDeliveryUnknown {
		status = "delivery_unknown"
	}
	return browserprotocol.HumanRequestReplyResult{RequestID: request.RequestID, Revision: decimalRevision(projection.Revision), Status: status}, nil
}

func (backend *browserBackend) CancelHumanRequestRun(ctx context.Context, principal browser.Principal, request browserprotocol.HumanRequestCancelRun) (browserprotocol.HumanRequestCancelRunResult, error) {
	requestID, err := browserID(request.RequestID, kernel.HumanRequestIDFromBytes)
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, browser.ErrStale
	}
	expectedRequest, err := browserDecimal(request.ExpectedRequestRevision)
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, browser.ErrStale
	}
	expectedRun, err := browserDecimal(request.ExpectedRunRevision)
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, browser.ErrStale
	}
	clientID, release, err := backend.authorizePrincipal(ctx, principal, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, err
	}
	defer release()
	at, err := backend.timestamp()
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, err
	}
	run, requestRow, err := backend.owner.cancelHumanRequestRun(ctx, clientID, requestID, expectedRequest, expectedRun, at)
	if err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, mapBrowserError(err)
	}
	return browserprotocol.HumanRequestCancelRunResult{RunID: run.ID.String(), RunRevision: decimalRevision(run.Revision), RequestID: requestRow.ID.String(), RequestRevision: decimalRevision(requestRow.Revision)}, nil
}

func (daemon *Daemon) cancelHumanRequestRun(ctx context.Context, clientID kernel.BrowserClientID, requestID kernel.HumanRequestID, expectedRequest, expectedRun kernel.Revision, at kernel.UnixMillis) (kernel.Run, kernel.HumanRequest, error) {
	if daemon == nil || daemon.store == nil {
		return kernel.Run{}, kernel.HumanRequest{}, kernel.ErrUnauthorized
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	run, request, err := daemon.store.CancelHumanRequestRun(ctx, clientID, requestID, expectedRequest, expectedRun, at)
	if err != nil {
		return kernel.Run{}, kernel.HumanRequest{}, err
	}
	// Durable revocation is authoritative. The owner-side generation fence is
	// checked after that commit; failure is visible but never turns cancellation
	// into a retry.
	daemon.attemptMu.Lock()
	attempt := daemon.attempts[run.ID]
	daemon.attemptMu.Unlock()
	if attempt != nil && terminalEffectsSupported {
		fenceCtx, cancel := context.WithTimeout(context.Background(), liveAttemptEffectLimit)
		fence := attempt.submitEffect(fenceCtx, terminalEffect{kind: terminalEffectRevokeCurrentBinding})
		cancel()
		if fenceErr := fence.effectError(-1); fenceErr != nil && !fence.terminalFence {
			// The transaction above already revoked the durable generation and
			// entered finalizing. Converge the owner without undoing that commit,
			// and preserve both the typed fence failure and any close uncertainty.
			return run, request, errors.Join(ErrHumanRequestOwnerFence, fenceErr, attempt.close())
		}
	}
	return run, request, nil
}

func (backend *browserBackend) authorizePrincipal(ctx context.Context, principal browser.Principal, capability kernel.BrowserCapabilityMask) (kernel.BrowserClientID, func(), error) {
	if backend == nil || backend.owner == nil || principal.ConnectionID == (browser.ConnectionID{}) {
		return kernel.BrowserClientID{}, nil, browser.ErrUnauthorized
	}
	clientID, release, client, err := backend.authorize(ctx, principal.ClientID, capability)
	if err != nil || client.ID != clientID {
		if release != nil {
			release()
		}
		return kernel.BrowserClientID{}, nil, browser.ErrUnauthorized
	}
	return clientID, release, nil
}

func leaseRequest(run, session string, expectedRun, expectedSession browserprotocol.Decimal) (kernel.RunID, kernel.TerminalSessionID, kernel.Revision, kernel.Revision, error) {
	runID, err := browserID(run, kernel.RunIDFromBytes)
	if err != nil {
		return kernel.RunID{}, kernel.TerminalSessionID{}, kernel.Revision{}, kernel.Revision{}, browser.ErrStale
	}
	sessionID, err := browserID(session, kernel.TerminalSessionIDFromBytes)
	if err != nil {
		return kernel.RunID{}, kernel.TerminalSessionID{}, kernel.Revision{}, kernel.Revision{}, browser.ErrStale
	}
	runRevision, err := browserDecimal(expectedRun)
	if err != nil {
		return kernel.RunID{}, kernel.TerminalSessionID{}, kernel.Revision{}, kernel.Revision{}, browser.ErrStale
	}
	sessionRevision, err := browserDecimal(expectedSession)
	if err != nil {
		return kernel.RunID{}, kernel.TerminalSessionID{}, kernel.Revision{}, kernel.Revision{}, browser.ErrStale
	}
	return runID, sessionID, runRevision, sessionRevision, nil
}

func projectLease(operation string, lease kernel.TerminalLease) browser.TerminalLeaseResult {
	expires := decimalMillis(lease.ExpiresAt)
	var expiry *browserprotocol.Decimal
	if operation != "released" {
		expiry = &expires
	}
	return browser.TerminalLeaseResult{Operation: operation, RunID: lease.RunID.String(), SessionID: lease.SessionID.String(), Generation: browserprotocol.Decimal(lease.Generation), ExpiresAtMS: expiry, LastInputSequence: browserprotocol.Decimal(lease.LastInputSequence), RunRevision: decimalRevision(lease.RunRevision), SessionRevision: decimalRevision(lease.SessionRevision)}
}

func browserDecimal(value browserprotocol.Decimal) (kernel.Revision, error) {
	if value < 1 || uint64(value) > math.MaxInt64 {
		return kernel.Revision{}, errors.New("invalid browser decimal")
	}
	return kernel.NewRevision(int64(value))
}

func browserSequence(value browserprotocol.Decimal) (uint64, error) {
	if value < 0 || uint64(value) > math.MaxInt64 {
		return 0, errors.New("invalid browser sequence")
	}
	return uint64(value), nil
}

func browserID[T any](value string, decode func([]byte) (T, error)) (T, error) {
	return decodeMust[T](value, decode)
}

func decodeMust[T any](value string, decode func([]byte) (T, error)) (T, error) {
	var zero T
	raw, err := parseID(value)
	if err != nil {
		return zero, err
	}
	return decode(raw)
}

func sessionIDBytes(id kernel.TerminalSessionID) [16]byte {
	var result [16]byte
	copy(result[:], id.Bytes())
	return result
}
