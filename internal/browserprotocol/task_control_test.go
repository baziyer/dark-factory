package browserprotocol

import (
	"strings"
	"testing"
)

func TestTaskEnqueueControlBoundsAndDirection(t *testing.T) {
	request := TaskEnqueue{TaskID: strings.Repeat("01", 16), IncarnationID: strings.Repeat("02", 16), AgentID: strings.Repeat("03", 16), ExpectedAgentRevision: 7, Instruction: "Run the focused smoke test."}
	wire, err := EncodeTaskEnqueue("enqueue-1", request)
	if err != nil {
		t.Fatal(err)
	}
	frame, err := DecodeClientControl(wire)
	if err != nil || frame.Type != TypeTaskEnqueue || frame.Body.(TaskEnqueue).Instruction != request.Instruction {
		t.Fatalf("request round-trip = %+v, err=%v", frame, err)
	}
	if _, err := DecodeServerControl(wire); err != ErrMalformed {
		t.Fatalf("client request crossed server decoder: %v", err)
	}
	resultWire, err := EncodeTaskEnqueueResult("enqueue-1", TaskEnqueueResult{TaskID: request.TaskID, Revision: 1, AgentRevision: request.ExpectedAgentRevision})
	if err != nil {
		t.Fatal(err)
	}
	if _, err := DecodeServerControl(resultWire); err != nil {
		t.Fatal(err)
	}
	for _, instruction := range []string{"", strings.Repeat("x", MaxTaskInstructionBytes+1)} {
		request.Instruction = instruction
		if _, err := EncodeTaskEnqueue("enqueue-2", request); err == nil {
			t.Fatalf("invalid instruction accepted: %q", instruction)
		}
	}
	request.Instruction = "\u0085"
	if _, err := EncodeTaskEnqueue("enqueue-3", request); err != nil {
		t.Fatalf("non-ASCII whitespace drifted from the TypeScript contract: %v", err)
	}
}

func TestAgentControlRejectsAmbiguousObjectives(t *testing.T) {
	request := AgentControl{OperationID: strings.Repeat("01", 16), TaskID: strings.Repeat("02", 16), RunID: strings.Repeat("03", 16), ExpectedTaskRevision: 1, ExpectedRunRevision: 2, Action: "interrupt"}
	for _, action := range []string{"interrupt", "stop", "message", "replace"} {
		request.Action = action
		request.Instruction = ""
		request.SuccessorTaskID = ""
		request.SuccessorIncarnationID = ""
		if action == "message" || action == "replace" {
			request.Instruction = "A new direction"
		}
		if action == "replace" {
			request.SuccessorTaskID = strings.Repeat("04", 16)
			request.SuccessorIncarnationID = strings.Repeat("05", 16)
		}
		wire, err := EncodeAgentControl("control", request)
		if err != nil {
			t.Fatal(err)
		}
		if _, err := DecodeClientControl(wire); err != nil {
			t.Fatal(err)
		}
		if _, err := DecodeServerControl(wire); err == nil {
			t.Fatal("crossed direction")
		}
	}
	request.SuccessorTaskID = request.TaskID
	if _, err := EncodeAgentControl("control", request); err == nil {
		t.Fatal("replacement reused task identity")
	}
	request.Action = "interrupt"
	if _, err := EncodeAgentControl("control", request); err == nil {
		t.Fatal("interrupt accepted objective payload")
	}
}
