package kernel

import (
	"context"
	"testing"
)

func TestV8PendingQuestionSurvivesOptionsMigration(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	question, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(73), QuestionText: "Keep this pending question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
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
	columns := "id, run_id, idempotency_key, kind, reason_code, question_text, status, delivery_id, delivery_started_at_ms, resolution_kind, closed_at_ms, revision, created_at_ms, updated_at_ms"
	if err := rebuildTable(ctx, connection, expectedSchemaOf(v8SchemaStatements()), "human_requests", columns, "human_requests_one_unresolved_per_run", "", ""); err != nil {
		t.Fatal(err)
	}
	if _, err := connection.ExecContext(ctx, "PRAGMA user_version = 8"); err != nil {
		t.Fatal(err)
	}
	if _, err := connection.ExecContext(ctx, "COMMIT"); err != nil {
		t.Fatal(err)
	}
	connection.Close()
	pool.Close()
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
	got, found, err := humanRequestByID(ctx, reader, question.ID)
	if err != nil || !found || got.QuestionText != question.QuestionText || got.Status != HumanRequestOpen || got.Revision != question.Revision || len(got.Options) != 0 {
		t.Fatalf("pending question changed: %+v found=%v err=%v", got, found, err)
	}
}
