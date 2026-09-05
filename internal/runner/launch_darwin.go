//go:build darwin

package runner

import (
	"crypto/sha256"
	"debug/macho"
	"encoding/hex"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

func PrepareExecSpec(spec ExecSpec) (*LaunchSpec, error) {
	if len(spec.Stdin) > maxInputBytes {
		return nil, fmt.Errorf("runner: stdin too large")
	}
	if err := validateArgv(append([]string{spec.Target}, spec.Args...), ""); err != nil {
		return nil, err
	}
	if len(spec.Env) > 128 {
		return nil, fmt.Errorf("runner: argv/env too large")
	}
	if err := validateEnvironment(spec.Env); err != nil {
		return nil, err
	}
	target, err := commitExecutable(spec.Target)
	if err != nil {
		return nil, err
	}
	cwd, err := commitDirectory(spec.Cwd)
	if err != nil {
		return nil, err
	}
	var controlID *descriptorCommitment
	if spec.Control != nil {
		committed, err := commitControl(spec.Control)
		if err != nil {
			return nil, fmt.Errorf("runner: control capability: %w", err)
		}
		controlID = &committed
	}
	argv := append([]string{target.Path}, spec.Args...)
	if err := validateArgv(argv, target.Path); err != nil {
		return nil, err
	}
	return &LaunchSpec{commit: launchCommitment{Executable: target, Cwd: cwd, Argv: argv, Env: append([]string{}, spec.Env...)}, stdin: append([]byte{}, spec.Stdin...), stdout: spec.Stdout, stderr: spec.Stderr, control: spec.Control, controlID: controlID}, nil
}

// PrepareCommittedExecSpec prepares the provider-only launch shape without
// reopening and recommitting its executable pathname. The executable was
// already selected and committed by provider construction; the exact argv[0]
// binding prevents a caller from describing a different process identity.
func PrepareCommittedExecSpec(executable ExecutableCommitment, argv, environment []string, cwd string) (*LaunchSpec, error) {
	if err := validateArgv(argv, executable.Path()); err != nil {
		return nil, err
	}
	if len(environment) > 128 {
		return nil, fmt.Errorf("runner: argv/env too large")
	}
	if err := validateEnvironment(environment); err != nil {
		return nil, err
	}
	if err := executable.Verify(); err != nil {
		return nil, err
	}
	directory, err := commitDirectory(cwd)
	if err != nil {
		return nil, err
	}
	return &LaunchSpec{commit: launchCommitment{
		Executable: executable.executable,
		Cwd:        directory,
		Argv:       append([]string(nil), argv...),
		Env:        append([]string(nil), environment...),
	}}, nil
}

func validateArgv(argv []string, executable string) error {
	if len(argv) == 0 || len(argv) > 129 {
		return fmt.Errorf("runner: argv does not name committed executable: %w", ErrIdentity)
	}
	if executable != "" && argv[0] != executable {
		return fmt.Errorf("runner: argv does not name committed executable: %w", ErrIdentity)
	}
	for _, value := range argv {
		if len(value) > MaxArgumentBytes || strings.IndexByte(value, 0) >= 0 || !utf8.ValidString(value) {
			return fmt.Errorf("runner: invalid argv")
		}
	}
	return nil
}

func commitControl(f *os.File) (descriptorCommitment, error) {
	if f == nil {
		return descriptorCommitment{}, ErrIdentity
	}
	var a, b unix.Stat_t
	if err := unix.Fstat(int(f.Fd()), &a); err != nil {
		return descriptorCommitment{}, err
	}
	if a.Mode&unix.S_IFMT != unix.S_IFSOCK || a.Uid != uint32(os.Geteuid()) || a.Dev == 0 || a.Ino == 0 {
		return descriptorCommitment{}, ErrIdentity
	}
	if kind, err := unix.GetsockoptInt(int(f.Fd()), unix.SOL_SOCKET, unix.SO_TYPE); err != nil || kind != unix.SOCK_STREAM {
		return descriptorCommitment{}, ErrIdentity
	}
	if _, err := unix.Getpeername(int(f.Fd())); err != nil {
		return descriptorCommitment{}, ErrIdentity
	}
	if err := unix.Fstat(int(f.Fd()), &b); err != nil {
		return descriptorCommitment{}, err
	}
	if statKey(a) != statKey(b) {
		return descriptorCommitment{}, ErrIdentity
	}
	return descriptorCommitment{FileIdentity: FileIdentity{Device: uint64(a.Dev), Inode: a.Ino}, UID: a.Uid, GID: a.Gid, Mode: uint32(a.Mode)}, nil
}

func verifyControl(f *os.File, want descriptorCommitment) error {
	got, err := commitControl(f)
	if err != nil {
		return err
	}
	if got != want {
		return ErrIdentity
	}
	return nil
}

func allowedEnv(k string) bool {
	switch k {
	case "HOME", "PATH", "LANG", "LC_ALL", "LC_CTYPE", "TMPDIR", "TERM", "SHELL", "USER", "LOGNAME",
		"DARK_FACTORY_SOCKET", "DARK_FACTORY_ATTEMPT_TOKEN_FILE", "DARK_FACTORY_FACTORYCTL",
		"CODEX_HOME", "CLAUDE_CONFIG_DIR",
		"GIT_CEILING_DIRECTORIES", "GIT_DISCOVERY_ACROSS_FILESYSTEM", "GIT_CONFIG_NOSYSTEM", "GIT_CONFIG_GLOBAL",
		"GIT_TERMINAL_PROMPT", "GIT_ASKPASS", "GIT_SSH_COMMAND", "GH_CONFIG_DIR":
		return true
	}
	return false
}

func validateEnvironment(environment []string) error {
	seen := make(map[string]struct{}, len(environment))
	for _, entry := range environment {
		if len(entry) > MaxEnvironmentEntryBytes || strings.IndexByte(entry, 0) >= 0 || !utf8.ValidString(entry) {
			return fmt.Errorf("runner: invalid environment")
		}
		parts := strings.SplitN(entry, "=", 2)
		if len(parts) != 2 || parts[0] == "" || parts[1] == "" {
			return fmt.Errorf("runner: invalid environment")
		}
		name := parts[0]
		if !allowedEnv(name) {
			return fmt.Errorf("runner: environment key %q not allowed", name)
		}
		if _, duplicate := seen[name]; duplicate {
			return fmt.Errorf("runner: duplicate environment key %q", name)
		}
		seen[name] = struct{}{}
	}
	return nil
}

func canonical(path string) (string, error) {
	if path == "" {
		return "", fmt.Errorf("runner: empty path")
	}
	abs, err := filepath.Abs(path)
	if err != nil {
		return "", err
	}
	resolved, err := filepath.EvalSymlinks(abs)
	if err != nil {
		return "", err
	}
	if !filepath.IsAbs(resolved) {
		return "", fmt.Errorf("runner: non-absolute path")
	}
	return resolved, nil
}

func CommitExecutableLocator(path string) (ExecutableCommitment, error) {
	if path == "" || !filepath.IsAbs(path) || filepath.Clean(path) != path || path == string(filepath.Separator) {
		return ExecutableCommitment{}, fmt.Errorf("runner: invalid executable locator: %w", ErrIdentity)
	}
	var named unix.Stat_t
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil || resolved != path || unix.Lstat(path, &named) != nil || named.Mode&unix.S_IFMT != unix.S_IFREG {
		return ExecutableCommitment{}, fmt.Errorf("runner: invalid executable locator: %w", ErrIdentity)
	}
	committed, err := commitExecutable(path)
	if err != nil || committed.Path != path || committed.Mode&0o022 != 0 {
		return ExecutableCommitment{}, fmt.Errorf("runner: invalid executable locator: %w", ErrIdentity)
	}
	return ExecutableCommitment{executable: committed}, nil
}

func (commitment ExecutableCommitment) Verify() error {
	if commitment.executable.Path == "" {
		return fmt.Errorf("runner: executable commitment changed: %w", ErrIdentity)
	}
	file, err := verifyCommit(commitment.executable, true)
	if err == nil {
		err = sameNamedIdentity(commitment.executable.Path, commitment.executable.FileIdentity)
		closeErr := file.Close()
		if err == nil {
			err = closeErr
		}
	}
	if err != nil {
		return fmt.Errorf("runner: executable commitment changed: %w", ErrIdentity)
	}
	return nil
}

func commitExecutable(path string) (fileCommitment, error) {
	path, err := canonical(path)
	if err != nil {
		return fileCommitment{}, err
	}
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return fileCommitment{}, err
	}
	f := os.NewFile(uintptr(fd), path)
	defer f.Close()
	c, err := commitOpen(f, path, true)
	if err != nil {
		return fileCommitment{}, err
	}
	if err := validateMachO(f); err != nil {
		return fileCommitment{}, err
	}
	return c, nil
}

func commitDirectory(path string) (fileCommitment, error) {
	path, err := canonical(path)
	if err != nil {
		return fileCommitment{}, err
	}
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return fileCommitment{}, err
	}
	f := os.NewFile(uintptr(fd), path)
	defer f.Close()
	return commitOpen(f, path, false)
}

func commitOpen(f *os.File, path string, executable bool) (fileCommitment, error) {
	var a, b unix.Stat_t
	if err := unix.Fstat(int(f.Fd()), &a); err != nil {
		return fileCommitment{}, err
	}
	if executable {
		if a.Mode&unix.S_IFMT != unix.S_IFREG || a.Mode&0o111 == 0 || a.Mode&(unix.S_ISUID|unix.S_ISGID) != 0 || a.Size < 1 || a.Size > maxTargetBytes {
			return fileCommitment{}, fmt.Errorf("runner: unsafe executable")
		}
	} else if a.Mode&unix.S_IFMT != unix.S_IFDIR {
		return fileCommitment{}, fmt.Errorf("runner: cwd is not directory")
	}
	h := sha256.New()
	if executable {
		if _, err := io.Copy(h, io.LimitReader(f, maxTargetBytes+1)); err != nil {
			return fileCommitment{}, err
		}
	}
	if err := unix.Fstat(int(f.Fd()), &b); err != nil {
		return fileCommitment{}, err
	}
	if statKey(a) != statKey(b) {
		return fileCommitment{}, ErrIdentity
	}
	return fileCommitment{Path: path, FileIdentity: FileIdentity{Device: uint64(a.Dev), Inode: a.Ino}, UID: a.Uid, GID: a.Gid, Mode: uint32(a.Mode), Size: a.Size, MtimeSec: a.Mtim.Sec, MtimeNsec: a.Mtim.Nsec, CtimeSec: a.Ctim.Sec, CtimeNsec: a.Ctim.Nsec, SHA256: hex.EncodeToString(h.Sum(nil))}, nil
}

func statKey(s unix.Stat_t) string {
	return fmt.Sprintf("%d:%d:%d:%d:%d:%d:%d:%d:%d", s.Dev, s.Ino, s.Uid, s.Gid, s.Mode, s.Size, s.Mtim.Sec, s.Mtim.Nsec, s.Ctim.Nsec)
}

func validateMachO(f *os.File) error {
	if _, err := f.Seek(0, 0); err != nil {
		return err
	}
	want := macho.CpuArm64
	if runtime.GOARCH == "amd64" {
		want = macho.CpuAmd64
	}
	if fat, err := macho.NewFatFile(f); err == nil {
		defer fat.Close()
		for _, a := range fat.Arches {
			if a.Cpu == want {
				return nil
			}
		}
		return fmt.Errorf("runner: no native Mach-O slice")
	}
	if _, err := f.Seek(0, 0); err != nil {
		return err
	}
	m, err := macho.NewFile(f)
	if err != nil {
		return fmt.Errorf("runner: target is not Mach-O: %w", err)
	}
	defer m.Close()
	if m.Cpu != want {
		return fmt.Errorf("runner: wrong Mach-O architecture")
	}
	return nil
}

func verifyCommit(c fileCommitment, executable bool) (*os.File, error) {
	flags := unix.O_RDONLY | unix.O_CLOEXEC | unix.O_NOFOLLOW
	if !executable {
		flags |= unix.O_DIRECTORY
	}
	fd, err := unix.Open(c.Path, flags, 0)
	if err != nil {
		return nil, err
	}
	f := os.NewFile(uintptr(fd), c.Path)
	got, err := commitOpen(f, c.Path, executable)
	if err != nil {
		f.Close()
		return nil, err
	}
	if got != c {
		f.Close()
		return nil, ErrIdentity
	}
	if executable {
		if err := validateMachO(f); err != nil {
			f.Close()
			return nil, err
		}
	}
	return f, nil
}

func statIdentity(f *os.File) (FileIdentity, error) {
	var s unix.Stat_t
	if err := unix.Fstat(int(f.Fd()), &s); err != nil {
		return FileIdentity{}, err
	}
	return FileIdentity{Device: uint64(s.Dev), Inode: s.Ino}, nil
}

func sameNamedIdentity(path string, want FileIdentity) error {
	var s unix.Stat_t
	if err := unix.Lstat(path, &s); err != nil {
		return err
	}
	if s.Mode&unix.S_IFMT != unix.S_IFREG || uint64(s.Dev) != want.Device || s.Ino != want.Inode {
		return ErrIdentity
	}
	return nil
}
