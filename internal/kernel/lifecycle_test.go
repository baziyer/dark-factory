package kernel

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"path/filepath"
	"testing"
)

func TestCredentialAuthorityExistsOnlyWhileExactRunIsRunning(t *testing.T) {
	store, run, keys := admittedOrchestratorRun(t)
	defer store.Close()
	ctx := context.Background()
	if _, err := store.AuthenticateAttempt(ctx, keys.AttemptDigest); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("admitted credential = %v", err)
	}
	active, run := activateAllResources(t, store, run, keys, 20)
	session := terminalSessionForRunTest(t, store, run.ID)
	running, err := store.ActivateRun(ctx, run.ID, session.ID, run.Revision, session.Revision, mustTime(t, 30))
	if err != nil {
		t.Fatal(err)
	}
	authority, err := store.AuthenticateAttempt(ctx, keys.AttemptDigest)
	if err != nil || authority.RunID != run.ID || authority.TaskID != run.TaskID || authority.Task() != "run" {
		t.Fatalf("authority = %+v, %v", authority, err)
	}
	forged, _ := AttemptDigestFromBytes(bytes.Repeat([]byte{0xfe}, DigestBytes))
	if _, err := store.AuthenticateAttempt(ctx, forged); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("forged credential = %v", err)
	}
	proposal, _ := NewSuccessProposal("PRIVATE_RESULT")
	finalizing, err := store.ProposeAttemptOutcome(ctx, keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil || finalizing.Phase != RunFinalizing || finalizing.Proposal == nil || !finalizing.Proposal.equal(proposal) {
		t.Fatalf("proposal = %+v, %v", finalizing, err)
	}
	if _, err := store.AuthenticateAttempt(ctx, keys.AttemptDigest); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("finalizing credential = %v", err)
	}
	if _, err := store.ProposeAttemptOutcome(ctx, keys.AttemptDigest, proposal, mustTime(t, 41)); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("revoked proposal replay = %v", err)
	}
	for _, resource := range resourcesForRunTest(t, store, run.ID) {
		if resource.State != ResourceReleasing {
			t.Fatalf("resource not releasing = %+v", resource)
		}
		if resource.Revision.Int64() != active[resource.Kind].Revision.Int64()+1 {
			t.Fatalf("resource revision = %+v", resource)
		}
	}
	_ = running
}

func TestAttemptAuthorityUsesExactEffectiveTask(t *testing.T) {
	for _, test := range []struct {
		name     string
		provider Provider
		title    string
		body     string
		want     string
	}{
		{name: "native body", provider: ProviderCodex, title: "title sentinel", body: "body sentinel", want: "body sentinel"},
		{name: "native title fallback", provider: ProviderCodex, title: "title sentinel", want: "title sentinel"},
		{name: "empty shell program", provider: ProviderShell, title: "must not become shell input", want: ""},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, _ := newTestStore(t)
			defer store.Close()
			ctx := context.Background()
			if _, err := store.SetDispatch(ctx, mustRevision(t, 1), true, mustTime(t, 2)); err != nil {
				t.Fatal(err)
			}
			project, err := store.CreateProject(ctx, NewProject{ID: projectID(t, 210), Name: "task authority", Root: "/task-authority"}, mustTime(t, 3))
			if err != nil {
				t.Fatal(err)
			}
			agent, err := store.CreateAgent(ctx, NewAgent{ID: agentID(t, 211), ProjectID: project.ID, Name: "agent", Role: RoleOrchestrator, Provider: test.provider, ToolBudgetLimit: 1}, mustTime(t, 4))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := store.EnqueueTask(ctx, NewTask{ID: taskID(t, 212), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 213), Title: test.title, Body: test.body}, mustTime(t, 5)); err != nil {
				t.Fatal(err)
			}
			keys := admissionKeys(t, 214, nil)
			admission, err := store.AdmitNext(ctx, keys, mustTime(t, 10))
			if err != nil || !admission.Admitted() {
				t.Fatalf("admission = %+v, %v", admission, err)
			}
			_, active := activateAllResources(t, store, *admission.Run, keys, 20)
			session := terminalSessionForRunTest(t, store, active.ID)
			if _, err := store.ActivateRun(ctx, active.ID, session.ID, active.Revision, session.Revision, mustTime(t, 30)); err != nil {
				t.Fatal(err)
			}
			authority, err := store.AuthenticateAttempt(ctx, keys.AttemptDigest)
			if err != nil || authority.Task() != test.want {
				t.Fatalf("authority task = %q, want %q, err %v", authority.Task(), test.want, err)
			}
			if test.want != "" {
				formatted := fmt.Sprintf("%v %+v %#v", authority, authority, authority)
				encoded, marshalErr := json.Marshal(authority)
				if marshalErr != nil || bytes.Contains([]byte(formatted), []byte(test.want)) || bytes.Contains(encoded, []byte(test.want)) {
					t.Fatalf("attempt authority exposed private task: formatted=%s JSON=%s error=%v", formatted, encoded, marshalErr)
				}
			}
		})
	}
}

func TestCompletionExitOrderPreservesFirstOutcomeAndExactExit(t *testing.T) {
	t.Run("completion then exit", func(t *testing.T) {
		store, run, keys := runningOrchestratorRun(t)
		defer store.Close()
		proposal, _ := NewSuccessProposal("done")
		_, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
		if err != nil {
			t.Fatal(err)
		}
		exit, _ := NewProcessExitCode(1, 0, mustTime(t, 41))
		observed := recordRunnerExitForTest(t, store, run.ID, exit, 42)
		if err != nil || observed.Proposal == nil || !observed.Proposal.equal(proposal) || observed.RunnerExit == nil || !observed.RunnerExit.equal(exit) {
			t.Fatalf("observed = %+v, %v", observed, err)
		}
	})
	t.Run("exit then completion", func(t *testing.T) {
		store, run, keys := runningOrchestratorRun(t)
		defer store.Close()
		_ = run
		// A runner exit can no longer be observed as a standalone lifecycle
		// transition; the typed outcome always wins the running run.
		success, _ := NewSuccessProposal("too late")
		observed, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, success, mustTime(t, 42))
		if err != nil {
			t.Fatalf("completion = %v", err)
		}
		if observed.Proposal == nil || !observed.Proposal.equal(success) || observed.RunnerExit != nil {
			t.Fatalf("completion changed outcome = %+v", observed)
		}
	})
}

func TestResourceGraphAndFinalizerAreOneWay(t *testing.T) {
	store, run, keys := admittedOrchestratorRun(t)
	defer store.Close()
	ctx := context.Background()
	resources := resourcesForRunTest(t, store, run.ID)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	pathIdentity, _ := NewPathResourceIdentity(11, 12)
	wrongPath, _ := NewPathResourceIdentity(11, 13)
	activeRuntime, err := store.ActivateResource(ctx, run.ID, runtime.ID, runtime.Revision, pathIdentity, mustTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	replayedRuntime, err := store.ActivateResource(ctx, run.ID, runtime.ID, runtime.Revision, pathIdentity, mustTime(t, 21))
	if err != nil || replayedRuntime.Revision != activeRuntime.Revision {
		t.Fatalf("activation replay = %+v, %v", replayedRuntime, err)
	}
	if _, err := store.ReleaseResource(ctx, run.ID, runtime.ID, activeRuntime.Revision, wrongPath, mustTime(t, 21)); !errors.Is(err, ErrConflict) {
		t.Fatalf("wrong release identity = %v", err)
	}
	if _, err := store.BeginResourceRelease(ctx, run.ID, runtime.ID, activeRuntime.Revision, pathIdentity, mustTime(t, 22)); !errors.Is(err, ErrConflict) {
		t.Fatalf("cleanup while admitted = %v", err)
	}
	if _, err := store.MarkResourceUnresolved(ctx, run.ID, runtime.ID, activeRuntime.Revision, pathIdentity, "too early", mustTime(t, 22)); !errors.Is(err, ErrConflict) {
		t.Fatalf("unresolved while admitted = %v", err)
	}
	failure, _ := NewFailureProposal(FailureActivation, "pre-exec cleanup")
	if _, err := store.FailRun(ctx, run.ID, run.Revision, failure, mustTime(t, 23)); err != nil {
		t.Fatal(err)
	}
	releasing, _, _ := store.Resource(ctx, runtime.ID)
	unresolved, err := store.MarkResourceUnresolved(ctx, run.ID, runtime.ID, releasing.Revision, pathIdentity, "identity uncertain", mustTime(t, 24))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ActivateResource(ctx, run.ID, runtime.ID, unresolved.Revision, pathIdentity, mustTime(t, 25)); !errors.Is(err, ErrConflict) {
		t.Fatalf("unresolved resurrection = %v", err)
	}
	released, err := store.ReleaseResource(ctx, run.ID, runtime.ID, unresolved.Revision, pathIdentity, mustTime(t, 26))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.ActivateResource(ctx, run.ID, runtime.ID, released.Revision, pathIdentity, mustTime(t, 27)); !errors.Is(err, ErrConflict) {
		t.Fatalf("released resurrection = %v", err)
	}
	if _, err := store.MarkResourceUnresolved(ctx, run.ID, runtime.ID, released.Revision, pathIdentity, "late", mustTime(t, 28)); !errors.Is(err, ErrConflict) {
		t.Fatalf("released unresolved = %v", err)
	}

	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	processIdentity := processIdentity(t, 101)
	runner, _, _ = store.Resource(ctx, runner.ID)
	if _, err := store.ReleaseResource(ctx, run.ID, runner.ID, runner.Revision, processIdentity, mustTime(t, 29)); !errors.Is(err, ErrConflict) {
		t.Fatalf("declared release without empty proof = %v", err)
	}
	if _, err := store.ReleaseResource(ctx, run.ID, runner.ID, runner.Revision, EmptyResourceIdentity(), mustTime(t, 30)); !errors.Is(err, ErrConflict) {
		t.Fatalf("generic declared runner release = %v", err)
	}
	_ = keys
}

func TestFinalizerRequiresEveryReleasedResourceAndExactTask(t *testing.T) {
	store, admitted, keys := runningOrchestratorRun(t)
	defer store.Close()
	proposal, _ := NewSuccessProposal("verified result")
	finalizing, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	finalizing = observeMissingProcessExits(t, store, admitted.ID, 41)
	resources := resourcesForRunTest(t, store, admitted.ID)
	if _, err := store.FinalizeRun(context.Background(), admitted.ID, finalizing.Revision, mustTime(t, 60)); !errors.Is(err, ErrConflict) {
		t.Fatalf("terminal with live resource = %v", err)
	}
	last := resourceOfKind(t, resources, ResourceRuntimeRoot)
	if _, err := store.MarkResourceUnresolved(context.Background(), admitted.ID, last.ID, last.Revision, last.Identity, "cleanup uncertain", mustTime(t, 61)); err != nil {
		t.Fatal(err)
	}
	unresolved, _, _ := store.Resource(context.Background(), last.ID)
	if _, err := store.FinalizeRun(context.Background(), admitted.ID, finalizing.Revision, mustTime(t, 62)); !errors.Is(err, ErrConflict) {
		t.Fatalf("terminal with unresolved resource = %v", err)
	}
	if _, err := store.ReleaseResource(context.Background(), admitted.ID, last.ID, unresolved.Revision, unresolved.Identity, mustTime(t, 63)); err != nil {
		t.Fatal(err)
	}
	finalizing = closeTerminalSessionAtCurrent(t, store, admitted.ID, 64)
	before, _ := store.Factory(context.Background())
	terminal, err := store.FinalizeRun(context.Background(), admitted.ID, finalizing.Revision, mustTime(t, 64))
	if err != nil || terminal.Phase != RunTerminal || terminal.Terminal == nil || !terminal.Terminal.equal(proposal) {
		t.Fatalf("terminal = %+v, %v", terminal, err)
	}
	freshTask, _, _ := store.Task(context.Background(), admitted.TaskID)
	if freshTask.Status != TaskSucceeded || freshTask.Result != "verified result" {
		t.Fatalf("terminal task = %+v", freshTask)
	}
	after, _ := store.Factory(context.Background())
	if after.Head.Int64() != before.Head.Int64()+3 {
		t.Fatalf("terminal invalidations before=%+v after=%+v", before, after)
	}
	replay, err := store.FinalizeRun(context.Background(), admitted.ID, finalizing.Revision, mustTime(t, 0))
	if err != nil || replay.Revision != terminal.Revision {
		t.Fatalf("finalizer replay = %+v, %v", replay, err)
	}
	afterReplay, _ := store.Factory(context.Background())
	if afterReplay.Head != after.Head {
		t.Fatalf("duplicate terminal invalidation: before=%+v after=%+v", after, afterReplay)
	}
}

func TestConfiguredWorkerSuccessCannotFinalizeWithoutVerifierState(t *testing.T) {
	success, _ := NewSuccessProposal("done")
	for _, policy := range []VerificationPolicy{VerificationRustWorkspaceTest, VerificationGoWorkspaceTest} {
		store, finalizing := finalizingReleasedRun(t, RoleWorker, policy, success)
		before, _ := store.Factory(context.Background())
		if _, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 80)); !errors.Is(err, ErrConflict) {
			store.Close()
			t.Fatalf("policy %s finalization = %v", policy.String(), err)
		}
		fresh, found, err := store.Run(context.Background(), finalizing.ID)
		after, _ := store.Factory(context.Background())
		store.Close()
		if err != nil || !found || fresh.Phase != RunFinalizing || fresh.Terminal != nil || fresh.CredentialRevokedAt == nil || after.Head != before.Head {
			t.Fatalf("policy %s footprint run=%+v found=%v err=%v before=%+v after=%+v", policy.String(), fresh, found, err, before, after)
		}
	}
}

func TestOnlyConfiguredWorkerSuccessRequiresLaterVerifier(t *testing.T) {
	success, _ := NewSuccessProposal("done")
	blocked, _ := NewBlockedProposal("blocked")
	failed, _ := NewFailureProposal(FailureInternal, "failed")
	cancelled, _ := NewCancelledProposal("cancelled")
	tests := []struct {
		name     string
		role     AgentRole
		policy   VerificationPolicy
		proposal Proposal
	}{
		{name: "orchestrator", role: RoleOrchestrator, policy: VerificationGoWorkspaceTest, proposal: success},
		{name: "none policy", role: RoleWorker, policy: VerificationNone, proposal: success},
		{name: "blocked", role: RoleWorker, policy: VerificationGoWorkspaceTest, proposal: blocked},
		{name: "failed", role: RoleWorker, policy: VerificationGoWorkspaceTest, proposal: failed},
		{name: "cancelled", role: RoleWorker, policy: VerificationGoWorkspaceTest, proposal: cancelled},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			store, finalizing := finalizingReleasedRun(t, test.role, test.policy, test.proposal)
			defer store.Close()
			terminal, err := finalizeTestRun(t, store, finalizing, 81)
			if err != nil || terminal.Phase != RunTerminal || terminal.Terminal == nil || !terminal.Terminal.equal(test.proposal) {
				t.Fatalf("terminal = %+v, %v", terminal, err)
			}
		})
	}
}

func TestAttemptRequestedFailureHasOneTypedDurableCode(t *testing.T) {
	store, running, keys := runningOrchestratorRun(t)
	defer store.Close()

	proposal, err := NewFailureProposal(FailureAttempt, "")
	if err != nil || proposal.Code() != FailureAttempt || proposal.Code().String() != "attempt" {
		t.Fatalf("attempt failure proposal = %+v, %v", proposal, err)
	}
	finalizing, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil || finalizing.Phase != RunFinalizing || finalizing.Proposal == nil || finalizing.Proposal.Code() != FailureAttempt || finalizing.Proposal.Detail() != "" {
		t.Fatalf("durable attempt failure = %+v, %v", finalizing, err)
	}
	read, found, err := store.Run(context.Background(), running.ID)
	if err != nil || !found || read.Proposal == nil || read.Proposal.Code() != FailureAttempt || read.Proposal.Code().String() != "attempt" {
		t.Fatalf("read attempt failure = %+v, found=%v err=%v", read, found, err)
	}
	finalizing = observeMissingProcessExits(t, store, running.ID, 41)
	for index, resource := range resourcesForRunTest(t, store, running.ID) {
		if resource.State == ResourceReleased {
			continue
		}
		if _, err := store.ReleaseResource(context.Background(), running.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, int64(50+index))); err != nil {
			t.Fatal(err)
		}
	}
	finalizing = closeTerminalSessionAtCurrent(t, store, running.ID, 60)
	terminal, err := store.FinalizeRun(context.Background(), running.ID, finalizing.Revision, mustTime(t, 60))
	if err != nil || terminal.Phase != RunTerminal || terminal.Terminal == nil || terminal.Terminal.Code() != FailureAttempt || terminal.Terminal.Detail() != "" {
		t.Fatalf("terminal attempt failure = %+v, %v", terminal, err)
	}
	read, found, err = store.Run(context.Background(), running.ID)
	if err != nil || !found || read.Terminal == nil || read.Terminal.Code() != FailureAttempt || read.Terminal.Code().String() != "attempt" || read.Terminal.Detail() != "" {
		t.Fatalf("read terminal attempt failure = %+v, found=%v err=%v", read, found, err)
	}
}

func TestVerificationAndTerminalCorruptionFailClosed(t *testing.T) {
	tests := []struct {
		name   string
		mutate string
	}{
		{name: "unknown run policy", mutate: `UPDATE runs SET verification_policy = 'mystery'`},
		{name: "unknown failure", mutate: `UPDATE runs SET terminal_code = 'mystery'`},
		{name: "arbitrary mismatch", mutate: `UPDATE runs SET terminal_kind = 'blocked', terminal_code = NULL, terminal_detail = 'arbitrary', terminal_result = NULL`},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			proposal, _ := NewFailureProposal(FailureInternal, "failed")
			store, finalizing := finalizingReleasedRun(t, RoleWorker, VerificationGoWorkspaceTest, proposal)
			path := storePath(t, store)
			if _, err := finalizeTestRun(t, store, finalizing, 80); err != nil {
				t.Fatal(err)
			}
			corruptSQL(t, store, test.mutate)
			if _, _, err := store.Run(context.Background(), finalizing.ID); !errors.Is(err, ErrCorruptState) {
				t.Fatalf("corrupt Run = %v", err)
			}
			if err := store.Close(); err != nil {
				t.Fatal(err)
			}
			before := captureDatabaseEvidence(t, path)
			if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
				t.Fatalf("corrupt Open = %v", err)
			}
			assertDatabaseEvidenceUnchanged(t, path, before)
		})
	}

	t.Run("unknown project policy", func(t *testing.T) {
		store, _, project, _ := newAdmissionStore(t, RoleOrchestrator, 2)
		defer store.Close()
		corruptSQL(t, store, `UPDATE projects SET verification_policy = 'mystery' WHERE id = ?`, project.ID.Bytes())
		if _, _, err := store.Project(context.Background(), project.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("corrupt Project = %v", err)
		}
	})
}

func TestFinalizingConsumesFactoryCapacity(t *testing.T) {
	store, _, project, firstAgent := newAdmissionStore(t, RoleOrchestrator, 1)
	defer store.Close()
	firstTask, _ := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 120), ProjectID: project.ID, AssignedAgentID: firstAgent.ID, IncarnationID: incarnationID(t, 121), Title: "first"}, mustTime(t, 5))
	_ = firstTask
	firstKeys := admissionKeys(t, 122, nil)
	first, err := store.AdmitNext(context.Background(), firstKeys, mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	failure, _ := NewFailureProposal(FailureSpawn, "spawn failed")
	failRunForTest(t, store, *first.Run, failure, 11)
	secondAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 123), ProjectID: project.ID, Name: "second", Role: RoleOrchestrator, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 12))
	if err != nil {
		t.Fatal(err)
	}
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 124), ProjectID: project.ID, AssignedAgentID: secondAgent.ID, IncarnationID: incarnationID(t, 125), Title: "second"}, mustTime(t, 13))
	result, err := store.AdmitNext(context.Background(), admissionKeys(t, 126, nil), mustTime(t, 14))
	if err != nil || result.Admitted() || result.Reason != NoAdmissionAtCapacity {
		t.Fatalf("capacity result = %+v, %v", result, err)
	}
}

func TestCancelRunIsFirstOutcomeAndRevokesAdmittedAuthority(t *testing.T) {
	store, run, keys := admittedOrchestratorRun(t)
	defer store.Close()
	runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(36, 37)
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
		t.Fatal(err)
	}
	cancelled, err := store.CancelRun(context.Background(), run.ID, run.Revision, "operator cancelled", mustTime(t, 20))
	if err != nil || cancelled.Phase != RunFinalizing || cancelled.Proposal == nil || cancelled.Proposal.kind != OutcomeCancelled || cancelled.CredentialRevokedAt == nil {
		t.Fatalf("cancelled = %+v, %v", cancelled, err)
	}
	replay, err := store.CancelRun(context.Background(), run.ID, run.Revision, "operator cancelled", mustTime(t, 99))
	if err != nil || replay.Revision != cancelled.Revision || replay.UpdatedAt != cancelled.UpdatedAt {
		t.Fatalf("cancel replay = %+v, %v", replay, err)
	}
	if _, err := store.CancelRun(context.Background(), run.ID, cancelled.Revision, "different", mustTime(t, 21)); !errors.Is(err, ErrConflict) {
		t.Fatalf("different cancel = %v", err)
	}
	if _, err := store.AuthenticateAttempt(context.Background(), keys.AttemptDigest); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("cancelled digest = %v", err)
	}
}

func TestFailRunRevokesRunningAuthorityAndPreservesFirstOutcome(t *testing.T) {
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	failure, _ := NewFailureProposal(FailureProtocol, "daemon control failed")
	failed, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 40))
	if err != nil || failed.Phase != RunFinalizing || failed.Proposal == nil || !failed.Proposal.equal(failure) || failed.CredentialRevokedAt == nil {
		t.Fatalf("FailRun = %+v, %v", failed, err)
	}
	if _, err := store.AuthenticateAttempt(context.Background(), keys.AttemptDigest); !errors.Is(err, ErrUnauthorized) {
		t.Fatalf("old attempt credential = %v", err)
	}
	for _, resource := range resourcesForRunTest(t, store, run.ID) {
		if resource.State != ResourceReleasing {
			t.Fatalf("%s state = %s", resource.Kind.String(), resource.State.String())
		}
	}
	replayed, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 99))
	if err != nil || replayed.Revision != failed.Revision {
		t.Fatalf("exact replay = %+v, %v", replayed, err)
	}
	different, _ := NewFailureProposal(FailureInternal, "different")
	if _, err := store.FailRun(context.Background(), run.ID, failed.Revision, different, mustTime(t, 41)); !errors.Is(err, ErrConflict) {
		t.Fatalf("overwrite first failure = %v", err)
	}
	success, _ := NewSuccessProposal("invalid daemon outcome")
	if _, err := store.FailRun(context.Background(), run.ID, failed.Revision, success, mustTime(t, 41)); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("non-failure daemon outcome = %v", err)
	}
}

func TestFailRunRacesAttemptSuccessAndCancellationWithoutOverwrite(t *testing.T) {
	for _, test := range []struct {
		name string
		race func(*Store, Run, AdmissionKeys) error
	}{
		{name: "attempt success", race: func(store *Store, run Run, keys AdmissionKeys) error {
			proposal, _ := NewSuccessProposal("attempt won")
			_, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 41))
			return err
		}},
		{name: "operator cancellation", race: func(store *Store, run Run, _ AdmissionKeys) error {
			_, err := store.CancelRun(context.Background(), run.ID, run.Revision, "operator won", mustTime(t, 41))
			return err
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			store, run, keys := runningOrchestratorRun(t)
			second, err := Open(context.Background(), storePath(t, store))
			if err != nil {
				store.Close()
				t.Fatal(err)
			}
			defer store.Close()
			defer second.Close()
			failure, _ := NewFailureProposal(FailureProtocol, "daemon won")
			start := make(chan struct{})
			results := make(chan error, 2)
			go func() {
				<-start
				_, raceErr := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 40))
				results <- raceErr
			}()
			go func() {
				<-start
				results <- test.race(second, run, keys)
			}()
			close(start)
			firstErr, secondErr := <-results, <-results
			if firstErr != nil && secondErr != nil {
				t.Fatalf("both racers failed: %v; %v", firstErr, secondErr)
			}
			fresh, found, err := store.Run(context.Background(), run.ID)
			if err != nil || !found || fresh.Phase != RunFinalizing || fresh.Proposal == nil || fresh.CredentialRevokedAt == nil {
				t.Fatalf("race result = %+v, found=%v, err=%v", fresh, found, err)
			}
			if fresh.Proposal.Kind() != OutcomeFailed && fresh.Proposal.Kind() != OutcomeSucceeded && fresh.Proposal.Kind() != OutcomeCancelled {
				t.Fatalf("invalid race winner = %+v", fresh.Proposal)
			}
			if _, err := store.AuthenticateAttempt(context.Background(), keys.AttemptDigest); !errors.Is(err, ErrUnauthorized) {
				t.Fatalf("race credential = %v", err)
			}
		})
	}
}

func TestResourceIdentityCannotBeReusedAcrossRuns(t *testing.T) {
	store, _, project, firstAgent := newAdmissionStore(t, RoleWorker, 4)
	defer store.Close()
	secondAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 211), ProjectID: project.ID, Name: "second", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 4))
	if err != nil {
		t.Fatal(err)
	}
	thirdAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 216), ProjectID: project.ID, Name: "third", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 4))
	if err != nil {
		t.Fatal(err)
	}
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 212), ProjectID: project.ID, AssignedAgentID: firstAgent.ID, IncarnationID: incarnationID(t, 213), Title: "first"}, mustTime(t, 5))
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 214), ProjectID: project.ID, AssignedAgentID: secondAgent.ID, IncarnationID: incarnationID(t, 215), Title: "second"}, mustTime(t, 5))
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 217), ProjectID: project.ID, AssignedAgentID: thirdAgent.ID, IncarnationID: incarnationID(t, 218), Title: "third"}, mustTime(t, 5)); err != nil {
		t.Fatal(err)
	}
	first, err := store.AdmitNext(context.Background(), admissionKeys(t, 220, nil), mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	second, err := store.AdmitNext(context.Background(), admissionKeys(t, 230, nil), mustTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	sharedProcess := processIdentity(t, 900)
	firstRuntime := resourceOfKind(t, resourcesForRunTest(t, store, first.Run.ID), ResourceRuntimeRoot)
	firstPath, _ := NewPathResourceIdentity(40, 41)
	if _, err := store.ActivateResource(context.Background(), first.Run.ID, firstRuntime.ID, firstRuntime.Revision, firstPath, mustTime(t, 19)); err != nil {
		t.Fatal(err)
	}
	firstRunner := resourceOfKind(t, resourcesForRunTest(t, store, first.Run.ID), ResourceRunnerProcess)
	firstStarted, firstStarting, err := store.BeginRunnerStart(context.Background(), first.Run.ID, firstRunner.ID, first.Run.Revision, firstRunner.Revision, mustTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.ActivateRunner(context.Background(), first.Run.ID, firstRunner.ID, firstStarted.Revision, firstStarting.Revision, sharedProcess, mustTime(t, 21)); err != nil {
		t.Fatal(err)
	}
	secondRunner := resourceOfKind(t, resourcesForRunTest(t, store, second.Run.ID), ResourceRunnerProcess)
	secondRuntime := resourceOfKind(t, resourcesForRunTest(t, store, second.Run.ID), ResourceRuntimeRoot)
	secondPath, _ := NewPathResourceIdentity(42, 43)
	if _, err := store.ActivateResource(context.Background(), second.Run.ID, secondRuntime.ID, secondRuntime.Revision, secondPath, mustTime(t, 20)); err != nil {
		t.Fatal(err)
	}
	secondStarted, secondStarting, err := store.BeginRunnerStart(context.Background(), second.Run.ID, secondRunner.ID, second.Run.Revision, secondRunner.Revision, mustTime(t, 21))
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.ActivateRunner(context.Background(), second.Run.ID, secondRunner.ID, secondStarted.Revision, secondStarting.Revision, sharedProcess, mustTime(t, 22)); !errors.Is(err, ErrConflict) {
		t.Fatalf("process identity reuse = %v", err)
	}
	// Seed 235 keeps the derived runtime-root locator distinct from the first
	// run's (admissionKeys derives it from seed%20; 240 would collide with 220).
	third, err := store.AdmitNext(context.Background(), admissionKeys(t, 235, nil), mustTime(t, 23))
	if err != nil || !third.Admitted() {
		t.Fatalf("third admission = %+v, %v", third, err)
	}
	thirdRuntime := resourceOfKind(t, resourcesForRunTest(t, store, third.Run.ID), ResourceRuntimeRoot)
	if _, err := store.ActivateResource(context.Background(), third.Run.ID, thirdRuntime.ID, thirdRuntime.Revision, firstPath, mustTime(t, 24)); !errors.Is(err, ErrConflict) {
		t.Fatalf("path identity reuse = %v", err)
	}
}

func TestProviderIdentityPairIsTheOnlySameRunProcessAlias(t *testing.T) {
	store, run, _ := admittedOrchestratorRun(t)
	path := storePath(t, store)
	resources := resourcesForRunTest(t, store, run.ID)
	sharedProvider := processIdentity(t, 930)
	runnerIdentity := processIdentity(t, 931)
	provider := resourceOfKind(t, resources, ResourceProviderProcess)
	group := resourceOfKind(t, resources, ResourceProviderGroup)
	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	runtimeIdentity, _ := NewPathResourceIdentity(930, 1930)
	if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
		t.Fatal(err)
	}
	started, starting, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, started.Revision, starting.Revision, runnerIdentity, mustTime(t, 21)); err != nil {
		t.Fatal(err)
	}
	if _, _, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, runnerIdentity, mustTime(t, 22)); !errors.Is(err, ErrConflict) {
		t.Fatalf("provider pair aliased runner identity: %v", err)
	}
	if _, _, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, sharedProvider, mustTime(t, 23)); err != nil {
		t.Fatalf("provider process/group pair rejected: %v", err)
	}
	pid, pgid, birth, _ := sharedProvider.Process()
	corruptSQL(t, store, `UPDATE resources SET pid = ?, pgid = ?, birth_digest = ? WHERE id = ?`, pid, pgid, birth.Bytes(), runner.ID.Bytes())
	if _, err := store.Snapshot(context.Background()); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("same-run runner/provider durable alias = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	before := captureDatabaseEvidence(t, path)
	if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open aliased process identity = %v", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, before)
}

func TestResourceTransitionsAreCoupledToRunPhaseAndCredential(t *testing.T) {
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	before, _ := store.Factory(context.Background())
	if _, err := store.ReleaseResource(context.Background(), run.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, 35)); !errors.Is(err, ErrConflict) {
		t.Fatalf("running release = %v", err)
	}
	if _, err := store.BeginResourceRelease(context.Background(), run.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, 36)); !errors.Is(err, ErrConflict) {
		t.Fatalf("running begin release = %v", err)
	}
	if _, err := store.MarkResourceUnresolved(context.Background(), run.ID, resource.ID, resource.Revision, resource.Identity, "too early", mustTime(t, 37)); !errors.Is(err, ErrConflict) {
		t.Fatalf("running unresolved = %v", err)
	}
	fresh, _, _ := store.Resource(context.Background(), resource.ID)
	after, _ := store.Factory(context.Background())
	if fresh.State != ResourceActive || fresh.Revision != resource.Revision || after.Head != before.Head {
		t.Fatalf("wrong-phase release footprint resource=%+v before=%+v after=%+v", fresh, before, after)
	}
	if _, err := store.AuthenticateAttempt(context.Background(), keys.AttemptDigest); err != nil {
		t.Fatalf("credential lost after rejected cleanup: %v", err)
	}
	proposal, _ := NewSuccessProposal("done")
	if _, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40)); err != nil {
		t.Fatal(err)
	}
	releasing, _, _ := store.Resource(context.Background(), resource.ID)
	released, err := store.ReleaseResource(context.Background(), run.ID, resource.ID, releasing.Revision, releasing.Identity, mustTime(t, 41))
	if err != nil {
		t.Fatalf("finalizing release: %v", err)
	}
	replay, err := store.ReleaseResource(context.Background(), run.ID, resource.ID, releasing.Revision, releasing.Identity, mustTime(t, 99))
	if err != nil || replay.Revision != released.Revision {
		t.Fatalf("release replay = %+v, %v", replay, err)
	}
}

func TestImpossibleRunningResourceLedgerFailsAuthenticationAndOpen(t *testing.T) {
	store, run, keys := runningOrchestratorRun(t)
	path := storePath(t, store)
	resource := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
	corruptSQL(t, store, `UPDATE resources SET state = 'released', released_at_ms = updated_at_ms WHERE id = ?`, resource.ID.Bytes())
	if _, err := store.AuthenticateAttempt(context.Background(), keys.AttemptDigest); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("credential on impossible ledger = %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatal(err)
	}
	before := captureDatabaseEvidence(t, path)
	if _, err := Open(context.Background(), path); !errors.Is(err, ErrCorruptState) {
		t.Fatalf("Open impossible ledger = %v", err)
	}
	assertDatabaseEvidenceUnchanged(t, path, before)
}

func TestWorkerRunCannotActivateBeforeExactChangeIsAvailable(t *testing.T) {
	store, _, project, agent := newAdmissionStore(t, RoleWorker, 2)
	defer store.Close()
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 130), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 131), Title: "worker"}, mustTime(t, 5))
	candidate := changeID(t, 132)
	keys := admissionKeys(t, 133, &candidate)
	admission, err := store.AdmitNext(context.Background(), keys, mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	_, activatedRun := activateAllResources(t, store, *admission.Run, keys, 20)
	session := terminalSessionForRunTest(t, store, admission.Run.ID)
	if _, err := store.ActivateRun(context.Background(), admission.Run.ID, session.ID, activatedRun.Revision, session.Revision, mustTime(t, 30)); !errors.Is(err, ErrConflict) {
		t.Fatalf("activation before available = %v", err)
	}
	format, _ := NewObjectFormat("sha1")
	commit, _ := NewCommitID(format, bytes.Repeat([]byte{1}, 20))
	digest := changeTreeDigest(t, 2)
	repository, _ := NewFileIdentity(61, 62)
	selection, _ := NewChangeSelection(format, commit, digest, 1, 1, repository)
	stage, _ := NewFileIdentity(3, 4)
	prepared, err := store.RecordChangePrepared(context.Background(), candidate, mustRevision(t, 1), selection, stage, mustTime(t, 32))
	if err != nil {
		t.Fatal(err)
	}
	availability, _ := NewChangeAvailability(digest, 1, 1, stage)
	if _, err := store.MarkChangeAvailable(context.Background(), candidate, prepared.Revision, availability, mustTime(t, 33)); err != nil {
		t.Fatal(err)
	}
	if _, err := store.ActivateRun(context.Background(), admission.Run.ID, session.ID, activatedRun.Revision, session.Revision, mustTime(t, 34)); err != nil {
		t.Fatalf("activation after available: %v", err)
	}
}

func TestTaskGuardAndPermanentDigestUniquenessRollbackTerminalOrAdmission(t *testing.T) {
	t.Run("stale task work revision", func(t *testing.T) {
		store, run, keys := runningOrchestratorRun(t)
		defer store.Close()
		proposal, _ := NewSuccessProposal("done")
		finalizing, _ := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
		finalizing = observeMissingProcessExits(t, store, run.ID, 41)
		for _, resource := range resourcesForRunTest(t, store, run.ID) {
			_, _ = store.ReleaseResource(context.Background(), run.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, 50))
		}
		if _, err := store.writer.Exec(`UPDATE tasks SET work_revision = work_revision + 1 WHERE id = ?`, run.TaskID.Bytes()); err != nil {
			t.Fatal(err)
		}
		before, _ := store.Factory(context.Background())
		if _, err := store.FinalizeRun(context.Background(), run.ID, finalizing.Revision, mustTime(t, 60)); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("stale task finalization = %v", err)
		}
		if _, _, err := store.Run(context.Background(), run.ID); !errors.Is(err, ErrCorruptState) {
			t.Fatalf("corrupt joined run read = %v", err)
		}
		var phase string
		if err := store.writer.QueryRow(`SELECT phase FROM runs WHERE id = ?`, run.ID.Bytes()).Scan(&phase); err != nil {
			t.Fatal(err)
		}
		after, _ := store.Factory(context.Background())
		if phase != RunFinalizing.String() || after.Head != before.Head {
			t.Fatalf("stale guard footprint phase=%s before=%+v after=%+v", phase, before, after)
		}
	})
	t.Run("digest never reused", func(t *testing.T) {
		store, run, keys := admittedOrchestratorRun(t)
		defer store.Close()
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(180, 1180)
		if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, 19)); err != nil {
			t.Fatal(err)
		}
		failure, _ := NewFailureProposal(FailureSpawn, "no process")
		finalizing, err := store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTime(t, 20))
		if err != nil {
			t.Fatal(err)
		}
		runtime = resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		if _, err := store.ReleaseResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtime.Identity, mustTime(t, 30)); err != nil {
			t.Fatal(err)
		}
		if _, err := store.FinalizeRun(context.Background(), run.ID, finalizing.Revision, mustTime(t, 40)); err != nil {
			t.Fatal(err)
		}
		agent, _, _ := store.Agent(context.Background(), run.AgentID)
		_, err = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 180), ProjectID: run.ProjectID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 181), Title: "retry"}, mustTime(t, 41))
		if err != nil {
			t.Fatal(err)
		}
		reuse := admissionKeys(t, 182, nil)
		reuse.AttemptDigest = keys.AttemptDigest
		before := admissionFootprint(t, store)
		if _, err := store.AdmitNext(context.Background(), reuse, mustTime(t, 42)); !errors.Is(err, ErrConflict) {
			t.Fatalf("digest reuse = %v", err)
		}
		after := admissionFootprint(t, store)
		if before != after {
			t.Fatalf("digest conflict footprint before=%+v after=%+v", before, after)
		}
	})
}

func TestConcurrentCompletionAndExitHaveOneImmutableWinner(t *testing.T) {
	store, run, keys := runningOrchestratorRun(t)
	path := storePath(t, store)
	second, err := Open(context.Background(), path)
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	defer store.Close()
	defer second.Close()
	proposal, _ := NewSuccessProposal("winner")
	exit, _ := NewProcessExitCode(1, 7, mustTime(t, 40))
	start := make(chan struct{})
	errs := make(chan error, 2)
	go func() {
		<-start
		_, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 41))
		errs <- err
	}()
	go func() {
		<-start
		_, err := second.ObserveProviderExit(context.Background(), run.ID, run.Revision, registeredProcessIdentity(t, store, run.ID, ResourceProviderProcess), exit, mustTime(t, 41))
		errs <- err
	}()
	close(start)
	accepted := 0
	for range 2 {
		err := <-errs
		if err == nil {
			accepted++
		} else if !errors.Is(err, ErrUnauthorized) && !errors.Is(err, ErrRevisionConflict) && !errors.Is(err, ErrConflict) {
			t.Fatalf("losing request = %v", err)
		}
	}
	if accepted != 1 {
		t.Fatalf("accepted requests = %d", accepted)
	}
	fresh, _, _ := store.Run(context.Background(), run.ID)
	if fresh.Phase != RunFinalizing || fresh.Proposal == nil {
		t.Fatalf("race run = %+v", fresh)
	}
	if fresh.Proposal.kind == OutcomeSucceeded {
		if fresh.ProviderExit != nil {
			t.Fatalf("lost exit was appended: %+v", fresh)
		}
	} else if fresh.Proposal.code != FailureProviderExit || fresh.ProviderExit == nil {
		t.Fatalf("invalid exit winner = %+v", fresh)
	}
}

func TestRecoverableRunsAreCanonicalOrderedAndPrivateStateStaysOutOfPublicProjection(t *testing.T) {
	store, _, project, firstAgent := newAdmissionStore(t, RoleWorker, 4)
	defer store.Close()
	secondAgent, _ := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 141), ProjectID: project.ID, Name: "second", Role: RoleWorker, Provider: ProviderCodex, Model: "MODEL_SENTINEL", ToolBudgetLimit: 2}, mustTime(t, 4))
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 142), ProjectID: project.ID, AssignedAgentID: secondAgent.ID, IncarnationID: incarnationID(t, 143), Title: "second", Body: "BODY_SENTINEL"}, mustTime(t, 5))
	_, _ = store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 144), ProjectID: project.ID, AssignedAgentID: firstAgent.ID, IncarnationID: incarnationID(t, 145), Title: "first"}, mustTime(t, 5))
	first, err := store.AdmitNext(context.Background(), admissionKeys(t, 170, nil), mustTime(t, 10))
	if err != nil {
		t.Fatal(err)
	}
	second, err := store.AdmitNext(context.Background(), admissionKeys(t, 160, nil), mustTime(t, 20))
	if err != nil {
		t.Fatal(err)
	}
	recoverable, err := store.RecoverableRuns(context.Background())
	if err != nil || len(recoverable) != 2 || recoverable[0].Run.ID != first.Run.ID || recoverable[1].Run.ID != second.Run.ID || len(recoverable[0].Resources) != 4 {
		t.Fatalf("recoverable = %+v, %v", recoverable, err)
	}
	snapshot, _ := store.Snapshot(context.Background())
	encoded, _ := json.Marshal(snapshot)
	for _, sentinel := range []string{"BODY_SENTINEL"} {
		if bytes.Contains(encoded, []byte(sentinel)) {
			t.Fatalf("public snapshot leaked %q: %s", sentinel, encoded)
		}
	}
}

func TestResourcesReturnsCanonicalSetAfterTerminalization(t *testing.T) {
	proposal, err := NewFailureProposal(FailureInternal, "terminal resource read")
	if err != nil {
		t.Fatal(err)
	}
	store, finalizing := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, proposal)
	defer store.Close()
	terminal, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 70))
	if err != nil {
		t.Fatal(err)
	}
	resources, err := store.Resources(context.Background(), terminal.ID)
	if err != nil {
		t.Fatal(err)
	}
	want := []ResourceKind{ResourceProviderGroup, ResourceProviderProcess, ResourceRunnerProcess, ResourceRuntimeRoot}
	if len(resources) != len(want) {
		t.Fatalf("resource count = %d", len(resources))
	}
	for index, resource := range resources {
		if resource.Kind != want[index] || resource.State != ResourceReleased || resource.RunID != terminal.ID {
			t.Fatalf("resource[%d] = %+v", index, resource)
		}
	}
	if _, err := store.Resources(context.Background(), RunID{}); !errors.Is(err, ErrInvalidValue) {
		t.Fatalf("zero run resources = %v", err)
	}
	if _, err := store.Resources(context.Background(), runID(t, 99)); !errors.Is(err, ErrNotFound) {
		t.Fatalf("missing run resources = %v", err)
	}
}

func admittedOrchestratorRun(t *testing.T) (*Store, Run, AdmissionKeys) {
	t.Helper()
	store, _, project, agent := newAdmissionStore(t, RoleOrchestrator, 4)
	_, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 150), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 151), Title: "run"}, mustTime(t, 5))
	if err != nil {
		t.Fatal(err)
	}
	keys := admissionKeys(t, 152, nil)
	result, err := store.AdmitNext(context.Background(), keys, mustTime(t, 10))
	if err != nil || !result.Admitted() {
		store.Close()
		t.Fatalf("admit = %+v, %v", result, err)
	}
	return store, *result.Run, keys
}

func runningOrchestratorRun(t *testing.T) (*Store, Run, AdmissionKeys) {
	t.Helper()
	store, run, keys := admittedOrchestratorRun(t)
	_, run = activateAllResources(t, store, run, keys, 20)
	session := terminalSessionForRunTest(t, store, run.ID)
	running, err := store.ActivateRun(context.Background(), run.ID, session.ID, run.Revision, session.Revision, mustTime(t, 30))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	return store, running, keys
}

func finalizingReleasedRun(t *testing.T, role AgentRole, policy VerificationPolicy, proposal Proposal) (*Store, Run) {
	t.Helper()
	path := filepath.Join(t.TempDir(), "kernel.db")
	store, err := createTestStore(context.Background(), path, FactoryConfig{DispatchEnabled: true, Capacity: 2}, mustTime(t, 1))
	if err != nil {
		t.Fatal(err)
	}
	project, err := store.CreateProject(context.Background(), NewProject{ID: projectID(t, 240), Name: "verified", Root: "/verified", VerificationPolicy: policy}, mustTime(t, 2))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	agent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 241), ProjectID: project.ID, Name: "agent", Role: role, Provider: ProviderCodex, ToolBudgetLimit: 2}, mustTime(t, 3))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	task, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 242), ProjectID: project.ID, AssignedAgentID: agent.ID, IncarnationID: incarnationID(t, 243), Title: "verify"}, mustTime(t, 4))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	var candidate *ChangeID
	if role == RoleWorker {
		value := changeID(t, 244)
		candidate = &value
	}
	keys := admissionKeys(t, 245, candidate)
	admission, err := store.AdmitNext(context.Background(), keys, mustTime(t, 10))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	if candidate != nil {
		format, _ := NewObjectFormat("sha1")
		commit, _ := NewCommitID(format, bytes.Repeat([]byte{1}, 20))
		digest := changeTreeDigest(t, 2)
		repository, _ := NewFileIdentity(61, 62)
		selection, _ := NewChangeSelection(format, commit, digest, 1, 1, repository)
		stage, _ := NewFileIdentity(3, 4)
		prepared, err := store.RecordChangePrepared(context.Background(), *candidate, mustRevision(t, 1), selection, stage, mustTime(t, 12))
		if err != nil {
			store.Close()
			t.Fatal(err)
		}
		availability, _ := NewChangeAvailability(digest, 1, 1, stage)
		if _, err := store.MarkChangeAvailable(context.Background(), *candidate, prepared.Revision, availability, mustTime(t, 13)); err != nil {
			store.Close()
			t.Fatal(err)
		}
	}
	_, activatedRun := activateAllResources(t, store, *admission.Run, keys, 20)
	session := terminalSessionForRunTest(t, store, admission.Run.ID)
	running, err := store.ActivateRun(context.Background(), admission.Run.ID, session.ID, activatedRun.Revision, session.Revision, mustTime(t, 30))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	finalizing, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil {
		store.Close()
		t.Fatal(err)
	}
	finalizing = observeMissingProcessExits(t, store, running.ID, 41)
	for index, resource := range resourcesForRunTest(t, store, running.ID) {
		if resource.State == ResourceReleased {
			continue
		}
		if _, err := store.ReleaseResource(context.Background(), running.ID, resource.ID, resource.Revision, resource.Identity, mustTime(t, int64(50+index))); err != nil {
			store.Close()
			t.Fatal(err)
		}
	}
	current, found, err := store.Run(context.Background(), running.ID)
	if err != nil || !found {
		store.Close()
		t.Fatalf("read finalizing run: %+v, found=%v, err=%v", current, found, err)
	}
	closeTerminalSessionAtCurrent(t, store, running.ID, 53)
	finalizing, found, err = store.Run(context.Background(), running.ID)
	if err != nil || !found {
		store.Close()
		t.Fatalf("read closed-session run: %+v, found=%v, err=%v", finalizing, found, err)
	}
	_ = task
	return store, finalizing
}

func finalizeTestRun(t *testing.T, store *Store, run Run, at int64) (Run, error) {
	t.Helper()
	if run.Role != RoleWorker {
		return store.FinalizeRun(context.Background(), run.ID, run.Revision, mustTime(t, at))
	}
	change, found, err := store.Change(context.Background(), *run.ChangeID)
	if err != nil || !found {
		if err == nil {
			err = ErrCorruptState
		}
		return Run{}, fmt.Errorf("read worker Change for settlement: %w", err)
	}
	if change.Phase == ChangeReserved || change.Phase == ChangePrepared {
		settlement, err := NewAbandonedChangeSettlement(change.Revision)
		if err != nil {
			return Run{}, err
		}
		result, err := store.FinalizeWorkerRun(context.Background(), run.ID, run.Revision, settlement, mustTime(t, at))
		if err != nil {
			return Run{}, fmt.Errorf("finalize abandoned worker Change: %w", err)
		}
		return result, nil
	}
	if change.Selection == nil || change.TreeIdentity == nil {
		return Run{}, fmt.Errorf("read worker Change for settlement: %w", ErrCorruptState)
	}
	availability, err := NewChangeAvailability(change.Selection.commitment, change.Selection.entries, change.Selection.bytes, *change.TreeIdentity)
	if err != nil {
		return Run{}, err
	}
	settlement, err := NewRetainedChangeSettlement(change.Revision, availability)
	if err != nil {
		return Run{}, err
	}
	result, err := store.FinalizeWorkerRun(context.Background(), run.ID, run.Revision, settlement, mustTime(t, at))
	if err != nil {
		return Run{}, fmt.Errorf("finalize worker Change settlement: %w", err)
	}
	return result, nil
}

func activateAllResources(t *testing.T, store *Store, run Run, keys AdmissionKeys, at int64) (map[ResourceKind]Resource, Run) {
	t.Helper()
	result := map[ResourceKind]Resource{}
	resources := resourcesForRunTest(t, store, run.ID)
	runtime := resourceOfKind(t, resources, ResourceRuntimeRoot)
	runtimeIdentity, err := NewPathResourceIdentity(10, 100)
	if err != nil {
		t.Fatal(err)
	}
	activeRuntime, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, at))
	if err != nil {
		t.Fatal(err)
	}
	result[ResourceRuntimeRoot] = activeRuntime
	runner := resourceOfKind(t, resources, ResourceRunnerProcess)
	startedRun, startingRunner, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, at+1))
	if err != nil {
		t.Fatal(err)
	}
	activeRun, activeRunner, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, startedRun.Revision, startingRunner.Revision, processIdentity(t, 202), mustTime(t, at+2))
	if err != nil {
		t.Fatal(err)
	}
	result[ResourceRunnerProcess] = activeRunner
	provider := resourceOfKind(t, resources, ResourceProviderProcess)
	group := resourceOfKind(t, resources, ResourceProviderGroup)
	activeProvider, activeGroup, err := store.ActivateProviderResources(context.Background(), run.ID, provider.ID, provider.Revision, group.ID, group.Revision, processIdentity(t, 250), mustTime(t, at+3))
	if err != nil {
		t.Fatal(err)
	}
	result[ResourceProviderProcess] = activeProvider
	result[ResourceProviderGroup] = activeGroup
	_ = activeRun
	return result, activeRun
}

func processIdentity(t *testing.T, seed int64) ResourceIdentity {
	t.Helper()
	birth, err := BirthDigestFromBytes(bytes.Repeat([]byte{byte(seed)}, DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	identity, err := NewProcessResourceIdentity(seed+1000, seed+2000, birth)
	if err != nil {
		t.Fatal(err)
	}
	return identity
}

func registeredProcessIdentity(t testing.TB, store *Store, runID RunID, kind ResourceKind) ResourceIdentity {
	t.Helper()
	resource := resourceOfKindTB(t, resourcesForRunTB(t, store, runID), kind)
	if resource.Identity.Empty() {
		t.Fatalf("%s identity is empty", kind.String())
	}
	return resource.Identity
}

func failRunForTest(t testing.TB, store *Store, run Run, failure Proposal, at int64) Run {
	t.Helper()
	runtime := resourceOfKindTB(t, resourcesForRunTB(t, store, run.ID), ResourceRuntimeRoot)
	var failed Run
	var err error
	if runtime.State == ResourceDeclared {
		failed, err = store.FailRunWithRuntimeAbsent(context.Background(), run.ID, runtime.ID, run.Revision, runtime.Revision, failure, mustTimeTB(t, at))
	} else {
		failed, err = store.FailRun(context.Background(), run.ID, run.Revision, failure, mustTimeTB(t, at))
	}
	if err != nil {
		t.Fatal(err)
	}
	return failed
}

func resourcesForRunTB(t testing.TB, store *Store, runID RunID) []Resource {
	t.Helper()
	connection, err := store.readerConnection(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	resources, err := resourcesForRun(context.Background(), connection, runID)
	if err != nil {
		t.Fatal(err)
	}
	return resources
}

func resourceOfKindTB(t testing.TB, resources []Resource, kind ResourceKind) Resource {
	t.Helper()
	for _, resource := range resources {
		if resource.Kind == kind {
			return resource
		}
	}
	t.Fatalf("resource %s not found", kind.String())
	return Resource{}
}

func resourceOfKind(t *testing.T, resources []Resource, kind ResourceKind) Resource {
	t.Helper()
	for _, resource := range resources {
		if resource.Kind == kind {
			return resource
		}
	}
	t.Fatalf("missing resource kind %s", kind.String())
	return Resource{}
}

func storePath(t *testing.T, store *Store) string {
	t.Helper()
	var path string
	connection, err := store.readerConnection(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if err := connection.QueryRowContext(context.Background(), `PRAGMA database_list`).Scan(new(int), new(string), &path); err != nil {
		t.Fatal(err)
	}
	return path
}
