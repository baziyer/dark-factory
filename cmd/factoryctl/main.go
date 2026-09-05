package main

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/buildinfo"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const (
	attemptRequestTimeout = 5 * time.Second
	serviceRequestTimeout = 30 * time.Second
	exitUsage             = 64
	exitFailure           = 1
	maxHomeArgumentBytes  = 4096

	// pairListenAddress is factoryd's fixed loopback listener and pairPageURL
	// the first-party pair page it serves there. A successful install opens
	// that page, so pairing a browser never needs a terminal. launchd returns
	// from bootstrap before factoryd listens, hence the bounded wait.
	pairListenAddress  = "127.0.0.1:43123"
	pairPageURL        = "http://" + pairListenAddress + "/pair"
	pairListenPatience = 10 * time.Second

	usage = `usage:
  factoryctl attempt task
  factoryctl attempt succeed [--result TEXT]
  factoryctl attempt block --detail TEXT
  factoryctl attempt fail [--detail TEXT]
  factoryctl attempt request-human --idempotency-key HEX32 --question TEXT
  factoryctl project create --name TEXT --root ABSOLUTE
  factoryctl agent create --project ID --name TEXT --provider shell|claude_code|codex --tool-budget N [--role worker|orchestrator] [--model TEXT] [--reasoning-effort low|medium|high|xhigh|max|ultra]
  factoryctl task add --project ID --agent ID --title TEXT [--body TEXT] [--priority N]
  factoryctl dispatch on|off
  factoryctl web status
  factoryctl web list-clients [--after CLIENT_ID]
  factoryctl web revoke CLIENT_ID --revision REVISION
  factoryctl remote status
  factoryctl init --home ABSOLUTE
  factoryctl doctor --home ABSOLUTE
  factoryctl service status --home ABSOLUTE [--label LABEL] [--plist-dir ABSOLUTE]
  factoryctl service install --home ABSOLUTE [--label LABEL] [--plist-dir ABSOLUTE] [--relay-origin WSS_ORIGIN]
  factoryctl service start --home ABSOLUTE [--label LABEL] [--plist-dir ABSOLUTE]
  factoryctl service stop --home ABSOLUTE [--label LABEL] [--plist-dir ABSOLUTE]
  factoryctl service uninstall --home ABSOLUTE [--label LABEL] [--plist-dir ABSOLUTE]
  factoryctl --version
  factoryctl --build-identity
`
)

type commandKind uint8

const (
	commandSucceed commandKind = iota + 1
	commandBlock
	commandFail
	commandRequestHuman
	commandAttemptTask
	commandWebStatus
	commandWebListClients
	commandWebRevoke
	commandRemoteStatus
	commandInit
	commandDoctor
	commandServiceStatus
	commandServiceInstall
	commandServiceStart
	commandServiceStop
	commandServiceUninstall
	commandProjectCreate
	commandAgentCreate
	commandTaskAdd
	commandDispatch
)

type attemptCommand struct {
	kind             commandKind
	home             string
	idempotencyKey   string
	text             string
	id               string
	after            string
	expectedRevision uint64

	label           string
	plistDir        string
	relayOrigin     string
	name            string
	root            string
	project         string
	agent           string
	role            string
	provider        string
	model           string
	reasoningEffort string
	title           string
	body            string
	toolBudget      uint64
	priority        int64
	enabled         bool
}

func main() {
	os.Exit(run(context.Background(), os.Args[1:], os.Getenv, os.Stdout, os.Stderr))
}

func run(ctx context.Context, args []string, getenv func(string) string, stdout, stderr io.Writer) int {
	return runWithOpener(ctx, args, getenv, stdout, stderr, openBrowser)
}

type browserOpener func(context.Context, string) error

func runWithOpener(ctx context.Context, args []string, getenv func(string) string, stdout, stderr io.Writer, opener browserOpener) int {
	return runWithDependencies(ctx, args, getenv, stdout, stderr, opener, install.InspectService)
}

type serviceInspector func(context.Context, string) (install.ServiceStatus, error)

func runWithDependencies(ctx context.Context, args []string, getenv func(string) string, stdout, stderr io.Writer, opener browserOpener, inspect serviceInspector) int {
	if len(args) == 1 && args[0] == "--version" {
		_, _ = fmt.Fprintf(stdout, "factoryctl %s\n", buildinfo.Current().Version())
		return 0
	}
	if len(args) == 1 && args[0] == "--build-identity" {
		if err := buildinfo.Current().WriteJSON(stdout); err != nil {
			return exitFailure
		}
		return 0
	}
	command, help, ok := parse(args)
	if help {
		_, _ = io.WriteString(stdout, usage)
		return 0
	}
	if !ok {
		_, _ = io.WriteString(stderr, usage)
		return exitUsage
	}
	if command.kind == commandInit || command.kind == commandDoctor {
		return runHome(ctx, command, stdout, stderr)
	}
	if command.kind == commandServiceStatus || command.kind == commandServiceInstall || command.kind == commandServiceStart || command.kind == commandServiceStop || command.kind == commandServiceUninstall {
		return runService(ctx, command, stdout, stderr, inspect, opener)
	}
	if command.kind == commandWebStatus || command.kind == commandWebListClients || command.kind == commandWebRevoke {
		return runWeb(ctx, command, getenv, stdout, stderr)
	}
	if command.kind == commandRemoteStatus {
		return runRemote(ctx, getenv, stdout, stderr)
	}
	if command.kind == commandProjectCreate || command.kind == commandAgentCreate || command.kind == commandTaskAdd || command.kind == commandDispatch {
		return runOperator(ctx, command, getenv, stdout, stderr)
	}

	socket := getenv("DARK_FACTORY_SOCKET")
	if socket == "" {
		_, _ = io.WriteString(stderr, "factoryctl: attempt client configuration is invalid\n")
		return exitFailure
	}
	client, err := api.NewAttemptClientFromEnvironment(socket)
	if err != nil {
		writeFailure(stderr, command.kind, err)
		return exitFailure
	}

	callContext, cancel := context.WithTimeout(ctx, attemptRequestTimeout)
	defer cancel()
	if command.kind == commandAttemptTask {
		result, taskErr := client.Task(callContext)
		if taskErr != nil {
			writeFailure(stderr, command.kind, taskErr)
			return exitFailure
		}
		return writeJSON(stdout, result)
	}
	var result api.MutationResult
	switch command.kind {
	case commandSucceed:
		result, err = client.Succeed(callContext, command.text)
	case commandBlock:
		result, err = client.Block(callContext, command.text)
	case commandFail:
		result, err = client.Fail(callContext, command.text)
	case commandRequestHuman:
		result, err = client.RequestHuman(callContext, api.HumanQuestionInput{IdempotencyKey: command.idempotencyKey, Question: command.text})
	default:
		err = api.ErrInvalidInput
	}
	if err != nil {
		writeFailure(stderr, command.kind, err)
		return exitFailure
	}
	if command.kind == commandRequestHuman {
		_, _ = fmt.Fprintf(stdout, "human request accepted: head=%d revision=%d\n", result.Head, result.Revision)
	} else {
		_, _ = fmt.Fprintf(stdout, "attempt outcome request accepted: head=%d revision=%d\n", result.Head, result.Revision)
	}
	return 0
}

func parse(args []string) (attemptCommand, bool, bool) {
	if len(args) == 1 && helpFlag(args[0]) {
		return attemptCommand{}, true, true
	}
	if len(args) == 3 && (args[0] == "init" || args[0] == "doctor") && args[1] == "--home" {
		if validHomeArg(args[2]) {
			kind := commandInit
			if args[0] == "doctor" {
				kind = commandDoctor
			}
			return attemptCommand{kind: kind, home: args[2]}, false, true
		}
	}
	if len(args) == 2 && (args[0] == "init" || args[0] == "doctor") && helpFlag(args[1]) {
		return attemptCommand{}, true, true
	}
	if len(args) >= 1 && args[0] == "service" {
		return parseServiceCommand(args)
	}
	if len(args) >= 1 && (args[0] == "project" || args[0] == "agent" || args[0] == "task" || args[0] == "dispatch") {
		return parseOperator(args)
	}
	if len(args) >= 1 && args[0] == "remote" {
		return parseRemote(args)
	}
	if len(args) < 2 || args[0] != "attempt" && args[0] != "web" {
		return attemptCommand{}, false, false
	}
	if len(args) == 2 && helpFlag(args[1]) {
		return attemptCommand{}, true, true
	}
	if len(args) == 3 && helpFlag(args[2]) {
		switch args[1] {
		case "task", "succeed", "block", "fail", "request-human":
			return attemptCommand{}, true, true
		case "status", "list-clients", "revoke":
			if args[0] == "web" {
				return attemptCommand{}, true, true
			}
		default:
			return attemptCommand{}, false, false
		}
	}

	if args[0] == "web" {
		return parseWeb(args)
	}
	switch args[1] {
	case "task":
		if len(args) == 2 {
			return attemptCommand{kind: commandAttemptTask}, false, true
		}
	case "succeed":
		if len(args) == 2 {
			return attemptCommand{kind: commandSucceed}, false, true
		}
		if len(args) == 4 && args[2] == "--result" {
			return attemptCommand{kind: commandSucceed, text: args[3]}, false, true
		}
	case "block":
		if len(args) == 4 && args[2] == "--detail" && args[3] != "" {
			return attemptCommand{kind: commandBlock, text: args[3]}, false, true
		}
	case "fail":
		if len(args) == 2 {
			return attemptCommand{kind: commandFail}, false, true
		}
		if len(args) == 4 && args[2] == "--detail" {
			return attemptCommand{kind: commandFail, text: args[3]}, false, true
		}
	case "request-human":
		if len(args) == 6 && args[2] == "--idempotency-key" && validHumanRequestKey(args[3]) && args[4] == "--question" && validQuestion(args[5]) {
			return attemptCommand{kind: commandRequestHuman, idempotencyKey: args[3], text: args[5]}, false, true
		}
	}
	return attemptCommand{}, false, false
}

func parseServiceCommand(args []string) (attemptCommand, bool, bool) {
	if len(args) == 2 && helpFlag(args[1]) {
		return attemptCommand{}, true, true
	}
	if len(args) >= 3 && helpFlag(args[2]) {
		switch args[1] {
		case "status", "install", "start", "stop", "uninstall":
			return attemptCommand{}, true, true
		}
	}
	if len(args) < 2 {
		return attemptCommand{}, false, false
	}
	command := attemptCommand{}
	switch args[1] {
	case "status":
		command.kind = commandServiceStatus
	case "install":
		command.kind = commandServiceInstall
	case "start":
		command.kind = commandServiceStart
	case "stop":
		command.kind = commandServiceStop
	case "uninstall":
		command.kind = commandServiceUninstall
	default:
		return attemptCommand{}, false, false
	}
	seen := map[string]bool{}
	for index := 2; index < len(args); index += 2 {
		if index+1 >= len(args) {
			return attemptCommand{}, false, false
		}
		name, value := args[index], args[index+1]
		if seen[name] {
			return attemptCommand{}, false, false
		}
		seen[name] = true
		switch name {
		case "--home":
			if !validHomeArg(value) {
				return attemptCommand{}, false, false
			}
			command.home = value
		case "--label":
			if value == "" || len(value) > 127 {
				return attemptCommand{}, false, false
			}
			command.label = value
		case "--plist-dir":
			if !validHomeArg(value) {
				return attemptCommand{}, false, false
			}
			command.plistDir = value
		case "--relay-origin":
			// Only install renders a plist; every other verb recovers the
			// origin from the receipt this install writes.
			if command.kind != commandServiceInstall || !install.ValidRelayOrigin(value) {
				return attemptCommand{}, false, false
			}
			command.relayOrigin = value
		default:
			return attemptCommand{}, false, false
		}
	}
	if command.home == "" {
		return attemptCommand{}, false, false
	}
	return command, false, true
}

func serviceConfigFor(command attemptCommand) install.ServiceConfig {
	config := install.DefaultServiceConfig()
	if command.label != "" {
		config.Label = command.label
	}
	config.PlistDirectory = command.plistDir
	config.RelayOrigin = command.relayOrigin
	return config
}

func runService(ctx context.Context, command attemptCommand, stdout, stderr io.Writer, inspect serviceInspector, opener browserOpener) int {
	callContext, cancel := context.WithTimeout(ctx, serviceRequestTimeout)
	defer cancel()
	config := serviceConfigFor(command)
	var status install.ServiceStatus
	var existing install.ServiceState
	var err error
	switch command.kind {
	case commandServiceStatus:
		if inspect == nil && command.label == "" && command.plistDir == "" {
			_, _ = io.WriteString(stderr, "factoryctl: service status configuration is invalid\n")
			return exitFailure
		}
		status, err = inspectService(callContext, command, config, inspect)
	case commandServiceInstall:
		var self string
		self, err = serviceSourceDirectory()
		if err == nil {
			// The service found before the install decides whether this
			// command started anything: repeating an install returns the
			// service it found, unchanged, and must open no browser.
			if found, inspectErr := inspectService(callContext, command, config, inspect); inspectErr == nil {
				existing = found.State
			}
			status, err = install.ServiceInstall(callContext, command.home, config, self)
		}
	case commandServiceStart:
		status, err = install.ServiceStart(callContext, command.home, config)
	case commandServiceStop:
		status, err = install.ServiceStop(callContext, command.home, config)
	case commandServiceUninstall:
		status, err = install.ServiceUninstall(callContext, command.home, config)
	}
	if err != nil {
		message := "factoryctl: the service operation is ambiguous; inspect the home and launchd state\n"
		switch {
		case errors.Is(err, context.Canceled):
			message = "factoryctl: service operation canceled\n"
		case errors.Is(err, context.DeadlineExceeded):
			message = "factoryctl: service operation timed out\n"
		case errors.Is(err, install.ErrUnsupported):
			message = "factoryctl: service operations are unsupported on this platform\n"
		case errors.Is(err, install.ErrInvalidHome):
			message = "factoryctl: service operations require an exact Go home\n"
		case errors.Is(err, install.ErrServiceRelayOrigin):
			// The only service error printed verbatim: it names the installed
			// origin and the way out, and the engine assembles it from its own
			// words alone.
			message = "factoryctl: " + err.Error() + "\n"
		case errors.Is(err, install.ErrServiceForeign):
			message = "factoryctl: a service artifact is not this installation's property; refusing\n"
		case errors.Is(err, install.ErrServiceResidue):
			message = "factoryctl: service residue found; run factoryctl service uninstall first\n"
		}
		_, _ = io.WriteString(stderr, message)
		return exitFailure
	}
	// The projection is exact or refused: states outside the finite set, or a
	// pid that disagrees with its state, never print as success.
	switch status.State {
	case install.ServiceAbsent, install.ServiceInstalled:
		if status.PID != 0 {
			_, _ = io.WriteString(stderr, "factoryctl: the service projection is ambiguous\n")
			return exitFailure
		}
	case install.ServiceRunning:
		if status.PID <= 0 {
			_, _ = io.WriteString(stderr, "factoryctl: the service projection is ambiguous\n")
			return exitFailure
		}
	default:
		_, _ = io.WriteString(stderr, "factoryctl: the service projection is ambiguous\n")
		return exitFailure
	}
	if command.kind == commandServiceInstall && pairPageOpens(existing, status.State) {
		// The one command whose result is more than the projection. Every word
		// of it goes in the JSON on stdout: this output is parsed, and a stray
		// stderr line would be merged into it by any caller reading both.
		return writeJSON(stdout, struct {
			install.ServiceStatus
			PairPage      string `json:"pair_page,omitempty"`
			BrowserOpened bool   `json:"browser_opened"`
		}{ServiceStatus: status, PairPage: pairPageURL, BrowserOpened: openPairPage(ctx, pairListenAddress, pairPageURL, opener)})
	}
	return writeJSON(stdout, status)
}

// inspectService is the read-only projection status and install share: the
// injected exact inspector for the default label and plist directory, and the
// explicit-config inspector otherwise.
func inspectService(ctx context.Context, command attemptCommand, config install.ServiceConfig, inspect serviceInspector) (install.ServiceStatus, error) {
	if inspect != nil && command.label == "" && command.plistDir == "" {
		return inspect(ctx, command.home)
	}
	return install.InspectServiceWithConfig(ctx, command.home, config)
}

// pairPageOpens is true only for an install that actually started the service.
func pairPageOpens(existing, resulting install.ServiceState) bool {
	return resulting == install.ServiceRunning && existing != install.ServiceInstalled && existing != install.ServiceRunning
}

// openPairPage waits, bounded, for factoryd to accept on its loopback listener
// and then opens the pair page exactly once, reporting whether it did. A
// listener that never appears or an opener that fails is not an install
// failure: the caller names the page in its own output and exits 0.
func openPairPage(ctx context.Context, address, page string, opener browserOpener) bool {
	return opener != nil && listenerAccepts(ctx, address) && opener(ctx, page) == nil
}

func listenerAccepts(ctx context.Context, address string) bool {
	deadline := time.Now().Add(pairListenPatience)
	for {
		connection, err := net.DialTimeout("tcp", address, time.Second)
		if err == nil {
			_ = connection.Close()
			return true
		}
		if ctx.Err() != nil || !time.Now().Before(deadline) {
			return false
		}
		time.Sleep(100 * time.Millisecond)
	}
}

// serviceSourceDirectory is the invoking factoryctl's own resolved directory:
// the managed installation installs exactly the sibling binaries it shipped
// with, never a path the operator did not run.
func serviceSourceDirectory() (string, error) {
	self, err := os.Executable()
	if err != nil {
		return "", err
	}
	resolved, err := filepath.EvalSymlinks(self)
	if err != nil {
		return "", err
	}
	return filepath.Dir(resolved), nil
}

func validHomeArg(value string) bool {
	return value != "" && len(value) <= maxHomeArgumentBytes && filepath.IsAbs(value) && filepath.Clean(value) == value && filepath.Base(value) != "." && filepath.Base(value) != string(filepath.Separator)
}

func runHome(ctx context.Context, command attemptCommand, stdout, stderr io.Writer) int {
	var result install.Result
	var err error
	if command.kind == commandInit {
		result, err = install.Init(ctx, command.home)
	} else {
		result, err = install.Doctor(ctx, command.home)
	}
	if err != nil {
		switch {
		case errors.Is(err, install.ErrUncertain):
			message := "factoryctl: home publication outcome is uncertain; inspect the explicit home path before retrying\n"
			if command.kind == commandDoctor {
				message = "factoryctl: home inspection outcome is uncertain; inspect the explicit home path again\n"
			}
			_, _ = io.WriteString(stderr, message)
		case errors.Is(err, install.ErrUnsupported):
			_, _ = io.WriteString(stderr, "factoryctl: Go home operations are unsupported on this platform\n")
		case errors.Is(err, install.ErrInvalidHome):
			_, _ = io.WriteString(stderr, "factoryctl: home is invalid or not an exact stopped Go home\n")
		default:
			_, _ = io.WriteString(stderr, "factoryctl: home operation failed\n")
		}
		return exitFailure
	}
	if result.State == install.Ready {
		_, _ = io.WriteString(stdout, "home ready\n")
	} else {
		_, _ = io.WriteString(stdout, "home initialized\n")
	}
	return 0
}

func parseWeb(args []string) (attemptCommand, bool, bool) {
	switch args[1] {
	case "status":
		if len(args) == 2 {
			return attemptCommand{kind: commandWebStatus}, false, true
		}
	case "list-clients":
		if len(args) == 2 {
			return attemptCommand{kind: commandWebListClients}, false, true
		}
		if len(args) == 4 && args[2] == "--after" && validBrowserClientID(args[3]) {
			return attemptCommand{kind: commandWebListClients, after: args[3]}, false, true
		}
	case "revoke":
		if len(args) == 5 && validBrowserClientID(args[2]) && args[3] == "--revision" {
			revision, ok := parseRevision(args[4])
			if ok {
				return attemptCommand{kind: commandWebRevoke, id: args[2], expectedRevision: revision}, false, true
			}
		}
	}
	return attemptCommand{}, false, false
}

func parseOperator(args []string) (attemptCommand, bool, bool) {
	if len(args) == 2 && helpFlag(args[1]) {
		return attemptCommand{}, true, true
	}
	if len(args) >= 3 && helpFlag(args[2]) {
		switch args[0] + " " + args[1] {
		case "project create", "agent create", "task add":
			return attemptCommand{}, true, true
		}
	}
	if args[0] == "dispatch" {
		if len(args) == 2 && (args[1] == "on" || args[1] == "off") {
			return attemptCommand{kind: commandDispatch, enabled: args[1] == "on"}, false, true
		}
		return attemptCommand{}, false, false
	}
	if len(args) < 2 {
		return attemptCommand{}, false, false
	}
	command := attemptCommand{role: "worker"}
	switch args[0] + " " + args[1] {
	case "project create":
		command.kind = commandProjectCreate
	case "agent create":
		command.kind = commandAgentCreate
	case "task add":
		command.kind = commandTaskAdd
	default:
		return attemptCommand{}, false, false
	}
	seen := map[string]bool{}
	for index := 2; index < len(args); index += 2 {
		if index+1 >= len(args) {
			return attemptCommand{}, false, false
		}
		name, value := args[index], args[index+1]
		if seen[name] {
			return attemptCommand{}, false, false
		}
		seen[name] = true
		switch {
		case name == "--name" && command.kind != commandTaskAdd && validOperatorText(value, 1, 128):
			command.name = value
		case name == "--root" && command.kind == commandProjectCreate && validHomeArg(value) && validOperatorText(value, 1, 4096):
			command.root = value
		case name == "--project" && command.kind != commandProjectCreate && validHumanRequestKey(value):
			command.project = value
		case name == "--agent" && command.kind == commandTaskAdd && validHumanRequestKey(value):
			command.agent = value
		case name == "--role" && command.kind == commandAgentCreate && (value == "worker" || value == "orchestrator"):
			command.role = value
		case name == "--provider" && command.kind == commandAgentCreate:
			command.provider = value
		case name == "--model" && command.kind == commandAgentCreate && validOperatorText(value, 0, 128):
			command.model = value
		case name == "--reasoning-effort" && command.kind == commandAgentCreate:
			command.reasoningEffort = value
		case name == "--tool-budget" && command.kind == commandAgentCreate:
			budget, err := strconv.ParseUint(value, 10, 64)
			if err != nil || value != strconv.FormatUint(budget, 10) || budget < 1 || budget > 1_000_000_000 {
				return attemptCommand{}, false, false
			}
			command.toolBudget = budget
		case name == "--title" && command.kind == commandTaskAdd && validOperatorText(value, 1, 1024):
			command.title = value
		case name == "--body" && command.kind == commandTaskAdd && validOperatorText(value, 0, 131072):
			command.body = value
		case name == "--priority" && command.kind == commandTaskAdd:
			priority, err := strconv.ParseInt(value, 10, 64)
			if err != nil || value != strconv.FormatInt(priority, 10) || priority < -1_000_000 || priority > 1_000_000 {
				return attemptCommand{}, false, false
			}
			command.priority = priority
		default:
			return attemptCommand{}, false, false
		}
	}
	switch command.kind {
	case commandProjectCreate:
		if command.name == "" || command.root == "" {
			return attemptCommand{}, false, false
		}
	case commandAgentCreate:
		provider, err := kernel.ParseProvider(command.provider)
		if command.project == "" || command.name == "" || command.toolBudget == 0 || err != nil || kernel.ValidateProviderLaunchControls(provider, command.model, command.reasoningEffort) != nil {
			return attemptCommand{}, false, false
		}
	case commandTaskAdd:
		if command.project == "" || command.agent == "" || command.title == "" {
			return attemptCommand{}, false, false
		}
	}
	return command, false, true
}

func validOperatorText(value string, minimum, maximum int) bool {
	return len(value) >= minimum && len(value) <= maximum && utf8.ValidString(value) && !strings.ContainsRune(value, 0)
}

func validBrowserClientID(value string) bool { return validHumanRequestKey(value) }

func parseRevision(value string) (uint64, bool) {
	if value == "" || value[0] == '0' || len(value) > 19 {
		return 0, false
	}
	for _, character := range value {
		if character < '0' || character > '9' {
			return 0, false
		}
	}
	parsed, err := strconv.ParseUint(value, 10, 64)
	return parsed, err == nil && parsed > 0 && parsed <= uint64(^uint64(0)>>1)
}

func helpFlag(value string) bool { return value == "-h" || value == "--help" }

func validHumanRequestKey(value string) bool {
	if len(value) != 32 {
		return false
	}
	nonzero := false
	for _, value := range []byte(value) {
		if value < '0' || value > '9' {
			if value < 'a' || value > 'f' {
				return false
			}
		}
		nonzero = nonzero || value != '0'
	}
	return nonzero
}

func validQuestion(value string) bool {
	return len(value) >= 1 && len(value) <= 8192 && utf8.ValidString(value) && !strings.ContainsRune(value, 0)
}

func writeFailure(stderr io.Writer, kind commandKind, err error) {
	subject, input := "outcome request", "attempt input"
	if kind == commandAttemptTask {
		subject = "task request"
	} else if kind == commandRequestHuman {
		subject, input = "human request", "human request input"
	}
	message := "factoryctl: " + subject + " failed\n"
	var remote *api.RemoteError
	switch {
	case errors.Is(err, api.ErrInvalidClient):
		message = "factoryctl: attempt client configuration is invalid\n"
	case errors.Is(err, api.ErrInvalidInput):
		message = "factoryctl: " + input + " is invalid\n"
	case errors.Is(err, context.Canceled):
		message = "factoryctl: " + subject + " canceled\n"
	case errors.Is(err, context.DeadlineExceeded):
		message = "factoryctl: " + subject + " timed out\n"
	case errors.As(err, &remote):
		message = "factoryctl: " + subject + " was not accepted\n"
	}
	_, _ = io.WriteString(stderr, message)
}

// newOperatorID mints one fresh lowercase 16-byte hex identity client-side,
// exactly the shape the operator API validates. Identities are never derived
// from user text.
func newOperatorID() (string, error) {
	value := make([]byte, 16)
	if _, err := rand.Read(value); err != nil {
		return "", err
	}
	encoded := hex.EncodeToString(value)
	if !validHumanRequestKey(encoded) {
		return "", errors.New("factoryctl: minted identity is invalid")
	}
	return encoded, nil
}

func runOperator(ctx context.Context, command attemptCommand, getenv func(string) string, stdout, stderr io.Writer) int {
	socket := getenv("DARK_FACTORY_SOCKET")
	token := getenv("DARK_FACTORY_OPERATOR_TOKEN_FILE")
	if socket == "" || token == "" {
		_, _ = io.WriteString(stderr, "factoryctl: operator client configuration is invalid\n")
		return exitFailure
	}
	client, err := api.NewOperatorClient(socket, token)
	if err != nil {
		_, _ = io.WriteString(stderr, "factoryctl: operator client configuration is invalid\n")
		return exitFailure
	}
	callContext, cancel := context.WithTimeout(ctx, attemptRequestTimeout)
	defer cancel()
	switch command.kind {
	case commandProjectCreate:
		id, err := newOperatorID()
		if err != nil {
			return writeWebFailure(stderr, "project create", err)
		}
		result, callErr := client.CreateProject(callContext, api.CreateProjectInput{ID: id, Name: command.name, Root: command.root})
		if callErr != nil {
			return writeWebFailure(stderr, "project create", callErr)
		}
		return writeJSON(stdout, struct {
			ID       string `json:"id"`
			Head     uint64 `json:"head"`
			Revision uint64 `json:"revision"`
		}{ID: id, Head: result.Head, Revision: result.Revision})
	case commandAgentCreate:
		id, err := newOperatorID()
		if err != nil {
			return writeWebFailure(stderr, "agent create", err)
		}
		result, callErr := client.CreateAgent(callContext, api.CreateAgentInput{
			ID: id, ProjectID: command.project, Name: command.name, Role: command.role,
			Provider: command.provider, Model: command.model, ReasoningEffort: command.reasoningEffort,
			ToolBudgetLimit: command.toolBudget,
		})
		if callErr != nil {
			return writeWebFailure(stderr, "agent create", callErr)
		}
		return writeJSON(stdout, struct {
			ID       string `json:"id"`
			Head     uint64 `json:"head"`
			Revision uint64 `json:"revision"`
		}{ID: id, Head: result.Head, Revision: result.Revision})
	case commandTaskAdd:
		id, err := newOperatorID()
		if err != nil {
			return writeWebFailure(stderr, "task add", err)
		}
		incarnation, err := newOperatorID()
		if err != nil {
			return writeWebFailure(stderr, "task add", err)
		}
		result, callErr := client.EnqueueTask(callContext, api.EnqueueTaskInput{ID: id, ProjectID: command.project, AssignedAgentID: command.agent, IncarnationID: incarnation, Title: command.title, Body: command.body, Priority: command.priority})
		if callErr != nil {
			return writeWebFailure(stderr, "task add", callErr)
		}
		return writeJSON(stdout, struct {
			ID            string `json:"id"`
			IncarnationID string `json:"incarnation_id"`
			Head          uint64 `json:"head"`
			Revision      uint64 `json:"revision"`
		}{ID: id, IncarnationID: incarnation, Head: result.Head, Revision: result.Revision})
	case commandDispatch:
		snapshot, callErr := client.Snapshot(callContext)
		if callErr != nil {
			return writeWebFailure(stderr, "dispatch", callErr)
		}
		result, callErr := client.SetDispatch(callContext, snapshot.Factory.Revision, command.enabled)
		if callErr != nil {
			return writeWebFailure(stderr, "dispatch", callErr)
		}
		return writeJSON(stdout, struct {
			Enabled  bool   `json:"enabled"`
			Head     uint64 `json:"head"`
			Revision uint64 `json:"revision"`
		}{Enabled: command.enabled, Head: result.Head, Revision: result.Revision})
	default:
		return exitUsage
	}
}

func runWeb(ctx context.Context, command attemptCommand, getenv func(string) string, stdout, stderr io.Writer) int {
	socket := getenv("DARK_FACTORY_SOCKET")
	if socket == "" {
		_, _ = io.WriteString(stderr, "factoryctl: web client configuration is invalid\n")
		return exitFailure
	}
	if command.kind == commandWebStatus {
		if !socketDaemonPresent(socket) {
			return writeJSON(stdout, api.WebStatus{State: "stopped", Ready: false})
		}
	}
	token := getenv("DARK_FACTORY_OPERATOR_TOKEN_FILE")
	if token == "" {
		_, _ = io.WriteString(stderr, "factoryctl: web client configuration is invalid\n")
		return exitFailure
	}
	client, err := api.NewOperatorClient(socket, token)
	if err != nil {
		if command.kind == commandWebStatus && errors.Is(err, api.ErrInvalidClient) {
			return writeJSON(stdout, api.WebStatus{State: "stopped", Ready: false})
		}
		_, _ = io.WriteString(stderr, "factoryctl: web client configuration is invalid\n")
		return exitFailure
	}
	callContext, cancel := context.WithTimeout(ctx, attemptRequestTimeout)
	defer cancel()
	switch command.kind {
	case commandWebStatus:
		result, callErr := client.WebStatus(callContext)
		if callErr != nil {
			if errors.Is(callErr, api.ErrInvalidClient) || errors.Is(callErr, api.ErrTransport) {
				return writeJSON(stdout, api.WebStatus{State: "stopped", Ready: false})
			}
			return writeWebFailure(stderr, "web status", callErr)
		}
		return writeJSON(stdout, result)
	case commandWebListClients:
		result, callErr := client.WebListClients(callContext, command.after)
		if callErr != nil {
			return writeWebFailure(stderr, "web list clients", callErr)
		}
		return writeJSON(stdout, result)
	case commandWebRevoke:
		result, callErr := client.WebRevokeClient(callContext, command.id, command.expectedRevision)
		if callErr != nil {
			return writeWebFailure(stderr, "web revoke", callErr)
		}
		return writeJSON(stdout, result)
	default:
		return exitUsage
	}
}

// socketDaemonPresent distinguishes a missing or stale pathname from a
// daemon that can actually accept a local connection. The probe sends no
// request and is used only by the read-only status command; all other web
// commands still require the authenticated API client.
func socketDaemonPresent(path string) bool {
	info, err := os.Lstat(path)
	if err != nil || info.Mode()&os.ModeSocket == 0 {
		return false
	}
	connection, err := net.DialTimeout("unix", path, 100*time.Millisecond)
	if err != nil {
		return false
	}
	_ = connection.Close()
	return true
}

func writeJSON(stdout io.Writer, value any) int {
	if err := json.NewEncoder(stdout).Encode(value); err != nil {
		return exitFailure
	}
	return 0
}

func writeWebFailure(stderr io.Writer, subject string, err error) int {
	message := "factoryctl: " + subject + " failed\n"
	var remote *api.RemoteError
	switch {
	case errors.Is(err, context.Canceled):
		message = "factoryctl: " + subject + " canceled\n"
	case errors.Is(err, context.DeadlineExceeded):
		message = "factoryctl: " + subject + " timed out\n"
	case errors.As(err, &remote):
		if remote.Code() == api.RemoteCleanupUnresolved {
			message = "factoryctl: web revoke committed but browser cleanup remains unresolved\n"
		} else {
			message = "factoryctl: " + subject + " was not accepted\n"
		}
	}
	_, _ = io.WriteString(stderr, message)
	return exitFailure
}
