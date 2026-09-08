//go:build darwin

package runner

import (
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"go/ast"
	"go/parser"
	"go/token"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestMain(m *testing.M) {
	if os.Getenv("RUNNER_TEST_OWNER") == "1" {
		if err := runParentDeathOwner(); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(98)
		}
		os.Exit(0)
	}
	if len(os.Args) == 2 && os.Args[1] == "--exec-gate" {
		if err := RunExecGate(); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(70)
		}
		os.Exit(0)
	}
	if len(os.Args) == 2 && os.Args[1] == "--attempt-runner" {
		if err := RunAttemptRunner(); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(70)
		}
		os.Exit(0)
	}
	if len(os.Args) >= 2 && os.Args[1] == "--attempt-worker" {
		if err := runAttemptWorkerHelper(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(98)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--attempt-provider" {
		if err := os.WriteFile(os.Args[2], []byte("provider"), 0o600); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(97)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--lifetime-provider" {
		if err := runLifetimeProviderHelper(os.Args[2]); err != nil {
			_ = os.WriteFile(filepath.Join(os.Args[2], "provider.error"), []byte(err.Error()), 0o600)
			fmt.Fprintln(os.Stderr, err)
			os.Exit(95)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--proof-provider" {
		if err := runProofProviderHelper(os.Args[2]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(95)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--cwd-provider" {
		if err := runCwdProviderHelper(os.Args[2]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(94)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--pty-provider" {
		if err := runPTYProviderHelper(os.Args[2]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(93)
		}
		os.Exit(0)
	}
	if len(os.Args) == 3 && os.Args[1] == "--pty-exit-pressure" {
		if err := runPTYExitPressureHelper(os.Args[2]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(93)
		}
		os.Exit(0)
	}
	if len(os.Args) == 5 && os.Args[1] == "--attempt-retirement-provider" {
		if err := runRetirementProviderHelper(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(96)
		}
		os.Exit(0)
	}
	if len(os.Args) == 4 && os.Args[1] == "--owned-descendant-helper" {
		switch os.Args[2] {
		case "leader":
			if err := runLeaderExitDescendantHelper(os.Args[3]); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(92)
			}
			os.Exit(23)
		case "term-fork":
			if err := runTERMForkDescendantHelper(os.Args[3]); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(92)
			}
			os.Exit(0)
		case "fifo-child":
			if err := runFIFODescendantHelper(os.Args[3]); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(91)
			}
			os.Exit(0)
		case "paused-child":
			term := make(chan os.Signal, 1)
			signal.Notify(term, unix.SIGTERM)
			if err := signalHelperReady(); err != nil {
				fmt.Fprintln(os.Stderr, err)
				os.Exit(91)
			}
			for range term {
			}
			os.Exit(91)
		}
	}
	beforeFD := fdCensus()
	code := m.Run()
	afterFD := fdCensus()
	if code == 0 && !sameCensus(beforeFD, afterFD) {
		fmt.Fprintf(os.Stderr, "runner FD leak before=%v after=%v\n", beforeFD, afterFD)
		code = 1
	}
	os.Exit(code)
}

func runProofProviderHelper(root string) error {
	proof := testResultProof()
	needles := [][]byte{proof.value[:], []byte(testResultProofHex())}
	for _, value := range append(append([]string{}, os.Args...), os.Environ()...) {
		for _, needle := range needles {
			if bytes.Contains([]byte(value), needle) {
				return errors.New("result proof leaked through provider argv or environment")
			}
		}
	}
	for fd := 3; fd < 256; fd++ {
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil {
			if errors.Is(err, unix.EBADF) {
				continue
			}
			return err
		}
		if stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Size <= 0 || stat.Size > maxConfigBytes {
			continue
		}
		body := make([]byte, int(stat.Size))
		n, err := unix.Pread(fd, body, 0)
		if err != nil || n != len(body) {
			return errors.Join(err, io.ErrUnexpectedEOF)
		}
		for _, needle := range needles {
			if bytes.Contains(body, needle) {
				return fmt.Errorf("result proof leaked through provider fd %d", fd)
			}
		}
	}
	return os.WriteFile(filepath.Join(root, "proof-census.safe"), nil, 0o600)
}

func runLeaderExitDescendantHelper(root string) error {
	command, descendant, err := startDescendantHelper("fifo-child", filepath.Join(root, "descendant.release"))
	if err != nil {
		return err
	}
	path := filepath.Join(root, "descendant.pid")
	pending := path + ".pending"
	if err := os.WriteFile(pending, []byte(strconv.Itoa(descendant.PID)), 0o600); err != nil {
		cleanupDescendantHelper(command)
		return err
	}
	if err := os.Rename(pending, path); err != nil {
		_ = os.Remove(pending)
		cleanupDescendantHelper(command)
		return err
	}
	return nil
}

func runTERMForkDescendantHelper(root string) error {
	term := make(chan os.Signal, 1)
	signal.Notify(term, unix.SIGTERM)
	defer signal.Stop(term)
	if _, err := fmt.Fprintln(os.Stdout, "ready"); err != nil {
		return err
	}
	<-term
	command, descendant, err := startDescendantHelper("paused-child", root)
	if err != nil {
		return err
	}
	if _, err := fmt.Fprintf(os.Stdout, "late:%d\n", descendant.PID); err != nil {
		cleanupDescendantHelper(command)
		return err
	}
	for range term {
	}
	return errors.New("TERM signal channel closed")
}

func startDescendantHelper(mode, argument string) (*exec.Cmd, Identity, error) {
	readyReader, readyWriter, err := os.Pipe()
	if err != nil {
		return nil, Identity{}, err
	}
	executable, err := os.Executable()
	if err != nil {
		_ = readyReader.Close()
		_ = readyWriter.Close()
		return nil, Identity{}, err
	}
	command := exec.Command(executable, "--owned-descendant-helper", mode, argument)
	command.ExtraFiles = []*os.File{readyWriter}
	if err := command.Start(); err != nil {
		_ = readyReader.Close()
		_ = readyWriter.Close()
		return nil, Identity{}, err
	}
	_ = readyWriter.Close()
	cleanup := true
	defer func() {
		_ = readyReader.Close()
		if cleanup {
			_ = command.Process.Kill()
			_, _ = command.Process.Wait()
		}
	}()
	if err := readyReader.SetReadDeadline(time.Now().Add(4 * time.Second)); err != nil {
		return nil, Identity{}, err
	}
	var ready [1]byte
	if _, err := io.ReadFull(readyReader, ready[:]); err != nil {
		return nil, Identity{}, fmt.Errorf("descendant readiness: %w", err)
	}
	identity, err := readIdentity(command.Process.Pid)
	if err != nil {
		return nil, Identity{}, err
	}
	pgid, err := unix.Getpgid(0)
	if err != nil || identity.PGID != pgid {
		return nil, Identity{}, fmt.Errorf("descendant group identity=%+v parent_pgid=%d err=%v", identity, pgid, err)
	}
	cleanup = false
	return command, identity, nil
}

func cleanupDescendantHelper(command *exec.Cmd) {
	_ = command.Process.Kill()
	_, _ = command.Process.Wait()
}

func runFIFODescendantHelper(path string) error {
	release, err := os.OpenFile(path, os.O_RDONLY, 0)
	if err != nil {
		return err
	}
	defer release.Close()
	if err := signalHelperReady(); err != nil {
		return err
	}
	var byte [1]byte
	_, err = io.ReadFull(release, byte[:])
	return err
}

func signalHelperReady() error {
	ready := os.NewFile(3, "descendant-ready")
	if ready == nil {
		return errors.New("missing descendant readiness descriptor")
	}
	defer ready.Close()
	_, err := ready.Write([]byte{1})
	return err
}

func runPTYProviderHelper(root string) error {
	for _, fd := range []int{0, 1, 2} {
		if _, err := unix.IoctlGetWinsize(fd, unix.TIOCGWINSZ); err != nil {
			return fmt.Errorf("provider fd %d is not a tty: %v", fd, err)
		}
	}
	if sid, err := unix.Getsid(0); err != nil || sid != os.Getpid() {
		return fmt.Errorf("provider session sid=%d err=%v", sid, err)
	}
	if pgid, err := unix.Getpgid(os.Getpid()); err != nil || pgid != os.Getpid() {
		return fmt.Errorf("provider process group pgid=%d err=%v", pgid, err)
	}
	if pgrp, err := unix.IoctlGetInt(0, unix.TIOCGPGRP); err != nil || pgrp != os.Getpid() {
		return fmt.Errorf("provider controlling tty pgrp=%d err=%v", pgrp, err)
	}
	for _, fd := range []int{3, 4, 5, 6, 7, 8, 9, 11, 12} {
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); !errors.Is(err, unix.EBADF) {
			return fmt.Errorf("provider inherited fd %d: %v", fd, err)
		}
	}
	if err := os.WriteFile(filepath.Join(root, "provider.effect"), []byte("provider-started"), 0o600); err != nil {
		return err
	}
	line := make([]byte, 128)
	n, err := os.Stdin.Read(line)
	if err != nil {
		return err
	}
	_, err = fmt.Fprintf(os.Stdout, "RESPONSE:%s\n", strings.TrimSpace(string(line[:n])))
	return err
}

const ptyExitPressureChunk = 8 << 10

func runPTYExitPressureHelper(ready string) error {
	term := make(chan os.Signal, 1)
	signal.Notify(term, unix.SIGTERM)
	defer signal.Stop(term)
	if err := unix.SetNonblock(1, true); err != nil {
		return err
	}
	payload := bytes.Repeat([]byte{'x'}, ptyExitPressureChunk)
	written := 0
	for {
		n, err := unix.Write(1, payload)
		if n > 0 {
			written += n
		}
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			break
		}
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrNoProgress
		}
	}
	if written == 0 {
		return errors.New("PTY accepted no output before reporting full")
	}
	pending := ready + ".pending"
	if err := os.WriteFile(pending, []byte(strconv.Itoa(written)), 0o600); err != nil {
		return err
	}
	if err := os.Rename(pending, ready); err != nil {
		return err
	}
	<-term
	syscall.Exit(0)
	return errors.New("syscall.Exit returned")
}

func runCwdProviderHelper(root string) error {
	err := runCwdProviderChecks(root)
	if err != nil {
		message := []byte(err.Error())
		if len(message) > maxAttemptReportBytes {
			message = message[:maxAttemptReportBytes]
		}
		writeCwdProviderDiagnostic(root, message)
	}
	return err
}

func writeCwdProviderDiagnostic(root string, message []byte) {
	// Keep helper failures inspectable without exposing provider output or
	// relying on a PTY diagnostic path. O_EXCL prevents a preexisting path,
	// including a symlink, from being followed or replaced.
	file, err := os.OpenFile(filepath.Join(root, "cwd.error"), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return
	}
	for len(message) > 0 {
		n, err := file.Write(message)
		if err != nil || n <= 0 {
			_ = file.Close()
			return
		}
		message = message[n:]
	}
	_ = file.Close()
}

const cwdDescriptorManifestName = "cwd.descriptors"
const maxCwdDescriptorManifestBytes = 32 << 10
const maxCwdDescriptorManifestEntries = 1024

type cwdDescriptorProof struct {
	Device uint64
	Inode  uint64
	Mode   uint16
}

func cwdDescriptorProofFromStat(stat *unix.Stat_t) cwdDescriptorProof {
	return cwdDescriptorProof{
		Device: uint64(stat.Dev), Inode: stat.Ino, Mode: stat.Mode,
	}
}

func (p cwdDescriptorProof) matches(stat *unix.Stat_t) bool {
	// Darwin may assign no stable device/inode identity to unnamed IPC.
	// Such a descriptor can never prove that the same descriptor crossed exec.
	return p.Device != 0 && p.Inode != 0 && p == cwdDescriptorProofFromStat(stat)
}

func cwdDescriptorSnapshot() (map[int]cwdDescriptorProof, int, error) {
	directory, err := os.Open("/dev/fd")
	if err != nil {
		return nil, 0, err
	}
	scanFD := int(directory.Fd())
	entries, err := directory.Readdirnames(-1)
	if err != nil {
		_ = directory.Close()
		return nil, scanFD, err
	}
	proofs := make(map[int]cwdDescriptorProof)
	for _, entry := range entries {
		fd, err := strconv.Atoi(entry)
		if err != nil || fd <= 2 || fd == scanFD {
			continue
		}
		flags, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0)
		if errors.Is(err, unix.EBADF) {
			continue
		}
		if err != nil {
			_ = directory.Close()
			return nil, scanFD, err
		}
		if flags&unix.FD_CLOEXEC != 0 {
			continue
		}
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); errors.Is(err, unix.EBADF) {
			continue
		} else if err != nil {
			_ = directory.Close()
			return nil, scanFD, err
		}
		if len(proofs) >= maxCwdDescriptorManifestEntries {
			_ = directory.Close()
			return nil, scanFD, ErrIdentity
		}
		if _, exists := proofs[fd]; exists {
			_ = directory.Close()
			return nil, scanFD, ErrIdentity
		}
		proofs[fd] = cwdDescriptorProofFromStat(&stat)
	}
	if err := directory.Close(); err != nil {
		return nil, scanFD, err
	}
	if _, ok := proofs[10]; !ok {
		return nil, scanFD, ErrIdentity
	}
	return proofs, scanFD, nil
}

// writeCwdDescriptorManifest records every non-CLOEXEC descriptor before the
// provider exec. The provider's Go runtime may create descriptors after exec
// (including unnamed sockets); matching number and fstat identity lets the
// child distinguish those from actual inheritance.
func writeCwdDescriptorManifest(root string) error {
	proofs, _, err := cwdDescriptorSnapshot()
	if err != nil {
		return err
	}
	if _, ok := proofs[10]; !ok {
		return ErrIdentity
	}
	keys := make([]int, 0, len(proofs))
	for fd := range proofs {
		keys = append(keys, fd)
	}
	sort.Ints(keys)
	var manifest strings.Builder
	for _, fd := range keys {
		proof := proofs[fd]
		fmt.Fprintf(&manifest, "%d %d %d %d\n", fd, proof.Device, proof.Inode, proof.Mode)
	}
	if manifest.Len() == 0 || manifest.Len() > maxCwdDescriptorManifestBytes {
		return ErrIdentity
	}
	file, err := os.OpenFile(filepath.Join(root, cwdDescriptorManifestName), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return err
	}
	data := []byte(manifest.String())
	for len(data) > 0 {
		n, writeErr := file.Write(data)
		if writeErr != nil || n <= 0 {
			_ = file.Close()
			if writeErr != nil {
				return writeErr
			}
			return io.ErrShortWrite
		}
		data = data[n:]
	}
	return file.Close()
}

func readCwdDescriptorManifest(root string) (map[int]cwdDescriptorProof, error) {
	fd, err := unix.Open(filepath.Join(root, cwdDescriptorManifestName), unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil, err
	}
	file := os.NewFile(uintptr(fd), "cwd-descriptor-manifest")
	if file == nil {
		_ = unix.Close(fd)
		return nil, ErrIdentity
	}
	var manifestStat unix.Stat_t
	if err := unix.Fstat(fd, &manifestStat); err != nil {
		_ = file.Close()
		return nil, err
	}
	if manifestStat.Mode&unix.S_IFMT != unix.S_IFREG || manifestStat.Mode&0o7777 != 0o600 || manifestStat.Nlink != 1 || manifestStat.Uid != uint32(os.Geteuid()) {
		_ = file.Close()
		return nil, ErrIdentity
	}
	body, err := io.ReadAll(io.LimitReader(file, maxCwdDescriptorManifestBytes+1))
	closeErr := file.Close()
	if err != nil {
		return nil, err
	}
	if closeErr != nil {
		return nil, closeErr
	}
	if len(body) == 0 || len(body) > maxCwdDescriptorManifestBytes || body[len(body)-1] != '\n' {
		return nil, ErrIdentity
	}
	manifest := map[int]cwdDescriptorProof{}
	lines := strings.Split(string(body), "\n")
	if len(lines) < 2 || len(lines) > maxCwdDescriptorManifestEntries+1 {
		return nil, ErrIdentity
	}
	previousFD := 2
	for index, line := range lines[:len(lines)-1] {
		if line == "" || line != strings.TrimSpace(line) {
			return nil, ErrIdentity
		}
		fields := strings.Fields(line)
		if len(fields) != 4 {
			return nil, ErrIdentity
		}
		fd, err := strconv.Atoi(fields[0])
		if err != nil || fd <= 2 {
			return nil, ErrIdentity
		}
		if index > 0 && fd <= previousFD {
			return nil, ErrIdentity
		}
		previousFD = fd
		device, err := strconv.ParseUint(fields[1], 10, 64)
		if err != nil {
			return nil, ErrIdentity
		}
		inode, err := strconv.ParseUint(fields[2], 10, 64)
		if err != nil {
			return nil, ErrIdentity
		}
		mode, err := strconv.ParseUint(fields[3], 10, 16)
		if err != nil {
			return nil, ErrIdentity
		}
		fileType := uint16(mode) & unix.S_IFMT
		if (device == 0 || inode == 0) && fileType != unix.S_IFIFO && fileType != unix.S_IFSOCK {
			return nil, fmt.Errorf("runner: cwd descriptor manifest fd %d has zero identity for mode %#o: %w", fd, mode, ErrIdentity)
		}
		if _, exists := manifest[fd]; exists {
			return nil, ErrIdentity
		}
		manifest[fd] = cwdDescriptorProof{Device: device, Inode: inode, Mode: uint16(mode)}
	}
	lifetime, ok := manifest[10]
	if !ok || lifetime.Device == 0 || lifetime.Inode == 0 || lifetime.Mode&unix.S_IFMT != unix.S_IFREG {
		return nil, fmt.Errorf("runner: cwd descriptor manifest lifetime=%+v: %w", lifetime, ErrIdentity)
	}
	return manifest, nil
}

func TestCwdDescriptorManifestRejectsIncompleteEvidence(t *testing.T) {
	const valid = "10 1 2 32768\n"
	for _, test := range []struct {
		name string
		body string
	}{
		{name: "empty"},
		{name: "truncated", body: strings.TrimSuffix(valid, "\n")},
		{name: "duplicate", body: valid + valid},
		{name: "missing lifetime", body: strings.Replace(valid, "10 ", "11 ", 1)},
		{name: "zero device", body: strings.Replace(valid, "10 1 ", "10 0 ", 1)},
		{name: "zero regular descriptor", body: valid + "12 0 0 32768\n"},
		{name: "wrong field count", body: "10 1 2\n"},
	} {
		t.Run(test.name, func(t *testing.T) {
			root := t.TempDir()
			if err := os.WriteFile(filepath.Join(root, cwdDescriptorManifestName), []byte(test.body), 0o600); err != nil {
				t.Fatal(err)
			}
			if _, err := readCwdDescriptorManifest(root); !errors.Is(err, ErrIdentity) {
				t.Fatalf("manifest error=%v", err)
			}
		})
	}
	root := t.TempDir()
	target := filepath.Join(root, "manifest-target")
	if err := os.WriteFile(target, []byte(valid), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(target, filepath.Join(root, cwdDescriptorManifestName)); err != nil {
		t.Fatal(err)
	}
	if _, err := readCwdDescriptorManifest(root); !errors.Is(err, unix.ELOOP) && !errors.Is(err, ErrIdentity) {
		t.Fatalf("manifest symlink error=%v", err)
	}
}

func TestCwdDescriptorManifestTreatsZeroIdentityAsNonEvidence(t *testing.T) {
	const lifetime = "10 1 2 32768\n"
	root := t.TempDir()
	pipeMode := uint16(unix.S_IFIFO | 0o600)
	if err := os.WriteFile(filepath.Join(root, cwdDescriptorManifestName), []byte(lifetime+fmt.Sprintf("12 0 0 %d\n", pipeMode)), 0o600); err != nil {
		t.Fatal(err)
	}
	manifest, err := readCwdDescriptorManifest(root)
	if err != nil {
		t.Fatalf("zero-identity descriptor manifest: %v", err)
	}
	if manifest[12].matches(&unix.Stat_t{Mode: pipeMode}) {
		t.Fatal("zero-identity descriptor became inherited-descriptor evidence")
	}
}

func TestCwdDescriptorSnapshotExcludesItsScanner(t *testing.T) {
	root := t.TempDir()
	file, err := os.OpenFile(filepath.Join(root, "scanner-proof"), os.O_CREATE|os.O_RDWR, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	if _, err := unix.FcntlInt(file.Fd(), unix.F_SETFD, 0); err != nil {
		t.Fatal(err)
	}
	original, originalErr := unix.FcntlInt(10, unix.F_DUPFD_CLOEXEC, 100)
	if err := unix.Dup2(int(file.Fd()), 10); err != nil {
		t.Fatal(err)
	}
	if _, err := unix.FcntlInt(10, unix.F_SETFD, 0); err != nil {
		t.Fatal(err)
	}
	defer func() {
		if originalErr == nil {
			_ = unix.Dup2(original, 10)
			_ = unix.Close(original)
		} else {
			_ = unix.Close(10)
		}
	}()
	proofs, scanFD, err := cwdDescriptorSnapshot()
	if err != nil {
		t.Fatal(err)
	}
	if _, ok := proofs[scanFD]; ok {
		t.Fatalf("scanner fd %d was included", scanFD)
	}
	if _, ok := proofs[int(file.Fd())]; !ok {
		t.Fatalf("open fd %d was omitted", file.Fd())
	}
}

func runCwdProviderChecks(root string) error {
	info, err := os.Stat(".")
	if err != nil {
		return err
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		return ErrIdentity
	}
	manifest, err := readCwdDescriptorManifest(root)
	if err != nil {
		return fmt.Errorf("cwd provider manifest: %w", err)
	}
	directory, err := os.Open("/dev/fd")
	if err != nil {
		return err
	}
	defer directory.Close()
	entries, err := directory.Readdirnames(-1)
	if err != nil {
		return err
	}
	for _, entry := range entries {
		fd, err := strconv.Atoi(entry)
		if err != nil || fd <= 2 || fd == int(directory.Fd()) {
			continue
		}
		flags, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0)
		if errors.Is(err, unix.EBADF) {
			continue
		}
		if err != nil {
			return err
		}
		// A descriptor with CLOEXEC cannot have survived the worker's exec;
		// Go may open one for its own runtime after this provider starts.
		if flags&unix.FD_CLOEXEC != 0 {
			if fd == 10 {
				return fmt.Errorf("cwd provider lifetime fd 10 is close-on-exec: %w", ErrIdentity)
			}
			continue
		}
		var inherited unix.Stat_t
		if err := unix.Fstat(fd, &inherited); errors.Is(err, unix.EBADF) {
			continue
		} else if err != nil {
			return err
		}
		if fd == 10 {
			want, ok := manifest[fd]
			if !ok || !want.matches(&inherited) {
				return fmt.Errorf("cwd provider lifetime identity want=%+v got=%+v: %w", want, cwdDescriptorProofFromStat(&inherited), ErrIdentity)
			}
			continue
		}
		want, ok := manifest[fd]
		if !ok || !want.matches(&inherited) {
			continue
		}
		return fmt.Errorf("cwd provider inherited fd %d stat=%+v flags=%#x", fd, inherited, flags)
	}
	var lifetime unix.Stat_t
	if err := unix.Fstat(10, &lifetime); err != nil || lifetime.Mode&unix.S_IFMT != unix.S_IFREG || lifetime.Size != 0 {
		return fmt.Errorf("cwd provider lifetime: %v", err)
	}
	if err := os.WriteFile(filepath.Join(root, "cwd.identity"), []byte(fmt.Sprintf("%d:%d", stat.Dev, stat.Ino)), 0o600); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(root, "provider.effect"), []byte("provider"), 0o600)
}

func TestCwdProviderDiagnosticPreservesExistingPaths(t *testing.T) {
	const original = "preserve this diagnostic"
	cases := []struct {
		name  string
		setup func(t *testing.T, root string)
		check func(t *testing.T, root string)
	}{
		{
			name: "regular file",
			setup: func(t *testing.T, root string) {
				t.Helper()
				if err := os.WriteFile(filepath.Join(root, "cwd.error"), []byte(original), 0o644); err != nil {
					t.Fatal(err)
				}
				// WriteFile is umask-masked; the check below asserts the
				// exact pre-existing mode survives, so pin it explicitly.
				if err := os.Chmod(filepath.Join(root, "cwd.error"), 0o644); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t *testing.T, root string) {
				t.Helper()
				info, err := os.Lstat(filepath.Join(root, "cwd.error"))
				if err != nil || info.Mode().Perm() != 0o644 || !info.Mode().IsRegular() {
					t.Fatalf("diagnostic identity=%v err=%v", info, err)
				}
				if got, err := os.ReadFile(filepath.Join(root, "cwd.error")); err != nil || string(got) != original {
					t.Fatalf("diagnostic=%q err=%v", got, err)
				}
			},
		},
		{
			name: "symlink",
			setup: func(t *testing.T, root string) {
				t.Helper()
				outside := filepath.Join(root, "outside")
				if err := os.WriteFile(outside, []byte(original), 0o644); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(outside, filepath.Join(root, "cwd.error")); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t *testing.T, root string) {
				t.Helper()
				path := filepath.Join(root, "cwd.error")
				info, err := os.Lstat(path)
				if err != nil || info.Mode()&os.ModeSymlink == 0 {
					t.Fatalf("diagnostic identity=%v err=%v", info, err)
				}
				target, err := os.Readlink(path)
				if err != nil || target != filepath.Join(root, "outside") {
					t.Fatalf("diagnostic target=%q err=%v", target, err)
				}
				if got, err := os.ReadFile(path); err != nil || string(got) != original {
					t.Fatalf("target diagnostic=%q err=%v", got, err)
				}
			},
		},
		{
			name: "directory",
			setup: func(t *testing.T, root string) {
				t.Helper()
				if err := os.Mkdir(filepath.Join(root, "cwd.error"), 0o755); err != nil {
					t.Fatal(err)
				}
			},
			check: func(t *testing.T, root string) {
				t.Helper()
				info, err := os.Lstat(filepath.Join(root, "cwd.error"))
				if err != nil || !info.IsDir() {
					t.Fatalf("diagnostic identity=%v err=%v", info, err)
				}
			},
		},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			root := t.TempDir()
			test.setup(t, root)
			path := filepath.Join(root, "cwd.error")
			before, err := os.Lstat(path)
			if err != nil {
				t.Fatal(err)
			}
			writeCwdProviderDiagnostic(root, []byte(original))
			after, err := os.Lstat(path)
			if err != nil || !os.SameFile(before, after) {
				t.Fatalf("diagnostic identity before=%v after=%v err=%v", before, after, err)
			}
			test.check(t, root)
		})
	}
}

func TestCwdProviderDiagnosticCreatesExactPrivateFile(t *testing.T) {
	root := t.TempDir()
	message := []byte("cwd provider inherited fd 5")
	writeCwdProviderDiagnostic(root, message)
	info, err := os.Lstat(filepath.Join(root, "cwd.error"))
	if err != nil {
		t.Fatal(err)
	}
	if !info.Mode().IsRegular() || info.Mode().Perm() != 0o600 {
		t.Fatalf("diagnostic mode=%v", info.Mode())
	}
	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok || stat.Nlink != 1 {
		t.Fatalf("diagnostic stat=%+v", info.Sys())
	}
	if got, err := os.ReadFile(filepath.Join(root, "cwd.error")); err != nil || !bytes.Equal(got, message) {
		t.Fatalf("diagnostic=%q err=%v", got, err)
	}
}

func runLifetimeProviderHelper(root string) error {
	if !signal.Ignored(unix.SIGHUP) {
		return errors.New("provider did not inherit ignored SIGHUP")
	}
	var lifetime unix.Stat_t
	if err := unix.Fstat(10, &lifetime); err != nil || lifetime.Mode&unix.S_IFMT != unix.S_IFREG || lifetime.Mode&0o7777 != 0o600 || lifetime.Size != 0 || lifetime.Nlink != 1 {
		return fmt.Errorf("provider lifetime descriptor: %v", err)
	}
	for _, fd := range []int{3, 4, 5, 6, 7, 8, 9, 12} {
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); !errors.Is(err, unix.EBADF) {
			return fmt.Errorf("provider inherited fd %d: %v", fd, err)
		}
	}
	if err := validateProviderTaskDescriptor(11); err != nil {
		return fmt.Errorf("provider task descriptor: %w", err)
	}
	if err := unix.Fchdir(10); !errors.Is(err, unix.ENOTDIR) && !errors.Is(err, unix.EBADF) {
		return fmt.Errorf("provider lifetime allowed fchdir: %v", err)
	}
	if _, err := unix.Write(10, []byte{1}); !errors.Is(err, unix.EBADF) {
		return fmt.Errorf("provider lifetime allowed write: %v", err)
	}
	if err := unix.Ftruncate(10, 1); !errors.Is(err, unix.EBADF) && !errors.Is(err, unix.EINVAL) {
		return fmt.Errorf("provider lifetime allowed truncate: %v", err)
	}
	if fd, err := unix.Openat(10, "change-worker.config", unix.O_RDONLY|unix.O_CLOEXEC, 0); !errors.Is(err, unix.ENOTDIR) && !errors.Is(err, unix.EBADF) {
		if err == nil {
			unix.Close(fd)
		}
		return fmt.Errorf("provider lifetime allowed openat: %v", err)
	}
	if err := os.WriteFile(filepath.Join(root, "provider.pid"), []byte(strconv.Itoa(os.Getpid())), 0o600); err != nil {
		return err
	}
	if err := os.WriteFile(filepath.Join(root, "provider.ready"), nil, 0o600); err != nil {
		return err
	}
	deadline := time.Now().Add(8 * time.Second)
	for {
		if _, err := os.Stat(filepath.Join(root, "continue")); err == nil {
			return nil
		}
		if time.Now().After(deadline) {
			return os.ErrDeadlineExceeded
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func fdCensus() map[int]FileIdentity {
	out := map[int]FileIdentity{}
	entries, _ := os.ReadDir("/dev/fd")
	for _, e := range entries {
		fd, err := strconv.Atoi(e.Name())
		if err != nil {
			continue
		}
		var s unix.Stat_t
		if unix.Fstat(fd, &s) == nil {
			out[fd] = FileIdentity{Device: uint64(s.Dev), Inode: s.Ino}
		}
	}
	return out
}
func sameCensus(a, b map[int]FileIdentity) bool {
	if len(a) != len(b) {
		return false
	}
	for fd, id := range a {
		if b[fd] != id {
			return false
		}
	}
	return true
}

type fixture struct {
	t        *testing.T
	root     string
	cwd      string
	dir      *os.File
	lifetime *os.File
	lease    *GateLease
	child    *OwnedChild
}

func newFixture(t *testing.T) *fixture {
	t.Helper()
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	cwd := filepath.Join(root, "work")
	if err := os.Mkdir(cwd, 0o700); err != nil {
		t.Fatal(err)
	}
	dir, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	lifetime := createTestRuntimeLifetime(t, dir)
	lease, _, err := CreateGateLease(dir, lifetime, OuterActivationMarkerName)
	if err != nil {
		t.Fatal(err)
	}
	f := &fixture{t: t, root: root, cwd: cwd, dir: dir, lifetime: lifetime, lease: lease}
	t.Cleanup(func() {
		if f.child != nil {
			_ = f.child.Close()
		}
		_ = lease.Close()
		_ = lifetime.Close()
		_ = dir.Close()
	})
	return f
}

func createTestRuntimeLifetime(t testing.TB, dir *os.File) *os.File {
	t.Helper()
	created, err := unix.Openat(int(dir.Fd()), RuntimeLifetimeLeaseName, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	if err := unix.Fsync(created); err != nil {
		unix.Close(created)
		t.Fatal(err)
	}
	if err := unix.Close(created); err != nil {
		t.Fatal(err)
	}
	fd, err := unix.Openat(int(dir.Fd()), RuntimeLifetimeLeaseName, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		t.Fatal(err)
	}
	file := os.NewFile(uintptr(fd), "test-runtime-lifetime")
	if err := unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		file.Close()
		t.Fatal(err)
	}
	return file
}
func (f *fixture) start(target string, args []string, input []byte, stdout *os.File) *OwnedChild {
	f.t.Helper()
	spec, err := PrepareExecSpec(ExecSpec{Target: target, Args: args, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: f.cwd, Stdin: input, Stdout: stdout, Stderr: stdout})
	if err != nil {
		f.t.Fatal(err)
	}
	gate, err := os.Executable()
	if err != nil {
		f.t.Fatal(err)
	}
	child, err := StartBlocked(f.lease, gate, spec, false)
	if err != nil {
		var diagnostic []byte
		if stdout != nil {
			_ = stdout.Sync()
			_, _ = stdout.Seek(0, 0)
			diagnostic, _ = io.ReadAll(stdout)
		}
		f.t.Fatalf("StartBlocked: %v; gate stderr=%q", err, diagnostic)
	}
	f.child = child
	return child
}

func (f *fixture) startPrepared(spec *LaunchSpec, keepLease bool) *OwnedChild {
	f.t.Helper()
	gate, err := os.Executable()
	if err != nil {
		f.t.Fatal(err)
	}
	child, err := StartBlocked(f.lease, gate, spec, keepLease)
	if err != nil {
		f.t.Fatalf("StartBlocked: %v", err)
	}
	f.child = child
	return child
}
func outputFile(t *testing.T, path string) *os.File {
	t.Helper()
	f, err := os.OpenFile(path, os.O_CREATE|os.O_RDWR|os.O_EXCL, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = f.Close() })
	return f
}
func waitFile(t *testing.T, path string) {
	t.Helper()
	deadline := time.Now().Add(6 * time.Second)
	for {
		if _, err := os.Stat(path); err == nil {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("witness %s absent", path)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func startExitedLeaderWithDescendant(t *testing.T, f *fixture) (*OwnedChild, *os.File, Identity) {
	t.Helper()
	releasePath := filepath.Join(f.root, "descendant.release")
	if err := unix.Mkfifo(releasePath, 0o600); err != nil {
		t.Fatal(err)
	}
	releaseFD, err := unix.Open(releasePath, unix.O_RDWR|unix.O_CLOEXEC, 0)
	if err != nil {
		t.Fatal(err)
	}
	release := os.NewFile(uintptr(releaseFD), "descendant-release")
	executable, err := os.Executable()
	if err != nil {
		_ = release.Close()
		t.Fatal(err)
	}
	child := f.start(executable, []string{"--owned-descendant-helper", "leader", f.root}, nil, outputFile(t, filepath.Join(f.root, "out")))
	if _, err := child.Activate(); err != nil {
		_ = release.Close()
		t.Fatal(err)
	}
	descendantPath := filepath.Join(f.root, "descendant.pid")
	waitFile(t, descendantPath)
	body, err := os.ReadFile(descendantPath)
	if err != nil {
		_ = release.Close()
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(body)))
	if err != nil {
		_ = release.Close()
		t.Fatal(err)
	}
	descendant, err := readIdentity(pid)
	if err != nil || descendant.PGID != child.Identity().PGID {
		_ = release.Close()
		t.Fatalf("descendant identity=%+v err=%v", descendant, err)
	}
	if _, err := child.waitForExit(4 * time.Second); err != nil {
		_ = release.Close()
		t.Fatal(err)
	}
	observation := ObserveProcessGroup(child.Identity())
	if observation.Presence != Present || len(observation.Members) < 2 {
		_ = release.Close()
		t.Fatalf("leader exit mistaken for group absence: %+v", observation)
	}
	return child, release, descendant
}

func installExactDescendantSafetyCleanup(t *testing.T, descendant Identity) func() {
	t.Helper()
	clean := false
	t.Cleanup(func() {
		if clean {
			return
		}
		observation := ObserveProcess(descendant)
		if observation.Presence == Absent {
			return
		}
		current, err := readIdentity(descendant.PID)
		if err != nil || current != descendant {
			t.Errorf("refusing unsafe descendant cleanup: want=%+v current=%+v err=%v", descendant, current, err)
			return
		}
		kq, err := unix.Kqueue()
		if err != nil {
			t.Errorf("exact descendant cleanup kqueue: %v", err)
			return
		}
		defer unix.Close(kq)
		if err := registerExit(kq, descendant.PID); err != nil {
			t.Errorf("exact descendant cleanup registration: %v", err)
			return
		}
		if err := unix.Kill(descendant.PID, unix.SIGKILL); err != nil && !errors.Is(err, unix.ESRCH) {
			t.Errorf("exact descendant cleanup: %v", err)
			return
		}
		timeout := unix.NsecToTimespec((4 * time.Second).Nanoseconds())
		events := make([]unix.Kevent_t, 1)
		n, err := unix.Kevent(kq, nil, events, &timeout)
		if err != nil || n != 1 || int(events[0].Ident) != descendant.PID || events[0].Filter != unix.EVFILT_PROC || events[0].Fflags&unix.NOTE_EXIT == 0 {
			t.Errorf("exact descendant cleanup exit evidence: n=%d event=%+v err=%v", n, events[0], err)
		}
	})
	return func() { clean = true }
}

func killExactTestDescendant(t *testing.T, descendant Identity) {
	t.Helper()
	kq := exactExitKqueue(t, descendant)
	current, err := readIdentity(descendant.PID)
	if err != nil || current != descendant {
		t.Fatalf("refusing unsafe descendant kill: want=%+v current=%+v err=%v", descendant, current, err)
	}
	if err := unix.Kill(descendant.PID, unix.SIGKILL); err != nil {
		t.Fatal(err)
	}
	waitKqueueExit(t, kq, descendant)
}

func exactExitKqueue(t *testing.T, identity Identity) int {
	t.Helper()
	kq, err := unix.Kqueue()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = unix.Close(kq) })
	if err := registerExit(kq, identity.PID); err != nil {
		t.Fatal(err)
	}
	return kq
}

func readOwnedPipeLine(t *testing.T, pipe *os.File, reader *bufio.Reader) string {
	t.Helper()
	if err := pipe.SetReadDeadline(time.Now().Add(4 * time.Second)); err != nil {
		t.Fatal(err)
	}
	line, err := reader.ReadString('\n')
	if err != nil {
		t.Fatalf("owned helper report: %v", err)
	}
	return strings.TrimSpace(line)
}

func runParentDeathOwner() error {
	root := os.Getenv("RUNNER_TEST_ROOT")
	if root == "" {
		return errors.New("runner test owner: missing root")
	}
	dir, err := os.Open(root)
	if err != nil {
		return err
	}
	defer dir.Close()
	lifetime := createTestRuntimeLifetimeTest(dir)
	if lifetime == nil {
		return errors.New("runner test owner: lifetime lease")
	}
	defer lifetime.Close()
	lease, _, err := CreateGateLease(dir, lifetime, OuterActivationMarkerName)
	if err != nil {
		return err
	}
	defer lease.Close()
	effect := filepath.Join(root, "effect")
	spec, err := PrepareExecSpec(ExecSpec{
		Target: "/bin/sh",
		Args:   []string{"-c", fmt.Sprintf("printf bad > %q", effect)},
		Env:    []string{"PATH=/usr/bin:/bin", "LANG=C"},
		Cwd:    filepath.Join(root, "work"),
	})
	if err != nil {
		return err
	}
	gate, err := os.Executable()
	if err != nil {
		return err
	}
	child, err := StartBlocked(lease, gate, spec, false)
	if err != nil {
		return err
	}
	defer child.Close()
	report := os.NewFile(3, "owner-report")
	if report == nil {
		return errors.New("runner test owner: missing report capability")
	}
	if err := writeFrame(report, gateFrame{Kind: "owned-gate", Identity: child.Identity()}, maxFrameBytes); err != nil {
		return err
	}
	if err := report.Close(); err != nil {
		return err
	}
	for {
		time.Sleep(time.Hour)
	}
}

func waitKqueueExit(t *testing.T, kq int, want Identity) {
	t.Helper()
	deadline := time.Now().Add(4 * time.Second)
	for {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			t.Fatal("timeout waiting for exact gate NOTE_EXIT")
		}
		ts := unix.NsecToTimespec(remaining.Nanoseconds())
		events := make([]unix.Kevent_t, 1)
		n, err := unix.Kevent(kq, nil, events, &ts)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			t.Fatal(err)
		}
		if n != 1 {
			t.Fatal("timeout waiting for exact gate NOTE_EXIT")
		}
		event := events[0]
		if int(event.Ident) != want.PID || event.Filter != unix.EVFILT_PROC || event.Fflags&unix.NOTE_EXIT == 0 || event.Flags&unix.EV_ERROR != 0 {
			t.Fatalf("unexpected gate exit event %+v", event)
		}
		return
	}
}

func waitExactAbsence(t *testing.T, want Identity) {
	t.Helper()
	deadline := time.Now().Add(4 * time.Second)
	for {
		got, infoErr := readIdentity(want.PID)
		if infoErr == nil && got != want {
			return
		}
		pidProbe := unix.Kill(want.PID, 0)
		if (errors.Is(infoErr, unix.EIO) || errors.Is(infoErr, unix.ESRCH) || errors.Is(infoErr, unix.ENOENT)) && errors.Is(pidProbe, unix.ESRCH) {
			return
		}
		if time.Now().After(deadline) {
			t.Fatalf("exact process did not become absent: want=%+v got=%+v info=%v pid=%v", want, got, infoErr, pidProbe)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func waitExactPresence(t *testing.T, want Identity) {
	t.Helper()
	deadline := time.Now().Add(time.Second)
	for {
		got, err := readIdentity(want.PID)
		if err == nil {
			if got != want {
				t.Fatalf("exact process identity changed: got=%+v want=%+v", got, want)
			}
			return
		}
		pidProbe := unix.Kill(want.PID, 0)
		groupProbe := unix.Kill(-want.PGID, 0)
		if !errors.Is(err, unix.EIO) || pidProbe != nil || groupProbe != nil || time.Now().After(deadline) {
			t.Fatalf("exact process presence unavailable: identity=%v pid=%v group=%v", err, pidProbe, groupProbe)
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func TestRealParentSIGKILLAbortsInertGate(t *testing.T) {
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(root, "work"), 0o700); err != nil {
		t.Fatal(err)
	}
	reportR, reportW, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	defer reportR.Close()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	diagnostic := outputFile(t, filepath.Join(root, "owner-output"))
	owner := exec.Command(executable, "-test.run=^$")
	owner.Env = []string{"RUNNER_TEST_OWNER=1", "RUNNER_TEST_ROOT=" + root, "TMPDIR=" + root}
	owner.ExtraFiles = []*os.File{reportW}
	owner.Stdout = diagnostic
	owner.Stderr = diagnostic
	owner.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if err := owner.Start(); err != nil {
		t.Fatal(err)
	}
	_ = reportW.Close()
	ownerWaited := false
	var gate Identity
	defer func() {
		if !ownerWaited {
			_ = owner.Process.Kill()
			_ = owner.Wait()
		}
		if gate.Valid() {
			if got, identityErr := readIdentity(gate.PID); identityErr == nil && got == gate {
				_ = unix.Kill(-gate.PGID, unix.SIGKILL)
			}
			deadline := time.Now().Add(4 * time.Second)
			for time.Now().Before(deadline) {
				if observation := ObserveProcess(gate); observation.Presence == Absent || observation.Presence == Reused {
					break
				}
				time.Sleep(5 * time.Millisecond)
			}
		}
	}()
	if err := reportR.SetReadDeadline(time.Now().Add(4 * time.Second)); err != nil {
		t.Fatal(err)
	}
	var report gateFrame
	if err := readFrame(reportR, &report, maxFrameBytes); err != nil {
		t.Fatalf("owner report: %v", err)
	}
	gate = report.Identity
	if report.Kind != "owned-gate" || !gate.Valid() || gate.PID != gate.PGID {
		t.Fatalf("bad owner report %+v", report)
	}
	if _, err := os.Stat(filepath.Join(root, "effect")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider effect before real parent death")
	}
	kq, err := unix.Kqueue()
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(kq)
	if err := registerExit(kq, gate.PID); err != nil {
		t.Fatal(err)
	}
	if err := owner.Process.Signal(unix.SIGKILL); err != nil {
		t.Fatal(err)
	}
	waitErr := owner.Wait()
	ownerWaited = true
	var exitErr *exec.ExitError
	if !errors.As(waitErr, &exitErr) || !exitErr.ProcessState.Sys().(syscall.WaitStatus).Signaled() || exitErr.ProcessState.Sys().(syscall.WaitStatus).Signal() != unix.SIGKILL {
		t.Fatalf("owner was not SIGKILLed: %v", waitErr)
	}
	waitKqueueExit(t, kq, gate)
	waitExactAbsence(t, gate)
	if _, err := os.Stat(filepath.Join(root, "effect")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider ran after its real owner was SIGKILLed before activation")
	}
}

func TestBlockedActivateExecutesOnceWithExactIdentityAndInput(t *testing.T) {
	f := newFixture(t)
	effect := filepath.Join(f.root, "effect")
	stdinCopy := filepath.Join(f.root, "stdin")
	out := outputFile(t, filepath.Join(f.root, "output"))
	child := f.start("/bin/sh", []string{"-c", fmt.Sprintf("test -z \"${HOME+x}\" || exit 90; for n in 3 4 5 6 7 8 9 11; do test ! -e /dev/fd/$n || exit 91; done; test -f /dev/fd/10 || exit 92; test ! -s /dev/fd/10 || exit 93; printf x >> %q; cat > %q; printf '%%s' $$", effect, stdinCopy)}, []byte("one-input"), out)
	id := child.Identity()
	if !id.Valid() || id.PID != id.PGID {
		t.Fatalf("bad ready identity %+v", id)
	}
	if got := ObserveProcessGroup(id); got.Presence != Present {
		t.Fatalf("ready observation %+v", got)
	}
	if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider effect before activation")
	}
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	if _, err := child.Activate(); !errors.Is(err, ErrState) {
		t.Fatalf("duplicate activation=%v", err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 0 {
		t.Fatalf("exit=%+v err=%v", exit, err)
	}
	if got, _ := os.ReadFile(effect); string(got) != "x" {
		t.Fatalf("effect=%q", got)
	}
	if got, _ := os.ReadFile(stdinCopy); string(got) != "one-input" {
		t.Fatalf("stdin=%q", got)
	}
	_ = out.Sync()
	_, _ = out.Seek(0, 0)
	body, _ := io.ReadAll(out)
	fields := string(body)
	if fields != fmt.Sprintf("%d", id.PID) {
		t.Fatalf("target identity %q want %d", fields, id.PID)
	}
	assertWaitedAndAbsent(t, child)
}

func TestPrivateDirectoryAndLifetimeInheritanceAreExplicit(t *testing.T) {
	f := newFixture(t)
	effect := filepath.Join(f.root, "effect")
	out := outputFile(t, filepath.Join(f.root, "out"))
	spec, err := PrepareExecSpec(ExecSpec{
		Target: "/bin/sh",
		Args:   []string{"-c", fmt.Sprintf("test -d /dev/fd/9 || exit 92; test -f /dev/fd/10 || exit 93; test ! -s /dev/fd/10 || exit 94; printf kept > %q", effect)},
		Env:    []string{"PATH=/usr/bin:/bin", "LANG=C"},
		Cwd:    f.cwd,
		Stdout: out,
		Stderr: out,
	})
	if err != nil {
		t.Fatal(err)
	}
	child := f.startPrepared(spec, true)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 0 {
		t.Fatalf("exit=%+v err=%v", exit, err)
	}
	if body, err := os.ReadFile(effect); err != nil || string(body) != "kept" {
		t.Fatalf("kept lease witness=%q err=%v", body, err)
	}
}

func TestInheritedRuntimeLifetimeSurvivesGateExecUntilTargetExit(t *testing.T) {
	f := newFixture(t)
	continuePath := filepath.Join(f.root, "continue")
	out := outputFile(t, filepath.Join(f.root, "out"))
	spec, err := PrepareExecSpec(ExecSpec{
		Target: "/bin/sh",
		Args:   []string{"-c", fmt.Sprintf("while test ! -e %q; do sleep 0.01; done", continuePath)},
		Env:    []string{"PATH=/usr/bin:/bin", "LANG=C"},
		Cwd:    f.cwd,
		Stdout: out,
		Stderr: out,
	})
	if err != nil {
		t.Fatal(err)
	}
	child := f.startPrepared(spec, true)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	if err := f.lease.Close(); err != nil {
		t.Fatal(err)
	}
	if err := f.dir.Close(); err != nil {
		t.Fatal(err)
	}
	if err := f.lifetime.Close(); err != nil {
		t.Fatal(err)
	}
	probe, err := unix.Open(filepath.Join(f.root, RuntimeLifetimeLeaseName), unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(probe)
	if err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB); !errors.Is(err, unix.EWOULDBLOCK) {
		t.Fatalf("inherited target lease was not held: %v", err)
	}
	if err := os.WriteFile(continuePath, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if exit, err := child.FinishAfterExit(4 * time.Second); err != nil || exit.Code != 0 {
		t.Fatalf("exit=%+v err=%v", exit, err)
	}
	if err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		t.Fatalf("lifetime lease retained after target exit: %v", err)
	}
	if err := unix.Flock(probe, unix.LOCK_UN); err != nil {
		t.Fatal(err)
	}
}

func TestAnonymousScratchMetadataFailureIsErrorAndExactCleanup(t *testing.T) {
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	dir, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	defer dir.Close()
	file, err := anonymousFileWithHook(dir, "config", []byte("private"), func(name string) {
		if err := os.Chmod(filepath.Join(root, name), 0o640); err != nil {
			t.Fatal(err)
		}
	})
	if !errors.Is(err, ErrIdentity) || file != nil {
		t.Fatalf("invalid scratch = %v, %v", file, err)
	}
	if _, err := os.Lstat(filepath.Join(root, GateConfigScratchName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("invalid scratch residue retained: %v", err)
	}
}

func TestLeashEOFAbortsWithoutProviderEffect(t *testing.T) {
	f := newFixture(t)
	effect := filepath.Join(f.root, "effect")
	out := outputFile(t, filepath.Join(f.root, "out"))
	child := f.start("/bin/sh", []string{"-c", "echo bad > " + effect}, nil, out)
	if err := child.Abort(); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("aborted provider ran")
	}
}

func TestActivationRefusesMarkerCreatedAfterReadiness(t *testing.T) {
	f := newFixture(t)
	effect := filepath.Join(f.root, "effect")
	out := outputFile(t, filepath.Join(f.root, "out"))
	child := f.start("/bin/sh", []string{"-c", "echo bad > " + effect}, nil, out)
	if err := os.WriteFile(filepath.Join(f.root, OuterActivationMarkerName), []byte("foreign"), 0o600); err != nil {
		t.Fatal(err)
	}
	if _, err := child.Activate(); !errors.Is(err, unix.EEXIST) {
		t.Fatalf("activation replaced marker: %v", err)
	}
	if err := child.Abort(); err != nil {
		t.Fatal(err)
	}
	if body, err := os.ReadFile(filepath.Join(f.root, OuterActivationMarkerName)); err != nil || string(body) != "foreign" {
		t.Fatalf("marker changed body=%q err=%v", body, err)
	}
	if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider ran after marker conflict")
	}
}

func TestFrozenExecutableRejectsReplacementAndMutation(t *testing.T) {
	for _, mutation := range []string{"replace", "bytes", "mode", "remove"} {
		t.Run(mutation, func(t *testing.T) {
			f := newFixture(t)
			target := filepath.Join(f.root, "shell")
			copyNative(t, "/bin/sh", target)
			effect := filepath.Join(f.root, "effect")
			out := outputFile(t, filepath.Join(f.root, "out"))
			child := f.start(target, []string{"-c", "echo bad > " + effect}, nil, out)
			switch mutation {
			case "replace":
				replacement := filepath.Join(f.root, "replacement")
				copyNative(t, "/bin/sh", replacement)
				if err := os.Rename(replacement, target); err != nil {
					t.Fatal(err)
				}
			case "bytes":
				file, err := os.OpenFile(target, os.O_WRONLY|os.O_APPEND, 0)
				if err != nil {
					t.Fatal(err)
				}
				_, _ = file.Write([]byte{0})
				_ = file.Close()
			case "mode":
				if err := os.Chmod(target, 0o700); err != nil {
					t.Fatal(err)
				}
			case "remove":
				if err := os.Remove(target); err != nil {
					t.Fatal(err)
				}
			}
			if _, err := child.Activate(); err != nil {
				t.Fatal(err)
			}
			exit, err := child.FinishAfterExit(4 * time.Second)
			if err != nil {
				t.Fatal(err)
			}
			if exit.LaunchErr == "" || exit.Code == 0 {
				t.Fatalf("mutation executed: %+v", exit)
			}
			if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
				t.Fatal("mutated target ran")
			}
		})
	}
}

func buildWitnessBinary(t *testing.T, output, value string) {
	t.Helper()
	source := filepath.Join(t.TempDir(), "main.go")
	body := fmt.Sprintf("package main\nimport \"os\"\nfunc main() { if len(os.Args) != 2 { os.Exit(2) }; if os.WriteFile(os.Args[1], []byte(%q), 0600) != nil { os.Exit(3) } }\n", value)
	if err := os.WriteFile(source, []byte(body), 0o600); err != nil {
		t.Fatal(err)
	}
	goBinary := filepath.Join(runtime.GOROOT(), "bin", "go")
	command := exec.Command(goBinary, "build", "-o", output, source)
	environment := []string{"GOENV=off", "GOWORK=off", "GOTOOLCHAIN=local", "CGO_ENABLED=0", "GOCACHE=" + t.TempDir(), "TMPDIR=" + t.TempDir()}
	for _, key := range []string{"GOMODCACHE", "GOPATH", "GOTMPDIR"} {
		if value := os.Getenv(key); value != "" {
			environment = append(environment, key+"="+value)
		}
	}
	command.Env = environment
	if output, err := command.CombinedOutput(); err != nil {
		t.Fatalf("build witness: %v: %s", err, output)
	}
}

func TestSameUIDReplacementAfterFinalCheckIsExplicitlyOutOfScope(t *testing.T) {
	f := newFixture(t)
	target := filepath.Join(f.root, "prepared-provider")
	replacement := filepath.Join(f.root, "replacement-provider")
	buildWitnessBinary(t, target, "prepared")
	buildWitnessBinary(t, replacement, "replacement")
	effect := filepath.Join(f.root, "effect")
	spec, err := PrepareExecSpec(ExecSpec{Target: target, Args: []string{effect}, Cwd: f.cwd})
	if err != nil {
		t.Fatal(err)
	}
	sockets, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	unix.CloseOnExec(sockets[0])
	unix.CloseOnExec(sockets[1])
	barrier := os.NewFile(uintptr(sockets[0]), "final-check-test-barrier")
	gateBarrier := os.NewFile(uintptr(sockets[1]), "gate-final-check-test-barrier")
	defer barrier.Close()
	defer gateBarrier.Close()
	spec.testFinal = gateBarrier
	child := f.startPrepared(spec, false)
	if err := gateBarrier.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	poll := []unix.PollFd{{Fd: int32(barrier.Fd()), Events: unix.POLLIN}}
	if n, err := unix.Poll(poll, 4000); err != nil || n != 1 || poll[0].Revents&unix.POLLIN == 0 {
		t.Fatalf("final-check barrier timeout/error: n=%d poll=%+v err=%v", n, poll[0], err)
	}
	var ready [1]byte
	if _, err := io.ReadFull(barrier, ready[:]); err != nil || ready[0] != 'R' {
		t.Fatalf("final-check barrier ready=%q err=%v", ready, err)
	}
	if err := os.Rename(replacement, target); err != nil {
		t.Fatal(err)
	}
	if _, err := barrier.Write([]byte{'X'}); err != nil {
		t.Fatal(err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 0 || exit.LaunchErr != "" {
		t.Fatalf("replacement exit=%+v err=%v", exit, err)
	}
	body, err := os.ReadFile(effect)
	if err != nil {
		t.Fatal(err)
	}
	if string(body) != "replacement" {
		t.Fatalf("post-check replacement did not demonstrate pathname TOCTOU: %q", body)
	}
}

func TestExecErrorAfterFinalCheckIsTyped(t *testing.T) {
	f := newFixture(t)
	target := filepath.Join(f.root, "prepared-provider")
	buildWitnessBinary(t, target, "unexpected")
	effect := filepath.Join(f.root, "effect")
	spec, err := PrepareExecSpec(ExecSpec{Target: target, Args: []string{effect}, Cwd: f.cwd})
	if err != nil {
		t.Fatal(err)
	}
	sockets, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM, 0)
	if err != nil {
		t.Fatal(err)
	}
	unix.CloseOnExec(sockets[0])
	unix.CloseOnExec(sockets[1])
	barrier := os.NewFile(uintptr(sockets[0]), "final-check-test-barrier")
	gateBarrier := os.NewFile(uintptr(sockets[1]), "gate-final-check-test-barrier")
	defer barrier.Close()
	defer gateBarrier.Close()
	spec.testFinal = gateBarrier
	child := f.startPrepared(spec, false)
	if err := gateBarrier.Close(); err != nil {
		t.Fatal(err)
	}
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	poll := []unix.PollFd{{Fd: int32(barrier.Fd()), Events: unix.POLLIN}}
	if n, err := unix.Poll(poll, 4000); err != nil || n != 1 || poll[0].Revents&unix.POLLIN == 0 {
		t.Fatalf("final-check barrier timeout/error: n=%d poll=%+v err=%v", n, poll[0], err)
	}
	var ready [1]byte
	if _, err := io.ReadFull(barrier, ready[:]); err != nil || ready[0] != 'R' {
		t.Fatalf("final-check barrier ready=%q err=%v", ready, err)
	}
	if err := os.Remove(target); err != nil {
		t.Fatal(err)
	}
	if _, err := barrier.Write([]byte{'X'}); err != nil {
		t.Fatal(err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code == 0 || exit.LaunchErr == "" {
		t.Fatalf("exec error was not typed: exit=%+v err=%v", exit, err)
	}
	if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("removed target produced provider effect")
	}
}

func TestResolvedInstallationSymlinkDoesNotRetarget(t *testing.T) {
	f := newFixture(t)
	link := filepath.Join(f.root, "current")
	if err := os.Symlink("/bin/sh", link); err != nil {
		t.Fatal(err)
	}
	effect := filepath.Join(f.root, "effect")
	out := outputFile(t, filepath.Join(f.root, "out"))
	child := f.start(link, []string{"-c", "echo old > " + effect}, nil, out)
	if err := os.Remove(link); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("/bin/echo", link); err != nil {
		t.Fatal(err)
	}
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 0 {
		t.Fatalf("%+v %v", exit, err)
	}
	body, _ := os.ReadFile(effect)
	if string(body) != "old\n" {
		t.Fatalf("effect=%q", body)
	}
}

func TestTerminateOwnsTERMKillAndWait(t *testing.T) {
	f := newFixture(t)
	ready := filepath.Join(f.root, "ready")
	out := outputFile(t, filepath.Join(f.root, "out"))
	// The loop is bounded by the fixture root, which t.TempDir removes on
	// every exit path including a panic — so no cleanup code has to run for
	// this child to die, and no failing path can leak it. It stays alive
	// across the Terminate this test witnesses, which is what it proves.
	child := f.start("/bin/sh", []string{"-c", "trap '' TERM; echo ready > " + ready + "; while test -d " + f.root + "; do sleep 0.1; done"}, nil, out)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	waitFile(t, ready)
	exit, err := child.Terminate(100 * time.Millisecond)
	if err != nil {
		t.Fatal(err)
	}
	if exit.Signal != int(syscall.SIGKILL) {
		t.Fatalf("exit=%+v", exit)
	}
	assertWaitedAndAbsent(t, child)
}

func TestLeaderExitWithLiveDescendantRetainsOwnerOnEPERM(t *testing.T) {
	f := newFixture(t)
	child, release, descendant := startExitedLeaderWithDescendant(t, f)
	defer release.Close()
	cleanupDone := installExactDescendantSafetyCleanup(t, descendant)
	child.testSignal = func(signal unix.Signal) error {
		if signal != unix.SIGKILL {
			return fmt.Errorf("unexpected signal %d", signal)
		}
		return fmt.Errorf("injected group EPERM: %w", classifyGroupSignal(unix.EPERM, false))
	}

	if _, err := child.Terminate(25 * time.Millisecond); !errors.Is(err, ErrUnresolved) || !strings.Contains(fmt.Sprint(err), "injected group EPERM") {
		t.Fatalf("leader-exit EPERM termination=%v", err)
	}
	if child.state != stateExited || !child.exitObserved || child.cmd.ProcessState != nil {
		t.Fatalf("EPERM lost unreaped owner: state=%d observed=%t process_state=%v", child.state, child.exitObserved, child.cmd.ProcessState)
	}
	if observation := ObserveProcess(child.Identity()); observation.Presence != Present {
		t.Fatalf("EPERM lost exact leader: %+v", observation)
	}
	if observation := ObserveProcess(descendant); observation.Presence != Present {
		t.Fatalf("EPERM invented descendant absence: %+v", observation)
	}

	killExactTestDescendant(t, descendant)
	child.testSignal = nil
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 23 || exit.Signal != 0 {
		t.Fatalf("retry after exact safety cleanup=%+v err=%v", exit, err)
	}
	assertWaitedAndAbsent(t, child)
	cleanupDone()
}

func TestLeaderExitRetryConvergesAfterDescendantQuiesces(t *testing.T) {
	f := newFixture(t)
	child, release, descendant := startExitedLeaderWithDescendant(t, f)
	defer release.Close()
	cleanupDone := installExactDescendantSafetyCleanup(t, descendant)
	child.testSignal = func(signal unix.Signal) error {
		if signal != unix.SIGKILL {
			return fmt.Errorf("unexpected signal %d", signal)
		}
		return fmt.Errorf("injected group EPERM: %w", classifyGroupSignal(unix.EPERM, false))
	}
	if _, err := child.Terminate(25 * time.Millisecond); !errors.Is(err, ErrUnresolved) {
		t.Fatalf("live descendant did not retain owner: %v", err)
	}
	descendantExit := exactExitKqueue(t, descendant)
	if _, err := release.Write([]byte{1}); err != nil {
		t.Fatal(err)
	}
	waitKqueueExit(t, descendantExit, descendant)

	child.testSignal = nil
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 23 || exit.Signal != 0 {
		t.Fatalf("retry after natural quiescence=%+v err=%v", exit, err)
	}
	assertWaitedAndAbsent(t, child)
	cleanupDone()
}

func TestTerminateGroupSignalCatchesDescendantForkedDuringTERMGrace(t *testing.T) {
	f := newFixture(t)
	reportReader, reportWriter, err := os.Pipe()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = reportReader.Close() })
	t.Cleanup(func() { _ = reportWriter.Close() })
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	child := f.start(executable, []string{"--owned-descendant-helper", "term-fork", f.root}, nil, reportWriter)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	_ = reportWriter.Close()
	reports := bufio.NewReader(reportReader)
	if line := readOwnedPipeLine(t, reportReader, reports); line != "ready" {
		t.Fatalf("provider readiness=%q", line)
	}
	type terminationResult struct {
		exit Exit
		err  error
	}
	done := make(chan terminationResult, 1)
	go func() {
		exit, err := child.Terminate(2 * time.Second)
		done <- terminationResult{exit: exit, err: err}
	}()
	line := readOwnedPipeLine(t, reportReader, reports)
	pid, err := strconv.Atoi(strings.TrimPrefix(line, "late:"))
	if err != nil || !strings.HasPrefix(line, "late:") {
		t.Fatalf("late descendant report=%q err=%v", line, err)
	}
	late, err := readIdentity(pid)
	if err != nil || late.PGID != child.Identity().PGID {
		t.Fatalf("late descendant identity=%+v err=%v", late, err)
	}
	cleanupDone := installExactDescendantSafetyCleanup(t, late)
	var result terminationResult
	select {
	case result = <-done:
	case <-time.After(7 * time.Second):
		t.Fatal("Terminate did not join after late descendant fork")
	}
	if result.err != nil || result.exit.Signal != int(unix.SIGKILL) {
		t.Fatalf("termination=%+v err=%v", result.exit, result.err)
	}
	waitExactAbsence(t, late)
	assertWaitedAndAbsent(t, child)
	cleanupDone()
}

func TestGroupSignalRequiresExactUnreapedLeader(t *testing.T) {
	f := newFixture(t)
	out := outputFile(t, filepath.Join(f.root, "out"))
	child := f.start("/bin/sh", []string{"-c", "exit 0"}, nil, out)
	id := child.Identity()
	changed := id
	changed.Birth.Microseconds++
	if err := signalOwnedGroup(changed, unix.SIGKILL); !errors.Is(err, ErrUnresolved) {
		t.Fatalf("changed leader authorized group signal: %v", err)
	}
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	if _, err := child.FinishAfterExit(4 * time.Second); err != nil {
		t.Fatal(err)
	}
	if err := signalOwnedGroup(id, unix.SIGKILL); !errors.Is(err, ErrUnresolved) {
		t.Fatalf("reaped leader authorized group signal: %v", err)
	}
}

func TestGroupSignalErrorClassificationFailsClosed(t *testing.T) {
	for _, test := range []struct {
		name   string
		err    error
		noLive bool
		wantOK bool
	}{
		{name: "success", err: nil, wantOK: true},
		{name: "corroborated ESRCH", err: unix.ESRCH, noLive: true, wantOK: true},
		{name: "uncorroborated ESRCH", err: unix.ESRCH},
		// Darwin reports EPERM once our unreaped zombie leader is the only
		// member left, so a converged census must forgive it; a census still
		// reporting life must not, whatever the errno claims.
		{name: "corroborated EPERM", err: unix.EPERM, noLive: true, wantOK: true},
		{name: "uncorroborated EPERM", err: unix.EPERM},
		{name: "EIO", err: unix.EIO, noLive: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			err := classifyGroupSignal(test.err, test.noLive)
			if (err == nil) != test.wantOK {
				t.Fatalf("classification=%v wantOK=%v", err, test.wantOK)
			}
		})
	}
}

func TestProcessCleanupHasNoPerMemberSignalPath(t *testing.T) {
	_, thisFile, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("locate test source")
	}
	fset := token.NewFileSet()
	parsed, err := parser.ParseFile(fset, filepath.Join(filepath.Dir(thisFile), "process_darwin.go"), nil, 0)
	if err != nil {
		t.Fatal(err)
	}
	signals := 0
	ast.Inspect(parsed, func(node ast.Node) bool {
		call, ok := node.(*ast.CallExpr)
		if !ok || len(call.Args) != 2 {
			return true
		}
		selector, ok := call.Fun.(*ast.SelectorExpr)
		if !ok {
			return true
		}
		pkg, pkgOK := selector.X.(*ast.Ident)
		if !pkgOK || pkg.Name != "unix" || selector.Sel.Name != "Kill" {
			return true
		}
		if literal, ok := call.Args[1].(*ast.BasicLit); ok && literal.Kind == token.INT && literal.Value == "0" {
			return true
		}
		signals++
		if negative, ok := call.Args[0].(*ast.UnaryExpr); !ok || negative.Op != token.SUB {
			t.Errorf("nonzero unix.Kill must target the owned negative PGID: %s", fset.Position(call.Pos()))
		}
		return true
	})
	if signals != 1 {
		t.Fatalf("nonzero unix.Kill call count=%d want=1", signals)
	}
}

func TestActivatedWaitPathsConvergeGroupBeforeSoleWait(t *testing.T) {
	_, thisFile, _, ok := runtime.Caller(0)
	if !ok {
		t.Fatal("locate test source")
	}
	fset := token.NewFileSet()
	parsed, err := parser.ParseFile(fset, filepath.Join(filepath.Dir(thisFile), "process_darwin.go"), nil, 0)
	if err != nil {
		t.Fatal(err)
	}
	activatedCallers := map[string]bool{"FinishAfterExit": false, "Terminate": false}
	for _, declaration := range parsed.Decls {
		function, ok := declaration.(*ast.FuncDecl)
		if !ok || function.Body == nil {
			continue
		}
		var convergencePosition, waitPosition token.Pos
		ast.Inspect(function.Body, func(node ast.Node) bool {
			call, ok := node.(*ast.CallExpr)
			if !ok {
				return true
			}
			var called string
			switch target := call.Fun.(type) {
			case *ast.SelectorExpr:
				called = target.Sel.Name
			case *ast.Ident:
				called = target.Name
			}
			switch called {
			case "waitActivatedOnce":
				if _, tracked := activatedCallers[function.Name.Name]; tracked {
					activatedCallers[function.Name.Name] = true
				}
			case "killRemainingGroup":
				if function.Name.Name == "waitActivatedOnce" {
					convergencePosition = call.Pos()
				}
			case "waitOnce":
				if function.Name.Name != "waitActivatedOnce" && function.Name.Name != "waitInertOnce" {
					t.Errorf("%s calls sole Wait without an ownership-specific guard at %s", function.Name.Name, fset.Position(call.Pos()))
				}
				if function.Name.Name == "waitActivatedOnce" {
					waitPosition = call.Pos()
				}
			}
			return true
		})
		if function.Name.Name == "waitActivatedOnce" && (convergencePosition == token.NoPos || waitPosition == token.NoPos || convergencePosition >= waitPosition) {
			t.Fatal("activated Wait is not structurally preceded by group convergence")
		}
	}
	for caller, found := range activatedCallers {
		if !found {
			t.Errorf("%s does not use the activated convergence-and-Wait path", caller)
		}
	}
}

func TestObservationFailsClosed(t *testing.T) {
	bad := []Identity{{}, {PID: 1, PGID: 1, Birth: Birth{Seconds: 1}}, {PID: 2, PGID: 2, Birth: Birth{}}}
	for _, id := range bad {
		if got := ObserveProcess(id); got.Presence != Unknown {
			t.Fatalf("malformed %+v => %+v", id, got)
		}
	}
	self, err := readIdentity(os.Getpid())
	if err != nil {
		t.Fatal(err)
	}
	reused := self
	reused.Birth.Microseconds++
	if got := ObserveProcess(reused); got.Presence != Reused {
		t.Fatalf("reuse=%+v", got)
	}
}

func TestUnavailableProcessClassifierFailsClosed(t *testing.T) {
	tests := []struct {
		name                 string
		info, process, group error
		want                 Presence
	}{
		{name: "EPERM never proves absence", info: unix.EPERM, process: unix.ESRCH, group: unix.ESRCH, want: Unknown},
		{name: "EIO needs process corroboration", info: unix.EIO, process: nil, group: unix.ESRCH, want: Unknown},
		{name: "EIO needs group corroboration", info: unix.EIO, process: unix.ESRCH, group: nil, want: Unknown},
		{name: "EIO with both negative probes", info: unix.EIO, process: unix.ESRCH, group: unix.ESRCH, want: Absent},
		{name: "ESRCH with both negative probes", info: unix.ESRCH, process: unix.ESRCH, group: unix.ESRCH, want: Absent},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			if got := classifyUnavailable(test.info, test.process, test.group); got.Presence != test.want {
				t.Fatalf("classification=%+v want=%s", got, test.want)
			}
		})
	}
}

func TestPrepareRejectsScriptAndPreexistingMarker(t *testing.T) {
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	script := filepath.Join(root, "provider")
	if err := os.WriteFile(script, []byte("#!/bin/sh\nexit 0\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := PrepareExecSpec(ExecSpec{Target: script, Cwd: root}); err == nil {
		t.Fatal("shebang script accepted")
	}
	if err := os.WriteFile(filepath.Join(root, OuterActivationMarkerName), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	dir, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	defer dir.Close()
	lifetime := createTestRuntimeLifetime(t, dir)
	defer lifetime.Close()
	if _, _, err := CreateGateLease(dir, lifetime, OuterActivationMarkerName); !errors.Is(err, os.ErrExist) {
		t.Fatalf("preexisting marker=%v", err)
	}
}

func TestGateLeaseRejectsMissingMalformedAndReplacedLifetime(t *testing.T) {
	for _, mutation := range []string{"missing", "mode", "replacement"} {
		t.Run(mutation, func(t *testing.T) {
			root := t.TempDir()
			if err := os.Chmod(root, 0o700); err != nil {
				t.Fatal(err)
			}
			dir, err := os.Open(root)
			if err != nil {
				t.Fatal(err)
			}
			defer dir.Close()
			if mutation == "missing" {
				if lease, _, err := CreateGateLease(dir, nil, OuterActivationMarkerName); err == nil || lease != nil {
					t.Fatalf("missing lifetime accepted: %v %v", lease, err)
				}
				return
			}
			lifetime := createTestRuntimeLifetime(t, dir)
			defer lifetime.Close()
			path := filepath.Join(root, RuntimeLifetimeLeaseName)
			if mutation == "mode" {
				if err := os.Chmod(path, 0o640); err != nil {
					t.Fatal(err)
				}
			} else {
				if err := os.Rename(path, path+".old"); err != nil {
					t.Fatal(err)
				}
				if err := os.WriteFile(path, nil, 0o600); err != nil {
					t.Fatal(err)
				}
			}
			if lease, _, err := CreateGateLease(dir, lifetime, OuterActivationMarkerName); err == nil || lease != nil {
				t.Fatalf("%s lifetime accepted: %v %v", mutation, lease, err)
			}
			if _, err := os.Stat(filepath.Join(root, OuterActivationMarkerName)); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("%s lifetime created activation effect: %v", mutation, err)
			}
		})
	}
}

func createTestRuntimeLifetimeTest(dir *os.File) *os.File {
	created, err := unix.Openat(int(dir.Fd()), RuntimeLifetimeLeaseName, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0o600)
	if err != nil {
		return nil
	}
	if unix.Fsync(created) != nil || unix.Close(created) != nil {
		return nil
	}
	fd, err := unix.Openat(int(dir.Fd()), RuntimeLifetimeLeaseName, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil
	}
	if err := unix.Flock(fd, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		unix.Close(fd)
		return nil
	}
	return os.NewFile(uintptr(fd), "test-runtime-lifetime")
}

func copyNative(t *testing.T, from, to string) {
	t.Helper()
	source, err := canonical(from)
	if err != nil {
		t.Fatal(err)
	}
	in, err := os.Open(source)
	if err != nil {
		t.Fatal(err)
	}
	defer in.Close()
	out, err := os.OpenFile(to, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o755)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(out, in); err != nil {
		t.Fatal(err)
	}
	if err := out.Close(); err != nil {
		t.Fatal(err)
	}
}
