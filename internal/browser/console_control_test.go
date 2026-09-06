package browser

import (
	"context"
	"encoding/hex"
	"fmt"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

type consoleDispatchBackend struct {
	*fakeBackend
	mu       sync.Mutex
	client   [browserprotocol.ClientIDSize]byte
	agent    browserprotocol.AgentUpdateResult
	task     browserprotocol.TaskUpdateResult
	topology browserprotocol.Topology
	account  browserprotocol.AccountLinkResult
	err      error
	calls    int
	walking  chan struct{} // when set, Topology and RunPaths block until it is closed, whatever the context says
	budgets  []time.Time   // each walk's context deadline, in the order the walks started
}

func newConsoleDispatchBackend() *consoleDispatchBackend {
	base := newFakeBackend()
	base.authentication.Capabilities |= browserprotocol.CapabilityHumanActions
	backend := &consoleDispatchBackend{fakeBackend: base}
	backend.agent = browserprotocol.AgentUpdateResult{AgentID: consoleAgentID, Revision: 8}
	backend.task = browserprotocol.TaskUpdateResult{TaskID: consoleTaskID, Revision: 4}
	backend.account = browserprotocol.AccountLinkResult{AccountID: consoleAccountID, Revision: 1}
	backend.topology = browserprotocol.Topology{
		ProjectID: consoleProjectID, Digest: strings.Repeat("ab", 32),
		Nodes: []browserprotocol.TopologyNode{{ID: strings.Repeat("a1", 32), Kind: "repository", Path: ".", Label: "repository", SizeBucket: "small"}},
	}
	return backend
}

func (backend *consoleDispatchBackend) record(client [browserprotocol.ClientIDSize]byte) error {
	backend.mu.Lock()
	defer backend.mu.Unlock()
	backend.calls++
	backend.client = client
	return backend.err
}

func (backend *consoleDispatchBackend) observed() (int, [browserprotocol.ClientIDSize]byte) {
	backend.mu.Lock()
	defer backend.mu.Unlock()
	return backend.calls, backend.client
}

func (backend *consoleDispatchBackend) UpdateAgent(_ context.Context, client [browserprotocol.ClientIDSize]byte, _ browserprotocol.AgentUpdate) (browserprotocol.AgentUpdateResult, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.AgentUpdateResult{}, err
	}
	return backend.agent, nil
}

func (backend *consoleDispatchBackend) UpdateTask(_ context.Context, client [browserprotocol.ClientIDSize]byte, _ browserprotocol.TaskUpdate) (browserprotocol.TaskUpdateResult, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.TaskUpdateResult{}, err
	}
	return backend.task, nil
}

func (backend *consoleDispatchBackend) Topology(ctx context.Context, client [browserprotocol.ClientIDSize]byte, _ browserprotocol.TopologyGet) (browserprotocol.Topology, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.Topology{}, err
	}
	if err := backend.walk(ctx); err != nil {
		return browserprotocol.Topology{}, err
	}
	return backend.topology, nil
}

func (backend *consoleDispatchBackend) RunPaths(ctx context.Context, client [browserprotocol.ClientIDSize]byte, request browserprotocol.RunPathsGet) (browserprotocol.RunPaths, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.RunPaths{}, err
	}
	if err := backend.walk(ctx); err != nil {
		return browserprotocol.RunPaths{}, err
	}
	return browserprotocol.RunPaths{AgentID: request.AgentID, Paths: []string{}}, nil
}

func (backend *consoleDispatchBackend) walk(ctx context.Context) error {
	backend.mu.Lock()
	walking := backend.walking
	deadline, _ := ctx.Deadline()
	backend.budgets = append(backend.budgets, deadline)
	backend.mu.Unlock()
	if walking != nil {
		<-walking
	}
	return nil
}

func (backend *consoleDispatchBackend) setWalking(walking chan struct{}) {
	backend.mu.Lock()
	backend.walking = walking
	backend.mu.Unlock()
}

// setErr is for a backend a live connection may be reading right now.
func (backend *consoleDispatchBackend) setErr(err error) {
	backend.mu.Lock()
	backend.err = err
	backend.mu.Unlock()
}

func (backend *consoleDispatchBackend) DiscoverAccounts(_ context.Context, client [browserprotocol.ClientIDSize]byte) (browserprotocol.Accounts, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.Accounts{}, err
	}
	return browserprotocol.Accounts{Accounts: []browserprotocol.DiscoveredAccount{
		{Provider: "codex", Home: "/Users/operator/.codex", Label: ".codex"},
	}}, nil
}

func (backend *consoleDispatchBackend) LinkAccount(_ context.Context, client [browserprotocol.ClientIDSize]byte, _ browserprotocol.AccountLink) (browserprotocol.AccountLinkResult, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.AccountLinkResult{}, err
	}
	return backend.account, nil
}

const (
	consoleAgentID   = "606162636465666768696a6b6c6d6e6f"
	consoleTaskID    = "404142434445464748494a4b4c4d4e4f"
	consoleProjectID = "505152535455565758595a5b5c5d5e5f"
	consoleAccountID = "707172737475767778797a7b7c7d7e7f"
)

// The console requests as a browser sends them. Only the server direction has
// exported encoders, so these are the wire bytes themselves.
var consoleRequests = []struct {
	request, reply browserprotocol.MessageType
	frame          string
}{
	{browserprotocol.TypeAgentUpdate, browserprotocol.TypeAgentUpdateResult,
		`{"type":"AGENT_UPDATE","id":"console-agent","body":{"agent_id":"` + consoleAgentID + `","expected_revision":"7","paused":true}}`},
	{browserprotocol.TypeTaskUpdate, browserprotocol.TypeTaskUpdateResult,
		`{"type":"TASK_UPDATE","id":"console-task","body":{"task_id":"` + consoleTaskID + `","expected_revision":"3","status":"cancelled"}}`},
	{browserprotocol.TypeTopologyGet, browserprotocol.TypeTopology,
		`{"type":"TOPOLOGY_GET","id":"console-topology","body":{"project_id":"` + consoleProjectID + `"}}`},
	{browserprotocol.TypeRunPathsGet, browserprotocol.TypeRunPaths,
		`{"type":"RUN_PATHS_GET","id":"console-rooms","body":{"agent_id":"` + consoleAgentID + `"}}`},
	{browserprotocol.TypeAccountsDiscover, browserprotocol.TypeAccounts,
		`{"type":"ACCOUNTS_DISCOVER","id":"console-accounts","body":{}}`},
	{browserprotocol.TypeAccountLink, browserprotocol.TypeAccountLinkResult,
		`{"type":"ACCOUNT_LINK","id":"console-account-link","body":{"provider":"codex","home":"/Users/operator/.codex","label":"codex"}}`},
}

func TestConsoleControlDispatchesAndCorrelatesExactResults(t *testing.T) {
	backend := newConsoleDispatchBackend()
	server := startTaskServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	wantClient, err := hex.DecodeString(testID)
	if err != nil {
		t.Fatal(err)
	}
	for _, expected := range consoleRequests {
		writeClientFrame(t, connection, []byte(expected.frame))
		frame := readServerFrame(t, connection)
		if frame.Type != expected.reply || !strings.HasPrefix(frame.ID, "console-") {
			t.Fatalf("%s answered with %+v", expected.request, frame)
		}
		if _, client := backend.observed(); string(client[:]) != string(wantClient) {
			t.Fatalf("%s reached the backend as client %x", expected.request, client)
		}
	}
	if calls, _ := backend.observed(); calls != len(consoleRequests) {
		t.Fatalf("backend calls = %d, want %d", calls, len(consoleRequests))
	}
}

func TestConsoleControlFailsClosedWithoutBackendAndOnBackendRefusal(t *testing.T) {
	// A daemon without the console half answers unauthorized rather than
	// silently accepting an operator mutation it cannot perform.
	t.Run("missing optional backend", func(t *testing.T) {
		for _, expected := range consoleRequests {
			server := startTaskServer(t, newFakeBackend())
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			writeClientFrame(t, connection, []byte(expected.frame))
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
		}
	})
	// The backend reloads durable authority per operation, so its refusal is
	// the authority. The transport forwards the exact kind.
	t.Run("backend refuses", func(t *testing.T) {
		for _, outcome := range []struct {
			err  error
			code browserprotocol.ErrorCode
		}{{ErrUnauthorized, browserprotocol.ErrorUnauthorized}, {ErrStale, browserprotocol.ErrorStale}, {ErrRateLimited, browserprotocol.ErrorRateLimited}, {ErrInvalidRequest, browserprotocol.ErrorInvalidRequest}} {
			for _, expected := range consoleRequests {
				backend := newConsoleDispatchBackend()
				backend.err = outcome.err
				server := startTaskServer(t, backend)
				connection, _ := dialServer(t, server, testOrigin)
				authenticate(t, connection)
				writeClientFrame(t, connection, []byte(expected.frame))
				assertError(t, readServerFrame(t, connection), outcome.code)
			}
		}
	})
	// A result that answers a different entity, or that fails to advance the
	// revision the client observed, is an internal fault and not an answer.
	t.Run("mismatched backend result", func(t *testing.T) {
		for _, corrupt := range []struct {
			request browserprotocol.MessageType
			mutate  func(*consoleDispatchBackend)
		}{
			{browserprotocol.TypeAgentUpdate, func(backend *consoleDispatchBackend) { backend.agent.Revision = 7 }},
			{browserprotocol.TypeAgentUpdate, func(backend *consoleDispatchBackend) { backend.agent.AgentID = consoleTaskID }},
			{browserprotocol.TypeTaskUpdate, func(backend *consoleDispatchBackend) { backend.task.Revision = 3 }},
			{browserprotocol.TypeTaskUpdate, func(backend *consoleDispatchBackend) { backend.task.TaskID = consoleAgentID }},
			{browserprotocol.TypeTopologyGet, func(backend *consoleDispatchBackend) { backend.topology.ProjectID = consoleAgentID }},
		} {
			backend := newConsoleDispatchBackend()
			corrupt.mutate(backend)
			server := startTaskServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			writeClientFrame(t, connection, []byte(consoleFrame(t, corrupt.request)))
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
		}
	})
}

func consoleFrame(t *testing.T, kind browserprotocol.MessageType) string {
	t.Helper()
	for _, expected := range consoleRequests {
		if expected.request == kind {
			return expected.frame
		}
	}
	t.Fatalf("no console frame for %s", kind)
	return ""
}

// TOPOLOGY_GET and RUN_PATHS_GET may walk a tree under the call budget. The
// connection keeps serving while they do, and a refusal from that path still
// ends it the way dispatch would.
func TestTreeWalksDoNotStallTheConnection(t *testing.T) {
	backend := newConsoleDispatchBackend()
	first := make(chan struct{})
	backend.walking = first
	server, clock := startHeldClockServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	writeClientFrame(t, connection, []byte(consoleFrame(t, browserprotocol.TypeTopologyGet)))
	writeClientFrame(t, connection, []byte(consoleFrame(t, browserprotocol.TypeRunPathsGet)))
	// The first walk is blocked and the second waits behind it; a state read
	// on the same connection is answered anyway.
	state, _ := browserprotocol.EncodeStateGet("state-during-walk", browserprotocol.StateGet{})
	writeClientFrame(t, connection, state)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeStateSnapshot {
		t.Fatalf("state during walks = %+v", frame)
	}
	// Each walk's budget starts with its work, not when its frame arrived:
	// the first walk is held for a spell, and the second, released on its
	// own, carries a deadline at least that much later. A budget taken at
	// dispatch would put the two deadlines a frame apart.
	for deadline := time.Now().Add(3 * time.Second); ; time.Sleep(5 * time.Millisecond) {
		if now, _ := backend.observed(); now >= 1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the first walk never started")
		}
	}
	const hold = 50 * time.Millisecond
	second := make(chan struct{})
	backend.setWalking(second)
	time.Sleep(hold)
	close(first)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTopology {
		t.Fatalf("first walk answer = %+v", frame)
	}
	close(second)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeRunPaths {
		t.Fatalf("second walk answer = %+v", frame)
	}
	backend.mu.Lock()
	budgets := append([]time.Time(nil), backend.budgets...)
	backend.mu.Unlock()
	if len(budgets) != 2 || budgets[1].Sub(budgets[0]) < hold {
		t.Fatalf("walk budgets = %v; the second should start at least %v after the first", budgets, hold)
	}
	// The backend refusal path still ends the connection as dispatch would.
	backend.setWalking(nil)
	backend.setErr(ErrUnauthorized)
	writeClientFrame(t, connection, []byte(strings.Replace(consoleFrame(t, browserprotocol.TypeTopologyGet), "console-topology", "console-topology-2", 1)))
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	if _, _, err := connection.Read(ctx); err == nil || ctx.Err() != nil {
		t.Fatalf("unauthorized walk left the connection open: err=%v ctx=%v", err, ctx.Err())
	}

	// Walks are answered one at a time in arrival order and the rest wait:
	// a window's worth may queue, so a normal round is never refused. Past
	// that, a walk the window would admit is refused as retryable without
	// reaching the backend, and the one in flight still holds.
	backend.setErr(nil)
	walking := make(chan struct{})
	backend.setWalking(walking)
	// A failure below must not leave the walk blocked for the cleanup's join.
	var released sync.Once
	release := func() { released.Do(func() { close(walking) }) }
	t.Cleanup(release)
	held, _ := dialServer(t, server, testOrigin)
	authenticate(t, held)
	calls, _ := backend.observed()
	walkFrame := func(id string) []byte {
		return []byte(strings.Replace(consoleFrame(t, browserprotocol.TypeRunPathsGet), "console-rooms", id, 1))
	}
	// Authentication spent one id of the window; the rest all go to walks.
	for index := range maxRequests - 1 {
		writeClientFrame(t, held, walkFrame(fmt.Sprintf("console-rooms-%d", index)))
	}
	// The first walk is in flight once the backend has been asked once.
	for deadline := time.Now().Add(3 * time.Second); ; time.Sleep(5 * time.Millisecond) {
		if now, _ := backend.observed(); now >= calls+1 {
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("the walk never started")
		}
	}
	// Two slots remain behind it. The window slides, so the next three are
	// admitted; the third finds the queue full.
	clock.Add(int64(requestWindow))
	writeClientFrame(t, held, walkFrame("console-rooms-fill-1"))
	writeClientFrame(t, held, walkFrame("console-rooms-fill-2"))
	writeClientFrame(t, held, []byte(consoleFrame(t, browserprotocol.TypeTopologyGet)))
	over := readServerFrame(t, held)
	assertError(t, over, browserprotocol.ErrorRateLimited)
	if over.ID != "console-topology" || !bool(over.Body.(browserprotocol.Error).Retryable) {
		t.Fatalf("walk past the queue = %+v", over)
	}
	if now, _ := backend.observed(); now != calls+1 {
		t.Fatalf("queued walks reached the backend: calls=%d", now-calls)
	}
	// A walk still running when the server closes is joined, not leaked:
	// Close cannot finish while the walker holds the connection's cleanup.
	closed := make(chan struct{})
	go func() {
		_ = server.Close()
		close(closed)
	}()
	select {
	case <-closed:
		t.Fatal("server close did not wait for the walk in flight")
	case <-time.After(200 * time.Millisecond):
	}
	release()
	select {
	case <-closed:
	case <-time.After(3 * time.Second):
		t.Fatal("server close did not finish once the walk ended")
	}
}

// A verb this build does not know is refused by its id and nothing else
// changes: the socket, the budget and every known verb are as they were.
func TestUnknownControlTypeIsRefusedByIDAndKeepsTheConnection(t *testing.T) {
	server := startTaskServer(t, newConsoleDispatchBackend())
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	writeClientFrame(t, connection, []byte(`{"type":"FUTURE_VERB","id":"console-future","body":{"account_id":"x"}}`))
	reply := readServerFrame(t, connection)
	assertError(t, reply, browserprotocol.ErrorUnsupported)
	if reply.ID != "console-future" || reply.Body.(browserprotocol.Error).Retryable {
		t.Fatalf("unsupported reply = %+v", reply)
	}
	// The socket is still open and the verbs this build knows still work.
	writeClientFrame(t, connection, []byte(consoleFrame(t, browserprotocol.TypeAgentUpdate)))
	if reply := readServerFrame(t, connection); reply.Type != browserprotocol.TypeAgentUpdateResult {
		t.Fatalf("known request after unsupported = %+v", reply)
	}
	// The refusal spent the id: repeating it is the transport's invalid_request.
	writeClientFrame(t, connection, []byte(`{"type":"FUTURE_VERB","id":"console-future","body":{}}`))
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
}

// invalid_request reaches a client by two routes that differ in what happens
// next. A member the backend refuses is one bad answer on a connection that
// keeps working; a frame the transport itself refuses ends the connection.
// Observing only one of them would let the backend route become the harsh one
// without anything noticing.
func TestConsoleInvalidRequestKeepsTheConnectionTheTransportWouldClose(t *testing.T) {
	backend := newConsoleDispatchBackend()
	backend.err = ErrInvalidRequest
	server := startTaskServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticate(t, connection)
	writeClientFrame(t, connection, []byte(consoleFrame(t, browserprotocol.TypeAgentUpdate)))
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)
	// The same connection still answers, so the refusal was about the member.
	writeClientFrame(t, connection, []byte(consoleFrame(t, browserprotocol.TypeTaskUpdate)))
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInvalidRequest)

	// The transport's own invalid_request, from a repeated request id, ends it.
	closing, _ := dialServer(t, startTaskServer(t, newConsoleDispatchBackend()), testOrigin)
	authenticate(t, closing)
	frame := []byte(consoleFrame(t, browserprotocol.TypeAgentUpdate))
	writeClientFrame(t, closing, frame)
	if reply := readServerFrame(t, closing); reply.Type != browserprotocol.TypeAgentUpdateResult {
		t.Fatalf("first request = %+v", reply)
	}
	writeClientFrame(t, closing, frame)
	assertError(t, readServerFrame(t, closing), browserprotocol.ErrorInvalidRequest)
	ctx, cancel := context.WithTimeout(context.Background(), 3*time.Second)
	defer cancel()
	// The transport closes abruptly, so a client sees EOF and no close status.
	// What has to be observed is that the read ended for some reason other than
	// this deadline: a connection merely left idle would expire here instead.
	if _, _, err := closing.Read(ctx); err == nil || ctx.Err() != nil {
		t.Fatalf("a repeated request id left the connection open: err=%v ctx=%v", err, ctx.Err())
	}
}
