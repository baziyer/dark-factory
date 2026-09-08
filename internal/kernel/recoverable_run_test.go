package kernel

import (
	"context"
	"errors"
	"reflect"
	"testing"
)

func TestRecoverableRunExactLookupMatchesPluralEntry(t *testing.T) {
	success, _ := NewSuccessProposal("recoverable")
	store, run := finalizingReleasedRun(t, RoleWorker, VerificationNone, success)
	defer store.Close()

	all, err := store.RecoverableRuns(context.Background())
	if err != nil || len(all) != 1 {
		t.Fatalf("RecoverableRuns = %+v, %v", all, err)
	}
	exact, found, err := store.RecoverableRun(context.Background(), run.ID)
	if err != nil || !found {
		t.Fatalf("RecoverableRun = %+v, found=%v, err=%v", exact, found, err)
	}
	if !reflect.DeepEqual(exact, all[0]) {
		t.Fatalf("exact lookup differs from plural entry: exact=%+v plural=%+v", exact, all[0])
	}
	if exact.Change == nil || len(exact.Resources) != 4 {
		t.Fatalf("exact lookup omitted durable relationships: %+v", exact)
	}
}

func TestRecoverableRunExactLookupReturnsNotFoundForTerminalAndUnknownRuns(t *testing.T) {
	failure, _ := NewFailureProposal(FailureInternal, "terminal")
	store, finalizing := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
	defer store.Close()
	if _, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 70)); err != nil {
		t.Fatal(err)
	}
	for _, id := range []RunID{finalizing.ID, runID(t, 99)} {
		if recovered, found, err := store.RecoverableRun(context.Background(), id); err != nil || found || !reflect.DeepEqual(recovered, RecoverableRun{}) {
			t.Fatalf("RecoverableRun(%s) = %+v, found=%v, err=%v", id, recovered, found, err)
		}
	}
	before := captureWriteFootprint(t, store)
	if _, found, err := store.RecoverableRun(context.Background(), RunID{}); !errors.Is(err, ErrInvalidValue) || found {
		t.Fatalf("zero RecoverableRun = found=%v, err=%v", found, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("zero RecoverableRun changed footprint: before=%+v after=%+v", before, after)
	}
}

func TestRecoverableRunExactLookupValidatesExactRelationships(t *testing.T) {
	store, run, _ := runningOrchestratorRun(t)
	defer store.Close()
	corruptSQL(t, store, `UPDATE resources SET state = 'released', released_at_ms = updated_at_ms WHERE run_id = ? AND kind = 'runtime_root'`, run.ID.Bytes())
	if _, found, err := store.RecoverableRun(context.Background(), run.ID); !errors.Is(err, ErrCorruptState) || found {
		t.Fatalf("corrupt exact relationship = found=%v, err=%v", found, err)
	}
}

func TestRecoverableRunExactLookupRejectsUnrelatedOwnershipCorruption(t *testing.T) {
	store, _, project, firstAgent := newAdmissionStore(t, RoleWorker, 4)
	defer store.Close()
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 180), ProjectID: project.ID, AssignedAgentID: firstAgent.ID, IncarnationID: incarnationID(t, 181), Title: "first"}, mustTime(t, 5)); err != nil {
		t.Fatal(err)
	}
	first, err := store.AdmitNext(context.Background(), admissionKeys(t, 182, nil), mustTime(t, 10))
	if err != nil || !first.Admitted() {
		t.Fatalf("first admission = %+v, %v", first, err)
	}
	secondAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 183), ProjectID: project.ID, Name: "second", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 184), ProjectID: project.ID, AssignedAgentID: secondAgent.ID, IncarnationID: incarnationID(t, 185), Title: "second"}, mustTime(t, 12)); err != nil {
		t.Fatal(err)
	}
	second, err := store.AdmitNext(context.Background(), admissionKeys(t, 186, nil), mustTime(t, 13))
	if err != nil || !second.Admitted() {
		t.Fatalf("second admission = %+v, %v", second, err)
	}
	firstRuntime := resourceOfKind(t, resourcesForRunTest(t, store, first.Run.ID), ResourceRuntimeRoot)
	corruptSQL(t, store, `UPDATE resources SET path = ? WHERE run_id = ? AND kind = 'runtime_root'`, firstRuntime.Path, second.Run.ID.Bytes())
	for _, requestedID := range []RunID{first.Run.ID, runID(t, 187)} {
		if _, found, err := store.RecoverableRun(context.Background(), requestedID); !errors.Is(err, ErrCorruptState) || found {
			t.Fatalf("unrelated ownership corruption for %s = found=%v, err=%v", requestedID, found, err)
		}
	}
}

func TestRecoverableRunExactLookupRejectsUnrelatedIdentityCollision(t *testing.T) {
	store, _, project, firstAgent := newAdmissionStore(t, RoleWorker, 4)
	defer store.Close()
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 190), ProjectID: project.ID, AssignedAgentID: firstAgent.ID, IncarnationID: incarnationID(t, 191), Title: "first"}, mustTime(t, 5)); err != nil {
		t.Fatal(err)
	}
	first, err := store.AdmitNext(context.Background(), admissionKeys(t, 192, nil), mustTime(t, 10))
	if err != nil || !first.Admitted() {
		t.Fatalf("first admission = %+v, %v", first, err)
	}
	secondAgent, err := store.CreateAgent(context.Background(), NewAgent{ID: agentID(t, 193), ProjectID: project.ID, Name: "second", Role: RoleWorker, Provider: ProviderCodex, ToolBudgetLimit: 5}, mustTime(t, 11))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(context.Background(), NewTask{ID: taskID(t, 194), ProjectID: project.ID, AssignedAgentID: secondAgent.ID, IncarnationID: incarnationID(t, 195), Title: "second"}, mustTime(t, 12)); err != nil {
		t.Fatal(err)
	}
	second, err := store.AdmitNext(context.Background(), admissionKeys(t, 196, nil), mustTime(t, 13))
	if err != nil || !second.Admitted() {
		t.Fatalf("second admission = %+v, %v", second, err)
	}
	materializeAdmittedWorkerChange(t, store, *first.Run, 14)
	materializeAdmittedWorkerChange(t, store, *second.Run, 17)
	activateRunnerForRun := func(run Run, pathSeed, identitySeed, at int64) Resource {
		t.Helper()
		runtime := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRuntimeRoot)
		runtimeIdentity, _ := NewPathResourceIdentity(pathSeed, pathSeed+1)
		if _, err := store.ActivateResource(context.Background(), run.ID, runtime.ID, runtime.Revision, runtimeIdentity, mustTime(t, at)); err != nil {
			t.Fatal(err)
		}
		runner := resourceOfKind(t, resourcesForRunTest(t, store, run.ID), ResourceRunnerProcess)
		started, starting, err := store.BeginRunnerStart(context.Background(), run.ID, runner.ID, run.Revision, runner.Revision, mustTime(t, at+1))
		if err != nil {
			t.Fatal(err)
		}
		_, active, err := store.ActivateRunner(context.Background(), run.ID, runner.ID, started.Revision, starting.Revision, processIdentity(t, identitySeed), mustTime(t, at+2))
		if err != nil {
			t.Fatal(err)
		}
		return active
	}
	firstRunner := activateRunnerForRun(*first.Run, 400, 300, 20)
	secondRunner := activateRunnerForRun(*second.Run, 402, 301, 23)
	pid, pgid, birth, ok := firstRunner.Identity.Process()
	if !ok {
		t.Fatal("first runner identity is not a process identity")
	}
	corruptSQL(t, store, `UPDATE resources SET pid = ?, pgid = ?, birth_digest = ? WHERE id = ?`, pid, pgid, birth.Bytes(), secondRunner.ID.Bytes())
	before := captureWriteFootprint(t, store)
	if _, found, err := store.RecoverableRun(context.Background(), first.Run.ID); !errors.Is(err, ErrCorruptState) || found {
		t.Fatalf("unrelated identity collision = found=%v, err=%v", found, err)
	}
	if after := captureWriteFootprint(t, store); after != before {
		t.Fatalf("identity collision lookup footprint before=%+v after=%+v", before, after)
	}
}

func TestRecoverableRunExactLookupAllowsHistoricalTerminalTaskState(t *testing.T) {
	failure, _ := NewFailureProposal(FailureInternal, "terminal")
	store, finalizing := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
	defer store.Close()
	terminal, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 70))
	if err != nil {
		t.Fatal(err)
	}
	corruptSQL(t, store, `UPDATE tasks SET assigned_agent_id = X'91919191919191919191919191919191', work_revision = work_revision + 1, status = 'queued', result = NULL, completed_at_ms = NULL WHERE id = ?`, terminal.TaskID.Bytes())
	if _, found, err := store.RecoverableRun(context.Background(), terminal.ID); err != nil || found {
		t.Fatalf("historical terminal graph = found=%v, err=%v", found, err)
	}
}

func TestTerminalRunRejectsUnownedLaterRunningTaskState(t *testing.T) {
	failure, _ := NewFailureProposal(FailureInternal, "terminal")
	store, finalizing := finalizingReleasedRun(t, RoleOrchestrator, VerificationNone, failure)
	defer store.Close()
	terminal, err := store.FinalizeRun(context.Background(), finalizing.ID, finalizing.Revision, mustTime(t, 70))
	if err != nil {
		t.Fatal(err)
	}
	corruptSQL(t, store, `UPDATE tasks SET work_revision = work_revision + 1, status = 'running', result = NULL, completed_at_ms = NULL WHERE id = ?`, terminal.TaskID.Bytes())
	if _, found, err := store.RecoverableRun(context.Background(), terminal.ID); !errors.Is(err, ErrCorruptState) || found {
		t.Fatalf("unowned later running task = found=%v, err=%v", found, err)
	}
}

func TestRecoverableRunExactLookupReloadsDurableState(t *testing.T) {
	store, run, keys := runningOrchestratorRun(t)
	defer store.Close()
	first, found, err := store.RecoverableRun(context.Background(), run.ID)
	if err != nil || !found || first.Run.Revision != run.Revision || first.Run.Phase != RunRunning {
		t.Fatalf("initial exact lookup = %+v, found=%v, err=%v", first, found, err)
	}
	proposal, _ := NewSuccessProposal("fresh")
	finalizing, err := store.ProposeAttemptOutcome(context.Background(), keys.AttemptDigest, proposal, mustTime(t, 40))
	if err != nil {
		t.Fatal(err)
	}
	second, found, err := store.RecoverableRun(context.Background(), run.ID)
	if err != nil || !found || second.Run.Revision != finalizing.Revision || second.Run.Phase != RunFinalizing || second.Run.Proposal == nil || !second.Run.Proposal.equal(proposal) {
		t.Fatalf("reloaded exact lookup = %+v, found=%v, err=%v", second, found, err)
	}
}
