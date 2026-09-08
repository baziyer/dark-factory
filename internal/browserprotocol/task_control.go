package browserprotocol

import (
	"fmt"
	"unicode/utf8"
)

const MaxTaskInstructionBytes = 32768

type TaskEnqueue struct {
	TaskID                string  `json:"task_id"`
	IncarnationID         string  `json:"incarnation_id"`
	AgentID               string  `json:"agent_id"`
	ExpectedAgentRevision Decimal `json:"expected_agent_revision"`
	Instruction           string  `json:"instruction"`
	Mode                  string  `json:"mode,omitempty"`
}

type TaskEnqueueResult struct {
	TaskID        string  `json:"task_id"`
	Revision      Decimal `json:"revision"`
	AgentRevision Decimal `json:"agent_revision"`
}

func EncodeTaskEnqueue(id string, value TaskEnqueue) ([]byte, error) {
	return encodeControl(TypeTaskEnqueue, id, value)
}

func EncodeTaskEnqueueResult(id string, value TaskEnqueueResult) ([]byte, error) {
	return encodeControl(TypeTaskEnqueueResult, id, value)
}

func validTaskControl(kind MessageType, body any) error {
	if value, ok := body.(*TaskEnqueue); ok {
		return validTaskControl(kind, *value)
	}
	if value, ok := body.(*TaskEnqueueResult); ok {
		return validTaskControl(kind, *value)
	}
	bad := func() error { return fmt.Errorf("%w: invalid %s", ErrMalformed, kind) }
	id := func(value string) bool {
		decoded, err := fixedHex("id", value, 16)
		if err != nil {
			return false
		}
		for _, b := range decoded {
			if b != 0 {
				return true
			}
		}
		return false
	}
	positive := func(value Decimal) bool { return value > 0 }
	switch value := body.(type) {
	case TaskEnqueue:
		if value.Mode != "" && value.Mode != "now" && value.Mode != "queue" || !id(value.TaskID) || !id(value.IncarnationID) || !id(value.AgentID) || !positive(value.ExpectedAgentRevision) || !utf8.ValidString(value.Instruction) || len(value.Instruction) == 0 || len([]byte(value.Instruction)) > MaxTaskInstructionBytes {
			return bad()
		}
	case TaskEnqueueResult:
		if !id(value.TaskID) || !positive(value.Revision) || !positive(value.AgentRevision) {
			return bad()
		}
	default:
		return bad()
	}
	return nil
}
