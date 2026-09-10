//go:build darwin

package install

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"time"

	"golang.org/x/sys/unix"
)

const (
	serviceBinaryMaxBytes  = int64(1) << 30
	serviceBootoutPatience = 5 * time.Second
)

var serviceBinaryNames = [3]string{"factoryd", "factoryctl", "factory-runner"}

// withServiceMutation serializes every lifecycle mutation on the exact home
// directory. factoryd's lifetime flock is on the separate factory.lock inode,
// so launchctl may start or stop the daemon while this lock remains held.
func withServiceMutation(ctx context.Context, home string, operation func(*serviceHomeCapability) (ServiceStatus, error)) (status ServiceStatus, resultErr error) {
	if ctx == nil || operation == nil {
		return ServiceStatus{}, fmt.Errorf("%w: invalid service mutation", ErrServiceAmbiguous)
	}
	capability, err := openServiceHomeCapability(ctx, home)
	if err != nil {
		return ServiceStatus{}, err
	}
	if err := unix.Flock(int(capability.home.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		closeErr := capability.close()
		if errors.Is(err, unix.EWOULDBLOCK) || errors.Is(err, unix.EAGAIN) {
			return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, ErrBusy, err, closeErr)
		}
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err, closeErr)
	}
	defer func() {
		verifyErr := errors.Join(capability.recheck(ctx), capability.stageAbsent())
		unlockErr := unix.Flock(int(capability.home.Fd()), unix.LOCK_UN)
		closeErr := capability.close()
		if cleanupErr := errors.Join(verifyErr, unlockErr, closeErr); cleanupErr != nil {
			resultErr = errors.Join(resultErr, ErrServiceAmbiguous, cleanupErr)
		}
	}()
	if err := errors.Join(capability.recheck(ctx), capability.stageAbsent()); err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return operation(capability)
}

// openServiceArtifacts opens <home>.service without following symlinks.
// Absence is an ordinary result, never an error.
func openServiceArtifacts(home string) (*os.File, bool, error) {
	if !validServicePath(home) {
		return nil, false, ErrInvalidHome
	}
	path := ServiceDirectoryPath(home)
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if errors.Is(err, unix.ENOENT) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("%w: open service directory: %v", ErrServiceReceipt, err)
	}
	directory := os.NewFile(uintptr(fd), path)
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o022 != 0 {
		_ = directory.Close()
		return nil, false, fmt.Errorf("%w: service directory authority", ErrServiceReceipt)
	}
	return directory, true, nil
}

// readServiceReceipt reads and canonically verifies the durable receipt.
// (zero, false, nil) means provably no receipt.
func readServiceReceipt(home string) (serviceReceipt, bool, error) {
	directory, present, err := openServiceArtifacts(home)
	if err != nil || !present {
		return serviceReceipt{}, false, err
	}
	defer func() { _ = directory.Close() }()
	fd, err := unix.Openat(int(directory.Fd()), serviceReceiptName, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if errors.Is(err, unix.ENOENT) {
		return serviceReceipt{}, false, nil
	}
	if err != nil {
		return serviceReceipt{}, false, fmt.Errorf("%w: open", ErrServiceReceipt)
	}
	file := os.NewFile(uintptr(fd), serviceReceiptName)
	defer func() { _ = file.Close() }()
	var before unix.Stat_t
	if err := unix.Fstat(fd, &before); err != nil || before.Mode&unix.S_IFMT != unix.S_IFREG || before.Mode&0o7777 != 0o600 || before.Uid != uint32(os.Geteuid()) || before.Nlink != 1 || before.Size <= 0 || before.Size > serviceReceiptMaxBytes {
		return serviceReceipt{}, false, fmt.Errorf("%w: metadata", ErrServiceReceipt)
	}
	body, err := io.ReadAll(io.LimitReader(file, serviceReceiptMaxBytes+1))
	if err != nil || int64(len(body)) != before.Size {
		return serviceReceipt{}, false, fmt.Errorf("%w: read", ErrServiceReceipt)
	}
	var after unix.Stat_t
	if err := unix.Fstat(fd, &after); err != nil || !sameServiceStat(before, after) {
		return serviceReceipt{}, false, fmt.Errorf("%w: identity changed", ErrServiceReceipt)
	}
	receipt, err := parseServiceReceipt(body)
	if err != nil {
		return serviceReceipt{}, false, err
	}
	return receipt, true, nil
}

// rejectServiceDirectoryResidue accepts only an absent or empty service
// directory: anything else is residue an uninstall must resolve.
func rejectServiceDirectoryResidue(home string) error {
	directory, present, err := openServiceArtifacts(home)
	if err != nil || !present {
		return err
	}
	defer func() { _ = directory.Close() }()
	names, err := directory.Readdirnames(1)
	if err != nil && !errors.Is(err, io.EOF) {
		return fmt.Errorf("%w: enumerate service directory", ErrServiceResidue)
	}
	if len(names) != 0 {
		return fmt.Errorf("%w: service directory holds %s", ErrServiceResidue, names[0])
	}
	return nil
}

// receiptMatchesInstallation proves the receipt, the rendered plist, and the
// installed program agree byte-for-byte with this home and configuration.
func receiptMatchesInstallation(receipt serviceReceipt, home string, config ServiceConfig, plistPath string) error {
	if receipt.Label != config.Label {
		return fmt.Errorf("%w: receipt label %q", ErrServiceForeign, receipt.Label)
	}
	if receipt.PlistPath != plistPath {
		return fmt.Errorf("%w: receipt plist path", ErrServiceForeign)
	}
	_, digest, err := ServicePlist(home, config.Label, receipt.RelayOrigin)
	if err != nil {
		return err
	}
	if receipt.PlistDigest != hex.EncodeToString(digest[:]) {
		return fmt.Errorf("%w: receipt plist digest", ErrServiceForeign)
	}
	programDigest, err := digestServiceProgram(home)
	if err != nil {
		return err
	}
	if receipt.ProgramDigest != programDigest {
		return fmt.Errorf("%w: installed program digest", ErrServiceForeign)
	}
	return nil
}

func digestServiceProgram(home string) (string, error) {
	fd, err := unix.Open(serviceProgramPath(home), unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return "", fmt.Errorf("%w: open installed program", ErrServiceReceipt)
	}
	file := os.NewFile(uintptr(fd), "factoryd")
	defer func() { _ = file.Close() }()
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o022 != 0 || stat.Mode&0o100 == 0 || stat.Size <= 0 || stat.Size > serviceBinaryMaxBytes {
		return "", fmt.Errorf("%w: installed program metadata", ErrServiceReceipt)
	}
	digest := sha256.New()
	if _, err := io.Copy(digest, file); err != nil {
		return "", fmt.Errorf("%w: read installed program", ErrServiceReceipt)
	}
	var after unix.Stat_t
	if err := unix.Fstat(fd, &after); err != nil || !sameServiceStat(stat, after) {
		return "", fmt.Errorf("%w: installed program changed", ErrServiceReceipt)
	}
	return hex.EncodeToString(digest.Sum(nil)), nil
}

func serviceInstall(ctx context.Context, home string, config ServiceConfig, sourceDir string) (ServiceStatus, error) {
	userHome, err := AccountHome()
	if err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return serviceInstallAt(ctx, home, userHome, config, sourceDir, runLaunchctl)
}

func serviceInstallAt(ctx context.Context, home, userHome string, config ServiceConfig, sourceDir string, launchctl launchctlRun) (ServiceStatus, error) {
	if ctx == nil || launchctl == nil || !config.valid() || !validServicePath(sourceDir) {
		return ServiceStatus{}, fmt.Errorf("%w: invalid install request", ErrServiceAmbiguous)
	}
	return withServiceMutation(ctx, home, func(capability *serviceHomeCapability) (ServiceStatus, error) {
		return serviceInstallLockedAt(ctx, home, userHome, config, sourceDir, launchctl, capability)
	})
}

func serviceInstallLockedAt(ctx context.Context, home, userHome string, config ServiceConfig, sourceDir string, launchctl launchctlRun, capability *serviceHomeCapability) (ServiceStatus, error) {
	inspection, err := inspectServiceWithCapabilityAt(ctx, home, userHome, config, launchctl, capability)
	status := inspection.status
	if err == nil && (status.State == ServiceInstalled || status.State == ServiceRunning) {
		if inspection.relayOrigin != config.RelayOrigin {
			// Repeating an install is recognized only when it would render the
			// same plist. A changed relay origin needs the old job removed.
			return ServiceStatus{State: ServiceAmbiguous}, fmt.Errorf("%w %q; run factoryctl service uninstall first", ErrServiceRelayOrigin, inspection.relayOrigin)
		}
		return status, nil
	}
	if err != nil && !errors.Is(err, ErrServiceResidue) {
		if status.State == ServiceAmbiguous {
			return status, err
		}
		return status, err
	}
	if errors.Is(err, ErrServiceResidue) {
		return ServiceStatus{State: ServiceAmbiguous}, err
	}
	plistDirectory, plistPath := servicePlistLocation(userHome, config)
	if err := ensureOwnedDirectory(plistDirectory); err != nil {
		return ServiceStatus{}, err
	}
	serviceDir := ServiceDirectoryPath(home)
	for _, path := range []string{serviceDir, filepath.Join(serviceDir, "bin"), filepath.Join(serviceDir, "bin", "current")} {
		if err := ensureOwnedDirectory(path); err != nil {
			return ServiceStatus{}, err
		}
	}
	var programDigest string
	for _, name := range serviceBinaryNames {
		digest, err := copyServiceBinary(filepath.Join(sourceDir, name), filepath.Join(serviceDir, "bin", "current"), name)
		if err != nil {
			return ServiceStatus{}, err
		}
		if name == "factoryd" {
			programDigest = digest
		}
	}
	plistBytes, plistDigest, err := ServicePlist(home, config.Label, config.RelayOrigin)
	if err != nil {
		return ServiceStatus{}, err
	}
	receipt := serviceReceipt{
		Version: serviceReceiptVersion, Label: config.Label, PlistPath: plistPath,
		PlistDigest: hex.EncodeToString(plistDigest[:]), ProgramDigest: programDigest,
		RelayOrigin: config.RelayOrigin,
	}
	body, err := encodeServiceReceipt(receipt)
	if err != nil {
		return ServiceStatus{}, err
	}
	stderr, err := os.OpenFile(serviceStderrPath(home), os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o600)
	if err != nil {
		return ServiceStatus{}, fmt.Errorf("%w: reserve stderr log: %v", ErrServiceAmbiguous, err)
	}
	if err := stderr.Close(); err != nil {
		return ServiceStatus{}, fmt.Errorf("%w: close stderr log: %v", ErrServiceAmbiguous, err)
	}
	// The receipt is published before the plist so the argument list is on
	// disk whenever the plist is. A crash in the other order would leave a
	// plist nothing could re-render, which uninstall could never resolve.
	if err := writeExactFile(serviceDir, serviceReceiptName, body, 0o600); err != nil {
		return ServiceStatus{}, err
	}
	if err := writeExactFile(plistDirectory, config.plistName(), plistBytes, 0o600); err != nil {
		return ServiceStatus{}, err
	}
	uid := strconv.Itoa(os.Geteuid())
	result := launchctl(ctx, "bootstrap", "gui/"+uid, plistPath)
	if result.err != nil || result.status != 0 {
		return ServiceStatus{State: ServiceInstalled}, fmt.Errorf("%w: bootstrap status %d: %v", ErrServiceLaunchctl, result.status, result.err)
	}
	return confirmServiceLoaded(ctx, home, config, plistPath, launchctl)
}

func serviceStart(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	userHome, err := AccountHome()
	if err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return serviceStartAt(ctx, home, userHome, config, runLaunchctl)
}

func serviceStartAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun) (ServiceStatus, error) {
	if ctx == nil || launchctl == nil || !config.valid() {
		return ServiceStatus{}, fmt.Errorf("%w: invalid start request", ErrServiceAmbiguous)
	}
	return withServiceMutation(ctx, home, func(capability *serviceHomeCapability) (ServiceStatus, error) {
		return serviceStartLockedAt(ctx, home, userHome, config, launchctl, capability)
	})
}

func serviceStartLockedAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun, capability *serviceHomeCapability) (ServiceStatus, error) {
	inspection, err := inspectServiceWithCapabilityAt(ctx, home, userHome, config, launchctl, capability)
	status := inspection.status
	if err != nil {
		return status, err
	}
	switch status.State {
	case ServiceRunning:
		return status, nil
	case ServiceInstalled:
		if inspection.observation.present {
			// RunAtLoad without KeepAlive can leave an exited job loaded. Remove
			// that definition before reusing the one bootstrap path below.
			if err := bootoutService(ctx, config, launchctl); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
		}
	default:
		return status, fmt.Errorf("%w: start requires an installed service", ErrServiceAmbiguous)
	}
	_, plistPath := servicePlistLocation(userHome, config)
	uid := strconv.Itoa(os.Geteuid())
	result := launchctl(ctx, "bootstrap", "gui/"+uid, plistPath)
	if result.err != nil || result.status != 0 {
		return ServiceStatus{State: ServiceInstalled}, fmt.Errorf("%w: bootstrap status %d: %v", ErrServiceLaunchctl, result.status, result.err)
	}
	return confirmServiceLoaded(ctx, home, config, plistPath, launchctl)
}

func serviceStop(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	userHome, err := AccountHome()
	if err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return serviceStopAt(ctx, home, userHome, config, runLaunchctl)
}

func serviceStopAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun) (ServiceStatus, error) {
	if ctx == nil || launchctl == nil || !config.valid() {
		return ServiceStatus{}, fmt.Errorf("%w: invalid stop request", ErrServiceAmbiguous)
	}
	return withServiceMutation(ctx, home, func(capability *serviceHomeCapability) (ServiceStatus, error) {
		return serviceStopLockedAt(ctx, home, userHome, config, launchctl, capability)
	})
}

func serviceStopLockedAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun, capability *serviceHomeCapability) (ServiceStatus, error) {
	inspection, err := inspectServiceWithCapabilityAt(ctx, home, userHome, config, launchctl, capability)
	status := inspection.status
	if err != nil {
		return status, err
	}
	if status.State == ServiceInstalled && !inspection.observation.present {
		return status, nil
	}
	if status.State != ServiceInstalled && status.State != ServiceRunning {
		return status, fmt.Errorf("%w: stop requires an installed service", ErrServiceAmbiguous)
	}
	if err := bootoutService(ctx, config, launchctl); err != nil {
		return ServiceStatus{State: ServiceAmbiguous}, err
	}
	return ServiceStatus{State: ServiceInstalled}, nil
}

func serviceUninstall(ctx context.Context, home string, config ServiceConfig) (ServiceStatus, error) {
	userHome, err := AccountHome()
	if err != nil {
		return ServiceStatus{}, errors.Join(ErrServiceAmbiguous, err)
	}
	return serviceUninstallAt(ctx, home, userHome, config, runLaunchctl)
}

// serviceUninstallAt removes exactly this installation's artifacts and is the
// resolution path for crash residue, including its own stage files. It is
// evidence-first: no mutating launchctl verb runs until a matching receipt or
// an exactly rendered plist proves the label maps to this home, and it never
// deletes bytes it cannot prove are its own property.
func serviceUninstallAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun) (ServiceStatus, error) {
	if ctx == nil || launchctl == nil || !config.valid() || !validServicePath(home) {
		return ServiceStatus{}, fmt.Errorf("%w: invalid uninstall request", ErrServiceAmbiguous)
	}
	return withServiceMutation(ctx, home, func(*serviceHomeCapability) (ServiceStatus, error) {
		return serviceUninstallLockedAt(ctx, home, userHome, config, launchctl)
	})
}

func serviceUninstallLockedAt(ctx context.Context, home, userHome string, config ServiceConfig, launchctl launchctlRun) (ServiceStatus, error) {
	plistDirectory, plistPath := servicePlistLocation(userHome, config)
	receipt, receiptPresent, receiptErr := readServiceReceipt(home)
	var expectedPlist []byte
	var err error
	evidence := receiptErr == nil && receiptPresent
	if evidence {
		if receipt.Label != config.Label || receipt.PlistPath != plistPath {
			// The service directory belongs to a different installation target;
			// removing it here would orphan that installation.
			return ServiceStatus{State: ServiceAmbiguous}, fmt.Errorf("%w: the receipt names a different installation target", ErrServiceForeign)
		}
		var present bool
		expectedPlist, present, err = readServicePlist(plistDirectory, config.plistName(), serviceMaxPlistBytes)
		if err != nil {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
		if present {
			actual := sha256.Sum256(expectedPlist)
			if hex.EncodeToString(actual[:]) != receipt.PlistDigest {
				return ServiceStatus{State: ServiceAmbiguous}, fmt.Errorf("%w: %s holds different bytes; refusing removal", ErrServiceForeign, config.plistName())
			}
		}
	} else {
		expectedPlist, _, err = ServicePlist(home, config.Label, "")
		if err != nil {
			return ServiceStatus{}, err
		}
		actualPlist, present, err := readServicePlist(plistDirectory, config.plistName(), len(expectedPlist))
		if err != nil {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
		if present && !bytes.Equal(actualPlist, expectedPlist) {
			return ServiceStatus{State: ServiceAmbiguous}, fmt.Errorf("%w: the plist at %s is not this installation's property", ErrServiceForeign, config.plistName())
		}
		evidence = present
	}
	if evidence {
		service := "gui/" + strconv.Itoa(os.Geteuid()) + "/" + config.Label
		observation, err := observeLaunchctl(ctx, launchctl, service, plistPath, serviceProgramPath(home))
		if err != nil {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
		if observation.present {
			if err := bootoutService(ctx, config, launchctl); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
		}
	}
	if err := removeExactFile(plistDirectory, config.plistName(), expectedPlist); err != nil {
		return ServiceStatus{State: ServiceAmbiguous}, err
	}
	if err := removeOwnedFile(plistDirectory, "."+config.plistName()+".stage"); err != nil {
		return ServiceStatus{State: ServiceAmbiguous}, err
	}
	directory, present, err := openServiceArtifacts(home)
	if err != nil {
		return ServiceStatus{State: ServiceAmbiguous}, err
	}
	if present {
		defer func() { _ = directory.Close() }()
		current := filepath.Join(ServiceDirectoryPath(home), "bin", "current")
		for _, name := range serviceBinaryNames {
			if err := removeOwnedFile(current, name); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
			// Exactly this engine's own stage names are crash residue it must
			// resolve; nothing else in the tree is deletable without proof.
			if err := removeOwnedFile(current, "."+name+".stage"); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
		}
		for _, path := range []string{current, filepath.Join(ServiceDirectoryPath(home), "bin")} {
			if err := removeEmptyOwnedDirectory(path); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
		}
		for _, name := range []string{serviceReceiptName, "." + serviceReceiptName + ".stage", serviceStderrLogName} {
			if err := removeOwnedFile(ServiceDirectoryPath(home), name); err != nil {
				return ServiceStatus{State: ServiceAmbiguous}, err
			}
		}
		if err := directory.Close(); err != nil {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
		if err := removeEmptyOwnedDirectory(ServiceDirectoryPath(home)); err != nil {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
	}
	return ServiceStatus{State: ServiceAbsent}, nil
}

func readServicePlist(directory, name string, limit int) ([]byte, bool, error) {
	path := filepath.Join(directory, name)
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if errors.Is(err, unix.ENOENT) {
		return nil, false, nil
	}
	if err != nil {
		return nil, false, fmt.Errorf("%w: open %s", ErrServiceAmbiguous, name)
	}
	file := os.NewFile(uintptr(fd), name)
	body, readErr := io.ReadAll(io.LimitReader(file, int64(limit)+1))
	closeErr := file.Close()
	if readErr != nil || closeErr != nil || len(body) > limit {
		return nil, false, fmt.Errorf("%w: read %s", ErrServiceAmbiguous, name)
	}
	return body, true, nil
}

func servicePlistLocation(userHome string, config ServiceConfig) (directory, path string) {
	directory = config.PlistDirectory
	if directory == "" {
		directory = filepath.Join(userHome, "Library", "LaunchAgents")
	}
	return directory, filepath.Join(directory, config.plistName())
}

func confirmServiceLoaded(ctx context.Context, home string, config ServiceConfig, plistPath string, launchctl launchctlRun) (ServiceStatus, error) {
	service := "gui/" + strconv.Itoa(os.Geteuid()) + "/" + config.Label
	deadline := time.Now().Add(serviceBootoutPatience)
	for {
		observation, err := observeLaunchctl(ctx, launchctl, service, plistPath, serviceProgramPath(home))
		if err == nil {
			if !observation.present {
				return ServiceStatus{State: ServiceInstalled}, fmt.Errorf("%w: job absent after bootstrap", ErrServiceLaunchctl)
			}
			if observation.pid > 0 {
				return ServiceStatus{State: ServiceRunning, PID: observation.pid}, nil
			}
			return ServiceStatus{State: ServiceInstalled}, nil
		}
		// launchd passes through transient spawn states whose print shapes the
		// strict parser refuses; each observation stays exact, the confirmation
		// merely waits out the transient within one bound.
		if time.Now().After(deadline) {
			return ServiceStatus{State: ServiceAmbiguous}, err
		}
		select {
		case <-ctx.Done():
			return ServiceStatus{State: ServiceAmbiguous}, ctx.Err()
		case <-time.After(150 * time.Millisecond):
		}
	}
}

func bootoutService(ctx context.Context, config ServiceConfig, launchctl launchctlRun) error {
	service := "gui/" + strconv.Itoa(os.Geteuid()) + "/" + config.Label
	result := launchctl(ctx, "bootout", service)
	if result.err != nil {
		return errors.Join(ErrServiceLaunchctl, result.err)
	}
	if result.status != 0 && result.status != launchctlNotFound && result.status != int(unix.EINPROGRESS) {
		return fmt.Errorf("%w: bootout status %d", ErrServiceLaunchctl, result.status)
	}
	deadline := time.Now().Add(serviceBootoutPatience)
	for {
		probe := launchctl(ctx, "print", service)
		if probe.err == nil && probe.status == launchctlNotFound && validNotFoundStderr(probe.stderr, service) {
			return nil
		}
		if time.Now().After(deadline) {
			return fmt.Errorf("%w: job survived bootout", ErrServiceLaunchctl)
		}
		select {
		case <-ctx.Done():
			return ctx.Err()
		case <-time.After(100 * time.Millisecond):
		}
	}
}

func ensureOwnedDirectory(path string) error {
	if err := os.Mkdir(path, 0o700); err != nil && !errors.Is(err, os.ErrExist) {
		return fmt.Errorf("%w: create %s: %v", ErrServiceAmbiguous, filepath.Base(path), err)
	}
	var stat unix.Stat_t
	if err := unix.Lstat(path, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o022 != 0 {
		return fmt.Errorf("%w: directory authority %s", ErrServiceForeign, filepath.Base(path))
	}
	return nil
}

func copyServiceBinary(sourcePath, destinationDir, name string) (string, error) {
	sourceFD, err := unix.Open(sourcePath, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err != nil {
		return "", fmt.Errorf("%w: open source %s", ErrServiceAmbiguous, name)
	}
	source := os.NewFile(uintptr(sourceFD), name)
	defer func() { _ = source.Close() }()
	var stat unix.Stat_t
	if err := unix.Fstat(sourceFD, &stat); err != nil || stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Uid != uint32(os.Geteuid()) || stat.Mode&0o022 != 0 || stat.Mode&0o100 == 0 || stat.Size <= 0 || stat.Size > serviceBinaryMaxBytes {
		return "", fmt.Errorf("%w: source binary %s", ErrServiceForeign, name)
	}
	stageName := "." + name + ".stage"
	stagePath := filepath.Join(destinationDir, stageName)
	destination, err := os.OpenFile(stagePath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o700)
	if errors.Is(err, os.ErrExist) {
		return "", fmt.Errorf("%w: stale %s stage; run factoryctl service uninstall", ErrServiceResidue, name)
	}
	if err != nil {
		return "", fmt.Errorf("%w: stage %s: %v", ErrServiceAmbiguous, name, err)
	}
	digest := sha256.New()
	_, copyErr := io.Copy(io.MultiWriter(destination, digest), source)
	syncErr := destination.Sync()
	closeErr := destination.Close()
	if copyErr != nil || syncErr != nil || closeErr != nil {
		_ = os.Remove(stagePath)
		return "", fmt.Errorf("%w: copy %s", ErrServiceAmbiguous, name)
	}
	var after unix.Stat_t
	if err := unix.Fstat(sourceFD, &after); err != nil || !sameServiceStat(stat, after) {
		_ = os.Remove(stagePath)
		return "", fmt.Errorf("%w: source binary %s changed", ErrServiceForeign, name)
	}
	if err := os.Rename(stagePath, filepath.Join(destinationDir, name)); err != nil {
		_ = os.Remove(stagePath)
		return "", fmt.Errorf("%w: publish %s", ErrServiceAmbiguous, name)
	}
	if err := syncServiceDirectory(destinationDir); err != nil {
		return "", err
	}
	return hex.EncodeToString(digest.Sum(nil)), nil
}

func writeExactFile(directory, name string, contents []byte, mode os.FileMode) error {
	path := filepath.Join(directory, name)
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if err == nil {
		file := os.NewFile(uintptr(fd), name)
		existing, readErr := io.ReadAll(io.LimitReader(file, int64(len(contents))+1))
		closeErr := file.Close()
		if readErr != nil || closeErr != nil {
			return fmt.Errorf("%w: read existing %s", ErrServiceAmbiguous, name)
		}
		if bytes.Equal(existing, contents) {
			return nil
		}
		return fmt.Errorf("%w: %s exists with different bytes", ErrServiceForeign, name)
	}
	if !errors.Is(err, unix.ENOENT) {
		return fmt.Errorf("%w: probe %s", ErrServiceAmbiguous, name)
	}
	stagePath := filepath.Join(directory, "."+name+".stage")
	file, err := os.OpenFile(stagePath, os.O_WRONLY|os.O_CREATE|os.O_EXCL, mode)
	if errors.Is(err, os.ErrExist) {
		return fmt.Errorf("%w: stale %s stage; run factoryctl service uninstall", ErrServiceResidue, name)
	}
	if err != nil {
		return fmt.Errorf("%w: stage %s: %v", ErrServiceAmbiguous, name, err)
	}
	_, writeErr := file.Write(contents)
	syncErr := file.Sync()
	closeErr := file.Close()
	if writeErr != nil || syncErr != nil || closeErr != nil {
		_ = os.Remove(stagePath)
		return fmt.Errorf("%w: write %s", ErrServiceAmbiguous, name)
	}
	if err := os.Rename(stagePath, path); err != nil {
		_ = os.Remove(stagePath)
		return fmt.Errorf("%w: publish %s", ErrServiceAmbiguous, name)
	}
	return syncServiceDirectory(directory)
}

func removeExactFile(directory, name string, expected []byte) error {
	path := filepath.Join(directory, name)
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_CLOEXEC|unix.O_NOFOLLOW, 0)
	if errors.Is(err, unix.ENOENT) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("%w: open %s", ErrServiceAmbiguous, name)
	}
	file := os.NewFile(uintptr(fd), name)
	body, readErr := io.ReadAll(io.LimitReader(file, int64(len(expected))+1))
	closeErr := file.Close()
	if readErr != nil || closeErr != nil {
		return fmt.Errorf("%w: read %s", ErrServiceAmbiguous, name)
	}
	if !bytes.Equal(body, expected) {
		return fmt.Errorf("%w: %s holds different bytes; refusing removal", ErrServiceForeign, name)
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("%w: remove %s", ErrServiceAmbiguous, name)
	}
	return syncServiceDirectory(directory)
}

func removeOwnedFile(directory, name string) error {
	path := filepath.Join(directory, name)
	var stat unix.Stat_t
	err := unix.Lstat(path, &stat)
	if errors.Is(err, unix.ENOENT) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("%w: probe %s", ErrServiceAmbiguous, name)
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Uid != uint32(os.Geteuid()) {
		return fmt.Errorf("%w: %s is not this installation's file", ErrServiceForeign, name)
	}
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return fmt.Errorf("%w: remove %s", ErrServiceAmbiguous, name)
	}
	return nil
}

func removeEmptyOwnedDirectory(path string) error {
	var stat unix.Stat_t
	err := unix.Lstat(path, &stat)
	if errors.Is(err, unix.ENOENT) {
		return nil
	}
	if err != nil || stat.Mode&unix.S_IFMT != unix.S_IFDIR || stat.Uid != uint32(os.Geteuid()) {
		return fmt.Errorf("%w: directory %s", ErrServiceForeign, filepath.Base(path))
	}
	if err := unix.Rmdir(path); err != nil {
		if errors.Is(err, unix.ENOTEMPTY) || errors.Is(err, unix.EEXIST) {
			return fmt.Errorf("%w: %s is not empty; refusing removal", ErrServiceForeign, filepath.Base(path))
		}
		return fmt.Errorf("%w: remove directory %s", ErrServiceAmbiguous, filepath.Base(path))
	}
	return nil
}

func syncServiceDirectory(path string) error {
	fd, err := unix.Open(path, unix.O_RDONLY|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return fmt.Errorf("%w: open directory for sync", ErrServiceAmbiguous)
	}
	syncErr := unix.Fsync(fd)
	closeErr := unix.Close(fd)
	if syncErr != nil || closeErr != nil {
		return fmt.Errorf("%w: sync directory", ErrServiceAmbiguous)
	}
	return nil
}
