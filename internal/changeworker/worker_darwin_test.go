//go:build darwin

package changeworker_test

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/changeworker"
	"github.com/dark-factory-build/dark-factory/internal/daemon"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

// workerEventPatience bounds each wait on the spawned worker chain, which
// re-execs this race-instrumented test binary through two gates before the
// worker can report. These waits have been observed to expire with no event
// and an empty diagnostic: once in 16 sustained race-instrumented package
// runs, and once more in a full authoritative gate run after this bound was
// raised to 30s (that gate evidence is recorded in the socket-fix section of
// docs/internal/cutover-completion-plan.md). Never reproduced in isolation;
// cause not isolated. The bound exists to catch a hang, not to assert latency;
// it is set well past any observed healthy wait so that an expiry is evidence
// of a real stall rather than of a slow machine.
const workerEventPatience = 30 * time.Second

func TestMain(m *testing.M) {
	if len(os.Args) == 2 {
		var err error
		switch os.Args[1] {
		case "--exec-gate":
			err = runner.RunExecGate()
		case "--attempt-runner":
			err = runner.RunAttemptRunner()
		case "--change-worker":
			err = changeworker.Run(context.Background())
		default:
			os.Exit(m.Run())
		}
		if err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(70)
		}
		os.Exit(0)
	}
	os.Exit(m.Run())
}

func TestRegisteredShellWorkerCompletesExactFourReleaseSequence(t *testing.T) {
	fixture := newWorkerFixture(t)
	inner := fixture.start(t)

	if err := fixture.controller.Release(runner.StagePreparation); !errors.Is(err, runner.ErrState) {
		t.Fatalf("out-of-order release: %v", err)
	}
	selectionEvent := fixture.release(t, runner.StageSelection, runner.StageSelection)
	if len(selectionEvent.Payload) != 0 {
		t.Fatal("selection acknowledgement carried data")
	}
	preparationEvent := fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	result, err := changeworker.DecodeResult(preparationEvent.Payload)
	if err != nil {
		t.Fatal(err)
	}
	if result.EntryCount != 1 || result.BlobBytes == 0 {
		t.Fatalf("result=%+v", result)
	}
	populationEvent := fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	if len(populationEvent.Payload) != 0 {
		t.Fatal("population acknowledgement carried data")
	}
	observation := runner.ObserveProcess(inner)
	if observation.Presence != runner.Present || len(observation.Members) != 0 {
		t.Fatalf("Git descendant survived population report: %+v", observation)
	}
	if _, err := os.Stat(fixture.witness); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider effect before provider release")
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	record := fixture.finish(t)
	if process, ok := record.Result().Process(); !ok || process != inner {
		t.Fatalf("result=%+v inner=%+v", record.Result(), inner)
	}
	if code, ok := record.Result().Code(); !ok || code != 0 {
		t.Fatalf("result exit=%+v", record.Result())
	}
	if body, err := os.ReadFile(fixture.witness); err != nil || string(body) != "x" {
		t.Fatalf("startup witness=%q err=%v", body, err)
	}
	changePath := filepath.Join(fixture.changeParent, fixture.finalName)
	if body, err := os.ReadFile(fixture.cwdWitness); err != nil || strings.TrimSpace(string(body)) != changePath {
		t.Fatalf("cwd=%q err=%v", body, err)
	}
	environment, err := os.ReadFile(fixture.envWitness)
	if err != nil {
		t.Fatal(err)
	}
	for _, forbidden := range []string{"SSH_AUTH_SOCK=", "GITHUB_TOKEN=", "DARK_FACTORY_OPERATOR_TOKEN", fixture.repositoryRoot} {
		if strings.Contains(string(environment), forbidden) {
			t.Fatalf("ambient/source authority leaked in environment")
		}
	}
	if strings.Count(string(environment), "DARK_FACTORY_FACTORYCTL="+fixture.factoryctl+"\n") != 1 || strings.Count(string(environment), "PATH="+fixture.toolPath+"\n") != 1 {
		t.Fatalf("provider helper/PATH environment is not exact: %q", environment)
	}
	for _, exact := range []string{"TERM=xterm-256color\n", "SHELL=/bin/sh\n"} {
		if strings.Count(string(environment), exact) != 1 {
			t.Fatalf("provider Build environment omitted %q: %q", exact, environment)
		}
	}
	for _, deleted := range []string{"TERM=dumb\n", "NO_COLOR=", "USER=", "LOGNAME="} {
		if strings.Contains(string(environment), deleted) {
			t.Fatalf("provider Build retained duplicate/ambient field %q: %q", deleted, environment)
		}
	}
	if diagnostic := fixture.output(); strings.Contains(diagnostic, fixture.repositoryRoot) || strings.Contains(diagnostic, "printf x") {
		t.Fatalf("private source/input leaked: %q", diagnostic)
	}
}

func TestRegisteredShellWorkerExecutesMaximumTaskWithoutPTYStartupTraffic(t *testing.T) {
	fixture := newWorkerFixtureWithInput(t, func(witness, _, _ string) []byte {
		quote := func(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'" }
		prefix := []byte("set -eu\n/bin/stty sane\ncount=$(/usr/bin/wc -c <<'DF_MAX_INPUT'\n")
		suffixTemplate := "DF_MAX_INPUT\n)\ntest \"$count\" -eq 000000\ntest -x \"$(command -v go)\"\nprintf x > " + quote(witness) + "\nexit\n"
		payloadSize := runner.MaxProviderTaskBytes - len(prefix) - len(suffixTemplate)
		if payloadSize < 100000 || payloadSize > 999999 {
			t.Fatalf("unexpected maximum-input payload size %d", payloadSize)
		}
		payload := append(bytes.Repeat([]byte{'x'}, payloadSize-1), '\n')
		suffix := strings.Replace(suffixTemplate, "000000", strconv.Itoa(payloadSize), 1)
		input := append(prefix, payload...)
		input = append(input, suffix...)
		if len(input) != runner.MaxProviderTaskBytes || input[len(input)-1] != '\n' {
			t.Fatalf("maximum task size=%d, want %d", len(input), runner.MaxProviderTaskBytes)
		}
		return input
	})
	inner := fixture.start(t)
	for _, stage := range []runner.AttemptStage{runner.StageSelection, runner.StagePreparation, runner.StagePopulation} {
		fixture.release(t, stage, stage)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	record := fixture.finish(t)
	if process, ok := record.Result().Process(); !ok || process != inner {
		t.Fatalf("result=%+v inner=%+v", record.Result(), inner)
	}
	if code, ok := record.Result().Code(); !ok || code != 0 {
		t.Fatalf("result exit=%+v", record.Result())
	}
	if body, err := os.ReadFile(fixture.witness); err != nil || string(body) != "x" {
		t.Fatalf("maximum-input witness=%q err=%v", body, err)
	}
}

func TestRegisteredShellWorkerKeepsPTYExclusiveForPostReadyInput(t *testing.T) {
	fixture := newWorkerFixtureWithInput(t, func(witness, reply, _ string) []byte {
		quote := func(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'" }
		return []byte("set -eu\nprintf ready > " + quote(witness) + "\nIFS= read -r line\nprintf '%s' \"$line\" > " + quote(reply) + "\nexit\n")
	})
	inner := fixture.start(t)
	for _, stage := range []runner.AttemptStage{runner.StageSelection, runner.StagePreparation, runner.StagePopulation} {
		fixture.release(t, stage, stage)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	if frame := fixture.nextTerminal(t, runner.TerminalReady, 0); frame.Kind != runner.TerminalReady {
		t.Fatalf("terminal ready=%+v", frame)
	}
	if err := fixture.controller.SendTerminalCommand(runner.TerminalCommand{Kind: runner.TerminalGenerationInstall, Correlation: 1, Generation: 1}); err != nil {
		t.Fatal(err)
	}
	if frame := fixture.nextTerminal(t, runner.TerminalGenerationResult, 1); frame.Status != runner.TerminalResultOK {
		t.Fatalf("generation install=%+v", frame)
	}
	payload := []byte("interactive-after-ready\n")
	if err := fixture.controller.SendTerminalCommand(runner.TerminalCommand{Kind: runner.TerminalInput, Correlation: 2, Generation: 1, Sequence: 1, Payload: payload}); err != nil {
		t.Fatal(err)
	}
	if frame := fixture.nextTerminal(t, runner.TerminalInputResult, 2); frame.Status != runner.TerminalResultOK || frame.Count != uint32(len(payload)) {
		t.Fatalf("terminal input=%+v", frame)
	}
	record := fixture.finish(t)
	if process, ok := record.Result().Process(); !ok || process != inner {
		t.Fatalf("result=%+v inner=%+v", record.Result(), inner)
	}
	if code, ok := record.Result().Code(); !ok || code != 0 {
		t.Fatalf("result exit=%+v", record.Result())
	}
	if body, err := os.ReadFile(fixture.cwdWitness); err != nil || string(body) != "interactive-after-ready" {
		t.Fatalf("interactive reply=%q err=%v", body, err)
	}
}

func TestFactoryctlLocatorValidationPrecedesSelectionAndProviderEffects(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name    string
		locator func(*testing.T) string
	}{
		{name: "missing", locator: func(t *testing.T) string { return filepath.Join(workerSecureTempDir(t), "missing-factoryctl") }},
		{name: "symlink", locator: func(t *testing.T) string {
			link := filepath.Join(workerSecureTempDir(t), "factoryctl")
			if err := os.Symlink(executable, link); err != nil {
				t.Fatal(err)
			}
			return link
		}},
		{name: "unsafe mode", locator: func(t *testing.T) string {
			target := filepath.Join(workerSecureTempDir(t), "factoryctl")
			copyWorkerExecutable(t, executable, target)
			if err := os.Chmod(target, 0o775); err != nil {
				t.Fatal(err)
			}
			return target
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			locator := test.locator(t)
			fixture := newWorkerFixtureWithFactoryctl(t, locator)
			fixture.start(t)
			if err := fixture.controller.Release(runner.StageSelection); err != nil {
				t.Fatal(err)
			}
			if event, err := fixture.controller.Next(workerEventPatience); !errors.Is(err, io.EOF) || event.Kind != "" {
				t.Fatalf("event=%+v err=%v diagnostic=%q", event, err, fixture.output())
			}
			if exit, err := fixture.child.FinishAfterExit(workerEventPatience); err != nil || exit.Code == 0 && exit.Signal == 0 {
				t.Fatalf("outer=%+v err=%v", exit, err)
			}
			for _, path := range []string{filepath.Join(fixture.changeParent, fixture.finalName), filepath.Join(fixture.changeParent, ".stage"), fixture.witness} {
				if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("effect %q exists: %v", path, err)
				}
			}
			if strings.Contains(fixture.output(), locator) {
				t.Fatal("private factoryctl locator leaked in worker diagnostic")
			}
		})
	}
}

func TestFinalFactoryctlCommitmentRejectsReplacementBeforeProviderWitness(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(workerSecureTempDir(t), "factoryctl")
	copyWorkerExecutable(t, executable, target)
	fixture := newWorkerFixtureWithFactoryctl(t, target)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	replacement := filepath.Join(workerSecureTempDir(t), "replacement")
	copyWorkerExecutable(t, executable, replacement)
	if err := os.Rename(replacement, target); err != nil {
		t.Fatal(err)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	fixture.finish(t, false)
	if _, err := os.Stat(fixture.witness); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("provider witness exists after factoryctl replacement: %v", err)
	}
	if strings.Contains(fixture.output(), target) {
		t.Fatal("private factoryctl locator leaked in worker diagnostic")
	}
}

func TestControllerEOFAfterSelectionLeavesNoLaterEffect(t *testing.T) {
	fixture := newWorkerFixture(t)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	if err := fixture.controller.Close(); err != nil {
		t.Fatal(err)
	}
	exit, err := fixture.child.FinishAfterExit(workerEventPatience)
	if err != nil {
		t.Fatalf("outer did not join after controller EOF: %v diag=%q", err, fixture.output())
	}
	if exit.Code == 0 && exit.Signal == 0 {
		t.Fatalf("controller EOF accepted: %+v", exit)
	}
	for _, path := range []string{filepath.Join(fixture.changeParent, fixture.finalName), filepath.Join(fixture.changeParent, ".stage"), fixture.witness} {
		if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("later effect %q after EOF: %v", filepath.Base(path), err)
		}
	}
}

func TestCompleteWorkerLeavesExactFDAndGoroutineCensus(t *testing.T) {
	beforeFD := fdCensus(t)
	beforeGoroutines := runtime.NumGoroutine()
	fixture := newWorkerFixture(t)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	fixture.finish(t)
	fixture.close()
	if after := fdCensus(t); !reflect.DeepEqual(after, beforeFD) {
		t.Fatalf("FD census changed: before=%v after=%v", beforeFD, after)
	}
	if after := runtime.NumGoroutine(); after != beforeGoroutines {
		t.Fatalf("goroutine census changed: before=%d after=%d", beforeGoroutines, after)
	}
}

func TestInitialRuntimeChildValidationPrecedesSelectionEffects(t *testing.T) {
	for _, name := range []string{"home", "tmp", "token"} {
		t.Run(name, func(t *testing.T) {
			before := fdCensus(t)
			fixture := newWorkerFixture(t)
			fixture.start(t)
			binding, err := fixture.runtime.Binding()
			if err != nil {
				t.Fatal(err)
			}
			var path string
			switch name {
			case "home":
				path, err = binding.ProviderHome()
			case "tmp":
				path, err = binding.ProviderTemp()
			case "token":
				path, err = binding.AttemptTokenPath()
			}
			if err != nil {
				t.Fatal(err)
			}
			if err := os.Chmod(path, 0o755); err != nil {
				t.Fatal(err)
			}
			if err := fixture.controller.Release(runner.StageSelection); err != nil {
				t.Fatal(err)
			}
			event, err := fixture.controller.Next(workerEventPatience)
			if !errors.Is(err, io.EOF) || event.Kind != "" || event.Stage != "" || event.Identity.Valid() || len(event.Payload) != 0 || event.Result != nil {
				t.Fatalf("event=%+v err=%v diag=%q", event, err, fixture.output())
			}
			if exit, err := fixture.child.FinishAfterExit(workerEventPatience); err != nil || exit.Code == 0 && exit.Signal == 0 {
				t.Fatalf("outer=%+v err=%v", exit, err)
			}
			for _, effect := range []string{filepath.Join(fixture.changeParent, fixture.finalName), filepath.Join(fixture.changeParent, ".stage"), fixture.witness} {
				if _, err := os.Lstat(effect); !errors.Is(err, os.ErrNotExist) {
					t.Fatalf("pre-validation effect %q: %v", effect, err)
				}
			}
			fixture.close()
			if after := fdCensus(t); !reflect.DeepEqual(after, before) {
				t.Fatalf("%s FD census changed: before=%v after=%v", name, before, after)
			}
		})
	}
}

func TestFinalReinspectionRejectsLateGitMetadata(t *testing.T) {
	fixture := newWorkerFixture(t)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	published := filepath.Join(fixture.changeParent, fixture.finalName)
	if err := os.Mkdir(filepath.Join(published, ".GiT"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	record := fixture.finish(t, false)
	if diagnostic := fixture.output(); !strings.Contains(diagnostic, "invalid Change input: .git path components are forbidden") {
		t.Fatalf("missing exact late-metadata rejection: %q", diagnostic)
	}
	if code, ok := record.Result().Code(); ok && code == 0 {
		t.Fatalf("late forbidden metadata reached provider: %+v", record.Result())
	}
	if _, err := os.Stat(fixture.witness); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider witness exists after failed final scan")
	}
}

func TestFinalReinspectionRejectsSameSizeContentMutation(t *testing.T) {
	fixture := newWorkerFixture(t)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	payload := filepath.Join(fixture.changeParent, fixture.finalName, "payload.txt")
	if err := os.WriteFile(payload, []byte("wrong bytes!\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	record := fixture.finish(t, false)
	if diagnostic := fixture.output(); !strings.Contains(diagnostic, "invalid Change input: published Change facts changed") {
		t.Fatalf("missing exact content rejection: %q", diagnostic)
	}
	if code, ok := record.Result().Code(); ok && code == 0 {
		t.Fatalf("mutated content reached provider: %+v", record.Result())
	}
	if _, err := os.Stat(fixture.witness); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider witness exists after changed content")
	}
}

func TestFinalRuntimeRecheckRejectsFixedChildReplacement(t *testing.T) {
	fixture := newWorkerFixture(t)
	fixture.start(t)
	fixture.release(t, runner.StageSelection, runner.StageSelection)
	fixture.release(t, runner.StagePreparation, runner.StagePreparation)
	fixture.release(t, runner.StagePopulation, runner.StagePopulation)
	binding, err := fixture.runtime.Binding()
	if err != nil {
		t.Fatal(err)
	}
	home, err := binding.ProviderHome()
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Rename(home, home+".retained"); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(home, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := fixture.controller.Release(runner.StageProvider); err != nil {
		t.Fatal(err)
	}
	record := fixture.finish(t, false)
	if diagnostic := fixture.output(); !strings.Contains(diagnostic, "runtime authority verification: Change worker failed") {
		t.Fatalf("missing exact runtime rejection: %q", diagnostic)
	}
	if code, ok := record.Result().Code(); ok && code == 0 {
		t.Fatalf("replacement HOME reached provider: %+v", record.Result())
	}
	if _, err := os.Stat(fixture.witness); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("provider witness exists after HOME replacement")
	}
}

type workerFixture struct {
	root, repositoryRoot, changeParent, finalName string
	witness, cwdWitness, envWitness               string
	factoryctl, toolPath                          string
	repositoryIdentity                            change.RepositoryIdentity
	runtime                                       *daemon.Runtime
	parent                                        *daemon.RuntimeParent
	home                                          *install.OperationalHome
	dir, lifetime                                 *os.File
	lease                                         *runner.GateLease
	controller                                    *runner.AttemptController
	child                                         *runner.OwnedChild
	diagnostic                                    *os.File
	closed                                        bool
}

func newWorkerFixture(t *testing.T) *workerFixture {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		t.Fatal(err)
	}
	return newWorkerFixtureWithFactoryctl(t, executable)
}

func newWorkerFixtureWithFactoryctl(t *testing.T, factoryctl string) *workerFixture {
	return newWorkerFixtureWithFactoryctlAndInput(t, factoryctl, nil)
}

func newWorkerFixtureWithInput(t *testing.T, input func(witness, cwdWitness, envWitness string) []byte) *workerFixture {
	t.Helper()
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	executable, err = filepath.EvalSymlinks(executable)
	if err != nil {
		t.Fatal(err)
	}
	return newWorkerFixtureWithFactoryctlAndInput(t, executable, input)
}

func newWorkerFixtureWithFactoryctlAndInput(t *testing.T, factoryctl string, input func(witness, cwdWitness, envWitness string) []byte) *workerFixture {
	t.Helper()
	root := workerSecureTempDir(t)
	git := nativeGit(t)
	repository := filepath.Join(root, "repository")
	if err := os.Mkdir(repository, 0o700); err != nil {
		t.Fatal(err)
	}
	runGit(t, git, "init", repository)
	runGit(t, git, "-C", repository, "config", "user.email", "test@example.invalid")
	runGit(t, git, "-C", repository, "config", "user.name", "test")
	if err := os.WriteFile(filepath.Join(repository, "payload.txt"), []byte("exact source\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	runGit(t, git, "-C", repository, "add", "payload.txt")
	runGit(t, git, "-C", repository, "commit", "-m", "base")
	stat, err := os.Stat(repository)
	if err != nil {
		t.Fatal(err)
	}
	sys := stat.Sys().(*syscall.Stat_t)
	repositoryID, err := change.NewRepositoryIdentity(uint64(sys.Dev), sys.Ino)
	if err != nil {
		t.Fatal(err)
	}
	changeParent := filepath.Join(root, "changes")
	if err := os.Mkdir(changeParent, 0o700); err != nil {
		t.Fatal(err)
	}
	homePath := filepath.Join(root, "runtime-home")
	if _, err := install.Init(context.Background(), homePath); err != nil {
		t.Fatal(err)
	}
	operationalHome, err := install.OpenOperationalHome(context.Background(), homePath)
	if err != nil {
		t.Fatal(err)
	}
	runtimes, err := operationalHome.Runtimes()
	if err != nil {
		t.Fatal(err)
	}
	runtimeParentPath := filepath.Join(homePath, "runtimes")
	parent, err := daemon.OpenRuntimeParent(context.Background(), runtimes, runtimeParentPath)
	if err != nil {
		t.Fatal(err)
	}
	const runtimeName = "00000000000000000000000000000001"
	runtimeValue, err := daemon.CreateRuntime(parent, runtimeName)
	if err != nil {
		t.Fatal(err)
	}
	binding, err := runtimeValue.Binding()
	if err != nil {
		t.Fatal(err)
	}
	runtimePath, runtimeID, err := binding.Values()
	if err != nil {
		t.Fatal(err)
	}
	if _, err := runtimeValue.PublishAttemptToken(context.Background(), [32]byte{1}); err != nil {
		t.Fatal(err)
	}
	witness, cwdWitness, envWitness := filepath.Join(root, "provider.witness"), filepath.Join(root, "provider.cwd"), filepath.Join(root, "provider.env")
	toolPath := workerToolPath(t)
	quote := func(value string) string { return "'" + strings.ReplaceAll(value, "'", "'\\''") + "'" }
	// The exact program arrives on the deliberate fd 11 capability. The PTY is
	// untouched until an authenticated terminal command writes interactive data.
	program := []byte(fmt.Sprintf("set -eu\nfor n in 3 9; do test ! -e /dev/fd/$n; done\ntest -e /dev/fd/10\ntest -e /dev/fd/11\ntest ! -e \"$TMPDIR/.provider-task\"\ngit rev-parse --is-inside-work-tree >/dev/null 2>&1 && exit 81 || :\nprintf x > %s\npwd > %s\nenv | sort > %s\nexit\n", quote(witness), quote(cwdWitness), quote(envWitness)))
	if input != nil {
		program = input(witness, cwdWitness, envWitness)
	}
	_, providerTask, err := provider.PrepareTask(kernel.ProviderShell, program)
	if err != nil {
		t.Fatal(err)
	}
	config := changeworker.Config{Provider: kernel.ProviderShell, Role: kernel.RoleWorker, RuntimePath: runtimePath, RuntimeIdentity: runtimeID, GitExecutable: git, FactoryctlExecutable: factoryctl, ToolPath: toolPath, AccountHome: filepath.Join(root, "account"), RepositoryRoot: repository, RepositoryIdentity: repositoryID, Revision: "HEAD", ChangeParent: changeParent, FinalName: "published", StagingName: ".stage", AttemptSocket: "/private/tmp/dark-factory-worker-api.sock", ProviderTask: providerTask}
	workerConfig, err := changeworker.EncodeConfig(config)
	if err != nil {
		t.Fatal(err)
	}
	dir, lifetime, err := runtimeValue.DuplicateRunnerFiles()
	if err != nil {
		t.Fatal(err)
	}
	lease, _, err := runner.CreateGateLease(dir, lifetime, runner.OuterActivationMarkerName)
	if err != nil {
		t.Fatal(err)
	}
	controller, childCapability, err := runner.NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	home, err := binding.ProviderHome()
	if err != nil {
		t.Fatal(err)
	}
	wrapper, err := runner.PrepareExecSpec(runner.ExecSpec{Target: executable, Args: []string{"--change-worker"}, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: home})
	if err != nil {
		t.Fatal(err)
	}
	if err := controller.Configure(runner.AttemptSpec{AttemptID: "attempt", Wrapper: wrapper, MarkerName: runner.InnerActivationMarkerName, ResultName: runner.AttemptResultSpoolName, ResultProof: workerResultProof(t)}); err != nil {
		t.Fatal(err)
	}
	diagnostic, err := os.OpenFile(filepath.Join(root, "diagnostic"), os.O_RDWR|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		t.Fatal(err)
	}
	outer, err := runner.PrepareExecSpec(runner.ExecSpec{Target: executable, Args: []string{"--attempt-runner"}, Env: []string{"PATH=/usr/bin:/bin", "LANG=C"}, Cwd: home, Stdin: workerConfig, Stdout: diagnostic, Stderr: diagnostic, Control: childCapability})
	if err != nil {
		t.Fatal(err)
	}
	child, err := runner.StartBlocked(lease, executable, outer, true)
	_ = childCapability.Close()
	if err != nil {
		t.Fatal(err)
	}
	f := &workerFixture{root: root, repositoryRoot: repository, changeParent: changeParent, finalName: "published", witness: witness, cwdWitness: cwdWitness, envWitness: envWitness, factoryctl: factoryctl, toolPath: toolPath, repositoryIdentity: repositoryID, runtime: runtimeValue, parent: parent, home: operationalHome, dir: dir, lifetime: lifetime, lease: lease, controller: controller, child: child, diagnostic: diagnostic}
	t.Cleanup(f.close)
	return f
}

func workerToolPath(t testing.TB) string {
	t.Helper()
	goExecutable, err := exec.LookPath("go")
	if err != nil {
		t.Fatal(err)
	}
	goExecutable, err = filepath.EvalSymlinks(goExecutable)
	if err != nil {
		t.Fatal(err)
	}
	components := []string{filepath.Dir(goExecutable), "/usr/bin", "/bin"}
	unique := components[:0]
	seen := make(map[string]struct{}, len(components))
	for _, component := range components {
		if _, found := seen[component]; found {
			continue
		}
		seen[component] = struct{}{}
		unique = append(unique, component)
	}
	return strings.Join(unique, string(filepath.ListSeparator))
}

func copyWorkerExecutable(t testing.TB, from, to string) {
	t.Helper()
	source, err := os.Open(from)
	if err != nil {
		t.Fatal(err)
	}
	defer source.Close()
	destination, err := os.OpenFile(to, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o700)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := io.Copy(destination, source); err != nil {
		_ = destination.Close()
		t.Fatal(err)
	}
	if err := destination.Close(); err != nil {
		t.Fatal(err)
	}
}

func (f *workerFixture) close() {
	if f == nil || f.closed {
		return
	}
	f.closed = true
	_ = f.controller.Close()
	_ = f.child.Close()
	_ = f.lease.Close()
	_ = f.lifetime.Close()
	_ = f.dir.Close()
	_ = f.runtime.Close()
	_ = f.parent.Close()
	_ = f.home.Close()
	_ = f.diagnostic.Close()
}

func (f *workerFixture) start(t *testing.T) runner.Identity {
	t.Helper()
	if _, err := f.child.Activate(); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(workerEventPatience)
	if err != nil || event.Kind != runner.AttemptInnerReady {
		t.Fatalf("ready=%+v err=%v diag=%q", event, err, f.output())
	}
	return event.Identity
}
func (f *workerFixture) release(t *testing.T, release, report runner.AttemptStage) runner.AttemptEvent {
	t.Helper()
	if err := f.controller.Release(release); err != nil {
		t.Fatal(err)
	}
	event, err := f.controller.Next(workerEventPatience)
	if err != nil || event.Kind != runner.AttemptCheckpoint || event.Stage != report {
		t.Fatalf("event=%+v err=%v diag=%q", event, err, f.output())
	}
	return event
}

func (f *workerFixture) nextTerminal(t *testing.T, kind runner.TerminalEventKind, correlation uint64) runner.TerminalFrame {
	t.Helper()
	for {
		event, err := f.controller.Next(workerEventPatience)
		if err != nil || event.Kind != runner.AttemptTerminalFrame || event.Frame == nil {
			t.Fatalf("terminal %s event=%+v err=%v diag=%q", kind, event, err, f.output())
		}
		if event.Frame.Kind == kind && event.Frame.Correlation == correlation {
			return *event.Frame
		}
	}
}
func workerResultProof(t testing.TB) runner.ResultProof {
	t.Helper()
	var value [32]byte
	for index := range value {
		value[index] = byte(index + 9)
	}
	proof, err := runner.NewResultProof(value)
	if err != nil {
		t.Fatal(err)
	}
	return proof
}

func (f *workerFixture) finish(t *testing.T, expectedSuccess ...bool) *runner.AttemptResultRecord {
	t.Helper()
	wantSuccess := true
	if len(expectedSuccess) != 0 {
		wantSuccess = expectedSuccess[0]
	}
	var event runner.AttemptEvent
	var err error
	for {
		event, err = f.controller.Next(workerEventPatience)
		if err != nil || event.Kind != runner.AttemptTerminalFrame {
			break
		}
	}
	if err != nil || event.Kind != runner.AttemptResultReady || event.Result == nil {
		t.Fatalf("result=%+v err=%v diag=%q", event, err, f.output())
	}
	record, err := runner.AuthenticateAttemptResult(f.dir, "attempt", event.Result)
	if err != nil {
		t.Fatalf("authenticate result: %v diag=%q", err, f.output())
	}
	exit, err := f.child.FinishAfterExit(workerEventPatience)
	if err != nil || wantSuccess && exit.Code != 0 || !wantSuccess && exit.Code == 0 && exit.Signal == 0 {
		t.Fatalf("outer exit=%+v err=%v diag=%q", exit, err, f.output())
	}
	return record
}
func (f *workerFixture) output() string {
	_ = f.diagnostic.Sync()
	_, _ = f.diagnostic.Seek(0, 0)
	body, _ := io.ReadAll(f.diagnostic)
	_, _ = f.diagnostic.Seek(0, 2)
	return string(body)
}

func nativeGit(t testing.TB) string {
	t.Helper()
	if _, err := os.Stat(change.TrustedGitExecutable); err != nil {
		t.Fatalf("Command Line Tools Git is unavailable: %v", err)
	}
	return change.TrustedGitExecutable
}
func runGit(t testing.TB, git string, args ...string) {
	t.Helper()
	command := exec.Command(git, args...)
	command.Env = []string{"PATH=/usr/bin:/bin", "HOME=" + workerSecureTempDir(t), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null", "GIT_TERMINAL_PROMPT=0"}
	if body, err := command.CombinedOutput(); err != nil {
		t.Fatalf("git failed: %v (%s)", err, body)
	}
}

func workerSecureTempDir(t testing.TB) string {
	t.Helper()
	path, err := os.MkdirTemp("/private/tmp", "dark-factory-worker-")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(path) })
	return path
}

func fdCensus(t testing.TB) map[int][2]uint64 {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	result := make(map[int][2]uint64)
	for _, entry := range entries {
		fd, err := strconv.Atoi(entry.Name())
		if err != nil {
			continue
		}
		var stat unix.Stat_t
		if unix.Fstat(fd, &stat) == nil {
			result[fd] = [2]uint64{uint64(stat.Dev), stat.Ino}
		}
	}
	return result
}
