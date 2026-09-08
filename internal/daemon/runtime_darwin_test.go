//go:build darwin

package daemon

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"syscall"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

const (
	runtimeTestName = "0123456789abcdef0123456789abcdef"
	runtimeGoneName = "fedcba9876543210fedcba9876543210"
)

func runtimeTempDir(t testing.TB) string {
	t.Helper()
	path, err := os.MkdirTemp("/private/tmp", "dark-factory-runtime-")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(path, 0o700); err != nil {
		_ = os.RemoveAll(path)
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(path) })
	return path
}

func TestRuntimeCreatePublishClosePreservesExactPrivateEffects(t *testing.T) {
	before := openFDCensus(t)
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	runtime, err := CreateRuntime(parent, runtimeTestName)
	if err != nil {
		t.Fatal(err)
	}
	runtimePath := mustRuntimePath(t, runtime)
	identity := mustRuntimeIdentity(t, runtime)
	if runtimePath != filepath.Join(parentPath, runtimeTestName) || identity.Device == 0 || identity.Inode == 0 {
		t.Fatalf("runtime = %q %+v", runtimePath, identity)
	}
	assertDirectory(t, runtimePath, identity.Device, identity.Inode, 0o700)
	assertDirectory(t, filepath.Join(runtimePath, runtimeHomeName), 0, 0, 0o700)
	assertDirectory(t, filepath.Join(runtimePath, runtimeTempName), 0, 0, 0o700)
	leaseInfo, err := os.Lstat(filepath.Join(runtimePath, runner.RuntimeLifetimeLeaseName))
	if err != nil || !leaseInfo.Mode().IsRegular() || leaseInfo.Mode().Perm() != 0o600 || leaseInfo.Size() != 0 {
		t.Fatalf("runtime lifetime = %v, %v", leaseInfo, err)
	}

	duplicate, lifetime, err := runtime.DuplicateRunnerFiles()
	if err != nil {
		t.Fatal(err)
	}
	if duplicate.Fd() == runtime.dir.Fd() {
		t.Fatal("directory descriptor was not duplicated")
	}
	if err := unix.Fchdir(int(lifetime.Fd())); !errors.Is(err, unix.ENOTDIR) && !errors.Is(err, unix.EBADF) {
		t.Fatalf("lifetime duplicate grants directory authority: %v", err)
	}
	if _, err := lifetime.Write([]byte{1}); !errors.Is(err, os.ErrPermission) && !errors.Is(err, unix.EBADF) {
		t.Fatalf("lifetime duplicate grants write authority: %v", err)
	}
	if err := duplicate.Close(); err != nil {
		t.Fatal(err)
	}
	if err := lifetime.Close(); err != nil {
		t.Fatal(err)
	}

	token := [32]byte{1, 2, 3, 4}
	tokenFile, err := runtime.PublishAttemptToken(context.Background(), token)
	if err != nil {
		t.Fatal(err)
	}
	assertPrivateFile(t, tokenFile, token[:])
	if _, err := runtime.PublishAttemptToken(context.Background(), token); !errors.Is(err, errInvalidContract) {
		t.Fatalf("duplicate token error = %v", err)
	}
	if err := runtime.Close(); err != nil {
		t.Fatal(err)
	}
	if err := runtime.Close(); err != nil {
		t.Fatalf("second close = %v", err)
	}
	if _, err := runtime.Binding(); !errors.Is(err, errInvalidContract) {
		t.Fatalf("closed runtime binding error = %v", err)
	}
	if _, err := os.Stat(runtimePath); err != nil {
		t.Fatalf("Close removed runtime: %v", err)
	}
	if err := parent.Close(); err != nil {
		t.Fatal(err)
	}
	assertFDCensus(t, before)
}

func TestRuntimeRejectsExistingAndUnsafeAuthoritiesWithoutMutation(t *testing.T) {
	tests := map[string]func(string) error{
		"directory": func(path string) error { return os.Mkdir(path, 0o700) },
		"file":      func(path string) error { return os.WriteFile(path, []byte("sentinel"), 0o600) },
		"symlink":   func(path string) error { return os.Symlink("missing", path) },
		"fifo":      func(path string) error { return unix.Mkfifo(path, 0o600) },
	}
	for name, create := range tests {
		t.Run(name, func(t *testing.T) {
			parentPath := filepath.Join(runtimeTempDir(t), "private")
			if err := os.Mkdir(parentPath, 0o700); err != nil {
				t.Fatal(err)
			}
			target := filepath.Join(parentPath, runtimeTestName)
			if err := create(target); err != nil {
				t.Fatal(err)
			}
			before, err := os.Lstat(target)
			if err != nil {
				t.Fatal(err)
			}
			parent := createManagedParent(t, parentPath)
			defer parent.Close()
			if _, err := CreateRuntime(parent, runtimeTestName); !errors.Is(err, errInvalidContract) {
				t.Fatalf("CreateRuntime error = %v", err)
			}
			after, err := os.Lstat(target)
			if err != nil || !os.SameFile(before, after) {
				t.Fatalf("existing authority changed: %v", err)
			}
			if name == "file" {
				contents, _ := os.ReadFile(target)
				if string(contents) != "sentinel" {
					t.Fatalf("existing file replaced: %q", contents)
				}
			}
		})
	}

	parentPath := filepath.Join(runtimeTempDir(t), "shared")
	if err := os.Mkdir(parentPath, 0o755); err != nil {
		t.Fatal(err)
	}
	if _, err := OpenRuntimeParent(context.Background(), install.MemberCapability{}, parentPath); !errors.Is(err, errInvalidContract) {
		t.Fatalf("shared parent capability error = %v", err)
	}
	parent := (*RuntimeParent)(nil)
	defer parent.Close()
	if _, err := CreateRuntime(parent, runtimeTestName); !errors.Is(err, errInvalidContract) {
		t.Fatalf("shared parent error = %v", err)
	}
	if _, err := os.Lstat(filepath.Join(parentPath, runtimeTestName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("shared parent gained effect: %v", err)
	}
}

func TestRuntimeParentMissingLockRejectsPopulatedParentUnchanged(t *testing.T) {
	home, _, capability, parentPath := newOperationalRuntimeCapability(t)
	defer home.Close()
	sentinel := filepath.Join(parentPath, install.LocalAPISocketName)
	contents := []byte("foreign")
	if err := os.WriteFile(sentinel, contents, 0o600); err != nil {
		t.Fatal(err)
	}
	if parent, err := OpenRuntimeParent(context.Background(), capability, parentPath); !errors.Is(err, errInvalidContract) || parent != nil {
		t.Fatalf("populated parent open = %v, %v", parent, err)
	}
	if got, err := os.ReadFile(sentinel); err != nil || !bytes.Equal(got, contents) {
		t.Fatalf("sentinel changed = %q, %v", got, err)
	}
	if _, err := os.Lstat(filepath.Join(parentPath, runtimeParentLockName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("missing lock was created: %v", err)
	}
}

func TestRuntimeParentRetainsOneDurableLock(t *testing.T) {
	home, _, capability, parentPath := newOperationalRuntimeCapability(t)
	defer home.Close()
	created, err := OpenRuntimeParent(context.Background(), capability, parentPath)
	if err != nil {
		t.Fatal(err)
	}
	defer created.Close()
	wantLock, err := os.Lstat(filepath.Join(parentPath, runtimeParentLockName))
	if err != nil {
		t.Fatal(err)
	}
	if second, err := OpenRuntimeParent(context.Background(), capability, parentPath); !errors.Is(err, errRuntimeBusy) || second != nil {
		if second != nil {
			_ = second.Close()
		}
		t.Fatalf("second lifetime owner = %v, %v", second, err)
	}
	if err := created.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := OpenRuntimeParent(context.Background(), capability, parentPath)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	gotLock, err := os.Lstat(filepath.Join(parentPath, runtimeParentLockName))
	if err != nil || !os.SameFile(wantLock, gotLock) {
		t.Fatalf("lock changed = %v, %v", gotLock, err)
	}
	runtime, err := CreateRuntime(reopened, runtimeTestName)
	if err != nil {
		t.Fatal(err)
	}
	if err := runtime.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestRuntimeParentLosingCreatorNeverUnlinksWinningLock(t *testing.T) {
	home, _, capability, parentPath := newOperationalRuntimeCapability(t)
	defer home.Close()
	created := make(chan struct{})
	releaseCreator := make(chan struct{})
	type openResult struct {
		parent *RuntimeParent
		err    error
	}
	creatorResult := make(chan openResult, 1)
	go func() {
		parent, err := openRuntimeParent(context.Background(), capability, parentPath, func(createdLock bool) {
			if createdLock {
				close(created)
				<-releaseCreator
			}
		})
		creatorResult <- openResult{parent: parent, err: err}
	}()
	select {
	case <-created:
	case <-time.After(time.Second):
		t.Fatal("creator did not reach flock election")
	}
	winner, err := OpenRuntimeParent(context.Background(), capability, parentPath)
	if err != nil {
		t.Fatalf("racing opener did not win created lock: %v", err)
	}
	defer winner.Close()
	close(releaseCreator)
	result := <-creatorResult
	if !errors.Is(result.err, errRuntimeBusy) || result.parent != nil {
		if result.parent != nil {
			_ = result.parent.Close()
		}
		t.Fatalf("losing creator = %v, %v", result.parent, result.err)
	}
	lockBefore, err := os.Lstat(filepath.Join(parentPath, runtimeParentLockName))
	if err != nil {
		t.Fatalf("losing creator unlinked winner's lock: %v", err)
	}
	if third, err := OpenRuntimeParent(context.Background(), capability, parentPath); !errors.Is(err, errRuntimeBusy) || third != nil {
		if third != nil {
			_ = third.Close()
		}
		t.Fatalf("third owner split lifetime lock = %v, %v", third, err)
	}
	if err := winner.Close(); err != nil {
		t.Fatal(err)
	}
	reopened, err := OpenRuntimeParent(context.Background(), capability, parentPath)
	if err != nil {
		t.Fatal(err)
	}
	defer reopened.Close()
	lockAfter, err := os.Lstat(filepath.Join(parentPath, runtimeParentLockName))
	if err != nil || !os.SameFile(lockBefore, lockAfter) {
		t.Fatalf("lock changed after losing creator: %v, %v", lockAfter, err)
	}
}

func TestRuntimeParentCloseWaitsForBegunOperation(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	operation, err := parent.begin()
	if err != nil {
		t.Fatal(err)
	}
	closed := make(chan error, 1)
	go func() { closed <- parent.Close() }()
	waitForRuntimeParentClosing(t, parent)
	select {
	case err := <-closed:
		t.Fatalf("Close raced begun operation: %v", err)
	default:
	}
	if _, err := operation.directory(); err != nil {
		t.Fatalf("begun operation lost authority: %v", err)
	}
	if err := operation.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("Close did not join begun operation")
	}
}

func TestRuntimeParentSerializesNamespaceOperationsAndRevokesClosedHandles(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	defer parent.Close()

	first, err := parent.begin()
	if err != nil {
		t.Fatal(err)
	}
	defer first.Close()
	type beginResult struct {
		operation *runtimeParentOperation
		err       error
	}
	secondDone := make(chan beginResult, 1)
	go func() {
		operation, err := parent.begin()
		secondDone <- beginResult{operation: operation, err: err}
	}()
	select {
	case second := <-secondDone:
		if second.operation != nil {
			_ = second.operation.Close()
		}
		t.Fatalf("concurrent namespace operation returned before release: %v, %v", second.operation, second.err)
	case <-time.After(50 * time.Millisecond):
	}
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := first.directory(); !errors.Is(err, errInvalidContract) {
		t.Fatalf("closed operation directory = %v", err)
	}
	if _, err := first.locator(runtimeTestName); !errors.Is(err, errInvalidContract) {
		t.Fatalf("closed operation locator = %v", err)
	}
	if err := first.Close(); err != nil {
		t.Fatalf("duplicate operation close = %v", err)
	}
	secondResult := <-secondDone
	if secondResult.err != nil || secondResult.operation == nil {
		t.Fatalf("operation gate did not reopen: %v, %v", secondResult.operation, secondResult.err)
	}
	second := secondResult.operation
	child, err := second.transfer()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := second.directory(); !errors.Is(err, errInvalidContract) {
		t.Fatalf("transferred operation directory = %v", err)
	}
	if err := second.Close(); err != nil {
		t.Fatalf("transferred operation close = %v", err)
	}
	if _, err := child.directory(); err != nil {
		t.Fatalf("transferred child lost authority: %v", err)
	}
	if err := child.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := child.directory(); !errors.Is(err, errInvalidContract) {
		t.Fatalf("closed child directory = %v", err)
	}
}

func TestCreateRuntimeWaitsForParentNamespaceOperation(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	defer parent.Close()
	held, err := parent.begin()
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	type createResult struct {
		runtime *Runtime
		err     error
	}
	done := make(chan createResult, 1)
	go func() {
		runtime, err := CreateRuntime(parent, runtimeTestName)
		done <- createResult{runtime: runtime, err: err}
	}()
	select {
	case result := <-done:
		if result.runtime != nil {
			_ = result.runtime.Close()
		}
		t.Fatalf("CreateRuntime returned before namespace release: %v, %v", result.runtime, result.err)
	case <-time.After(50 * time.Millisecond):
	}
	if err := held.Close(); err != nil {
		t.Fatal(err)
	}
	result := <-done
	if result.err != nil || result.runtime == nil {
		t.Fatalf("CreateRuntime after namespace release = %v, %v", result.runtime, result.err)
	}
	if err := result.runtime.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestRuntimeParentCloseRejectsQueuedNamespaceOperation(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	defer parent.Close()
	held, err := parent.begin()
	if err != nil {
		t.Fatal(err)
	}
	defer held.Close()
	beginDone := make(chan error, 1)
	go func() {
		operation, err := parent.begin()
		if operation != nil {
			_ = operation.Close()
		}
		beginDone <- err
	}()
	select {
	case err := <-beginDone:
		t.Fatalf("queued begin returned before close: %v", err)
	case <-time.After(50 * time.Millisecond):
	}
	closed := make(chan error, 1)
	go func() { closed <- parent.Close() }()
	deadline := time.After(time.Second)
	for {
		parent.mu.Lock()
		closing := parent.closing
		parent.mu.Unlock()
		if closing {
			break
		}
		select {
		case <-deadline:
			t.Fatal("Close did not claim the parent")
		case <-time.After(time.Millisecond):
		}
	}
	if err := held.Close(); err != nil {
		t.Fatal(err)
	}
	if err := <-closed; err != nil {
		t.Fatal(err)
	}
	if err := <-beginDone; !errors.Is(err, errInvalidContract) {
		t.Fatalf("queued begin after Close = %v", err)
	}
}

func TestOpenRuntimeParentRejectsUnboundDiagnosticBeforeLockCreation(t *testing.T) {
	home, _, capability, parentPath := newOperationalRuntimeCapability(t)
	defer home.Close()
	wrongPath := filepath.Join(runtimeTempDir(t), "wrong")
	if err := os.Mkdir(wrongPath, 0o700); err != nil {
		t.Fatal(err)
	}
	if parent, err := OpenRuntimeParent(context.Background(), capability, wrongPath); !errors.Is(err, errInvalidContract) || parent != nil {
		t.Fatalf("unbound diagnostic open = %v, %v", parent, err)
	}
	for _, path := range []string{parentPath, wrongPath} {
		entries, err := os.ReadDir(path)
		if err != nil {
			t.Fatal(err)
		}
		if len(entries) != 0 {
			t.Fatalf("unbound diagnostic created effect in %s: %v", path, entries)
		}
	}
}

func TestRuntimeChildOwnsParentUntilChildClose(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	runtime, err := CreateRuntime(parent, runtimeTestName)
	if err != nil {
		t.Fatal(err)
	}
	closed := make(chan error, 1)
	go func() { closed <- parent.Close() }()
	waitForRuntimeParentClosing(t, parent)
	if _, err := runtime.PublishAttemptToken(context.Background(), [32]byte{1}); err != nil {
		t.Fatalf("child lost retained parent during Close: %v", err)
	}
	select {
	case err := <-closed:
		t.Fatalf("parent closed before child: %v", err)
	default:
	}
	if err := runtime.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("parent Close did not join child")
	}
}

func TestRuntimeParentCloseJoinsEveryLiveChild(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	first, err := CreateRuntime(parent, "11111111111111111111111111111111")
	if err != nil {
		t.Fatal(err)
	}
	second, err := CreateRuntime(parent, "22222222222222222222222222222222")
	if err != nil {
		t.Fatal(err)
	}
	closed := make(chan error, 1)
	go func() { closed <- parent.Close() }()
	waitForRuntimeParentClosing(t, parent)
	if err := first.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-closed:
		t.Fatalf("parent closed with second child live: %v", err)
	default:
	}
	if err := second.Close(); err != nil {
		t.Fatal(err)
	}
	select {
	case err := <-closed:
		if err != nil {
			t.Fatal(err)
		}
	case <-time.After(time.Second):
		t.Fatal("parent Close did not join every child")
	}
}

func TestRuntimeParentCloseRetainsAuthorityOnReleaseUncertainty(t *testing.T) {
	t.Run("directory close", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		dir, lock := parent.dir, parent.lock
		unlockCalls, lockCloseCalls := 0, 0
		parent.closeDir = func(*os.File) error { return syscall.EIO }
		parent.unlock = func(int) error { unlockCalls++; return nil }
		parent.closeLock = func(*os.File) error { lockCloseCalls++; return nil }
		err := parent.Close()
		if !errors.Is(err, errRetainedRuntime) || !errors.Is(err, syscall.EIO) {
			t.Fatalf("directory close uncertainty = %v", err)
		}
		if unlockCalls != 0 || lockCloseCalls != 0 || parent.dir != dir || parent.lock != lock {
			t.Fatalf("release continued after directory uncertainty: unlock=%d lock-close=%d", unlockCalls, lockCloseCalls)
		}
		if again := parent.Close(); !errors.Is(again, syscall.EIO) || unlockCalls != 0 || lockCloseCalls != 0 {
			t.Fatalf("stable close result = %v, unlock=%d lock-close=%d", again, unlockCalls, lockCloseCalls)
		}
		if err := unix.Flock(int(lock.Fd()), unix.LOCK_UN); err != nil {
			t.Fatal(err)
		}
		if err := lock.Close(); err != nil {
			t.Fatal(err)
		}
		if err := dir.Close(); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("lock release", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		lock := parent.lock
		lockCloseCalls := 0
		parent.unlock = func(int) error { return syscall.EIO }
		parent.closeLock = func(*os.File) error { lockCloseCalls++; return nil }
		err := parent.Close()
		if !errors.Is(err, errRetainedRuntime) || !errors.Is(err, syscall.EIO) {
			t.Fatalf("lock release uncertainty = %v", err)
		}
		if parent.dir != nil || parent.lock != lock || lockCloseCalls != 0 {
			t.Fatalf("lock close continued after unlock uncertainty: dir=%v lock=%v calls=%d", parent.dir, parent.lock, lockCloseCalls)
		}
		if err := unix.Flock(int(lock.Fd()), unix.LOCK_UN); err != nil {
			t.Fatal(err)
		}
		if err := lock.Close(); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("lock close", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		lock := parent.lock
		parent.closeLock = func(*os.File) error { return syscall.EIO }
		err := parent.Close()
		if !errors.Is(err, errRetainedRuntime) || !errors.Is(err, syscall.EIO) {
			t.Fatalf("lock close uncertainty = %v", err)
		}
		if parent.dir != nil || parent.lock != lock {
			t.Fatalf("uncertain lock descriptor was discarded: dir=%v lock=%v", parent.dir, parent.lock)
		}
		if err := lock.Close(); err != nil {
			t.Fatal(err)
		}
	})
}

func TestRuntimeNamesAreExactAndSocketIsReserved(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	defer parent.Close()
	socketPath := filepath.Join(parentPath, install.LocalAPISocketName)
	if err := os.WriteFile(socketPath, []byte("socket authority"), 0o600); err != nil {
		t.Fatal(err)
	}
	before, err := os.ReadDir(parentPath)
	if err != nil {
		t.Fatal(err)
	}
	invalid := []string{
		"", "0", "0123456789abcdef0123456789abcde",
		"0123456789abcdef0123456789abcdef0",
		"0123456789abcdef0123456789abcdeF",
		"0123456789abcdef0123456789abcdeg",
		"../0123456789abcdef0123456789a",
		runtimeParentLockName, install.LocalAPISocketName,
	}
	expected := runner.FileIdentity{Device: 1, Inode: 1}
	for _, name := range invalid {
		if runtime, err := CreateRuntime(parent, name); !errors.Is(err, errInvalidContract) || runtime != nil {
			if runtime != nil {
				_ = runtime.Close()
			}
			t.Fatalf("CreateRuntime(%q) = %v, %v", name, runtime, err)
		}
		if runtime, err := AdoptRuntime(parent, name); !errors.Is(err, errInvalidContract) || runtime != nil {
			if runtime != nil {
				_ = runtime.Close()
			}
			t.Fatalf("AdoptRuntime(%q) = %v, %v", name, runtime, err)
		}
		if recovered, err := OpenRecoveredRuntime(context.Background(), parent, name, expected); !errors.Is(err, errInvalidContract) || recovered != nil {
			releaseUnexpectedRecovered(recovered)
			t.Fatalf("OpenRecoveredRuntime(%q) = %v, %v", name, recovered, err)
		}
		if _, err := ObserveRuntimeLifetime(parent, name, expected); !errors.Is(err, errInvalidContract) {
			t.Fatalf("ObserveRuntimeLifetime(%q) = %v", name, err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, name, expected); !errors.Is(err, errInvalidContract) || done {
			t.Fatalf("RemoveRecordedRuntime(%q) = %v, %v", name, done, err)
		}
	}
	after, err := os.ReadDir(parentPath)
	if err != nil {
		t.Fatal(err)
	}
	if fmt.Sprint(before) != fmt.Sprint(after) {
		t.Fatalf("invalid names changed parent: before=%v after=%v", before, after)
	}
	if got, err := os.ReadFile(socketPath); err != nil || string(got) != "socket authority" {
		t.Fatalf("factory.sock was treated as runtime: %q, %v", got, err)
	}
}

func TestRuntimeLifetimeLeaseRequiresLastDuplicateClose(t *testing.T) {
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	defer parent.Close()
	runtime, err := CreateRuntime(parent, runtimeTestName)
	if err != nil {
		t.Fatal(err)
	}
	identity := mustRuntimeIdentity(t, runtime)
	directory, duplicate, err := runtime.DuplicateRunnerFiles()
	if err != nil {
		t.Fatal(err)
	}
	if got, err := ObserveRuntimeLifetime(parent, runtimeTestName, identity); err != nil || got != RuntimeLeaseHeld {
		t.Fatalf("live runtime observation = %v, %v", got, err)
	}
	if err := runtime.Close(); err != nil {
		t.Fatal(err)
	}
	if err := directory.Close(); err != nil {
		t.Fatal(err)
	}
	if got, err := ObserveRuntimeLifetime(parent, runtimeTestName, identity); err != nil || got != RuntimeLeaseHeld {
		t.Fatalf("duplicate-held observation = %v, %v", got, err)
	}
	if err := duplicate.Close(); err != nil {
		t.Fatal(err)
	}
	if got, err := ObserveRuntimeLifetime(parent, runtimeTestName, identity); err != nil || got != RuntimeLeaseAvailable {
		t.Fatalf("released observation = %v, %v", got, err)
	}
}

func TestAdoptRuntimeClosesPreBindingCreationCrash(t *testing.T) {
	for _, partial := range []string{"empty", "home"} {
		t.Run(partial, func(t *testing.T) {
			parentPath := filepath.Join(runtimeTempDir(t), "private")
			if err := os.Mkdir(parentPath, 0o700); err != nil {
				t.Fatal(err)
			}
			parent := createManagedParent(t, parentPath)
			defer parent.Close()
			operation, err := parent.begin()
			if err != nil {
				t.Fatal(err)
			}
			ownedParent, err := operation.directory()
			if err != nil {
				t.Fatal(err)
			}
			if err := unix.Mkdirat(int(ownedParent.Fd()), runtimeTestName, 0o700); err != nil {
				t.Fatal(err)
			}
			if partial == "home" {
				rootFD, err := unix.Openat(int(ownedParent.Fd()), runtimeTestName, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
				if err != nil {
					t.Fatal(err)
				}
				if err := unix.Mkdirat(rootFD, runtimeHomeName, 0o700); err != nil {
					unix.Close(rootFD)
					t.Fatal(err)
				}
				if err := unix.Close(rootFD); err != nil {
					t.Fatal(err)
				}
			}
			if err := operation.Close(); err != nil {
				t.Fatal(err)
			}
			runtime, err := AdoptRuntime(parent, runtimeTestName)
			if err != nil {
				t.Fatal(err)
			}
			path, identity := mustRuntimeValues(t, runtime)
			for _, name := range []string{runtimeHomeName, runtimeTempName} {
				assertDirectory(t, filepath.Join(path, name), 0, 0, 0o700)
			}
			if got, err := ObserveRuntimeLifetime(parent, runtimeTestName, identity); err != nil || got != RuntimeLeaseHeld {
				t.Fatalf("adopted lifetime = %v, %v", got, err)
			}
			if err := runtime.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}

	t.Run("later phase effect is not adoptable", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		runtime, err := CreateRuntime(parent, runtimeTestName)
		if err != nil {
			t.Fatal(err)
		}
		path := mustRuntimePath(t, runtime)
		if _, err := runtime.PublishAttemptToken(context.Background(), [32]byte{1}); err != nil {
			t.Fatal(err)
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		if adopted, err := AdoptRuntime(parent, runtimeTestName); !errors.Is(err, errInvalidContract) || adopted != nil {
			if adopted != nil {
				_ = adopted.Close()
			}
			t.Fatalf("later phase adoption = %v, %v", adopted, err)
		}
		if _, err := os.Stat(filepath.Join(path, attemptTokenName)); err != nil {
			t.Fatalf("rejected adoption mutated token: %v", err)
		}
	})
}

func TestAdoptRuntimeAcquiresLifetimeBeforeRepair(t *testing.T) {
	t.Run("held lifetime with missing layout", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		root := filepath.Join(parentPath, runtimeTestName)
		if err := os.Mkdir(root, 0o700); err != nil {
			t.Fatal(err)
		}
		lifetime := createTestRuntimeLifetime(t, root, true)
		defer lifetime.Close()
		before := snapshotRuntimeGraph(t, root)
		if runtime, err := AdoptRuntime(parent, runtimeTestName); !errors.Is(err, errRuntimeBusy) || runtime != nil {
			if runtime != nil {
				_ = runtime.Close()
			}
			t.Fatalf("held lifetime adoption = %v, %v", runtime, err)
		}
		if after := snapshotRuntimeGraph(t, root); after != before {
			t.Fatalf("held lifetime adoption mutated graph\nbefore:\n%s\nafter:\n%s", before, after)
		}
	})

	t.Run("held lifetime with malformed graph", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		root := filepath.Join(parentPath, runtimeTestName)
		if err := os.Mkdir(root, 0o700); err != nil {
			t.Fatal(err)
		}
		lifetime := createTestRuntimeLifetime(t, root, true)
		defer lifetime.Close()
		if err := os.WriteFile(filepath.Join(root, "unexpected"), []byte("sentinel"), 0o600); err != nil {
			t.Fatal(err)
		}
		before := snapshotRuntimeGraph(t, root)
		if runtime, err := AdoptRuntime(parent, runtimeTestName); !errors.Is(err, errInvalidContract) || runtime != nil {
			if runtime != nil {
				_ = runtime.Close()
			}
			t.Fatalf("malformed held lifetime adoption = %v, %v", runtime, err)
		}
		if after := snapshotRuntimeGraph(t, root); after != before {
			t.Fatalf("malformed held adoption mutated graph\nbefore:\n%s\nafter:\n%s", before, after)
		}
	})

	t.Run("lifetime replacement during acquisition", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		root := filepath.Join(parentPath, runtimeTestName)
		if err := os.Mkdir(root, 0o700); err != nil {
			t.Fatal(err)
		}
		if err := createTestRuntimeLifetime(t, root, false).Close(); err != nil {
			t.Fatal(err)
		}
		rootDirectory := openDirectory(t, root)
		defer rootDirectory.Close()
		rootIdentity, err := inspectPrivateDirectory(int(rootDirectory.Fd()))
		if err != nil {
			t.Fatal(err)
		}
		fd, err := unix.Openat(int(rootDirectory.Fd()), runner.RuntimeLifetimeLeaseName, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
		if err != nil {
			t.Fatal(err)
		}
		opened := os.NewFile(uintptr(fd), "replaced-lifetime-proof")
		defer opened.Close()
		if _, _, err := inspectRuntimeLifetimeBinding(int(rootDirectory.Fd()), fd, rootIdentity.device); err != nil {
			t.Fatal(err)
		}
		leasePath := filepath.Join(root, runner.RuntimeLifetimeLeaseName)
		movedPath := filepath.Join(root, "moved-lifetime")
		if err := os.Rename(leasePath, movedPath); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(leasePath, nil, 0o600); err != nil {
			t.Fatal(err)
		}
		replacement, err := os.Lstat(leasePath)
		if err != nil {
			t.Fatal(err)
		}
		if err := unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB); err != nil {
			t.Fatal(err)
		}
		if _, _, err := inspectRuntimeLifetimeBinding(int(rootDirectory.Fd()), fd, rootIdentity.device); !errors.Is(err, errInvalidContract) {
			t.Fatalf("replacement binding recheck = %v", err)
		}
		after, statErr := os.Lstat(leasePath)
		if statErr != nil || !os.SameFile(replacement, after) {
			t.Fatalf("replacement lifetime changed: %v, %v", after, statErr)
		}
		if _, statErr := os.Lstat(movedPath); statErr != nil {
			t.Fatalf("opened original lifetime changed: %v", statErr)
		}
		for _, name := range []string{runtimeHomeName, runtimeTempName} {
			if _, statErr := os.Lstat(filepath.Join(root, name)); !errors.Is(statErr, os.ErrNotExist) {
				t.Fatalf("replacement race created %s: %v", name, statErr)
			}
		}
	})

}

func TestRuntimeBindingAndDuplicationRejectChangedParentLock(t *testing.T) {
	for _, mutation := range []string{"missing", "replacement", "mode", "hardlink"} {
		t.Run(mutation, func(t *testing.T) {
			parentPath := filepath.Join(runtimeTempDir(t), "private")
			if err := os.Mkdir(parentPath, 0o700); err != nil {
				t.Fatal(err)
			}
			parent := createManagedParent(t, parentPath)
			defer parent.Close()
			runtime, err := CreateRuntime(parent, runtimeTestName)
			if err != nil {
				t.Fatal(err)
			}
			defer runtime.Close()
			binding, err := runtime.Binding()
			if err != nil {
				t.Fatal(err)
			}
			lockPath := filepath.Join(parentPath, runtimeParentLockName)
			switch mutation {
			case "missing":
				if err := os.Remove(lockPath); err != nil {
					t.Fatal(err)
				}
			case "replacement":
				if err := os.Rename(lockPath, lockPath+".old"); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(lockPath, nil, 0o600); err != nil {
					t.Fatal(err)
				}
			case "mode":
				if err := os.Chmod(lockPath, 0o640); err != nil {
					t.Fatal(err)
				}
			case "hardlink":
				if err := os.Link(lockPath, lockPath+".link"); err != nil {
					t.Fatal(err)
				}
			}
			if _, _, err := binding.Values(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("cached Binding after %s = %v", mutation, err)
			}
			if directory, lifetime, err := runtime.DuplicateRunnerFiles(); !errors.Is(err, errInvalidContract) || directory != nil || lifetime != nil {
				t.Fatalf("DuplicateRunnerFiles after %s = %v, %v, %v", mutation, directory, lifetime, err)
			}
		})
	}
}

func TestRuntimeBindingRejectsFixedChildReplacement(t *testing.T) {
	for _, name := range []string{runtimeHomeName, runtimeTempName} {
		t.Run(name, func(t *testing.T) {
			runtime := newTestRuntime(t)
			defer runtime.Close()
			binding, err := runtime.Binding()
			if err != nil {
				t.Fatal(err)
			}
			path := mustRuntimePath(t, runtime)
			child := filepath.Join(path, name)
			if err := os.Rename(child, child+".old"); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(child, 0o700); err != nil {
				t.Fatal(err)
			}
			if _, _, err := binding.Values(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("cached binding accepted replaced %s: %v", name, err)
			}
			if _, err := binding.ProviderHome(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("home locator accepted replaced %s: %v", name, err)
			}
			if _, err := binding.ProviderTemp(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("temp locator accepted replaced %s: %v", name, err)
			}
			if _, err := binding.AttemptTokenPath(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("token locator accepted replaced %s: %v", name, err)
			}
		})
	}
}

func TestRuntimeBindingRejectsLifetimeMutation(t *testing.T) {
	for _, mutation := range []string{"mode", "hardlink", "replacement"} {
		t.Run(mutation, func(t *testing.T) {
			runtime := newTestRuntime(t)
			defer runtime.Close()
			binding, err := runtime.Binding()
			if err != nil {
				t.Fatal(err)
			}
			path := mustRuntimePath(t, runtime)
			lease := filepath.Join(path, runner.RuntimeLifetimeLeaseName)
			switch mutation {
			case "mode":
				if err := os.Chmod(lease, 0o640); err != nil {
					t.Fatal(err)
				}
			case "hardlink":
				if err := os.Link(lease, lease+".link"); err != nil {
					t.Fatal(err)
				}
			case "replacement":
				if err := os.Rename(lease, lease+".old"); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(lease, nil, 0o600); err != nil {
					t.Fatal(err)
				}
			}
			if _, _, err := binding.Values(); !errors.Is(err, errInvalidContract) {
				t.Fatalf("Binding after lifetime %s = %v", mutation, err)
			}
			if directory, lifetime, err := runtime.DuplicateRunnerFiles(); !errors.Is(err, errInvalidContract) || directory != nil || lifetime != nil {
				t.Fatalf("DuplicateRunnerFiles after lifetime %s = %v, %v, %v", mutation, directory, lifetime, err)
			}
		})
	}
}

func TestRuntimeParentReplacementFailsClosedAndLeafSwapFailsClosed(t *testing.T) {
	t.Run("parent replacement revokes diagnostic publication", func(t *testing.T) {
		root := runtimeTempDir(t)
		parentPath := filepath.Join(root, "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		runtime, err := createRuntime(parent, runtimeTestName, func() {
			if err := os.Rename(parentPath, parentPath+".old"); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(parentPath, 0o700); err != nil {
				t.Fatal(err)
			}
		}, nil, nil)
		if !errors.Is(err, errInvalidContract) || runtime != nil {
			if runtime != nil {
				_ = runtime.Close()
			}
			t.Fatalf("parent replacement create = %v, %v", runtime, err)
		}
		if _, err := os.Lstat(filepath.Join(parentPath, runtimeTestName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("replacement parent gained runtime: %v", err)
		}
		if _, err := os.Lstat(filepath.Join(parentPath+".old", runtimeTestName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("retained parent kept unpublished runtime: %v", err)
		}
	})

	t.Run("leaf", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		_, err := createRuntime(parent, runtimeTestName, nil, func() {
			if err := os.Rename(filepath.Join(parentPath, runtimeTestName), filepath.Join(parentPath, "old")); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(filepath.Join(parentPath, runtimeTestName), 0o700); err != nil {
				t.Fatal(err)
			}
		}, nil)
		if !errors.Is(err, errInvalidContract) {
			t.Fatalf("leaf swap error = %v", err)
		}
		if _, err := os.Lstat(filepath.Join(parentPath, "old")); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("exact moved runtime was not cleaned: %v", err)
		}
		if info, err := os.Lstat(filepath.Join(parentPath, runtimeTestName)); err != nil || !info.IsDir() {
			t.Fatalf("foreign runtime replacement changed: %v %v", info, err)
		}
	})
}

func TestRuntimeAuthorityRejectsPathReplacementAtEveryAccessor(t *testing.T) {
	t.Run("before publication", func(t *testing.T) {
		runtime := newTestRuntime(t)
		defer runtime.Close()
		path := mustRuntimePath(t, runtime)
		moved := path + ".moved"
		if err := os.Rename(path, moved); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
		if _, err := runtime.Binding(); !errors.Is(err, errInvalidContract) {
			t.Fatalf("Binding after replacement = %v", err)
		}
		if directory, lifetime, err := runtime.DuplicateRunnerFiles(); !errors.Is(err, errInvalidContract) || directory != nil || lifetime != nil {
			t.Fatalf("DuplicateRunnerFiles after replacement = %v, %v, %v", directory, lifetime, err)
		}
		if _, err := runtime.PublishAttemptToken(context.Background(), [32]byte{1}); !errors.Is(err, errInvalidContract) {
			t.Fatalf("token after replacement = %v", err)
		}
		for _, root := range []string{path, moved} {
			for _, name := range []string{attemptTokenName} {
				if _, err := os.Lstat(filepath.Join(root, name)); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("replaced runtime gained %s under %s: %v", name, root, err)
				}
			}
		}
	})

	t.Run("after publication", func(t *testing.T) {
		runtime := newTestRuntime(t)
		defer runtime.Close()
		file, err := runtime.PublishAttemptToken(context.Background(), [32]byte{2})
		if err != nil {
			t.Fatal(err)
		}
		path := mustRuntimePath(t, runtime)
		moved := path + ".moved"
		if err := os.Rename(path, moved); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
		if _, err := file.Path(); !errors.Is(err, errInvalidContract) {
			t.Fatalf("private file path after runtime replacement = %v", err)
		}
		if _, err := runtime.Binding(); !errors.Is(err, errInvalidContract) {
			t.Fatalf("runtime binding after publication replacement = %v", err)
		}
		if _, err := os.Lstat(filepath.Join(path, attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("foreign replacement gained token: %v", err)
		}
	})
}

func TestFailedRuntimeCreationCleansOnlyExactEmptyIdentity(t *testing.T) {
	t.Run("fsync failure", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		calls := 0
		syncDirectory := func(fd int) error {
			calls++
			if calls == 1 {
				return syscall.EIO
			}
			return unix.Fsync(fd)
		}
		if _, err := createRuntime(parent, runtimeTestName, nil, nil, syncDirectory); !errors.Is(err, syscall.EIO) || errors.Is(err, errRetainedRuntime) {
			t.Fatalf("fsync failure = %v", err)
		}
		if calls < 2 {
			t.Fatalf("cleanup did not fsync parent: calls=%d", calls)
		}
		if _, err := os.Lstat(filepath.Join(parentPath, runtimeTestName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("failed runtime retained after exact cleanup: %v", err)
		}
	})

	t.Run("post-lifetime failure cleans exact layout", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		rootSyncs := 0
		syncDirectory := func(fd int) error {
			var lifetime unix.Stat_t
			if err := unix.Fstatat(fd, runner.RuntimeLifetimeLeaseName, &lifetime, unix.AT_SYMLINK_NOFOLLOW); err == nil {
				rootSyncs++
				if rootSyncs == 2 {
					return syscall.EIO
				}
			}
			return unix.Fsync(fd)
		}
		if _, err := createRuntime(parent, runtimeTestName, nil, nil, syncDirectory); !errors.Is(err, syscall.EIO) || errors.Is(err, errRetainedRuntime) {
			t.Fatalf("post-lifetime failure = %v", err)
		}
		if rootSyncs < 2 {
			t.Fatalf("post-lifetime failure was not reached: syncs=%d", rootSyncs)
		}
		if _, err := os.Lstat(filepath.Join(parentPath, runtimeTestName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("exact failed runtime retained: %v", err)
		}
	})

	t.Run("moved exact lifetime cleanup preserves replacement", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		root := filepath.Join(parentPath, runtimeTestName)
		leasePath := filepath.Join(root, runner.RuntimeLifetimeLeaseName)
		movedPath := filepath.Join(root, "moved-lifetime")
		rootSyncs := 0
		var replacement os.FileInfo
		syncDirectory := func(fd int) error {
			var lifetime unix.Stat_t
			if err := unix.Fstatat(fd, runner.RuntimeLifetimeLeaseName, &lifetime, unix.AT_SYMLINK_NOFOLLOW); err == nil {
				rootSyncs++
				if rootSyncs == 2 {
					if err := os.Rename(leasePath, movedPath); err != nil {
						t.Fatal(err)
					}
					if err := os.WriteFile(leasePath, nil, 0o600); err != nil {
						t.Fatal(err)
					}
					replacement, err = os.Lstat(leasePath)
					if err != nil {
						t.Fatal(err)
					}
					return syscall.EIO
				}
			}
			return unix.Fsync(fd)
		}
		if _, err := createRuntime(parent, runtimeTestName, nil, nil, syncDirectory); !errors.Is(err, errRetainedRuntime) || !errors.Is(err, syscall.EIO) {
			t.Fatalf("replaced lifetime cleanup = %v", err)
		}
		if _, err := os.Lstat(movedPath); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("moved exact lifetime retained: %v", err)
		}
		after, err := os.Lstat(leasePath)
		if err != nil || !os.SameFile(replacement, after) {
			t.Fatalf("replacement lifetime changed: %v, %v", after, err)
		}
		for _, name := range []string{runtimeHomeName, runtimeTempName} {
			if _, err := os.Lstat(filepath.Join(root, name)); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("exact layout child %s retained: %v", name, err)
			}
		}
	})

	t.Run("nonempty retained uncertainty", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		_, err := createRuntime(parent, runtimeTestName, nil, func() {
			if err := os.Rename(filepath.Join(parentPath, runtimeTestName), filepath.Join(parentPath, "retained")); err != nil {
				t.Fatal(err)
			}
			if err := os.WriteFile(filepath.Join(parentPath, "retained", "foreign"), []byte("sentinel"), 0o600); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(filepath.Join(parentPath, runtimeTestName), 0o700); err != nil {
				t.Fatal(err)
			}
		}, nil)
		if !errors.Is(err, errRetainedRuntime) {
			t.Fatalf("nonempty retained failure = %T %v", err, err)
		}
		contents, readErr := os.ReadFile(filepath.Join(parentPath, "retained", "foreign"))
		if readErr != nil || string(contents) != "sentinel" {
			t.Fatalf("retained uncertainty was deleted: %q %v", contents, readErr)
		}
		if info, statErr := os.Lstat(filepath.Join(parentPath, runtimeTestName)); statErr != nil || !info.IsDir() {
			t.Fatalf("foreign replacement changed: %v %v", info, statErr)
		}
	})

	t.Run("bounded search refusal", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		for index := 0; index <= cleanupSearchLimit; index++ {
			if err := os.Mkdir(filepath.Join(parentPath, fmt.Sprintf("filler-%03d", index)), 0o700); err != nil {
				t.Fatal(err)
			}
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		_, err := createRuntime(parent, runtimeTestName, nil, func() {
			if err := os.Rename(filepath.Join(parentPath, runtimeTestName), filepath.Join(parentPath, "retained")); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(filepath.Join(parentPath, runtimeTestName), 0o700); err != nil {
				t.Fatal(err)
			}
		}, nil)
		if !errors.Is(err, errRetainedRuntime) {
			t.Fatalf("bounded search failure = %v", err)
		}
		for _, name := range []string{runtimeTestName, "retained"} {
			if info, statErr := os.Lstat(filepath.Join(parentPath, name)); statErr != nil || !info.IsDir() {
				t.Fatalf("bounded refusal deleted %s: %v %v", name, info, statErr)
			}
		}
	})
}

func TestPrivatePublicationRejectsSpecialExistingAndCleansPartialWrite(t *testing.T) {
	for name, create := range map[string]func(string) error{
		"symlink": func(path string) error { return os.Symlink("missing", path) },
		"fifo":    func(path string) error { return unix.Mkfifo(path, 0o600) },
		"dir":     func(path string) error { return os.Mkdir(path, 0o700) },
	} {
		t.Run(name, func(t *testing.T) {
			runtime := newTestRuntime(t)
			defer runtime.Close()
			path := filepath.Join(mustRuntimePath(t, runtime), attemptTokenName)
			if err := create(path); err != nil {
				t.Fatal(err)
			}
			before, _ := os.Lstat(path)
			if _, err := runtime.PublishAttemptToken(context.Background(), [32]byte{1}); !errors.Is(err, errInvalidContract) {
				t.Fatalf("publish error = %v", err)
			}
			after, err := os.Lstat(path)
			if err != nil || !os.SameFile(before, after) {
				t.Fatalf("existing leaf mutated: %v", err)
			}
		})
	}

	runtime := newTestRuntime(t)
	defer runtime.Close()
	writes := 0
	_, err := runtime.publish(context.Background(), attemptTokenName, []byte("partial-write"), 32, func(fd int, value []byte) (int, error) {
		writes++
		if writes == 1 {
			return unix.Write(fd, value[:3])
		}
		return 0, syscall.EIO
	}, nil)
	if !errors.Is(err, syscall.EIO) {
		t.Fatalf("partial write error = %v", err)
	}
	if _, err := os.Lstat(filepath.Join(mustRuntimePath(t, runtime), attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("partial file retained: %v", err)
	}

	leafRuntime := newTestRuntime(t)
	defer leafRuntime.Close()
	leafPath := filepath.Join(mustRuntimePath(t, leafRuntime), attemptTokenName)
	_, err = leafRuntime.publish(context.Background(), attemptTokenName, []byte("private"), 32, func(fd int, value []byte) (int, error) {
		if err := os.Rename(leafPath, leafPath+".old"); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(leafPath, []byte("replacement"), 0o600); err != nil {
			t.Fatal(err)
		}
		return unix.Write(fd, value)
	}, nil)
	if !errors.Is(err, errInvalidContract) {
		t.Fatalf("leaf swap error = %v", err)
	}
	replacement, readErr := os.ReadFile(leafPath)
	if readErr != nil || string(replacement) != "replacement" {
		t.Fatalf("leaf replacement mutated = %q, %v", replacement, readErr)
	}
	if _, err := os.Lstat(leafPath + ".old"); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("exact moved private file was not cleaned: %v", err)
	}

	canceled, cancel := context.WithCancel(context.Background())
	cancel()
	if _, err := runtime.publish(canceled, attemptTokenName, []byte("private"), 32, nil, nil); !errors.Is(err, context.Canceled) {
		t.Fatalf("cancellation error = %v", err)
	}
	if _, err := os.Lstat(filepath.Join(mustRuntimePath(t, runtime), attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("canceled publish gained effect: %v", err)
	}

	midRuntime := newTestRuntime(t)
	defer midRuntime.Close()
	midContext, midCancel := context.WithCancel(context.Background())
	_, err = midRuntime.publish(midContext, attemptTokenName, []byte("cancel-between-writes"), 32, func(fd int, value []byte) (int, error) {
		written, writeErr := unix.Write(fd, value[:1])
		midCancel()
		return written, writeErr
	}, nil)
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("mid-write cancellation error = %v", err)
	}
	if _, err := os.Lstat(filepath.Join(mustRuntimePath(t, midRuntime), attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("mid-write cancellation retained file: %v", err)
	}
}

func TestFailedPrivatePublicationCleansThroughRetainedDirectoryAuthority(t *testing.T) {
	t.Run("runtime and leaf replaced", func(t *testing.T) {
		runtime := newTestRuntime(t)
		defer runtime.Close()
		path := mustRuntimePath(t, runtime)
		moved := path + ".moved"
		_, err := runtime.publish(context.Background(), attemptTokenName, []byte("private"), 32, func(fd int, value []byte) (int, error) {
			if err := os.Rename(path, moved); err != nil {
				t.Fatal(err)
			}
			if err := os.Mkdir(path, 0o700); err != nil {
				t.Fatal(err)
			}
			return unix.Write(fd, value)
		}, nil)
		if !errors.Is(err, errInvalidContract) || errors.Is(err, errRetainedRuntime) {
			t.Fatalf("runtime replacement publication error = %v", err)
		}
		if _, err := os.Lstat(filepath.Join(moved, attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("created file was not removed through retained dirfd: %v", err)
		}
		if entries, readErr := os.ReadDir(path); readErr != nil || len(entries) != 0 {
			t.Fatalf("foreign runtime replacement changed: %v %v", entries, readErr)
		}
	})

	t.Run("file fsync failure", func(t *testing.T) {
		runtime := newTestRuntime(t)
		defer runtime.Close()
		calls := 0
		syncDirectory := func(fd int) error {
			calls++
			if calls == 1 {
				return syscall.EIO
			}
			return unix.Fsync(fd)
		}
		_, err := runtime.publish(context.Background(), attemptTokenName, []byte("private"), 32, nil, syncDirectory)
		if !errors.Is(err, syscall.EIO) || errors.Is(err, errRetainedRuntime) {
			t.Fatalf("file fsync failure = %v", err)
		}
		if calls < 2 {
			t.Fatalf("file cleanup did not fsync containing directory: calls=%d", calls)
		}
		if _, err := os.Lstat(filepath.Join(mustRuntimePath(t, runtime), attemptTokenName)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("failed private file retained after exact cleanup: %v", err)
		}
	})

	t.Run("hardlink retained uncertainty", func(t *testing.T) {
		runtime := newTestRuntime(t)
		defer runtime.Close()
		path := filepath.Join(mustRuntimePath(t, runtime), attemptTokenName)
		link := path + ".link"
		_, err := runtime.publish(context.Background(), attemptTokenName, []byte("private"), 32, func(fd int, value []byte) (int, error) {
			if err := os.Link(path, link); err != nil {
				t.Fatal(err)
			}
			return unix.Write(fd, value)
		}, nil)
		if !errors.Is(err, errRetainedRuntime) {
			t.Fatalf("hardlink uncertainty = %T %v", err, err)
		}
		for _, retained := range []string{path, link} {
			if info, statErr := os.Lstat(retained); statErr != nil || !info.Mode().IsRegular() {
				t.Fatalf("uncertain hardlink deleted %s: %v %v", retained, info, statErr)
			}
		}
	})
}

func TestRuntimeValuesAndErrorsRedactPrivatePaths(t *testing.T) {
	runtime := newTestRuntime(t)
	defer runtime.Close()
	file, err := runtime.PublishAttemptToken(context.Background(), [32]byte{1})
	if err != nil {
		t.Fatal(err)
	}
	for _, formatted := range []string{fmt.Sprint(file), fmt.Sprintf("%#v", file)} {
		if stringsContainsAny(formatted, mustRuntimePath(t, runtime)) {
			t.Fatalf("private file formatting leaked: %q", formatted)
		}
	}
	_, err = runtime.PublishAttemptToken(context.Background(), [32]byte{2})
	if err == nil || stringsContainsAny(err.Error(), mustRuntimePath(t, runtime)) {
		t.Fatalf("private error leaked: %v", err)
	}
}

func TestRemoveRecordedRuntimeUsesFixedBoundedGrammar(t *testing.T) {
	t.Run("active then complete and idempotent", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		runtime, err := CreateRuntime(parent, runtimeTestName)
		if err != nil {
			t.Fatal(err)
		}
		path, identity := mustRuntimeValues(t, runtime)
		if err := os.MkdirAll(filepath.Join(path, runtimeHomeName, "deep"), 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(path, runtimeHomeName, "deep", "provider"), []byte("ordinary"), 0o644); err != nil {
			t.Fatal(err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); err != nil || done {
			t.Fatalf("active removal = %v, %v", done, err)
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); err != nil || !done {
			t.Fatalf("complete removal = %v, %v", done, err)
		}
		if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("runtime retained: %v", err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); err != nil || !done {
			t.Fatalf("idempotent removal = %v, %v", done, err)
		}
	})

	t.Run("terminal and unexpected entries block", func(t *testing.T) {
		for _, name := range []string{runner.TerminalSpoolName, "unexpected"} {
			t.Run(name, func(t *testing.T) {
				parentPath := filepath.Join(runtimeTempDir(t), "private")
				if err := os.Mkdir(parentPath, 0o700); err != nil {
					t.Fatal(err)
				}
				parent := createManagedParent(t, parentPath)
				defer parent.Close()
				runtime, err := CreateRuntime(parent, runtimeTestName)
				if err != nil {
					t.Fatal(err)
				}
				path, identity := mustRuntimeValues(t, runtime)
				if err := os.WriteFile(filepath.Join(path, name), []byte("sentinel"), 0o600); err != nil {
					t.Fatal(err)
				}
				if err := runtime.Close(); err != nil {
					t.Fatal(err)
				}
				got, removeErr := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity)
				if !errors.Is(removeErr, errInvalidContract) || got {
					t.Fatalf("unexpected removal = %v, %v", got, removeErr)
				}
				contents, err := os.ReadFile(filepath.Join(path, name))
				if err != nil || string(contents) != "sentinel" {
					t.Fatalf("blocking entry mutated: %q %v", contents, err)
				}
			})
		}
	})

	t.Run("bounded progress", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		runtime, err := CreateRuntime(parent, runtimeTestName)
		if err != nil {
			t.Fatal(err)
		}
		path, identity := mustRuntimeValues(t, runtime)
		for index := 0; index < 4; index++ {
			if err := os.WriteFile(filepath.Join(path, runtimeTempName, fmt.Sprintf("%d", index)), nil, 0o600); err != nil {
				t.Fatal(err)
			}
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		if done, err := removeRecordedRuntime(context.Background(), parent, runtimeTestName, identity, 2, nil); err != nil || done {
			t.Fatalf("bounded pass = %v, %v", done, err)
		}
		for attempts := 0; attempts < 8; attempts++ {
			done, err := removeRecordedRuntime(context.Background(), parent, runtimeTestName, identity, 2, nil)
			if err != nil {
				t.Fatal(err)
			}
			if done {
				return
			}
		}
		t.Fatal("bounded removal did not converge")
	})

	t.Run("bounded readonly nested directory", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		cache := filepath.Join(path, runtimeHomeName, "module-cache")
		if err := os.Mkdir(cache, 0o700); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(cache, "module"), []byte("cached"), 0o400); err != nil {
			t.Fatal(err)
		}
		if err := unix.Chmod(cache, 0o500); err != nil {
			t.Fatal(err)
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		for attempts := 0; attempts < 8; attempts++ {
			done, err := removeRecordedRuntime(context.Background(), parent, runtimeTestName, identity, 2, nil)
			if err != nil {
				t.Fatal(err)
			}
			if done {
				if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("readonly runtime remains: %v", err)
				}
				return
			}
		}
		t.Fatal("readonly bounded removal did not converge")
	})
}

func TestRemoveRecordedRuntimeUnlinksAnyNameAndRejectsAuthorityChanges(t *testing.T) {
	t.Run("terminal blocks before any deletion", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		if err := os.WriteFile(filepath.Join(path, attemptTokenName), []byte("token"), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(filepath.Join(path, runner.TerminalSpoolName), []byte("terminal"), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); !errors.Is(err, errInvalidContract) || done {
			t.Fatalf("terminal removal = %v, %v", done, err)
		}
		if body, err := os.ReadFile(filepath.Join(path, attemptTokenName)); err != nil || string(body) != "token" {
			t.Fatalf("preflight deleted token: %q %v", body, err)
		}
	})

	// A name that is not a directory is unlinked whatever it is: the link,
	// the socket, the fifo or one name of a hard-linked file goes, and what
	// it pointed at or shared is untouched. A provider's tree holds all of
	// these, and refusing any of them left its run finalizing for good.
	for _, kind := range []string{"symlink", "fifo", "socket", "hardlink"} {
		t.Run(kind, func(t *testing.T) {
			var parent *RuntimeParent
			var runtime *Runtime
			var path string
			var identity runner.FileIdentity
			if kind == "socket" {
				short := runtimeTempDir(t)
				parent, runtime, path, identity = removableRuntimeFixtureAt(t, filepath.Join(short, "private"))
			} else {
				parent, runtime, path, identity = removableRuntimeFixture(t)
			}
			defer parent.Close()
			target := filepath.Join(runtimeTempDir(t), "external")
			if err := os.WriteFile(target, []byte("external"), 0o600); err != nil {
				t.Fatal(err)
			}
			leaf := filepath.Join(path, runtimeHomeName, "unsafe")
			switch kind {
			case "symlink":
				if err := os.Symlink(target, leaf); err != nil {
					t.Fatal(err)
				}
			case "fifo":
				if err := unix.Mkfifo(leaf, 0o600); err != nil {
					t.Fatal(err)
				}
			case "socket":
				listener, err := net.Listen("unix", leaf)
				if err != nil {
					t.Fatal(err)
				}
				defer listener.Close()
			case "hardlink":
				if err := os.Link(target, leaf); err != nil {
					t.Fatal(err)
				}
			}
			if err := runtime.Close(); err != nil {
				t.Fatal(err)
			}
			if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); err != nil || !done {
				t.Fatalf("%s removal = %v, %v", kind, done, err)
			}
			if body, err := os.ReadFile(target); err != nil || string(body) != "external" {
				t.Fatalf("external target mutated: %q %v", body, err)
			}
			if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("runtime with a %s remains: %v", kind, err)
			}
		})
	}

	t.Run("root replacement", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		moved := path + ".old"
		if err := os.Rename(path, moved); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); !errors.Is(err, errInvalidContract) || done {
			t.Fatalf("replacement removal = %v, %v", done, err)
		}
		for _, retained := range []string{path, moved} {
			if info, err := os.Lstat(retained); err != nil || !info.IsDir() {
				t.Fatalf("replacement attack mutated %s: %v %v", retained, info, err)
			}
		}
	})

}

func TestRemoveRecordedRuntimeBoundsDepthAndReestablishesDurableAbsence(t *testing.T) {
	t.Run("depth bound", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		current := filepath.Join(path, runtimeHomeName)
		for depth := 0; depth <= runtimeRemovalDepthLimit; depth++ {
			current = filepath.Join(current, "d")
			if err := os.Mkdir(current, 0o700); err != nil {
				t.Fatal(err)
			}
		}
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		before := openFDCensus(t)
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeTestName, identity); !errors.Is(err, errInvalidContract) || done {
			t.Fatalf("deep removal = %v, %v", done, err)
		}
		assertFDCensus(t, before)
	})

	t.Run("already absent fsync and recheck", func(t *testing.T) {
		parentPath := filepath.Join(runtimeTempDir(t), "private")
		if err := os.Mkdir(parentPath, 0o700); err != nil {
			t.Fatal(err)
		}
		parent := createManagedParent(t, parentPath)
		defer parent.Close()
		expected := runner.FileIdentity{Device: 1, Inode: 1}
		calls := 0
		if done, err := removeRecordedRuntime(context.Background(), parent, runtimeGoneName, expected, 1, func(int) error {
			calls++
			return syscall.EIO
		}); !errors.Is(err, syscall.EIO) || done {
			t.Fatalf("absent fsync failure = %v, %v", done, err)
		}
		if calls != 1 {
			t.Fatalf("absent fsync calls = %d", calls)
		}
		if done, err := RemoveRecordedRuntime(context.Background(), parent, runtimeGoneName, expected); err != nil || !done {
			t.Fatalf("durable absent retry = %v, %v", done, err)
		}
	})

	t.Run("final absence rejects concurrent replacement", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		done, err := removeRecordedRuntimeWithHook(context.Background(), parent, runtimeTestName, identity, runtimeRemovalEffectLimit, nil, func() {
			if err := os.Mkdir(path, 0o700); err != nil {
				t.Fatal(err)
			}
		})
		if !errors.Is(err, errInvalidContract) || done {
			t.Fatalf("replacement after parent fsync = %v, %v", done, err)
		}
		if info, err := os.Lstat(path); err != nil || !info.IsDir() {
			t.Fatalf("replacement removed: %v %v", info, err)
		}
	})

	t.Run("cancellation before effect", func(t *testing.T) {
		parent, runtime, path, identity := removableRuntimeFixture(t)
		defer parent.Close()
		if err := runtime.Close(); err != nil {
			t.Fatal(err)
		}
		ctx, cancel := context.WithCancel(context.Background())
		cancel()
		if done, err := RemoveRecordedRuntime(ctx, parent, runtimeTestName, identity); !errors.Is(err, context.Canceled) || done {
			t.Fatalf("canceled removal = %v, %v", done, err)
		}
		if _, err := os.Lstat(path); err != nil {
			t.Fatalf("canceled removal mutated runtime: %v", err)
		}
	})
}

func removableRuntimeFixture(t testing.TB) (*RuntimeParent, *Runtime, string, runner.FileIdentity) {
	t.Helper()
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	return removableRuntimeFixtureAt(t, parentPath)
}

func removableRuntimeFixtureAt(t testing.TB, parentPath string) (*RuntimeParent, *Runtime, string, runner.FileIdentity) {
	t.Helper()
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	runtime, err := CreateRuntime(parent, runtimeTestName)
	if err != nil {
		parent.Close()
		t.Fatal(err)
	}
	path, identity := mustRuntimeValues(t, runtime)
	return parent, runtime, path, identity
}

func newTestRuntime(t testing.TB) *Runtime {
	t.Helper()
	parentPath := filepath.Join(runtimeTempDir(t), "private")
	if err := os.Mkdir(parentPath, 0o700); err != nil {
		t.Fatal(err)
	}
	parent := createManagedParent(t, parentPath)
	runtime, err := CreateRuntime(parent, runtimeTestName)
	if err != nil {
		parent.Close()
		t.Fatalf("CreateRuntime = %v", err)
	}
	t.Cleanup(func() { parent.Close() })
	return runtime
}

func openDirectory(t testing.TB, path string) *os.File {
	t.Helper()
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		t.Fatal(err)
	}
	return os.NewFile(uintptr(fd), path)
}

func assertDirectory(t testing.TB, path string, device, inode uint64, mode os.FileMode) {
	t.Helper()
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	wrongIdentity := device != 0 && inode != 0 && (uint64(stat.Dev) != device || stat.Ino != inode)
	if !ok || !info.IsDir() || info.Mode().Perm() != mode || wrongIdentity || stat.Uid != uint32(os.Geteuid()) {
		t.Fatalf("unsafe directory: %+v", info)
	}
}

func assertPrivateFile(t testing.TB, file PrivateFile, want []byte) {
	t.Helper()
	path := mustPrivatePath(t, file)
	got, err := os.ReadFile(path)
	if err != nil || !bytes.Equal(got, want) {
		t.Fatalf("private contents = %x, %v", got, err)
	}
	info, err := os.Lstat(path)
	if err != nil {
		t.Fatal(err)
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || !info.Mode().IsRegular() || info.Mode().Perm() != 0o600 || stat.Nlink != 1 || stat.Uid != uint32(os.Geteuid()) || uint64(stat.Dev) != file.Identity().Device || stat.Ino != file.Identity().Inode {
		t.Fatalf("unsafe private file: %+v", info)
	}
}

func mustRuntimePath(t testing.TB, runtime *Runtime) string {
	t.Helper()
	path, _ := mustRuntimeValues(t, runtime)
	return path
}

func mustRuntimeIdentity(t testing.TB, runtime *Runtime) runner.FileIdentity {
	t.Helper()
	_, identity := mustRuntimeValues(t, runtime)
	return identity
}

func mustRuntimeValues(t testing.TB, runtime *Runtime) (string, runner.FileIdentity) {
	t.Helper()
	binding, err := runtime.Binding()
	if err != nil {
		t.Fatal(err)
	}
	path, identity, err := binding.Values()
	if err != nil {
		t.Fatal(err)
	}
	return path, identity
}

func createManagedParent(t testing.TB, path string) *RuntimeParent {
	t.Helper()
	directoryFD, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		t.Fatal(err)
	}
	directoryFile := os.NewFile(uintptr(directoryFD), "test-runtime-parent")
	directory, err := inspectPrivateDirectory(directoryFD)
	if err != nil {
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	lockFD, err := unix.Openat(directoryFD, runtimeParentLockName, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0o600)
	if err != nil {
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	lockFile := os.NewFile(uintptr(lockFD), "test-runtime-parent-lock")
	lockID, err := inspectRuntimeLock(lockFD)
	if err != nil {
		_ = lockFile.Close()
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	if err := unix.Flock(lockFD, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		_ = lockFile.Close()
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	if err := unix.Fsync(lockFD); err != nil {
		_ = unix.Flock(lockFD, unix.LOCK_UN)
		_ = lockFile.Close()
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	if err := unix.Fsync(directoryFD); err != nil {
		_ = unix.Flock(lockFD, unix.LOCK_UN)
		_ = lockFile.Close()
		_ = directoryFile.Close()
		t.Fatal(err)
	}
	parent := newRuntimeParent(path, directoryFile, directory, lockFile, lockID)
	// Tests deliberately replace retained parents and lock names. Production
	// Close must retain descriptors on that uncertainty, so the fixture also
	// owns an independent exact-descriptor safety cleanup after ordinary
	// defers have had their chance to prove the fail-closed result.
	t.Cleanup(func() {
		parent.mu.Lock()
		dir, lock := parent.dir, parent.lock
		parent.dir, parent.lock = nil, nil
		parent.closed = true
		parent.mu.Unlock()
		if lock != nil {
			_ = unix.Flock(int(lock.Fd()), unix.LOCK_UN)
			_ = lock.Close()
		}
		if dir != nil {
			_ = dir.Close()
		}
	})
	return parent
}

func newOperationalRuntimeCapability(t testing.TB) (*install.OperationalHome, string, install.MemberCapability, string) {
	t.Helper()
	homePath := filepath.Join(runtimeTempDir(t), "home")
	if _, err := install.Init(context.Background(), homePath); err != nil {
		t.Fatal(err)
	}
	home, err := install.OpenOperationalHome(context.Background(), homePath)
	if err != nil {
		t.Fatal(err)
	}
	capability, err := home.Runtimes()
	if err != nil {
		_ = home.Close()
		t.Fatal(err)
	}
	return home, homePath, capability, filepath.Join(homePath, "runtimes")
}

func waitForRuntimeParentClosing(t testing.TB, parent *RuntimeParent) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for time.Now().Before(deadline) {
		parent.mu.Lock()
		closing := parent.closing
		parent.mu.Unlock()
		if closing {
			return
		}
		time.Sleep(time.Millisecond)
	}
	t.Fatal("RuntimeParent.Close did not start")
}

func mustPrivatePath(t testing.TB, file PrivateFile) string {
	t.Helper()
	path, err := file.Path()
	if err != nil {
		t.Fatal(err)
	}
	return path
}

func openFDCensus(t testing.TB) int {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func assertFDCensus(t testing.TB, before int) {
	t.Helper()
	after := openFDCensus(t)
	if after != before {
		t.Fatalf("FD census before=%d after=%d", before, after)
	}
}

func createTestRuntimeLifetime(t testing.TB, root string, lock bool) *os.File {
	t.Helper()
	path := filepath.Join(root, runner.RuntimeLifetimeLeaseName)
	file, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	if err := file.Sync(); err != nil {
		file.Close()
		t.Fatal(err)
	}
	if lock {
		if err := unix.Flock(int(file.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
			file.Close()
			t.Fatal(err)
		}
	}
	return file
}

func snapshotRuntimeGraph(t testing.TB, root string) string {
	t.Helper()
	var snapshot bytes.Buffer
	inspect := func(name, path string) os.FileInfo {
		info, err := os.Lstat(path)
		if err != nil {
			t.Fatal(err)
		}
		stat, ok := info.Sys().(*syscall.Stat_t)
		if !ok {
			t.Fatalf("missing stat for %s", path)
		}
		fmt.Fprintf(&snapshot, "%s dev=%d ino=%d mode=%#o uid=%d gid=%d nlink=%d size=%d mtime=%d.%d ctime=%d.%d\n",
			name, stat.Dev, stat.Ino, stat.Mode, stat.Uid, stat.Gid, stat.Nlink, stat.Size,
			stat.Mtimespec.Sec, stat.Mtimespec.Nsec, stat.Ctimespec.Sec, stat.Ctimespec.Nsec)
		return info
	}
	inspect(".", root)
	entries, err := os.ReadDir(root)
	if err != nil {
		t.Fatal(err)
	}
	for _, entry := range entries {
		path := filepath.Join(root, entry.Name())
		info := inspect(entry.Name(), path)
		if info.IsDir() {
			children, err := os.ReadDir(path)
			if err != nil {
				t.Fatal(err)
			}
			for _, child := range children {
				fmt.Fprintf(&snapshot, "%s/%s\n", entry.Name(), child.Name())
			}
		} else if info.Mode().IsRegular() {
			contents, err := os.ReadFile(path)
			if err != nil {
				t.Fatal(err)
			}
			fmt.Fprintf(&snapshot, "%s contents=%x\n", entry.Name(), contents)
		}
	}
	return snapshot.String()
}

func stringsContainsAny(value string, candidates ...string) bool {
	for _, candidate := range candidates {
		if candidate != "" && bytes.Contains([]byte(value), []byte(candidate)) {
			return true
		}
	}
	return false
}
