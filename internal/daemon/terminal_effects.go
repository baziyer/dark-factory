package daemon

import (
	"context"
	"crypto/rand"
	"errors"
	"fmt"
	"io"
	"math"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const terminalDimensionLimit = 4096

var (
	ErrTerminalEffectsUnsupported = errors.New("daemon: terminal effects are unsupported on this platform")
	ErrTerminalEffectRejected     = errors.New("daemon: terminal effect was rejected")
	ErrTerminalEffectPartial      = errors.New("daemon: terminal effect was partial")
	ErrTerminalEffectUncertain    = errors.New("daemon: terminal effect outcome is uncertain")
)

// terminalEffectVerdict reports whether an owner already decided what happened
// to an effect. Such an error must keep its own outcome: nothing downstream may
// relabel it as a condition worth retrying.
func terminalEffectVerdict(err error) bool {
	var unknown *kernel.OutcomeUnknownError
	return errors.As(err, &unknown) || errors.Is(err, ErrTerminalEffectUncertain) ||
		errors.Is(err, ErrTerminalEffectPartial) || errors.Is(err, ErrTerminalEffectRejected)
}

type terminalEffectKind uint8

const (
	terminalEffectCheck terminalEffectKind = iota + 1
	terminalEffectRenew
	terminalEffectInstall
	terminalEffectRevoke
	terminalEffectInput
	terminalEffectResize
	terminalEffectHumanReply
	terminalEffectRevokeClient
	terminalEffectRevokeCurrentBinding
)

type terminalBinding struct {
	client     kernel.BrowserClientID
	connection browser.ConnectionID
	generation uint64
}

type terminalLeaseRenewal func(context.Context, kernel.BrowserClientID, uint64, kernel.Revision, kernel.Revision, kernel.UnixMillis) (kernel.TerminalLease, error)

type terminalEffect struct {
	kind            terminalEffectKind
	client          kernel.BrowserClientID
	connection      browser.ConnectionID
	generation      uint64
	sequence        uint64
	rows            uint16
	cols            uint16
	payload         []byte
	submit          bool
	requireBinding  bool
	expectedRun     kernel.Revision
	expectedSession kernel.Revision
}

type terminalEffectResult struct {
	status        runner.TerminalResultStatus
	count         uint32
	lease         kernel.TerminalLease
	terminalFence bool
	err           error
}

func uncertainTerminalEffect(cause error) terminalEffectResult {
	return terminalEffectResult{status: runner.TerminalResultUncertain, err: cause}
}

func (result terminalEffectResult) effectError(expectedBytes int) error {
	if result.status == runner.TerminalResultOK && (expectedBytes < 0 || result.count == uint32(expectedBytes)) && !result.terminalFence && result.err == nil {
		return nil
	}
	if result.status == "" {
		if result.err != nil {
			return result.err
		}
		return ErrTerminalEffectUncertain
	}
	var kind error
	switch result.status {
	case runner.TerminalResultRejected:
		kind = ErrTerminalEffectRejected
	case runner.TerminalResultPartial:
		kind = ErrTerminalEffectPartial
	case runner.TerminalResultOK, runner.TerminalResultUncertain:
		kind = ErrTerminalEffectUncertain
	default:
		kind = ErrTerminalEffectUncertain
	}
	return errors.Join(kind, result.err)
}

func sameTerminalBinding(binding terminalBinding, client kernel.BrowserClientID, connection browser.ConnectionID, generation uint64) bool {
	return binding.client == client && binding.connection == connection && binding.generation == generation
}

func (daemon *Daemon) terminalLeaseAcquire(ctx context.Context, principal browser.Principal, runID kernel.RunID, sessionID kernel.TerminalSessionID, expectedRun, expectedSession kernel.Revision) (kernel.TerminalLease, error) {
	if ctx == nil || runID == (kernel.RunID{}) || sessionID == (kernel.TerminalSessionID{}) || expectedRun.Int64() < 1 || expectedSession.Int64() < 1 {
		return kernel.TerminalLease{}, fmt.Errorf("%w: invalid terminal lease acquisition", kernel.ErrInvalidValue)
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityTerminalInput)
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return kernel.TerminalLease{}, ErrTerminalEffectsUnsupported
	}
	attempt, err := daemon.liveTerminalAttempt(runID, sessionID)
	if err != nil {
		return kernel.TerminalLease{}, err
	}

	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	at, err := daemon.timestamp()
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	lease, err := daemon.store.AcquireTerminalLease(ctx, runID, sessionID, clientID, expectedRun, expectedSession, at)
	if err != nil {
		if terminalStoreOutcomeUnknown(err) {
			_ = attempt.close()
		}
		return kernel.TerminalLease{}, err
	}
	result := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectInstall, client: clientID, connection: principal.ConnectionID, generation: lease.Generation,
	})
	if effectErr := result.effectError(-1); effectErr == nil {
		return lease, nil
	} else {
		return kernel.TerminalLease{}, daemon.revokeFailedLease(attempt, lease, effectErr)
	}
}

func (daemon *Daemon) terminalLeaseRenew(ctx context.Context, principal browser.Principal, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation uint64, expectedRun, expectedSession kernel.Revision) (kernel.TerminalLease, error) {
	if err := validateLeaseLocator(runID, sessionID, generation, expectedRun, expectedSession); err != nil {
		return kernel.TerminalLease{}, err
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityTerminalInput)
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return kernel.TerminalLease{}, ErrTerminalEffectsUnsupported
	}
	attempt, err := daemon.liveTerminalAttempt(runID, sessionID)
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	result := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectRenew, client: clientID, connection: principal.ConnectionID, generation: generation,
		expectedRun: expectedRun, expectedSession: expectedSession,
	})
	effectErr := result.effectError(-1)
	if effectErr == nil {
		return result.lease, nil
	}
	// A lost Store response, or an impossible post-commit owner mismatch,
	// cannot preserve writable authority. Revoke the exact generation without
	// retrying the renewal itself.
	if terminalStoreOutcomeUnknown(result.err) || result.lease != (kernel.TerminalLease{}) {
		cleanupErr := daemon.revokeAmbiguousRenewal(attempt, clientID, principal.ConnectionID, runID, sessionID, generation, expectedRun, expectedSession)
		return kernel.TerminalLease{}, errors.Join(ErrTerminalEffectUncertain, effectErr, cleanupErr)
	}
	return kernel.TerminalLease{}, effectErr
}

// terminalLeaseRelease shares operationMu with finalization. It either clears
// the live lease first while the run is still running, or observes the lease
// already atomically cleared by finalization and fails without a runner effect.
func (daemon *Daemon) terminalLeaseRelease(ctx context.Context, principal browser.Principal, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation uint64, expectedRun, expectedSession kernel.Revision) (kernel.TerminalLease, error) {
	if err := validateLeaseLocator(runID, sessionID, generation, expectedRun, expectedSession); err != nil {
		return kernel.TerminalLease{}, err
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityTerminalInput)
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return kernel.TerminalLease{}, ErrTerminalEffectsUnsupported
	}
	attempt, err := daemon.liveTerminalAttempt(runID, sessionID)
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	checked := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectCheck, client: clientID, connection: principal.ConnectionID, generation: generation,
	})
	if err := checked.effectError(-1); err != nil {
		// A runner that refuses the check does not hold this binding any
		// more: the provider exited, the generation was fenced, or another
		// connection holds it. Nothing to give up, so at the browser boundary
		// this is stale, not an internal fault (#542).
		if errors.Is(err, ErrTerminalEffectRejected) {
			err = errors.Join(kernel.ErrRevisionConflict, err)
		}
		return kernel.TerminalLease{}, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return kernel.TerminalLease{}, err
	}
	lease, err := daemon.store.ReleaseTerminalLease(ctx, runID, sessionID, clientID, generation, expectedRun, expectedSession, at)
	if err != nil {
		if terminalStoreOutcomeUnknown(err) {
			fenceErr := daemon.revokeRunnerGeneration(attempt, clientID, principal.ConnectionID, generation+1, true, false)
			return kernel.TerminalLease{}, errors.Join(err, fenceErr)
		}
		return kernel.TerminalLease{}, err
	}
	if err := daemon.revokeRunnerGeneration(attempt, clientID, principal.ConnectionID, lease.Generation, true, false); err != nil {
		return lease, err
	}
	return lease, nil
}

func (daemon *Daemon) terminalInput(ctx context.Context, principal browser.Principal, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation, sequence uint64, expectedRun, expectedSession kernel.Revision, payload []byte) (uint32, error) {
	if err := validateLeaseLocator(runID, sessionID, generation, expectedRun, expectedSession); err != nil || sequence == 0 || sequence > math.MaxInt64 || len(payload) == 0 || len(payload) > terminalPayloadCap {
		if err != nil {
			return 0, err
		}
		return 0, fmt.Errorf("%w: invalid terminal input", kernel.ErrInvalidValue)
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityTerminalInput)
	if err != nil {
		return 0, err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return 0, ErrTerminalEffectsUnsupported
	}
	attempt, err := daemon.liveTerminalAttempt(runID, sessionID)
	if err != nil {
		return 0, err
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	checked := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectCheck, client: clientID, connection: principal.ConnectionID, generation: generation,
	})
	if err := checked.effectError(-1); err != nil {
		return 0, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return 0, err
	}
	_, err = daemon.store.ReserveTerminalInputSequence(ctx, runID, sessionID, clientID, generation, sequence, expectedRun, expectedSession, at)
	if err != nil {
		if terminalStoreOutcomeUnknown(err) {
			cleanupErr := daemon.revokeReservedInput(attempt, clientID, principal.ConnectionID, runID, sessionID, generation, sequence, expectedRun, expectedSession)
			return 0, errors.Join(err, cleanupErr)
		}
		return 0, err
	}
	result := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectInput, client: clientID, connection: principal.ConnectionID,
		generation: generation, sequence: sequence, payload: append([]byte(nil), payload...),
	})
	effectErr := result.effectError(len(payload))
	if effectErr == nil {
		return result.count, nil
	}
	cleanupErr := daemon.revokeReservedInput(attempt, clientID, principal.ConnectionID, runID, sessionID, generation, sequence, expectedRun, expectedSession)
	return result.count, errors.Join(effectErr, cleanupErr)
}

func (daemon *Daemon) terminalResize(ctx context.Context, principal browser.Principal, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation uint64, expectedRun, expectedSession kernel.Revision, rows, cols uint16) error {
	if err := validateLeaseLocator(runID, sessionID, generation, expectedRun, expectedSession); err != nil || rows == 0 || rows > terminalDimensionLimit || cols == 0 || cols > terminalDimensionLimit {
		if err != nil {
			return err
		}
		return fmt.Errorf("%w: invalid terminal resize", kernel.ErrInvalidValue)
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityTerminalInput)
	if err != nil {
		return err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return ErrTerminalEffectsUnsupported
	}
	attempt, err := daemon.liveTerminalAttempt(runID, sessionID)
	if err != nil {
		return err
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	if _, err := daemon.store.CheckTerminalLease(ctx, runID, sessionID, clientID, generation, expectedRun, expectedSession, at); err != nil {
		return err
	}
	result := attempt.submitEffect(ctx, terminalEffect{
		kind: terminalEffectResize, client: clientID, connection: principal.ConnectionID,
		generation: generation, rows: rows, cols: cols,
	})
	return result.effectError(-1)
}

func (daemon *Daemon) humanReply(ctx context.Context, principal browser.Principal, requestID kernel.HumanRequestID, expected kernel.Revision, reply string) (uint32, error) {
	if requestID == (kernel.HumanRequestID{}) || expected.Int64() < 1 || !utf8.ValidString(reply) || len(reply) == 0 || len(reply) > kernel.MaxHumanRequestReplyBytes {
		return 0, fmt.Errorf("%w: invalid human reply", kernel.ErrInvalidValue)
	}
	clientID, releaseClient, err := daemon.authorizeEffectPrincipal(ctx, principal, kernel.BrowserCapabilityHumanActions)
	if err != nil {
		return 0, err
	}
	defer releaseClient()
	if !terminalEffectsSupported {
		return 0, ErrTerminalEffectsUnsupported
	}
	daemon.operationMu.Lock()
	defer daemon.operationMu.Unlock()
	deliveryID, err := newHumanDeliveryID(rand.Reader)
	if err != nil {
		return 0, err
	}
	at, err := daemon.timestamp()
	if err != nil {
		return 0, err
	}
	delivery, err := daemon.store.BeginHumanReply(ctx, clientID, requestID, expected, deliveryID, reply, at)
	if err != nil {
		if terminalStoreOutcomeUnknown(err) {
			deliveryRevision, revisionErr := kernel.NewRevision(expected.Int64() + 1)
			var unknownErr error
			if revisionErr == nil {
				unknownErr = daemon.markHumanReplyUnknown(requestID, deliveryID, deliveryRevision)
			}
			return 0, errors.Join(err, revisionErr, unknownErr)
		}
		return 0, err
	}
	if delivery.RequestID != requestID || delivery.DeliveryID != deliveryID {
		return 0, kernel.ErrCorruptState
	}
	attempt, err := daemon.liveTerminalAttempt(delivery.RunID, kernel.TerminalSessionID{})
	if err != nil {
		unknownErr := daemon.markHumanReplyUnknown(requestID, deliveryID, delivery.Revision)
		return 0, errors.Join(err, unknownErr)
	}
	payload := append([]byte(nil), delivery.Reply...)
	result := attempt.submitEffect(ctx, terminalEffect{kind: terminalEffectHumanReply, payload: payload, submit: delivery.Provider == kernel.ProviderCodex})
	effectErr := result.effectError(len(payload))
	if effectErr != nil {
		unknownErr := daemon.markHumanReplyUnknown(requestID, deliveryID, delivery.Revision)
		return result.count, errors.Join(effectErr, unknownErr)
	}
	ackAt, err := daemon.timestamp()
	if err == nil {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		err = daemon.store.AcknowledgeHumanReply(storeCtx, requestID, deliveryID, delivery.Revision, ackAt)
		cancel()
	}
	if err != nil {
		unknownErr := daemon.markHumanReplyUnknown(requestID, deliveryID, delivery.Revision)
		return result.count, errors.Join(ErrTerminalEffectUncertain, err, unknownErr)
	}
	return result.count, nil
}

func (daemon *Daemon) authorizeEffectPrincipal(ctx context.Context, principal browser.Principal, capability kernel.BrowserCapabilityMask) (kernel.BrowserClientID, func(), error) {
	if daemon == nil || daemon.store == nil || daemon.browserClientGates == nil || ctx == nil || principal.ConnectionID == (browser.ConnectionID{}) {
		return kernel.BrowserClientID{}, nil, kernel.ErrUnauthorized
	}
	clientID, err := kernel.BrowserClientIDFromBytes(principal.ClientID[:])
	if err != nil {
		return kernel.BrowserClientID{}, nil, kernel.ErrUnauthorized
	}
	release, err := daemon.browserClientGates.acquire(ctx, clientID)
	if err != nil {
		return kernel.BrowserClientID{}, nil, err
	}
	client, found, err := daemon.store.BrowserClient(ctx, clientID)
	if err != nil || !found || client.RevokedAt != nil || !client.CapabilityMask.Has(capability) {
		release()
		if err != nil {
			return kernel.BrowserClientID{}, nil, err
		}
		return kernel.BrowserClientID{}, nil, kernel.ErrUnauthorized
	}
	return clientID, release, nil
}

func (daemon *Daemon) liveTerminalAttempt(runID kernel.RunID, sessionID kernel.TerminalSessionID) (*liveAttempt, error) {
	if daemon == nil || daemon.store == nil || runID == (kernel.RunID{}) {
		return nil, fmt.Errorf("%w: invalid terminal owner locator", kernel.ErrInvalidValue)
	}
	daemon.attemptMu.Lock()
	defer daemon.attemptMu.Unlock()
	if daemon.closing {
		return nil, ErrTerminalClosed
	}
	attempt := daemon.attempts[runID]
	if attempt == nil {
		return nil, kernel.ErrNotFound
	}
	if sessionID != (kernel.TerminalSessionID{}) && attempt.sessionID != sessionID {
		return nil, kernel.ErrConflict
	}
	return attempt, nil
}

func validateLeaseLocator(runID kernel.RunID, sessionID kernel.TerminalSessionID, generation uint64, expectedRun, expectedSession kernel.Revision) error {
	if runID == (kernel.RunID{}) || sessionID == (kernel.TerminalSessionID{}) || generation == 0 || generation > math.MaxInt64 || expectedRun.Int64() < 1 || expectedSession.Int64() < 1 {
		return fmt.Errorf("%w: invalid terminal lease locator", kernel.ErrInvalidValue)
	}
	return nil
}

func terminalStoreOutcomeUnknown(err error) bool {
	var unknown *kernel.OutcomeUnknownError
	return errors.As(err, &unknown)
}

func (daemon *Daemon) revokeFailedLease(attempt *liveAttempt, lease kernel.TerminalLease, effectErr error) error {
	at, err := daemon.timestamp()
	if err == nil {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		_, err = daemon.store.RevokeTerminalLease(storeCtx, lease.RunID, lease.SessionID, lease.ClientID, lease.Generation, lease.RunRevision, lease.SessionRevision, at)
		cancel()
	}
	fenceErr := daemon.revokeRunnerGeneration(attempt, lease.ClientID, browser.ConnectionID{}, lease.Generation+1, false, true)
	if err != nil {
		return errors.Join(effectErr, ErrTerminalEffectUncertain, err, fenceErr)
	}
	return errors.Join(effectErr, fenceErr)
}

func (daemon *Daemon) revokeReservedInput(attempt *liveAttempt, client kernel.BrowserClientID, connection browser.ConnectionID, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation, sequence uint64, expectedRun, expectedSession kernel.Revision) error {
	at, err := daemon.timestamp()
	if err == nil {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		err = daemon.store.RevokeTerminalInputReservation(storeCtx, runID, sessionID, client, generation, sequence, expectedRun, expectedSession, at)
		cancel()
	}
	fenceErr := daemon.revokeRunnerGeneration(attempt, client, connection, generation+1, false, true)
	if err != nil {
		return errors.Join(ErrTerminalEffectUncertain, err, fenceErr)
	}
	return fenceErr
}

func (daemon *Daemon) revokeRunnerGeneration(attempt *liveAttempt, client kernel.BrowserClientID, connection browser.ConnectionID, generation uint64, requireBinding, acceptTerminalFence bool) error {
	cleanupCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	defer cancel()
	result := attempt.submitEffect(cleanupCtx, terminalEffect{
		kind: terminalEffectRevoke, client: client, connection: connection, generation: generation, requireBinding: requireBinding,
	})
	if err := result.effectError(-1); err != nil {
		if result.terminalFence {
			if acceptTerminalFence {
				return nil
			}
			return err
		}
		return errors.Join(err, attempt.close())
	}
	return nil
}

func (daemon *Daemon) revokeAmbiguousRenewal(attempt *liveAttempt, client kernel.BrowserClientID, connection browser.ConnectionID, runID kernel.RunID, sessionID kernel.TerminalSessionID, generation uint64, expectedRun, expectedSession kernel.Revision) error {
	at, err := daemon.timestamp()
	if err == nil {
		storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		_, err = daemon.store.RevokeTerminalLease(storeCtx, runID, sessionID, client, generation, expectedRun, expectedSession, at)
		cancel()
	}
	fenceErr := daemon.revokeRunnerGeneration(attempt, client, connection, generation+1, false, true)
	if err != nil {
		return errors.Join(ErrTerminalEffectUncertain, err, fenceErr)
	}
	return fenceErr
}

func (daemon *Daemon) markHumanReplyUnknown(requestID kernel.HumanRequestID, deliveryID kernel.HumanRequestDeliveryID, expected kernel.Revision) error {
	at, err := daemon.timestamp()
	if err != nil {
		return err
	}
	storeCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
	defer cancel()
	return daemon.store.MarkHumanDeliveryUnknown(storeCtx, requestID, deliveryID, expected, at)
}

func newHumanDeliveryID(reader io.Reader) (kernel.HumanRequestDeliveryID, error) {
	if reader == nil {
		return kernel.HumanRequestDeliveryID{}, fmt.Errorf("%w: missing delivery random source", kernel.ErrInvalidValue)
	}
	var raw [kernel.IDBytes]byte
	if _, err := io.ReadFull(reader, raw[:]); err != nil {
		return kernel.HumanRequestDeliveryID{}, err
	}
	return kernel.HumanRequestDeliveryIDFromBytes(raw[:])
}

// revokeBrowserClientEffectsHeld runs after durable client revocation while
// the caller still holds that client's gate. operationMu orders the durable
// revocation with finalization and every terminal effect; each owner then
// clears only a matching private binding before advancing the runner fence.
func (daemon *Daemon) revokeBrowserClientEffectsHeld(clientID kernel.BrowserClientID) error {
	if !terminalEffectsSupported {
		return nil
	}
	daemon.attemptMu.Lock()
	attempts := make([]*liveAttempt, 0, len(daemon.attempts))
	for _, attempt := range daemon.attempts {
		attempts = append(attempts, attempt)
	}
	daemon.attemptMu.Unlock()
	var result error
	for _, attempt := range attempts {
		cleanupCtx, cancel := context.WithTimeout(context.Background(), liveAttemptStoreTimeout)
		effect := attempt.submitEffect(cleanupCtx, terminalEffect{kind: terminalEffectRevokeClient, client: clientID})
		cancel()
		if err := effect.effectError(-1); err != nil {
			if effect.terminalFence {
				continue
			}
			result = errors.Join(result, err, attempt.close())
		}
	}
	return result
}
