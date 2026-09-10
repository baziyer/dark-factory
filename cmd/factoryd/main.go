package main

import (
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/url"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/api"
	"github.com/dark-factory-build/dark-factory/internal/buildinfo"
	"github.com/dark-factory-build/dark-factory/internal/change"
	"github.com/dark-factory-build/dark-factory/internal/daemon"
	"github.com/dark-factory-build/dark-factory/internal/install"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/provider"
	"github.com/dark-factory-build/dark-factory/internal/relayhost"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

const (
	defaultBrowserAddress = daemon.DefaultBrowserAddress
	defaultBrowserOrigin  = "https://app.darkfactory.build"
	defaultGitExecutable  = change.TrustedGitExecutable
	defaultToolPath       = "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
	defaultBaseRevision   = "HEAD"
	runnerSiblingName     = "factory-runner"
	factoryctlSiblingName = "factoryctl"
	maxHomeArgumentBytes  = 4096
	maxAPIHandlers        = 32
	apiHandlerTimeout     = 10 * time.Second
	exitFailure           = 1
	exitUsage             = 64

	usage = `usage:
  factoryd --home ABSOLUTE [--git PATH] [--tool-path PATH] [--base-revision REVISION]
           [--runner PATH] [--factoryctl PATH]
           [--relay-origin RELAY_ORIGIN]
           [--development-browser-address LOOPBACK]
           [--development-browser-origin EXACT_ORIGIN ...]
  factoryd --version
  factoryd --build-identity
`
)

var (
	cleanStartupCancellation = errors.New("factoryd: clean startup cancellation")
	startupPhaseHook         func(string)
	closeDaemon              = func(value *daemon.Daemon) error { return value.Close() }
	closeRuntimeParent       = func(value *daemon.RuntimeParent) error { return value.Close() }
	selfExecutable           = os.Executable
	recoveryLog              = io.Writer(os.Stderr)
)

type config struct {
	home           string
	browserAddress string
	browserOrigins []string
	// relayOrigin enables the outbound remote-access relay. Empty is
	// disabled: the loopback browser surface is unchanged either way.
	relayOrigin string
	// Supervisor inputs. Empty runner/factoryctl select sibling self-location
	// at boot; every executable is committed before the process serves.
	gitExecutable        string
	toolPath             string
	toolPathExplicit     bool
	baseRevision         string
	runnerExecutable     string
	factoryctlExecutable string
}

type process struct {
	lifecycleMu sync.Mutex
	closeDone   chan struct{}
	closeErr    error
	waitDone    chan struct{}
	waitErr     error

	cancel context.CancelFunc

	home          *install.OperationalHome
	runtimeParent *daemon.RuntimeParent
	daemon        *daemon.Daemon
	apiAuthority  *install.LocalAPIAuthority
	listener      *api.Listener
	browser       *daemon.BrowserRuntime
	relay         *daemon.RelayRuntime
	// supervisorSpec is derived and committed at boot; the scheduler
	// activation consumes it. A specification that cannot be proven at boot
	// refuses the whole process rather than failing the first attempt.
	supervisorSpec daemon.SupervisorSpec

	apiDone  chan struct{}
	apiErr   error
	apiStart bool
	handlers sync.WaitGroup
	slots    chan struct{}

	// schedulerDone closes after RunScheduler returns; shutdown joins it
	// before closing the daemon because the scheduler owns synchronous
	// RunNext children that hold daemon resources.
	schedulerDone  chan struct{}
	schedulerErr   error
	schedulerStart bool
}

func main() {
	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	os.Exit(run(ctx, os.Args[1:], os.Stdout, os.Stderr))
}

func run(ctx context.Context, args []string, stdout, stderr io.Writer) int {
	if len(args) == 1 && args[0] == "--version" {
		_, _ = fmt.Fprintf(stdout, "factoryd %s\n", buildinfo.Current().Version())
		return 0
	}
	if len(args) == 1 && args[0] == "--build-identity" {
		if err := buildinfo.Current().WriteJSON(stdout); err != nil {
			return exitFailure
		}
		return 0
	}
	configuration, help, ok := parse(args)
	if help {
		_, _ = io.WriteString(stdout, usage)
		return 0
	}
	if !ok {
		_, _ = io.WriteString(stderr, usage)
		return exitUsage
	}
	if err := serve(ctx, configuration); err != nil {
		_, _ = fmt.Fprintf(stderr, "factoryd: %v\n", err)
		return exitFailure
	}
	return 0
}

func parse(args []string) (config, bool, bool) {
	if len(args) == 1 && (args[0] == "-h" || args[0] == "--help") {
		return config{}, true, true
	}
	result := config{browserAddress: defaultBrowserAddress, browserOrigins: []string{defaultBrowserOrigin}}
	developmentBrowserAddress := false
	for index := 0; index < len(args); {
		if index+1 >= len(args) {
			return config{}, false, false
		}
		name, value := args[index], args[index+1]
		switch name {
		case "--home":
			if result.home != "" || !validHome(value) {
				return config{}, false, false
			}
			result.home = value
		case "--git":
			if result.gitExecutable != "" || !validHome(value) {
				return config{}, false, false
			}
			result.gitExecutable = value
		case "--tool-path":
			if result.toolPath != "" || provider.ValidateToolPath(value) != nil {
				return config{}, false, false
			}
			result.toolPath = value
			result.toolPathExplicit = true
		case "--base-revision":
			if result.baseRevision != "" || !validBaseRevision(value) {
				return config{}, false, false
			}
			result.baseRevision = value
		case "--runner":
			if result.runnerExecutable != "" || !validHome(value) {
				return config{}, false, false
			}
			result.runnerExecutable = value
		case "--factoryctl":
			if result.factoryctlExecutable != "" || !validHome(value) {
				return config{}, false, false
			}
			result.factoryctlExecutable = value
		case "--relay-origin":
			if result.relayOrigin != "" || !validRelayOrigin(value) {
				return config{}, false, false
			}
			result.relayOrigin = value
		case "--development-browser-address":
			if developmentBrowserAddress || !validBrowserAddress(value, true) {
				return config{}, false, false
			}
			result.browserAddress = value
			developmentBrowserAddress = true
		case "--development-browser-origin":
			if !validBrowserOrigin(value) || len(result.browserOrigins) == 8 {
				return config{}, false, false
			}
			for _, present := range result.browserOrigins {
				if present == value {
					return config{}, false, false
				}
			}
			result.browserOrigins = append(result.browserOrigins, value)
		default:
			return config{}, false, false
		}
		index += 2
	}
	if result.home == "" {
		return config{}, false, false
	}
	if result.gitExecutable == "" {
		result.gitExecutable = defaultGitExecutable
	}
	if result.toolPath == "" {
		result.toolPath = defaultToolPath
	}
	if result.baseRevision == "" {
		result.baseRevision = defaultBaseRevision
	}
	return result, false, true
}

// validBaseRevision bounds the boot-owned revision policy string. Each
// attempt still resolves and validates it against the exact project
// repository; an unresolvable policy fails that run closed.
func validBaseRevision(value string) bool {
	return value != "" && len(value) <= maxHomeArgumentBytes && utf8.ValidString(value) && !strings.ContainsRune(value, 0) && !strings.HasPrefix(value, "-")
}

func validHome(value string) bool {
	return value != "/" && len(value) > 1 && len(value) <= maxHomeArgumentBytes && utf8.ValidString(value) && !strings.ContainsRune(value, 0) && filepath.IsAbs(value) && filepath.Clean(value) == value
}

func validBrowserAddress(value string, allowZero bool) bool {
	host, rawPort, err := net.SplitHostPort(value)
	if err != nil || host != "127.0.0.1" || rawPort == "" {
		return false
	}
	port, err := strconv.Atoi(rawPort)
	minimum := 1
	if allowZero {
		minimum = 0
	}
	return err == nil && port >= minimum && port <= 65535 && strconv.Itoa(port) == rawPort
}

// validRelayOrigin defers to the connector's own validator so the flag and
// the dial cannot disagree about what a relay origin is.
func validRelayOrigin(value string) bool {
	return len(value) <= 8192 && relayhost.ValidateRelayOrigin(value) == nil
}

func validBrowserOrigin(value string) bool {
	parsed, err := url.Parse(value)
	return err == nil && (parsed.Scheme == "https" || parsed.Scheme == "http") && parsed.Host != "" && parsed.User == nil && parsed.Path == "" && parsed.RawQuery == "" && parsed.Fragment == "" && parsed.String() == value && !strings.Contains(value, "*") && len(value) <= 8192
}

func serve(ctx context.Context, configuration config) error {
	if ctx == nil || !validHome(configuration.home) || !validBrowserAddress(configuration.browserAddress, true) || len(configuration.browserOrigins) == 0 ||
		!validHome(configuration.gitExecutable) || provider.ValidateToolPath(configuration.toolPath) != nil || !validBaseRevision(configuration.baseRevision) ||
		(configuration.runnerExecutable != "" && !validHome(configuration.runnerExecutable)) || (configuration.factoryctlExecutable != "" && !validHome(configuration.factoryctlExecutable)) ||
		(configuration.relayOrigin != "" && !validRelayOrigin(configuration.relayOrigin)) {
		return errors.New("invalid factoryd configuration")
	}
	for _, origin := range configuration.browserOrigins {
		if !validBrowserOrigin(origin) {
			return errors.New("invalid factoryd configuration")
		}
	}
	owner, err := openProcess(ctx, configuration)
	if err != nil {
		if errors.Is(err, cleanStartupCancellation) {
			return nil
		}
		return err
	}
	return owner.wait(ctx)
}

func openProcess(ctx context.Context, configuration config) (_ *process, resultErr error) {
	ownedContext, cancel := context.WithCancel(ctx)
	owner := &process{cancel: cancel, apiDone: make(chan struct{}), slots: make(chan struct{}, maxAPIHandlers)}
	keep := false
	defer func() {
		if !keep {
			cleanupErr := owner.close()
			resultErr = errors.Join(resultErr, cleanupErr)
			if cleanupErr == nil && errors.Is(resultErr, context.Canceled) {
				resultErr = errors.Join(resultErr, cleanStartupCancellation)
			}
		}
	}()

	var err error
	owner.home, err = install.OpenOperationalHome(ownedContext, configuration.home)
	if err != nil {
		return nil, err
	}
	startupPhase("home")
	store, err := owner.home.OpenStore(ownedContext)
	if err != nil {
		return nil, err
	}
	startupPhase("store")
	runtimes, err := owner.home.Runtimes()
	if err != nil {
		return nil, err
	}
	owner.runtimeParent, err = daemon.OpenRuntimeParent(ownedContext, runtimes, install.RuntimesPath(configuration.home))
	if err != nil {
		return nil, err
	}
	startupPhase("runtime parent")
	owner.supervisorSpec, err = deriveSupervisorSpec(configuration, owner.runtimeParent)
	if err != nil {
		return nil, err
	}
	startupPhase("supervisor spec")
	owner.daemon, err = daemon.NewDaemon(store)
	if err != nil {
		return nil, err
	}
	startupPhase("daemon")
	// The sweep runs to a quiet state before any listener opens so no client
	// can act on unrecovered durable state. A run the sweep leaves unresolved
	// is durable fail-closed residue, reported but never a boot refusal.
	dispositions, err := owner.daemon.RecoverAbandonedRuns(ownedContext, owner.runtimeParent, owner.supervisorSpec.ChangeParent)
	if err != nil {
		return nil, err
	}
	for _, disposition := range dispositions {
		if disposition.Err != nil {
			_, _ = fmt.Fprintf(recoveryLog, "factoryd: recovered run %s: %s: %v\n", disposition.RunID.String(), disposition.Action, disposition.Err)
			continue
		}
		_, _ = fmt.Fprintf(recoveryLog, "factoryd: recovered run %s: %s\n", disposition.RunID.String(), disposition.Action)
	}
	startupPhase("recovery sweep")
	owner.apiAuthority, err = owner.home.OpenLocalAPI(ownedContext)
	if err != nil {
		return nil, err
	}
	startupPhase("local API")
	owner.listener, err = api.Listen(owner.apiAuthority)
	if err != nil {
		return nil, err
	}
	startupPhase("listener")
	owner.browser, err = owner.daemon.ListenBrowser(configuration.browserAddress, configuration.browserOrigins)
	if err != nil {
		return nil, err
	}
	startupPhase("browser")
	if configuration.relayOrigin != "" {
		// The relay is a client of the listener above. It is started after the
		// listener exists so a relayed controller can never reach a daemon
		// with no loopback surface, and it never blocks boot on reachability.
		owner.relay, err = owner.daemon.DialRelay(ownedContext, configuration.relayOrigin, configuration.home, owner.browser.Addr())
		if err != nil {
			return nil, err
		}
		startupPhase("relay")
	}
	owner.apiStart = true
	go owner.accept(ownedContext, owner.listener)
	owner.supervisorSpec.UnsettledCompletion = func(id kernel.RunID, err error) {
		_, _ = fmt.Fprintf(recoveryLog, "factoryd: unsettled run %s: %v\n", id.String(), err)
	}
	owner.schedulerDone = make(chan struct{})
	owner.schedulerStart = true
	go func() {
		defer close(owner.schedulerDone)
		owner.schedulerErr = owner.daemon.RunScheduler(ownedContext, owner.supervisorSpec)
	}()
	startupPhase("scheduler")
	keep = true
	return owner, nil
}

func (owner *process) accept(ctx context.Context, listener *api.Listener) {
	defer close(owner.apiDone)
	for {
		select {
		case owner.slots <- struct{}{}:
		case <-ctx.Done():
			owner.apiErr = ctx.Err()
			return
		}
		connection, err := listener.Accept()
		if err != nil {
			<-owner.slots
			owner.apiErr = err
			return
		}
		owner.handlers.Add(1)
		go func() {
			defer owner.handlers.Done()
			defer func() { <-owner.slots }()
			handlerContext, cancel := context.WithTimeout(ctx, apiHandlerTimeout)
			defer cancel()
			_ = owner.daemon.HandleConnection(handlerContext, connection)
		}()
	}
}

func (owner *process) wait(ctx context.Context) error {
	if owner == nil || ctx == nil {
		return errors.New("invalid factoryd owner")
	}
	owner.lifecycleMu.Lock()
	if owner.waitDone != nil {
		done := owner.waitDone
		owner.lifecycleMu.Unlock()
		<-done
		owner.lifecycleMu.Lock()
		err := owner.waitErr
		owner.lifecycleMu.Unlock()
		return err
	}
	if owner.closeDone != nil {
		done := owner.closeDone
		owner.lifecycleMu.Unlock()
		<-done
		owner.lifecycleMu.Lock()
		err := owner.closeErr
		owner.lifecycleMu.Unlock()
		return err
	}
	if owner.browser == nil || owner.apiDone == nil || owner.schedulerDone == nil {
		owner.lifecycleMu.Unlock()
		return errors.New("invalid factoryd owner")
	}
	waitDone := make(chan struct{})
	owner.waitDone = waitDone
	apiDone, browser, schedulerDone := owner.apiDone, owner.browser, owner.schedulerDone
	owner.lifecycleMu.Unlock()
	var cause error
	select {
	case <-ctx.Done():
	case <-apiDone:
		owner.lifecycleMu.Lock()
		closing := owner.closeDone != nil
		owner.lifecycleMu.Unlock()
		if ctx.Err() == nil && !closing && !errors.Is(owner.apiErr, context.Canceled) {
			cause = fmt.Errorf("local API owner stopped: %w", owner.apiErr)
		}
	case <-browser.Done():
		owner.lifecycleMu.Lock()
		closing := owner.closeDone != nil
		owner.lifecycleMu.Unlock()
		if ctx.Err() == nil && !closing {
			if err := browser.Err(); err != nil {
				cause = err
			} else {
				cause = errors.New("browser owner stopped")
			}
		}
	case <-schedulerDone:
		owner.lifecycleMu.Lock()
		closing := owner.closeDone != nil
		owner.lifecycleMu.Unlock()
		if ctx.Err() == nil && !closing && !errors.Is(owner.schedulerErr, context.Canceled) {
			if err := owner.schedulerErr; err != nil {
				cause = fmt.Errorf("scheduler stopped: %w", err)
			} else {
				cause = errors.New("scheduler stopped")
			}
		}
	}
	result := errors.Join(cause, owner.close())
	owner.lifecycleMu.Lock()
	owner.waitErr = result
	close(owner.waitDone)
	owner.lifecycleMu.Unlock()
	return result
}

func (owner *process) close() error {
	if owner == nil {
		return nil
	}
	owner.lifecycleMu.Lock()
	if owner.closeDone != nil {
		done := owner.closeDone
		owner.lifecycleMu.Unlock()
		<-done
		owner.lifecycleMu.Lock()
		err := owner.closeErr
		owner.lifecycleMu.Unlock()
		return err
	}
	owner.closeDone = make(chan struct{})
	owner.lifecycleMu.Unlock()
	result := owner.shutdown()
	owner.lifecycleMu.Lock()
	owner.closeErr = result
	close(owner.closeDone)
	owner.lifecycleMu.Unlock()
	return result
}

func (owner *process) shutdown() error {
	if owner.cancel != nil {
		owner.cancel()
	}
	var result error
	listener := owner.listener
	if owner.listener != nil {
		result = errors.Join(result, listener.Close())
	} else if owner.apiAuthority != nil {
		result = errors.Join(result, owner.apiAuthority.Close())
	}
	if result == nil {
		owner.lifecycleMu.Lock()
		owner.listener = nil
		owner.apiAuthority = nil
		owner.lifecycleMu.Unlock()
	}
	if owner.apiStart {
		<-owner.apiDone
	}
	owner.handlers.Wait()
	if owner.schedulerStart {
		// The scheduler joins before the daemon closes: it owns synchronous
		// RunNext children whose convergence holds daemon resources.
		<-owner.schedulerDone
		owner.lifecycleMu.Lock()
		reported := owner.waitDone != nil
		owner.lifecycleMu.Unlock()
		if !reported && owner.schedulerErr != nil && !errors.Is(owner.schedulerErr, context.Canceled) {
			result = errors.Join(result, owner.schedulerErr)
		}
	}
	// The daemon closes the relay before the listeners it is a client of, so
	// shutdown here does not depend on this ordering.
	if owner.daemon != nil {
		if err := closeDaemon(owner.daemon); err != nil {
			result = errors.Join(result, err)
		} else {
			owner.lifecycleMu.Lock()
			owner.daemon = nil
			owner.browser = nil
			owner.relay = nil
			owner.lifecycleMu.Unlock()
		}
	}
	if result != nil {
		return result
	}
	if owner.runtimeParent != nil {
		if err := closeRuntimeParent(owner.runtimeParent); err != nil {
			result = errors.Join(result, err)
		} else {
			owner.lifecycleMu.Lock()
			owner.runtimeParent = nil
			owner.lifecycleMu.Unlock()
		}
	}
	if result != nil {
		return result
	}
	if owner.home != nil {
		result = errors.Join(result, owner.home.Close())
		if result == nil {
			owner.lifecycleMu.Lock()
			owner.home = nil
			owner.lifecycleMu.Unlock()
		}
	}
	return result
}

func startupPhase(name string) {
	if startupPhaseHook != nil {
		startupPhaseHook(name)
	}
}

// deriveSupervisorSpec composes and proves the production supervisor
// specification at boot. Executables are symlink-resolved before commitment
// because the per-attempt locator rejects any path that is not its own
// resolution; missing or unsafe inputs refuse the process.
func deriveSupervisorSpec(configuration config, parent *daemon.RuntimeParent) (daemon.SupervisorSpec, error) {
	runnerPath, factoryctlPath := configuration.runnerExecutable, configuration.factoryctlExecutable
	if runnerPath == "" || factoryctlPath == "" {
		self, err := selfExecutable()
		if err != nil {
			return daemon.SupervisorSpec{}, err
		}
		resolvedSelf, err := filepath.EvalSymlinks(self)
		if err != nil || !filepath.IsAbs(resolvedSelf) {
			return daemon.SupervisorSpec{}, errors.Join(errors.New("factoryd: self location is unresolvable"), err)
		}
		siblings := filepath.Dir(resolvedSelf)
		if runnerPath == "" {
			runnerPath = filepath.Join(siblings, runnerSiblingName)
		}
		if factoryctlPath == "" {
			factoryctlPath = filepath.Join(siblings, factoryctlSiblingName)
		}
	}
	gitExecutable, err := commitBootExecutable(configuration.gitExecutable)
	if err != nil {
		return daemon.SupervisorSpec{}, err
	}
	if !change.TrustedDeveloperGitPath(gitExecutable) {
		return daemon.SupervisorSpec{}, fmt.Errorf("factoryd: git executable %q is not the trusted Command Line Tools git", gitExecutable)
	}
	runnerExecutable, err := commitBootExecutable(runnerPath)
	if err != nil {
		return daemon.SupervisorSpec{}, err
	}
	factoryctlExecutable, err := commitBootExecutable(factoryctlPath)
	if err != nil {
		return daemon.SupervisorSpec{}, err
	}
	if err := provider.ValidateToolPath(configuration.toolPath); err != nil {
		return daemon.SupervisorSpec{}, err
	}
	accountHome, err := install.AccountHome()
	if err != nil {
		return daemon.SupervisorSpec{}, err
	}
	toolPath := configuration.toolPath
	if !configuration.toolPathExplicit {
		toolPath = filepath.Join(accountHome, ".local", "bin") + string(filepath.ListSeparator) + toolPath
	}
	if err := provider.ValidateToolPath(toolPath); err != nil {
		return daemon.SupervisorSpec{}, err
	}
	if !validBaseRevision(configuration.baseRevision) {
		return daemon.SupervisorSpec{}, errors.New("factoryd: invalid base revision policy")
	}
	return daemon.SupervisorSpec{
		RuntimeParent:        parent,
		ChangeParent:         install.ChangesPath(configuration.home),
		GitExecutable:        gitExecutable,
		BaseRevision:         configuration.baseRevision,
		AttemptSocket:        install.LocalAPISocketPath(configuration.home),
		RunnerExecutable:     runnerExecutable,
		FactoryctlExecutable: factoryctlExecutable,
		ToolPath:             toolPath,
		AccountHome:          accountHome,
	}, nil
}

func commitBootExecutable(path string) (string, error) {
	resolved, err := filepath.EvalSymlinks(path)
	if err != nil {
		return "", errors.Join(fmt.Errorf("factoryd: executable %q is unresolvable", path), err)
	}
	if _, err := runner.CommitExecutableLocator(resolved); err != nil {
		return "", errors.Join(fmt.Errorf("factoryd: executable %q was refused", path), err)
	}
	return resolved, nil
}
