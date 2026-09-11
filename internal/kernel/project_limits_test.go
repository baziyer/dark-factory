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
}
