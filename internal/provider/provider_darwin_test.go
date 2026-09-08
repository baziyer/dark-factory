//go:build darwin

package provider

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"sync"
	"testing"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

func runtimeFixture(t *testing.T, toolPath, accountHome string) RuntimePaths {
	t.Helper()
	root := t.TempDir()
	runtime, err := NewRuntimePaths(
		root+"/home", root+"/tmp", "/private/tmp/df-provider-test.sock", root+"/attempt.token",
		"/usr/local/bin/factoryctl", root+"/changes", toolPath, accountHome, "",
	)
	if err != nil {
		t.Fatal(err)
	}
	return runtime
}

func providerFixture(t *testing.T) (Installation, RuntimePaths) {
	t.Helper()
	toolPath := "/opt/homebrew/bin:/usr/bin:/bin"
	installation, err := ResolveInstallation(kernel.ProviderShell, toolPath)
	if err != nil {
		t.Fatal(err)
	}
	return installation, runtimeFixture(t, toolPath, filepath.Join(t.TempDir(), "account"))
}

func nativeFixture(t *testing.T, kind kernel.Provider) (Installation, RuntimePaths, string) {
	t.Helper()
	missing := t.TempDir()
	tools := t.TempDir()
	var tool string
	if kind == kernel.ProviderClaudeCode {
		tool = claudeTool
	} else if kind == kernel.ProviderCodex {
		tool = codexTool
	} else {
		t.Fatalf("provider %s is not native", kind)
	}
	locator := filepath.Join(tools, tool)
	if err := os.Symlink("/usr/bin/true", locator); err != nil {
		t.Fatal(err)
	}
	toolPath := strings.Join([]string{missing, tools, "/usr/bin", "/bin"}, string(filepath.ListSeparator))
	installation, err := ResolveInstallation(kind, toolPath)
	if err != nil {
		t.Fatal(err)
	}
	accountHome := filepath.Join(t.TempDir(), "account")
	return installation, runtimeFixture(t, toolPath, accountHome), locator
}

func requestFor(t *testing.T, kind kernel.Provider, installation Installation, runtime RuntimePaths, model, effort string) Request {
	t.Helper()
	return roleRequestFor(t, kind, installation, runtime, model, effort, kernel.RoleWorker)
}

func roleRequestFor(t *testing.T, kind kernel.Provider, installation Installation, runtime RuntimePaths, model, effort string, role kernel.AgentRole) Request {
	t.Helper()
	request, err := NewRequest(kind, installation, model, effort, runtime, filepath.Join(t.TempDir(), "change"), role)
	if err != nil {
		t.Fatal(err)
	}
	return request
}

// An orchestrator's Claude session is handed the Maintainer bridge as its one
// MCP server, resolved on the fixed tool path; a worker's is handed none, and
// an orchestrator without the bridge is not launched at all.
func TestBuildOrchestratorClaudeIsGivenTheMaintainerBridge(t *testing.T) {
	installation, runtime, locator := nativeFixture(t, kernel.ProviderClaudeCode)
	worker, err := Build(roleRequestFor(t, kernel.ProviderClaudeCode, installation, runtime, "", "", kernel.RoleWorker))
	if err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(worker.Argv(), []string{"/usr/bin/true", "--dangerously-skip-permissions", "--strict-mcp-config"}) {
		t.Fatalf("worker argv = %q, want strict MCP with no server", worker.Argv())
	}
	if _, err := Build(roleRequestFor(t, kernel.ProviderClaudeCode, installation, runtime, "", "", kernel.RoleOrchestrator)); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("orchestrator without the bridge = %v, want ErrUnavailable", err)
	}
	// The installed bridge is a script, which the CLI commitment would refuse;
	// it needs only to be a regular executable nobody but its owner can write.
	bridge := filepath.Join(filepath.Dir(locator), maintainerBridge)
	if err := os.WriteFile(bridge, []byte("#!/bin/sh\nexit 0\n"), 0o755); err != nil {
		t.Fatal(err)
	}
	resolvedBridge, err := filepath.EvalSymlinks(bridge)
	if err != nil {
		t.Fatal(err)
	}
	launch, err := Build(roleRequestFor(t, kernel.ProviderClaudeCode, installation, runtime, "", "", kernel.RoleOrchestrator))
	if err != nil {
		t.Fatal(err)
	}
	want := []string{"/usr/bin/true", "--dangerously-skip-permissions", "--strict-mcp-config", "--mcp-config", `{"mcpServers":{"maintainer":{"command":"` + resolvedBridge + `"}}}`}
	if !reflect.DeepEqual(launch.Argv(), want) {
		t.Fatalf("orchestrator argv = %q, want %q", launch.Argv(), want)
	}
	// A bridge that is present but unfit is refused by name, unlike a
	// missing one, so the operator learns which of the two it is.
	for name, mode := range map[string]os.FileMode{"not executable": 0o644, "group writable": 0o775} {
		if err := os.Chmod(bridge, mode); err != nil {
			t.Fatal(err)
		}
		if _, err := Build(roleRequestFor(t, kernel.ProviderClaudeCode, installation, runtime, "", "", kernel.RoleOrchestrator)); !errors.Is(err, ErrUnavailable) || !errors.Is(err, errBridgeUnfit) {
			t.Fatalf("%s bridge = %v, want ErrUnavailable and errBridgeUnfit", name, err)
		}
	}
	if err := os.Remove(bridge); err != nil {
		t.Fatal(err)
	}
	if _, err := Build(roleRequestFor(t, kernel.ProviderClaudeCode, installation, runtime, "", "", kernel.RoleOrchestrator)); !errors.Is(err, ErrUnavailable) || errors.Is(err, errBridgeUnfit) {
		t.Fatalf("missing bridge = %v, want ErrUnavailable alone", err)
	}
	if _, err := NewRequest(kernel.ProviderClaudeCode, installation, "", "", runtime, "/private/change", kernel.AgentRole(0)); !errors.Is(err, ErrInvalid) {
		t.Fatalf("request without a role = %v, want ErrInvalid", err)
	}
}

func TestBuildShellReturnsExactImmutableLaunchAndTask(t *testing.T) {
	installation, runtime := providerFixture(t)
	t.Setenv("ANTHROPIC_API_KEY", "AMBIENT_SENTINEL")
	t.Setenv("HTTP_PROXY", "AMBIENT_SENTINEL")
	launch, err := Build(requestFor(t, kernel.ProviderShell, installation, runtime, "", ""))
	if err != nil {
		t.Fatal(err)
	}
	if launch.Executable().Path() != shellPath || launch.Executable() != installation.executable {
		t.Fatal("launch did not retain the exact Shell executable commitment")
	}
	if launch.TaskDelivery() != TaskDeliveryFD11 {
		t.Fatalf("task delivery=%d, want fd 11", launch.TaskDelivery())
	}
	wantArgv := []string{"/bin/sh", runner.ProviderTaskPath}
	if got := launch.Argv(); !slices.Equal(got, wantArgv) {
		t.Fatalf("argv=%q, want %q", got, wantArgv)
	}
	wantEnvironment := []string{
		"DARK_FACTORY_SOCKET=" + runtime.socket,
		"DARK_FACTORY_ATTEMPT_TOKEN_FILE=" + runtime.token,
		"DARK_FACTORY_FACTORYCTL=" + runtime.factoryctl,
		"HOME=" + runtime.home,
		"TMPDIR=" + runtime.temp,
		"PATH=" + runtime.toolPath,
		"LANG=C", "LC_ALL=C", "TERM=xterm-256color", "SHELL=/bin/sh",
		"GIT_CEILING_DIRECTORIES=" + runtime.gitCeiling,
		"GIT_DISCOVERY_ACROSS_FILESYSTEM=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_TERMINAL_PROMPT=0", "GIT_ASKPASS=/usr/bin/false", "GIT_SSH_COMMAND=/usr/bin/false", "GH_CONFIG_DIR=/dev/null",
	}
	if got := launch.Environment(); !slices.Equal(got, wantEnvironment) {
		t.Fatalf("environment=\n%q\nwant=\n%q", got, wantEnvironment)
	}
	for _, entry := range launch.Environment() {
		if strings.Contains(entry, "AMBIENT_SENTINEL") || strings.HasPrefix(entry, "ANTHROPIC_API_KEY=") || strings.HasPrefix(entry, "HTTP_PROXY=") {
			t.Fatalf("ambient environment reached launch: %q", entry)
		}
	}

	argv := launch.Argv()
	environment := launch.Environment()
	argv[0] = "/bin/false"
	environment[0] = "DARK_FACTORY_SOCKET=/ambient"
	if !slices.Equal(launch.Argv(), wantArgv) || !slices.Equal(launch.Environment(), wantEnvironment) {
		t.Fatal("Launch accessors exposed mutable authoritative slices")
	}
	task := []byte("echo TASK_SENTINEL\n")
	wantTask := bytes.Clone(task)
	delivery, prepared, err := PrepareTask(kernel.ProviderShell, task)
	if err != nil {
		t.Fatal(err)
	}
	if delivery != launch.TaskDelivery() {
		t.Fatalf("prepared delivery=%d, want %d", delivery, launch.TaskDelivery())
	}
	task[0] = 'X'
	if !bytes.Equal(prepared, wantTask) {
		t.Fatalf("prepared task=%q, want exact program %q", prepared, wantTask)
	}
	if _, err := runner.PrepareCommittedExecSpec(launch.Executable(), launch.Argv(), launch.Environment(), t.TempDir()); err != nil {
		t.Fatalf("runner refused exact provider launch: %v", err)
	}
}

func TestResolveNativeInstallationCommitsResolvedDirectTargetOnce(t *testing.T) {
	for _, kind := range []kernel.Provider{kernel.ProviderClaudeCode, kernel.ProviderCodex} {
		t.Run(kind.String(), func(t *testing.T) {
			installation, runtime, locator := nativeFixture(t, kind)
			if installation.provider != kind || installation.executable.Path() != "/usr/bin/true" {
				t.Fatalf("installation provider=%s path=%q", installation.provider, installation.executable.Path())
			}
			if err := os.Remove(locator); err != nil {
				t.Fatal(err)
			}
			if _, err := Build(requestFor(t, kind, installation, runtime, "", "")); err != nil {
				t.Fatalf("resolved commitment depended on removed symlink: %v", err)
			}
		})
	}
}

func TestResolveInstallationFailsClosedAtInvalidExistingCandidate(t *testing.T) {
	first := t.TempDir()
	second := t.TempDir()
	if err := os.WriteFile(filepath.Join(first, codexTool), []byte("#!/bin/sh\nexit 0\n"), 0o500); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink("/usr/bin/true", filepath.Join(second, codexTool)); err != nil {
		t.Fatal(err)
	}
	toolPath := first + string(filepath.ListSeparator) + second
	if _, err := ResolveInstallation(kernel.ProviderCodex, toolPath); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("invalid first candidate error=%v, want ErrUnavailable", err)
	}
	if err := os.Remove(filepath.Join(first, codexTool)); err != nil {
		t.Fatal(err)
	}
	if err := os.Symlink(filepath.Join(first, "missing-target"), filepath.Join(first, codexTool)); err != nil {
		t.Fatal(err)
	}
	if _, err := ResolveInstallation(kernel.ProviderCodex, toolPath); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("broken first symlink error=%v, want ErrUnavailable", err)
	}
	if _, err := ResolveInstallation(kernel.ProviderCodex, "relative:/bin"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("invalid tool path error=%v, want ErrInvalid", err)
	}
	if _, err := ResolveInstallation(kernel.Provider(255), "/usr/bin:/bin"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("unknown provider error=%v, want ErrInvalid", err)
	}
}

func TestBuildNativeReturnsExactArgvEnvironmentAndSafeStartupTask(t *testing.T) {
	tests := []struct {
		kind         kernel.Provider
		model        string
		effort       string
		wantDelivery TaskDelivery
		wantArgv     []string
	}{
		{
			kind: kernel.ProviderClaudeCode, model: "claude-model", effort: "max", wantDelivery: TaskDeliveryStartupTerminal,
			wantArgv: []string{"/usr/bin/true", "--dangerously-skip-permissions", "--model", "claude-model", "--effort", "max", "--strict-mcp-config"},
		},
		{
			kind: kernel.ProviderCodex, model: "codex-model", effort: "xhigh", wantDelivery: TaskDeliveryAttemptAPI,
			wantArgv: []string{"/usr/bin/true", "--dangerously-bypass-approvals-and-sandbox", "--no-alt-screen", "-c", "check_for_update_on_startup=false", "-c", "tool_output_token_limit=32768", "-c", "projects=<working-directory>", "--model", "codex-model", "-c", `model_reasoning_effort="xhigh"`, "Run \"$DARK_FACTORY_FACTORYCTL\" attempt task before doing anything else. The returned JSON task field is the exact task: complete only that task. Before exiting, report the durable outcome with \"$DARK_FACTORY_FACTORYCTL\" attempt succeed, block, or fail."},
		},
	}
	for _, test := range tests {
		t.Run(test.kind.String(), func(t *testing.T) {
			const privateTaskSentinel = "PRIVATE_TASK_SENTINEL"
			task := []byte(privateTaskSentinel + "\nline 1\n\"quoted\"\x1b café 😀\u007f\u0085")
			installation, runtime, _ := nativeFixture(t, test.kind)
			request := requestFor(t, test.kind, installation, runtime, test.model, test.effort)
			launch, err := Build(request)
			if err != nil {
				t.Fatal(err)
			}
			if launch.TaskDelivery() != test.wantDelivery {
				t.Fatalf("task delivery=%d, want %d", launch.TaskDelivery(), test.wantDelivery)
			}
			wantArgv := test.wantArgv
			if test.kind == kernel.ProviderCodex {
				wantArgv = slices.Replace(wantArgv, 8, 9, codexUntrustedProjectConfig(request.workingDirectory))
			}
			if got := launch.Argv(); !slices.Equal(got, wantArgv) {
				t.Fatalf("argv=%q, want %q", got, wantArgv)
			}
			wantHome := runtime.home
			if test.kind == kernel.ProviderClaudeCode {
				wantHome = runtime.accountHome
			} else {
				wantConfig := "CODEX_HOME=" + filepath.Join(runtime.accountHome, codexConfigDir)
				if !slices.Contains(launch.Environment(), wantConfig) {
					t.Fatalf("environment lacks exact config root %q: %q", wantConfig, launch.Environment())
				}
			}
			if !slices.Contains(launch.Environment(), "HOME="+wantHome) {
				t.Fatalf("environment lacks exact HOME %q: %q", wantHome, launch.Environment())
			}
			for _, entry := range launch.Environment() {
				if strings.HasPrefix(entry, "HOME=") && entry != "HOME="+wantHome {
					t.Fatalf("native provider received wrong HOME: %q", entry)
				}
				if test.kind == kernel.ProviderClaudeCode && (strings.HasPrefix(entry, "CODEX_HOME=") || strings.HasPrefix(entry, "CLAUDE_CONFIG_DIR=")) ||
					test.kind == kernel.ProviderCodex && strings.HasPrefix(entry, "CLAUDE_CONFIG_DIR=") {
					t.Fatalf("provider received another provider's config root: %q", entry)
				}
			}

			for _, value := range append(launch.Argv(), launch.Environment()...) {
				if strings.Contains(value, privateTaskSentinel) {
					t.Fatalf("private task reached provider argv or environment: %q", value)
				}
			}
			delivery, payload, err := PrepareTask(test.kind, task)
			if err != nil {
				t.Fatal(err)
			}
			if delivery != launch.TaskDelivery() {
				t.Fatalf("prepared delivery=%d, want %d", delivery, launch.TaskDelivery())
			}
			if test.kind == kernel.ProviderCodex {
				if payload != nil {
					t.Fatalf("Codex task bytes escaped attempt API delivery: %q", payload)
				}
			} else {
				want := claudeTaskLead + `"PRIVATE_TASK_SENTINEL\nline 1\n\"quoted\"\u001b café 😀\u007f\u0085"` + "\r"
				if string(payload) != want {
					t.Fatalf("startup payload=%q, want %q", payload, want)
				}
				if payload[len(payload)-1] != '\r' || bytes.IndexByte(payload[:len(payload)-1], '\r') >= 0 || bytes.IndexByte(payload, '\n') >= 0 || bytes.IndexByte(payload, 0x1b) >= 0 || bytes.IndexByte(payload, 0x7f) >= 0 {
					t.Fatalf("startup payload contains raw terminal control: %q", payload)
				}
				var decoded string
				if err := json.Unmarshal(payload[len(claudeTaskLead):len(payload)-1], &decoded); err != nil || decoded != string(task) {
					t.Fatalf("JSON task decoded as %q: %v", decoded, err)
				}
			}
		})
	}
}

func TestCodexProjectTrustOverrideUsesExactTomlEncoding(t *testing.T) {
	path := "/private/change.with dots/quote\"slash\\\b\t\n\f\r\x01\x1f\x7f\u0080é/working"
	want := `"/private/change.with dots/quote\"slash\\\b\t\n\f\r\u0001\u001f\u007f\u0080é/working"`
	if got := tomlBasicString(path); got != want {
		t.Fatalf("TOML path=%q, want %q", got, want)
	}
	wantConfig := `projects={"/private/change.with dots/quote\"slash\\\b\t\n\f\r\u0001\u001f\u007f\u0080é/working"={trust_level="untrusted"}}`
	if got := codexUntrustedProjectConfig(path); got != wantConfig {
		t.Fatalf("Codex project config=%q", got)
	}
}

func TestNewRequestRejectsMismatchedProviderAndControls(t *testing.T) {
	shell, shellRuntime := providerFixture(t)
	for _, controls := range []struct{ model, effort string }{
		{model: "model-sentinel"},
		{effort: "high"},
		{model: "model-sentinel", effort: "high"},
	} {
		if _, err := NewRequest(kernel.ProviderShell, shell, controls.model, controls.effort, shellRuntime, "/private/change", kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
			t.Fatalf("Shell controls %+v error=%v, want ErrInvalid", controls, err)
		}
	}
	if _, err := NewRequest(kernel.ProviderCodex, shell, "", "", shellRuntime, "/private/change", kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
		t.Fatalf("mismatched installation error=%v, want ErrInvalid", err)
	}
	claude, claudeRuntime, _ := nativeFixture(t, kernel.ProviderClaudeCode)
	for _, effort := range []string{"ultra", "speculative"} {
		if _, err := NewRequest(kernel.ProviderClaudeCode, claude, "", effort, claudeRuntime, "/private/change", kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
			t.Fatalf("Claude effort %q error=%v, want ErrInvalid", effort, err)
		}
	}
	codex, codexRuntime, _ := nativeFixture(t, kernel.ProviderCodex)
	if _, err := NewRequest(kernel.ProviderCodex, codex, "", "ultra", codexRuntime, "/private/change", kernel.RoleWorker); err != nil {
		t.Fatalf("Codex durable ultra effort rejected: %v", err)
	}
	for _, model := range []string{string([]byte{0xff}), "model\x00suffix"} {
		if _, err := NewRequest(kernel.ProviderCodex, codex, model, "", codexRuntime, "/private/change", kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
			t.Fatalf("model %x error=%v, want ErrInvalid", []byte(model), err)
		}
	}
	for _, workingDirectory := range []string{"", "relative/change", "/", "/private/../change", "/private/change\x00suffix", string([]byte{0xff})} {
		if _, err := NewRequest(kernel.ProviderCodex, codex, "", "", codexRuntime, workingDirectory, kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
			t.Fatalf("working directory %q error=%v, want ErrInvalid", workingDirectory, err)
		}
	}
	tooLarge := "/" + strings.Repeat("\x7f", maxPathBytes-1)
	if len(codexUntrustedProjectConfig(tooLarge)) <= runner.MaxArgumentBytes {
		t.Fatalf("expanded project config=%d, want larger than argv bound", len(codexUntrustedProjectConfig(tooLarge)))
	}
	if _, err := NewRequest(kernel.ProviderCodex, codex, "", "", codexRuntime, tooLarge, kernel.RoleWorker); !errors.Is(err, ErrInvalid) {
		t.Fatalf("oversized encoded working directory error=%v, want ErrInvalid", err)
	}
}

func TestNewRuntimePathsRejectsMissingAndMalformedValues(t *testing.T) {
	root := t.TempDir()
	valid := []string{root + "/home", root + "/tmp", "/private/tmp/df-provider-path-test.sock", root + "/token", "/usr/local/bin/factoryctl", root + "/changes", "/usr/bin:/bin", root + "/account"}
	for index := range valid {
		t.Run(string(rune('a'+index)), func(t *testing.T) {
			values := append([]string(nil), valid...)
			values[index] = ""
			if _, err := NewRuntimePaths(values[0], values[1], values[2], values[3], values[4], values[5], values[6], values[7], ""); !errors.Is(err, ErrInvalid) {
				t.Fatalf("missing index %d error=%v, want ErrInvalid", index, err)
			}
		})
	}
	for _, toolPath := range []string{"relative:/bin", "/usr/bin::/bin", "/usr/bin:/usr/bin"} {
		if _, err := NewRuntimePaths(valid[0], valid[1], valid[2], valid[3], valid[4], valid[5], toolPath, valid[7], ""); !errors.Is(err, ErrInvalid) {
			t.Fatalf("tool path %q error=%v, want ErrInvalid", toolPath, err)
		}
	}
	first := "/" + strings.Repeat("a", maxPathBytes-1)
	exact := first + ":/" + strings.Repeat("b", runner.MaxEnvironmentEntryBytes-len("PATH=")-len(first)-2)
	if len(exact) != runner.MaxEnvironmentEntryBytes-len("PATH=") {
		t.Fatalf("exact ToolPath length = %d", len(exact))
	}
	if err := ValidateToolPath(exact); err != nil {
		t.Fatalf("exact ToolPath rejected: %v", err)
	}
	if err := ValidateToolPath(exact + "b"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("ToolPath wider than PATH environment error=%v, want ErrInvalid", err)
	}

	invalid := append([]string(nil), valid...)
	invalid[0] = "relative/home"
	if _, err := NewRuntimePaths(invalid[0], invalid[1], invalid[2], invalid[3], invalid[4], invalid[5], invalid[6], invalid[7], ""); !errors.Is(err, ErrInvalid) {
		t.Fatalf("relative home error=%v, want ErrInvalid", err)
	}
	invalid = append([]string(nil), valid...)
	invalid[2] = "/" + strings.Repeat("s", install.MaxSocketPathBytes)
	if _, err := NewRuntimePaths(invalid[0], invalid[1], invalid[2], invalid[3], invalid[4], invalid[5], invalid[6], invalid[7], ""); !errors.Is(err, ErrInvalid) {
		t.Fatalf("oversized socket error=%v, want ErrInvalid", err)
	}
	invalid = append([]string(nil), valid...)
	invalid[5] = valid[5] + ":/private/other-ceiling"
	if _, err := NewRuntimePaths(invalid[0], invalid[1], invalid[2], invalid[3], invalid[4], invalid[5], invalid[6], invalid[7], ""); !errors.Is(err, ErrInvalid) {
		t.Fatalf("multi-ceiling git path error=%v, want ErrInvalid", err)
	}
	for _, accountHome := range []string{"relative", "/", valid[0], valid[1]} {
		if _, err := NewRuntimePaths(valid[0], valid[1], valid[2], valid[3], valid[4], valid[5], valid[6], accountHome, ""); !errors.Is(err, ErrInvalid) {
			t.Fatalf("account home %q error=%v, want ErrInvalid", accountHome, err)
		}
	}
}

func TestTaskValidationUsesDeliverySpecificBound(t *testing.T) {
	for _, kind := range []kernel.Provider{kernel.ProviderShell, kernel.ProviderClaudeCode, kernel.ProviderCodex} {
		for _, task := range [][]byte{nil, {}, []byte("bad\x00task"), {0xff}} {
			if _, _, err := PrepareTask(kind, task); !errors.Is(err, ErrInvalid) {
				t.Fatalf("provider=%s invalid task len=%d error=%v, want ErrInvalid", kind, len(task), err)
			}
		}
	}
	shellMaximum := bytes.Repeat([]byte{'x'}, runner.MaxProviderTaskBytes)
	if delivery, program, err := PrepareTask(kernel.ProviderShell, shellMaximum); err != nil || delivery != TaskDeliveryFD11 || !bytes.Equal(program, shellMaximum) {
		t.Fatalf("Shell maximum task error=%v", err)
	}
	if _, _, err := PrepareTask(kernel.ProviderShell, append(shellMaximum, 'x')); !errors.Is(err, ErrInvalid) {
		t.Fatalf("Shell over-limit task error=%v, want ErrInvalid", err)
	}
	claudeMaximum := bytes.Repeat([]byte{'x'}, maxClaudePrompt-len(claudeTaskLead)-3)
	if delivery, _, err := PrepareTask(kernel.ProviderClaudeCode, claudeMaximum); err != nil || delivery != TaskDeliveryStartupTerminal {
		t.Fatalf("Claude exact startup bound rejected: %v", err)
	}
	if _, _, err := PrepareTask(kernel.ProviderClaudeCode, append(claudeMaximum, 'x')); !errors.Is(err, ErrInvalid) {
		t.Fatalf("Claude over-limit task error=%v, want ErrInvalid", err)
	}
	codexMaximum := bytes.Repeat([]byte{'x'}, maxCodexTask)
	if delivery, payload, err := PrepareTask(kernel.ProviderCodex, codexMaximum); err != nil || delivery != TaskDeliveryAttemptAPI || payload != nil {
		t.Fatalf("Codex maximum API task delivery=(%d, %d bytes), error=%v", delivery, len(payload), err)
	}
	if _, _, err := PrepareTask(kernel.ProviderCodex, append(codexMaximum, 'x')); !errors.Is(err, ErrInvalid) {
		t.Fatalf("Codex over-limit task error=%v, want ErrInvalid", err)
	}
	if _, _, err := PrepareTask(kernel.Provider(255), []byte("task")); !errors.Is(err, ErrInvalid) {
		t.Fatalf("unknown provider error=%v, want ErrInvalid", err)
	}
}

func TestLaunchFormattingDoesNotRevealRuntimeOrExecutable(t *testing.T) {
	installation, runtime, _ := nativeFixture(t, kernel.ProviderCodex)
	launch, err := Build(requestFor(t, kernel.ProviderCodex, installation, runtime, "", ""))
	if err != nil {
		t.Fatal(err)
	}
	for _, formatted := range []string{launch.String(), launch.GoString(), installation.String(), runtime.String()} {
		for _, secret := range []string{runtime.home, runtime.socket, runtime.accountHome, installation.executable.Path()} {
			if strings.Contains(formatted, secret) {
				t.Fatalf("formatting exposed %q in %q", secret, formatted)
			}
		}
	}
}

// The Change is a path Claude Code has never seen, so the account's own trust
// record is written for it before launch, and nothing else in that file moves:
// other projects, the login, and a number JSON would otherwise round.
func TestTrustClaudeDirectoryRecordsOnlyTheWorkingDirectory(t *testing.T) {
	accountHome := t.TempDir()
	runtime := runtimeFixture(t, "/usr/bin:/bin", accountHome)
	path := filepath.Join(accountHome, ".claude.json")
	original := `{"oauthAccount":{"emailAddress":"login@example.invalid"},"numStartups":9007199254740993,"lastPrompt":"a<b&c","projects":{"/other":{"hasTrustDialogAccepted":true,"lastCost":0.25}}}`
	if err := os.WriteFile(path, []byte(original), 0o600); err != nil {
		t.Fatal(err)
	}
	if err := TrustClaudeDirectory(runtime, "/private/change"); err != nil {
		t.Fatal(err)
	}
	written, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	var config map[string]any
	if err := json.Unmarshal(written, &config); err != nil {
		t.Fatal(err)
	}
	projects := config["projects"].(map[string]any)
	if projects["/private/change"].(map[string]any)["hasTrustDialogAccepted"] != true {
		t.Fatalf("working directory is not trusted: %s", written)
	}
	other := projects["/other"].(map[string]any)
	if other["hasTrustDialogAccepted"] != true || other["lastCost"] != 0.25 || config["oauthAccount"].(map[string]any)["emailAddress"] != "login@example.invalid" || !bytes.Contains(written, []byte("9007199254740993")) || !bytes.Contains(written, []byte(`"a<b&c"`)) {
		t.Fatalf("other values moved: %s", written)
	}
	if info, err := os.Stat(path); err != nil || info.Mode().Perm() != 0o600 {
		t.Fatalf("mode = %v, %v", info, err)
	}
	if err := TrustClaudeDirectory(runtime, "/private/change"); err != nil {
		t.Fatal(err)
	}
	if again, err := os.ReadFile(path); err != nil || !bytes.Equal(again, written) {
		t.Fatalf("second record rewrote the file: %v", err)
	}
	// No file yet is a fresh login-less home; the record is still written.
	if err := os.Remove(path); err != nil {
		t.Fatal(err)
	}
	if err := TrustClaudeDirectory(runtime, "/private/change"); err != nil {
		t.Fatal(err)
	}
	if fresh, err := os.ReadFile(path); err != nil || !bytes.Contains(fresh, []byte(`"/private/change":{"hasTrustDialogAccepted":true}`)) {
		t.Fatalf("fresh record = %s, %v", fresh, err)
	}
	// A linked account keeps its configuration, and so its trust, in its own directory.
	accountConfig := filepath.Join(t.TempDir(), "claude-dogfood")
	if err := os.Mkdir(accountConfig, 0o700); err != nil {
		t.Fatal(err)
	}
	linked, err := NewRuntimePaths(runtime.home, runtime.temp, runtime.socket, runtime.token, runtime.factoryctl, runtime.gitCeiling, runtime.toolPath, accountHome, accountConfig)
	if err != nil {
		t.Fatal(err)
	}
	if err := TrustClaudeDirectory(linked, "/private/change"); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(accountConfig, ".claude.json")); err != nil {
		t.Fatalf("linked account record: %v", err)
	}
	// The default login discovered as an account names the default directory;
	// its record still lives beside that directory, in the home's own file.
	discovered, err := NewRuntimePaths(runtime.home, runtime.temp, runtime.socket, runtime.token, runtime.factoryctl, runtime.gitCeiling, runtime.toolPath, accountHome, ConfigHome(kernel.ProviderClaudeCode, accountHome))
	if err != nil {
		t.Fatal(err)
	}
	if err := TrustClaudeDirectory(discovered, "/private/discovered"); err != nil {
		t.Fatal(err)
	}
	if home, err := os.ReadFile(path); err != nil || !bytes.Contains(home, []byte(`"/private/discovered":{"hasTrustDialogAccepted":true}`)) {
		t.Fatalf("default login record = %s, %v", home, err)
	}
	if _, err := os.Stat(filepath.Join(ConfigHome(kernel.ProviderClaudeCode, accountHome), ".claude.json")); !errors.Is(err, os.ErrNotExist) {
		t.Fatalf("default login record landed inside the flags directory: %v", err)
	}
	// Concurrent workers on one account never publish a torn file: each
	// writes its own temporary name and renames whole.
	var group sync.WaitGroup
	for index := range 8 {
		group.Add(1)
		go func() {
			defer group.Done()
			if err := TrustClaudeDirectory(runtime, fmt.Sprintf("/private/concurrent-%d", index)); err != nil {
				t.Error(err)
			}
		}()
	}
	group.Wait()
	final, err := os.ReadFile(path)
	if err != nil || !json.Valid(final) || !bytes.Contains(final, []byte("/private/concurrent-")) {
		t.Fatalf("concurrent records left %s, %v", final, err)
	}
	if leftovers, _ := filepath.Glob(filepath.Join(accountHome, ".claude.json.*")); len(leftovers) != 0 {
		t.Fatalf("temporary files remain: %v", leftovers)
	}
	if err := TrustClaudeDirectory(runtime, "relative/change"); !errors.Is(err, ErrInvalid) {
		t.Fatalf("relative working directory = %v, want ErrInvalid", err)
	}
	// An empty or whitespace-only file is a home with no login yet, like a
	// missing one, and gets the record.
	for name, content := range map[string]string{"empty": "", "whitespace": " \n\t\n"} {
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := TrustClaudeDirectory(runtime, "/private/change"); err != nil {
			t.Fatalf("%s file: %v", name, err)
		}
		if recorded, err := os.ReadFile(path); err != nil || !bytes.Contains(recorded, []byte(`"/private/change":{"hasTrustDialogAccepted":true}`)) {
			t.Fatalf("%s file record = %s, %v", name, recorded, err)
		}
	}
	// A file that is not JSON, or whose projects are not the CLI's shape, is
	// refused without its path in the error rather than rewritten.
	for name, content := range map[string]string{"not JSON": "{", "null": "null", "projects not an object": `{"projects":[]}`, "project not an object": `{"projects":{"/private/change":true}}`} {
		if err := os.WriteFile(path, []byte(content), 0o600); err != nil {
			t.Fatal(err)
		}
		if err := TrustClaudeDirectory(runtime, "/private/change"); !errors.Is(err, errClaudeConfiguration) || strings.Contains(err.Error(), accountHome) {
			t.Fatalf("%s = %v", name, err)
		}
		if kept, err := os.ReadFile(path); err != nil || string(kept) != content {
			t.Fatalf("%s was rewritten: %s, %v", name, kept, err)
		}
	}
}

func TestCodexOverseerDiscoversScopedControlsWithoutChangingWorkerTask(t *testing.T) {
	installation, runtime, _ := nativeFixture(t, kernel.ProviderCodex)
	request := requestFor(t, kernel.ProviderCodex, installation, runtime, "gpt-5.6-sol", "high")
	worker, err := Build(request)
	if err != nil {
		t.Fatal(err)
	}
	request.role = kernel.RoleOrchestrator
	overseer, err := Build(request)
	if err != nil {
		t.Fatal(err)
	}
	workerArgs, overseerArgs := worker.Argv(), overseer.Argv()
	if strings.Contains(workerArgs[len(workerArgs)-1], "overseer status") {
		t.Fatal("worker was given overseer authority instructions")
	}
	prompt := overseerArgs[len(overseerArgs)-1]
	for _, command := range []string{"attempt task", "overseer status", "worker interrupt", "worker replace", "Maintainer App"} {
		if !strings.Contains(prompt, command) {
			t.Fatalf("overseer cannot discover %q", command)
		}
	}
	if overseer.TaskDelivery() != TaskDeliveryAttemptAPI {
		t.Fatal("overseer stopped reading the exact durable task")
	}
}
