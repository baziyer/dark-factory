package kernel

import (
	"context"
	"errors"
	"strings"
	"testing"
)

func TestOverseerSnapshotIsProjectScopedAndTaskSelected(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	other, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 241), Name: "other", Root: "/other", VerificationPolicy: VerificationNone}, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	otherAgent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 242), ProjectID: other.ID, Name: "other", Role: RoleWorker, Provider: ProviderShell, ToolBudgetLimit: 1}, mustTime(t, 41))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 243), ProjectID: other.ID, AssignedAgentID: otherAgent.ID, IncarnationID: incarnationID(t, 244), Title: "foreign", Body: "must not appear"}, mustTime(t, 42)); err != nil {
		t.Fatal(err)
	}
	snapshot, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{})
	if err != nil || snapshot.ProjectID != run.ProjectID || len(snapshot.Tasks) != 1 || snapshot.Tasks[0].ID != run.TaskID {
		t.Fatalf("scoped snapshot = %+v, %v", snapshot, err)
	}
	selected := run.TaskID
	detail, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{TaskID: &selected})
	if err != nil || len(detail.Tasks) != 1 || detail.Tasks[0].ID != selected || detail.Tasks[0].ObjectiveTruncated || detail.Tasks[0].ResultTruncated {
		t.Fatalf("selected snapshot = %+v, %v", detail, err)
	}
	foreign := taskID(t, 243)
	if _, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{TaskID: &foreign}); !errors.Is(err, ErrNotFound) {
		t.Fatalf("foreign selected task = %v", err)
	}
}

func TestOverseerSnapshotPagesWithHeadFenceAndTaskTextChunks(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	for index := 0; index < OverseerSnapshotPageSize; index++ {
		if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, byte(250+index)), ProjectID: run.ProjectID, AssignedAgentID: run.AgentID, IncarnationID: incarnationID(t, byte(250+index)), Title: "queued"}, mustTime(t, int64(40+index))); err != nil {
			t.Fatal(err)
		}
	}
	first, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{})
	if err != nil || len(first.Tasks) != OverseerSnapshotPageSize || first.NextOffset == nil || *first.NextOffset != OverseerSnapshotPageSize {
		t.Fatalf("first page = %+v, %v", first, err)
	}
	if _, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{ExpectedHead: first.Head, TextOffset: 1}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("unselected text page = %v", err)
	}
	second, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{Offset: *first.NextOffset, ExpectedHead: first.Head})
	if err != nil || len(second.Tasks) != 1 || second.NextOffset != nil {
		t.Fatalf("second page = %+v, %v", second, err)
	}
	priority := int64(1)
	if _, err := store.UpdateTask(ctx, second.Tasks[0].ID, second.Tasks[0].Revision, TaskPatch{Priority: &priority}, mustTime(t, 50)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{Offset: *first.NextOffset, ExpectedHead: first.Head}); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale page = %v", err)
	}
	chunked, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 240), ProjectID: run.ProjectID, AssignedAgentID: run.AgentID, IncarnationID: incarnationID(t, 241), Title: "chunked", Body: strings.Repeat("界", 4097)}, mustTime(t, 51))
	if err != nil {
		t.Fatal(err)
	}
	detail, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{TaskID: &chunked.ID})
	if err != nil || len(detail.Tasks) != 1 || len([]rune(detail.Tasks[0].Objective)) != 4096 || !detail.Tasks[0].ObjectiveTruncated || detail.NextTextOffset == nil || *detail.NextTextOffset != 4096 {
		t.Fatalf("first text chunk = %+v, %v", detail, err)
	}
	tail, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, OverseerSnapshotRequest{TaskID: &chunked.ID, ExpectedHead: detail.Head, TextOffset: *detail.NextTextOffset})
	if err != nil || tail.Tasks[0].Objective != "界" || !tail.Tasks[0].ObjectiveTruncated || tail.NextTextOffset != nil {
		t.Fatalf("text tail = %+v, %v", tail, err)
	}
}

func TestOverseerCannotAnswerItsOwnHumanRequest(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(245), QuestionText: "question"}, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginHumanReplyForAttempt(ctx, run.CredentialDigest, request.ID, request.Revision, humanDeliveryID(t, 246), "answer", mustTime(t, 41)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("self answer = %v", err)
	}
}
