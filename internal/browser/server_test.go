package browser

import (
	"context"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

const (
	testOrigin = "https://app.darkfactory.build"
	devOrigin  = "http://127.0.0.1:3000"
	testID     = "101112131415161718191a1b1c1d1e1f"
	projectID  = "202122232425262728292a2b2c2d2e2f"
	requestID  = "303132333435363738393a3b3c3d3e3f"
)

type fakeBackend struct {
	mu sync.Mutex

	identity       Identity
	authentication Authentication
	pairErr        error
	pairStarted    chan struct{}
	pairRelease    <-chan struct{}
	authErr        error
	authWait       bool
	stateErr       error
	stateWait      bool
	detailErr      error
	subErr         error
	subWait        bool
	subFactory     func() StateSubscription
	snapshot       browserprotocol.StateSnapshot
	snapshotFunc   func(int) browserprotocol.StateSnapshot
	detail         browserprotocol.HumanRequestDetail
	target         browserprotocol.TerminalTarget
	targetErr      error
	targetWait     bool
	targetStarted  chan struct{}
	targetDone     chan struct{}
	sub            *fakeSubscription

	pairRequest PairRequest
	authRequest AuthRequest
	clients     [][16]byte
	pairCalls   int
	authCalls   int
	stateCalls  int
	detailCalls int
	targetCalls int
	subCalls    int
}

func newFakeBackend() *fakeBackend {
	backend := &fakeBackend{}
	backend.identity.DaemonID[0] = 1
	backend.identity.BootID[0] = 2
	clientBytes, _ := hex.DecodeString(testID)
	copy(backend.authentication.Principal.ClientID[:], clientBytes)
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityPrivateHumanRequestDetail
	backend.snapshot = browserprotocol.StateSnapshot{
		Head:          7,
		Factory:       browserprotocol.FactoryItem{DispatchEnabled: true, Capacity: 2, ActiveRuns: 1, Revision: 1},
		Projects:      []browserprotocol.ProjectItem{{ID: projectID, Name: "Factory", Revision: 1}},
		Agents:        []browserprotocol.AgentItem{},
		Tasks:         []browserprotocol.TaskItem{},
		HumanRequests: []browserprotocol.HumanRequestItem{},
	}
	backend.detail = browserprotocol.HumanRequestDetail{RequestID: requestID, Revision: 1, Question: "Choose one", ReplyMaxBytes: browserprotocol.MaxHumanReplyBytes}
	backend.target = browserprotocol.TerminalTarget{AgentID: strings.Repeat("01", 16), AgentRevision: 1, Head: 7}
	backend.sub = newFakeSubscription()
	return backend
}

func (backend *fakeBackend) Identity(context.Context) (Identity, error) { return backend.identity, nil }
func (backend *fakeBackend) Pair(ctx context.Context, request PairRequest) (Authentication, error) {
	backend.mu.Lock()
	backend.pairCalls++
	backend.pairRequest = request
	started, release, result, err := backend.pairStarted, backend.pairRelease, backend.authentication, backend.pairErr
	backend.mu.Unlock()
	if started != nil {
		close(started)
	}
	if release != nil {
		select {
		case <-release:
		case <-ctx.Done():
		}
	}
	return result, err
}
func (backend *fakeBackend) Authenticate(ctx context.Context, request AuthRequest) (Authentication, error) {
	backend.mu.Lock()
	backend.authCalls++
	backend.authRequest = request
	wait, result, err := backend.authWait, backend.authentication, backend.authErr
	backend.mu.Unlock()
	if wait {
		<-ctx.Done()
	}
	return result, err
}
func (backend *fakeBackend) StateSnapshot(ctx context.Context, client [16]byte) (browserprotocol.StateSnapshot, error) {
	backend.mu.Lock()
	backend.stateCalls++
	backend.clients = append(backend.clients, client)
	wait, snapshot, backendErr, snapshotFunc, call := backend.stateWait, backend.snapshot, backend.stateErr, backend.snapshotFunc, backend.stateCalls
	backend.mu.Unlock()
	if wait {
		<-ctx.Done()
	}
	if snapshotFunc != nil {
		return snapshotFunc(call), backendErr
	}
	return snapshot, backendErr
}
func (backend *fakeBackend) HumanRequestDetail(_ context.Context, client [16]byte, _ browserprotocol.HumanRequestDetailGet) (browserprotocol.HumanRequestDetail, error) {
	backend.mu.Lock()
	defer backend.mu.Unlock()
	backend.detailCalls++
	backend.clients = append(backend.clients, client)
	return backend.detail, backend.detailErr
}

func (*fakeBackend) TaskDetail(_ context.Context, _ [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskDetailGet) (browserprotocol.TaskDetail, error) {
	return browserprotocol.TaskDetail{TaskID: request.TaskID, Revision: request.ExpectedRevision, PeerQuestions: []browserprotocol.TaskPeerQuestion{}}, nil
}
func (backend *fakeBackend) TerminalTarget(ctx context.Context, client [16]byte, _ browserprotocol.TerminalTargetGet) (browserprotocol.TerminalTarget, error) {
	backend.mu.Lock()
	backend.targetCalls++
	wait, started, done := backend.targetWait, backend.targetStarted, backend.targetDone
	backend.clients = append(backend.clients, client)
	result, backendErr := backend.target, backend.targetErr
	if started != nil {
		close(started)
	}
	backend.mu.Unlock()
	if wait {
		<-ctx.Done()
	}
	if done != nil {
		close(done)
	}
	return result, backendErr
}
func (backend *fakeBackend) WatchState(ctx context.Context, client [16]byte, _ browserprotocol.Decimal) (StateSubscription, error) {
	backend.mu.Lock()
	backend.subCalls++
	backend.clients = append(backend.clients, client)
	wait, defaultSubscription, factory, err := backend.subWait, backend.sub, backend.subFactory, backend.subErr
	backend.mu.Unlock()
	if wait {
		<-ctx.Done()
	}
	var subscription StateSubscription = defaultSubscription
	if factory != nil {
		subscription = factory()
	}
	return subscription, err
}

type fakeSubscription struct {
	updates         chan StateUpdate
	once            sync.Once
	closed          atomic.Int32
	done            chan struct{}
	completionBlock <-chan struct{}
	neverDone       bool
	err             error
}

func newFakeSubscription() *fakeSubscription {
	return &fakeSubscription{updates: make(chan StateUpdate, 8), done: make(chan struct{})}
}
func (subscription *fakeSubscription) Updates() <-chan StateUpdate { return subscription.updates }
func (subscription *fakeSubscription) Cancel() {
	subscription.once.Do(func() {
		if subscription.neverDone {
			return
		}
		finish := func() {
			if subscription.completionBlock != nil {
				<-subscription.completionBlock
			}
			subscription.closed.Add(1)
			close(subscription.done)
		}
		if subscription.completionBlock == nil {
			finish()
		} else {
			go finish()
		}
	})
}
func (subscription *fakeSubscription) Done() <-chan struct{} { return subscription.done }
func (subscription *fakeSubscription) Err() error            { return subscription.err }

func startServer(t *testing.T, backend *fakeBackend) *Server {
	t.Helper()
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin, devOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := server.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return server
}

func dialServer(t *testing.T, server *Server, origin string) (*websocket.Conn, *http.Response) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), backendCallLimit+2*time.Second)
	defer cancel()
	header := make(http.Header)
	header.Set("Origin", origin)
	connection, response, err := websocket.Dial(ctx, "ws://"+server.Addr()+Path, &websocket.DialOptions{
		HTTPHeader: header, CompressionMode: websocket.CompressionNoContextTakeover,
	})
	if err != nil {
		status := 0
		if response != nil {
			status = response.StatusCode
		}
		t.Fatalf("dial status=%d: %v", status, err)
	}
	t.Cleanup(func() { _ = connection.CloseNow() })
	return connection, response
}

// dialStalledObserver dials with a deliberately small receive buffer. The test
// that uses it never reads, so the server's writes stall after a bounded number
// of frames instead of after whatever the kernel is willing to buffer.
func dialStalledObserver(t *testing.T, server *Server) *websocket.Conn {
	t.Helper()
	dialer := &net.Dialer{Control: func(_, _ string, raw syscall.RawConn) error {
		var setErr error
		if err := raw.Control(func(handle uintptr) {
			setErr = syscall.SetsockoptInt(int(handle), syscall.SOL_SOCKET, syscall.SO_RCVBUF, 4096)
		}); err != nil {
			return err
		}
		return setErr
	}}
	ctx, cancel := context.WithTimeout(context.Background(), backendCallLimit+2*time.Second)
	defer cancel()
	header := make(http.Header)
	header.Set("Origin", testOrigin)
	connection, _, err := websocket.Dial(ctx, "ws://"+server.Addr()+Path, &websocket.DialOptions{
		HTTPClient:      &http.Client{Transport: &http.Transport{DialContext: dialer.DialContext}},
		HTTPHeader:      header,
		CompressionMode: websocket.CompressionNoContextTakeover,
	})
	if err != nil {
		t.Fatalf("dial: %v", err)
	}
	t.Cleanup(func() { _ = connection.CloseNow() })
	return connection
}

func readServerFrame(t *testing.T, connection *websocket.Conn) browserprotocol.ControlFrame {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), backendCallLimit+2*time.Second)
	defer cancel()
	kind, payload, err := connection.Read(ctx)
	if err != nil {
		t.Fatalf("read: %v", err)
	}
	if kind != websocket.MessageText {
		t.Fatalf("message type=%v", kind)
	}
	frame, err := browserprotocol.DecodeServerControl(payload)
	if err != nil {
		t.Fatalf("decode %q: %v", payload, err)
	}
	return frame
}

func writeClientFrame(t *testing.T, connection *websocket.Conn, payload []byte) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if err := connection.Write(ctx, websocket.MessageText, payload); err != nil {
		t.Fatalf("write: %v", err)
	}
}

func authenticate(t *testing.T, connection *websocket.Conn) browserprotocol.Hello {
	t.Helper()
	helloFrame := readServerFrame(t, connection)
	if helloFrame.Type != browserprotocol.TypeHello {
		t.Fatalf("first frame=%s", helloFrame.Type)
	}
	hello := helloFrame.Body.(browserprotocol.Hello)
	proof, err := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{
		ClientID: testID, Signature: strings.Repeat("01", browserprotocol.SignatureSize),
	})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, proof)
	result := readServerFrame(t, connection)
	if result.Type != browserprotocol.TypeAuthResult || result.ID != "auth" {
		t.Fatalf("auth result=%+v", result)
	}
	return hello
}

func assertError(t *testing.T, frame browserprotocol.ControlFrame, code browserprotocol.ErrorCode) {
	t.Helper()
	if frame.Type != browserprotocol.TypeError {
		t.Fatalf("type=%s want ERROR", frame.Type)
	}
	value := frame.Body.(browserprotocol.Error)
	if value.Code != code {
		t.Fatalf("code=%s want %s", value.Code, code)
	}
}

func TestListenRejectsNonLoopbackAndUnsafeOriginConfig(t *testing.T) {
	t.Parallel()
	backend := newFakeBackend()
	for _, address := range []string{"0.0.0.0:0", ":0", "localhost:0", "[::1]:0", "192.168.1.10:0", "127.0.0.1:01", "127.0.0.1"} {
		if server, err := Listen(Config{Address: address, AllowedOrigins: []string{testOrigin}, Backend: backend}); err == nil {
			_ = server.Close()
			t.Fatalf("accepted address %q", address)
		}
	}
	badOrigins := [][]string{nil, {}, {"*"}, {"https://*.darkfactory.build"}, {"null"}, {"HTTPS://app.darkfactory.build"}, {testOrigin + "/"}, {testOrigin, testOrigin}}
	for _, origins := range badOrigins {
		if server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: origins, Backend: backend}); err == nil {
			_ = server.Close()
			t.Fatalf("accepted origins %q", origins)
		}
	}
}

func TestListenRefusesOccupiedPort(t *testing.T) {
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer listener.Close()
	if server, err := Listen(Config{Address: listener.Addr().String(), AllowedOrigins: []string{testOrigin}, Backend: newFakeBackend()}); err == nil {
		_ = server.Close()
		t.Fatal("accepted occupied port")
	}
}

func TestUpgradePolicyIsExactAndCompressionDisabled(t *testing.T) {
	server := startServer(t, newFakeBackend())
	connection, response := dialServer(t, server, testOrigin)
	if extension := response.Header.Get("Sec-WebSocket-Extensions"); extension != "" {
		t.Fatalf("compression negotiated: %q", extension)
	}
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeHello {
		t.Fatalf("first frame=%s", frame.Type)
	}

	cases := []struct {
		name   string
		path   string
		host   string
		origin []string
		want   int
	}{
		{"wrong path", "/", server.Addr(), []string{testOrigin}, http.StatusNotFound},
		{"encoded path", "/browser%2Fv1", server.Addr(), []string{testOrigin}, http.StatusNotFound},
		{"query", Path + "?challenge=secret", server.Addr(), []string{testOrigin}, http.StatusNotFound},
		{"wrong host", Path, "localhost:" + strings.Split(server.Addr(), ":")[1], []string{testOrigin}, http.StatusForbidden},
		{"missing origin", Path, server.Addr(), nil, http.StatusForbidden},
		{"duplicate origin", Path, server.Addr(), []string{testOrigin, testOrigin}, http.StatusForbidden},
		{"case variant", Path, server.Addr(), []string{"https://APP.darkfactory.build"}, http.StatusForbidden},
		{"cross site", Path, server.Addr(), []string{"https://attacker.invalid"}, http.StatusForbidden},
		{"null", Path, server.Addr(), []string{"null"}, http.StatusForbidden},
		{"wildcard", Path, server.Addr(), []string{"*"}, http.StatusForbidden},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			status := rawHTTPStatus(t, server.Addr(), test.path, test.host, test.origin, "")
			if status != test.want {
				t.Fatalf("status=%d want %d", status, test.want)
			}
		})
	}
	large := strings.Repeat("x", 64<<10)
	status := rawHTTPStatus(t, server.Addr(), Path, server.Addr(), []string{testOrigin}, large)
	if status != http.StatusRequestHeaderFieldsTooLarge {
		t.Fatalf("oversized header status=%d", status)
	}
	for _, test := range []struct {
		name   string
		origin []string
		host   string
	}{
		{"cross-site WebSocket", []string{"https://attacker.invalid"}, server.Addr()},
		{"missing WebSocket Origin", nil, server.Addr()},
		{"duplicate WebSocket Origin", []string{testOrigin, testOrigin}, server.Addr()},
		{"wrong WebSocket Host", []string{testOrigin}, "localhost:" + strings.Split(server.Addr(), ":")[1]},
	} {
		t.Run(test.name, func(t *testing.T) {
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
			defer cancel()
			header := make(http.Header)
			for _, value := range test.origin {
				header.Add("Origin", value)
			}
			client, response, err := websocket.Dial(ctx, "ws://"+server.Addr()+Path, &websocket.DialOptions{HTTPHeader: header, Host: test.host})
			if client != nil {
				_ = client.CloseNow()
			}
			if err == nil || response == nil || response.StatusCode != http.StatusForbidden {
				t.Fatalf("hijack attempt err=%v response=%v", err, response)
			}
		})
	}
}

func rawHTTPStatus(t *testing.T, address, path, host string, origins []string, extra string) int {
	t.Helper()
	connection, err := net.DialTimeout("tcp4", address, 2*time.Second)
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	var request strings.Builder
	fmt.Fprintf(&request, "GET %s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n", path, host)
	for _, origin := range origins {
		fmt.Fprintf(&request, "Origin: %s\r\n", origin)
	}
	if extra != "" {
		fmt.Fprintf(&request, "X-Oversized: %s\r\n", extra)
	}
	request.WriteString("\r\n")
	if _, err := io.WriteString(connection, request.String()); err != nil {
		t.Fatal(err)
	}
	_ = connection.SetReadDeadline(time.Now().Add(2 * time.Second))
	var protocol string
	var status int
	if _, err := fmt.Fscanf(connection, "%s %d", &protocol, &status); err != nil {
		t.Fatal(err)
	}
	return status
}

func TestHelloPairAndAuthBindValidatedRequest(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	hello := readServerFrame(t, connection).Body.(browserprotocol.Hello)
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
	if result := readServerFrame(t, connection); result.Type != browserprotocol.TypePairResult {
		t.Fatalf("pair result=%s", result.Type)
	}
	backend.mu.Lock()
	request := backend.pairRequest
	backend.mu.Unlock()
	if request.Host != server.Addr() || request.Origin != testOrigin || hex.EncodeToString(request.ConnectionNonce[:]) != hello.ConnectionNonce || request.Identity != backend.identity {
		t.Fatalf("pair transcript inputs not request-bound: %+v", request)
	}
	authConnection, _ := dialServer(t, server, testOrigin)
	authHello := authenticate(t, authConnection)
	backend.mu.Lock()
	authRequest := backend.authRequest
	backend.mu.Unlock()
	if authRequest.Host != server.Addr() || authRequest.Origin != testOrigin || hex.EncodeToString(authRequest.ConnectionNonce[:]) != authHello.ConnectionNonce || authRequest.Identity != backend.identity {
		t.Fatalf("auth transcript inputs not request-bound: %+v", authRequest)
	}
}

func TestProofFailuresAndUnsupportedCapabilitiesFailClosed(t *testing.T) {
	for _, name := range []string{"expired challenge", "replayed challenge", "wrong proof", "revoked client"} {
		t.Run(name, func(t *testing.T) {
			backend := newFakeBackend()
			backend.authErr = ErrUnauthorized
			server := startServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			_ = readServerFrame(t, connection)
			proof, _ := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{ClientID: testID, Signature: strings.Repeat("01", 64)})
			writeClientFrame(t, connection, proof)
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
		})
	}
	for _, capability := range []browserprotocol.Capabilities{1 << 12, browserprotocol.CapabilityObserve | (1 << 12)} {
		backend := newFakeBackend()
		backend.authentication.Capabilities = capability
		server := startServer(t, backend)
		connection, _ := dialServer(t, server, testOrigin)
		_ = readServerFrame(t, connection)
		proof, _ := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{ClientID: testID, Signature: strings.Repeat("01", 64)})
		writeClientFrame(t, connection, proof)
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	}
}

func TestMalformedOversizedBinaryAndSecondAuthAreRejected(t *testing.T) {
	t.Run("unauthenticated state", func(t *testing.T) {
		backend := newFakeBackend()
		server := startServer(t, backend)
		connection, _ := dialServer(t, server, testOrigin)
		_ = readServerFrame(t, connection)
		request, _ := browserprotocol.EncodeStateGet("before-auth", browserprotocol.StateGet{})
		writeClientFrame(t, connection, request)
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
		ctx, cancel := context.WithTimeout(context.Background(), time.Second)
		defer cancel()
		if err := connection.Write(ctx, websocket.MessageText, request); err == nil {
			_, _, err = connection.Read(ctx)
			if err == nil {
				t.Fatal("unauthenticated connection remained usable")
			}
		}
		backend.mu.Lock()
		calls := backend.stateCalls
		backend.mu.Unlock()
		if calls != 0 {
			t.Fatalf("unauthenticated state reached backend %d times", calls)
		}
	})
	t.Run("malformed", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		_ = readServerFrame(t, connection)
		writeClientFrame(t, connection, []byte(`{"type":"AUTH_PROVE"}`))
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
	})
	// A site artifact that still sends the deleted envelope generation is
	// served: the member is unknown, so it is ignored like any other.
	t.Run("legacy envelope generation", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		_ = readServerFrame(t, connection)
		writeClientFrame(t, connection, []byte(`{"v":1,"type":"AUTH_PROVE","id":"auth","body":{"client_id":"`+testID+`","signature":"`+strings.Repeat("01", browserprotocol.SignatureSize)+`"}}`))
		result := readServerFrame(t, connection)
		if result.Type != browserprotocol.TypeAuthResult || result.ID != "auth" {
			t.Fatalf("legacy envelope member refused: %+v", result)
		}
	})
	t.Run("oversized", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		_ = readServerFrame(t, connection)
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		_ = connection.Write(ctx, websocket.MessageText, make([]byte, browserprotocol.MaxControlBytes+1))
		_, _, err := connection.Read(ctx)
		if err == nil || websocket.CloseStatus(err) != websocket.StatusMessageTooBig {
			t.Fatalf("oversized read err=%v status=%v", err, websocket.CloseStatus(err))
		}
	})
	t.Run("binary", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		authenticate(t, connection)
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		if err := connection.Write(ctx, websocket.MessageBinary, []byte{1}); err != nil {
			t.Fatal(err)
		}
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
	})
	t.Run("second auth", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		authenticate(t, connection)
		proof, _ := browserprotocol.EncodeAuthProve("auth-2", browserprotocol.AuthProve{ClientID: testID, Signature: strings.Repeat("01", 64)})
		writeClientFrame(t, connection, proof)
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
	})
}

func TestAuthenticationDeadline(t *testing.T) {
	server := startServer(t, newFakeBackend())
	connection, _ := dialServer(t, server, testOrigin)
	_ = readServerFrame(t, connection)
	started := time.Now()
	ctx, cancel := context.WithTimeout(context.Background(), authenticationLimit+2*time.Second)
	defer cancel()
	kind, payload, err := connection.Read(ctx)
	if err != nil || kind != websocket.MessageText {
		t.Fatalf("deadline read kind=%v err=%v", kind, err)
	}
	frame, err := browserprotocol.DecodeServerControl(payload)
	if err != nil {
		t.Fatal(err)
	}
	assertError(t, frame, browserprotocol.ErrorUnauthorized)
	if elapsed := time.Since(started); elapsed < authenticationLimit/2 || elapsed > authenticationLimit+2*time.Second {
		t.Fatalf("deadline elapsed=%v", elapsed)
	}
}

func TestAuthenticationDeadlineIncludesBackendProof(t *testing.T) {
	backend := newFakeBackend()
	backend.authWait = true
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	_ = readServerFrame(t, connection)
	proof, _ := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{ClientID: testID, Signature: strings.Repeat("01", 64)})
	started := time.Now()
	writeClientFrame(t, connection, proof)
	ctx, cancel := context.WithTimeout(context.Background(), authenticationLimit+2*time.Second)
	defer cancel()
	kind, payload, err := connection.Read(ctx)
	if err != nil || kind != websocket.MessageText {
		t.Fatalf("proof deadline read kind=%v err=%v", kind, err)
	}
	frame, err := browserprotocol.DecodeServerControl(payload)
	if err != nil {
		t.Fatal(err)
	}
	assertError(t, frame, browserprotocol.ErrorUnauthorized)
	if elapsed := time.Since(started); elapsed < authenticationLimit/2 || elapsed > authenticationLimit+2*time.Second {
		t.Fatalf("proof deadline elapsed=%v", elapsed)
	}
	server.mu.Lock()
	lifecycle := server.clientLifecycle[backend.authentication.Principal.ClientID]
	registered := 0
	if lifecycle != nil {
		registered = len(lifecycle.connections)
	}
	server.mu.Unlock()
	if registered != 0 {
		t.Fatalf("timed-out proof registered %d connections", registered)
	}
}

func TestStateOperationsChronologyCapabilitiesAndRedaction(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)

	state, _ := browserprotocol.EncodeStateGet("state", browserprotocol.StateGet{})
	writeClientFrame(t, connection, state)
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeStateSnapshot || frame.ID != "state" {
		t.Fatalf("snapshot=%+v", frame)
	}
	snapshot := frame.Body.(browserprotocol.StateSnapshot)
	if snapshot.Head != 7 || len(snapshot.Projects) != 1 {
		t.Fatalf("snapshot=%+v", snapshot)
	}

	detailRequest, _ := browserprotocol.EncodeHumanRequestDetailGet("detail", browserprotocol.HumanRequestDetailGet{RequestID: requestID, ExpectedRevision: 1})
	writeClientFrame(t, connection, detailRequest)
	if detail := readServerFrame(t, connection); detail.Type != browserprotocol.TypeHumanRequestDetail {
		t.Fatalf("detail=%+v", detail)
	}

	watch, _ := browserprotocol.EncodeStateWatch("watch", browserprotocol.StateWatch{AfterHead: 7})
	writeClientFrame(t, connection, watch)
	backend.sub.updates <- StateUpdate{Head: 8}
	changed := readServerFrame(t, connection)
	if changed.Type != browserprotocol.TypeStateChanged || changed.ID != "watch" || changed.Body.(browserprotocol.StateChanged).Head != 8 {
		t.Fatalf("change=%+v", changed)
	}

	backend.mu.Lock()
	clients := append([][16]byte(nil), backend.clients...)
	backend.mu.Unlock()
	for _, client := range clients {
		if client != backend.authentication.Principal.ClientID {
			t.Fatalf("operation used non-principal client %x", client)
		}
	}

	backend.stateErr = errors.New("operator-token-SENTINEL provider-secret-SENTINEL")
	redactedRequest, _ := browserprotocol.EncodeStateGet("redacted", browserprotocol.StateGet{})
	writeClientFrame(t, connection, redactedRequest)
	redacted := readServerFrame(t, connection)
	assertError(t, redacted, browserprotocol.ErrorInternal)
	encoded := fmt.Sprintf("%+v", redacted)
	if strings.Contains(encoded, "SENTINEL") {
		t.Fatalf("private error escaped: %s", encoded)
	}
}

// The contract tolerates additive change. A newer console may add a member to
// a frame this daemon does not know and still be served. Tolerance is additive
// only: the same frame is still refused when it exceeds the control bound.
func TestServerServesFramesCarryingUnknownMembers(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)

	writeClientFrame(t, connection, []byte(`{"type":"STATE_GET","id":"state","future":{"added":true},"body":{"future_selector":"ignored"}}`))
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeStateSnapshot || frame.ID != "state" {
		t.Fatalf("tolerant STATE_GET answered with %+v", frame)
	}
	if snapshot := frame.Body.(browserprotocol.StateSnapshot); snapshot.Head != 7 || len(snapshot.Projects) != 1 {
		t.Fatalf("snapshot=%+v", snapshot)
	}

	padded := fmt.Sprintf(`{"type":"STATE_GET","id":"big","future":%q,"body":{}}`, strings.Repeat("x", browserprotocol.MaxControlBytes))
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	_ = connection.Write(ctx, websocket.MessageText, []byte(padded))
	if _, _, err := connection.Read(ctx); err == nil || websocket.CloseStatus(err) != websocket.StatusMessageTooBig {
		t.Fatalf("oversized tolerant frame err=%v status=%v", err, websocket.CloseStatus(err))
	}
}

// Only members are tolerated silently. An unknown frame type is refused by
// name as unsupported; a server-direction frame arriving from a client is a
// violation. Neither reaches the backend.
func TestServerRefusesUnknownTypesAndWrongDirection(t *testing.T) {
	for name, test := range map[string]struct {
		payload string
		code    browserprotocol.ErrorCode
	}{
		"unknown type":    {`{"type":"STATE_FUTURE","id":"x","body":{}}`, browserprotocol.ErrorUnsupported},
		"wrong direction": {`{"type":"STATE_CHANGED","id":"x","body":{"head":"8"}}`, browserprotocol.ErrorInvalidRequest},
	} {
		t.Run(name, func(t *testing.T) {
			backend := newFakeBackend()
			server := startServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			writeClientFrame(t, connection, []byte(test.payload))
			assertError(t, readServerFrame(t, connection), test.code)
			backend.mu.Lock()
			calls := backend.stateCalls
			backend.mu.Unlock()
			if calls != 0 {
				t.Fatalf("refused frame reached the backend %d times", calls)
			}
		})
	}
}

// A head that repeats or regresses within one watch is a backend fault. The
// transport must not forward it, and the connection ends rather than letting a
// client see a non-monotonic chronology.
func TestStateChangeHeadsAreStrictlyIncreasing(t *testing.T) {
	for _, second := range []browserprotocol.Decimal{7, 6, 0} {
		backend := newFakeBackend()
		server := startServer(t, backend)
		connection, _ := dialServer(t, server, testOrigin)
		authenticate(t, connection)
		watch, _ := browserprotocol.EncodeStateWatch("watch", browserprotocol.StateWatch{AfterHead: 6})
		writeClientFrame(t, connection, watch)
		backend.sub.updates <- StateUpdate{Head: 7}
		if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeStateChanged {
			t.Fatalf("first change=%+v", frame)
		}
		backend.sub.updates <- StateUpdate{Head: second}
		if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeError {
			t.Fatalf("non-monotonic head %d produced %+v", second, frame)
		}
	}
}

// An oversized snapshot is a finite too_large answer. The client never sees a
// truncated snapshot, and the connection stays usable.
func TestOversizedSnapshotIsAFiniteTooLargeAnswer(t *testing.T) {
	backend := newFakeBackend()
	oversized := browserprotocol.StateSnapshot{
		Head: 7, Factory: browserprotocol.FactoryItem{DispatchEnabled: true, Capacity: 2, ActiveRuns: 1, Revision: 1},
		Projects: []browserprotocol.ProjectItem{}, Agents: []browserprotocol.AgentItem{},
		Tasks: []browserprotocol.TaskItem{}, HumanRequests: []browserprotocol.HumanRequestItem{},
	}
	for index := 1; index <= 2048; index++ {
		raw := make([]byte, 16)
		raw[14], raw[15] = byte(index/251), byte(index%251)
		raw[0] = 0xa3
		oversized.Tasks = append(oversized.Tasks, browserprotocol.TaskItem{
			ID: fmt.Sprintf("%x", raw), ProjectID: projectID, AssignedAgentID: testID,
			Title: strings.Repeat("t", browserprotocol.MaxTaskTitleBytes), Status: "queued", Priority: 1, Revision: 1,
		})
	}
	backend.snapshotFunc = func(call int) browserprotocol.StateSnapshot {
		if call == 1 {
			return oversized
		}
		return backend.snapshot
	}
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeStateGet("too-large", browserprotocol.StateGet{})
	writeClientFrame(t, connection, request)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorTooLarge)
	// The refusal is finite: the same connection still serves a fitting snapshot.
	again, _ := browserprotocol.EncodeStateGet("fits", browserprotocol.StateGet{})
	writeClientFrame(t, connection, again)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeStateSnapshot {
		t.Fatalf("connection did not survive a too_large answer: %+v", frame)
	}
}

func TestFiniteBackendErrorMapping(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	for index, backendErr := range []error{ErrNotFound, ErrStale, ErrTooLarge} {
		backend.mu.Lock()
		backend.stateErr = backendErr
		backend.mu.Unlock()
		request, _ := browserprotocol.EncodeStateGet(fmt.Sprintf("state-%d", index), browserprotocol.StateGet{})
		writeClientFrame(t, connection, request)
		frame := readServerFrame(t, connection)
		want := []browserprotocol.ErrorCode{browserprotocol.ErrorNotFound, browserprotocol.ErrorStale, browserprotocol.ErrorTooLarge}[index]
		assertError(t, frame, want)
	}
}

func TestPrivateDetailRequiresCapabilityBeforeBackend(t *testing.T) {
	backend := newFakeBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve
	backend.detailErr = ErrUnauthorized
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	request, _ := browserprotocol.EncodeHumanRequestDetailGet("detail", browserprotocol.HumanRequestDetailGet{RequestID: requestID, ExpectedRevision: 1})
	writeClientFrame(t, connection, request)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	backend.mu.Lock()
	calls := backend.detailCalls
	backend.mu.Unlock()
	if calls != 1 {
		t.Fatalf("detail authority was not reloaded exactly once: %d", calls)
	}
}

func TestDuplicateRequestIDAndConnectionLimit(t *testing.T) {
	t.Run("duplicate", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connection, _ := dialServer(t, server, testOrigin)
		authenticate(t, connection)
		request, _ := browserprotocol.EncodeStateGet("same", browserprotocol.StateGet{})
		writeClientFrame(t, connection, request)
		_ = readServerFrame(t, connection)
		writeClientFrame(t, connection, request)
		assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
	})
	t.Run("connection cap", func(t *testing.T) {
		server := startServer(t, newFakeBackend())
		connections := make([]*websocket.Conn, 0, maxConnections)
		for index := 0; index < maxConnections; index++ {
			connection, _ := dialServer(t, server, testOrigin)
			_ = readServerFrame(t, connection)
			connections = append(connections, connection)
		}
		ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
		defer cancel()
		header := make(http.Header)
		header.Set("Origin", testOrigin)
		connection, response, err := websocket.Dial(ctx, "ws://"+server.Addr()+Path, &websocket.DialOptions{HTTPHeader: header})
		if connection != nil {
			_ = connection.CloseNow()
		}
		if err == nil || response == nil || response.StatusCode != http.StatusServiceUnavailable {
			t.Fatalf("cap dial err=%v response=%v", err, response)
		}
	})
}

// startHeldClockServer starts a server whose request window is measured on
// a clock held still until the test moves it; the serve goroutine reads it
// through the atomic.
func startHeldClockServer(t *testing.T, backend Backend) (*Server, *atomic.Int64) {
	t.Helper()
	var clock atomic.Int64
	clock.Store(time.Unix(1_700_000_000, 0).UnixNano())
	listener, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	server := start(backend, map[string]struct{}{testOrigin: {}, devOrigin: {}}, listener, func() time.Time { return time.Unix(0, clock.Load()) })
	t.Cleanup(func() {
		if err := server.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	return server, &clock
}

func TestRequestBudgetIsASlidingWindow(t *testing.T) {
	backend := newFakeBackend()
	// The window is measured on the server's clock, given before the server
	// starts and held still so the fill cannot age out under a slow gate,
	// then moved by hand; the serve goroutine reads it through the atomic.
	server, clock := startHeldClockServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	// Authentication consumes one ID of the window. Exactly maxRequests-1
	// distinct operations may then complete; the next is rejected without
	// backend work.
	for index := 0; index < maxRequests-1; index++ {
		request, _ := browserprotocol.EncodeStateGet(fmt.Sprintf("request-%d", index), browserprotocol.StateGet{})
		writeClientFrame(t, connection, request)
		if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeStateSnapshot {
			t.Fatalf("request %d response=%+v", index, frame)
		}
	}
	request, _ := browserprotocol.EncodeStateGet("over-budget", browserprotocol.StateGet{})
	writeClientFrame(t, connection, request)
	frame := readServerFrame(t, connection)
	assertError(t, frame, browserprotocol.ErrorRateLimited)
	if !bool(frame.Body.(browserprotocol.Error).Retryable) {
		t.Fatal("rate limit was not marked retryable")
	}
	backend.mu.Lock()
	calls := backend.stateCalls
	backend.mu.Unlock()
	if calls != maxRequests-1 {
		t.Fatalf("over-budget request reached backend: calls=%d", calls)
	}
	// The refusal spent nothing and closed nothing. Once the window has
	// passed the same connection has budget again, and the id it refused is
	// admitted like any other.
	clock.Add(int64(requestWindow))
	request, _ = browserprotocol.EncodeStateGet("over-budget", browserprotocol.StateGet{})
	writeClientFrame(t, connection, request)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeStateSnapshot || frame.ID != "over-budget" {
		t.Fatalf("request after the window = %+v", frame)
	}
}

func TestCloseClientAndServerJoinConnectionsWithoutReauthorization(t *testing.T) {
	baseline := runtime.NumGoroutine()
	backend := newFakeBackend()
	server := startServer(t, backend)
	first, _ := dialServer(t, server, testOrigin)
	authenticate(t, first)
	second, _ := dialServer(t, server, testOrigin)
	authenticate(t, second)
	backend.mu.Lock()
	authCalls := backend.authCalls
	backend.mu.Unlock()
	done := make(chan struct{})
	go func() {
		_ = server.CloseClient(backend.authentication.Principal.ClientID)
		close(done)
	}()
	select {
	case <-done:
	case <-time.After(3 * time.Second):
		t.Fatal("CloseClient did not join")
	}
	backend.mu.Lock()
	if backend.authCalls != authCalls {
		t.Fatalf("revocation seam reauthorized: %d -> %d", authCalls, backend.authCalls)
	}
	backend.mu.Unlock()
	server.mu.Lock()
	lifecycle := server.clientLifecycle[backend.authentication.Principal.ClientID]
	clientConnections := 0
	if lifecycle != nil {
		clientConnections = len(lifecycle.connections)
	}
	server.mu.Unlock()
	if clientConnections != 0 {
		t.Fatalf("client registry retained %d connections", clientConnections)
	}
	if err := server.Close(); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(2 * time.Second)
	for runtime.NumGoroutine() > baseline+4 && time.Now().Before(deadline) {
		runtime.Gosched()
		time.Sleep(10 * time.Millisecond)
	}
	if got := runtime.NumGoroutine(); got > baseline+4 {
		t.Fatalf("goroutine census baseline=%d after=%d", baseline, got)
	}
}

func TestServerCloseWaitsForConnectionOwnedSubscriptionJoin(t *testing.T) {
	backend := newFakeBackend()
	release := make(chan struct{})
	backend.sub.completionBlock = release
	server := startServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	subscribe, _ := browserprotocol.EncodeStateWatch("sub", browserprotocol.StateWatch{})
	writeClientFrame(t, connection, subscribe)
	deadline := time.Now().Add(time.Second)
	for {
		backend.mu.Lock()
		calls := backend.subCalls
		backend.mu.Unlock()
		if calls == 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("subscription was not installed")
		}
		time.Sleep(time.Millisecond)
	}
	closed := make(chan struct{})
	go func() {
		_ = server.Close()
		close(closed)
	}()
	select {
	case <-closed:
		t.Fatal("Server.Close returned before subscription joined")
	case <-time.After(50 * time.Millisecond):
	}
	close(release)
	select {
	case <-closed:
	case <-time.After(2 * time.Second):
		t.Fatal("Server.Close did not finish after subscription joined")
	}
	server.mu.Lock()
	remaining := len(server.connections)
	server.mu.Unlock()
	if remaining != 0 {
		t.Fatalf("server retained %d connections after Close", remaining)
	}
}

func TestSlowSubscriberCannotGrowTransportMemoryOrBlockShutdown(t *testing.T) {
	backend := newFakeBackend()
	server := startServer(t, backend)
	// The observer's receive window is pinned small so the stall is caused by
	// the observer, not by how much this machine's loopback happens to absorb.
	// With an auto-tuned window the kernel buffers tens of megabytes and the
	// proof silently becomes a race between the producer and the buffer.
	connection := dialStalledObserver(t, server)
	authenticate(t, connection)
	subscribe, _ := browserprotocol.EncodeStateWatch("slow", browserprotocol.StateWatch{AfterHead: 0})
	writeClientFrame(t, connection, subscribe)
	deadline := time.Now().Add(2 * time.Second)
	for {
		backend.mu.Lock()
		calls := backend.subCalls
		backend.mu.Unlock()
		if calls == 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("subscription was not installed")
		}
		time.Sleep(time.Millisecond)
	}
	producerDone := make(chan struct{})
	producerContext, cancelProducer := context.WithTimeout(context.Background(), writeLimit+3*time.Second)
	defer cancelProducer()
	go func() {
		defer close(producerDone)
		for sequence := uint64(1); sequence <= 100_000; sequence++ {
			select {
			case backend.sub.updates <- StateUpdate{Head: browserprotocol.Decimal(sequence)}:
			case <-backend.sub.done:
				return
			case <-producerContext.Done():
				return
			}
		}
	}()
	select {
	case <-backend.sub.done:
	case <-time.After(writeLimit + 5*time.Second):
		t.Fatal("slow observer was not disconnected")
	}
	select {
	case <-producerDone:
	case <-time.After(time.Second):
		t.Fatal("bounded producer did not stop")
	}
	joinDeadline := time.Now().Add(time.Second)
	for {
		server.mu.Lock()
		remaining := len(server.connections)
		server.mu.Unlock()
		if remaining == 0 {
			break
		}
		if time.Now().After(joinDeadline) {
			t.Fatalf("slow connection remained registered: %d", remaining)
		}
		time.Sleep(time.Millisecond)
	}
}
