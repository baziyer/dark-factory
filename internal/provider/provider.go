package provider

import (
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const (
	shellPath            = "/bin/sh"
	claudeTool           = "claude"
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
	for _, directory := range filepath.SplitList(toolPath) {
		candidate := filepath.Join(directory, tool)
		if _, err := os.Lstat(candidate); os.IsNotExist(err) {
			continue
		} else if err != nil {
			return Installation{}, unavailable(kind)
		}
		resolved, err := filepath.EvalSymlinks(candidate)
		if err != nil || !validAbsolute(resolved, maxPathBytes) {
			return Installation{}, unavailable(kind)
		}
		executable, err := runner.CommitExecutableLocator(resolved)
		if err != nil {
			return Installation{}, unavailable(kind)
		}
		return Installation{provider: kind, executable: executable}, nil
	}
	return Installation{}, unavailable(kind)
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
}

func NewRequest(kind kernel.Provider, installation Installation, model, reasoningEffort string, runtime RuntimePaths, workingDirectory string) (Request, error) {
	if kernel.ValidateProviderLaunchControls(kind, model, reasoningEffort) != nil || installation.provider != kind || installation.executable.Path() == "" || !runtime.valid() ||
		kind == kernel.ProviderCodex && (!validAbsolute(workingDirectory, maxPathBytes) || len(codexUntrustedProjectConfig(workingDirectory)) > runner.MaxArgumentBytes) {
		return Request{}, ErrInvalid
	}
	return Request{
		provider: kind, installation: installation,
		model: model, reasoningEffort: reasoningEffort, runtime: runtime, workingDirectory: workingDirectory,
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

// ClaudeConfigFile is the .claude.json a Claude Code launch reads, and the one
// place that rule is spelled: the launch environment names CLAUDE_CONFIG_DIR
// only when this file is not the account home's own, and the daemon's account
// discovery reads a login's identity from the same file. The default
// directory's file is beside it in the home, because that directory holds
// only local flags; a directory made with CLAUDE_CONFIG_DIR set carries its
// own.
func ClaudeConfigFile(accountHome, accountConfig string) string {
	if accountConfig != "" && accountConfig != ConfigHome(kernel.ProviderClaudeCode, accountHome) {
		return filepath.Join(accountConfig, ".claude.json")
	}
	return filepath.Join(accountHome, ".claude.json")
}

// errClaudeConfiguration names a failure around the account's own file
// without carrying its path into a run's durable failure detail.
var errClaudeConfiguration = errors.New("provider: claude configuration")

const maxClaudeConfigBytes = 16 << 20

// TrustClaudeDirectory records cwd as trusted in the account's Claude Code
// configuration, which is what answering the CLI's folder-trust dialog does.
// Every Change is a path the CLI has never seen, so without this record the
// interactive session stops at that dialog and the startup task is typed into
// it. Only this one key is added; every other value in the file is kept, with
// numbers as their own digits and strings unescaped, and a file whose shape
// is not the CLI's is refused rather than rewritten.
// ponytail: a read-modify-write like the CLI's own sessions do on the same
// file, published by rename so a reader never sees a torn file; a concurrent
// writer's key can still be lost between read and rename. Take a lock file
// if a lost update is ever observed. Every Change adds one entry that nothing
// removes; past maxClaudeConfigBytes every Claude launch on the account is
// refused. Prune entries whose directory is gone if that ceiling nears.
func TrustClaudeDirectory(runtime RuntimePaths, cwd string) error {
	if !runtime.valid() || !validAbsolute(cwd, maxPathBytes) {
		return ErrInvalid
	}
	path := ClaudeConfigFile(runtime.accountHome, runtime.accountConfig)
	config := map[string]any{}
	if raw, err := readClaudeConfig(path); err != nil {
		return err
	} else if len(bytes.TrimSpace(raw)) > 0 {
		decoder := json.NewDecoder(bytes.NewReader(raw))
		decoder.UseNumber()
		if err := decoder.Decode(&config); err != nil {
			return errClaudeConfiguration
		}
	}
	projects, ok := config["projects"].(map[string]any)
	if !ok {
		if _, present := config["projects"]; present {
			return errClaudeConfiguration
		}
		projects = map[string]any{}
	}
	project, ok := projects[cwd].(map[string]any)
	if !ok {
		if _, present := projects[cwd]; present {
			return errClaudeConfiguration
		}
		project = map[string]any{}
	}
	if project["hasTrustDialogAccepted"] == true {
		return nil
	}
	project["hasTrustDialogAccepted"] = true
	projects[cwd] = project
	config["projects"] = projects
	var encoded bytes.Buffer
	encoder := json.NewEncoder(&encoded)
	encoder.SetEscapeHTML(false)
	if err := encoder.Encode(config); err != nil {
		return errClaudeConfiguration
	}
	temp, err := os.CreateTemp(filepath.Dir(path), ".claude.json.*")
	if err != nil {
		return errClaudeConfiguration
	}
	_, writeErr := temp.Write(encoded.Bytes())
	if err := errors.Join(writeErr, temp.Sync(), temp.Close()); err != nil || os.Rename(temp.Name(), path) != nil {
		_ = os.Remove(temp.Name())
		return errClaudeConfiguration
	}
	return nil
}

// readClaudeConfig returns the file's bytes, none when there is no file yet
// or an empty one, and refuses one past the bound rather than decoding it.
func readClaudeConfig(path string) ([]byte, error) {
	file, err := os.Open(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, errClaudeConfiguration
	}
	defer file.Close()
	raw, err := io.ReadAll(io.LimitReader(file, maxClaudeConfigBytes+1))
	if err != nil || len(raw) > maxClaudeConfigBytes {
		return nil, errClaudeConfiguration
	}
	return raw, nil
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
		if configDir := filepath.Dir(ClaudeConfigFile(runtime.accountHome, runtime.accountConfig)); configDir != runtime.accountHome {
			environment = append(environment, "CLAUDE_CONFIG_DIR="+configDir)
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
