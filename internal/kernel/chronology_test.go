package kernel

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"testing"
)

func TestActivateRunRejectsCausallyEarlyResources(t *testing.T) {
	store, run, _ := admittedOrchestratorRun(t)
	defer store.Close()
	run = activateResourcesAt(t, store, run, 20)
	before := captureWriteFootprint(t, store)
	session := terminalSessionForRunTest(t, store, run.ID)
	if _, err := store.ActivateRun(context.Background(), run.ID, session.ID, run.Revision, session.Revision, mustTime(t, 11)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("early activation = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("early activation footprint before=%+v after=%+v", before, after)
	}
	if _, err := store.ActivateRun(context.Background(), run.ID, session.ID, run.Revision, session.Revision, mustTime(t, 20)); err != nil {
		t.Fatalf("activation boundary = %v", err)
	}
}

func TestFactoryTimestampGuardsWritesButNotExactReplay(t *testing.T) {
	store, _ := newTestStore(t)
	defer store.Close()
	initial, err := store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	before := captureWriteFootprint(t, store)
	if _, err := store.SetCapacity(context.Background(), initial.Revision, 2, mustTime(t, 0)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale factory update = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("stale factory update footprint before=%+v after=%+v", before, after)
	}
	updated, err := store.SetCapacity(context.Background(), initial.Revision, 2, initial.updatedAt)
	if err != nil {
		t.Fatal(err)
	}
	before = captureWriteFootprint(t, store)
	replay, err := store.SetCapacity(context.Background(), initial.Revision, 2, mustTime(t, 0))
	if err != nil || replay.Revision != updated.Revision || replay.updatedAt != updated.updatedAt {
		t.Fatalf("older factory replay = %+v, %v", replay, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("older factory replay footprint before=%+v after=%+v", before, after)
	}
}

func TestAdmitNextRejectsBeforeFactoryTimestamp(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	defer store.Close()
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 220), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 221), Title: "factory"}, mustTime(t, 5)); err != nil {
		t.Fatal(err)
	}
	state, err := store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	state, err = store.SetCapacity(context.Background(), state.Revision, 1, mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	keys := admissionKeys(t, 222, nil)
	before := captureWriteFootprint(t, store)
	if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 9)); !errors.Is(err, ErrRevisionConflict) || result.Admitted() {
		t.Fatalf("stale admission = %+v, %v", result, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("stale admission footprint before=%+v after=%+v", before, after)
	}
	if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 10)); err != nil || !result.Admitted() {
		t.Fatalf("admission boundary = %+v, %v", result, err)
	}
}

func TestFinalizeRunRejectsBeforeFactoryTimestamp(t *testing.T) {
	failure, _ := NewFailureProposal(FailureInternal, "cleanup")
	store, finalizing := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
	defer store.Close()
	factory, err := store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetCapacity(context.Background(), factory.Revision, 1, mustTime(t, 60)); err != nil {
		t.Fatal(err)
	}
	before := captureWriteFootprint(t, store)
	if _, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 53)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale finalization = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("stale finalization footprint before=%+v after=%+v", before, after)
	}
	if _, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 60)); err != nil {
		t.Fatalf("finalization boundary = %v", err)
	}
}

func TestFinalizingRejectsCausallyEarlyResourceUpdate(t *testing.T) {
	store, run, _ := admittedOrchestratorRun(t)
	defer store.Close()
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(699, 1699)
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 20)); err != nil {
		t.Fatal(err)
	}
	failure, _ := NewFailureProposal(FailureActivation, "cleanup")
	before := captureWriteFootprint(t, store)
	if _, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 11)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("early finalizing = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("early finalizing footprint before=%+v after=%+v", before, after)
	}
}

func TestExitDrivenFinalizingRejectsCausallyEarlyResourceUpdate(t *testing.T) {
	store, run, _ := admittedOrchestratorRun(t)
	defer store.Close()
	runner := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRunnerProcess)
	exit, _ := NewProcessExitCode(1, 1, mustTime(t, 10))
	before := captureWriteFootprint(t, store)
	if _, _, err := store.RecordLiveRunnerExitAndRelease(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, processIdentity(t, 701), exit, mustTime(t, 11)); !errors.Is(err, ErrConflict) {
		t.Fatalf("early exit-driven finalizing = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("early exit-driven footprint before=%+v after=%+v", before, after)
	}
}

func TestReleaseProviderRejectsBeforeExit(t *testing.T) {
	proposal, _ := NewSuccessProposal("done")
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	finalizing, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	provider := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceProviderProcess)
	group := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceProviderGroup)
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	if _, err := store.MarkResourceUnresolved(context.Background(), run.ID, runtime.ID, runtime.Revision, runtime.Identity, "later cleanup", mustTime(t, 50)); err != nil {
		t.Fatalf("advance independent resource cleanup = %v", err)
	}
	exit, _ := NewProcessExitCode(1, 0, mustTime(t, 42))
	observed, err := store.ObserveProviderExit(context.Background(), run.ID, finalizing.Revision, provider.Identity, exit, mustTime(t, 43))
	if err != nil {
		t.Fatal(err)
	}
	provider = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceProviderProcess)
	before := captureWriteFootprint(t, store)
	if _, _, _, err := store.ReleaseProviderResources(context.Background(), run.ID, provider.ID, group.ID, observed.Revision, provider.Revision, group.Revision, provider.Identity, mustTime(t, 41)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("release before exit = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("release-before-exit footprint before=%+v after=%+v", before, after)
	}
	releasedRun, releasedProvider, releasedGroup, err := store.ReleaseProviderResources(context.Background(), run.ID, provider.ID, group.ID, observed.Revision, provider.Revision, group.Revision, provider.Identity, mustTime(t, 50))
	if err != nil {
		t.Fatalf("release at exit = %v", err)
	}
	_, replay, replayGroup, err := store.ReleaseProviderResources(context.Background(), run.ID, provider.ID, group.ID, observed.Revision, provider.Revision, group.Revision, provider.Identity, mustTime(t, 40))
	if err != nil || replay.State != ResourceReleased || replayGroup.State != ResourceReleased || releasedRun.Revision.Int64() != observed.Revision.Int64()+1 || releasedProvider.Revision != replay.Revision || releasedGroup.Revision != replayGroup.Revision {
		t.Fatalf("older release replay = %+v/%+v, %v", replay, replayGroup, err)
	}
	_ = observed
}

func TestFinalizeRunRejectsBeforeResourceCleanupTime(t *testing.T) {
	failure, _ := NewFailureProposal(FailureInternal, "cleanup")
	store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
	defer store.Close()
	before := captureWriteFootprint(t, store)
	if _, err := store.FinalizeRun(context.Background(), run.ID, run.Revision, mustTime(t, 45)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("early finalization = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("early finalization footprint before=%+v after=%+v", before, after)
	}
	if _, err := store.FinalizeRun(context.Background(), run.ID, run.Revision, mustTime(t, 53)); err != nil {
		t.Fatalf("finalization boundary = %v", err)
	}
}

func TestRetryAdmissionCannotPrecedeQueuedTaskUpdate(t *testing.T) {
	store, terminal, _, keys := retryQueuedWorker(t, 50)
	defer store.Close()
	before := captureWriteFootprint(t, store)
	if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 40)); !errors.Is(err, ErrRevisionConflict) || result.Admitted() {
		t.Fatalf("admission before queued task update = %+v, %v", result, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("admission before queued task update footprint before=%+v after=%+v", before, after)
	}
	if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 50)); err != nil || !result.Admitted() {
		t.Fatalf("admission at queued task update = %+v, %v", result, err)
	}
	_ = terminal
}

// A finished task, a success included, goes back to its queue at the next
// work revision with the note in its body, the store stays valid, and the
// retry is admitted on the task's own Change.
func TestSuccessfulTerminalCanBeSentBackAndRetried(t *testing.T) {
	success, _ := NewSuccessProposal("finished")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
	defer store.Close()
	terminal, err := finalizeTestRun(t, store, finalizing, 80)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	task, found, err := store.Task(ctx, terminal.TaskID)
	if err != nil || !found || task.Status != TaskSucceeded {
		t.Fatalf("finished task = %+v, found=%v, %v", task, found, err)
	}
	if _, err := store.SendBackTask(ctx, task.ID, task.Revision, "", mustTime(t, 90)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("empty note = %v", err)
	}
	if _, err := store.SendBackTask(ctx, task.ID, task.Revision, strings.Repeat("x", MaxSendBackNoteBytes+1), mustTime(t, 90)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("oversized note = %v", err)
	}
	if _, err := store.SendBackTask(ctx, task.ID, task.Revision, "later", mustTime(t, task.UpdatedAt.Int64()-1)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("send-back before the terminal run = %v", err)
	}
	// The fixture's task has a title and no body, which a run receives as
	// the title; the send-back must keep that instruction.
	if task.Body != "" || task.Title == "" {
		t.Fatalf("fixture task = %+v", task)
	}
	sent, err := store.SendBackTask(ctx, task.ID, task.Revision, "the review wants a test", mustTime(t, 90))
	if err != nil || sent.Status != TaskQueued || sent.WorkRevision.Int64() != task.WorkRevision.Int64()+1 || sent.Result != "" || sent.CompletedAt != nil || sent.BlockedReason != "" ||
		sent.Body != task.Title+"\n\n## Sent back for work revision 2\n\nthe review wants a test" || sent.Body != SentBackBody(task, "the review wants a test") {
		t.Fatalf("sent back task = %+v, %v", sent, err)
	}
	if body := SentBackBody(Task{Title: "titled", Body: "the body", WorkRevision: mustRevision(t, 1)}, "n"); !strings.HasPrefix(body, "the body\n\n") {
		t.Fatalf("body with a body = %q", body)
	}
	if _, _, err := store.Run(ctx, terminal.ID); err != nil {
		t.Fatalf("store after send-back = %v", err)
	}
	if _, err := store.SendBackTask(ctx, task.ID, task.Revision, "again", mustTime(t, 91)); !errors.Is(err, ErrRevisionConflict) {
		t.Fatalf("stale revision = %v", err)
	}
	if _, err := store.SendBackTask(ctx, sent.ID, sent.Revision, "again", mustTime(t, 92)); !errors.Is(err, ErrConflict) {
		t.Fatalf("queued task sent back = %v", err)
	}
	candidate := changeID(t, 110)
	keys := admissionKeys(t, 100, &candidate)
	result, err := store.AdmitNext(ctx, keys, mustTime(t, 100))
	if err != nil || !result.Admitted() || result.Run.AdmittedTaskWorkRevision.Int64() != 2 || result.Run.ChangeID == nil || *result.Run.ChangeID != *terminal.ChangeID {
		t.Fatalf("retry admission = %+v, %v", result, err)
	}
}

// A task that never ran has no run to go back to.
func TestSendBackRefusesATaskWithoutARun(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleWorker, 2)
	defer store.Close()
	ctx := context.Background()
	task, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 60), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 61), Title: "never ran"}, mustTime(t, 5))
	if err != nil {
		t.Fatal(err)
	}
	cancelled, err := store.UpdateTask(ctx, task.ID, task.Revision, TaskPatch{Cancel: true}, mustTime(t, 6))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTask(ctx, cancelled.ID, cancelled.Revision, "try again", mustTime(t, 7)); !errors.Is(err, ErrConflict) {
		t.Fatalf("send-back without a run = %v", err)
	}
	// A shell agent's task is a program: no note can be appended to it,
	// whatever its status.
	shell, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 70), ProjectID: project.ID, Name: "shell", Role: RoleWorker, Provider: ProviderShell, ToolBudgetLimit: 2}, mustTime(t, 8))
	if err != nil {
		t.Fatal(err)
	}
	program, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 71), ProjectID: project.ID, AssignedAgentID: shell.ID, IncarnationID: incarnationID(t, 72), Title: "run", Body: "printf x"}, mustTime(t, 9))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTask(ctx, program.ID, program.Revision, "a note", mustTime(t, 10)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("send-back of a shell task = %v", err)
	}
}

// A running orchestrator of the same project may send a worker's finished
// task back; before it runs, its own task, another project's task, an
// unknown credential and (below) a worker's credential may not.
func TestOrchestratorAttemptSendsBackAWorkerTask(t *testing.T) {
	success, _ := NewSuccessProposal("finished")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
	defer store.Close()
	terminal, err := finalizeTestRun(t, store, finalizing, 80)
	if err != nil {
		t.Fatal(err)
	}
	ctx := context.Background()
	overseer, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 250), ProjectID: terminal.ProjectID, Name: "overseer", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 81))
	if err != nil {
		t.Fatal(err)
	}
	own, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 251), ProjectID: terminal.ProjectID, AssignedAgentID: overseer.ID, IncarnationID: incarnationID(t, 252), Title: "publish"}, mustTime(t, 82))
	if err != nil {
		t.Fatal(err)
	}
	keys := admissionKeys(t, 120, nil)
	admission, err := store.AdmitNext(ctx, keys, mustTime(t, 83))
	if err != nil || !admission.Admitted() || admission.Run.Role != RoleOrchestrator {
		t.Fatalf("orchestrator admission = %+v, %v", admission, err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, terminal.TaskID, "not yet running", mustTime(t, 84)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("admitted but not running orchestrator = %v", err)
	}
	activated := activateAllResourcesUnique(t, store, *admission.Run, 90, 7)
	session := terminalSessionForRunTest(t, store, admission.Run.ID)
	if _, err := store.ActivateRun(ctx, admission.Run.ID, session.ID, activated.Revision, session.Revision, mustTime(t, 100)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, own.ID, "myself", mustTime(t, 101)); !errors.Is(err, ErrConflict) {
		t.Fatalf("own task = %v", err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, taskID(t, 99), "missing", mustTime(t, 101)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("missing task = %v", err)
	}
	elsewhere, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 60), Name: "elsewhere", Root: "/elsewhere"}, mustTime(t, 101))
	if err != nil {
		t.Fatal(err)
	}
	stranger, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 61), ProjectID: elsewhere.ID, Name: "stranger", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 101))
	if err != nil {
		t.Fatal(err)
	}
	foreign, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 62), ProjectID: elsewhere.ID, AssignedAgentID: stranger.ID, IncarnationID: incarnationID(t, 63), Title: "not yours"}, mustTime(t, 101))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, foreign.ID, "another project", mustTime(t, 101)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("another project's task = %v", err)
	}
	sibling, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 64), ProjectID: terminal.ProjectID, Name: "sibling", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 101))
	if err != nil {
		t.Fatal(err)
	}
	siblingTask, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 65), ProjectID: terminal.ProjectID, AssignedAgentID: sibling.ID, IncarnationID: incarnationID(t, 66), Title: "another overseer's"}, mustTime(t, 101))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, siblingTask.ID, "not a worker's", mustTime(t, 101)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("a sibling orchestrator's task = %v", err)
	}
	task, found, err := store.Task(ctx, terminal.TaskID)
	if err != nil || !found {
		t.Fatalf("worker task = %+v, found=%v, %v", task, found, err)
	}
	sent, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, terminal.TaskID, "five findings", mustTime(t, 102))
	if err != nil || sent.Status != TaskQueued || sent.WorkRevision.Int64() != 2 || sent.Body != SentBackBody(task, "five findings") {
		t.Fatalf("orchestrator send-back = %+v, %v", sent, err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, keys.AttemptDigest, terminal.TaskID, "twice", mustTime(t, 103)); !errors.Is(err, ErrConflict) {
		t.Fatalf("queued task sent back again = %v", err)
	}
	unknown, err := AttemptDigestFromBytes(bytes.Repeat([]byte{9}, DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SendBackTaskForAttempt(ctx, unknown, terminal.TaskID, "stranger", mustTime(t, 104)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("unknown credential = %v", err)
	}
}

// A worker's credential is never a send-back authority.
func TestWorkerAttemptCannotSendBack(t *testing.T) {
	store, run, keys := runningWorkerRun(t)
	defer store.Close()
	if _, err := store.SendBackTaskForAttempt(context.Background(), keys.AttemptDigest, run.TaskID, "myself", mustTime(t, 200)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("worker send-back = %v", err)
	}
}

func TestNonSuccessTerminalAllowsQueuedRetry(t *testing.T) {
	blocked, _ := NewBlockedProposal("retry")
	failed, _ := NewFailureProposal(FailureInternal, "retry")
	cancelled, _ := NewCancelledProposal("retry")
	tests := []struct {
		name     string
		proposal Proposal
	}{
		{name: "blocked", proposal: blocked},
		{name: "failed", proposal: failed},
		{name: "cancelled", proposal: cancelled},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, test.proposal)
			defer store.Close()
			terminal, err := finalizeTestRun(t, store, finalizing, 80)
			if err != nil {
				t.Fatal(err)
			}
			_, keys := queueRetryForTerminal(t, store, terminal, 80)
			if _, _, err := store.Run(context.Background(), terminal.ID); err != nil {
				t.Fatalf("historical %s terminal Run = %v", test.name, err)
			}
			if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 80)); err != nil || !result.Admitted() {
				t.Fatalf("%s retry admission = %+v, %v", test.name, result, err)
			}
		})
	}
}

func TestQueuedRetryMustFollowPredecessorTerminal(t *testing.T) {
	t.Run("before", func(t *testing.T) {
		store, terminal, _, keys := retryQueuedWorker(t, 32)
		path := storePath(t, store)
		before := captureWriteFootprint(t, store)
		if _, _, err := store.Run(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("early queued retry Run = %v", err)
		}
		if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("early queued retry Snapshot = %v", err)
		}
		if _, err := store.RecoverableRuns(context.Background()); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("early queued retry RecoverableRuns = %v", err)
		}
		if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 33)); !errors.Is(err, ErrCorruptState) || result.Admitted() {
			t.Fatalf("early queued retry admission = %+v, %v", result, err)
		}
		if after := captureWriteFootprint(t, store); after != before {
			t.Fatalf("early queued retry footprint before=%+v after=%+v", before, after)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		beforeDatabase := captureDatabaseEvidence(t, path)
		if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("early queued retry Open = %v", err)
		}
		assertDatabaseEvidenceUnchanged(t, path, beforeDatabase)
	})

	t.Run("boundary", func(t *testing.T) {
		store, terminal, _, keys := retryQueuedWorker(t, 33)
		defer store.Close()
		if _, _, err := store.Run(context.Background(), terminal.ID); err != nil {
			t.Fatalf("boundary queued retry Run = %v", err)
		}
		if result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 33)); err != nil || !result.Admitted() {
			t.Fatalf("boundary queued retry admission = %+v, %v", result, err)
		}
	})
}

func TestHistoricalRetryMustFollowEveryPredecessor(t *testing.T) {
	store, predecessor, successor := retryAdmittedWorker(t, 33, 33)
	path := storePath(t, store)
	corruptSQL(t, store, `UPDATE runs SET terminal_at_ms = 100, updated_at_ms = 100 WHERE id = ?`, predecessor.ID.Bytes())
	assertCorruptTaskHistory(t, store, path, predecessor.ID, successor.ID)
}

// A successful run may precede later history: a send-back returns a finished
// task, a success included, to its queue.
func TestEarlierSuccessfulRunMayPrecedeLaterHistory(t *testing.T) {
	store, predecessor, successor := retryAdmittedWorker(t, 33, 33)
	defer store.Close()
	corruptSQL(t, store, `UPDATE runs SET proposal_kind = 'succeeded', proposal_code = NULL, proposal_detail = NULL, proposal_result = 'hidden success', terminal_kind = 'succeeded', terminal_code = NULL, terminal_detail = NULL, terminal_result = 'hidden success' WHERE id = ?`, predecessor.ID.Bytes())
	if _, _, err := store.Run(context.Background(), successor.ID); err != nil {
		t.Fatalf("history after a successful run = %v", err)
	}
	if _, _, err := store.Run(context.Background(), predecessor.ID); err != nil {
		t.Fatalf("successful predecessor = %v", err)
	}
}

// The daemon's refusal of a published tree ends the run as a source failure,
// abandons the available Change, keeps the store valid, and lets the task
// retry fresh on a reserved Change four revisions on.
func TestRefusedPublicationAbandonsTheAvailableChangeAndRetriesFresh(t *testing.T) {
	success, _ := NewSuccessProposal("finished")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
	defer store.Close()
	ctx := context.Background()
	changeState, found, err := store.Change(ctx, *finalizing.ChangeID)
	if err != nil || !found || changeState.Phase != ChangeAvailable {
		t.Fatalf("change before refusal = %+v, found=%v, %v", changeState, found, err)
	}
	if _, err := NewRefusedChangeSettlement(changeState.Revision, ""); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("refusal without a reason = %v", err)
	}
	settlement, err := NewRefusedChangeSettlement(changeState.Revision, "empty directory")
	if err != nil {
		t.Fatal(err)
	}
	terminal, err := store.FinalizeWorkerRun(ctx, finalizing.ID, finalizing.Revision, settlement, mustTime(t, 80))
	if err != nil || terminal.Phase != RunTerminal || terminal.Terminal == nil || terminal.Terminal.Kind() != OutcomeFailed || terminal.Terminal.Code() != FailureSource ||
		terminal.Terminal.Detail() != RefusedPublicationDetailPrefix+"empty directory" || terminal.Proposal == nil || !terminal.Proposal.equal(*terminal.Terminal) {
		t.Fatalf("refused run = %+v, %v", terminal, err)
	}
	if long, err := NewRefusedChangeSettlement(changeState.Revision, strings.Repeat("r", 5000)); err != nil || byteLen(long.refusal.detail) != 4096 {
		t.Fatalf("long refusal = %+v, %v", long, err)
	}
	task, found, err := store.Task(ctx, terminal.TaskID)
	if err != nil || !found || task.Status != TaskFailed || task.Result != "" {
		t.Fatalf("task after refusal = %+v, found=%v, %v", task, found, err)
	}
	abandoned, found, err := store.Change(ctx, changeState.ID)
	if err != nil || !found || abandoned.Phase != ChangeAbandoned || abandoned.SettledRunID == nil || *abandoned.SettledRunID != terminal.ID || abandoned.Revision.Int64() != changeState.Revision.Int64()+1 {
		t.Fatalf("change after refusal = %+v, found=%v, %v", abandoned, found, err)
	}
	if replay, err := store.FinalizeWorkerRun(ctx, finalizing.ID, finalizing.Revision, settlement, mustTime(t, 81)); err != nil || replay.Revision != terminal.Revision {
		t.Fatalf("refusal replay = %+v, %v", replay, err)
	}
	if _, _, err := store.Run(ctx, terminal.ID); err != nil {
		t.Fatalf("store after refusal = %v", err)
	}
	_, keys := queueRetryForTerminal(t, store, terminal, 90)
	result, err := store.AdmitNext(ctx, keys, mustTime(t, 100))
	if err != nil || !result.Admitted() || result.Run.ChangeID == nil || *result.Run.ChangeID != changeState.ID || result.Run.AdmittedChangeRevision.Int64() != changeState.Revision.Int64()+2 {
		t.Fatalf("retry after refusal = %+v, %v", result, err)
	}
	reopened, found, err := store.Change(ctx, changeState.ID)
	if err != nil || !found || reopened.Phase != ChangeReserved {
		t.Fatalf("reopened change = %+v, found=%v, %v", reopened, found, err)
	}
	if _, _, err := store.Run(ctx, result.Run.ID); err != nil {
		t.Fatalf("store after retry admission = %v", err)
	}
}

// A refusal is only for a published tree: an unpublished Change abandons
// the ordinary way, and a refused settlement on it is a conflict.
func TestRefusedSettlementNeedsAnAvailableChange(t *testing.T) {
	store, run, _ := admittedWorkerRun(t)
	defer store.Close()
	ctx := context.Background()
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(301, 302)
	if _, err := store.ActivateResource(ctx, run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
		t.Fatal(err)
	}
	failure, _ := NewFailureProposal(FailureInternal, "cleanup")
	finalizing, err := store.FailRun(ctx, run.ID, run.Revision, failure, mustTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	if _, err := store.ReleaseResource(ctx, run.ID, runtime.ID, runtime.Revision, runtime.Identity, mustTime(t, 30)); err != nil {
		t.Fatal(err)
	}
	settlement, err := NewRefusedChangeSettlement(*run.AdmittedChangeRevision, "refused")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.FinalizeWorkerRun(ctx, run.ID, finalizing.Revision, settlement, mustTime(t, 33)); !errors.Is(err, ErrConflict) {
		t.Fatalf("refused settlement of a reserved change = %v", err)
	}
}

// An ordinary source failure is not a refusal: a run that failed with
// FailureSource before its Change was published abandons from reserved and
// retries two revisions on, and one that failed with FailureSource while
// its Change was available retains it and retries on the retained tree.
func TestSourceFailuresThatAreNotRefusalsRetryAsBefore(t *testing.T) {
	t.Run("reserved", func(t *testing.T) {
		store, run, _ := admittedWorkerRun(t)
		defer store.Close()
		ctx := context.Background()
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(301, 302)
		if _, err := store.ActivateResource(ctx, run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
			t.Fatal(err)
		}
		failure, _ := NewFailureProposal(FailureSource, "source tree could not be materialized")
		finalizing, err := store.FailRun(ctx, run.ID, run.Revision, failure, mustTime(t, 20))
		if err != nil {
			t.Fatal(err)
		}
		runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		if _, err := store.ReleaseResource(ctx, run.ID, runtime.ID, runtime.Revision, runtime.Identity, mustTime(t, 30)); err != nil {
			t.Fatal(err)
		}
		abandoned, _ := NewAbandonedChangeSettlement(*run.AdmittedChangeRevision)
		terminal, err := store.FinalizeWorkerRun(ctx, run.ID, finalizing.Revision, abandoned, mustTime(t, 33))
		if err != nil {
			t.Fatal(err)
		}
		_, keys := queueRetryForTerminal(t, store, terminal, 40)
		result, err := store.AdmitNext(ctx, keys, mustTime(t, 50))
		if err != nil || !result.Admitted() || result.Run.AdmittedChangeRevision.Int64() != run.AdmittedChangeRevision.Int64()+2 {
			t.Fatalf("retry after a source failure = %+v, %v", result, err)
		}
		if _, _, err := store.Run(ctx, result.Run.ID); err != nil {
			t.Fatalf("store after the retry = %v", err)
		}
	})
	t.Run("available", func(t *testing.T) {
		failure, _ := NewFailureProposal(FailureSource, "source tree could not be adopted")
		store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, failure)
		defer store.Close()
		ctx := context.Background()
		change, found, err := store.Change(ctx, *finalizing.ChangeID)
		if err != nil || !found || change.Phase != ChangeAvailable {
			t.Fatalf("available Change = %+v, found=%v, %v", change, found, err)
		}
		availability := mustChangeAvailability(t, change.Selection.commitment, change.Selection.entries, change.Selection.bytes, *change.TreeIdentity)
		retained, _ := NewRetainedChangeSettlement(change.Revision, availability)
		terminal, err := store.FinalizeWorkerRun(ctx, finalizing.ID, finalizing.Revision, retained, mustTime(t, 80))
		if err != nil {
			t.Fatal(err)
		}
		_, keys := queueRetryForTerminal(t, store, terminal, 81)
		result, err := store.AdmitNext(ctx, keys, mustTime(t, 90))
		if err != nil || !result.Admitted() || result.Run.AdmittedChangeRevision.Int64() != change.Revision.Int64()+2 {
			t.Fatalf("retry after a source failure on an available Change = %+v, %v", result, err)
		}
		if reopened, found, err := store.Change(ctx, change.ID); err != nil || !found || reopened.Phase != ChangeAvailable {
			t.Fatalf("reopened Change = %+v, found=%v, %v", reopened, found, err)
		}
		if _, _, err := store.Run(ctx, result.Run.ID); err != nil {
			t.Fatalf("store after the retained retry = %v", err)
		}
	})
}

// A refusal on a retry's reopened retained Change abandons it one revision
// on, and the next retry starts fresh two revisions later.
func TestRefusedPublicationOnARetainedRetryAbandonsAndRetriesFresh(t *testing.T) {
	blocked, _ := NewBlockedProposal("retry")
	store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, blocked)
	defer store.Close()
	ctx := context.Background()
	change, found, err := store.Change(ctx, *finalizing.ChangeID)
	if err != nil || !found || change.Phase != ChangeAvailable {
		t.Fatalf("available Change = %+v, found=%v, %v", change, found, err)
	}
	availability := mustChangeAvailability(t, change.Selection.commitment, change.Selection.entries, change.Selection.bytes, *change.TreeIdentity)
	retained, _ := NewRetainedChangeSettlement(change.Revision, availability)
	first, err := store.FinalizeWorkerRun(ctx, finalizing.ID, finalizing.Revision, retained, mustTime(t, 80))
	if err != nil {
		t.Fatal(err)
	}
	_, retryKeys := queueRetryForTerminal(t, store, first, 81)
	retry, err := store.AdmitNext(ctx, retryKeys, mustTime(t, 90))
	if err != nil || !retry.Admitted() {
		t.Fatalf("retained retry admission = %+v, %v", retry, err)
	}
	reopened, found, err := store.Change(ctx, change.ID)
	if err != nil || !found || reopened.Phase != ChangeAvailable {
		t.Fatalf("reopened Change = %+v, found=%v, %v", reopened, found, err)
	}
	activated := activateAllResourcesUnique(t, store, *retry.Run, 100, 3)
	session := terminalSessionForRunTest(t, store, retry.Run.ID)
	running, err := store.ActivateRun(ctx, retry.Run.ID, session.ID, activated.Revision, session.Revision, mustTime(t, 110))
	if err != nil {
		t.Fatal(err)
	}
	success, _ := NewSuccessProposal("changed the tree")
	if _, err := store.ProposeAttemptOutcome(ctx, retryKeys.AttemptDigest, success, mustTime(t, 120)); err != nil {
		t.Fatal(err)
	}
	observeMissingProcessExits(t, store, running.ID, 121)
	for index, resource := range resourcesForRunTest(t, store, running.ID) {
		if resource.State == ResourceReleased {
			continue
		}
		if _, err := store.ReleaseResource(ctx, running.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, int64(130+index))); err != nil {
			t.Fatal(err)
		}
	}
	closeTerminalSessionAtCurrent(t, store, running.ID, 140)
	second, found, err := store.Run(ctx, running.ID)
	if err != nil || !found || second.Phase != RunFinalizing {
		t.Fatalf("retry before settlement = %+v, found=%v, %v", second, found, err)
	}
	settlement, err := NewRefusedChangeSettlement(reopened.Revision, "group-writable file")
	if err != nil {
		t.Fatal(err)
	}
	terminal, err := store.FinalizeWorkerRun(ctx, second.ID, second.Revision, settlement, mustTime(t, 150))
	if err != nil || terminal.Terminal == nil || terminal.Terminal.Code() != FailureSource {
		t.Fatalf("refused retry = %+v, %v", terminal, err)
	}
	abandoned, found, err := store.Change(ctx, change.ID)
	if err != nil || !found || abandoned.Phase != ChangeAbandoned || abandoned.Revision.Int64() != reopened.Revision.Int64()+1 {
		t.Fatalf("Change after refused retry = %+v, found=%v, %v", abandoned, found, err)
	}
	if _, _, err := store.Run(ctx, terminal.ID); err != nil {
		t.Fatalf("store after refused retry = %v", err)
	}
	_, againKeys := queueRetryForTerminalSeed(t, store, terminal, 160, 47)
	// The default retry seed shares a runtime root with the first retry's.
	again, err := store.AdmitNext(ctx, againKeys, mustTime(t, 170))
	if err != nil || !again.Admitted() || again.Run.AdmittedChangeRevision.Int64() != abandoned.Revision.Int64()+1 {
		t.Fatalf("fresh retry after refusal = %+v, %v", again, err)
	}
	if fresh, found, err := store.Change(ctx, change.ID); err != nil || !found || fresh.Phase != ChangeReserved {
		t.Fatalf("Change after fresh retry = %+v, found=%v, %v", fresh, found, err)
	}
	if _, _, err := store.Run(ctx, again.Run.ID); err != nil {
		t.Fatalf("store after fresh retry = %v", err)
	}
}

func TestRetryHistoryAllowsMultipleNonSuccessRuns(t *testing.T) {
	store, predecessor, _, keys := retryQueuedWorker(t, 33)
	defer store.Close()
	second, err := store.AdmitNext(context.Background(), keys, mustTime(t, 33))
	if err != nil || !second.Admitted() {
		t.Fatalf("first retry admission = %+v, %v", second, err)
	}
	materializeAdmittedWorkerChange(t, store, *second.Run, 34)
	_, activated := activateAllResources(t, store, *second.Run, keys, 36)
	session := terminalSessionForRunTest(t, store, second.Run.ID)
	running, err := store.ActivateRun(context.Background(), second.Run.ID, session.ID, activated.Revision, session.Revision, mustTime(t, 39))
	if err != nil {
		t.Fatal(err)
	}
	finalizing, err := store.CancelRun(context.Background(), second.Run.ID, running.Revision, "retry again", mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	runnerExit, err := NewProcessExitCode(1, 0, mustTime(t, 41))
	if err != nil {
		t.Fatal(err)
	}
	finalizing = recordRunnerExitForTest(t, store, second.Run.ID, runnerExit, 41)
	releaseAllRunResources(t, store, second.Run.ID, 42)
	finalizing = closeTerminalSessionAtCurrent(t, store, second.Run.ID, 45)
	secondTerminal, err := finalizeTestRun(t, store, finalizing, 45)
	if err != nil {
		t.Fatal(err)
	}
	_, thirdKeys := queueRetryForTerminalSeed(t, store, secondTerminal, 45, 230)
	third, err := store.AdmitNext(context.Background(), thirdKeys, mustTime(t, 45))
	if err != nil || !third.Admitted() {
		t.Fatalf("second retry admission = %+v, %v", third, err)
	}
	for _, id := range []RunID{predecessor.ID, second.Run.ID, third.Run.ID} {
		if _, _, err := store.Run(context.Background(), id); err != nil {
			t.Fatalf("non-success retry history Run(%s) = %v", id, err)
		}
	}
}

func assertCorruptTaskHistory(t *testing.T, store *Store, path string, runIDs ...RunID) {
	t.Helper()
	before := captureWriteFootprint(t, store)
	for _, id := range runIDs {
		if _, _, err := store.Run(context.Background(), id); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("corrupt task history Run(%s) = %v", id, err)
		}
		if _, found, err := store.RecoverableRun(context.Background(), id); !errors.Is(err, ErrCorruptState) || found {
			t.Fatalf("corrupt task history RecoverableRun(%s) = found=%v, err=%v", id, found, err)
		}
	}
	if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt task history Snapshot = %v", err)
	}
	if _, err := store.RecoverableRuns(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt task history RecoverableRuns = %v", err)
	}
	factory, err := store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetDispatch(context.Background(), factory.Revision, !factory.DispatchEnabled, mustTime(t, 100)); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt task history mutation = %v", err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("corrupt task history footprint before=%+v after=%+v", before, after)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	beforeDatabase := captureDatabaseEvidence(t, path)
	if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("corrupt task history Open = %v", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, beforeDatabase)
}

func TestFinalizingResourceActivationMustNotFollowFinalizing(t *testing.T) {
	t.Run("after finalizing", func(t *testing.T) {
		store, run, _ := admittedOrchestratorRun(t)
		path := storePath(t, store)
		failure, _ := NewFailureProposal(FailureInternal, "cleanup")
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(1, 2)
		if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
			t.Fatal(err)
		}
		finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
		if err != nil {
			t.Fatal(err)
		}
		runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		corruptSQL(t, store, `UPDATE resources SET state = 'released', path_dev = 1, path_inode = 2, activated_at_ms = 21, released_at_ms = 22, updated_at_ms = 22, revision = revision + 1 WHERE id = ?`, runtime.ID.Bytes())
		before := captureWriteFootprint(t, store)
		if _, _, err := store.Run(context.Background(), finalizing.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("late activation Run = %v", err)
		}
		if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("late activation Snapshot = %v", err)
		}
		if _, err := store.RecoverableRuns(context.Background()); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("late activation RecoverableRuns = %v", err)
		}
		if _, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 22)); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("late activation FinalizeRun = %v", err)
		}
		if after := captureWriteFootprint(t, store); after != before {
			t.Fatalf("late activation footprint before=%+v after=%+v", before, after)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		beforeDatabase := captureDatabaseEvidence(t, path)
		if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("late activation Open = %v", err)
		}
		assertDatabaseEvidenceUnchanged(t, path, beforeDatabase)
	})

	t.Run("at finalizing", func(t *testing.T) {
		store, run, _ := admittedOrchestratorRun(t)
		defer store.Close()
		failure, _ := NewFailureProposal(FailureInternal, "cleanup")
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(1, 2)
		if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
			t.Fatal(err)
		}
		finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
		if err != nil {
			t.Fatal(err)
		}
		runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		corruptSQL(t, store, `UPDATE resources SET state = 'released', path_dev = 1, path_inode = 2, activated_at_ms = 20, released_at_ms = 22, updated_at_ms = 22, revision = revision + 1 WHERE id = ?`, runtime.ID.Bytes())
		if _, _, err := store.Run(context.Background(), finalizing.ID); err != nil {
			t.Fatalf("boundary activation Run = %v", err)
		}
		if _, err := store.Snapshot(context.Background()); err != nil {
			t.Fatalf("boundary activation Snapshot = %v", err)
		}
	})

	t.Run("declared origin", func(t *testing.T) {
		store, run, _ := admittedOrchestratorRun(t)
		defer store.Close()
		failure, _ := NewFailureProposal(FailureInternal, "cleanup")
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(1, 2)
		if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
			t.Fatal(err)
		}
		finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
		if err != nil {
			t.Fatal(err)
		}
		if _, _, err := store.Run(context.Background(), finalizing.ID); err != nil {
			t.Fatalf("declared-origin cleanup Run = %v", err)
		}
	})
}

func TestRunningWorkerChangeCausalitySurvivesLaterPhases(t *testing.T) {
	t.Run("finalizing", func(t *testing.T) {
		store, run, keys := runningWorkerRun(t)
		path := storePath(t, store)
		proposal, _ := NewSuccessProposal("done")
		if _, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40)); err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE changes SET available_at_ms = 31, updated_at_ms = 31 WHERE id = ?`, run.ChangeID.Bytes())
		if _, _, err := store.Run(context.Background(), run.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("finalizing Run accepted late Change = %v", err)
		}
		if _, err := store.RecoverableRuns(context.Background()); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("finalizing RecoverableRuns accepted late Change = %v", err)
		}
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
		if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("Open accepted finalizing late Change = %v", err)
		}
	})

	t.Run("terminal", func(t *testing.T) {
		proposal, _ := NewSuccessProposal("done")
		store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationNone, proposal)
		defer store.Close()
		terminal, err := finalizeTestRun(t, store, finalizing, 80)
		if err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE changes SET available_at_ms = 90, updated_at_ms = 90 WHERE id = ?`, terminal.ChangeID.Bytes())
		if _, _, err := store.Run(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("terminal Run accepted late Change = %v", err)
		}
	})
}

func retryAdmittedWorker(t *testing.T, taskUpdatedAt, admissionAt int64) (*Store, Run, Run) {
	t.Helper()
	store, terminal, _, keys := retryQueuedWorker(t, taskUpdatedAt)
	admission, err := store.AdmitNext(context.Background(), keys, mustTime(t, admissionAt))
	if err != nil || !admission.Admitted() {
		store.Close()
		t.Fatalf("retry admission = %+v, %v", admission, err)
	}
	return store, terminal, *admission.Run
}

func retryQueuedWorker(t *testing.T, taskUpdatedAt int64) (*Store, Run, AgentID, AdmissionKeys) {
	t.Helper()
	store, terminal, _ := terminalPreRunningWorker(t)
	agentID, keys := queueRetryForTerminal(t, store, terminal, taskUpdatedAt)
	return store, terminal, agentID, keys
}

func queueRetryForTerminal(t *testing.T, store *Store, terminal Run, taskUpdatedAt int64) (AgentID, AdmissionKeys) {
	return queueRetryForTerminalSeed(t, store, terminal, taskUpdatedAt, 220)
}

func queueRetryForTerminalSeed(t *testing.T, store *Store, terminal Run, taskUpdatedAt int64, seed byte) (AgentID, AdmissionKeys) {
	t.Helper()
	project, found, err := store.Project(context.Background(), terminal.ProjectID)
	if err != nil || !found {
		store.Close()
		t.Fatalf("project = %+v, %v", project, err)
	}
	secondAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 214), ProjectID: project.ID, Name: "retry", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 4}, mustTime(t, taskUpdatedAt))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	corruptSQL(t, store, `UPDATE tasks SET assigned_agent_id = ?, work_revision = work_revision + 1, status = 'queued', blocked_reason = NULL, result = NULL, completed_at_ms = NULL, revision = revision + 1, updated_at_ms = ? WHERE id = ?`, secondAgent.ID.Bytes(), taskUpdatedAt, terminal.TaskID.Bytes())
	candidate := changeID(t, seed+10)
	keys := admissionKeys(t, seed, &candidate)
	return secondAgent.ID, keys
}

func TestTaskRunTopologyRejectsRevisionSkip(t *testing.T) {
	store, terminal, _ := terminalPreRunningWorker(t)
	defer store.Close()
	corruptSQL(t, store, `UPDATE tasks SET work_revision = work_revision + 2, status = 'queued', result = NULL, completed_at_ms = NULL WHERE id = ?`, terminal.TaskID.Bytes())
	if _, _, err := store.Run(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("revision skip accepted = %v", err)
	}
}

func TestOrphanTaskRevisionRejectedEveryBoundary(t *testing.T) {
	store, path, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	task, err := store.EnqueueTask(context.Background(), NewTask{
		ID: taskID(t, 240), ProjectID: project.ID, AssignedAgentID: agent.ID,
		IncarnationID: incarnationID(t, 241), Title: "orphan",
	}, mustTime(t, 5))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	corruptSQL(t, store, `UPDATE tasks SET work_revision = 2 WHERE id = ?`, task.ID.Bytes())
	before := captureWriteFootprint(t, store)
	if _, found, err := store.Task(context.Background(), task.ID); !errors.Is(err, ErrCorruptState) || found {
		t.Fatalf("orphan Task = found=%v, err=%v", found, err)
	}
	if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("orphan Snapshot = %v", err)
	}
	if _, err := store.RecoverableRuns(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("orphan RecoverableRuns = %v", err)
	}
	if result, err := store.AdmitNext(context.Background(), admissionKeys(t, 242, nil), mustTime(t, 10)); !errors.Is(err, ErrCorruptState) || result.Admitted() {
		t.Fatalf("orphan admission = %+v, %v", result, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("orphan rejection footprint before=%+v after=%+v", before, after)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	beforeDatabase := captureDatabaseEvidence(t, path)
	if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("orphan Open = %v", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, beforeDatabase)
}

func TestFreshQueuedTaskWithoutRunRemainsValidAndAdmissible(t *testing.T) {
	store, path, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	task, err := store.EnqueueTask(context.Background(), NewTask{
		ID: taskID(t, 243), ProjectID: project.ID, AssignedAgentID: agent.ID,
		IncarnationID: incarnationID(t, 244), Title: "fresh",
	}, mustTime(t, 5))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	if _, err := store.Snapshot(context.Background()); err != nil {
		t.Fatalf("fresh Snapshot = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	opened, err := Open(context.Background(), path)
	if err != nil {
		t.Fatalf("fresh Open = %v", err)
	}
	defer opened.Close()
	if fresh, found, err := opened.Task(context.Background(), task.ID); err != nil || !found || fresh.WorkRevision.Int64() != 1 || fresh.Status != TaskQueued {
		t.Fatalf("fresh Task = %+v, found=%v, err=%v", fresh, found, err)
	}
	result, err := opened.AdmitNext(context.Background(), admissionKeys(t, 245, nil), mustTime(t, 10))
	if err != nil || !result.Admitted() || result.Run.AdmittedTaskWorkRevision.Int64() != 1 {
		t.Fatalf("fresh admission = %+v, %v", result, err)
	}
}

func TestNoRunNonQueuedTaskRejected(t *testing.T) {
	store, path, project, agent := newAdmissionStore(t, RoleOrchestrator, 2)
	task, err := store.EnqueueTask(context.Background(), NewTask{
		ID: taskID(t, 246), ProjectID: project.ID, AssignedAgentID: agent.ID,
		IncarnationID: incarnationID(t, 247), Title: "nonfresh",
	}, mustTime(t, 5))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	corruptSQL(t, store, `UPDATE tasks SET status = 'running' WHERE id = ?`, task.ID.Bytes())
	if _, found, err := store.Task(context.Background(), task.ID); !errors.Is(err, ErrCorruptState) || found {
		t.Fatalf("nonfresh Task = found=%v, err=%v", found, err)
	}
	if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("nonfresh Snapshot = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	beforeDatabase := captureDatabaseEvidence(t, path)
	if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("nonfresh Open = %v", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, beforeDatabase)
}

func TestRunsRejectDuplicateAdmittedTaskWorkRevision(t *testing.T) {
	store, terminal, _ := terminalPreRunningWorker(t)
	defer store.Close()
	_, err := store.writer.Exec(`INSERT INTO runs(
		id, project_id, agent_id, task_id, task_incarnation_id, admitted_task_work_revision,
		change_id, role, provider, model, reasoning_effort, verification_policy, phase,
		proposal_kind, proposal_code, proposal_detail, proposal_result,
		terminal_kind, terminal_code, terminal_detail, terminal_result,
		credential_digest, result_proof_digest, credential_revoked_at_ms,
		provider_exit_kind, provider_exit_sequence, provider_exit_code, provider_exit_signal, provider_exit_at_ms,
		runner_exit_kind, runner_exit_sequence, runner_exit_code, runner_exit_signal, runner_exit_at_ms,
		revision, admitted_at_ms, running_at_ms, finalizing_at_ms, terminal_at_ms, updated_at_ms
	) SELECT ?, project_id, agent_id, task_id, task_incarnation_id, admitted_task_work_revision,
		change_id, role, provider, model, reasoning_effort, verification_policy, phase,
		proposal_kind, proposal_code, proposal_detail, proposal_result,
		terminal_kind, terminal_code, terminal_detail, terminal_result,
		zeroblob(32), randomblob(32), credential_revoked_at_ms,
		provider_exit_kind, provider_exit_sequence, provider_exit_code, provider_exit_signal, provider_exit_at_ms,
		runner_exit_kind, runner_exit_sequence, runner_exit_code, runner_exit_signal, runner_exit_at_ms,
		revision, admitted_at_ms, running_at_ms, finalizing_at_ms, terminal_at_ms, updated_at_ms
	FROM runs WHERE id = ?`, runID(t, 215).Bytes(), terminal.ID.Bytes())
	if err == nil {
		t.Fatal("duplicate terminal run accepted")
	}
}

func TestTerminalRunRejectsSameRevisionLateChangeCheckpoint(t *testing.T) {
	store, terminal := terminalPreRunningAvailableWorker(t)
	defer store.Close()
	corruptSQL(t, store, `UPDATE changes SET available_at_ms = 90, updated_at_ms = 90 WHERE id = ?`, terminal.ChangeID.Bytes())
	if _, _, err := store.Run(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("same-revision late Change accepted = %v", err)
	}
}

func terminalPreRunningAvailableWorker(t *testing.T) (*Store, Run) {
	t.Helper()
	store, run, _ := admittedWorkerRun(t)
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(305, 306)
	_, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 10))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	selection := testChangeSelection(t)
	stage, _ := NewFileIdentity(70, 80)
	prepared, err := store.RecordChangePrepared(context.Background(), *run.ChangeID, mustRevision(t, 1), selection, stage, mustTime(t, 12))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	availability := mustChangeAvailability(t, selection.commitment, selection.entries, selection.bytes, stage)
	if _, err := store.MarkChangeAvailable(context.Background(), *run.ChangeID, prepared.Revision, availability, mustTime(t, 13)); err != nil {
		store.Close()
		t.Fatal(err)
	}
	failure, _ := NewFailureProposal(FailureInternal, "cleanup")
	finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	if _, err := store.ReleaseResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtime.Identity, mustTime(t, 30)); err != nil {
		store.Close()
		t.Fatal(err)
	}
	settlement, _ := NewRetainedChangeSettlement(mustRevision(t, 3), availability)
	terminal, err := store.FinalizeWorkerRun(context.Background(), run.ID, finalizing.Revision, settlement, mustTime(t, 33))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	return store, terminal
}

func terminalPreRunningWorker(t *testing.T) (*Store, Run, AdmissionKeys) {
	t.Helper()
	failure, _ := NewFailureProposal(FailureInternal, "cleanup")
	store, run, keys := admittedWorkerRun(t)
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(301, 302)
	_, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	if _, err := store.ReleaseResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtime.Identity, mustTime(t, 30)); err != nil {
		store.Close()
		t.Fatal(err)
	}
	settlement, _ := NewAbandonedChangeSettlement(*run.AdmittedChangeRevision)
	terminal, err := store.FinalizeWorkerRun(context.Background(), run.ID, finalizing.Revision, settlement, mustTime(t, 33))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	return store, terminal, keys
}

func admittedWorkerRun(t *testing.T) (*Store, Run, AdmissionKeys) {
	t.Helper()
	store, _, project, agent := newAdmissionStore(t, RoleWorker, 4)
	_, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 210), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 211), Title: "worker"}, mustTime(t, 5))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	candidate := changeID(t, 212)
	keys := admissionKeys(t, 213, &candidate)
	keys.RuntimeRoot = "/worker/runtime"
	admission, err := store.AdmitNext(context.Background(), keys, mustTime(t, 10))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	return store, *admission.Run, keys
}

func TestChronologyScannersRejectImpossibleRows(t *testing.T) {
	tests := []struct {
		name  string
		setup func(*testing.T) (*Store, func() error)
	}{
		{name: "admitted update changed", setup: func(t *testing.T) (*Store, func() error) {
			store, run, _ := admittedOrchestratorRun(t)
			corruptSQL(t, store, `UPDATE runs SET updated_at_ms = admitted_at_ms + 1 WHERE id = ?`, run.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "declared resource update changed", setup: func(t *testing.T) (*Store, func() error) {
			store, run, _ := admittedOrchestratorRun(t)
			resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRunnerProcess)
			corruptSQL(t, store, `UPDATE resources SET updated_at_ms = declared_at_ms + 1 WHERE id = ?`, resource.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "active resource update changed", setup: func(t *testing.T) (*Store, func() error) {
			store, run, _ := runningOrchestratorRun(t)
			resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRunnerProcess)
			corruptSQL(t, store, `UPDATE resources SET updated_at_ms = activated_at_ms + 1 WHERE id = ?`, resource.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "running after updated", setup: func(t *testing.T) (*Store, func() error) {
			store, run, _ := runningOrchestratorRun(t)
			corruptSQL(t, store, `UPDATE runs SET running_at_ms = updated_at_ms + 1 WHERE id = ?`, run.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "finalizing credential mismatch", setup: func(t *testing.T) (*Store, func() error) {
			failure, _ := NewFailureProposal(FailureInternal, "cleanup")
			store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
			corruptSQL(t, store, `UPDATE runs SET credential_revoked_at_ms = finalizing_at_ms - 1 WHERE id = ?`, run.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "terminal before finalizing", setup: func(t *testing.T) (*Store, func() error) {
			failure, _ := NewFailureProposal(FailureInternal, "cleanup")
			store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
			terminal, err := finalizeTestRun(t, store, run, 80)
			if err != nil {
				t.Fatal(err)
			}
			corruptSQL(t, store, `UPDATE runs SET terminal_at_ms = finalizing_at_ms - 1 WHERE id = ?`, terminal.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), terminal.ID); return err }
		}},
		{name: "resource activation after running", setup: func(t *testing.T) (*Store, func() error) {
			store, run, _ := runningOrchestratorRun(t)
			runner := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRunnerProcess)
			corruptSQL(t, store, `UPDATE resources SET activated_at_ms = 31, updated_at_ms = 31 WHERE id = ?`, runner.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "finalizing resource before finalizing", setup: func(t *testing.T) (*Store, func() error) {
			failure, _ := NewFailureProposal(FailureInternal, "cleanup")
			store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
			resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
			corruptSQL(t, store, `UPDATE resources SET updated_at_ms = 39 WHERE id = ?`, resource.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "released provider before exit", setup: func(t *testing.T) (*Store, func() error) {
			failure, _ := NewFailureProposal(FailureInternal, "cleanup")
			store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
			resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceProviderProcess)
			corruptSQL(t, store, `UPDATE resources SET released_at_ms = 40, updated_at_ms = 40 WHERE id = ?`, resource.ID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), run.ID); return err }
		}},
		{name: "terminal before Change", setup: func(t *testing.T) (*Store, func() error) {
			success, _ := NewSuccessProposal("done")
			store, run := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
			terminal, err := finalizeTestRun(t, store, run, 80)
			if err != nil {
				t.Fatal(err)
			}
			corruptSQL(t, store, `UPDATE changes SET available_at_ms = 90, updated_at_ms = 90 WHERE id = ?`, terminal.ChangeID.Bytes())
			return store, func() error { _, _, err := store.Run(context.Background(), terminal.ID); return err }
		}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, read := test.setup(t)
			defer store.Close()
			if !errors.Is(read(), ErrCorruptState) {
				t.Fatalf("read accepted corruption: %v", read())
			}
		})
	}

	t.Run("change checkpoint", func(t *testing.T) {
		store, run, _ := runningWorkerRun(t)
		defer store.Close()
		corruptSQL(t, store, `UPDATE changes SET updated_at_ms = available_at_ms - 1 WHERE id = ?`, run.ChangeID.Bytes())
		if _, _, err := store.Change(context.Background(), *run.ChangeID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("Change accepted corruption: %v", err)
		}
	})
	t.Run("task completion checkpoint", func(t *testing.T) {
		failure, _ := NewFailureProposal(FailureInternal, "cleanup")
		store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
		defer store.Close()
		terminal, err := store.FinalizeRun(context.Background(), run.ID, run.Revision, mustTime(t, 80))
		if err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE tasks SET completed_at_ms = updated_at_ms - 1 WHERE id = ?`, terminal.TaskID.Bytes())
		if _, _, err := store.Task(context.Background(), terminal.TaskID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("task completion corruption accepted: %v", err)
		}
	})
	t.Run("blocked task terminal checkpoint", func(t *testing.T) {
		blocked, _ := NewBlockedProposal("blocked")
		store, run := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, blocked)
		defer store.Close()
		terminal, err := store.FinalizeRun(context.Background(), run.ID, run.Revision, mustTime(t, 80))
		if err != nil {
			t.Fatal(err)
		}
		corruptSQL(t, store, `UPDATE tasks SET incarnation_id = X'91919191919191919191919191919191' WHERE id = ?`, terminal.TaskID.Bytes())
		if _, _, err := store.Run(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("blocked task/run mismatch accepted: %v", err)
		}
	})
}

func activateResourcesAt(t *testing.T, store *Store, run Run, at int64) Run {
	t.Helper()
	resources := resourcesForRunTest(t, store, run.ID)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(700, 1700)
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, at)); err != nil {
		t.Fatal(err)
	}
	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	startedRun, startingRunner, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, at))
	if err != nil {
		t.Fatal(err)
	}
	activeRun, _, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, startedRun.Revision, startingRunner.Revision, processIdentity(t, 702), mustTime(t, at))
	if err != nil {
		t.Fatal(err)
	}
	provider := resourceOfKind(t, resources, ResourceProviderProcess)
	group := resourceOfKind(t, resources, ResourceProviderGroup)
	if _, _, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, processIdentity(t, 700), mustTime(t, at)); err != nil {
		t.Fatal(err)
	}
	return activeRun
}
