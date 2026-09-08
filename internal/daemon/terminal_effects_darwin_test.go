//go:build darwin

package daemon

import (
	"bytes"
	"context"
	"crypto/ecdsa"
	"crypto/elliptic"
	"crypto/rand"
	"encoding/binary"
	"encoding/json"
	"errors"
	"io"
	"os"
	"strings"
	"testing"
	"time"
	"unsafe"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

type terminalEffectWireFrame struct {
	Version      int                  `json:"version"`
	Kind         string               `json:"kind"`
	Stage        runner.AttemptStage  `json:"stage,omitempty"`
	Identity     runner.Identity      `json:"identity,omitempty"`
	Correlation  uint64               `json:"correlation,omitempty"`
	Generation   uint64               `json:"generation,omitempty"`
	Sequence     uint64               `json:"sequence,omitempty"`
	Start        uint64               `json:"start,omitempty"`
	End          uint64               `json:"end,omitempty"`
	Floor        uint64               `json:"floor,omitempty"`
	Head         uint64               `json:"head,omitempty"`
	Count        uint32               `json:"count,omitempty"`
	Rows         uint16               `json:"rows,omitempty"`
	Cols         uint16               `json:"cols,omitempty"`
	Credit       uint32               `json:"credit,omitempty"`
	Submit       bool                 `json:"submit,omitempty"`
	Status       string               `json:"status,omitempty"`
	Payload      []byte               `json:"payload,omitempty"`
	FileIdentity *runner.FileIdentity `json:"file_identity,omitempty"`
	Digest       string               `json:"digest,omitempty"`
}

type terminalEffectFixture struct {
	adapter   *adapterFixture
	run       kernel.Run
	session   kernel.TerminalSession
	attempt   *liveAttempt
	peer      *os.File
	identity  runner.Identity
	principal browser.Principal
}

func newTerminalEffectFixture(t *testing.T) *terminalEffectFixture {
	return newTerminalEffectFixtureWithProvider(t, kernel.ProviderCodex)
}

func newTerminalEffectFixtureConfigured(t *testing.T, configure func(*liveAttempt)) *terminalEffectFixture {
	return newTerminalEffectFixtureConfiguredWithProvider(t, kernel.ProviderCodex, configure)
}

func newTerminalEffectFixtureWithProvider(t *testing.T, provider kernel.Provider) *terminalEffectFixture {
	return newTerminalEffectFixtureConfiguredWithProvider(t, provider, nil)
}

func newTerminalEffectFixtureConfiguredWithProvider(t *testing.T, provider kernel.Provider, configure func(*liveAttempt)) *terminalEffectFixture {
	t.Helper()
	adapter := newAdapterFixture(t, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityTerminalInput|kernel.BrowserCapabilityHumanActions)
	adapter.pair(t)
	run := adapterRunningRoleProviderRun(t, adapter.store, 170, kernel.RoleOrchestrator, provider)
	session, found, err := adapter.store.TerminalSessionForRun(context.Background(), run.ID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	controller, peer := readyTerminalEffectController(t)
	attempt := newLiveAttempt(adapter.daemon, run.ID, session.ID, controller)
	attempt.releaseSent = true
	attempt.readySeen = true
	attempt.effectLimit = 100 * time.Millisecond
	if configure != nil {
		configure(attempt)
	}
	if err := adapter.daemon.registerLiveAttempt(attempt); err != nil {
		t.Fatal(err)
	}
	startLiveAttempt(attempt, context.Background())
	fixture := &terminalEffectFixture{
		adapter: adapter, run: run, session: session, attempt: attempt, peer: peer,
		identity:  runner.Identity{PID: 41001, PGID: 41001, Birth: runner.Birth{Seconds: 17, Microseconds: 9}},
		principal: terminalEffectPrincipal(adapter.client.ID, 1),
	}
	t.Cleanup(func() {
		_ = attempt.close()
		_ = peer.Close()
	})
	return fixture
}

func readyTerminalEffectController(t *testing.T) (*runner.AttemptController, *os.File) {
	t.Helper()
	controller, peer := innerReadyControlPair(t, "daemon-terminal-effect")
	for _, stage := range []runner.AttemptStage{runner.StageSelection, runner.StagePreparation, runner.StagePopulation} {
		if err := controller.Release(stage); err != nil {
			t.Fatal(err)
		}
		if frame := readTerminalEffectWire(t, peer); frame.Kind != "release" || frame.Stage != stage {
			t.Fatalf("release frame = %+v", frame)
		}
		writeTerminalEffectWire(t, peer, terminalEffectWireFrame{Version: 1, Kind: "checkpoint", Stage: stage})
		if event, err := controller.Next(time.Second); err != nil || event.Kind != runner.AttemptCheckpoint || event.Stage != stage {
			t.Fatalf("checkpoint %s = %+v, %v", stage, event, err)
		}
	}
	if err := controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	if frame := readTerminalEffectWire(t, peer); frame.Kind != "release" || frame.Stage != runner.StageProvider {
		t.Fatalf("provider release = %+v", frame)
	}
	writeTerminalEffectWire(t, peer, terminalEffectWireFrame{Version: 1, Kind: string(runner.TerminalReady)})
	if event, err := controller.Next(time.Second); err != nil || event.Kind != runner.AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != runner.TerminalReady {
		t.Fatalf("terminal ready = %+v, %v", event, err)
	}
	return controller, peer
}

func terminalEffectPrincipal(client kernel.BrowserClientID, seed byte) browser.Principal {
	var principal browser.Principal
	copy(principal.ClientID[:], client.Bytes())
	// ConnectionID deliberately has no public constructor. This package-level
	// causal fixture needs two transport-minted identities without weakening
	// that production boundary, so it initializes only the opaque test value.
	if unsafe.Sizeof(principal.ConnectionID) != kernel.IDBytes {
		panic("unexpected browser connection identity size")
	}
	raw := (*[kernel.IDBytes]byte)(unsafe.Pointer(&principal.ConnectionID))
	for index := range raw {
		raw[index] = seed
	}
	raw[kernel.IDBytes-1] ^= 0x5a
	return principal
}

func newTerminalEffectClient(t *testing.T, fixture *terminalEffectFixture, seed byte, capabilities kernel.BrowserCapabilityMask) kernel.BrowserClient {
	t.Helper()
	challenge := bytes.Repeat([]byte{seed}, browserprotocol.ChallengeSize)
	if _, err := fixture.adapter.store.CreateBrowserPairingChallenge(context.Background(), kernel.HashBrowserChallenge(challenge), fixture.adapter.backend.boot, adapterOrigin, capabilities, adapterTime(t, 2_000), adapterTime(t, 3_000)); err != nil {
		t.Fatal(err)
	}
	key, err := ecdsa.GenerateKey(elliptic.P256(), rand.Reader)
	if err != nil {
		t.Fatal(err)
	}
	clientID, err := kernel.BrowserClientIDFromBytes(adapterID(t, seed))
	if err != nil {
		t.Fatal(err)
	}
	client, err := fixture.adapter.store.RedeemBrowserPairingChallenge(context.Background(), kernel.HashBrowserChallenge(challenge), fixture.adapter.backend.boot, adapterOrigin, clientID, elliptic.Marshal(elliptic.P256(), key.X, key.Y), adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	return client
}

func readTerminalEffectWire(t *testing.T, peer *os.File) terminalEffectWireFrame {
	t.Helper()
	if err := peer.SetReadDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	defer peer.SetReadDeadline(time.Time{})
	var header [4]byte
	if _, err := io.ReadFull(peer, header[:]); err != nil {
		t.Fatal(err)
	}
	size := int(binary.BigEndian.Uint32(header[:]))
	if size <= 0 || size > 256<<10 {
		t.Fatalf("invalid terminal effect frame size %d", size)
	}
	body := make([]byte, size)
	if _, err := io.ReadFull(peer, body); err != nil {
		t.Fatal(err)
	}
	var frame terminalEffectWireFrame
	if err := json.Unmarshal(body, &frame); err != nil {
		t.Fatalf("decode terminal effect frame: %v", err)
	}
	return frame
}

func writeTerminalEffectWire(t *testing.T, peer *os.File, frame terminalEffectWireFrame) {
	t.Helper()
	body, err := json.Marshal(frame)
	if err != nil {
		t.Fatal(err)
	}
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(body)))
	if err := peer.SetWriteDeadline(time.Now().Add(3 * time.Second)); err != nil {
		t.Fatal(err)
	}
	defer peer.SetWriteDeadline(time.Time{})
	if _, err := peer.Write(append(header[:], body...)); err != nil {
		t.Fatal(err)
	}
}

func replyTerminalEffect(t *testing.T, peer *os.File, command terminalEffectWireFrame, status runner.TerminalResultStatus, count uint32) {
	t.Helper()
	result := terminalEffectWireFrame{
		Version: 1, Correlation: command.Correlation, Generation: command.Generation,
		Sequence: command.Sequence, Rows: command.Rows, Cols: command.Cols,
		Status: string(status), Count: count,
	}
	switch command.Kind {
	case string(runner.TerminalGenerationInstall), string(runner.TerminalGenerationRevoke):
		result.Kind = string(runner.TerminalGenerationResult)
	case string(runner.TerminalInput):
		result.Kind = string(runner.TerminalInputResult)
	case string(runner.TerminalResize):
		result.Kind = string(runner.TerminalResizeResult)
	case string(runner.TerminalHumanReply):
		result.Kind = string(runner.TerminalHumanReplyResult)
		result.Generation, result.Sequence, result.Rows, result.Cols = 0, 0, 0, 0
	default:
		t.Fatalf("cannot reply to terminal command %+v", command)
	}
	writeTerminalEffectWire(t, peer, result)
}

func testResultProof(t *testing.T) runner.ResultProof {
	t.Helper()
	var value [32]byte
	for index := range value {
		value[index] = byte(index + 1)
	}
	proof, err := runner.NewResultProof(value)
	if err != nil {
		t.Fatal(err)
	}
	return proof
}

func sendTerminalEffectExit(t *testing.T, fixture *terminalEffectFixture) {
	t.Helper()
	identity := runner.FileIdentity{Device: 7, Inode: 9}
	writeTerminalEffectWire(t, fixture.peer, terminalEffectWireFrame{
		Version: 1, Kind: "attempt-result-ready", FileIdentity: &identity, Digest: strings.Repeat("0", 64),
	})
}

func awaitTerminalEffectExit(t *testing.T, attempt *liveAttempt) *runner.AttemptResultNotice {
	t.Helper()
	select {
	case result := <-attempt.result:
		if result.err != nil || result.notice == nil {
			t.Fatalf("provider result = %+v", result)
		}
		return result.notice
	case <-time.After(time.Second):
		t.Fatal("provider result was not routed")
		return nil
	}
}

func acknowledgeTerminalEffectExit(t *testing.T, fixture *terminalEffectFixture, notice *runner.AttemptResultNotice) {
	t.Helper()
	if notice == nil {
		t.Fatal("missing result notice")
	}
	// The ACK-free contract broadcasts only the store-committed exit; no wire
	// frame reaches the runner for a consumed result.
	if err := fixture.attempt.finishExit(context.Background(), TerminalEvent{Kind: TerminalEventExit}); err != nil {
		t.Fatalf("exit broadcast: %v", err)
	}
	select {
	case <-fixture.attempt.done:
	case <-time.After(time.Second):
		t.Fatal("terminal owner did not join after the exit broadcast")
	}
	if !fixture.attempt.controller.Closed() || fixture.attempt.binding != (terminalBinding{}) {
		t.Fatalf("converged owner census: closed=%v binding=%+v", fixture.attempt.controller.Closed(), fixture.attempt.binding)
	}
}

func expectNoTerminalEffectWire(t *testing.T, peer *os.File) {
	t.Helper()
	if err := peer.SetReadDeadline(time.Now().Add(25 * time.Millisecond)); err != nil {
		t.Fatal(err)
	}
	defer peer.SetReadDeadline(time.Time{})
	var octet [1]byte
	if count, err := peer.Read(octet[:]); count != 0 || !os.IsTimeout(err) {
		t.Fatalf("unexpected runner command prefix count=%d byte=%x err=%v", count, octet[0], err)
	}
}

func (fixture *terminalEffectFixture) acquire(t *testing.T, principal browser.Principal) kernel.TerminalLease {
	t.Helper()
	type response struct {
		lease kernel.TerminalLease
		err   error
	}
	done := make(chan response, 1)
	go func() {
		lease, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
		done <- response{lease: lease, err: err}
	}()
	command := readTerminalEffectWire(t, fixture.peer)
	if command.Kind != string(runner.TerminalGenerationInstall) || command.Generation == 0 {
		t.Fatalf("lease install command = %+v", command)
	}
	replyTerminalEffect(t, fixture.peer, command, runner.TerminalResultOK, 0)
	result := <-done
	if result.err != nil {
		t.Fatalf("acquire terminal lease: %v", result.err)
	}
	return result.lease
}

func TestTerminalEffectsBindOneExactConnectionAndReleaseDurableFirst(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	other := terminalEffectPrincipal(fixture.adapter.client.ID, 2)
	type acquireResult struct {
		lease kernel.TerminalLease
		err   error
	}
	acquired := make(chan acquireResult, 1)
	go func() {
		lease, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
		acquired <- acquireResult{lease: lease, err: err}
	}()
	install := readTerminalEffectWire(t, fixture.peer)
	committed, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || committed.LeaseClientID == nil || *committed.LeaseClientID != fixture.adapter.client.ID || committed.LeaseGeneration != install.Generation {
		t.Fatalf("durable lease before install ACK = %+v, found=%v, err=%v", committed, found, err)
	}
	replyTerminalEffect(t, fixture.peer, install, runner.TerminalResultOK, 0)
	leaseResult := <-acquired
	if leaseResult.err != nil {
		t.Fatal(leaseResult.err)
	}
	lease := leaseResult.lease

	if _, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), other, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("second connection renew = %v", err)
	}
	if _, err := fixture.adapter.daemon.terminalInput(context.Background(), other, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("blocked")); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("second connection input = %v", err)
	}
	if err := fixture.adapter.daemon.terminalResize(context.Background(), other, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision, 24, 80); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("second connection resize = %v", err)
	}
	if _, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), other, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("second connection release = %v", err)
	}

	fixture.adapter.daemon.now = func() time.Time { return time.UnixMilli(2_100) }
	renewed, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
	if err != nil || renewed.Generation != lease.Generation || renewed.LastInputSequence != 0 || renewed.ExpiresAt.Int64() <= lease.ExpiresAt.Int64() {
		t.Fatalf("renewed lease = %+v, %v", renewed, err)
	}

	released := make(chan acquireResult, 1)
	go func() {
		lease, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
		released <- acquireResult{lease: lease, err: err}
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseExpiresAt != nil || cleared.LeaseGeneration != revoke.Generation {
		t.Fatalf("durable release before runner revoke ACK = %+v, found=%v, err=%v", cleared, found, err)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	releaseResult := <-released
	if releaseResult.err != nil || releaseResult.lease.Generation != lease.Generation+1 {
		t.Fatalf("release result = %+v, %v", releaseResult.lease, releaseResult.err)
	}
	if _, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("repeated release = %v", err)
	}
}

func TestTerminalRenewalIsSerializedWithOwnerEvidence(t *testing.T) {
	t.Run("terminal evidence first rejects renewal", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		sendTerminalEffectExit(t, fixture)
		terminal := awaitTerminalEffectExit(t, fixture.attempt)
		if renewed, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); err == nil || renewed != (kernel.TerminalLease{}) {
			t.Fatalf("renewal after terminal evidence = %+v, %v", renewed, err)
		}
		acknowledgeTerminalEffectExit(t, fixture, terminal)
	})

	t.Run("accepted renewal commits before queued terminal evidence", func(t *testing.T) {
		checked := make(chan struct{})
		resume := make(chan struct{})
		fixture := newTerminalEffectFixtureConfigured(t, func(attempt *liveAttempt) {
			attempt.beforeRenewCommit = func() {
				close(checked)
				<-resume
			}
		})
		lease := fixture.acquire(t, fixture.principal)
		fixture.adapter.daemon.now = func() time.Time { return time.UnixMilli(2_100) }
		type result struct {
			lease kernel.TerminalLease
			err   error
		}
		done := make(chan result, 1)
		go func() {
			renewed, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
			done <- result{lease: renewed, err: err}
		}()
		select {
		case <-checked:
		case <-time.After(time.Second):
			t.Fatal("renewal did not reach owner-serialized commit boundary")
		}
		sendTerminalEffectExit(t, fixture)
		select {
		case result := <-fixture.attempt.result:
			t.Fatalf("result crossed owner-serialized renewal: %+v", result)
		case <-time.After(25 * time.Millisecond):
		}
		close(resume)
		renewed := <-done
		if renewed.err != nil || renewed.lease.Generation != lease.Generation || renewed.lease.ExpiresAt.Int64() <= lease.ExpiresAt.Int64() {
			t.Fatalf("owner-first renewal = %+v, %v", renewed.lease, renewed.err)
		}
		terminal := awaitTerminalEffectExit(t, fixture.attempt)
		acknowledgeTerminalEffectExit(t, fixture, terminal)
	})
}

func TestTerminalRenewalOutcomeUnknownRevokesExactGeneration(t *testing.T) {
	injected := errors.New("injected renewal response loss")
	fixture := newTerminalEffectFixtureConfigured(t, func(attempt *liveAttempt) {
		attempt.renewLease = func(context.Context, kernel.BrowserClientID, uint64, kernel.Revision, kernel.Revision, kernel.UnixMillis) (kernel.TerminalLease, error) {
			return kernel.TerminalLease{}, kernel.NewOutcomeUnknownError(injected)
		}
	})
	lease := fixture.acquire(t, fixture.principal)
	type result struct {
		lease kernel.TerminalLease
		err   error
	}
	done := make(chan result, 1)
	go func() {
		renewed, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
		done <- result{lease: renewed, err: err}
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
		t.Fatalf("ambiguous renewal runner fence = %+v", revoke)
	}
	cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseGeneration != lease.Generation+1 {
		t.Fatalf("ambiguous renewal durable fence = %+v, found=%v, err=%v", cleared, found, err)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	got := <-done
	var unknown *kernel.OutcomeUnknownError
	if got.lease != (kernel.TerminalLease{}) || !errors.Is(got.err, ErrTerminalEffectUncertain) || !errors.As(got.err, &unknown) || !errors.Is(got.err, injected) {
		t.Fatalf("ambiguous renewal result = %+v, %v", got.lease, got.err)
	}
}

func TestTerminalRenewalPostconditionMismatchCannotReportSuccess(t *testing.T) {
	fixture := newTerminalEffectFixtureConfigured(t, func(attempt *liveAttempt) {
		attempt.beforeRenewCommit = func() { attempt.binding = terminalBinding{} }
	})
	lease := fixture.acquire(t, fixture.principal)
	fixture.adapter.daemon.now = func() time.Time { return time.UnixMilli(2_100) }
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.terminalLeaseRenew(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
		done <- err
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
		t.Fatalf("renewal postcondition runner fence = %+v", revoke)
	}
	cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseGeneration != lease.Generation+1 {
		t.Fatalf("renewal postcondition durable fence = %+v, found=%v, err=%v", cleared, found, err)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	if err := <-done; !errors.Is(err, ErrTerminalEffectUncertain) || !errors.Is(err, kernel.ErrCorruptState) {
		t.Fatalf("renewal postcondition result = %v", err)
	}
}

func TestTerminalEffectsRefuseZeroAndStaleConnectionIdentity(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	if _, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, kernel.RunID{}, fixture.session.ID, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, kernel.ErrInvalidValue) {
		t.Fatalf("zero acquire run locator = %v", err)
	}
	zero := browser.Principal{ClientID: fixture.principal.ClientID}
	if _, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), zero, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, kernel.ErrUnauthorized) {
		t.Fatalf("zero/backend-selected connection = %v", err)
	}
	lease := fixture.acquire(t, fixture.principal)
	stale := terminalEffectPrincipal(fixture.adapter.client.ID, 3)
	if _, err := fixture.adapter.daemon.terminalInput(context.Background(), stale, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("stale")); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("stale connection = %v", err)
	}
}

func TestExpiredLeaseReplacementRevokesOldConnectionBeforeInstallingNewOne(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	first := fixture.acquire(t, fixture.principal)
	replacementPrincipal := terminalEffectPrincipal(fixture.adapter.client.ID, 4)
	fixture.adapter.daemon.now = func() time.Time { return time.UnixMilli(first.ExpiresAt.Int64()) }
	type result struct {
		lease kernel.TerminalLease
		err   error
	}
	done := make(chan result, 1)
	go func() {
		lease, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), replacementPrincipal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
		done <- result{lease: lease, err: err}
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != first.Generation {
		t.Fatalf("expired binding revoke = %+v", revoke)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	install := readTerminalEffectWire(t, fixture.peer)
	if install.Kind != string(runner.TerminalGenerationInstall) || install.Generation != first.Generation+1 {
		t.Fatalf("replacement install = %+v", install)
	}
	replyTerminalEffect(t, fixture.peer, install, runner.TerminalResultOK, 0)
	replaced := <-done
	if replaced.err != nil || replaced.lease.Generation != first.Generation+1 {
		t.Fatalf("replacement lease = %+v, %v", replaced.lease, replaced.err)
	}
	if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, first.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("stale")); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("expired connection resumed = %v", err)
	}
}

func TestTerminalAcquireFailuresRevokeDurableLeaseAndFenceRunner(t *testing.T) {
	for _, test := range []struct {
		name   string
		status runner.TerminalResultStatus
		late   bool
	}{
		{name: "rejected", status: runner.TerminalResultRejected},
		{name: "partial", status: runner.TerminalResultPartial},
		{name: "timeout", status: runner.TerminalResultUncertain, late: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newTerminalEffectFixture(t)
			if test.late {
				fixture.attempt.effectLimit = 25 * time.Millisecond
			}
			done := make(chan error, 1)
			go func() {
				_, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
				done <- err
			}()
			install := readTerminalEffectWire(t, fixture.peer)
			if !test.late {
				replyTerminalEffect(t, fixture.peer, install, test.status, 0)
			}
			revoke := readTerminalEffectWire(t, fixture.peer)
			if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != install.Generation+1 {
				t.Fatalf("failed-install revoke = %+v after %+v", revoke, install)
			}
			cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
			if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseGeneration != revoke.Generation {
				t.Fatalf("failed-install durable cleanup = %+v, found=%v, err=%v", cleared, found, err)
			}
			if test.late {
				replyTerminalEffect(t, fixture.peer, install, runner.TerminalResultOK, 0)
			}
			replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
			err = <-done
			if test.status == runner.TerminalResultRejected && !errors.Is(err, ErrTerminalEffectRejected) || test.status == runner.TerminalResultPartial && !errors.Is(err, ErrTerminalEffectPartial) || test.late && !errors.Is(err, ErrTerminalEffectUncertain) {
				t.Fatalf("failed acquire error = %v", err)
			}
		})
	}

	t.Run("controller loss", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		done := make(chan error, 1)
		go func() {
			_, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
			done <- err
		}()
		_ = readTerminalEffectWire(t, fixture.peer)
		if err := fixture.peer.Close(); err != nil {
			t.Fatal(err)
		}
		err := <-done
		if !errors.Is(err, ErrTerminalEffectUncertain) {
			t.Fatalf("controller loss = %v", err)
		}
		cleared, found, readErr := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
		if readErr != nil || !found || cleared.LeaseClientID != nil {
			t.Fatalf("controller-loss durable cleanup = %+v, found=%v, err=%v", cleared, found, readErr)
		}
	})
}

func TestTerminalEffectWrongCorrelationFailsClosed(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
		done <- err
	}()
	install := readTerminalEffectWire(t, fixture.peer)
	writeTerminalEffectWire(t, fixture.peer, terminalEffectWireFrame{
		Version: 1, Kind: string(runner.TerminalGenerationResult), Correlation: install.Correlation + 1,
		Generation: install.Generation, Status: string(runner.TerminalResultOK),
	})
	if err := <-done; !errors.Is(err, ErrTerminalEffectUncertain) || !errors.Is(err, runner.ErrIdentity) {
		t.Fatalf("wrong correlation = %v", err)
	}
	cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseGeneration != install.Generation+1 {
		t.Fatalf("wrong-correlation durable fence = %+v, found=%v, err=%v", cleared, found, err)
	}
}

func TestTerminalInputReservesExactlyOnceAndPartialNeverReplays(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	type inputResult struct {
		count uint32
		err   error
	}
	callInput := func(sequence uint64, payload string) (<-chan inputResult, terminalEffectWireFrame) {
		done := make(chan inputResult, 1)
		go func() {
			count, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, sequence, fixture.run.Revision, fixture.session.Revision, []byte(payload))
			done <- inputResult{count: count, err: err}
		}()
		return done, readTerminalEffectWire(t, fixture.peer)
	}

	firstDone, first := callInput(1, "first")
	reserved, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || reserved.LastInputSequence != 1 {
		t.Fatalf("input reservation before write = %+v, found=%v, err=%v", reserved, found, err)
	}
	replyTerminalEffect(t, fixture.peer, first, runner.TerminalResultOK, uint32(len("first")))
	if result := <-firstDone; result.err != nil || result.count != uint32(len("first")) {
		t.Fatalf("complete input = %+v", result)
	}
	for _, sequence := range []uint64{1, 3} {
		if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, sequence, fixture.run.Revision, fixture.session.Revision, []byte("refused")); !errors.Is(err, kernel.ErrRevisionConflict) {
			t.Fatalf("sequence %d = %v", sequence, err)
		}
	}

	secondDone, second := callInput(2, "partial")
	replyTerminalEffect(t, fixture.peer, second, runner.TerminalResultPartial, 2)
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
		t.Fatalf("partial input fence = %+v", revoke)
	}
	cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || cleared.LeaseClientID != nil || cleared.LastInputSequence != 0 || cleared.LeaseGeneration != revoke.Generation {
		t.Fatalf("partial input durable revoke = %+v, found=%v, err=%v", cleared, found, err)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	result := <-secondDone
	if result.count != 2 || !errors.Is(result.err, ErrTerminalEffectPartial) {
		t.Fatalf("partial input result = %+v", result)
	}
	if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 2, fixture.run.Revision, fixture.session.Revision, []byte("partial")); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("partial input replay = %v", err)
	}
}

func TestTerminalInputRequiresExactCompleteByteCount(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	type result struct {
		count uint32
		err   error
	}
	done := make(chan result, 1)
	go func() {
		count, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("exact"))
		done <- result{count: count, err: err}
	}()
	input := readTerminalEffectWire(t, fixture.peer)
	replyTerminalEffect(t, fixture.peer, input, runner.TerminalResultOK, uint32(len("exact")-1))
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
		t.Fatalf("short OK result fence = %+v", revoke)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	got := <-done
	if got.count != uint32(len("exact")-1) || !errors.Is(got.err, ErrTerminalEffectUncertain) {
		t.Fatalf("short OK result = %+v", got)
	}
	session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || session.LeaseClientID != nil || session.LeaseGeneration != lease.Generation+1 {
		t.Fatalf("short OK durable fence = %+v, found=%v, err=%v", session, found, err)
	}
}

func TestTerminalDuringCorrelatedEffectPreservesSupervisorAcknowledgement(t *testing.T) {
	for _, phase := range []string{"install", "input", "release"} {
		t.Run(phase, func(t *testing.T) {
			fixture := newTerminalEffectFixture(t)
			type result struct {
				count uint32
				lease kernel.TerminalLease
				err   error
			}
			done := make(chan result, 1)
			var generation uint64
			switch phase {
			case "install":
				go func() {
					lease, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
					done <- result{lease: lease, err: err}
				}()
			case "input":
				lease := fixture.acquire(t, fixture.principal)
				generation = lease.Generation
				go func() {
					count, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("terminal race"))
					done <- result{count: count, err: err}
				}()
			case "release":
				lease := fixture.acquire(t, fixture.principal)
				generation = lease.Generation
				go func() {
					released, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision)
					done <- result{lease: released, err: err}
				}()
			}
			command := readTerminalEffectWire(t, fixture.peer)
			if phase == "install" {
				generation = command.Generation
			}
			sendTerminalEffectExit(t, fixture)
			terminal := awaitTerminalEffectExit(t, fixture.attempt)
			got := <-done
			if !errors.Is(got.err, ErrTerminalEffectUncertain) || !errors.Is(got.err, ErrTerminalClosed) {
				t.Fatalf("terminal during %s = count %d lease %+v err %v", phase, got.count, got.lease, got.err)
			}
			cleared, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
			if err != nil || !found || cleared.LeaseClientID != nil || cleared.LeaseGeneration != generation+1 {
				t.Fatalf("terminal during %s durable cleanup = %+v, found=%v, err=%v", phase, cleared, found, err)
			}
			select {
			case <-fixture.attempt.done:
				t.Fatalf("terminal during %s closed controller before supervisor ACK", phase)
			default:
			}
			expectNoTerminalEffectWire(t, fixture.peer)
			acknowledgeTerminalEffectExit(t, fixture, terminal)
		})
	}
}

func TestLateEffectResultAfterTerminalEvidenceCannotResurrectBinding(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("one shot"))
		done <- err
	}()
	input := readTerminalEffectWire(t, fixture.peer)
	sendTerminalEffectExit(t, fixture)
	terminal := awaitTerminalEffectExit(t, fixture.attempt)
	replyTerminalEffect(t, fixture.peer, input, runner.TerminalResultOK, uint32(len("one shot")))
	if err := <-done; !errors.Is(err, ErrTerminalEffectUncertain) || !errors.Is(err, ErrTerminalClosed) {
		t.Fatalf("input interrupted by terminal = %v", err)
	}
	if fixture.attempt.binding != (terminalBinding{}) {
		t.Fatalf("late effect result resurrected binding %+v", fixture.attempt.binding)
	}
	expectNoTerminalEffectWire(t, fixture.peer)
	acknowledgeTerminalEffectExit(t, fixture, terminal)
}

func TestTerminalResizeIsExactAndUncertaintyIsVisible(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	done := make(chan error, 1)
	go func() {
		done <- fixture.adapter.daemon.terminalResize(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision, 37, 111)
	}()
	command := readTerminalEffectWire(t, fixture.peer)
	if command.Kind != string(runner.TerminalResize) || command.Rows != 37 || command.Cols != 111 || command.Generation != lease.Generation {
		t.Fatalf("resize command = %+v", command)
	}
	replyTerminalEffect(t, fixture.peer, command, runner.TerminalResultUncertain, 0)
	if err := <-done; !errors.Is(err, ErrTerminalEffectUncertain) {
		t.Fatalf("uncertain resize = %v", err)
	}
	other := terminalEffectPrincipal(fixture.adapter.client.ID, 8)
	if err := fixture.adapter.daemon.terminalResize(context.Background(), other, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision, 37, 111); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("stale resize = %v", err)
	}
}

func TestHumanReplyUsesExactRunAndResolvesOnlyAfterFullDelivery(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 210))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "question"}, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	type replyResult struct {
		count uint32
		err   error
	}
	done := make(chan replyResult, 1)
	ctx, cancel := context.WithCancel(context.Background())
	go func() {
		count, err := fixture.adapter.daemon.humanReply(ctx, fixture.principal, request.ID, request.Revision, "exact reply")
		done <- replyResult{count: count, err: err}
	}()
	command := readTerminalEffectWire(t, fixture.peer)
	if command.Kind != string(runner.TerminalHumanReply) || string(command.Payload) != "exact reply" || !command.Submit || command.Generation != 0 || command.Sequence != 0 {
		t.Fatalf("human reply command = %+v", command)
	}
	delivering, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || delivering.Status != kernel.HumanRequestDelivering || delivering.Revision.Int64() != request.Revision.Int64()+1 {
		t.Fatalf("durable delivery before write ACK = %+v, found=%v, err=%v", delivering, found, err)
	}
	cancel()
	replyTerminalEffect(t, fixture.peer, command, runner.TerminalResultOK, uint32(len("exact reply")))
	result := <-done
	if result.err != nil || result.count != uint32(len("exact reply")) {
		t.Fatalf("human reply after caller cancellation = %+v", result)
	}
	resolved, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || resolved.Status != kernel.HumanRequestResolved {
		t.Fatalf("resolved human request = %+v, found=%v, err=%v", resolved, found, err)
	}
	if _, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "duplicate"); !errors.Is(err, kernel.ErrRevisionConflict) {
		t.Fatalf("duplicate reply = %v", err)
	}

	copy(key[:], adapterID(t, 211))
	second, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "second"}, adapterTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	partialDone := make(chan replyResult, 1)
	go func() {
		count, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, second.ID, second.Revision, "partial reply")
		partialDone <- replyResult{count: count, err: err}
	}()
	partial := readTerminalEffectWire(t, fixture.peer)
	if string(partial.Payload) != "partial reply" || !partial.Submit {
		t.Fatalf("partial codex reply = %+v", partial)
	}
	replyTerminalEffect(t, fixture.peer, partial, runner.TerminalResultPartial, 3)
	partialResult := <-partialDone
	if partialResult.count != 3 || !errors.Is(partialResult.err, ErrTerminalEffectPartial) {
		t.Fatalf("partial human reply = %+v", partialResult)
	}
	unknown, found, err := fixture.adapter.store.HumanRequest(context.Background(), second.ID)
	if err != nil || !found || unknown.Status != kernel.HumanRequestDeliveryUnknown {
		t.Fatalf("unknown human delivery = %+v, found=%v, err=%v", unknown, found, err)
	}
	if _, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, second.ID, unknown.Revision, "stale"); !errors.Is(err, kernel.ErrRevisionConflict) {
		t.Fatalf("uncertain request reply = %v", err)
	}
}

func TestHumanReplyKeepsShellPayloadRaw(t *testing.T) {
	fixture := newTerminalEffectFixtureWithProvider(t, kernel.ProviderShell)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 214))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "question"}, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "exact reply")
		done <- err
	}()
	command := readTerminalEffectWire(t, fixture.peer)
	if command.Kind != string(runner.TerminalHumanReply) || string(command.Payload) != "exact reply" || command.Submit {
		t.Fatalf("shell human reply command = %+v", command)
	}
	replyTerminalEffect(t, fixture.peer, command, runner.TerminalResultOK, uint32(len(command.Payload)))
	if err := <-done; err != nil {
		t.Fatalf("shell human reply = %v", err)
	}
}

func TestCodexHumanReplyDefersSubmitForMaximumReply(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 215))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "question"}, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	reply := strings.Repeat("x", kernel.MaxHumanRequestReplyBytes)
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, reply)
		done <- err
	}()
	answer := readTerminalEffectWire(t, fixture.peer)
	if len(answer.Payload) != kernel.MaxHumanRequestReplyBytes || string(answer.Payload) != reply || !answer.Submit {
		t.Fatalf("maximum codex reply = %d bytes", len(answer.Payload))
	}
	replyTerminalEffect(t, fixture.peer, answer, runner.TerminalResultOK, uint32(len(answer.Payload)))
	if err := <-done; err != nil {
		t.Fatalf("maximum codex human reply = %v", err)
	}
}

func TestUncertainHumanReplyRemainsDurablyVisibleWithoutReplay(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 212))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "uncertain"}, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	type result struct {
		count uint32
		err   error
	}
	done := make(chan result, 1)
	go func() {
		count, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "one shot")
		done <- result{count: count, err: err}
	}()
	command := readTerminalEffectWire(t, fixture.peer)
	if string(command.Payload) != "one shot" || !command.Submit {
		t.Fatalf("uncertain codex reply = %+v", command)
	}
	replyTerminalEffect(t, fixture.peer, command, runner.TerminalResultUncertain, 0)
	got := <-done
	if got.count != 0 || !errors.Is(got.err, ErrTerminalEffectUncertain) {
		t.Fatalf("uncertain human reply = %+v", got)
	}
	unknown, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || unknown.Status != kernel.HumanRequestDeliveryUnknown {
		t.Fatalf("uncertain human delivery = %+v, found=%v, err=%v", unknown, found, err)
	}
	if _, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "replay"); !errors.Is(err, kernel.ErrRevisionConflict) {
		t.Fatalf("uncertain delivery replay = %v", err)
	}
}

func TestHumanReplyReservesDerivedRunBeforeLiveOwnerLookup(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 213))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "owner vanished"}, adapterTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	fixture.adapter.daemon.unregisterLiveAttempt(fixture.run.ID, fixture.attempt)
	count, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "one shot")
	if count != 0 || !errors.Is(err, kernel.ErrNotFound) {
		t.Fatalf("missing owner reply = count %d, err %v", count, err)
	}
	expectNoTerminalEffectWire(t, fixture.peer)
	unknown, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || unknown.Status != kernel.HumanRequestDeliveryUnknown || unknown.Revision.Int64() != request.Revision.Int64()+2 {
		t.Fatalf("missing owner durable reservation = %+v, found=%v, err=%v", unknown, found, err)
	}
	if _, err := fixture.adapter.daemon.humanReply(context.Background(), fixture.principal, request.ID, request.Revision, "replay"); !errors.Is(err, kernel.ErrRevisionConflict) {
		t.Fatalf("missing owner replay = %v", err)
	}
	expectNoTerminalEffectWire(t, fixture.peer)
}

func TestClientRevocationDurablyClearsLeaseBeforeRunnerFence(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	done := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.RevokeBrowserClient(context.Background(), fixture.adapter.client.ID, fixture.adapter.client.Revision)
		done <- err
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
		t.Fatalf("client revoke runner fence = %+v", revoke)
	}
	client, found, err := fixture.adapter.store.BrowserClient(context.Background(), fixture.adapter.client.ID)
	if err != nil || !found || client.RevokedAt == nil {
		t.Fatalf("durable client revocation before runner ACK = %+v, found=%v, err=%v", client, found, err)
	}
	session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || session.LeaseClientID != nil || session.LeaseGeneration != revoke.Generation {
		t.Fatalf("durable client lease clear before runner ACK = %+v, found=%v, err=%v", session, found, err)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	if err := <-done; err != nil {
		t.Fatalf("client revocation: %v", err)
	}
}

func TestCancelHumanRequestSurfacesOwnerFenceFailureAfterCommit(t *testing.T) {
	for _, test := range []struct {
		name   string
		status runner.TerminalResultStatus
	}{
		{name: "rejected", status: runner.TerminalResultRejected},
		{name: "partial", status: runner.TerminalResultPartial},
		{name: "uncertain", status: runner.TerminalResultUncertain},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newTerminalEffectFixture(t)
			lease := fixture.acquire(t, fixture.principal)
			cancellingClient := newTerminalEffectClient(t, fixture, 222, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions)
			if !sameTerminalBinding(fixture.attempt.binding, fixture.adapter.client.ID, fixture.principal.ConnectionID, lease.Generation) || cancellingClient.ID == fixture.adapter.client.ID {
				t.Fatalf("pre-cancel binding = %+v, cancelling client=%v", fixture.attempt.binding, cancellingClient.ID)
			}
			var key [kernel.IDBytes]byte
			copy(key[:], adapterID(t, 220))
			request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "cancel"}, adapterTime(t, 2_000))
			if err != nil {
				t.Fatal(err)
			}
			currentRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
			if err != nil || !found {
				t.Fatalf("current run = %+v, found=%v, err=%v", currentRun, found, err)
			}
			done := make(chan error, 1)
			go func() {
				_, _, cancelErr := fixture.adapter.daemon.cancelHumanRequestRun(context.Background(), cancellingClient.ID, request.ID, request.Revision, currentRun.Revision, adapterTime(t, 2_001))
				done <- cancelErr
			}()
			revoke := readTerminalEffectWire(t, fixture.peer)
			if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
				t.Fatalf("cancel owner fence = %+v", revoke)
			}
			replyTerminalEffect(t, fixture.peer, revoke, test.status, 0)
			if cancelErr := <-done; !errors.Is(cancelErr, ErrHumanRequestOwnerFence) {
				t.Fatalf("cancel fence status = %v", cancelErr)
			}
			observedRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
			if err != nil || !found || observedRun.Phase != kernel.RunFinalizing {
				t.Fatalf("durable cancel run = %+v, found=%v, err=%v", observedRun, found, err)
			}
			resolved, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
			if err != nil || !found || resolved.Status != kernel.HumanRequestResolved {
				t.Fatalf("durable cancel request = %+v, found=%v, err=%v", resolved, found, err)
			}
			session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
			if err != nil || !found || session.LeaseClientID != nil || session.LeaseGeneration != lease.Generation+1 {
				t.Fatalf("durable cancel input revoke = %+v, found=%v, err=%v", session, found, err)
			}
			if _, _, duplicateErr := fixture.adapter.daemon.cancelHumanRequestRun(context.Background(), cancellingClient.ID, request.ID, request.Revision, currentRun.Revision, adapterTime(t, 2_002)); !errors.Is(duplicateErr, kernel.ErrRevisionConflict) {
				t.Fatalf("duplicate cancel = %v", duplicateErr)
			}
			if !fixture.attempt.controller.Closed() {
				t.Fatal("owner was not closed after an unacknowledged fence")
			}
		})
	}
}

func TestCancelHumanRequestWithoutLiveBindingIsDefinitiveSuccess(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 223))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "cancel without binding"}, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	currentRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("current run = %+v, found=%v, err=%v", currentRun, found, err)
	}
	updatedRun, resolved, err := fixture.adapter.daemon.cancelHumanRequestRun(context.Background(), fixture.adapter.client.ID, request.ID, request.Revision, currentRun.Revision, adapterTime(t, 2_001))
	if err != nil || updatedRun.Phase != kernel.RunFinalizing || resolved.Status != kernel.HumanRequestResolved {
		t.Fatalf("cancel without binding = run %+v, request %+v, err=%v", updatedRun, resolved, err)
	}
	if terminate := readTerminalEffectWire(t, fixture.peer); terminate.Kind != "terminate" {
		t.Fatalf("post-cancel lifecycle command = %+v", terminate)
	}
	expectNoTerminalEffectWire(t, fixture.peer)
}

func TestCancelHumanRequestSurfacesControllerFailureAfterCommit(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	fixture.acquire(t, fixture.principal)
	cancellingClient := newTerminalEffectClient(t, fixture, 224, kernel.BrowserCapabilityObserve|kernel.BrowserCapabilityHumanActions)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 225))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "cancel after controller loss"}, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	currentRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("current run = %+v, found=%v, err=%v", currentRun, found, err)
	}
	done := make(chan error, 1)
	go func() {
		_, _, cancelErr := fixture.adapter.daemon.cancelHumanRequestRun(context.Background(), cancellingClient.ID, request.ID, request.Revision, currentRun.Revision, adapterTime(t, 2_001))
		done <- cancelErr
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) {
		t.Fatalf("controller-failure revoke = %+v", revoke)
	}
	if err := fixture.peer.Close(); err != nil {
		t.Fatal(err)
	}
	cancelErr := <-done
	if !errors.Is(cancelErr, ErrHumanRequestOwnerFence) {
		t.Fatalf("controller failure = %v", cancelErr)
	}
	observedRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found || observedRun.Phase != kernel.RunFinalizing {
		t.Fatalf("durable controller-failure run = %+v, found=%v, err=%v", observedRun, found, err)
	}
	resolved, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || resolved.Status != kernel.HumanRequestResolved {
		t.Fatalf("durable controller-failure request = %+v, found=%v, err=%v", resolved, found, err)
	}
}

// The #542 timeline: a run cancelled from a decision card revokes the runner
// binding before the console's terminal lets go of its lease, with the
// provider result still pending. The release's check is then a plain
// rejection, and the browser boundary answers stale, not internal.
func TestReleaseAfterCancelIsStaleNotInternal(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	lease := fixture.acquire(t, fixture.principal)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 225))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "cancel then release"}, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	currentRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("current run = %+v, found=%v, err=%v", currentRun, found, err)
	}
	cancelled := make(chan error, 1)
	go func() {
		_, _, err := fixture.adapter.daemon.cancelHumanRequestRun(context.Background(), fixture.adapter.client.ID, request.ID, request.Revision, currentRun.Revision, adapterTime(t, 2_001))
		cancelled <- err
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	if err := <-cancelled; err != nil {
		t.Fatalf("cancel with a live binding = %v", err)
	}
	if terminate := readTerminalEffectWire(t, fixture.peer); terminate.Kind != "terminate" {
		t.Fatalf("post-cancel lifecycle command = %+v", terminate)
	}
	// The console still holds the revisions it attached with; the check is
	// refused before the store would compare them.
	release := browserprotocol.TerminalLeaseRelease{
		RunID: fixture.run.ID.String(), SessionID: fixture.session.ID.String(), Generation: browserprotocol.Decimal(lease.Generation),
		ExpectedRunRevision: decimalRevision(fixture.run.Revision), ExpectedSessionRevision: decimalRevision(fixture.session.Revision),
	}
	if _, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); !errors.Is(err, ErrTerminalEffectRejected) || !errors.Is(err, kernel.ErrRevisionConflict) {
		t.Fatalf("release after cancel = %v, want a rejection carrying a revision conflict", err)
	}
	if _, err := fixture.adapter.backend.ReleaseTerminalLease(context.Background(), fixture.principal, release); !errors.Is(err, browser.ErrStale) {
		t.Fatalf("release after cancel at the browser boundary = %v", err)
	}
}

func TestBrowserCancelOwnerFenceErrorIsNonRetryable(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	fixture.acquire(t, fixture.principal)
	var key [kernel.IDBytes]byte
	copy(key[:], adapterID(t, 221))
	request, err := fixture.adapter.store.CreateHumanQuestionForAttempt(context.Background(), fixture.run.CredentialDigest, kernel.NewHumanQuestion{IdempotencyKey: key, QuestionText: "cancel over browser"}, adapterTime(t, 2_000))
	if err != nil {
		t.Fatal(err)
	}
	currentRun, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("current run = %+v, found=%v, err=%v", currentRun, found, err)
	}
	connection := fixture.adapter.authenticate(t)
	payload, err := browserprotocol.EncodeHumanRequestCancelRun("cancel-browser", browserprotocol.HumanRequestCancelRun{
		RequestID: request.ID.String(), ExpectedRequestRevision: browserprotocol.Decimal(request.Revision.Int64()), ExpectedRunRevision: browserprotocol.Decimal(currentRun.Revision.Int64()),
	})
	if err != nil {
		t.Fatal(err)
	}
	adapterWrite(t, connection, payload)
	revoke := readTerminalEffectWire(t, fixture.peer)
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultPartial, 0)
	frame := adapterRead(t, connection)
	if frame.Type != browserprotocol.TypeError {
		t.Fatalf("browser cancel response = %+v", frame)
	}
	response := frame.Body.(browserprotocol.Error)
	if response.Code != browserprotocol.ErrorInternal || response.Retryable {
		t.Fatalf("browser cancel error = %+v", response)
	}
	resolved, found, err := fixture.adapter.store.HumanRequest(context.Background(), request.ID)
	if err != nil || !found || resolved.Status != kernel.HumanRequestResolved {
		t.Fatalf("durable browser cancel request = %+v, found=%v, err=%v", resolved, found, err)
	}
}

func TestPublicAttachRequiresExactDurableRevisions(t *testing.T) {
	for _, test := range []struct {
		name         string
		staleRun     bool
		staleSession bool
	}{
		{name: "run revision", staleRun: true},
		{name: "session revision", staleSession: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			fixture := newTerminalEffectFixture(t)
			runRevision := fixture.run.Revision
			sessionRevision := fixture.session.Revision
			if test.staleRun {
				runRevision = mustRevision(t, runRevision.Int64()+1)
			}
			if test.staleSession {
				sessionRevision = mustRevision(t, sessionRevision.Int64()+1)
			}
			attachment, err := fixture.adapter.daemon.AttachTerminal(context.Background(), fixture.run.ID, fixture.session.ID, runRevision, sessionRevision, 0)
			if attachment != nil || !errors.Is(err, kernel.ErrConflict) {
				t.Fatalf("stale attach = attachment %v, err %v", attachment, err)
			}
			expectNoTerminalEffectWire(t, fixture.peer)
			if len(fixture.attempt.subs) != 0 || len(fixture.attempt.correlations) != 0 {
				t.Fatalf("stale attach registered observer: subs=%d correlations=%d", len(fixture.attempt.subs), len(fixture.attempt.correlations))
			}
		})
	}
}

func TestPublicAttachSerializesWithFinalization(t *testing.T) {
	t.Run("finalization commits before queued attach", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		fixture.adapter.daemon.operationMu.Lock()
		type result struct {
			attachment *TerminalAttachment
			err        error
		}
		attached := make(chan result, 1)
		go func() {
			attachment, err := fixture.adapter.daemon.AttachTerminal(context.Background(), fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision, 0)
			attached <- result{attachment: attachment, err: err}
		}()
		select {
		case got := <-attached:
			fixture.adapter.daemon.operationMu.Unlock()
			t.Fatalf("public attach crossed operation gate: %+v", got)
		case <-time.After(25 * time.Millisecond):
		}
		expectNoTerminalEffectWire(t, fixture.peer)
		proposal, err := kernel.NewSuccessProposal("done")
		if err == nil {
			_, err = fixture.adapter.store.ProposeAttemptOutcome(context.Background(), fixture.run.CredentialDigest, proposal, adapterTime(t, 2_200))
		}
		fixture.adapter.daemon.operationMu.Unlock()
		if err != nil {
			t.Fatal(err)
		}
		got := <-attached
		if got.attachment != nil || got.err == nil {
			t.Fatalf("attach queued behind finalization = %+v", got)
		}
		if terminate := readTerminalEffectWire(t, fixture.peer); terminate.Kind != "terminate" {
			t.Fatalf("post-finalization command = %+v", terminate)
		}
		expectNoTerminalEffectWire(t, fixture.peer)
	})

	t.Run("accepted attach completes before finalization", func(t *testing.T) {
		entered := make(chan struct{})
		resume := make(chan struct{})
		fixture := newTerminalEffectFixtureConfigured(t, func(attempt *liveAttempt) {
			attempt.beforeAttachEffect = func() {
				close(entered)
				<-resume
			}
		})
		type result struct {
			attachment *TerminalAttachment
			err        error
		}
		attached := make(chan result, 1)
		go func() {
			attachment, err := fixture.adapter.daemon.AttachTerminal(context.Background(), fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision, 0)
			attached <- result{attachment: attachment, err: err}
		}()
		select {
		case <-entered:
		case <-time.After(time.Second):
			t.Fatal("public attach did not reach runner-effect boundary")
		}
		finalized := make(chan error, 1)
		go func() {
			proposal, _ := kernel.NewSuccessProposal("done")
			fixture.adapter.daemon.operationMu.Lock()
			_, err := fixture.adapter.store.ProposeAttemptOutcome(context.Background(), fixture.run.CredentialDigest, proposal, adapterTime(t, 2_200))
			fixture.adapter.daemon.operationMu.Unlock()
			finalized <- err
		}()
		select {
		case err := <-finalized:
			t.Fatalf("finalization crossed accepted public attach: %v", err)
		case <-time.After(25 * time.Millisecond):
		}
		observed, found, err := fixture.adapter.store.Run(context.Background(), fixture.run.ID)
		if err != nil || !found || observed.Phase != kernel.RunRunning {
			t.Fatalf("run changed before accepted attach effect: %+v, found=%v, err=%v", observed, found, err)
		}
		close(resume)
		got := <-attached
		if got.err != nil || got.attachment == nil {
			t.Fatalf("accepted public attach = %+v", got)
		}
		if attach := readTerminalEffectWire(t, fixture.peer); attach.Kind != string(runner.TerminalAttach) {
			t.Fatalf("accepted public attach command = %+v", attach)
		}
		if credit := readTerminalEffectWire(t, fixture.peer); credit.Kind != string(runner.TerminalCredit) {
			t.Fatalf("accepted public attach credit = %+v", credit)
		}
		if err := <-finalized; err != nil {
			t.Fatalf("serialized finalization: %v", err)
		}
		if terminate := readTerminalEffectWire(t, fixture.peer); terminate.Kind != "terminate" {
			t.Fatalf("post-attach finalization command = %+v", terminate)
		}
	})
}

func TestFinalizationOrdersAcceptedEffectsAndFencesBothRevocationOrderings(t *testing.T) {
	t.Run("finalizing before effect and client revoke", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		proposal, err := kernel.NewSuccessProposal("done")
		if err != nil {
			t.Fatal(err)
		}
		fixture.adapter.daemon.operationMu.Lock()
		finalizing, err := fixture.adapter.store.ProposeAttemptOutcome(context.Background(), fixture.run.CredentialDigest, proposal, adapterTime(t, 2_200))
		fixture.adapter.daemon.operationMu.Unlock()
		if err != nil || finalizing.Phase != kernel.RunFinalizing {
			t.Fatalf("finalizing = %+v, %v", finalizing, err)
		}
		terminate := readTerminalEffectWire(t, fixture.peer)
		if terminate.Kind != "terminate" {
			t.Fatalf("finalization fence = %+v", terminate)
		}
		if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("too late")); err == nil {
			t.Fatal("input after finalizing succeeded")
		}
		if _, err := fixture.adapter.daemon.terminalLeaseRelease(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, fixture.run.Revision, fixture.session.Revision); err == nil {
			t.Fatal("release after finalizing succeeded")
		}
		if _, err := fixture.adapter.daemon.RevokeBrowserClient(context.Background(), fixture.adapter.client.ID, fixture.adapter.client.Revision); err != nil {
			t.Fatalf("client revoke after finalization: %v", err)
		}
		session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
		if err != nil || !found || session.LeaseClientID != nil || session.LeaseGeneration != lease.Generation+1 {
			t.Fatalf("finalization/revocation convergence = %+v, found=%v, err=%v", session, found, err)
		}
	})

	t.Run("accepted input before finalizing", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		inputDone := make(chan error, 1)
		go func() {
			_, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("accepted"))
			inputDone <- err
		}()
		input := readTerminalEffectWire(t, fixture.peer)
		if input.Kind != string(runner.TerminalInput) {
			t.Fatalf("accepted input command = %+v", input)
		}
		finalized := make(chan error, 1)
		go func() {
			proposal, _ := kernel.NewSuccessProposal("done")
			fixture.adapter.daemon.operationMu.Lock()
			_, err := fixture.adapter.store.ProposeAttemptOutcome(context.Background(), fixture.run.CredentialDigest, proposal, adapterTime(t, 2_200))
			fixture.adapter.daemon.operationMu.Unlock()
			finalized <- err
		}()
		select {
		case err := <-finalized:
			t.Fatalf("finalization crossed accepted effect: %v", err)
		case <-time.After(25 * time.Millisecond):
		}
		replyTerminalEffect(t, fixture.peer, input, runner.TerminalResultOK, uint32(len("accepted")))
		if err := <-inputDone; err != nil {
			t.Fatalf("accepted input result: %v", err)
		}
		if err := <-finalized; err != nil {
			t.Fatalf("serialized finalization: %v", err)
		}
		terminate := readTerminalEffectWire(t, fixture.peer)
		if terminate.Kind != "terminate" {
			t.Fatalf("post-effect finalization fence = %+v", terminate)
		}
		session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
		if err != nil || !found || session.LeaseClientID != nil || session.LeaseGeneration != lease.Generation+1 {
			t.Fatalf("post-effect finalization lease = %+v, found=%v, err=%v", session, found, err)
		}
	})

	t.Run("client revoke before finalizing", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		revoked := make(chan error, 1)
		go func() {
			_, err := fixture.adapter.daemon.RevokeBrowserClient(context.Background(), fixture.adapter.client.ID, fixture.adapter.client.Revision)
			revoked <- err
		}()
		revoke := readTerminalEffectWire(t, fixture.peer)
		if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != lease.Generation+1 {
			t.Fatalf("client-first fence = %+v", revoke)
		}
		replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
		if err := <-revoked; err != nil {
			t.Fatal(err)
		}
		proposal, _ := kernel.NewSuccessProposal("done")
		fixture.adapter.daemon.operationMu.Lock()
		_, err := fixture.adapter.store.ProposeAttemptOutcome(context.Background(), fixture.run.CredentialDigest, proposal, adapterTime(t, 2_200))
		fixture.adapter.daemon.operationMu.Unlock()
		if err != nil {
			t.Fatalf("finalization after client revoke: %v", err)
		}
		if terminate := readTerminalEffectWire(t, fixture.peer); terminate.Kind != "terminate" {
			t.Fatalf("client-first finalization fence = %+v", terminate)
		}
	})
}

func TestProviderExitAndOwnerDeathFencePrivateBinding(t *testing.T) {
	t.Run("provider exit", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		sendTerminalEffectExit(t, fixture)
		select {
		case result := <-fixture.attempt.result:
			if result.err != nil || result.notice == nil {
				t.Fatalf("provider result = %+v", result)
			}
		case <-time.After(time.Second):
			t.Fatal("provider result was not routed")
		}
		if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("after exit")); !errors.Is(err, ErrTerminalEffectRejected) {
			t.Fatalf("input after provider exit = %v", err)
		}
	})

	t.Run("owner death", func(t *testing.T) {
		fixture := newTerminalEffectFixture(t)
		lease := fixture.acquire(t, fixture.principal)
		if err := fixture.peer.Close(); err != nil {
			t.Fatal(err)
		}
		select {
		case result := <-fixture.attempt.result:
			if result.err == nil || result.notice != nil {
				t.Fatalf("owner fallback = %+v", result)
			}
		case <-time.After(time.Second):
			t.Fatal("dead owner did not report fallback")
		}
		if fixture.attempt.binding != (terminalBinding{}) {
			t.Fatalf("owner death retained private binding = %+v", fixture.attempt.binding)
		}
		select {
		case <-fixture.attempt.done:
			t.Fatal("dead owner abandoned the authenticated-result window")
		default:
		}
		if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("after death")); !errors.Is(err, ErrTerminalEffectRejected) {
			t.Fatalf("input while fallback pending = %v", err)
		}
		if err := fixture.attempt.close(); err == nil {
			t.Fatal("fallback cleanup discarded the controller error")
		}
		if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, lease.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("after cleanup")); !errors.Is(err, kernel.ErrNotFound) {
			t.Fatalf("input after owner death = %v", err)
		}
	})
}

// A terminal effect in flight when the runner exits is the other place the
// daemon learns the control end is gone by reading it. That error carries the
// transport sentinel the supervisor uses to decide whether the runner's exit
// status is the missing part of the diagnosis, so this path must wrap it rather
// than replace it with prose, exactly as the owner loop does.
func TestTerminalEffectKeepsControlEndedSentinel(t *testing.T) {
	controller, peer := readyTerminalEffectController(t)
	runID, sessionID := liveTestIDs(t, 10021)
	attempt := newLiveAttempt(nil, runID, sessionID, controller)
	attempt.releaseSent = true
	attempt.readySeen = true
	attempt.effectLimit = 4 * time.Second
	drained := make(chan error, 1)
	go func() {
		// Consuming the command proves the write landed while the runner was
		// alive, so the exit that follows is the runner's own.
		var header [4]byte
		if _, err := io.ReadFull(peer, header[:]); err != nil {
			drained <- err
			return
		}
		body := make([]byte, binary.BigEndian.Uint32(header[:]))
		if _, err := io.ReadFull(peer, body); err != nil {
			drained <- err
			return
		}
		drained <- peer.Close()
	}()
	_, err := attempt.runTerminalEffect(runner.TerminalCommand{Kind: runner.TerminalGenerationInstall, Generation: 1})
	if peerErr := <-drained; peerErr != nil {
		t.Fatalf("runner peer: %v", peerErr)
	}
	if !errors.Is(err, io.EOF) {
		t.Fatalf("effect after runner exit lost the stream sentinel: %v", err)
	}
	if !strings.Contains(err.Error(), "attempt controller closed") {
		t.Fatalf("effect error does not name the closed controller: %v", err)
	}
}

func TestHolderReconnectSupersedesUnexpiredLeaseBeforeExpiry(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	first := fixture.acquire(t, fixture.principal)
	// One accepted input under the first lease moves the durable sequence off
	// zero so the reacquire's reset below is observable.
	inputDone := make(chan error, 1)
	go func() {
		_, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, first.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("held"))
		inputDone <- err
	}()
	held := readTerminalEffectWire(t, fixture.peer)
	replyTerminalEffect(t, fixture.peer, held, runner.TerminalResultOK, uint32(len("held")))
	if err := <-inputDone; err != nil {
		t.Fatalf("input under the first lease = %v", err)
	}
	if before, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID); err != nil || !found || before.LastInputSequence != 1 {
		t.Fatalf("durable sequence before reconnect = %+v, found=%v, err=%v", before, found, err)
	}
	// A reload keeps the paired client but arrives on a new connection while
	// the earlier lease is still far from expiry.
	reconnected := terminalEffectPrincipal(fixture.adapter.client.ID, 4)
	if reconnected.ConnectionID == fixture.principal.ConnectionID {
		t.Fatal("reconnect fixture reused the connection identity")
	}
	type result struct {
		lease kernel.TerminalLease
		err   error
	}
	done := make(chan result, 1)
	go func() {
		lease, err := fixture.adapter.daemon.terminalLeaseAcquire(context.Background(), reconnected, fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision)
		done <- result{lease: lease, err: err}
	}()
	revoke := readTerminalEffectWire(t, fixture.peer)
	if revoke.Kind != string(runner.TerminalGenerationRevoke) || revoke.Generation != first.Generation {
		t.Fatalf("superseded binding revoke = %+v", revoke)
	}
	replyTerminalEffect(t, fixture.peer, revoke, runner.TerminalResultOK, 0)
	install := readTerminalEffectWire(t, fixture.peer)
	if install.Kind != string(runner.TerminalGenerationInstall) || install.Generation != first.Generation+1 {
		t.Fatalf("reconnect install = %+v", install)
	}
	replyTerminalEffect(t, fixture.peer, install, runner.TerminalResultOK, 0)
	replaced := <-done
	// The fixture clock is frozen, so a fresh full TTL expires exactly when the
	// first lease would have.
	if replaced.err != nil || replaced.lease.Generation != first.Generation+1 || replaced.lease.ExpiresAt != first.ExpiresAt {
		t.Fatalf("reconnect lease = %+v, %v", replaced.lease, replaced.err)
	}
	// The durable row, not the returned lease, carries the sequence, which the
	// reacquire restarts from the 1 recorded above.
	session, found, err := fixture.adapter.store.TerminalSession(context.Background(), fixture.session.ID)
	if err != nil || !found || session.LeaseClientID == nil || *session.LeaseClientID != fixture.adapter.client.ID || session.LeaseGeneration != replaced.lease.Generation || session.LastInputSequence != 0 {
		t.Fatalf("durable lease after reconnect = %+v, found=%v, err=%v", session, found, err)
	}
	if _, err := fixture.adapter.daemon.terminalInput(context.Background(), fixture.principal, fixture.run.ID, fixture.session.ID, first.Generation, 1, fixture.run.Revision, fixture.session.Revision, []byte("stale")); !errors.Is(err, ErrTerminalEffectRejected) {
		t.Fatalf("superseded connection resumed = %v", err)
	}
}
