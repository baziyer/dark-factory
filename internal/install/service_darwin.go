//go:build darwin

package install

import (
	"bytes"
	"context"
	"crypto/sha256"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"os/user"
	"path/filepath"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

const (
	launchctlPath         = "/bin/launchctl"
	launchctlNotFound     = 113
	launchctlNotFoundText = "113: Could not find specified service"
	launchctlOutputLimit  = 64 << 10
	launchctlTimeout      = 3 * time.Second
	servicePlistName      = serviceLabel + ".plist"
)

type launchctlResult struct {
	stdout   []byte
	stderr   []byte
	status   int
	err      error
	overflow bool
}

type launchctlRun func(context.Context, ...string) launchctlResult

type boundedCommandBuffer struct {
	buffer   bytes.Buffer
	overflow bool
}

func (buffer *boundedCommandBuffer) Write(value []byte) (int, error) {
	written := len(value)
	remaining := launchctlOutputLimit - buffer.buffer.Len()
	if remaining <= 0 {
		buffer.overflow = true
		return written, nil
	}
	if len(value) > remaining {
		buffer.overflow = true
		value = value[:remaining]
	}
	_, _ = buffer.buffer.Write(value)
	return written, nil
}

func runLaunchctl(ctx context.Context, arguments ...string) launchctlResult {
	return runLaunchctlBinary(ctx, launchctlPath, arguments...)
}

func runLaunchctlBinary(ctx context.Context, binary string, arguments ...string) launchctlResult {
	if ctx == nil {
		return launchctlResult{status: -1, err: errors.New("nil launchctl context")}
	}
	callContext, cancel := context.WithTimeout(ctx, launchctlTimeout)
	defer cancel()
	command := exec.CommandContext(callContext, binary, arguments...)
	command.Env = []string{"LANG=C", "LC_ALL=C", "PATH=/usr/bin:/bin"}
	command.WaitDelay = 250 * time.Millisecond
	var stdout, stderr boundedCommandBuffer
	command.Stdout = &stdout
	command.Stderr = &stderr
	err := command.Run()
	result := launchctlResult{
		stdout: stdout.buffer.Bytes(), stderr: stderr.buffer.Bytes(), status: 0,
		err: err, overflow: stdout.overflow || stderr.overflow,
	}
	if err == nil {
		return result
	}
	if callContext.Err() != nil {
		result.err = callContext.Err()
		result.status = -1
		return result
	}
	var exit *exec.ExitError
	if errors.As(err, &exit) && exit.ProcessState != nil {
		result.status = exit.ExitCode()
		result.err = nil
		return result
	}
	result.status = -1
	return result
}

type launchctlObservation struct {
	present bool
	pid     int
}

type serviceInspection struct {
	status      ServiceStatus
	observation launchctlObservation
	// relayOrigin is the origin the receipt says the installed plist was
	// rendered from. Install compares it to the requested one so a changed
	// relay origin refuses instead of silently keeping the old plist.
	relayOrigin string
	// developmentBrowserAddress is the receipt-bound requested address, which
	// may be port zero even though the live listener chose another port.
	developmentBrowserAddress string
}

func inspectServiceForAccount(ctx context.Context, home string, config ServiceConfig, launchctl launchctlRun) (status ServiceStatus, resultErr error) {
	if ctx == nil || launchctl == nil {
		return ServiceStatus{}, fmt.Errorf("%w: invalid status request", ErrServiceAmbiguous)
	}
	userHome, err := AccountHome()
	if err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return inspectServiceAtHome(ctx, home, userHome, config, launchctl)
}

func inspectService(ctx context.Context, home, userHome string, launchctl launchctlRun) (ServiceStatus, error) {
	return inspectServiceAtHome(ctx, home, userHome, DefaultServiceConfig(), launchctl)
}

// inspectServiceAtHome is a package-private test seam. Production status uses
// accountHome, never a caller-provided HOME value.
func inspectServiceAtHome(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun) (status ServiceStatus, resultErr error) {
	if ctx == nil || launchctl == nil || !config.valid() {
		return ServiceStatus{}, fmt.Errorf("%w: invalid status request", ErrServiceAmbiguous)
	}
	homeCapability, err := openServiceHomeCapability(ctx, home)
	if err != nil {
		return ServiceStatus{}, err
	}
	defer func() {
		if closeErr := homeCapability.close(); closeErr != nil {
			resultErr = errors.Join(resultErr, ErrServiceAmbiguous, closeErr)
		}
	}()
	inspection, err := inspectServiceWithCapabilityAt(ctx, home, userHome, config, launchctl, homeCapability)
	return inspection.status, err
}

// inspectServiceWithCapabilityAt performs the external service observation
// against one already-retained home. Mutation callers reuse the capability
// that carries their directory lock instead of taking a second home snapshot.
func inspectServiceWithCapabilityAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun, homeCapability *serviceHomeCapability) (inspection serviceInspection, resultErr error) {
	if ctx == nil || launchctl == nil || !config.valid() || homeCapability == nil || homeCapability.home == nil {
		return serviceInspection{}, fmt.Errorf("%w: invalid status request", ErrServiceAmbiguous)
	}
	plistDirectory := config.PlistDirectory
	if plistDirectory == "" {
		plistDirectory = filepath.Join(userHome, "Library", "LaunchAgents")
	}
	walkRoot := userHome
	if config.PlistDirectory != "" {
		walkRoot = config.PlistDirectory
	}
	userDirectory, err := openServiceDirectory(walkRoot)
	if err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, fmt.Errorf("%w: user home", ErrServiceAmbiguous)
	}
	defer func() {
		if closeErr := userDirectory.close(); closeErr != nil {
			resultErr = errors.Join(resultErr, ErrServiceAmbiguous, closeErr)
		}
	}()
	// The receipt is read before the plist census because it names the exact
	// argument list the plist was rendered from: without it, an installation
	// carrying --relay-origin could not be re-rendered, and so could neither
	// be recognized nor uninstalled.
	receipt, receiptPresent, receiptErr := readServiceReceipt(home)
	renderedOrigin := ""
	renderedDevelopmentBrowserAddress := ""
	if receiptErr == nil && receiptPresent {
		renderedOrigin = receipt.RelayOrigin
		renderedDevelopmentBrowserAddress = receipt.DevelopmentBrowserAddress
	}
	plist, err := inspectServicePlist(userDirectory, home, config, renderedOrigin, renderedDevelopmentBrowserAddress)
	if err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}
	if err := userDirectory.recheck(); err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}

	uid := os.Geteuid()
	service := "gui/" + strconv.Itoa(uid) + "/" + config.Label
	plistPath := filepath.Join(plistDirectory, config.plistName())
	programPath := serviceProgramPath(home)
	observation, err := observeLaunchctl(ctx, launchctl, service, plistPath, programPath)
	if err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, err
	}
	// launchctl is an external observation boundary: the exact staging name
	// must be censused again before its result can influence status.
	if err := homeCapability.recheck(ctx); err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}
	if err := homeCapability.stageAbsent(); err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}
	secondPlist, err := inspectServicePlist(userDirectory, home, config, renderedOrigin, renderedDevelopmentBrowserAddress)
	if err != nil || secondPlist != plist {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}
	if err := userDirectory.recheck(); err != nil {
		return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
	}
	err = receiptErr
	if !plist.present && !observation.present {
		if err != nil || receiptPresent {
			// Install residue without a live job is not provable absence.
			return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, ErrServiceResidue, err)
		}
		if residueErr := rejectServiceDirectoryResidue(home); residueErr != nil {
			return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, residueErr)
		}
		// Keep this check adjacent to the only absent projection. A stage that
		// appears during the second plist census must not be reported as absent.
		if err := homeCapability.recheck(ctx); err != nil {
			return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
		}
		if err := homeCapability.stageAbsent(); err != nil {
			return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, err)
		}
		return serviceInspection{status: ServiceStatus{State: ServiceAbsent}}, nil
	}
	if err == nil && receiptPresent && plist.present {
		if matchErr := receiptMatchesInstallation(receipt, home, config, plistPath); matchErr == nil {
			if observation.present && observation.pid > 0 {
				return serviceInspection{status: ServiceStatus{State: ServiceRunning, PID: observation.pid}, observation: observation, relayOrigin: renderedOrigin, developmentBrowserAddress: renderedDevelopmentBrowserAddress}, nil
			}
			// A loaded-but-idle job and an unloaded plist are both restartable
			// installations; neither grants process authority.
			return serviceInspection{status: ServiceStatus{State: ServiceInstalled}, observation: observation, relayOrigin: renderedOrigin, developmentBrowserAddress: renderedDevelopmentBrowserAddress}, nil
		} else {
			return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous}}, errors.Join(ErrServiceAmbiguous, matchErr)
		}
	}
	return serviceInspection{status: ServiceStatus{State: ServiceAmbiguous, PID: observation.pid}}, errors.Join(ErrServiceAmbiguous, err)
}

type serviceHomeCapability struct {
	parent *homeParent
	home   *os.File
	base   string
	stat   unix.Stat_t
	image  serviceHomeImage
}

func openServiceHomeCapability(ctx context.Context, path string) (_ *serviceHomeCapability, resultErr error) {
	if ctx == nil {
		return nil, context.Canceled
	}
	parentPath, base, err := splitHome(path)
	if err != nil {
		return nil, err
	}
	parent, err := openParent(parentPath)
	if err != nil {
		return nil, err
	}
	capability := &serviceHomeCapability{parent: parent, base: base}
	defer func() {
		if resultErr != nil {
			resultErr = errors.Join(resultErr, capability.close())
		}
	}()
	if err := capability.stageAbsent(); err != nil {
		return nil, err
	}
	var expected unix.Stat_t
	var first serviceHomeImage
	for pass := 0; pass < 2; pass++ {
		home, stat, err := openDirectoryMember(parent.file, base)
		if err != nil {
			return nil, fmt.Errorf("open service home: %w", err)
		}
		if pass == 0 {
			expected = stat
		} else if !sameServiceRootStat(expected, stat) {
			_ = home.Close()
			return nil, fmt.Errorf("%w: service home identity changed", ErrInvalidHome)
		}
		image, inspectErr := snapshotServiceHome(ctx, home)
		if inspectErr == nil && pass == 0 {
			first = image
		} else if inspectErr == nil {
			inspectErr = sameServiceHomeImage(first, image)
		}
		if inspectErr == nil {
			inspectErr = recheckBinding(parent.file, base, stat)
		}
		if inspectErr != nil {
			_ = home.Close()
			return nil, inspectErr
		}
		if pass == 0 {
			if closeErr := home.Close(); closeErr != nil {
				return nil, closeErr
			}
		} else {
			capability.home, capability.stat, capability.image = home, stat, image
		}
		if err := parent.recheck(); err != nil {
			return nil, err
		}
		if err := ctx.Err(); err != nil {
			return nil, err
		}
	}
	return capability, nil
}

func (capability *serviceHomeCapability) stageAbsent() error {
	if capability == nil || capability.parent == nil {
		return ErrInvalidHome
	}
	if err := capability.parent.recheck(); err != nil {
		return err
	}
	return rejectIfPresent(capability.parent.file, "."+capability.base+stageSuffix, "staging path")
}

func (capability *serviceHomeCapability) recheck(ctx context.Context) error {
	if capability == nil || capability.parent == nil || capability.home == nil {
		return ErrInvalidHome
	}
	if err := capability.parent.recheck(); err != nil {
		return err
	}
	var current, binding unix.Stat_t
	if err := unix.Fstat(int(capability.home.Fd()), &current); err != nil || !sameServiceRootStat(capability.stat, current) {
		return fmt.Errorf("%w: service home descriptor changed", ErrInvalidHome)
	}
	if err := unix.Fstatat(int(capability.parent.file.Fd()), capability.base, &binding, unix.AT_SYMLINK_NOFOLLOW); err != nil || !sameServiceRootStat(capability.stat, binding) {
		return fmt.Errorf("%w: service home binding changed", ErrInvalidHome)
	}
	image, err := snapshotServiceHome(ctx, capability.home)
	if err != nil {
		return err
	}
	return sameServiceHomeImage(capability.image, image)
}

func (capability *serviceHomeCapability) close() error {
	if capability == nil {
		return nil
	}
	var result error
	if capability.home != nil {
		result = errors.Join(result, capability.home.Close())
		capability.home = nil
	}
	if capability.parent != nil {
		result = errors.Join(result, capability.parent.close())
		capability.parent = nil
	}
	return result
}

// AccountHome returns the current UID's exact account-record home without
// trusting the caller's environment.
func AccountHome() (string, error) {
	account, err := user.Current()
	if err != nil || account == nil || account.HomeDir == "" || account.Uid != strconv.Itoa(os.Geteuid()) || !validServicePath(account.HomeDir) {
		return "", errors.New("current account home is not exact")
	}
	return account.HomeDir, nil
}

type serviceHomeImage struct {
	root        serviceIdentity
	files       map[string]serviceMemberSnapshot
	directories map[string]serviceIdentity
}

type serviceIdentity struct {
	identity
	ctime unix.Timespec
}

type serviceMemberSnapshot struct {
	serviceIdentity
	digest [sha256.Size]byte
}

func toServiceIdentity(stat unix.Stat_t) serviceIdentity {
	return serviceIdentity{identity: toIdentity(stat), ctime: stat.Ctim}
}

// volatileServiceMember names the members a LIVE daemon legitimately rewrites
// while a status observation runs. Their bytes and times are not part of the
// service-status invariant; their filesystem identity and bounds are.
func volatileServiceMember(name string) bool {
	return name == databaseName || name == databaseName+"-wal" || name == databaseName+"-shm"
}

// reducedServiceIdentity strips volatile stat dimensions (size, times, and
// link count driven by SQLite checkpointing or child entry churn) while
// keeping the identity that detects replacement and authority changes.
func reducedServiceIdentity(stat unix.Stat_t) serviceIdentity {
	reduced := toServiceIdentity(stat)
	reduced.identity.size = 0
	reduced.identity.nlink = 0
	reduced.ctime = unix.Timespec{}
	return reduced
}

// sameServiceRootStat compares a directory's identity and authority —
// device, inode, mode, owner — without the fields unrelated sibling
// activity legitimately churns (timestamps, size, link count as entries
// come and go). Every directory recheck uses it; regular files keep the
// full sameServiceStat comparison, where byte-stability is meaningful.
//
// Dropping Ctim costs one canary: a transient tamper that renames the
// directory away, swaps a decoy in and renames the original back leaves a
// later recheck clean, where the old comparison would have caught the
// changed ctime afterwards. Detection inside the window is unaffected — the
// swapped inode still fails the identity and binding probes — and ctime was
// unusable as a tamper signal here precisely because it churns for the
// legitimate reason this relaxation exists.
func sameServiceRootStat(left, right unix.Stat_t) bool {
	return left.Dev == right.Dev && left.Ino == right.Ino && left.Mode == right.Mode && left.Uid == right.Uid && left.Gid == right.Gid
}

func sameServiceIdentity(left, right serviceIdentity) bool {
	return left == right
}

func snapshotServiceHome(ctx context.Context, home *os.File) (serviceHomeImage, error) {
	if ctx == nil {
		return serviceHomeImage{}, context.Canceled
	}
	var rootBefore unix.Stat_t
	if err := unix.Fstat(int(home.Fd()), &rootBefore); err != nil {
		return serviceHomeImage{}, err
	}
	if err := exactDirectory(home); err != nil {
		return serviceHomeImage{}, err
	}
	if _, err := home.Seek(0, io.SeekStart); err != nil {
		return serviceHomeImage{}, err
	}
	names, err := readOperationalCensus(home)
	if err != nil {
		return serviceHomeImage{}, err
	}
	if err := inspectFile(ctx, home, formatName, []byte(formatBytes)); err != nil {
		return serviceHomeImage{}, err
	}
	if err := inspectToken(home); err != nil {
		return serviceHomeImage{}, err
	}
	if err := inspectLockPair(home); err != nil {
		return serviceHomeImage{}, err
	}
	image := serviceHomeImage{root: reducedServiceIdentity(rootBefore), files: make(map[string]serviceMemberSnapshot, len(names)-2), directories: make(map[string]serviceIdentity, 2)}
	for _, name := range []string{formatName, databaseName, tokenName, lockName, lockAnchorName, databaseName + "-wal", databaseName + "-shm"} {
		if !names[name] {
			continue
		}
		var file *os.File
		var stat unix.Stat_t
		var err error
		if name == lockName || name == lockAnchorName {
			file, stat, err = openLockMember(home, name)
		} else {
			file, stat, err = openMember(home, name)
		}
		if err != nil {
			return serviceHomeImage{}, err
		}
		minimum, maximum := int64(0), int64(0)
		switch name {
		case formatName:
			minimum, maximum = int64(len(formatBytes)), int64(len(formatBytes))
		case tokenName:
			minimum, maximum = operatorTokenBytes, operatorTokenBytes
		case lockName, lockAnchorName:
			minimum, maximum = 0, 0
		case databaseName:
			minimum, maximum = 100, operationalMaxDatabaseBytes
		default:
			minimum, maximum = operationalSidecarBounds(name)
		}
		if volatileServiceMember(name) {
			closeErr := file.Close()
			if stat.Size < minimum || stat.Size > maximum || closeErr != nil {
				return serviceHomeImage{}, fmt.Errorf("%w: live member %s bounds", ErrInvalidHome, name)
			}
			image.files[name] = serviceMemberSnapshot{serviceIdentity: reducedServiceIdentity(stat)}
			continue
		}
		digest, digestErr := digestMember(ctx, file, stat.Size, minimum, maximum)
		bindingErr := recheckBinding(home, name, stat)
		closeErr := file.Close()
		if digestErr != nil || bindingErr != nil || closeErr != nil {
			return serviceHomeImage{}, errors.Join(digestErr, bindingErr, closeErr)
		}
		image.files[name] = serviceMemberSnapshot{serviceIdentity: toServiceIdentity(stat), digest: digest}
	}
	for _, name := range []string{runtimesName, changesName} {
		directory, stat, err := openDirectoryMember(home, name)
		if err != nil {
			return serviceHomeImage{}, err
		}
		closeErr := directory.Close()
		if closeErr != nil {
			return serviceHomeImage{}, closeErr
		}
		image.directories[name] = reducedServiceIdentity(stat)
	}
	var rootAfter unix.Stat_t
	if err := unix.Fstat(int(home.Fd()), &rootAfter); err != nil {
		return serviceHomeImage{}, err
	}
	if !sameServiceIdentity(reducedServiceIdentity(rootBefore), reducedServiceIdentity(rootAfter)) {
		return serviceHomeImage{}, fmt.Errorf("%w: service home identity changed", ErrInvalidHome)
	}
	if err := ctx.Err(); err != nil {
		return serviceHomeImage{}, err
	}
	return image, nil
}

func sameServiceHomeImage(left, right serviceHomeImage) error {
	if !sameServiceIdentity(left.root, right.root) {
		return fmt.Errorf("%w: service home identity changed", ErrInvalidHome)
	}
	if len(left.directories) != len(right.directories) {
		return fmt.Errorf("%w: service home census changed", ErrInvalidHome)
	}
	names := map[string]bool{}
	for name := range left.files {
		names[name] = true
	}
	for name := range right.files {
		names[name] = true
	}
	for name := range names {
		expected, inLeft := left.files[name]
		actual, inRight := right.files[name]
		if !inLeft || !inRight {
			// A live daemon creates and removes the SQLite sidecars between
			// passes; every durable member must hold its census position.
			if name == databaseName+"-wal" || name == databaseName+"-shm" {
				continue
			}
			return fmt.Errorf("%w: service home census changed", ErrInvalidHome)
		}
		if actual != expected {
			return fmt.Errorf("%w: service home member changed", ErrInvalidHome)
		}
	}
	for name, expected := range left.directories {
		if right.directories[name] != expected {
			return fmt.Errorf("%w: service home directory changed", ErrInvalidHome)
		}
	}
	return nil
}

func observeLaunchctl(ctx context.Context, launchctl launchctlRun, service, plistPath, programPath string) (launchctlObservation, error) {
	result := launchctl(ctx, "print", service)
	if result.overflow {
		return launchctlObservation{}, fmt.Errorf("%w: output exceeded bound", ErrServiceLaunchctl)
	}
	if result.err != nil {
		return launchctlObservation{}, errors.Join(ErrServiceLaunchctl, result.err)
	}
	if result.status == 0 {
		if len(bytes.TrimSpace(result.stderr)) != 0 {
			return launchctlObservation{}, fmt.Errorf("%w: successful print carried stderr", ErrServiceLaunchctl)
		}
		pid, err := parseLaunchctlPrint(result.stdout, service, plistPath, programPath)
		if err != nil {
			return launchctlObservation{}, err
		}
		return launchctlObservation{present: true, pid: pid}, nil
	}
	if result.status != launchctlNotFound {
		return launchctlObservation{}, fmt.Errorf("%w: print status %d", ErrServiceLaunchctl, result.status)
	}
	if !validNotFoundStderr(result.stderr, service) {
		return launchctlObservation{}, fmt.Errorf("%w: not-found print carried unexpected stderr", ErrServiceLaunchctl)
	}
	classification := launchctl(ctx, "error", strconv.Itoa(result.status))
	if classification.overflow || classification.err != nil || classification.status != 0 || strings.TrimSpace(string(classification.stdout)) != launchctlNotFoundText || len(bytes.TrimSpace(classification.stderr)) != 0 {
		if classification.err != nil {
			return launchctlObservation{}, errors.Join(ErrServiceLaunchctl, classification.err)
		}
		return launchctlObservation{}, fmt.Errorf("%w: not-found classification mismatch", ErrServiceLaunchctl)
	}
	return launchctlObservation{}, nil
}

// validNotFoundStderr accepts exactly the two not-found stderr shapes real
// launchd produces: nothing, or the fixed two-line diagnostic naming this
// exact query's label and uid. Anything else is not authoritative.
func validNotFoundStderr(stderr []byte, service string) bool {
	trimmed := strings.TrimSpace(string(stderr))
	if trimmed == "" {
		return true
	}
	slash := strings.LastIndexByte(service, '/')
	if slash < 0 {
		return false
	}
	label := service[slash+1:]
	uid := strconv.Itoa(os.Geteuid())
	want := "Bad request.\nCould not find service \"" + label + "\" in domain for user gui: " + uid
	return trimmed == want
}

func parseLaunchctlPrint(output []byte, service, plistPath, programPath string) (int, error) {
	if len(output) == 0 || len(output) > launchctlOutputLimit || !utf8.Valid(output) || bytes.IndexByte(output, 0) >= 0 {
		return 0, fmt.Errorf("%w: malformed print output", ErrServiceLaunchctl)
	}
	lines := strings.Split(strings.TrimSuffix(string(output), "\n"), "\n")
	if len(lines) < 3 || len(lines) > 4096 || strings.TrimSpace(lines[0]) != service+" = {" || strings.TrimSpace(lines[len(lines)-1]) != "}" {
		return 0, fmt.Errorf("%w: malformed service envelope", ErrServiceLaunchctl)
	}
	fields := make(map[string]string, 4)
	nesting := []byte{'{'}
	for _, line := range lines[1 : len(lines)-1] {
		trimmed := strings.TrimSpace(line)
		if trimmed == "" {
			// Real launchd separates sections with blank lines.
			continue
		}
		if len(nesting) == 1 {
			rawKey, rawValue, found := strings.Cut(trimmed, "=")
			key, value := strings.TrimSpace(rawKey), strings.TrimSpace(rawValue)
			recognized := false
			for _, authorityKey := range []string{"path", "state", "program", "pid"} {
				if key != authorityKey {
					continue
				}
				recognized = true
				if !found || value == "" {
					return 0, fmt.Errorf("%w: malformed %s", ErrServiceLaunchctl, key)
				}
				if _, duplicate := fields[key]; duplicate {
					return 0, fmt.Errorf("%w: duplicate %s", ErrServiceLaunchctl, key)
				}
				fields[key] = value
			}
			if !recognized && (!found || !validLaunchctlFieldName(key) || value == "") {
				return 0, fmt.Errorf("%w: unknown service field", ErrServiceLaunchctl)
			}
		}
		if err := updateLaunchctlNesting(&nesting, trimmed); err != nil {
			return 0, err
		}
		if len(nesting) > 32 {
			return 0, fmt.Errorf("%w: service nesting exceeded bound", ErrServiceLaunchctl)
		}
	}
	if len(nesting) != 1 {
		return 0, fmt.Errorf("%w: unclosed service value", ErrServiceLaunchctl)
	}
	if fields["path"] != plistPath || fields["program"] != programPath {
		return 0, fmt.Errorf("%w: foreign launchd ownership", ErrServiceLaunchctl)
	}
	switch fields["state"] {
	case "running":
		pid, err := strconv.ParseInt(fields["pid"], 10, 32)
		if err != nil || pid <= 1 {
			return 0, fmt.Errorf("%w: invalid launchd pid", ErrServiceLaunchctl)
		}
		return int(pid), nil
	case "not running":
		if _, present := fields["pid"]; present {
			return 0, fmt.Errorf("%w: stopped service carries pid", ErrServiceLaunchctl)
		}
		return 0, nil
	default:
		return 0, fmt.Errorf("%w: unknown launchd state", ErrServiceLaunchctl)
	}
}

func validLaunchctlFieldName(value string) bool {
	if value == "" {
		return false
	}
	for _, character := range value {
		if character != ' ' && character != '-' && character != '_' && character != '(' && character != ')' && (character < '0' || character > '9') && (character < 'A' || character > 'Z') && (character < 'a' || character > 'z') {
			return false
		}
	}
	return true
}

func updateLaunchctlNesting(nesting *[]byte, line string) error {
	trimmed := strings.TrimSpace(line)
	if len(trimmed) == 1 {
		switch trimmed[0] {
		case '(', '[', '{':
			*nesting = append(*nesting, trimmed[0])
			return nil
		case ')', ']', '}':
			return closeLaunchctlNesting(nesting, trimmed[0])
		}
	}
	_, value, found := strings.Cut(trimmed, " = ")
	if found {
		switch strings.TrimSpace(value) {
		case "(", "[", "{":
			*nesting = append(*nesting, strings.TrimSpace(value)[0])
		case ")", "]", "}":
			return closeLaunchctlNesting(nesting, strings.TrimSpace(value)[0])
		}
	}
	return nil
}

func closeLaunchctlNesting(nesting *[]byte, close byte) error {
	if len(*nesting) <= 1 || (*nesting)[len(*nesting)-1] != matchingLaunchctlDelimiter(close) {
		return fmt.Errorf("%w: mismatched service delimiter", ErrServiceLaunchctl)
	}
	*nesting = (*nesting)[:len(*nesting)-1]
	return nil
}

func matchingLaunchctlDelimiter(close byte) byte {
	switch close {
	case ')':
		return '('
	case ']':
		return '['
	default:
		return '{'
	}
}

type serviceDirectory struct {
	files []*os.File
	names []string
	stats []unix.Stat_t
}

func openServiceDirectory(path string) (*serviceDirectory, error) {
	if !validServicePath(path) || path == "/" {
		return nil, errors.New("invalid service directory path")
	}
	rootFD, err := unix.Open("/", unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil, err
	}
	root := os.NewFile(uintptr(rootFD), "/")
	if root == nil {
		_ = unix.Close(rootFD)
		return nil, errors.New("invalid service root descriptor")
	}
	directory := &serviceDirectory{files: []*os.File{root}, names: []string{""}}
	parts := strings.Split(strings.TrimPrefix(path, "/"), "/")
	for _, name := range parts {
		if name == "" || name == "." || name == ".." || len(name) > maxNameSize {
			_ = directory.close()
			return nil, errors.New("invalid service directory component")
		}
		fd, openErr := unix.Openat(int(directory.files[len(directory.files)-1].Fd()), name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
		if openErr != nil {
			_ = directory.close()
			return nil, openErr
		}
		file := os.NewFile(uintptr(fd), name)
		if file == nil {
			_ = unix.Close(fd)
			_ = directory.close()
			return nil, errors.New("invalid service directory descriptor")
		}
		directory.files = append(directory.files, file)
		directory.names = append(directory.names, name)
	}
	for _, file := range directory.files {
		var stat unix.Stat_t
		if err := unix.Fstat(int(file.Fd()), &stat); err != nil {
			_ = directory.close()
			return nil, err
		}
		if stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Nlink < 2 {
			_ = directory.close()
			return nil, errors.New("service path component is not a stable directory")
		}
		directory.stats = append(directory.stats, stat)
	}
	final := directory.stats[len(directory.stats)-1]
	if final.Uid != uint32(os.Geteuid()) || final.Mode&0o022 != 0 {
		_ = directory.close()
		return nil, errors.New("service directory is not owned and protected")
	}
	if err := directory.recheck(); err != nil {
		_ = directory.close()
		return nil, err
	}
	return directory, nil
}

func (directory *serviceDirectory) recheck() error {
	for index, file := range directory.files {
		var current unix.Stat_t
		if err := unix.Fstat(int(file.Fd()), &current); err != nil {
			return err
		}
		if !sameServiceRootStat(directory.stats[index], current) {
			return errors.New("service directory identity changed")
		}
		if index == 0 {
			continue
		}
		var binding unix.Stat_t
		if err := unix.Fstatat(int(directory.files[index-1].Fd()), directory.names[index], &binding, unix.AT_SYMLINK_NOFOLLOW); err != nil || !sameServiceRootStat(directory.stats[index], binding) {
			return errors.New("service directory binding changed")
		}
	}
	return nil
}

func (directory *serviceDirectory) close() error {
	var result error
	for index := len(directory.files) - 1; index >= 0; index-- {
		result = errors.Join(result, directory.files[index].Close())
	}
	return result
}

func sameServiceStat(left, right unix.Stat_t) bool {
	return left.Dev == right.Dev && left.Ino == right.Ino && left.Mode == right.Mode && left.Uid == right.Uid && left.Gid == right.Gid && left.Nlink == right.Nlink && left.Size == right.Size && left.Mtim == right.Mtim && left.Ctim == right.Ctim
}

type servicePlistObservation struct {
	present             bool
	libraryPresent      bool
	launchAgentsPresent bool
	library             identity
	launchAgents        identity
}

type servicePlistBinding struct {
	parent *os.File
	child  *os.File
	name   string
	stat   unix.Stat_t
}

func inspectServicePlist(userHome *serviceDirectory, home string, config ServiceConfig, relayOrigin, developmentBrowserAddress string) (observation servicePlistObservation, resultErr error) {
	parent := userHome.files[len(userHome.files)-1]
	var children []*os.File
	var bindings []servicePlistBinding
	defer func() {
		for index := len(children) - 1; index >= 0; index-- {
			if closeErr := children[index].Close(); closeErr != nil {
				resultErr = errors.Join(resultErr, ErrServicePlist, closeErr)
			}
		}
	}()
	descent := []string{"Library", "LaunchAgents"}
	if config.PlistDirectory != "" {
		// The isolation override IS the plist directory; there is no descent.
		descent = nil
	}
	for index, name := range descent {
		fd, err := unix.Openat(int(parent.Fd()), name, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
		if errors.Is(err, unix.ENOENT) {
			if checkErr := recheckServicePlistParents(userHome, bindings); checkErr != nil {
				return servicePlistObservation{}, checkErr
			}
			return observation, nil
		}
		if err != nil {
			return servicePlistObservation{}, fmt.Errorf("%w: plist parent", ErrServicePlist)
		}
		child := os.NewFile(uintptr(fd), name)
		if child == nil {
			_ = unix.Close(fd)
			return servicePlistObservation{}, fmt.Errorf("%w: invalid plist parent descriptor", ErrServicePlist)
		}
		children = append(children, child)
		var stat unix.Stat_t
		if err := unix.Fstat(fd, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) || stat.Nlink < 2 || stat.Mode&0o022 != 0 {
			return servicePlistObservation{}, fmt.Errorf("%w: plist parent authority", ErrServicePlist)
		}
		bindings = append(bindings, servicePlistBinding{parent: parent, child: child, name: name, stat: stat})
		if index == 0 {
			observation.libraryPresent = true
			observation.library = toIdentity(stat)
		} else {
			observation.launchAgentsPresent = true
			observation.launchAgents = toIdentity(stat)
		}
		parent = child
	}
	fd, err := unix.Openat(int(parent.Fd()), config.plistName(), unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if errors.Is(err, unix.ENOENT) {
		if checkErr := recheckServicePlistParents(userHome, bindings); checkErr != nil {
			return servicePlistObservation{}, checkErr
		}
		return observation, nil
	}
	if err != nil {
		return servicePlistObservation{}, fmt.Errorf("%w: open plist", ErrServicePlist)
	}
	file := os.NewFile(uintptr(fd), config.plistName())
	if file == nil {
		_ = unix.Close(fd)
		return servicePlistObservation{}, fmt.Errorf("%w: invalid plist descriptor", ErrServicePlist)
	}
	children = append(children, file)
	var before unix.Stat_t
	if err := unix.Fstat(fd, &before); err != nil || before.Mode&unix.S_IFMT != unix.S_IFREG || before.Mode&0o7777 != 0o600 || before.Uid != uint32(os.Geteuid()) || before.Nlink != 1 || before.Size <= 0 || before.Size > launchctlOutputLimit {
		return servicePlistObservation{}, fmt.Errorf("%w: plist metadata", ErrServicePlist)
	}
	expected, _, err := ServicePlist(home, config.Label, relayOrigin, developmentBrowserAddress)
	if err != nil || int64(len(expected)) != before.Size {
		return servicePlistObservation{}, fmt.Errorf("%w: plist size", ErrServicePlist)
	}
	body, err := io.ReadAll(io.LimitReader(file, launchctlOutputLimit+1))
	if err != nil || !bytes.Equal(body, expected) {
		return servicePlistObservation{}, fmt.Errorf("%w: plist bytes", ErrServicePlist)
	}
	var after, binding unix.Stat_t
	if err := unix.Fstat(fd, &after); err != nil || !sameServiceStat(before, after) {
		return servicePlistObservation{}, fmt.Errorf("%w: plist identity changed", ErrServicePlist)
	}
	if err := unix.Fstatat(int(parent.Fd()), config.plistName(), &binding, unix.AT_SYMLINK_NOFOLLOW); err != nil || !sameServiceStat(before, binding) {
		return servicePlistObservation{}, fmt.Errorf("%w: plist binding changed", ErrServicePlist)
	}
	if err := recheckServicePlistParents(userHome, bindings); err != nil {
		return servicePlistObservation{}, err
	}
	observation.present = true
	return observation, nil
}

func recheckServicePlistParents(userHome *serviceDirectory, bindings []servicePlistBinding) error {
	if err := userHome.recheck(); err != nil {
		return fmt.Errorf("%w: plist home binding", ErrServicePlist)
	}
	for _, binding := range bindings {
		var current, bound unix.Stat_t
		if err := unix.Fstat(int(binding.child.Fd()), &current); err != nil {
			return fmt.Errorf("%w: plist parent binding changed", ErrServicePlist)
		}
		if err := unix.Fstatat(int(binding.parent.Fd()), binding.name, &bound, unix.AT_SYMLINK_NOFOLLOW); err != nil {
			return fmt.Errorf("%w: plist parent binding changed", ErrServicePlist)
		}
		if !sameServiceRootStat(binding.stat, current) || !sameServiceRootStat(binding.stat, bound) {
			return fmt.Errorf("%w: plist parent %s binding changed", ErrServicePlist, binding.name)
		}
	}
	return nil
}
