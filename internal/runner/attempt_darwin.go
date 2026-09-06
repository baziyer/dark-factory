//go:build darwin

package runner

import (
	"bytes"
	"context"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"time"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

type attemptControllerState uint8

const (
	controllerNew attemptControllerState = iota + 1
	controllerConfigured
	controllerInnerReady
	controllerSelectionReleased
	controllerSelectionReported
	controllerPreparationReleased
	controllerPreparationReported
	controllerPopulationReleased
	controllerPopulationReported
	controllerProviderReleased
	controllerResult
	controllerSpent
)

// AttemptController is the daemon side of one fixed attempt-runner protocol.
// Its peer is the sole control capability accepted by --attempt-runner.
type AttemptController struct {
	file          *os.File
	state         attemptControllerState
	attemptID     string
	inner         Identity
	terminalReady bool
}

// writeFrame is the controller's only authoritative write path. A failed
// framed write may have left a peer with an indistinguishable prefix, so the
// capability is spent immediately and can never append a retry to it.
func (c *AttemptController) writeFrame(value any, limit int) error {
	if c == nil || c.file == nil {
		return ErrState
	}
	err := writeControlFrame(c.file, value, limit)
	if err == nil {
		return nil
	}
	return errors.Join(err, c.spend())
}

// spend closes the sole control capability and puts the controller in its
// terminal state, so every later call answers ErrState.
func (c *AttemptController) spend() error {
	closeErr := c.file.Close()
	c.file = nil
	c.state = controllerSpent
	return closeErr
}

// controlSocketBytes is each control socket's send and receive buffer. Darwin
// gives a Unix stream socket eight kilobytes, and the attempt config the
// daemon writes before the runner is released to read it carries the
// provider's prompt, so a real task's config did not fit and the write sat
// against a peer that could not drain it until the control timeout.
const controlSocketBytes = maxConfigBytes + maxFrameBytes

// newControlSocketPair makes one non-blocking Unix stream pair whose buffers
// hold the largest frame either side writes.
func newControlSocketPair() ([2]int, error) {
	fds, err := unix.Socketpair(unix.AF_UNIX, unix.SOCK_STREAM, 0)
	if err != nil {
		return [2]int{}, err
	}
	for _, fd := range fds {
		if err == nil {
			err = unix.SetNonblock(fd, true)
		}
		for _, option := range []int{unix.SO_SNDBUF, unix.SO_RCVBUF} {
			if err == nil {
				err = unix.SetsockoptInt(fd, unix.SOL_SOCKET, option, controlSocketBytes)
			}
			if size, getErr := unix.GetsockoptInt(fd, unix.SOL_SOCKET, option); err == nil && (getErr != nil || size < controlSocketBytes) {
				err = fmt.Errorf("runner: control socket buffer %d below %d: %w", size, controlSocketBytes, ErrState)
			}
		}
	}
	if err != nil {
		unix.Close(fds[0])
		unix.Close(fds[1])
		return [2]int{}, err
	}
	return fds, nil
}

func NewAttemptController() (*AttemptController, *os.File, error) {
	fds, err := newControlSocketPair()
	if err != nil {
		return nil, nil, err
	}
	unix.CloseOnExec(fds[0])
	unix.CloseOnExec(fds[1])
	parent := os.NewFile(uintptr(fds[0]), "attempt-daemon-control")
	child := os.NewFile(uintptr(fds[1]), "attempt-runner-control")
	if _, err := commitControl(parent); err != nil {
		parent.Close()
		child.Close()
		return nil, nil, err
	}
	if _, err := commitControl(child); err != nil {
		parent.Close()
		child.Close()
		return nil, nil, err
	}
	return &AttemptController{file: parent, state: controllerNew}, child, nil
}

func (c *AttemptController) Configure(spec AttemptSpec) error {
	if c == nil || c.file == nil || c.state != controllerNew || spec.Wrapper == nil {
		return ErrState
	}
	if err := validateAttemptName(spec.AttemptID, 256); err != nil {
		return err
	}
	if spec.MarkerName != InnerActivationMarkerName || spec.ResultName != AttemptResultSpoolName {
		return ErrIdentity
	}
	proof, err := encodeResultProof(spec.ResultProof)
	if err != nil {
		return err
	}
	if spec.Wrapper.control != nil || spec.Wrapper.controlID != nil || len(spec.Wrapper.stdin) != 0 || spec.Wrapper.stdout != nil || spec.Wrapper.stderr != nil || spec.Wrapper.testFinal != nil || spec.Wrapper.testCurrentFinal {
		return fmt.Errorf("runner: wrapper launch contains unsupported capabilities")
	}
	if len(spec.StartupInput) > MaxProviderTaskBytes || len(spec.StartupInput) > 0 && (!utf8.Valid(spec.StartupInput) || bytes.IndexByte(spec.StartupInput, 0) >= 0) {
		return ErrState
	}
	cfg := attemptConfig{
		Version: 1, AttemptID: spec.AttemptID, Wrapper: spec.Wrapper.commit,
		MarkerName: spec.MarkerName, ResultName: spec.ResultName, ResultProof: proof,
		StartupInput: append([]byte(nil), spec.StartupInput...),
	}
	if err := c.writeFrame(cfg, maxConfigBytes); err != nil {
		return err
	}
	c.state = controllerConfigured
	c.attemptID = spec.AttemptID
	return nil
}

func (c *AttemptController) Next(timeout time.Duration) (AttemptEvent, error) {
	if c == nil || c.file == nil {
		return AttemptEvent{}, ErrState
	}
	if timeout <= 0 {
		timeout = attemptControlTimeout
	}
	if err := c.file.SetReadDeadline(time.Now().Add(timeout)); err != nil {
		return AttemptEvent{}, err
	}
	defer c.file.SetReadDeadline(time.Time{})
	var frame attemptFrame
	if err := readFrame(c.file, &frame, maxConfigBytes); err != nil {
		// These two errors, and only these two, mean the read stopped because
		// the stream ended: readFrame gets them from io.ReadFull, and
		// decodeFrameBody renames the decoder's identically-named answers so a
		// malformed body cannot reach here. A deadline reports itself
		// separately and never lands here.
		//
		// The stream ending is conclusive rather than transient, because this
		// control socket is only ever closed outright. Nothing in the tree
		// half-closes it with shutdown(2), which is the one way a live peer
		// could show a reader EOF while still accepting writes. Given that,
		// no further frame can arrive and no later write can be delivered, so
		// spending the capability makes ErrState — rather than a broken pipe
		// from a write nobody could have received — the answer to every call
		// that follows.
		if errors.Is(err, io.EOF) || errors.Is(err, io.ErrUnexpectedEOF) {
			return AttemptEvent{}, errors.Join(err, c.spend())
		}
		return AttemptEvent{}, err
	}
	if frame.Version != 1 {
		return AttemptEvent{}, ErrIdentity
	}
	if frame.Kind == string(AttemptResultReady) {
		return c.acceptAttemptResult(frame)
	}
	switch c.state {
	case controllerConfigured:
		if frame.Kind != "inner-ready" || !frame.Identity.Valid() || frame.Identity.PID != frame.Identity.PGID || frame.Stage != "" || len(frame.Payload) != 0 || frame.FileIdentity != nil || frame.Digest != "" || !noTerminalFields(frame) {
			return AttemptEvent{}, ErrState
		}
		c.state = controllerInnerReady
		c.inner = frame.Identity
		return AttemptEvent{Kind: AttemptInnerReady, Identity: frame.Identity}, nil
	case controllerSelectionReleased:
		return c.acceptCheckpoint(frame, StageSelection, controllerSelectionReported)
	case controllerPreparationReleased:
		return c.acceptCheckpoint(frame, StagePreparation, controllerPreparationReported)
	case controllerPopulationReleased:
		return c.acceptCheckpoint(frame, StagePopulation, controllerPopulationReported)
	case controllerProviderReleased:
		if isTerminalEventKind(frame.Kind) {
			if frame.Version != commandVersion || !validTerminalEnvelope(frame, false) {
				return AttemptEvent{}, ErrState
			}
			event, err := terminalEventFromFrame(frame)
			if err != nil {
				return AttemptEvent{}, err
			}
			if event.Kind == TerminalReady {
				if c.terminalReady {
					return AttemptEvent{}, ErrState
				}
				c.terminalReady = true
			}
			return AttemptEvent{Kind: AttemptTerminalFrame, Frame: &event}, nil
		}
		if frame.Kind == "current-exec-check" && noLegacyFields(frame) && noTerminalFields(frame) && len(frame.Payload) == 0 {
			return AttemptEvent{Kind: AttemptCheckpoint, Stage: StageProvider}, nil
		}
		return AttemptEvent{}, ErrState
	default:
		return AttemptEvent{}, ErrState
	}
}

func (c *AttemptController) acceptAttemptResult(frame attemptFrame) (AttemptEvent, error) {
	if c.state < controllerConfigured || c.state >= controllerResult || frame.Stage != "" || frame.Identity != (Identity{}) || len(frame.Payload) != 0 || frame.FileIdentity == nil || frame.FileIdentity.Device == 0 || frame.FileIdentity.Inode == 0 || len(frame.Digest) != 64 || !noTerminalFields(frame) {
		return AttemptEvent{}, ErrState
	}
	if _, err := hex.DecodeString(frame.Digest); err != nil || frame.Digest != strings.ToLower(frame.Digest) {
		return AttemptEvent{}, ErrIdentity
	}
	notice := AttemptResultNotice{Identity: *frame.FileIdentity, Digest: frame.Digest}
	c.state = controllerResult
	return AttemptEvent{Kind: AttemptResultReady, Result: &notice}, nil
}

func isTerminalEventKind(kind string) bool {
	switch TerminalEventKind(kind) {
	case TerminalGenerationResult, TerminalInputResult, TerminalResizeResult, TerminalHumanReplyResult, TerminalAttached, TerminalOutput, TerminalReset, TerminalReady, TerminalPTYEOF:
		return true
	default:
		return false
	}
}

// NextReady waits for the exact control socket to become readable or close
// without consuming any frame bytes. A timeout is an ordinary false result;
// callers invoke Next once with its full frame timeout only after readiness.
func (c *AttemptController) NextReady(timeout time.Duration) (bool, error) {
	if c == nil || c.file == nil || timeout < 0 {
		return false, ErrState
	}
	milliseconds := int(timeout / time.Millisecond)
	if timeout > 0 && milliseconds == 0 {
		milliseconds = 1
	}
	fds := []unix.PollFd{{Fd: int32(c.file.Fd()), Events: unix.POLLIN}}
	for {
		count, err := unix.Poll(fds, milliseconds)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return false, err
		}
		if count == 0 {
			return false, nil
		}
		revents := fds[0].Revents
		if revents&unix.POLLNVAL != 0 || revents&^(unix.POLLIN|unix.POLLHUP|unix.POLLERR) != 0 {
			return false, ErrIdentity
		}
		if revents&(unix.POLLIN|unix.POLLHUP|unix.POLLERR) != 0 {
			return true, nil
		}
		return false, ErrIdentity
	}
}

func (c *AttemptController) acceptCheckpoint(frame attemptFrame, stage AttemptStage, next attemptControllerState) (AttemptEvent, error) {
	if frame.Kind != "checkpoint" || frame.Stage != stage || frame.Identity != (Identity{}) || len(frame.Payload) > maxAttemptReportBytes || frame.FileIdentity != nil || frame.Digest != "" || !noTerminalFields(frame) {
		return AttemptEvent{}, ErrState
	}
	c.state = next
	return AttemptEvent{Kind: AttemptCheckpoint, Stage: stage, Payload: append([]byte{}, frame.Payload...)}, nil
}

func (c *AttemptController) Release(stage AttemptStage) error {
	if c == nil || c.file == nil {
		return ErrState
	}
	var want attemptControllerState
	var next attemptControllerState
	switch stage {
	case StageSelection:
		want, next = controllerInnerReady, controllerSelectionReleased
	case StagePreparation:
		want, next = controllerSelectionReported, controllerPreparationReleased
	case StagePopulation:
		want, next = controllerPreparationReported, controllerPopulationReleased
	case StageProvider:
		want, next = controllerPopulationReported, controllerProviderReleased
	default:
		return ErrState
	}
	if c.state != want {
		return ErrState
	}
	if err := c.writeFrame(attemptFrame{Version: 1, Kind: "release", Stage: stage}, maxFrameBytes); err != nil {
		return err
	}
	c.state = next
	return nil
}

// SendTerminalCommand validates and writes one complete terminal command to
// the already-running outer runner. The daemon supervisor owns the controller
// call and serializes it with its other lifecycle operations; this method does
// not add a concurrent RPC abstraction or a background writer.
func (c *AttemptController) SendTerminalCommand(command TerminalCommand) error {
	if c == nil || c.file == nil || c.state != controllerProviderReleased || !c.terminalReady {
		return ErrState
	}
	if err := command.validate(); err != nil {
		return err
	}
	return c.writeFrame(terminalCommandFrame(command), maxFrameBytes)
}

func (c *AttemptController) Terminate() error {
	if c == nil || c.file == nil || c.state < controllerInnerReady || c.state >= controllerResult {
		return ErrState
	}
	return c.writeFrame(attemptFrame{Version: 1, Kind: "terminate"}, maxFrameBytes)
}

// acknowledgeCurrentExecCheck exists only for the deterministic package test
// of the documented same-UID pathname race. Production controllers never see
// this frame because production LaunchSpecs cannot enable the seam.
func (c *AttemptController) acknowledgeCurrentExecCheck() error {
	if c == nil || c.file == nil || c.state != controllerProviderReleased {
		return ErrState
	}
	return c.writeFrame(attemptFrame{Version: 1, Kind: "current-exec-check-ack"}, maxFrameBytes)
}

// Spent reports that the capability ended by failing, rather than because a
// caller closed it. Only writeFrame and Next set that state: a read that ended
// the stream, or a write that could not be completed. The second is broader
// than a dead peer — a write that exhausts its deadline against a live but
// unresponsive peer also spends the capability, because a partial frame is
// unrecoverable either way. What this is not is a fact reconstructed by
// matching sentinels in an error tree any decoder or subprocess could have
// contributed to.
func (c *AttemptController) Spent() bool {
	return c != nil && c.state == controllerSpent
}

// Closed reports that the capability is gone, however it ended. Spent says
// which of the two ways.
func (c *AttemptController) Closed() bool {
	return c == nil || c.file == nil
}

// Close drops the capability deliberately. It leaves the state alone, so Spent
// keeps telling a caller which of the two ended it; and it clears the file even
// when the close itself fails, so no caller can be handed a descriptor it
// might use again.
func (c *AttemptController) Close() error {
	if c == nil || c.file == nil {
		return nil
	}
	err := c.file.Close()
	c.file = nil
	return err
}

type workerState uint8

const (
	workerConfig workerState = iota + 1
	workerConfigConsumed
	workerSelection
	workerSelectionReported
	workerPreparation
	workerPreparationReported
	workerPopulation
	workerPopulationReported
	workerProvider
	workerExec
)

// WorkerControl is the source wrapper's one inherited private capability. It
// exposes the fixed checkpoint sequence and the sole same-process provider
// exec; it does not expose arbitrary frames or descriptors.
type WorkerControl struct {
	file                  *os.File
	dir                   *os.File
	dirID                 fileCommitment
	dirIssued             bool
	lifetime              *os.File
	lifetimeID            descriptorCommitment
	identity              Identity
	state                 workerState
	providerErrorReported bool
}

func OpenWorkerControl() (*WorkerControl, error) {
	control := os.NewFile(3, "attempt-worker-control")
	dir := os.NewFile(9, "attempt-private-dir")
	lifetime := os.NewFile(10, "runtime-lifetime")
	if control == nil || dir == nil || lifetime == nil {
		return nil, ErrIdentity
	}
	keep := false
	defer func() {
		if !keep {
			control.Close()
			dir.Close()
			lifetime.Close()
		}
	}()
	if _, err := commitControl(control); err != nil {
		return nil, fmt.Errorf("runner: worker control: %w", err)
	}
	dirID, err := validatePrivateDirectory(dir)
	if err != nil {
		return nil, fmt.Errorf("runner: worker private directory: %w", err)
	}
	lifetimeID, err := commitRuntimeLifetime(dir, lifetime)
	if err != nil {
		return nil, fmt.Errorf("runner: worker runtime lifetime: %w", err)
	}
	// These capabilities belong only to the wrapper. Its bounded Git children
	// and eventual provider must never gain accidental directory authority.
	// Only the empty regular lifetime file is deliberately retained for exec.
	unix.CloseOnExec(3)
	unix.CloseOnExec(9)
	if _, err := unix.FcntlInt(lifetime.Fd(), unix.F_SETFD, 0); err != nil {
		return nil, err
	}
	id, err := readIdentity(os.Getpid())
	if err != nil || id.PID != id.PGID {
		return nil, ErrIdentity
	}
	keep = true
	return &WorkerControl{file: control, dir: dir, dirID: dirID, lifetime: lifetime, lifetimeID: lifetimeID, identity: id, state: workerConfig}, nil
}

func (w *WorkerControl) Identity() Identity {
	if w == nil {
		return Identity{}
	}
	return w.identity
}

// ReadConfig receives the daemon-owned worker configuration over the already
// inherited control capability. The read is one-shot even when the peer sends
// a truncated or otherwise invalid frame: retrying on a partially consumed
// stream could reinterpret the remaining bytes as a second configuration.
func (w *WorkerControl) ReadConfig(maximum int) ([]byte, error) {
	if w == nil || w.file == nil || w.state != workerConfig || maximum <= 0 || maximum > maxConfigBytes {
		return nil, ErrState
	}
	w.state = workerConfigConsumed
	if err := w.file.SetReadDeadline(time.Now().Add(attemptControlTimeout)); err != nil {
		return nil, err
	}
	defer w.file.SetReadDeadline(time.Time{})
	var config json.RawMessage
	if err := readFrame(w.file, &config, maximum); err != nil {
		return nil, err
	}
	if len(config) == 0 || !json.Valid(config) {
		return nil, ErrIdentity
	}
	w.state = workerSelection
	return append([]byte(nil), config...), nil
}

// DuplicateRuntimeDirectory returns the worker's one caller-owned CLOEXEC
// duplicate of its already-validated fd9 runtime directory. A successful or
// effectful attempt is one-shot; the caller owns and must close the result.
func (w *WorkerControl) DuplicateRuntimeDirectory(ctx context.Context) (_ *os.File, resultErr error) {
	if ctx == nil || w == nil || w.file == nil || w.dir == nil || w.lifetime == nil || w.state != workerSelection || w.dirIssued {
		return nil, ErrState
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	w.dirIssued = true
	got, err := validatePrivateDirectory(w.dir)
	if err != nil || got != w.dirID {
		return nil, fmt.Errorf("runner: worker runtime directory: %w", errors.Join(ErrIdentity, err))
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	fd, err := unix.FcntlInt(w.dir.Fd(), unix.F_DUPFD_CLOEXEC, 0)
	if err != nil {
		return nil, err
	}
	duplicate := os.NewFile(uintptr(fd), "worker-runtime-directory")
	keep := false
	defer func() {
		if !keep {
			resultErr = errors.Join(resultErr, duplicate.Close())
		}
	}()
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	duplicateID, err := validatePrivateDirectory(duplicate)
	if err != nil || duplicateID != w.dirID {
		return nil, fmt.Errorf("runner: duplicate runtime directory: %w", errors.Join(ErrIdentity, err))
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	originalID, err := validatePrivateDirectory(w.dir)
	if err != nil || originalID != w.dirID {
		return nil, fmt.Errorf("runner: recheck runtime directory: %w", errors.Join(ErrIdentity, err))
	}
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	keep = true
	return duplicate, nil
}

func (w *WorkerControl) ReportSelection(payload []byte) error {
	return w.report(StageSelection, workerSelection, workerSelectionReported, payload)
}

func (w *WorkerControl) AwaitPreparation() error {
	return w.await(StagePreparation, workerSelectionReported, workerPreparation)
}

func (w *WorkerControl) ReportPreparation(payload []byte) error {
	return w.report(StagePreparation, workerPreparation, workerPreparationReported, payload)
}

func (w *WorkerControl) AwaitPopulation() error {
	return w.await(StagePopulation, workerPreparationReported, workerPopulation)
}

func (w *WorkerControl) ReportPopulation(payload []byte) error {
	return w.report(StagePopulation, workerPopulation, workerPopulationReported, payload)
}

func (w *WorkerControl) AwaitProvider() error {
	return w.await(StageProvider, workerPopulationReported, workerProvider)
}

// ReportProviderError is the one bounded pre-exec failure report. It lets the
// outer owner distinguish a deliberate final source/authority rejection from
// an unexplained capability close, without introducing a general worker RPC.
func (w *WorkerControl) ReportProviderError(cause error) error {
	if w == nil || w.file == nil || w.state != workerProvider || w.providerErrorReported || cause == nil {
		return ErrState
	}
	message, err := providerErrorPayload(cause)
	if err != nil {
		return err
	}
	if err := writeControlFrame(w.file, attemptFrame{Version: 1, Kind: "provider-exec-error", Payload: message}, maxFrameBytes); err != nil {
		return err
	}
	w.providerErrorReported = true
	return nil
}

func (w *WorkerControl) report(stage AttemptStage, before, after workerState, payload []byte) error {
	if w == nil || w.file == nil || w.state != before || len(payload) > maxAttemptReportBytes {
		return ErrState
	}
	if err := writeControlFrame(w.file, attemptFrame{Version: 1, Kind: "checkpoint", Stage: stage, Payload: append([]byte{}, payload...)}, maxConfigBytes); err != nil {
		return err
	}
	w.state = after
	return nil
}

func (w *WorkerControl) await(stage AttemptStage, before, after workerState) error {
	if w == nil || w.file == nil || w.state != before {
		return ErrState
	}
	if err := w.file.SetReadDeadline(time.Now().Add(attemptControlTimeout)); err != nil {
		return err
	}
	defer w.file.SetReadDeadline(time.Time{})
	var frame attemptFrame
	if err := readFrame(w.file, &frame, maxFrameBytes); err != nil {
		return err
	}
	if frame.Version != 1 || frame.Kind != "release" || frame.Stage != stage || frame.Identity != (Identity{}) || len(frame.Payload) != 0 || frame.FileIdentity != nil || frame.Digest != "" || !noTerminalFields(frame) {
		return ErrState
	}
	w.state = after
	return nil
}

// ExecProvider takes ownership of cwd and an optional shell task on every call.
// Shell supplies an exact unlinked descriptor inherited as fd 11. Claude and
// Codex supply nil: Claude receives startup PTY input, while Codex reads its
// task through the attempt API.
func (w *WorkerControl) ExecProvider(spec *LaunchSpec, cwd, task *os.File) error {
	if w == nil || w.file == nil || w.dir == nil || w.lifetime == nil || w.state != workerProvider || spec == nil || cwd == nil || len(spec.stdin) != 0 || spec.control != nil || spec.controlID != nil || spec.stdout != nil || spec.stderr != nil {
		if cwd != nil {
			_ = cwd.Close()
		}
		if task != nil {
			_ = task.Close()
		}
		return ErrState
	}
	w.state = workerExec
	err := execPreparedCurrent(spec, cwd, task, w)
	if err != nil && w.file != nil {
		message, payloadErr := providerErrorPayload(err)
		// A failed pre-exec check must be observable by the outer owner. EOF
		// alone is ambiguous with successful unix.Exec, so publish one bounded
		// diagnostic before the worker exits; this is private control evidence,
		// not public provider output.
		if payloadErr == nil {
			_ = writeControlFrame(w.file, attemptFrame{Version: 1, Kind: "provider-exec-error", Payload: message}, maxFrameBytes)
			w.providerErrorReported = true
		}
	}
	return err
}

func providerErrorPayload(cause error) ([]byte, error) {
	if cause == nil {
		return nil, ErrState
	}
	message := cause.Error()
	if message == "" || !utf8.ValidString(message) || strings.IndexByte(message, 0) >= 0 {
		return nil, ErrState
	}
	if len(message) > maxProviderErrorBytes {
		message = message[:maxProviderErrorBytes]
		for !utf8.ValidString(message) {
			message = message[:len(message)-1]
		}
	}
	if message == "" {
		return nil, ErrState
	}
	return []byte(message), nil
}

func (w *WorkerControl) Close() error {
	if w == nil {
		return nil
	}
	var err error
	if w.file != nil {
		err = errors.Join(err, w.file.Close())
		w.file = nil
	}
	if w.dir != nil {
		err = errors.Join(err, w.dir.Close())
		w.dir = nil
	}
	if w.lifetime != nil {
		err = errors.Join(err, w.lifetime.Close())
		w.lifetime = nil
	}
	return err
}

func execPreparedCurrent(spec *LaunchSpec, cwd, task *os.File, worker *WorkerControl) (resultErr error) {
	defer func() {
		if cwd != nil {
			resultErr = errors.Join(resultErr, cwd.Close())
		}
		if task != nil {
			resultErr = errors.Join(resultErr, task.Close())
		}
	}()
	if task != nil {
		if err := validateProviderTask(task); err != nil {
			return fmt.Errorf("runner: provider task: %w", err)
		}
	}
	unix.CloseOnExec(int(cwd.Fd()))
	if err := verifyCurrentDirectory(cwd, spec.commit.Cwd); err != nil {
		return fmt.Errorf("runner: current cwd: %w", err)
	}
	target, err := verifyCommit(spec.commit.Executable, true)
	if err != nil {
		return fmt.Errorf("runner: current executable: %w", err)
	}
	if err := sameNamedIdentity(spec.commit.Executable.Path, spec.commit.Executable.FileIdentity); err != nil {
		_ = target.Close()
		return err
	}
	if spec.testCurrentFinal {
		if err := writeControlFrame(worker.file, attemptFrame{Version: 1, Kind: "current-exec-check"}, maxFrameBytes); err != nil {
			_ = target.Close()
			return err
		}
		var ack attemptFrame
		if err := readFrame(worker.file, &ack, maxFrameBytes); err != nil || !validCurrentExecCheckAck(ack) {
			_ = target.Close()
			return fmt.Errorf("runner: current exec test seam: %w", errors.Join(err, ErrState))
		}
	}
	if err := target.Close(); err != nil {
		return err
	}
	if got, err := commitRuntimeLifetime(worker.dir, worker.lifetime); err != nil || got != worker.lifetimeID {
		return fmt.Errorf("runner: current runtime lifetime: %w", errors.Join(ErrIdentity, err))
	}
	if err := verifyCurrentDirectory(cwd, spec.commit.Cwd); err != nil {
		return fmt.Errorf("runner: final current cwd: %w", err)
	}
	if err := unix.Fchdir(int(cwd.Fd())); err != nil {
		return err
	}
	closing := cwd
	cwd = nil
	if err := closing.Close(); err != nil {
		return err
	}
	if err := worker.dir.Close(); err != nil {
		return err
	}
	worker.dir = nil
	hasProviderTask := task != nil
	if hasProviderTask {
		installing := task
		task = nil
		if err := installProviderTask(installing); err != nil {
			return fmt.Errorf("runner: install provider task: %w", err)
		}
		defer unix.Close(providerTaskFD)
	} else {
		_ = unix.Close(providerTaskFD)
	}
	// The worker capability remains open only as a CLOEXEC diagnostic path for
	// a failed final exec. A successful provider never inherits it.
	unix.CloseOnExec(3)
	_ = unix.Close(9)
	if _, err := unix.FcntlInt(worker.lifetime.Fd(), unix.F_SETFD, 0); err != nil {
		return fmt.Errorf("runner: retain current runtime lifetime: %w", err)
	}
	// The provider owns the attempt after exec. Losing the outer runner closes
	// its PTY master, but must not let that hangup kill the still-live provider
	// and make recovery mistake process death for clean convergence.
	signal.Ignore(unix.SIGHUP)
	if err := sealProviderDescriptors(hasProviderTask); err != nil {
		return fmt.Errorf("runner: seal provider descriptors: %w", err)
	}
	if err := unix.Exec(spec.commit.Executable.Path, spec.commit.Argv, spec.commit.Env); err != nil {
		return fmt.Errorf("runner: current exec: %w", err)
	}
	return nil
}

func sealProviderDescriptors(keepProviderTask bool) error {
	directory, err := os.Open("/dev/fd")
	if err != nil {
		return err
	}
	scanFD := int(directory.Fd())
	entries, err := directory.Readdirnames(-1)
	if err != nil {
		return errors.Join(err, directory.Close())
	}
	for _, entry := range entries {
		fd, err := strconv.Atoi(entry)
		if err != nil || fd <= 2 || fd == 10 || keepProviderTask && fd == providerTaskFD || fd == scanFD {
			continue
		}
		flags, err := unix.FcntlInt(uintptr(fd), unix.F_GETFD, 0)
		if errors.Is(err, unix.EBADF) {
			continue
		}
		if err != nil {
			return errors.Join(err, directory.Close())
		}
		if _, err := unix.FcntlInt(uintptr(fd), unix.F_SETFD, flags|unix.FD_CLOEXEC); errors.Is(err, unix.EBADF) {
			continue
		} else if err != nil {
			return errors.Join(err, directory.Close())
		}
	}
	return directory.Close()
}

func validateProviderTask(task *os.File) error {
	if task == nil {
		return ErrIdentity
	}
	return validateProviderTaskDescriptor(task.Fd())
}

func validateProviderTaskDescriptor(descriptor uintptr) error {
	fd := int(descriptor)
	var before unix.Stat_t
	if err := unix.Fstat(fd, &before); err != nil || before.Mode&unix.S_IFMT != unix.S_IFREG || before.Uid != uint32(os.Geteuid()) || before.Mode&0o7777 != 0o600 || before.Nlink != 0 || before.Size <= 0 || before.Size > int64(MaxProviderTaskBytes) || before.Dev == 0 || before.Ino == 0 {
		return ErrIdentity
	}
	flags, err := unix.FcntlInt(descriptor, unix.F_GETFL, 0)
	if err != nil || flags&unix.O_ACCMODE != unix.O_RDONLY {
		return ErrIdentity
	}
	offset, err := unix.Seek(fd, 0, io.SeekCurrent)
	if err != nil || offset != 0 {
		return ErrIdentity
	}
	body := make([]byte, int(before.Size))
	n, err := unix.Pread(fd, body, 0)
	if err != nil || n != len(body) || !utf8.Valid(body) || bytes.IndexByte(body, 0) >= 0 {
		return ErrIdentity
	}
	var after unix.Stat_t
	if err := unix.Fstat(fd, &after); err != nil || !sameProviderTaskStat(&before, &after) {
		return ErrIdentity
	}
	return nil
}

func sameProviderTaskStat(left, right *unix.Stat_t) bool {
	return left != nil && right != nil &&
		left.Dev == right.Dev && left.Ino == right.Ino && left.Mode == right.Mode &&
		left.Nlink == right.Nlink && left.Uid == right.Uid && left.Gid == right.Gid &&
		left.Rdev == right.Rdev && left.Size == right.Size
}

func installProviderTask(task *os.File) (resultErr error) {
	defer func() {
		if task != nil {
			resultErr = errors.Join(resultErr, task.Close())
		}
	}()
	if err := validateProviderTask(task); err != nil {
		return err
	}
	duplicate, err := unix.FcntlInt(task.Fd(), unix.F_DUPFD_CLOEXEC, 64)
	if err != nil {
		return err
	}
	if err := task.Close(); err != nil {
		task = nil
		_ = unix.Close(duplicate)
		return err
	}
	task = nil
	if err := unix.Dup2(duplicate, providerTaskFD); err != nil {
		_ = unix.Close(duplicate)
		return err
	}
	if err := unix.Close(duplicate); err != nil {
		_ = unix.Close(providerTaskFD)
		return err
	}
	if err := validateProviderTaskDescriptor(uintptr(providerTaskFD)); err != nil {
		_ = unix.Close(providerTaskFD)
		return err
	}
	flags, err := unix.FcntlInt(uintptr(providerTaskFD), unix.F_GETFD, 0)
	if err != nil || flags&unix.FD_CLOEXEC != 0 {
		_ = unix.Close(providerTaskFD)
		return ErrIdentity
	}
	return nil
}

func verifyCurrentDirectory(cwd *os.File, want fileCommitment) error {
	if cwd == nil {
		return ErrIdentity
	}
	// commitOpen uses want.Path only as the immutable commitment label. Every
	// authority fact comes from cwd; this does not open or stat the pathname.
	got, err := commitOpen(cwd, want.Path, false)
	if err != nil {
		return err
	}
	if got.UID != uint32(os.Geteuid()) || got.Mode&uint32(unix.S_IFMT) != uint32(unix.S_IFDIR) ||
		got.Mode&uint32(unix.S_IWGRP|unix.S_IWOTH|unix.S_ISUID|unix.S_ISGID) != 0 {
		return ErrIdentity
	}
	if got != want {
		return ErrIdentity
	}
	return nil
}

func RunAttemptRunner() error {
	control := os.NewFile(3, "attempt-daemon-control")
	dir := os.NewFile(9, "attempt-private-dir")
	lifetime := os.NewFile(10, "runtime-lifetime")
	if control == nil || dir == nil || lifetime == nil {
		return fmt.Errorf("runner: missing attempt capability")
	}
	defer control.Close()
	defer dir.Close()
	defer lifetime.Close()
	if _, err := commitControl(control); err != nil {
		return fmt.Errorf("runner: daemon control: %w", err)
	}
	if _, err := validatePrivateDirectory(dir); err != nil {
		return fmt.Errorf("runner: private directory: %w", err)
	}
	if _, err := commitRuntimeLifetime(dir, lifetime); err != nil {
		return fmt.Errorf("runner: runtime lifetime: %w", err)
	}
	workerConfig, err := readInheritedWorkerConfig(os.Stdin)
	closeConfigErr := os.Stdin.Close()
	if err != nil || closeConfigErr != nil {
		return fmt.Errorf("runner: worker config: %w", errors.Join(err, closeConfigErr))
	}
	// The attempt runner passes only deliberate duplicates through StartBlocked;
	// neither daemon control nor its retained directory may leak into the inner
	// gate through ordinary fork/exec inheritance.
	unix.CloseOnExec(3)
	unix.CloseOnExec(9)
	if _, err := unix.FcntlInt(lifetime.Fd(), unix.F_SETFD, 0); err != nil {
		return err
	}
	if err := control.SetReadDeadline(time.Now().Add(attemptControlTimeout)); err != nil {
		return err
	}
	var cfg attemptConfig
	if err := readFrame(control, &cfg, maxConfigBytes); err != nil {
		return err
	}
	_ = control.SetReadDeadline(time.Time{})
	if err := validateAttemptConfig(cfg); err != nil {
		return err
	}
	return runAttempt(control, dir, lifetime, cfg, workerConfig)
}

func readInheritedWorkerConfig(file *os.File) ([]byte, error) {
	if file == nil {
		return nil, ErrIdentity
	}
	var before, after unix.Stat_t
	if err := unix.Fstat(int(file.Fd()), &before); err != nil || before.Mode&unix.S_IFMT != unix.S_IFREG || before.Uid != uint32(os.Geteuid()) || before.Mode&0o7777 != 0o600 || before.Nlink != 0 || before.Size <= 0 || before.Size > maxConfigBytes || before.Dev == 0 || before.Ino == 0 {
		return nil, errors.Join(ErrIdentity, err)
	}
	body := make([]byte, int(before.Size))
	if _, err := io.ReadFull(file, body); err != nil {
		return nil, errors.Join(ErrIdentity, err)
	}
	if err := unix.Fstat(int(file.Fd()), &after); err != nil || before.Dev != after.Dev || before.Ino != after.Ino || before.Mode != after.Mode || before.Uid != after.Uid || before.Nlink != after.Nlink || before.Size != after.Size || !json.Valid(body) {
		return nil, errors.Join(ErrIdentity, err)
	}
	return body, nil
}

func validateAttemptConfig(cfg attemptConfig) error {
	if cfg.Version != 1 || validateAttemptName(cfg.AttemptID, 256) != nil || cfg.MarkerName != InnerActivationMarkerName || cfg.ResultName != AttemptResultSpoolName {
		return ErrIdentity
	}
	if len(cfg.StartupInput) > MaxProviderTaskBytes || len(cfg.StartupInput) > 0 && (!utf8.Valid(cfg.StartupInput) || bytes.IndexByte(cfg.StartupInput, 0) >= 0) {
		return ErrIdentity
	}
	if cfg.Wrapper.Executable.Path == "" || cfg.Wrapper.Cwd.Path == "" {
		return ErrIdentity
	}
	if _, err := decodeResultProof(cfg.ResultProof); err != nil {
		return err
	}
	if err := validateArgv(cfg.Wrapper.Argv, cfg.Wrapper.Executable.Path); err != nil {
		return ErrIdentity
	}
	if len(cfg.Wrapper.Env) > 128 {
		return ErrIdentity
	}
	if err := validateEnvironment(cfg.Wrapper.Env); err != nil {
		return ErrIdentity
	}
	return nil
}

func validateAttemptName(value string, limit int) error {
	if value == "" || len(value) > limit {
		return ErrIdentity
	}
	// encoding/json must emit the name byte-for-byte. Control bytes, quotes,
	// backslashes and the HTML-escaped <, >, & all inflate to escape
	// sequences that could push the canonical attempt result past its fixed
	// byte bound, turning a length-valid name into a publish-time failure.
	for index := 0; index < len(value); index++ {
		b := value[index]
		if b < 0x20 || b > 0x7e || b == '"' || b == '\\' || b == '<' || b == '>' || b == '&' {
			return ErrIdentity
		}
	}
	return nil
}

func validateBasename(value string) error {
	if value == "" || len(value) > 255 || filepath.Base(value) != value || value == "." || value == ".." {
		return ErrIdentity
	}
	return nil
}

func runAttempt(daemon, dir, lifetime *os.File, cfg attemptConfig, workerConfig []byte) (result error) {
	proof, err := decodeResultProof(cfg.ResultProof)
	if err != nil {
		return err
	}
	lease, _, err := CreateGateLease(dir, lifetime, cfg.MarkerName)
	if err != nil {
		return err
	}
	defer lease.Close()
	workerParent, workerChild, err := newControlPair("attempt-runner-worker", "attempt-worker-runner")
	if err != nil {
		return err
	}
	defer workerParent.Close()
	defer workerChild.Close()
	controlID, err := commitControl(workerChild)
	if err != nil {
		return err
	}
	// The inner worker and its eventual provider share the runner-owned PTY;
	// no separate stdout/stderr capability is allowed in this topology.
	wrapper := &LaunchSpec{commit: cfg.Wrapper, control: workerChild, controlID: &controlID}
	gate, err := os.Executable()
	if err != nil {
		return err
	}
	prepared, err := PrepareBlockedPTY(lease, gate, wrapper, true)
	if err != nil {
		return err
	}
	child, record, startErr := prepared.LaunchAttempt(dir, cfg.AttemptID, proof)
	if record != nil {
		notifyAttemptResult(daemon, record)
		return startErr
	}
	if startErr != nil {
		return startErr
	}
	defer func() { result = errors.Join(result, child.Close()) }()
	_ = workerChild.Close()
	workerChild = nil
	reads := attemptReadSet{kq: child.kq, daemonFD: int(daemon.Fd()), workerFD: int(workerParent.Fd())}
	if err := reads.registerDaemon(); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	if err := reads.registerWorker(); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	if err := writeControlFrame(daemon, attemptFrame{Version: 1, Kind: "inner-ready", Identity: child.Identity()}, maxFrameBytes); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	frame, source, err := nextAttemptFrame(child, daemon, workerParent, true, true, nil, 0)
	if err != nil || source != sourceDaemon || !validReleaseFrame(frame, StageSelection) {
		return finishAttemptFailure(child, dir, cfg, &reads, protocolError("selection release", source, err))
	}
	if _, err := child.Activate(); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	if err := writeControlFrame(workerParent, json.RawMessage(workerConfig), maxConfigBytes); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	// The activated worker owns the PTY slave as stdin/stdout/stderr. Its
	// output buffer is 1024 bytes, so the master must be read from this point
	// on or a talkative worker blocks in write(2) and never exits.
	retained := &terminalByteRing{}
	reads.ptyFD = int(child.ptyMaster.Fd())
	if err := reads.registerPTY(); err != nil {
		return finishAttemptFailure(child, dir, cfg, &reads, err)
	}
	stagePTY := &ptyStageSink{fd: reads.ptyFD, ring: retained}
	sequence := []struct {
		report  AttemptStage
		release AttemptStage
	}{
		{StageSelection, StagePreparation},
		{StagePreparation, StagePopulation},
		{StagePopulation, StageProvider},
	}
	for _, step := range sequence {
		frame, source, err = nextAttemptFrame(child, daemon, workerParent, true, true, stagePTY, 0)
		if err != nil || source != sourceWorker || !validCheckpointFrame(frame, step.report) {
			return finishAttemptFailure(child, dir, cfg, &reads, protocolError(string(step.report)+" report", source, err))
		}
		if err := writeControlFrame(daemon, frame, maxConfigBytes); err != nil {
			return finishAttemptFailure(child, dir, cfg, &reads, err)
		}
		frame, source, err = nextAttemptFrame(child, daemon, workerParent, true, true, stagePTY, 0)
		if err != nil || source != sourceDaemon || !validReleaseFrame(frame, step.release) {
			return finishAttemptFailure(child, dir, cfg, &reads, protocolError(string(step.release)+" release", source, err))
		}
		if err := writeControlFrame(workerParent, frame, maxFrameBytes); err != nil {
			return finishAttemptFailure(child, dir, cfg, &reads, err)
		}
	}
	daemonOpen, err := runReleasedProvider(child, daemon, workerParent, &reads, stagePTY, retained, cfg.StartupInput)
	return finishAttemptWithExit(child, dir, cfg, &reads, daemon, daemonOpen, err)
}

func newControlPair(parentName, childName string) (*os.File, *os.File, error) {
	fds, err := newControlSocketPair()
	if err != nil {
		return nil, nil, err
	}
	unix.CloseOnExec(fds[0])
	unix.CloseOnExec(fds[1])
	return os.NewFile(uintptr(fds[0]), parentName), os.NewFile(uintptr(fds[1]), childName), nil
}

type attemptSource uint8

const (
	sourceDaemon attemptSource = iota + 1
	sourceWorker
	sourceChild
)

func registerRead(kq, fd int) error {
	change := unix.Kevent_t{Ident: uint64(fd), Filter: unix.EVFILT_READ, Flags: unix.EV_ADD | unix.EV_ENABLE | unix.EV_RECEIPT}
	receipt := make([]unix.Kevent_t, 1)
	n, err := unix.Kevent(kq, []unix.Kevent_t{change}, receipt, nil)
	if err != nil {
		return err
	}
	if n != 1 || receipt[0].Flags&unix.EV_ERROR == 0 || receipt[0].Data != 0 {
		if n == 1 && receipt[0].Data != 0 {
			return unix.Errno(receipt[0].Data)
		}
		return ErrIdentity
	}
	return nil
}

func unregisterRead(kq, fd int) error {
	change := unix.Kevent_t{Ident: uint64(fd), Filter: unix.EVFILT_READ, Flags: unix.EV_DELETE | unix.EV_RECEIPT}
	receipt := make([]unix.Kevent_t, 1)
	n, err := unix.Kevent(kq, []unix.Kevent_t{change}, receipt, nil)
	if err != nil {
		return err
	}
	if n != 1 || receipt[0].Flags&unix.EV_ERROR == 0 || receipt[0].Data != 0 {
		if n == 1 && receipt[0].Data != 0 {
			return unix.Errno(receipt[0].Data)
		}
		return ErrIdentity
	}
	return nil
}

type attemptReadSet struct {
	kq               int
	daemonFD         int
	workerFD         int
	ptyFD            int
	daemonRegistered bool
	workerRegistered bool
	ptyRegistered    bool
	testUnregister   func(int) error // package-test-only; production is nil
}

func (reads *attemptReadSet) registerDaemon() error {
	if reads == nil || reads.daemonRegistered {
		return ErrState
	}
	if err := registerRead(reads.kq, reads.daemonFD); err != nil {
		return err
	}
	reads.daemonRegistered = true
	return nil
}

func (reads *attemptReadSet) registerWorker() error {
	if reads == nil || reads.workerRegistered {
		return ErrState
	}
	if err := registerRead(reads.kq, reads.workerFD); err != nil {
		return err
	}
	reads.workerRegistered = true
	return nil
}

func (reads *attemptReadSet) registerPTY() error {
	if reads == nil || reads.ptyRegistered || reads.ptyFD < 0 {
		return ErrState
	}
	if err := registerRead(reads.kq, reads.ptyFD); err != nil {
		return err
	}
	reads.ptyRegistered = true
	return nil
}

func (reads *attemptReadSet) removeDaemon() error {
	if reads == nil || !reads.daemonRegistered {
		return nil
	}
	if err := reads.unregister(reads.daemonFD); err != nil {
		return err
	}
	reads.daemonRegistered = false
	return nil
}

func (reads *attemptReadSet) removeWorker() error {
	if reads == nil || !reads.workerRegistered {
		return nil
	}
	if err := reads.unregister(reads.workerFD); err != nil {
		return err
	}
	reads.workerRegistered = false
	return nil
}

func (reads *attemptReadSet) removePTY() error {
	if reads == nil || !reads.ptyRegistered {
		return nil
	}
	if err := reads.unregister(reads.ptyFD); err != nil {
		return err
	}
	reads.ptyRegistered = false
	return nil
}

func (reads *attemptReadSet) unregister(fd int) error {
	if reads.testUnregister != nil {
		if err := reads.testUnregister(fd); err != nil {
			return err
		}
	}
	return unregisterRead(reads.kq, fd)
}

func retireReadableFilter(remove func() error) error {
	if remove == nil {
		return ErrState
	}
	deadline := time.Now().Add(500 * time.Millisecond)
	var lastErr error
	for {
		if err := remove(); err == nil {
			return nil
		} else {
			lastErr = err
		}
		remaining := time.Until(deadline)
		if remaining <= 0 {
			return errors.Join(ErrUnresolved, lastErr)
		}
		if remaining > 25*time.Millisecond {
			remaining = 25 * time.Millisecond
		}
		time.Sleep(remaining)
	}
}

func (reads *attemptReadSet) processOnly() error {
	if reads == nil {
		return nil
	}
	return errors.Join(
		retireReadableFilter(reads.removeDaemon),
		retireReadableFilter(reads.removeWorker),
		retireReadableFilter(reads.removePTY),
	)
}

// ptyStageSink drains the runner-owned PTY master while the inner worker is
// running but before the provider terminal loop owns it. The macOS PTY output
// buffer is 1024 bytes: an inner worker that writes more than that to its
// stdout/stderr blocks in write(2) forever unless the master is read, so it
// would never exit and its NOTE_EXIT would never arrive. Retaining the bytes
// here keeps them in the same order for the terminal stream that later
// replays them.
type ptyStageSink struct {
	fd   int
	ring *terminalByteRing
}

func (s *ptyStageSink) drain() error {
	if s == nil || s.fd < 0 || s.ring == nil {
		return ErrState
	}
	buf := make([]byte, 4096)
	for {
		n, err := unix.Read(s.fd, buf)
		if n > 0 {
			if appendErr := s.ring.Append(buf[:n]); appendErr != nil {
				return appendErr
			}
		}
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if errors.Is(err, unix.EAGAIN) {
			return nil
		}
		if err != nil {
			// EIO is the ordinary macOS master read after every slave closed.
			if errors.Is(err, unix.EIO) {
				return nil
			}
			return err
		}
		if n == 0 {
			return nil
		}
	}
}

func nextAttemptFrame(child *OwnedChild, daemon, worker *os.File, daemonOpen, workerOpen bool, pty *ptyStageSink, timeout time.Duration) (attemptFrame, attemptSource, error) {
	for {
		events := make([]unix.Kevent_t, 1)
		var ts *unix.Timespec
		var value unix.Timespec
		if timeout > 0 {
			value = unix.NsecToTimespec(timeout.Nanoseconds())
			ts = &value
		}
		n, err := unix.Kevent(child.kq, nil, events, ts)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return attemptFrame{}, 0, err
		}
		if n == 0 {
			return attemptFrame{}, 0, os.ErrDeadlineExceeded
		}
		ev := events[0]
		if ev.Filter == unix.EVFILT_PROC && ev.Ident == uint64(child.identity.PID) && ev.Fflags&unix.NOTE_EXIT != 0 {
			child.exitObserved = true
			child.state = stateExited
			return attemptFrame{}, sourceChild, nil
		}
		if ev.Filter != unix.EVFILT_READ || ev.Flags&unix.EV_ERROR != 0 {
			return attemptFrame{}, 0, ErrIdentity
		}
		var file *os.File
		var source attemptSource
		switch {
		case daemonOpen && ev.Ident == uint64(daemon.Fd()):
			file, source = daemon, sourceDaemon
		case workerOpen && ev.Ident == uint64(worker.Fd()):
			file, source = worker, sourceWorker
		case pty != nil && pty.fd >= 0 && ev.Ident == uint64(pty.fd):
			if err := pty.drain(); err != nil {
				return attemptFrame{}, 0, err
			}
			continue
		default:
			return attemptFrame{}, 0, ErrIdentity
		}
		if err := file.SetReadDeadline(time.Now().Add(attemptControlTimeout)); err != nil {
			return attemptFrame{}, source, err
		}
		var frame attemptFrame
		err = readFrame(file, &frame, maxConfigBytes)
		_ = file.SetReadDeadline(time.Time{})
		return frame, source, err
	}
}

func protocolError(want string, got attemptSource, err error) error {
	if err == nil {
		err = ErrState
	}
	return fmt.Errorf("runner: expected %s (source %d): %w", want, got, err)
}

func validReleaseFrame(frame attemptFrame, stage AttemptStage) bool {
	return frame.Version == 1 && frame.Kind == "release" && frame.Stage == stage && frame.Identity == (Identity{}) && len(frame.Payload) == 0 && frame.FileIdentity == nil && frame.Digest == "" && noTerminalFields(frame)
}

func validCheckpointFrame(frame attemptFrame, stage AttemptStage) bool {
	return frame.Version == 1 && frame.Kind == "checkpoint" && frame.Stage == stage && frame.Identity == (Identity{}) && len(frame.Payload) <= maxAttemptReportBytes && frame.FileIdentity == nil && frame.Digest == "" && noTerminalFields(frame)
}

func finishAttemptFailure(child *OwnedChild, dir *os.File, cfg attemptConfig, reads *attemptReadSet, cause error) error {
	return finishAttemptWithExit(child, dir, cfg, reads, nil, false, cause)
}

func finishAttemptWithExit(child *OwnedChild, dir *os.File, cfg attemptConfig, reads *attemptReadSet, daemon *os.File, daemonOpen bool, cause error) error {
	var filterErr error
	if reads != nil {
		filterErr = reads.processOnly()
	}
	cause = errors.Join(cause, filterErr)
	exit, cleanupErr := waitForAttemptChild(child)
	if cleanupErr != nil {
		return errors.Join(cause, fmt.Errorf("runner: converge inner: %w", cleanupErr))
	}
	// An AttemptResult is evidence that the complete owner cleanup path was
	// observed. Protocol or kill errors marked unresolved must never be turned
	// into durable convergence evidence merely because Wait eventually returned.
	if errors.Is(cause, ErrUnresolved) {
		return cause
	}
	if err := drainAndCloseAttemptPTY(child, reads, daemon, daemonOpen); err != nil {
		return errors.Join(cause, fmt.Errorf("runner: drain inner PTY: %w", err))
	}
	proof, err := decodeResultProof(cfg.ResultProof)
	if err != nil {
		return errors.Join(cause, err)
	}
	result, err := innerConvergedResult(cfg.AttemptID, proof, child.Identity(), exit)
	if err != nil {
		return errors.Join(cause, err)
	}
	record, err := publishAttemptResult(dir, result)
	if err != nil {
		return errors.Join(cause, fmt.Errorf("runner: publish attempt result: %w", err))
	}
	if daemonOpen {
		notifyAttemptResult(daemon, record)
	}
	return cause
}

// waitForAttemptChild is intentionally synchronous and has no terminal error
// path after a child exists. Cleanup uncertainty retains the sole live owner and
// retries with a bounded delay until exact group convergence and the sole Wait
// succeed. It may therefore remain here indefinitely rather than orphaning the
// child or publishing a false terminal observation.
func waitForAttemptChild(child *OwnedChild) (Exit, error) {
	if child == nil {
		return Exit{}, ErrState
	}
	var cleanupErr error
	for {
		if exit, err := child.waitedExit(); err == nil {
			return exit, cleanupErr
		}
		// Failure paths may arrive after the owner loop has retired its read
		// filters. Observe the exact child exit before attempting a group signal;
		// otherwise a dead gate can make every retry look like an authority
		// failure and prevent deterministic reaping.
		if child.state == stateActivated && !child.exitObserved {
			if err := child.refreshExit(); err != nil {
				if !interruptedCleanup(err) {
					cleanupErr = errors.Join(cleanupErr, err)
				}
			}
		}
		switch child.state {
		case stateBlocked:
			if err := child.Abort(); err != nil {
				cleanupErr = errors.Join(cleanupErr, err)
			}
		case stateActivated:
			// A signal can race a natural exit (Darwin may report EPERM after
			// census but before kill). Retry while the exact owner is live; only
			// uncertainty observed after exit is retained as a publication block.
			_, _ = child.Terminate(defaultStopTimeout)
		case stateExited:
			if child.activated {
				_, err := child.FinishAfterExit(attemptControlTimeout)
				if err != nil && child.state != stateWaited && !interruptedCleanup(err) {
					cleanupErr = errors.Join(cleanupErr, err)
				}
			} else {
				_, err := child.finishInertAfterExit()
				if err != nil && child.state != stateWaited && !interruptedCleanup(err) {
					cleanupErr = errors.Join(cleanupErr, err)
				}
			}
		case stateWaited:
			exit, err := child.waitedExit()
			return exit, errors.Join(cleanupErr, err)
		}
		if exit, err := child.waitedExit(); err == nil {
			return exit, cleanupErr
		}
		time.Sleep(25 * time.Millisecond)
	}
}

func interruptedCleanup(err error) bool {
	return errors.Is(err, unix.EINTR) || strings.Contains(err.Error(), "interrupted system call")
}

func drainAndCloseAttemptPTY(child *OwnedChild, reads *attemptReadSet, daemon *os.File, daemonOpen bool) error {
	if child == nil {
		return ErrState
	}
	if child.ptyMaster == nil {
		child.ptyDrained = true
		return nil
	}
	var drainErr error
	if !child.ptyDrained {
		owner := terminalOwner{
			child: child, daemon: daemon, reads: reads, daemonOpen: daemonOpen,
			ptyOpen: true, ring: &terminalByteRing{},
		}
		drainErr = owner.drainPTY()
	}
	// Once actual EOF/EIO has been observed, a failed best-effort PTY EOF
	// notice cannot revoke the descriptor-local drain proof.
	if drainErr != nil && !child.ptyDrained {
		return drainErr
	}
	if !child.ptyDrained {
		return ErrUnresolved
	}
	return child.closePTY()
}

func notifyAttemptResult(daemon *os.File, record *AttemptResultRecord) {
	if daemon == nil || record == nil {
		return
	}
	notice := record.Notice()
	_ = writeControlFrame(daemon, attemptFrame{
		Version: 1, Kind: string(AttemptResultReady),
		FileIdentity: &notice.Identity, Digest: notice.Digest,
	}, maxFrameBytes)
}

func writeControlFrame(file *os.File, value any, limit int) error {
	if file == nil {
		return ErrIdentity
	}
	if err := file.SetWriteDeadline(time.Now().Add(attemptControlTimeout)); err != nil {
		return err
	}
	defer file.SetWriteDeadline(time.Time{})
	return writeFrame(file, value, limit)
}
