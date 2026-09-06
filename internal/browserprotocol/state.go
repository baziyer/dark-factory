package browserprotocol

import (
	"bytes"
	"encoding/json"
	"fmt"
	"math"
	"strconv"
	"unicode/utf8"
)

const (
	// MaxSnapshotEntities is the exact total the server may place in one
	// STATE_SNAPSHOT. It mirrors the kernel's public read guard; exceeding it
	// is a finite too_large failure, never a truncated snapshot.
	MaxSnapshotEntities = 4096
	// MaxSnapshotBytes bounds one encoded STATE_SNAPSHOT. Every other frame,
	// in both directions, stays at MaxControlBytes.
	MaxSnapshotBytes    = 1 << 20
	MaxProjectNameBytes = 128
	MaxAgentNameBytes   = 128
	MaxAgentModelBytes  = 128
	// MaxModelSourceBytes bounds the configuration path an effective model was
	// read from. It is a local filesystem path, not free text.
	MaxModelSourceBytes          = 1024
	MaxTaskTitleBytes            = 1024
	MaxHumanQuestionBytes        = 8192
	MaxHumanReplyBytes           = 8192
	MaxFactoryCapacity           = 1024
	MaxTaskPriority       int64  = 1_000_000
	MaxSQLiteInteger      uint64 = math.MaxInt64
)

// Decimal is one non-negative SQLite chronology value. JSON represents it as
// a canonical decimal string so JavaScript never truncates it.
type Decimal uint64

func (value Decimal) MarshalJSON() ([]byte, error) {
	if uint64(value) > MaxSQLiteInteger {
		return nil, fmt.Errorf("%w: decimal overflow", ErrMalformed)
	}
	return strconv.AppendQuote(nil, strconv.FormatUint(uint64(value), 10)), nil
}

func (value *Decimal) UnmarshalJSON(data []byte) error {
	var encoded string
	if err := json.Unmarshal(data, &encoded); err != nil {
		return fmt.Errorf("%w: decimal string required", ErrMalformed)
	}
	parsed, err := parseDecimal(encoded)
	if err != nil {
		return err
	}
	*value = parsed
	return nil
}

func parseDecimal(value string) (Decimal, error) {
	if value == "" || len(value) > 19 || len(value) > 1 && value[0] == '0' {
		return 0, fmt.Errorf("%w: non-canonical decimal", ErrMalformed)
	}
	for _, character := range []byte(value) {
		if character < '0' || character > '9' {
			return 0, fmt.Errorf("%w: non-canonical decimal", ErrMalformed)
		}
	}
	parsed, err := strconv.ParseUint(value, 10, 64)
	if err != nil || parsed > MaxSQLiteInteger {
		return 0, fmt.Errorf("%w: decimal overflow", ErrMalformed)
	}
	return Decimal(parsed), nil
}

// Bool is an exact JSON boolean. encoding/json otherwise accepts null for a
// bool field and silently turns it into false, which would make Go looser than
// the browser parser at the authority boundary.
type Bool bool

func (value Bool) MarshalJSON() ([]byte, error) {
	if value {
		return []byte("true"), nil
	}
	return []byte("false"), nil
}

func (value *Bool) UnmarshalJSON(data []byte) error {
	switch string(bytes.TrimSpace(data)) {
	case "true":
		*value = true
	case "false":
		*value = false
	default:
		return fmt.Errorf("%w: boolean required", ErrMalformed)
	}
	return nil
}

// StateGet asks for the current complete snapshot. It carries no cursor,
// continuation or selector: there is exactly one thing to ask for.
type StateGet struct{}

type FactoryItem struct {
	DispatchEnabled Bool    `json:"dispatch_enabled"`
	Capacity        uint16  `json:"capacity"`
	ActiveRuns      uint16  `json:"active_runs"`
	Revision        Decimal `json:"revision"`
}

type ProjectItem struct {
	ID       string  `json:"id"`
	Name     string  `json:"name"`
	Revision Decimal `json:"revision"`
}

type AgentItem struct {
	ID        string `json:"id"`
	ProjectID string `json:"project_id"`
	Name      string `json:"name"`
	Role      string `json:"role"`
	// Provider is a public fact used for display. Live activity facts are
	// deliberately not item fields; clients derive them from task and
	// human-request state in the same coherent snapshot.
	Provider string `json:"provider"`
	Paused   Bool   `json:"paused"`
	// Model and ReasoningEffort are the operator-editable launch controls the
	// console displays and AGENT_UPDATE edits. Empty means unset.
	Model           string `json:"model"`
	ReasoningEffort string `json:"reasoning_effort"`
	// EffectiveModel and EffectiveReasoningEffort are what the run will
	// actually use: the agent's own value when set, otherwise the provider
	// CLI's own configured default. ModelSource says where the model came
	// from: "agent" when the agent names one, the provider configuration path
	// the default was read from, or "" when the CLI keeps a default the
	// factory cannot see.
	EffectiveModel           string  `json:"effective_model"`
	EffectiveReasoningEffort string  `json:"effective_reasoning_effort"`
	ModelSource              string  `json:"model_source"`
	Revision                 Decimal `json:"revision"`
	// AccountID is the linked provider login this agent launches with. Empty
	// means the provider's own default configuration directory, and so the
	// default EffectiveModel above was read from.
	AccountID string `json:"account_id"`
	// The idle rule, as CONFIG shows and edits it. IdleRunsUsed counts only
	// the runs the rule enqueued itself.
	IdlePolicy       string `json:"idle_policy"`
	IdleAfterSeconds uint32 `json:"idle_after_seconds"`
	IdleInstruction  string `json:"idle_instruction"`
	IdleRunBudget    uint32 `json:"idle_run_budget"`
	IdleRunsUsed     uint32 `json:"idle_runs_used"`
}

// AccountItem is one linked provider login. Only which login it is and where
// its configuration directory lives; nothing that proves it.
type AccountItem struct {
	ID       string  `json:"id"`
	Provider string  `json:"provider"`
	Home     string  `json:"home"`
	Label    string  `json:"label"`
	Revision Decimal `json:"revision"`
}

type TaskItem struct {
	ID              string  `json:"id"`
	ProjectID       string  `json:"project_id"`
	AssignedAgentID string  `json:"assigned_agent_id"`
	Title           string  `json:"title"`
	Status          string  `json:"status"`
	Priority        int64   `json:"priority"`
	Revision        Decimal `json:"revision"`
}

// HumanRequestItem is deliberately only the public card projection. Private
// question/reply text and copied display prose cannot be represented here.
type HumanRequestItem struct {
	ID            string  `json:"id"`
	ProjectID     string  `json:"project_id"`
	AgentID       string  `json:"agent_id"`
	TaskID        string  `json:"task_id"`
	CreatedAt     Decimal `json:"created_at"`
	UpdatedAt     Decimal `json:"updated_at"`
	Revision      Decimal `json:"revision"`
	Kind          string  `json:"kind"`
	Status        string  `json:"status"`
	ReplyMaxBytes uint16  `json:"reply_max_bytes"`
	CanReply      Bool    `json:"can_reply"`
}

// StateSnapshot is one complete, coherent public projection read at Head.
// There is no partial, staged or continued form: a client either has a whole
// snapshot or it has none.
type StateSnapshot struct {
	Head          Decimal            `json:"head"`
	Factory       FactoryItem        `json:"factory"`
	Projects      []ProjectItem      `json:"projects"`
	Agents        []AgentItem        `json:"agents"`
	Tasks         []TaskItem         `json:"tasks"`
	HumanRequests []HumanRequestItem `json:"human_requests"`
	Accounts      []AccountItem      `json:"accounts"`
}

// StateWatch asks to be told when durable state moves past AfterHead. The
// server closes the snapshot-to-watch gap by rereading the durable head after
// installing the watcher.
type StateWatch struct {
	AfterHead Decimal `json:"after_head"`
}

// StateChanged carries no entity data at all. It is a bare invalidation: the
// client refetches one whole snapshot when it wants current state.
type StateChanged struct {
	Head Decimal `json:"head"`
}

type HumanRequestDetailGet struct {
	RequestID        string  `json:"request_id"`
	ExpectedRevision Decimal `json:"expected_revision"`
}

type HumanRequestDetail struct {
	RequestID      string                           `json:"request_id"`
	Revision       Decimal                          `json:"revision"`
	Question       string                           `json:"question"`
	CanReply       Bool                             `json:"can_reply"`
	ReplyMaxBytes  uint16                           `json:"reply_max_bytes"`
	TerminalTarget *TerminalTargetDescriptor        `json:"terminal_target"`
	CancelRun      *HumanRequestCancelRunDescriptor `json:"cancel_run"`
}

type HumanRequestCancelRunDescriptor struct {
	ExpectedRequestRevision Decimal `json:"expected_request_revision"`
	ExpectedRunRevision     Decimal `json:"expected_run_revision"`
}

// validateDynamicID accepts one canonical lowercase 16-byte hex identity. The
// all-zero identity is the durable factory sentinel and can never appear.
func validateDynamicID(id string) error {
	if _, err := fixedHex("entity id", id, 16); err != nil || id == "00000000000000000000000000000000" {
		return fmt.Errorf("%w: entity id", ErrMalformed)
	}
	return nil
}

func validateBoundedText(value string, minimum, maximum int) error {
	if !utf8.ValidString(value) || len([]byte(value)) < minimum || len([]byte(value)) > maximum {
		return fmt.Errorf("%w: text bound", ErrMalformed)
	}
	return nil
}

func validateFactoryItem(value FactoryItem) error {
	if value.Capacity < 1 || value.Capacity > MaxFactoryCapacity || value.ActiveRuns > MaxFactoryCapacity || value.ActiveRuns > value.Capacity || value.Revision == 0 {
		return fmt.Errorf("%w: factory item", ErrMalformed)
	}
	return nil
}

func validateProjectItem(value ProjectItem) error {
	if validateDynamicID(value.ID) != nil || validateBoundedText(value.Name, 1, MaxProjectNameBytes) != nil || value.Revision == 0 {
		return fmt.Errorf("%w: project item", ErrMalformed)
	}
	return nil
}

func validateAgentItem(value AgentItem) error {
	if validateDynamicID(value.ID) != nil || validateDynamicID(value.ProjectID) != nil || validateBoundedText(value.Name, 1, MaxAgentNameBytes) != nil || value.Revision == 0 || value.Role != "orchestrator" && value.Role != "worker" {
		return fmt.Errorf("%w: agent item", ErrMalformed)
	}
	if value.Provider != "claude_code" && value.Provider != "codex" && value.Provider != "shell" {
		return fmt.Errorf("%w: agent provider", ErrMalformed)
	}
	if validateBoundedText(value.Model, 0, MaxAgentModelBytes) != nil || validateBoundedText(value.ReasoningEffort, 0, MaxAgentModelBytes) != nil {
		return fmt.Errorf("%w: agent launch controls", ErrMalformed)
	}
	if validateBoundedText(value.EffectiveModel, 0, MaxAgentModelBytes) != nil || validateBoundedText(value.EffectiveReasoningEffort, 0, MaxAgentModelBytes) != nil || validateBoundedText(value.ModelSource, 0, MaxModelSourceBytes) != nil {
		return fmt.Errorf("%w: agent effective model", ErrMalformed)
	}
	if value.AccountID != "" && (validateDynamicID(value.AccountID) != nil || value.Provider == "shell") {
		return fmt.Errorf("%w: agent account", ErrMalformed)
	}
	// No policy at all is a snapshot from before idle rules: the agent waits.
	if value.IdlePolicy != "" && !validIdlePolicy(value.IdlePolicy) || value.IdleAfterSeconds > MaxIdleAfterSeconds || validateBoundedText(value.IdleInstruction, 0, MaxTaskInstructionBytes) != nil ||
		value.IdleRunBudget > MaxIdleRunBudget || value.IdleRunsUsed > value.IdleRunBudget ||
		value.IdlePolicy == "standing_instruction" && (value.IdleAfterSeconds == 0 || value.IdleInstruction == "" || value.IdleRunBudget == 0) {
		return fmt.Errorf("%w: agent idle rule", ErrMalformed)
	}
	return nil
}

func validateAccountItem(value AccountItem) error {
	if validateDynamicID(value.ID) != nil || !validProviderAccount(value.Provider) ||
		validAccountHome(value.Home) != nil || validateBoundedText(value.Label, 1, MaxAgentNameBytes) != nil || value.Revision == 0 {
		return fmt.Errorf("%w: account item", ErrMalformed)
	}
	return nil
}

func validateTaskItem(value TaskItem) error {
	if validateDynamicID(value.ID) != nil || validateDynamicID(value.ProjectID) != nil || validateDynamicID(value.AssignedAgentID) != nil || validateBoundedText(value.Title, 1, MaxTaskTitleBytes) != nil || value.Priority < -MaxTaskPriority || value.Priority > MaxTaskPriority || value.Revision == 0 {
		return fmt.Errorf("%w: task item", ErrMalformed)
	}
	switch value.Status {
	case "queued", "running", "blocked", "succeeded", "failed", "cancelled":
		return nil
	default:
		return fmt.Errorf("%w: task status", ErrMalformed)
	}
}

func validateHumanRequestItem(value HumanRequestItem) error {
	if validateDynamicID(value.ID) != nil || validateDynamicID(value.ProjectID) != nil || validateDynamicID(value.AgentID) != nil || validateDynamicID(value.TaskID) != nil || value.UpdatedAt < value.CreatedAt || value.Revision == 0 || value.Kind != "question" || value.ReplyMaxBytes < 1 || value.ReplyMaxBytes > MaxHumanReplyBytes {
		return fmt.Errorf("%w: human request item", ErrMalformed)
	}
	switch value.Status {
	case "open", "delivering", "delivery_unknown":
		return nil
	default:
		return fmt.Errorf("%w: human request status", ErrMalformed)
	}
}

func validateStateGet(StateGet) error { return nil }

// validateStateSnapshot enforces the exact count bound and per-collection
// identity uniqueness. It never trims: an oversized snapshot is malformed.
func validateStateSnapshot(value StateSnapshot) error {
	if err := validateFactoryItem(value.Factory); err != nil {
		return err
	}
	total := 1 + len(value.Projects) + len(value.Agents) + len(value.Tasks) + len(value.HumanRequests) + len(value.Accounts)
	if total > MaxSnapshotEntities {
		return fmt.Errorf("%w: snapshot entity count", ErrMalformed)
	}
	seen := make(map[string]struct{}, total)
	claim := func(prefix, id string) error {
		key := prefix + id
		if _, duplicate := seen[key]; duplicate {
			return fmt.Errorf("%w: duplicate snapshot identity", ErrMalformed)
		}
		seen[key] = struct{}{}
		return nil
	}
	for _, item := range value.Projects {
		if err := validateProjectItem(item); err != nil {
			return err
		}
		if err := claim("project:", item.ID); err != nil {
			return err
		}
	}
	for _, item := range value.Agents {
		if err := validateAgentItem(item); err != nil {
			return err
		}
		if err := claim("agent:", item.ID); err != nil {
			return err
		}
	}
	for _, item := range value.Tasks {
		if err := validateTaskItem(item); err != nil {
			return err
		}
		if err := claim("task:", item.ID); err != nil {
			return err
		}
	}
	for _, item := range value.HumanRequests {
		if err := validateHumanRequestItem(item); err != nil {
			return err
		}
		if err := claim("human_request:", item.ID); err != nil {
			return err
		}
	}
	for _, item := range value.Accounts {
		if err := validateAccountItem(item); err != nil {
			return err
		}
		if err := claim("account:", item.ID); err != nil {
			return err
		}
	}
	return nil
}

func validateStateWatch(StateWatch) error { return nil }

func validateStateChanged(value StateChanged) error {
	if value.Head == 0 {
		return fmt.Errorf("%w: state change head", ErrMalformed)
	}
	return nil
}

func validateHumanRequestDetailGet(value HumanRequestDetailGet) error {
	if validateDynamicID(value.RequestID) != nil || value.ExpectedRevision == 0 {
		return fmt.Errorf("%w: human request detail request", ErrMalformed)
	}
	return nil
}

func validateHumanRequestDetail(value HumanRequestDetail) error {
	if validateDynamicID(value.RequestID) != nil || value.Revision == 0 || validateBoundedText(value.Question, 1, MaxHumanQuestionBytes) != nil || value.ReplyMaxBytes != MaxHumanReplyBytes {
		return fmt.Errorf("%w: human request detail", ErrMalformed)
	}
	if value.TerminalTarget != nil {
		if err := validTerminalTargetDescriptor(*value.TerminalTarget); err != nil {
			return fmt.Errorf("%w: human request terminal target", ErrMalformed)
		}
	}
	if value.CancelRun != nil {
		if value.TerminalTarget == nil || !bool(value.CanReply) || value.CancelRun.ExpectedRequestRevision != value.Revision || value.CancelRun.ExpectedRunRevision != value.TerminalTarget.RunRevision {
			return fmt.Errorf("%w: human request cancellation", ErrMalformed)
		}
	}
	if bool(value.CanReply) && (value.TerminalTarget == nil || value.CancelRun == nil) {
		return fmt.Errorf("%w: human request reply availability", ErrMalformed)
	}
	return nil
}
