package api

import (
	"encoding/json"
	"errors"
	"unicode/utf8"

	"github.com/dark-factory-build/dark-factory/internal/kernel"
)

const (
	maxFrameBytes      = 1 << 20
	maxSnapshotEntries = 4096
	credentialBytes    = 32
)

var (
	ErrInvalidClient   = errors.New("local API client configuration is invalid")
	ErrInvalidListener = errors.New("local API listener configuration is invalid")
	ErrInvalidInput    = errors.New("local API input is invalid")
	ErrProtocol        = errors.New("local API protocol is invalid")
	ErrTransport       = errors.New("local API transport failed")
)

type RemoteErrorCode string

const (
	RemoteInvalidRequest    RemoteErrorCode = "invalid_request"
	RemoteUnauthorized      RemoteErrorCode = "unauthorized"
	RemoteForbidden         RemoteErrorCode = "forbidden"
	RemoteNotFound          RemoteErrorCode = "not_found"
	RemoteConflict          RemoteErrorCode = "conflict"
	RemoteRevisionConflict  RemoteErrorCode = "revision_conflict"
	RemoteTooLarge          RemoteErrorCode = "too_large"
	RemoteUnavailable       RemoteErrorCode = "unavailable"
	RemoteCleanupUnresolved RemoteErrorCode = "cleanup_unresolved"
	RemoteInternal          RemoteErrorCode = "internal"
)

type RemoteError struct {
	code RemoteErrorCode
}

func (err *RemoteError) Error() string {
	switch err.code {
	case RemoteInvalidRequest:
		return "local API rejected the request"
	case RemoteUnauthorized:
		return "local API credential is unauthorized"
	case RemoteForbidden:
		return "local API request is forbidden"
	case RemoteNotFound:
		return "local API entity was not found"
	case RemoteConflict:
		return "local API request conflicts with durable state"
	case RemoteRevisionConflict:
		return "local API revision is stale"
	case RemoteTooLarge:
		return "local API request exceeds a bound"
	case RemoteUnavailable:
		return "local API is unavailable"
	case RemoteCleanupUnresolved:
		return "local API completed revocation but could not prove browser cleanup"
	case RemoteInternal:
		return "local API failed internally"
	default:
		return "local API returned an invalid error"
	}
}

func (err *RemoteError) Code() RemoteErrorCode { return err.code }

type HealthStatus struct {
	Ready bool `json:"ready"`
}

// WebStatus is the bounded, non-secret operator view of the loopback browser
// adapter. It intentionally contains no challenge, key, token or client
// identity data.
type WebStatus struct {
	State            string   `json:"state"`
	Ready            bool     `json:"ready"`
	Address          string   `json:"address"`
	Path             string   `json:"path"`
	Origins          []string `json:"origins"`
	ActiveClients    uint64   `json:"active_clients"`
	RevokedClients   uint64   `json:"revoked_clients"`
	ActiveChallenges uint64   `json:"active_challenges"`
}

type WebClient struct {
	ID             string  `json:"id"`
	CapabilityMask uint8   `json:"capability_mask"`
	Revision       uint64  `json:"revision"`
	CreatedAtMs    uint64  `json:"created_at_ms"`
	UpdatedAtMs    uint64  `json:"updated_at_ms"`
	RevokedAtMs    *uint64 `json:"revoked_at_ms"`
}

type WebClientPage struct {
	Clients   []WebClient `json:"clients"`
	NextAfter *string     `json:"next_after"`
}

type WebRevokeResult struct {
	ID       string `json:"id"`
	Revision uint64 `json:"revision"`
}

// RemoteStatus is the bounded, non-secret operator view of the outbound relay
// connector. It carries no ticket, key, challenge or controller identity.
type RemoteStatus struct {
	NodeID      string `json:"node_id"`
	RelayOrigin string `json:"relay_origin"`
	Connected   bool   `json:"connected"`
	Sessions    int    `json:"sessions"`
}

type MutationResult struct {
	Head         uint64                      `json:"head"`
	Revision     uint64                      `json:"revision"`
	Intervention *OverseerInterventionResult `json:"intervention,omitempty"`
	HumanReply   *OverseerHumanReplyResult   `json:"human_reply,omitempty"`
}

type OverseerHumanReplyResult struct {
	RequestID string `json:"request_id"`
	State     string `json:"state"`
}

func validMutation(result MutationResult) bool {
	if result.Revision == 0 {
		return false
	}
	if result.Intervention != nil && (!validID(result.Intervention.OperationID) || (result.Intervention.State != "delivered" && result.Intervention.State != "unknown" && result.Intervention.State != "rejected") || !validText(result.Intervention.Detail, 0, 4096)) {
		return false
	}
	return result.HumanReply == nil || validID(result.HumanReply.RequestID) && (result.HumanReply.State == "resolved" || result.HumanReply.State == "delivery_unknown")
}

type OverseerInterventionResult struct {
	OperationID string `json:"operation_id"`
	State       string `json:"state"`
	Detail      string `json:"detail"`
}

// AttemptTask is the exact private task text visible only to the authenticated
// live attempt that owns it.
type AttemptTask struct {
	Task string `json:"task"`
}

func (AttemptTask) String() string   { return "AttemptTask(<redacted>)" }
func (AttemptTask) GoString() string { return "AttemptTask(<redacted>)" }

// MarshalJSON keeps private task text safe when factoryctl prints it inside a
// provider terminal. encoding/json already escapes C0 controls; this also
// escapes DEL and C1 controls, which terminal emulators may interpret.
func (task AttemptTask) MarshalJSON() ([]byte, error) {
	if !validAttemptTask(task) {
		return nil, ErrInvalidInput
	}
	quoted, err := json.Marshal(task.Task)
	if err != nil {
		return nil, err
	}
	encoded := append([]byte(`{"task":`), terminalSafeJSON(nil, quoted)...)
	return append(encoded, '}'), nil
}

func terminalSafeJSON(dst, encoded []byte) []byte {
	const hex = "0123456789abcdef"
	for len(encoded) > 0 {
		value, width := utf8.DecodeRune(encoded)
		if value >= 0x7f && value <= 0x9f {
			dst = append(dst, '\\', 'u', '0', '0', hex[value>>4], hex[value&0xf])
		} else {
			dst = append(dst, encoded[:width]...)
		}
		encoded = encoded[width:]
	}
	return dst
}

func validAttemptTask(task AttemptTask) bool {
	return validText(task.Task, 0, 131072)
}

type FactorySummary struct {
	DispatchEnabled bool   `json:"dispatch_enabled"`
	Capacity        uint16 `json:"capacity"`
	ActiveRuns      uint16 `json:"active_runs"`
	Revision        uint64 `json:"revision"`
}

type ProjectSummary struct {
	ID       string `json:"id"`
	Name     string `json:"name"`
	Revision uint64 `json:"revision"`
}

type AgentSummary struct {
	ID        string `json:"id"`
	ProjectID string `json:"project_id"`
	Name      string `json:"name"`
	Role      string `json:"role"`
	Provider  string `json:"provider"`
	Paused    bool   `json:"paused"`
	Revision  uint64 `json:"revision"`
}

type TaskSummary struct {
	ID              string `json:"id"`
	ProjectID       string `json:"project_id"`
	AssignedAgentID string `json:"assigned_agent_id"`
	Title           string `json:"title"`
	Status          string `json:"status"`
	Priority        int64  `json:"priority"`
	Revision        uint64 `json:"revision"`
}

// DashboardSnapshot deliberately contains only the bounded public Store
// projection. Roots, task bodies/results, models, credentials and source data
// have no representable field here.
type DashboardSnapshot struct {
	Head     uint64           `json:"head"`
	Factory  FactorySummary   `json:"factory"`
	Projects []ProjectSummary `json:"projects"`
	Agents   []AgentSummary   `json:"agents"`
	Tasks    []TaskSummary    `json:"tasks"`
}

// OverseerSnapshot is the private, project-scoped view granted to a running
// orchestrator. Its project identity is derived from the attempt credential;
// callers cannot select a different project.
type OverseerSnapshot struct {
	ProjectID string                 `json:"project_id"`
	Head      uint64                 `json:"head"`
	Agents    []AgentSummary         `json:"agents"`
	Tasks     []OverseerTask         `json:"tasks"`
	Runs      []OverseerRun          `json:"runs"`
	Questions []OverseerQuestion     `json:"questions"`
	History   []OverseerIntervention `json:"history"`
}

type OverseerIntervention struct {
	OperationID      string `json:"operation_id"`
	TaskID           string `json:"task_id"`
	RunID            string `json:"run_id"`
	SuccessorTaskID  string `json:"successor_task_id"`
	Kind             string `json:"kind"`
	Actor            string `json:"actor"`
	Payload          string `json:"payload"`
	PayloadTruncated bool   `json:"payload_truncated"`
	State            string `json:"state"`
	Detail           string `json:"detail"`
	CreatedAtMs      uint64 `json:"created_at_ms"`
}

// OverseerSnapshotInput optionally selects one task for its complete private
// objective and result. An unselected status response uses bounded excerpts.
type OverseerSnapshotInput struct {
	TaskID string `json:"task_id,omitempty"`
}

// OverseerTask carries private task progress for the authenticated project's
// live orchestrator. The public dashboard has no fields for these texts.
type OverseerTask struct {
	ID                 string `json:"id"`
	ProjectID          string `json:"project_id"`
	AssignedAgentID    string `json:"assigned_agent_id"`
	Title              string `json:"title"`
	Objective          string `json:"objective"`
	ObjectiveTruncated bool   `json:"objective_truncated"`
	Status             string `json:"status"`
	Priority           int64  `json:"priority"`
	BlockedReason      string `json:"blocked_reason"`
	Result             string `json:"result"`
	ResultTruncated    bool   `json:"result_truncated"`
	Revision           uint64 `json:"revision"`
}

// MarshalJSON applies the same terminal-safe escaping as attempt task text:
// overseer question text is printed by factoryctl inside a provider terminal.
func (snapshot OverseerSnapshot) MarshalJSON() ([]byte, error) {
	type plain OverseerSnapshot
	encoded, err := json.Marshal(plain(snapshot))
	if err != nil {
		return nil, err
	}
	return terminalSafeJSON(nil, encoded), nil
}

type OverseerRun struct {
	ID       string `json:"id"`
	AgentID  string `json:"agent_id"`
	TaskID   string `json:"task_id"`
	Phase    string `json:"phase"`
	Revision uint64 `json:"revision"`
}

// OverseerQuestion carries the exact question only to the project's running
// orchestrator. It is not a browser or operator dashboard projection.
type OverseerQuestion struct {
	ID       string `json:"id"`
	AgentID  string `json:"agent_id"`
	TaskID   string `json:"task_id"`
	Status   string `json:"status"`
	Revision uint64 `json:"revision"`
	Question string `json:"question"`
}

// OverseerTaskCreateInput intentionally has no project selector: the daemon
// derives it from the live orchestrator attempt.
type OverseerTaskCreateInput struct {
	ID              string `json:"id"`
	AssignedAgentID string `json:"assigned_agent_id"`
	IncarnationID   string `json:"incarnation_id"`
	Title           string `json:"title"`
	Body            string `json:"body"`
	Priority        int64  `json:"priority"`
}

type OverseerTaskUpdateInput struct {
	TaskID           string  `json:"task_id"`
	ExpectedRevision uint64  `json:"expected_revision"`
	Priority         *int64  `json:"priority,omitempty"`
	AssignedAgentID  *string `json:"assigned_agent_id,omitempty"`
	Cancel           bool    `json:"cancel,omitempty"`
}

type OverseerAgentUpdateInput struct {
	AgentID          string `json:"agent_id"`
	ExpectedRevision uint64 `json:"expected_revision"`
	Paused           bool   `json:"paused"`
}

type OverseerRunStopInput struct {
	OperationID          string `json:"operation_id"`
	TaskID               string `json:"task_id"`
	ExpectedTaskRevision uint64 `json:"expected_task_revision"`
	RunID                string `json:"run_id"`
	ExpectedRunRevision  uint64 `json:"expected_run_revision"`
}

type OverseerRunReplaceInput struct {
	OverseerRunStopInput
	SuccessorTaskID        string `json:"successor_task_id"`
	SuccessorIncarnationID string `json:"successor_incarnation_id"`
	Instruction            string `json:"instruction"`
}

type OverseerWorkerMessageInput struct {
	OperationID          string `json:"operation_id"`
	TaskID               string `json:"task_id"`
	ExpectedTaskRevision uint64 `json:"expected_task_revision"`
	RunID                string `json:"run_id"`
	ExpectedRunRevision  uint64 `json:"expected_run_revision"`
	Message              string `json:"message"`
}

type OverseerWorkerInterruptInput struct {
	OperationID          string `json:"operation_id"`
	TaskID               string `json:"task_id"`
	ExpectedTaskRevision uint64 `json:"expected_task_revision"`
	RunID                string `json:"run_id"`
	ExpectedRunRevision  uint64 `json:"expected_run_revision"`
}

type OverseerHumanReplyInput struct {
	OperationID      string `json:"operation_id"`
	RequestID        string `json:"request_id"`
	ExpectedRevision uint64 `json:"expected_revision"`
	Reply            string `json:"reply"`
}

type CreateProjectInput struct {
	ID   string `json:"id"`
	Name string `json:"name"`
	Root string `json:"root"`
}

type CreateAgentInput struct {
	ID              string `json:"id"`
	ProjectID       string `json:"project_id"`
	Name            string `json:"name"`
	Role            string `json:"role"`
	Provider        string `json:"provider"`
	Model           string `json:"model,omitempty"`
	ReasoningEffort string `json:"reasoning_effort,omitempty"`
	// AccountID selects one linked provider login. Empty means the provider's
	// default configuration directory.
	AccountID       string `json:"account_id,omitempty"`
	ToolBudgetLimit uint64 `json:"tool_budget_limit"`
}

func validCreateAgentInput(input CreateAgentInput) bool {
	provider, err := kernel.ParseProvider(input.Provider)
	if err != nil {
		return false
	}
	return validID(input.ID) && validID(input.ProjectID) && validText(input.Name, 1, 128) &&
		(input.Role == "worker" || input.Role == "orchestrator") &&
		(input.AccountID == "" || validID(input.AccountID) && provider != kernel.ProviderShell) &&
		kernel.ValidateProviderLaunchControls(provider, input.Model, input.ReasoningEffort) == nil &&
		input.ToolBudgetLimit >= 1 && input.ToolBudgetLimit <= 1_000_000_000
}

type EnqueueTaskInput struct {
	ID              string `json:"id"`
	ProjectID       string `json:"project_id"`
	AssignedAgentID string `json:"assigned_agent_id"`
	IncarnationID   string `json:"incarnation_id"`
	Title           string `json:"title"`
	Body            string `json:"body"`
	Priority        int64  `json:"priority"`
}

// HumanQuestionInput is the bounded provider-authored portion of a
// HumanRequest. The daemon derives the run and all public projection fields
// from the authenticated attempt; callers cannot supply those identities.
type HumanQuestionInput struct {
	IdempotencyKey string `json:"idempotency_key"`
	Question       string `json:"question"`
}

// SendBackInput returns a finished task to its worker's queue with a note.
// An orchestrator's attempt names a task of its own project; the operator
// names any task.
type SendBackInput struct {
	TaskID string `json:"task_id"`
	Note   string `json:"note"`
}

type WebClientRevocationInput struct {
	ID               string `json:"id"`
	ExpectedRevision uint64 `json:"expected_revision"`
}
