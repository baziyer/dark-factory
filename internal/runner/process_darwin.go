//go:build darwin

package runner

import (
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"syscall"
	"time"
	"unsafe"

	"golang.org/x/sys/unix"
)

type GateLease struct {
	dir         *os.File
	dirIdentity fileCommitment
	lifetime    *os.File
	lifetimeID  descriptorCommitment
	basename    string
	marker      *FileIdentity
	closed      bool
}

func CreateGateLease(dir, lifetime *os.File, basename string) (*GateLease, FileIdentity, error) {
	if dir == nil || lifetime == nil || basename != OuterActivationMarkerName && basename != InnerActivationMarkerName {
		return nil, FileIdentity{}, fmt.Errorf("runner: invalid marker name")
	}
	dup, err := unix.FcntlInt(dir.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, FileIdentity{}, err
	}
	f := os.NewFile(uintptr(dup), "gate-lease-dir")
	c, err := validatePrivateDirectory(f)
	if err != nil {
		f.Close()
		return nil, FileIdentity{}, err
	}
	lifetimeDup, err := unix.FcntlInt(lifetime.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		f.Close()
		return nil, FileIdentity{}, err
	}
	lifetimeFile := os.NewFile(uintptr(lifetimeDup), "runtime-lifetime")
	lifetimeID, err := commitRuntimeLifetime(f, lifetimeFile)
	if err != nil {
		f.Close()
		lifetimeFile.Close()
		return nil, FileIdentity{}, err
	}
	var s unix.Stat_t
	err = unix.Fstatat(int(f.Fd()), basename, &s, unix.AT_SYMLINK_NOFOLLOW)
	if err == nil {
		f.Close()
		lifetimeFile.Close()
		return nil, FileIdentity{}, os.ErrExist
	}
	if !errors.Is(err, unix.ENOENT) {
		f.Close()
		lifetimeFile.Close()
		return nil, FileIdentity{}, err
	}
	return &GateLease{dir: f, dirIdentity: c, lifetime: lifetimeFile, lifetimeID: lifetimeID, basename: basename}, c.FileIdentity, nil
}

func commitRuntimeLifetime(dir, lifetime *os.File) (descriptorCommitment, error) {
	if dir == nil || lifetime == nil {
		return descriptorCommitment{}, ErrIdentity
	}
	flags, err := unix.FcntlInt(lifetime.Fd(), unix.F_GETFL, 0)
	if err != nil || flags&unix.O_ACCMODE != unix.O_RDONLY {
		return descriptorCommitment{}, errors.Join(ErrIdentity, err)
	}
	var opened, named unix.Stat_t
	if err := unix.Fstat(int(lifetime.Fd()), &opened); err != nil {
		return descriptorCommitment{}, err
	}
	if err := unix.Fstatat(int(dir.Fd()), RuntimeLifetimeLeaseName, &named, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return descriptorCommitment{}, err
	}
	valid := func(stat unix.Stat_t) bool {
		return stat.Mode&unix.S_IFMT == unix.S_IFREG && stat.Mode&0o7777 == 0o600 && stat.Uid == uint32(os.Geteuid()) && stat.Nlink == 1 && stat.Size == 0 && stat.Dev != 0 && stat.Ino != 0
	}
	if !valid(opened) || !valid(named) || opened.Dev != named.Dev || opened.Ino != named.Ino {
		return descriptorCommitment{}, ErrIdentity
	}
	if err := unix.Flock(int(lifetime.Fd()), unix.LOCK_EX|unix.LOCK_NB); err != nil {
		return descriptorCommitment{}, err
	}
	var recheck, namedRecheck unix.Stat_t
	if err := unix.Fstat(int(lifetime.Fd()), &recheck); err != nil || unix.Fstatat(int(dir.Fd()), RuntimeLifetimeLeaseName, &namedRecheck, unix.AT_SYMLINK_NOFOLLOW) != nil || !valid(recheck) || !valid(namedRecheck) || recheck.Dev != opened.Dev || recheck.Ino != opened.Ino || namedRecheck.Dev != opened.Dev || namedRecheck.Ino != opened.Ino {
		return descriptorCommitment{}, errors.Join(ErrIdentity, err)
	}
	return descriptorCommitment{FileIdentity: FileIdentity{Device: uint64(opened.Dev), Inode: opened.Ino}, UID: opened.Uid, GID: opened.Gid, Mode: uint32(opened.Mode)}, nil
}

func fdPath(f *os.File) (string, error) {
	b := make([]byte, 1024)
	_, _, errno := syscall.Syscall(syscall.SYS_FCNTL, f.Fd(), unix.F_GETPATH, uintptr(unsafePointer(&b[0])))
	if errno != 0 {
		return "", errno
	}
	n := 0
	for n < len(b) && b[n] != 0 {
		n++
	}
	if n == 0 {
		return "", ErrIdentity
	}
	return string(b[:n]), nil
}

// Kept in the Darwin file: F_GETPATH has no typed Go wrapper.
func unsafePointer(p *byte) uintptr { return uintptr(unsafe.Pointer(p)) }

func (l *GateLease) Close() error {
	if l == nil || l.closed {
		return nil
	}
	l.closed = true
	return errors.Join(l.dir.Close(), l.lifetime.Close())
}

type OwnedChild struct {
	cmd            *exec.Cmd
	identity       Identity
	exit           Exit
	kq             int
	activation     *os.File
	status         *os.File
	lease          *GateLease
	state          state
	activated      bool
	exitObserved   bool
	exitRegistered bool
	keepDirectory  bool
	ptyMaster      *os.File
	ptyDrained     bool
	// testSignal injects a package-test-only owned-group signal result.
	// Production children always leave it nil and call signalOwnedGroup.
	testSignal func(unix.Signal) error
	// testConvergence injects a package-test-only failure immediately before
	// activated group convergence. Production children always leave it nil.
	testConvergence func() error
	// testHardCleanup injects a package-test-only pre-cleanup uncertainty.
	// Production children always leave it nil.
	testHardCleanup func() error
}

// These seams exist only for package tests to force post-spawn cleanup paths;
// production leaves both nil and always closes the real slave synchronously.
var (
	testPTYAfterStart func(*OwnedChild)
	testPTYSlaveClose func(*os.File) error
)

func closePTYSlave(slave *os.File) error {
	if testPTYSlaveClose != nil {
		return testPTYSlaveClose(slave)
	}
	return slave.Close()
}

func (c *OwnedChild) Identity() Identity {
	if c == nil {
		return Identity{}
	}
	return c.identity
}

// WritePTY writes terminal input only while this live owner has an activated
// process. It is intentionally synchronous and bounded; the browser/runtime
// transport is responsible for serialization and backpressure later.
func (c *OwnedChild) WritePTY(input []byte) (int, error) {
	if c == nil || c.ptyMaster == nil || c.state != stateActivated || c.exitObserved {
		return 0, ErrState
	}
	if len(input) == 0 || len(input) > maxInputBytes {
		return 0, ErrState
	}
	if err := c.refreshExit(); err != nil {
		return 0, err
	}
	if c.exitObserved {
		return 0, ErrState
	}
	return c.writePTYOwned(input, 250*time.Millisecond)
}

// writePTYOwned is for the synchronous owner loop, which already consumes
// the child's EVFILT_PROC event. Calling refreshExit from that loop could
// consume a readiness event belonging to the loop and lose the only exit
// notification.
func (c *OwnedChild) writePTYOwned(input []byte, timeout time.Duration) (int, error) {
	if c == nil || c.ptyMaster == nil || c.state != stateActivated || c.exitObserved {
		return 0, ErrState
	}
	if len(input) == 0 || len(input) > maxInputBytes || timeout <= 0 {
		return 0, ErrState
	}
	deadline := time.Now().Add(timeout)
	written := 0
	for written < len(input) {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return written, os.ErrDeadlineExceeded
		}
		milliseconds := int(remaining / time.Millisecond)
		if milliseconds < 1 {
			milliseconds = 1
		}
		fds := []unix.PollFd{{Fd: int32(c.ptyMaster.Fd()), Events: unix.POLLOUT}}
		ready, err := unix.Poll(fds, milliseconds)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return written, err
		}
		if ready == 0 {
			return written, os.ErrDeadlineExceeded
		}
		if fds[0].Revents&(unix.POLLERR|unix.POLLHUP|unix.POLLNVAL) != 0 {
			return written, ErrState
		}
		n, err := unix.Write(int(c.ptyMaster.Fd()), input[written:])
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			continue
		}
		if err != nil {
			return written, err
		}
		if n == 0 {
			return written, io.ErrNoProgress
		}
		written += n
	}
	return written, nil
}

// ReadPTY reads terminal output from the runner-owned master with a fixed
// bounded wait. It never starts a goroutine and never changes process
// lifecycle state.
func (c *OwnedChild) ReadPTY(output []byte) (int, error) {
	if c == nil || c.ptyMaster == nil || len(output) == 0 || len(output) > maxInputBytes {
		return 0, ErrState
	}
	if c.state != stateActivated && c.state != stateExited && c.state != stateWaited {
		return 0, ErrState
	}
	deadline := time.Now().Add(time.Second)
	for {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return 0, os.ErrDeadlineExceeded
		}
		milliseconds := int(remaining / time.Millisecond)
		if milliseconds < 1 {
			milliseconds = 1
		}
		fds := []unix.PollFd{{Fd: int32(c.ptyMaster.Fd()), Events: unix.POLLIN}}
		ready, err := unix.Poll(fds, milliseconds)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return 0, err
		}
		if ready == 0 {
			return 0, os.ErrDeadlineExceeded
		}
		if fds[0].Revents&unix.POLLNVAL != 0 {
			return 0, ErrState
		}
		n, err := unix.Read(int(c.ptyMaster.Fd()), output)
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			continue
		}
		if errors.Is(err, unix.EIO) {
			return n, io.EOF
		}
		if n == 0 && err == nil && fds[0].Revents&(unix.POLLHUP|unix.POLLERR) != 0 {
			return 0, io.EOF
		}
		return n, err
	}
}

func (c *OwnedChild) refreshExit() error {
	if c == nil || c.kq < 0 || c.exitObserved {
		return nil
	}
	zero := unix.Timespec{}
	for {
		events := make([]unix.Kevent_t, 1)
		n, err := unix.Kevent(c.kq, nil, events, &zero)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return err
		}
		if n == 0 {
			return nil
		}
		if events[0].Filter == unix.EVFILT_READ {
			// A filter removed just before cleanup may already have queued one
			// readiness record. It carries no lifecycle authority; keep draining
			// until the exact process filter is observed.
			continue
		}
		if events[0].Ident != uint64(c.identity.PID) || events[0].Filter != unix.EVFILT_PROC || events[0].Fflags&unix.NOTE_EXIT == 0 {
			return ErrIdentity
		}
		c.exitObserved = true
		c.state = stateExited
		return nil
	}
}

func StartBlocked(lease *GateLease, gateExecutable string, spec *LaunchSpec, keepDirectoryAcrossExec bool) (child *OwnedChild, err error) {
	prepared, err := PrepareBlocked(lease, gateExecutable, spec, keepDirectoryAcrossExec)
	if err != nil {
		return nil, err
	}
	started, err := prepared.Start()
	if err != nil {
		return nil, err
	}
	child, err = started.Bind()
	if err != nil {
		return nil, errors.Join(err, convergeStartedChild(started))
	}
	return child, nil
}

// StartBlockedPTY is the Darwin-only concrete PTY launch seam. It retains the
// same activation gate and identity checks as StartBlocked, but gives the
// gate, and ultimately the target, one controlling terminal on fd 0/1/2.
// The returned child owns the master; callers must use the child methods for
// synchronous I/O and must not close the master independently.
func StartBlockedPTY(lease *GateLease, gateExecutable string, spec *LaunchSpec, keepDirectoryAcrossExec bool) (child *OwnedChild, err error) {
	prepared, err := PrepareBlockedPTY(lease, gateExecutable, spec, keepDirectoryAcrossExec)
	if err != nil {
		return nil, err
	}
	started, err := prepared.Start()
	if err != nil {
		return nil, err
	}
	child, err = started.Bind()
	if err != nil {
		return nil, errors.Join(err, convergeStartedChild(started))
	}
	return child, nil
}

// PreparedChild owns a completely prepared fork/exec attempt before its sole
// Start call. Callers may place a durable transaction immediately between
// PrepareBlocked and Start without hiding further setup inside that cut.
type PreparedChild struct {
	lease         *GateLease
	cmd           *exec.Cmd
	kq            int
	leashW        *os.File
	statusR       *os.File
	ptyMaster     *os.File
	ptySlave      *os.File
	startFiles    []*os.File
	keepDirectory bool
	usePTY        bool
	started       bool
	closed        bool
}

// StartedChild is the exact post-Start, pre-birth-binding owner. A non-nil
// value proves cmd.Start returned nil even if Bind later cannot establish a
// safe durable identity.
type StartedChild struct {
	child      *OwnedChild
	ptySlave   *os.File
	cleanupErr error
}

func PrepareBlocked(lease *GateLease, gateExecutable string, spec *LaunchSpec, keepDirectoryAcrossExec bool) (*PreparedChild, error) {
	return prepareBlocked(lease, gateExecutable, spec, keepDirectoryAcrossExec, false)
}

func PrepareBlockedPTY(lease *GateLease, gateExecutable string, spec *LaunchSpec, keepDirectoryAcrossExec bool) (*PreparedChild, error) {
	if spec == nil || len(spec.stdin) != 0 || spec.stdout != nil || spec.stderr != nil {
		return nil, ErrState
	}
	return prepareBlocked(lease, gateExecutable, spec, keepDirectoryAcrossExec, true)
}

func prepareBlocked(lease *GateLease, gateExecutable string, spec *LaunchSpec, keepDirectoryAcrossExec, usePTY bool) (_ *PreparedChild, resultErr error) {
	if lease == nil || lease.closed || lease.marker != nil || lease.lifetime == nil || spec == nil {
		return nil, ErrState
	}
	prepared := &PreparedChild{lease: lease, kq: -1, keepDirectory: keepDirectoryAcrossExec, usePTY: usePTY}
	keep := false
	defer func() {
		if !keep {
			resultErr = errors.Join(resultErr, prepared.Close())
		}
	}()
	gate, err := canonical(gateExecutable)
	if err != nil {
		return nil, err
	}
	if _, err := commitExecutable(gate); err != nil {
		return nil, fmt.Errorf("runner: gate executable: %w", err)
	}
	config, err := anonymousFile(lease.dir, "config", nil)
	if err != nil {
		return nil, err
	}
	prepared.startFiles = append(prepared.startFiles, config)
	if err := writeFrame(config, gateConfig{Version: 1, Target: spec.commit, LeaseDirectory: lease.dirIdentity, Lifetime: lease.lifetimeID, MarkerName: lease.basename, KeepDirectory: keepDirectoryAcrossExec, Control: spec.controlID, TestFinalCheck: spec.testFinal != nil, PTY: usePTY}, maxConfigBytes); err != nil {
		return nil, err
	}
	if _, err := config.Seek(0, 0); err != nil {
		return nil, err
	}
	var ptyMaster, ptySlave *os.File
	if usePTY {
		ptyMaster, ptySlave, err = openPTY()
		if err != nil {
			return nil, err
		}
		prepared.ptyMaster = ptyMaster
		prepared.ptySlave = ptySlave
	}
	var stdin, stdout, stderr *os.File
	if usePTY {
		// The gate keeps the fixed 6/7/8 numbering, but these are harmless
		// slave references rather than obsolete startup-input resources.
		stdin, stdout, stderr = ptySlave, ptySlave, ptySlave
	} else {
		stdin, err = anonymousFile(lease.dir, "stdin", spec.stdin)
		if err != nil {
			return nil, err
		}
		prepared.startFiles = append(prepared.startFiles, stdin)
		stdout = spec.stdout
		stderr = spec.stderr
		var closeOut, closeErr *os.File
		if stdout == nil {
			closeOut, err = os.OpenFile("/dev/null", os.O_WRONLY, 0)
			if err != nil {
				return nil, err
			}
			stdout = closeOut
			prepared.startFiles = append(prepared.startFiles, closeOut)
		}
		if stderr == nil {
			closeErr, err = os.OpenFile("/dev/null", os.O_WRONLY, 0)
			if err != nil {
				return nil, err
			}
			stderr = closeErr
			prepared.startFiles = append(prepared.startFiles, closeErr)
		}
	}
	leashR, leashW, err := os.Pipe()
	if err != nil {
		return nil, err
	}
	prepared.startFiles = append(prepared.startFiles, leashR)
	statusR, statusW, err := os.Pipe()
	if err != nil {
		_ = leashW.Close()
		return nil, err
	}
	prepared.leashW = leashW
	prepared.statusR = statusR
	prepared.startFiles = append(prepared.startFiles, statusW)
	leaseDup, err := unix.FcntlInt(lease.dir.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	leaseFile := os.NewFile(uintptr(leaseDup), "lease-dir")
	prepared.startFiles = append(prepared.startFiles, leaseFile)
	lifetimeDup, err := unix.FcntlInt(lease.lifetime.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	lifetimeFile := os.NewFile(uintptr(lifetimeDup), "runtime-lifetime")
	prepared.startFiles = append(prepared.startFiles, lifetimeFile)
	kq, err := unix.Kqueue()
	if err != nil {
		return nil, err
	}
	unix.CloseOnExec(kq)
	prepared.kq = kq
	cmd := exec.Command(gate, "--exec-gate")
	cmd.Env = []string{}
	if usePTY {
		cmd.Stdin, cmd.Stdout, cmd.Stderr = ptySlave, ptySlave, ptySlave
	} else {
		cmd.Stderr = stderr
	}
	cmd.ExtraFiles = []*os.File{config, leashR, statusW, stdin, stdout, stderr, leaseFile, lifetimeFile}
	if spec.controlID != nil {
		cmd.ExtraFiles = append(cmd.ExtraFiles, spec.control)
	}
	if spec.testFinal != nil {
		cmd.ExtraFiles = append(cmd.ExtraFiles, spec.testFinal)
	}
	if usePTY {
		cmd.SysProcAttr = &syscall.SysProcAttr{Setsid: true, Setctty: true, Ctty: 0}
	} else {
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	}
	prepared.cmd = cmd
	keep = true
	return prepared, nil
}

// Start performs the sole OS Start call. A nil StartedChild means Start itself
// returned a non-nil error; every post-Start problem is retained by the
// non-nil StartedChild and resolved by Bind or Close.
func (prepared *PreparedChild) Start() (*StartedChild, error) {
	if prepared == nil || prepared.closed || prepared.started || prepared.cmd == nil {
		return nil, ErrState
	}
	prepared.started = true
	if err := prepared.cmd.Start(); err != nil {
		return nil, errors.Join(err, prepared.Close())
	}
	c := &OwnedChild{
		cmd: prepared.cmd, identity: Identity{PID: prepared.cmd.Process.Pid, PGID: prepared.cmd.Process.Pid},
		kq: prepared.kq, activation: prepared.leashW, status: prepared.statusR, lease: prepared.lease,
		state: stateBlocked, keepDirectory: prepared.keepDirectory, ptyMaster: prepared.ptyMaster,
	}
	prepared.cmd = nil
	prepared.kq = -1
	prepared.leashW = nil
	prepared.statusR = nil
	prepared.ptyMaster = nil
	closeErr := closePreparedFiles(prepared.startFiles)
	prepared.startFiles = nil
	ptySlave := prepared.ptySlave
	prepared.ptySlave = nil
	prepared.closed = true
	if testPTYAfterStart != nil && prepared.usePTY {
		testPTYAfterStart(c)
	}
	return &StartedChild{child: c, ptySlave: ptySlave, cleanupErr: closeErr}, nil
}

// LaunchAttempt owns the complete pre-registration cut. Exact Start failure
// and successful Start followed by Bind uncertainty have different local
// histories, but after positive convergence share one external fact: no inner
// identity was durably registered and no inner process remains.
func (prepared *PreparedChild) LaunchAttempt(dir *os.File, attemptID string, proof ResultProof) (*OwnedChild, *AttemptResultRecord, error) {
	started, startErr := prepared.Start()
	if started != nil {
		child, bindErr := started.Bind()
		if bindErr == nil {
			return child, nil, nil
		}
		convergeErr := convergeStartedChild(started)
		if started.child != nil {
			return nil, nil, errors.Join(bindErr, convergeErr, ErrUnresolved)
		}
		record, publishErr := publishInnerUnregisteredConverged(dir, attemptID, proof)
		return nil, record, errors.Join(bindErr, convergeErr, publishErr)
	}
	if startErr == nil {
		return nil, nil, ErrState
	}
	record, publishErr := publishInnerUnregisteredConverged(dir, attemptID, proof)
	return nil, record, errors.Join(startErr, publishErr)
}

func publishInnerUnregisteredConverged(dir *os.File, attemptID string, proof ResultProof) (*AttemptResultRecord, error) {
	result, resultErr := innerUnregisteredConvergedResult(attemptID, proof)
	if resultErr != nil {
		return nil, resultErr
	}
	return publishAttemptResult(dir, result)
}

// Bind acquires exact birth identity and readiness after a successful Start.
// Any uncertainty synchronously converges and reaps the direct child but never
// changes the historical fact that Start returned nil.
func (started *StartedChild) Bind() (_ *OwnedChild, resultErr error) {
	if started == nil || started.child == nil {
		return nil, ErrState
	}
	c := started.child
	defer func() {
		if resultErr != nil && started.child != nil {
			if err := started.releaseSlave(); err != nil {
				resultErr = errors.Join(resultErr, err)
			}
			cleanupErr := c.hardCleanup()
			if cleanupErr != nil {
				resultErr = errors.Join(resultErr, fmt.Errorf("runner: failed launch cleanup: %w", cleanupErr))
			}
			if cleanupErr == nil && started.ptySlave == nil && c.state == stateWaited && c.ptyMaster == nil {
				started.child = nil
			}
		}
	}()
	first, e := readIdentity(c.identity.PID)
	if e != nil {
		return nil, fmt.Errorf("runner: first identity: %w", e)
	}
	if first.PGID != first.PID {
		return nil, ErrIdentity
	}
	c.identity = first
	if e = registerExit(c.kq, c.identity.PID); e != nil {
		return nil, fmt.Errorf("runner: register exit: %w", e)
	}
	c.exitRegistered = true
	if err := started.releaseSlave(); err != nil {
		return nil, err
	}
	if started.cleanupErr != nil {
		return nil, errors.Join(ErrUnresolved, started.cleanupErr)
	}
	if e = c.status.SetReadDeadline(time.Now().Add(4 * time.Second)); e != nil {
		return nil, fmt.Errorf("runner: ready deadline: %w", e)
	}
	var ready gateFrame
	if e = readFrame(c.status, &ready, maxFrameBytes); e != nil {
		return nil, fmt.Errorf("runner: ready frame: %w", e)
	}
	second, e := readIdentity(c.identity.PID)
	if e != nil {
		return nil, fmt.Errorf("runner: second identity: %w", e)
	}
	if ready.Kind != "ready" || !ready.Identity.Valid() || ready.Identity != first || second != first {
		return nil, ErrIdentity
	}
	c.identity = first
	started.child = nil
	return c, nil
}

// releaseSlave converges the started PTY slave. os.File.Close invalidates the
// descriptor even when it reports an error, so the slave never survives one
// close attempt; os.ErrClosed from a repeated attempt is residue, not failure.
func (started *StartedChild) releaseSlave() error {
	if started.ptySlave == nil {
		return nil
	}
	err := closePTYSlave(started.ptySlave)
	started.ptySlave = nil
	if errors.Is(err, os.ErrClosed) {
		return nil
	}
	return err
}

func (started *StartedChild) Close() error {
	if started == nil || started.child == nil {
		return nil
	}
	slaveErr := started.releaseSlave()
	cleanupErr := started.child.hardCleanup()
	err := errors.Join(started.cleanupErr, slaveErr, cleanupErr)
	if cleanupErr == nil && started.ptySlave == nil && started.child.state == stateWaited && started.child.ptyMaster == nil {
		started.child = nil
		started.cleanupErr = nil
	}
	return err
}

// convergeStartedChild makes exactly three cleanup attempts. Cleanup still
// failing afterwards is permanent uncertainty that the caller must surface as
// its typed outcome; unbounded retry here made that arm unreachable.
func convergeStartedChild(started *StartedChild) error {
	var result error
	for attempt := 0; started != nil && started.child != nil && attempt < 3; attempt++ {
		if attempt > 0 {
			time.Sleep(25 * time.Millisecond)
		}
		result = errors.Join(result, started.Close())
	}
	return result
}

func (prepared *PreparedChild) Close() error {
	if prepared == nil || prepared.closed {
		return nil
	}
	prepared.closed = true
	err := closePreparedFiles(prepared.startFiles)
	prepared.startFiles = nil
	if prepared.leashW != nil {
		err = errors.Join(err, prepared.leashW.Close())
		prepared.leashW = nil
	}
	if prepared.statusR != nil {
		err = errors.Join(err, prepared.statusR.Close())
		prepared.statusR = nil
	}
	if prepared.kq >= 0 {
		err = errors.Join(err, unix.Close(prepared.kq))
		prepared.kq = -1
	}
	if prepared.ptyMaster != nil {
		err = errors.Join(err, prepared.ptyMaster.Close())
		prepared.ptyMaster = nil
	}
	if prepared.ptySlave != nil {
		err = errors.Join(err, prepared.ptySlave.Close())
		prepared.ptySlave = nil
	}
	return err
}

func closePreparedFiles(files []*os.File) error {
	var result error
	for _, file := range files {
		if file != nil {
			result = errors.Join(result, file.Close())
		}
	}
	return result
}

func anonymousFile(dir *os.File, prefix string, body []byte) (*os.File, error) {
	return anonymousFileWithHook(dir, prefix, body, nil)
}

func anonymousFileWithHook(dir *os.File, prefix string, body []byte, afterOpen func(string)) (*os.File, error) {
	name := ""
	switch prefix {
	case "config":
		name = GateConfigScratchName
	case "stdin":
		name = GateStdinScratchName
	default:
		return nil, ErrIdentity
	}
	fd, err := unix.Openat(int(dir.Fd()), name, unix.O_RDWR|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0o600)
	if err != nil {
		return nil, err
	}
	f := os.NewFile(uintptr(fd), name)
	if afterOpen != nil {
		afterOpen(name)
	}
	var stat unix.Stat_t
	if err := unix.Fstat(fd, &stat); err != nil {
		f.Close()
		return nil, err
	}
	if stat.Mode&unix.S_IFMT != unix.S_IFREG || stat.Mode&0o7777 != 0o600 || stat.Uid != uint32(os.Geteuid()) || stat.Nlink != 1 {
		_ = unlinkExactScratch(dir, name, stat)
		f.Close()
		return nil, ErrIdentity
	}
	// Fixed create-only names make a crash residue enumerable. The open inode
	// is unlinked immediately; successful launches leave no named scratch.
	if err := unix.Unlinkat(int(dir.Fd()), name, 0); err != nil {
		f.Close()
		return nil, err
	}
	if len(body) > 0 {
		if _, err := f.Write(body); err != nil {
			f.Close()
			return nil, err
		}
	}
	if _, err := f.Seek(0, 0); err != nil {
		f.Close()
		return nil, err
	}
	return f, nil
}

func (c *OwnedChild) Activate() (FileIdentity, error) {
	if c == nil || c.state != stateBlocked || c.lease.closed {
		return FileIdentity{}, ErrState
	}
	fd, err := unix.Openat(int(c.lease.dir.Fd()), c.lease.basename, unix.O_WRONLY|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0o600)
	if err != nil {
		return FileIdentity{}, err
	}
	f := os.NewFile(uintptr(fd), "activation-marker")
	id, err := statIdentity(f)
	if err == nil {
		err = unix.Fsync(fd)
	}
	if err == nil {
		err = unix.Fsync(int(c.lease.dir.Fd()))
	}
	closeErr := f.Close()
	if err == nil {
		err = closeErr
	}
	if err != nil {
		return FileIdentity{}, err
	}
	c.lease.marker = &id
	// The durable marker is the activation linearization point. From here on,
	// a lost leash acknowledgement is still an activated child, never inert.
	c.activated = true
	c.state = stateActivated
	if err := writeFrame(c.activation, gateFrame{Kind: "activate", Marker: &id}, maxFrameBytes); err != nil {
		return id, err
	}
	if err := c.activation.Close(); err != nil {
		return id, err
	}
	c.activation = nil
	return id, nil
}

func (c *OwnedChild) Abort() error {
	if c == nil || c.state != stateBlocked {
		return ErrState
	}
	var err error
	if c.activation != nil {
		err = c.activation.Close()
		c.activation = nil
	}
	if !c.exitObserved {
		if _, werr := c.waitForExit(4 * time.Second); werr != nil {
			return werr
		}
	}
	_, finishErr := c.finishInertAfterExit()
	return errors.Join(err, finishErr)
}

func (c *OwnedChild) waitForExit(timeout time.Duration) (unix.Kevent_t, error) {
	deadline := time.Now().Add(timeout)
	for {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return unix.Kevent_t{}, os.ErrDeadlineExceeded
		}
		events := make([]unix.Kevent_t, 1)
		ts := unix.NsecToTimespec(remaining.Nanoseconds())
		n, err := unix.Kevent(c.kq, nil, events, &ts)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return unix.Kevent_t{}, err
		}
		if n == 0 {
			return unix.Kevent_t{}, os.ErrDeadlineExceeded
		}
		ev := events[0]
		if ev.Filter == unix.EVFILT_READ {
			// Readiness from a filter retired by the owner loop can remain queued;
			// it is not a process transition and must not make cleanup fail open.
			continue
		}
		if ev.Ident != uint64(c.identity.PID) || ev.Filter != unix.EVFILT_PROC || ev.Fflags&unix.NOTE_EXIT == 0 {
			return unix.Kevent_t{}, ErrIdentity
		}
		c.exitObserved = true
		c.state = stateExited
		return ev, nil
	}
}

func (c *OwnedChild) FinishAfterExit(timeout time.Duration) (Exit, error) {
	if c == nil || !c.activated || c.state == stateWaited {
		return Exit{}, ErrState
	}
	if !c.exitObserved {
		if _, err := c.waitForExit(timeout); err != nil {
			return Exit{}, err
		}
	}
	waitErr := c.waitActivatedOnce()
	if c.state != stateWaited {
		return Exit{}, waitErr
	}
	_ = c.status.SetReadDeadline(time.Now().Add(100 * time.Millisecond))
	var frame gateFrame
	if ferr := readFrame(c.status, &frame, maxFrameBytes); ferr == nil && frame.Kind == "launch-error" {
		c.exit.LaunchErr = frame.Error
	}
	return c.exit, nil
}

func exitFromWait(ps *os.ProcessState, err error) Exit {
	e := Exit{Code: -1}
	if ps == nil {
		e.LaunchErr = fmt.Sprint(err)
		return e
	}
	ws, ok := ps.Sys().(syscall.WaitStatus)
	if !ok {
		e.LaunchErr = "unknown wait status"
		return e
	}
	if ws.Exited() {
		e.Code = ws.ExitStatus()
	}
	if ws.Signaled() {
		e.Signal = int(ws.Signal())
	}
	return e
}

func (c *OwnedChild) waitOnce() error {
	if c.state == stateWaited {
		return ErrState
	}
	err := c.cmd.Wait()
	c.state = stateWaited
	c.exit = exitFromWait(c.cmd.ProcessState, err)
	return err
}

func (c *OwnedChild) waitInertOnce() error {
	if c.activated {
		return ErrState
	}
	return c.waitOnce()
}

func (c *OwnedChild) finishInertAfterExit() (Exit, error) {
	if c == nil || c.activated || !c.exitObserved || c.state != stateExited {
		return Exit{}, ErrState
	}
	waitErr := c.waitInertOnce()
	if c.state != stateWaited {
		return Exit{}, waitErr
	}
	c.exit.Aborted = true
	return c.exit, nil
}

func (c *OwnedChild) waitActivatedOnce() error {
	if !c.activated || !c.exitObserved {
		return ErrState
	}
	if c.testConvergence != nil {
		if err := c.testConvergence(); err != nil {
			return err
		}
	}
	if err := killRemainingGroup(c.identity); err != nil {
		return err
	}
	return c.waitOnce()
}

func (c *OwnedChild) Terminate(timeout time.Duration) (Exit, error) {
	if c == nil || !c.activated || c.state == stateWaited || !c.identity.Valid() {
		return Exit{}, ErrState
	}
	if timeout <= 0 {
		timeout = defaultStopTimeout
	}
	var signalFailure error
	if !c.exitObserved {
		if err := c.signalGroup(unix.SIGTERM); err != nil {
			// The exact child can exit between signalOwnedGroup's census and
			// kill. Retain the failure while the owned kqueue waits for proof.
			signalFailure = fmt.Errorf("%w: TERM: %v", ErrUnresolved, err)
			if _, exitErr := c.waitForExit(timeout); exitErr != nil {
				return Exit{}, errors.Join(signalFailure, exitErr)
			}
		}
	}
	withSignalFailure := func(err error) error {
		if signalFailure == nil {
			return err
		}
		return errors.Join(signalFailure, err)
	}
	escalate := false
	if !c.exitObserved {
		_, err := c.waitForExit(timeout)
		if err != nil {
			if !errors.Is(err, os.ErrDeadlineExceeded) {
				return Exit{}, withSignalFailure(err)
			}
			escalate = true
		}
	}
	if !escalate {
		if err := waitForOwnedGroupQuiescence(c.identity, timeout); err != nil {
			if !errors.Is(err, os.ErrDeadlineExceeded) {
				return Exit{}, withSignalFailure(err)
			}
			escalate = true
		}
	}
	if escalate {
		if err := c.signalGroup(unix.SIGKILL); err != nil {
			return Exit{}, withSignalFailure(fmt.Errorf("%w: KILL: %v", ErrUnresolved, err))
		}
		if !c.exitObserved {
			if _, err := c.waitForExit(4 * time.Second); err != nil {
				return Exit{}, withSignalFailure(err)
			}
		}
	}
	waitErr := c.waitActivatedOnce()
	if c.state != stateWaited {
		return Exit{}, withSignalFailure(waitErr)
	}
	return c.exit, nil
}

func (c *OwnedChild) signalGroup(signal unix.Signal) error {
	if c.testSignal != nil {
		return c.testSignal(signal)
	}
	return signalOwnedGroup(c.identity, signal)
}

func (c *OwnedChild) waitedExit() (Exit, error) {
	if c == nil || c.state != stateWaited {
		return Exit{}, ErrState
	}
	return c.exit, nil
}

func waitForOwnedGroupQuiescence(leader Identity, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for {
		census, err := censusOwnedGroup(leader)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return err
		}
		if !census.hasLiveMember {
			return nil
		}
		if time.Now().After(deadline) {
			return os.ErrDeadlineExceeded
		}
		time.Sleep(5 * time.Millisecond)
	}
}

func killRemainingGroup(leader Identity) error {
	deadline := time.Now().Add(4 * time.Second)
	var noLiveSince time.Time
	for {
		census, err := censusOwnedGroup(leader)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return err
		}
		now := time.Now()
		if !now.Before(deadline) {
			return ErrUnresolved
		}
		if census.onlyLeader {
			return nil
		}
		if census.hasLiveMember {
			noLiveSince = time.Time{}
			if err := signalOwnedGroupBefore(leader, unix.SIGKILL, deadline); err != nil {
				return err
			}
		} else if noLiveSince.IsZero() {
			noLiveSince = now
		} else if !now.Before(noLiveSince.Add(groupSignalSettle)) {
			return nil
		}
		time.Sleep(5 * time.Millisecond)
	}
}

// testGroupSignalResult rewrites the exact group kill's reported errno for
// package tests. The kill itself is never faked — only what the kernel is
// taken to have reported — so the sole negated-PGID unix.Kill call stays
// exactly where the per-member-signal AST guard can see it.
var testGroupSignalResult func(error) error

// testGroupCensus can inject a package-test-only census result while retaining
// the real census as the default. It lets tests exercise transient syscall
// errors without replacing the process-group authority they are proving.
var testGroupCensus func(Identity) (ownedGroupCensus, error)

// groupSignalSettle bounds how long a failed group signal re-samples the
// census before deciding. It absorbs snapshot lag, never real liveness.
const groupSignalSettle = 250 * time.Millisecond

func signalOwnedGroup(leader Identity, signal unix.Signal) error {
	return signalOwnedGroupBefore(leader, signal, time.Time{})
}

func signalOwnedGroupBefore(leader Identity, signal unix.Signal, hardDeadline time.Time) error {
	deadlineReached := func(now time.Time) bool {
		return !hardDeadline.IsZero() && !now.Before(hardDeadline)
	}
	if deadlineReached(time.Now()) {
		return ErrUnresolved
	}
	census, err := censusForGroupSignal(leader)
	if err != nil {
		return err
	}
	if deadlineReached(time.Now()) {
		return ErrUnresolved
	}
	if !census.hasLiveMember {
		return nil
	}
	signalErr := unix.Kill(-leader.PGID, signal)
	if testGroupSignalResult != nil {
		signalErr = testGroupSignalResult(signalErr)
	}
	if signalErr == nil {
		return nil
	}
	// Re-census after ANY signal error: the census, not the errno, is the
	// absence authority, and Darwin reports more than one errno for a group
	// that has already converged.
	//
	// One sample is not proof. censusOwnedGroup reads a kern.proc.pgrp
	// snapshot, which can still list a member the kernel's own killpg walk
	// has already passed over — observed as EPERM answered by a census that
	// still claims life. Allow one bounded window for the exact census to first
	// become no-live. Once it does, start a separate bounded window in which
	// every sample must remain known no-live. A live or uncertain sample makes
	// this attempt unresolved and the caller retries the signal.
	phaseOneDeadline := time.Now().Add(groupSignalSettle)
	for {
		census, err = censusForGroupSignal(leader)
		if err != nil {
			if errors.Is(err, unix.EINTR) {
				// An interrupted census is an observation gap, not evidence of
				// continuous absence. Do not restart or extend this attempt.
				return classifyGroupSignal(signalErr, false)
			}
			return err
		}
		now := time.Now()
		if deadlineReached(now) {
			return classifyGroupSignal(signalErr, false)
		}
		if census.hasLiveMember {
			if !now.Before(phaseOneDeadline) {
				return classifyGroupSignal(signalErr, false)
			}
			time.Sleep(5 * time.Millisecond)
			continue
		}
		if !now.Before(phaseOneDeadline) {
			return classifyGroupSignal(signalErr, false)
		}

		phaseTwoDeadline := now.Add(groupSignalSettle)
		for {
			census, err = censusForGroupSignal(leader)
			if err != nil {
				if errors.Is(err, unix.EINTR) {
					// An interrupted census is an observation gap, not evidence
					// of continuous absence. Do not restart or extend either
					// proof window.
					return classifyGroupSignal(signalErr, false)
				}
				return err
			}
			now = time.Now()
			if deadlineReached(now) {
				return classifyGroupSignal(signalErr, false)
			}
			if census.hasLiveMember {
				return classifyGroupSignal(signalErr, false)
			}
			if !now.Before(phaseTwoDeadline) {
				return classifyGroupSignal(signalErr, true)
			}
			time.Sleep(5 * time.Millisecond)
		}
	}
}

func censusForGroupSignal(leader Identity) (ownedGroupCensus, error) {
	if testGroupCensus != nil {
		return testGroupCensus(leader)
	}
	return censusOwnedGroup(leader)
}

// classifyGroupSignal forgives a failed group signal only when the exact
// leader-anchored census proves no live member remains.
//
// signalOwnedGroup reaches the kill only after censusOwnedGroup proved this
// exact leader (PID, PGID and birth) present and unreaped, and an unreaped
// leader holds the pid that IS the process-group id — so on this path the
// group provably exists and its id cannot have been recycled. Darwin's
// observed vocabulary for that kill is: success while any live owned member
// remains, and EPERM once our unreaped zombie leader is the only member
// left. ESRCH is unreachable here, so refusing EPERM outright would refuse
// the one errno the kernel actually uses for a converged group.
//
// The safety asymmetry that makes this sound: the census enumerates the
// group through kern.proc.pgrp, which lists every member regardless of
// whether we may signal it, and marks any non-zombie live. A live member we
// merely lack permission to signal therefore leaves hasLiveMember true and
// is never forgiven — the census is a strict superset of what the kernel's
// killpg walk considers. Residual risk, unreproduced: a kill reporting EPERM
// while the census still reports a live member stays fail-closed unresolved,
// which is the safe direction.
func classifyGroupSignal(signalErr error, exactCensusHasNoLiveMembers bool) error {
	if signalErr == nil {
		return nil
	}
	if (errors.Is(signalErr, unix.ESRCH) || errors.Is(signalErr, unix.EPERM)) && exactCensusHasNoLiveMembers {
		return nil
	}
	return fmt.Errorf("%w: group signal: %v", ErrUnresolved, signalErr)
}

type ownedGroupCensus struct {
	onlyLeader    bool
	hasLiveMember bool
}

const darwinZombieState = 5

func censusOwnedGroup(leader Identity) (ownedGroupCensus, error) {
	observed, err := readIdentity(leader.PID)
	if err != nil || observed != leader {
		if errors.Is(err, unix.EINTR) {
			return ownedGroupCensus{}, unix.EINTR
		}
		return ownedGroupCensus{}, fmt.Errorf("%w: direct leader identity changed during group census", ErrUnresolved)
	}
	members, err := unix.SysctlKinfoProcSlice("kern.proc.pgrp", leader.PGID)
	if err != nil {
		if errors.Is(err, unix.EINTR) {
			return ownedGroupCensus{}, unix.EINTR
		}
		return ownedGroupCensus{}, fmt.Errorf("%w: group census: %v", ErrUnresolved, err)
	}
	foundLeader := false
	hasDescendant := false
	hasLiveMember := false
	for _, member := range members {
		identity := Identity{PID: int(member.Proc.P_pid), PGID: int(member.Eproc.Pgid), Birth: Birth{Seconds: member.Proc.P_starttime.Sec, Microseconds: member.Proc.P_starttime.Usec}}
		if identity.PID == leader.PID {
			if identity != leader {
				return ownedGroupCensus{}, fmt.Errorf("%w: leader changed during group census", ErrUnresolved)
			}
			foundLeader = true
			if member.Proc.P_stat != darwinZombieState {
				hasLiveMember = true
			}
			continue
		}
		hasDescendant = true
		if member.Proc.P_stat != darwinZombieState {
			hasLiveMember = true
		}
	}
	if !foundLeader {
		return ownedGroupCensus{}, fmt.Errorf("%w: exact leader missing from group census", ErrUnresolved)
	}
	return ownedGroupCensus{onlyLeader: !hasDescendant, hasLiveMember: hasLiveMember}, nil
}

func (c *OwnedChild) Close() error {
	if c == nil {
		return nil
	}
	if c.state != stateWaited {
		if c.activated {
			if _, err := c.Terminate(defaultStopTimeout); err != nil {
				return err
			}
		} else if c.state == stateBlocked {
			if err := c.Abort(); err != nil {
				return err
			}
		} else if c.state == stateExited {
			if err := c.waitInertOnce(); err != nil {
				return err
			}
		} else {
			return ErrState
		}
	}
	if c.activation != nil {
		_ = c.activation.Close()
	}
	if c.status != nil {
		_ = c.status.Close()
	}
	if c.kq >= 0 {
		_ = unix.Close(c.kq)
		c.kq = -1
	}
	return c.closePTY()
}
func (c *OwnedChild) hardCleanup() error {
	if c == nil || c.cmd == nil {
		return nil
	}
	if c.testHardCleanup != nil {
		if err := c.testHardCleanup(); err != nil {
			return err
		}
	}
	var cleanupErr error
	if err := c.cmd.Process.Kill(); err != nil && !errors.Is(err, os.ErrProcessDone) {
		cleanupErr = errors.Join(cleanupErr, err)
	}
	// An observed or reaped exit is already proven; re-waiting would consume
	// nothing and doom every retry once the one-shot event is gone.
	if c.exitRegistered && !c.exitObserved && c.state != stateWaited && c.kq >= 0 {
		if _, err := c.waitForExit(4 * time.Second); err != nil {
			cleanupErr = errors.Join(cleanupErr, err)
		}
	}
	if c.state != stateWaited {
		if err := c.waitInertOnce(); err != nil && c.state != stateWaited {
			cleanupErr = errors.Join(cleanupErr, err)
		}
	}
	if c.activation != nil {
		_ = c.activation.Close()
		c.activation = nil
	}
	if c.status != nil {
		_ = c.status.Close()
		c.status = nil
	}
	cleanupErr = errors.Join(cleanupErr, c.closePTY())
	// The kqueue is the only remaining exit-observation capability; a failed
	// pass keeps it so retry can still converge instead of losing the child.
	if cleanupErr != nil {
		return cleanupErr
	}
	if c.kq >= 0 {
		_ = unix.Close(c.kq)
		c.kq = -1
	}
	return nil
}

func (c *OwnedChild) closePTY() error {
	if c == nil || c.ptyMaster == nil {
		return nil
	}
	err := c.ptyMaster.Close()
	c.ptyMaster = nil
	return err
}

func registerExit(kq, pid int) error {
	change := unix.Kevent_t{Ident: uint64(pid), Filter: unix.EVFILT_PROC, Flags: unix.EV_ADD | unix.EV_ENABLE | unix.EV_ONESHOT | unix.EV_RECEIPT, Fflags: unix.NOTE_EXIT}
	events := make([]unix.Kevent_t, 1)
	n, err := unix.Kevent(kq, []unix.Kevent_t{change}, events, nil)
	if err != nil {
		return err
	}
	if n != 1 || events[0].Flags&unix.EV_ERROR == 0 {
		return ErrIdentity
	}
	if events[0].Data != 0 {
		return unix.Errno(events[0].Data)
	}
	return nil
}

func readIdentity(pid int) (Identity, error) {
	if pid <= 1 {
		return Identity{}, ErrIdentity
	}
	p, err := unix.SysctlKinfoProc("kern.proc.pid", pid)
	if err != nil {
		return Identity{}, err
	}
	id := Identity{PID: pid, PGID: int(p.Eproc.Pgid), Birth: Birth{Seconds: p.Proc.P_starttime.Sec, Microseconds: p.Proc.P_starttime.Usec}}
	if !id.Valid() || int(p.Proc.P_pid) != pid {
		return Identity{}, ErrIdentity
	}
	return id, nil
}

func ObserveProcess(want Identity) Observation {
	if !want.Valid() {
		return Observation{Presence: Unknown, Err: ErrIdentity}
	}
	got, err := readIdentity(want.PID)
	if err == nil {
		if got == want {
			return Observation{Presence: Present}
		}
		return Observation{Presence: Reused}
	}
	pzero := unix.Kill(want.PID, 0)
	gzero := unix.Kill(-want.PGID, 0)
	return classifyUnavailable(err, pzero, gzero)
}

func classifyUnavailable(infoErr, processProbe, groupProbe error) Observation {
	if (errors.Is(infoErr, unix.EIO) || errors.Is(infoErr, unix.ESRCH) || errors.Is(infoErr, unix.ENOENT)) && errors.Is(processProbe, unix.ESRCH) && errors.Is(groupProbe, unix.ESRCH) {
		return Observation{Presence: Absent}
	}
	return Observation{Presence: Unknown, Err: fmt.Errorf("%w: info=%v pid=%v group=%v", ErrUnresolved, infoErr, processProbe, groupProbe)}
}

func ObserveProcessGroup(want Identity) Observation {
	base := ObserveProcess(want)
	if base.Presence != Present {
		return base
	}
	procs, err := unix.SysctlKinfoProcSlice("kern.proc.pgrp", want.PGID)
	if err != nil {
		return Observation{Presence: Unknown, Err: err}
	}
	members := make([]Identity, 0, len(procs))
	for _, p := range procs {
		id := Identity{PID: int(p.Proc.P_pid), PGID: int(p.Eproc.Pgid), Birth: Birth{Seconds: p.Proc.P_starttime.Sec, Microseconds: p.Proc.P_starttime.Usec}}
		if !id.Valid() {
			return Observation{Presence: Unknown, Err: ErrIdentity}
		}
		members = append(members, id)
	}
	return Observation{Presence: Present, Members: members}
}

func RunExecGate() error {
	config := os.NewFile(3, "gate-config")
	leash := os.NewFile(4, "gate-leash")
	status := os.NewFile(5, "gate-status")
	stdin := os.NewFile(6, "target-stdin")
	stdout := os.NewFile(7, "target-stdout")
	stderr := os.NewFile(8, "target-stderr")
	leaseDir := os.NewFile(9, "lease-dir")
	lifetime := os.NewFile(10, "runtime-lifetime")
	for _, f := range []*os.File{config, leash, status, stdin, stdout, stderr, leaseDir, lifetime} {
		if f == nil {
			return fmt.Errorf("runner: missing inherited capability")
		}
	}
	// Keep FD 3 reserved until control is remapped onto it. Wrapping the
	// nonblocking FD 11 may initialize Go's netpoll kqueue; a free FD 3 would
	// let that runtime-owned descriptor be allocated where Dup2 later closes it.
	defer config.Close()
	var cfg gateConfig
	if err := readFrame(io.LimitReader(config, maxConfigBytes+4), &cfg, maxConfigBytes); err != nil {
		return err
	}
	if cfg.Version != 1 || cfg.MarkerName == "" || filepath.Base(cfg.MarkerName) != cfg.MarkerName {
		return fmt.Errorf("runner: invalid gate config")
	}
	if got, err := commitOpen(leaseDir, cfg.LeaseDirectory.Path, false); err != nil || got.FileIdentity != cfg.LeaseDirectory.FileIdentity || got.UID != cfg.LeaseDirectory.UID || got.GID != cfg.LeaseDirectory.GID || got.Mode != cfg.LeaseDirectory.Mode {
		return fmt.Errorf("runner: lease directory: %w", ErrIdentity)
	}
	if got, err := commitRuntimeLifetime(leaseDir, lifetime); err != nil || got != cfg.Lifetime {
		return fmt.Errorf("runner: runtime lifetime: %w", errors.Join(ErrIdentity, err))
	}
	var control *os.File
	if cfg.Control != nil {
		control = os.NewFile(11, "target-control")
		if control == nil || verifyControl(control, *cfg.Control) != nil {
			return fmt.Errorf("runner: control capability: %w", ErrIdentity)
		}
	}
	target, err := verifyCommit(cfg.Target.Executable, true)
	if err != nil {
		return fmt.Errorf("runner: prepared executable: %w", err)
	}
	_ = target.Close()
	cwd, err := verifyCommit(cfg.Target.Cwd, false)
	if err != nil {
		return fmt.Errorf("runner: prepared cwd: %w", err)
	}
	defer cwd.Close()
	id, err := readIdentity(os.Getpid())
	if err != nil {
		return err
	}
	if err := writeFrame(status, gateFrame{Kind: "ready", Identity: id}, maxFrameBytes); err != nil {
		return err
	}
	var activation gateFrame
	if err := readFrame(leash, &activation, maxFrameBytes); err != nil {
		if errors.Is(err, io.EOF) {
			_ = writeFrame(status, gateFrame{Kind: "aborted"}, maxFrameBytes)
			return nil
		}
		return err
	}
	_ = leash.Close()
	if activation.Kind != "activate" || activation.Marker == nil {
		return fmt.Errorf("runner: invalid activation")
	}
	var marker unix.Stat_t
	if err := unix.Fstatat(int(leaseDir.Fd()), cfg.MarkerName, &marker, unix.AT_SYMLINK_NOFOLLOW); err != nil {
		return err
	}
	if marker.Mode&unix.S_IFMT != unix.S_IFREG || marker.Mode&0o777 != 0o600 || uint64(marker.Dev) != activation.Marker.Device || marker.Ino != activation.Marker.Inode {
		return ErrIdentity
	}
	target, err = verifyCommit(cfg.Target.Executable, true)
	if err != nil {
		_ = writeFrame(status, gateFrame{Kind: "launch-error", Error: err.Error()}, maxFrameBytes)
		return err
	}
	if err := sameNamedIdentity(cfg.Target.Executable.Path, cfg.Target.Executable.FileIdentity); err != nil {
		target.Close()
		_ = writeFrame(status, gateFrame{Kind: "launch-error", Error: err.Error()}, maxFrameBytes)
		return err
	}
	if cfg.TestFinalCheck {
		// This package-test-only barrier makes the documented cooperative
		// same-UID pathname race measurable; it is not a production defense.
		seamFD := uintptr(11)
		if control != nil {
			seamFD = 12
		}
		seam := os.NewFile(seamFD, "test-final-check")
		if seam == nil {
			return ErrIdentity
		}
		if _, err := seam.Write([]byte{'R'}); err != nil {
			return err
		}
		var ack [1]byte
		if _, err := io.ReadFull(seam, ack[:]); err != nil || ack[0] != 'X' {
			return fmt.Errorf("runner: test final-check seam: %v", err)
		}
		_ = seam.Close()
	}
	_ = target.Close()
	if err := unix.Fchdir(int(cwd.Fd())); err != nil {
		return err
	}
	_ = cwd.Close()
	if !cfg.PTY {
		if err := unix.Dup2(int(stdin.Fd()), 0); err != nil {
			return err
		}
		if err := unix.Dup2(int(stdout.Fd()), 1); err != nil {
			return err
		}
		if err := unix.Dup2(int(stderr.Fd()), 2); err != nil {
			return err
		}
	}
	if control != nil {
		if err := unix.Dup2(int(control.Fd()), int(config.Fd())); err != nil {
			return err
		}
		if err := control.Close(); err != nil {
			return fmt.Errorf("runner: close staged control: %w", err)
		}
	} else if err := config.Close(); err != nil {
		return fmt.Errorf("runner: close gate config: %w", err)
	}
	if err := errors.Join(stdin.Close(), stdout.Close(), stderr.Close()); err != nil {
		return fmt.Errorf("runner: close staged standard streams: %w", err)
	}
	if _, err := unix.FcntlInt(status.Fd(), unix.F_SETFD, unix.FD_CLOEXEC); err != nil {
		return fmt.Errorf("runner: seal gate status: %w", err)
	}
	if !cfg.KeepDirectory {
		if err := leaseDir.Close(); err != nil {
			return fmt.Errorf("runner: close gate directory: %w", err)
		}
	}
	if _, err := unix.FcntlInt(lifetime.Fd(), unix.F_SETFD, 0); err != nil {
		return fmt.Errorf("runner: retain runtime lifetime: %w", err)
	}
	if cfg.PTY {
		if err := validatePTYDescriptors(); err != nil {
			return err
		}
	}
	if err := unix.Exec(cfg.Target.Executable.Path, cfg.Target.Argv, cfg.Target.Env); err != nil {
		_ = writeFrame(status, gateFrame{Kind: "launch-error", Error: err.Error()}, maxFrameBytes)
		return err
	}
	return nil
}
