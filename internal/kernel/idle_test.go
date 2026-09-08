package kernel

import (
	"context"
	"errors"
	"testing"
)

// A standing instruction is enqueued to its agent once the quiet spell has
// passed, spends one idle run each time, never stacks on queued work, and
// stops at the budget until the operator sets a new one.
func TestStandingInstructionEnqueuesItselfWithinItsBudget(t *testing.T) {
	ctx := context.Background()
	store, _ := newTestStore(t)
	seedDurableAuthority(t, store)
	project := projectID(t, 1)
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 30), ProjectID: project, Name: "idle", Role: RoleWorker, Provider: ProviderShell, ToolBudgetLimit: 4}, mustTime(t, 100_000))
	if err != nil {
		t.Fatal(err)
	}
	// Nothing to do without a rule, and a rule that names no instruction is
	// refused where the operator sets it.
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 1_000_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("wait policy enqueued %d tasks, err=%v", len(tasks), err)
	}
	policy, after, budget := IdleStandingInstruction, uint32(60), uint32(2)
	if _, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleRunBudget: &budget}, mustTime(t, 100_001)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("rule without an instruction = %v", err)
	}
	instruction := "Look for something useful to do."
	ruled, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleInstruction: &instruction, IdleRunBudget: &budget}, mustTime(t, 100_001))
	if err != nil || ruled.Idle != (IdleRule{Policy: policy, AfterSeconds: after, Instruction: instruction, RunBudget: budget}) {
		t.Fatalf("ruled agent = %+v, %v", ruled.Idle, err)
	}
	// The clock starts at the rule's edit: 59 s later is too soon, 60 s is due.
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+59_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("early round enqueued %d tasks, err=%v", len(tasks), err)
	}
	tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+60_000))
	if err != nil || len(tasks) != 1 || tasks[0].AssignedAgentID != agent.ID || tasks[0].Body != instruction || tasks[0].Title != "Standing instruction" || tasks[0].Status != TaskQueued {
		t.Fatalf("due round = %+v, %v", tasks, err)
	}
	spent, found, err := store.Agent(ctx, agent.ID)
	if err != nil || !found || spent.Idle.RunsUsed != 1 || spent.Revision.Int64() != ruled.Revision.Int64()+1 {
		t.Fatalf("agent after one idle run = %+v, found=%v, err=%v", spent, found, err)
	}
	// The queued task keeps the rule quiet, and so does a running one.
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+600_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("round over a queued task enqueued %d tasks, err=%v", len(tasks), err)
	}
	if _, err := store.UpdateTask(ctx, tasks[0].ID, tasks[0].Revision, TaskPatch{Cancel: true}, mustTime(t, 100_001+600_001)); err != nil {
		t.Fatal(err)
	}
	// Cancelling the task does not restart the clock (only an edit or a run's
	// end does), so the quiet spell since the spend has long passed: the
	// second run is the last the budget allows.
	tasks, err = store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+600_002))
	if err != nil || len(tasks) != 1 {
		t.Fatalf("second due round = %+v, %v", tasks, err)
	}
	second, _, _ := store.Agent(ctx, agent.ID)
	if _, err := store.UpdateTask(ctx, tasks[0].ID, tasks[0].Revision, TaskPatch{Cancel: true}, mustTime(t, 100_001+600_003)); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+10_000_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("round past the budget enqueued %d tasks, err=%v", len(tasks), err)
	}
	// A new budget starts the count again; a paused agent stays quiet.
	paused := true
	if _, err := store.UpdateAgent(ctx, agent.ID, second.Revision, AgentPatch{IdleRunBudget: &budget, Paused: &paused}, mustTime(t, 100_001+10_000_001)); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+20_000_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("paused agent enqueued %d tasks, err=%v", len(tasks), err)
	}
	reset, _, _ := store.Agent(ctx, agent.ID)
	if reset.Idle.RunsUsed != 0 || reset.Idle.RunBudget != budget {
		t.Fatalf("new budget did not restart the count: %+v", reset.Idle)
	}
	// An agent admission would not take (its tool budget is spent) draws
	// nothing either, unpaused or not.
	unpaused := false
	if _, err := store.UpdateAgent(ctx, agent.ID, reset.Revision, AgentPatch{Paused: &unpaused}, mustTime(t, 100_001+20_000_001)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`UPDATE agents SET tool_calls_used = tool_budget_limit WHERE id = ?`, agent.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100_001+30_000_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("agent past its tool budget enqueued %d tasks, err=%v", len(tasks), err)
	}
}

// A run keeps the rule quiet while it lasts, and the quiet spell restarts at
// the run's end rather than at the rule's edit, so a long run is never
// followed by a back-to-back fire.
func TestStandingInstructionWaitsForTheRunAndThenItsQuietSpell(t *testing.T) {
	ctx := context.Background()
	proposal, _ := NewSuccessProposal("done")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, proposal)
	defer store.Close()
	agent, _, err := store.Agent(ctx, finalizing.AgentID)
	if err != nil {
		t.Fatal(err)
	}
	policy, after, budget, instruction := IdleStandingInstruction, uint32(60), uint32(2), "Look for something useful to do."
	if _, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleInstruction: &instruction, IdleRunBudget: &budget}, mustTime(t, 100)); err != nil {
		t.Fatal(err)
	}
	// The run still in flight keeps the rule quiet long past the wait.
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 100+600_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("round during a run enqueued %d tasks, err=%v", len(tasks), err)
	}
	if _, err := finalizeTestRun(t, store, finalizing, 1_000_000); err != nil {
		t.Fatal(err)
	}
	// The clock starts at the run's end: 59 s after it is too soon, 60 s is due.
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 1_000_000+59_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("round just after the run enqueued %d tasks, err=%v", len(tasks), err)
	}
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 1_000_000+60_000)); err != nil || len(tasks) != 1 || tasks[0].AssignedAgentID != agent.ID {
		t.Fatalf("round after the quiet spell = %+v, %v", tasks, err)
	}
}

func TestOverseerWakeupConsumesWorkerEventsAndLeavesEventsDuringItsRunPending(t *testing.T) {
	ctx := context.Background()
	store, err := createTestStore(ctx, t.TempDir()+"/kernel.db", FactoryConfig{DispatchEnabled: true, Capacity: 1}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 80), Name: "p", Root: "/p"}, mustTime(t, 2))
	if err != nil {
		t.Fatal(err)
	}
	overseer, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 81), ProjectID: project.ID, Name: "overseer", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 8}, mustTime(t, 3))
	if err != nil {
		t.Fatal(err)
	}
	policy, after, budget, instruction := IdleStandingInstruction, uint32(60), uint32(3), "Inspect worker progress."
	overseer, err = store.UpdateAgent(ctx, overseer.ID, overseer.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleRunBudget: &budget, IdleInstruction: &instruction}, mustTime(t, 4))
	if err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 5)); err != nil || len(tasks) != 0 {
		t.Fatalf("overseer ignored its initial cooldown: %+v, %v", tasks, err)
	}
	// Enabling a standing overseer gets exactly one initial inspection; the
	// ordinary idle timer no longer polls orchestrators.
	initial, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 64_000))
	if err != nil || len(initial) != 1 || initial[0].AssignedAgentID != overseer.ID {
		t.Fatalf("initial wake = %+v, %v", initial, err)
	}
	if tasks, err := store.EnqueueIdleInstructions(ctx, mustTime(t, 1_000_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("ordinary idle polled overseer: %+v, %v", tasks, err)
	}
	if _, err := store.UpdateTask(ctx, initial[0].ID, initial[0].Revision, TaskPatch{Cancel: true}, mustTime(t, 64_001)); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 124_000)); err != nil || len(tasks) != 0 {
		t.Fatalf("wake without worker activity = %+v, %v", tasks, err)
	}
	worker, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 82), ProjectID: project.ID, Name: "worker", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 8}, mustTime(t, 124_001))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 83), ProjectID: project.ID, AssignedAgentID: worker.ID, IncarnationID: incarnationID(t, 84), Title: "worker event", Priority: -1}, mustTime(t, 124_002)); err != nil {
		t.Fatal(err)
	}
	first, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 124_003))
	if err != nil || len(first) != 1 {
		t.Fatalf("worker wake = %+v, %v", first, err)
	}
	keys := admissionKeys(t, 85, nil)
	admission, err := store.AdmitNext(ctx, keys, mustTime(t, 124_004))
	if err != nil || !admission.Admitted() || admission.Run == nil || admission.Run.TaskID != first[0].ID {
		t.Fatalf("overseer admission = %+v, %v", admission, err)
	}
	_, running := activateAllResources(t, store, *admission.Run, keys, 124_005)
	session := terminalSessionForRunTest(t, store, running.ID)
	running, err = store.ActivateRun(ctx, running.ID, session.ID, running.Revision, session.Revision, mustTime(t, 124_009))
	if err != nil || running.Phase != RunRunning {
		t.Fatalf("overseer running = %+v, %v", running, err)
	}
	priority := int64(1)
	if _, err := store.UpdateTask(ctx, taskID(t, 83), mustRevision(t, 1), TaskPatch{Priority: &priority}, mustTime(t, 124_010)); err != nil {
		t.Fatalf("worker event during overseer: %v", err)
	}
	if tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 124_011)); err != nil || len(tasks) != 0 {
		t.Fatalf("wake stacked during running overseer: %+v, %v", tasks, err)
	}
	proposal, err := NewSuccessProposal("done")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ProposeAttemptOutcome(ctx, keys.AttemptDigest, proposal, mustTime(t, 124_012)); err != nil {
		t.Fatal(err)
	}
	observeMissingProcessExits(t, store, running.ID, 124_013)
	releaseAllRunResources(t, store, running.ID, 124_016)
	closed := closeTerminalSessionAtCurrent(t, store, running.ID, 124_020)
	if _, err := store.FinalizeRun(ctx, closed.ID, closed.Revision, mustTime(t, 124_021)); err != nil {
		t.Fatal(err)
	}
	followup, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 184_021))
	if err != nil || len(followup) != 1 {
		t.Fatalf("worker event during overseer was lost: %+v, %v", followup, err)
	}
}

func TestOverseerWakeupRecoversOneInspectionAfterCursorPruning(t *testing.T) {
	ctx := context.Background()
	store, err := createTestStore(ctx, t.TempDir()+"/kernel.db", FactoryConfig{DispatchEnabled: true, Capacity: 1}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	defer store.Close()
	project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 100), Name: "p", Root: "/p"}, mustTime(t, 2))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 101), ProjectID: project.ID, Name: "overseer", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 3))
	if err != nil {
		t.Fatal(err)
	}
	policy, after, budget, instruction := IdleStandingInstruction, uint32(1), uint32(2), "Inspect worker progress."
	agent, err = store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleRunBudget: &budget, IdleInstruction: &instruction}, mustTime(t, 4))
	if err != nil {
		t.Fatal(err)
	}
	// This is the same coherent state appendInvalidations leaves after pruning:
	// the first retained entry is two, while a stale cursor remains at zero.
	if _, err := store.writer.Exec(`DELETE FROM invalidations WHERE sequence = 1`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`UPDATE factory SET invalidation_floor = 2 WHERE singleton = 1`); err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`INSERT INTO overseer_wake_cursors(agent_id, project_id, invalidation_sequence) VALUES(?, ?, 0)`, agent.ID.Bytes(), project.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 1_004))
	if err != nil || len(tasks) != 1 || tasks[0].AssignedAgentID != agent.ID {
		t.Fatalf("pruned cursor wake = %+v, %v", tasks, err)
	}
	if _, err := store.UpdateTask(ctx, tasks[0].ID, tasks[0].Revision, TaskPatch{Cancel: true}, mustTime(t, 1_005)); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 2_005)); err != nil || len(tasks) != 0 {
		t.Fatalf("pruned cursor repeated without worker activity: %+v, %v", tasks, err)
	}
	factory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	var cursor int64
	if err := store.readers.QueryRow(`SELECT invalidation_sequence FROM overseer_wake_cursors WHERE agent_id = ?`, agent.ID.Bytes()).Scan(&cursor); err != nil || cursor != factory.Head.Int64() {
		t.Fatalf("no-activity cursor = %d, head = %d, err = %v", cursor, factory.Head.Int64(), err)
	}
}
