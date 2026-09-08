package kernel

import (
	"fmt"
	"unicode/utf8"
)

const (
	MaxHumanRequestQuestionBytes = 8192
	MaxHumanRequestReplyBytes    = 8192
	MaxOpenHumanRequests         = 1024
)

type HumanRequestKind uint8

const HumanRequestQuestion HumanRequestKind = 1

func (kind HumanRequestKind) String() string {
	if kind == HumanRequestQuestion {
		return "question"
	}
	return ""
}

func parseHumanRequestKind(value string) (HumanRequestKind, error) {
	if value != HumanRequestQuestion.String() {
		return 0, corruptControl("human request kind", value)
	}
	return HumanRequestQuestion, nil
}

type HumanRequestStatus uint8

const (
	HumanRequestOpen HumanRequestStatus = iota + 1
	HumanRequestDelivering
	HumanRequestDeliveryUnknown
	HumanRequestResolved
	HumanRequestStale
)

func (status HumanRequestStatus) String() string {
	switch status {
	case HumanRequestOpen:
		return "open"
	case HumanRequestDelivering:
		return "delivering"
	case HumanRequestDeliveryUnknown:
		return "delivery_unknown"
	case HumanRequestResolved:
		return "resolved"
	case HumanRequestStale:
		return "stale"
	default:
		return ""
	}
}

func parseHumanRequestStatus(value string) (HumanRequestStatus, error) {
	for status := HumanRequestOpen; status <= HumanRequestStale; status++ {
		if status.String() == value {
			return status, nil
		}
	}
	return 0, corruptControl("human request status", value)
}

type HumanRequestResolution uint8

const (
	HumanRequestResolutionReply HumanRequestResolution = iota + 1
	HumanRequestResolutionStale
	HumanRequestResolutionCancelRun
)

func (resolution HumanRequestResolution) String() string {
	switch resolution {
	case HumanRequestResolutionReply:
		return "reply"
	case HumanRequestResolutionStale:
		return "stale"
	case HumanRequestResolutionCancelRun:
		return "cancel_run"
	default:
		return ""
	}
}

func parseHumanRequestResolution(value string) (HumanRequestResolution, error) {
	for resolution := HumanRequestResolutionReply; resolution <= HumanRequestResolutionCancelRun; resolution++ {
		if resolution.String() == value {
			return resolution, nil
		}
	}
	return 0, corruptControl("human request resolution", value)
}

type HumanRequest struct {
	ID                HumanRequestID
	RunID             RunID
	IdempotencyKey    [IDBytes]byte `json:"-"`
	Kind              HumanRequestKind
	Status            HumanRequestStatus
	QuestionText      string                  `json:"-"`
	DeliveryID        *HumanRequestDeliveryID `json:"-"`
	DeliveryStartedAt *UnixMillis             `json:"-"`
	Resolution        *HumanRequestResolution `json:"-"`
	ClosedAt          *UnixMillis             `json:"-"`
	Revision          Revision
	CreatedAt         UnixMillis
	UpdatedAt         UnixMillis
}

// HumanRequestProjection is the bounded public list item. Canonical project,
// agent, and task display text is joined by the client from their summaries;
// provider-authored question and reply text are never present here.
type HumanRequestProjection struct {
	ID            HumanRequestID
	ProjectID     ProjectID
	AgentID       AgentID
	TaskID        TaskID
	CreatedAt     UnixMillis
	UpdatedAt     UnixMillis
	Revision      Revision
	Kind          HumanRequestKind
	Status        HumanRequestStatus
	ReplyMaxBytes uint32
	CanReply      bool
}

type HumanRequestDetail struct {
	ID             HumanRequestID
	Revision       Revision
	QuestionText   string
	CanReply       bool
	ReplyMaxBytes  uint32
	TerminalTarget *TerminalTarget
	CancelRun      *HumanRequestCancelRun
}

// HumanRequestCancelRun is the one concrete daemon-minted cancellation
// precondition. The fields remain private so a caller cannot substitute a run
// or revision after Store has projected the exact originating relationship.
type HumanRequestCancelRun struct {
	expectedRequestRevision Revision
	expectedRunRevision     Revision
}

func (cancelRun HumanRequestCancelRun) ExpectedRequestRevision() Revision {
	return cancelRun.expectedRequestRevision
}

func (cancelRun HumanRequestCancelRun) ExpectedRunRevision() Revision {
	return cancelRun.expectedRunRevision
}

type NewHumanQuestion struct {
	IdempotencyKey [IDBytes]byte
	QuestionText   string
}

type HumanDelivery struct {
	RequestID  HumanRequestID
	RunID      RunID
	Provider   Provider
	DeliveryID HumanRequestDeliveryID
	Revision   Revision
	Reply      []byte `json:"-"`
}

func (input NewHumanQuestion) valid() error {
	if input.IdempotencyKey == [IDBytes]byte{} {
		return fmt.Errorf("%w: zero human request idempotency key", ErrInvalidValue)
	}
	if !utf8TextWithin(input.QuestionText, 1, MaxHumanRequestQuestionBytes) {
		return fmt.Errorf("%w: invalid human request question", ErrInvalidValue)
	}
	return nil
}

func utf8TextWithin(value string, minimum, maximum int) bool {
	bytes := len([]byte(value))
	return utf8.ValidString(value) && bytes >= minimum && bytes <= maximum
}
