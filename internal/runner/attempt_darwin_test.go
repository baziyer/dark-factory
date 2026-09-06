//go:build darwin

package runner

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync/atomic"
	"syscall"
	"testing"
	"time"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

var runnerTestWorkerConfig = []byte(`{"kind":"runner-test"}`)

func TestInheritedWorkerConfigIsExactBoundedAndUnlinked(t *testing.T) {
	directory, err := os.Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	defer directory.Close()
	want := []byte(`{"private":"exact"}`)
	input, err := anonymousFile(directory, "stdin", want)
	if err != nil {
		t.Fatal(err)
	}
	defer input.Close()
	got, err := readInheritedWorkerConfig(input)
	if err != nil || !bytes.Equal(got, want) {
		t.Fatalf("worker config=%q err=%v", got, err)
	}

	for name, body := range map[string][]byte{
		"missing":   nil,
		"invalid":   []byte(`{"unterminated":`),
		"oversized": append(append([]byte{'"'}, bytes.Repeat([]byte{'x'}, maxConfigBytes)...), '"'),
	} {
		t.Run(name, func(t *testing.T) {
			input, err := anonymousFile(directory, "stdin", body)
			if err != nil {
				t.Fatal(err)
			}
			defer input.Close()
			if got, err := readInheritedWorkerConfig(input); !errors.Is(err, ErrIdentity) || got != nil {
				t.Fatalf("config=%q err=%v", got, err)
			}
		})
	}

	t.Run("linked substitution", func(t *testing.T) {
		input, err := os.CreateTemp(t.TempDir(), "config")
		if err != nil {
			t.Fatal(err)
		}
		defer input.Close()
		if _, err := input.Write(want); err != nil {
			t.Fatal(err)
		}
		if _, err := input.Seek(0, io.SeekStart); err != nil {
			t.Fatal(err)
		}
		if got, err := readInheritedWorkerConfig(input); !errors.Is(err, ErrIdentity) || got != nil {
			t.Fatalf("linked config=%q err=%v", got, err)
		}
	})

	t.Run("partial", func(t *testing.T) {
		input, err := anonymousFile(directory, "stdin", want)
		if err != nil {
			t.Fatal(err)
		}
		defer input.Close()
		if _, err := input.Seek(1, io.SeekStart); err != nil {
			t.Fatal(err)
		}
		if got, err := readInheritedWorkerConfig(input); !errors.Is(err, ErrIdentity) || got != nil {
			t.Fatalf("partial config=%q err=%v", got, err)
		}
	})
}

func TestWorkerConfigReadIsExactOneShotAndPrecedesSelection(t *testing.T) {
	worker, peer := newWorkerConfigFixture(t)
	if err := worker.ReportSelection(nil); !errors.Is(err, ErrState) {
		t.Fatalf("selection before config=%v", err)
	}
	want := []byte(`{"private":"exact"}`)
	writeRawWorkerConfigFrame(t, peer, want)
	got, err := worker.ReadConfig(maxConfigBytes)
	if err != nil || !bytes.Equal(got, want) {
		t.Fatalf("worker config=%q err=%v want=%q", got, err, want)
	}
	if got, err := worker.ReadConfig(maxConfigBytes); !errors.Is(err, ErrState) || got != nil {
		t.Fatalf("second config=%q err=%v", got, err)
	}
	if err := worker.ReportSelection(nil); err != nil {
		t.Fatalf("selection after config=%v", err)
	}
	var frame attemptFrame
	if err := readFrame(peer, &frame, maxConfigBytes); err != nil {
		t.Fatal(err)
	}
	if !validCheckpointFrame(frame, StageSelection) {
		t.Fatalf("selection frame=%+v", frame)
	}
}

func TestWorkerConfigMalformedFrameCannotBeRetried(t *testing.T) {
	for _, test := range []struct {
		name string
		send func(*testing.T, *os.File)
	}{
		{name: "missing", send: func(t *testing.T, peer *os.File) {
			if err := peer.Close(); err != nil {
				t.Fatal(err)
			}
		}},
		{name: "oversized", send: func(t *testing.T, peer *os.File) {
			var header [4]byte
			binary.BigEndian.PutUint32(header[:], maxConfigBytes+1)
			if err := writeFully(peer, header[:]); err != nil {
				t.Fatal(err)
			}
		}},
		{name: "partial", send: func(t *testing.T, peer *os.File) {
			var header [4]byte
			binary.BigEndian.PutUint32(header[:], uint32(len(runnerTestWorkerConfig)))
			if err := writeFully(peer, append(header[:], runnerTestWorkerConfig[:3]...)); err != nil {
				t.Fatal(err)
			}
			if err := peer.Close(); err != nil {
				t.Fatal(err)
			}
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			worker, peer := newWorkerConfigFixture(t)
			test.send(t, peer)
			if config, err := worker.ReadConfig(maxConfigBytes); err == nil || config != nil {
				t.Fatalf("malformed config=%q err=%v", config, err)
			}
			if config, err := worker.ReadConfig(maxConfigBytes); !errors.Is(err, ErrState) || config != nil {
				t.Fatalf("retried config=%q err=%v", config, err)
			}
		})
	}
}

func newWorkerConfigFixture(t *testing.T) (*WorkerControl, *os.File) {
	t.Helper()
	workerSide, peer, err := newControlPair("worker-config", "worker-config-peer")
	if err != nil {
		t.Fatal(err)
	}
	worker := &WorkerControl{file: workerSide, state: workerConfig}
	t.Cleanup(func() {
		_ = worker.Close()
		_ = peer.Close()
	})
	return worker, peer
}

func writeRawWorkerConfigFrame(t *testing.T, peer *os.File, body []byte) {
	t.Helper()
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(body)))
	if err := writeFully(peer, append(header[:], body...)); err != nil {
		t.Fatal(err)
	}
}

func TestAttemptConfigureFreezesAndRejectsStartupInput(t *testing.T) {
	root := t.TempDir()
	wrapper, err := PrepareExecSpec(ExecSpec{
		Target: "/bin/sh", Args: []string{"-c", "exit 0"},
		Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: root,
	})
	if err != nil {
		t.Fatal(err)
	}
	base := AttemptSpec{
		AttemptID: "startup-input", Wrapper: wrapper,
		MarkerName: InnerActivationMarkerName, ResultName: AttemptResultSpoolName,
		ResultProof: testResultProof(),
	}

	startup := []byte("exact startup\n")
	controller, peer, err := NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		_ = controller.Close()
		_ = peer.Close()
	})
	spec := base
	spec.StartupInput = startup
	if err := controller.Configure(spec); err != nil {
		t.Fatal(err)
	}
	startup[0] = 'X'
	var frozen attemptConfig
	if err := readFrame(peer, &frozen, maxConfigBytes); err != nil {
		t.Fatal(err)
	}
	if got, want := string(frozen.StartupInput), "exact startup\n"; got != want {
		t.Fatalf("frozen startup input=%q want=%q", got, want)
	}
	maximum := frozen
	maximum.StartupInput = bytes.Repeat([]byte{'x'}, MaxProviderTaskBytes)
	if err := validateAttemptConfig(maximum); err != nil {
		t.Fatalf("maximum startup input rejected: %v", err)
	}

	for name, input := range map[string][]byte{
		"oversized":     bytes.Repeat([]byte{'x'}, MaxProviderTaskBytes+1),
		"invalid UTF-8": {0xff},
		"NUL":           {'x', 0},
	} {
		t.Run(name, func(t *testing.T) {
			controller, peer, err := NewAttemptController()
			if err != nil {
				t.Fatal(err)
			}
			defer controller.Close()
			defer peer.Close()
			spec := base
			spec.StartupInput = input
			if err := controller.Configure(spec); !errors.Is(err, ErrState) {
				t.Fatalf("invalid startup input error=%v want ErrState", err)
			}
			recovered := frozen
			recovered.StartupInput = input
			if err := validateAttemptConfig(recovered); !errors.Is(err, ErrIdentity) {
				t.Fatalf("invalid recovered startup input error=%v want ErrIdentity", err)
			}
		})
	}
}

func shellPIDWitness(pid, path string) string {
	pending := path + ".pending"
	return fmt.Sprintf("printf '%%s' %s > %q && mv %q %q", pid, pending, pending, path)
}

func runAttemptWorkerHelper(args []string) error {
	if len(args) < 2 {
		return errors.New("attempt worker: missing mode/root")
	}
	mode, root := args[0], args[1]
	control, err := OpenWorkerControl()
	if err != nil {
		return err
	}
	defer control.Close()
	config, err := control.ReadConfig(maxConfigBytes)
	if err != nil {
		return err
	}
	if !bytes.Equal(config, runnerTestWorkerConfig) {
		return fmt.Errorf("attempt worker: config mismatch")
	}
	runtimeDirectory, err := control.DuplicateRuntimeDirectory(context.Background())
	if err != nil {
		return err
	}
	var runtimeStat unix.Stat_t
	flags, flagErr := unix.FcntlInt(runtimeDirectory.Fd(), unix.F_GETFD, 0)
	statErr := unix.Fstat(int(runtimeDirectory.Fd()), &runtimeStat)
	closeErr := runtimeDirectory.Close()
	if flagErr != nil || statErr != nil || closeErr != nil || flags&unix.FD_CLOEXEC == 0 || runtimeStat.Mode&unix.S_IFMT != unix.S_IFDIR || runtimeStat.Uid != uint32(os.Geteuid()) || runtimeStat.Mode&0o7777 != 0o700 {
		return fmt.Errorf("attempt worker: runtime duplicate: %w", errors.Join(flagErr, statErr, closeErr, ErrIdentity))
	}
	write := func(name, value string) error {
		f, err := os.OpenFile(filepath.Join(root, name), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			return err
		}
		if _, err := io.WriteString(f, value); err != nil {
			f.Close()
			return err
		}
		return f.Close()
	}
	if err := write("selection", fmt.Sprintf("%d", control.Identity().PID)); err != nil {
		return err
	}
	if mode == "loud-selection" {
		// The macOS PTY output buffer is 1024 bytes. A worker that says more
		// than that before its stage report blocks in write(2) unless the
		// outer runner is already draining the master.
		if _, err := os.Stderr.Write([]byte(strings.Repeat("d", 8192))); err != nil {
			return err
		}
	}
	if err := control.ReportSelection([]byte("selected")); err != nil {
		return err
	}
	if err := control.AwaitPreparation(); err != nil {
		return err
	}
	if err := write("preparation", "prepared"); err != nil {
		return err
	}
	if err := control.ReportPreparation([]byte("prepared")); err != nil {
		return err
	}
	if err := control.AwaitPopulation(); err != nil {
		return err
	}
	if err := write("population", "populated"); err != nil {
		return err
	}
	providerCwd := filepath.Join(root, "work")
	var provider ExecSpec
	var providerTask []byte
	switch mode {
	case "native-input":
		script := fmt.Sprintf("test ! -e /dev/fd/11 || exit 97; IFS= read -r startup || exit 98; printf '%%s' \"$startup\" > %q || exit 99; IFS= read -r interactive || exit 100; printf '%%s' \"$interactive\" > %q || exit 101; while test ! -f %q; do sleep 0.01; done", filepath.Join(root, "provider.startup"), filepath.Join(root, "provider.stdin"), filepath.Join(root, "finish"))
		provider = ExecSpec{Target: "/bin/sh", Args: []string{"-c", script}, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: providerCwd}
	case "shell", "shell-input", "term", "leader", "tail", "reply", "loud-adoption":
		providerWitness := shellPIDWitness("$$", filepath.Join(root, "provider.pid"))
		script := fmt.Sprintf("test -z \"${HOME+x}\" || exit 90; test -z \"${DARK_FACTORY_ATTEMPT_TOKEN+x}\" || exit 91; for n in 3 4 5 6 7 8 9; do test ! -e /dev/fd/$n || exit 92; done; test -f /dev/fd/10 || exit 93; test ! -s /dev/fd/10 || exit 94; test -f /dev/fd/11 || exit 97; IFS= read -r task < /dev/fd/11; test \"$task\" = one-startup || exit 98; cat /dev/fd/10/change-worker.config >/dev/null 2>&1 && exit 95; cd /dev/fd/10 >/dev/null 2>&1 && exit 96; %s; printf 'pre-output\\n'; while test ! -f %q; do sleep 0.01; done; printf 'post-output\\n'; printf x >> %q; while test ! -f %q; do sleep 0.01; done", providerWitness, filepath.Join(root, "continue"), filepath.Join(root, "provider.effect"), filepath.Join(root, "finish"))
		if mode == "shell-input" {
			script += fmt.Sprintf("; IFS= read -r line; printf '%%s' \"$line\" > %q", filepath.Join(root, "provider.stdin"))
		}
		if mode == "term" {
			script = fmt.Sprintf("trap '' TERM; sleep 30 & %s; %s; while :; do sleep 1; done", shellPIDWitness("$!", filepath.Join(root, "descendant.pid")), providerWitness)
		}
		if mode == "leader" {
			script = fmt.Sprintf("sleep 30 & %s; %s; while test ! -f %q; do sleep 0.01; done; exit 0", shellPIDWitness("$!", filepath.Join(root, "descendant.pid")), providerWitness, filepath.Join(root, "leader.release"))
		}
		if mode == "tail" {
			script = "printf 'tail-output\\n'; exit 0"
		}
		if mode == "reply" {
			script = fmt.Sprintf("%s; printf 'pre-output\\n'; while test ! -f %q; do sleep 0.01; done; printf 'post-output\\n'; stty -icanon -echo min 1 time 0; dd if=/dev/stdin bs=12 count=1 2>/dev/null > %q; while test ! -f %q; do sleep 0.01; done", providerWitness, filepath.Join(root, "continue"), filepath.Join(root, "provider.reply"), filepath.Join(root, "finish"))
			providerTask = nil
		} else {
			providerTask = []byte("one-startup\n")
		}
		provider = ExecSpec{Target: "/bin/sh", Args: []string{"-c", script}, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: providerCwd}
	case "binary", "seam", "lifetime", "lease-seam", "proof-census", "cwd", "cwd-seam", "cwd-unrelated", "cwd-file", "cwd-closed", "cwd-reused", "cwd-mode", "cwd-inherited", "cwd-inherited-11":
		if len(args) != 3 {
			return errors.New("attempt worker: missing binary target")
		}
		providerArgs := []string{"--attempt-provider", filepath.Join(root, "provider.effect")}
		if mode == "lifetime" {
			providerArgs = []string{"--lifetime-provider", root}
		}
		if mode == "proof-census" {
			providerArgs = []string{"--proof-provider", root}
		}
		if strings.HasPrefix(mode, "cwd") {
			providerCwd = filepath.Join(root, "change", "work")
		}
		if mode == "cwd" || mode == "cwd-seam" || strings.HasPrefix(mode, "cwd-inherited") {
			providerArgs = []string{"--cwd-provider", root}
		}
		provider = ExecSpec{Target: args[2], Args: providerArgs, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: providerCwd}
	default:
		return fmt.Errorf("attempt worker: invalid mode %q", mode)
	}
	prepared, err := PrepareExecSpec(provider)
	if err != nil {
		return err
	}
	if mode == "seam" || mode == "lease-seam" || mode == "cwd-seam" {
		prepared.testCurrentFinal = true
	}
	cwdPath := providerCwd
	if mode == "cwd-unrelated" {
		cwdPath = filepath.Join(root, "unrelated")
	}
	if mode == "cwd-file" {
		cwdPath = filepath.Join(root, "cwd-file")
	}
	cwd, err := os.Open(cwdPath)
	if err != nil {
		return err
	}
	if strings.HasPrefix(mode, "cwd-inherited") {
		fd := 20
		if mode != "cwd-inherited" {
			parsed, err := strconv.Atoi(strings.TrimPrefix(mode, "cwd-inherited-"))
			if err != nil || parsed <= 2 {
				cwd.Close()
				return errors.New("attempt worker: invalid inherited descriptor")
			}
			fd = parsed
		}
		if err := unix.Dup2(int(cwd.Fd()), fd); err != nil {
			cwd.Close()
			return err
		}
		if _, err := unix.FcntlInt(uintptr(fd), unix.F_SETFD, 0); err != nil {
			unix.Close(fd)
			cwd.Close()
			return err
		}
	}
	if mode == "cwd-closed" {
		if err := cwd.Close(); err != nil {
			return err
		}
	}
	if mode == "cwd-reused" {
		raw := int(cwd.Fd())
		if err := unix.Close(raw); err != nil {
			return err
		}
		replacementPath, err := filepath.EvalSymlinks(filepath.Join(root, "unrelated"))
		if err != nil {
			return err
		}
		replacement, err := unix.Open(replacementPath, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW_ANY, 0)
		if err != nil {
			return err
		}
		if replacement != raw {
			if err := unix.Dup2(replacement, raw); err != nil {
				unix.Close(replacement)
				return err
			}
			if err := unix.Close(replacement); err != nil {
				return err
			}
		}
	}
	if mode == "cwd-mode" {
		if err := os.Chmod(providerCwd, 0o755); err != nil {
			cwd.Close()
			return err
		}
	}
	if err := control.ReportPopulation([]byte("populated")); err != nil {
		cwd.Close()
		return err
	}
	if err := control.AwaitProvider(); err != nil {
		cwd.Close()
		return err
	}
	if mode == "loud-adoption" {
		// Exactly the window where the stage sink hands its retained ring to
		// the terminal owner. These bytes must survive that adoption.
		if _, err := os.Stderr.Write(adoptionWindowPayload()); err != nil {
			cwd.Close()
			return err
		}
	}
	if strings.HasPrefix(mode, "cwd") {
		if err := writeCwdDescriptorManifest(root); err != nil {
			cwd.Close()
			return err
		}
	}
	var task *os.File
	if mode != "native-input" {
		if len(providerTask) == 0 {
			providerTask = []byte("test-provider-task\n")
		}
		task, err = createUnlinkedProviderTask(root, providerTask)
		if err != nil {
			cwd.Close()
			return err
		}
	}
	return control.ExecProvider(prepared, cwd, task)
}

func createUnlinkedProviderTask(root string, body []byte) (*os.File, error) {
	path := filepath.Join(root, ".test-provider-task")
	writer, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return nil, err
	}
	n, writeErr := writer.Write(body)
	syncErr := writer.Sync()
	closeErr := writer.Close()
	if writeErr != nil || n != len(body) || syncErr != nil || closeErr != nil {
		_ = os.Remove(path)
		return nil, errors.Join(writeErr, syncErr, closeErr, io.ErrShortWrite)
	}
	reader, err := os.Open(path)
	if err != nil {
		_ = os.Remove(path)
		return nil, err
	}
	if err := os.Remove(path); err != nil {
		_ = reader.Close()
		return nil, err
	}
	return reader, nil
}

func runRetirementProviderHelper(args []string) error {
	if len(args) != 3 {
		return errors.New("retirement provider: missing witnesses or delay")
	}
	delayMillis, err := strconv.Atoi(args[2])
	if err != nil || delayMillis < 1 || delayMillis > 5000 {
		return errors.New("retirement provider: invalid delay")
	}
	signal.Ignore(unix.SIGTERM)
	write := func(path string) error {
		file, err := os.OpenFile(path, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
		if err != nil {
			return err
		}
		return file.Close()
	}
	if err := write(args[0]); err != nil {
		return err
	}
	time.Sleep(time.Duration(delayMillis) * time.Millisecond)
	return write(args[1])
}

type attemptFixture struct {
	t          *testing.T
	root       string
	dir        *os.File
	lifetime   *os.File
	lease      *GateLease
	controller *AttemptController
	spec       AttemptSpec
	childCap   *os.File
	outer      *OwnedChild
	inner      Identity
	diagnostic *os.File
}

func newAttemptFixture(t *testing.T, mode string, target string) *attemptFixture {
	t.Helper()
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(root, "work"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(filepath.Join(root, "change", "work"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(root, "unrelated"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "cwd-file"), nil, 0o600); err != nil {
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
	controller, childCap, err := NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	workerArgs := []string{"--attempt-worker", mode, root}
	if target != "" {
		workerArgs = append(workerArgs, target)
	}
	wrapper, err := PrepareExecSpec(ExecSpec{Target: executable, Args: workerArgs, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: filepath.Join(root, "work")})
	if err != nil {
		t.Fatal(err)
	}
	attemptSpec := AttemptSpec{AttemptID: "attempt-1", Wrapper: wrapper, MarkerName: InnerActivationMarkerName, ResultName: AttemptResultSpoolName, ResultProof: testResultProof()}
	diagnostic := outputFile(t, filepath.Join(root, "runner.output"))
	outerSpec, err := PrepareExecSpec(ExecSpec{Target: executable, Args: []string{"--attempt-runner"}, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: filepath.Join(root, "work"), Stdin: runnerTestWorkerConfig, Stdout: diagnostic, Stderr: diagnostic, Control: childCap})
	if err != nil {
		t.Fatal(err)
	}
	outer, err := StartBlocked(lease, executable, outerSpec, true)
	if err != nil {
		t.Fatalf("outer StartBlocked: %v", err)
	}
	_ = childCap.Close()
	f := &attemptFixture{t: t, root: root, dir: dir, lifetime: lifetime, lease: lease, controller: controller, spec: attemptSpec, outer: outer, diagnostic: diagnostic}
	t.Cleanup(func() {
		_ = controller.Close()
		if f.inner.Valid() {
			if got, identityErr := readIdentity(f.inner.PID); identityErr == nil && got == f.inner {
				_ = unix.Kill(-f.inner.PGID, unix.SIGKILL)
			}
		}
		_ = outer.Close()
		_ = lease.Close()
		_ = lifetime.Close()
		_ = dir.Close()
	})
	return f
}

func (f *attemptFixture) activateOuter() Identity {
	f.t.Helper()
	if _, err := os.Stat(filepath.Join(f.root, "selection")); !errors.Is(err, os.ErrNotExist) {
		f.t.Fatal("wrapper effect before outer activation")
	}
	if _, err := f.outer.Activate(); err != nil {
		f.t.Fatal(err)
	}
	if err := f.controller.Configure(f.spec); err != nil {
		f.t.Fatal(err)
	}
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptInnerReady || !event.Identity.Valid() {
		f.t.Fatalf("inner ready=%+v err=%v output=%q", event, err, f.output())
	}
	f.inner = event.Identity
	if _, err := os.Stat(filepath.Join(f.root, "selection")); !errors.Is(err, os.ErrNotExist) {
		f.t.Fatal("wrapper effect before selection release")
	}
	return event.Identity
}

func (f *attemptFixture) advanceToProvider() {
	f.t.Helper()
	steps := []struct {
		release AttemptStage
		report  AttemptStage
		witness string
	}{
		{StageSelection, StageSelection, "selection"},
		{StagePreparation, StagePreparation, "preparation"},
		{StagePopulation, StagePopulation, "population"},
	}
	for _, step := range steps {
		if err := f.controller.Release(step.release); err != nil {
			f.t.Fatalf("release %s: %v", step.release, err)
		}
		event, err := f.controller.Next(4 * time.Second)
		if err != nil || event.Kind != AttemptCheckpoint || event.Stage != step.report {
			f.t.Fatalf("checkpoint %s event=%+v err=%v output=%q", step.report, event, err, f.output())
		}
		if _, err := os.Stat(filepath.Join(f.root, step.witness)); err != nil {
			f.t.Fatalf("missing %s witness: %v", step.witness, err)
		}
	}
}

func (f *attemptFixture) finishAndAck(expectOuterSuccess ...bool) *TerminalRecord {
	f.t.Helper()
	wantOuterSuccess := true
	if len(expectOuterSuccess) != 0 {
		wantOuterSuccess = expectOuterSuccess[0]
	}
	var event AttemptEvent
	var err error
	for {
		event, err = f.controller.Next(6 * time.Second)
		if err != nil || event.Kind != AttemptTerminalFrame {
			break
		}
	}
	if err != nil || event.Kind != AttemptResultReady || event.Result == nil {
		f.t.Fatalf("result event=%+v err=%v output=%q", event, err, f.output())
	}
	record := loadAttemptResultForTest(f.t, f.dir, f.spec.AttemptID, event.Result)
	if record.Digest != event.Result.Digest || record.Identity != event.Result.Identity {
		f.t.Fatalf("durable result=%+v event=%+v", record, event.Result)
	}
	// The authenticated result is published only after group convergence and
	// the sole Wait. Do not probe the numeric PGID after Wait, when it may
	// already belong to an unrelated process group.
	exit, err := f.outer.FinishAfterExit(6 * time.Second)
	if err != nil || wantOuterSuccess && exit.Code != 0 {
		f.t.Fatalf("outer exit=%+v err=%v output=%q", exit, err, f.output())
	}
	if _, err := os.Stat(filepath.Join(f.root, AttemptResultSpoolName)); err != nil {
		f.t.Fatalf("runner removed result without Store authority: %v", err)
	}
	return record
}

func loadAttemptResultForTest(t *testing.T, dir *os.File, attemptID string, notice *AttemptResultNotice) *TerminalRecord {
	t.Helper()
	record, err := AuthenticateAttemptResult(dir, attemptID, notice)
	if err != nil {
		t.Fatal(err)
	}
	result := record.Result()
	process, ok := result.Process()
	if !ok {
		return &TerminalRecord{Terminal: Terminal{AttemptID: result.AttemptID()}, Identity: record.Notice().Identity, Digest: record.Notice().Digest}
	}
	exit := Exit{Code: -1}
	if code, ok := result.Code(); ok {
		exit.Code = code
	} else if signal, ok := result.Signal(); ok {
		exit.Signal = signal
	}
	return &TerminalRecord{Terminal: Terminal{AttemptID: result.AttemptID(), Process: process, Exit: exit}, Identity: record.Notice().Identity, Digest: record.Notice().Digest}
}

func (f *attemptFixture) output() string {
	_ = f.diagnostic.Sync()
	_, _ = f.diagnostic.Seek(0, 0)
	body, _ := io.ReadAll(f.diagnostic)
	_, _ = f.diagnostic.Seek(0, 2)
	return string(body)
}

func (f *attemptFixture) nextTerminal(kind TerminalEventKind, correlation uint64) TerminalFrame {
	f.t.Helper()
	for {
		event, err := f.controller.Next(4 * time.Second)
		if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil {
			f.t.Fatalf("terminal %s event=%+v err=%v output=%q", kind, event, err, f.output())
		}
		if event.Frame.Kind == kind && event.Frame.Correlation == correlation {
			return *event.Frame
		}
	}
}

func TestAttemptRunnerOrdersOuterWrapperAndShellProvider(t *testing.T) {
	f := newAttemptFixture(t, "shell-input", "")
	outer := f.outer.Identity()
	inner := f.activateOuter()
	if inner == outer || inner.PID == outer.PID || inner.PGID == outer.PGID {
		t.Fatalf("ownership boundaries collapsed outer=%+v inner=%+v", outer, inner)
	}
	if err := f.controller.Release(StagePreparation); !errors.Is(err, ErrState) {
		t.Fatalf("out-of-order release accepted: %v", err)
	}
	f.advanceToProvider()
	if err := f.controller.Release(StagePopulation); !errors.Is(err, ErrState) {
		t.Fatalf("replayed release accepted: %v", err)
	}
	if _, err := os.Stat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider effect before provider release")
	}
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	if ready := f.nextTerminal(TerminalReady, 0); ready.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v", ready)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalGenerationInstall, Correlation: 1, Generation: 1}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalGenerationResult, 1); result.Status != TerminalResultOK {
		t.Fatalf("generation install=%+v", result)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalInput, Correlation: 2, Generation: 1, Sequence: 1, Payload: []byte("interactive-after-ready\n")}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalInputResult, 2); result.Status != TerminalResultOK || result.Count != uint32(len("interactive-after-ready\n")) {
		t.Fatalf("terminal input=%+v", result)
	}
	waitFile(t, filepath.Join(f.root, "provider.pid"))
	body, err := os.ReadFile(filepath.Join(f.root, "provider.pid"))
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(string(body))
	if err != nil || pid != inner.PID {
		t.Fatalf("provider pid=%q err=%v want=%d", body, err, inner.PID)
	}
	if got, err := readIdentity(pid); err != nil || got != inner {
		t.Fatalf("provider identity=%+v err=%v want=%+v", got, err, inner)
	}
	if err := os.WriteFile(filepath.Join(f.root, "continue"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Process != inner || record.Terminal.Exit.Code != 0 || record.Terminal.Exit.Signal != 0 {
		t.Fatalf("terminal=%+v", record.Terminal)
	}
	if body, err := os.ReadFile(filepath.Join(f.root, "provider.effect")); err != nil || string(body) != "x" {
		t.Fatalf("effect=%q err=%v", body, err)
	}
	if body, err := os.ReadFile(filepath.Join(f.root, "provider.stdin")); err != nil || string(body) != "interactive-after-ready" {
		t.Fatalf("stdin=%q err=%v", body, err)
	}
}

// A startup prompt ending in CR reaches the provider as its text first and
// the CR later, as a keystroke of its own once the prompt is quiet: the
// provider's line completes no earlier than the floor after ready, and
// interactive input still follows in order.
func TestAttemptRunnerSubmitsStartupCarriageReturnAfterThePromptIsQuiet(t *testing.T) {
	f := newAttemptFixture(t, "native-input", "")
	f.spec.StartupInput = []byte("native-startup\r")
	inner := f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	if ready := f.nextTerminal(TerminalReady, 0); ready.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v", ready)
	}
	readyAt := time.Now()
	startup := filepath.Join(f.root, "provider.startup")
	waitFile(t, startup)
	if body, err := os.ReadFile(startup); err != nil || string(body) != "native-startup" {
		t.Fatalf("provider.startup=%q err=%v", body, err)
	}
	if elapsed := time.Since(readyAt); elapsed < startupEnterFloor-50*time.Millisecond {
		t.Fatalf("startup line completed %v after ready, before the %v floor", elapsed, startupEnterFloor)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalGenerationInstall, Correlation: 1, Generation: 1}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalGenerationResult, 1); result.Status != TerminalResultOK {
		t.Fatalf("generation install=%+v", result)
	}
	interactive := []byte("interactive-after-ready\n")
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalInput, Correlation: 2, Generation: 1, Sequence: 1, Payload: interactive}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalInputResult, 2); result.Status != TerminalResultOK {
		t.Fatalf("terminal input=%+v", result)
	}
	stdin := filepath.Join(f.root, "provider.stdin")
	waitFile(t, stdin)
	if body, err := os.ReadFile(stdin); err != nil || string(body) != "interactive-after-ready" {
		t.Fatalf("provider.stdin=%q err=%v", body, err)
	}
	if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Process != inner || record.Terminal.Exit.Code != 0 {
		t.Fatalf("terminal=%+v", record.Terminal)
	}
}

func TestAttemptRunnerInjectsNativeStartupOnceBeforeInteractiveInput(t *testing.T) {
	f := newAttemptFixture(t, "native-input", "")
	f.spec.StartupInput = []byte("native-startup\n")
	inner := f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	if ready := f.nextTerminal(TerminalReady, 0); ready.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v", ready)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalGenerationInstall, Correlation: 1, Generation: 1}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalGenerationResult, 1); result.Status != TerminalResultOK {
		t.Fatalf("generation install=%+v", result)
	}
	interactive := []byte("interactive-after-ready\n")
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalInput, Correlation: 2, Generation: 1, Sequence: 1, Payload: interactive}); err != nil {
		t.Fatal(err)
	}
	if result := f.nextTerminal(TerminalInputResult, 2); result.Status != TerminalResultOK || result.Count != uint32(len(interactive)) {
		t.Fatalf("terminal input=%+v", result)
	}
	for path, want := range map[string]string{
		"provider.startup": "native-startup",
		"provider.stdin":   "interactive-after-ready",
	} {
		path := filepath.Join(f.root, path)
		waitFile(t, path)
		if body, err := os.ReadFile(path); err != nil || string(body) != want {
			t.Fatalf("%s=%q err=%v want=%q", filepath.Base(path), body, err, want)
		}
	}
	if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Process != inner || record.Terminal.Exit.Code != 0 || record.Terminal.Exit.Signal != 0 {
		t.Fatalf("terminal=%+v", record.Terminal)
	}
}

func TestAttemptRunnerPTYTerminalOwnerCommandsAndReplay(t *testing.T) {
	f := newAttemptFixture(t, "shell", "")
	inner := f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	waitFile(t, filepath.Join(f.root, "provider.pid"))
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v err=%v", event, err)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalGenerationInstall, Correlation: 1, Generation: 1}); err != nil {
		t.Fatal(err)
	}
	event, err = f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalGenerationResult || event.Frame.Status != TerminalResultOK {
		t.Fatalf("generation install=%+v err=%v", event, err)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalAttach, Correlation: 2, Sequence: 0}); err != nil {
		t.Fatal(err)
	}
	event, err = f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalAttached || event.Frame.Status != TerminalResultOK {
		t.Fatalf("attach=%+v err=%v", event, err)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalCredit, Credit: 256}); err != nil {
		t.Fatal(err)
	}
	outputSeen := false
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalResize, Correlation: 3, Generation: 1, Cols: 80, Rows: 24}); err != nil {
		t.Fatal(err)
	}
	for {
		event, err = f.controller.Next(4 * time.Second)
		if err != nil {
			t.Fatalf("resize=%+v err=%v", event, err)
		}
		if event.Kind != AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("resize=%+v", event)
		}
		if event.Frame.Kind == TerminalOutput {
			outputSeen = outputSeen || strings.Contains(string(event.Frame.Payload), "output")
		}
		if event.Frame.Kind == TerminalResizeResult {
			if event.Frame.Status != TerminalResultOK {
				t.Fatalf("resize=%+v", event)
			}
			break
		}
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalInput, Correlation: 4, Generation: 1, Sequence: 1, Payload: []byte("interactive\n")}); err != nil {
		t.Fatal(err)
	}
	for {
		event, err = f.controller.Next(4 * time.Second)
		if err != nil {
			t.Fatalf("input=%+v err=%v", event, err)
		}
		if event.Kind != AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("input=%+v", event)
		}
		if event.Frame.Kind == TerminalOutput {
			outputSeen = outputSeen || strings.Contains(string(event.Frame.Payload), "output")
		}
		if event.Frame.Kind == TerminalInputResult {
			if event.Frame.Status != TerminalResultOK || event.Frame.Count != uint32(len("interactive\n")) {
				t.Fatalf("input=%+v", event)
			}
			break
		}
	}
	if err := os.WriteFile(filepath.Join(f.root, "continue"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	finishSent := false
	var eofSeen bool
	for {
		event, err = f.controller.Next(6 * time.Second)
		if err != nil {
			t.Fatalf("terminal stream event=%+v err=%v output=%t diagnostic=%q", event, err, outputSeen, f.output())
		}
		if event.Kind == AttemptResultReady {
			break
		}
		if event.Kind != AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("unexpected terminal stream event=%+v", event)
		}
		switch event.Frame.Kind {
		case TerminalOutput:
			if eofSeen {
				t.Fatal("terminal output followed PTY EOF")
			}
			outputSeen = outputSeen || strings.Contains(string(event.Frame.Payload), "output")
			if !finishSent {
				if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
					t.Fatal(err)
				}
				finishSent = true
			}
		case TerminalPTYEOF:
			eofSeen = true
		}
	}
	record := loadAttemptResultForTest(t, f.dir, f.spec.AttemptID, event.Result)
	if !outputSeen || !eofSeen || record.Terminal.Process != inner {
		t.Fatalf("terminal evidence output=%t eof=%t event=%+v", outputSeen, eofSeen, event)
	}
	if _, err := f.outer.FinishAfterExit(6 * time.Second); err != nil {
		t.Fatal(err)
	}
}

func TestAttemptRunnerHumanReplyWritesExactBytesOnce(t *testing.T) {
	f := newAttemptFixture(t, "reply", "")
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	if event, err := f.controller.Next(4 * time.Second); err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v err=%v", event, err)
	}

	// HumanRequest delivery has its own daemon authority. It must not require
	// a browser generation or sequence and must reach the PTY byte-for-byte.
	payload := []byte("human-reply!")
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalHumanReply, Correlation: 41, Payload: payload}); err != nil {
		t.Fatal(err)
	}
	for {
		event, err := f.controller.Next(4 * time.Second)
		if err != nil {
			t.Fatalf("human reply=%+v err=%v", event, err)
		}
		if event.Kind != AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("human reply=%+v", event)
		}
		if event.Frame.Kind != TerminalHumanReplyResult {
			continue
		}
		if event.Frame.Correlation != 41 || event.Frame.Status != TerminalResultOK || event.Frame.Count != uint32(len(payload)) || event.Frame.Generation != 0 || event.Frame.Sequence != 0 {
			t.Fatalf("human reply result=%+v", event.Frame)
		}
		break
	}

	if err := os.WriteFile(filepath.Join(f.root, "continue"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	replyPath := filepath.Join(f.root, "provider.reply")
	deadline := time.Now().Add(4 * time.Second)
	var received []byte
	var err error
	for {
		received, err = os.ReadFile(replyPath)
		if err == nil && bytes.Equal(received, payload) {
			break
		}
		if time.Now().After(deadline) {
			t.Fatalf("provider received %q err=%v, want exact %q", received, err, payload)
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	f.finishAndAck()
}

func TestAttemptRunnerPTYEOFFollowsQueuedTailOutput(t *testing.T) {
	f := newAttemptFixture(t, "tail", "")
	inner := f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v err=%v", event, err)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalCredit, Credit: 256}); err != nil {
		t.Fatal(err)
	}
	outputSeen, eofSeen := false, false
	for {
		event, err = f.controller.Next(6 * time.Second)
		if err != nil {
			t.Fatalf("tail stream event=%+v err=%v output=%t eof=%t diagnostic=%q", event, err, outputSeen, eofSeen, f.output())
		}
		if event.Kind == AttemptResultReady {
			break
		}
		if event.Kind != AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("unexpected tail stream event=%+v", event)
		}
		switch event.Frame.Kind {
		case TerminalOutput:
			if eofSeen {
				t.Fatal("terminal output followed PTY EOF")
			}
			outputSeen = outputSeen || strings.Contains(string(event.Frame.Payload), "tail-output")
		case TerminalPTYEOF:
			eofSeen = true
		}
	}
	record := loadAttemptResultForTest(t, f.dir, f.spec.AttemptID, event.Result)
	if !outputSeen || !eofSeen || record.Terminal.Process != inner {
		t.Fatalf("tail evidence output=%t eof=%t result=%+v", outputSeen, eofSeen, record)
	}
	if _, err := f.outer.FinishAfterExit(6 * time.Second); err != nil {
		t.Fatal(err)
	}
}

func TestRuntimeLifetimeRemainsHeldAcrossOuterAndInnerOwnership(t *testing.T) {
	f := newAttemptFixture(t, "shell", "")
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	waitFile(t, filepath.Join(f.root, "provider.pid"))
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
		t.Fatalf("attempt runner lost inherited lifetime lease: %v", err)
	}
	if err := os.WriteFile(filepath.Join(f.root, "continue"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(f.root, "finish"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	var event AttemptEvent
	for {
		event, err = f.controller.Next(6 * time.Second)
		if err != nil || event.Kind != AttemptTerminalFrame {
			break
		}
	}
	if err != nil || event.Kind != AttemptResultReady || event.Result == nil {
		t.Fatalf("terminal=%+v err=%v output=%q", event, err, f.output())
	}
	if exit, err := f.outer.FinishAfterExit(6 * time.Second); err != nil || exit.Code != 0 {
		t.Fatalf("outer exit=%+v err=%v", exit, err)
	}
	if err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB); err != nil {
		t.Fatalf("attempt runner retained lease after exit: %v", err)
	}
	if err := unix.Flock(probe, unix.LOCK_UN); err != nil {
		t.Fatal(err)
	}
}

func TestProviderRetainsLeastPrivilegeLifetimeAfterOuterSIGKILL(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "lifetime", executable)
	inner := f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	readyPath := filepath.Join(f.root, "provider.ready")
	readyDeadline := time.Now().Add(4 * time.Second)
	for {
		if _, err := os.Stat(readyPath); err == nil {
			break
		} else if !errors.Is(err, os.ErrNotExist) || time.Now().After(readyDeadline) {
			var observed []string
			for range 4 {
				event, eventErr := f.controller.Next(500 * time.Millisecond)
				detail := fmt.Sprintf("kind=%s err=%v", event.Kind, eventErr)
				if event.Frame != nil {
					detail += fmt.Sprintf(" frame=%+v", *event.Frame)
				}
				if event.Result != nil {
					detail += fmt.Sprintf(" result=%+v", event.Result)
				}
				observed = append(observed, detail)
				if eventErr != nil || event.Kind == AttemptResultReady {
					break
				}
			}
			providerError, _ := os.ReadFile(filepath.Join(f.root, "provider.error"))
			t.Fatalf("provider ready absent: %v events=%v provider_error=%q output=%q", err, observed, providerError, f.output())
		}
		time.Sleep(5 * time.Millisecond)
	}
	waitExactPresence(t, inner)
	if err := f.lease.Close(); err != nil {
		t.Fatal(err)
	}
	if err := f.dir.Close(); err != nil {
		t.Fatal(err)
	}
	if err := f.lifetime.Close(); err != nil {
		t.Fatal(err)
	}
	probe, err := unix.Open(filepath.Join(f.root, RuntimeLifetimeLeaseName), unix.O_RDWR|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer unix.Close(probe)
	if err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB); !errors.Is(err, unix.EWOULDBLOCK) {
		t.Fatalf("provider did not hold lifetime before outer death: %v", err)
	}
	if err := f.outer.cmd.Process.Signal(unix.SIGKILL); err != nil {
		t.Fatal(err)
	}
	if exit, err := f.outer.FinishAfterExit(6 * time.Second); err != nil || exit.Signal != int(unix.SIGKILL) {
		t.Fatalf("outer SIGKILL exit=%+v err=%v", exit, err)
	}
	waitExactPresence(t, inner)
	if err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB); !errors.Is(err, unix.EWOULDBLOCK) {
		t.Fatalf("recovery observed available while provider lived: %v", err)
	}
	if err := os.WriteFile(filepath.Join(f.root, "continue"), nil, 0o600); err != nil {
		t.Fatal(err)
	}
	deadline := time.Now().Add(6 * time.Second)
	for {
		err := unix.Flock(probe, unix.LOCK_EX|unix.LOCK_NB)
		if err == nil {
			break
		}
		if !errors.Is(err, unix.EWOULDBLOCK) || time.Now().After(deadline) {
			t.Fatalf("lifetime did not become available after provider exit: %v", err)
		}
		time.Sleep(5 * time.Millisecond)
	}
	if err := unix.Flock(probe, unix.LOCK_UN); err != nil {
		t.Fatal(err)
	}
	waitExactAbsence(t, inner)
}

func TestAttemptRunnerDaemonEOFCuts(t *testing.T) {
	t.Run("before-provider-release", func(t *testing.T) {
		f := newAttemptFixture(t, "shell", "")
		inner := f.activateOuter()
		if err := f.controller.Close(); err != nil {
			t.Fatal(err)
		}
		exit, err := f.outer.FinishAfterExit(6 * time.Second)
		if err != nil || exit.Code == 0 {
			t.Fatalf("outer exit=%+v err=%v output=%q", exit, err, f.output())
		}
		waitExactAbsence(t, inner)
		if _, err := os.Stat(filepath.Join(f.root, "selection")); !errors.Is(err, os.ErrNotExist) {
			t.Fatal("wrapper ran after daemon EOF before selection release")
		}
		authenticated, authErr := AuthenticateAttemptResult(f.dir, f.spec.AttemptID, nil)
		if authErr != nil {
			t.Fatalf("authenticate result: %v output=%q", authErr, f.output())
		}
		result := authenticated.Result()
		process, _ := result.Process()
		record := &TerminalRecord{Terminal: Terminal{Process: process}}
		if record.Terminal.Process != inner {
			t.Fatalf("result=%+v", record)
		}
	})

	t.Run("after-provider-release", func(t *testing.T) {
		f := newAttemptFixture(t, "shell", "")
		inner := f.activateOuter()
		f.advanceToProvider()
		if err := f.controller.Release(StageProvider); err != nil {
			t.Fatal(err)
		}
		if err := f.controller.Close(); err != nil {
			t.Fatal(err)
		}
		exit, err := f.outer.FinishAfterExit(6 * time.Second)
		if err != nil || exit.Code == 0 && exit.Signal == 0 {
			t.Fatalf("outer exit=%+v err=%v output=%q", exit, err, f.output())
		}
		authenticated, authErr := AuthenticateAttemptResult(f.dir, f.spec.AttemptID, nil)
		if authErr != nil {
			t.Fatalf("authenticate result: %v output=%q", authErr, f.output())
		}
		result := authenticated.Result()
		process, _ := result.Process()
		record := &TerminalRecord{Terminal: Terminal{Process: process}}
		if record.Terminal.Process != inner {
			t.Fatalf("replay result=%+v", record)
		}
		if _, err := os.Stat(filepath.Join(f.root, "provider.pid")); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("provider ran after daemon EOF before input handoff: %v", err)
		}
	})

	t.Run("active-wrapper-before-provider-release", func(t *testing.T) {
		f := newAttemptFixture(t, "shell", "")
		inner := f.activateOuter()
		f.advanceToProvider()
		if err := f.controller.Close(); err != nil {
			t.Fatal(err)
		}
		exit, err := f.outer.FinishAfterExit(6 * time.Second)
		if err != nil || exit.Code == 0 {
			t.Fatalf("outer exit=%+v err=%v output=%q", exit, err, f.output())
		}
		waitExactAbsence(t, inner)
		if _, err := os.Stat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("provider ran after daemon EOF before provider release: %v", err)
		}
		record := loadAttemptResultForTest(t, f.dir, f.spec.AttemptID, nil)
		if record.Terminal.Process != inner {
			t.Fatalf("result=%+v", record)
		}
	})
}

func TestAttemptRunnerReapsInertInnerExitBeforeSelection(t *testing.T) {
	f := newAttemptFixture(t, "shell", "")
	inner := f.activateOuter()
	if err := unix.Kill(-inner.PGID, unix.SIGKILL); err != nil {
		t.Fatal(err)
	}
	exit, err := f.outer.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code == 0 {
		t.Fatalf("outer exit=%+v err=%v output=%q", exit, err, f.output())
	}
	record := loadAttemptResultForTest(t, f.dir, f.spec.AttemptID, nil)
	if record.Terminal.Process != inner || record.Terminal.Exit.Signal != int(unix.SIGKILL) {
		t.Fatalf("inert-exit result=%+v", record)
	}
	for _, witness := range []string{"selection", "preparation", "population", "provider.effect"} {
		if _, err := os.Stat(filepath.Join(f.root, witness)); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("inert inner exit left %s witness: %v", witness, err)
		}
	}
	waitExactAbsence(t, inner)
}

func TestAttemptCleanupReapsObservedInertExit(t *testing.T) {
	f := newFixture(t)
	effect := filepath.Join(f.root, "effect")
	child := f.start("/bin/sh", []string{"-c", "printf bad > " + effect}, nil, nil)
	daemon, daemonPeer, err := newControlPair("test-daemon", "test-daemon-peer")
	if err != nil {
		t.Fatal(err)
	}
	worker, workerPeer, err := newControlPair("test-worker", "test-worker-peer")
	if err != nil {
		t.Fatal(err)
	}
	for _, file := range []*os.File{daemon, daemonPeer, worker, workerPeer} {
		file := file
		t.Cleanup(func() { _ = file.Close() })
	}
	reads := &attemptReadSet{kq: child.kq, daemonFD: int(daemon.Fd()), workerFD: int(worker.Fd())}
	if err := reads.registerDaemon(); err != nil {
		t.Fatal(err)
	}
	if err := reads.registerWorker(); err != nil {
		t.Fatal(err)
	}
	identity := child.Identity()
	if err := unix.Kill(-identity.PGID, unix.SIGKILL); err != nil {
		t.Fatal(err)
	}
	_, source, err := nextAttemptFrame(child, daemon, worker, true, true, nil, time.Second)
	if err != nil || source != sourceChild || child.state != stateExited || !child.exitObserved {
		t.Fatalf("observed inert exit source=%d err=%v state=%d observed=%t", source, err, child.state, child.exitObserved)
	}
	cfg := attemptConfig{AttemptID: "attempt-inert-exit", ResultName: AttemptResultSpoolName}
	done := make(chan error, 1)
	go func() {
		done <- finishAttemptWithExit(child, f.dir, cfg, reads, nil, false, nil)
	}()
	select {
	case finishErr := <-done:
		if !errors.Is(finishErr, ErrIdentity) {
			t.Fatalf("invalid outer marker census = %v", finishErr)
		}
	case <-time.After(2 * time.Second):
		reads.processOnly()
		_, _ = child.finishInertAfterExit()
		select {
		case <-done:
		case <-time.After(4 * time.Second):
		}
		t.Fatal("observed inert exit was not reaped")
	}
	if child.state != stateWaited || reads.daemonRegistered || reads.workerRegistered {
		t.Fatalf("inert exit did not converge: state=%d reads=%+v", child.state, reads)
	}
	if _, err := os.Stat(filepath.Join(f.root, AttemptResultSpoolName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("invalid marker census published result: %v", err)
	}
	if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("observed inert exit executed target: %v", err)
	}
	waitExactAbsence(t, identity)
}

func TestAttemptCleanupRetiresProtocolReadinessBeforeProcessWait(t *testing.T) {
	for _, mode := range []string{"eof", "malformed"} {
		t.Run(mode, func(t *testing.T) {
			f := newFixture(t)
			effect := filepath.Join(f.root, "effect")
			child := f.start("/bin/sh", []string{"-c", "printf bad > " + effect}, nil, nil)
			daemon, daemonPeer, err := newControlPair("test-daemon", "test-daemon-peer")
			if err != nil {
				t.Fatal(err)
			}
			worker, workerPeer, err := newControlPair("test-worker", "test-worker-peer")
			if err != nil {
				t.Fatal(err)
			}
			for _, file := range []*os.File{daemon, daemonPeer, worker, workerPeer} {
				file := file
				t.Cleanup(func() { _ = file.Close() })
			}
			reads := &attemptReadSet{kq: child.kq, daemonFD: int(daemon.Fd()), workerFD: int(worker.Fd())}
			if err := reads.registerDaemon(); err != nil {
				t.Fatal(err)
			}
			if err := reads.registerWorker(); err != nil {
				t.Fatal(err)
			}
			switch mode {
			case "eof":
				if err := daemonPeer.Close(); err != nil {
					t.Fatal(err)
				}
			case "malformed":
				if _, err := daemonPeer.Write(make([]byte, 64)); err != nil {
					t.Fatal(err)
				}
			}
			_, source, cause := nextAttemptFrame(child, daemon, worker, true, true, nil, time.Second)
			if cause == nil || source != sourceDaemon || child.state != stateBlocked || child.exitObserved {
				t.Fatalf("delayed process setup source=%d cause=%v state=%d observed=%t", source, cause, child.state, child.exitObserved)
			}
			cfg := attemptConfig{AttemptID: "attempt-" + mode, ResultName: AttemptResultSpoolName}
			done := make(chan error, 1)
			started := time.Now()
			go func() {
				done <- finishAttemptWithExit(child, f.dir, cfg, reads, nil, false, cause)
			}()
			select {
			case finishErr := <-done:
				if finishErr == nil {
					t.Fatal("protocol failure lost its cause")
				}
			case <-time.After(2 * time.Second):
				// Independent safety transition keeps a deliberately broken
				// mutation from leaking the real fixture after this failure.
				reads.processOnly()
				_, _ = child.finishInertAfterExit()
				select {
				case <-done:
				case <-time.After(4 * time.Second):
				}
				t.Fatal("protocol readiness starved the process exit")
			}
			if time.Since(started) >= 2*time.Second || reads.daemonRegistered || reads.workerRegistered || child.state != stateWaited {
				t.Fatalf("process-only transition did not converge: elapsed=%v reads=%+v state=%d", time.Since(started), reads, child.state)
			}
			if _, err := os.Stat(filepath.Join(f.root, AttemptResultSpoolName)); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("invalid marker census published result: %v", err)
			}
			if _, err := os.Stat(effect); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("inert protocol failure executed target: %v", err)
			}
			waitExactAbsence(t, child.Identity())
		})
	}
}

func TestAttemptReadSetRemovalIsRetryableAndIdempotent(t *testing.T) {
	kq, err := unix.Kqueue()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = unix.Close(kq) })
	daemon, daemonPeer, err := newControlPair("test-daemon", "test-daemon-peer")
	if err != nil {
		t.Fatal(err)
	}
	worker, workerPeer, err := newControlPair("test-worker", "test-worker-peer")
	if err != nil {
		t.Fatal(err)
	}
	for _, file := range []*os.File{daemon, daemonPeer, worker, workerPeer} {
		file := file
		t.Cleanup(func() { _ = file.Close() })
	}
	reads := &attemptReadSet{kq: kq, daemonFD: int(daemon.Fd()), workerFD: int(worker.Fd())}
	if err := reads.registerDaemon(); err != nil {
		t.Fatal(err)
	}
	if err := reads.registerWorker(); err != nil {
		t.Fatal(err)
	}
	reads.kq = -1
	daemonErr := reads.removeDaemon()
	workerErr := reads.removeWorker()
	if daemonErr == nil || workerErr == nil || !reads.daemonRegistered || !reads.workerRegistered {
		t.Fatalf("failed removal lost registration authority: daemon=%v worker=%v reads=%+v", daemonErr, workerErr, reads)
	}
	reads.kq = kq
	reads.processOnly()
	if reads.daemonRegistered || reads.workerRegistered {
		t.Fatalf("retried removal reads=%+v", reads)
	}
	reads.processOnly()
	if _, err := daemonPeer.Write([]byte("still-open")); err != nil {
		t.Fatal(err)
	}
	if _, err := workerPeer.Write([]byte("still-open")); err != nil {
		t.Fatal(err)
	}
	events := make([]unix.Kevent_t, 1)
	timeout := unix.NsecToTimespec((10 * time.Millisecond).Nanoseconds())
	if n, err := unix.Kevent(kq, nil, events, &timeout); err != nil || n != 0 {
		t.Fatalf("removed read filter remained ready: n=%d events=%+v err=%v", n, events, err)
	}
}

func TestAttemptRunnerTerminatesOwnedProviderGroup(t *testing.T) {
	for _, mode := range []string{"term", "leader"} {
		t.Run(mode, func(t *testing.T) {
			f := newAttemptFixture(t, mode, "")
			inner := f.activateOuter()
			f.advanceToProvider()
			if err := f.controller.Release(StageProvider); err != nil {
				t.Fatal(err)
			}
			descendantPath := filepath.Join(f.root, "descendant.pid")
			waitFile(t, descendantPath)
			descendantBody, err := os.ReadFile(descendantPath)
			if err != nil {
				t.Fatal(err)
			}
			descendantPID, err := strconv.Atoi(strings.TrimSpace(string(descendantBody)))
			if err != nil {
				t.Fatal(err)
			}
			descendant, err := readIdentity(descendantPID)
			if err != nil || descendant.PGID != inner.PGID {
				t.Fatalf("provider descendant=%+v err=%v inner=%+v", descendant, err, inner)
			}
			if mode == "term" {
				if err := f.controller.Terminate(); err != nil {
					t.Fatal(err)
				}
			} else if err := os.WriteFile(filepath.Join(f.root, "leader.release"), nil, 0o600); err != nil {
				t.Fatal(err)
			}
			record := f.finishAndAck(false)
			if record.Terminal.Process != inner {
				t.Fatalf("terminal identity=%+v", record.Terminal.Process)
			}
			waitExactAbsence(t, descendant)
		})
	}
}

func TestCurrentExecRejectsCommittedTargetChanges(t *testing.T) {
	mutations := []string{"replace", "mode", "bytes", "symlink", "remove"}
	for _, mutation := range mutations {
		t.Run(mutation, func(t *testing.T) {
			root := t.TempDir()
			target := filepath.Join(root, "provider")
			executable, err := os.Executable()
			if err != nil {
				t.Fatal(err)
			}
			copyExecutable(t, executable, target)
			f := newAttemptFixture(t, "binary", target)
			f.activateOuter()
			f.advanceToProvider()
			switch mutation {
			case "replace":
				replacement := filepath.Join(root, "replacement")
				copyExecutable(t, executable, replacement)
				if err := os.Rename(replacement, target); err != nil {
					t.Fatal(err)
				}
			case "mode":
				if err := os.Chmod(target, 0o600); err != nil {
					t.Fatal(err)
				}
			case "bytes":
				file, err := os.OpenFile(target, os.O_WRONLY|os.O_APPEND, 0)
				if err != nil {
					t.Fatal(err)
				}
				if _, err := file.Write([]byte("changed")); err != nil {
					t.Fatal(err)
				}
				_ = file.Close()
			case "remove":
				if err := os.Remove(target); err != nil {
					t.Fatal(err)
				}
			case "symlink":
				if err := os.Remove(target); err != nil {
					t.Fatal(err)
				}
				if err := os.Symlink(executable, target); err != nil {
					t.Fatal(err)
				}
			}
			if err := f.controller.Release(StageProvider); err != nil {
				t.Fatal(err)
			}
			record := f.finishAndAck(false)
			if record.Terminal.Exit.Code == 0 {
				t.Fatalf("mutated provider reported success: %+v", record.Terminal)
			}
			if _, err := os.Stat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("mutated provider effect exists: %v", err)
			}
		})
	}
}

func TestCurrentExecUsesTransferredCwdAfterParentPathReplacement(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "cwd", executable)
	inner := f.activateOuter()
	f.advanceToProvider()
	originalPath := filepath.Join(f.root, "change", "work")
	originalInfo, err := os.Lstat(originalPath)
	if err != nil {
		t.Fatal(err)
	}
	original := originalInfo.Sys().(*syscall.Stat_t)
	if err := os.Rename(filepath.Join(f.root, "change"), filepath.Join(f.root, "change.old")); err != nil {
		t.Fatal(err)
	}
	if err := os.MkdirAll(originalPath, 0o700); err != nil {
		t.Fatal(err)
	}
	replacementInfo, err := os.Lstat(originalPath)
	if err != nil {
		t.Fatal(err)
	}
	replacement := replacementInfo.Sys().(*syscall.Stat_t)
	if original.Dev == replacement.Dev && original.Ino == replacement.Ino {
		t.Fatal("replacement reused original cwd identity")
	}
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Exit.Code != 0 || record.Terminal.Process != inner {
		diagnostic, _ := os.ReadFile(filepath.Join(f.root, "cwd.error"))
		t.Fatalf("descriptor cwd terminal=%+v output=%q cwd_diagnostic=%q", record.Terminal, f.output(), diagnostic)
	}
	want := fmt.Sprintf("%d:%d", original.Dev, original.Ino)
	if got, err := os.ReadFile(filepath.Join(f.root, "cwd.identity")); err != nil || string(got) != want {
		t.Fatalf("provider cwd identity=%q err=%v want=%q", got, err, want)
	}
	if got, err := os.ReadFile(filepath.Join(f.root, "provider.effect")); err != nil || string(got) != "provider" {
		t.Fatalf("provider witness=%q err=%v", got, err)
	}
}

func TestCurrentExecClosesUnexpectedInheritedDescriptor(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "cwd-inherited", executable)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck(false)
	if record.Terminal.Exit.Code != 0 {
		t.Fatalf("sealed descriptor terminal=%+v output=%q", record.Terminal, f.output())
	}
	if got, err := os.ReadFile(filepath.Join(f.root, "provider.effect")); err != nil || string(got) != "provider" {
		t.Fatalf("provider witness=%q err=%v", got, err)
	}
	if _, err := os.Lstat(filepath.Join(f.root, "cwd.error")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("closed descriptor reported as inherited: %v", err)
	}
}

func TestProviderCannotInheritOuterResultProof(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "proof-census", executable)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Exit.Code != 0 {
		t.Fatalf("proof census result=%+v output=%q", record.Terminal, f.output())
	}
	if _, err := os.Stat(filepath.Join(f.root, "proof-census.safe")); err != nil {
		t.Fatalf("provider proof census did not complete: %v", err)
	}
}

func TestCurrentExecReplacesReservedProviderTaskDescriptor(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "cwd-inherited-11", executable)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck(false)
	if record.Terminal.Exit.Code != 0 {
		t.Fatalf("reserved descriptor replacement terminal=%+v output=%q", record.Terminal, f.output())
	}
	if got, err := os.ReadFile(filepath.Join(f.root, "provider.effect")); err != nil || string(got) != "provider" {
		t.Fatalf("provider witness=%q err=%v", got, err)
	}
	if _, err := os.Lstat(filepath.Join(f.root, "cwd.error")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("reserved descriptor reported foreign authority: %v", err)
	}
}

func TestCurrentExecRejectsInvalidTransferredCwd(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	for _, mode := range []string{"cwd-unrelated", "cwd-file", "cwd-closed", "cwd-reused", "cwd-mode"} {
		t.Run(mode, func(t *testing.T) {
			f := newAttemptFixture(t, mode, executable)
			f.activateOuter()
			f.advanceToProvider()
			if err := f.controller.Release(StageProvider); err != nil {
				t.Fatal(err)
			}
			record := f.finishAndAck(false)
			if record.Terminal.Exit.Code == 0 {
				t.Fatalf("invalid cwd reported success: %+v", record.Terminal)
			}
			if _, err := os.Lstat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
				t.Fatalf("invalid cwd provider effect: %v", err)
			}
			if mode == "cwd-reused" {
				output := f.output()
				if !strings.Contains(output, "runner: current cwd: runner: identity mismatch") {
					t.Fatalf("reused cwd error lost identity cause: %q", output)
				}
				if strings.Contains(output, "file already closed") || strings.Contains(output, os.ErrInvalid.Error()) {
					t.Fatalf("reused cwd gained spurious close error: %q", output)
				}
			}
		})
	}
}

func TestCurrentExecRechecksTransferredCwdAtFinalSeam(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "cwd-seam", executable)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptCheckpoint || event.Stage != StageProvider {
		t.Fatalf("final-check event=%+v err=%v", event, err)
	}
	if err := os.Chmod(filepath.Join(f.root, "change", "work"), 0o755); err != nil {
		t.Fatal(err)
	}
	if err := f.controller.acknowledgeCurrentExecCheck(); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck(false)
	if record.Terminal.Exit.Code == 0 {
		t.Fatalf("mutated cwd reported success: %+v", record.Terminal)
	}
	if _, err := os.Lstat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("mutated cwd provider effect: %v", err)
	}
}

func TestExecProviderTakesTransferredDescriptorOwnershipOnRejectedCall(t *testing.T) {
	cwd, err := os.Open(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	task, err := createUnlinkedProviderTask(t.TempDir(), []byte("task"))
	if err != nil {
		t.Fatal(err)
	}
	cwdFD, taskFD := int(cwd.Fd()), int(task.Fd())
	var worker *WorkerControl
	if err := worker.ExecProvider(nil, cwd, task); !errors.Is(err, ErrState) {
		t.Fatalf("rejected transfer=%v", err)
	}
	var stat unix.Stat_t
	if err := unix.Fstat(cwdFD, &stat); !errors.Is(err, unix.EBADF) {
		t.Fatalf("rejected cwd remained open: %v", err)
	}
	if err := unix.Fstat(taskFD, &stat); !errors.Is(err, unix.EBADF) {
		t.Fatalf("rejected task remained open: %v", err)
	}
}

func TestProviderTaskDescriptorFailsClosed(t *testing.T) {
	good, err := createUnlinkedProviderTask(t.TempDir(), []byte("printf '工場'\n"))
	if err != nil {
		t.Fatal(err)
	}
	if err := validateProviderTask(good); err != nil {
		t.Fatalf("exact task rejected: %v", err)
	}
	_ = good.Close()

	cases := []struct {
		name string
		open func(*testing.T) *os.File
	}{
		{name: "linked", open: func(t *testing.T) *os.File {
			path := filepath.Join(t.TempDir(), "task")
			if err := os.WriteFile(path, []byte("task"), 0o600); err != nil {
				t.Fatal(err)
			}
			file, err := os.Open(path)
			if err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "writable", open: func(t *testing.T) *os.File {
			path := filepath.Join(t.TempDir(), "task")
			file, err := os.OpenFile(path, os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o600)
			if err != nil {
				t.Fatal(err)
			}
			if _, err := file.Write([]byte("task")); err != nil {
				t.Fatal(err)
			}
			if _, err := file.Seek(0, io.SeekStart); err != nil {
				t.Fatal(err)
			}
			if err := os.Remove(path); err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "nonzero offset", open: func(t *testing.T) *os.File {
			file, err := createUnlinkedProviderTask(t.TempDir(), []byte("task"))
			if err != nil {
				t.Fatal(err)
			}
			if _, err := file.Seek(1, io.SeekStart); err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "empty", open: func(t *testing.T) *os.File {
			file, err := createUnlinkedProviderTask(t.TempDir(), nil)
			if err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "oversized", open: func(t *testing.T) *os.File {
			file, err := createUnlinkedProviderTask(t.TempDir(), bytes.Repeat([]byte{'x'}, MaxProviderTaskBytes+1))
			if err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "invalid utf8", open: func(t *testing.T) *os.File {
			file, err := createUnlinkedProviderTask(t.TempDir(), []byte{0xff})
			if err != nil {
				t.Fatal(err)
			}
			return file
		}},
		{name: "nul", open: func(t *testing.T) *os.File {
			file, err := createUnlinkedProviderTask(t.TempDir(), []byte{'x', 0})
			if err != nil {
				t.Fatal(err)
			}
			return file
		}},
	}
	for _, test := range cases {
		t.Run(test.name, func(t *testing.T) {
			file := test.open(t)
			defer file.Close()
			if err := validateProviderTask(file); !errors.Is(err, ErrIdentity) {
				t.Fatalf("invalid descriptor error=%v, want ErrIdentity", err)
			}
		})
	}
}

func TestProviderErrorPayloadAlwaysFitsPrivateControlFrame(t *testing.T) {
	payload, err := providerErrorPayload(errors.New(strings.Repeat("工", maxAttemptReportBytes)))
	if err != nil || len(payload) == 0 || len(payload) > maxProviderErrorBytes || !utf8.Valid(payload) {
		t.Fatalf("bounded payload len=%d err=%v", len(payload), err)
	}
	var wire bytes.Buffer
	if err := writeFrame(&wire, attemptFrame{Version: 1, Kind: "provider-exec-error", Payload: payload}, maxFrameBytes); err != nil {
		t.Fatalf("bounded provider error does not fit frame: %v", err)
	}
	for _, invalid := range []error{errors.New(""), errors.New("bad\x00error"), errors.New(string([]byte{0xff}))} {
		if _, err := providerErrorPayload(invalid); !errors.Is(err, ErrState) {
			t.Fatalf("invalid error %q accepted: %v", invalid, err)
		}
	}
}

func TestWorkerRuntimeDirectoryDuplicateOwnership(t *testing.T) {
	t.Run("exact cloexec duplicate and independent close", func(t *testing.T) {
		worker, _ := newWorkerDirectoryFixture(t)
		before := fdCensus()
		duplicate, err := worker.DuplicateRuntimeDirectory(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		got, err := validatePrivateDirectory(duplicate)
		if err != nil || got != worker.dirID {
			t.Fatalf("duplicate identity=%+v err=%v want=%+v", got, err, worker.dirID)
		}
		flags, err := unix.FcntlInt(duplicate.Fd(), unix.F_GETFD, 0)
		if err != nil || flags&unix.FD_CLOEXEC == 0 {
			t.Fatalf("duplicate flags=%#x err=%v", flags, err)
		}
		if err := duplicate.Close(); err != nil {
			t.Fatal(err)
		}
		if got, err := validatePrivateDirectory(worker.dir); err != nil || got != worker.dirID {
			t.Fatalf("original after duplicate close=%+v err=%v", got, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("duplicate close census before=%v after=%v", before, after)
		}
	})

	t.Run("duplicate survives original close", func(t *testing.T) {
		worker, _ := newWorkerDirectoryFixture(t)
		want := worker.dirID
		duplicate, err := worker.DuplicateRuntimeDirectory(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if err := worker.Close(); err != nil {
			t.Fatal(err)
		}
		if got, err := validatePrivateDirectory(duplicate); err != nil || got != want {
			t.Fatalf("duplicate after original close=%+v err=%v", got, err)
		}
		if err := duplicate.Close(); err != nil {
			t.Fatal(err)
		}
	})

	t.Run("one shot and initial state only", func(t *testing.T) {
		worker, _ := newWorkerDirectoryFixture(t)
		duplicate, err := worker.DuplicateRuntimeDirectory(context.Background())
		if err != nil {
			t.Fatal(err)
		}
		if err := duplicate.Close(); err != nil {
			t.Fatal(err)
		}
		before := fdCensus()
		if duplicate, err := worker.DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrState) || duplicate != nil {
			t.Fatalf("second duplicate=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("second call leaked fd before=%v after=%v", before, after)
		}

		advanced, _ := newWorkerDirectoryFixture(t)
		advanced.state = workerSelectionReported
		closed, _ := newWorkerDirectoryFixture(t)
		if err := closed.Close(); err != nil {
			t.Fatal(err)
		}
		before = fdCensus()
		if duplicate, err := advanced.DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrState) || duplicate != nil {
			t.Fatalf("post-stage duplicate=%v err=%v", duplicate, err)
		}
		if duplicate, err := (*WorkerControl)(nil).DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrState) || duplicate != nil {
			t.Fatalf("nil duplicate=%v err=%v", duplicate, err)
		}
		if duplicate, err := advanced.DuplicateRuntimeDirectory(nil); !errors.Is(err, ErrState) || duplicate != nil {
			t.Fatalf("nil-context duplicate=%v err=%v", duplicate, err)
		}
		if duplicate, err := closed.DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrState) || duplicate != nil {
			t.Fatalf("closed duplicate=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("rejected calls leaked fd before=%v after=%v", before, after)
		}
	})
}

func TestWorkerRuntimeDirectoryDuplicateFailsClosed(t *testing.T) {
	t.Run("mode mutation", func(t *testing.T) {
		worker, root := newWorkerDirectoryFixture(t)
		if err := os.Chmod(root, 0o755); err != nil {
			t.Fatal(err)
		}
		before := fdCensus()
		if duplicate, err := worker.DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrIdentity) || duplicate != nil {
			t.Fatalf("mode-mutated duplicate=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("mode mutation leaked fd before=%v after=%v", before, after)
		}
	})

	t.Run("pathname replacement", func(t *testing.T) {
		worker, root := newWorkerDirectoryFixture(t)
		moved := root + ".moved"
		if err := os.Rename(root, moved); err != nil {
			t.Fatal(err)
		}
		if err := os.Mkdir(root, 0o700); err != nil {
			t.Fatal(err)
		}
		if duplicate, err := worker.DuplicateRuntimeDirectory(context.Background()); !errors.Is(err, ErrIdentity) || duplicate != nil {
			t.Fatalf("replaced-path duplicate=%v err=%v", duplicate, err)
		}
	})

	t.Run("original replaced after duplicate", func(t *testing.T) {
		worker, _ := newWorkerDirectoryFixture(t)
		replacementPath := filepath.Join(t.TempDir(), "replacement")
		if err := os.Mkdir(replacementPath, 0o700); err != nil {
			t.Fatal(err)
		}
		replacement, err := os.Open(replacementPath)
		if err != nil {
			t.Fatal(err)
		}
		defer replacement.Close()
		ctx := &runtimeDirectoryTestContext{replaceAt: 3, worker: worker, replacement: replacement}
		before := fdCensus()
		if duplicate, err := worker.DuplicateRuntimeDirectory(ctx); !errors.Is(err, ErrIdentity) || duplicate != nil {
			t.Fatalf("post-dup replacement=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameFDNumbers(before, after) {
			t.Fatalf("post-dup replacement leaked fd before=%v after=%v", before, after)
		}
	})

	t.Run("cancellation before and after duplicate", func(t *testing.T) {
		beforeWorker, _ := newWorkerDirectoryFixture(t)
		canceled, cancel := context.WithCancel(context.Background())
		cancel()
		before := fdCensus()
		if duplicate, err := beforeWorker.DuplicateRuntimeDirectory(canceled); !errors.Is(err, context.Canceled) || duplicate != nil {
			t.Fatalf("pre-canceled duplicate=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("pre-canceled call leaked fd before=%v after=%v", before, after)
		}

		betweenWorker, _ := newWorkerDirectoryFixture(t)
		ctx := &runtimeDirectoryTestContext{cancelAt: 3}
		before = fdCensus()
		if duplicate, err := betweenWorker.DuplicateRuntimeDirectory(ctx); !errors.Is(err, context.Canceled) || duplicate != nil {
			t.Fatalf("post-dup cancellation=%v err=%v", duplicate, err)
		}
		if after := fdCensus(); !sameCensus(before, after) {
			t.Fatalf("post-dup cancellation leaked fd before=%v after=%v", before, after)
		}
	})
}

func TestWorkerRuntimeDirectoryDuplicateHasNoPathOpen(t *testing.T) {
	source, err := os.ReadFile("attempt_darwin.go")
	if err != nil {
		t.Fatal(err)
	}
	start := strings.Index(string(source), "func (w *WorkerControl) DuplicateRuntimeDirectory")
	if start < 0 {
		t.Fatal("runtime directory method not found")
	}
	end := strings.Index(string(source[start:]), "\nfunc ")
	if end < 0 {
		t.Fatal("runtime directory method end not found")
	}
	method := string(source[start : start+end])
	for _, forbidden := range []string{"unix.Open(", "unix.Openat(", "os.Open(", "os.OpenFile("} {
		if strings.Contains(method, forbidden) {
			t.Fatalf("runtime duplicate contains pathname open %q", forbidden)
		}
	}
}

func newWorkerDirectoryFixture(t *testing.T) (*WorkerControl, string) {
	t.Helper()
	root := t.TempDir()
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	directory, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	directoryID, err := validatePrivateDirectory(directory)
	if err != nil {
		_ = directory.Close()
		t.Fatal(err)
	}
	control, err := os.CreateTemp(t.TempDir(), "control")
	if err != nil {
		_ = directory.Close()
		t.Fatal(err)
	}
	lifetime, err := os.CreateTemp(t.TempDir(), "lifetime")
	if err != nil {
		_ = control.Close()
		_ = directory.Close()
		t.Fatal(err)
	}
	worker := &WorkerControl{
		file:     control,
		dir:      directory,
		dirID:    directoryID,
		lifetime: lifetime,
		state:    workerSelection,
	}
	t.Cleanup(func() {
		if err := worker.Close(); err != nil {
			t.Errorf("close worker directory fixture: %v", err)
		}
	})
	return worker, root
}

// runtimeDirectoryTestContext injects a cancellation or swaps the original
// descriptor at an exact validation boundary without adding a production seam.
type runtimeDirectoryTestContext struct {
	step        int
	cancelAt    int
	replaceAt   int
	worker      *WorkerControl
	replacement *os.File
}

func sameFDNumbers(left, right map[int]FileIdentity) bool {
	if len(left) != len(right) {
		return false
	}
	for fd := range left {
		if _, ok := right[fd]; !ok {
			return false
		}
	}
	return true
}

func (c *runtimeDirectoryTestContext) Deadline() (time.Time, bool) { return time.Time{}, false }
func (c *runtimeDirectoryTestContext) Done() <-chan struct{}       { return nil }
func (c *runtimeDirectoryTestContext) Value(any) any               { return nil }
func (c *runtimeDirectoryTestContext) Err() error {
	c.step++
	if c.step == c.replaceAt {
		if c.worker == nil || c.worker.dir == nil || c.replacement == nil {
			return ErrIdentity
		}
		if err := unix.Dup2(int(c.replacement.Fd()), int(c.worker.dir.Fd())); err != nil {
			return err
		}
	}
	if c.step == c.cancelAt {
		return context.Canceled
	}
	return nil
}

func TestCurrentExecDocumentsCooperativeSameUIDRace(t *testing.T) {
	root := t.TempDir()
	target := filepath.Join(root, "provider")
	copyExecutable(t, "/usr/bin/false", target)
	f := newAttemptFixture(t, "seam", target)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptCheckpoint || event.Stage != StageProvider {
		t.Fatalf("final-check event=%+v err=%v", event, err)
	}
	replacement := filepath.Join(root, "replacement")
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	copyExecutable(t, executable, replacement)
	if err := os.Rename(replacement, target); err != nil {
		t.Fatal(err)
	}
	if err := f.controller.acknowledgeCurrentExecCheck(); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck()
	if record.Terminal.Exit.Code != 0 {
		t.Fatalf("replacement did not win documented race: %+v", record.Terminal)
	}
	if body, err := os.ReadFile(filepath.Join(f.root, "provider.effect")); err != nil || string(body) != "provider" {
		t.Fatalf("replacement witness=%q err=%v", body, err)
	}
}

func TestCurrentExecRejectsReplacedRuntimeLifetime(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	f := newAttemptFixture(t, "lease-seam", executable)
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(4 * time.Second)
	if err != nil || event.Kind != AttemptCheckpoint || event.Stage != StageProvider {
		t.Fatalf("final-check event=%+v err=%v", event, err)
	}
	lifetimePath := filepath.Join(f.root, RuntimeLifetimeLeaseName)
	if err := os.Rename(lifetimePath, lifetimePath+".old"); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(lifetimePath, nil, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := f.controller.acknowledgeCurrentExecCheck(); err != nil {
		t.Fatal(err)
	}
	record := f.finishAndAck(false)
	if record.Terminal.Exit.Code == 0 {
		t.Fatalf("replaced lifetime reported success: %+v", record.Terminal)
	}
	if _, err := os.Stat(filepath.Join(f.root, "provider.effect")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("provider ran with replaced lifetime: %v", err)
	}
}

func TestAttemptProtocolSurfaceIsClosed(t *testing.T) {
	controller, child, err := NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	defer controller.Close()
	defer child.Close()
	if err := controller.Release(StageSelection); !errors.Is(err, ErrState) {
		t.Fatalf("release before configuration=%v", err)
	}
	source, err := os.ReadFile("process_darwin.go")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(source), ".Process.Signal(") {
		t.Fatal("numeric per-process signal surface introduced")
	}
	gateStart := strings.Index(string(source), "func RunExecGate() error {")
	if gateStart < 0 {
		t.Fatal("exec gate is missing")
	}
	gateBody := string(source)[gateStart:]
	if next := strings.Index(gateBody[len("func RunExecGate() error {"):], "\nfunc "); next >= 0 {
		gateBody = gateBody[:len("func RunExecGate() error {")+next]
	}
	if strings.Contains(gateBody, "unix.Close(") {
		t.Fatal("exec gate closes an unowned raw descriptor number")
	}
	for _, required := range []string{
		"unix.Dup2(int(control.Fd()), int(config.Fd()))",
		"control.Close()",
		"stdin.Close()",
		"stdout.Close()",
		"stderr.Close()",
	} {
		if !strings.Contains(gateBody, required) {
			t.Fatalf("exec gate does not preserve descriptor ownership through %q", required)
		}
	}
	dup := strings.Index(gateBody, "unix.Dup2(int(control.Fd()), int(config.Fd()))")
	if dup < 0 || !strings.Contains(gateBody[:dup], "defer config.Close()") || strings.Count(gateBody[:dup], "config.Close()") != 1 {
		t.Fatal("exec gate can release reserved config fd3 before the control remap")
	}
	attemptSource, err := os.ReadFile("attempt_darwin.go")
	if err != nil {
		t.Fatal(err)
	}
	for _, forbidden := range []string{".Wait(", "unix.Kill("} {
		if strings.Contains(string(attemptSource), forbidden) {
			t.Fatalf("attempt runner bypasses OwnedChild with %q", forbidden)
		}
	}
	if strings.Contains(string(attemptSource), "terminal-ack") || strings.Contains(string(attemptSource), "AcknowledgeTerminal") {
		t.Fatal("attempt runner retained terminal acknowledgement choreography")
	}
	for _, ordered := range []string{"waitForAttemptChild(child)", "drainAndCloseAttemptPTY(child", "publishAttemptResult(dir, result)"} {
		if !strings.Contains(string(attemptSource), ordered) {
			t.Fatalf("attempt result path missing %q", ordered)
		}
	}
	finishStart := strings.Index(string(attemptSource), "func finishAttemptWithExit(")
	waitStart := strings.Index(string(attemptSource), "func waitForAttemptChild(")
	if finishStart < 0 || waitStart <= finishStart {
		t.Fatal("attempt cleanup functions missing")
	}
	finishBody := string(attemptSource)[finishStart:waitStart]
	if strings.Index(finishBody, "reads.processOnly()") < 0 || strings.Index(finishBody, "reads.processOnly()") > strings.Index(finishBody, "waitForAttemptChild(child)") {
		t.Fatal("process cleanup begins before protocol read filters are retired")
	}
	waitBody := string(attemptSource)[waitStart:]
	for _, lifecycleCase := range []string{"case stateBlocked:", "case stateActivated:", "case stateExited:", "case stateWaited:"} {
		if !strings.Contains(waitBody, lifecycleCase) {
			t.Fatalf("attempt cleanup omits %s", lifecycleCase)
		}
	}
	typesSource, err := os.ReadFile("types.go")
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(string(typesSource), "ExtraFiles ") {
		t.Fatal("generic arbitrary inherited-FD surface introduced")
	}
}

func TestAttemptCleanupUncertaintyRetainsOwnerBeforeTerminal(t *testing.T) {
	f := newFixture(t)
	child := f.start("/bin/sh", []string{"-c", "exit 0"}, nil, nil)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	cfg := attemptConfig{AttemptID: "attempt-retry", ResultName: "terminal.json"}
	identity := child.Identity()
	entered := make(chan int, 4)
	release := make(chan struct{})
	released := false
	t.Cleanup(func() {
		if !released {
			close(release)
		}
	})
	var calls atomic.Int32
	child.testConvergence = func() error {
		call := int(calls.Add(1))
		entered <- call
		if call <= 2 {
			<-release
			return fmt.Errorf("%w: injected convergence uncertainty", ErrUnresolved)
		}
		return nil
	}
	done := make(chan error, 1)
	go func() {
		done <- finishAttemptWithExit(child, f.dir, cfg, nil, nil, false, nil)
	}()
	select {
	case call := <-entered:
		if call != 1 {
			t.Fatalf("first convergence call=%d", call)
		}
	case <-time.After(4 * time.Second):
		t.Fatal("cleanup did not reach injected uncertainty")
	}
	if _, err := os.Stat(filepath.Join(f.root, cfg.ResultName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("uncertain cleanup published terminal: %v", err)
	}
	if got := ObserveProcess(identity); got.Presence != Present {
		t.Fatalf("uncertain cleanup lost exact unreaped owner: %+v", got)
	}
	select {
	case err := <-done:
		t.Fatalf("uncertain cleanup returned early: %v", err)
	default:
	}
	close(release)
	released = true
	select {
	case err := <-done:
		if !errors.Is(err, ErrUnresolved) {
			t.Fatalf("uncertain cleanup result=%v, want unresolved evidence", err)
		}
	case <-time.After(4 * time.Second):
		t.Fatal("cleanup did not converge after uncertainty cleared")
	}
	if got := calls.Load(); got != 3 {
		t.Fatalf("convergence calls=%d want=3", got)
	}
	if child.state != stateWaited {
		t.Fatalf("terminal returned without sole Wait: state=%d", child.state)
	}
	if _, err := LoadTerminal(f.dir, cfg.ResultName); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("uncertain cleanup published terminal: %v", err)
	}
	waitExactAbsence(t, identity)
}

func TestAttemptPermanentCleanupUncertaintyPublishesNothing(t *testing.T) {
	f := newFixture(t)
	child := f.start("/bin/sh", []string{"-c", "exit 0"}, nil, nil)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	cfg := attemptConfig{AttemptID: "attempt-unresolved", ResultName: "terminal.json"}
	identity := child.Identity()
	entered := make(chan struct{}, 16)
	var restored atomic.Bool
	child.testConvergence = func() error {
		entered <- struct{}{}
		if !restored.Load() {
			return fmt.Errorf("%w: permanent injected convergence uncertainty", ErrUnresolved)
		}
		return nil
	}
	done := make(chan error, 1)
	finished := false
	t.Cleanup(func() {
		if finished {
			return
		}
		restored.Store(true)
		select {
		case <-done:
		case <-time.After(4 * time.Second):
			t.Error("cleanup owner did not join after safety restoration")
		}
	})
	go func() {
		done <- finishAttemptWithExit(child, f.dir, cfg, nil, nil, false, nil)
	}()
	for range 3 {
		select {
		case <-entered:
		case err := <-done:
			finished = true
			t.Fatalf("cleanup owner abandoned permanent uncertainty: %v", err)
		case <-time.After(4 * time.Second):
			t.Fatal("cleanup stopped retrying permanent uncertainty")
		}
	}
	if _, err := os.Stat(filepath.Join(f.root, cfg.ResultName)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("permanent uncertainty published terminal: %v", err)
	}
	if got := ObserveProcess(identity); got.Presence != Present {
		t.Fatalf("permanent uncertainty lost exact unreaped owner: %+v", got)
	}
	select {
	case err := <-done:
		t.Fatalf("permanent uncertainty returned early: %v", err)
	default:
	}
	restored.Store(true)
	select {
	case err := <-done:
		finished = true
		if !errors.Is(err, ErrUnresolved) {
			t.Fatalf("restored cleanup result=%v, want unresolved evidence", err)
		}
	case <-time.After(4 * time.Second):
		t.Fatal("cleanup did not converge after safety restoration")
	}
	if child.state != stateWaited {
		t.Fatalf("restored cleanup returned before Wait: state=%d", child.state)
	}
	if _, err := LoadTerminal(f.dir, cfg.ResultName); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("restored cleanup published terminal after uncertainty: %v", err)
	}
	waitExactAbsence(t, identity)
}

func TestAttemptControllerRejectsResultNoticeWithContentAuthority(t *testing.T) {
	controller, peer, err := NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	defer controller.Close()
	defer peer.Close()
	controller.state = controllerProviderReleased
	identity := FileIdentity{Device: 1, Inode: 2}
	process := Identity{PID: 22, PGID: 22, Birth: Birth{Seconds: 3, Microseconds: 4}}
	if err := writeControlFrame(peer, attemptFrame{Version: 1, Kind: string(AttemptResultReady), Identity: process, FileIdentity: &identity, Digest: strings.Repeat("0", 64)}, maxConfigBytes); err != nil {
		t.Fatal(err)
	}
	if _, err := controller.Next(time.Second); !errors.Is(err, ErrState) {
		t.Fatalf("result notice carried process authority: %v", err)
	}
}

func TestDirectAttemptRunnerWithoutCapabilitiesFailsBeforeEffect(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	command := execCommand(executable, "--attempt-runner")
	output, err := command.CombinedOutput()
	if err == nil || !strings.Contains(string(output), "runner:") {
		t.Fatalf("direct attempt runner err=%v output=%q", err, output)
	}
}

func copyExecutable(t *testing.T, from, to string) {
	t.Helper()
	source, err := os.Open(from)
	if err != nil {
		t.Fatal(err)
	}
	defer source.Close()
	target, err := os.OpenFile(to, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o700)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(target, source); err != nil {
		t.Fatal(err)
	}
	if err := target.Close(); err != nil {
		t.Fatal(err)
	}
}

// Kept behind this seam so the direct-invocation test cannot accidentally
// acquire any of the fixture's inherited descriptors.
func execCommand(name string, args ...string) *exec.Cmd {
	cmd := exec.Command(name, args...)
	cmd.Env = []string{}
	return cmd
}

// A worker that writes more than the 1024-byte macOS PTY output buffer before
// its stage report must not deadlock the attempt. The outer runner owns the
// PTY master from inner activation, so the worker never blocks in write(2),
// still reports its checkpoint, and the retained bytes stay in the stream.
func TestInnerWorkerOutputBeforeStageReportCannotDeadlockTheAttempt(t *testing.T) {
	f := newAttemptFixture(t, "loud-selection", "")
	f.activateOuter()
	if err := f.controller.Release(StageSelection); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(8 * time.Second)
	if err != nil || event.Kind != AttemptCheckpoint || event.Stage != StageSelection {
		t.Fatalf("selection checkpoint event=%+v err=%v output=%q", event, err, f.output())
	}
	if _, err := os.Stat(filepath.Join(f.root, "selection")); err != nil {
		t.Fatalf("missing selection witness: %v", err)
	}
}

// adoptionWindowPayload is 214 bytes carrying two markers, written by the
// worker between AwaitProvider returning and ExecProvider.
func adoptionWindowPayload() []byte {
	return []byte(adoptionMarker + strings.Repeat("a", 214-2*len(adoptionMarker)) + adoptionMarker)
}

const adoptionMarker = "MK"

// Bytes the worker writes between AwaitProvider returning and ExecProvider
// cross the boundary where the stage sink's retained ring is adopted by the
// terminal owner. They must arrive in the delivered stream, in order: a
// snapshot rather than a shared ring drops them silently, with dense
// sequence numbers so no reset or gap ever tells a client.
func TestWorkerOutputInTheAdoptionWindowStaysInTheTerminalStream(t *testing.T) {
	f := newAttemptFixture(t, "loud-adoption", "")
	f.activateOuter()
	f.advanceToProvider()
	if err := f.controller.Release(StageProvider); err != nil {
		t.Fatal(err)
	}
	waitFile(t, filepath.Join(f.root, "provider.pid"))
	event, err := f.controller.Next(8 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalReady {
		t.Fatalf("terminal ready=%+v err=%v output=%q", event, err, f.output())
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalAttach, Correlation: 1, Sequence: 0}); err != nil {
		t.Fatal(err)
	}
	event, err = f.controller.Next(8 * time.Second)
	if err != nil || event.Kind != AttemptTerminalFrame || event.Frame == nil || event.Frame.Kind != TerminalAttached || event.Frame.Status != TerminalResultOK {
		t.Fatalf("attach=%+v err=%v", event, err)
	}
	if event.Frame.Sequence != 0 || event.Frame.Floor != 0 {
		t.Fatalf("attach did not start at the stream floor: %+v", event.Frame)
	}
	if err := f.controller.SendTerminalCommand(TerminalCommand{Kind: TerminalCredit, Credit: 8192}); err != nil {
		t.Fatal(err)
	}
	want := adoptionWindowPayload()
	var stream []byte
	deadline := time.Now().Add(10 * time.Second)
	for !bytes.Contains(stream, want) && time.Now().Before(deadline) {
		event, err = f.controller.Next(4 * time.Second)
		if err != nil {
			break
		}
		if event.Kind == AttemptTerminalFrame && event.Frame != nil && event.Frame.Kind == TerminalOutput {
			stream = append(stream, event.Frame.Payload...)
		}
	}
	markers := bytes.Count(stream, []byte(adoptionMarker))
	if !bytes.Contains(stream, want) {
		t.Fatalf("adoption-window bytes dropped: delivered=%d markers=%d want=%d err=%v output=%q", len(stream), markers, len(want), err, f.output())
	}
	if markers != 2 {
		t.Fatalf("adoption-window payload not delivered intact: delivered=%d markers=%d", len(stream), markers)
	}
	t.Logf("adoption window retained: delivered=%d markers=%d", len(stream), markers)
}
