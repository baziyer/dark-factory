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
  factoryctl attempt peer status
  factoryctl attempt peer ask --task ID --idempotency-key HEX32 --question TEXT
  factoryctl attempt peer answer --question ID --revision REVISION --idempotency-key HEX32 --answer TEXT
  factoryctl attempt send-back --task ID --note TEXT
  factoryctl overseer status [--task ID] [--offset N --head HEAD] [--text-offset RUNES --head HEAD]
  factoryctl overseer task add --agent ID --title TEXT [--body TEXT] [--priority N] [--task-id ID --incarnation-id ID]
  factoryctl overseer task update --task ID --revision REVISION [--priority N] [--agent ID] [--cancel]
  factoryctl overseer task send-back --task ID --note TEXT
  factoryctl overseer agent pause|resume --agent ID --revision REVISION
  factoryctl overseer worker stop --operation-id ID --task ID --task-revision REVISION --run ID --run-revision REVISION
  factoryctl overseer worker replace --operation-id ID --task ID --task-revision REVISION --run ID --run-revision REVISION --successor-task ID --successor-incarnation ID --instruction TEXT
  factoryctl overseer worker message --operation-id ID --task ID --task-revision REVISION --run ID --run-revision REVISION --message TEXT
  factoryctl overseer worker interrupt --operation-id ID --task ID --task-revision REVISION --run ID --run-revision REVISION
  factoryctl overseer human reply --operation-id ID --request ID --revision REVISION --reply TEXT
  factoryctl project create --name TEXT --root ABSOLUTE
  factoryctl agent create --project ID --name TEXT --provider shell|claude_code|codex --tool-budget N [--role worker|orchestrator] [--model TEXT] [--reasoning-effort low|medium|high|xhigh|max|ultra] [--account ID]
  factoryctl task add --project ID --agent ID --title TEXT [--body TEXT] [--priority N]
  factoryctl task send-back --task ID --note TEXT
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
	commandPeerStatus
	commandPeerAsk
	commandPeerAnswer
	commandSendBack
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
	commandTaskSendBack
	commandDispatch
	commandOverseerStatus
	commandOverseerTaskAdd
	commandOverseerTaskUpdate
	commandOverseerTaskSendBack
	commandOverseerAgentUpdate
	commandOverseerStopWorker
	commandOverseerReplaceWorker
	commandOverseerMessageWorker
	commandOverseerInterruptWorker
	commandOverseerReplyHuman
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
	account         string
	title           string
	body            string
	toolBudget      uint64
	priority        int64
	prioritySet     bool
	offset          uint64
	head            uint64
	textOffset      uint64
	enabled         bool
	operationID     string
	taskRevision    uint64
	runRevision     uint64
	run             string
	paused          bool
	cancel          bool
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
	if command.kind == commandProjectCreate || command.kind == commandAgentCreate || command.kind == commandTaskAdd || command.kind == commandTaskSendBack || command.kind == commandDispatch {
		return runOperator(ctx, command, getenv, stdout, stderr)
	}
	if command.kind >= commandOverseerStatus && command.kind <= commandOverseerReplyHuman {
		return runOverseer(ctx, command, getenv, stdout, stderr)
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

	timeout := attemptRequestTimeout
	if command.kind == commandPeerAsk || command.kind == commandPeerAnswer {
		timeout = serviceRequestTimeout
	}
	callContext, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()
	if command.kind == commandAttemptTask {
		result, taskErr := client.Task(callContext)
		if taskErr != nil {
			writeFailure(stderr, command.kind, taskErr)
			return exitFailure
		}
		return writeJSON(stdout, result)
	}
	if command.kind == commandPeerStatus {
		result, statusErr := client.PeerStatus(callContext)
		if statusErr != nil {
			writeFailure(stderr, command.kind, statusErr)
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
	case commandPeerAsk:
		result, err = client.PeerAsk(callContext, api.PeerQuestionInput{TargetTaskID: command.id, IdempotencyKey: command.idempotencyKey, Question: command.text})
	case commandPeerAnswer:
		result, err = client.PeerAnswer(callContext, api.PeerAnswerInput{QuestionID: command.id, ExpectedRevision: command.expectedRevision, IdempotencyKey: command.idempotencyKey, Answer: command.text})
	case commandSendBack:
		result, err = client.SendBack(callContext, api.SendBackInput{TaskID: command.id, Note: command.text})
	default:
		err = api.ErrInvalidInput
	}
	if err != nil {
		writeFailure(stderr, command.kind, err)
		return exitFailure
	}
	if command.kind == commandRequestHuman {
		_, _ = fmt.Fprintf(stdout, "human request accepted: head=%d revision=%d\n", result.Head, result.Revision)
	} else if command.kind == commandSendBack {
		_, _ = fmt.Fprintf(stdout, "task sent back: head=%d revision=%d\n", result.Head, result.Revision)
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
	if len(args) >= 1 && args[0] == "overseer" {
		if len(args) >= 2 && helpFlag(args[len(args)-1]) {
			return attemptCommand{}, true, true
		}
		return parseOverseer(args)
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
		case "task", "succeed", "block", "fail", "request-human", "send-back", "peer":
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
	case "peer":
		if len(args) == 3 && args[2] == "status" {
			return attemptCommand{kind: commandPeerStatus}, false, true
		}
		if len(args) == 9 && args[2] == "ask" && args[3] == "--task" && validHumanRequestKey(args[4]) && args[5] == "--idempotency-key" && validHumanRequestKey(args[6]) && args[7] == "--question" && validPeerText(args[8]) {
			return attemptCommand{kind: commandPeerAsk, id: args[4], idempotencyKey: args[6], text: args[8]}, false, true
		}
		if len(args) == 11 && args[2] == "answer" && args[3] == "--question" && validHumanRequestKey(args[4]) && args[5] == "--revision" && validRevision(args[6]) && args[7] == "--idempotency-key" && validHumanRequestKey(args[8]) && args[9] == "--answer" && validPeerText(args[10]) {
			revision, _ := strconv.ParseUint(args[6], 10, 64)
			return attemptCommand{kind: commandPeerAnswer, id: args[4], expectedRevision: revision, idempotencyKey: args[8], text: args[10]}, false, true
		}
	case "send-back":
		if len(args) == 6 && args[2] == "--task" && validHumanRequestKey(args[3]) && args[4] == "--note" && validQuestion(args[5]) {
			return attemptCommand{kind: commandSendBack, id: args[3], text: args[5]}, false, true
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
		case "project create", "agent create", "task add", "task send-back":
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
	case "task send-back":
		command.kind = commandTaskSendBack
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
		case name == "--name" && (command.kind == commandProjectCreate || command.kind == commandAgentCreate) && validOperatorText(value, 1, 128):
			command.name = value
		case name == "--root" && command.kind == commandProjectCreate && validHomeArg(value) && validOperatorText(value, 1, 4096):
			command.root = value
		case name == "--project" && (command.kind == commandAgentCreate || command.kind == commandTaskAdd) && validHumanRequestKey(value):
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
		case name == "--account" && command.kind == commandAgentCreate && validHumanRequestKey(value):
			command.account = value
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
		case name == "--task" && command.kind == commandTaskSendBack && validHumanRequestKey(value):
			command.id = value
		case name == "--note" && command.kind == commandTaskSendBack && validQuestion(value):
			command.text = value
		case name == "--priority" && command.kind == commandTaskAdd:
			priority, err := strconv.ParseInt(value, 10, 64)
			if err != nil || value != strconv.FormatInt(priority, 10) || priority < -1_000_000 || priority > 1_000_000 {
				return attemptCommand{}, false, false
			}
			command.priority = priority
			command.prioritySet = true
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
	case commandTaskSendBack:
		if command.id == "" || command.text == "" {
			return attemptCommand{}, false, false
		}
	}
	return command, false, true
}

func parseOverseer(args []string) (attemptCommand, bool, bool) {
	if len(args) >= 2 && args[1] == "status" {
		command := attemptCommand{kind: commandOverseerStatus}
		seen := map[string]bool{}
		for index := 2; index < len(args); index += 2 {
			if index+1 >= len(args) || seen[args[index]] {
				return attemptCommand{}, false, false
			}
			seen[args[index]] = true
			value := args[index+1]
			switch args[index] {
			case "--task":
				if !validHumanRequestKey(value) {
					return attemptCommand{}, false, false
				}
				command.id = value
			case "--offset":
				offset, ok := parseOverseerOffset(value, true)
				if !ok {
					return attemptCommand{}, false, false
				}
				command.offset = offset
			case "--head":
				head, ok := parseRevision(value)
				if !ok {
					return attemptCommand{}, false, false
				}
				command.head = head
			case "--text-offset":
				offset, ok := parseOverseerOffset(value, false)
				if !ok {
					return attemptCommand{}, false, false
				}
				command.textOffset = offset
			default:
				return attemptCommand{}, false, false
			}
		}
		if command.head == 0 && (command.offset != 0 || command.textOffset != 0) || command.textOffset != 0 && command.id == "" {
			return attemptCommand{}, false, false
		}
		return command, false, true
	}
	if len(args) < 3 {
		return attemptCommand{}, false, false
	}
	command := attemptCommand{}
	switch strings.Join(args[1:3], " ") {
	case "task add":
		command.kind = commandOverseerTaskAdd
	case "task update":
		command.kind = commandOverseerTaskUpdate
	case "task send-back":
		command.kind = commandOverseerTaskSendBack
	case "agent pause":
		command.kind, command.paused = commandOverseerAgentUpdate, true
	case "agent resume":
		command.kind = commandOverseerAgentUpdate
	case "worker stop":
		command.kind = commandOverseerStopWorker
	case "worker replace":
		command.kind = commandOverseerReplaceWorker
	case "worker message":
		command.kind = commandOverseerMessageWorker
	case "worker interrupt":
		command.kind = commandOverseerInterruptWorker
	case "human reply":
		command.kind = commandOverseerReplyHuman
	default:
		return attemptCommand{}, false, false
	}
	seen := map[string]bool{}
	for index := 3; index < len(args); {
		name := args[index]
		if name == "--cancel" && command.kind == commandOverseerTaskUpdate {
			if seen[name] {
				return attemptCommand{}, false, false
			}
			seen[name], command.cancel = true, true
			index++
			continue
		}
		if index+1 >= len(args) || seen[name] {
			return attemptCommand{}, false, false
		}
		seen[name] = true
		value := args[index+1]
		index += 2
		switch name {
		case "--agent":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.agent = value
		case "--task":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.id = value
		case "--task-id":
			if command.kind != commandOverseerTaskAdd || !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.id = value
		case "--incarnation-id":
			if command.kind != commandOverseerTaskAdd || !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.project = value
		case "--run":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.run = value
		case "--request":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.id = value
		case "--successor-task":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.project = value
		case "--successor-incarnation":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.account = value
		case "--title":
			if command.kind != commandOverseerTaskAdd || !validOperatorText(value, 1, 1024) {
				return attemptCommand{}, false, false
			}
			command.title = value
		case "--body":
			if command.kind != commandOverseerTaskAdd || !validOperatorText(value, 0, 131072) {
				return attemptCommand{}, false, false
			}
			command.body = value
		case "--priority":
			priority, err := strconv.ParseInt(value, 10, 64)
			if err != nil || value != strconv.FormatInt(priority, 10) || priority < -1_000_000 || priority > 1_000_000 || command.kind != commandOverseerTaskAdd && command.kind != commandOverseerTaskUpdate {
				return attemptCommand{}, false, false
			}
			command.priority = priority
			command.prioritySet = true
		case "--revision":
			revision, ok := parseRevision(value)
			if !ok {
				return attemptCommand{}, false, false
			}
			command.expectedRevision = revision
		case "--task-revision":
			revision, ok := parseRevision(value)
			if !ok {
				return attemptCommand{}, false, false
			}
			command.taskRevision = revision
		case "--run-revision":
			revision, ok := parseRevision(value)
			if !ok {
				return attemptCommand{}, false, false
			}
			command.runRevision = revision
		case "--note", "--detail", "--message", "--reply", "--instruction":
			if !validQuestion(value) {
				return attemptCommand{}, false, false
			}
			command.text = value
		case "--operation-id":
			if !validHumanRequestKey(value) {
				return attemptCommand{}, false, false
			}
			command.operationID = value
		default:
			return attemptCommand{}, false, false
		}
	}
	switch command.kind {
	case commandOverseerTaskAdd:
		if command.agent == "" || command.title == "" || command.id == "" != (command.project == "") {
			return attemptCommand{}, false, false
		}
	case commandOverseerTaskUpdate:
		if command.id == "" || command.expectedRevision == 0 || !command.cancel && command.agent == "" && !command.prioritySet {
			return attemptCommand{}, false, false
		}
	case commandOverseerTaskSendBack:
		if command.id == "" || command.text == "" {
			return attemptCommand{}, false, false
		}
	case commandOverseerAgentUpdate:
		if command.agent == "" || command.expectedRevision == 0 {
			return attemptCommand{}, false, false
		}
	case commandOverseerStopWorker:
		if command.operationID == "" || command.id == "" || command.run == "" || command.taskRevision == 0 || command.runRevision == 0 {
			return attemptCommand{}, false, false
		}
	case commandOverseerReplaceWorker:
		if command.operationID == "" || command.id == "" || command.run == "" || command.taskRevision == 0 || command.runRevision == 0 || command.project == "" || command.account == "" || command.text == "" {
			return attemptCommand{}, false, false
		}
	case commandOverseerMessageWorker, commandOverseerInterruptWorker:
		if command.operationID == "" || command.id == "" || command.run == "" || command.taskRevision == 0 || command.runRevision == 0 || command.kind == commandOverseerMessageWorker && command.text == "" {
			return attemptCommand{}, false, false
		}
	case commandOverseerReplyHuman:
		if command.operationID == "" || command.id == "" || command.expectedRevision == 0 || command.text == "" {
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

func parseOverseerOffset(value string, allowZero bool) (uint64, bool) {
	if value == "" || len(value) > 19 || value[0] == '0' && (len(value) > 1 || !allowZero) {
		return 0, false
	}
	for _, character := range value {
		if character < '0' || character > '9' {
			return 0, false
		}
	}
	parsed, err := strconv.ParseUint(value, 10, 64)
	return parsed, err == nil && parsed <= uint64(^uint64(0)>>1)
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

func validPeerText(value string) bool {
	return len(value) >= 1 && len(value) <= 2048 && utf8.ValidString(value) && !strings.ContainsRune(value, 0)
}

func validRevision(value string) bool {
	parsed, err := strconv.ParseUint(value, 10, 64)
	return err == nil && parsed > 0 && parsed <= uint64(^uint64(0)>>1)
}

func writeFailure(stderr io.Writer, kind commandKind, err error) {
	subject, input := "outcome request", "attempt input"
	if kind == commandAttemptTask {
		subject = "task request"
	} else if kind == commandRequestHuman {
		subject, input = "human request", "human request input"
	} else if kind == commandPeerStatus || kind == commandPeerAsk || kind == commandPeerAnswer {
		subject, input = "peer collaboration", "peer collaboration input"
	} else if kind == commandSendBack {
		subject, input = "send-back", "send-back input"
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
			AccountID: command.account, ToolBudgetLimit: command.toolBudget,
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
	case commandTaskSendBack:
		result, callErr := client.SendBackTask(callContext, api.SendBackInput{TaskID: command.id, Note: command.text})
		if callErr != nil {
			return writeWebFailure(stderr, "task send-back", callErr)
		}
		return writeJSON(stdout, struct {
			ID       string `json:"id"`
			Head     uint64 `json:"head"`
			Revision uint64 `json:"revision"`
		}{ID: command.id, Head: result.Head, Revision: result.Revision})
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

func runOverseer(ctx context.Context, command attemptCommand, getenv func(string) string, stdout, stderr io.Writer) int {
	socket := getenv("DARK_FACTORY_SOCKET")
	if socket == "" {
		_, _ = io.WriteString(stderr, "factoryctl: overseer client configuration is invalid\n")
		return exitFailure
	}
	client, err := api.NewAttemptClientFromEnvironment(socket)
	if err != nil {
		_, _ = io.WriteString(stderr, "factoryctl: overseer client configuration is invalid\n")
		return exitFailure
	}
	callContext, cancel := context.WithTimeout(ctx, serviceRequestTimeout)
	defer cancel()
	if command.kind == commandOverseerStatus {
		result, callErr := client.OverseerSnapshotPage(callContext, api.OverseerSnapshotInput{TaskID: command.id, Offset: command.offset, ExpectedHead: command.head, TextOffset: command.textOffset})
		if callErr != nil {
			return writeWebFailure(stderr, "overseer status", callErr)
		}
		return writeJSON(stdout, result)
	}
	var result api.MutationResult
	switch command.kind {
	case commandOverseerTaskAdd:
		id, incarnation := command.id, command.project
		if id == "" {
			var idErr error
			id, idErr = newOperatorID()
			if idErr != nil {
				return writeWebFailure(stderr, "overseer task add", idErr)
			}
			incarnation, idErr = newOperatorID()
			if idErr != nil {
				return writeWebFailure(stderr, "overseer task add", idErr)
			}
		}
		result, err = client.OverseerEnqueueTask(callContext, api.OverseerTaskCreateInput{ID: id, AssignedAgentID: command.agent, IncarnationID: incarnation, Title: command.title, Body: command.body, Priority: command.priority})
	case commandOverseerTaskUpdate:
		input := api.OverseerTaskUpdateInput{TaskID: command.id, ExpectedRevision: command.expectedRevision, Cancel: command.cancel}
		if command.prioritySet {
			priority := command.priority
			input.Priority = &priority
		}
		if command.agent != "" {
			agent := command.agent
			input.AssignedAgentID = &agent
		}
		result, err = client.OverseerUpdateTask(callContext, input)
	case commandOverseerTaskSendBack:
		result, err = client.SendBack(callContext, api.SendBackInput{TaskID: command.id, Note: command.text})
	case commandOverseerAgentUpdate:
		result, err = client.OverseerUpdateAgent(callContext, api.OverseerAgentUpdateInput{AgentID: command.agent, ExpectedRevision: command.expectedRevision, Paused: command.paused})
	case commandOverseerStopWorker:
		result, err = client.OverseerStopRun(callContext, api.OverseerRunStopInput{OperationID: command.operationID, TaskID: command.id, ExpectedTaskRevision: command.taskRevision, RunID: command.run, ExpectedRunRevision: command.runRevision})
	case commandOverseerReplaceWorker:
		result, err = client.OverseerReplaceRun(callContext, api.OverseerRunReplaceInput{OverseerRunStopInput: api.OverseerRunStopInput{OperationID: command.operationID, TaskID: command.id, ExpectedTaskRevision: command.taskRevision, RunID: command.run, ExpectedRunRevision: command.runRevision}, SuccessorTaskID: command.project, SuccessorIncarnationID: command.account, Instruction: command.text})
	case commandOverseerMessageWorker:
		result, err = client.OverseerMessageWorker(callContext, api.OverseerWorkerMessageInput{OperationID: command.operationID, TaskID: command.id, ExpectedTaskRevision: command.taskRevision, RunID: command.run, ExpectedRunRevision: command.runRevision, Message: command.text})
	case commandOverseerInterruptWorker:
		result, err = client.OverseerInterruptWorker(callContext, api.OverseerWorkerInterruptInput{OperationID: command.operationID, TaskID: command.id, ExpectedTaskRevision: command.taskRevision, RunID: command.run, ExpectedRunRevision: command.runRevision})
	case commandOverseerReplyHuman:
		result, err = client.OverseerReplyHuman(callContext, api.OverseerHumanReplyInput{OperationID: command.operationID, RequestID: command.id, ExpectedRevision: command.expectedRevision, Reply: command.text})
	default:
		return exitUsage
	}
	if err != nil {
		return writeWebFailure(stderr, "overseer control", err)
	}
	return writeJSON(stdout, result)
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
