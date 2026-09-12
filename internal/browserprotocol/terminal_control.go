package browserprotocol

import (
	"fmt"
	"unicode/utf8"
)

func EncodeHumanRequestReply(id string, v HumanRequestReply) ([]byte, error) {
	return encodeControl(TypeHumanRequestReply, id, v)
}
func EncodeHumanRequestReplyResult(id string, v HumanRequestReplyResult) ([]byte, error) {
	return encodeControl(TypeHumanRequestReplyResult, id, v)
}
func EncodeHumanRequestCancelRun(id string, v HumanRequestCancelRun) ([]byte, error) {
	return encodeControl(TypeHumanRequestCancelRun, id, v)
}
func EncodeHumanRequestCancelRunResult(id string, v HumanRequestCancelRunResult) ([]byte, error) {
	return encodeControl(TypeHumanRequestCancelRunResult, id, v)
}
func EncodeTerminalAttach(id string, v TerminalAttach) ([]byte, error) {
	return encodeControl(TypeTerminalAttach, id, v)
}
func EncodeTerminalTargetGet(id string, v TerminalTargetGet) ([]byte, error) {
	return encodeControl(TypeTerminalTargetGet, id, v)
}
func EncodeTerminalTarget(id string, v TerminalTarget) ([]byte, error) {
	return encodeControl(TypeTerminalTarget, id, v)
}
func EncodeTerminalAttached(id string, v TerminalAttached) ([]byte, error) {
	return encodeControl(TypeTerminalAttached, id, v)
}
func EncodeTerminalAck(v TerminalAck) ([]byte, error) { return encodeControl(TypeTerminalAck, "", v) }
func EncodeTerminalLeaseAcquire(id string, v TerminalLeaseAcquire) ([]byte, error) {
	return encodeControl(TypeTerminalLeaseAcquire, id, v)
}
func EncodeTerminalLeaseRenew(id string, v TerminalLeaseRenew) ([]byte, error) {
	return encodeControl(TypeTerminalLeaseRenew, id, v)
}
func EncodeTerminalLeaseRelease(id string, v TerminalLeaseRelease) ([]byte, error) {
	return encodeControl(TypeTerminalLeaseRelease, id, v)
}
func EncodeTerminalLeaseResult(id string, v TerminalLeaseResult) ([]byte, error) {
	return encodeControl(TypeTerminalLeaseResult, id, v)
}
func EncodeTerminalResized(id string, v TerminalResized) ([]byte, error) {
	return encodeControl(TypeTerminalResized, id, v)
}
func EncodeTerminalDetach(id string, v TerminalDetach) ([]byte, error) {
	return encodeControl(TypeTerminalDetach, id, v)
}
func EncodeTerminalDetached(id string, v TerminalDetached) ([]byte, error) {
	return encodeControl(TypeTerminalDetached, id, v)
}
func EncodeTerminalInputResult(id string, v TerminalInputResult) ([]byte, error) {
	return encodeControl(TypeTerminalInputResult, id, v)
}
func EncodeTerminalEOF(id string, v TerminalEOF) ([]byte, error) {
	return encodeControl(TypeTerminalEOF, id, v)
}
func EncodeTerminalExit(id string, v TerminalExit) ([]byte, error) {
	return encodeControl(TypeTerminalExit, id, v)
}
func EncodeTerminalReset(id string, v TerminalReset) ([]byte, error) {
	return encodeControl(TypeTerminalReset, id, v)
}

const (
	MaxTerminalUnackedBytes             = 65536
	TerminalAckTimeoutMS                = 10000
	TerminalLeaseRenewIntervalMS        = 10000
	MaxTerminalRows              uint16 = 4096
	MaxTerminalCols              uint16 = 4096
	MaxJSONInteger               int64  = 1<<53 - 1
)

type HumanRequestReply struct {
	RequestID        string  `json:"request_id"`
	ExpectedRevision Decimal `json:"expected_revision"`
	Reply            string  `json:"reply"`
}
type HumanRequestReplyResult struct {
	RequestID string  `json:"request_id"`
	Revision  Decimal `json:"revision"`
	Status    string  `json:"status"`
}
type HumanRequestCancelRun struct {
	RequestID               string  `json:"request_id"`
	ExpectedRequestRevision Decimal `json:"expected_request_revision"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
}
type HumanRequestCancelRunResult struct {
	RunID           string  `json:"run_id"`
	RunRevision     Decimal `json:"run_revision"`
	RequestID       string  `json:"request_id"`
	RequestRevision Decimal `json:"request_revision"`
}
type TerminalAttach struct {
	RunID                   string  `json:"run_id"`
	SessionID               string  `json:"session_id"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
	ExpectedSessionRevision Decimal `json:"expected_session_revision"`
	AfterSequence           Decimal `json:"after_sequence"`
}
type TerminalTargetGet struct {
	AgentID               string  `json:"agent_id"`
	ExpectedAgentRevision Decimal `json:"expected_agent_revision"`
	ExpectedHead          Decimal `json:"expected_head"`
}
type TerminalTargetDescriptor struct {
	RunID           string  `json:"run_id"`
	SessionID       string  `json:"session_id"`
	RunRevision     Decimal `json:"run_revision"`
	SessionRevision Decimal `json:"session_revision"`
}
type TerminalTarget struct {
	AgentID       string                    `json:"agent_id"`
	AgentRevision Decimal                   `json:"agent_revision"`
	Head          Decimal                   `json:"head"`
	Target        *TerminalTargetDescriptor `json:"target"`
}
type TerminalAttached struct {
	SessionID            string  `json:"session_id"`
	Floor                Decimal `json:"floor"`
	Head                 Decimal `json:"head"`
	AcknowledgedSequence Decimal `json:"acknowledged_sequence"`
	MaxUnackedBytes      Decimal `json:"max_unacked_bytes"`
}
type TerminalAck struct {
	SessionID    string  `json:"session_id"`
	NextSequence Decimal `json:"next_sequence"`
}
type TerminalLeaseAcquire struct {
	RunID                   string  `json:"run_id"`
	SessionID               string  `json:"session_id"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
	ExpectedSessionRevision Decimal `json:"expected_session_revision"`
}
type TerminalLeaseRenew struct {
	RunID                   string  `json:"run_id"`
	SessionID               string  `json:"session_id"`
	Generation              Decimal `json:"generation"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
	ExpectedSessionRevision Decimal `json:"expected_session_revision"`
}
type TerminalLeaseRelease struct {
	RunID                   string  `json:"run_id"`
	SessionID               string  `json:"session_id"`
	Generation              Decimal `json:"generation"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
	ExpectedSessionRevision Decimal `json:"expected_session_revision"`
}
type TerminalLeaseResult struct {
	Operation         string   `json:"operation"`
	RunID             string   `json:"run_id"`
	SessionID         string   `json:"session_id"`
	Generation        Decimal  `json:"generation"`
	ExpiresAtMS       *Decimal `json:"expires_at_ms,omitempty"`
	LastInputSequence Decimal  `json:"last_input_sequence"`
	RunRevision       Decimal  `json:"run_revision"`
	SessionRevision   Decimal  `json:"session_revision"`
}
type TerminalResize struct {
	RunID                   string  `json:"run_id"`
	SessionID               string  `json:"session_id"`
	Generation              Decimal `json:"generation"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
	ExpectedSessionRevision Decimal `json:"expected_session_revision"`
	Rows                    uint16  `json:"rows"`
	Cols                    uint16  `json:"cols"`
}
type TerminalResized struct {
	SessionID  string  `json:"session_id"`
	Generation Decimal `json:"generation"`
	Rows       uint16  `json:"rows"`
	Cols       uint16  `json:"cols"`
}
type TerminalDetach struct {
	SessionID string `json:"session_id"`
}
type TerminalDetached struct {
	SessionID string `json:"session_id"`
}
type TerminalInputResult struct {
	SessionID     string  `json:"session_id"`
	Generation    Decimal `json:"generation"`
	Sequence      Decimal `json:"sequence"`
	Status        string  `json:"status"`
	AcceptedBytes Decimal `json:"accepted_bytes"`
}
type TerminalEOF struct {
	SessionID string `json:"session_id"`
}
type TerminalExit struct {
	SessionID  string `json:"session_id"`
	ExitCode   int64  `json:"exit_code"`
	ExitSignal int64  `json:"exit_signal"`
	Aborted    bool   `json:"aborted"`
}
type TerminalReset struct {
	SessionID string  `json:"session_id"`
	Floor     Decimal `json:"floor"`
	Head      Decimal `json:"head"`
}

func validTerminalControl(kind MessageType, body any) error {
	body = indirect(body)
	bad := func() error { return fmt.Errorf("%w: invalid %s", ErrMalformed, kind) }
	id := func(s string) bool {
		value, err := fixedHex("id", s, 16)
		if err != nil {
			return false
		}
		for _, b := range value {
			if b != 0 {
				return true
			}
		}
		return false
	}
	pos := func(d Decimal) bool { return d > 0 }
	dims := func(n uint16) bool { return n >= 1 && n <= 4096 }
	switch v := body.(type) {
	case HumanRequestReply:
		if !id(v.RequestID) || v.ExpectedRevision == 0 || !utf8.ValidString(v.Reply) || v.Reply == "" || len([]byte(v.Reply)) > MaxHumanReplyBytes {
			return bad()
		}
	case HumanRequestReplyResult:
		if !id(v.RequestID) || v.Revision == 0 || (v.Status != "resolved" && v.Status != "delivery_unknown") {
			return bad()
		}
	case HumanRequestCancelRun:
		if !id(v.RequestID) || !pos(v.ExpectedRequestRevision) || !pos(v.ExpectedRunRevision) {
			return bad()
		}
	case HumanRequestCancelRunResult:
		if !id(v.RunID) || !id(v.RequestID) || !pos(v.RunRevision) || !pos(v.RequestRevision) {
			return bad()
		}
	case TerminalAttach:
		if !id(v.RunID) || !id(v.SessionID) || !pos(v.ExpectedRunRevision) || !pos(v.ExpectedSessionRevision) {
			return bad()
		}
	case TerminalTargetGet:
		if !id(v.AgentID) || !pos(v.ExpectedAgentRevision) {
			return bad()
		}
	case TerminalTarget:
		if !id(v.AgentID) || !pos(v.AgentRevision) {
			return bad()
		}
		if v.Target != nil && (!id(v.Target.RunID) || !id(v.Target.SessionID) || !pos(v.Target.RunRevision) || !pos(v.Target.SessionRevision)) {
			return bad()
		}
	case TerminalAttached:
		if !id(v.SessionID) || v.Floor > v.Head || v.AcknowledgedSequence > v.Head || v.MaxUnackedBytes != MaxTerminalUnackedBytes {
			return bad()
		}
	case TerminalAck:
		if !id(v.SessionID) || !pos(v.NextSequence) {
			return bad()
		}
	case TerminalLeaseAcquire:
		if !id(v.RunID) || !id(v.SessionID) || !pos(v.ExpectedRunRevision) || !pos(v.ExpectedSessionRevision) {
			return bad()
		}
	case TerminalLeaseRenew:
		if !id(v.RunID) || !id(v.SessionID) || !pos(v.Generation) || !pos(v.ExpectedRunRevision) || !pos(v.ExpectedSessionRevision) {
			return bad()
		}
	case TerminalLeaseRelease:
		if !id(v.RunID) || !id(v.SessionID) || !pos(v.Generation) || !pos(v.ExpectedRunRevision) || !pos(v.ExpectedSessionRevision) {
			return bad()
		}
	case TerminalLeaseResult:
		if (v.Operation != "acquired" && v.Operation != "renewed" && v.Operation != "released") || !id(v.RunID) || !id(v.SessionID) || !pos(v.Generation) || (v.Operation == "released" && v.ExpiresAtMS != nil) || (v.Operation != "released" && (v.ExpiresAtMS == nil || !pos(*v.ExpiresAtMS))) {
			return bad()
		}
	case TerminalResize:
		if !id(v.RunID) || !id(v.SessionID) || !pos(v.Generation) || !pos(v.ExpectedRunRevision) || !pos(v.ExpectedSessionRevision) || !dims(v.Rows) || !dims(v.Cols) {
			return bad()
		}
	case TerminalResized:
		if !id(v.SessionID) || !pos(v.Generation) || !dims(v.Rows) || !dims(v.Cols) {
			return bad()
		}
	case TerminalDetach:
		if !id(v.SessionID) {
			return bad()
		}
	case TerminalDetached:
		if !id(v.SessionID) {
			return bad()
		}
	case TerminalInputResult:
		if !id(v.SessionID) || !pos(v.Generation) || !pos(v.Sequence) || !validTerminalInputResult(v.Status, v.AcceptedBytes) {
			return bad()
		}
	case TerminalEOF:
		if !id(v.SessionID) {
			return bad()
		}
	case TerminalExit:
		if !id(v.SessionID) || !validTerminalExit(v.ExitCode, v.ExitSignal) {
			return bad()
		}
	case TerminalReset:
		if !id(v.SessionID) || v.Floor > v.Head {
			return bad()
		}
	default:
		return bad()
	}
	return nil
}

func validTerminalExit(code, signal int64) bool {
	if code < 0 || code > MaxJSONInteger || signal < 0 || signal > MaxJSONInteger {
		return false
	}
	return signal == 0 || code == 0
}

func validTerminalTargetDescriptor(value TerminalTargetDescriptor) error {
	if _, err := fixedHex("run_id", value.RunID, 16); err != nil || value.RunID == "00000000000000000000000000000000" {
		return fmt.Errorf("%w: terminal target run", ErrMalformed)
	}
	if _, err := fixedHex("session_id", value.SessionID, 16); err != nil || value.SessionID == "00000000000000000000000000000000" || value.RunRevision == 0 || value.SessionRevision == 0 {
		return fmt.Errorf("%w: terminal target session", ErrMalformed)
	}
	return nil
}

func validTerminalInputResult(status string, acceptedBytes Decimal) bool {
	switch status {
	case "accepted", "partial":
		return acceptedBytes >= 1 && acceptedBytes <= MaxTerminalPayload
	case "rejected", "uncertain":
		return acceptedBytes == 0
	default:
		return false
	}
}
