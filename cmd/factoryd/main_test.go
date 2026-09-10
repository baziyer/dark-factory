//go:build darwin

package main

import (
	"bytes"
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/daemon"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const testOrigin = "https://factoryd.test.invalid"

func TestVersionDoesNotOpenRuntime(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if exit := run(context.Background(), []string{"--version"}, &stdout, &stderr); exit != 0 || stdout.String() != "factoryd development\n" || stderr.Len() != 0 {
		t.Fatalf("version exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
	}
}

func TestBuildIdentityDoesNotOpenRuntime(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if exit := run(context.Background(), []string{"--build-identity"}, &stdout, &stderr); exit != 0 || !strings.Contains(stdout.String(), `"release":false`) || stderr.Len() != 0 {
		t.Fatalf("build identity exit=%d stdout=%q stderr=%q", exit, stdout.String(), stderr.String())
	}
}

func TestParseOwnsOneFreshHomeAndExactLoopbackBrowserPolicy(t *testing.T) {
	home := filepath.Join(t.TempDir(), "factory")
	tests := []struct {
		name    string
		args    []string
		ok      bool
		address string
		relay   string
	}{
		{name: "default", args: []string{"--home", home}, ok: true, address: defaultBrowserAddress},
		{name: "development address", args: []string{"--home", home, "--development-browser-address", "127.0.0.1:0"}, ok: true, address: "127.0.0.1:0"},
		{name: "development origin", args: []string{"--home", home, "--development-browser-origin", testOrigin}, ok: true},
		{name: "missing home", args: nil},
		{name: "relative home", args: []string{"--home", "relative"}},
		{name: "root home", args: []string{"--home", "/"}},
		{name: "duplicate home", args: []string{"--home", home, "--home", home}},
		{name: "nonloopback", args: []string{"--home", home, "--development-browser-address", "0.0.0.0:43123"}},
		{name: "localhost", args: []string{"--home", home, "--development-browser-address", "localhost:43123"}},
		{name: "duplicate development address", args: []string{"--home", home, "--development-browser-address", "127.0.0.1:43124", "--development-browser-address", "127.0.0.1:43125"}},
		{name: "unknown browser address", args: []string{"--home", home, "--browser-address", "127.0.0.1:43124"}},
		{name: "wildcard origin", args: []string{"--home", home, "--development-browser-origin", "https://*.invalid"}},
		{name: "origin path", args: []string{"--home", home, "--development-browser-origin", testOrigin + "/path"}},
		{name: "duplicate origin", args: []string{"--home", home, "--development-browser-origin", testOrigin, "--development-browser-origin", testOrigin}},
		{name: "production origin is implicit", args: []string{"--home", home, "--development-browser-origin", defaultBrowserOrigin}},
		{name: "relay origin", args: []string{"--home", home, "--relay-origin", "wss://relay.darkfactory.build"}, ok: true, relay: "wss://relay.darkfactory.build"},
		{name: "development relay origin", args: []string{"--home", home, "--relay-origin", "ws://127.0.0.1:8787"}, ok: true, relay: "ws://127.0.0.1:8787"},
		{name: "relay origin over https", args: []string{"--home", home, "--relay-origin", "https://relay.darkfactory.build"}},
		{name: "relay origin with a path", args: []string{"--home", home, "--relay-origin", "wss://relay.darkfactory.build/host"}},
		{name: "relay origin with a query", args: []string{"--home", home, "--relay-origin", "wss://relay.darkfactory.build?a=b"}},
		{name: "duplicate relay origin", args: []string{"--home", home, "--relay-origin", "wss://relay.darkfactory.build", "--relay-origin", "wss://relay.darkfactory.build"}},
		{name: "unknown", args: []string{"--home", home, "--socket", "/private/socket"}},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			configuration, help, ok := parse(test.args)
			if help || ok != test.ok {
				t.Fatalf("parse = %+v, help=%v, ok=%v", configuration, help, ok)
			}
			expectedAddress := test.address
			if expectedAddress == "" {
				expectedAddress = defaultBrowserAddress
			}
			if test.ok && (configuration.home != home || configuration.browserAddress != expectedAddress || len(configuration.browserOrigins) == 0 || configuration.relayOrigin != test.relay) {
				t.Fatalf("valid configuration = %+v", configuration)
			}
		})
	}
	if _, help, ok := parse([]string{"--help"}); !help || !ok {
		t.Fatalf("help = %v, %v", help, ok)
	}
}

func TestProcessServesAPIAndBrowserThenReleasesExactHome(t *testing.T) {
	home := initializedHome(t)
	ctx, cancel := context.WithCancel(context.Background())
	owner, err := openProcess(ctx, testConfig(home))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- owner.wait(ctx) }()

	client := waitOperatorClient(t, home)
	callContext, callCancel := context.WithTimeout(context.Background(), 3*time.Second)
	health, err := client.Health(callContext)
	callCancel()
	if err != nil || !health.Ready {
		t.Fatalf("health = %+v, %v", health, err)
	}
	callContext, callCancel = context.WithTimeout(context.Background(), 3*time.Second)
	status, err := client.WebStatus(callContext)
	callCancel()
	if err != nil || !status.Ready || status.Address != owner.browser.Addr() || status.Path != "/browser" || len(status.Origins) != 1 || status.Origins[0] != testOrigin {
		t.Fatalf("web status = %+v, %v", status, err)
	}

	address := owner.browser.Addr()
	idle, err := net.Dial("unix", install.LocalAPISocketPath(home))
	if err != nil {
		t.Fatal(err)
	}
	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("shutdown = %v", err)
		}
	case <-time.After(5 * time.Second):
		t.Fatal("factoryd did not join cancelled API handler")
	}
	_ = idle.Close()
	assertReleased(t, home, address)
}

func TestProcessPortCollisionCleansLocalAPIAndHomeAuthority(t *testing.T) {
	home := initializedHome(t)
	blocker, err := net.Listen("tcp4", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	defer blocker.Close()
	configuration := testConfig(home)
	configuration.browserAddress = blocker.Addr().String()
	if owner, err := openProcess(context.Background(), configuration); err == nil || owner != nil {
		if owner != nil {
			_ = owner.close()
		}
		t.Fatalf("colliding browser start = %v, %v", owner, err)
	}
	if _, err := os.Lstat(install.LocalAPISocketPath(home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("failed startup retained socket: %v", err)
	}
	reopened, err := install.OpenOperationalHome(context.Background(), home)
	if err != nil {
		t.Fatalf("failed startup retained home lock: %v", err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestSecondProcessCannotSplitHomeOwnershipOrDisruptFirst(t *testing.T) {
	home := initializedHome(t)
	ctx, cancel := context.WithCancel(context.Background())
	owner, err := openProcess(ctx, testConfig(home))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- owner.wait(ctx) }()
	client := waitOperatorClient(t, home)
	if second, err := openProcess(context.Background(), testConfig(home)); err == nil || second != nil {
		if second != nil {
			_ = second.close()
		}
		t.Fatalf("second process = %v, %v", second, err)
	}
	callContext, callCancel := context.WithTimeout(context.Background(), 3*time.Second)
	health, err := client.Health(callContext)
	callCancel()
	if err != nil || !health.Ready {
		t.Fatalf("first process after rejected split = %+v, %v", health, err)
	}
	address := owner.browser.Addr()
	cancel()
	if err := <-done; err != nil {
		t.Fatal(err)
	}
	assertReleased(t, home, address)
}

func TestOwnedListenerFailuresStopAndReleaseWholeProcess(t *testing.T) {
	tests := []struct {
		name string
		stop func(*process) error
	}{
		{name: "local API", stop: func(owner *process) error { return owner.listener.Close() }},
		{name: "browser", stop: func(owner *process) error { return owner.browser.Close() }},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			home := initializedHome(t)
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			owner, err := openProcess(ctx, testConfig(home))
			if err != nil {
				t.Fatal(err)
			}
			address := owner.browser.Addr()
			done := make(chan error, 1)
			go func() { done <- owner.wait(ctx) }()
			if err := test.stop(owner); err != nil {
				t.Fatalf("stop owner: %v", err)
			}
			select {
			case err := <-done:
				if err == nil {
					t.Fatal("unexpected owner stop reported success")
				}
			case <-time.After(5 * time.Second):
				t.Fatal("factoryd did not converge after owned listener stopped")
			}
			assertReleased(t, home, address)
		})
	}
}

func TestRunReportsStartupFailureAndUsage(t *testing.T) {
	var stdout, stderr bytes.Buffer
	if exit := run(context.Background(), nil, &stdout, &stderr); exit != exitUsage || stdout.Len() != 0 || stderr.String() != usage {
		t.Fatalf("usage = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
	stdout.Reset()
	stderr.Reset()
	home := filepath.Join(t.TempDir(), "missing")
	if exit := run(context.Background(), []string{"--home", home}, &stdout, &stderr); exit != exitFailure || stdout.Len() != 0 || !strings.HasPrefix(stderr.String(), "factoryd: ") || !strings.Contains(stderr.String(), "open home parent component") || stderr.String() == "factoryd: runtime unavailable\n" {
		t.Fatalf("failure = exit %d stdout %q stderr %q", exit, stdout.String(), stderr.String())
	}
}

func TestStartupCancellationIsCleanAtEveryStartupPhase(t *testing.T) {
	for _, phase := range []string{"home", "store", "runtime parent", "supervisor spec", "daemon", "recovery sweep", "local API", "listener", "browser", "scheduler"} {
		t.Run(phase, func(t *testing.T) {
			home := initializedHome(t)
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			startupPhaseHook = func(observed string) {
				if observed == phase {
					cancel()
				}
			}
			defer func() { startupPhaseHook = nil }()
			err := serve(ctx, testConfig(home))
			if err != nil {
				t.Fatalf("startup cancellation = %v", err)
			}
			reopened, err := install.OpenOperationalHome(context.Background(), home)
			if err != nil {
				t.Fatalf("home was not released after clean cancellation: %v", err)
			}
			if err := reopened.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestCleanupFailureRetainsDependentAuthorityAndStableCloseResult(t *testing.T) {
	failure := errors.New("injected cleanup failure")
	for _, test := range []struct {
		name string
		set  func()
	}{
		{name: "daemon", set: func() {
			closeDaemon = func(value *daemon.Daemon) error {
				_ = value
				return failure
			}
		}},
		{name: "runtime parent", set: func() {
			calls := 0
			closeRuntimeParent = func(value *daemon.RuntimeParent) error {
				calls++
				if calls == 1 {
					_ = value.Close()
				}
				return failure
			}
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			closeDaemon = func(value *daemon.Daemon) error { return value.Close() }
			closeRuntimeParent = func(value *daemon.RuntimeParent) error { return value.Close() }
			defer func() {
				closeDaemon = func(value *daemon.Daemon) error { return value.Close() }
				closeRuntimeParent = func(value *daemon.RuntimeParent) error { return value.Close() }
			}()
			home := initializedHome(t)
			owner, err := openProcess(context.Background(), testConfig(home))
			if err != nil {
				t.Fatal(err)
			}
			test.set()
			first := owner.close()
			if !errors.Is(first, failure) {
				t.Fatalf("cleanup = %v", first)
			}
			if reopened, openErr := install.OpenOperationalHome(context.Background(), home); openErr == nil {
				_ = reopened.Close()
				t.Fatal("failed cleanup released home authority")
			}
			if second := owner.close(); !errors.Is(second, failure) {
				t.Fatalf("duplicate cleanup = %v", second)
			}
			closeDaemon = func(value *daemon.Daemon) error { return value.Close() }
			closeRuntimeParent = func(value *daemon.RuntimeParent) error { return value.Close() }
			if err := owner.shutdown(); err != nil {
				t.Fatalf("test cleanup = %v", err)
			}
			if reopened, openErr := install.OpenOperationalHome(context.Background(), home); openErr != nil {
				t.Fatalf("cleanup retained home authority: %v", openErr)
			} else if err := reopened.Close(); err != nil {
				t.Fatal(err)
			}
		})
	}
}

func TestConcurrentCloseAndWaitReturnOneStableResult(t *testing.T) {
	home := initializedHome(t)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	owner, err := openProcess(ctx, testConfig(home))
	if err != nil {
		t.Fatal(err)
	}
	address := owner.browser.Addr()
	results := make(chan error, 3)
	go func() { results <- owner.wait(ctx) }()
	go func() { results <- owner.close() }()
	go func() { results <- owner.close() }()
	for index := 0; index < cap(results); index++ {
		if err := <-results; err != nil {
			t.Fatalf("lifecycle result = %v", err)
		}
	}
	if err := owner.wait(ctx); err != nil {
		t.Fatalf("repeated wait = %v", err)
	}
	assertReleased(t, home, address)
}

func initializedHome(t *testing.T) string {
	t.Helper()
	root, err := os.MkdirTemp("/private/tmp", "dark-factory-factoryd-")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(root) })
	home := filepath.Join(root, "factory")
	if socket := install.LocalAPISocketPath(home); len(socket) > install.MaxSocketPathBytes {
		t.Fatalf("api socket path is %d bytes, over the %d-byte budget: %q", len(socket), install.MaxSocketPathBytes, socket)
	}
	if _, err := install.Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	return home
}

func testConfig(home string) config {
	return config{
		home: home, browserAddress: "127.0.0.1:0", browserOrigins: []string{testOrigin},
		gitExecutable: defaultGitExecutable, toolPath: defaultToolPath, baseRevision: defaultBaseRevision,
		runnerExecutable: "/bin/sh", factoryctlExecutable: "/bin/sh",
	}
}

func waitOperatorClient(t *testing.T, home string) *api.OperatorClient {
	t.Helper()
	deadline := time.Now().Add(3 * time.Second)
	socket := install.LocalAPISocketPath(home)
	token := filepath.Join(home, "operator.token")
	for time.Now().Before(deadline) {
		client, err := api.NewOperatorClient(socket, token)
		if err == nil {
			return client
		}
		time.Sleep(10 * time.Millisecond)
	}
	t.Fatal("local API did not become available")
	return nil
}

func assertReleased(t *testing.T, home, browserAddress string) {
	t.Helper()
	if _, err := os.Lstat(install.LocalAPISocketPath(home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("socket remains: %v", err)
	}
	listener, err := net.Listen("tcp4", browserAddress)
	if err != nil {
		t.Fatalf("browser address remains: %v", err)
	}
	_ = listener.Close()
	reopened, err := install.OpenOperationalHome(context.Background(), home)
	if err != nil {
		t.Fatalf("home authority remains: %v", err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestParseSupervisorFlagsAndDefaults(t *testing.T) {
	home := filepath.Join(t.TempDir(), "factory")
	configuration, help, ok := parse([]string{"--home", home})
	if help || !ok || configuration.gitExecutable != defaultGitExecutable || configuration.toolPath != defaultToolPath || configuration.baseRevision != defaultBaseRevision || configuration.runnerExecutable != "" || configuration.factoryctlExecutable != "" {
		t.Fatalf("default supervisor configuration = %+v, help=%v, ok=%v", configuration, help, ok)
	}
	configuration, help, ok = parse([]string{"--home", home, "--git", "/opt/git/bin/git", "--tool-path", "/opt/tools:/usr/bin", "--base-revision", "refs/heads/main", "--runner", "/opt/df/factory-runner", "--factoryctl", "/opt/df/factoryctl"})
	if help || !ok || configuration.gitExecutable != "/opt/git/bin/git" || configuration.toolPath != "/opt/tools:/usr/bin" || !configuration.toolPathExplicit || configuration.baseRevision != "refs/heads/main" || configuration.runnerExecutable != "/opt/df/factory-runner" || configuration.factoryctlExecutable != "/opt/df/factoryctl" {
		t.Fatalf("explicit supervisor configuration = %+v, help=%v, ok=%v", configuration, help, ok)
	}
	for _, test := range []struct {
		name string
		args []string
	}{
		{name: "relative git", args: []string{"--home", home, "--git", "git"}},
		{name: "duplicate git", args: []string{"--home", home, "--git", "/usr/bin/git", "--git", "/usr/bin/git"}},
		{name: "relative tool path entry", args: []string{"--home", home, "--tool-path", "/usr/bin:relative"}},
		{name: "empty tool path", args: []string{"--home", home, "--tool-path", ""}},
		{name: "duplicate tool path", args: []string{"--home", home, "--tool-path", "/usr/bin", "--tool-path", "/usr/bin"}},
		{name: "empty base revision", args: []string{"--home", home, "--base-revision", ""}},
		{name: "option-shaped base revision", args: []string{"--home", home, "--base-revision", "--exec=evil"}},
		{name: "nul base revision", args: []string{"--home", home, "--base-revision", "HEAD\x00"}},
		{name: "duplicate base revision", args: []string{"--home", home, "--base-revision", "HEAD", "--base-revision", "HEAD"}},
		{name: "relative runner", args: []string{"--home", home, "--runner", "factory-runner"}},
		{name: "relative factoryctl", args: []string{"--home", home, "--factoryctl", "factoryctl"}},
	} {
		t.Run(test.name, func(t *testing.T) {
			if configuration, help, ok := parse(test.args); help || ok {
				t.Fatalf("invalid supervisor flag accepted: %+v", configuration)
			}
		})
	}
}

func copyExecutable(t *testing.T, destination string) {
	t.Helper()
	content, err := os.ReadFile("/bin/sh")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(destination, content, 0o755); err != nil {
		t.Fatal(err)
	}
}

func TestDeriveSupervisorSpecResolvesSymlinkedSelfToCommittedSiblings(t *testing.T) {
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	copyExecutable(t, filepath.Join(base, "factoryd"))
	copyExecutable(t, filepath.Join(base, "factory-runner"))
	copyExecutable(t, filepath.Join(base, "factoryctl"))
	link := filepath.Join(base, "factoryd-link")
	if err := os.Symlink(filepath.Join(base, "factoryd"), link); err != nil {
		t.Fatal(err)
	}
	selfExecutable = func() (string, error) { return link, nil }
	defer func() { selfExecutable = os.Executable }()

	home := filepath.Join(base, "home")
	configuration := config{home: home, gitExecutable: defaultGitExecutable, toolPath: defaultToolPath, baseRevision: "refs/heads/main"}
	spec, err := deriveSupervisorSpec(configuration, nil)
	if err != nil {
		t.Fatal(err)
	}
	if spec.RunnerExecutable != filepath.Join(base, "factory-runner") || spec.FactoryctlExecutable != filepath.Join(base, "factoryctl") {
		t.Fatalf("sibling derivation = %+v", spec)
	}
	accountHome, err := install.AccountHome()
	if err != nil {
		t.Fatal(err)
	}
	wantToolPath := filepath.Join(accountHome, ".local", "bin") + string(filepath.ListSeparator) + defaultToolPath
	if spec.GitExecutable != defaultGitExecutable || spec.BaseRevision != "refs/heads/main" || spec.ToolPath != wantToolPath || spec.AccountHome != accountHome {
		t.Fatalf("boot inputs = %+v", spec)
	}
	if spec.ChangeParent != filepath.Join(home, "changes") || spec.AttemptSocket != install.LocalAPISocketPath(home) {
		t.Fatalf("home-derived paths = %+v", spec)
	}
}

func TestDeriveSupervisorSpecRefusesMissingAndUnsafeExecutables(t *testing.T) {
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	copyExecutable(t, filepath.Join(base, "factoryd"))
	copyExecutable(t, filepath.Join(base, "factoryctl"))
	selfExecutable = func() (string, error) { return filepath.Join(base, "factoryd"), nil }
	defer func() { selfExecutable = os.Executable }()
	home := filepath.Join(base, "home")
	valid := config{home: home, gitExecutable: defaultGitExecutable, toolPath: defaultToolPath, baseRevision: defaultBaseRevision}

	if _, err := deriveSupervisorSpec(valid, nil); err == nil {
		t.Fatal("missing sibling factory-runner was accepted")
	}
	copyExecutable(t, filepath.Join(base, "factory-runner"))
	if _, err := deriveSupervisorSpec(valid, nil); err != nil {
		t.Fatalf("complete siblings were refused: %v", err)
	}
	if err := os.Chmod(filepath.Join(base, "factory-runner"), 0o777); err != nil {
		t.Fatal(err)
	}
	if _, err := deriveSupervisorSpec(valid, nil); err == nil {
		t.Fatal("group and world writable runner was accepted")
	}
	if err := os.Chmod(filepath.Join(base, "factory-runner"), 0o755); err != nil {
		t.Fatal(err)
	}
	broken := valid
	broken.gitExecutable = filepath.Join(base, "missing-git")
	if _, err := deriveSupervisorSpec(broken, nil); err == nil {
		t.Fatal("missing git executable was accepted")
	}
	broken = valid
	broken.baseRevision = ""
	if _, err := deriveSupervisorSpec(broken, nil); err == nil {
		t.Fatal("empty base revision policy was accepted")
	}
}

func TestOpenProcessRefusesUnprovableSupervisorSpecAndReleasesHome(t *testing.T) {
	home := initializedHome(t)
	configuration := testConfig(home)
	configuration.gitExecutable = filepath.Join(home, "missing-git")
	if owner, err := openProcess(context.Background(), configuration); err == nil || owner != nil {
		t.Fatalf("unprovable supervisor specification started a process: %v, %v", owner, err)
	}
	reopened, err := install.OpenOperationalHome(context.Background(), home)
	if err != nil {
		t.Fatalf("home was not released after boot refusal: %v", err)
	}
	if err := reopened.Close(); err != nil {
		t.Fatal(err)
	}
}

func TestBootGitTrustMatchesTheExactPerAttemptAuthority(t *testing.T) {
	if !change.TrustedDeveloperGitPath(defaultGitExecutable) {
		t.Fatalf("default git %q would be refused by the per-attempt trust predicate", defaultGitExecutable)
	}
	if _, err := os.Stat(defaultGitExecutable); err != nil {
		t.Skipf("CommandLineTools git is unavailable: %v", err)
	}
	base, err := filepath.EvalSymlinks(t.TempDir())
	if err != nil {
		t.Fatal(err)
	}
	selfExecutable = func() (string, error) { return filepath.Join(base, "factoryd"), nil }
	defer func() { selfExecutable = os.Executable }()
	copyExecutable(t, filepath.Join(base, "factoryd"))
	copyExecutable(t, filepath.Join(base, "factory-runner"))
	copyExecutable(t, filepath.Join(base, "factoryctl"))
	home := filepath.Join(base, "home")
	valid := config{home: home, gitExecutable: defaultGitExecutable, toolPath: defaultToolPath, baseRevision: defaultBaseRevision}
	if _, err := deriveSupervisorSpec(valid, nil); err != nil {
		t.Fatalf("toolchain default git was refused at boot: %v", err)
	}
	for _, untrusted := range []string{"/usr/bin/git", "/bin/sh"} {
		broken := valid
		broken.gitExecutable = untrusted
		_, err := deriveSupervisorSpec(broken, nil)
		if err == nil {
			t.Fatalf("untrusted git %q was accepted at boot", untrusted)
		}
		if untrusted == "/usr/bin/git" && !strings.Contains(err.Error(), "trusted Command Line Tools git") {
			t.Fatalf("refusal does not name the trust requirement: %v", err)
		}
	}
}

func seedOperatorState(t *testing.T, home string, admitAbandonedRun bool) (taskID kernel.TaskID, runID kernel.RunID) {
	t.Helper()
	ctx := context.Background()
	opened, err := install.OpenOperationalHome(ctx, home)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := opened.Close(); err != nil {
			t.Fatal(err)
		}
	}()
	store, err := opened.OpenStore(ctx)
	if err != nil {
		t.Fatal(err)
	}
	defer func() {
		if err := store.Close(); err != nil {
			t.Fatal(err)
		}
	}()
	// Seed times stay monotonic without running ahead of the daemon's own
	// wall clock; any residual skew is drained before the seed returns.
	clock := int64(0)
	tick := func() kernel.UnixMillis {
		t.Helper()
		if now := time.Now().UnixMilli(); now > clock {
			clock = now
		} else {
			clock++
		}
		at, err := kernel.NewUnixMillis(clock)
		if err != nil {
			t.Fatal(err)
		}
		return at
	}
	defer func() {
		for time.Now().UnixMilli() <= clock {
			time.Sleep(time.Millisecond)
		}
	}()
	identifier := func(seed byte, from func([]byte) error) {
		t.Helper()
		if err := from(bytes.Repeat([]byte{seed}, kernel.IDBytes)); err != nil {
			t.Fatal(err)
		}
	}
	var projectID kernel.ProjectID
	identifier(0x31, func(value []byte) error { var err error; projectID, err = kernel.ProjectIDFromBytes(value); return err })
	repo := filepath.Join(filepath.Dir(home), "repo")
	if _, err := store.CreateProject(ctx, kernel.NewProject{ID: projectID, Name: "boot-project", Root: repo}, tick()); err != nil {
		t.Fatal(err)
	}
	var agentID kernel.AgentID
	identifier(0x32, func(value []byte) error { var err error; agentID, err = kernel.AgentIDFromBytes(value); return err })
	if _, err := store.CreateAgent(ctx, kernel.NewAgent{ID: agentID, ProjectID: projectID, Name: "boot-agent", Role: kernel.RoleWorker, Provider: kernel.ProviderShell, ToolBudgetLimit: 1}, tick()); err != nil {
		t.Fatal(err)
	}
	identifier(0x33, func(value []byte) error { var err error; taskID, err = kernel.TaskIDFromBytes(value); return err })
	var incarnation kernel.IncarnationID
	identifier(0x34, func(value []byte) error {
		var err error
		incarnation, err = kernel.IncarnationIDFromBytes(value)
		return err
	})
	if _, err := store.EnqueueTask(ctx, kernel.NewTask{ID: taskID, ProjectID: projectID, AssignedAgentID: agentID, IncarnationID: incarnation, Title: "boot-task", Body: "true"}, tick()); err != nil {
		t.Fatal(err)
	}
	factory, err := store.Factory(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := store.SetDispatch(ctx, factory.Revision, true, tick()); err != nil {
		t.Fatal(err)
	}
	if !admitAbandonedRun {
		return taskID, kernel.RunID{}
	}
	identifier(0x35, func(value []byte) error { var err error; runID, err = kernel.RunIDFromBytes(value); return err })
	var sessionID kernel.TerminalSessionID
	identifier(0x36, func(value []byte) error {
		var err error
		sessionID, err = kernel.TerminalSessionIDFromBytes(value)
		return err
	})
	var attemptDigest kernel.AttemptDigest
	if attemptDigest, err = kernel.AttemptDigestFromBytes(bytes.Repeat([]byte{0x37}, kernel.DigestBytes)); err != nil {
		t.Fatal(err)
	}
	var proofDigest kernel.ResultProofDigest
	if proofDigest, err = kernel.ResultProofDigestFromBytes(bytes.Repeat([]byte{0x38}, kernel.DigestBytes)); err != nil {
		t.Fatal(err)
	}
	var changeID kernel.ChangeID
	identifier(0x39, func(value []byte) error { var err error; changeID, err = kernel.ChangeIDFromBytes(value); return err })
	resource := func(seed byte) kernel.ResourceID {
		t.Helper()
		value, err := kernel.ResourceIDFromBytes(bytes.Repeat([]byte{seed}, kernel.IDBytes))
		if err != nil {
			t.Fatal(err)
		}
		return value
	}
	keys := kernel.AdmissionKeys{
		RunID: runID, TerminalSessionID: sessionID,
		AttemptDigest: attemptDigest, ResultProofDigest: proofDigest, CandidateChangeID: changeID,
		RuntimeRoot: filepath.Join(install.RuntimesPath(home), runID.String()),
		Resources: kernel.AdmissionResourceIDs{
			RuntimeRoot: resource(0x3a), RunnerProcess: resource(0x3b),
			ProviderProcess: resource(0x3c), ProviderGroup: resource(0x3d),
		},
	}
	admission, err := store.AdmitNext(ctx, keys, tick())
	if err != nil || !admission.Admitted() {
		t.Fatalf("admission = %+v, %v", admission, err)
	}
	return taskID, runID
}

// TestBootRecoverySweepConvergesResidueBeforeAnyListener seeds an admitted
// run whose runtime was never created, then proves the boot sweep converges
// it while no listener exists and reports the disposition.
func TestBootRecoverySweepConvergesResidueBeforeAnyListener(t *testing.T) {
	home := initializedHome(t)
	_, runID := seedOperatorState(t, home, true)

	var log bytes.Buffer
	recoveryLog = &log
	defer func() { recoveryLog = os.Stderr }()
	socketAtSweep := errors.New("unobserved")
	phases := make([]string, 0, 16)
	startupPhaseHook = func(observed string) {
		phases = append(phases, observed)
		if observed == "recovery sweep" {
			_, socketAtSweep = os.Lstat(install.LocalAPISocketPath(home))
		}
	}
	defer func() { startupPhaseHook = nil }()

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	owner, err := openProcess(ctx, testConfig(home))
	if err != nil {
		t.Fatal(err)
	}
	done := make(chan error, 1)
	go func() { done <- owner.wait(ctx) }()
	address := owner.browser.Addr()

	if !errors.Is(socketAtSweep, os.ErrNotExist) {
		t.Fatalf("local API socket existed when the sweep completed: %v", socketAtSweep)
	}
	sweepIndex, listenerIndex := -1, -1
	for index, phase := range phases {
		switch phase {
		case "recovery sweep":
			sweepIndex = index
		case "local API":
			listenerIndex = index
		}
	}
	if sweepIndex == -1 || listenerIndex == -1 || sweepIndex > listenerIndex {
		t.Fatalf("phase order = %v", phases)
	}
	line := log.String()
	if !strings.Contains(line, runID.String()) || !strings.Contains(line, string(daemon.RecoveredRuntimeAbsent)) {
		t.Fatalf("recovery log = %q", line)
	}

	// The recovered run is durably settled to a terminal failed task; the
	// factory serves clients only after that conversion was in the log.
	client := waitOperatorClient(t, home)
	callContext, callCancel := context.WithTimeout(context.Background(), 3*time.Second)
	snapshot, err := client.Snapshot(callContext)
	callCancel()
	if err != nil {
		t.Fatal(err)
	}
	recovered := ""
	for _, task := range snapshot.Tasks {
		recovered = task.Status
	}
	if recovered != "failed" {
		t.Fatalf("recovered task status = %q", recovered)
	}
	cancel()
	if err := <-done; err != nil {
		t.Fatalf("shutdown = %v", err)
	}
	assertReleased(t, home, address)
}

// TestSchedulerDrivesQueuedTaskAndJoinsBeforeDaemonClose proves the wired
// scheduler admits a queued task at boot, drives it through a real attempt to
// a terminal task status, and is joined before the daemon closes.
func TestSchedulerDrivesQueuedTaskAndJoinsBeforeDaemonClose(t *testing.T) {
	home := initializedHome(t)
	taskID, _ := seedOperatorState(t, home, false)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	owner, err := openProcess(ctx, testConfig(home))
	if err != nil {
		t.Fatal(err)
	}
	joinedBeforeDaemonClose := errors.New("daemon close was never observed")
	previousCloseDaemon := closeDaemon
	closeDaemon = func(value *daemon.Daemon) error {
		select {
		case <-owner.schedulerDone:
			joinedBeforeDaemonClose = nil
		default:
			joinedBeforeDaemonClose = errors.New("daemon closed before the scheduler joined")
		}
		return previousCloseDaemon(value)
	}
	defer func() { closeDaemon = previousCloseDaemon }()
	done := make(chan error, 1)
	go func() { done <- owner.wait(ctx) }()
	address := owner.browser.Addr()

	client := waitOperatorClient(t, home)
	deadline := time.Now().Add(30 * time.Second)
	status := ""
	for time.Now().Before(deadline) {
		callContext, callCancel := context.WithTimeout(context.Background(), 3*time.Second)
		snapshot, err := client.Snapshot(callContext)
		callCancel()
		if err != nil {
			select {
			case shutdown := <-done:
				t.Fatalf("factoryd stopped while the task was scheduled: %v", shutdown)
			default:
				t.Fatal(err)
			}
		}
		for _, task := range snapshot.Tasks {
			if task.ID == taskID.String() {
				status = task.Status
			}
		}
		if status != "queued" && status != "running" {
			break
		}
		time.Sleep(25 * time.Millisecond)
	}
	if status != "failed" {
		t.Fatalf("scheduled task status = %q", status)
	}

	cancel()
	select {
	case err := <-done:
		if err != nil {
			t.Fatalf("shutdown = %v", err)
		}
	case <-time.After(20 * time.Second):
		t.Fatal("factoryd did not join the scheduler on cancellation")
	}
	if joinedBeforeDaemonClose != nil {
		t.Fatal(joinedBeforeDaemonClose)
	}
	assertReleased(t, home, address)
}
