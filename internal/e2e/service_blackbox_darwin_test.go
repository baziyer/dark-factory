//go:build darwin

package e2e_test

import (
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/install"
)

// TestBlackBoxServiceLifecycle proves the managed launchd installation with
// the real binaries, the real launchctl, and one disposable unique label in
// the user's gui domain. Every file lives under a temporary root; the
// operator's home, ~/.dark-factory, and the production label are never
// touched. Cleanup boots the label out and removes the root even on failure.
func TestBlackBoxServiceLifecycle(t *testing.T) {
	if os.Getenv("DARK_FACTORY_SERVICE_E2E") != "1" {
		t.Skip("service E2E runs only under scripts/go-service-e2e.sh")
	}
	fixture := newBlackBoxFixture(t)
	label := os.Getenv("DARK_FACTORY_E2E_SERVICE_LABEL")
	if !strings.HasPrefix(label, "com.dark-factory.e2e.") {
		t.Fatalf("the harness must mint the disposable label; got %q", label)
	}
	plistDir := filepath.Join(fixture.root, "plists")
	if err := os.Mkdir(plistDir, 0o700); err != nil {
		t.Fatal(err)
	}
	serviceArgs := func(verb string) []string {
		return []string{"service", verb, "--home", fixture.home, "--label", label, "--plist-dir", plistDir}
	}
	t.Cleanup(func() {
		// Guaranteed teardown: the disposable label must not survive the test
		// process regardless of which assertion failed.
		_ = exec.Command("/bin/launchctl", "bootout", fmt.Sprintf("gui/%d/%s", os.Geteuid(), label)).Run()
		awaitNoHomeProcesses(t, fixture.home, 20*time.Second)
	})

	// Install: launchd starts factoryd from the sibling service directory.
	output := fixture.runFactoryctl(t, 0, serviceArgs("install")...)
	state := serviceState(t, output)
	if state.State != "running" || state.PID <= 1 {
		t.Fatalf("install state = %+v (%s)", state, output)
	}
	// A fresh install names the pair page and reports whether it opened it,
	// inside the JSON: the whole result is one parseable document. runFactoryctl
	// gives the child no PATH, so `open` is not findable and no browser can be
	// launched — which is also what keeps this test from throwing a window onto
	// the operator's screen.
	if state.PairPage != "http://127.0.0.1:43123/pair" || state.BrowserOpened {
		t.Fatalf("install pairing report = %+v (%s)", state, output)
	}
	client := fixture.waitClient(t)

	// The managed daemon serves a real task end to end.
	project := fixture.operatorID(t, fixture.runFactoryctl(t, 0, "project", "create", "--name", "Managed", "--root", fixture.repo))
	agent := fixture.operatorID(t, fixture.runFactoryctl(t, 0, "agent", "create", "--project", project, "--name", "Service Smith", "--provider", "shell", "--tool-budget", "4"))
	task := fixture.operatorID(t, fixture.runFactoryctl(t, 0, "task", "add", "--project", project, "--agent", agent, "--title", "Managed service run", "--body", happyPathBody))
	fixture.runFactoryctl(t, 0, "dispatch", "on")
	fixture.awaitTaskStatus(t, client, task, "succeeded", 60*time.Second)

	// Status proves running through the receipt.
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("status")...))
	if state.State != "running" || state.PID <= 1 {
		t.Fatalf("running status = %+v", state)
	}

	// Stop unloads the job, the daemon exits, and the socket dies.
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("stop")...))
	if state.State != "installed" || state.PID != 0 {
		t.Fatalf("stop state = %+v", state)
	}
	awaitSocketGone(t, install.LocalAPISocketPath(fixture.home), 20*time.Second)
	// launchd absence and a closed listener do not prove that factoryd has
	// joined its scheduler and released the home. Do not bootstrap a second
	// owner until the first process has actually left.
	awaitNoHomeProcesses(t, fixture.home, 20*time.Second)
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("status")...))
	if state.State != "installed" {
		t.Fatalf("stopped status = %+v", state)
	}

	// Start serves again: a real restart cycle through launchd.
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("start")...))
	if state.State != "running" && state.State != "installed" {
		t.Fatalf("start state = %+v", state)
	}
	client = fixture.waitClient(t)
	second := fixture.operatorID(t, fixture.runFactoryctl(t, 0, "task", "add", "--project", project, "--agent", agent, "--title", "Managed restart run", "--body", happyPathBody))
	fixture.awaitTaskStatus(t, client, second, "succeeded", 60*time.Second)

	// Uninstall removes the job and every artifact; absence is provable.
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("uninstall")...))
	if state.State != "absent" {
		t.Fatalf("uninstall state = %+v", state)
	}
	awaitSocketGone(t, install.LocalAPISocketPath(fixture.home), 20*time.Second)
	if _, err := os.Lstat(install.ServiceDirectoryPath(fixture.home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("service directory survived uninstall")
	}
	if _, err := os.Lstat(filepath.Join(plistDir, label+".plist")); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("plist survived uninstall")
	}
	print := exec.Command("/bin/launchctl", "print", fmt.Sprintf("gui/%d/%s", os.Geteuid(), label))
	if err := print.Run(); err == nil {
		t.Fatal("launchd job survived uninstall")
	} else {
		var exit *exec.ExitError
		if !errors.As(err, &exit) || exit.ExitCode() != 113 {
			t.Fatalf("launchctl print after uninstall = %v", err)
		}
	}
	state = serviceState(t, fixture.runFactoryctl(t, 0, serviceArgs("status")...))
	if state.State != "absent" {
		t.Fatalf("final status = %+v", state)
	}
	awaitNoHomeProcesses(t, fixture.home, 20*time.Second)
}

type serviceStateOutput struct {
	State         string `json:"state"`
	PID           int    `json:"pid"`
	PairPage      string `json:"pair_page"`
	BrowserOpened bool   `json:"browser_opened"`
}

func serviceState(t *testing.T, output string) serviceStateOutput {
	t.Helper()
	var state serviceStateOutput
	if err := json.Unmarshal([]byte(output), &state); err != nil {
		t.Fatalf("service output %q: %v", output, err)
	}
	return state
}

func awaitSocketGone(t *testing.T, socket string, patience time.Duration) {
	t.Helper()
	deadline := time.Now().Add(patience)
	for time.Now().Before(deadline) {
		connection, err := net.DialTimeout("unix", socket, time.Second)
		if err != nil {
			return
		}
		_ = connection.Close()
		time.Sleep(100 * time.Millisecond)
	}
	t.Fatal("the daemon socket kept accepting after stop")
}

// awaitNoHomeProcesses is the process census: nothing referencing the
// temporary home may survive teardown.
func awaitNoHomeProcesses(t *testing.T, home string, patience time.Duration) {
	t.Helper()
	deadline := time.Now().Add(patience)
	for {
		command := exec.Command("/usr/bin/pgrep", "-f", home)
		output, err := command.Output()
		if err != nil {
			var exit *exec.ExitError
			if errors.As(err, &exit) && exit.ExitCode() == 1 {
				return
			}
			t.Fatalf("pgrep: %v", err)
		}
		if time.Now().After(deadline) {
			survivors := strings.ReplaceAll(strings.TrimSpace(string(output)), "\n", ",")
			t.Fatalf("processes referencing the home survived: pids %s", survivors)
		}
		time.Sleep(200 * time.Millisecond)
	}
}
