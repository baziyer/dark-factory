package provider

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const (
	shellPath  = "/bin/sh"
	claudeTool = "claude"
	// maintainerBridge is the Maintainer App's MCP bridge. An orchestrator
	// launch names it to Claude, which is how an overseer publishes: the
	// daemon itself exposes no repository or publication operation.
	maintainerBridge     = "dark-factory-maintainer-mcp-bridge"
	codexTool            = "codex"
	maxPathBytes         = 4096
	maxClaudePrompt      = 8 << 10
	maxCodexTask         = 8 << 10
	claudeConfigDir      = ".claude"
	codexConfigDir       = ".codex"
	claudeTaskLead       = "Complete this Dark Factory task. Before exiting, report the durable outcome with $DARK_FACTORY_FACTORYCTL attempt succeed, block, or fail. Task: "
	codexBootstrapPrompt = "Run \"$DARK_FACTORY_FACTORYCTL\" attempt task before doing anything else. The returned JSON task field is the exact task: complete only that task. Before exiting, report the durable outcome with \"$DARK_FACTORY_FACTORYCTL\" attempt succeed, block, or fail."
)

var (
	ErrInvalid     = errors.New("provider: invalid launch contract")
	ErrUnavailable = errors.New("provider: unavailable")
)

// Installation binds one provider kind to one exact executable commitment.
type Installation struct {
	provider   kernel.Provider
	executable runner.ExecutableCommitment
}

// ResolveInstallation selects one executable from the daemon's fixed tool
// path. Native tool locators may be symlinks, but the returned commitment is
// always to the resolved direct Mach-O target; no symlink is trusted again at
// exec. An invalid existing candidate fails closed instead of falling through
// to a different executable with the same name.
func ResolveInstallation(kind kernel.Provider, toolPath string) (Installation, error) {
	if !validToolPath(toolPath) {
		return Installation{}, ErrInvalid
	}
	if kind == kernel.ProviderShell {
		executable, err := runner.CommitExecutableLocator(shellPath)
		if err != nil {
			return Installation{}, unavailable(kind)
		}
		return Installation{provider: kind, executable: executable}, nil
	}
	var tool string
	switch kind {
	case kernel.ProviderClaudeCode:
		tool = claudeTool
	case kernel.ProviderCodex:
		tool = codexTool
	default:
		return Installation{}, ErrInvalid
	}
	executable, err := resolveTool(toolPath, tool)
	if err != nil {
		return Installation{}, unavailable(kind)
	}
	return Installation{provider: kind, executable: executable}, nil
}

// resolveBridge finds the Maintainer bridge on the fixed tool path. It is
// not committed like a CLI: Claude spawns it much later and it may be a
// script, which the executable commitment refuses. It must be a regular
// executable file that nobody but its owner can write.
func resolveBridge(toolPath string) (string, error) {
	for _, directory := range filepath.SplitList(toolPath) {
		candidate := filepath.Join(directory, maintainerBridge)
		if _, err := os.Lstat(candidate); os.IsNotExist(err) {
			continue
		} else if err != nil {
			return "", ErrUnavailable
		}
		resolved, err := filepath.EvalSymlinks(candidate)
		if err != nil || !validAbsolute(resolved, maxPathBytes) {
			return "", ErrUnavailable
		}
		info, err := os.Stat(resolved)
		if err != nil || !info.Mode().IsRegular() || info.Mode().Perm()&0o100 == 0 || info.Mode().Perm()&0o022 != 0 {
			return "", ErrUnavailable
		}
		return resolved, nil
	}
	return "", ErrUnavailable
}

// resolveTool commits the first tool of that name on the fixed tool path.
func resolveTool(toolPath, tool string) (runner.ExecutableCommitment, error) {
	for _, directory := range filepath.SplitList(toolPath) {
		candidate := filepath.Join(directory, tool)
		if _, err := os.Lstat(candidate); os.IsNotExist(err) {
			continue
		} else if err != nil {
			return runner.ExecutableCommitment{}, ErrUnavailable
		}
		resolved, err := filepath.EvalSymlinks(candidate)
		if err != nil || !validAbsolute(resolved, maxPathBytes) {
			return runner.ExecutableCommitment{}, ErrUnavailable
		}
		executable, err := runner.CommitExecutableLocator(resolved)
		if err != nil {
			return runner.ExecutableCommitment{}, ErrUnavailable
		}
		return executable, nil
	}
	return runner.ExecutableCommitment{}, ErrUnavailable
}

// ConfigDirName is the directory a provider CLI keeps its login and
// configuration in, under an account home. Shell keeps none, so it answers
// empty. This is the one definition of those names: the launch environment
// below and the daemon's account discovery and default reader all derive
// their paths from it, so a launch and a reading of it cannot disagree.
func ConfigDirName(kind kernel.Provider) string {
	switch kind {
	case kernel.ProviderClaudeCode:
		return claudeConfigDir
	case kernel.ProviderCodex:
		return codexConfigDir
	}
	return ""
}

// ConfigHome is the directory a provider CLI reads its configuration from
// under one account home. It is the one place that join is spelled: the launch
// environment, the daemon's default reader and its account discovery all ask
// here, so none of them can name a different directory than a run uses.
func ConfigHome(kind kernel.Provider, accountHome string) string {
	name := ConfigDirName(kind)
	if accountHome == "" || name == "" {
		return ""
	}
	return filepath.Join(accountHome, name)
}

func (Installation) String() string   { return "provider installation (private)" }
func (Installation) GoString() string { return "provider.Installation{private}" }

// RuntimePaths is the complete set of external strings permitted to enter the
// provider environment. Construction is lexical only; the daemon-owned Change
// worker must retain and revalidate the exact filesystem/executable
// capabilities that make these paths true immediately around Build and exec.
// This value is never authority by itself.
type RuntimePaths struct {
	home, temp, socket, token, factoryctl, gitCeiling, toolPath, accountHome string
	// accountConfig is one linked provider login's own configuration
	// directory. Empty means the provider's default, which is what every
	// launch used before accounts existed.
	accountConfig string
}

func NewRuntimePaths(home, temp, socket, token, factoryctl, gitCeiling, toolPath, accountHome, accountConfig string) (RuntimePaths, error) {
	runtime := RuntimePaths{
		home: home, temp: temp, socket: socket, token: token,
		factoryctl: factoryctl, gitCeiling: gitCeiling, toolPath: toolPath, accountHome: accountHome,
		accountConfig: accountConfig,
	}
	if !runtime.valid() {
		return RuntimePaths{}, ErrInvalid
	}
	return runtime, nil
}

func (RuntimePaths) String() string   { return "provider runtime paths (private)" }
func (RuntimePaths) GoString() string { return "provider.RuntimePaths{private}" }

type Request struct {
	provider         kernel.Provider
	installation     Installation
	model            string
	reasoningEffort  string
	runtime          RuntimePaths
	workingDirectory string
	role             kernel.AgentRole
}

func NewRequest(kind kernel.Provider, installation Installation, model, reasoningEffort string, runtime RuntimePaths, workingDirectory string, role kernel.AgentRole) (Request, error) {
	if kernel.ValidateProviderLaunchControls(kind, model, reasoningEffort) != nil || installation.provider != kind || installation.executable.Path() == "" || !runtime.valid() || role.String() == "" ||
		kind == kernel.ProviderCodex && (!validAbsolute(workingDirectory, maxPathBytes) || len(codexUntrustedProjectConfig(workingDirectory)) > runner.MaxArgumentBytes) {
		return Request{}, ErrInvalid
	}
	return Request{
		provider: kind, installation: installation,
		model: model, reasoningEffort: reasoningEffort, runtime: runtime, workingDirectory: workingDirectory, role: role,
	}, nil
}

func (Request) String() string   { return "provider build request (private)" }
func (Request) GoString() string { return "provider.Request{private}" }

// Launch is the entire provider-owned result. It deliberately has no cwd,
// task input, descriptors, callbacks, process controls, or output decoder.
type Launch struct {
	executable   runner.ExecutableCommitment
	argv         []string
	environment  []string
	taskDelivery TaskDelivery
}

func (launch Launch) Executable() runner.ExecutableCommitment { return launch.executable }
func (launch Launch) Argv() []string                          { return append([]string(nil), launch.argv...) }
func (launch Launch) Environment() []string                   { return append([]string(nil), launch.environment...) }
func (launch Launch) TaskDelivery() TaskDelivery              { return launch.taskDelivery }

func (Launch) String() string   { return "provider launch (private)" }
func (Launch) GoString() string { return "provider.Launch{private}" }

// TaskDelivery is the one task-input channel selected with a provider launch.
// Shell reads its program from the inherited sealed descriptor. Claude receives
// its prompt once through the PTY. Codex starts from a fixed positional prompt
// and reads the exact task through its attempt-scoped local API capability.
type TaskDelivery uint8

const (
	TaskDeliveryFD11 TaskDelivery = iota + 1
	TaskDeliveryStartupTerminal
	TaskDeliveryAttemptAPI
)

// Build is the one closed provider-selection switch.
func Build(request Request) (Launch, error) {
	if err := request.installation.executable.Verify(); err != nil {
		return Launch{}, errors.Join(ErrUnavailable, err)
	}
	path := request.installation.executable.Path()
	switch request.provider {
	case kernel.ProviderShell:
		if request.model != "" || request.reasoningEffort != "" || path != shellPath {
			return Launch{}, ErrInvalid
		}
		return Launch{
			executable:   request.installation.executable,
			argv:         []string{shellPath, runner.ProviderTaskPath},
			environment:  request.runtime.environment(request.provider),
			taskDelivery: TaskDeliveryFD11,
		}, nil
	case kernel.ProviderClaudeCode:
		argv := []string{path, "--dangerously-skip-permissions"}
		if request.model != "" {
			argv = append(argv, "--model", request.model)
		}
		if request.reasoningEffort != "" {
			argv = append(argv, "--effort", request.reasoningEffort)
		}
		// Only the servers named here reach the session: none for a worker,
		// so neither the account's configuration nor a .mcp.json in the
		// Change can add one; the Maintainer App alone for an orchestrator,
		// which publishes through it. Without the bridge an orchestrator
		// cannot do its job, so the launch is refused rather than started
		// blind.
		argv = append(argv, "--strict-mcp-config")
		if request.role == kernel.RoleOrchestrator {
			bridge, err := resolveBridge(request.runtime.toolPath)
			if err != nil {
				return Launch{}, errors.Join(ErrUnavailable, fmt.Errorf("%s: %s", maintainerBridge, request.runtime.toolPath))
			}
			config, err := json.Marshal(map[string]any{"mcpServers": map[string]any{"maintainer": map[string]string{"command": bridge}}})
			if err != nil || len(config) > runner.MaxArgumentBytes {
				return Launch{}, ErrInvalid
			}
			argv = append(argv, "--mcp-config", string(config))
		}
		return Launch{
			executable: request.installation.executable, argv: argv,
			environment: request.runtime.environment(request.provider), taskDelivery: TaskDeliveryStartupTerminal,
		}, nil
	case kernel.ProviderCodex:
		argv := []string{path, "--dangerously-bypass-approvals-and-sandbox", "--no-alt-screen", "-c", "check_for_update_on_startup=false", "-c", "tool_output_token_limit=32768", "-c", codexUntrustedProjectConfig(request.workingDirectory)}
		if request.model != "" {
			argv = append(argv, "--model", request.model)
		}
		if request.reasoningEffort != "" {
			argv = append(argv, "-c", fmt.Sprintf("model_reasoning_effort=%q", request.reasoningEffort))
		}
		argv = append(argv, codexBootstrapPrompt)
		return Launch{
			executable: request.installation.executable, argv: argv,
			environment: request.runtime.environment(request.provider), taskDelivery: TaskDeliveryAttemptAPI,
		}, nil
	default:
		return Launch{}, ErrInvalid
	}
}

func codexUntrustedProjectConfig(path string) string {
	return "projects={" + tomlBasicString(path) + "={trust_level=\"untrusted\"}}"
}

func tomlBasicString(value string) string {
	encoded, err := json.Marshal(value)
	if err != nil {
		// Strings cannot make json.Marshal fail; the path has already passed
		// validValue, which also excludes invalid UTF-8.
		panic("provider: failed to encode valid TOML path")
	}
	// TOML basic strings exclude C0 controls, DEL, and C1 controls. JSON
	// escapes C0 but permits the latter two ranges literally.
	encodedString := string(encoded)
	for character := rune(0x7f); character <= 0x9f; character++ {
		encodedString = strings.ReplaceAll(encodedString, string(character), fmt.Sprintf(`\u%04x`, character))
	}
	return encodedString
}

// PrepareTask is also available before executable selection so the daemon can
// freeze descriptor or terminal input where required. Codex task bytes remain
// in the daemon and are retrieved through the attempt-scoped API. Build returns
// the same closed delivery value, which the Change worker must compare before
// exec.
func PrepareTask(kind kernel.Provider, task []byte) (TaskDelivery, []byte, error) {
	if len(task) == 0 || len(task) > runner.MaxProviderTaskBytes || !utf8.Valid(task) || bytes.IndexByte(task, 0) >= 0 {
		return 0, nil, ErrInvalid
	}
	switch kind {
	case kernel.ProviderShell:
		return TaskDeliveryFD11, bytes.Clone(task), nil
	case kernel.ProviderClaudeCode:
		encoded, err := claudeTaskInput(task)
		if err != nil {
			return 0, nil, ErrInvalid
		}
		return TaskDeliveryStartupTerminal, encoded, nil
	case kernel.ProviderCodex:
		// Codex reads this value through a shell-tool result. Keep the exact task
		// comfortably below the model-visible result bound even after JSON turns
		// every DEL/C1 code point into a six-byte escape.
		if len(task) > maxCodexTask {
			return 0, nil, ErrInvalid
		}
		return TaskDeliveryAttemptAPI, nil, nil
	default:
		return 0, nil, ErrInvalid
	}
}

func unavailable(kind kernel.Provider) error {
	return fmt.Errorf("%w: %s", ErrUnavailable, kind.String())
}

func claudeTaskInput(task []byte) ([]byte, error) {
	quoted, err := json.Marshal(string(task))
	if err != nil {
		return nil, ErrInvalid
	}
	payload := make([]byte, 0, len(claudeTaskLead)+len(quoted)+1)
	payload = append(payload, claudeTaskLead...)
	payload = appendTerminalSafeJSON(payload, quoted)
	payload = append(payload, '\r')
	if len(payload) > maxClaudePrompt {
		return nil, ErrInvalid
	}
	return payload, nil
}

func appendTerminalSafeJSON(dst, quoted []byte) []byte {
	const hex = "0123456789abcdef"
	for len(quoted) > 0 {
		value, width := utf8.DecodeRune(quoted)
		if value >= 0x7f && value <= 0x9f {
			dst = append(dst, '\\', 'u', '0', '0', hex[value>>4], hex[value&0xf])
		} else {
			dst = append(dst, quoted[:width]...)
		}
		quoted = quoted[width:]
	}
	return dst
}

func (runtime RuntimePaths) valid() bool {
	paths := []string{runtime.home, runtime.temp, runtime.socket, runtime.token, runtime.factoryctl}
	for _, path := range paths {
		if !validAbsolute(path, maxPathBytes) {
			return false
		}
	}
	return len(runtime.socket) <= install.MaxSocketPathBytes && runtime.home != runtime.temp &&
		validGitCeiling(runtime.gitCeiling) && validToolPath(runtime.toolPath) &&
		validAbsolute(runtime.accountHome, maxPathBytes-len("/"+codexConfigDir)) &&
		runtime.accountHome != runtime.home && runtime.accountHome != runtime.temp &&
		(runtime.accountConfig == "" || validAbsolute(runtime.accountConfig, maxPathBytes))
}

func (runtime RuntimePaths) environment(kind kernel.Provider) []string {
	home := runtime.home
	if kind == kernel.ProviderClaudeCode {
		home = runtime.accountHome
	}
	environment := []string{
		"DARK_FACTORY_SOCKET=" + runtime.socket,
		"DARK_FACTORY_ATTEMPT_TOKEN_FILE=" + runtime.token,
		"DARK_FACTORY_FACTORYCTL=" + runtime.factoryctl,
		"HOME=" + home,
		"TMPDIR=" + runtime.temp,
		"PATH=" + runtime.toolPath,
	}
	// A run whose agent selects an account points that CLI at the account's
	// own configuration directory. No account leaves the environment exactly
	// as it was.
	switch kind {
	case kernel.ProviderCodex:
		codexHome := ConfigHome(kind, runtime.accountHome)
		if runtime.accountConfig != "" {
			codexHome = runtime.accountConfig
		}
		environment = append(environment, "CODEX_HOME="+codexHome)
	case kernel.ProviderClaudeCode:
		// Only a directory beside the default one is named. The default is
		// what the CLI already reaches through HOME, and its OAuth account
		// lives in $HOME/.claude.json rather than inside it, so naming it
		// would point the CLI at the flags-only file it does contain and
		// launch the run with no login at all.
		if runtime.accountConfig != "" && runtime.accountConfig != ConfigHome(kind, runtime.accountHome) {
			environment = append(environment, "CLAUDE_CONFIG_DIR="+runtime.accountConfig)
		}
	}
	return append(environment,
		"LANG=C",
		"LC_ALL=C",
		"TERM=xterm-256color",
		"SHELL=/bin/sh",
		"GIT_CEILING_DIRECTORIES="+runtime.gitCeiling,
		"GIT_DISCOVERY_ACROSS_FILESYSTEM=0",
		"GIT_CONFIG_NOSYSTEM=1",
		"GIT_CONFIG_GLOBAL=/dev/null",
		"GIT_TERMINAL_PROMPT=0",
		"GIT_ASKPASS=/usr/bin/false",
		"GIT_SSH_COMMAND=/usr/bin/false",
		"GH_CONFIG_DIR=/dev/null",
	)
}

func validAbsolute(value string, limit int) bool {
	return validValue(value, limit) && filepath.IsAbs(value) && filepath.Clean(value) == value && value != string(filepath.Separator)
}

func validToolPath(value string) bool {
	if !validValue(value, runner.MaxEnvironmentEntryBytes-len("PATH=")) {
		return false
	}
	seen := make(map[string]struct{})
	for _, component := range filepath.SplitList(value) {
		if !validAbsolute(component, maxPathBytes) {
			return false
		}
		if _, duplicate := seen[component]; duplicate {
			return false
		}
		seen[component] = struct{}{}
	}
	return len(seen) > 0
}

// ValidateToolPath lets the daemon freeze one bounded startup-owned PATH and
// lets the worker codec reject drift without reimplementing its grammar.
func ValidateToolPath(value string) error {
	if !validToolPath(value) {
		return ErrInvalid
	}
	return nil
}

func validGitCeiling(value string) bool {
	return validAbsolute(value, maxPathBytes) && !strings.ContainsRune(value, rune(filepath.ListSeparator))
}

func validValue(value string, limit int) bool {
	return value != "" && len(value) <= limit && utf8.ValidString(value) && strings.IndexByte(value, 0) < 0
}
