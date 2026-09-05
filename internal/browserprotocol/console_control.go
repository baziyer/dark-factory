package browserprotocol

import "fmt"

// AgentUpdate is the console's bounded agent-configuration edit. Model,
// ReasoningEffort and Paused are each optional: an absent member leaves the
// durable value alone, so one console screen can edit one control at a time.
type AgentUpdate struct {
	AgentID          string  `json:"agent_id"`
	ExpectedRevision Decimal `json:"expected_revision"`
	Model            *string `json:"model,omitempty"`
	ReasoningEffort  *string `json:"reasoning_effort,omitempty"`
	Paused           *Bool   `json:"paused,omitempty"`
}

type AgentUpdateResult struct {
	AgentID  string  `json:"agent_id"`
	Revision Decimal `json:"revision"`
}

// TaskUpdate edits one still-queued task. Status is the only member that is
// not free: it may say "cancelled" and nothing else.
type TaskUpdate struct {
	TaskID           string  `json:"task_id"`
	ExpectedRevision Decimal `json:"expected_revision"`
	Title            *string `json:"title,omitempty"`
	Priority         *int64  `json:"priority,omitempty"`
	AssignedAgentID  *string `json:"assigned_agent_id,omitempty"`
	Status           *string `json:"status,omitempty"`
}

type TaskUpdateResult struct {
	TaskID   string  `json:"task_id"`
	Revision Decimal `json:"revision"`
}

type TopologyGet struct {
	ProjectID string `json:"project_id"`
}

// Topology is the regenerable project structure, computed on demand. It is not
// durable state, so it carries no head and no revision; the digest is the only
// identity a client needs to tell one computation from another. Containment is
// implied by ParentID, so v1 has no edges.
type Topology struct {
	ProjectID      string         `json:"project_id"`
	Digest         string         `json:"digest"`
	SourceRevision string         `json:"source_revision"`
	Nodes          []TopologyNode `json:"nodes"`
}

type RunPathsGet struct {
	AgentID string `json:"agent_id"`
}

// RunPaths places one live worker in the part of the tree it is changing. It
// is a regenerable observation of that run's working directory, not durable
// state, so it carries no revision: the run identity is all a client needs to
// tell one worker's rooms from another's. An agent with no live run answers
// with an empty run identity and no paths.
type RunPaths struct {
	AgentID string   `json:"agent_id"`
	RunID   string   `json:"run_id"`
	Paths   []string `json:"paths"`
}

type TopologyNode struct {
	ID         string `json:"id"`
	ParentID   string `json:"parent_id"`
	Kind       string `json:"kind"`
	Path       string `json:"path"`
	Label      string `json:"label"`
	Language   string `json:"language"`
	SizeBucket string `json:"size_bucket"`
}

func EncodeAgentUpdateResult(id string, value AgentUpdateResult) ([]byte, error) {
	return encodeControl(TypeAgentUpdateResult, id, value)
}

func EncodeTaskUpdateResult(id string, value TaskUpdateResult) ([]byte, error) {
	return encodeControl(TypeTaskUpdateResult, id, value)
}

func EncodeTopology(id string, value Topology) ([]byte, error) {
	return encodeControl(TypeTopology, id, value)
}

// EncodeRunPaths normalizes an absent path list to an empty one: JSON null is
// not an array on either side of the boundary.
func EncodeRunPaths(id string, value RunPaths) ([]byte, error) {
	if value.Paths == nil {
		value.Paths = []string{}
	}
	return encodeControl(TypeRunPaths, id, value)
}

func validConsoleControl(kind MessageType, body any) error {
	bad := func() error { return fmt.Errorf("%w: invalid %s", ErrMalformed, kind) }
	switch value := body.(type) {
	case *AgentUpdate:
		return validConsoleControl(kind, *value)
	case *AgentUpdateResult:
		return validConsoleControl(kind, *value)
	case *TaskUpdate:
		return validConsoleControl(kind, *value)
	case *TaskUpdateResult:
		return validConsoleControl(kind, *value)
	case *TopologyGet:
		return validConsoleControl(kind, *value)
	case *Topology:
		return validConsoleControl(kind, *value)
	case *RunPathsGet:
		return validConsoleControl(kind, *value)
	case *RunPaths:
		return validConsoleControl(kind, *value)
	case AgentUpdate:
		if validateDynamicID(value.AgentID) != nil || value.ExpectedRevision == 0 ||
			value.Model != nil && validateBoundedText(*value.Model, 0, MaxAgentModelBytes) != nil ||
			value.ReasoningEffort != nil && validateBoundedText(*value.ReasoningEffort, 0, MaxAgentModelBytes) != nil {
			return bad()
		}
	case AgentUpdateResult:
		if validateDynamicID(value.AgentID) != nil || value.Revision == 0 {
			return bad()
		}
	case TaskUpdate:
		if validateDynamicID(value.TaskID) != nil || value.ExpectedRevision == 0 ||
			value.Title != nil && validateBoundedText(*value.Title, 1, MaxTaskTitleBytes) != nil ||
			value.Priority != nil && (*value.Priority < -MaxTaskPriority || *value.Priority > MaxTaskPriority) ||
			value.AssignedAgentID != nil && validateDynamicID(*value.AssignedAgentID) != nil ||
			value.Status != nil && *value.Status != "cancelled" {
			return bad()
		}
	case TaskUpdateResult:
		if validateDynamicID(value.TaskID) != nil || value.Revision == 0 {
			return bad()
		}
	case TopologyGet:
		if validateDynamicID(value.ProjectID) != nil {
			return bad()
		}
	case Topology:
		if validateDynamicID(value.ProjectID) != nil || !validTopologyDigest(value.Digest) ||
			!validTopologySource(value.SourceRevision) || len(value.Nodes) > MaxSnapshotEntities {
			return bad()
		}
		for _, node := range value.Nodes {
			if !validTopologyNode(node) {
				return bad()
			}
		}
	case RunPathsGet:
		if validateDynamicID(value.AgentID) != nil {
			return bad()
		}
	case RunPaths:
		// No live run means no rooms, so an empty run identity may not carry
		// paths. The array itself keeps the generic control item bound.
		if validateDynamicID(value.AgentID) != nil || value.Paths == nil || len(value.Paths) > MaxJSONArray ||
			value.RunID == "" && len(value.Paths) != 0 ||
			value.RunID != "" && validateDynamicID(value.RunID) != nil {
			return bad()
		}
		for _, path := range value.Paths {
			if validateBoundedText(path, 1, MaxTaskTitleBytes) != nil {
				return bad()
			}
		}
	default:
		return bad()
	}
	return nil
}

func validTopologyDigest(value string) bool {
	_, err := fixedHex("digest", value, 32)
	return err == nil
}

// validTopologySource accepts an empty revision or one canonical Git object
// name, in either the SHA-1 or the SHA-256 length Git itself uses.
func validTopologySource(value string) bool {
	if value == "" {
		return true
	}
	if _, err := fixedHex("source_revision", value, 20); err == nil {
		return true
	}
	_, err := fixedHex("source_revision", value, 32)
	return err == nil
}

func validTopologyNode(node TopologyNode) bool {
	if _, err := fixedHex("node id", node.ID, 32); err != nil {
		return false
	}
	if node.ParentID != "" {
		if _, err := fixedHex("node parent id", node.ParentID, 32); err != nil {
			return false
		}
	}
	switch node.Kind {
	case "repository", "module", "package", "directory":
	default:
		return false
	}
	switch node.SizeBucket {
	case "empty", "tiny", "small", "medium", "large":
	default:
		return false
	}
	return validateBoundedText(node.Path, 1, MaxTaskTitleBytes) == nil &&
		validateBoundedText(node.Label, 1, MaxAgentNameBytes) == nil &&
		validateBoundedText(node.Language, 0, MaxAgentNameBytes) == nil
}
