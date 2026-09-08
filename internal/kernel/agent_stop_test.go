package kernel

import (
	"bytes"
	"context"
	"errors"
	"testing"
)

func TestReplaceTaskIsAtomicAndReplayBindsObjective(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 220, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	task, _, err := store.Task(ctx, run.TaskID)
	if err != nil {
		t.Fatal(err)
	}
	operation, err := TaskInterventionIDFromBytes(bytes.Repeat([]byte{221}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	request := TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: run.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: run.Revision, Kind: TaskInterventionReplace}
	successor := NewTask{ID: taskID(t, 222), IncarnationID: incarnationID(t, 223), Body: "New objective"}
	stale := request
	stale.ExpectedRunRevision = mustRevision(t, run.Revision.Int64()+1)
	if _, err := store.StopRunForBrowser(ctx, client.ID, stale, &successor, mustTime(t, 400)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale replacement: %v", err)
	}
	if _, found, err := store.Task(ctx, successor.ID); err != nil || found {
		t.Fatalf("stale replacement left a successor: %v %v", found, err)
	}
	receipt, err := store.StopRunForBrowser(ctx, client.ID, request, &successor, mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	next, found, err := store.Task(ctx, successor.ID)
	if err != nil || !found || next.Status != TaskQueued || next.Body != successor.Body || next.ProjectID != task.ProjectID || next.AssignedAgentID != task.AssignedAgentID {
		t.Fatalf("successor: %+v %v", next, err)
	}
	old, _, err := store.Run(ctx, run.ID)
	if err != nil || old.Phase != RunFinalizing || old.CredentialRevokedAt == nil || receipt.State != TaskInterventionDelivered {
		t.Fatalf("old objective still running: %+v %v", old, err)
	}
	before, _ := store.Factory(ctx)
	replay, err := store.StopRunForBrowser(ctx, client.ID, request, &successor, mustTime(t, 402))
	if err != nil || replay.OperationID != receipt.OperationID {
		t.Fatalf("exact replay: %+v %v", replay, err)
	}
	after, _ := store.Factory(ctx)
	if before.Head != after.Head {
		t.Fatal("replay changed durable state")
	}
	successor.Body = "Different objective"
	if _, err := store.StopRunForBrowser(ctx, client.ID, request, &successor, mustTime(t, 403)); !errors.Is(err, ErrConflict) {
		t.Fatalf("changed objective replay: %v", err)
	}
	history, err := store.TaskInterventions(ctx, task.ProjectID, task.ID)
	if err != nil || len(history) != 1 || history[0].SuccessorTaskID == nil || *history[0].SuccessorTaskID != successor.ID {
		t.Fatalf("replacement history: %+v %v", history, err)
	}
}

func TestStopTaskRevalidatesBrowserAuthority(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 220, BrowserCapabilityObserve)
	task, _, _ := store.Task(ctx, run.TaskID)
	operation, _ := TaskInterventionIDFromBytes(bytes.Repeat([]byte{221}, IDBytes))
	request := TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: run.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: run.Revision, Kind: TaskInterventionStop}
	if _, err := store.StopRunForBrowser(ctx, client.ID, request, nil, mustTime(t, 400)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("unauthorized stop: %v", err)
	}
	current, _, _ := store.Run(ctx, run.ID)
	if current.Phase != RunRunning || current.Revision != run.Revision {
		t.Fatal("unauthorized stop changed run")
	}
}

func TestStopRunForAttemptTargetsOnlyWorkers(t *testing.T) {
	ctx := context.Background()
	store, worker, overseer, _ := runningWorkerAndOverseer(t)
	defer store.Close()
	task, found, err := store.Task(ctx, worker.TaskID)
	if err != nil || !found {
		t.Fatalf("worker task = %+v, found=%v, err=%v", task, found, err)
	}
	operation, err := TaskInterventionIDFromBytes(bytes.Repeat([]byte{224}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	withLegacyOrchestratorTarget(t, store, worker.ID, func(tx *writeTx) {
		_, err := store.stopRunTx(ctx, tx, TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: worker.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: worker.Revision, Actor: TaskInterventionOrchestrator, ActorRunID: &overseer.ID, Kind: TaskInterventionStop}, nil, mustTime(t, 400))
		if !errors.Is(err, ErrUnauthorized) {
			t.Fatalf("legacy orchestrator target stop = %v", err)
		}
	})
	operation, err = TaskInterventionIDFromBytes(bytes.Repeat([]byte{225}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	receipt, err := store.StopRunForAttempt(ctx, overseer.CredentialDigest, TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: worker.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: worker.Revision, Kind: TaskInterventionStop}, nil, mustTime(t, 401))
	if err != nil || receipt.State != TaskInterventionDelivered {
		t.Fatalf("worker stop = %+v, %v", receipt, err)
	}
	stopped, found, err := store.Run(ctx, worker.ID)
	if err != nil || !found || stopped.Phase != RunFinalizing || stopped.CredentialRevokedAt == nil {
		t.Fatalf("durable worker stop = %+v, found=%v, err=%v", stopped, found, err)
	}
	history, err := store.TaskInterventions(ctx, task.ProjectID, task.ID)
	if err != nil || len(history) != 1 || history[0].OperationID != operation || history[0].State != TaskInterventionDelivered {
		t.Fatalf("durable worker stop history = %+v, %v", history, err)
	}
}
