package browser

import (
	"bytes"
	"context"
	"errors"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/coder/websocket"
	"github.com/dark-factory-build/dark-factory/internal/browserprotocol"
)

// terminalTestBackend keeps the transport tests causal: events are delivered
// through the same receive-only attachment queue that production uses, and
// effect requests are captured at the backend boundary.
type terminalTestBackend struct {
	*fakeBackend

	terminalMu                                  sync.Mutex
	attachment                                  *terminalTestAttachment
	attachmentHit                               chan struct{}
	invalidAttach                               bool
	attachmentCloseErr, attachmentCloseFirstErr error
	input                                       TerminalInputRequest
	inputCalls                                  int
	replyResult                                 browserprotocol.HumanRequestReplyResult
	cancelResult                                browserprotocol.HumanRequestCancelRunResult
	leaseResult                                 TerminalLeaseResult
	controlEntered, controlRelease              chan struct{}
}

var _ AgentControlBackend = (*terminalTestBackend)(nil)

type terminalTestAttachment struct {
	events        chan TerminalEvent
	closeDone     chan struct{}
	closed        atomic.Int32
	closeCalls    atomic.Int32
	closeOnce     sync.Once
	closeErr      error
	closeFirstErr error
	resetRequired bool
}

func newTerminalTestBackend() *terminalTestBackend {
	return &terminalTestBackend{fakeBackend: newFakeBackend(), attachmentHit: make(chan struct{})}
}

func startTerminalServer(t *testing.T, backend *terminalTestBackend) *Server {
	return startTerminalServerWithAckTimeout(t, backend, time.Duration(browserprotocol.TerminalAckTimeoutMS)*time.Millisecond)
}

func startTerminalServerWithAckTimeout(t *testing.T, backend *terminalTestBackend, ackTimeout time.Duration) *Server {
	t.Helper()
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin, devOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() {
		if err := server.Close(); err != nil {
			t.Errorf("close: %v", err)
		}
	})
	server.terminalAckTimeout = ackTimeout
	return server
}

func (attachment *terminalTestAttachment) Events() <-chan TerminalEvent { return attachment.events }
func (attachment *terminalTestAttachment) ResetRequired() bool          { return attachment.resetRequired }
func (attachment *terminalTestAttachment) Close() error {
	call := attachment.closeCalls.Add(1)
	if call == 1 && attachment.closeFirstErr != nil {
		return attachment.closeFirstErr
	}
	if attachment.closeErr != nil {
		return attachment.closeErr
	}
	attachment.closeOnce.Do(func() {
		attachment.closed.Add(1)
		close(attachment.closeDone)
	})
	return nil
}

func (backend *terminalTestBackend) AttachTerminal(context.Context, TerminalAttachRequest) (TerminalAttachment, error) {
	var events chan TerminalEvent
	if !backend.invalidAttach {
		events = make(chan TerminalEvent, 128)
	}
	attachment := &terminalTestAttachment{events: events, closeDone: make(chan struct{}), closeErr: backend.attachmentCloseErr, closeFirstErr: backend.attachmentCloseFirstErr}
	backend.terminalMu.Lock()
	backend.attachment = attachment
	backend.terminalMu.Unlock()
	select {
	case <-backend.attachmentHit:
	default:
		close(backend.attachmentHit)
	}
	return attachment, nil
}

func (backend *terminalTestBackend) currentAttachment(t *testing.T) *terminalTestAttachment {
	t.Helper()
	select {
	case <-backend.attachmentHit:
	case <-time.After(time.Second):
		t.Fatal("terminal attachment was not created")
	}
	backend.terminalMu.Lock()
	defer backend.terminalMu.Unlock()
	return backend.attachment
}

func (backend *terminalTestBackend) AcquireTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseAcquire) (TerminalLeaseResult, error) {
	backend.terminalMu.Lock()
	defer backend.terminalMu.Unlock()
	return backend.leaseResult, nil
}
func (backend *terminalTestBackend) RenewTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseRenew) (TerminalLeaseResult, error) {
	backend.terminalMu.Lock()
	defer backend.terminalMu.Unlock()
	return backend.leaseResult, nil
}
func (backend *terminalTestBackend) ReleaseTerminalLease(context.Context, Principal, browserprotocol.TerminalLeaseRelease) (TerminalLeaseResult, error) {
	backend.terminalMu.Lock()
	defer backend.terminalMu.Unlock()
	return backend.leaseResult, nil
}
func (backend *terminalTestBackend) ResizeTerminal(context.Context, TerminalResizeRequest) error {
	return errors.New("unexpected terminal resize")
}
func (backend *terminalTestBackend) InputTerminal(_ context.Context, request TerminalInputRequest) (uint32, error) {
	backend.terminalMu.Lock()
	backend.input = request
	backend.inputCalls++
	backend.terminalMu.Unlock()
	return uint32(len(request.Frame.Payload)), nil
}
func (backend *terminalTestBackend) waitControl(ctx context.Context) error {
	if backend.controlEntered == nil {
		return nil
	}
	select {
	case <-backend.controlEntered:
	default:
		close(backend.controlEntered)
	}
	select {
	case <-backend.controlRelease:
		return nil
	case <-ctx.Done():
		return ctx.Err()
	}
}
func (backend *terminalTestBackend) ControlAgent(ctx context.Context, _ Principal, request browserprotocol.AgentControl) (browserprotocol.AgentControlResult, error) {
	if err := backend.waitControl(ctx); err != nil {
		return browserprotocol.AgentControlResult{}, err
	}
	return browserprotocol.AgentControlResult{OperationID: request.OperationID, TaskID: request.TaskID, RunID: request.RunID, SuccessorTaskID: request.SuccessorTaskID, Status: "delivered"}, nil
}
func (*terminalTestBackend) TaskHistory(_ context.Context, _ [browserprotocol.ClientIDSize]byte, request browserprotocol.TaskHistoryGet) (browserprotocol.TaskHistory, error) {
	return browserprotocol.TaskHistory{TaskID: request.TaskID, Entries: []browserprotocol.TaskHistoryEntry{}}, nil
}
func (backend *terminalTestBackend) ReplyHumanRequest(ctx context.Context, _ Principal, request browserprotocol.HumanRequestReply) (browserprotocol.HumanRequestReplyResult, error) {
	if err := backend.waitControl(ctx); err != nil {
		return browserprotocol.HumanRequestReplyResult{}, err
	}
	return backend.replyResult, nil
}
func (backend *terminalTestBackend) CancelHumanRequestRun(ctx context.Context, _ Principal, request browserprotocol.HumanRequestCancelRun) (browserprotocol.HumanRequestCancelRunResult, error) {
	if err := backend.waitControl(ctx); err != nil {
		return browserprotocol.HumanRequestCancelRunResult{}, err
	}
	return backend.cancelResult, nil
}

func authenticateTerminalTest(t *testing.T, connection *websocket.Conn) browserprotocol.AuthResult {
	t.Helper()
	_ = readServerFrame(t, connection)
	proof, err := browserprotocol.EncodeAuthProve("auth", browserprotocol.AuthProve{
		ClientID: testID, Signature: strings.Repeat("01", browserprotocol.SignatureSize),
	})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, proof)
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeAuthResult || frame.ID != "auth" {
		t.Fatalf("auth result = %+v", frame)
	}
	return frame.Body.(browserprotocol.AuthResult)
}

func readTerminalBinary(t *testing.T, connection *websocket.Conn) browserprotocol.TerminalFrame {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	kind, payload, err := connection.Read(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if kind != websocket.MessageBinary {
		t.Fatalf("terminal frame kind=%v", kind)
	}
	frame, err := browserprotocol.DecodeTerminalFrame(payload)
	if err != nil {
		t.Fatalf("decode terminal frame: %v", err)
	}
	return frame
}

type terminalReadResult struct {
	kind    websocket.MessageType
	payload []byte
	err     error
}

func beginTerminalRead(connection *websocket.Conn) <-chan terminalReadResult {
	result := make(chan terminalReadResult, 1)
	go func() {
		kind, payload, err := connection.Read(context.Background())
		result <- terminalReadResult{kind: kind, payload: payload, err: err}
	}()
	return result
}

func expectTerminalReadError(t *testing.T, connection *websocket.Conn) {
	t.Helper()
	result := beginTerminalRead(connection)
	select {
	case read := <-result:
		if read.err == nil {
			t.Fatal("unexpected terminal frame")
		}
	case <-time.After(time.Second):
		t.Fatal("connection remained open")
	}
}

func terminalAttachRequest(t *testing.T, id string, after uint64) []byte {
	t.Helper()
	payload, err := browserprotocol.EncodeTerminalAttach(id, browserprotocol.TerminalAttach{
		RunID: testID, SessionID: projectID, ExpectedRunRevision: 1, ExpectedSessionRevision: 1,
		AfterSequence: browserprotocol.Decimal(after),
	})
	if err != nil {
		t.Fatal(err)
	}
	return payload
}

func sendTerminalEvent(t *testing.T, backend *terminalTestBackend, event TerminalEvent) {
	t.Helper()
	attachment := backend.currentAttachment(t)
	select {
	case attachment.events <- event:
	case <-time.After(time.Second):
		t.Fatal("terminal event queue did not accept event")
	}
}

func TestTerminalTransportKeepsTinyOutputFlowingDuringSlowControl(t *testing.T) {
	for _, test := range []struct {
		name    string
		request func(*terminalTestBackend) ([]byte, error)
		result  browserprotocol.MessageType
	}{
		{
			name: "agent control",
			request: func(*terminalTestBackend) ([]byte, error) {
				return browserprotocol.EncodeAgentControl("control", browserprotocol.AgentControl{
					OperationID: strings.Repeat("11", 16), TaskID: strings.Repeat("12", 16), RunID: strings.Repeat("13", 16),
					ExpectedTaskRevision: 1, ExpectedRunRevision: 1, Action: "message", Instruction: "continue",
				})
			},
			result: browserprotocol.TypeAgentControlResult,
		},
		{
			name: "human reply",
			request: func(backend *terminalTestBackend) ([]byte, error) {
				backend.replyResult = browserprotocol.HumanRequestReplyResult{RequestID: requestID, Revision: 3, Status: "resolved"}
				return browserprotocol.EncodeHumanRequestReply("reply", browserprotocol.HumanRequestReply{RequestID: requestID, ExpectedRevision: 1, Reply: "continue"})
			},
			result: browserprotocol.TypeHumanRequestReplyResult,
		},
		{
			name: "human cancellation",
			request: func(backend *terminalTestBackend) ([]byte, error) {
				backend.cancelResult = browserprotocol.HumanRequestCancelRunResult{RunID: testID, RunRevision: 2, RequestID: requestID, RequestRevision: 2}
				return browserprotocol.EncodeHumanRequestCancelRun("cancel", browserprotocol.HumanRequestCancelRun{RequestID: requestID, ExpectedRequestRevision: 1, ExpectedRunRevision: 1})
			},
			result: browserprotocol.TypeHumanRequestCancelRunResult,
		},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityHumanActions | browserprotocol.CapabilityTerminalInput
			backend.controlEntered, backend.controlRelease = make(chan struct{}), make(chan struct{})
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true})
			if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
				t.Fatalf("attach result = %+v", frame)
			}
			payload, err := test.request(backend)
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, payload)
			select {
			case <-backend.controlEntered:
			case <-time.After(time.Second):
				t.Fatal("control did not reach backend")
			}

			attachment := backend.currentAttachment(t)
			sent := make(chan error, 1)
			go func() {
				for sequence := uint64(0); sequence < 130; sequence++ {
					select {
					case attachment.events <- TerminalEvent{Kind: TerminalEventOutput, Start: sequence, End: sequence + 1, Payload: []byte{byte(sequence)}}:
					case <-time.After(time.Second):
						sent <- errors.New("terminal output stopped behind slow control")
						return
					}
				}
				sent <- nil
			}()
			for sequence := uint64(0); sequence < 130; sequence++ {
				frame := readTerminalBinary(t, connection)
				if frame.Sequence != sequence || len(frame.Payload) != 1 || frame.Payload[0] != byte(sequence) {
					t.Fatalf("tiny output[%d] = %+v", sequence, frame)
				}
				ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: browserprotocol.Decimal(sequence + 1)})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, ack)
			}
			if err := <-sent; err != nil {
				t.Fatal(err)
			}
			close(backend.controlRelease)
			if frame := readServerFrame(t, connection); frame.Type != test.result {
				t.Fatalf("control result = %+v", frame)
			}
		})
	}
}

func TestTerminalTransportAdvertisesExactDurableCapabilities(t *testing.T) {
	all := browserprotocol.CapabilityObserve | browserprotocol.CapabilityPrivateHumanRequestDetail | browserprotocol.CapabilityHumanActions | browserprotocol.CapabilityTerminalInput
	for _, test := range []struct {
		name string
		caps browserprotocol.Capabilities
	}{
		{name: "all", caps: all},
		{name: "observe only", caps: browserprotocol.CapabilityObserve},
		{name: "human action only", caps: browserprotocol.CapabilityObserve | browserprotocol.CapabilityHumanActions},
		{name: "terminal input only", caps: browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = test.caps
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			result := authenticateTerminalTest(t, connection)
			if result.Capabilities != test.caps {
				t.Fatalf("capabilities=%d want %d", result.Capabilities, test.caps)
			}
		})
	}
}

func TestTerminalTransportDeliversCanonicalExitAndRetainsConnection(t *testing.T) {
	for _, test := range []struct {
		name         string
		code, signal int
		aborted      bool
	}{
		{name: "success"},
		{name: "failure", code: 7, aborted: true},
		{name: "signal", signal: 15},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true})
			_ = readServerFrame(t, connection)

			attachment := backend.currentAttachment(t)
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventExit, ExitCode: test.code, ExitSignal: test.signal, Aborted: test.aborted})
			frame := readServerFrame(t, connection)
			exit, ok := frame.Body.(browserprotocol.TerminalExit)
			if frame.Type != browserprotocol.TypeTerminalExit || !ok || exit.ExitCode != int64(test.code) || exit.ExitSignal != int64(test.signal) || exit.Aborted != test.aborted {
				t.Fatalf("terminal exit = %+v", frame)
			}
			select {
			case <-attachment.closeDone:
			case <-time.After(time.Second):
				t.Fatal("exit attachment did not close")
			}
			if attachment.closed.Load() != 1 || attachment.closeCalls.Load() != 1 {
				t.Fatalf("exit attachment close = %d calls %d", attachment.closed.Load(), attachment.closeCalls.Load())
			}

			target, err := browserprotocol.EncodeTerminalTargetGet("still-open", browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, target)
			if next := readServerFrame(t, connection); next.Type != browserprotocol.TypeTerminalTarget || next.ID != "still-open" {
				t.Fatalf("connection was not retained: %+v", next)
			}
		})
	}
}

func TestTerminalTransportAcknowledgesOutputBeforeClosingTerminal(t *testing.T) {
	for _, test := range []struct {
		name     string
		event    TerminalEvent
		wantType browserprotocol.MessageType
	}{
		{name: "exit", event: TerminalEvent{Kind: TerminalEventExit}, wantType: browserprotocol.TypeTerminalExit},
		{name: "reset", event: TerminalEvent{Kind: TerminalEventReset, Floor: 1, Head: 1}, wantType: browserprotocol.TypeTerminalReset},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Head: 1})
			_ = readServerFrame(t, connection)

			attachment := backend.currentAttachment(t)
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 0, End: 1, Payload: []byte("x")})
			_ = readTerminalBinary(t, connection)
			closing := beginTerminalRead(connection)
			sendTerminalEvent(t, backend, test.event)
			select {
			case result := <-closing:
				t.Fatalf("terminal closed before output ACK: kind=%v err=%v", result.kind, result.err)
			case <-time.After(100 * time.Millisecond):
			}
			if attachment.closed.Load() != 0 {
				t.Fatal("attachment closed before output ACK")
			}

			ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: 1})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, ack)
			var result terminalReadResult
			select {
			case result = <-closing:
			case <-time.After(time.Second):
				t.Fatal("terminal did not close after output ACK")
			}
			if result.err != nil || result.kind != websocket.MessageText {
				t.Fatalf("closing frame kind=%v err=%v", result.kind, result.err)
			}
			frame, err := browserprotocol.DecodeServerControl(result.payload)
			if err != nil || frame.Type != test.wantType || frame.ID != "attach" {
				t.Fatalf("closing frame=%+v err=%v", frame, err)
			}
			select {
			case <-attachment.closeDone:
			case <-time.After(time.Second):
				t.Fatal("closing event did not close attachment")
			}
			if attachment.closed.Load() != 1 || attachment.closeCalls.Load() != 1 {
				t.Fatalf("closing event attachment close = %d calls %d", attachment.closed.Load(), attachment.closeCalls.Load())
			}

			target, err := browserprotocol.EncodeTerminalTargetGet("still-open", browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, target)
			if next := readServerFrame(t, connection); next.Type != browserprotocol.TypeTerminalTarget || next.ID != "still-open" {
				t.Fatalf("connection was not retained: %+v", next)
			}
		})
	}
}

func TestTerminalTransportConvertsSlowAttachmentCloseToReset(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Head: 1})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}

	attachment := backend.currentAttachment(t)
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 0, End: 1, Payload: []byte("x")})
	if output := readTerminalBinary(t, connection); output.Sequence != 0 || !bytes.Equal(output.Payload, []byte("x")) {
		t.Fatalf("output = %+v", output)
	}
	// The daemon closes a bounded slow observer after its queue is full. That
	// close must reuse the reset protocol and wait for the output ACK instead
	// of turning the attachment lifecycle into a WebSocket close.
	attachment.resetRequired = true
	close(attachment.events)
	read := beginTerminalRead(connection)
	select {
	case result := <-read:
		t.Fatalf("slow close reset before output ACK: kind=%v err=%v", result.kind, result.err)
	case <-time.After(100 * time.Millisecond):
	}

	ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: 1})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, ack)
	var result terminalReadResult
	select {
	case result = <-read:
	case <-time.After(time.Second):
		t.Fatal("slow close did not send reset after output ACK")
	}
	if result.err != nil || result.kind != websocket.MessageText {
		t.Fatalf("slow close result kind=%v err=%v", result.kind, result.err)
	}
	reset, err := browserprotocol.DecodeServerControl(result.payload)
	if err != nil || reset.Type != browserprotocol.TypeTerminalReset || reset.ID != "attach" {
		t.Fatalf("slow close reset=%+v err=%v", reset, err)
	}
	body := reset.Body.(browserprotocol.TerminalReset)
	// The reset captures the last browser-confirmed cursor at closure time.
	// It may replay the one output awaiting ACK on the fresh surface.
	if body.Floor != 0 || body.Head != 0 {
		t.Fatalf("slow close resume cursor = %+v", body)
	}
	if attachment.closed.Load() != 1 || attachment.closeCalls.Load() != 1 {
		t.Fatalf("slow close attachment close=%d calls=%d", attachment.closed.Load(), attachment.closeCalls.Load())
	}

	target, err := browserprotocol.EncodeTerminalTargetGet("still-open", browserprotocol.TerminalTargetGet{AgentID: strings.Repeat("01", 16), ExpectedAgentRevision: 1, ExpectedHead: 7})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, target)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalTarget || frame.ID != "still-open" {
		t.Fatalf("connection was not retained after slow close: %+v", frame)
	}
}

func TestTerminalTransportRejectsNoncanonicalExitWithoutFrame(t *testing.T) {
	for _, test := range []struct {
		name  string
		event TerminalEvent
	}{
		{name: "runner sentinel", event: TerminalEvent{Kind: TerminalEventExit, ExitCode: -1, ExitSignal: 15}},
		{name: "contradictory", event: TerminalEvent{Kind: TerminalEventExit, ExitCode: 7, ExitSignal: 9}},
		{name: "negative signal", event: TerminalEvent{Kind: TerminalEventExit, ExitSignal: -1}},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true})
			_ = readServerFrame(t, connection)
			sendTerminalEvent(t, backend, test.event)
			expectTerminalReadError(t, connection)
		})
	}
}

func TestTerminalTransportAttachCreditReplayAndInput(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)

	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 77))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 91, Floor: 90, Head: 94})
	attached := readServerFrame(t, connection)
	if attached.Type != browserprotocol.TypeTerminalAttached || attached.ID != "attach" {
		t.Fatalf("attached frame=%+v", attached)
	}
	attachedBody := attached.Body.(browserprotocol.TerminalAttached)
	if attachedBody.AcknowledgedSequence != 91 || attachedBody.Floor != 90 || attachedBody.Head != 94 {
		t.Fatalf("attached body=%+v", attachedBody)
	}

	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 91, End: 94, Payload: []byte("out")})
	output := readTerminalBinary(t, connection)
	if output.Opcode != browserprotocol.TerminalOutputOpcode || output.SessionID != fixedID(projectID) || output.Sequence != 91 || !bytes.Equal(output.Payload, []byte("out")) {
		t.Fatalf("output frame=%+v", output)
	}

	input, err := browserprotocol.EncodeTerminalInput(fixedID(projectID), 1, 7, []byte("in"))
	if err != nil {
		t.Fatal(err)
	}
	writeRawClientFrame(t, connection, websocket.MessageBinary, input)
	result := readServerFrame(t, connection)
	if result.Type != browserprotocol.TypeTerminalInputResult || result.ID != "attach" {
		t.Fatalf("input result=%+v", result)
	}
	inputResult := result.Body.(browserprotocol.TerminalInputResult)
	if inputResult.Status != "accepted" || inputResult.AcceptedBytes != 2 || inputResult.Sequence != 1 || inputResult.Generation != 7 {
		t.Fatalf("input result body=%+v", inputResult)
	}

	ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: 94})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, ack)
	writeClientFrame(t, connection, ack)
	expectTerminalReadError(t, connection)
}

func writeRawClientFrame(t *testing.T, connection *websocket.Conn, kind websocket.MessageType, payload []byte) {
	t.Helper()
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	if err := connection.Write(ctx, kind, payload); err != nil {
		t.Fatal(err)
	}
}

func TestTerminalTransportCreditWindowHoldsOnePendingOutput(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 73728})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	for index := 0; index < browserprotocol.MaxTerminalUnackedBytes/browserprotocol.MaxTerminalPayload; index++ {
		start := uint64(index * browserprotocol.MaxTerminalPayload)
		payload := bytes.Repeat([]byte{'x'}, browserprotocol.MaxTerminalPayload)
		sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: start, End: start + uint64(len(payload)), Payload: payload})
		frame := readTerminalBinary(t, connection)
		if frame.Sequence != start || len(frame.Payload) != browserprotocol.MaxTerminalPayload {
			t.Fatalf("output[%d]=sequence %d payload=%d", index, frame.Sequence, len(frame.Payload))
		}
	}
	// The ninth event is queued by the daemon but must remain pending while the
	// full 64KiB window is unacknowledged.
	payload := bytes.Repeat([]byte{'y'}, browserprotocol.MaxTerminalPayload)
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: browserprotocol.MaxTerminalUnackedBytes, End: browserprotocol.MaxTerminalUnackedBytes + browserprotocol.MaxTerminalPayload, Payload: payload})
	read := beginTerminalRead(connection)
	select {
	case result := <-read:
		if result.err == nil {
			t.Fatalf("output exceeded credit window: kind=%v bytes=%d", result.kind, len(result.payload))
		}
		t.Fatalf("connection closed before credit ACK: %v", result.err)
	case <-time.After(100 * time.Millisecond):
	}
	ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: browserprotocol.MaxTerminalUnackedBytes})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, ack)
	result := <-read
	if result.err != nil || result.kind != websocket.MessageBinary {
		t.Fatalf("pending output read = kind=%v err=%v", result.kind, result.err)
	}
	frame, err := browserprotocol.DecodeTerminalFrame(result.payload)
	if err != nil {
		t.Fatal(err)
	}
	if frame.Sequence != browserprotocol.MaxTerminalUnackedBytes || !bytes.Equal(frame.Payload, payload) {
		t.Fatalf("pending output=%+v", frame)
	}

	// Duplicate ACKs are not idempotent: the browser must not be able to move
	// credit state backward or reuse a cursor outside the sent range.
	writeClientFrame(t, connection, ack)
	expectTerminalReadError(t, connection)
}

func TestTerminalTransportRejectsAheadAndBackwardACKs(t *testing.T) {
	for _, test := range []struct {
		name       string
		firstACK   uint64
		invalidACK uint64
		ackSession string
	}{
		{name: "ahead", invalidACK: 3},
		{name: "backward", firstACK: 2, invalidACK: 1},
		{name: "wrong session", invalidACK: 1, ackSession: testID},
	} {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 2})
			if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
				t.Fatalf("attached frame=%+v", frame)
			}
			sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 0, End: 2, Payload: []byte("xy")})
			_ = readTerminalBinary(t, connection)
			if test.firstACK != 0 {
				ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: browserprotocol.Decimal(test.firstACK)})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, ack)
			}
			ackSession := test.ackSession
			if ackSession == "" {
				ackSession = projectID
			}
			ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: ackSession, NextSequence: browserprotocol.Decimal(test.invalidACK)})
			if err != nil {
				t.Fatal(err)
			}
			writeClientFrame(t, connection, ack)
			expectTerminalReadError(t, connection)
		})
	}
}

func TestTerminalTransportDisconnectsAfterControlledACKTimeout(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServerWithAckTimeout(t, backend, 5*time.Millisecond)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 1})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 0, End: 1, Payload: []byte("x")})
	_ = readTerminalBinary(t, connection)
	expectTerminalReadError(t, connection)
}

func TestTerminalTransportACKProgressExtendsTimeoutWhileOutputRemains(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	ackTimeout := 200 * time.Millisecond
	server := startTerminalServerWithAckTimeout(t, backend, ackTimeout)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: browserprotocol.MaxTerminalUnackedBytes + browserprotocol.MaxTerminalPayload})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	for start := uint64(0); start < browserprotocol.MaxTerminalUnackedBytes; start += browserprotocol.MaxTerminalPayload {
		payload := bytes.Repeat([]byte{'x'}, browserprotocol.MaxTerminalPayload)
		sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: start, End: start + uint64(len(payload)), Payload: payload})
		_ = readTerminalBinary(t, connection)
	}
	// The ninth frame is held at the browser's 64 KiB credit limit. Its later
	// delivery proves the forward ACK was accepted while output remains pending.
	payload := bytes.Repeat([]byte{'y'}, browserprotocol.MaxTerminalPayload)
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: browserprotocol.MaxTerminalUnackedBytes, End: browserprotocol.MaxTerminalUnackedBytes + uint64(len(payload)), Payload: payload})
	time.Sleep(ackTimeout / 2)
	ack, err := browserprotocol.EncodeTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: browserprotocol.MaxTerminalPayload})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, ack)
	if frame := readTerminalBinary(t, connection); frame.Sequence != browserprotocol.MaxTerminalUnackedBytes || !bytes.Equal(frame.Payload, payload) {
		t.Fatalf("released frame=%+v", frame)
	}

	// This passes the original deadline but not the deadline renewed by the
	// forward ACK. The remaining output must keep observing without a close.
	time.Sleep(ackTimeout * 5 / 8)
	read := beginTerminalRead(connection)
	select {
	case result := <-read:
		t.Fatalf("terminal closed after forward ACK: kind=%v err=%v", result.kind, result.err)
	case <-time.After(ackTimeout / 8):
	}
	// No further ACK is progress, so the renewed deadline still closes the
	// connection and preserves the existing stalled-reader protection.
	select {
	case result := <-read:
		if result.err == nil {
			t.Fatalf("expected stalled ACK close, got kind=%v bytes=%d", result.kind, len(result.payload))
		}
	case <-time.After(ackTimeout):
		t.Fatal("terminal remained open after ACK progress stopped")
	}
}

func TestTerminalACKAfterExpiredDeadlineCannotReviveTimer(t *testing.T) {
	for _, test := range []struct {
		name       string
		sent, next uint64
	}{
		{name: "partial progress", sent: 2, next: 1},
		{name: "caught up", sent: 1, next: 1},
	} {
		t.Run(test.name, func(t *testing.T) {
			timer := time.NewTimer(time.Hour)
			t.Cleanup(func() { timer.Stop() })
			current := &connection{
				server:            &Server{terminalAckTimeout: time.Hour},
				attachment:        &terminalTestAttachment{},
				terminalAttach:    browserprotocol.TerminalAttach{SessionID: projectID},
				terminalSent:      test.sent,
				terminalAckTimer:  timer,
				terminalAckExpiry: time.Now().Add(-time.Nanosecond),
			}
			if current.handleTerminalAck(browserprotocol.TerminalAck{SessionID: projectID, NextSequence: browserprotocol.Decimal(test.next)}) {
				t.Fatal("late ACK revived an expired timer")
			}
			if current.terminalAck != 0 || current.terminalAckTimer != timer {
				t.Fatalf("late ACK changed terminal state: ack=%d timer=%p", current.terminalAck, current.terminalAckTimer)
			}
		})
	}
}

func TestTerminalTransportResetAndDetachJoinAttachment(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 1))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 1, Floor: 1, Head: 1})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventReset, Floor: 5, Head: 7})
	reset := readServerFrame(t, connection)
	if reset.Type != browserprotocol.TypeTerminalReset || reset.ID != "attach" {
		t.Fatalf("reset frame=%+v", reset)
	}
	attachment := backend.currentAttachment(t)
	select {
	case <-attachment.closeDone:
	case <-time.After(time.Second):
		t.Fatal("reset attachment did not close")
	}
	if attachment.closed.Load() != 1 || attachment.closeCalls.Load() != 1 {
		t.Fatalf("reset close count=%d calls=%d", attachment.closed.Load(), attachment.closeCalls.Load())
	}
	// A reset retires the attachment; a late queue event cannot become output.
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventOutput, Start: 1, End: 2, Payload: []byte("x")})

	backend2 := newTerminalTestBackend()
	backend2.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server2 := startTerminalServer(t, backend2)
	connection2, _ := dialServer(t, server2, testOrigin)
	authenticateTerminalTest(t, connection2)
	writeClientFrame(t, connection2, terminalAttachRequest(t, "attach", 1))
	sendTerminalEvent(t, backend2, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 1, Floor: 1, Head: 1})
	if frame := readServerFrame(t, connection2); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	detach, err := browserprotocol.EncodeTerminalDetach("detach", browserprotocol.TerminalDetach{SessionID: projectID})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection2, detach)
	if frame := readServerFrame(t, connection2); frame.Type != browserprotocol.TypeTerminalDetached {
		t.Fatalf("detached frame=%+v", frame)
	}
	if attachment := backend2.currentAttachment(t); attachment.closed.Load() != 1 || attachment.closeCalls.Load() != 1 {
		t.Fatalf("detach close count=%d calls=%d", attachment.closed.Load(), attachment.closeCalls.Load())
	}
}

func TestTerminalTransportRetainsAttachmentAfterCloseFailure(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 0})
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalAttached {
		t.Fatalf("attached frame=%+v", frame)
	}
	attachment := backend.currentAttachment(t)
	closeErr := errors.New("attachment close failed once")
	attachment.closeFirstErr = closeErr
	detach, err := browserprotocol.EncodeTerminalDetach("detach-fail", browserprotocol.TerminalDetach{SessionID: projectID})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, detach)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
	if attachment.closeCalls.Load() != 1 || attachment.closed.Load() != 0 {
		t.Fatalf("failed detach cleanup = calls %d, closed %d", attachment.closeCalls.Load(), attachment.closed.Load())
	}

	secondAttach := terminalAttachRequest(t, "attach-again", 0)
	writeClientFrame(t, connection, secondAttach)
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorUnauthorized)
	expectTerminalReadError(t, connection)
	if attachment.closeCalls.Load() != 2 || attachment.closed.Load() != 1 {
		t.Fatalf("shutdown attachment cleanup = calls %d, closed %d", attachment.closeCalls.Load(), attachment.closed.Load())
	}
}

func TestTerminalTransportRetriesFailedDetachAndClearsOnlyAfterSuccess(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 0})
	_ = readServerFrame(t, connection)
	attachment := backend.currentAttachment(t)
	attachment.closeFirstErr = errors.New("temporary attachment close failure")
	for _, test := range []struct {
		id       string
		wantType browserprotocol.MessageType
	}{
		{id: "detach-fail", wantType: browserprotocol.TypeError},
		{id: "detach-success", wantType: browserprotocol.TypeTerminalDetached},
	} {
		detach, err := browserprotocol.EncodeTerminalDetach(test.id, browserprotocol.TerminalDetach{SessionID: projectID})
		if err != nil {
			t.Fatal(err)
		}
		writeClientFrame(t, connection, detach)
		frame := readServerFrame(t, connection)
		if frame.Type != test.wantType {
			t.Fatalf("%s response=%+v", test.id, frame)
		}
		if test.wantType == browserprotocol.TypeError {
			assertError(t, frame, browserprotocol.ErrorInternal)
			if attachment.closeCalls.Load() != 1 || attachment.closed.Load() != 0 {
				t.Fatalf("failed detach state = calls %d, closed %d", attachment.closeCalls.Load(), attachment.closed.Load())
			}
		}
	}
	if attachment.closeCalls.Load() != 2 || attachment.closed.Load() != 1 {
		t.Fatalf("successful detach cleanup = calls %d, closed %d", attachment.closeCalls.Load(), attachment.closed.Load())
	}
}

func TestTerminalTransportClosesInvalidBackendAttachment(t *testing.T) {
	closeErr := errors.New("invalid attachment close failed")
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve
	backend.invalidAttach = true
	backend.attachmentCloseErr = closeErr
	server, err := Listen(Config{Address: "127.0.0.1:0", AllowedOrigins: []string{testOrigin, devOrigin}, Backend: backend})
	if err != nil {
		t.Fatal(err)
	}
	server.terminalAckTimeout = time.Duration(browserprotocol.TerminalAckTimeoutMS) * time.Millisecond
	defer func() { _ = server.Close() }()
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	attachmentRequest := terminalAttachRequest(t, "invalid-attach", 0)
	writeClientFrame(t, connection, attachmentRequest)
	attachment := backend.currentAttachment(t)
	// The backend call and validation are synchronous, so the invalid
	// attachment has already been closed before its error reaches the client.
	assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
	expectTerminalReadError(t, connection)
	if attachment.closeCalls.Load() != 1 {
		t.Fatalf("invalid attachment close calls=%d", attachment.closeCalls.Load())
	}
	cleanupErr := server.Close()
	if !errors.Is(cleanupErr, errInvalidTerminalAttachment) || !errors.Is(cleanupErr, closeErr) {
		t.Fatalf("invalid attachment cleanup = %v", cleanupErr)
	}
}

func TestTerminalTransportHumanRequestEffectDispatch(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityHumanActions
	backend.replyResult = browserprotocol.HumanRequestReplyResult{RequestID: requestID, Revision: 3, Status: "resolved"}
	backend.cancelResult = browserprotocol.HumanRequestCancelRunResult{RunID: testID, RunRevision: 3, RequestID: requestID, RequestRevision: 2}
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	reply, err := browserprotocol.EncodeHumanRequestReply("reply", browserprotocol.HumanRequestReply{RequestID: requestID, ExpectedRevision: 1, Reply: "yes"})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, reply)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeHumanRequestReplyResult || frame.ID != "reply" || frame.Body.(browserprotocol.HumanRequestReplyResult).Status != "resolved" {
		t.Fatalf("reply result=%+v", frame)
	}
	cancel, err := browserprotocol.EncodeHumanRequestCancelRun("cancel", browserprotocol.HumanRequestCancelRun{RequestID: requestID, ExpectedRequestRevision: 1, ExpectedRunRevision: 2})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, cancel)
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeHumanRequestCancelRunResult || frame.ID != "cancel" || frame.Body.(browserprotocol.HumanRequestCancelRunResult).RunID != testID {
		t.Fatalf("cancel result=%+v", frame)
	}
}

func TestTerminalTransportRejectsMismatchedHumanEffectResults(t *testing.T) {
	tests := []struct {
		name      string
		configure func(*terminalTestBackend)
		write     func(*testing.T, *websocket.Conn)
	}{
		{
			name: "reply request id",
			configure: func(backend *terminalTestBackend) {
				backend.replyResult = browserprotocol.HumanRequestReplyResult{RequestID: testID, Revision: 3, Status: "resolved"}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeHumanRequestReply("reply-request", browserprotocol.HumanRequestReply{RequestID: requestID, ExpectedRevision: 1, Reply: "yes"})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "reply revision",
			configure: func(backend *terminalTestBackend) {
				backend.replyResult = browserprotocol.HumanRequestReplyResult{RequestID: requestID, Revision: 1, Status: "resolved"}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeHumanRequestReply("reply-revision", browserprotocol.HumanRequestReply{RequestID: requestID, ExpectedRevision: 1, Reply: "yes"})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "reply status",
			configure: func(backend *terminalTestBackend) {
				backend.replyResult = browserprotocol.HumanRequestReplyResult{RequestID: requestID, Revision: 3, Status: "pending"}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeHumanRequestReply("reply-status", browserprotocol.HumanRequestReply{RequestID: requestID, ExpectedRevision: 1, Reply: "yes"})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "cancel request id",
			configure: func(backend *terminalTestBackend) {
				backend.cancelResult = browserprotocol.HumanRequestCancelRunResult{RunID: testID, RunRevision: 3, RequestID: testID, RequestRevision: 2}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeHumanRequestCancelRun("cancel-request", browserprotocol.HumanRequestCancelRun{RequestID: requestID, ExpectedRequestRevision: 1, ExpectedRunRevision: 2})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "cancel revisions",
			configure: func(backend *terminalTestBackend) {
				backend.cancelResult = browserprotocol.HumanRequestCancelRunResult{RunID: testID, RunRevision: 2, RequestID: requestID, RequestRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeHumanRequestCancelRun("cancel-revisions", browserprotocol.HumanRequestCancelRun{RequestID: requestID, ExpectedRequestRevision: 1, ExpectedRunRevision: 2})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityHumanActions
			test.configure(backend)
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			test.write(t, connection)
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
			expectTerminalReadError(t, connection)
		})
	}
}

func TestTerminalTransportValidatesLeaseResultRelations(t *testing.T) {
	tests := []struct {
		name      string
		configure func(*terminalTestBackend)
		write     func(*testing.T, *websocket.Conn)
	}{
		{
			name: "operation",
			configure: func(backend *terminalTestBackend) {
				expires := browserprotocol.Decimal(20)
				backend.leaseResult = TerminalLeaseResult{Operation: "renewed", RunID: testID, SessionID: projectID, Generation: 3, ExpiresAtMS: &expires, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseAcquire("lease-operation", browserprotocol.TerminalLeaseAcquire{RunID: testID, SessionID: projectID, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "run session",
			configure: func(backend *terminalTestBackend) {
				expires := browserprotocol.Decimal(20)
				backend.leaseResult = TerminalLeaseResult{Operation: "acquired", RunID: testID, SessionID: testID, Generation: 3, ExpiresAtMS: &expires, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseAcquire("lease-identity", browserprotocol.TerminalLeaseAcquire{RunID: testID, SessionID: projectID, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "generation",
			configure: func(backend *terminalTestBackend) {
				expires := browserprotocol.Decimal(20)
				backend.leaseResult = TerminalLeaseResult{Operation: "renewed", RunID: testID, SessionID: projectID, Generation: 4, ExpiresAtMS: &expires, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRenew("lease-generation", browserprotocol.TerminalLeaseRenew{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "revisions",
			configure: func(backend *terminalTestBackend) {
				expires := browserprotocol.Decimal(20)
				backend.leaseResult = TerminalLeaseResult{Operation: "renewed", RunID: testID, SessionID: projectID, Generation: 3, ExpiresAtMS: &expires, RunRevision: 2, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRenew("lease-revisions", browserprotocol.TerminalLeaseRenew{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "expiry",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 3, ExpiresAtMS: pointerDecimal(20), RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-expiry", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release same generation",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 3, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-same", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release skips generation",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 5, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-skip", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release rewinds generation",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 2, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-rewind", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release resets input sequence",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 4, LastInputSequence: 1, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-sequence", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release zero generation",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 0, RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-zero", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
		{
			name: "release generation overflow",
			configure: func(backend *terminalTestBackend) {
				backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: browserprotocol.Decimal(browserprotocol.MaxSQLiteInteger), RunRevision: 1, SessionRevision: 1}
			},
			write: func(t *testing.T, connection *websocket.Conn) {
				payload, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release-overflow", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: browserprotocol.Decimal(browserprotocol.MaxSQLiteInteger), ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
				if err != nil {
					t.Fatal(err)
				}
				writeClientFrame(t, connection, payload)
			},
		},
	}
	for _, test := range tests {
		t.Run(test.name, func(t *testing.T) {
			backend := newTerminalTestBackend()
			backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
			test.configure(backend)
			server := startTerminalServer(t, backend)
			connection, _ := dialServer(t, server, testOrigin)
			authenticateTerminalTest(t, connection)
			test.write(t, connection)
			assertError(t, readServerFrame(t, connection), browserprotocol.ErrorInternal)
			expectTerminalReadError(t, connection)
		})
	}
}

func pointerDecimal(value browserprotocol.Decimal) *browserprotocol.Decimal { return &value }

func TestTerminalTransportAcceptsExactLeaseResults(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)

	expires := browserprotocol.Decimal(20)
	backend.leaseResult = TerminalLeaseResult{Operation: "acquired", RunID: testID, SessionID: projectID, Generation: 3, ExpiresAtMS: &expires, RunRevision: 1, SessionRevision: 1}
	acquire, err := browserprotocol.EncodeTerminalLeaseAcquire("lease-acquire", browserprotocol.TerminalLeaseAcquire{RunID: testID, SessionID: projectID, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, acquire)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalLeaseResult || frame.Body.(browserprotocol.TerminalLeaseResult).Operation != "acquired" {
		t.Fatalf("acquire result=%+v", frame)
	}

	backend.leaseResult = TerminalLeaseResult{Operation: "renewed", RunID: testID, SessionID: projectID, Generation: 3, ExpiresAtMS: &expires, LastInputSequence: 4, RunRevision: 1, SessionRevision: 1}
	renew, err := browserprotocol.EncodeTerminalLeaseRenew("lease-renew", browserprotocol.TerminalLeaseRenew{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, renew)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalLeaseResult || frame.Body.(browserprotocol.TerminalLeaseResult).Operation != "renewed" {
		t.Fatalf("renew result=%+v", frame)
	}

	backend.leaseResult = TerminalLeaseResult{Operation: "released", RunID: testID, SessionID: projectID, Generation: 4, RunRevision: 1, SessionRevision: 1}
	release, err := browserprotocol.EncodeTerminalLeaseRelease("lease-release", browserprotocol.TerminalLeaseRelease{RunID: testID, SessionID: projectID, Generation: 3, ExpectedRunRevision: 1, ExpectedSessionRevision: 1})
	if err != nil {
		t.Fatal(err)
	}
	writeClientFrame(t, connection, release)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeTerminalLeaseResult || frame.Body.(browserprotocol.TerminalLeaseResult).Operation != "released" || frame.Body.(browserprotocol.TerminalLeaseResult).Generation != 4 || frame.Body.(browserprotocol.TerminalLeaseResult).LastInputSequence != 0 || frame.Body.(browserprotocol.TerminalLeaseResult).ExpiresAtMS != nil {
		t.Fatalf("release result=%+v", frame)
	}
}

func TestTerminalTransportBinaryDirectionAndSessionGuards(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	input, err := browserprotocol.EncodeTerminalInput(fixedID(projectID), 1, 1, []byte("x"))
	if err != nil {
		t.Fatal(err)
	}
	writeRawClientFrame(t, connection, websocket.MessageBinary, input)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeError || frame.Body.(browserprotocol.Error).Code != browserprotocol.ErrorInvalidRequest {
		t.Fatalf("unattached binary result=%+v", frame)
	}

	backend2 := newTerminalTestBackend()
	backend2.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server2 := startTerminalServer(t, backend2)
	connection2, _ := dialServer(t, server2, testOrigin)
	authenticateTerminalTest(t, connection2)
	writeClientFrame(t, connection2, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend2, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 1})
	_ = readServerFrame(t, connection2)
	wrong, err := browserprotocol.EncodeTerminalInput(fixedID(testID), 1, 1, []byte("x"))
	if err != nil {
		t.Fatal(err)
	}
	writeRawClientFrame(t, connection2, websocket.MessageBinary, wrong)
	if frame := readServerFrame(t, connection2); frame.Type != browserprotocol.TypeError || frame.Body.(browserprotocol.Error).Code != browserprotocol.ErrorInvalidRequest {
		t.Fatalf("wrong-session binary result=%+v", frame)
	}
}

func TestTerminalTransportOversizedBinaryCloses(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	oversize := make([]byte, browserprotocol.TerminalHeaderSize+browserprotocol.MaxTerminalPayload+1)
	copy(oversize[:4], []byte{'D', 'F', 1, byte(browserprotocol.TerminalInputOpcode)})
	sessionID := fixedID(projectID)
	copy(oversize[4:20], sessionID[:])
	writeRawClientFrame(t, connection, websocket.MessageBinary, oversize)
	if frame := readServerFrame(t, connection); frame.Type != browserprotocol.TypeError {
		t.Fatalf("oversize result=%+v", frame)
	}
	expectTerminalReadError(t, connection)
}

func TestTerminalTransportInputResultKeepsExactBackendCount(t *testing.T) {
	backend := newTerminalTestBackend()
	backend.authentication.Capabilities = browserprotocol.CapabilityObserve | browserprotocol.CapabilityTerminalInput
	server := startTerminalServer(t, backend)
	connection, _ := dialServer(t, server, testOrigin)
	authenticateTerminalTest(t, connection)
	writeClientFrame(t, connection, terminalAttachRequest(t, "attach", 0))
	sendTerminalEvent(t, backend, TerminalEvent{Kind: TerminalEventAttached, Accepted: true, Sequence: 0, Floor: 0, Head: 1})
	_ = readServerFrame(t, connection)
	input, err := browserprotocol.EncodeTerminalInput(fixedID(projectID), 1, 3, []byte("exact"))
	if err != nil {
		t.Fatal(err)
	}
	writeRawClientFrame(t, connection, websocket.MessageBinary, input)
	frame := readServerFrame(t, connection)
	if frame.Type != browserprotocol.TypeTerminalInputResult || frame.Body.(browserprotocol.TerminalInputResult).AcceptedBytes != 5 {
		t.Fatalf("input result=%+v", frame)
	}
	backend.terminalMu.Lock()
	got := backend.input
	calls := backend.inputCalls
	backend.terminalMu.Unlock()
	if calls != 1 || got.RunID != testID || got.SessionID != projectID || got.RunRevision != 1 || got.SessionRevision != 1 || got.Frame.Sequence != 1 || got.Frame.LeaseGeneration != 3 {
		t.Fatalf("backend input=%+v calls=%d", got, calls)
	}
}
