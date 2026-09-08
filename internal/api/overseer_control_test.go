package api

import (
	"bytes"
	"encoding/json"
	"fmt"
	"strings"
	"testing"
)

func TestOverseerControlMethodsRemainAttemptScoped(t *testing.T) {
	var bearer credential
	for index := range bearer {
		bearer[index] = byte(index + 1)
	}
	params := []byte(`{"operation_id":"11111111111111111111111111111111","task_id":"22222222222222222222222222222222","expected_task_revision":1,"run_id":"33333333333333333333333333333333","expected_run_revision":1,"message":"continue"}`)
	request := append([]byte(`{"method":"overseer_message_worker","params":`), params...)
	request = append(request, '}')
	call, code := decodeCall(attemptDomain, bearer, request)
	if code != "" || call.Kind() != CallOverseerMessageWorker {
		t.Fatalf("attempt control = %v, %v", call.Kind(), code)
	}
	if _, ok := call.AttemptDigest(); !ok {
		t.Fatal("attempt control omitted its bound digest")
	}
	if _, code := decodeCall(operatorDomain, bearer, request); code != RemoteForbidden {
		t.Fatalf("operator control domain = %v", code)
	}
	if _, code := decodeCall(attemptDomain, bearer, bytes.Replace(request, []byte("overseer_message_worker"), []byte("overseer_unknown_worker"), 1)); code != RemoteInvalidRequest {
		t.Fatalf("unknown control = %v", code)
	}
}

func TestMutationReplyValidatesOverseerHumanReplyState(t *testing.T) {
	valid := MutationResult{Head: 3, Revision: 2, HumanReply: &OverseerHumanReplyResult{RequestID: "11111111111111111111111111111111", State: "delivery_unknown"}}
	if _, err := NewMutationReply(valid); err != nil {
		t.Fatalf("valid delivery state = %v", err)
	}
	invalid := valid
	invalid.HumanReply = &OverseerHumanReplyResult{RequestID: valid.HumanReply.RequestID, State: "resolved_or_unknown"}
	if _, err := NewMutationReply(invalid); err == nil {
		t.Fatal("invalid delivery state accepted")
	}
}

func TestOverseerSnapshotPagesFitTheResponseFrameAfterEscaping(t *testing.T) {
	project := strings.Repeat("1", 32)
	for _, selected := range []bool{false, true} {
		snapshot := OverseerSnapshot{ProjectID: project, Head: 1, Agents: []AgentSummary{}, Tasks: []OverseerTask{}, Runs: []OverseerRun{}, Questions: []OverseerQuestion{}, History: []OverseerIntervention{}}
		for index := 0; index < 4; index++ {
			id := fmt.Sprintf("%032x", index+2)
			snapshot.Agents = append(snapshot.Agents, AgentSummary{ID: id, ProjectID: project, Name: strings.Repeat("<", 128), Role: "worker", Provider: "codex", Revision: 1})
			snapshot.Runs = append(snapshot.Runs, OverseerRun{ID: id, AgentID: id, TaskID: id, Phase: "running", Revision: 1})
			snapshot.Questions = append(snapshot.Questions, OverseerQuestion{ID: id, AgentID: id, TaskID: id, Status: "open", Revision: 1, Question: strings.Repeat("<", 8192)})
			snapshot.History = append(snapshot.History, OverseerIntervention{OperationID: id, TaskID: id, RunID: id, Kind: "message", Actor: "orchestrator", Payload: strings.Repeat("<", 8192), State: "delivered", Detail: strings.Repeat("<", 4096), CreatedAtMs: 1})
			if !selected || index == 0 {
				text := 1024
				if selected {
					text = 4096
				}
				snapshot.Tasks = append(snapshot.Tasks, OverseerTask{ID: id, ProjectID: project, AssignedAgentID: id, Title: strings.Repeat("<", 1024), Objective: strings.Repeat("<", text), Status: "queued", BlockedReason: strings.Repeat("<", 8192), Result: strings.Repeat("<", text), Revision: 1})
			}
		}
		reply, err := NewOverseerSnapshotReply(snapshot)
		if err != nil {
			t.Fatal(err)
		}
		encoded, err := json.Marshal(reply.overseer)
		if err != nil {
			t.Fatal(err)
		}
		if len(encoded)+responsePrelude > maxFrameBytes {
			t.Fatalf("selected=%t encoded page is %d bytes", selected, len(encoded)+responsePrelude)
		}
	}
}

func TestOverseerSnapshotReplyKeepsEmptyCollections(t *testing.T) {
	snapshot := OverseerSnapshot{
		ProjectID: strings.Repeat("1", 32), Head: 1,
		Agents: []AgentSummary{}, Tasks: []OverseerTask{}, Runs: []OverseerRun{}, Questions: []OverseerQuestion{}, History: []OverseerIntervention{},
	}
	reply, err := NewOverseerSnapshotReply(snapshot)
	if err != nil {
		t.Fatal(err)
	}
	encoded, err := json.Marshal(reply.overseer)
	if err != nil {
		t.Fatal(err)
	}
	var decoded OverseerSnapshot
	if err := decodeExact(encoded, &decoded); err != nil || !validOverseerSnapshot(decoded) {
		t.Fatalf("empty snapshot round trip = %v, %#v", err, decoded)
	}
}

func TestOverseerSnapshotContinuationRequiresTaskForText(t *testing.T) {
	id := strings.Repeat("1", 32)
	if !validOverseerSnapshotInput(OverseerSnapshotInput{TaskID: id, Offset: 4, TextOffset: 4096, ExpectedHead: 7}) {
		t.Fatal("valid continuation rejected")
	}
	if validOverseerSnapshotInput(OverseerSnapshotInput{TextOffset: 1, ExpectedHead: 7}) {
		t.Fatal("unselected text continuation accepted")
	}
}
