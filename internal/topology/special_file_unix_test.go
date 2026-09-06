//go:build darwin || linux

package topology

import (
	"context"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"
)

func TestBuildDoesNotOpenSpecialGitOrCacheFiles(t *testing.T) {
	root := t.TempDir()
	writeFixture(t, root, map[string]string{"source/file.txt": "safe"})
	if err := os.Mkdir(filepath.Join(root, ".git"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := syscall.Mkfifo(filepath.Join(root, ".git", "HEAD"), 0o600); err != nil {
		t.Fatal(err)
	}
	assertCompletes(t, func() error {
		snapshot, err := Build(context.Background(), root, "", nil)
		if err == nil && snapshot.SourceRevision != "" {
			t.Errorf("source revision = %q, want empty", snapshot.SourceRevision)
		}
		return err
	})

	cache := filepath.Join(t.TempDir(), "topology", "project", "snapshot.json")
	if err := os.MkdirAll(filepath.Dir(cache), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := syscall.Mkfifo(cache, 0o600); err != nil {
		t.Fatal(err)
	}
	assertCompletes(t, func() error {
		_, err := BuildCached(context.Background(), root, "", cache)
		return err
	})
}

func assertCompletes(t *testing.T, operation func() error) {
	t.Helper()
	done := make(chan error, 1)
	go func() { done <- operation() }()
	select {
	case err := <-done:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(2 * time.Second):
		t.Fatal("topology read blocked on a special file")
	}
}
