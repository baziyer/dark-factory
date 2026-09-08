package runner

import (
	"bytes"
	"errors"
	"io"
	"reflect"
	"syscall"
	"testing"
)

type shortWriter struct {
	buf bytes.Buffer
	max int
}

func (w *shortWriter) Write(p []byte) (int, error) {
	if len(p) > w.max {
		p = p[:w.max]
	}
	return w.buf.Write(p)
}

type noProgressWriter struct{}

func (noProgressWriter) Write([]byte) (int, error) { return 0, nil }

type errnoWriter struct {
	partial int
	err     error
}

func (w errnoWriter) Write(p []byte) (int, error) {
	if w.partial > len(p) {
		w.partial = len(p)
	}
	return w.partial, w.err
}

func TestWriteFrameCompletesShortHeaderAndBody(t *testing.T) {
	var wire shortWriter
	wire.max = 1
	want := gateFrame{Kind: "ready", Error: "short writes are not a frame boundary"}
	if err := writeFrame(&wire, want, maxFrameBytes); err != nil {
		t.Fatal(err)
	}
	var got gateFrame
	if err := readFrame(bytes.NewReader(wire.buf.Bytes()), &got, maxFrameBytes); err != nil {
		t.Fatal(err)
	}
	if got.Kind != want.Kind || got.Error != want.Error {
		t.Fatalf("frame = %+v, want %+v", got, want)
	}
}

func TestWriteFrameRejectsZeroProgress(t *testing.T) {
	if err := writeFrame(noProgressWriter{}, gateFrame{Kind: "ready"}, maxFrameBytes); !errors.Is(err, io.ErrNoProgress) {
		t.Fatalf("zero-progress write = %v, want io.ErrNoProgress", err)
	}
}

func TestWriteFullySurfacesWouldBlockBeforeAndAfterProgress(t *testing.T) {
	if err := writeFully(errnoWriter{err: syscall.EAGAIN}, []byte("header")); !errors.Is(err, syscall.EAGAIN) {
		t.Fatalf("would-block before progress = %v", err)
	}
	if err := writeFully(errnoWriter{partial: 1, err: syscall.EAGAIN}, []byte("body")); !errors.Is(err, syscall.EAGAIN) {
		t.Fatalf("would-block after progress = %v", err)
	}
}

func TestTerminalCommandAndEventFramesRoundTrip(t *testing.T) {
	commands := []TerminalCommand{
		{Kind: TerminalGenerationInstall, Correlation: 1, Generation: 2},
		{Kind: TerminalGenerationRevoke, Correlation: 2, Generation: 2},
		{Kind: TerminalAttach, Correlation: 3, Sequence: 4},
		{Kind: TerminalCredit, Credit: 4096},
		{Kind: TerminalInput, Correlation: 5, Generation: 2, Sequence: 1, Payload: []byte("input")},
		{Kind: TerminalResize, Correlation: 6, Generation: 2, Rows: 24, Cols: 80},
		{Kind: TerminalHumanReply, Correlation: 7, Payload: bytes.Repeat([]byte{'x'}, maxTerminalFramePayload), Submit: true},
	}
	for _, want := range commands {
		if err := want.validate(); err != nil {
			t.Fatalf("command %q rejected: %v", want.Kind, err)
		}
		var wire bytes.Buffer
		if err := writeFrame(&wire, terminalCommandFrame(want), maxFrameBytes); err != nil {
			t.Fatal(err)
		}
		var raw attemptFrame
		if err := readFrame(&wire, &raw, maxFrameBytes); err != nil {
			t.Fatal(err)
		}
		got, err := terminalCommandFromFrame(raw)
		if err != nil || got.Kind != want.Kind || got.Correlation != want.Correlation || got.Generation != want.Generation || got.Sequence != want.Sequence || got.Credit != want.Credit || got.Rows != want.Rows || got.Cols != want.Cols || got.Submit != want.Submit || !bytes.Equal(got.Payload, want.Payload) {
			t.Fatalf("command = %+v, err=%v, want %+v", got, err, want)
		}
	}

	events := []TerminalFrame{
		{Kind: TerminalGenerationResult, Correlation: 7, Generation: 2, Status: TerminalResultOK},
		{Kind: TerminalInputResult, Correlation: 8, Generation: 2, Sequence: 1, Status: TerminalResultPartial, Count: 3},
		{Kind: TerminalResizeResult, Correlation: 9, Generation: 2, Rows: 24, Cols: 80, Status: TerminalResultOK},
		{Kind: TerminalHumanReplyResult, Correlation: 13, Status: TerminalResultOK, Count: maxTerminalFramePayload},
		{Kind: TerminalHumanReplyResult, Correlation: 14, Status: TerminalResultPartial, Count: 3},
		{Kind: TerminalHumanReplyResult, Correlation: 15, Status: TerminalResultUncertain},
		{Kind: TerminalAttached, Correlation: 10, Sequence: 12, Floor: 10, Head: 15, Status: TerminalResultOK},
		{Kind: TerminalOutput, Start: 10, End: 15, Payload: []byte("hello")},
		{Kind: TerminalOutput, Correlation: 11, Start: 10, End: 15, Payload: []byte("hello")},
		{Kind: TerminalReset, Floor: 10, Head: 15},
		{Kind: TerminalReset, Correlation: 12, Floor: 10, Head: 15},
		{Kind: TerminalPTYEOF},
	}
	for _, want := range events {
		if err := want.validate(); err != nil {
			t.Fatalf("event %q rejected: %v", want.Kind, err)
		}
		var wire bytes.Buffer
		if err := writeFrame(&wire, terminalEventFrame(want), maxFrameBytes); err != nil {
			t.Fatal(err)
		}
		var raw attemptFrame
		if err := readFrame(&wire, &raw, maxFrameBytes); err != nil {
			t.Fatal(err)
		}
		got, err := terminalEventFromFrame(raw)
		if err != nil || got.Kind != want.Kind || got.Correlation != want.Correlation || got.Generation != want.Generation || got.Sequence != want.Sequence || got.Start != want.Start || got.End != want.End || got.Floor != want.Floor || got.Head != want.Head || got.Count != want.Count || got.Rows != want.Rows || got.Cols != want.Cols || got.Status != want.Status || !bytes.Equal(got.Payload, want.Payload) {
			t.Fatalf("event = %+v, err=%v, want %+v", got, err, want)
		}
	}
}

func TestTerminalWireValidationRejectsAmbiguousFrames(t *testing.T) {
	badCommands := []TerminalCommand{
		{Kind: TerminalInput, Correlation: 1, Generation: 1, Sequence: 1},
		{Kind: TerminalResize, Correlation: 1, Generation: 1, Rows: 0, Cols: 80},
		{Kind: TerminalCredit, Credit: maxTerminalCredit + 1},
		{Kind: TerminalInput, Correlation: 1, Generation: 1, Sequence: 1, Payload: bytes.Repeat([]byte{'x'}, maxTerminalFramePayload+1)},
		{Kind: TerminalInput, Correlation: 1, Generation: 1, Sequence: 1, Payload: []byte("input"), Submit: true},
		{Kind: TerminalHumanReply, Correlation: 1, Payload: bytes.Repeat([]byte{'x'}, maxTerminalFramePayload+1)},
		{Kind: TerminalHumanReply, Correlation: 1, Generation: 1, Payload: []byte("reply")},
		{Kind: TerminalHumanReply, Correlation: 1, Sequence: 1, Payload: []byte("reply")},
		{Kind: TerminalHumanReply, Correlation: 1, Payload: nil},
		{Kind: "terminal-unknown", Correlation: 1},
		{Kind: TerminalResize, Correlation: 1, Generation: 1, Rows: 4097, Cols: 80},
	}
	for _, command := range badCommands {
		if err := command.validate(); err == nil {
			t.Fatalf("accepted invalid command %+v", command)
		}
	}
	badEvents := []TerminalFrame{
		{Kind: TerminalOutput, Start: 2, End: 4, Payload: []byte("x")},
		{Kind: TerminalOutput, Correlation: maxTerminalCorrelation + 1, Start: 2, End: 3, Payload: []byte("x")},
		{Kind: TerminalAttached, Correlation: 1, Sequence: 5, Floor: 0, Head: 4, Status: TerminalResultOK},
		{Kind: TerminalAttached, Correlation: 1, Sequence: 4, Floor: 0, Head: 4, Status: TerminalResultRejected},
		{Kind: TerminalAttached, Correlation: 1, Sequence: 4, Floor: 0, Head: 4, Status: TerminalResultPartial},
		{Kind: TerminalReset, Correlation: 1, Generation: 1, Floor: 3, Head: 2},
		{Kind: TerminalGenerationResult, Correlation: 1, Generation: 1, Status: "unknown"},
		{Kind: TerminalPTYEOF, Generation: 1, Payload: []byte("unexpected")},
		{Kind: TerminalHumanReplyResult, Correlation: 1, Generation: 1, Status: TerminalResultOK},
		{Kind: TerminalHumanReplyResult, Correlation: 1, Status: "unknown"},
		{Kind: TerminalHumanReplyResult, Correlation: 1, Status: TerminalResultOK, Count: maxTerminalFramePayload + 1},
		{Kind: "terminal-unknown", Generation: 1},
	}
	for _, event := range badEvents {
		if err := event.validate(); err == nil {
			t.Fatalf("accepted invalid event %+v", event)
		}
	}
}

func TestTerminalObservationFramesAreIndependentOfGeneration(t *testing.T) {
	valid := []TerminalFrame{
		{Kind: TerminalOutput, Start: 0, End: 5, Payload: []byte("hello")},
		{Kind: TerminalOutput, Correlation: 1, Start: 5, End: 10, Payload: []byte("world")},
		{Kind: TerminalOutput, Correlation: 2, Start: 0, End: 5, Payload: []byte("hello")},
		{Kind: TerminalReset, Floor: 5, Head: 10},
		{Kind: TerminalReset, Correlation: 1, Floor: 5, Head: 10},
		{Kind: TerminalPTYEOF},
	}
	for _, frame := range valid {
		if err := frame.validate(); err != nil {
			t.Errorf("%s rejected before generation or lease: %v", frame.Kind, err)
		}
		wire := terminalEventFrame(frame)
		got, err := terminalEventFromFrame(wire)
		if err != nil {
			t.Errorf("%s conversion failed: %v", frame.Kind, err)
			continue
		}
		if !reflect.DeepEqual(got, frame) {
			t.Errorf("%s conversion changed frame: got %+v want %+v", frame.Kind, got, frame)
		}
	}
}

func TestTerminalAttachResultBoundaries(t *testing.T) {
	for _, sequence := range []uint64{4, 5, 9} {
		frame := TerminalFrame{Kind: TerminalAttached, Correlation: 1, Sequence: sequence, Floor: 4, Head: 9, Status: TerminalResultOK}
		if err := frame.validate(); err != nil {
			t.Fatalf("attach success at sequence %d rejected: %v", sequence, err)
		}
	}
	for _, frame := range []TerminalFrame{
		{Kind: TerminalAttached, Correlation: 1, Sequence: 10, Floor: 4, Head: 9, Status: TerminalResultRejected},
		{Kind: TerminalAttached, Correlation: 1, Sequence: 10, Floor: 4, Head: 9, Status: TerminalResultOK},
	} {
		if err := frame.validate(); frame.Status == TerminalResultRejected && err != nil {
			t.Fatalf("attach rejection above head rejected: %v", err)
		} else if frame.Status == TerminalResultOK && err == nil {
			t.Fatalf("attach success above head accepted")
		}
	}
	for _, status := range []TerminalResultStatus{TerminalResultPartial, TerminalResultUncertain, ""} {
		frame := TerminalFrame{Kind: TerminalAttached, Correlation: 1, Sequence: 5, Floor: 4, Head: 9, Status: status}
		if err := frame.validate(); err == nil {
			t.Fatalf("attach status %q accepted", status)
		}
	}
}

func TestTerminalCreditIsAggregateAndResponseCommandsNeedCorrelation(t *testing.T) {
	if err := (TerminalCommand{Kind: TerminalCredit, Credit: 1}).validate(); err != nil {
		t.Fatalf("aggregate credit rejected: %v", err)
	}
	if err := (TerminalCommand{Kind: TerminalCredit, Correlation: 1, Credit: 1}).validate(); err == nil {
		t.Fatal("credit with response correlation accepted")
	}
	for _, kind := range []TerminalCommandKind{TerminalGenerationInstall, TerminalGenerationRevoke, TerminalAttach, TerminalInput, TerminalResize, TerminalHumanReply} {
		command := TerminalCommand{Kind: kind}
		switch kind {
		case TerminalGenerationInstall, TerminalGenerationRevoke:
			command.Generation = 1
		case TerminalAttach:
			command.Sequence = ^uint64(0)
		case TerminalInput:
			command.Generation, command.Sequence, command.Payload = 1, 1, []byte("x")
		case TerminalResize:
			command.Generation, command.Rows, command.Cols = 1, 24, 80
		case TerminalHumanReply:
			command.Payload = []byte("reply")
		}
		if err := command.validate(); err == nil {
			t.Fatalf("response command %q without correlation accepted", kind)
		}
		command.Correlation = 1
		if err := command.validate(); err != nil {
			t.Fatalf("response command %q with correlation rejected: %v", kind, err)
		}
	}
}

func TestTerminalResizeUsesPTYOwnerBounds(t *testing.T) {
	valid := TerminalCommand{Kind: TerminalResize, Correlation: 1, Generation: 1, Rows: maxPTYDimension, Cols: maxPTYDimension}
	if err := valid.validate(); err != nil {
		t.Fatalf("owner-bound resize rejected: %v", err)
	}
	for _, version := range []int{0, commandVersion - 1, commandVersion + 1, 99} {
		frame := terminalCommandFrame(TerminalCommand{Kind: TerminalCredit, Credit: 1})
		frame.Version = version
		if _, err := terminalCommandFromFrame(frame); !errors.Is(err, ErrIdentity) {
			t.Fatalf("command version %d accepted: %v", version, err)
		}
		event := terminalEventFrame(TerminalFrame{Kind: TerminalPTYEOF})
		event.Version = version
		if _, err := terminalEventFromFrame(event); !errors.Is(err, ErrIdentity) {
			t.Fatalf("event version %d accepted: %v", version, err)
		}
	}
}

func TestTerminalConversionsRejectLifecycleAndCrossUnionFields(t *testing.T) {
	commandBase := terminalCommandFrame(TerminalCommand{Kind: TerminalCredit, Credit: 1})
	commandContaminations := []struct {
		name   string
		mutate func(*attemptFrame)
	}{
		{"stage", func(f *attemptFrame) { f.Stage = StageProvider }},
		{"identity", func(f *attemptFrame) { f.Identity = Identity{PID: 2, PGID: 2, Birth: Birth{Seconds: 1}} }},
		{"file_identity", func(f *attemptFrame) { f.FileIdentity = &FileIdentity{Device: 1, Inode: 1} }},
		{"digest", func(f *attemptFrame) { f.Digest = "digest" }},
		{"generation", func(f *attemptFrame) { f.Generation = 1 }},
		{"sequence", func(f *attemptFrame) { f.Sequence = 1 }},
		{"start", func(f *attemptFrame) { f.Start = 1 }},
		{"end", func(f *attemptFrame) { f.End = 1 }},
		{"floor", func(f *attemptFrame) { f.Floor = 1 }},
		{"head", func(f *attemptFrame) { f.Head = 1 }},
		{"count", func(f *attemptFrame) { f.Count = 1 }},
		{"rows", func(f *attemptFrame) { f.Rows = 1 }},
		{"cols", func(f *attemptFrame) { f.Cols = 1 }},
		{"status", func(f *attemptFrame) { f.Status = "ok" }},
		{"submit", func(f *attemptFrame) { f.Submit = true }},
		{"payload", func(f *attemptFrame) { f.Payload = []byte("unexpected") }},
	}
	for _, test := range commandContaminations {
		t.Run("command/"+test.name, func(t *testing.T) {
			frame := commandBase
			test.mutate(&frame)
			if _, err := terminalCommandFromFrame(frame); err == nil {
				t.Fatalf("accepted contaminated command frame %+v", frame)
			}
		})
	}

	eventBase := terminalEventFrame(TerminalFrame{Kind: TerminalPTYEOF})
	eventContaminations := []struct {
		name   string
		mutate func(*attemptFrame)
	}{
		{"stage", func(f *attemptFrame) { f.Stage = StageProvider }},
		{"identity", func(f *attemptFrame) { f.Identity = Identity{PID: 2, PGID: 2, Birth: Birth{Seconds: 1}} }},
		{"file_identity", func(f *attemptFrame) { f.FileIdentity = &FileIdentity{Device: 1, Inode: 1} }},
		{"digest", func(f *attemptFrame) { f.Digest = "digest" }},
		{"correlation", func(f *attemptFrame) { f.Correlation = 1 }},
		{"sequence", func(f *attemptFrame) { f.Sequence = 1 }},
		{"start", func(f *attemptFrame) { f.Start = 1 }},
		{"end", func(f *attemptFrame) { f.End = 1 }},
		{"floor", func(f *attemptFrame) { f.Floor = 1 }},
		{"head", func(f *attemptFrame) { f.Head = 1 }},
		{"count", func(f *attemptFrame) { f.Count = 1 }},
		{"rows", func(f *attemptFrame) { f.Rows = 1 }},
		{"cols", func(f *attemptFrame) { f.Cols = 1 }},
		{"credit", func(f *attemptFrame) { f.Credit = 1 }},
		{"submit", func(f *attemptFrame) { f.Submit = true }},
		{"status", func(f *attemptFrame) { f.Status = "ok" }},
		{"payload", func(f *attemptFrame) { f.Payload = []byte("unexpected") }},
	}
	for _, test := range eventContaminations {
		t.Run("event/"+test.name, func(t *testing.T) {
			frame := eventBase
			test.mutate(&frame)
			if _, err := terminalEventFromFrame(frame); err == nil {
				t.Fatalf("accepted contaminated event frame %+v", frame)
			}
		})
	}
}

func TestCurrentExecAcknowledgementRejectsEveryForeignField(t *testing.T) {
	current := attemptFrame{Version: commandVersion, Kind: "current-exec-check-ack"}
	mutations := []struct {
		name   string
		mutate func(*attemptFrame)
	}{
		{"stage", func(f *attemptFrame) { f.Stage = StageProvider }},
		{"identity", func(f *attemptFrame) { f.Identity = Identity{PID: 2, PGID: 2, Birth: Birth{Seconds: 1}} }},
		{"file_identity", func(f *attemptFrame) { f.FileIdentity = &FileIdentity{Device: 1, Inode: 1} }},
		{"digest", func(f *attemptFrame) { f.Digest = "other" }},
		{"correlation", func(f *attemptFrame) { f.Correlation = 1 }},
		{"generation", func(f *attemptFrame) { f.Generation = 1 }},
		{"sequence", func(f *attemptFrame) { f.Sequence = 1 }},
		{"start", func(f *attemptFrame) { f.Start = 1 }},
		{"end", func(f *attemptFrame) { f.End = 1 }},
		{"floor", func(f *attemptFrame) { f.Floor = 1 }},
		{"head", func(f *attemptFrame) { f.Head = 1 }},
		{"count", func(f *attemptFrame) { f.Count = 1 }},
		{"rows", func(f *attemptFrame) { f.Rows = 1 }},
		{"cols", func(f *attemptFrame) { f.Cols = 1 }},
		{"credit", func(f *attemptFrame) { f.Credit = 1 }},
		{"status", func(f *attemptFrame) { f.Status = "ok" }},
	}
	for _, test := range mutations {
		t.Run("current/"+test.name, func(t *testing.T) {
			frame := current
			test.mutate(&frame)
			if validCurrentExecCheckAck(frame) {
				t.Fatalf("accepted contaminated current-exec ACK %+v", frame)
			}
		})
	}
	if !validCurrentExecCheckAck(current) {
		t.Fatal("valid ACK baseline rejected")
	}
}
