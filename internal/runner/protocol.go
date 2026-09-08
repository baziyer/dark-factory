package runner

import (
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
)

const maxAttemptReportBytes = 32 << 10

type gateConfig struct {
	Version        int                   `json:"version"`
	Target         launchCommitment      `json:"target"`
	LeaseDirectory fileCommitment        `json:"lease_directory"`
	Lifetime       descriptorCommitment  `json:"lifetime"`
	MarkerName     string                `json:"marker_name"`
	KeepDirectory  bool                  `json:"keep_directory"`
	Control        *descriptorCommitment `json:"control,omitempty"`
	TestFinalCheck bool                  `json:"test_final_check,omitempty"`
	PTY            bool                  `json:"pty,omitempty"`
}

type gateFrame struct {
	Kind     string        `json:"kind"`
	Identity Identity      `json:"identity,omitempty"`
	Marker   *FileIdentity `json:"marker,omitempty"`
	Error    string        `json:"error,omitempty"`
}

type attemptConfig struct {
	Version      int              `json:"version"`
	AttemptID    string           `json:"attempt_id"`
	Wrapper      launchCommitment `json:"wrapper"`
	MarkerName   string           `json:"marker_name"`
	ResultName   string           `json:"result_name"`
	ResultProof  string           `json:"result_proof"`
	StartupInput []byte           `json:"startup_input,omitempty"`
}

// String, GoString and Format keep the hex result proof out of every
// diagnostic rendering; only the JSON control frame may carry it.
func (cfg attemptConfig) String() string {
	return fmt.Sprintf("runner.attemptConfig{version:%d attempt_id:%q marker_name:%q result_name:%q result_proof:[redacted]}", cfg.Version, cfg.AttemptID, cfg.MarkerName, cfg.ResultName)
}
func (cfg attemptConfig) GoString() string { return cfg.String() }
func (cfg attemptConfig) Format(state fmt.State, _ rune) {
	_, _ = io.WriteString(state, cfg.String())
}

type attemptFrame struct {
	Version      int           `json:"version"`
	Kind         string        `json:"kind"`
	Stage        AttemptStage  `json:"stage,omitempty"`
	Identity     Identity      `json:"identity,omitempty"`
	Payload      []byte        `json:"payload,omitempty"`
	FileIdentity *FileIdentity `json:"file_identity,omitempty"`
	Digest       string        `json:"digest,omitempty"`
	Correlation  uint64        `json:"correlation,omitempty"`
	Generation   uint64        `json:"generation,omitempty"`
	Sequence     uint64        `json:"sequence,omitempty"`
	Start        uint64        `json:"start,omitempty"`
	End          uint64        `json:"end,omitempty"`
	Floor        uint64        `json:"floor,omitempty"`
	Head         uint64        `json:"head,omitempty"`
	Count        uint32        `json:"count,omitempty"`
	Rows         uint16        `json:"rows,omitempty"`
	Cols         uint16        `json:"cols,omitempty"`
	Credit       uint32        `json:"credit,omitempty"`
	Status       string        `json:"status,omitempty"`
}

func terminalCommandFrame(command TerminalCommand) attemptFrame {
	return attemptFrame{
		Version: commandVersion, Kind: string(command.Kind), Correlation: command.Correlation,
		Generation: command.Generation, Sequence: command.Sequence, Credit: command.Credit,
		Rows: command.Rows, Cols: command.Cols, Payload: append([]byte(nil), command.Payload...),
	}
}

func terminalCommandFromFrame(frame attemptFrame) (TerminalCommand, error) {
	if frame.Version != commandVersion {
		return TerminalCommand{}, ErrIdentity
	}
	if !validTerminalEnvelope(frame, true) {
		return TerminalCommand{}, ErrState
	}
	command := TerminalCommand{
		Kind: TerminalCommandKind(frame.Kind), Correlation: frame.Correlation,
		Generation: frame.Generation, Sequence: frame.Sequence, Credit: frame.Credit,
		Rows: frame.Rows, Cols: frame.Cols, Payload: append([]byte(nil), frame.Payload...),
	}
	if err := command.validate(); err != nil {
		return TerminalCommand{}, err
	}
	return command, nil
}

func terminalEventFromFrame(frame attemptFrame) (TerminalFrame, error) {
	if frame.Version != commandVersion {
		return TerminalFrame{}, ErrIdentity
	}
	if !validTerminalEnvelope(frame, false) {
		return TerminalFrame{}, ErrState
	}
	event := TerminalFrame{
		Kind: TerminalEventKind(frame.Kind), Correlation: frame.Correlation,
		Generation: frame.Generation, Sequence: frame.Sequence,
		Start: frame.Start, End: frame.End, Floor: frame.Floor, Head: frame.Head,
		Count: frame.Count, Rows: frame.Rows, Cols: frame.Cols,
		Status: TerminalResultStatus(frame.Status), Payload: append([]byte(nil), frame.Payload...),
	}
	if err := event.validate(); err != nil {
		return TerminalFrame{}, err
	}
	return event, nil
}

func terminalEventFrame(event TerminalFrame) attemptFrame {
	return attemptFrame{
		Version: commandVersion, Kind: string(event.Kind), Correlation: event.Correlation,
		Generation: event.Generation, Sequence: event.Sequence, Start: event.Start,
		End: event.End, Floor: event.Floor, Head: event.Head, Count: event.Count,
		Rows: event.Rows, Cols: event.Cols, Status: string(event.Status),
		Payload: append([]byte(nil), event.Payload...),
	}
}

func noTerminalFields(frame attemptFrame) bool {
	return frame.Correlation == 0 && frame.Generation == 0 && frame.Sequence == 0 &&
		frame.Start == 0 && frame.End == 0 && frame.Floor == 0 && frame.Head == 0 &&
		frame.Count == 0 && frame.Rows == 0 && frame.Cols == 0 && frame.Credit == 0 &&
		frame.Status == ""
}

func noLegacyFields(frame attemptFrame) bool {
	return frame.Stage == "" && frame.Identity == (Identity{}) && frame.FileIdentity == nil && frame.Digest == ""
}

// validTerminalEnvelope is the single shared boundary between the legacy
// attempt union and the terminal union. Per-kind validation below still owns
// operation-specific fields; this predicate rejects fields that belong only
// to the other direction or to lifecycle messages.
func validTerminalEnvelope(frame attemptFrame, command bool) bool {
	if !noLegacyFields(frame) {
		return false
	}
	if command {
		return frame.Start == 0 && frame.End == 0 && frame.Floor == 0 && frame.Head == 0 && frame.Count == 0 && frame.Status == ""
	}
	return frame.Credit == 0
}

func validCurrentExecCheckAck(frame attemptFrame) bool {
	return frame.Version == commandVersion && frame.Kind == "current-exec-check-ack" &&
		noLegacyFields(frame) && noTerminalFields(frame) && len(frame.Payload) == 0
}

func writeFrame(w io.Writer, value any, limit int) error {
	if w == nil {
		return ErrIdentity
	}
	body, err := json.Marshal(value)
	if err != nil {
		return err
	}
	if len(body) == 0 || len(body) > limit {
		return fmt.Errorf("runner: frame size %d", len(body))
	}
	var header [4]byte
	binary.BigEndian.PutUint32(header[:], uint32(len(body)))
	if err := writeFully(w, header[:]); err != nil {
		return fmt.Errorf("runner: frame header: %w", err)
	}
	if err := writeFully(w, body); err != nil {
		return fmt.Errorf("runner: frame body (%T, %d bytes): %w", value, len(body), err)
	}
	return nil
}

// writeFully is deliberately small and synchronous. Framing a message is an
// all-or-nothing operation from the caller's point of view: returning after a
// short write would leave the peer with a valid-looking prefix and make the
// next lifecycle transition ambiguous. A writer that makes no progress is
// treated as failed rather than spun on forever; socket callers install their
// own write deadline before entering this helper.
func writeFully(w io.Writer, p []byte) error {
	for len(p) != 0 {
		n, err := w.Write(p)
		if n < 0 || n > len(p) {
			return fmt.Errorf("runner: invalid write count %d", n)
		}
		if n != 0 {
			p = p[n:]
		}
		if err != nil {
			return err
		}
		if n == 0 {
			return io.ErrNoProgress
		}
	}
	return nil
}

func readFrame(r io.Reader, dst any, limit int) error {
	var header [4]byte
	if _, err := io.ReadFull(r, header[:]); err != nil {
		return err
	}
	n := int(binary.BigEndian.Uint32(header[:]))
	if n <= 0 || n > limit {
		return fmt.Errorf("runner: invalid frame size %d", n)
	}
	body := make([]byte, n)
	if _, err := io.ReadFull(r, body); err != nil {
		return err
	}
	// io.EOF and io.ErrUnexpectedEOF out of this function mean the stream
	// ended, and only that: decodeFrameBody renames the decoder's own two
	// EOF answers so a malformed body from a live peer cannot impersonate a
	// closed one.
	return decodeFrameBody(body, dst)
}

func errorsNewTrailing(err error) error { return fmt.Errorf("runner: trailing frame data: %v", err) }
