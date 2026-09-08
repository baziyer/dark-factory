package kernel

import (
	"context"
	"errors"
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
	snapshot, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, nil)
	if err != nil || snapshot.ProjectID != run.ProjectID || len(snapshot.Tasks) != 1 || snapshot.Tasks[0].ID != run.TaskID {
		t.Fatalf("scoped snapshot = %+v, %v", snapshot, err)
	}
	selected := run.TaskID
	detail, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, &selected)
	if err != nil || len(detail.Tasks) != 1 || detail.Tasks[0].ID != selected || detail.Tasks[0].ObjectiveTruncated || detail.Tasks[0].ResultTruncated {
		t.Fatalf("selected snapshot = %+v, %v", detail, err)
	}
	foreign := taskID(t, 243)
	if _, err := store.OverseerSnapshotForAttempt(ctx, run.CredentialDigest, &foreign); !errors.Is(err, ErrNotFound) {
		t.Fatalf("foreign selected task = %v", err)
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
