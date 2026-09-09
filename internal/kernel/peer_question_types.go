package kernel

import (
	"fmt"
)

// 2 KiB keeps a complete escaped question and answer below the 64 KiB control
// frame beside a paged task-detail text chunk.
const MaxPeerQuestionBytes = 2048

type PeerDeliveryState uint8

const (
	PeerDeliveryPending PeerDeliveryState = iota + 1
	PeerDeliveryDelivered
	PeerDeliveryUnknown
)

func (state PeerDeliveryState) String() string {
	switch state {
	case PeerDeliveryPending:
		return "pending"
	case PeerDeliveryDelivered:
		return "delivered"
	case PeerDeliveryUnknown:
		return "unknown"
	}
	return ""
}

func parsePeerDeliveryState(value string) (PeerDeliveryState, error) {
	for state := PeerDeliveryPending; state <= PeerDeliveryUnknown; state++ {
		if state.String() == value {
			return state, nil
		}
	}
	return 0, corruptControl("peer delivery state", value)
}

// PeerQuestion is immutable question/answer text plus exactly two delivery
// receipts. Availability and stale status are derived from the linked tasks.
type PeerQuestion struct {
	ID                     PeerQuestionID
	ProjectID              ProjectID
	SourceTaskID           TaskID
	TargetTaskID           TaskID
	IdempotencyKey         [IDBytes]byte `json:"-"`
	Question               string
	AnswerIdempotencyKey   *[IDBytes]byte `json:"-"`
	Answer                 string
	RecipientDeliveryID    *PeerDeliveryID `json:"-"`
	RecipientDeliveryState PeerDeliveryState
	AnswerDeliveryID       *PeerDeliveryID `json:"-"`
	AnswerDeliveryState    PeerDeliveryState
	Revision               Revision
	CreatedAt              UnixMillis
	UpdatedAt              UnixMillis
}

type NewPeerQuestion struct {
	TargetTaskID   TaskID
	IdempotencyKey [IDBytes]byte
	Question       string
}

func (input NewPeerQuestion) valid() error {
	if input.TargetTaskID.zero() || input.IdempotencyKey == [IDBytes]byte{} || !utf8TextWithin(input.Question, 1, MaxPeerQuestionBytes) {
		return fmt.Errorf("%w: invalid peer question", ErrInvalidValue)
	}
	return nil
}

type PeerAnswer struct {
	QuestionID     PeerQuestionID
	Expected       Revision
	IdempotencyKey [IDBytes]byte
	Answer         string
}

func (input PeerAnswer) valid() error {
	if input.QuestionID.zero() || input.Expected.Int64() < 1 || input.IdempotencyKey == [IDBytes]byte{} || !utf8TextWithin(input.Answer, 1, MaxPeerQuestionBytes) {
		return fmt.Errorf("%w: invalid peer answer", ErrInvalidValue)
	}
	return nil
}

type PeerDelivery struct {
	QuestionID PeerQuestionID
	RunID      RunID
	Provider   Provider
	DeliveryID PeerDeliveryID
	Revision   Revision
	Payload    []byte `json:"-"`
	Answer     bool
}
