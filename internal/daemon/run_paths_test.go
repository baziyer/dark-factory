//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"fmt"
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

func TestRunPathsForAgentWithoutRunIsEmpty(t *testing.T) {
	ctx := context.Background()
	initial, _ := kernel.NewUnixMillis(1)
	store, err := createTestStore(ctx, filepath.Join(t.TempDir(), "kernel.sqlite"), kernel.FactoryConfig{Capacity: 1}, initial)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = store.Close() })
	daemon, err := newDaemon(store, time.Now)
	if err != nil {
		t.Fatal(err)
	}
	projectID, _ := kernel.ProjectIDFromBytes(bytes.Repeat([]byte{0x61}, kernel.IDBytes))
	created, _ := kernel.NewUnixMillis(2)
	if _, err := store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "rooms", Root: t.TempDir()}, created); err != nil {
		t.Fatal(err)
	}
	agentID, _ := kernel.AgentIDFromBytes(bytes.Repeat([]byte{0x62}, kernel.IDBytes))
	if _, err := store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: projectID, Name: "idle", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, created); err != nil {
		t.Fatal(err)
	}
	runID, paths, err := daemon.RunPaths(ctx, agentID)
	if err != nil {
		t.Fatal(err)
	}
	if runID != (kernel.RunID{}) || len(paths) != 0 || paths == nil {
		t.Fatalf("idle agent placed in a room: %v %v", runID, paths)
	}
}

// The live-run answer end to end, over the same observe-only pairing a console
// uses. The published tree is materialized by hand: this exercises the daemon's
// projection of a run onto its change directory, not the publication itself.
func TestRunPathsAnswersForALiveRunAndCachesTheWalk(t *testing.T) {
	ctx := context.Background()
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	run := adapterRunningRoleRun(t, fixture.store, 0x70, kernel.RoleWorker)
	changeParent := t.TempDir()
	fixture.daemon.rememberSupervisorAccount(changeParent, "")
	published, found, err := fixture.store.Change(ctx, *run.ChangeID)
	if err != nil || !found {
		t.Fatalf("published change: found=%v err=%v", found, err)
	}

	// AvailableAt precedes RunningAt, so the run's own start is the cut: a
	// retained change keeps the earlier stamp and its edits are not this run's.
	if published.AvailableAt == nil || run.RunningAt == nil || published.AvailableAt.Int64() >= run.RunningAt.Int64() {
		t.Fatalf("fixture does not separate publication from the run start: %+v %+v", published.AvailableAt, run.RunningAt)
	}
	root := filepath.Join(changeParent, run.ChangeID.String())
	writeRunFile(t, root, "internal/kernel/store.go", time.UnixMilli(run.RunningAt.Int64()+10))
	writeRunFile(t, root, "web/ui/app.ts", time.UnixMilli(run.RunningAt.Int64()+10))
	// Written between publication and the run start: an earlier run's edit.
	writeRunFile(t, root, "docs/old.md", time.UnixMilli(published.AvailableAt.Int64()+1))

	request := browserprotocol.RunPathsGet{AgentID: run.AgentID.String()}
	answer, err := fixture.backend.RunPaths(ctx, rawBrowserClient(fixture.client.ID), request)
	if err != nil {
		t.Fatal(err)
	}
	want := []string{filepath.Join("internal", "kernel"), filepath.Join("web", "ui")}
	if answer.RunID != run.ID.String() || answer.AgentID != request.AgentID || fmt.Sprint(answer.Paths) != fmt.Sprint(want) {
		t.Fatalf("live run answered %+v, want run %s and %v", answer, run.ID.String(), want)
	}

	// The cache is the only reason a tree that moved answers the same way.
	writeRunFile(t, root, "cmd/factoryd/main.go", time.UnixMilli(run.RunningAt.Int64()+20))
	cached, err := fixture.backend.RunPaths(ctx, rawBrowserClient(fixture.client.ID), request)
	if err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(cached.Paths) != fmt.Sprint(want) {
		t.Fatalf("second call re-walked within the TTL: %v", cached.Paths)
	}

	// An agent with no live run is in no room, even while another run is live.
	idle, _ := kernel.AgentIDFromBytes(adapterID(t, 0x9a))
	if _, err := fixture.store.CreateAgent(ctx, kernel.NewAgent{ID: idle, ProjectID: run.ProjectID, Name: "idle-neighbour", Role: kernel.RoleOrchestrator, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, adapterTime(t, 340)); err != nil {
		t.Fatal(err)
	}
	empty, err := fixture.backend.RunPaths(ctx, rawBrowserClient(fixture.client.ID), browserprotocol.RunPathsGet{AgentID: idle.String()})
	if err != nil {
		t.Fatal(err)
	}
	if empty.RunID != "" || len(empty.Paths) != 0 {
		t.Fatalf("idle agent answered %+v", empty)
	}
}

// Without a recorded changes root the daemon cannot locate any run's tree, so
// it answers empty rather than guessing at the operational home layout.
func TestRunPathsWithoutARecordedChangeParentIsEmpty(t *testing.T) {
	ctx := context.Background()
	fixture := newAdapterFixture(t, kernel.BrowserCapabilityObserve)
	fixture.pair(t)
	run := adapterRunningRoleRun(t, fixture.store, 0x80, kernel.RoleWorker)
	answer, err := fixture.backend.RunPaths(ctx, rawBrowserClient(fixture.client.ID), browserprotocol.RunPathsGet{AgentID: run.AgentID.String()})
	if err != nil {
		t.Fatal(err)
	}
	if answer.RunID != run.ID.String() || len(answer.Paths) != 0 {
		t.Fatalf("unlocated tree answered %+v", answer)
	}
}

func TestChangedDirectoriesDedupesSortsSkipsAndStaysBounded(t *testing.T) {
	ctx := context.Background()
	root := t.TempDir()
	published := time.Now().Add(-time.Hour)
	// Two files in one directory dedupe to one room; the answer is sorted, not
	// walk-ordered. The skipped trees are newer than everything else.
	for _, name := range []string{"web/ui/app.ts", "web/ui/view.ts", "internal/kernel/store.go", ".hidden/secret.go", "node_modules/dep/index.js", "vendor/pkg/lib.go", "target/debug/build.rs"} {
		writeRunFile(t, root, name, time.Now())
	}
	writeRunFile(t, root, "README.md", published.Add(-time.Minute))

	paths := changedDirectories(ctx, root, published)
	want := []string{filepath.Join("internal", "kernel"), filepath.Join("web", "ui")}
	if fmt.Sprint(paths) != fmt.Sprint(want) {
		t.Fatalf("walk answered %v, want %v", paths, want)
	}

	// A tree the console cannot draw is truncated, not refused. Both bounds
	// are lowered rather than built, so each one is proven on its own.
	for index := 0; index < 6; index++ {
		writeRunFile(t, root, fmt.Sprintf("wide/room%02d/file.go", index), time.Now())
	}
	restoreMax, restoreLimit := maxRunPaths, runPathsWalkLimit
	t.Cleanup(func() { maxRunPaths, runPathsWalkLimit = restoreMax, restoreLimit })
	maxRunPaths = 3
	if capped := changedDirectories(ctx, root, published); len(capped) != 3 {
		t.Fatalf("result cap answered %d rooms: %v", len(capped), capped)
	}
	maxRunPaths = restoreMax
	runPathsWalkLimit = 2
	if budgeted := changedDirectories(ctx, root, published); len(budgeted) != 0 {
		t.Fatalf("entry budget answered %v", budgeted)
	}
	runPathsWalkLimit = restoreLimit

	// A cancelled request stops the walk instead of finishing it.
	cancelled, cancel := context.WithCancel(ctx)
	cancel()
	if stopped := changedDirectories(cancelled, root, published); len(stopped) != 0 {
		t.Fatalf("cancelled walk answered %v", stopped)
	}
	// A tree that is not there is an empty answer, not a failure.
	if missing := changedDirectories(ctx, filepath.Join(root, "absent"), published); len(missing) != 0 {
		t.Fatalf("missing tree answered %v", missing)
	}
}

func writeRunFile(t *testing.T, root, name string, at time.Time) {
	t.Helper()
	path := filepath.Join(root, filepath.FromSlash(name))
	if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(path, []byte(name+"\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(path, at, at); err != nil {
		t.Fatal(err)
	}
}

// adapterPublishChange takes the run's reserved change through to available,
// which is the only durable record of when its tree was published.
func adapterPublishChange(t *testing.T, store *kernel.Store, run kernel.Run) kernel.Change {
	t.Helper()
	ctx := context.Background()
	if run.ChangeID == nil {
		t.Fatal("run has no candidate change")
	}
	state, found, err := store.Change(ctx, *run.ChangeID)
	if err != nil || !found {
		t.Fatalf("change: found=%v err=%v", found, err)
	}
	format := kernel.ObjectSHA1
	commit, err := kernel.NewCommitID(format, bytes.Repeat([]byte{0x81}, 20))
	if err != nil {
		t.Fatal(err)
	}
	digest, err := kernel.TreeDigestFromBytes(bytes.Repeat([]byte{0x82}, kernel.DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	repository, err := kernel.NewFileIdentity(7, 8)
	if err != nil {
		t.Fatal(err)
	}
	selection, err := kernel.NewChangeSelection(format, commit, digest, 1, 64, repository)
	if err != nil {
		t.Fatal(err)
	}
	tree, err := kernel.NewFileIdentity(9, 10)
	if err != nil {
		t.Fatal(err)
	}
	prepared, err := store.RecordChangePrepared(ctx, state.ID, state.Revision, selection, tree, adapterTime(t, 322))
	if err != nil {
		t.Fatal(err)
	}
	availability, err := kernel.NewChangeAvailability(digest, 1, 64, tree)
	if err != nil {
		t.Fatal(err)
	}
	available, err := store.MarkChangeAvailable(ctx, state.ID, prepared.Revision, availability, adapterTime(t, 324))
	if err != nil {
		t.Fatal(err)
	}
	return available
}
