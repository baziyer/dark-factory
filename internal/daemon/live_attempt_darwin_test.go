//go:build darwin

package daemon

import (
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"io"
	"net"
	"testing"
	"time"

	"github.com/dark-factory-build/dark-factory/internal/browser"
	"github.com/dark-factory-build/dark-factory/internal/kernel"
	"github.com/dark-factory-build/dark-factory/internal/runner"
)

func TestLiveAttemptAttachBeforeReadyIsTypedRetryable(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 11101)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	attachment := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap)}
	revision, err := kernel.NewRevision(1)
	if err != nil {
		t.Fatal(err)
	}
	// The supervisor can commit session activation before this owner consumes
	// the runner's ready frame. An attach in that window is early, not wrong:
	// it must classify as retryable busyness on the wire, never internal.
	if err := attempt.handleAttach(attachment, sessionID, revision, revision, 0); !errors.Is(err, ErrTerminalNotReady) {
		t.Fatalf("attach before ready = %v", err)
	}
	if mapped := mapBrowserError(ErrTerminalNotReady); !errors.Is(mapped, browser.ErrRateLimited) {
		t.Fatalf("not-ready wire mapping = %v", mapped)
	}
	// After the result is seen the attach window is over: the durable world
	// moved past the pinned target, so the client re-resolves via the typed
	// stale arm instead of retrying in place.
	for _, readySeen := range []bool{false, true} {
		attempt.readySeen = readySeen
		attempt.resultReturned = true
		if err := attempt.handleAttach(attachment, sessionID, revision, revision, 0); !errors.Is(err, kernel.ErrConflict) {
			t.Fatalf("attach after result ready=%t = %v", readySeen, err)
		}
		if mapped := mapBrowserError(kernel.ErrConflict); !errors.Is(mapped, browser.ErrStale) {
			t.Fatalf("post-result ready=%t wire mapping = %v", readySeen, mapped)
		}
	}
	if len(attempt.subs) != 0 || len(attempt.correlations) != 0 {
		t.Fatal("refused attaches changed subscribers")
	}
}

func TestLiveAttemptGlobalResetAdvancesEveryObserverCursor(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 10006)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	first := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap), correlation: 1, expected: 3, replayHead: 10, replaying: true, pending: []TerminalEvent{{Kind: TerminalEventOutput}}}
	second := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap), correlation: 2, expected: 4, replayHead: 10, replaying: true}
	attempt.subs[first] = struct{}{}
	attempt.subs[second] = struct{}{}
	attempt.correlations[1] = first
	attempt.correlations[2] = second
	attempt.lastCorrelation = 2
	if err := attempt.routeFrame(runner.TerminalFrame{Kind: runner.TerminalReset, Floor: 8, Head: 11}); err != nil {
		t.Fatal(err)
	}
	for name, subscriber := range map[string]*TerminalAttachment{"first": first, "second": second} {
		if subscriber.replaying || subscriber.expected != 11 || subscriber.replayHead != 11 || len(subscriber.pending) != 0 || subscriber.correlation != 0 {
			t.Fatalf("%s reset state = %+v", name, subscriber)
		}
		if _, present := attempt.subs[subscriber]; !present {
			t.Fatalf("%s observer was detached by global reset", name)
		}
		event, ok := <-subscriber.queue
		if !ok || event.Kind != TerminalEventReset || event.Floor != 8 || event.Head != 11 {
			t.Fatalf("%s reset event = %+v, present=%v", name, event, ok)
		}
	}
}

func TestLiveAttemptDeliversResultNoticeAndBroadcastsCommittedExit(t *testing.T) {
	for _, test := range []struct {
		name              string
		event             TerminalEvent
		terminated        bool
		wantCode, wantSig int
		wantAborted       bool
	}{
		{name: "success", event: TerminalEvent{Kind: TerminalEventExit}},
		{name: "failure", event: TerminalEvent{Kind: TerminalEventExit, ExitCode: 7}, wantCode: 7},
		{name: "signal-after-termination", event: TerminalEvent{Kind: TerminalEventExit, ExitSignal: 15}, terminated: true, wantSig: 15, wantAborted: true},
	} {
		t.Run(test.name, func(t *testing.T) {
			runID, sessionID := liveTestIDs(t, 11000)
			attempt := newLiveAttempt(nil, runID, sessionID, nil)
			attachment := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap)}
			attempt.subs[attachment] = struct{}{}
			attempt.terminationDelivered = test.terminated
			notice := &runner.AttemptResultNotice{Identity: runner.FileIdentity{Device: 3, Inode: 9}, Digest: "aa"}
			stop, err := attempt.handleRunnerEvent(runner.AttemptEvent{Kind: runner.AttemptResultReady, Result: notice})
			if err != nil || stop {
				t.Fatalf("result event = stop %v err %v", stop, err)
			}
			if !attempt.resultReturned || attempt.resultNotice != notice {
				t.Fatal("result notice was not retained")
			}
			// The shape-only notice must not reach observers as an exit; only the
			// store-committed exit sent by the supervisor is broadcast.
			if len(attachment.queue) != 0 {
				t.Fatalf("notice broadcast to observers: %d queued", len(attachment.queue))
			}
			if result := <-attempt.result; result.err != nil || result.notice != notice || !result.observersRetained {
				t.Fatalf("result delivery = %+v", result)
			}
			command := liveAttemptCommand{kind: liveCommandFinishExit, exit: &test.event, result: make(chan error, 1)}
			stop, err = attempt.handleRunningCommand(command)
			if err != nil || !stop {
				t.Fatalf("finish exit = stop %v err %v", stop, err)
			}
			if commandErr := <-command.result; commandErr != nil {
				t.Fatal(commandErr)
			}
			event, err := attachment.Next(context.Background())
			if err != nil {
				t.Fatal(err)
			}
			if event.Kind != TerminalEventExit || event.ExitCode != test.wantCode || event.ExitSignal != test.wantSig || event.Aborted != test.wantAborted {
				t.Fatalf("browser exit = %+v", event)
			}
		})
	}
}

func TestLiveAttemptLostNoticeRetainsObserversForCommittedExit(t *testing.T) {
	for _, test := range []struct {
		name       string
		finishExit bool
	}{
		{name: "authenticated artifact broadcasts exit", finishExit: true},
		{name: "rejected artifact shuts owner down", finishExit: false},
	} {
		t.Run(test.name, func(t *testing.T) {
			controller, peer := readyTerminalEffectController(t)
			runID, sessionID := liveTestIDs(t, 11002)
			attempt := newLiveAttempt(nil, runID, sessionID, controller)
			attempt.releaseSent = true
			attempt.readySeen = true
			attachment := &TerminalAttachment{owner: attempt, queue: make(chan TerminalEvent, terminalSubscriberCap)}
			attempt.subs[attachment] = struct{}{}
			startLiveAttempt(attempt, context.Background())
			if err := peer.Close(); err != nil {
				t.Fatal(err)
			}
			result := attempt.waitResult()
			if result.err == nil || result.notice != nil || !result.observersRetained || !attempt.resultReturned {
				t.Fatalf("lost-notice result = %+v returned=%t", result, attempt.resultReturned)
			}
			select {
			case <-attempt.done:
				t.Fatal("lost notice abandoned terminal observers before artifact authentication")
			default:
			}
			if !test.finishExit {
				if err := attempt.close(); err == nil {
					t.Fatal("lost-notice owner discarded its controller failure")
				}
				if _, err := attachment.Next(context.Background()); err == nil {
					t.Fatalf("shutdown observer = %v", err)
				}
				return
			}
			exit := TerminalEvent{Kind: TerminalEventExit, ExitCode: 7}
			if err := attempt.finishExit(context.Background(), exit); err != nil {
				t.Fatal(err)
			}
			event, err := attachment.Next(context.Background())
			if err != nil || event.Kind != TerminalEventExit || event.ExitCode != 7 {
				t.Fatalf("committed exit = %+v, %v", event, err)
			}
			if err := attempt.join(); err == nil {
				t.Fatal("committed fallback exit discarded its controller diagnosis")
			}
		})
	}
}

func TestLiveAttemptShutdownReturnsFallbackToSupervisor(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 11003)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	startLiveAttempt(attempt, context.Background())
	resultDone := make(chan liveAttemptResult, 1)
	closeDone := make(chan error, 1)
	go func() { resultDone <- attempt.waitResult() }()
	go func() { closeDone <- attempt.close() }()
	select {
	case result := <-resultDone:
		if result.err == nil || result.notice != nil || result.observersRetained {
			t.Fatalf("shutdown fallback = %+v", result)
		}
	case <-time.After(time.Second):
		t.Fatal("supervisor waiter remained blocked after owner shutdown")
	}
	select {
	case err := <-closeDone:
		if err == nil {
			t.Fatal("shutdown discarded its terminal-closed result")
		}
	case <-time.After(time.Second):
		t.Fatal("owner shutdown did not join")
	}
}

func TestLiveAttemptRejectsExitBroadcastWithoutCommittedResult(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 11001)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	attachment := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap)}
	attempt.subs[attachment] = struct{}{}
	if stop, err := attempt.handleRunnerEvent(runner.AttemptEvent{Kind: runner.AttemptResultReady}); stop || !errors.Is(err, runner.ErrState) {
		t.Fatalf("nil notice = stop %v err %v", stop, err)
	}
	if attempt.resultReturned || attempt.resultNotice != nil || len(attempt.result) != 0 {
		t.Fatal("nil notice changed state")
	}
	exit := TerminalEvent{Kind: TerminalEventExit}
	command := liveAttemptCommand{kind: liveCommandFinishExit, exit: &exit, result: make(chan error, 1)}
	if stop, err := attempt.handleRunningCommand(command); stop || err != nil {
		t.Fatalf("finish exit before result = stop %v err %v", stop, err)
	}
	if commandErr := <-command.result; !errors.Is(commandErr, runner.ErrIdentity) {
		t.Fatalf("finish exit before result = %v", commandErr)
	}
	attempt.resultReturned = true
	wrongKind := TerminalEvent{Kind: TerminalEventOutput}
	command = liveAttemptCommand{kind: liveCommandFinishExit, exit: &wrongKind, result: make(chan error, 1)}
	if stop, err := attempt.handleRunningCommand(command); stop || err != nil {
		t.Fatalf("finish exit with wrong kind = stop %v err %v", stop, err)
	}
	if commandErr := <-command.result; !errors.Is(commandErr, runner.ErrIdentity) {
		t.Fatalf("finish exit with wrong kind = %v", commandErr)
	}
	command = liveAttemptCommand{kind: liveCommandFinishExit, result: make(chan error, 1)}
	if stop, err := attempt.handleRunningCommand(command); stop || err != nil {
		t.Fatalf("finish exit with nil event = stop %v err %v", stop, err)
	}
	if commandErr := <-command.result; !errors.Is(commandErr, runner.ErrIdentity) {
		t.Fatalf("finish exit with nil event = %v", commandErr)
	}
	if len(attachment.queue) != 0 {
		t.Fatal("rejected commands reached observers")
	}
}

func TestLiveAttemptRejectsWrongSessionBeforeRunnerAttach(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 10009)
	_, wrongSession := liveTestIDs(t, 10010)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	attempt.readySeen = true
	attachment := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap)}
	revision, err := kernel.NewRevision(1)
	if err != nil {
		t.Fatal(err)
	}
	if err := attempt.handleAttach(attachment, wrongSession, revision, revision, 0); !errors.Is(err, kernel.ErrConflict) {
		t.Fatalf("wrong session attach = %v", err)
	}
	if len(attempt.subs) != 0 || len(attempt.correlations) != 0 {
		t.Fatalf("wrong session changed subscribers: subs=%d correlations=%d", len(attempt.subs), len(attempt.correlations))
	}
}

func TestLiveAttemptCancellationPreemptsOutcomeReceipt(t *testing.T) {
	controller, peer := readyTerminalEffectController(t)
	t.Cleanup(func() {
		_ = controller.Close()
		_ = peer.Close()
	})
	attempt := newLiveAttempt(&Daemon{}, kernel.RunID{}, kernel.TerminalSessionID{}, controller)
	attempt.outcomeReceiptPending = true
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	if stop, err := attempt.processLifecycle(ctx); err != nil || stop || !attempt.terminationSent || !attempt.terminationDelivered {
		t.Fatalf("cancelled receipt = stop=%v err=%v sent=%v delivered=%v", stop, err, attempt.terminationSent, attempt.terminationDelivered)
	}
	if frame := readTerminalEffectWire(t, peer); frame.Kind != "terminate" {
		t.Fatalf("cancelled receipt controller frame = %+v", frame)
	}
}

func TestHandleConnectionClearsOnlyTheFailedOutcomeReceipt(t *testing.T) {
	fixture := newDispatchFixture(t)
	active := prepareActiveAttempt(t, fixture, 114)
	session, found, err := fixture.store.TerminalSessionForRun(context.Background(), active.run.ID)
	if err != nil || !found {
		t.Fatalf("terminal session = %+v, found=%v, err=%v", session, found, err)
	}
	controller, peer := readyTerminalEffectController(t)
	t.Cleanup(func() {
		_ = controller.Close()
		_ = peer.Close()
	})
	attempt := newLiveAttempt(fixture.daemon, active.run.ID, session.ID, controller)
	attempt.releaseSent = true
	attempt.readySeen = true
	if err := fixture.daemon.registerLiveAttempt(attempt); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { fixture.daemon.unregisterLiveAttempt(active.run.ID, attempt) })

	otherRun, otherSession := liveTestIDs(t, 11400)
	other := newLiveAttempt(fixture.daemon, otherRun, otherSession, nil)
	if err := fixture.daemon.registerLiveAttempt(other); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { fixture.daemon.unregisterLiveAttempt(otherRun, other) })
	fixture.daemon.operationMu.Lock()
	other.outcomeReceiptPending = true
	fixture.daemon.operationMu.Unlock()

	done := fixture.serve(t)
	connection, err := net.DialUnix("unix", nil, &net.UnixAddr{Name: fixture.socket, Net: "unix"})
	if err != nil {
		t.Fatal(err)
	}
	defer connection.Close()
	if err := connection.SetDeadline(time.Now().Add(2 * time.Second)); err != nil {
		t.Fatal(err)
	}
	writeFrame := func(payload []byte) {
		t.Helper()
		var header [4]byte
		binary.BigEndian.PutUint32(header[:], uint32(len(payload)))
		frame := append(header[:], payload...)
		for len(frame) != 0 {
			written, writeErr := connection.Write(frame)
			if writeErr != nil || written < 1 || written > len(frame) {
				t.Fatalf("write frame = %d, %v", written, writeErr)
			}
			frame = frame[written:]
		}
	}
	readFrame := func() []byte {
		t.Helper()
		var header [4]byte
		if _, err := io.ReadFull(connection, header[:]); err != nil {
			t.Fatal(err)
		}
		payload := make([]byte, int(binary.BigEndian.Uint32(header[:])))
		if _, err := io.ReadFull(connection, payload); err != nil {
			t.Fatal(err)
		}
		return payload
	}
	body := []byte(`{"method":"succeed","params":{"result":"receipt-failure"}}`)
	// The local API frame prelude is one auth-domain byte: 2 is the attempt
	// domain. There is no protocol generation byte in front of it.
	request := make([]byte, 1+len(active.bearer)+len(body))
	request[0] = 2
	copy(request[1:], active.bearer)
	copy(request[1+len(active.bearer):], body)
	writeFrame(request)
	response := readFrame()
	receipt := readFrame()
	if len(response) < 1 || !bytes.Contains(response[1:], []byte(`"ok":true`)) || len(receipt) != 32 {
		t.Fatalf("outcome response = %q, receipt bytes = %d", response, len(receipt))
	}
	var extra [1]byte
	if count, readErr := connection.Read(extra[:]); count != 0 || !errors.Is(readErr, io.EOF) {
		t.Fatalf("outcome response did not end: count=%d error=%v", count, readErr)
	}
	fixture.daemon.operationMu.Lock()
	pending, otherPending := attempt.outcomeReceiptPending, other.outcomeReceiptPending
	fixture.daemon.operationMu.Unlock()
	if !pending || !otherPending {
		t.Fatalf("pending receipts before acknowledgement = attempt:%t other:%t", pending, otherPending)
	}
	if stop, err := attempt.processLifecycle(context.Background()); err != nil || stop || attempt.terminationSent {
		t.Fatalf("lifecycle crossed handler receipt: stop=%v err=%v terminated=%v", stop, err, attempt.terminationSent)
	}

	receipt[0] ^= 0xff
	writeFrame(receipt)
	if err := connection.CloseWrite(); err != nil {
		t.Fatal(err)
	}
	select {
	case handlerErr := <-done:
		if handlerErr == nil {
			t.Fatal("malformed receipt was accepted")
		}
	case <-time.After(2 * time.Second):
		t.Fatal("daemon API handler did not reject malformed receipt")
	}
	fixture.daemon.operationMu.Lock()
	pending, otherPending = attempt.outcomeReceiptPending, other.outcomeReceiptPending
	fixture.daemon.operationMu.Unlock()
	if pending || !otherPending {
		t.Fatalf("pending receipts after failed acknowledgement = attempt:%t other:%t", pending, otherPending)
	}
	if stop, err := attempt.processLifecycle(context.Background()); err != nil || stop || !attempt.terminationSent || !attempt.terminationDelivered {
		t.Fatalf("post-failure lifecycle = stop=%v err=%v sent=%v delivered=%v", stop, err, attempt.terminationSent, attempt.terminationDelivered)
	}
	if frame := readTerminalEffectWire(t, peer); frame.Kind != "terminate" {
		t.Fatalf("post-failure controller frame = %+v", frame)
	}
}

func TestLiveAttemptOwnerClosesControllerBeforeFallbackResult(t *testing.T) {
	daemon := &Daemon{attempts: make(map[kernel.RunID]*liveAttempt)}
	runID, sessionID := liveTestIDs(t, 10007)
	controller, peer, err := runner.NewAttemptController()
	if err != nil {
		t.Fatal(err)
	}
	if err := peer.Close(); err != nil {
		t.Fatal(err)
	}
	attempt := newLiveAttempt(daemon, runID, sessionID, controller)
	attempt.releaseSent = true
	if err := daemon.registerLiveAttempt(attempt); err != nil {
		t.Fatal(err)
	}
	startLiveAttempt(attempt, context.Background())
	if result := attempt.waitResult(); result.err == nil || result.notice != nil {
		t.Fatal("owner read error was hidden")
	}
	if !attempt.controller.Closed() {
		t.Fatal("owner published fallback before controller closure")
	}
	select {
	case <-attempt.done:
		t.Fatal("owner abandoned fallback before artifact authentication")
	default:
	}
	if err := attempt.close(); err == nil {
		t.Fatal("fallback cleanup discarded the controller error")
	}
}

func TestLiveAttemptSlowSubscriberIsDroppedExactlyOnce(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 10004)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	attachment := &TerminalAttachment{queue: make(chan TerminalEvent, terminalSubscriberCap), correlation: 1, expected: 0}
	for range terminalSubscriberCap {
		// The cap is sized from maximum payloads, but native spinners can emit
		// far more tiny frames while still far below the one-megabyte credit.
		attachment.queue <- TerminalEvent{Kind: TerminalEventOutput, Payload: []byte{'x'}}
	}
	if queuedBytes := len(attachment.queue); uint64(queuedBytes) >= liveAttemptCredit {
		t.Fatalf("tiny queued output reached credit: %d >= %d", queuedBytes, liveAttemptCredit)
	}
	attempt.subs[attachment] = struct{}{}
	attempt.routeLive(attachment, runner.TerminalFrame{Kind: runner.TerminalOutput, Start: 0, End: 1, Payload: []byte{'x'}})
	if _, present := attempt.subs[attachment]; present || !attachment.finished {
		t.Fatalf("slow subscriber remained registered: present=%v finished=%v", present, attachment.finished)
	}
	if !attachment.ResetRequired() {
		t.Fatal("slow subscriber did not require a terminal reset")
	}
	// A stale delivery path must be harmless after the queue has been closed;
	// the owner removes the subscriber before any later correlated frame can
	// route to it.
	attempt.routeLive(attachment, runner.TerminalFrame{Kind: runner.TerminalOutput, Start: 1, End: 2, Payload: []byte{'y'}})
	// A late frame for an already allocated correlation is harmless. A frame
	// from a correlation the owner never allocated is a protocol violation.
	attempt.lastCorrelation = 3
	attempt.creditOutstanding = liveAttemptCredit
	if err := attempt.routeFrame(runner.TerminalFrame{Kind: runner.TerminalOutput, Correlation: 2, Payload: []byte{'y'}}); err != nil {
		t.Fatalf("late retired correlation error = %v", err)
	}
	if err := attempt.routeFrame(runner.TerminalFrame{Kind: runner.TerminalOutput, Correlation: 4, Payload: []byte{'z'}}); err == nil {
		t.Fatal("unallocated correlation was accepted")
	}
}

func TestLiveAttemptCreditedReplayDoesNotOverflowBrowserAttachment(t *testing.T) {
	fixture := newTerminalEffectFixture(t)
	attachment, err := fixture.adapter.daemon.AttachTerminal(context.Background(), fixture.run.ID, fixture.session.ID, fixture.run.Revision, fixture.session.Revision, 0)
	if err != nil {
		t.Fatal(err)
	}
	attach := readTerminalEffectWire(t, fixture.peer)
	if attach.Kind != string(runner.TerminalAttach) || attach.Correlation == 0 || attach.Sequence != 0 {
		t.Fatalf("attach command = %+v", attach)
	}
	if credit := readTerminalEffectWire(t, fixture.peer); credit.Kind != string(runner.TerminalCredit) || credit.Credit != liveAttemptCredit {
		t.Fatalf("initial replay credit = %+v", credit)
	}

	// Do not read attachment.Events: this is a paused browser while the runner
	// replays one full credited burst through the ordinary controller route.
	writeTerminalEffectWire(t, fixture.peer, terminalEffectWireFrame{Version: 1, Kind: string(runner.TerminalAttached), Correlation: attach.Correlation, Head: liveAttemptCredit, Status: string(runner.TerminalResultOK)})
	for start := uint64(0); start < liveAttemptCredit; start += terminalPayloadCap {
		writeTerminalEffectWire(t, fixture.peer, terminalEffectWireFrame{
			Version: 1, Kind: string(runner.TerminalOutput), Correlation: attach.Correlation,
			Start: start, End: start + terminalPayloadCap, Payload: make([]byte, terminalPayloadCap),
		})
	}

	want := liveAttemptCredit/terminalPayloadCap + 1 // 128 output frames plus ATTACHED.
	deadline := time.Now().Add(3 * time.Second)
	for len(attachment.queue) != want && time.Now().Before(deadline) {
		time.Sleep(time.Millisecond)
	}
	if _, present := fixture.attempt.subs[attachment]; !present || attachment.finished {
		t.Fatalf("credited replay dropped paused browser attachment: present=%v finished=%v", present, attachment.finished)
	}
	if got := len(attachment.queue); got != want {
		t.Fatalf("queued credited replay events = %d, want %d", got, want)
	}
}

func TestLiveAttemptReplayPendingBytesAreBounded(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 10008)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	attachment := &TerminalAttachment{
		queue:      make(chan TerminalEvent, terminalSubscriberCap),
		replaying:  true,
		replayHead: 0,
		expected:   0,
	}
	attempt.subs[attachment] = struct{}{}
	for i := 0; i < terminalPendingBytesCap/terminalPayloadCap; i++ {
		attempt.routeLive(attachment, runner.TerminalFrame{
			Kind: runner.TerminalOutput, Start: 0, End: 1,
			Payload: make([]byte, terminalPayloadCap),
		})
	}
	if _, present := attempt.subs[attachment]; !present {
		t.Fatal("subscriber was dropped before reaching pending byte bound")
	}
	if got := attachment.pendingBytes; got != terminalPendingBytesCap {
		t.Fatalf("pending bytes = %d, want %d", got, terminalPendingBytesCap)
	}
	attempt.routeLive(attachment, runner.TerminalFrame{
		Kind: runner.TerminalOutput, Start: 0, End: 1,
		Payload: make([]byte, terminalPayloadCap),
	})
	if _, present := attempt.subs[attachment]; present || !attachment.finished {
		t.Fatal("subscriber survived pending byte overflow")
	}
}

func TestLiveAttemptEffectTypingSeparatesNotReadyFromPublishedResult(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 11102)
	controller, peer := readyTerminalEffectController(t)
	attempt := newLiveAttempt(nil, runID, sessionID, controller)
	t.Cleanup(func() {
		_ = controller.Close()
		_ = peer.Close()
	})
	effect := terminalEffect{kind: terminalEffectCheck}

	// Before the runner's ready frame the window opens by itself, so the wire
	// must type the refusal retryable — the same condition attach reports.
	result, err := attempt.handleTerminalEffect(effect)
	if err != nil || !errors.Is(result.err, ErrTerminalNotReady) {
		t.Fatalf("pre-ready effect = %+v err=%v", result, err)
	}
	if mapped := mapBrowserError(result.err); !errors.Is(mapped, browser.ErrRateLimited) {
		t.Fatalf("pre-ready wire mapping = %v", mapped)
	}

	// Once the result is returned this effect can never succeed. Typing it
	// retryable would invite a client to spin on a permanent condition, so it
	// must reach the wire as stale and keep its terminal fence regardless of
	// whether the runner's ready frame was consumed first.
	for _, readySeen := range []bool{false, true} {
		attempt.readySeen = readySeen
		attempt.resultReturned = true
		result, err = attempt.handleTerminalEffect(effect)
		if err != nil || !errors.Is(result.err, kernel.ErrConflict) {
			t.Fatalf("post-result ready=%t effect = %+v err=%v", readySeen, result, err)
		}
		if !result.terminalFence || result.status != runner.TerminalResultRejected {
			t.Fatalf("post-result ready=%t effect lost its fence or status: %+v", readySeen, result)
		}
		mapped := mapBrowserError(result.err)
		if !errors.Is(mapped, browser.ErrStale) || errors.Is(mapped, browser.ErrRateLimited) {
			t.Fatalf("post-result ready=%t wire mapping = %v", readySeen, mapped)
		}
	}
}

func TestLiveAttemptPreReleaseEffectCompletesThroughOwnerMailbox(t *testing.T) {
	runID, sessionID := liveTestIDs(t, 11103)
	attempt := newLiveAttempt(nil, runID, sessionID, nil)
	ownerContext, cancelOwner := context.WithCancel(context.Background())
	ownerClosed := false
	t.Cleanup(func() {
		cancelOwner()
		if ownerClosed {
			return
		}
		select {
		case <-attempt.done:
		case <-time.After(100 * time.Millisecond):
		}
	})
	startLiveAttempt(attempt, ownerContext)

	done := make(chan terminalEffectResult, 1)
	go func() {
		done <- attempt.submitEffect(context.Background(), terminalEffect{kind: terminalEffectCheck})
	}()
	select {
	case result := <-done:
		if result.status != runner.TerminalResultRejected || !errors.Is(result.err, ErrTerminalNotReady) {
			t.Fatalf("pre-release owner effect = %+v", result)
		}
		_ = attempt.close()
		ownerClosed = true
	case <-time.After(time.Second):
		t.Fatal("pre-release owner effect remained blocked in the mailbox")
	}
}
