package browser

import (
	"context"
	"strings"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

type agentControlTestBackend struct {
	*fakeBackend
	mismatch bool
}

var _ AgentControlBackend = (*agentControlTestBackend)(nil)

func (b *agentControlTestBackend) ControlAgent(_ context.Context, _ Principal, r browserprotocol.AgentControl) (browserprotocol.AgentControlResult, error) {
	result := browserprotocol.AgentControlResult{OperationID: r.OperationID, TaskID: r.TaskID, RunID: r.RunID, Status: "delivered"}
	if b.mismatch {
		result.OperationID = strings.Repeat("04", 16)
	}
	return result, nil
}
func (b *agentControlTestBackend) TaskHistory(_ context.Context, _ [16]byte, r browserprotocol.TaskHistoryGet) (browserprotocol.TaskHistory, error) {
	return browserprotocol.TaskHistory{TaskID: r.TaskID, Entries: []browserprotocol.TaskHistoryEntry{}}, nil
}

func TestAgentControlTransportCorrelatesDurableOperation(t *testing.T) {
	request := browserprotocol.AgentControl{OperationID: strings.Repeat("01", 16), TaskID: strings.Repeat("02", 16), RunID: strings.Repeat("03", 16), ExpectedTaskRevision: 1, ExpectedRunRevision: 2, Action: "interrupt"}
	for _, mismatch := range []bool{false, true} {
		t.Run(map[bool]string{false: "exact", true: "wrong operation"}[mismatch], func(t *testing.T) {
			backend := &agentControlTestBackend{fakeBackend: newFakeBackend(), mismatch: mismatch}
			server := startTaskServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticate(t, connection)
			payload, err := browserprotocol.EncodeAgentControl("control", request)
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, payload)
			frame := readServerFrame(t, connection)
			if mismatch {
				assertError(t, frame, browserprotocol.ErrorInternal)
				return
			}
			if frame.Type != browserprotocol.TypeAgentControlResult || frame.ID != "control" || frame.Body.(browserprotocol.AgentControlResult).OperationID != request.OperationID {
				t.Fatalf("result: %+v", frame)
			}
			payload, err = browserprotocol.EncodeTaskHistoryGet("history", browserprotocol.TaskHistoryGet{TaskID: request.TaskID})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, payload)
			frame = readServerFrame(t, connection)
			if frame.Type != browserprotocol.TypeTaskHistory || frame.ID != "history" || frame.Body.(browserprotocol.TaskHistory).TaskID != request.TaskID {
				t.Fatalf("history: %+v", frame)
			}
			payload, err = browserprotocol.EncodeTaskDetailGet("detail", browserprotocol.TaskDetailGet{TaskID: request.TaskID, ExpectedRevision: request.ExpectedTaskRevision})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, payload)
			frame = readServerFrame(t, connection)
			if frame.Type != browserprotocol.TypeTaskDetail || frame.ID != "detail" || frame.Body.(browserprotocol.TaskDetail).TaskID != request.TaskID {
				t.Fatalf("detail: %+v", frame)
			}
		})
	}
}
