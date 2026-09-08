package browserprotocol

import "fmt"

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
