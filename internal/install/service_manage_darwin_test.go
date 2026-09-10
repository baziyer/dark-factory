//go:build darwin

package install

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

const manageTestLabel = "com.dark-factory.test.manage"

type manageFixture struct {
	root      string
	home      string
	userHome  string
	plistDir  string
	sourceDir string
	config    ServiceConfig
}

func newManageFixture(t *testing.T) *manageFixture {
	t.Helper()
	root := serviceTestRoot(t)
	fixture := &manageFixture{
		root:     root,
		home:     filepath.Join(root, "factory"),
		userHome: filepath.Join(root, "user"),
		plistDir: filepath.Join(root, "plists"),
	}
	fixture.sourceDir = filepath.Join(root, "release")
	fixture.config = ServiceConfig{Label: manageTestLabel, PlistDirectory: fixture.plistDir}
	for _, path := range []string{fixture.userHome, fixture.plistDir, fixture.sourceDir} {
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := Init(context.Background(), fixture.home); err != nil {
		t.Fatal(err)
	}
	for _, name := range serviceBinaryNames {
		if err := os.WriteFile(filepath.Join(fixture.sourceDir, name), []byte("#!binary "+name+"\n"), 0o700); err != nil {
			t.Fatal(err)
		}
	}
	return fixture
}

func (fixture *manageFixture) plistPath() string {
	return filepath.Join(fixture.plistDir, fixture.config.plistName())
}

func (fixture *manageFixture) service() string {
	return fmt.Sprintf("gui/%d/%s", os.Geteuid(), fixture.config.Label)
}

func (fixture *manageFixture) printRunning(pid int) launchctlResult {
	output := fixture.service() + " = {\n\tpath = " + fixture.plistPath() + "\n\tstate = running\n\tprogram = " + serviceProgramPath(fixture.home) + "\n\tpid = " + fmt.Sprint(pid) + "\n}\n"
	return launchctlResult{status: 0, stdout: []byte(output)}
}

func (fixture *manageFixture) printIdle() launchctlResult {
	output := fixture.service() + " = {\n\tpath = " + fixture.plistPath() + "\n\tstate = not running\n\tprogram = " + serviceProgramPath(fixture.home) + "\n}\n"
	return launchctlResult{status: 0, stdout: []byte(output)}
}

func (fixture *manageFixture) printAbsent() []launchctlResult {
	return []launchctlResult{{status: launchctlNotFound}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}
}

func (fixture *manageFixture) install(t *testing.T, launchctl launchctlRun) ServiceStatus {
	t.Helper()
	status, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, launchctl)
	if err != nil {
		t.Fatalf("install: %v", err)
	}
	return status
}

func TestServiceReceiptIsCanonicalAndTamperEvident(t *testing.T) {
	digest := strings.Repeat("ab", sha256.Size)
	receipt := serviceReceipt{Version: serviceReceiptVersion, Label: manageTestLabel, PlistPath: "/private/tmp/plists/" + manageTestLabel + ".plist", PlistDigest: digest, ProgramDigest: digest}
	body, err := encodeServiceReceipt(receipt)
	if err != nil {
		t.Fatal(err)
	}
	parsed, err := parseServiceReceipt(body)
	if err != nil || parsed != receipt {
		t.Fatalf("round trip = %+v, %v", parsed, err)
	}
	for name, mutated := range map[string][]byte{
		"trailing junk":  append(append([]byte{}, body...), 'x'),
		"reordered keys": []byte(`{"label":"` + manageTestLabel + `","version":1,"plist_path":"/private/tmp/p.plist","plist_digest":"` + digest + `","program_digest":"` + digest + `"}` + "\n"),
		"empty":          nil,
	} {
		if _, err := parseServiceReceipt(mutated); !errors.Is(err, ErrServiceReceipt) {
			t.Fatalf("%s accepted: %v", name, err)
		}
	}
	for name, bad := range map[string]serviceReceipt{
		"version":     {Version: 2, Label: receipt.Label, PlistPath: receipt.PlistPath, PlistDigest: digest, ProgramDigest: digest},
		"label":       {Version: 1, Label: ".bad.", PlistPath: receipt.PlistPath, PlistDigest: digest, ProgramDigest: digest},
		"digest hex":  {Version: 1, Label: receipt.Label, PlistPath: receipt.PlistPath, PlistDigest: strings.Repeat("zz", sha256.Size), ProgramDigest: digest},
		"plist name":  {Version: 1, Label: receipt.Label, PlistPath: "/private/tmp/plists/other.plist", PlistDigest: digest, ProgramDigest: digest},
		"digest size": {Version: 1, Label: receipt.Label, PlistPath: receipt.PlistPath, PlistDigest: digest[:10], ProgramDigest: digest},
	} {
		if _, err := encodeServiceReceipt(bad); !errors.Is(err, ErrServiceReceipt) {
			t.Fatalf("%s encoded: %v", name, err)
		}
	}
}

func TestServiceLabelValidationIsExact(t *testing.T) {
	for _, label := range []string{DefaultServiceLabel, "com.dark-factory.e2e.12345", "A1-b.c"} {
		if !validServiceLabel(label) {
			t.Fatalf("valid label rejected: %q", label)
		}
	}
	for _, label := range []string{"", ".lead", "trail.", "a..b", "with space", "with/slash", "with\x00nul", strings.Repeat("a", serviceMaxLabelBytes+1)} {
		if validServiceLabel(label) {
			t.Fatalf("invalid label accepted: %q", label)
		}
	}
}

func TestServiceInstallLifecycleConvergesThroughEveryVerb(t *testing.T) {
	fixture := newManageFixture(t)
	install := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(4321))}
	status := fixture.install(t, install.run)
	if status != (ServiceStatus{State: ServiceRunning, PID: 4321}) {
		t.Fatalf("install status = %+v", status)
	}
	if got := install.calls[2]; len(got) != 3 || got[0] != "bootstrap" || got[1] != fmt.Sprintf("gui/%d", os.Geteuid()) || got[2] != fixture.plistPath() {
		t.Fatalf("bootstrap call = %q", install.calls[2])
	}

	// The durable artifacts exist with exact content.
	receipt, present, err := readServiceReceipt(fixture.home)
	if err != nil || !present || receipt.Label != manageTestLabel || receipt.PlistPath != fixture.plistPath() {
		t.Fatalf("receipt = %+v present=%t err=%v", receipt, present, err)
	}
	source, err := os.ReadFile(filepath.Join(fixture.sourceDir, "factoryd"))
	if err != nil {
		t.Fatal(err)
	}
	wantProgram := sha256.Sum256(source)
	if receipt.ProgramDigest != hex.EncodeToString(wantProgram[:]) {
		t.Fatal("receipt program digest is not the installed binary's digest")
	}
	for _, name := range serviceBinaryNames {
		installed, err := os.ReadFile(filepath.Join(ServiceDirectoryPath(fixture.home), "bin", "current", name))
		if err != nil || len(installed) == 0 {
			t.Fatalf("installed %s = %v", name, err)
		}
	}

	// Status proves running through the receipt.
	inspect := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(4321)}}
	status, err = inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, fixture.config, inspect.run)
	if err != nil || status != (ServiceStatus{State: ServiceRunning, PID: 4321}) {
		t.Fatalf("running status = %+v, %v", status, err)
	}

	// Idempotent install over a running service recognizes and changes nothing.
	again := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(4321)}}
	status, err = serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, again.run)
	if err != nil || status.State != ServiceRunning || len(again.calls) != 1 {
		t.Fatalf("repeat install = %+v, %v, calls=%d", status, err, len(again.calls))
	}

	// Stop keeps the installation; the loaded job leaves launchd.
	stop := &recordedLaunchctl{results: append([]launchctlResult{fixture.printRunning(4321), {status: 0}}, launchctlResult{status: launchctlNotFound})}
	status, err = serviceStopAt(context.Background(), fixture.home, fixture.userHome, fixture.config, stop.run)
	if err != nil || status.State != ServiceInstalled {
		t.Fatalf("stop = %+v, %v", status, err)
	}
	if got := stop.calls[1]; len(got) != 2 || got[0] != "bootout" || got[1] != fixture.service() {
		t.Fatalf("bootout call = %q", stop.calls[1])
	}
	alreadyStopped := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err = serviceStopAt(context.Background(), fixture.home, fixture.userHome, fixture.config, alreadyStopped.run)
	if err != nil || status.State != ServiceInstalled || len(alreadyStopped.calls) != 2 {
		t.Fatalf("repeat stop = %+v, %v, calls=%q", status, err, alreadyStopped.calls)
	}
	loadedIdleStop := &recordedLaunchctl{results: []launchctlResult{fixture.printIdle(), {status: 0}, {status: launchctlNotFound}}}
	status, err = serviceStopAt(context.Background(), fixture.home, fixture.userHome, fixture.config, loadedIdleStop.run)
	if err != nil || status.State != ServiceInstalled || len(loadedIdleStop.calls) != 3 || loadedIdleStop.calls[1][0] != "bootout" {
		t.Fatalf("loaded-idle stop = %+v, %v, calls=%q", status, err, loadedIdleStop.calls)
	}

	// Stopped status is installed, not ambiguous.
	stopped := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err = inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, fixture.config, stopped.run)
	if err != nil || status != (ServiceStatus{State: ServiceInstalled}) {
		t.Fatalf("stopped status = %+v, %v", status, err)
	}

	// Start bootstraps the retained plist.
	start := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(4400))}
	status, err = serviceStartAt(context.Background(), fixture.home, fixture.userHome, fixture.config, start.run)
	if err != nil || status != (ServiceStatus{State: ServiceRunning, PID: 4400}) {
		t.Fatalf("start = %+v, %v", status, err)
	}
	loadedIdleStart := &recordedLaunchctl{results: []launchctlResult{fixture.printIdle(), {status: 0}, {status: launchctlNotFound}, {status: 0}, fixture.printRunning(4500)}}
	status, err = serviceStartAt(context.Background(), fixture.home, fixture.userHome, fixture.config, loadedIdleStart.run)
	if err != nil || status != (ServiceStatus{State: ServiceRunning, PID: 4500}) || len(loadedIdleStart.calls) != 5 || loadedIdleStart.calls[1][0] != "bootout" || loadedIdleStart.calls[3][0] != "bootstrap" {
		t.Fatalf("loaded-idle start = %+v, %v, calls=%q", status, err, loadedIdleStart.calls)
	}
	if err := os.WriteFile(serviceStderrPath(fixture.home), []byte("factoryd: serve error\n"), 0o600); err != nil {
		t.Fatal(err)
	}

	// Uninstall removes every artifact and proves absence.
	uninstall := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(4400), {status: 0}, {status: launchctlNotFound}}}
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, uninstall.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("uninstall = %+v, %v", status, err)
	}
	if _, err := os.Lstat(ServiceDirectoryPath(fixture.home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("service directory survived uninstall")
	}
	if _, err := os.Lstat(fixture.plistPath()); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("plist survived uninstall")
	}
	final := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err = inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, fixture.config, final.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("final status = %+v, %v", status, err)
	}
}

func TestServiceUninstallAcceptsAReceiptBoundPriorPlist(t *testing.T) {
	fixture := newManageFixture(t)
	fixture.install(t, (&recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(77))}).run)
	current, err := os.ReadFile(fixture.plistPath())
	if err != nil {
		t.Fatal(err)
	}
	prior := bytes.Replace(current, []byte("    <key>StandardErrorPath</key>\n    <string>"+serviceStderrPath(fixture.home)+"</string>\n"), nil, 1)
	if bytes.Equal(prior, current) {
		t.Fatal("current plist did not contain the new stderr path")
	}
	if err := os.WriteFile(fixture.plistPath(), prior, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(filepath.Join(ServiceDirectoryPath(fixture.home), "bin", "current", "factoryctl")); err != nil {
		t.Fatal(err)
	}
	priorDigest := sha256.Sum256(prior)
	receipt, present, err := readServiceReceipt(fixture.home)
	if err != nil || !present {
		t.Fatalf("receipt = %+v present=%t err=%v", receipt, present, err)
	}
	receipt.PlistDigest = hex.EncodeToString(priorDigest[:])
	receiptBody, err := encodeServiceReceipt(receipt)
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(ServiceDirectoryPath(fixture.home), serviceReceiptName), receiptBody, 0o600); err != nil {
		t.Fatal(err)
	}
	removed := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(77), {status: 0}, {status: launchctlNotFound}}}
	status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, removed.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("prior plist uninstall = %+v, %v", status, err)
	}
}

func TestServiceInstallRefusesForeignPlistAndResidue(t *testing.T) {
	fixture := newManageFixture(t)
	if err := os.WriteFile(fixture.plistPath(), []byte("foreign bytes"), 0o600); err != nil {
		t.Fatal(err)
	}
	foreign := &recordedLaunchctl{results: fixture.printAbsent()}
	_, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, foreign.run)
	if err == nil || errors.Is(err, ErrServiceForeign) == false && errors.Is(err, ErrServiceAmbiguous) == false {
		t.Fatalf("foreign plist install = %v", err)
	}
	if err := os.Remove(fixture.plistPath()); err != nil {
		t.Fatal(err)
	}

	// Crash residue: a receipt with no plist is surfaced as residue, install
	// refuses, and uninstall resolves it to provable absence.
	full := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(77))}
	fixture.install(t, full.run)
	if err := os.Remove(fixture.plistPath()); err != nil {
		t.Fatal(err)
	}
	residue := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err := inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, fixture.config, residue.run)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceResidue) {
		t.Fatalf("residue status = %+v, %v", status, err)
	}
	refuse := &recordedLaunchctl{results: fixture.printAbsent()}
	if _, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, refuse.run); !errors.Is(err, ErrServiceResidue) {
		t.Fatalf("install over residue = %v", err)
	}
	resolve := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, resolve.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("uninstall residue = %+v, %v", status, err)
	}
}

func TestServiceStatusRefusesTamperedProgramAndForeignUninstall(t *testing.T) {
	fixture := newManageFixture(t)
	full := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(88))}
	fixture.install(t, full.run)

	program := serviceProgramPath(fixture.home)
	if err := os.WriteFile(program, []byte("#!tampered\n"), 0o700); err != nil {
		t.Fatal(err)
	}
	tampered := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(88)}}
	status, err := inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, fixture.config, tampered.run)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("tampered program status = %+v, %v", status, err)
	}

	// A foreign file inside the service tree blocks uninstall's directory
	// removal instead of being deleted.
	extra := filepath.Join(ServiceDirectoryPath(fixture.home), "bin", "current", "keepsake")
	if err := os.WriteFile(extra, []byte("not ours to delete"), 0o600); err != nil {
		t.Fatal(err)
	}
	blocked := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(88), {status: 0}, {status: launchctlNotFound}}}
	if _, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, blocked.run); !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("uninstall with foreign file = %v", err)
	}
	if _, statErr := os.Lstat(extra); statErr != nil {
		t.Fatal("foreign file was deleted")
	}
}

func TestServiceMutationsValidateRequestsBeforeTouchingAnything(t *testing.T) {
	fixture := newManageFixture(t)
	deny := func(context.Context, ...string) launchctlResult {
		t.Fatal("launchctl invoked for an invalid request")
		return launchctlResult{}
	}
	badConfig := ServiceConfig{Label: "..bad", PlistDirectory: fixture.plistDir}
	if _, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, badConfig, fixture.sourceDir, deny); !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("bad label install = %v", err)
	}
	if _, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, "relative/source", deny); !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("bad source install = %v", err)
	}
	if _, err := serviceStartAt(nil, fixture.home, fixture.userHome, fixture.config, deny); !errors.Is(err, ErrServiceAmbiguous) { //nolint:staticcheck
		t.Fatalf("nil context start = %v", err)
	}

	// A group-writable source binary is refused before any artifact exists.
	if err := os.Chmod(filepath.Join(fixture.sourceDir, "factoryctl"), 0o770); err != nil {
		t.Fatal(err)
	}
	absent := &recordedLaunchctl{results: fixture.printAbsent()}
	if _, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, absent.run); !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("group-writable source = %v", err)
	}
	var stat unix.Stat_t
	if err := unix.Lstat(fixture.plistPath(), &stat); !errors.Is(err, unix.ENOENT) {
		t.Fatal("plist written despite refused source")
	}
	if _, present, err := readServiceReceipt(fixture.home); err != nil || present {
		t.Fatalf("receipt written despite refused source: present=%t err=%v", present, err)
	}
}

func TestServiceMutationLockRejectsEveryConcurrentVerbBeforeLaunchctl(t *testing.T) {
	fixture := newManageFixture(t)
	var calls atomic.Int64
	launchctl := func(context.Context, ...string) launchctlResult {
		calls.Add(1)
		return launchctlResult{status: -1, err: errors.New("launchctl must not run")}
	}
	_, err := withServiceMutation(context.Background(), fixture.home, func(*serviceHomeCapability) (ServiceStatus, error) {
		tests := []struct {
			name string
			run  func() (ServiceStatus, error)
		}{
			{"install", func() (ServiceStatus, error) {
				return serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, launchctl)
			}},
			{"start", func() (ServiceStatus, error) {
				return serviceStartAt(context.Background(), fixture.home, fixture.userHome, fixture.config, launchctl)
			}},
			{"stop", func() (ServiceStatus, error) {
				return serviceStopAt(context.Background(), fixture.home, fixture.userHome, fixture.config, launchctl)
			}},
			{"uninstall", func() (ServiceStatus, error) {
				return serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, launchctl)
			}},
		}
		for _, test := range tests {
			if _, err := test.run(); !errors.Is(err, ErrBusy) {
				t.Fatalf("%s concurrent mutation = %v", test.name, err)
			}
		}
		return ServiceStatus{State: ServiceAbsent}, nil
	})
	if err != nil {
		t.Fatalf("hold mutation lock: %v", err)
	}
	if calls.Load() != 0 {
		t.Fatalf("concurrent mutations made %d launchctl calls", calls.Load())
	}
}

func TestServiceMutationLockIsIndependentFromOperationalHomeLock(t *testing.T) {
	t.Run("service then operational", func(t *testing.T) {
		fixture := newManageFixture(t)
		_, err := withServiceMutation(context.Background(), fixture.home, func(*serviceHomeCapability) (ServiceStatus, error) {
			home, err := OpenOperationalHome(context.Background(), fixture.home)
			if err != nil {
				return ServiceStatus{}, err
			}
			return ServiceStatus{State: ServiceAbsent}, home.Close()
		})
		if err != nil {
			t.Fatalf("operational lock while service lock held: %v", err)
		}
	})

	t.Run("operational then service", func(t *testing.T) {
		fixture := newManageFixture(t)
		home, err := OpenOperationalHome(context.Background(), fixture.home)
		if err != nil {
			t.Fatal(err)
		}
		defer func() {
			if err := home.Close(); err != nil {
				t.Errorf("close operational home: %v", err)
			}
		}()
		if _, err := withServiceMutation(context.Background(), fixture.home, func(*serviceHomeCapability) (ServiceStatus, error) {
			return ServiceStatus{State: ServiceAbsent}, nil
		}); err != nil {
			t.Fatalf("service lock while operational lock held: %v", err)
		}
	})
}

func TestServiceUninstallIsEvidenceFirst(t *testing.T) {
	fixture := newManageFixture(t)
	deny := func(context.Context, ...string) launchctlResult {
		t.Fatal("a mutating launchctl verb ran without ownership evidence")
		return launchctlResult{}
	}

	// A foreign plist under the requested label refuses the whole uninstall
	// before any mutation: no bootout, no deletion, bytes retained.
	if err := os.WriteFile(fixture.plistPath(), []byte("operator property"), 0o600); err != nil {
		t.Fatal(err)
	}
	status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, deny)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("foreign plist uninstall = %+v, %v", status, err)
	}
	body, readErr := os.ReadFile(fixture.plistPath())
	if readErr != nil || string(body) != "operator property" {
		t.Fatalf("foreign plist mutated: %q, %v", body, readErr)
	}
	if err := os.Remove(fixture.plistPath()); err != nil {
		t.Fatal(err)
	}

	// No receipt, no plist: nothing proves the label, so no launchctl verb
	// runs at all; the empty result is provable absence.
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, deny)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("no-evidence uninstall = %+v, %v", status, err)
	}

	// A valid receipt for a DIFFERENT label refuses: deleting this home's
	// service directory would break that label's installation.
	full := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(91))}
	fixture.install(t, full.run)
	otherConfig := fixture.config
	otherConfig.Label = manageTestLabel + ".other"
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, otherConfig, deny)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("label-mismatch uninstall = %+v, %v", status, err)
	}
	otherDirectory := fixture.config
	otherDirectory.PlistDirectory = filepath.Join(fixture.root, "other-plists")
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, otherDirectory, deny)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceForeign) {
		t.Fatalf("plist-path-mismatch uninstall = %+v, %v", status, err)
	}

	// With the matching receipt an exact observation precedes the mutating
	// bootout, and removal completes as before.
	proven := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(91), {status: 0}, {status: launchctlNotFound}}}
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, proven.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("proven uninstall = %+v, %v", status, err)
	}
	if len(proven.calls) < 2 || proven.calls[0][0] != "print" || proven.calls[1][0] != "bootout" {
		t.Fatalf("proven uninstall verbs = %q", proven.calls)
	}
}

func TestServiceUninstallSkipsBootoutWhenAlreadyUnloaded(t *testing.T) {
	fixture := newManageFixture(t)
	installed := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(92))}
	fixture.install(t, installed.run)

	unloaded := &recordedLaunchctl{results: fixture.printAbsent()}
	status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, unloaded.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("unloaded uninstall = %+v, %v", status, err)
	}
	if len(unloaded.calls) != 2 || unloaded.calls[0][0] != "print" || unloaded.calls[1][0] != "error" {
		t.Fatalf("unloaded uninstall verbs = %q", unloaded.calls)
	}
	if _, err := os.Lstat(ServiceDirectoryPath(fixture.home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("service directory survived unloaded uninstall")
	}
	if _, err := os.Lstat(fixture.plistPath()); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("plist survived unloaded uninstall")
	}
}

func TestServiceUninstallSerializesAbsentObservationAgainstStart(t *testing.T) {
	fixture := newManageFixture(t)
	installed := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(94))}
	fixture.install(t, installed.run)

	var loaded atomic.Bool
	var startCalls atomic.Int64
	startLaunchctl := func(_ context.Context, arguments ...string) launchctlResult {
		startCalls.Add(1)
		switch arguments[0] {
		case "print":
			if loaded.Load() {
				return fixture.printRunning(95)
			}
			return launchctlResult{status: launchctlNotFound}
		case "error":
			return launchctlResult{status: 0, stdout: []byte(launchctlNotFoundText + "\n")}
		case "bootstrap":
			loaded.Store(true)
			return launchctlResult{status: 0}
		default:
			return launchctlResult{status: -1, err: fmt.Errorf("unexpected start verb %q", arguments[0])}
		}
	}

	observedAbsent := make(chan struct{})
	releaseObservation := make(chan struct{})
	var observedOnce sync.Once
	var releaseOnce sync.Once
	defer releaseOnce.Do(func() { close(releaseObservation) })
	uninstallLaunchctl := func(_ context.Context, arguments ...string) launchctlResult {
		switch arguments[0] {
		case "print":
			// Capture absence before exposing the interleaving. Without the
			// mutation lock, start can bootstrap while this stale observation
			// is paused and uninstall will then delete its live artifacts.
			wasLoaded := loaded.Load()
			observedOnce.Do(func() { close(observedAbsent) })
			<-releaseObservation
			if wasLoaded {
				return fixture.printRunning(95)
			}
			return launchctlResult{status: launchctlNotFound}
		case "error":
			return launchctlResult{status: 0, stdout: []byte(launchctlNotFoundText + "\n")}
		case "bootout":
			loaded.Store(false)
			return launchctlResult{status: 0}
		default:
			return launchctlResult{status: -1, err: fmt.Errorf("unexpected uninstall verb %q", arguments[0])}
		}
	}

	type result struct {
		status ServiceStatus
		err    error
	}
	uninstallDone := make(chan result, 1)
	go func() {
		status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, uninstallLaunchctl)
		uninstallDone <- result{status: status, err: err}
	}()
	select {
	case <-observedAbsent:
	case <-time.After(3 * time.Second):
		t.Fatal("uninstall did not reach the absent observation")
	}

	startDone := make(chan result, 1)
	go func() {
		status, err := serviceStartAt(context.Background(), fixture.home, fixture.userHome, fixture.config, startLaunchctl)
		startDone <- result{status: status, err: err}
	}()
	var start result
	select {
	case start = <-startDone:
	case <-time.After(3 * time.Second):
		t.Fatal("concurrent start did not refuse the held mutation lock")
	}
	releaseOnce.Do(func() { close(releaseObservation) })
	var uninstall result
	select {
	case uninstall = <-uninstallDone:
	case <-time.After(3 * time.Second):
		t.Fatal("uninstall did not finish after observation release")
	}
	if !errors.Is(start.err, ErrBusy) {
		t.Fatalf("concurrent start = %+v, %v", start.status, start.err)
	}
	if startCalls.Load() != 0 {
		t.Fatalf("concurrent start made %d launchctl calls", startCalls.Load())
	}
	if uninstall.err != nil || uninstall.status.State != ServiceAbsent {
		t.Fatalf("uninstall = %+v, %v", uninstall.status, uninstall.err)
	}
	if loaded.Load() {
		t.Fatal("start installed a live job behind uninstall's absent observation")
	}
	if _, err := os.Lstat(ServiceDirectoryPath(fixture.home)); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("service directory survived serialized uninstall")
	}
	if _, err := os.Lstat(fixture.plistPath()); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("plist survived serialized uninstall")
	}
}

func TestServiceUninstallRefusesForeignLoadedJobBeforeBootout(t *testing.T) {
	fixture := newManageFixture(t)
	installed := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(93))}
	fixture.install(t, installed.run)

	foreignResult := fixture.printRunning(93)
	foreignResult.stdout = []byte(strings.Replace(string(foreignResult.stdout), fixture.plistPath(), fixture.plistPath()+".foreign", 1))
	foreign := &recordedLaunchctl{results: []launchctlResult{foreignResult}}
	status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, foreign.run)
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceLaunchctl) {
		t.Fatalf("foreign loaded job uninstall = %+v, %v", status, err)
	}
	if len(foreign.calls) != 1 || foreign.calls[0][0] != "print" {
		t.Fatalf("foreign loaded job verbs = %q", foreign.calls)
	}
	if _, err := os.Lstat(ServiceDirectoryPath(fixture.home)); err != nil {
		t.Fatal("service directory was removed")
	}
	if _, err := os.Lstat(fixture.plistPath()); err != nil {
		t.Fatal("plist was removed")
	}
}

func TestServiceStageResidueRefusesInstallAndResolvesThroughUninstall(t *testing.T) {
	fixture := newManageFixture(t)
	serviceDir := ServiceDirectoryPath(fixture.home)
	current := filepath.Join(serviceDir, "bin", "current")
	if err := os.MkdirAll(current, 0o700); err != nil {
		t.Fatal(err)
	}
	stages := []string{
		filepath.Join(current, ".factoryd.stage"),
		filepath.Join(serviceDir, "."+serviceReceiptName+".stage"),
		filepath.Join(fixture.plistDir, "."+fixture.config.plistName()+".stage"),
	}
	for _, stage := range stages {
		if err := os.WriteFile(stage, []byte("crash residue"), 0o600); err != nil {
			t.Fatal(err)
		}
	}

	// Install refuses the collision instead of silently deleting it.
	absent := &recordedLaunchctl{results: fixture.printAbsent()}
	if _, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, absent.run); !errors.Is(err, ErrServiceResidue) {
		t.Fatalf("install over stage residue = %v", err)
	}

	// Uninstall resolves exactly its own stage names without launchd contact
	// (no receipt and no plist means no evidence, and none is needed for the
	// name-scoped residue).
	deny := func(context.Context, ...string) launchctlResult {
		t.Fatal("launchctl ran during stage-residue resolution")
		return launchctlResult{}
	}
	status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, deny)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("stage-residue uninstall = %+v, %v", status, err)
	}
	for _, stage := range stages {
		if _, err := os.Lstat(stage); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("stage survived uninstall: %s", stage)
		}
	}
	if _, err := os.Lstat(serviceDir); !errors.Is(err, os.ErrNotExist) {
		t.Fatal("service directory survived stage-residue uninstall")
	}

	// The cleaned home installs normally afterwards.
	full := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(92))}
	if status := fixture.install(t, full.run); status.State != ServiceRunning {
		t.Fatalf("install after resolution = %+v", status)
	}
}

func TestStagedWritersRefuseCollisionsInsteadOfDeleting(t *testing.T) {
	fixture := newManageFixture(t)
	destination := filepath.Join(fixture.root, "dest")
	if err := os.Mkdir(destination, 0o700); err != nil {
		t.Fatal(err)
	}
	for _, stage := range []string{".factoryd.stage", ".note.stage"} {
		if err := os.WriteFile(filepath.Join(destination, stage), []byte("someone else's bytes"), 0o600); err != nil {
			t.Fatal(err)
		}
	}
	if _, err := copyServiceBinary(filepath.Join(fixture.sourceDir, "factoryd"), destination, "factoryd"); !errors.Is(err, ErrServiceResidue) {
		t.Fatalf("binary stage collision = %v", err)
	}
	if err := writeExactFile(destination, "note", []byte("payload"), 0o600); !errors.Is(err, ErrServiceResidue) {
		t.Fatalf("file stage collision = %v", err)
	}
	for _, stage := range []string{".factoryd.stage", ".note.stage"} {
		body, err := os.ReadFile(filepath.Join(destination, stage))
		if err != nil || string(body) != "someone else's bytes" {
			t.Fatalf("stage %s mutated: %q, %v", stage, body, err)
		}
	}
}

func TestServiceRelayOriginSurvivesInstallStatusAndUninstall(t *testing.T) {
	const origin = "wss://relay.darkfactory.build"
	fixture := newManageFixture(t)
	deny := func(context.Context, ...string) launchctlResult {
		t.Fatal("launchctl ran for a request that should have been refused")
		return launchctlResult{}
	}

	// An origin the render cannot accept refuses before a single byte lands.
	invalid := fixture.config
	invalid.RelayOrigin = "https://relay.darkfactory.build"
	status, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, invalid, fixture.sourceDir, deny)
	if status.State != "" || !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("invalid relay origin install = %+v, %v", status, err)
	}
	for _, path := range []string{ServiceDirectoryPath(fixture.home), fixture.plistPath()} {
		if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("refused install created %s", filepath.Base(path))
		}
	}

	fixture.config.RelayOrigin = origin
	installed := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(77))}
	fixture.install(t, installed.run)

	expected, expectedDigest, err := ServicePlist(fixture.home, fixture.config.Label, origin)
	if err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(fixture.plistPath())
	if err != nil || !bytes.Equal(body, expected) {
		t.Fatalf("installed plist is not the relay rendering: %v", err)
	}
	receiptBody, err := os.ReadFile(filepath.Join(ServiceDirectoryPath(fixture.home), serviceReceiptName))
	if err != nil {
		t.Fatal(err)
	}
	receipt, err := parseServiceReceipt(receiptBody)
	if err != nil || receipt.RelayOrigin != origin || receipt.PlistDigest != hex.EncodeToString(expectedDigest[:]) {
		t.Fatalf("receipt does not bind the relayed plist: %+v, %v", receipt, err)
	}

	// Status re-renders from the receipt, so the relayed plist is recognized
	// as this installation's property rather than read as foreign bytes.
	bare := fixture.config
	bare.RelayOrigin = ""
	running := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(77)}}
	status, err = inspectServiceAtHome(context.Background(), fixture.home, fixture.userHome, bare, running.run)
	if err != nil || status.State != ServiceRunning || status.PID != 77 {
		t.Fatalf("relayed status = %+v, %v", status, err)
	}

	// Uninstall is invoked without the flag too: the receipt is what
	// reproduces the exact installed bytes, and removal proves it did.
	proven := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(77), {status: 0}, {status: launchctlNotFound}}}
	status, err = serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, bare, proven.run)
	if err != nil || status.State != ServiceAbsent {
		t.Fatalf("relayed uninstall = %+v, %v", status, err)
	}
	for _, path := range []string{ServiceDirectoryPath(fixture.home), fixture.plistPath()} {
		if _, err := os.Lstat(path); !errors.Is(err, os.ErrNotExist) {
			t.Fatalf("%s survived the relayed uninstall", filepath.Base(path))
		}
	}
}

func TestServiceInstallRefusesAChangedRelayOriginInsteadOfNoOpping(t *testing.T) {
	const originA = "wss://relay.darkfactory.build"
	const originB = "wss://relay.example.test"
	fixture := newManageFixture(t)

	// A repeated install that would render a different plist must refuse
	// rather than report the old installation as satisfying the request.
	refuses := func(t *testing.T, from, to string) {
		t.Helper()
		fixture.config.RelayOrigin = from
		first := &recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(88))}
		fixture.install(t, first.run)
		installed, err := os.ReadFile(fixture.plistPath())
		if err != nil {
			t.Fatal(err)
		}
		changed := fixture.config
		changed.RelayOrigin = to
		status, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, changed, fixture.sourceDir, (&recordedLaunchctl{results: []launchctlResult{fixture.printRunning(88)}}).run)
		if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceRelayOrigin) || !errors.Is(err, ErrServiceForeign) {
			t.Fatalf("install %q over %q = %+v, %v", to, from, status, err)
		}
		// The refusal must name what is installed, quoted, so an empty
		// installed origin is still distinguishable from a relayed one.
		if !strings.Contains(err.Error(), strconv.Quote(from)) || !strings.Contains(err.Error(), "service uninstall") {
			t.Fatalf("refusal text = %v", err)
		}
		if body, readErr := os.ReadFile(fixture.plistPath()); readErr != nil || !bytes.Equal(body, installed) {
			t.Fatalf("refused install rewrote the plist: %v", readErr)
		}
		removed := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(88), {status: 0}, {status: launchctlNotFound}}}
		if status, err := serviceUninstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, removed.run); err != nil || status.State != ServiceAbsent {
			t.Fatalf("uninstall after refusal = %+v, %v", status, err)
		}
	}
	refuses(t, "", originA)
	refuses(t, originA, originB)
	refuses(t, originA, "")

	// The same origin twice stays idempotent: no plist rewrite, no launchctl
	// bootstrap, just the recognized installation.
	fixture.config.RelayOrigin = originA
	fixture.install(t, (&recordedLaunchctl{results: append(fixture.printAbsent(), launchctlResult{status: 0}, fixture.printRunning(89))}).run)
	repeat := &recordedLaunchctl{results: []launchctlResult{fixture.printRunning(89)}}
	status, err := serviceInstallAt(context.Background(), fixture.home, fixture.userHome, fixture.config, fixture.sourceDir, repeat.run)
	if err != nil || status.State != ServiceRunning || status.PID != 89 {
		t.Fatalf("repeated identical install = %+v, %v", status, err)
	}
	if len(repeat.calls) != 1 || repeat.calls[0][0] != "print" {
		t.Fatalf("repeated identical install verbs = %q", repeat.calls)
	}
}
