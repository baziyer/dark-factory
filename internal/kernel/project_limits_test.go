package kernel

import (
	"context"
	"testing"
)

func TestProjectRunAllowanceCountsAdmissionsOnce(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 1)
	defer store.Close()
	ctx := context.Background()
	project, err := store.SetProjectLimits(ctx, project.ID, project.Revision, 1, 0, mustTime(t, 3))
	if err != nil || project.RunBudgetLimit != 1 || project.RunsUsed != 0 {
		t.Fatalf("set allowance = %+v, %v", project, err)
	}
	if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 201), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 202), Title: "one"}, mustTime(t, 4)); err != nil {
		t.Fatal(err)
	}
	keys := admissionKeys(t, 203, nil)
	first, err := store.AdmitNext(ctx, keys, mustTime(t, 5))
	if err != nil || !first.Admitted() {
		t.Fatalf("first admission = %+v, %v", first, err)
	}
	replay, err := store.AdmitNext(ctx, keys, mustTime(t, 6))
	if err != nil || !replay.Admitted() || replay.Run.ID != first.Run.ID {
		t.Fatalf("admission replay = %+v, %v", replay, err)
	}
	project, found, err := store.Project(ctx, project.ID)
	if err != nil || !found || project.RunsUsed != 1 {
		t.Fatalf("used runs = %+v, found=%v, err=%v", project, found, err)
	}
	project, err = store.SetProjectLimits(ctx, project.ID, project.Revision, 0, 1, mustTime(t, 6))
	if err != nil {
		t.Fatal(err)
	}
	due, err := store.OverdueRuns(ctx, mustTime(t, 1005))
	if err != nil || len(due) != 1 || due[0].ID != first.Run.ID {
		t.Fatalf("overdue runs = %+v, %v", due, err)
	}
}

func TestProjectLimitsUseAdditionalAllowanceAndDefaultToDisabled(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	ctx := context.Background()
	project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 210), Name: "p", Root: "/limits"}, mustTime(t, 1))
	if err != nil || project.RunBudgetLimit != 0 || project.MaxRunSeconds != 0 {
		t.Fatalf("new project limits = %+v, %v", project, err)
	}
	if _, err := store.writer.Exec(`UPDATE projects SET runs_used = 7 WHERE id = ?`, project.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	project, found, err := store.Project(ctx, project.ID)
	if err != nil || !found {
		t.Fatal(err)
	}
	project, err = store.SetProjectLimits(ctx, project.ID, project.Revision, 3, 120, mustTime(t, 2))
	if err != nil || project.RunBudgetLimit != 10 || project.RunsUsed != 7 || project.MaxRunSeconds != 120 {
		t.Fatalf("additional limits = %+v, %v", project, err)
	}
	snapshot, err := store.Snapshot(ctx)
	if err != nil || len(snapshot.Projects) != 1 {
		t.Fatalf("dashboard snapshot = %+v, %v", snapshot, err)
	}
	if got := snapshot.Projects[0]; got.RunBudgetLimit != 10 || got.RunsUsed != 7 || got.MaxRunSeconds != 120 {
		t.Fatalf("dashboard project limits = %+v", got)
	}
}

func TestAdmissionSkipsExhaustedProjectBeforePriority(t *testing.T) {
	for _, role := range []AgentRole{RoleWorker, RoleOrchestrator} {
		t.Run(role.String(), func(t *testing.T) {
			store, _, exhausted, exhaustedAgent := newAdmissionStore(t, role, 4)
			defer store.Close()
			ctx := context.Background()
			if _, err := store.writer.Exec(`UPDATE projects SET run_budget_limit = 1, runs_used = 1 WHERE id = ?`, exhausted.ID.Bytes()); err != nil {
				t.Fatal(err)
			}
			funded, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 210), Name: "funded", Root: "/funded"}, mustTime(t, 4))
			if err != nil {
				t.Fatal(err)
			}
			fundedAgent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 211), ProjectID: funded.ID, Name: "funded", Role: role, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 5))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 212), ProjectID: exhausted.ID, AssignedAgentID: exhaustedAgent.ID, IncarnationID: incarnationID(t, 213), Title: "exhausted", Priority: 9}, mustTime(t, 6)); err != nil {
				t.Fatal(err)
			}
			fundedTask, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 214), ProjectID: funded.ID, AssignedAgentID: fundedAgent.ID, IncarnationID: incarnationID(t, 215), Title: "funded", Priority: 1}, mustTime(t, 7))
			if err != nil {
				t.Fatal(err)
			}
			result, err := store.AdmitNext(ctx, admissionKeys(t, 216, nil), mustTime(t, 8))
			if err != nil || !result.Admitted() || result.Run.TaskID != fundedTask.ID {
				t.Fatalf("admission = %+v, %v", result, err)
			}
		})
	}
}
