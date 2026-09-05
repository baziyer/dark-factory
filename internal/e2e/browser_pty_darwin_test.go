//go:build darwin

package e2e_test

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/daemon"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
	"golang.org/x/sys/unix"
)

const (
	e2eOrigin  = "https://app.darkfactory.build"
	e2eTimeout = 40 * time.Second
)

type scenario struct {
	name, question, program string
	want                    kernel.OutcomeKind
}

type nodeResult struct {
	Scenario               string `json:"scenario"`
	RequestID              string `json:"requestId"`
	RunID                  string `json:"runId"`
	OutputCursor           string `json:"outputCursor"`
	StaleInput             string `json:"staleInput"`
	ClientID               string `json:"clientId"`
	Capabilities           uint8  `json:"capabilities"`
	InputMarkers           int    `json:"inputMarkers"`
	ResizeMarkers          int    `json:"resizeMarkers"`
	ReplyMarkers           int    `json:"replyMarkers"`
	Reconnects             int    `json:"reconnects"`
	ConnectionErrors       int    `json:"connectionErrors"`
	ExitCount              int    `json:"exitCount"`
	PostExitRequest        bool   `json:"postExitRequest"`
	CanonicalHead          string `json:"canonicalHead"`
	CanonicalAgentRevision string `json:"canonicalAgentRevision"`
	Exit                   struct {
		SessionID  string `json:"sessionId"`
		ExitCode   int    `json:"exitCode"`
		ExitSignal int    `json:"exitSignal"`
	} `json:"exit"`
}

type runResult struct {
	run kernel.Run
	err error
}

func TestGoTypeScriptBrowserPTYLifecycle(t *testing.T) {
	if os.Getenv("DARK_FACTORY_BROWSER_E2E") != "1" {
		t.Skip("run through scripts/go-browser-e2e.sh")
	}
	factoryctl := requiredExecutable(t, "DARK_FACTORY_E2E_FACTORYCTL")
	runnerExecutable := requiredExecutable(t, "DARK_FACTORY_E2E_RUNNER")
	node := requiredExecutable(t, "DARK_FACTORY_E2E_NODE")
	nodeScript := requiredFile(t, "DARK_FACTORY_E2E_NODE_SCRIPT")

	scenarios := []scenario{
		{
			name:     "interactive_reconnect_reply_success",
			question: "Which deterministic answer should this exact run use?",
			want:     kernel.OutcomeSucceeded,
			program: `set -eu
printf 'E2E_READY\n'
IFS= read -r first
printf 'E2E_INPUT:%s\n' "$first"
IFS= read -r measure
test "$measure" = measure
set -- $(/bin/stty size)
printf 'E2E_SIZE:%s:%s\n' "$1" "$2"
IFS= read -r proceed
test "$proceed" = proceed
"$DARK_FACTORY_FACTORYCTL" attempt request-human --idempotency-key 11111111111111111111111111111111 --question 'Which deterministic answer should this exact run use?'
/bin/stty -icanon min 1 time 0
reply=$(/bin/dd bs=1 count=12 2>/dev/null)
printf 'E2E_REPLY:%s\n' "$reply"
"$DARK_FACTORY_FACTORYCTL" attempt succeed --result typed-success
`,
		},
		{
			name:     "typed_cancel_revokes_input_and_reaps",
			question: "Cancel this exact deterministic run?",
			want:     kernel.OutcomeCancelled,
			program: `set -eu
printf 'E2E_WAITING\n'
"$DARK_FACTORY_FACTORYCTL" attempt request-human --idempotency-key 22222222222222222222222222222222 --question 'Cancel this exact deterministic run?'
IFS= read -r forbidden
printf 'E2E_FORBIDDEN:%s\n' "$forbidden"
exit 91
`,
		},
	}

	for index, test := range scenarios {
		t.Run(test.name, func(t *testing.T) {
			fixture := newFixture(t, byte(16+index*16), test, factoryctl, runnerExecutable)
			runDone := fixture.startRun()
			result := fixture.runNode(node, nodeScript, test)
			completed := waitRun(t, runDone)
			fixture.run = completed.run
			if completed.err != nil {
				t.Fatalf("RunNext: %v", completed.err)
			}
			fixture.assertTerminal(completed.run, test.want)
			fixture.assertNodeResult(result, test)
			fixture.assertHumanRequest(result.RequestID)
			fixture.assertReleased(completed.run)
			fixture.close()
		})
	}
}

type fixture struct {
	t                     *testing.T
	root                  string
	store                 *kernel.Store
	daemon                *daemon.Daemon
	runtimeParent         *daemon.RuntimeParent
	listener              *api.Listener
	apiHome               *install.OperationalHome
	apiAuthority          *install.LocalAPIAuthority
	apiDone               chan error
	agentID               kernel.AgentID
	spec                  daemon.SupervisorSpec
	run                   kernel.Run
	runCancel             context.CancelFunc
	baselineFDs           int
	baselineGoroutines    int
	browserAddress        string
	nodeMu                sync.Mutex
	node                  *exec.Cmd
	nodeDone              chan struct{}
	runMu                 sync.Mutex
	runStarted            time.Time
	runFinished           time.Time
	runObserved           *runResult
	closeOnce             sync.Once
	processCensusObserved int
}

func newFixture(t *testing.T, seed byte, test scenario, factoryctl, runnerExecutable string) *fixture {
	t.Helper()
	root, err := os.MkdirTemp("/private/tmp", "dark-factory-browser-pty-e2e-")
	if err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(root, 0o700); err != nil {
		t.Fatal(err)
	}
	result := &fixture{t: t, root: root, baselineFDs: fdCount(t), baselineGoroutines: runtime.NumGoroutine()}
	// Cleanups run last-in-first-out. The ordinary owner gets the first chance
	// to close and assert everything; the separately registered fallback then
	// retries only retained live capabilities if ordinary cleanup panics or
	// returns partway through. Neither path derives signal authority from Store.
	t.Cleanup(result.safetyClose)
	t.Cleanup(result.close)

	gitHome := filepath.Join(root, "git-home")
	repository := filepath.Join(root, "repository")
	changeParent := filepath.Join(root, "changes")
	for _, path := range []string{gitHome, repository, changeParent} {
		if err := os.Mkdir(path, 0o700); err != nil {
			t.Fatal(err)
		}
	}
	git := nativeGit(t)
	gitRun(t, git, gitHome, "init", repository)
	gitRun(t, git, gitHome, "-C", repository, "config", "user.email", "e2e@example.invalid")
	gitRun(t, git, gitHome, "-C", repository, "config", "user.name", "e2e")
	if err := os.WriteFile(filepath.Join(repository, "payload.txt"), []byte("exact source\n"), 0o644); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(repository, "fixture.sh"), []byte("#!/bin/sh\n"+test.program), 0o700); err != nil {
		t.Fatal(err)
	}
	gitRun(t, git, gitHome, "-C", repository, "add", "payload.txt", "fixture.sh")
	gitRun(t, git, gitHome, "-C", repository, "commit", "-m", "base")
	base := strings.TrimSpace(gitOutput(t, git, gitHome, "-C", repository, "rev-parse", "HEAD"))

	apiHomePath := filepath.Join(root, "api-home")
	if socket := install.LocalAPISocketPath(apiHomePath); len(socket) > install.MaxSocketPathBytes {
		t.Fatalf("api socket path is %d bytes, over the %d-byte budget: %q", len(socket), install.MaxSocketPathBytes, socket)
	}
	if _, err := install.Init(context.Background(), apiHomePath); err != nil {
		t.Fatal(err)
	}
	operatorToken := filepath.Join(apiHomePath, "operator.token")
	if err := os.WriteFile(operatorToken, bytes.Repeat([]byte{'o'}, 32), 0o600); err != nil {
		t.Fatal(err)
	}
	result.apiHome, err = install.OpenOperationalHome(context.Background(), apiHomePath)
	if err != nil {
		t.Fatal(err)
	}
	runtimes, err := result.apiHome.Runtimes()
	if err != nil {
		t.Fatal(err)
	}
	result.runtimeParent, err = daemon.OpenRuntimeParent(context.Background(), runtimes, filepath.Join(apiHomePath, "runtimes"))
	if err != nil {
		t.Fatal(err)
	}

	now := e2eTime(t)
	result.store, err = createTestStore(context.Background(), filepath.Join(root, "factory.sqlite3"), kernel.FactoryConfig{Capacity: 1}, now)
	if err != nil {
		t.Fatal(err)
	}
	projectID := projectID(t, seed)
	result.agentID = agentID(t, seed+1)
	project, err := result.store.CreateProject(context.Background(), kernel.NewProject{ID: projectID, Name: "e2e-project", Root: repository, VerificationPolicy: kernel.VerificationNone}, now)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := result.store.CreateAgent(context.Background(), kernel.NewAgent{
		ID: result.agentID, ProjectID: project.ID, Name: "e2e-worker", Role: kernel.RoleWorker,
		Provider: kernel.ProviderShell, ToolBudgetLimit: 20,
	}, now); err != nil {
		t.Fatal(err)
	}
	if _, err := result.store.EnqueueTask(context.Background(), kernel.NewTask{
		ID: taskID(t, seed+2), ProjectID: project.ID, AssignedAgentID: result.agentID, IncarnationID: incarnationID(t, seed+3),
		Title: "browser PTY E2E", Body: "exec ./fixture.sh\n", Priority: 1,
	}, now); err != nil {
		t.Fatal(err)
	}
	factory, err := result.store.Factory(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	if _, err := result.store.SetDispatch(context.Background(), factory.Revision, true, e2eTime(t)); err != nil {
		t.Fatal(err)
	}

	result.daemon, err = daemon.NewDaemon(result.store)
	if err != nil {
		t.Fatal(err)
	}
	result.apiAuthority, err = result.apiHome.OpenLocalAPI(context.Background())
	if err != nil {
		t.Fatal(err)
	}
	result.listener, err = api.Listen(result.apiAuthority)
	if err != nil {
		t.Fatal(err)
	}
	socket := install.LocalAPISocketPath(apiHomePath)
	result.apiDone = make(chan error, 1)
	go func() {
		for {
			connection, acceptErr := result.listener.Accept()
			if acceptErr != nil {
				result.apiDone <- acceptErr
				return
			}
			if handleErr := result.daemon.HandleConnection(context.Background(), connection); handleErr != nil {
				result.apiDone <- handleErr
				return
			}
		}
	}()

	browserRuntime, err := result.daemon.ListenBrowser("127.0.0.1:0", []string{e2eOrigin})
	if err != nil {
		t.Fatal(err)
	}
	result.browserAddress = browserRuntime.Addr()
	result.spec = daemon.SupervisorSpec{
		RuntimeParent: result.runtimeParent, ChangeParent: changeParent,
		GitExecutable: git, BaseRevision: base, AttemptSocket: socket,
		RunnerExecutable: runnerExecutable, FactoryctlExecutable: factoryctl,
		ToolPath: filepath.Join(runtime.GOROOT(), "bin") + ":/usr/bin:/bin", AccountHome: root,
	}
	return result
}

func (fixture *fixture) startRun() <-chan runResult {
	fixture.t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), e2eTimeout)
	fixture.runCancel = cancel
	fixture.runMu.Lock()
	fixture.runStarted = time.Now()
	fixture.runMu.Unlock()
	done := make(chan runResult, 1)
	go func() {
		run, err := fixture.daemon.RunNext(ctx, fixture.spec)
		result := runResult{run: run, err: err}
		fixture.runMu.Lock()
		fixture.runFinished = time.Now()
		fixture.runObserved = &result
		fixture.runMu.Unlock()
		done <- result
	}()
	return done
}

func (fixture *fixture) runNode(node, script string, test scenario) nodeResult {
	fixture.t.Helper()
	link, err := fixture.daemon.OpenBrowser(context.Background())
	if err != nil {
		fixture.t.Fatalf("OpenBrowser: %v", err)
	}
	const fragmentPrefix = e2eOrigin + "/#df_pair="
	if !strings.HasPrefix(link, fragmentPrefix) {
		fixture.t.Fatalf("unexpected pairing launch shape")
	}
	challenge := strings.TrimPrefix(link, fragmentPrefix)
	if decoded, decodeErr := hex.DecodeString(challenge); decodeErr != nil || len(decoded) != 32 {
		fixture.t.Fatalf("invalid one-time challenge shape")
	}
	scenarioName := "interactive"
	if test.want == kernel.OutcomeCancelled {
		scenarioName = "cancel"
	}
	configuration := map[string]string{
		"url":       "ws://" + fixture.browserAddress + browser.Path,
		"host":      fixture.browserAddress,
		"origin":    e2eOrigin,
		"challenge": challenge,
		"agentId":   fixture.agentID.String(),
		"question":  test.question,
		"scenario":  scenarioName,
	}
	encoded, err := json.Marshal(configuration)
	if err != nil {
		fixture.t.Fatal(err)
	}
	configPath := filepath.Join(fixture.root, "browser-e2e.json")
	if err := os.WriteFile(configPath, encoded, 0o600); err != nil {
		fixture.t.Fatal(err)
	}
	nodeHome := filepath.Join(fixture.root, "node-home")
	temp := filepath.Join(fixture.root, "node-tmp")
	for _, path := range []string{nodeHome, temp} {
		if err := os.Mkdir(path, 0o700); err != nil {
			fixture.t.Fatal(err)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), e2eTimeout)
	defer cancel()
	command := exec.CommandContext(ctx, node, script, configPath)
	command.Env = []string{
		"HOME=" + nodeHome,
		"TMPDIR=" + temp,
		"PATH=/usr/bin:/bin",
		"LANG=C",
		"LC_ALL=C",
		"NODE_OPTIONS=--unhandled-rejections=strict",
	}
	var stdout, stderr bytes.Buffer
	command.Stdout, command.Stderr = &stdout, &stderr
	fixture.nodeMu.Lock()
	fixture.node, fixture.nodeDone = command, make(chan struct{})
	fixture.nodeMu.Unlock()
	if err := command.Start(); err != nil {
		fixture.t.Fatalf("start Node E2E: %v", err)
	}
	err = command.Wait()
	fixture.nodeMu.Lock()
	close(fixture.nodeDone)
	fixture.node = nil
	fixture.nodeMu.Unlock()
	if err != nil {
		fixture.t.Fatalf("Node E2E: %v\ndurable: %s\nstderr:\n%s\nstdout:\n%s", err, fixture.durableDiagnostic(), bounded(stderr.String()), bounded(stdout.String()))
	}
	decoder := json.NewDecoder(bytes.NewReader(stdout.Bytes()))
	decoder.DisallowUnknownFields()
	var result nodeResult
	if err := decoder.Decode(&result); err != nil {
		fixture.t.Fatalf("decode Node result: %v: %s", err, bounded(stdout.String()))
	}
	var extra any
	if decoder.Decode(&extra) == nil {
		fixture.t.Fatal("Node E2E emitted more than one result")
	}
	return result
}

func (fixture *fixture) durableDiagnostic() string {
	if fixture.store == nil {
		return "store=closed"
	}
	recoverable, err := fixture.store.RecoverableRuns(context.Background())
	if err != nil {
		return "recovery_error=" + err.Error()
	}
	fixture.runMu.Lock()
	started, finished, observed := fixture.runStarted, fixture.runFinished, fixture.runObserved
	fixture.runMu.Unlock()
	runOwner := "not-started"
	if !started.IsZero() {
		runOwner = "running:" + time.Since(started).Round(time.Millisecond).String()
	}
	if observed != nil {
		runOwner = "returned:" + finished.Sub(started).Round(time.Millisecond).String() + ":phase=" + observed.run.Phase.String()
		runOwner += ":" + runControlDiagnostic(observed.run)
		if observed.err != nil {
			runOwner += ":error=" + observed.err.Error()
		}
	}
	census := fmt.Sprintf("run_owner=%s:fds=%d/%d:goroutines=%d/%d", runOwner, fdCount(fixture.t), fixture.baselineFDs, runtime.NumGoroutine(), fixture.baselineGoroutines)
	if len(recoverable) == 0 {
		return census + ":recoverable=0"
	}
	parts := make([]string, 0, len(recoverable))
	for _, item := range recoverable {
		resources := make([]string, 0, len(item.Resources))
		for _, resource := range item.Resources {
			detail := resource.Kind.String() + ":" + resource.State.String()
			identity, process, identityErr := processIdentity(resource)
			if identityErr != nil {
				detail += ":identity-error"
			} else if process {
				observation := runner.ObserveProcess(identity)
				members := make([]string, 0, len(observation.Members))
				for _, member := range observation.Members {
					members = append(members, fmt.Sprintf("%d", member.PID))
				}
				detail += ":" + string(observation.Presence) + ":members=" + strings.Join(members, "/")
			}
			resources = append(resources, detail)
		}
		parts = append(parts, item.Run.ID.String()+":"+item.Run.Phase.String()+":"+item.TerminalSession.State.String()+":"+runControlDiagnostic(item.Run)+":"+strings.Join(resources, ",")+":"+fixture.terminalSpoolDiagnostic(item))
	}
	return census + ":" + strings.Join(parts, ";")
}

func runControlDiagnostic(run kernel.Run) string {
	proposal := "proposal=none"
	if run.Proposal != nil {
		proposal = fmt.Sprintf("proposal=%s/%s:detail-bytes=%d:result-bytes=%d", run.Proposal.Kind().String(), run.Proposal.Code().String(), len(run.Proposal.Detail()), len(run.Proposal.Result()))
	}
	return proposal + ":provider-exit=" + processExitDiagnostic(run.ProviderExit) + ":runner-exit=" + processExitDiagnostic(run.RunnerExit)
}

func processExitDiagnostic(exit *kernel.ProcessExit) string {
	if exit == nil {
		return "none"
	}
	if code, ok := exit.Code(); ok {
		return fmt.Sprintf("code/%d@%d", code, exit.Sequence())
	}
	if signal, ok := exit.Signal(); ok {
		return fmt.Sprintf("signal/%d@%d", signal, exit.Sequence())
	}
	if exit.RecoveredAbsence() {
		return fmt.Sprintf("recovered-absence@%d", exit.Sequence())
	}
	return fmt.Sprintf("invalid@%d", exit.Sequence())
}

func (fixture *fixture) terminalSpoolDiagnostic(item kernel.RecoverableRun) string {
	var expected kernel.FileIdentity
	var found bool
	for _, resource := range item.Resources {
		if resource.Kind == kernel.ResourceRuntimeRoot {
			expected, found = resource.Identity.Path()
			break
		}
	}
	if !found {
		return "spool=no-runtime-identity"
	}
	path := filepath.Join(fixture.root, "runtimes", item.Run.ID.String())
	var status unix.Stat_t
	if err := unix.Lstat(path, &status); err != nil {
		return "spool=lstat:" + err.Error()
	}
	if status.Mode&unix.S_IFMT != unix.S_IFDIR || int64(status.Dev) != expected.Device() || int64(status.Ino) != expected.Inode() {
		return "spool=runtime-identity-mismatch"
	}
	directory, err := os.Open(path)
	if err != nil {
		return "spool=open:" + err.Error()
	}
	record, loadErr := runner.LoadTerminal(directory, runner.TerminalSpoolName)
	closeErr := directory.Close()
	if loadErr != nil || closeErr != nil {
		return fmt.Sprintf("spool=load:%v:close:%v", loadErr, closeErr)
	}
	exit := record.Terminal.Exit
	return fmt.Sprintf("spool=present:exit=%d/%d:aborted=%t", exit.Code, exit.Signal, exit.Aborted)
}

func (fixture *fixture) assertNodeResult(result nodeResult, test scenario) {
	fixture.t.Helper()
	if result.ClientID == "" || result.Capabilities != uint8(kernel.BrowserCapabilityKnownMask) {
		fixture.t.Fatalf("browser identity/capabilities = %q/%d", result.ClientID, result.Capabilities)
	}
	canonicalExit := result.Exit.ExitSignal == 0 && result.Exit.ExitCode >= 0 || result.Exit.ExitCode == 0 && result.Exit.ExitSignal > 0
	if result.ExitCount != 1 || !result.PostExitRequest || result.CanonicalHead == "" || result.CanonicalAgentRevision == "" || result.Exit.SessionID == "" || !canonicalExit {
		fixture.t.Fatalf("browser terminal exit = %+v", result.Exit)
	}
	if fixture.run.ProviderExit == nil {
		fixture.t.Fatal("durable provider exit is missing")
	}
	if code, ok := fixture.run.ProviderExit.Code(); result.Exit.ExitSignal != 0 || !ok || code != int64(result.Exit.ExitCode) {
		if signal, signalOK := fixture.run.ProviderExit.Signal(); result.Exit.ExitCode != 0 || !signalOK || signal != int64(result.Exit.ExitSignal) {
			fixture.t.Fatalf("wire provider exit %d/%d != durable %+v", result.Exit.ExitCode, result.Exit.ExitSignal, fixture.run.ProviderExit)
		}
	}
	if test.want == kernel.OutcomeSucceeded {
		if result.Scenario != "interactive" || result.InputMarkers != 1 || result.ResizeMarkers != 1 || result.ReplyMarkers != 1 || result.Reconnects != 1 || result.ConnectionErrors != 1 {
			fixture.t.Fatalf("interactive Node proof = %+v", result)
		}
		if result.OutputCursor == "" {
			fixture.t.Fatal("interactive terminal cursor was not retained")
		}
		return
	}
	if result.Scenario != "cancel" || result.RunID != fixture.run.ID.String() || result.Exit.ExitSignal <= 0 || result.StaleInput != "locally_rejected" && result.StaleInput != "rejected" {
		fixture.t.Fatalf("cancel Node proof = %+v", result)
	}
}

func (fixture *fixture) assertTerminal(run kernel.Run, want kernel.OutcomeKind) {
	fixture.t.Helper()
	if run.Phase != kernel.RunTerminal || run.Proposal == nil || run.Terminal == nil || run.Proposal.Kind() != want || run.Terminal.Kind() != want || run.CredentialRevokedAt == nil {
		fixture.t.Fatalf("terminal run = %+v, want %s", run, want.String())
	}
	if want == kernel.OutcomeSucceeded && run.Terminal.Result() != "typed-success" {
		fixture.t.Fatalf("success result = %q", run.Terminal.Result())
	}
	session, found, err := fixture.store.TerminalSessionForRun(context.Background(), run.ID)
	if err != nil || !found || session.State != kernel.TerminalSessionClosed || session.LeaseClientID != nil || session.LeaseExpiresAt != nil {
		fixture.t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	state, err := fixture.store.Factory(context.Background())
	if err != nil {
		fixture.t.Fatal(err)
	}
	agent, found, err := fixture.store.Agent(context.Background(), fixture.agentID)
	if err != nil || !found {
		fixture.t.Fatalf("terminal agent: found=%v err=%v", found, err)
	}
	rawClient, err := hex.DecodeString(fixture.lastClientID())
	if err != nil {
		fixture.t.Fatal(err)
	}
	clientID, err := kernel.BrowserClientIDFromBytes(rawClient)
	if err != nil {
		fixture.t.Fatal(err)
	}
	if _, available, err := fixture.store.ResolveAgentTerminalTarget(context.Background(), clientID, fixture.agentID, agent.Revision, state.Head); err != nil || available {
		fixture.t.Fatalf("terminal run remained targetable: available=%v err=%v", available, err)
	}
}

// lastClientID reads the sole paired client only through the bounded Store page.
func (fixture *fixture) lastClientID() string {
	fixture.t.Helper()
	page, err := fixture.store.ListBrowserClients(context.Background(), nil)
	if err != nil || len(page.Items) != 1 {
		fixture.t.Fatalf("paired client page: count=%d err=%v", len(page.Items), err)
	}
	return page.Items[0].ID.String()
}

func (fixture *fixture) assertHumanRequest(rawID string) {
	fixture.t.Helper()
	raw, err := hex.DecodeString(rawID)
	if err != nil {
		fixture.t.Fatal(err)
	}
	id, err := kernel.HumanRequestIDFromBytes(raw)
	if err != nil {
		fixture.t.Fatal(err)
	}
	request, found, err := fixture.store.HumanRequest(context.Background(), id)
	if err != nil || !found || request.Status != kernel.HumanRequestResolved || request.CanReply {
		fixture.t.Fatalf("resolved HumanRequest = %+v, found=%v, err=%v", request, found, err)
	}
}

func (fixture *fixture) assertReleased(run kernel.Run) {
	fixture.t.Helper()
	resources, err := fixture.store.Resources(context.Background(), run.ID)
	if err != nil || len(resources) != 4 {
		fixture.t.Fatalf("resources: count=%d err=%v", len(resources), err)
	}
	for _, resource := range resources {
		if resource.State != kernel.ResourceReleased {
			fixture.t.Fatalf("resource %s = %s", resource.Kind.String(), resource.State.String())
		}
		identity, ok, err := processIdentity(resource)
		if err != nil {
			fixture.t.Fatal(err)
		}
		if ok {
			fixture.processCensusObserved++
			if observation := runner.ObserveProcess(identity); observation.Presence != runner.Absent && observation.Presence != runner.Reused {
				fixture.t.Fatalf("released %s remains %+v", resource.Kind.String(), observation)
			}
		}
	}
	if fixture.processCensusObserved != 3 {
		fixture.t.Fatalf("process census covered %d identities, want three registered process resources", fixture.processCensusObserved)
	}
}

func (fixture *fixture) close() {
	if fixture == nil {
		return
	}
	fixture.closeOnce.Do(fixture.closeOwned)
}

// safetyClose is deliberately registered separately from close and does not
// share closeOnce. If ordinary cleanup panics or returns after retaining an
// owner, this fallback retries the actual Daemon/exec.Cmd capabilities. Store
// process rows remain observation evidence only and never authorize a signal.
func (fixture *fixture) safetyClose() {
	if fixture != nil {
		fixture.closeOwned()
	}
}

func (fixture *fixture) closeOwned() {
	if fixture.runCancel != nil {
		fixture.runCancel()
	}
	fixture.nodeMu.Lock()
	node, nodeDone := fixture.node, fixture.nodeDone
	fixture.nodeMu.Unlock()
	if node != nil && node.Process != nil {
		_ = node.Process.Kill()
		if nodeDone != nil {
			select {
			case <-nodeDone:
				fixture.nodeMu.Lock()
				if fixture.node == node {
					fixture.node = nil
				}
				fixture.nodeMu.Unlock()
			case <-time.After(3 * time.Second):
				fixture.t.Errorf("Node child did not join")
				return
			}
		}
	}
	if fixture.daemon != nil {
		if err := fixture.daemon.Close(); err != nil {
			fixture.t.Errorf("daemon close: %v", err)
			return
		} else {
			fixture.daemon = nil
		}
	}
	if fixture.listener != nil {
		closeErr := fixture.listener.Close()
		joined := false
		select {
		case err := <-fixture.apiDone:
			joined = true
			if err != nil && !errors.Is(err, api.ErrTransport) {
				fixture.t.Errorf("API owner: %v", err)
			}
		case <-time.After(3 * time.Second):
			fixture.t.Errorf("API owner did not join")
		}
		if closeErr != nil {
			fixture.t.Errorf("API listener close: %v", closeErr)
			return
		} else if joined {
			fixture.listener = nil
		} else {
			return
		}
	}
	if fixture.apiAuthority != nil {
		if err := fixture.apiAuthority.Close(); err != nil {
			fixture.t.Errorf("API authority close: %v", err)
			return
		}
		fixture.apiAuthority = nil
	}
	if fixture.runtimeParent != nil {
		if err := fixture.runtimeParent.Close(); err != nil {
			fixture.t.Errorf("runtime parent close: %v", err)
			return
		} else {
			fixture.runtimeParent = nil
		}
	}
	if fixture.apiHome != nil {
		if err := fixture.apiHome.Close(); err != nil {
			fixture.t.Errorf("API home close: %v", err)
			return
		}
		fixture.apiHome = nil
	}
	if fixture.store != nil {
		if err := fixture.store.Close(); err != nil {
			fixture.t.Errorf("Store close: %v", err)
			return
		} else {
			fixture.store = nil
		}
	}
	if fixture.browserAddress != "" {
		probe, err := net.Listen("tcp4", fixture.browserAddress)
		if err != nil {
			fixture.t.Errorf("browser socket retained: %v", err)
			return
		} else {
			_ = probe.Close()
			fixture.browserAddress = ""
		}
	}
	if fixture.root != "" {
		root := fixture.root
		if err := os.RemoveAll(root); err != nil {
			fixture.t.Errorf("temporary root cleanup: %v", err)
			return
		}
		if _, err := os.Stat(root); !errors.Is(err, os.ErrNotExist) {
			fixture.t.Errorf("temporary root remains: %v", err)
			return
		} else {
			fixture.root = ""
		}
	}
	fixture.assertCensus()
}

func (fixture *fixture) assertCensus() {
	deadline := time.Now().Add(4 * time.Second)
	for {
		runtime.GC()
		fds, goroutines := fdCount(fixture.t), runtime.NumGoroutine()
		if fds == fixture.baselineFDs && goroutines == fixture.baselineGoroutines {
			fixture.t.Logf("E2E census stable: fds=%d goroutines=%d process_resources=%d", fds, goroutines, fixture.processCensusObserved)
			return
		}
		if time.Now().After(deadline) {
			fixture.t.Errorf("E2E census: fds %d -> %d; goroutines %d -> %d", fixture.baselineFDs, fds, fixture.baselineGoroutines, goroutines)
			return
		}
		runtime.Gosched()
		time.Sleep(10 * time.Millisecond)
	}
}

func processIdentity(resource kernel.Resource) (runner.Identity, bool, error) {
	pid, pgid, digest, ok := resource.Identity.Process()
	if !ok {
		return runner.Identity{}, false, nil
	}
	encoded := digest.Bytes()
	magic := []byte{'D', 'F', 'B', 'I', 'R', 'T', 'H', 1}
	if len(encoded) != 32 || !bytes.Equal(encoded[:8], magic) || !allZero(encoded[20:]) {
		return runner.Identity{}, false, errors.New("invalid persisted process birth encoding")
	}
	seconds := binary.BigEndian.Uint64(encoded[8:16])
	microseconds := binary.BigEndian.Uint32(encoded[16:20])
	if seconds == 0 || microseconds >= 1_000_000 || pid > int64(^uint(0)>>1) || pgid > int64(^uint(0)>>1) {
		return runner.Identity{}, false, errors.New("invalid persisted process identity")
	}
	identity := runner.Identity{PID: int(pid), PGID: int(pgid), Birth: runner.Birth{Seconds: int64(seconds), Microseconds: int32(microseconds)}}
	if !identity.Valid() {
		return runner.Identity{}, false, errors.New("invalid runner identity")
	}
	return identity, true, nil
}

func waitRun(t testing.TB, done <-chan runResult) runResult {
	t.Helper()
	select {
	case result := <-done:
		return result
	case <-time.After(e2eTimeout):
		t.Fatal("RunNext did not converge")
		return runResult{}
	}
}

func requiredExecutable(t testing.TB, name string) string {
	t.Helper()
	path := requiredFile(t, name)
	info, err := os.Stat(path)
	if err != nil || info.Mode().Perm()&0o111 == 0 {
		t.Fatalf("%s is not executable", name)
	}
	return path
}

func requiredFile(t testing.TB, name string) string {
	t.Helper()
	path := os.Getenv(name)
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		t.Fatalf("%s must be one absolute path", name)
	}
	if _, err := os.Stat(path); err != nil {
		t.Fatalf("%s: %v", name, err)
	}
	return path
}

func nativeGit(t testing.TB) string {
	t.Helper()
	if _, err := os.Stat(change.TrustedGitExecutable); err != nil {
		t.Fatalf("Command Line Tools Git is unavailable: %v", err)
	}
	return change.TrustedGitExecutable
}

func gitRun(t testing.TB, git, home string, args ...string) {
	t.Helper()
	_ = gitOutput(t, git, home, args...)
}
func gitOutput(t testing.TB, git, home string, args ...string) string {
	t.Helper()
	command := exec.Command(git, args...)
	command.Env = []string{"PATH=/usr/bin:/bin", "HOME=" + home, "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL=/dev/null", "GIT_TERMINAL_PROMPT=0"}
	body, err := command.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v: %s", args, err, bounded(string(body)))
	}
	return string(body)
}

func e2eTime(t testing.TB) kernel.UnixMillis {
	t.Helper()
	value, err := kernel.NewUnixMillis(time.Now().UnixMilli())
	if err != nil {
		t.Fatal(err)
	}
	return value
}

func idBytes(seed byte) []byte { return bytes.Repeat([]byte{seed}, kernel.IDBytes) }
func projectID(t testing.TB, seed byte) kernel.ProjectID {
	t.Helper()
	value, err := kernel.ProjectIDFromBytes(idBytes(seed))
	if err != nil {
		t.Fatal(err)
	}
	return value
}
func agentID(t testing.TB, seed byte) kernel.AgentID {
	t.Helper()
	value, err := kernel.AgentIDFromBytes(idBytes(seed))
	if err != nil {
		t.Fatal(err)
	}
	return value
}
func taskID(t testing.TB, seed byte) kernel.TaskID {
	t.Helper()
	value, err := kernel.TaskIDFromBytes(idBytes(seed))
	if err != nil {
		t.Fatal(err)
	}
	return value
}
func incarnationID(t testing.TB, seed byte) kernel.IncarnationID {
	t.Helper()
	value, err := kernel.IncarnationIDFromBytes(idBytes(seed))
	if err != nil {
		t.Fatal(err)
	}
	return value
}

func fdCount(t testing.TB) int {
	t.Helper()
	entries, err := os.ReadDir("/dev/fd")
	if err != nil {
		t.Fatal(err)
	}
	return len(entries)
}

func allZero(value []byte) bool {
	for _, item := range value {
		if item != 0 {
			return false
		}
	}
	return true
}

func bounded(value string) string {
	if len(value) <= 4096 {
		return value
	}
	return value[:4096] + "…"
}

func TestE2EProcessIdentityDecoderRejectsMalformedBirth(t *testing.T) {
	birth, err := kernel.BirthDigestFromBytes(bytes.Repeat([]byte{1}, kernel.DigestBytes))
	if err != nil {
		t.Fatal(err)
	}
	identity, err := kernel.NewProcessResourceIdentity(10, 10, birth)
	if err != nil {
		t.Fatal(err)
	}
	if _, _, err := processIdentity(kernel.Resource{Identity: identity}); err == nil {
		t.Fatal("malformed birth digest became hard-safety signal authority")
	}
}
