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

func TestReplacementAdmissionStaysAheadOfItsWorkersQueueOnly(t *testing.T) {
	ctx := context.Background()
	store, running, _ := runningWorkerRun(t)
	defer store.Close()
	task, found, err := store.Task(ctx, running.TaskID)
	if err != nil || !found {
		t.Fatalf("running task = %+v, found=%v, err=%v", task, found, err)
	}
	// Its priority is deliberately higher: a delivered replacement is the
	// worker's next work, not merely another normally ordered queue entry.
	queued, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 230), ProjectID: task.ProjectID, AssignedAgentID: task.AssignedAgentID, IncarnationID: incarnationID(t, 231), Title: "older queued work", Priority: 999}, mustTime(t, 35))
	if err != nil {
		t.Fatal(err)
	}
	other, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 232), ProjectID: task.ProjectID, Name: "other", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 36))
	if err != nil {
		t.Fatal(err)
	}
	unrelated, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 233), ProjectID: task.ProjectID, AssignedAgentID: other.ID, IncarnationID: incarnationID(t, 234), Title: "unrelated work", Priority: 1}, mustTime(t, 37))
	if err != nil {
		t.Fatal(err)
	}
	client := humanQuestionClient(t, store, 235, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	operation, err := TaskInterventionIDFromBytes(bytes.Repeat([]byte{236}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	successor := NewTask{ID: taskID(t, 237), IncarnationID: incarnationID(t, 238), Body: "replacement work"}
	if _, err := store.StopRunForBrowser(ctx, client.ID, TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: running.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: running.Revision, Kind: TaskInterventionReplace}, &successor, mustTime(t, 40)); err != nil {
		t.Fatal(err)
	}
	replacement, found, err := store.Task(ctx, successor.ID)
	if err != nil || !found {
		t.Fatalf("replacement = %+v, found=%v, err=%v", replacement, found, err)
	}
	if _, err := store.UpdateTask(ctx, replacement.ID, replacement.Revision, TaskPatch{AssignedAgentID: &other.ID}, mustTime(t, 45)); err != nil {
		t.Fatalf("reassign replacement = %v", err)
	}
	observeMissingProcessExits(t, store, running.ID, 41)
	releaseAllRunResources(t, store, running.ID, 44)
	finalizing := closeTerminalSessionAtCurrent(t, store, running.ID, 47)
	if _, err := finalizeTestRun(t, store, finalizing, 50); err != nil {
		t.Fatal(err)
	}

	first, err := store.AdmitNext(ctx, admissionKeys(t, 239, nil), mustTime(t, 51))
	if err != nil || !first.Admitted() || first.Run.TaskID != queued.ID {
		t.Fatalf("ordinary priority remains global = %+v, err=%v", first, err)
	}
	second, err := store.AdmitNext(ctx, admissionKeys(t, 245, nil), mustTime(t, 52))
	if err != nil || !second.Admitted() || second.Run.TaskID != successor.ID || second.Run.TaskID == unrelated.ID {
		t.Fatalf("replacement follows current worker = %+v, err=%v", second, err)
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
