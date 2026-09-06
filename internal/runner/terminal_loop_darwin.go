//go:build darwin

package runner

import (
	"errors"
	"fmt"
	"io"
	"os"
	"time"

	"golang.org/x/sys/unix"
)

// runReleasedProvider is the single owner loop for a released PTY provider.
// It never returns with an unjoined goroutine: the outer attempt runner owns
// the PTY, child group, two capability sockets and every terminal cursor.
func runReleasedProvider(child *OwnedChild, daemon, worker *os.File, reads *attemptReadSet, stagePTY *ptyStageSink, retained *terminalByteRing, startup []byte) (bool, error) {
	if child == nil || daemon == nil || worker == nil || reads == nil || child.ptyMaster == nil || retained == nil {
		return false, ErrState
	}
	// The PTY was registered and drained from inner activation, so pre-provider
	// worker output is already retained in exact order. The stage sink and this
	// loop share that one ring by pointer: any copy here would silently drop
	// every byte the worker writes between adoption and provider exec.
	loop := terminalOwner{child: child, daemon: daemon, worker: worker, reads: reads, daemonOpen: true, workerOpen: true, ring: retained}
	if err := loop.awaitProviderExec(stagePTY); err != nil {
		return loop.daemonOpen, err
	}
	if err := reads.removeWorker(); err != nil {
		return true, err
	}
	loop.workerOpen = false
	if !reads.ptyRegistered {
		if err := reads.registerPTY(); err != nil {
			return true, err
		}
	}
	loop.ptyOpen = true
	// The worker's CLOEXEC capability has closed, proving provider exec, and the
	// PTY is now registered. Claude receives its frozen prompt once. Shell reads
	// its program from fd 11, and Codex reads its task through the attempt API;
	// neither has startup PTY bytes.
	// A prompt ending in CR is submitted by that CR as a keystroke of its own,
	// once the provider has drawn its prompt: an interactive provider reads one
	// chunk that carries text and a newline as a paste, and a paste does not
	// submit. The text goes now; the CR follows from serve.
	if len(startup) > 0 {
		text := startup
		if text[len(text)-1] == '\r' {
			text = text[:len(text)-1]
			now := time.Now()
			loop.enterAfter, loop.enterBy = now.Add(startupEnterFloor), now.Add(startupEnterCeiling)
		}
		if len(text) > 0 {
			n, err := loop.child.writePTYOwned(text, attemptControlTimeout)
			count, status := terminalPayloadResult(n, len(text), err)
			if status != TerminalResultOK {
				clear(startup)
				stopErr := loop.stop()
				return loop.daemonOpen, errors.Join(fmt.Errorf("runner: provider startup input %s after %d bytes", status, count), stopErr)
			}
		}
		clear(startup)
	}
	if err := loop.send(TerminalFrame{Kind: TerminalReady}); err != nil {
		return false, err
	}
	return loop.serve()
}

type terminalOwner struct {
	child      *OwnedChild
	daemon     *os.File
	worker     *os.File
	reads      *attemptReadSet
	daemonOpen bool
	workerOpen bool
	ptyOpen    bool

	daemonDecoder *attemptFrameDecoder
	generation    uint64
	inputActive   bool
	nextInput     uint64
	ptyEOF        bool
	ptyDrained    bool
	stopRequested bool

	ring             *terminalByteRing
	credit           uint64
	sent             uint64
	observerAttached bool
	replay           []terminalReplay

	// enterAfter and enterBy bound the startup CR still owed to the provider;
	// lastOutput is when the provider last wrote, so the CR follows a quiet
	// prompt rather than a banner still being drawn.
	enterAfter, enterBy, lastOutput time.Time
}

const (
	startupEnterFloor   = time.Second
	startupEnterQuiet   = 500 * time.Millisecond
	startupEnterCeiling = 5 * time.Second
	startupEnterTick    = 100 * time.Millisecond
)

// submitStartup writes the owed CR once the provider's output has been quiet
// for a spell after the floor, or at the ceiling regardless.
func (o *terminalOwner) submitStartup() error {
	if o.enterBy.IsZero() {
		return nil
	}
	now := time.Now()
	if now.Before(o.enterAfter) || now.Before(o.enterBy) && (o.lastOutput.IsZero() || now.Sub(o.lastOutput) < startupEnterQuiet) {
		return nil
	}
	o.enterAfter, o.enterBy = time.Time{}, time.Time{}
	n, err := o.child.writePTYOwned([]byte{'\r'}, attemptControlTimeout)
	if _, status := terminalPayloadResult(n, 1, err); status != TerminalResultOK {
		return fmt.Errorf("runner: provider startup submit %s", status)
	}
	return nil
}

type terminalReplay struct {
	correlation uint64
	cursor      uint64
	head        uint64
}

func (o *terminalOwner) awaitProviderExec(stagePTY *ptyStageSink) error {
	for {
		frame, source, err := nextAttemptFrame(o.child, o.daemon, o.worker, o.daemonOpen, o.workerOpen, stagePTY, 0)
		switch source {
		case sourceChild:
			if err == nil {
				return ErrState
			}
			return err
		case sourceDaemon:
			if errors.Is(err, io.EOF) {
				return errors.New("runner: daemon closed before provider exec")
			}
			if err != nil {
				return err
			}
			if frame.Kind == "terminate" && validBareAttemptFrame(frame) {
				return errors.New("runner: daemon terminated before provider exec")
			}
			return ErrState
		case sourceWorker:
			if errors.Is(err, io.EOF) {
				return nil
			}
			if err != nil {
				return err
			}
			if frame.Version == 1 && frame.Kind == "provider-exec-error" && noLegacyFields(frame) && noTerminalFields(frame) && len(frame.Payload) > 0 && len(frame.Payload) <= maxProviderErrorBytes {
				return fmt.Errorf("runner: provider exec: %s", frame.Payload)
			}
			if !validCurrentExecCheck(frame) {
				return ErrState
			}
			if err := o.writeDaemonFrame(frame); err != nil {
				return err
			}
			ack, ackSource, ackErr := nextAttemptFrame(o.child, o.daemon, o.worker, o.daemonOpen, o.workerOpen, stagePTY, 0)
			if ackErr != nil || ackSource != sourceDaemon || !validCurrentExecCheckAck(ack) {
				return protocolError("current exec check ack", ackSource, ackErr)
			}
			if err := o.writeWorkerFrame(ack); err != nil {
				return err
			}
		default:
			return protocolError("provider exec", source, err)
		}
	}
}

func (o *terminalOwner) serve() (bool, error) {
	decoder, err := newAttemptFrameDecoder(maxFrameBytes)
	if err != nil {
		return o.daemonOpen, err
	}
	o.daemonDecoder = decoder
	for {
		ev, err := o.nextEvent()
		if err != nil {
			return o.daemonOpen, err
		}
		if err := o.submitStartup(); err != nil {
			return o.daemonOpen, err
		}
		switch ev.source {
		case sourceTick:
			continue
		case sourceChild:
			// First converge the exact process group and perform the sole Wait;
			// only then is PTY tail output drained. PTY EOF is emitted exclusively
			// from an actual EOF/EIO read, never from child exit.
			if _, err := o.child.FinishAfterExit(8 * time.Second); err != nil {
				return o.daemonOpen, err
			}
			if err := o.drainPTY(); err != nil {
				return o.daemonOpen, err
			}
			return o.daemonOpen, nil
		case sourcePTY:
			o.lastOutput = time.Now()
			if err := o.consumePTY(ev.bytes, ev.err); err != nil {
				return o.daemonOpen, err
			}
		case sourceDaemon:
			if errors.Is(ev.err, io.EOF) {
				if err := o.daemonLost(); err != nil {
					return false, err
				}
				return false, errors.New("runner: daemon control closed")
			}
			if ev.err != nil {
				return o.daemonOpen, ev.err
			}
			frames, err := o.daemonDecoder.Feed(ev.bytes)
			if err != nil {
				return o.daemonOpen, err
			}
			for _, frame := range frames {
				if err := o.command(frame); err != nil {
					return o.daemonOpen, err
				}
				if o.stopRequested {
					return o.daemonOpen, nil
				}
			}
		}
	}
}

type terminalReady struct {
	source attemptSource
	bytes  []byte
	err    error
}

const (
	sourcePTY  attemptSource = sourceChild + 1
	sourceTick attemptSource = sourceChild + 2
)

func (o *terminalOwner) nextEvent() (terminalReady, error) {
	events := make([]unix.Kevent_t, 1)
	for {
		// While a startup CR is owed the wait is bounded, so the quiet prompt
		// is noticed without any event arriving.
		var timeout *unix.Timespec
		if !o.enterBy.IsZero() {
			timeout = &unix.Timespec{Nsec: int64(startupEnterTick)}
		}
		n, err := unix.Kevent(o.child.kq, nil, events, timeout)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return terminalReady{}, err
		}
		if n == 0 && timeout != nil {
			return terminalReady{source: sourceTick}, nil
		}
		if n != 1 {
			return terminalReady{}, ErrIdentity
		}
		ev := events[0]
		if ev.Filter == unix.EVFILT_PROC && ev.Ident == uint64(o.child.identity.PID) && ev.Fflags&unix.NOTE_EXIT != 0 {
			o.child.exitObserved = true
			o.child.state = stateExited
			return terminalReady{source: sourceChild}, nil
		}
		if ev.Filter != unix.EVFILT_READ || ev.Flags&unix.EV_ERROR != 0 {
			return terminalReady{}, ErrIdentity
		}
		var file *os.File
		var source attemptSource
		switch {
		case o.daemonOpen && ev.Ident == uint64(o.daemon.Fd()):
			file, source = o.daemon, sourceDaemon
		case o.ptyOpen && ev.Ident == uint64(o.child.ptyMaster.Fd()):
			file, source = o.child.ptyMaster, sourcePTY
		default:
			return terminalReady{}, ErrIdentity
		}
		buf := make([]byte, 16<<10)
		n, readErr := unix.Read(int(file.Fd()), buf)
		if n > 0 {
			return terminalReady{source: source, bytes: buf[:n], err: nil}, nil
		}
		if source == sourceDaemon && errors.Is(readErr, unix.EAGAIN) {
			continue
		}
		if source == sourcePTY && errors.Is(readErr, unix.EAGAIN) {
			continue
		}
		if source == sourcePTY && errors.Is(readErr, unix.EIO) {
			return terminalReady{source: sourcePTY, err: io.EOF}, nil
		}
		if readErr == nil {
			return terminalReady{source: source, err: io.EOF}, nil
		}
		return terminalReady{source: source, err: readErr}, nil
	}
}

func (o *terminalOwner) command(raw attemptFrame) error {
	if raw.Version == 1 && raw.Kind == "terminate" && validBareAttemptFrame(raw) {
		return o.stop()
	}
	command, err := terminalCommandFromFrame(raw)
	if err != nil {
		return err
	}
	switch command.Kind {
	case TerminalGenerationInstall:
		return o.install(command)
	case TerminalGenerationRevoke:
		return o.revoke(command)
	case TerminalAttach:
		return o.attach(command)
	case TerminalCredit:
		if o.credit+uint64(command.Credit) > maxTerminalCredit {
			return ErrState
		}
		o.credit += uint64(command.Credit)
		return o.flush()
	case TerminalInput:
		return o.input(command)
	case TerminalResize:
		return o.resize(command)
	case TerminalHumanReply:
		return o.humanReply(command)
	default:
		return ErrState
	}
}

// stop is the typed owner transition used by daemon cancellation/finalizing.
// Competing kqueue filters are retired before group convergence so no stale
// PTY or process event can race the exact kill/wait owner.
func (o *terminalOwner) stop() error {
	o.stopRequested = true
	o.inputActive = false
	var cleanupErr error
	if o.reads != nil {
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removeWorker))
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removePTY))
	}
	if o.child == nil || o.child.state != stateActivated {
		return errors.Join(cleanupErr, ErrState)
	}
	return errors.Join(cleanupErr, o.terminateDrainingPTY())
}

// terminateDrainingPTY preserves the PTY tail while Darwin tears down the
// provider's controlling terminal. Termination owns kqueue process events;
// this owner concurrently drains only the PTY master, then joins termination
// before either ownership domain can escape this call.
func (o *terminalOwner) terminateDrainingPTY() error {
	if o == nil || o.child == nil || o.child.state != stateActivated {
		return ErrState
	}
	terminated := make(chan error, 1)
	go func() {
		_, err := o.child.Terminate(defaultStopTimeout)
		terminated <- err
	}()
	drainErr := o.drainPTY()
	return errors.Join(<-terminated, drainErr)
}

func (o *terminalOwner) install(c TerminalCommand) error {
	status := TerminalResultOK
	if c.Generation <= o.generation || o.inputActive {
		if c.Generation == o.generation && o.inputActive {
			return o.send(TerminalFrame{Kind: TerminalGenerationResult, Correlation: c.Correlation, Generation: o.generation, Status: status})
		}
		status = TerminalResultRejected
		return o.send(TerminalFrame{Kind: TerminalGenerationResult, Correlation: c.Correlation, Generation: c.Generation, Status: status})
	}
	o.generation, o.inputActive, o.nextInput = c.Generation, true, 1
	return o.send(TerminalFrame{Kind: TerminalGenerationResult, Correlation: c.Correlation, Generation: c.Generation, Status: status})
}

func (o *terminalOwner) revoke(c TerminalCommand) error {
	status := TerminalResultOK
	if c.Generation < o.generation {
		status = TerminalResultRejected
	} else if c.Generation == o.generation && !o.inputActive {
		status = TerminalResultOK
	} else if c.Generation == o.generation && o.inputActive {
		o.inputActive = false
	} else {
		o.generation, o.inputActive = c.Generation, false
	}
	return o.send(TerminalFrame{Kind: TerminalGenerationResult, Correlation: c.Correlation, Generation: c.Generation, Status: status})
}

func (o *terminalOwner) input(c TerminalCommand) error {
	status := TerminalResultOK
	count := uint32(0)
	if !o.inputActive || c.Generation != o.generation || c.Sequence != o.nextInput {
		status = TerminalResultRejected
	} else {
		count, status = o.writeTerminalPayload(c.Payload)
		if status != TerminalResultOK {
			o.inputActive = false
		} else {
			o.nextInput++
		}
	}
	return o.send(TerminalFrame{Kind: TerminalInputResult, Correlation: c.Correlation, Generation: c.Generation, Sequence: c.Sequence, Count: count, Status: status})
}

// humanReply is a daemon-authorized one-shot write for an exact durable
// HumanRequest. It intentionally bypasses browser generation/sequence checks,
// but shares the sole owner-only PTY write primitive and its fail-closed
// result mapping with terminal input. The payload is written byte-for-byte;
// this path never appends a newline or retries a partial write.
func (o *terminalOwner) humanReply(c TerminalCommand) error {
	count, status := o.writeTerminalPayload(c.Payload)
	return o.send(TerminalFrame{Kind: TerminalHumanReplyResult, Correlation: c.Correlation, Count: count, Status: status})
}

func (o *terminalOwner) writeTerminalPayload(payload []byte) (uint32, TerminalResultStatus) {
	if o == nil || o.stopRequested || o.ptyEOF || !o.ptyOpen || o.child == nil {
		return 0, TerminalResultRejected
	}
	n, err := o.child.writePTYOwned(payload, 250*time.Millisecond)
	count, status := terminalPayloadResult(n, len(payload), err)
	if status == TerminalResultOK {
		return count, status
	}
	// A partial or uncertain write is an irreversible operation boundary. Do
	// not retry or write a suffix; the caller receives the exact count and must
	// decide how to recover. Browser input separately loses its generation
	// authority in input, while daemon-authorized HumanRequest delivery remains
	// a distinct deliberate operation.
	return count, status
}

func terminalPayloadResult(written, total int, err error) (uint32, TerminalResultStatus) {
	if written == total && err == nil {
		return uint32(written), TerminalResultOK
	}
	if written > 0 {
		return uint32(written), TerminalResultPartial
	}
	return 0, TerminalResultUncertain
}

func (o *terminalOwner) resize(c TerminalCommand) error {
	status := TerminalResultOK
	if !o.inputActive || c.Generation != o.generation {
		status = TerminalResultRejected
	} else if err := o.child.resizePTYOwned(int(c.Cols), int(c.Rows)); err != nil {
		status = TerminalResultUncertain
	}
	return o.send(TerminalFrame{Kind: TerminalResizeResult, Correlation: c.Correlation, Generation: c.Generation, Rows: c.Rows, Cols: c.Cols, Status: status})
}

func (o *terminalOwner) attach(c TerminalCommand) error {
	if c.Sequence < o.ring.Floor() {
		return o.send(TerminalFrame{Kind: TerminalReset, Correlation: c.Correlation, Floor: o.ring.Floor(), Head: o.ring.Head()})
	}
	if c.Sequence > o.ring.Head() {
		return o.send(TerminalFrame{Kind: TerminalAttached, Correlation: c.Correlation, Sequence: c.Sequence, Floor: o.ring.Floor(), Head: o.ring.Head(), Status: TerminalResultRejected})
	}
	head := o.ring.Head()
	if len(o.replay) >= terminalReplayRequestCapacity {
		return errors.Join(ErrUnresolved, errors.New("runner: terminal replay request queue is full"))
	}
	if err := o.send(TerminalFrame{Kind: TerminalAttached, Correlation: c.Correlation, Sequence: c.Sequence, Floor: o.ring.Floor(), Head: head, Status: TerminalResultOK}); err != nil {
		return err
	}
	// The first observer owns the live cursor. Its historical bytes are routed
	// by correlation, so advancing the live cursor here prevents replay from
	// being emitted a second time as uncorrelated live output. Later observers
	// never mutate that cursor.
	if !o.observerAttached {
		o.observerAttached = true
		o.sent = head
	}
	o.replay = append(o.replay, terminalReplay{correlation: c.Correlation, cursor: c.Sequence, head: head})
	return o.flush()
}

func (o *terminalOwner) consumePTY(data []byte, readErr error) error {
	if len(data) > 0 {
		if o.ptyEOF {
			return ErrState
		}
		if appendErr := o.ring.Append(data); appendErr != nil {
			return appendErr
		}
		if flushErr := o.flush(); flushErr != nil {
			return flushErr
		}
	}
	if errors.Is(readErr, unix.EIO) || errors.Is(readErr, io.EOF) {
		var retireErr error
		if o.reads != nil && o.ptyOpen {
			retireErr = retireReadableFilter(o.reads.removePTY)
			if retireErr == nil {
				o.ptyOpen = false
			}
		}
		if retireErr != nil {
			return retireErr
		}
		o.ptyDrained = true
		return o.emitPTYEOF()
	}
	if errors.Is(readErr, unix.EAGAIN) || errors.Is(readErr, unix.EWOULDBLOCK) || readErr == nil {
		return nil
	}
	return readErr
}

func (o *terminalOwner) drainPTY() error {
	deadline := time.Now().Add(2 * time.Second)
	var streamErr error
	for o.ptyOpen {
		buf := make([]byte, terminalReplayChunk)
		n, err := unix.Read(int(o.child.ptyMaster.Fd()), buf)
		if n > 0 {
			if err := o.ring.Append(buf[:n]); err != nil {
				return err
			}
			if streamErr == nil {
				streamErr = o.flush()
			}
			continue
		}
		if n == 0 && err == nil || errors.Is(err, unix.EIO) || errors.Is(err, io.EOF) {
			var retireErr error
			if o.reads != nil && o.ptyOpen {
				retireErr = retireReadableFilter(o.reads.removePTY)
				if retireErr == nil {
					o.ptyOpen = false
				}
			}
			if retireErr != nil {
				return errors.Join(streamErr, retireErr)
			}
			o.ptyDrained = true
			return errors.Join(streamErr, o.emitPTYEOF())
		}
		if errors.Is(err, unix.EAGAIN) || errors.Is(err, unix.EWOULDBLOCK) {
			if time.Now().After(deadline) {
				return errors.Join(streamErr, ErrUnresolved)
			}
			time.Sleep(time.Millisecond)
			continue
		}
		return errors.Join(streamErr, err)
	}
	return streamErr
}

func (o *terminalOwner) flush() error {
	if o.credit == 0 || !o.daemonOpen {
		return nil
	}
	for len(o.replay) > 0 && o.credit != 0 {
		replay := &o.replay[0]
		if replay.cursor < o.ring.Floor() {
			if err := o.send(TerminalFrame{Kind: TerminalReset, Correlation: replay.correlation, Floor: o.ring.Floor(), Head: o.ring.Head()}); err != nil {
				return err
			}
			o.replay = o.replay[1:]
			continue
		}
		if replay.cursor > replay.head {
			return ErrIdentity
		}
		if replay.cursor == replay.head {
			o.replay = o.replay[1:]
			continue
		}
		chunk, next, err := o.ring.Read(replay.cursor)
		if err != nil {
			return err
		}
		if next > replay.head {
			chunk = chunk[:replay.head-replay.cursor]
			next = replay.head
		}
		if uint64(len(chunk)) > o.credit {
			chunk = chunk[:o.credit]
			next = replay.cursor + uint64(len(chunk))
		}
		if len(chunk) == 0 {
			return ErrState
		}
		if err := o.send(TerminalFrame{Kind: TerminalOutput, Correlation: replay.correlation, Start: replay.cursor, End: next, Payload: chunk}); err != nil {
			return err
		}
		replay.cursor = next
		o.credit -= uint64(len(chunk))
		if replay.cursor == replay.head {
			o.replay = o.replay[1:]
		}
	}
	// PTY EOF stops only future uncorrelated live output. Correlated replay
	// above remains valid for reconnecting observers after the PTY closes.
	if o.ptyEOF {
		return nil
	}
	if o.credit == 0 {
		return nil
	}
	if o.sent < o.ring.Floor() {
		if err := o.send(TerminalFrame{Kind: TerminalReset, Floor: o.ring.Floor(), Head: o.ring.Head()}); err != nil {
			return err
		}
		o.sent = o.ring.Floor()
	}
	for o.sent < o.ring.Head() && o.credit != 0 {
		chunk, next, err := o.ring.Read(o.sent)
		if err != nil {
			return err
		}
		if uint64(len(chunk)) > o.credit {
			chunk = chunk[:o.credit]
			next = o.sent + uint64(len(chunk))
		}
		if err := o.send(TerminalFrame{Kind: TerminalOutput, Start: o.sent, End: next, Payload: chunk}); err != nil {
			return err
		}
		o.sent = next
		o.credit -= uint64(len(chunk))
	}
	return nil
}

func (o *terminalOwner) send(frame TerminalFrame) error {
	if o == nil || !o.daemonOpen || o.daemon == nil {
		return io.EOF
	}
	if err := writeControlFrame(o.daemon, terminalEventFrame(frame), maxFrameBytes); err != nil {
		return o.poisonDaemon(err)
	}
	return nil
}

func (o *terminalOwner) writeDaemonFrame(frame attemptFrame) error {
	if o == nil || !o.daemonOpen || o.daemon == nil {
		return io.EOF
	}
	if err := writeControlFrame(o.daemon, frame, maxFrameBytes); err != nil {
		return o.poisonDaemon(err)
	}
	return nil
}

func (o *terminalOwner) writeWorkerFrame(frame attemptFrame) error {
	if o == nil || !o.workerOpen || o.worker == nil {
		return io.EOF
	}
	if err := writeControlFrame(o.worker, frame, maxFrameBytes); err != nil {
		o.workerOpen = false
		var closeErr error
		if o.reads != nil {
			closeErr = retireReadableFilter(o.reads.removeWorker)
		}
		closeErr = errors.Join(closeErr, o.worker.Close())
		o.worker = nil
		return errors.Join(err, closeErr)
	}
	return nil
}

func (o *terminalOwner) poisonDaemon(cause error) error {
	if o == nil {
		return cause
	}
	o.daemonOpen = false
	var cleanupErr error
	if o.reads != nil {
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removeDaemon))
	}
	if o.daemon != nil {
		cleanupErr = errors.Join(cleanupErr, o.daemon.Close())
		o.daemon = nil
	}
	return errors.Join(cause, cleanupErr)
}

func (o *terminalOwner) emitPTYEOF() error {
	if o == nil || !o.ptyDrained {
		return ErrState
	}
	if o.child != nil {
		o.child.ptyDrained = true
	}
	if o.ptyEOF {
		return nil
	}
	o.ptyEOF = true
	o.inputActive = false
	if !o.daemonOpen {
		return nil
	}
	return o.send(TerminalFrame{Kind: TerminalPTYEOF})
}

func (o *terminalOwner) daemonLost() error {
	o.daemonOpen = false
	var cleanupErr error
	if o.reads != nil {
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removeDaemon))
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removeWorker))
		cleanupErr = errors.Join(cleanupErr, retireReadableFilter(o.reads.removePTY))
	}
	if o.daemon != nil {
		cleanupErr = errors.Join(cleanupErr, o.daemon.Close())
		o.daemon = nil
	}
	o.inputActive = false
	if o.child != nil && o.child.state == stateActivated {
		return errors.Join(cleanupErr, o.terminateDrainingPTY())
	}
	return cleanupErr
}

func validBareAttemptFrame(frame attemptFrame) bool {
	return frame.Version == 1 && frame.Stage == "" && frame.Identity == (Identity{}) && len(frame.Payload) == 0 && frame.FileIdentity == nil && frame.Digest == "" && noTerminalFields(frame)
}

func validCurrentExecCheck(frame attemptFrame) bool {
	return frame.Version == 1 && frame.Kind == "current-exec-check" && noLegacyFields(frame) && noTerminalFields(frame) && len(frame.Payload) == 0
}
