package runner

import (
	"fmt"
	"time"
)

// AttemptStage is the closed sequence of daemon-authorized releases. Selection
// is the inner gate activation; every later release crosses the already-running
// wrapper control capability.
type AttemptStage string

const (
	StageSelection   AttemptStage = "selection"
	StagePreparation AttemptStage = "preparation"
	StagePopulation  AttemptStage = "population"
	StageProvider    AttemptStage = "provider"
)

// AttemptSpec freezes the source wrapper launch before the attempt runner is
// released. Wrapper must not contain a caller-supplied control descriptor: the
// attempt runner creates and owns that one fixed capability itself.
type AttemptSpec struct {
	AttemptID   string
	Wrapper     *LaunchSpec
	MarkerName  string
	ResultName  string
	ResultProof ResultProof
	// StartupInput is the optional exact terminal-delivered provider prompt.
	// The attempt runner writes it once after provider exec and before
	// TerminalReady. Codex instead reads its task through the attempt API.
	StartupInput []byte
}

type AttemptEventKind string

const (
	AttemptInnerReady    AttemptEventKind = "inner-ready"
	AttemptCheckpoint    AttemptEventKind = "checkpoint"
	AttemptResultReady   AttemptEventKind = "attempt-result-ready"
	AttemptTerminalFrame AttemptEventKind = "terminal-frame"
)

// TerminalCommandKind and TerminalEventKind are intentionally closed unions.
// The private runner socket is a capability, not a general-purpose RPC bus;
// adding a wire operation requires adding its validation here first.
type TerminalCommandKind string

const (
	TerminalGenerationInstall TerminalCommandKind = "terminal-generation-install"
	TerminalGenerationRevoke  TerminalCommandKind = "terminal-generation-revoke"
	TerminalAttach            TerminalCommandKind = "terminal-attach"
	TerminalCredit            TerminalCommandKind = "terminal-credit"
	TerminalInput             TerminalCommandKind = "terminal-input"
	TerminalResize            TerminalCommandKind = "terminal-resize"
	TerminalHumanReply        TerminalCommandKind = "terminal-human-reply"
)

type TerminalEventKind string

const (
	TerminalGenerationResult TerminalEventKind = "terminal-generation-result"
	TerminalInputResult      TerminalEventKind = "terminal-input-result"
	TerminalResizeResult     TerminalEventKind = "terminal-resize-result"
	TerminalAttached         TerminalEventKind = "terminal-attached"
	TerminalOutput           TerminalEventKind = "terminal-output"
	TerminalReset            TerminalEventKind = "terminal-reset"
	TerminalReady            TerminalEventKind = "terminal-ready"
	TerminalPTYEOF           TerminalEventKind = "terminal-pty-eof"
	TerminalHumanReplyResult TerminalEventKind = "terminal-human-reply-result"
)

type TerminalResultStatus string

const (
	TerminalResultOK        TerminalResultStatus = "ok"
	TerminalResultRejected  TerminalResultStatus = "rejected"
	TerminalResultPartial   TerminalResultStatus = "partial"
	TerminalResultUncertain TerminalResultStatus = "uncertain"
)

const (
	// Terminal frames are JSON control messages on the private socket. Payload
	// bytes are base64 encoded by encoding/json, so keep the decoded byte bound
	// below its independent 8 KiB browser/terminal contract. Provider task bytes
	// never enter this framed control path.
	maxTerminalFramePayload = 8 << 10
	maxTerminalCredit       = 1 << 20
	maxTerminalDimension    = 4096
	maxTerminalCorrelation  = ^uint64(0) >> 1
	maxPTYDimension         = maxTerminalDimension
)

// TerminalCommand is the only supported daemon-to-runner terminal command.
// Zero values are invalid; SendTerminalCommand performs the closed-union
// validation before writing anything to the capability socket.
type TerminalCommand struct {
	Kind        TerminalCommandKind
	Correlation uint64
	Generation  uint64
	Sequence    uint64
	Credit      uint32
	Rows        uint16
	Cols        uint16
	Submit      bool
	Payload     []byte
}

// TerminalFrame is a runner-to-daemon terminal event returned by AttemptEvent.
// The same bounded fields are used for every event kind, with strict
// per-kind validation preventing accidental cross-operation interpretation.
type TerminalFrame struct {
	Kind        TerminalEventKind
	Correlation uint64
	Generation  uint64
	Sequence    uint64
	Start       uint64
	End         uint64
	Floor       uint64
	Head        uint64
	Count       uint32
	Rows        uint16
	Cols        uint16
	Status      TerminalResultStatus
	Payload     []byte
}

func (c TerminalCommand) validate() error {
	switch c.Kind {
	case TerminalGenerationInstall, TerminalGenerationRevoke:
		if !validTerminalCorrelation(c.Correlation) {
			return fmt.Errorf("runner: terminal command correlation is invalid")
		}
		if c.Generation == 0 || c.Sequence != 0 || c.Credit != 0 || c.Rows != 0 || c.Cols != 0 || len(c.Payload) != 0 || c.Submit {
			return ErrState
		}
	case TerminalAttach:
		if !validTerminalCorrelation(c.Correlation) || c.Generation != 0 || c.Credit != 0 || c.Rows != 0 || c.Cols != 0 || len(c.Payload) != 0 || c.Submit {
			return ErrState
		}
	case TerminalCredit:
		if c.Correlation != 0 || c.Generation != 0 || c.Sequence != 0 || c.Credit == 0 || c.Credit > maxTerminalCredit || c.Rows != 0 || c.Cols != 0 || len(c.Payload) != 0 || c.Submit {
			return ErrState
		}
	case TerminalInput:
		if !validTerminalCorrelation(c.Correlation) || c.Generation == 0 || c.Sequence == 0 || len(c.Payload) == 0 || len(c.Payload) > maxTerminalFramePayload || c.Credit != 0 || c.Rows != 0 || c.Cols != 0 || c.Submit {
			return ErrState
		}
	case TerminalHumanReply:
		// A human reply is daemon-authorized separately from the browser's
		// terminal lease. It deliberately carries no generation or sequence:
		// the daemon has already resolved the exact HumanRequest/run before
		// sending this one-shot payload to its owner.
		if !validTerminalCorrelation(c.Correlation) || c.Generation != 0 || c.Sequence != 0 || c.Credit != 0 || c.Rows != 0 || c.Cols != 0 || len(c.Payload) == 0 || len(c.Payload) > maxTerminalFramePayload {
			return ErrState
		}
	case TerminalResize:
		if !validTerminalCorrelation(c.Correlation) || c.Generation == 0 || c.Rows == 0 || c.Rows > maxTerminalDimension || c.Cols == 0 || c.Cols > maxTerminalDimension || c.Sequence != 0 || c.Credit != 0 || len(c.Payload) != 0 || c.Submit {
			return ErrState
		}
	default:
		return ErrState
	}
	return nil
}

func (f TerminalFrame) validate() error {
	switch f.Kind {
	case TerminalGenerationResult:
		if f.Correlation == 0 || f.Correlation > maxTerminalCorrelation || f.Generation == 0 || !validTerminalResult(f.Status) || f.Sequence != 0 || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Count != 0 || f.Rows != 0 || f.Cols != 0 || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalInputResult:
		if f.Correlation == 0 || f.Correlation > maxTerminalCorrelation || f.Generation == 0 || f.Sequence == 0 || !validTerminalResult(f.Status) || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Rows != 0 || f.Cols != 0 || f.Count > maxTerminalFramePayload || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalHumanReplyResult:
		if f.Correlation == 0 || f.Correlation > maxTerminalCorrelation || f.Generation != 0 || f.Sequence != 0 || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Rows != 0 || f.Cols != 0 || !validTerminalResult(f.Status) || f.Count > maxTerminalFramePayload || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalResizeResult:
		if f.Correlation == 0 || f.Correlation > maxTerminalCorrelation || f.Generation == 0 || !validTerminalResult(f.Status) || f.Sequence != 0 || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Count != 0 || f.Rows == 0 || f.Rows > maxTerminalDimension || f.Cols == 0 || f.Cols > maxTerminalDimension || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalAttached:
		if !validTerminalCorrelation(f.Correlation) || f.Generation != 0 || f.Floor > f.Head || f.Rows != 0 || f.Cols != 0 || f.Count != 0 || f.Start != 0 || f.End != 0 || len(f.Payload) != 0 {
			return ErrState
		}
		switch f.Status {
		case TerminalResultOK:
			if f.Sequence < f.Floor || f.Sequence > f.Head {
				return ErrState
			}
		case TerminalResultRejected:
			if f.Sequence <= f.Head {
				return ErrState
			}
		default:
			return ErrState
		}
	case TerminalOutput:
		if f.Correlation > maxTerminalCorrelation || f.Generation != 0 || f.Start >= f.End || f.End-f.Start != uint64(len(f.Payload)) || len(f.Payload) == 0 || len(f.Payload) > maxTerminalFramePayload || f.Sequence != 0 || f.Floor != 0 || f.Head != 0 || f.Count != 0 || f.Rows != 0 || f.Cols != 0 || f.Status != "" {
			return ErrState
		}
	case TerminalReset:
		if f.Correlation > maxTerminalCorrelation || f.Generation != 0 || f.Floor > f.Head || f.Start != 0 || f.End != 0 || f.Sequence != 0 || f.Count != 0 || f.Rows != 0 || f.Cols != 0 || f.Status != "" || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalReady:
		if f.Correlation != 0 || f.Generation != 0 || f.Sequence != 0 || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Count != 0 || f.Rows != 0 || f.Cols != 0 || f.Status != "" || len(f.Payload) != 0 {
			return ErrState
		}
	case TerminalPTYEOF:
		if f.Correlation != 0 || f.Generation != 0 || f.Sequence != 0 || f.Start != 0 || f.End != 0 || f.Floor != 0 || f.Head != 0 || f.Count != 0 || f.Rows != 0 || f.Cols != 0 || f.Status != "" || len(f.Payload) != 0 {
			return ErrState
		}
	default:
		return ErrState
	}
	return nil
}

func validTerminalCorrelation(value uint64) bool {
	return value != 0 && value <= maxTerminalCorrelation
}

func validTerminalResult(value TerminalResultStatus) bool {
	switch value {
	case TerminalResultOK, TerminalResultRejected, TerminalResultPartial, TerminalResultUncertain:
		return true
	default:
		return false
	}
}

type AttemptEvent struct {
	Kind     AttemptEventKind
	Stage    AttemptStage
	Identity Identity
	Payload  []byte
	Result   *AttemptResultNotice
	Frame    *TerminalFrame
}

const attemptControlTimeout = 4 * time.Second

const commandVersion = 1
