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
