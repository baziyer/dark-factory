package browserprotocol

import "fmt"

// MaxTaskDetailTextOffset covers a whole task body or result. Text offsets
// count runes, and valid UTF-8 task text has no more runes than its byte cap.
const MaxTaskDetailTextOffset = 131072

// AgentControl targets one observed task/run pair. Replacement IDs belong to
// the caller so retrying a lost response cannot create a second successor.
type AgentControl struct {
	OperationID            string  `json:"operation_id"`
	TaskID                 string  `json:"task_id"`
	RunID                  string  `json:"run_id"`
	ExpectedTaskRevision   Decimal `json:"expected_task_revision"`
	ExpectedRunRevision    Decimal `json:"expected_run_revision"`
	Action                 string  `json:"action"`
	Instruction            string  `json:"instruction"`
	SuccessorTaskID        string  `json:"successor_task_id"`
	SuccessorIncarnationID string  `json:"successor_incarnation_id"`
}

type AgentControlResult struct {
	OperationID     string `json:"operation_id"`
	TaskID          string `json:"task_id"`
	RunID           string `json:"run_id"`
	Status          string `json:"status"`
	SuccessorTaskID string `json:"successor_task_id"`
}

type TaskHistoryGet struct {
	TaskID string `json:"task_id"`
}

// TaskDetailGet reads private task text at one exact queued revision. The
// browser never receives task bodies in a public state snapshot.
type TaskDetailGet struct {
	TaskID           string   `json:"task_id"`
	ExpectedRevision Decimal  `json:"expected_revision"`
	TextOffset       Decimal  `json:"text_offset,omitempty"`
	PeerOffset       Decimal  `json:"peer_offset,omitempty"`
	ExpectedHead     *Decimal `json:"expected_head,omitempty"`
}
type TaskDetail struct {
	TaskID         string             `json:"task_id"`
	Revision       Decimal            `json:"revision"`
	Head           Decimal            `json:"head"`
	Instruction    string             `json:"instruction"`
	Feedback       string             `json:"feedback"`
	Outcome        *string            `json:"outcome,omitempty"`
	NextTextOffset *Decimal           `json:"next_text_offset,omitempty"`
	PeerQuestions  []TaskPeerQuestion `json:"peer_questions"`
	NextPeerOffset *Decimal           `json:"next_peer_offset,omitempty"`
}
type TaskPeerQuestion struct {
	ID                     string  `json:"id"`
	SourceTaskID           string  `json:"source_task_id"`
	TargetTaskID           string  `json:"target_task_id"`
	Question               string  `json:"question"`
	Answer                 *string `json:"answer,omitempty"`
	RecipientDeliveryState string  `json:"recipient_delivery_state"`
	AnswerDeliveryState    string  `json:"answer_delivery_state"`
	Revision               Decimal `json:"revision"`
	CreatedAtMillis        Decimal `json:"created_at_ms"`
	UpdatedAtMillis        Decimal `json:"updated_at_ms"`
}
type TaskHistory struct {
	TaskID  string             `json:"task_id"`
	Entries []TaskHistoryEntry `json:"entries"`
}
type TaskHistoryEntry struct {
	OperationID     string  `json:"operation_id"`
	Kind            string  `json:"kind"`
	Actor           string  `json:"actor"`
	Body            string  `json:"body"`
	Status          string  `json:"status"`
	CreatedAtMillis Decimal `json:"created_at_ms"`
}

func EncodeAgentControl(id string, value AgentControl) ([]byte, error) {
	return encodeControl(TypeAgentControl, id, value)
}
func EncodeAgentControlResult(id string, value AgentControlResult) ([]byte, error) {
	return encodeControl(TypeAgentControlResult, id, value)
}
func EncodeTaskHistoryGet(id string, value TaskHistoryGet) ([]byte, error) {
	return encodeControl(TypeTaskHistoryGet, id, value)
}
func EncodeTaskDetailGet(id string, value TaskDetailGet) ([]byte, error) {
	return encodeControl(TypeTaskDetailGet, id, value)
}
func EncodeTaskDetail(id string, value TaskDetail) ([]byte, error) {
	return encodeControl(TypeTaskDetail, id, value)
}
func EncodeTaskHistory(id string, value TaskHistory) ([]byte, error) {
	return encodeControl(TypeTaskHistory, id, value)
}

func validAgentAction(action string) bool {
	return action == "message" || action == "interrupt" || action == "stop" || action == "replace"
}
func validAgentControl(kind MessageType, body any) error {
	bad := func() error { return fmt.Errorf("%w: invalid %s", ErrMalformed, kind) }
	switch v := body.(type) {
	case *AgentControl:
		return validAgentControl(kind, *v)
	case *AgentControlResult:
		return validAgentControl(kind, *v)
	case *TaskHistoryGet:
		return validAgentControl(kind, *v)
	case *TaskDetailGet:
		return validAgentControl(kind, *v)
	case *TaskDetail:
		return validAgentControl(kind, *v)
	case *TaskHistory:
		return validAgentControl(kind, *v)
	case AgentControl:
		if validateDynamicID(v.OperationID) != nil || validateDynamicID(v.TaskID) != nil || validateDynamicID(v.RunID) != nil || v.ExpectedTaskRevision == 0 || v.ExpectedRunRevision == 0 || !validAgentAction(v.Action) {
			return bad()
		}
		if v.Action == "replace" {
			if validateDynamicID(v.SuccessorTaskID) != nil || validateDynamicID(v.SuccessorIncarnationID) != nil || v.SuccessorTaskID == v.TaskID {
				return bad()
			}
		} else if v.SuccessorTaskID != "" || v.SuccessorIncarnationID != "" {
			return bad()
		}
		if v.Action == "message" || v.Action == "replace" {
			limit := MaxHumanReplyBytes
			if v.Action == "replace" {
				limit = MaxTaskInstructionBytes
			}
			if validateBoundedText(v.Instruction, 1, limit) != nil {
				return bad()
			}
		} else if v.Instruction != "" {
			return bad()
		}
	case AgentControlResult:
		if validateDynamicID(v.OperationID) != nil || validateDynamicID(v.TaskID) != nil || validateDynamicID(v.RunID) != nil {
			return bad()
		}
		switch v.Status {
		case "delivered", "delivery_unknown", "rejected", "stopping", "queued":
		default:
			return bad()
		}
		if v.Status == "queued" {
			if validateDynamicID(v.SuccessorTaskID) != nil {
				return bad()
			}
		} else if v.SuccessorTaskID != "" {
			return bad()
		}
	case TaskHistoryGet:
		if validateDynamicID(v.TaskID) != nil {
			return bad()
		}
	case TaskDetailGet:
		if validateDynamicID(v.TaskID) != nil || v.ExpectedRevision == 0 || v.TextOffset > MaxTaskDetailTextOffset || v.PeerOffset > MaxJSONArray || v.ExpectedHead != nil && *v.ExpectedHead == 0 || v.PeerOffset != 0 && v.ExpectedHead == nil {
			return bad()
		}
	case TaskDetail:
		if validateDynamicID(v.TaskID) != nil || v.Revision == 0 || v.Head == 0 || validateBoundedText(v.Instruction, 0, MaxTaskInstructionBytes) != nil || validateBoundedText(v.Feedback, 0, MaxTaskInstructionBytes) != nil || v.Outcome != nil && validateBoundedText(*v.Outcome, 0, MaxTaskInstructionBytes) != nil || len(v.PeerQuestions) > 1 || v.PeerQuestions == nil || v.NextTextOffset != nil && *v.NextTextOffset == 0 || v.NextPeerOffset != nil && *v.NextPeerOffset == 0 {
			return bad()
		}
		for _, question := range v.PeerQuestions {
			if validateDynamicID(question.ID) != nil || validateDynamicID(question.SourceTaskID) != nil || validateDynamicID(question.TargetTaskID) != nil || validateBoundedText(question.Question, 1, 2048) != nil || question.Answer != nil && validateBoundedText(*question.Answer, 0, 2048) != nil || !validPeerDeliveryState(question.RecipientDeliveryState) || !validPeerDeliveryState(question.AnswerDeliveryState) || question.Revision == 0 || question.CreatedAtMillis == 0 || question.UpdatedAtMillis < question.CreatedAtMillis {
				return bad()
			}
		}
	case TaskHistory:
		if validateDynamicID(v.TaskID) != nil || v.Entries == nil || len(v.Entries) > MaxJSONArray {
			return bad()
		}
		for _, entry := range v.Entries {
			if validateDynamicID(entry.OperationID) != nil || !validAgentAction(entry.Kind) || validateBoundedText(entry.Actor, 1, 128) != nil || validateBoundedText(entry.Body, 0, 1024) != nil {
				return bad()
			}
			switch entry.Status {
			case "pending", "delivered", "unknown", "rejected":
			default:
				return bad()
			}
		}
	default:
		return bad()
	}
	return nil
}

func validPeerDeliveryState(value string) bool {
	return value == "pending" || value == "delivered" || value == "unknown"
}
