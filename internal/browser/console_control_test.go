package browser

import (
	"context"
	"encoding/hex"
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
	err      error
	calls    int
}

func newConsoleDispatchBackend() *consoleDispatchBackend {
	base := newFakeBackend()
	base.authentication.Capabilities |= browserprotocol.CapabilityHumanActions
	backend := &consoleDispatchBackend{fakeBackend: base}
	backend.agent = browserprotocol.AgentUpdateResult{AgentID: consoleAgentID, Revision: 8}
	backend.task = browserprotocol.TaskUpdateResult{TaskID: consoleTaskID, Revision: 4}
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

func (backend *consoleDispatchBackend) Topology(_ context.Context, client [browserprotocol.ClientIDSize]byte, _ browserprotocol.TopologyGet) (browserprotocol.Topology, error) {
	if err := backend.record(client); err != nil {
		return browserprotocol.Topology{}, err
	}
	return backend.topology, nil
}

const (
	consoleAgentID   = "606162636465666768696a6b6c6d6e6f"
	consoleTaskID    = "404142434445464748494a4b4c4d4e4f"
	consoleProjectID = "505152535455565758595a5b5c5d5e5f"
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
