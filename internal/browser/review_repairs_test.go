package browser

import (
	"context"
	"encoding/hex"
	"errors"
	"net"
	"strings"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

func authProof(t *testing.T, connection *websocket.Conn) {
	t.Helper()
	proof, err := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{
		ClientID: testID, Signature: strings.Repeat("01", browserprotocol.SignatureSize),
	})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, proof)
}

func waitFor(t *testing.T, condition func() bool) {
	t.Helper()
	deadline := time.Now().Add(2 * time.Second)
	for !condition() {
		if time.Now().After(deadline) {
			t.Fatal("condition did not become true")
		}
		time.Sleep(time.Millisecond)
	}
}

func TestRevocationClosesInflightAuthenticationBeforeReturn(t *testing.T) {
	backend := newFakeBackend()
	backend.authWait = true
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	_ = readServerFrame(t, connection)
	authProof(t, connection)
	waitFor(t, func() bool {
		backend.mu.Lock()
		defer backend.mu.Unlock()
		return backend.authCalls == 1
	})
	clientID := backend.authentication.Principal.ClientID
	done := make(chan error, 1)
	go func() { done <- server.CloseClient(clientID) }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("revocation did not cancel and join in-flight authentication")
	}
	server.mu.Lock()
	lifecycle := server.clientLifecycle[clientID]
	registered, inflight, revoking := 0, 0, 0
	if lifecycle != nil {
		registered = len(lifecycle.connections)
		inflight = len(lifecycle.authenticating)
		revoking = lifecycle.revoking
	}
	server.mu.Unlock()
	if registered != 0 || inflight != 0 || revoking != 0 {
		t.Fatalf("revocation did not quiesce registered=%d inflight=%d revoking=%d", registered, inflight, revoking)
	}
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if _, _, err := connection.Read(ctx); err == nil {
		t.Fatal("revoked in-flight authentication received a result")
	}
}

func TestAuthResultMustMatchRequestedClient(t *testing.T) {
	backend := newFakeBackend()
	backend.authentication.Principal.ClientID[0] ^= 0xff
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	_ = readServerFrame(t, connection)
	authProof(t, connection)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	server.mu.Lock()
	lifecycle := server.clientLifecycle[backend.authentication.Principal.ClientID]
	registered := 0
	if lifecycle != nil {
		registered = len(lifecycle.connections)
	}
	server.mu.Unlock()
	if registered != 0 {
		t.Fatalf("mismatched backend principal registered %d connections", registered)
	}
}

func TestPairResultCannotRegisterAfterExactClientRevocation(t *testing.T) {
	backend := newFakeBackend()
	backend.pairStarted = make(chan struct{})
	release := make(chan struct{})
	backend.pairRelease = release
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	_ = readServerFrame(t, connection)
	publicKey := append([]byte{4}, make([]byte, browserprotocol.PublicKeySize-1)...)
	pair, err := browserprotocol.EncodePairProve("pair", browserprotocol.PairProve{
		Challenge:     strings.Repeat("04", browserprotocol.ChallengeSize),
		PublicKeySEC1: hex.EncodeToString(publicKey),
		Signature:     strings.Repeat("05", browserprotocol.SignatureSize),
	})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, pair)
	select {
	case <-backend.pairStarted:
	case <-time.After(time.Second):
		t.Fatal("pair backend did not start")
	}
	if err := server.CloseClient(backend.authentication.Principal.ClientID); err != nil {
		t.Fatal(err)
	}
	close(release)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if _, payload, err := connection.Read(ctx); err == nil {
		t.Fatalf("revoked pairing received a result: %s", payload)
	}
}

func TestRevocationFenceIsTemporaryAndBounded(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	for value := 1; value <= 4096; value++ {
		var clientID [browserprotocol.ClientIDSize]byte
		clientID[0] = byte(value)
		clientID[1] = byte(value >> 8)
		if err := server.CloseClient(clientID); err != nil {
			t.Fatal(err)
		}
	}
	server.mu.Lock()
	lifecycles, pairingBlocked := len(server.clientLifecycle), server.pairingBlocked
	server.mu.Unlock()
	if lifecycles != 0 || pairingBlocked != 0 {
		t.Fatalf("quiescent revocation state lifecycles=%d pairing_blocked=%d", lifecycles, pairingBlocked)
	}

	// Transport fencing is not durable authorization. Once CloseClient has
	// quiesced, a later proof reaches Backend, which must enforce revocation.
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	backend.mu.Lock()
	authCalls := backend.authCalls
	backend.mu.Unlock()
	if authCalls != 1 {
		t.Fatalf("future authentication did not reach durable authority: calls=%d", authCalls)
	}
}

func TestOperationAuthorizationIsReloadedByBackend(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	backend.mu.Lock()
	backend.detailErr = ErrUnauthorized
	backend.mu.Unlock()
	request, _ := browserprotocol.EncodeHumanRequestDetailGet("detail-revoked", browserprotocol.HumanRequestDetailGet{RequestID: requestID, ExpectedRevision: 1})
	writeClientFrame(t, connection, request)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	backend.mu.Lock()
	calls := backend.detailCalls
	backend.mu.Unlock()
	if calls != 1 {
		t.Fatalf("backend authorization calls=%d want 1", calls)
	}
}

func TestBackendResponseCorrelationFailsClosed(t *testing.T) {
	t.Run("detail id and revision", func(t *testing.T) {
		for _, mutate := range []func(*browserprotocol.HumanRequestDetail){
			func(value *browserprotocol.HumanRequestDetail) { value.RequestID = projectID },
			func(value *browserprotocol.HumanRequestDetail) { value.Revision = 2 },
		} {
			backend := newFakeBackend()
			mutate(&backend.detail)
			server := startServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			request, _ := browserprotocol.EncodeHumanRequestDetailGet("detail-mismatch", browserprotocol.HumanRequestDetailGet{RequestID: requestID, ExpectedRevision: 1})
			writeClientFrame(t, connection, request)
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
		}
	})
	t.Run("terminal target identity and observation", func(t *testing.T) {
		for _, mutate := range []func(*browserprotocol.TerminalTarget){
			func(value *browserprotocol.TerminalTarget) { value.AgentID = projectID },
			func(value *browserprotocol.TerminalTarget) { value.AgentRevision = 2 },
			func(value *browserprotocol.TerminalTarget) { value.Head = 8 },
			func(value *browserprotocol.TerminalTarget) {
				value.Target = &browserprotocol.TerminalTargetDescriptor{}
			},
		} {
			backend := newFakeBackend()
			mutate(&backend.target)
			server := startServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			request, _ := browserprotocol.EncodeTerminalTargetGet("target-mismatch", browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7})
			writeClientFrame(t, connection, request)
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
			ctx, cancel := context.WithTimeout(context.Background(), time.Second)
			defer cancel()
			if _, _, err := connection.Read(ctx); err == nil {
				t.Fatal("forged terminal target left connection open")
			}
		}
	})
}

func TestTerminalTargetRoutesThroughCodecAndPreservesNull(t *testing.T) {
	backend := newFakeBackend()
	backend.target.Target = &browserprotocol.TerminalTargetDescriptor{
		RunID: strings.Repeat("02", 16), SessionID: strings.Repeat("03", 16), RunRevision: 4, SessionRevision: 5,
	}
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	requestBody := browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7}
	request, _ := browserprotocol.EncodeTerminalTargetGet("target-active", requestBody)
	writeClientFrame(t, connection, request)
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeTerminalTarget || frame.ID != "target-active" {
		t.Fatalf("target response = %+v", frame)
	}
	got := frame.Body.(browserprotocol.TerminalTarget)
	if got.AgentID != backend.target.AgentID || got.AgentRevision != backend.target.AgentRevision || got.Head != backend.target.Head || got.Target == nil || *got.Target != *backend.target.Target {
		t.Fatalf("active target = %+v, want %+v", got, backend.target)
	}
	backend.mu.Lock()
	backend.target.Target = nil
	backend.mu.Unlock()
	request, _ = browserprotocol.EncodeTerminalTargetGet("target-null", requestBody)
	writeClientFrame(t, connection, request)
	frame = readServerFrame(t, connection)
	if got := frame.Body.(browserprotocol.TerminalTarget); got.Target != nil || got.AgentID != requestBody.AgentID || got.AgentRevision != requestBody.ExpectedAgentRevision || got.Head != requestBody.ExpectedHead {
		t.Fatalf("null target = %+v", got)
	}
}

func TestTerminalTargetBackendCancellationDoesNotPublishLateResult(t *testing.T) {
	backend := newFakeBackend()
	backend.targetWait = true
	backend.targetStarted = make(chan struct{})
	backend.targetDone = make(chan struct{})
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeTerminalTargetGet("target-cancel", browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7})
	writeClientFrame(t, connection, request)
	select {
	case <-backend.targetStarted:
	case <-time.After(time.Second):
		t.Fatal("target backend did not start")
	}
	if err := server.CloseClient(backend.authentication.Principal.ClientID); err != nil {
		t.Fatal(err)
	}
	select {
	case <-backend.targetDone:
	case <-time.After(time.Second):
		t.Fatal("target backend was not cancelled after connection close")
	}
	server.mu.Lock()
	lifecycle := server.clientLifecycle[backend.authentication.Principal.ClientID]
	registered := 0
	if lifecycle != nil {
		registered = len(lifecycle.connections)
	}
	server.mu.Unlock()
	if registered != 0 {
		t.Fatalf("cancelled target retained %d connection(s)", registered)
	}
	backend.mu.Lock()
	calls := backend.targetCalls
	backend.mu.Unlock()
	if calls != 1 {
		t.Fatalf("target backend calls = %d, want one cancelled call", calls)
	}
}

func TestBackendSuccessAfterOperationDeadlineIsDiscarded(t *testing.T) {
	backend := newFakeBackend()
	backend.stateWait = true
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeStateGet("late-state", browserprotocol.StateGet{})
	writeClientFrame(t, connection, request)
	frame := readServerFrame(t, connection)
	assertError(t, frame, browserprotocol.ErrorInternal)
}

func TestLateSubscriptionIsCancelledAndNeverInstalled(t *testing.T) {
	backend := newFakeBackend()
	backend.subWait = true
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeStateWatch("late-sub", browserprotocol.StateWatch{})
	writeClientFrame(t, connection, request)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
	if backend.sub.closed.Load() != 1 {
		t.Fatalf("late subscription cleanup count=%d", backend.sub.closed.Load())
	}
}

func TestRejectedSubscriptionsAreCancelledAndJoined(t *testing.T) {
	tests := []struct {
		name  string
		setup func(*fakeBackend)
		code  browserprotocol.ErrorCode
	}{
		{
			name: "backend result plus error",
			setup: func(backend *fakeBackend) {
				backend.subErr = ErrRateLimited
			},
			code: browserprotocol.ErrorRateLimited,
		},
		{
			name: "nil updates",
			setup: func(backend *fakeBackend) {
				backend.sub.updates = nil
			},
			code: browserprotocol.ErrorInternal,
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			backend := newFakeBackend()
			test.setup(backend)
			server := startServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			request, _ := browserprotocol.EncodeStateWatch("rejected", browserprotocol.StateWatch{})
			writeClientFrame(t, connection, request)
			assertError(t, readServerFrame(t, connection), test.code)
			if got := backend.sub.closed.Load(); got != 1 {
				t.Fatalf("rejected subscription cleanup count=%d", got)
			}
		})
	}
}

func TestRejectedSubscriptionCleanupUncertaintyIsObservable(t *testing.T) {
	backend := newFakeBackend()
	backend.sub.updates = nil
	backend.sub.neverDone = true
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeStateWatch("unresolved-rejected", browserprotocol.StateWatch{})
	writeClientFrame(t, connection, request)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
	_ = connection.CloseNow()
	waitFor(t, func() bool {
		server.mu.Lock()
		defer server.mu.Unlock()
		return len(server.connections) == 0
	})
	if err := server.Close(); !errors.Is(err, ErrSubscriptionUnresolved) {
		t.Fatalf("close error=%v", err)
	}
}

func TestUnresolvedSubscriptionPoisonsEveryBoundedConnection(t *testing.T) {
	backend := newFakeBackend()
	backend.subFactory = func() StateSubscription {
		subscription := newFakeSubscription()
		subscription.updates = nil
		subscription.neverDone = true
		return subscription
	}
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = server.Close() })

	connections := make([]*websocket.Conn, 0, maxConnections)
	for range maxConnections {
		connection, _ := dialServer(t, server, testOrigin)
		authenticate(t, connection)
		connections = append(connections, connection)
	}
	first, _ := browserprotocol.EncodeStateWatch("poison", browserprotocol.StateWatch{})
	second, _ := browserprotocol.EncodeStateWatch("must-not-run", browserprotocol.StateWatch{})
	for _, connection := range connections {
		writeClientFrame(t, connection, first)
		writeClientFrame(t, connection, second)
	}
	for _, connection := range connections {
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
		ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
		_, payload, readErr := connection.Read(ctx)
		cancel()
		if readErr == nil {
			t.Fatalf("poisoned connection remained usable: %s", payload)
		}
	}
	backend.mu.Lock()
	subCalls := backend.subCalls
	backend.mu.Unlock()
	if subCalls != maxConnections {
		t.Fatalf("SubscribeState calls=%d want exactly %d", subCalls, maxConnections)
	}
	if err := server.Close(); !errors.Is(err, ErrSubscriptionUnresolved) {
		t.Fatalf("close error=%v", err)
	}
}

type nilDoneSubscription struct {
	cancelled atomic.Int32
}

func (subscription *nilDoneSubscription) Updates() <-chan StateUpdate { return nil }
func (subscription *nilDoneSubscription) Cancel()                     { subscription.cancelled.Add(1) }
func (subscription *nilDoneSubscription) Done() <-chan struct{}       { return nil }
func (subscription *nilDoneSubscription) Err() error                  { return nil }

func TestStopSubscriptionCancelsBeforeNilDoneFailure(t *testing.T) {
	subscription := &nilDoneSubscription{}
	if err := stopSubscription(subscription); !errors.Is(err, ErrSubscriptionUnresolved) {
		t.Fatalf("cleanup error=%v", err)
	}
	if got := subscription.cancelled.Load(); got != 1 {
		t.Fatalf("Cancel calls=%d want 1", got)
	}
}

func TestUncooperativeSubscriptionCleanupIsBoundedAndObservable(t *testing.T) {
	backend := newFakeBackend()
	backend.sub.neverDone = true
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	subscribe, _ := browserprotocol.EncodeStateWatch("unresolved", browserprotocol.StateWatch{})
	writeClientFrame(t, connection, subscribe)
	waitFor(t, func() bool {
		backend.mu.Lock()
		defer backend.mu.Unlock()
		return backend.subCalls == 1
	})
	started := time.Now()
	err = server.Close()
	if !errors.Is(err, ErrSubscriptionUnresolved) {
		t.Fatalf("close error=%v", err)
	}
	if elapsed := time.Since(started); elapsed > subscriptionCloseLimit+time.Second {
		t.Fatalf("uncooperative cleanup took %v", elapsed)
	}
}

type failingListener struct {
	err error
}

func (listener failingListener) Accept() (net.Conn, error) { return nil, listener.err }
func (failingListener) Close() error                       { return nil }
func (failingListener) Addr() net.Addr {
	return &net.TCPAddr{IP: net.ParseIP("127.0.0.1"), Port: 43123}
}

func TestUnexpectedServeFailureIsObservable(t *testing.T) {
	sentinel := errors.New("serve-SENTINEL")
	server := start(newFakeBackend(), map[string]struct{}{testOrigin: {}}, failingListener{err: sentinel}, time.Now)
	select {
	case <-server.ServeDone():
	case <-time.After(time.Second):
		t.Fatal("Serve failure was not observable")
	}
	if !errors.Is(server.Err(), sentinel) {
		t.Fatalf("server Err=%v", server.Err())
	}
	if err := server.Close(); !errors.Is(err, sentinel) {
		t.Fatalf("Close error=%v", err)
	}
}
