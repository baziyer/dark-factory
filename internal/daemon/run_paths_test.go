//go:build darwin || linux

package daemon

import (
	"bytes"
	"context"
	"os"
	"path/filepath"
	"testing"
	"time"

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

func TestChangedDirectoriesReportsTheDeepestTouchedDirectory(t *testing.T) {
	root := t.TempDir()
	published := time.Now().Add(-time.Hour)
	for _, name := range []string{"README.md", "internal/kernel/store.go", "internal/kernel/untouched.go", ".hidden/secret.go", "node_modules/dep/index.js"} {
		path := filepath.Join(root, filepath.FromSlash(name))
		if err := os.MkdirAll(filepath.Dir(path), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte("published\n"), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := os.Chtimes(path, published, published); err != nil {
			t.Fatal(err)
		}
	}
	touched := filepath.Join(root, "internal", "kernel", "store.go")
	if err := os.WriteFile(touched, []byte("edited by the worker\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	paths, err := changedDirectories(root, published)
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 1 || paths[0] != filepath.Join("internal", "kernel") {
		t.Fatalf("touched file did not place the worker: %v", paths)
	}
	// A dot directory and node_modules stay out even when they are newer.
	for _, name := range []string{".hidden/secret.go", "node_modules/dep/index.js"} {
		if err := os.WriteFile(filepath.Join(root, filepath.FromSlash(name)), []byte("noise\n"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	paths, err = changedDirectories(root, published)
	if err != nil {
		t.Fatal(err)
	}
	if len(paths) != 1 || paths[0] != filepath.Join("internal", "kernel") {
		t.Fatalf("skipped directories reached the console: %v", paths)
	}
}
