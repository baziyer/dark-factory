//go:build darwin

package runner

import (
	"debug/macho"
	"encoding/binary"
	"io"
	"os"
	"path/filepath"
	"slices"
	"strings"
	"testing"
	"time"
)

var providerEnvironmentNames = []string{
	"DARK_FACTORY_SOCKET",
	"DARK_FACTORY_ATTEMPT_TOKEN_FILE",
	"DARK_FACTORY_FACTORYCTL",
	"CODEX_HOME",
	"CLAUDE_CONFIG_DIR",
	"HOME",
	"TMPDIR",
	"PATH",
	"LANG",
	"LC_ALL",
	"TERM",
	"GIT_CEILING_DIRECTORIES",
	"GIT_DISCOVERY_ACROSS_FILESYSTEM",
	"GIT_CONFIG_NOSYSTEM",
	"GIT_CONFIG_GLOBAL",
	"GIT_TERMINAL_PROMPT",
	"GIT_ASKPASS",
	"GIT_SSH_COMMAND",
	"GH_CONFIG_DIR",
}

func TestPrepareExecSpecAcceptsOnlyClosedProviderEnvironmentNames(t *testing.T) {
	for _, name := range append(append([]string{}, providerEnvironmentNames...), "LC_CTYPE", "SHELL", "USER", "LOGNAME") {
		if !allowedEnv(name) {
			t.Errorf("required environment name %q rejected", name)
		}
	}

	forbidden := []string{
		"HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY",
		"NO_COLOR",
		"ANTHROPIC_API_KEY", "OPENAI_API_KEY", "CODEX_API_KEY",
		"SSH_AUTH_SOCK", "GIT_SSH", "GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0",
		"GIT_DIR", "GIT_WORK_TREE", "GH_TOKEN", "GITHUB_TOKEN",
		"DYLD_INSERT_LIBRARIES", "DYLD_LIBRARY_PATH", "LD_PRELOAD", "LD_LIBRARY_PATH",
		"DARK_FACTORY_HOME", "DARK_FACTORY_RUN_ID", "DARK_FACTORY_TASK_ID", "DARK_FACTORY_FACTORYCTL_PATH", "DARK_FACTORY_FACTORYCTL_HELPER", "ARBITRARY",
	}
	missing := filepath.Join(t.TempDir(), "must-not-be-observed")
	for _, name := range forbidden {
		t.Run(name, func(t *testing.T) {
			if allowedEnv(name) {
				t.Fatalf("forbidden environment name %q allowed", name)
			}
			_, err := PrepareExecSpec(ExecSpec{Target: missing, Cwd: missing, Env: []string{name + "=private"}})
			if err == nil || !strings.Contains(err.Error(), "environment key") {
				t.Fatalf("forbidden environment was not rejected before path commitment: %v", err)
			}
		})
	}
}

func TestPrepareExecSpecRejectsDuplicateAndMalformedEnvironmentBeforePaths(t *testing.T) {
	missing := filepath.Join(t.TempDir(), "must-not-be-observed")
	tests := []struct {
		name string
		env  []string
		want string
	}{
		{name: "same duplicate", env: []string{"PATH=/usr/bin", "PATH=/usr/bin"}, want: "duplicate environment"},
		{name: "different-value duplicate", env: []string{"PATH=/usr/bin", "PATH=/bin"}, want: "duplicate environment"},
		{name: "factoryctl duplicate", env: []string{"DARK_FACTORY_FACTORYCTL=/private/a", "DARK_FACTORY_FACTORYCTL=/private/b"}, want: "duplicate environment"},
		{name: "factoryctl empty", env: []string{"DARK_FACTORY_FACTORYCTL="}, want: "invalid environment"},
		{name: "case-distinct remains distinct and unallowed", env: []string{"PATH=/usr/bin", "path=/bin"}, want: "environment key"},
		{name: "missing separator", env: []string{"PATH"}, want: "invalid environment"},
		{name: "empty name", env: []string{"=value"}, want: "invalid environment"},
		{name: "empty value", env: []string{"PATH="}, want: "invalid environment"},
		{name: "name NUL", env: []string{"PA\x00TH=value"}, want: "invalid environment"},
		{name: "value NUL", env: []string{"PATH=va\x00lue"}, want: "invalid environment"},
		{name: "invalid UTF-8 name", env: []string{"LA" + string([]byte{0xff}) + "G=value"}, want: "invalid environment"},
		{name: "invalid UTF-8 value", env: []string{"LANG=" + string([]byte{0xff})}, want: "invalid environment"},
		{name: "oversized value", env: []string{"PATH=" + strings.Repeat("x", 8193)}, want: "invalid environment"},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			_, err := PrepareExecSpec(ExecSpec{Target: missing, Cwd: missing, Env: test.env})
			if err == nil || !strings.Contains(err.Error(), test.want) {
				t.Fatalf("invalid environment was not rejected before path commitment: %v", err)
			}
		})
	}
}

func TestPrepareExecSpecRejectsInvalidUTF8BeforeControlCommitment(t *testing.T) {
	control, err := os.Open("/dev/null")
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = control.Close() })
	_, err = PrepareExecSpec(ExecSpec{
		Target: "/bin/sh", Cwd: t.TempDir(), Env: []string{"LANG=" + string([]byte{0xff})}, Control: control,
	})
	if err == nil || !strings.Contains(err.Error(), "invalid environment") {
		t.Fatalf("invalid UTF-8 was not rejected before control commitment: %v", err)
	}
}

func TestAttemptConfigRejectsInvalidEnvironment(t *testing.T) {
	root := t.TempDir()
	prepared, err := PrepareExecSpec(ExecSpec{Target: "/bin/sh", Cwd: root, Env: []string{"PATH=/usr/bin:/bin"}})
	if err != nil {
		t.Fatal(err)
	}
	for _, test := range []struct {
		name string
		env  []string
	}{
		{name: "duplicate", env: []string{"PATH=/usr/bin", "PATH=/bin"}},
		{name: "invalid UTF-8 name", env: []string{"LA" + string([]byte{0xff}) + "G=value"}},
		{name: "invalid UTF-8 value", env: []string{"LANG=" + string([]byte{0xff})}},
	} {
		t.Run(test.name, func(t *testing.T) {
			commitment := prepared.commit
			commitment.Env = test.env
			cfg := attemptConfig{Version: 1, AttemptID: "attempt", MarkerName: "marker", ResultName: "terminal", Wrapper: commitment}
			if err := validateAttemptConfig(cfg); err != ErrIdentity {
				t.Fatalf("invalid recovered environment = %v, want ErrIdentity", err)
			}
		})
	}
}

func TestPreparedEnvironmentOrderAndBytesReachRealChildExactly(t *testing.T) {
	f := newFixture(t)
	output := outputFile(t, filepath.Join(f.root, "environment.out"))
	environment := []string{
		"DARK_FACTORY_SOCKET=/private/runtime/api.sock",
		"DARK_FACTORY_ATTEMPT_TOKEN_FILE=/private/runtime/attempt.token",
		"DARK_FACTORY_FACTORYCTL=/private/release/factoryctl",
		"HOME=/private/provider-home",
		"TMPDIR=/private/provider-tmp",
		"PATH=/usr/bin:/bin",
		"LANG=工場.UTF-8",
		"LC_ALL=C",
		"TERM=dumb",
		"GIT_CEILING_DIRECTORIES=/private/changes",
		"GIT_DISCOVERY_ACROSS_FILESYSTEM=0",
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_TERMINAL_PROMPT=0",
		"GIT_ASKPASS=/usr/bin/false",
		"GIT_SSH_COMMAND=/usr/bin/false -F /dev/null",
		"GH_CONFIG_DIR=/dev/null",
	}
	want := append([]string{}, environment...)
	prepared, err := PrepareExecSpec(ExecSpec{
		Target: "/usr/bin/env", Cwd: f.cwd, Env: environment, Stdout: output, Stderr: output,
	})
	if err != nil {
		t.Fatal(err)
	}
	environment[0] = "DARK_FACTORY_SOCKET=/mutated"
	if !slices.Equal(prepared.commit.Env, want) {
		t.Fatalf("prepared environment changed with caller slice: %q", prepared.commit.Env)
	}
	child := f.startPrepared(prepared, false)
	if _, err := child.Activate(); err != nil {
		t.Fatal(err)
	}
	exit, err := child.FinishAfterExit(4 * time.Second)
	if err != nil || exit.Code != 0 || exit.Signal != 0 || exit.LaunchErr != "" {
		t.Fatalf("environment child exit=%+v err=%v", exit, err)
	}
	if err := output.Sync(); err != nil {
		t.Fatal(err)
	}
	if _, err := output.Seek(0, 0); err != nil {
		t.Fatal(err)
	}
	body, err := io.ReadAll(output)
	if err != nil {
		t.Fatal(err)
	}
	if got, expected := string(body), strings.Join(want, "\n")+"\n"; got != expected {
		t.Fatalf("child environment bytes/order differ\ngot:  %q\nwant: %q", got, expected)
	}
	if !slices.Contains(want, "PATH=/usr/bin:/bin") {
		t.Fatal("provider PATH changed")
	}
	assertWaitedAndAbsent(t, child)
}

func TestExecutableCommitmentRequiresExactDirectNativeTarget(t *testing.T) {
	executable, err := os.Executable()
	if err != nil {
		t.Fatal(err)
	}
	newTarget := func(t *testing.T) string {
		t.Helper()
		root, err := filepath.EvalSymlinks(t.TempDir())
		if err != nil {
			t.Fatal(err)
		}
		target := filepath.Join(root, "factoryctl")
		copyNative(t, executable, target)
		return target
	}
	t.Run("exact and replacement", func(t *testing.T) {
		target := newTarget(t)
		commitment, err := CommitExecutableLocator(target)
		if err != nil || commitment.Path() != target || commitment.Verify() != nil {
			t.Fatalf("commitment=%v path=%q err=%v", commitment, commitment.Path(), err)
		}
		if formatted := commitment.String() + commitment.GoString(); strings.Contains(formatted, target) {
			t.Fatal("commitment formatting exposed locator")
		}
		replacement := filepath.Join(t.TempDir(), "replacement")
		copyNative(t, executable, replacement)
		if err := os.Rename(replacement, target); err != nil {
			t.Fatal(err)
		}
		if err := commitment.Verify(); err == nil || strings.Contains(err.Error(), target) {
			t.Fatalf("replacement verification=%v", err)
		}
	})
	for _, test := range []struct {
		name   string
		mutate func(*testing.T, string) string
	}{
		{name: "missing", mutate: func(t *testing.T, target string) string { return target + ".missing" }},
		{name: "relative", mutate: func(t *testing.T, target string) string { return "factoryctl" }},
		{name: "unclean", mutate: func(t *testing.T, target string) string {
			return filepath.Dir(target) + "/sub/../" + filepath.Base(target)
		}},
		{name: "symlink", mutate: func(t *testing.T, target string) string {
			link := target + ".link"
			if err := os.Symlink(target, link); err != nil {
				t.Fatal(err)
			}
			return link
		}},
		{name: "unsafe mode", mutate: func(t *testing.T, target string) string {
			if err := os.Chmod(target, 0o775); err != nil {
				t.Fatal(err)
			}
			return target
		}},
		{name: "wrong architecture", mutate: func(t *testing.T, target string) string {
			rewriteMachOArchitecture(t, target)
			return target
		}},
	} {
		t.Run(test.name, func(t *testing.T) {
			target := test.mutate(t, newTarget(t))
			if _, err := CommitExecutableLocator(target); err == nil || strings.Contains(err.Error(), target) {
				t.Fatalf("invalid locator accepted or exposed: %v", err)
			}
		})
	}
}

func rewriteMachOArchitecture(t testing.TB, path string) {
	t.Helper()
	file, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		t.Fatal(err)
	}
	defer file.Close()
	header := make([]byte, 8)
	if _, err := file.ReadAt(header, 0); err != nil {
		t.Fatal(err)
	}
	order := binary.LittleEndian
	if binary.LittleEndian.Uint32(header[:4]) != macho.Magic32 && binary.LittleEndian.Uint32(header[:4]) != macho.Magic64 {
		t.Fatal("test helper is not a thin little-endian Mach-O")
	}
	want := uint32(macho.CpuAmd64)
	if binary.LittleEndian.Uint32(header[4:8]) == want {
		want = uint32(macho.CpuArm64)
	}
	order.PutUint32(header[4:8], want)
	if _, err := file.WriteAt(header[4:8], 4); err != nil {
		t.Fatal(err)
	}
}
