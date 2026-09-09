//go:build darwin

package daemon

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"errors"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

// forgedResultWire mirrors the runner's canonical result encoding exactly so
// the test can place raw artifacts without any runner publishing authority.
type forgedResultWire struct {
	Version   int    `json:"version"`
	AttemptID string `json:"attempt_id"`
	Kind      string `json:"kind"`
	Proof     string `json:"proof"`
}

// TestForgedProofArtifactIsRefusedByComposedConsumption is the O1 invariant:
// runner authentication deliberately does not verify the proof, so a forged
// artifact authenticates and merely reports a different proof digest. The
// composed daemon flow must still refuse it, because the only kernel-result
// constructor binds the record's own digest and the Store compares it against
// the digest committed at admission.
func TestForgedProofArtifactIsRefusedByComposedConsumption(t *testing.T) {
	ctx := context.Background()
	base, err := os.MkdirTemp("/private/tmp", "dark-factory-proof-binding-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(base) })
	base, err = filepath.EvalSymlinks(base)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(base, 0o700); err != nil {
		t.Fatal(err)
	}
	store, err := createTestStore(ctx, filepath.Join(base, "kernel.sqlite"), kernel.FactoryConfig{Capacity: 1}, mustKernelTime(t, 100))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })

	genuine := bytes.Repeat([]byte{0x51}, 32)
	genuineDigest := sha256.Sum256(genuine)
	storedDigest, err := kernel.ResultProofDigestFromBytes(genuineDigest[:])
	if err != nil {
		t.Fatal(err)
	}
	run, runtimeIdentity, runtimeDir := admittedRunWithActiveRunner(t, store, base, storedDigest)
	defer runtimeDir.Close()

	place := func(t *testing.T, proof []byte) *runner.AttemptResultRecord {
		t.Helper()
		body, err := json.Marshal(forgedResultWire{Version: 1, AttemptID: run.ID.String(), Kind: "inner_unregistered_converged", Proof: hex.EncodeToString(proof)})
		if err != nil {
			t.Fatal(err)
		}
		spool := filepath.Join(base, "runtime", runner.AttemptResultSpoolName)
		if err := os.WriteFile(spool, body, 0o600); err != nil {
			t.Fatal(err)
		}
		t.Cleanup(func() { _ = os.Remove(spool) })
		record, err := runner.AuthenticateAttemptResult(runtimeDir, run.ID.String(), nil)
		if err != nil {
			t.Fatalf("artifact did not authenticate at the runner layer: %v", err)
		}
		return record
	}

	forged := place(t, bytes.Repeat([]byte{0x52}, 32))
	result, err := kernelAttemptResult(forged, run.ID, run.CredentialDigest, runtimeIdentity)
	if err != nil {
		t.Fatalf("forged record construction = %v", err)
	}
	daemon, err := newDaemon(store, func() time.Time { return time.UnixMilli(5000) })
	if err != nil {
		t.Fatal(err)
	}
	if _, err := daemon.consumeAttemptResult(result, false); err == nil {
		t.Fatal("forged-proof artifact was consumed")
	} else if !errors.Is(err, kernel.ErrConflict) {
		t.Fatalf("forged-proof refusal = %v, want conflict", err)
	}
	after, found, err := store.Run(ctx, run.ID)
	if err != nil || !found || after.Phase != kernel.RunAdmitted || after.Revision != run.Revision {
		t.Fatalf("forged consumption mutated the run: %+v found=%v err=%v", after, found, err)
	}
	if err := os.Remove(filepath.Join(base, "runtime", runner.AttemptResultSpoolName)); err != nil {
		t.Fatal(err)
	}

	genuineRecord := place(t, genuine)
	result, err = kernelAttemptResult(genuineRecord, run.ID, run.CredentialDigest, runtimeIdentity)
	if err != nil {
		t.Fatal(err)
	}
	consumed, err := daemon.consumeAttemptResult(result, false)
	if err != nil || consumed.Phase != kernel.RunFinalizing {
		t.Fatalf("genuine consumption = %+v err=%v", consumed, err)
	}
}

// admittedRunWithActiveRunner builds the exact admitted footprint an
// unregistered result requires: active runtime bound to the on-disk runtime
// directory, active runner, declared provider pair, declared session.
func admittedRunWithActiveRunner(t *testing.T, store *kernel.Store, base string, proofDigest kernel.ResultProofDigest) (kernel.Run, kernel.ResourceIdentity, *os.File) {
	t.Helper()
	ctx := context.Background()
	at := mustKernelTime(t, 200)
	projectID := mustProjectID(t, testID(0xa1))
	if _, err := store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "proof-project", Root: filepath.Join(base, "root")}, at); err != nil {
		t.Fatal(err)
	}
	agentID := mustAgentID(t, testID(0xa2))
	if _, err := store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: projectID, Name: "proof-agent", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, at); err != nil {
		t.Fatal(err)
	}
	taskID := mustTaskID(t, testID(0xa3))
	incarnation, err := kernel.IncarnationIDFromBytes(bytes.Repeat([]byte{0xa4}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnation, Title: "proof-task"}, at); err != nil {
		t.Fatal(err)
	}
	factory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetDispatch(ctx, factory.Revision, true, at); err != nil {
		t.Fatal(err)
	}
	runID := mustRunID(t, testID(0xa5))
	sessionID := mustTerminalSessionID(t, testID(0xa6))
	attemptDigest, err := kernel.AttemptDigestFromBytes(bytes.Repeat([]byte{0xa7}, kernel.DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	changeBytes, err := kernel.ChangeIDFromBytes(bytes.Repeat([]byte{0xa8}, kernel.IDBytes))
	if err != nil {
		t.Fatal(err)
	}
	runtimePath := filepath.Join(base, "runtime")
	if err := os.Mkdir(runtimePath, 0o700); err != nil {
		t.Fatal(err)
	}
	keys := kernel.AdmissionKeys{
		RunID: runID, TerminalSessionID: sessionID, AttemptDigest: attemptDigest, ResultProofDigest: proofDigest,
		CandidateChangeID: changeBytes, RuntimeRoot: runtimePath,
		Resources: kernel.AdmissionResourceIDs{
			RuntimeRoot: mustResourceID(t, testID(0xa9)), RunnerProcess: mustResourceID(t, testID(0xaa)),
			ProviderProcess: mustResourceID(t, testID(0xab)), ProviderGroup: mustResourceID(t, testID(0xac)),
		},
	}
	admission, err := store.AdmitNext(ctx, keys, at)
	if err != nil || !admission.Admitted() {
		t.Fatalf("admission = %+v, %v", admission, err)
	}
	// The outer activation marker is the census the artifact reader demands.
	marker := filepath.Join(runtimePath, runner.OuterActivationMarkerName)
	if err := os.WriteFile(marker, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	var stat syscall.Stat_t
	if err := syscall.Stat(runtimePath, &stat); err != nil {
		t.Fatal(err)
	}
	runtimeIdentity, err := kernel.NewPathResourceIdentity(int64(stat.Dev), int64(stat.Ino))
	if err != nil {
		t.Fatal(err)
	}
	runtimeResource, found, err := store.Resource(ctx, keys.Resources.RuntimeRoot)
	if err != nil || !found {
		t.Fatalf("runtime resource: found=%v err=%v", found, err)
	}
	if _, err := store.ActivateResource(ctx, runID, runtimeResource.ID, runtimeResource.Revision, runtimeIdentity, at); err != nil {
		t.Fatal(err)
	}
	runnerBirth, err := kernel.BirthDigestFromBytes(bytes.Repeat([]byte{0xad}, 32))
	if err != nil {
		t.Fatal(err)
	}
	runnerIdentity, err := kernel.NewProcessResourceIdentity(4321, 4321, runnerBirth)
	if err != nil {
		t.Fatal(err)
	}
	run := startAndActivateRunner(t, store, runID, keys.Resources.RunnerProcess, runnerIdentity, at)
	dir, err := os.Open(runtimePath)
	if err != nil {
		t.Fatal(err)
	}
	return run, runtimeIdentity, dir
}
