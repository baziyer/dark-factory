package kernel

import (
	"context"
	"errors"
	"testing"
)

// Pausing an agent through the console must actually stop dispatch: the
// durable column existed before this and admission is the only place that can
// honour it.
func TestUpdateAgentPauseWithholdsTheAgentFromAdmission(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	if _, err := store.EnqueueTask(ctx, NewTask{
		ID: taskID(t, 45), ProjectID: project.ID, AssignedAgentID: agent.ID,
		IncarnationID: incarnationID(t, 46), Title: "queued while paused",
	}, mustTime(t, 5)); err != nil {
		t.Fatal(err)
	}
	paused, resumed := true, false
	updated, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{Paused: &paused}, mustTime(t, 6))
	if err != nil || !updated.Paused || updated.Revision.Int64() != agent.Revision.Int64()+1 || updated.UpdatedAt.Int64() != 6 {
		t.Fatalf("pause = %+v, %v", updated, err)
	}
	result, err := store.AdmitNext(ctx, admissionKeys(t, 47, nil), mustTime(t, 7))
	if err != nil || result.Admitted() || result.Reason != NoAdmissionNoEligibleWork {
		t.Fatalf("paused admission = %+v, %v", result, err)
	}
	if _, err := store.UpdateAgent(ctx, agent.ID, updated.Revision, AgentPatch{Paused: &resumed}, mustTime(t, 8)); err != nil {
		t.Fatal(err)
	}
	result, err = store.AdmitNext(ctx, admissionKeys(t, 48, nil), mustTime(t, 9))
	if err != nil || !result.Admitted() {
		t.Fatalf("resumed admission = %+v, %v", result, err)
	}
}

func TestUpdateAgentValidatesLaunchControlsAtTheObservedRevision(t *testing.T) {
	store, _, _, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	model, effort, bad := "gpt-5-codex", "high", "sideways"
	updated, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{Model: &model, ReasoningEffort: &effort}, mustTime(t, 6))
	if err != nil || updated.Model != model || updated.ReasoningEffort != effort {
		t.Fatalf("controls = %+v, %v", updated, err)
	}
	if _, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{Model: &model}, mustTime(t, 7)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale revision = %v", err)
	}
	// The wire may carry any bounded string; the domain still owns the set.
	if _, err := store.UpdateAgent(ctx, agent.ID, updated.Revision, AgentPatch{ReasoningEffort: &bad}, mustTime(t, 8)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("invalid effort = %v", err)
	}
}

// A stored row may hold a combination new launches refuse. Pausing such an
// agent touches no launch control, so the launch rules do not apply to it.
func TestUpdateAgentPausesALegacyAgentItCouldNotRelaunch(t *testing.T) {
	store, _, project, _ := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	legacy, err := store.CreateAgent(ctx, NewAgent{
		ID: agentID(t, 234), ProjectID: project.ID, Name: "legacy-claude", Role: RoleOrchestrator,
		Provider: ProviderClaudeCode, ReasoningEffort: "max", ToolBudgetLimit: 1,
	}, mustTime(t, 4))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`UPDATE agents SET reasoning_effort = 'ultra' WHERE id = ?`, legacy.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	paused, model := true, "claude-opus-5"
	updated, err := store.UpdateAgent(ctx, legacy.ID, legacy.Revision, AgentPatch{Paused: &paused}, mustTime(t, 5))
	if err != nil || !updated.Paused || updated.ReasoningEffort != "ultra" {
		t.Fatalf("pause a legacy agent = %+v, %v", updated, err)
	}
	// Editing a launch control does hold the whole pair to the launch rules.
	if _, err := store.UpdateAgent(ctx, legacy.ID, updated.Revision, AgentPatch{Model: &model}, mustTime(t, 6)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("relaunchable-controls edit = %v", err)
	}
}

func TestUpdateTaskEditsAndCancelsOnlyWhileQueued(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	task, err := store.EnqueueTask(ctx, NewTask{
		ID: taskID(t, 45), ProjectID: project.ID, AssignedAgentID: agent.ID,
		IncarnationID: incarnationID(t, 46), Title: "before", Body: "original instruction", Priority: 1,
	}, mustTime(t, 5))
	if err != nil {
		t.Fatal(err)
	}
	title, body, priority := "after", "replacement instruction", int64(9)
	edited, err := store.UpdateTask(ctx, task.ID, task.Revision, TaskPatch{Title: &title, Body: &body, Priority: &priority}, mustTime(t, 6))
	if err != nil || edited.Title != title || edited.Body != body || edited.WorkRevision != task.WorkRevision || edited.Priority != priority || edited.Status != TaskQueued || edited.Revision.Int64() != task.Revision.Int64()+1 {
		t.Fatalf("edit = %+v, %v", edited, err)
	}
	// A foreign agent cannot be assigned: the durable key is (agent, project).
	foreign, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 8), Name: "other", Root: "/other"}, mustTime(t, 6))
	if err != nil {
		t.Fatal(err)
	}
	stranger, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 9), ProjectID: foreign.ID, Name: "b", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 6))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.UpdateTask(ctx, task.ID, edited.Revision, TaskPatch{AssignedAgentID: &stranger.ID}, mustTime(t, 7)); !errors.Is(err, ErrConflict) {
		t.Fatalf("cross-project reassignment = %v", err)
	}
	cancelled, err := store.UpdateTask(ctx, task.ID, edited.Revision, TaskPatch{Cancel: true}, mustTime(t, 8))
	if err != nil || cancelled.Status != TaskCancelled || cancelled.CompletedAt == nil || cancelled.CompletedAt.Int64() != 8 {
		t.Fatalf("cancel = %+v, %v", cancelled, err)
	}
	if _, err := store.UpdateTask(ctx, task.ID, cancelled.Revision, TaskPatch{Title: &title}, mustTime(t, 9)); !errors.Is(err, ErrConflict) {
		t.Fatalf("edit after cancel = %v", err)
	}
	result, err := store.AdmitNext(ctx, admissionKeys(t, 47, nil), mustTime(t, 10))
	if err != nil || result.Admitted() || result.Reason != NoAdmissionQueueEmpty {
		t.Fatalf("cancelled task admission = %+v, %v", result, err)
	}
}

func TestTaskBodyWithInstructionRetainsOnlyLatestSendBackNote(t *testing.T) {
	previousOffset := int64(len("original"))
	task := Task{Title: "fallback title", Body: "original\n\n## Sent back for work revision 2\n\nlatest review", SentBackInstructionBytes: &previousOffset}
	body, offset := TaskBodyWithInstruction(task, "replacement")
	if body != "replacement\n\n## Sent back for work revision 2\n\nlatest review" || offset == nil || *offset != int64(len("replacement")) || TaskInstruction(Task{Body: body, SentBackInstructionBytes: offset}) != "replacement" || TaskFeedback(Task{Body: body, SentBackInstructionBytes: offset}) != "\n\n## Sent back for work revision 2\n\nlatest review" {
		t.Fatalf("edited send-back body = %q offset=%v", body, offset)
	}
	empty := Task{Title: "fallback title", Body: "\n\n## Sent back for work revision 2\n\nlatest review", SentBackInstructionBytes: new(int64)}
	body, offset = TaskBodyWithInstruction(empty, "")
	if body != "fallback title\n\n## Sent back for work revision 2\n\nlatest review" || offset == nil || *offset != int64(len("fallback title")) || SentBackBody(Task{Title: empty.Title, Body: body, SentBackInstructionBytes: offset, WorkRevision: mustRevision(t, 2)}, "next") != "fallback title\n\n## Sent back for work revision 3\n\nnext" {
		t.Fatalf("empty edited send-back body = %q offset=%v", body, offset)
	}
}

func TestUpdateTaskForOverseerTargetsOnlyWorkers(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	overseer, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 230), ProjectID: run.ProjectID, Name: "other overseer", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 1}, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	queued, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 231), ProjectID: run.ProjectID, AssignedAgentID: overseer.ID, IncarnationID: incarnationID(t, 232), Title: "overseer work"}, mustTime(t, 41))
	if err != nil {
		t.Fatal(err)
	}
	priority := int64(1)
	for _, update := range []struct {
		priority *int64
		cancel   bool
	}{{priority: &priority}, {cancel: true}} {
		if _, err := store.UpdateTaskForOverseer(ctx, keys.AttemptDigest, queued.ID, queued.Revision, TaskPatch{Priority: update.priority, Cancel: update.cancel}, mustTime(t, 42)); !errors.Is(err, ErrUnauthorized) {
			t.Fatalf("orchestrator task patch = %v", err)
		}
	}
	if err := store.AuthorizeWorkerTaskForOverseer(ctx, keys.AttemptDigest, queued.ID); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("orchestrator task authorization = %v", err)
	}
	worker, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 233), ProjectID: run.ProjectID, Name: "worker", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 1}, mustTime(t, 43))
	if err != nil {
		t.Fatal(err)
	}
	workerTask, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 234), ProjectID: run.ProjectID, AssignedAgentID: worker.ID, IncarnationID: incarnationID(t, 235), Title: "worker work"}, mustTime(t, 44))
	if err != nil {
		t.Fatal(err)
	}
	if err := store.AuthorizeWorkerTaskForOverseer(ctx, keys.AttemptDigest, workerTask.ID); err != nil {
		t.Fatalf("worker task authorization = %v", err)
	}
	if err := store.AuthorizeWorkerTaskForOverseer(ctx, keys.AttemptDigest, taskID(t, 236)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("missing task authorization = %v", err)
	}
	updated, err := store.UpdateTaskForOverseer(ctx, keys.AttemptDigest, workerTask.ID, workerTask.Revision, TaskPatch{Priority: &priority}, mustTime(t, 45))
	if err != nil || updated.Priority != priority || updated.Title != workerTask.Title {
		t.Fatalf("worker task patch = %+v, %v", updated, err)
	}
}

func TestUpdateAgentForOverseerOnlyPausesWorkers(t *testing.T) {
	ctx := context.Background()
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	account, err := store.LinkAccount(ctx, NewAccount{ID: accountID(t, 236), Provider: ProviderCodex, Home: "/Users/operator/.codex-overseer", Label: "overseer"}, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	worker, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 237), ProjectID: run.ProjectID, Name: "worker", Role: RoleWorker, Provider: ProviderCodex, Model: "gpt-5-codex", ReasoningEffort: "high", AccountID: account.ID, ToolBudgetLimit: 1}, mustTime(t, 41))
	if err != nil {
		t.Fatal(err)
	}
	policy, after, instruction, budget := IdleStandingInstruction, uint32(1), "inspect worker work", uint32(2)
	configured, err := store.UpdateAgent(ctx, worker.ID, worker.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleInstruction: &instruction, IdleRunBudget: &budget}, mustTime(t, 42))
	if err != nil {
		t.Fatal(err)
	}
	updated, err := store.UpdateAgentForOverseer(ctx, keys.AttemptDigest, configured.ID, configured.Revision, true, mustTime(t, 43))
	if err != nil || !updated.Paused || updated.Model != configured.Model || updated.ReasoningEffort != configured.ReasoningEffort || updated.AccountID != configured.AccountID || updated.Idle != configured.Idle {
		t.Fatalf("overseer pause = %+v, %v", updated, err)
	}
}

// A task re-queued after a terminal run is the second shape cancellation can
// reach: work revision 2, with a run history stopping one revision behind it.
// The run-topology invariant admitted only a queued task there.
func TestUpdateTaskCancelsATaskRequeuedAfterATerminalRun(t *testing.T) {
	store, terminal, _, _ := retryQueuedWorker(t, 90)
	defer store.Close()
	ctx := context.Background()
	task, found, err := store.Task(ctx, terminal.TaskID)
	if err != nil || !found || task.Status != TaskQueued || task.WorkRevision.Int64() != 2 {
		t.Fatalf("re-queued task = %+v, found=%v, err=%v", task, found, err)
	}
	cancelled, err := store.UpdateTask(ctx, task.ID, task.Revision, TaskPatch{Cancel: true}, mustTime(t, 91))
	if err != nil || cancelled.Status != TaskCancelled || cancelled.WorkRevision.Int64() != 2 {
		t.Fatalf("cancel after retry = %+v, %v", cancelled, err)
	}
	// The durable validator runs on every read, so a topology it rejects
	// surfaces as corrupt state rather than as a failed write.
	if _, _, err := store.Run(ctx, terminal.ID); err != nil {
		t.Fatalf("run topology after cancelling a re-queued task: %v", err)
	}
	if _, err := store.ReadPublicSnapshot(ctx); err != nil {
		t.Fatalf("public snapshot after cancelling a re-queued task: %v", err)
	}
}
