package kernel

import (
	"bytes"
	"context"
	"errors"
	"testing"
)

func peerKey(seed byte) [IDBytes]byte { return humanKey(seed) }

func TestPeerQuestionIsTaskLinkedIdempotentAndPrivate(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	targetAgent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 250), ProjectID: source.ProjectID, Name: "peer", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	target, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 251), ProjectID: source.ProjectID, AssignedAgentID: targetAgent.ID, IncarnationID: incarnationID(t, 252), Title: "peer task", Body: "PRIVATE_TARGET_BODY"}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	before, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	input := NewPeerQuestion{TargetTaskID: target.ID, IdempotencyKey: peerKey(1), Question: "PRIVATE_PEER_QUESTION"}
	question, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, input, mustTime(t, 33))
	if err != nil {
		t.Fatal(err)
	}
	replay, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, input, mustTime(t, 34))
	if err != nil || replay.ID != question.ID || replay.Revision != question.Revision || question.RecipientDeliveryState != PeerDeliveryPending || question.AnswerDeliveryState != PeerDeliveryPending {
		t.Fatalf("replay = %+v, %v", replay, err)
	}
	changed := input
	changed.Question = "changed"
	if _, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, changed, mustTime(t, 35)); !errors.Is(err, ErrConflict) {
		t.Fatalf("changed replay = %v", err)
	}
	history, next, err := store.PeerQuestionsForTask(ctx, target.ID, 0)
	if err != nil || next != nil || len(history) != 1 || history[0].Question != input.Question || history[0].SourceTaskID != source.TaskID {
		t.Fatalf("history = %+v, %v, next=%v", history, err, next)
	}
	seen := false
	for _, item := range invalidationsAfter(t, store, before.Head) {
		if item.EntityKind == EntityPeerQuestion.String() && item.EntityID == question.ID.String() && item.Revision.Int64() == 1 {
			seen = true
		}
	}
	if !seen {
		t.Fatal("peer question invalidation missing")
	}
}

func TestPeerQuestionRejectsCrossProjectAndNonCodexTarget(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	other, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 254), Name: "other", Root: "/other"}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 255), ProjectID: other.ID, Name: "other-worker", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	target, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 156), ProjectID: other.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 157), Title: "other"}, mustTime(t, 33))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: target.ID, IdempotencyKey: peerKey(2), Question: "no cross project"}, mustTime(t, 34)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("cross-project = %v", err)
	}
	shell, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 153), ProjectID: source.ProjectID, Name: "shell", Role: RoleWorker, Provider: ProviderShell, ToolBudgetLimit: 4}, mustTime(t, 35))
	if err != nil {
		t.Fatal(err)
	}
	shellTask, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 154), ProjectID: source.ProjectID, AssignedAgentID: shell.ID, IncarnationID: incarnationID(t, 155), Title: "shell"}, mustTime(t, 36))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: shellTask.ID, IdempotencyKey: peerKey(6), Question: "no shell"}, mustTime(t, 37)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("shell target=%v", err)
	}
}

func TestPeerTargetsAndHistoryPageWithoutLeakingOtherProjects(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 170), ProjectID: source.ProjectID, Name: "peer", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	for i := byte(0); i < 5; i++ {
		target, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 171+i), ProjectID: source.ProjectID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 181+i), Title: "peer"}, mustTime(t, int64(32+i)))
		if err != nil {
			t.Fatal(err)
		}
		if _, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: target.ID, IdempotencyKey: peerKey(10 + i), Question: "question"}, mustTime(t, int64(40+i))); err != nil {
			t.Fatal(err)
		}
	}
	targets, next, err := store.PeerTargetsForAttempt(ctx, source.CredentialDigest, 0)
	if err != nil || len(targets) != 4 || next == nil || *next != 4 {
		t.Fatalf("targets=%+v next=%v err=%v", targets, next, err)
	}
	page, next, err := store.PeerTargetsForAttempt(ctx, source.CredentialDigest, *next)
	if err != nil || len(page) != 1 || next != nil {
		t.Fatalf("target page=%+v next=%v err=%v", page, next, err)
	}
	history, nextHistory, err := store.PeerQuestionsForTask(ctx, source.TaskID, 0)
	if err != nil || len(history) != 1 || nextHistory == nil || *nextHistory != 1 {
		t.Fatalf("history=%+v next=%v err=%v", history, nextHistory, err)
	}
	if _, _, err := store.PeerTargetsForAttempt(ctx, AttemptDigest{}, 0); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("unauthenticated targets=%v", err)
	}
}

func TestPeerAnswerReplaysAfterDeliveryRevisionAdvances(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	targetAgent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 158), ProjectID: source.ProjectID, Name: "target", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	targetTask, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 159), ProjectID: source.ProjectID, AssignedAgentID: targetAgent.ID, IncarnationID: incarnationID(t, 160), Title: "target"}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	targetRun := activatePeerWorker(t, store, targetTask, 161)
	question, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: targetTask.ID, IdempotencyKey: peerKey(3), Question: "need answer"}, mustTime(t, 60))
	if err != nil {
		t.Fatal(err)
	}
	answer := PeerAnswer{QuestionID: question.ID, Expected: question.Revision, IdempotencyKey: peerKey(4), Answer: "answer"}
	answered, err := store.AnswerPeerQuestionForAttempt(ctx, targetRun.CredentialDigest, answer, mustTime(t, 61))
	if err != nil {
		t.Fatal(err)
	}
	delivery, newly, err := store.ReservePeerDelivery(ctx, answered.ID, source.ID, peerDeliveryID(t, 5), true, mustTime(t, 62))
	if err != nil || !newly {
		t.Fatalf("reserve = %+v, %v, %v", delivery, newly, err)
	}
	if _, err := store.AcknowledgePeerDelivery(ctx, answered.ID, delivery.DeliveryID, true, delivery.Revision, mustTime(t, 63)); err != nil {
		t.Fatal(err)
	}
	replay, err := store.AnswerPeerQuestionForAttempt(ctx, targetRun.CredentialDigest, answer, mustTime(t, 64))
	if err != nil || replay.Answer != answer.Answer || replay.AnswerDeliveryState != PeerDeliveryDelivered {
		t.Fatalf("reply-loss replay = %+v, %v", replay, err)
	}
}

func TestPeerDeliveryNoticeNeverContainsPeerText(t *testing.T) {
	ctx := context.Background()
	store, source, _ := runningWorkerRun(t)
	defer store.Close()
	agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 190), ProjectID: source.ProjectID, Name: "target", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, 31))
	if err != nil {
		t.Fatal(err)
	}
	task, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 191), ProjectID: source.ProjectID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 192), Title: "target"}, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	target := activatePeerWorker(t, store, task, 193)
	question, err := store.CreatePeerQuestionForAttempt(ctx, source.CredentialDigest, NewPeerQuestion{TargetTaskID: task.ID, IdempotencyKey: peerKey(20), Question: "\x1b[2J PRIVATE_PEER_TEXT"}, mustTime(t, 60))
	if err != nil {
		t.Fatal(err)
	}
	delivery, newly, err := store.ReservePeerDelivery(ctx, question.ID, target.ID, peerDeliveryID(t, 21), false, mustTime(t, 61))
	if err != nil || !newly {
		t.Fatalf("reserve=%+v %v %v", delivery, newly, err)
	}
	if bytes.Contains(delivery.Payload, []byte(question.Question)) || bytes.Contains(delivery.Payload, []byte{0x1b}) {
		t.Fatalf("unsafe fixed notice=%q", delivery.Payload)
	}
}

func peerDeliveryID(t *testing.T, seed byte) PeerDeliveryID {
	t.Helper()
	raw := bytes.Repeat([]byte{seed}, IDBytes)
	raw[IDBytes-1] ^= 0x5a
	result, err := PeerDeliveryIDFromBytes(raw)
	if err != nil {
		t.Fatal(err)
	}
	return result
}

func activatePeerWorker(t *testing.T, store *Store, task Task, seed byte) Run {
	t.Helper()
	candidate := changeID(t, seed)
	keys := admissionKeys(t, seed+1, &candidate)
	keys.RuntimeRoot = "/peer/runtime"
	admission, err := store.AdmitNext(context.Background(), keys, mustTime(t, 33))
	if err != nil || !admission.Admitted() || admission.Run.TaskID != task.ID {
		t.Fatalf("admit = %+v, %v", admission, err)
	}
	format, _ := NewObjectFormat("sha1")
	commit, _ := NewCommitID(format, bytes.Repeat([]byte{1}, format.oidLength()))
	digest := changeTreeDigest(t, 3)
	repository, _ := NewFileIdentity(80, 81)
	selection, _ := NewChangeSelection(format, commit, digest, 1, 1, repository)
	stage, _ := NewFileIdentity(82, 83)
	prepared, err := store.RecordChangePrepared(context.Background(), candidate, mustRevision(t, 1), selection, stage, mustTime(t, 34))
	if err != nil {
		t.Fatalf("prepare peer change: %v", err)
	}
	availability, _ := NewChangeAvailability(digest, 1, 1, stage)
	if _, err := store.MarkChangeAvailable(context.Background(), candidate, prepared.Revision, availability, mustTime(t, 35)); err != nil {
		t.Fatalf("make peer change available: %v", err)
	}
	activated := activatePeerResources(t, store, *admission.Run, 36)
	session := terminalSessionForRunTest(t, store, admission.Run.ID)
	latest, found, err := store.Run(context.Background(), activated.ID)
	if err != nil || !found {
		t.Fatalf("read peer run before activation: %v found=%v", err, found)
	}
	run, err := store.ActivateRun(context.Background(), admission.Run.ID, session.ID, latest.Revision, session.Revision, mustTime(t, 50))
	if err != nil {
		t.Fatalf("activate peer run: %v", err)
	}
	return run
}

func activatePeerResources(t *testing.T, store *Store, run Run, at int64) Run {
	t.Helper()
	resources := resourcesForRunTest(t, store, run.ID)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	identity, _ := NewPathResourceIdentity(110, 120)
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, identity, mustTime(t, at)); err != nil {
		t.Fatal(err)
	}
	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	started, starting, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, at+1))
	if err != nil {
		t.Fatal(err)
	}
	active, _, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, started.Revision, starting.Revision, processIdentity(t, 402), mustTime(t, at+2))
	if err != nil {
		t.Fatal(err)
	}
	provider := resourceOfKind(t, resources, ResourceProviderProcess)
	group := resourceOfKind(t, resources, ResourceProviderGroup)
	if _, _, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, processIdentity(t, 450), mustTime(t, at+3)); err != nil {
		t.Fatal(err)
	}
	current, found, err := store.Run(context.Background(), active.ID)
	if err != nil || !found {
		t.Fatalf("read activated peer run: %v found=%v", err, found)
	}
	return current
}
