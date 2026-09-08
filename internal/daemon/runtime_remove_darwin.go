//go:build darwin

package daemon

import (
	"context"
	"errors"
	"io"
	"os"
	"sort"

	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

const runtimeRemovalEffectLimit = 256
const runtimeRemovalDepthLimit = 32
const runtimeTopEntryLimit = 12 // home, tmp, terminal, lifetime, and eight removable fixed files.

// RemoveRecordedRuntime removes only a positively identified, inactive
// runtime using the fixed V1 grammar. false,nil is bounded progress or lock
// contention. Any uncertain authority or unexpected entry is an error.
func RemoveRecordedRuntime(ctx context.Context, parent *RuntimeParent, basename string, expected runner.FileIdentity) (bool, error) {
	return removeRecordedRuntime(ctx, parent, basename, expected, runtimeRemovalEffectLimit, unix.Fsync)
}

func removeRecordedRuntime(ctx context.Context, parent *RuntimeParent, basename string, expected runner.FileIdentity, limit int, syncDirectory func(int) error) (bool, error) {
	return removeRecordedRuntimeWithHook(ctx, parent, basename, expected, limit, syncDirectory, nil)
}

func removeRecordedRuntimeWithHook(ctx context.Context, parent *RuntimeParent, basename string, expected runner.FileIdentity, limit int, syncDirectory func(int) error, afterParentSync func()) (bool, error) {
	if ctx == nil || parent == nil || !validRuntimeName(basename) || expected.Device == 0 || expected.Inode == 0 || limit <= 0 {
		return false, invalidContract(nil)
	}
	if err := ctx.Err(); err != nil {
		return false, invalidContract(err)
	}
	if syncDirectory == nil {
		syncDirectory = unix.Fsync
	}
	operation, err := parent.begin()
	if errors.Is(err, errRuntimeBusy) {
		return false, nil
	}
	if err != nil {
		return false, err
	}
	defer operation.Close()
	ownedParent, err := operation.directory()
	if err != nil {
		return false, err
	}
	parentFD := int(ownedParent.Fd())
	var named unix.Stat_t
	if err := unix.Fstatat(parentFD, basename, &named, unix.AT_SYMLINK_NOFOLLOW); errors.Is(err, unix.ENOENT) {
		if err := syncDirectory(parentFD); err != nil {
			return false, invalidContract(err)
		}
		if err := unix.Fstatat(parentFD, basename, &named, unix.AT_SYMLINK_NOFOLLOW); !errors.Is(err, unix.ENOENT) {
			return false, invalidContract(err)
		}
		return true, nil
	} else if err != nil {
		return false, invalidContract(err)
	}
	if uint64(named.Dev) != expected.Device || named.Ino != expected.Inode {
		return false, invalidContract(nil)
	}
	fd, err := unix.Openat(parentFD, basename, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return false, invalidContract(err)
	}
	root := os.NewFile(uintptr(fd), "recorded-runtime-removal")
	defer root.Close()
	rootIdentity, err := inspectExpectedDirectory(fd, expected)
	if err != nil || verifyNamedChild(parentFD, basename, rootIdentity) != nil {
		return false, invalidContract(err)
	}
	if err := verifyNamedChild(parentFD, basename, rootIdentity); err != nil {
		return false, invalidContract(err)
	}
	lifetime, lifetimeIdentity, err := openRuntimeLifetime(fd, rootIdentity.device, nil)
	if errors.Is(err, errRuntimeBusy) {
		return false, nil
	}
	if err != nil {
		// A prior cleanup may have durably removed the lifetime immediately
		// before a failed root removal. Only an otherwise empty exact root is a
		// safe continuation of that crash cut.
		if !errors.Is(err, errInvalidContract) || !runtimeRootEmpty(fd) {
			return false, err
		}
		if err := syncDirectory(fd); err != nil {
			return false, invalidContract(err)
		}
		if err := unix.Unlinkat(parentFD, basename, unix.AT_REMOVEDIR); err != nil {
			return false, invalidContract(err)
		}
		if err := syncDirectory(parentFD); err != nil {
			return false, invalidContract(err)
		}
		if err := unix.Fstatat(parentFD, basename, &named, unix.AT_SYMLINK_NOFOLLOW); !errors.Is(err, unix.ENOENT) {
			return false, invalidContract(err)
		}
		return true, nil
	}
	defer lifetime.Close()
	entries, more, err := readRuntimeEntries(fd, runtimeTopEntryLimit)
	if err != nil || more {
		return false, invalidContract(err)
	}
	for _, name := range entries {
		if name == runner.TerminalSpoolName {
			return false, invalidContract(nil)
		}
		if name != runtimeHomeName && name != runtimeTempName {
			if !isRemovableRuntimeFile(name) {
				return false, invalidContract(nil)
			}
		}
	}
	budget := limit
	for _, name := range []string{runtimeHomeName, runtimeTempName} {
		done, err := removeRuntimeTree(ctx, fd, name, rootIdentity.device, &budget, syncDirectory, true, 0)
		if err != nil {
			return false, invalidContract(err)
		}
		if !done {
			return false, nil
		}
	}
	for _, name := range entries {
		if name == runtimeHomeName || name == runtimeTempName || name == runner.RuntimeLifetimeLeaseName {
			continue
		}
		if err := ctx.Err(); err != nil {
			return false, invalidContract(err)
		}
		if budget == 0 {
			return false, nil
		}
		var stat unix.Stat_t
		if err := unix.Fstatat(fd, name, &stat, unix.AT_SYMLINK_NOFOLLOW); errors.Is(err, unix.ENOENT) {
			continue
		} else if err != nil || !validRuntimeOrdinaryFile(stat, rootIdentity.device, true) {
			return false, invalidContract(err)
		}
		if err := unix.Unlinkat(fd, name, 0); err != nil {
			return false, invalidContract(err)
		}
		budget--
		if err := syncDirectory(fd); err != nil {
			return false, invalidContract(err)
		}
	}
	if err := ctx.Err(); err != nil {
		return false, invalidContract(err)
	}
	if err := verifyNamedChild(parentFD, basename, rootIdentity); err != nil {
		return false, invalidContract(err)
	}
	var namedLifetime unix.Stat_t
	if err := unix.Fstatat(fd, runner.RuntimeLifetimeLeaseName, &namedLifetime, unix.AT_SYMLINK_NOFOLLOW); err != nil || !validRuntimeLifetime(namedLifetime, rootIdentity.device) || (runner.FileIdentity{Device: uint64(namedLifetime.Dev), Inode: namedLifetime.Ino}) != lifetimeIdentity {
		return false, invalidContract(err)
	}
	if err := unix.Unlinkat(fd, runner.RuntimeLifetimeLeaseName, 0); err != nil {
		return false, invalidContract(err)
	}
	if err := syncDirectory(fd); err != nil {
		return false, invalidContract(err)
	}
	remaining, more, err := readRuntimeEntries(fd, runtimeTopEntryLimit)
	if err != nil || more || len(remaining) != 0 {
		return false, invalidContract(err)
	}
	if err := syncDirectory(fd); err != nil {
		return false, invalidContract(err)
	}
	if err := unix.Unlinkat(parentFD, basename, unix.AT_REMOVEDIR); err != nil {
		return false, invalidContract(err)
	}
	if err := syncDirectory(parentFD); err != nil {
		return false, invalidContract(err)
	}
	if afterParentSync != nil {
		afterParentSync()
	}
	if err := unix.Fstatat(parentFD, basename, &named, unix.AT_SYMLINK_NOFOLLOW); !errors.Is(err, unix.ENOENT) {
		return false, invalidContract(err)
	}
	return true, nil
}

func isRemovableRuntimeFile(name string) bool {
	switch name {
	case attemptTokenName,
		runner.OuterActivationMarkerName, runner.InnerActivationMarkerName,
		runner.GateConfigScratchName, runner.GateStdinScratchName,
		runner.TerminalScratchName,
		runner.RuntimeLifetimeLeaseName:
		return true
	default:
		return false
	}
}

func runtimeRootEmpty(fd int) bool {
	entries, more, err := readRuntimeEntries(fd, 1)
	return err == nil && !more && len(entries) == 0
}

func removeRuntimeTree(ctx context.Context, parentFD int, name string, device uint64, budget *int, syncDirectory func(int) error, exactLayout bool, depth int) (bool, error) {
	if err := ctx.Err(); err != nil {
		return false, err
	}
	if depth > runtimeRemovalDepthLimit {
		return false, errInvalidContract
	}
	var named unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &named, unix.AT_SYMLINK_NOFOLLOW); errors.Is(err, unix.ENOENT) {
		return true, nil
	} else if err != nil || !validRuntimeOrdinaryDirectory(named, device, exactLayout) {
		return false, errInvalidContract
	}
	fd, err := unix.Openat(parentFD, name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return false, err
	}
	directory := os.NewFile(uintptr(fd), "runtime-tree-removal")
	defer directory.Close()
	if *budget == 0 {
		return false, nil
	}
	entries, more, err := readRuntimeEntries(fd, *budget)
	if err != nil {
		return false, err
	}
	mutated := false
	for _, child := range entries {
		if err := ctx.Err(); err != nil {
			return false, err
		}
		var stat unix.Stat_t
		if err := unix.Fstatat(fd, child, &stat, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return false, err
		}
		switch stat.Mode & unix.S_IFMT {
		case unix.S_IFDIR:
			done, err := removeRuntimeTree(ctx, fd, child, device, budget, syncDirectory, false, depth+1)
			if err != nil || !done {
				return false, err
			}
			mutated = true
		case unix.S_IFREG, unix.S_IFLNK, unix.S_IFSOCK, unix.S_IFIFO:
			// A provider's home and temp directory hold whatever it made: a
			// module cache of hard links, a socket a test listened on, a
			// symlink into node_modules. Unlinking a name never follows it
			// and never touches what another name shares, so every name on
			// this device owned by this user goes; a name on another device
			// or another owner is not this runtime's to remove.
			if !validRuntimeOrdinaryName(stat, device) {
				return false, errInvalidContract
			}
			if *budget == 0 {
				return false, nil
			}
			if err := unix.Unlinkat(fd, child, 0); err != nil {
				return false, err
			}
			*budget--
			mutated = true
		default:
			return false, errInvalidContract
		}
	}
	if more {
		if mutated {
			if err := syncDirectory(fd); err != nil {
				return false, err
			}
		}
		return false, nil
	}
	if mutated {
		if err := syncDirectory(fd); err != nil {
			return false, err
		}
	}
	if *budget == 0 {
		return false, nil
	}
	var current unix.Stat_t
	if err := unix.Fstatat(parentFD, name, &current, unix.AT_SYMLINK_NOFOLLOW); err != nil || !sameFileObject(named, current) || !validRuntimeOrdinaryDirectory(current, device, exactLayout) {
		return false, errInvalidContract
	}
	if err := unix.Unlinkat(parentFD, name, unix.AT_REMOVEDIR); err != nil {
		return false, err
	}
	*budget--
	if err := syncDirectory(parentFD); err != nil {
		return false, err
	}
	return true, nil
}

func readRuntimeEntries(fd int, limit int) ([]string, bool, error) {
	if limit <= 0 {
		return nil, false, errInvalidContract
	}
	duplicate, err := unix.Openat(fd, ".", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
	if err != nil {
		return nil, false, err
	}
	directory := os.NewFile(uintptr(duplicate), "runtime-census")
	entries, readErr := directory.ReadDir(limit + 1)
	closeErr := directory.Close()
	if readErr != nil && !errors.Is(readErr, io.EOF) || closeErr != nil {
		return nil, false, errors.Join(readErr, closeErr)
	}
	more := len(entries) > limit
	if more {
		entries = entries[:limit]
	}
	names := make([]string, len(entries))
	for index, entry := range entries {
		names[index] = entry.Name()
	}
	sort.Strings(names)
	return names, more, nil
}

func validRuntimeOrdinaryDirectory(stat unix.Stat_t, device uint64, exactMode bool) bool {
	if stat.Mode&unix.S_IFMT != unix.S_IFDIR || uint64(stat.Dev) != device || stat.Uid != uint32(os.Geteuid()) || stat.Nlink == 0 || stat.Mode&(unix.S_ISUID|unix.S_ISGID|unix.S_ISVTX) != 0 {
		return false
	}
	return !exactMode || stat.Mode&0o7777 == 0o700
}

// validRuntimeOrdinaryName is the bound on a non-directory name inside a
// runtime's home or temp tree: on the runtime's device, owned by this user,
// with no special bits. Its kind and link count are the provider's business.
func validRuntimeOrdinaryName(stat unix.Stat_t, device uint64) bool {
	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFREG, unix.S_IFLNK, unix.S_IFSOCK, unix.S_IFIFO:
	default:
		return false
	}
	return uint64(stat.Dev) == device && stat.Uid == uint32(os.Geteuid()) && stat.Mode&(unix.S_ISUID|unix.S_ISGID|unix.S_ISVTX) == 0
}

func validRuntimeOrdinaryFile(stat unix.Stat_t, device uint64, exactMode bool) bool {
	if stat.Mode&unix.S_IFMT != unix.S_IFREG || uint64(stat.Dev) != device || stat.Uid != uint32(os.Geteuid()) || stat.Nlink != 1 || stat.Mode&(unix.S_ISUID|unix.S_ISGID|unix.S_ISVTX) != 0 {
		return false
	}
	return !exactMode || stat.Mode&0o7777 == 0o600
}
