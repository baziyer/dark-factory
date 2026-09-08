//go:build darwin

package daemon

import (
	"context"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sync"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

const (
	runtimeParentLockName = ".runtime.lock"
)

var errRuntimeBusy = errors.New("daemon: runtime ownership busy")

// RuntimeParent is the one concrete managed-runtime namespace capability. It
// retains the exact descriptor derived from install.MemberCapability and one
// lifetime lock. Its locator is diagnostic only and is never reopened.
type RuntimeParent struct {
	mu        sync.Mutex
	cond      *sync.Cond
	locator   string
	dir       *os.File
	directory directoryIdentity
	lock      *os.File
	lockID    runner.FileIdentity
	operation bool
	children  uint64
	closing   bool
	closed    bool
	closeErr  error
	closeDir  func(*os.File) error
	unlock    func(int) error
	closeLock func(*os.File) error
}

// OpenRuntimeParent consumes the retained operational-home member capability.
// A missing lock is initialized only when the parent is otherwise empty.
func OpenRuntimeParent(ctx context.Context, capability install.MemberCapability, diagnosticLocator string) (_ *RuntimeParent, resultErr error) {
	return openRuntimeParent(ctx, capability, diagnosticLocator, nil)
}

func openRuntimeParent(ctx context.Context, capability install.MemberCapability, diagnosticLocator string, beforeFlock func(bool)) (_ *RuntimeParent, resultErr error) {
	if ctx == nil || !filepath.IsAbs(diagnosticLocator) || filepath.Clean(diagnosticLocator) != diagnosticLocator {
		return nil, invalidContract(nil)
	}
	if err := ctx.Err(); err != nil {
		return nil, invalidContract(err)
	}

	// This is intentionally the first authority-bearing filesystem operation.
	dir, err := capability.Open()
	if err != nil {
		return nil, invalidContract(err)
	}
	keepDir := false
	defer func() {
		if !keepDir {
			resultErr = errors.Join(resultErr, dir.Close())
		}
	}()
	directory, err := inspectPrivateDirectory(int(dir.Fd()))
	if err != nil {
		return nil, invalidContract(err)
	}
	if err := verifyNamedDirectory(diagnosticLocator, directory); err != nil {
		return nil, invalidContract(err)
	}
	if err := ctx.Err(); err != nil {
		return nil, invalidContract(err)
	}

	lock, lockID, created, err := openRuntimeParentLock(ctx, int(dir.Fd()), beforeFlock)
	if err != nil {
		return nil, err
	}
	keepLock := false
	cleanupCreated := created
	defer func() {
		if cleanupCreated {
			if cleanupErr := cleanupCreatedRuntimeLock(int(dir.Fd()), lockID); cleanupErr != nil {
				resultErr = retainedContract(errors.Join(resultErr, cleanupErr))
			}
		}
		if !keepLock {
			resultErr = errors.Join(resultErr, unix.Flock(int(lock.Fd()), unix.LOCK_UN), lock.Close())
		}
	}()
	if created {
		if err := requireOnlyRuntimeLock(int(dir.Fd()), lockID); err != nil {
			return nil, invalidContract(err)
		}
	}
	// An opener may win the flock on a lock another racing opener just created.
	// Every winner therefore completes both durability barriers.
	if err := unix.Fsync(int(lock.Fd())); err != nil {
		return nil, invalidContract(err)
	}
	if err := unix.Fsync(int(dir.Fd())); err != nil {
		return nil, invalidContract(err)
	}
	if err := verifyRuntimeParentLock(int(dir.Fd()), int(lock.Fd()), lockID); err != nil {
		return nil, invalidContract(err)
	}
	if err := ctx.Err(); err != nil {
		return nil, invalidContract(err)
	}

	parent := newRuntimeParent(diagnosticLocator, dir, directory, lock, lockID)
	cleanupCreated = false
	keepLock = true
	keepDir = true
	return parent, nil
}

func newRuntimeParent(diagnosticLocator string, dir *os.File, directory directoryIdentity, lock *os.File, lockID runner.FileIdentity) *RuntimeParent {
	parent := &RuntimeParent{
		locator:   diagnosticLocator,
		dir:       dir,
		directory: directory,
		lock:      lock,
		lockID:    lockID,
		closeDir:  func(file *os.File) error { return file.Close() },
		unlock:    func(fd int) error { return unix.Flock(fd, unix.LOCK_UN) },
		closeLock: func(file *os.File) error { return file.Close() },
	}
	parent.cond = sync.NewCond(&parent.mu)
	return parent
}

func openRuntimeParentLock(ctx context.Context, parentFD int, beforeFlock func(bool)) (*os.File, runner.FileIdentity, bool, error) {
	for {
		if err := ctx.Err(); err != nil {
			return nil, runner.FileIdentity{}, false, invalidContract(err)
		}
		fd, err := unix.Openat(parentFD, runtimeParentLockName, unix.O_RDWR|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0)
		created := false
		if errors.Is(err, unix.ENOENT) {
			empty, censusErr := runtimeParentEmpty(parentFD)
			if censusErr != nil || !empty {
				return nil, runner.FileIdentity{}, false, invalidContract(censusErr)
			}
			fd, err = unix.Openat(parentFD, runtimeParentLockName, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC|unix.O_NONBLOCK, 0o600)
			if errors.Is(err, unix.EEXIST) {
				continue
			}
			created = err == nil
		}
		if err != nil {
			return nil, runner.FileIdentity{}, false, invalidContract(err)
		}
		lock := os.NewFile(uintptr(fd), "runtime-parent-lock")
		lockID, inspectErr := inspectRuntimeLock(fd)
		if inspectErr != nil {
			closeErr := lock.Close()
			if created {
				return nil, runner.FileIdentity{}, false, retainedContract(errors.Join(inspectErr, closeErr))
			}
			return nil, runner.FileIdentity{}, false, invalidContract(errors.Join(inspectErr, closeErr))
		}
		if beforeFlock != nil {
			beforeFlock(created)
		}
		if err := unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB); err != nil {
			closeErr := lock.Close()
			// A creator that loses this election must not unlink the lock: the
			// winning opener now owns that exact inode.
			if errors.Is(err, unix.EWOULDBLOCK) {
				return nil, runner.FileIdentity{}, false, errors.Join(errRuntimeBusy, closeErr)
			}
			return nil, runner.FileIdentity{}, false, invalidContract(errors.Join(err, closeErr))
		}
		return lock, lockID, created, nil
	}
}

func runtimeParentEmpty(parentFD int) (bool, error) {
	entries, err := readRuntimeParentNames(parentFD, 1)
	if err != nil && !errors.Is(err, io.EOF) {
		return false, err
	}
	return len(entries) == 0, nil
}

func requireOnlyRuntimeLock(parentFD int, expected runner.FileIdentity) error {
	entries, err := readRuntimeParentNames(parentFD, 2)
	if err != nil && !errors.Is(err, io.EOF) {
		return err
	}
	if len(entries) != 1 || entries[0] != runtimeParentLockName {
		return errInvalidContract
	}
	return verifyNamedRuntimeLock(parentFD, expected)
}

func readRuntimeParentNames(parentFD, limit int) ([]string, error) {
	fd, err := unix.Openat(parentFD, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, err
	}
	directory := os.NewFile(uintptr(fd), "runtime-parent-census")
	entries, readErr := directory.Readdirnames(limit)
	return entries, errors.Join(readErr, directory.Close())
}

func inspectRuntimeLock(fd int) (runner.FileIdentity, error) {
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		return runner.FileIdentity{}, invalidContract(err)
	}
	if !validRuntimeLock(stat) {
		return runner.FileIdentity{}, invalidContract(nil)
	}
	return runner.FileIdentity{Device: uint64(stat.Dev), Inode: stat.Ino}, nil
}

func validRuntimeLock(stat unix.Stat_t) bool {
	return stat.Mode&unix.S_IFMT == unix.S_IFREG && stat.Mode&0o7777 == 0o600 && stat.Uid == uint32(os.Geteuid()) && stat.Nlink == 1 && stat.Size == 0 && stat.Dev != 0 && stat.Ino != 0
}

func verifyNamedRuntimeLock(parentFD int, expected runner.FileIdentity) error {
	var stat unix.Stat_t
	if err := unix.Fstatat(parentFD, runtimeParentLockName, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil || !validRuntimeLock(stat) || uint64(stat.Dev) != expected.Device || stat.Ino != expected.Inode {
		return errors.Join(errInvalidContract, err)
	}
	return nil
}

func verifyRuntimeParentLock(parentFD, lockFD int, expected runner.FileIdentity) error {
	actual, err := inspectRuntimeLock(lockFD)
	if err != nil || actual != expected {
		return errors.Join(errInvalidContract, err)
	}
	return verifyNamedRuntimeLock(parentFD, expected)
}

func cleanupCreatedRuntimeLock(parentFD int, expected runner.FileIdentity) error {
	if err := verifyNamedRuntimeLock(parentFD, expected); err != nil {
		return err
	}
	if err := unix.Unlinkat(parentFD, runtimeParentLockName, 0); err != nil {
		return err
	}
	return unix.Fsync(parentFD)
}

type runtimeParentOperation struct {
	mu     sync.Mutex
	parent *RuntimeParent
	closed bool
}

// runtimeParentChild is the lifetime reference transferred to one live
// Runtime. It retains the parent without holding the short namespace-operation
// gate, so distinct admitted runtimes may coexist.
type runtimeParentChild struct {
	mu     sync.Mutex
	parent *RuntimeParent
	closed bool
}

func (parent *RuntimeParent) begin() (*runtimeParentOperation, error) {
	if parent == nil {
		return nil, invalidContract(nil)
	}
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if parent.closing || parent.closed || parent.dir == nil || parent.lock == nil {
		return nil, invalidContract(parent.closeErr)
	}
	for parent.operation && !parent.closing {
		parent.cond.Wait()
	}
	if parent.closing || parent.closed || parent.dir == nil || parent.lock == nil {
		return nil, invalidContract(parent.closeErr)
	}
	if err := parent.verifyRetained(); err != nil {
		return nil, invalidContract(err)
	}
	parent.operation = true
	return &runtimeParentOperation{parent: parent}, nil
}

func (parent *RuntimeParent) verifyRetained() error {
	actual, err := inspectPrivateDirectory(int(parent.dir.Fd()))
	if err != nil || actual != parent.directory {
		return errors.Join(errInvalidContract, err)
	}
	return verifyRuntimeParentLock(int(parent.dir.Fd()), int(parent.lock.Fd()), parent.lockID)
}

func (operation *runtimeParentOperation) directory() (*os.File, error) {
	if operation == nil || operation.parent == nil {
		return nil, invalidContract(nil)
	}
	operation.mu.Lock()
	defer operation.mu.Unlock()
	if operation.closed {
		return nil, invalidContract(nil)
	}
	parent := operation.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if !parent.operation || parent.closed || parent.dir == nil || parent.lock == nil {
		return nil, invalidContract(parent.closeErr)
	}
	if err := parent.verifyRetained(); err != nil {
		return nil, invalidContract(err)
	}
	return parent.dir, nil
}

func (operation *runtimeParentOperation) locator(name string) (string, error) {
	if operation == nil || operation.parent == nil || !validRuntimeName(name) {
		return "", invalidContract(nil)
	}
	operation.mu.Lock()
	defer operation.mu.Unlock()
	if operation.closed {
		return "", invalidContract(nil)
	}
	parent := operation.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if !parent.operation || parent.closed || parent.dir == nil || parent.lock == nil {
		return "", invalidContract(parent.closeErr)
	}
	if err := parent.verifyDiagnosticBinding(); err != nil {
		return "", invalidContract(err)
	}
	return filepath.Join(parent.locator, name), nil
}

func (operation *runtimeParentOperation) Close() error {
	if operation == nil || operation.parent == nil {
		return nil
	}
	operation.mu.Lock()
	defer operation.mu.Unlock()
	if operation.closed {
		return nil
	}
	parent := operation.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if !parent.operation {
		return invalidContract(nil)
	}
	operation.closed = true
	parent.operation = false
	parent.cond.Broadcast()
	return nil
}

func (operation *runtimeParentOperation) transfer() (*runtimeParentChild, error) {
	if operation == nil || operation.parent == nil {
		return nil, invalidContract(nil)
	}
	operation.mu.Lock()
	defer operation.mu.Unlock()
	if operation.closed {
		return nil, invalidContract(nil)
	}
	parent := operation.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if !parent.operation || parent.closed || parent.dir == nil || parent.lock == nil {
		return nil, invalidContract(parent.closeErr)
	}
	if err := parent.verifyRetained(); err != nil {
		return nil, invalidContract(err)
	}
	operation.closed = true
	parent.operation = false
	parent.children++
	parent.cond.Broadcast()
	return &runtimeParentChild{parent: parent}, nil
}

func (child *runtimeParentChild) directory() (*os.File, error) {
	if child == nil || child.parent == nil {
		return nil, invalidContract(nil)
	}
	child.mu.Lock()
	defer child.mu.Unlock()
	if child.closed {
		return nil, invalidContract(nil)
	}
	parent := child.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if parent.closed || parent.dir == nil || parent.lock == nil || parent.children == 0 {
		return nil, invalidContract(parent.closeErr)
	}
	if err := parent.verifyRetained(); err != nil {
		return nil, invalidContract(err)
	}
	return parent.dir, nil
}

func (child *runtimeParentChild) Close() error {
	if child == nil || child.parent == nil {
		return nil
	}
	child.mu.Lock()
	defer child.mu.Unlock()
	if child.closed {
		return nil
	}
	parent := child.parent
	parent.mu.Lock()
	defer parent.mu.Unlock()
	if parent.children == 0 {
		return invalidContract(nil)
	}
	child.closed = true
	parent.children--
	parent.cond.Broadcast()
	return nil
}

func (parent *RuntimeParent) verifyDiagnosticBinding() error {
	if parent == nil || parent.locator == "" {
		return errInvalidContract
	}
	return verifyNamedDirectory(parent.locator, parent.directory)
}

// runtimeLocator derives only the diagnostic value used in admission and
// persisted resource records. Runtime filesystem effects must use begin.
func (parent *RuntimeParent) runtimeLocator(name string) (string, error) {
	if parent == nil || !validRuntimeName(name) {
		return "", invalidContract(nil)
	}
	operation, err := parent.begin()
	if err != nil {
		return "", err
	}
	defer operation.Close()
	return operation.locator(name)
}

func (parent *RuntimeParent) Close() error {
	if parent == nil {
		return nil
	}
	parent.mu.Lock()
	if parent.cond == nil {
		parent.mu.Unlock()
		return invalidContract(nil)
	}
	for parent.closing && !parent.closed {
		parent.cond.Wait()
	}
	if parent.closed {
		err := parent.closeErr
		parent.mu.Unlock()
		return err
	}
	parent.closing = true
	for parent.operation || parent.children != 0 {
		parent.cond.Wait()
	}
	verifyErr := parent.verifyRetained()
	lock, dir := parent.lock, parent.dir
	closeDir, unlock, closeLock := parent.closeDir, parent.unlock, parent.closeLock
	parent.mu.Unlock()

	var closeErr error
	if verifyErr != nil {
		closeErr = retainedContract(verifyErr)
	} else if closeDir == nil || unlock == nil || closeLock == nil {
		closeErr = retainedContract(errInvalidContract)
	} else if err := closeDir(dir); err != nil {
		closeErr = retainedContract(err)
	} else if err := unlock(int(lock.Fd())); err != nil {
		closeErr = retainedContract(err)
	} else if err := closeLock(lock); err != nil {
		closeErr = retainedContract(err)
	}
	parent.mu.Lock()
	if closeErr == nil {
		parent.lock, parent.dir = nil, nil
	} else if dir != nil && dir.Fd() == ^uintptr(0) {
		// A successful directory close is remembered even if later lock
		// release/close was uncertain. The lifetime lock remains the retained
		// cooperating-daemon authority until OperationalHome also refuses to
		// release its own lease.
		parent.dir = nil
	}
	parent.closeErr = closeErr
	parent.closed = true
	parent.closing = false
	parent.cond.Broadcast()
	parent.mu.Unlock()
	return closeErr
}

func validRuntimeName(name string) bool {
	if len(name) != 32 {
		return false
	}
	for _, char := range []byte(name) {
		if !('0' <= char && char <= '9' || 'a' <= char && char <= 'f') {
			return false
		}
	}
	return name != runtimeParentLockName && name != install.LocalAPISocketName
}
