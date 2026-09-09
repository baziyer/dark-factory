//go:build darwin

package daemon

import (
	"bytes"
	"context"
	"crypto/sha256"
	"database/sql"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

type recoveryFixture struct {
	daemon       *Daemon
	store        *kernel.Store
	parent       *RuntimeParent
	parentPath   string
	changeParent string
	storePath    string
	keys         kernel.AdmissionKeys
	run          kernel.Run
	proof        [32]byte
}

func newRecoveryFixture(t *testing.T, seed byte) *recoveryFixture {
	t.Helper()
	return newRecoveryFixtureWithRole(t, seed, kernel.RoleOrchestrator)
}

func newRecoveryFixtureWithRole(t *testing.T, seed byte, role kernel.AgentRole) *recoveryFixture {
	t.Helper()
	ctx := context.Background()
	root, err := os.MkdirTemp("/private/tmp", "dark-factory-recovery-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(root) })
	root, err = filepath.EvalSymlinks(root)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	homePath := filepath.Join(root, "home")
	if _, err := install.Init(ctx, homePath); err != nil {
		t.Fatal(err)
	}
	home, err := install.OpenOperationalHome(ctx, homePath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = home.Close() })
	runtimes, err := home.Runtimes()
	if err != nil {
		t.Fatal(err)
	}
	parentPath := filepath.Join(homePath, "runtimes")
	parent, err := OpenRuntimeParent(ctx, runtimes, parentPath)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = parent.Close() })
	storePath := filepath.Join(root, "kernel.sqlite")
	store, err := createTestStore(ctx, storePath, kernel.FactoryConfig{Capacity: 1}, mustKernelTime(t, 100))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(9000) })
	if err != nil {
		t.Fatal(err)
	}
	at := mustKernelTime(t, 200)
	projectID := mustProjectID(t, testID(seed))
	if _, err := store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "recovery-project", Root: filepath.Join(root, "repo")}, at); err != nil {
		t.Fatal(err)
	}
	agentID := mustAgentID(t, testID(seed+1))
	if _, err := store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: projectID, Name: "recovery-agent", Role: role, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, at); err != nil {
		t.Fatal(err)
	}
	taskID := mustTaskID(t, testID(seed+2))
	incarnation, err := kernel.IncarnationIDFromBytes(bytes.Repeat([]byte{seed + 3}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnation, Title: "recovery-task"}, at); err != nil {
		t.Fatal(err)
	}
	factory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetDispatch(ctx, factory.Revision, true, at); err != nil {
		t.Fatal(err)
	}
	fixture := &recoveryFixture{daemon: daemon, store: store, parent: parent, parentPath: parentPath, changeParent: filepath.Join(homePath, "changes"), storePath: storePath}
	copy(fixture.proof[:], bytes.Repeat([]byte{seed + 4}, 32))
	proofDigest := sha256.Sum256(fixture.proof[:])
	storedProof, err := kernel.ResultProofDigestFromBytes(proofDigest[:])
	if err != nil {
		t.Fatal(err)
	}
	attemptDigest, err := kernel.AttemptDigestFromBytes(bytes.Repeat([]byte{seed + 5}, kernel.DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	change, err := kernel.ChangeIDFromBytes(bytes.Repeat([]byte{seed + 6}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	runID := mustRunID(t, testID(seed+7))
	fixture.keys = kernel.AdmissionKeys{
		RunID: runID, TerminalSessionID: mustTerminalSessionID(t, testID(seed+8)),
		AttemptDigest: attemptDigest, ResultProofDigest: storedProof, CandidateChangeID: change,
		RuntimeRoot: filepath.Join(parentPath, runID.String()),
		Resources: kernel.AdmissionResourceIDs{
			RuntimeRoot: mustResourceID(t, testID(seed+9)), RunnerProcess: mustResourceID(t, testID(seed+10)),
			ProviderProcess: mustResourceID(t, testID(seed+11)), ProviderGroup: mustResourceID(t, testID(seed+12)),
		},
	}
	admission, err := store.AdmitNext(ctx, fixture.keys, at)
	if err != nil || !admission.Admitted() {
		t.Fatalf("admission = %+v, %v", admission, err)
	}
	fixture.run = *admission.Run
	return fixture
}

// stageRuntime creates the real runtime, publishes a token, durably activates
// the runtime resource, and closes the live handles so the lease is free.
func (fixture *recoveryFixture) stageRuntime(t *testing.T) kernel.ResourceIdentity {
	t.Helper()
	ctx := context.Background()
	runtimeValue, err := CreateRuntime(fixture.parent, fixture.run.ID.String())
	if err != nil {
		t.Fatal(err)
	}
	var token [32]byte
	copy(token[:], bytes.Repeat([]byte{0x77}, 32))
	if _, err := runtimeValue.PublishAttemptToken(ctx, token); err != nil {
		_ = runtimeValue.Close()
		t.Fatal(err)
	}
	binding, err := runtimeValue.Binding()
	if err != nil {
		_ = runtimeValue.Close()
		t.Fatal(err)
	}
	_, fileIdentity, err := binding.Values()
	if err != nil {
		_ = runtimeValue.Close()
		t.Fatal(err)
	}
	identity, err := pathResourceIdentity(fileIdentity)
	if err != nil {
		_ = runtimeValue.Close()
		t.Fatal(err)
	}
	resource, found, err := fixture.store.Resource(ctx, fixture.keys.Resources.RuntimeRoot)
	if err != nil || !found {
		t.Fatalf("runtime resource: found=%v err=%v", found, err)
	}
	if _, err := fixture.store.ActivateResource(ctx, fixture.run.ID, resource.ID, resource.Revision, identity, mustKernelTime(t, 210)); err != nil {
		t.Fatal(err)
	}
	if err := runtimeValue.Close(); err != nil {
		t.Fatal(err)
	}
	return identity
}

func (fixture *recoveryFixture) beginRunnerStart(t *testing.T) {
	t.Helper()
	ctx := context.Background()
	resource, found, err := fixture.store.Resource(ctx, fixture.keys.Resources.RunnerProcess)
	if err != nil || !found {
		t.Fatalf("runner resource: found=%v err=%v", found, err)
	}
	run, _, err := fixture.store.BeginRunnerStart(ctx, fixture.run.ID, resource.ID, fixture.currentRun(t).Revision, resource.Revision, mustKernelTime(t, 220))
	if err != nil {
		t.Fatal(err)
	}
	fixture.run = run
}

func (fixture *recoveryFixture) activateRunner(t *testing.T) kernel.ResourceIdentity {
	t.Helper()
	ctx := context.Background()
	// The PID range top on Darwin with a synthetic birth guarantees a
	// positive absence observation for an identity that can never be live.
	identity, err := processResourceIdentity(runner.Identity{PID: 99998, PGID: 99998, Birth: runner.Birth{Seconds: 1700, Microseconds: 1}})
	if err != nil {
		t.Fatal(err)
	}
	resource, found, err := fixture.store.Resource(ctx, fixture.keys.Resources.RunnerProcess)
	if err != nil || !found {
		t.Fatalf("runner resource: found=%v err=%v", found, err)
	}
	run, _, err := fixture.store.ActivateRunner(ctx, fixture.run.ID, resource.ID, fixture.currentRun(t).Revision, resource.Revision, identity, mustKernelTime(t, 230))
	if err != nil {
		t.Fatal(err)
	}
	fixture.run = run
	return identity
}

func (fixture *recoveryFixture) currentRun(t *testing.T) kernel.Run {
	t.Helper()
	run, found, err := fixture.store.Run(context.Background(), fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("run read: found=%v err=%v", found, err)
	}
	fixture.run = run
	return run
}

func (fixture *recoveryFixture) writeMarker(t *testing.T, name string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(fixture.parentPath, fixture.run.ID.String(), name), nil, 0o600); err != nil {
		t.Fatal(err)
	}
}

func (fixture *recoveryFixture) writeArtifact(t *testing.T, body []byte) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runner.AttemptResultSpoolName), body, 0o600); err != nil {
		t.Fatal(err)
	}
}

func (fixture *recoveryFixture) sweep(t *testing.T) RecoveredRunDisposition {
	t.Helper()
	dispositions, err := fixture.daemon.RecoverAbandonedRuns(context.Background(), fixture.parent, fixture.changeParent)
	if err != nil {
		t.Fatal(err)
	}
	for _, disposition := range dispositions {
		if disposition.RunID == fixture.run.ID {
			return disposition
		}
	}
	return RecoveredRunDisposition{}
}

func (fixture *recoveryFixture) resourceStates(t *testing.T) map[kernel.ResourceKind]kernel.Resource {
	t.Helper()
	resources, err := fixture.store.Resources(context.Background(), fixture.run.ID)
	if err != nil {
		t.Fatal(err)
	}
	byKind := make(map[kernel.ResourceKind]kernel.Resource, len(resources))
	for _, resource := range resources {
		byKind[resource.Kind] = resource
	}
	return byKind
}

func TestRecoverySweepFailsRunWhoseRuntimeIsPositivelyAbsent(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x10)
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredRuntimeAbsent || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.Proposal == nil || run.Proposal.Code() != kernel.FailureSpawn || run.Terminal == nil || run.Terminal.Code() != kernel.FailureSpawn {
		t.Fatalf("recovered run = %+v", run)
	}
	for kind, resource := range fixture.resourceStates(t) {
		if resource.State != kernel.ResourceReleased || !resource.Identity.Empty() {
			t.Fatalf("recovered %s = %+v", kind, resource)
		}
	}
}

func TestReconciliationWaitsForBriefWriterContention(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x0f)
	blocker, err := sql.Open("sqlite3", "file:"+fixture.storePath+"?mode=rw")
	if err != nil {
		t.Fatal(err)
	}
	defer blocker.Close()
	connection, err := blocker.Conn(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if _, err := connection.ExecContext(context.Background(), "BEGIN IMMEDIATE"); err != nil {
		t.Fatal(err)
	}
	released := false
	defer func() {
		if !released {
			_, _ = connection.ExecContext(context.Background(), "ROLLBACK")
		}
	}()

	type result struct {
		run kernel.Run
		err error
	}
	cause := errors.New("writer contention")
	completed := make(chan result, 1)
	go func() {
		run, err := fixture.daemon.failRunBeforeRuntime(fixture.run, fixture.keys.Resources.RuntimeRoot, kernel.FailureInternal, cause)
		completed <- result{run: run, err: err}
	}()

	// The retired 250ms reconciliation window exhausted all three attempts
	// before this writer releases; the shared two-second store bound must wait
	// and preserve the admitted run's durable failure transition.
	time.Sleep(time.Second)
	if _, err := connection.ExecContext(context.Background(), "ROLLBACK"); err != nil {
		t.Fatal(err)
	}
	released = true
	select {
	case outcome := <-completed:
		if !errors.Is(outcome.err, cause) || outcome.run.Phase != kernel.RunFinalizing {
			t.Fatalf("reconciliation outcome = %+v, %v", outcome.run, outcome.err)
		}
	case <-time.After(time.Second):
		t.Fatal("reconciliation did not complete after the writer released")
	}
}

func TestReturnedRunRecoveryIgnoresStaleLiveAttemptRegistry(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x1a)
	stale := newLiveAttempt(fixture.daemon, fixture.run.ID, fixture.keys.TerminalSessionID, nil)
	fixture.daemon.attemptMu.Lock()
	fixture.daemon.attempts[fixture.run.ID] = stale
	fixture.daemon.attemptMu.Unlock()
	defer func() {
		fixture.daemon.attemptMu.Lock()
		delete(fixture.daemon.attempts, fixture.run.ID)
		fixture.daemon.attemptMu.Unlock()
	}()

	run, err := fixture.daemon.recoverReturnedRun(fixture.parent, fixture.changeParent, fixture.run.ID)
	if err != nil {
		t.Fatalf("returned-run recovery = %+v, %v", run, err)
	}
	if run.Phase != kernel.RunTerminal || run.Terminal == nil || run.Terminal.Code() != kernel.FailureSpawn {
		t.Fatalf("stale-registry recovery = %+v", run)
	}
}

func TestRecoverySweepSettlesFinalizingReleasedRuntimeAfterResultCleanup(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x18)
	fixture.stageRuntime(t)
	failure, err := kernel.NewFailureProposal(kernel.FailureSource, "source failed after runtime cleanup")
	if err != nil {
		t.Fatal(err)
	}
	failed, err := fixture.store.FailRun(context.Background(), fixture.run.ID, fixture.run.Revision, failure, mustKernelTime(t, 240))
	if err != nil || failed.Phase != kernel.RunFinalizing {
		t.Fatalf("finalizing source failure = %+v, %v", failed, err)
	}
	fixture.run = failed
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredConverged || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.Terminal == nil || run.Terminal.Code() != kernel.FailureSource {
		t.Fatalf("recovered source failure = %+v", run)
	}
	for kind, resource := range fixture.resourceStates(t) {
		if resource.State != kernel.ResourceReleased {
			t.Fatalf("recovered %s = %+v", kind, resource)
		}
	}
	if _, err := os.Stat(filepath.Join(fixture.parentPath, run.ID.String())); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("recovered runtime directory persists: %v", err)
	}
	session, found, err := fixture.store.TerminalSessionForRun(context.Background(), run.ID)
	if err != nil || !found || session.State != kernel.TerminalSessionClosed {
		t.Fatalf("recovered session = %+v found=%v err=%v", session, found, err)
	}
}

func TestRecoverySweepSettlesAfterRuntimeReleaseBeforeFinalization(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x19)
	runtimeIdentity := fixture.stageRuntime(t)
	failure, err := kernel.NewFailureProposal(kernel.FailureSource, "crash after runtime release")
	if err != nil {
		t.Fatal(err)
	}
	failed, err := fixture.store.FailRun(context.Background(), fixture.run.ID, fixture.run.Revision, failure, mustKernelTime(t, 240))
	if err != nil || failed.Phase != kernel.RunFinalizing {
		t.Fatalf("finalizing source failure = %+v, %v", failed, err)
	}
	fixture.run = failed
	fileIdentity, err := runtimeFileIdentity(runtimeIdentity)
	if err != nil {
		t.Fatal(err)
	}
	done, err := RemoveRecordedRuntime(context.Background(), fixture.parent, fixture.run.ID.String(), fileIdentity)
	if err != nil || !done {
		t.Fatalf("runtime removal = done=%v err=%v", done, err)
	}
	runtimeResource, found, err := fixture.store.Resource(context.Background(), fixture.keys.Resources.RuntimeRoot)
	if err != nil || !found {
		t.Fatalf("runtime resource = %+v found=%v err=%v", runtimeResource, found, err)
	}
	if _, err := fixture.store.ReleaseResource(context.Background(), fixture.run.ID, runtimeResource.ID, runtimeResource.Revision, runtimeResource.Identity, mustKernelTime(t, 250)); err != nil {
		t.Fatal(err)
	}
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredConverged || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.Terminal == nil || run.Terminal.Code() != kernel.FailureSource {
		t.Fatalf("recovered source failure = %+v", run)
	}
}

func TestRecoverySweepConvergesStartingRunnerWithoutResidue(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x20)
	fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredUnregistered || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.Proposal == nil || run.Proposal.Code() != kernel.FailureSpawn || run.Terminal == nil || run.Terminal.Code() != kernel.FailureSpawn {
		t.Fatalf("recovered run = %+v", run)
	}
	states := fixture.resourceStates(t)
	if states[kernel.ResourceRunnerProcess].State != kernel.ResourceReleased || states[kernel.ResourceRuntimeRoot].State != kernel.ResourceReleased {
		t.Fatalf("recovered resources = %+v", states)
	}
	if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String())); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("recovered runtime directory persists: %v", err)
	}
	session, found, err := fixture.store.TerminalSessionForRun(context.Background(), fixture.run.ID)
	if err != nil || !found || session.State != kernel.TerminalSessionClosed {
		t.Fatalf("recovered session = %+v found=%v err=%v", session, found, err)
	}
	// The settled terminal run is no longer recoverable; a second sweep
	// leaves it untouched with no disposition at all.
	again := fixture.sweep(t)
	if again.Action != RecoveredRunAction("") || again.Err != nil {
		t.Fatalf("second sweep = %+v", again)
	}
}

func TestRecoverySweepConvergesActivatedRunnerAbsenceBeforeExecRelease(t *testing.T) {
	for _, marker := range []bool{false, true} {
		name := "no outer marker"
		if marker {
			name = "outer marker present"
		}
		t.Run(name, func(t *testing.T) {
			fixture := newRecoveryFixture(t, 0x30)
			fixture.stageRuntime(t)
			fixture.beginRunnerStart(t)
			fixture.activateRunner(t)
			if marker {
				fixture.writeMarker(t, runner.OuterActivationMarkerName)
			}
			disposition := fixture.sweep(t)
			if disposition.Action != RecoveredPreSessionAbsence || disposition.Err != nil {
				t.Fatalf("disposition = %+v", disposition)
			}
			run := fixture.currentRun(t)
			if run.Phase != kernel.RunTerminal || run.Proposal == nil || run.Proposal.Code() != kernel.FailureActivation || run.Terminal == nil || run.RunnerExit == nil || !run.RunnerExit.RecoveredAbsence() {
				t.Fatalf("recovered run = %+v", run)
			}
			states := fixture.resourceStates(t)
			if states[kernel.ResourceRunnerProcess].State != kernel.ResourceReleased || states[kernel.ResourceProviderProcess].State != kernel.ResourceReleased {
				t.Fatalf("recovered resources = %+v", states)
			}
			if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String())); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("recovered runtime directory persists: %v", err)
			}
		})
	}
}

func TestRecoverySweepConsumesAuthenticResultBeforeAnyAbsenceEdge(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x40)
	fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	fixture.activateRunner(t)
	fixture.writeMarker(t, runner.OuterActivationMarkerName)
	body, err := json.Marshal(forgedResultWire{Version: 1, AttemptID: fixture.run.ID.String(), Kind: "inner_unregistered_converged", Proof: hex.EncodeToString(fixture.proof[:])})
	if err != nil {
		t.Fatal(err)
	}
	fixture.writeArtifact(t, body)
	// The durable state alone also matches the pre-session absence cell; the
	// ordering rule requires the artifact to win, or removal wedges forever.
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredResultConsumed || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.Proposal == nil || run.Proposal.Code() != kernel.FailureSpawn || run.Terminal == nil || run.RunnerExit == nil || !run.RunnerExit.RecoveredAbsence() {
		t.Fatalf("recovered run = %+v", run)
	}
	states := fixture.resourceStates(t)
	for _, kind := range []kernel.ResourceKind{kernel.ResourceRunnerProcess, kernel.ResourceProviderProcess, kernel.ResourceProviderGroup, kernel.ResourceRuntimeRoot} {
		if states[kind].State != kernel.ResourceReleased {
			t.Fatalf("recovered %s = %+v", kind, states[kind])
		}
	}
	session, found, err := fixture.store.TerminalSessionForRun(context.Background(), fixture.run.ID)
	if err != nil || !found || session.State != kernel.TerminalSessionClosed {
		t.Fatalf("recovered session = %+v found=%v err=%v", session, found, err)
	}
	if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String())); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("recovered runtime directory persists: %v", err)
	}
	// The settled terminal run is no longer recoverable; a second sweep
	// leaves it untouched with no disposition at all.
	again := fixture.sweep(t)
	if again.Action != RecoveredRunAction("") || again.Err != nil {
		t.Fatalf("second sweep = %+v", again)
	}
}

func TestRecoveryReplaysResultConsumedBeforeRunnerAbsence(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x41)
	runtimeIdentity := fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	fixture.activateRunner(t)
	fixture.writeMarker(t, runner.OuterActivationMarkerName)
	result, err := kernel.NewInnerUnregisteredConvergedAttemptResult(fixture.run.ID, fixture.run.CredentialDigest, fixture.run.ResultProofDigest(), runtimeIdentity)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := fixture.daemon.consumeAttemptResult(result, false); err != nil {
		t.Fatalf("initial result consume: %v", err)
	}
	// A crash before runner-absence convergence leaves the authenticated spool
	// and this exact consumed result. Recovery must recognize it without
	// accepting a different result or reapplying a write.
	body, err := json.Marshal(forgedResultWire{Version: 1, AttemptID: fixture.run.ID.String(), Kind: "inner_unregistered_converged", Proof: hex.EncodeToString(fixture.proof[:])})
	if err != nil {
		t.Fatal(err)
	}
	fixture.writeArtifact(t, body)
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredResultConsumed || disposition.Err != nil {
		t.Fatalf("replay recovery = %+v", disposition)
	}
	run := fixture.currentRun(t)
	if run.Phase != kernel.RunTerminal || run.RunnerExit == nil || !run.RunnerExit.RecoveredAbsence() {
		t.Fatalf("replayed recovery run = %+v", run)
	}
}

func TestRecoverySweepConvergesPartialReleasingRuntimeOnlyAfterResultConsumption(t *testing.T) {
	t.Run("consumed result", func(t *testing.T) {
		fixture := newRecoveryFixture(t, 0x48)
		fixture.stageRuntime(t)
		fixture.beginRunnerStart(t)
		fixture.activateRunner(t)
		fixture.writeMarker(t, runner.OuterActivationMarkerName)
		body, err := json.Marshal(forgedResultWire{Version: 1, AttemptID: fixture.run.ID.String(), Kind: "inner_unregistered_converged", Proof: hex.EncodeToString(fixture.proof[:])})
		if err != nil {
			t.Fatal(err)
		}
		fixture.writeArtifact(t, body)
		unsafe := filepath.Join(fixture.parentPath, fixture.run.ID.String(), runtimeTempName, "unsafe")
		if err := os.WriteFile(unsafe, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		if err := unix.Chmod(unsafe, 0o4600); err != nil {
			t.Fatal(err)
		}
		first := fixture.sweep(t)
		if first.Action != RecoveredResultConsumed || !errors.Is(first.Err, errInvalidContract) {
			t.Fatalf("first sweep = %+v", first)
		}
		if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runtimeHomeName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("home remains after partial cleanup: %v", err)
		}
		if states := fixture.resourceStates(t); states[kernel.ResourceRuntimeRoot].State != kernel.ResourceReleasing {
			t.Fatalf("runtime after refusal = %+v", states[kernel.ResourceRuntimeRoot])
		}
		if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runner.AttemptResultSpoolName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("consumed artifact remains: %v", err)
		}
		if err := unix.Chmod(unsafe, 0o600); err != nil {
			t.Fatal(err)
		}
		second := fixture.sweep(t)
		if second.Action != RecoveredConverged || second.Err != nil {
			t.Fatalf("second sweep = %+v", second)
		}
		if states := fixture.resourceStates(t); states[kernel.ResourceRuntimeRoot].State != kernel.ResourceReleased {
			t.Fatalf("runtime after recovery = %+v", states[kernel.ResourceRuntimeRoot])
		}
		if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String())); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("recovered runtime directory persists: %v", err)
		}
	})

	t.Run("unconsumed artifact", func(t *testing.T) {
		fixture := newRecoveryFixture(t, 0x49)
		fixture.stageRuntime(t)
		fixture.beginRunnerStart(t)
		fixture.activateRunner(t)
		fixture.writeMarker(t, runner.OuterActivationMarkerName)
		providerIdentity, err := processResourceIdentity(runner.Identity{PID: 99996, PGID: 99996, Birth: runner.Birth{Seconds: 1700, Microseconds: 3}})
		if err != nil {
			t.Fatal(err)
		}
		states := fixture.resourceStates(t)
		process, group := states[kernel.ResourceProviderProcess], states[kernel.ResourceProviderGroup]
		if _, _, err := fixture.store.ActivateProviderResources(context.Background(), fixture.run.ID, process.ID, process.Revision, group.ID, group.Revision, providerIdentity, mustKernelTime(t, 240)); err != nil {
			t.Fatal(err)
		}
		session, found, err := fixture.store.TerminalSessionForRun(context.Background(), fixture.run.ID)
		if err != nil || !found {
			t.Fatalf("session: found=%v err=%v", found, err)
		}
		if fixture.run, err = fixture.store.ActivateRun(context.Background(), fixture.run.ID, session.ID, fixture.currentRun(t).Revision, session.Revision, mustKernelTime(t, 250)); err != nil {
			t.Fatal(err)
		}
		failure, err := kernel.NewFailureProposal(kernel.FailureInternal, "recovery fixture refusal")
		if err != nil {
			t.Fatal(err)
		}
		if fixture.run, err = fixture.store.FailRun(context.Background(), fixture.run.ID, fixture.currentRun(t).Revision, failure, mustKernelTime(t, 260)); err != nil {
			t.Fatal(err)
		}
		body := []byte(fmt.Sprintf(`{"version":1,"attempt_id":%q,"kind":"inner_converged","proof":%q,"process":{"pid":99996,"pgid":99996,"birth":{"seconds":1700,"microseconds":3}},"exit":{"code":0}}`, fixture.run.ID.String(), hex.EncodeToString(fixture.proof[:])))
		fixture.writeArtifact(t, body)
		if err := os.Remove(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runtimeHomeName)); err != nil {
			t.Fatal(err)
		}
		disposition := fixture.sweep(t)
		if disposition.Action != RecoveredUncertain || disposition.Err == nil {
			t.Fatalf("sweep = %+v", disposition)
		}
		if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runner.AttemptResultSpoolName)); err != nil {
			t.Fatalf("unconsumed artifact was discarded: %v", err)
		}
		if states := fixture.resourceStates(t); states[kernel.ResourceRuntimeRoot].State != kernel.ResourceReleasing {
			t.Fatalf("unconsumed runtime = %+v", states[kernel.ResourceRuntimeRoot])
		}
	})
}

func TestRecoverySweepRetainsTornArtifactAndConcludesNothing(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x50)
	fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	fixture.activateRunner(t)
	fixture.writeMarker(t, runner.OuterActivationMarkerName)
	fixture.writeArtifact(t, []byte(`{"version":1,"attempt_id":"torn`))
	before := fixture.currentRun(t)
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredUncertain || disposition.Err == nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	after := fixture.currentRun(t)
	if after.Phase != before.Phase || after.Revision != before.Revision {
		t.Fatalf("torn artifact mutated the run: %+v -> %+v", before, after)
	}
	if _, err := os.Stat(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runner.AttemptResultSpoolName)); err != nil {
		t.Fatalf("torn artifact was not retained: %v", err)
	}
}

func TestRecoverySweepConcludesNothingWhileLifetimeLeaseIsHeld(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x60)
	fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	lease, err := os.OpenFile(filepath.Join(fixture.parentPath, fixture.run.ID.String(), runner.RuntimeLifetimeLeaseName), os.O_RDONLY, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer lease.Close()
	if err := unix.Flock(int(lease.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		t.Fatal(err)
	}
	before := fixture.currentRun(t)
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredLiveHolder || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	after := fixture.currentRun(t)
	if after.Phase != before.Phase || after.Revision != before.Revision {
		t.Fatalf("held lease mutated the run: %+v -> %+v", before, after)
	}
}

func TestRecoverySweepFailsClosedForActiveAttemptWithoutResult(t *testing.T) {
	fixture := newRecoveryFixture(t, 0x70)
	fixture.stageRuntime(t)
	fixture.beginRunnerStart(t)
	fixture.activateRunner(t)
	fixture.writeMarker(t, runner.OuterActivationMarkerName)
	fixture.writeMarker(t, runner.InnerActivationMarkerName)
	ctx := context.Background()
	providerIdentity, err := processResourceIdentity(runner.Identity{PID: 99997, PGID: 99997, Birth: runner.Birth{Seconds: 1700, Microseconds: 2}})
	if err != nil {
		t.Fatal(err)
	}
	states := fixture.resourceStates(t)
	process, group := states[kernel.ResourceProviderProcess], states[kernel.ResourceProviderGroup]
	if _, _, err := fixture.store.ActivateProviderResources(ctx, fixture.run.ID, process.ID, process.Revision, group.ID, group.Revision, providerIdentity, mustKernelTime(t, 240)); err != nil {
		t.Fatal(err)
	}
	session, found, err := fixture.store.TerminalSessionForRun(ctx, fixture.run.ID)
	if err != nil || !found {
		t.Fatalf("session: found=%v err=%v", found, err)
	}
	run, err := fixture.store.ActivateRun(ctx, fixture.run.ID, session.ID, fixture.currentRun(t).Revision, session.Revision, mustKernelTime(t, 250))
	if err != nil {
		t.Fatal(err)
	}
	fixture.run = run
	disposition := fixture.sweep(t)
	if disposition.Action != RecoveredNoResultUnresolved || disposition.Err != nil {
		t.Fatalf("disposition = %+v", disposition)
	}
	recovered := fixture.currentRun(t)
	if recovered.Phase != kernel.RunFinalizing || recovered.Proposal == nil || recovered.Proposal.Code() != kernel.FailureInternal || recovered.RunnerExit == nil || !recovered.RunnerExit.RecoveredAbsence() {
		t.Fatalf("recovered run = %+v", recovered)
	}
	states = fixture.resourceStates(t)
	if states[kernel.ResourceProviderProcess].State != kernel.ResourceUnresolved || states[kernel.ResourceProviderGroup].State != kernel.ResourceUnresolved {
		t.Fatalf("provider pair = %+v", states)
	}
	if states[kernel.ResourceRunnerProcess].State != kernel.ResourceReleased {
		t.Fatalf("runner = %+v", states[kernel.ResourceRunnerProcess])
	}
	// Deliberately not terminal: the run keeps its unresolved residue.
	if states[kernel.ResourceRuntimeRoot].State == kernel.ResourceReleased {
		t.Fatalf("runtime released without teardown proof: %+v", states[kernel.ResourceRuntimeRoot])
	}
	// A re-sweep fires no edge and must say so rather than re-reporting the
	// converging disposition.
	resweep := fixture.sweep(t)
	if resweep.Action != RecoveredConverged || resweep.Err != nil {
		t.Fatalf("re-sweep disposition = %+v", resweep)
	}
}
