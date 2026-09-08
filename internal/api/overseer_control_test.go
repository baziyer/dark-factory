package api

import (
	"bytes"
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
