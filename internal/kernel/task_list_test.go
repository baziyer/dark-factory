package kernel

import (
	"bytes"
	"context"
	"reflect"
	"testing"
)

func TestCompletedWorkDoesNotFillActiveSnapshotAndPagesIndependently(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	tx, err := store.writer.BeginTx(ctx, nil)
	if err != nil {
		t.Fatal(err)
	}
	defer tx.Rollback()
	statement, err := tx.PrepareContext(ctx, `INSERT INTO tasks(id,project_id,assigned_agent_id,incarnation_id,work_revision,title,body,status,priority,completed_at_ms,revision,created_at_ms,updated_at_ms) VALUES(?,?,?,?,1,'completed','private instruction','cancelled',0,?,1,0,?)`)
	if err != nil {
		t.Fatal(err)
	}
	defer statement.Close()
	const count = PublicStateEntityLimit + 20
	for i := 1; i <= count; i++ {
		id := publicTaskID(t, i)
		if _, err := statement.ExecContext(ctx, id.Bytes(), project.ID.Bytes(), agent.ID.Bytes(), id.Bytes(), i, i); err != nil {
			t.Fatal(err)
		}
	}
	if err := tx.Commit(); err != nil {
		t.Fatal(err)
	}
	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil || len(snapshot.Tasks) != 1 || snapshot.Tasks[0].ID != publicTaskID(t, count) {
		t.Fatalf("active snapshot = %+v, %v", snapshot.Tasks, err)
	}
	first, err := store.ReadTaskList(ctx, agent.ID, UnixMillis{}, TaskID{})
	if err != nil || first.Total != count || !first.HasMore || len(first.Tasks) != TaskListPageSize || first.Tasks[0].ID != publicTaskID(t, count) {
		t.Fatalf("first page = %+v, %v", first, err)
	}
	// A new completion and unrelated head movement must not invalidate a cursor.
	id := publicTaskID(t, count+1)
	if _, err := store.writer.ExecContext(ctx, `INSERT INTO tasks(id,project_id,assigned_agent_id,incarnation_id,work_revision,title,body,status,priority,completed_at_ms,revision,created_at_ms,updated_at_ms) VALUES(?,?,?,?,1,'new completion','','cancelled',0,?,1,0,?)`, id.Bytes(), project.ID.Bytes(), agent.ID.Bytes(), id.Bytes(), count+1, count+1); err != nil {
		t.Fatal(err)
	}
	last := first.Tasks[len(first.Tasks)-1]
	second, err := store.ReadTaskList(ctx, agent.ID, last.UpdatedAt, last.ID)
	if err != nil || second.Total != count+1 || len(second.Tasks) != TaskListPageSize || second.Tasks[0].ID != publicTaskID(t, count-TaskListPageSize) {
		t.Fatalf("second page = %+v, %v", second, err)
	}
	tail, err := store.ReadTaskList(ctx, agent.ID, mustTime(t, 10), publicTaskID(t, 10))
	if err != nil || tail.HasMore || len(tail.Tasks) != 9 || tail.Tasks[8].ID != publicTaskID(t, 1) {
		t.Fatalf("oldest history = %+v, %v", tail, err)
	}
}

func TestPublicQueueOrderMatchesAdmissionForTiedPriorities(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	ctx := context.Background()
	older, newer := publicTaskID(t, 9000), publicTaskID(t, 8000)
	for i, id := range []TaskID{older, newer} {
		if _, err := store.EnqueueTask(ctx, NewTask{ID: id, ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, byte(80+i)), Title: "same priority"}, mustTime(t, int64(10+i))); err != nil {
			t.Fatal(err)
		}
	}
	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil || len(snapshot.Tasks) != 2 || snapshot.Tasks[0].ID != older {
		t.Fatalf("queue = %+v, %v", snapshot.Tasks, err)
	}
	admitted, err := store.AdmitNext(ctx, admissionKeys(t, 100, nil), mustTime(t, 20))
	if err != nil || admitted.Run == nil || admitted.Run.TaskID != snapshot.Tasks[0].ID {
		t.Fatalf("admission = %+v, %v", admitted, err)
	}
}

func TestPublicQueueKeepsReplacementAheadOnlyWithinItsAgent(t *testing.T) {
	ctx := context.Background()
	store, running, _ := runningWorkerRun(t)
	defer store.Close()
	task, found, err := store.Task(ctx, running.TaskID)
	if err != nil || !found {
		t.Fatalf("running task = %+v, found=%v, err=%v", task, found, err)
	}
	queued, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 240), ProjectID: task.ProjectID, AssignedAgentID: task.AssignedAgentID, IncarnationID: incarnationID(t, 241), Title: "same worker later", Priority: 9}, mustTime(t, 30))
	if err != nil {
		t.Fatal(err)
	}
	other, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 242), ProjectID: task.ProjectID, Name: "other worker", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	global, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 243), ProjectID: task.ProjectID, AssignedAgentID: other.ID, IncarnationID: incarnationID(t, 244), Title: "other worker first", Priority: 1}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	client := humanQuestionClient(t, store, 245, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	operation, err := TaskInterventionIDFromBytes(bytes.Repeat([]byte{246}, IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	successor := NewTask{ID: taskID(t, 247), IncarnationID: incarnationID(t, 248), Body: "replacement"}
	if _, err := store.StopRunForBrowser(ctx, client.ID, TaskInterventionRequest{OperationID: operation, TaskID: task.ID, RunID: running.ID, ExpectedTaskRevision: task.Revision, ExpectedRunRevision: running.Revision, Kind: TaskInterventionReplace}, &successor, mustTime(t, 40)); err != nil {
		t.Fatal(err)
	}
	observeMissingProcessExits(t, store, running.ID, 41)
	releaseAllRunResources(t, store, running.ID, 44)
	finalizing := closeTerminalSessionAtCurrent(t, store, running.ID, 47)
	if _, err := finalizeTestRun(t, store, finalizing, 50); err != nil {
		t.Fatal(err)
	}

	snapshot, err := store.ReadPublicSnapshot(ctx)
	if err != nil {
		t.Fatal(err)
	}
	got := make([]TaskID, 0, 3)
	for _, item := range snapshot.Tasks {
		if item.Status == TaskQueued.String() {
			got = append(got, item.ID)
		}
	}
	if want := []TaskID{successor.ID, queued.ID, global.ID}; !reflect.DeepEqual(got, want) {
		t.Fatalf("public per-agent queue = %v, want %v", got, want)
	}
	admitted, err := store.AdmitNext(ctx, admissionKeys(t, 249, nil), mustTime(t, 51))
	if err != nil || !admitted.Admitted() || admitted.Run.TaskID != global.ID {
		t.Fatalf("global candidate remains first = %+v, err=%v", admitted, err)
	}
}
