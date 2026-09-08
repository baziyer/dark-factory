package kernel

import (
	"bytes"
	"context"
	"errors"
	"testing"
)

func TestTaskInterventionAttemptReceiptPreventsPendingReplay(t *testing.T) {
	ctx := context.Background()
	store, runningWorker, _ := runningWorkerRun(t)
	defer store.Close()
	overseerAgent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 90), ProjectID: runningWorker.ProjectID, Name: "overseer", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	queued, found, err := store.Task(ctx, runningWorker.TaskID)
	if err != nil || !found {
		t.Fatalf("worker task = %+v, found=%v, err=%v", queued, found, err)
	}
	if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 91), ProjectID: overseerAgent.ProjectID, AssignedAgentID: overseerAgent.ID, IncarnationID: incarnationID(t, 92), Title: "supervise"}, mustTime(t, 41)); err != nil {
		t.Fatal(err)
	}
	overseerKeys := admissionKeys(t, 93, nil)
	admission, err := store.AdmitNext(ctx, overseerKeys, mustTime(t, 42))
	if err != nil || !admission.Admitted() {
		t.Fatalf("overseer admission = %+v, %v", admission, err)
	}
	runningOverseer := activateAllResourcesUnique(t, store, *admission.Run, 43, 500)
	runningOverseer, found, err = store.Run(ctx, runningOverseer.ID)
	if err != nil || !found {
		t.Fatalf("overseer after resources = %+v, found=%v, err=%v", runningOverseer, found, err)
	}
	session := terminalSessionForRunTest(t, store, runningOverseer.ID)
	runningOverseer, err = store.ActivateRun(ctx, runningOverseer.ID, session.ID, runningOverseer.Revision, session.Revision, mustTime(t, 50))
	if err != nil {
		t.Fatal(err)
	}
	operation, err := TaskInterventionIDFromBytes(taskID(t, 94).Bytes())
	if err != nil {
		t.Fatal(err)
	}
	request := TaskInterventionRequest{OperationID: operation, TaskID: queued.ID, RunID: runningWorker.ID, ExpectedTaskRevision: queued.Revision, ExpectedRunRevision: runningWorker.Revision, Kind: TaskInterventionMessage, Payload: "Please stop after the current command."}
	receipt, reserved, err := store.ReserveTaskInterventionForAttempt(ctx, runningOverseer.CredentialDigest, request, mustTime(t, 60))
	if err != nil || !reserved || receipt.State != TaskInterventionPending {
		t.Fatalf("reserve = %+v, reserved=%v, err=%v", receipt, reserved, err)
	}
	if replay, reserved, err := store.ReserveTaskInterventionForAttempt(ctx, runningOverseer.CredentialDigest, request, mustTime(t, 61)); err != nil || reserved || replay.OperationID != receipt.OperationID || replay.State != TaskInterventionPending {
		t.Fatalf("pending replay = %+v, reserved=%v, err=%v", replay, reserved, err)
	}
	unknown, err := store.ResolveTaskIntervention(ctx, operation, TaskInterventionUnknown, "delivery uncertain", mustTime(t, 62))
	if err != nil || unknown.State != TaskInterventionUnknown || unknown.TerminalAt == nil {
		t.Fatalf("unknown outcome = %+v, %v", unknown, err)
	}
	if _, err := store.ResolveTaskIntervention(ctx, operation, TaskInterventionDelivered, "", mustTime(t, 63)); !errors.Is(err, ErrConflict) {
		t.Fatalf("conflicting terminal replay = %v", err)
	}
	history, err := store.TaskInterventions(ctx, runningWorker.ProjectID, queued.ID)
	if err != nil || len(history) != 1 || history[0].OperationID != unknown.OperationID || history[0].State != unknown.State {
		t.Fatalf("history = %+v, %v", history, err)
	}
	foreign, err := AttemptDigestFromBytes(bytes.Repeat([]byte{0xee}, DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	otherOperation, err := TaskInterventionIDFromBytes(taskID(t, 95).Bytes())
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.ReserveTaskInterventionForAttempt(ctx, foreign, TaskInterventionRequest{OperationID: otherOperation, TaskID: queued.ID, RunID: runningWorker.ID, ExpectedTaskRevision: request.ExpectedTaskRevision, ExpectedRunRevision: request.ExpectedRunRevision, Kind: TaskInterventionInterrupt}, mustTime(t, 64)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("unauthenticated attempt = %v", err)
	}
	if _, _, err := store.ReserveTaskInterventionForAttempt(ctx, overseerKeys.AttemptDigest, TaskInterventionRequest{OperationID: operation, TaskID: queued.ID, RunID: runningWorker.ID, ExpectedTaskRevision: request.ExpectedTaskRevision, ExpectedRunRevision: request.ExpectedRunRevision, Kind: TaskInterventionInterrupt}, mustTime(t, 64)); !errors.Is(err, ErrConflict) {
		// The same operation id with a different immutable payload is refused
		// before any second terminal effect can be issued.
		t.Fatalf("changed replay = %v", err)
	}
}
