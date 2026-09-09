package kernel

import (
	"context"
	"errors"
	"reflect"
	"testing"
)

func TestPeerQuestionCorruptProjectFailsClosed(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	target, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 242), ProjectID: source.ProjectID, AssignedAgentID: source.AgentID, IncarnationID: incarnationID(t, 243), Title: "queued peer"}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	question, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: target.ID, IdempotencyKey: peerKey(12), Question: "question"}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	other, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 244), Name: "other", Root: "/other"}, mustTime(t, 33))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.writer.Exec(`UPDATE peer_questions SET project_id = ? WHERE id = ?`, other.ID.Bytes(), question.ID.Bytes()); err != nil {
		t.Fatal(err)
	}
	if _, err := store.Snapshot(ctx); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt project snapshot = %v", err)
	}
	path := storePath(t, store)
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := Open(ctx, path)
	if reopened != nil {
		reopened.Close()
	}
	if !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt project reopen = %v", err)
	}
}

func TestV6MigrationPreservesSendBackAndSupervision(t *testing.T) {
	ctx := context.Background()
	success, _ := NewSuccessProposal("finished")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
	defer store.Close()
	terminal, err := finalizeTestRun(t, store, finalizing, 80)
	if err != nil {
		t.Fatal(err)
	}
	task, found, err := store.Task(ctx, terminal.TaskID)
	if err != nil || !found {
		t.Fatalf("task = %v, %v", found, err)
	}
	if _, err := store.SendBackTask(ctx, task.ID, task.Revision, "retained review feedback", mustTime(t, 90)); err != nil {
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 247), ProjectID: terminal.ProjectID, Name: "supervisor", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 10}, mustTime(t, 91))
	if err != nil {
		t.Fatal(err)
	}
	policy, after, budget, instruction := IdleStandingInstruction, uint32(1), uint32(3), "inspect worker activity"
	if _, err := store.UpdateAgent(ctx, agent.ID, agent.Revision, AgentPatch{IdlePolicy: &policy, IdleAfterSeconds: &after, IdleRunBudget: &budget, IdleInstruction: &instruction}, mustTime(t, 92)); err != nil {
		t.Fatal(err)
	}
	if tasks, err := store.EnqueueOverseerWakeups(ctx, mustTime(t, 2000)); err != nil || len(tasks) != 1 {
		t.Fatalf("wake = %v, %v", tasks, err)
	}
	path := storePath(t, store)
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	pool, connection := openRawDatabase(t, path, false)
	if err := setForeignKeys(ctx, connection, false); err != nil {
		t.Fatal(err)
	}
	if _, err := connection.ExecContext(ctx, "BEGIN IMMEDIATE"); err != nil {
		t.Fatal(err)
	}
	if err := rebuildTable(ctx, connection, expectedSchemaOf(v6SchemaStatements()), "invalidations", testInvalidationColumns, "invalidations_entity_revision_unique", "", ""); err != nil {
		t.Fatal(err)
	}
	for _, statement := range []string{"DROP TABLE peer_questions", "PRAGMA user_version = 6", "COMMIT"} {
		if _, err := connection.ExecContext(ctx, statement); err != nil {
			t.Fatal(err)
		}
	}
	before := snapshotSchemaRows(t, ctx, connection, v6SchemaStatements(), false)
	if err := errors.Join(connection.Close(), pool.Close()); err != nil {
		t.Fatal(err)
	}
	migrated, err := Open(ctx, path)
	if err != nil {
		t.Fatal(err)
	}
	defer migrated.Close()
	reader, err := migrated.readerConnection(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer reader.Close()
	if after := snapshotSchemaRows(t, ctx, reader, v6SchemaStatements(), false); !reflect.DeepEqual(before, after) {
		t.Fatal("v6 migration changed existing rows")
	}
}
