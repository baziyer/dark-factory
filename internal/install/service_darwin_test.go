//go:build darwin

package install

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"runtime"
	"strconv"
	"strings"
	"syscall"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

type recordedLaunchctl struct {
	results []launchctlResult
	calls   [][]string
}

func (fake *recordedLaunchctl) run(_ context.Context, arguments ...string) launchctlResult {
	fake.calls = append(fake.calls, append([]string(nil), arguments...))
	if len(fake.results) == 0 {
		return launchctlResult{status: -1, err: errors.New("unexpected launchctl call")}
	}
	result := fake.results[0]
	fake.results = fake.results[1:]
	return result
}

func TestServiceStatusExactAbsenceIsReadOnly(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	before := snapshotServiceTrees(t, home, userHome)
	fake := &recordedLaunchctl{results: []launchctlResult{
		{status: launchctlNotFound},
		{status: 0, stdout: []byte(launchctlNotFoundText + "\n")},
	}}
	status, err := inspectService(context.Background(), home, userHome, fake.run)
	if err != nil || status != (ServiceStatus{State: ServiceAbsent}) {
		t.Fatalf("status = %+v, %v", status, err)
	}
	wantService := fmt.Sprintf("gui/%d/%s", os.Geteuid(), serviceLabel)
	wantCalls := [][]string{{"print", wantService}, {"error", "113"}}
	if !reflect.DeepEqual(fake.calls, wantCalls) {
		t.Fatalf("launchctl calls = %q, want %q", fake.calls, wantCalls)
	}
	after := snapshotServiceTrees(t, home, userHome)
	if !reflect.DeepEqual(before, after) {
		t.Fatalf("read-only status changed filesystem\nbefore: %#v\nafter:  %#v", before, after)
	}
}

func TestServiceStatusRefusesNonGoHomesBeforeLaunchctl(t *testing.T) {
	root := serviceTestRoot(t)
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name  string
		build func(string) error
	}{
		{name: "rust", build: func(path string) error {
			if err := os.Mkdir(path, 0o700); err != nil {
				return err
			}
			return os.WriteFile(filepath.Join(path, "factory.db"), []byte("rust"), 0o600)
		}},
		{name: "unknown", build: func(path string) error {
			if err := os.Mkdir(path, 0o700); err != nil {
				return err
			}
			return os.WriteFile(filepath.Join(path, "format"), []byte("unknown\n"), 0o600)
		}},
		{name: "symlink", build: func(path string) error {
			target := path + "-target"
			if err := os.Mkdir(target, 0o700); err != nil {
				return err
			}
			return os.Symlink(target, path)
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			home := filepath.Join(root, "home-"+test.name)
			if err := test.build(home); err != nil {
				t.Fatal(err)
			}
			before := snapshotServiceTrees(t, root)
			calls := 0
			_, err := inspectService(context.Background(), home, userHome, func(context.Context, ...string) launchctlResult {
				calls++
				return launchctlResult{}
			})
			if err == nil || calls != 0 {
				t.Fatalf("status error = %v, launchctl calls = %d", err, calls)
			}
			if after := snapshotServiceTrees(t, root); !reflect.DeepEqual(before, after) {
				t.Fatal("refused home was modified")
			}
		})
	}
}

func TestLaunchctlAbsenceClassificationIsExact(t *testing.T) {
	service := fmt.Sprintf("gui/%d/%s", os.Geteuid(), serviceLabel)
	plist := "/private/tmp/user/Library/LaunchAgents/" + servicePlistName
	program := "/private/tmp/factory/bin/current/factoryd"
	for _, test := range []struct {
		name    string
		results []launchctlResult
		want    bool
	}{
		{name: "documented", results: []launchctlResult{{status: 113}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}, want: true},
		{name: "contradictory print stderr", results: []launchctlResult{{status: 113, stderr: []byte("Operation not permitted\n")}}, want: false},
		{name: "permission", results: []launchctlResult{{status: 1, stderr: []byte("Operation not permitted\n")}}},
		{name: "wrong status same text", results: []launchctlResult{{status: 3}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}},
		{name: "wrong text", results: []launchctlResult{{status: 113}, {status: 0, stdout: []byte("113: other\n")}}},
		{name: "classification stderr", results: []launchctlResult{{status: 113}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n"), stderr: []byte("warning")}}},
		{name: "classification failed", results: []launchctlResult{{status: 113}, {status: 1}}},
		{name: "spawn", results: []launchctlResult{{status: -1, err: errors.New("spawn")}}},
		{name: "oversized", results: []launchctlResult{{status: 0, overflow: true}}},
	} {
		t.Run(test.name, func(t *testing.T) {
			fake := &recordedLaunchctl{results: append([]launchctlResult(nil), test.results...)}
			observation, err := observeLaunchctl(context.Background(), fake.run, service, plist, program)
			if test.want {
				if err != nil || observation.present {
					t.Fatalf("absence = %+v, %v", observation, err)
				}
			} else if !errors.Is(err, ErrServiceLaunchctl) {
				t.Fatalf("classification error = %v", err)
			}
		})
	}
}

func TestLaunchctlCommandCancellationReapsAndBoundsTheExactChild(t *testing.T) {
	root := serviceTestRoot(t)
	pidPath := filepath.Join(root, "pid")
	ctx, cancel := context.WithTimeout(context.Background(), 50*time.Millisecond)
	defer cancel()
	result := runLaunchctlBinary(ctx, os.Args[0], "-test.run=^TestLaunchctlCommandHelper$", "--", "hang", pidPath)
	if result.status != -1 || !errors.Is(result.err, context.DeadlineExceeded) {
		t.Fatalf("hung launchctl = status %d, err %v, stderr %q", result.status, result.err, result.stderr)
	}
	body, err := os.ReadFile(pidPath)
	if err != nil {
		t.Fatal(err)
	}
	pid, err := strconv.Atoi(string(body))
	if err != nil || pid <= 1 {
		t.Fatalf("helper pid = %q, %v", body, err)
	}
	if err := unix.Kill(pid, 0); !errors.Is(err, unix.ESRCH) {
		t.Fatalf("launchctl child %d was not reaped: %v", pid, err)
	}

	result = runLaunchctlBinary(context.Background(), os.Args[0], "-test.run=^TestLaunchctlCommandHelper$", "--", "overflow")
	if result.err != nil || result.status != 0 || !result.overflow || len(result.stdout) != launchctlOutputLimit {
		t.Fatalf("oversized launchctl = status %d err %v overflow %t size %d", result.status, result.err, result.overflow, len(result.stdout))
	}
	result = runLaunchctlBinary(context.Background(), filepath.Join(root, "missing"), "print", "service")
	if result.status != -1 || result.err == nil {
		t.Fatalf("missing launchctl = status %d err %v", result.status, result.err)
	}
}

func TestLaunchctlCommandHelper(t *testing.T) {
	separator := -1
	for index, argument := range os.Args {
		if argument == "--" {
			separator = index
			break
		}
	}
	if separator < 0 || separator+1 >= len(os.Args) {
		return
	}
	switch os.Args[separator+1] {
	case "hang":
		if separator+2 >= len(os.Args) {
			os.Exit(2)
		}
		if err := os.WriteFile(os.Args[separator+2], []byte(strconv.Itoa(os.Getpid())), 0o600); err != nil {
			os.Exit(3)
		}
		for {
			time.Sleep(time.Hour)
		}
	case "overflow":
		_, _ = os.Stdout.Write(bytes.Repeat([]byte{'x'}, launchctlOutputLimit+1))
		os.Exit(0)
	default:
		os.Exit(4)
	}
}

func TestLaunchctlPrintRequiresExactOwnedFields(t *testing.T) {
	service := fmt.Sprintf("gui/%d/%s", os.Geteuid(), serviceLabel)
	plist := "/private/tmp/user/Library/LaunchAgents/" + servicePlistName
	program := "/private/tmp/factory/bin/current/factoryd"
	valid := func(state, pid string) []byte {
		pidLine := ""
		if pid != "" {
			pidLine = "\n\tpid = " + pid
		}
		return []byte(service + " = {\n\tpath = " + plist + "\n\tstate = " + state + "\n\tprogram = " + program + pidLine + "\n}\n")
	}
	if pid, err := parseLaunchctlPrint(valid("running", "731"), service, plist, program); err != nil || pid != 731 {
		t.Fatalf("running parse = %d, %v", pid, err)
	}
	if pid, err := parseLaunchctlPrint(valid("not running", ""), service, plist, program); err != nil || pid != 0 {
		t.Fatalf("stopped parse = %d, %v", pid, err)
	}
	documented := []byte(service + " = {\n" +
		"\tactive count = 1\n" +
		"\tpath = " + plist + "\n" +
		"\ttype = LaunchAgent\n" +
		"\tstate = running\n" +
		"\tprogram = " + program + "\n" +
		"\targuments = (\n" +
		"\t\t" + program + "\n" +
		"\t\t--home\n" +
		"\t\t/private/tmp/factory\n" +
		"\t)\n" +
		"\tworking directory = /private/tmp/factory\n" +
		"\tstdout path = /dev/null\n" +
		"\tstderr path = /dev/null\n" +
		"\tproperties = {\n" +
		"\t\tAbandonProcessGroup = true\n" +
		"\t}\n" +
		"\tpid = 731\n" +
		"}\n")
	if pid, err := parseLaunchctlPrint(documented, service, plist, program); err != nil || pid != 731 {
		t.Fatalf("documented print = %d, %v", pid, err)
	}
	nestedForeign := bytes.Replace(documented, []byte("AbandonProcessGroup = true"), []byte("path = /private/foreign"), 1)
	if pid, err := parseLaunchctlPrint(nestedForeign, service, plist, program); err != nil || pid != 731 {
		t.Fatalf("nested foreign field = %d, %v", pid, err)
	}
	for name, mutation := range map[string][]byte{
		"documented duplicate path":        bytes.Replace(documented, []byte("\tpid = 731\n"), []byte("\tpath = "+plist+"\n\tpid = 731\n"), 1),
		"documented unclosed arguments":    bytes.Replace(documented, []byte("\t)\n"), []byte("\t\t--unclosed\n"), 1),
		"documented mismatched properties": bytes.Replace(documented, []byte("\t}\n\tpid = 731"), []byte("\t)\n\tpid = 731"), 1),
	} {
		t.Run(name, func(t *testing.T) {
			if _, err := parseLaunchctlPrint(mutation, service, plist, program); !errors.Is(err, ErrServiceLaunchctl) {
				t.Fatalf("documented mutation accepted: %v", err)
			}
		})
	}
	fake := &recordedLaunchctl{results: []launchctlResult{{status: 0, stdout: valid("running", "731"), stderr: []byte("warning")}}}
	if _, err := observeLaunchctl(context.Background(), fake.run, service, plist, program); !errors.Is(err, ErrServiceLaunchctl) {
		t.Fatalf("successful print stderr accepted: %v", err)
	}
	mutations := map[string][]byte{
		"foreign label":             bytes.Replace(valid("running", "731"), []byte(service), []byte("gui/501/com.foreign"), 1),
		"wildcard label":            bytes.Replace(valid("running", "731"), []byte(service), []byte("gui/501/*"), 1),
		"foreign plist":             bytes.Replace(valid("running", "731"), []byte(plist), []byte("/private/foreign.plist"), 1),
		"whitespace foreign plist":  bytes.Replace(valid("running", "731"), []byte("\tpath = "+plist), []byte("\tpath  = /private/foreign.plist"), 1),
		"whitespace duplicate path": bytes.Replace(valid("running", "731"), []byte("\tstate = running"), []byte("\tpath  = "+plist+"\n\tstate = running"), 1),
		"foreign program":           bytes.Replace(valid("running", "731"), []byte(program), []byte("/private/foreign"), 1),
		"missing pid":               valid("running", ""),
		"pid one":                   valid("running", "1"),
		"unknown state":             valid("waiting", ""),
		"stopped pid":               valid("not running", "731"),
		"duplicate pid":             bytes.Replace(valid("running", "731"), []byte("\n}"), []byte("\n\tpid = 732\n}"), 1),
		"malformed field":           bytes.Replace(valid("running", "731"), []byte("\n}"), []byte("\n\tunknown value\n}"), 1),
		"nul":                       append(valid("running", "731"), 0),
	}
	for name, mutation := range mutations {
		t.Run(name, func(t *testing.T) {
			if _, err := parseLaunchctlPrint(mutation, service, plist, program); !errors.Is(err, ErrServiceLaunchctl) {
				t.Fatalf("mutation accepted: %v", err)
			}
		})
	}
}

func TestServicePlistIsOneFiniteAllowlist(t *testing.T) {
	home := "/private/tmp/factory & <operator>"
	body, digest, err := ServicePlist(home, DefaultServiceLabel, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if sha256.Sum256(body) != digest || bytes.Count(body, []byte("<key>AbandonProcessGroup</key>")) != 1 || bytes.Count(body, []byte("<true/>")) != 2 {
		t.Fatalf("plist identity or required keys invalid: %s", body)
	}
	for _, forbidden := range []string{"KeepAlive", "EnvironmentVariables", "Sockets", "NetworkState", "StandardOutPath", "--development-browser-address", "*"} {
		if bytes.Contains(body, []byte(forbidden)) {
			t.Fatalf("plist contains forbidden %q", forbidden)
		}
	}
	var escapedStderr bytes.Buffer
	escapeXML(&escapedStderr, serviceStderrPath(home))
	for _, expected := range []string{"factory &amp; &lt;operator&gt;", serviceLabel, "--home", "<key>StandardErrorPath</key>\n    <string>" + escapedStderr.String() + "</string>", "<integer>63</integer>"} {
		if !bytes.Contains(body, []byte(expected)) {
			t.Fatalf("plist omitted %q", expected)
		}
	}
	if _, _, err := ServicePlist("relative", DefaultServiceLabel, "", ""); !errors.Is(err, ErrServicePlist) {
		t.Fatalf("relative home = %v", err)
	}
	for _, home := range []string{"/private/tmp/invalid-\x00", "/private/tmp/invalid-\x01", "/private/tmp/invalid-\xff"} {
		if _, _, err := ServicePlist(home, DefaultServiceLabel, "", ""); !errors.Is(err, ErrServicePlist) {
			t.Fatalf("invalid plist path %q accepted: %v", home, err)
		}
	}
}

func TestServicePlistRendersOptionalArgumentsExactly(t *testing.T) {
	const home = "/private/tmp/factory"
	const origin = "wss://relay.darkfactory.build"
	const address = "127.0.0.1:0"
	arguments := func(extra string) string {
		return "    <array>\n        <string>" + serviceProgramPath(home) + "</string>\n" +
			"        <string>--home</string>\n        <string>" + home + "</string>" + extra + "\n    </array>\n"
	}
	plain, _, err := ServicePlist(home, DefaultServiceLabel, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Contains(plain, []byte(arguments(""))) || bytes.Contains(plain, []byte("relay")) {
		t.Fatalf("plist without the flag = %s", plain)
	}
	relayed, digest, err := ServicePlist(home, DefaultServiceLabel, origin, "")
	if err != nil {
		t.Fatal(err)
	}
	extra := "\n        <string>--relay-origin</string>\n        <string>" + origin + "</string>"
	if !bytes.Contains(relayed, []byte(arguments(extra))) {
		t.Fatalf("plist with the flag = %s", relayed)
	}
	if sha256.Sum256(relayed) != digest || len(relayed) != len(plain)+len(extra) {
		t.Fatal("the relay argument is not the only rendered difference")
	}
	developed, _, err := ServicePlist(home, DefaultServiceLabel, "", address)
	if err != nil {
		t.Fatal(err)
	}
	developmentExtra := "\n        <string>--development-browser-address</string>\n        <string>" + address + "</string>"
	if !bytes.Contains(developed, []byte(arguments(developmentExtra))) || len(developed) != len(plain)+len(developmentExtra) {
		t.Fatalf("plist with development browser address = %s", developed)
	}
	// The bound is the exact origin grammar: anything that would render a job
	// factoryd refuses at boot, or smuggle a second flag into
	// ProgramArguments, is rejected before a plist exists.
	for _, bad := range []string{
		"--home", "https://relay.darkfactory.build", "relay.darkfactory.build", "wss://",
		"wss://relay.darkfactory.build/host", "wss://relay.darkfactory.build?x=1",
		"wss://relay.darkfactory.build#f", "wss://user@relay.darkfactory.build",
		"wss://relay .build", "wss://relay\nx", "wss://relay\x00",
		"wss://" + strings.Repeat("a", MaxRelayOriginBytes),
	} {
		if _, _, err := ServicePlist(home, DefaultServiceLabel, bad, ""); !errors.Is(err, ErrServicePlist) {
			t.Fatalf("invalid relay origin %q accepted: %v", bad, err)
		}
	}
	for _, bad := range []string{"localhost:43123", "127.0.0.1:65536", "127.0.0.1:-1", "127.0.0.1:01"} {
		if _, _, err := ServicePlist(home, DefaultServiceLabel, "", bad); !errors.Is(err, ErrServicePlist) {
			t.Fatalf("invalid development browser address %q accepted: %v", bad, err)
		}
	}
}

func TestServiceStatusRejectsDetachedLaunchAgentsDuringRead(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	launchAgents := filepath.Join(userHome, "Library", "LaunchAgents")
	if err := os.MkdirAll(launchAgents, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	plistBody, _, err := ServicePlist(home, DefaultServiceLabel, "", "")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(launchAgents, servicePlistName), plistBody, 0o600); err != nil {
		t.Fatal(err)
	}
	detached := launchAgents + ".detached"
	fake := &recordedLaunchctl{}
	fake.results = []launchctlResult{{status: launchctlNotFound}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}
	mutated := false
	status, err := inspectService(context.Background(), home, userHome, func(ctx context.Context, args ...string) launchctlResult {
		if !mutated {
			mutated = true
			if err := os.Rename(launchAgents, detached); err != nil {
				t.Fatalf("detach LaunchAgents: %v", err)
			}
			if err := os.Mkdir(launchAgents, 0o700); err != nil {
				t.Fatalf("replace LaunchAgents: %v", err)
			}
		}
		return fake.run(ctx, args...)
	})
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("detached parent accepted: %+v, %v", status, err)
	}
}

func TestServiceStatusLaunchctlFailureIsReadOnly(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	before := snapshotServiceTrees(t, home, userHome)
	status, err := inspectService(context.Background(), home, userHome, func(context.Context, ...string) launchctlResult {
		return launchctlResult{status: 1, stderr: []byte("permission denied")}
	})
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceLaunchctl) {
		t.Fatalf("launchctl failure = %+v, %v", status, err)
	}
	if after := snapshotServiceTrees(t, home, userHome); !reflect.DeepEqual(before, after) {
		t.Fatal("launchctl failure changed filesystem")
	}
}

func TestServiceTreeCensusDetectsSameContentReplacement(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	before := snapshotServiceTrees(t, home)
	formatPath := filepath.Join(home, formatName)
	formatInfo, err := os.Stat(formatPath)
	if err != nil {
		t.Fatal(err)
	}
	homeInfo, err := os.Stat(home)
	if err != nil {
		t.Fatal(err)
	}
	body, err := os.ReadFile(formatPath)
	if err != nil {
		t.Fatal(err)
	}
	moved := formatPath + ".replacement"
	if err := os.Rename(formatPath, moved); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(formatPath, body, formatInfo.Mode().Perm()); err != nil {
		t.Fatal(err)
	}
	if err := os.Remove(moved); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(formatPath, formatInfo.ModTime(), formatInfo.ModTime()); err != nil {
		t.Fatal(err)
	}
	if err := os.Chtimes(home, homeInfo.ModTime(), homeInfo.ModTime()); err != nil {
		t.Fatal(err)
	}
	after := snapshotServiceTrees(t, home)
	if reflect.DeepEqual(before, after) {
		t.Fatal("same-content replacement was invisible to census")
	}
	if before[formatPath].Ino == after[formatPath].Ino && before[formatPath].Ctime == after[formatPath].Ctime {
		t.Fatal("replacement identity was not retained")
	}
}

func TestServiceStatusRejectsStageCreatedAfterHomeCensus(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	parent := filepath.Dir(home)
	stage := filepath.Join(parent, "."+filepath.Base(home)+stageSuffix)
	results := &recordedLaunchctl{results: []launchctlResult{{status: launchctlNotFound}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}}
	status, err := inspectService(context.Background(), home, userHome, func(ctx context.Context, args ...string) launchctlResult {
		if len(results.calls) == 0 {
			if err := os.Mkdir(stage, 0o700); err != nil {
				t.Fatalf("create stage: %v", err)
			}
		}
		return results.run(ctx, args...)
	})
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceAmbiguous) || !errors.Is(err, ErrInvalidHome) {
		t.Fatalf("stage created after census accepted: %+v, %v", status, err)
	}
}

func TestServiceStatusRejectsHomeRemovalDuringLaunchctl(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	moved := home + ".removed"
	results := &recordedLaunchctl{results: []launchctlResult{{status: launchctlNotFound}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}}
	status, err := inspectService(context.Background(), home, userHome, func(ctx context.Context, args ...string) launchctlResult {
		if len(results.calls) == 0 {
			if err := os.Rename(home, moved); err != nil {
				t.Fatalf("remove home binding: %v", err)
			}
		}
		return results.run(ctx, args...)
	})
	if status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceAmbiguous) || !errors.Is(err, ErrInvalidHome) {
		t.Fatalf("home removal during launchctl accepted: %+v, %v", status, err)
	}
}

func TestServiceStatusRefusesPlistMutationsAndPresentJobs(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	launchAgents := filepath.Join(userHome, "Library", "LaunchAgents")
	if err := os.MkdirAll(launchAgents, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	plistBody, _, err := ServicePlist(home, DefaultServiceLabel, "", "")
	if err != nil {
		t.Fatal(err)
	}
	plistPath := filepath.Join(launchAgents, servicePlistName)
	if err := os.WriteFile(plistPath, plistBody, 0o600); err != nil {
		t.Fatal(err)
	}
	absent := &recordedLaunchctl{results: []launchctlResult{{status: 113}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}}
	if status, err := inspectService(context.Background(), home, userHome, absent.run); status.State != ServiceAmbiguous || !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("plist without receipt = %+v, %v", status, err)
	}

	if err := os.WriteFile(plistPath, append(plistBody, 'x'), 0o600); err != nil {
		t.Fatal(err)
	}
	calls := 0
	if status, err := inspectService(context.Background(), home, userHome, func(context.Context, ...string) launchctlResult {
		calls++
		return launchctlResult{}
	}); status.State != ServiceAmbiguous || !errors.Is(err, ErrServicePlist) || calls != 0 {
		t.Fatalf("tampered plist = %+v, %v, calls=%d", status, err, calls)
	}

	if err := os.Remove(plistPath); err != nil {
		t.Fatal(err)
	}
	service := fmt.Sprintf("gui/%d/%s", os.Geteuid(), serviceLabel)
	program := serviceProgramPath(home)
	presentOutput := []byte(service + " = {\n\tpath = " + plistPath + "\n\tstate = running\n\tprogram = " + program + "\n\tpid = 731\n}\n")
	present := &recordedLaunchctl{results: []launchctlResult{{status: 0, stdout: presentOutput}}}
	if status, err := inspectService(context.Background(), home, userHome, present.run); status != (ServiceStatus{State: ServiceAmbiguous, PID: 731}) || !errors.Is(err, ErrServiceAmbiguous) {
		t.Fatalf("present job without receipt = %+v, %v", status, err)
	}
}

func TestServiceStatusRejectsPlistMetadataWithoutLaunchctl(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	body, _, err := ServicePlist(home, DefaultServiceLabel, "", "")
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name   string
		mutate func(*testing.T, string, string)
	}{
		{name: "wrong mode", mutate: func(t *testing.T, path, _ string) {
			if err := os.Chmod(path, 0o644); err != nil {
				t.Fatal(err)
			}
		}},
		{name: "hardlink", mutate: func(t *testing.T, path, directory string) {
			if err := os.Link(path, filepath.Join(directory, "second-link")); err != nil {
				t.Fatal(err)
			}
		}},
		{name: "symlink", mutate: func(t *testing.T, path, directory string) {
			target := filepath.Join(directory, "target")
			if err := os.Rename(path, target); err != nil {
				t.Fatal(err)
			}
			if err := os.Symlink(target, path); err != nil {
				t.Fatal(err)
			}
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			userHome := filepath.Join(root, "user-"+strings.ReplaceAll(test.name, " ", "-"))
			directory := filepath.Join(userHome, "Library", "LaunchAgents")
			if err := os.MkdirAll(directory, 0o700); err != nil {
				t.Fatal(err)
			}
			path := filepath.Join(directory, servicePlistName)
			if err := os.WriteFile(path, body, 0o600); err != nil {
				t.Fatal(err)
			}
			test.mutate(t, path, directory)
			calls := 0
			status, err := inspectService(context.Background(), home, userHome, func(context.Context, ...string) launchctlResult {
				calls++
				return launchctlResult{}
			})
			if status.State != ServiceAmbiguous || !errors.Is(err, ErrServicePlist) || calls != 0 {
				t.Fatalf("metadata mutation = %+v, %v, calls=%d", status, err, calls)
			}
		})
	}
}

func TestServiceStatusCancellationAndRepeatedCallsLeakNothing(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	userHome := filepath.Join(root, "user")
	if err := os.Mkdir(userHome, 0o700); err != nil {
		t.Fatal(err)
	}
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	// Settled, not sampled. A single sample is inflated whenever something
	// started earlier is still winding down -- `Init` above leaves a
	// `database/sql` connection opener running -- and an inflated baseline is
	// the condition under which `<=` passes a real leak. How often that bites
	// is a property of the scheduler, not of this code: never observed at the
	// default GOMAXPROCS, every run at GOMAXPROCS=1.
	//
	// Taking the running minimum reduces it rather than removing it: a
	// goroutine that has not exited inside the window still raises the floor.
	// The window is generous against what it is budgeted for -- the winding-down
	// goroutines are runnable, not blocked on I/O, so the floor was reached
	// within 3us of entering the loop in every run measured, against 100ms
	// allowed.
	//
	// Descriptors are not sampled here. Nothing in this process closes one
	// asynchronously, so a minimum over time cannot differ from the first
	// reading, and it never did across roughly 180,000 polls.
	baselineFD := serviceFDCount(t)
	baselineGoroutines := runtime.NumGoroutine()
	for settle := time.Now().Add(100 * time.Millisecond); time.Now().Before(settle); {
		runtime.Gosched()
		if count := runtime.NumGoroutine(); count < baselineGoroutines {
			baselineGoroutines = count
		}
	}
	ctx, cancel := context.WithCancel(context.Background())
	_, err := inspectService(ctx, home, userHome, func(ctx context.Context, _ ...string) launchctlResult {
		cancel()
		return launchctlResult{status: -1, err: ctx.Err()}
	})
	if !errors.Is(err, context.Canceled) || !errors.Is(err, ErrServiceLaunchctl) {
		t.Fatalf("cancel error = %v", err)
	}
	for index := 0; index < 20; index++ {
		fake := &recordedLaunchctl{results: []launchctlResult{{status: 113}, {status: 0, stdout: []byte(launchctlNotFoundText + "\n")}}}
		if _, err := inspectService(context.Background(), home, userHome, fake.run); err != nil {
			t.Fatal(err)
		}
	}
	// `<=`, not equality: a goroutine that merely *exits* during the run is not
	// a leak, and demanding equality reported one -- observed on the base
	// commit as "goroutines = 2, want 3", a count that had gone down. Against a
	// settled floor, `<=` still fails on anything genuinely retained.
	//
	// Only the goroutine count is waited on. Nothing here closes a descriptor
	// asynchronously, so waiting for the FD count to fall could only ever burn
	// the whole deadline before failing.
	deadline := time.Now().Add(time.Second)
	for runtime.NumGoroutine() > baselineGoroutines && time.Now().Before(deadline) {
		runtime.Gosched()
	}
	if got := serviceFDCount(t); got > baselineFD {
		t.Fatalf("FD count = %d, want at most %d", got, baselineFD)
	}
	if got := runtime.NumGoroutine(); got > baselineGoroutines {
		t.Fatalf("goroutines = %d, want at most %d", got, baselineGoroutines)
	}
}

type serviceTreeEntry struct {
	Mode    os.FileMode
	Size    int64
	ModTime int64
	Digest  [sha256.Size]byte
	Link    string
	Dev     uint64
	Ino     uint64
	UID     uint32
	GID     uint32
	Nlink   uint64
	Ctime   unix.Timespec
}

func snapshotServiceTrees(t *testing.T, roots ...string) map[string]serviceTreeEntry {
	t.Helper()
	result := make(map[string]serviceTreeEntry)
	for _, root := range roots {
		err := filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
			if err != nil {
				return err
			}
			entry := serviceTreeEntry{Mode: info.Mode(), Size: info.Size(), ModTime: info.ModTime().UnixNano()}
			stat, ok := info.Sys().(*syscall.Stat_t)
			if !ok || stat == nil {
				return errors.New("service census missing unix identity")
			}
			entry.Dev = uint64(stat.Dev)
			entry.Ino = uint64(stat.Ino)
			entry.UID = uint32(stat.Uid)
			entry.GID = uint32(stat.Gid)
			entry.Nlink = uint64(stat.Nlink)
			entry.Ctime = unix.Timespec{Sec: stat.Ctimespec.Sec, Nsec: stat.Ctimespec.Nsec}
			if info.Mode().IsRegular() {
				body, readErr := os.ReadFile(path)
				if readErr != nil {
					return readErr
				}
				entry.Digest = sha256.Sum256(body)
			} else if info.Mode()&os.ModeSymlink != 0 {
				link, readErr := os.Readlink(path)
				if readErr != nil {
					return readErr
				}
				entry.Link = link
			}
			result[path] = entry
			return nil
		})
		if err != nil {
			t.Fatal(err)
		}
	}
	return result
}

func serviceTestRoot(t *testing.T) string {
	t.Helper()
	root, err := os.MkdirTemp("/private/tmp", "dark-factory-service-")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = os.RemoveAll(root) })
	return root
}

func serviceFDCount(t *testing.T) int {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func TestServiceDirectoryRecheckToleratesAncestorChurnAndStaysFailClosed(t *testing.T) {
	root := serviceTestRoot(t)
	middle := filepath.Join(root, "middle")
	leaf := filepath.Join(middle, "leaf")
	if err := os.MkdirAll(leaf, 0o700); err != nil {
		t.Fatal(err)
	}
	directory, err := openServiceDirectory(leaf)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = directory.close() }()
	// Unrelated sibling activity in ancestor directories (every parallel
	// test's mktemp; anything written into the user's home in production)
	// legitimately changes their timestamps, sizes and link counts. The
	// chain's identity — device, inode, mode, owner — is what recheck
	// guards, so churn must not read as an identity change.
	if err := os.WriteFile(filepath.Join(root, "churn"), []byte("x"), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(filepath.Join(middle, "sibling"), 0o700); err != nil {
		t.Fatal(err)
	}
	if err := directory.recheck(); err != nil {
		t.Fatalf("ancestor churn read as identity change: %v", err)
	}
	// Fail-closed retention: a mode change on a chain member is an identity
	// change and must still refuse.
	if err := os.Chmod(leaf, 0o750); err != nil {
		t.Fatal(err)
	}
	if err := directory.recheck(); err == nil {
		t.Fatal("mode change passed recheck")
	}
	if err := os.Chmod(leaf, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := directory.recheck(); err != nil {
		t.Fatalf("restored chain refused: %v", err)
	}
	// A swapped directory of the same name is a different inode: the
	// parent-binding probe must refuse it.
	if err := os.Remove(filepath.Join(middle, "sibling")); err != nil {
		t.Fatal(err)
	}
	if err := os.RemoveAll(leaf); err != nil {
		t.Fatal(err)
	}
	if err := os.Mkdir(leaf, 0o700); err != nil {
		t.Fatal(err)
	}
	if err := directory.recheck(); err == nil {
		t.Fatal("swapped leaf passed recheck")
	}
}

func TestServiceHomeSnapshotsTolerateLiveDirectoryChurnButRejectReplacement(t *testing.T) {
	root := serviceTestRoot(t)
	home := filepath.Join(root, "factory")
	if _, err := Init(context.Background(), home); err != nil {
		t.Fatal(err)
	}
	capability, err := openServiceHomeCapability(context.Background(), home)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = capability.close() })
	baseline := capability.image

	for _, name := range []string{runtimesName, changesName} {
		entry := filepath.Join(home, name, "live-entry")
		if err := os.WriteFile(entry, []byte("churn"), 0o600); err != nil {
			t.Fatal(err)
		}
		image, err := snapshotServiceHome(context.Background(), capability.home)
		if err != nil {
			t.Fatalf("snapshot after %s child churn: %v", name, err)
		}
		if err := sameServiceHomeImage(baseline, image); err != nil {
			t.Fatalf("%s child churn changed service image: %v", name, err)
		}
		if err := os.Remove(entry); err != nil {
			t.Fatal(err)
		}
	}

	tokenPath := filepath.Join(home, tokenName)
	token, err := os.ReadFile(tokenPath)
	if err != nil {
		t.Fatal(err)
	}
	mutated := append([]byte(nil), token...)
	mutated[0] ^= 0xff
	if err := os.WriteFile(tokenPath, mutated, 0o600); err != nil {
		t.Fatal(err)
	}
	image, err := snapshotServiceHome(context.Background(), capability.home)
	if err != nil {
		t.Fatal(err)
	}
	if err := sameServiceHomeImage(baseline, image); err == nil {
		t.Fatal("durable token replacement passed service image comparison")
	}
	if err := os.WriteFile(tokenPath, token, 0o600); err != nil {
		t.Fatal(err)
	}
	baseline, err = snapshotServiceHome(context.Background(), capability.home)
	if err != nil {
		t.Fatal(err)
	}

	original := filepath.Join(home, runtimesName)
	moved := filepath.Join(root, "runtimes.original")
	if err := os.Rename(original, moved); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if _, statErr := os.Stat(moved); statErr == nil {
			_ = os.RemoveAll(original)
			_ = os.Rename(moved, original)
		}
	})
	if err := os.Mkdir(original, 0o700); err != nil {
		t.Fatal(err)
	}
	replacement, err := snapshotServiceHome(context.Background(), capability.home)
	if err != nil {
		t.Fatal(err)
	}
	if err := sameServiceHomeImage(baseline, replacement); err == nil {
		t.Fatal("same-name directory replacement passed service image comparison")
	}
}
