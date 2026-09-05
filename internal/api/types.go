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
	Head     uint64 `json:"head"`
	Revision uint64 `json:"revision"`
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
	ToolBudgetLimit uint64 `json:"tool_budget_limit"`
}

func validCreateAgentInput(input CreateAgentInput) bool {
	provider, err := kernel.ParseProvider(input.Provider)
	if err != nil {
		return false
	}
	return validID(input.ID) && validID(input.ProjectID) && validText(input.Name, 1, 128) &&
		(input.Role == "worker" || input.Role == "orchestrator") &&
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

type WebClientRevocationInput struct {
	ID               string `json:"id"`
	ExpectedRevision uint64 `json:"expected_revision"`
}
