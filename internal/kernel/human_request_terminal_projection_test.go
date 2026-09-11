package kernel

import (
	"context"
	"errors"
	"reflect"
	"testing"
)

func TestHumanRequestDetailProjectsExactTargetAndCapabilityBoundAuthority(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(180), QuestionText: "private question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	private := humanQuestionClient(t, store, 181, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail)
	detail, err := store.HumanRequestDetail(ctx, private.ID, request.ID, request.Revision)
	if err != nil || detail.TerminalTarget == nil || detail.TerminalTarget.RunID() != run.ID || detail.CanReply || detail.CancelRun != nil || detail.ReplyMaxBytes != MaxHumanRequestReplyBytes {
		t.Fatalf("private-only detail = %+v, err=%v", detail, err)
	}
	cancelClient := humanQuestionClient(t, store, 182, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	detail, err = store.HumanRequestDetail(ctx, cancelClient.ID, request.ID, request.Revision)
	if err != nil || detail.TerminalTarget == nil || !detail.CanReply || detail.CancelRun == nil || detail.CancelRun.ExpectedRequestRevision() != request.Revision || detail.CancelRun.ExpectedRunRevision() != run.Revision {
		t.Fatalf("cancel detail = %+v, err=%v", detail, err)
	}
}

func TestHumanRequestDetailUnavailableStatesCarryNoTargetOrAuthority(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 183, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(183), QuestionText: "private"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	delivery, err := store.BeginHumanReply(ctx, client.ID, request.ID, request.Revision, humanDeliveryID(t, 184), "reply", mustTime(t, 401))
	if err != nil {
		t.Fatal(err)
	}
	detail, err := store.HumanRequestDetail(ctx, client.ID, request.ID, delivery.Revision)
	if err != nil || detail.TerminalTarget != nil || detail.CanReply || detail.CancelRun != nil {
		t.Fatalf("delivering detail = %+v, err=%v", detail, err)
	}
	if err := store.MarkHumanDeliveryUnknown(ctx, request.ID, delivery.DeliveryID, delivery.Revision, mustTime(t, 402)); err != nil {
		t.Fatal(err)
	}
	current, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil {
		t.Fatal(err)
	}
	detail, err = store.HumanRequestDetail(ctx, client.ID, request.ID, current.Revision)
	if err != nil || detail.TerminalTarget != nil || detail.CanReply || detail.CancelRun != nil {
		t.Fatalf("delivery-unknown detail = %+v, err=%v", detail, err)
	}
}

func TestHumanRequestDetailMissingSessionIsUnavailableAndCorruptFailsClosed(t *testing.T) {
	for _, test := range []struct {
		name, mutate string
		corrupt      bool
	}{
		{name: "missing", mutate: `DELETE FROM terminal_sessions WHERE run_id = ?`},
		{name: "non-active", mutate: `UPDATE terminal_sessions SET state = 'closed', closed_at_ms = updated_at_ms, revision = revision + 1 WHERE run_id = ?`},
		{name: "corrupt", mutate: `UPDATE terminal_sessions SET state = 'unknown' WHERE run_id = ?`, corrupt: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			defer store.Close()
			client := humanQuestionClient(t, store, 185, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
			request, err := store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(185), QuestionText: "private"}, mustTime(t, 400))
			if err != nil {
				t.Fatal(err)
			}
			corruptSQL(t, store, test.mutate, run.ID.Bytes())
			detail, err := store.HumanRequestDetail(context.Background(), client.ID, request.ID, request.Revision)
			if test.corrupt {
				if !errors.Is(err, ErrCorruptState) {
					t.Fatalf("corrupt detail = %+v, err=%v", detail, err)
				}
			} else if err != nil || detail.TerminalTarget != nil || detail.CanReply || detail.CancelRun != nil {
				t.Fatalf("missing-session detail = %+v, err=%v", detail, err)
			}
		})
	}
}

func TestHumanRequestDetailRejectsCorruptActiveRunRelationships(t *testing.T) {
	for _, test := range []struct {
		name    string
		prepare func(*testing.T, *Store, Run)
		mutate  string
		args    func(Run) []any
	}{
		{
			name:   "terminal activation differs from run",
			mutate: `UPDATE terminal_sessions SET activated_at_ms = activated_at_ms - 1 WHERE run_id = ?`,
			args:   func(run Run) []any { return []any{run.ID.Bytes()} },
		},
		{
			name:   "terminal update exceeds run",
			mutate: `UPDATE terminal_sessions SET updated_at_ms = updated_at_ms + 1 WHERE run_id = ?`,
			args:   func(run Run) []any { return []any{run.ID.Bytes()} },
		},
		{
			name: "task assigned agent differs from run",
			prepare: func(t *testing.T, store *Store, run Run) {
				if _, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 253), ProjectID: run.ProjectID, Name: "different", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 399)); err != nil {
					t.Fatal(err)
				}
			},
			mutate: `UPDATE tasks SET assigned_agent_id = ? WHERE id = ?`,
			args:   func(run Run) []any { return []any{agentID(t, 253).Bytes(), run.TaskID.Bytes()} },
		},
		{
			name:   "provider resource identity is not atomic",
			mutate: `UPDATE resources SET pgid = pgid + 1 WHERE run_id = ? AND kind = 'provider_group'`,
			args:   func(run Run) []any { return []any{run.ID.Bytes()} },
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, run, _ := runningOrchestratorRun(t)
			defer store.Close()
			if test.prepare != nil {
				test.prepare(t, store, run)
			}
			client := humanQuestionClient(t, store, 191, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
			request, err := store.CreateHumanQuestionForAttempt(context.Background(), run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(191), QuestionText: "private"}, mustTime(t, 400))
			if err != nil {
				t.Fatal(err)
			}
			corruptSQL(t, store, test.mutate, test.args(run)...)
			detail, err := store.HumanRequestDetail(context.Background(), client.ID, request.ID, request.Revision)
			if !errors.Is(err, ErrCorruptState) || !reflect.DeepEqual(detail, HumanRequestDetail{}) {
				t.Fatalf("corrupt active detail = %+v, err=%v", detail, err)
			}
		})
	}
}

func TestHumanRequestDetailRejectsCanonicalSessionMismatchBeforeProjection(t *testing.T) {
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	selected := terminalSessionForRunTest(t, store, run.ID)
	validated := selected
	validated.Revision = mustRevision(t, selected.Revision.Int64()+1)
	if target, err := humanRequestTerminalTarget(run, selected, validated); !errors.Is(err, ErrCorruptState) || target != (TerminalTarget{}) {
		t.Fatalf("mismatched validated terminal target = %+v, err=%v", target, err)
	}
}

func TestHumanRequestDetailFinalizingAndTerminalOriginsAreUnavailable(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	client := humanQuestionClient(t, store, 186, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(186), QuestionText: "private"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	proposal, err := NewBlockedProposal("done")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ProposeAttemptOutcome(ctx, run.CredentialDigest, proposal, mustTime(t, 401)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.HumanRequestDetail(ctx, client.ID, request.ID, request.Revision); !errors.Is(err, ErrConflict) {
		t.Fatalf("finalizing detail = %v", err)
	}
	observeMissingProcessExits(t, store, run.ID, 402)
	releaseAllRunResources(t, store, run.ID, 410)
	closed := closeTerminalSessionAtCurrent(t, store, run.ID, 415)
	if _, err := store.FinalizeRun(ctx, run.ID, closed.Revision, mustTime(t, 420)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.HumanRequestDetail(ctx, client.ID, request.ID, request.Revision); !errors.Is(err, ErrConflict) {
		t.Fatalf("terminal detail = %v", err)
	}
}

func TestHumanRequestDetailUsesOnePinnedClientAndStateSnapshot(t *testing.T) {
	ctx := context.Background()
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	detailClient := humanQuestionClient(t, store, 187, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	deliveryClient := humanQuestionClient(t, store, 188, BrowserCapabilityObserve|BrowserCapabilityHumanActions)
	request, err := store.CreateHumanQuestionForAttempt(ctx, run.CredentialDigest, NewHumanQuestion{IdempotencyKey: humanKey(187), QuestionText: "snapshot question"}, mustTime(t, 400))
	if err != nil {
		t.Fatal(err)
	}
	read, err := store.beginRead(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer read.Close()
	if _, found, err := browserClientByID(ctx, read.connection, detailClient.ID); err != nil || !found {
		t.Fatalf("pin client = found %v, err %v", found, err)
	}
	if _, err := store.RevokeBrowserClient(ctx, detailClient.ID, detailClient.Revision, mustTime(t, 401)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.BeginHumanReply(ctx, deliveryClient.ID, request.ID, request.Revision, humanDeliveryID(t, 189), "new state", mustTime(t, 402)); err != nil {
		t.Fatal(err)
	}
	detail, err := humanRequestDetail(ctx, read.connection, detailClient.ID, request.ID, request.Revision)
	if err != nil || detail.QuestionText != "snapshot question" || detail.Revision != request.Revision || detail.ReplyMaxBytes != MaxHumanRequestReplyBytes || detail.TerminalTarget == nil || detail.TerminalTarget.RunID() != run.ID || !detail.CanReply || detail.CancelRun == nil || detail.CancelRun.ExpectedRequestRevision() != request.Revision || detail.CancelRun.ExpectedRunRevision() != run.Revision {
		t.Fatalf("pinned detail = %+v, err=%v", detail, err)
	}
	if _, err := store.HumanRequestDetail(ctx, detailClient.ID, request.ID, request.Revision); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("current revoked detail = %v", err)
	}
	currentClient := humanQuestionClient(t, store, 190, BrowserCapabilityObserve|BrowserCapabilityPrivateHumanRequestDetail|BrowserCapabilityHumanActions)
	current, _, err := store.HumanRequest(ctx, request.ID)
	if err != nil {
		t.Fatal(err)
	}
	currentDetail, err := store.HumanRequestDetail(ctx, currentClient.ID, request.ID, current.Revision)
	if err != nil || currentDetail.TerminalTarget != nil || currentDetail.CanReply || currentDetail.CancelRun != nil {
		t.Fatalf("current detail = %+v, err=%v", currentDetail, err)
	}
}
