package kernel

import (
	"encoding/hex"
	"fmt"
	"strconv"
)

const (
	IDBytes                = 16
	DigestBytes            = 32
	EventRetentionLimit    = 4096
	SnapshotEntityLimit    = 4096
	MaxFactoryCapacity     = 1024
	MaxChangeTreeEntries   = 10_000
	MaxChangeTreeBlobBytes = 1 << 30
)

type identifier struct {
	b [IDBytes]byte
}

func identifierFromBytes(value []byte) (identifier, error) {
	if len(value) != IDBytes {
		return identifier{}, fmt.Errorf("%w: identifier is %d bytes, want %d", ErrInvalidValue, len(value), IDBytes)
	}
	var id identifier
	copy(id.b[:], value)
	if id.zero() {
		return identifier{}, fmt.Errorf("%w: identifier is all zero", ErrInvalidValue)
	}
	return id, nil
}

func (id identifier) zero() bool {
	return id.b == [IDBytes]byte{}
}

func (id identifier) bytes() []byte {
	value := make([]byte, IDBytes)
	copy(value, id.b[:])
	return value
}

func (id identifier) String() string { return hex.EncodeToString(id.b[:]) }

type ProjectID struct{ identifier }
type AgentID struct{ identifier }
type AccountID struct{ identifier }
type TaskID struct{ identifier }
type IncarnationID struct{ identifier }
type ChangeID struct{ identifier }
type RunID struct{ identifier }
type ResourceID struct{ identifier }
type TerminalSessionID struct{ identifier }
type DaemonID struct{ identifier }
type BootID struct{ identifier }
type BrowserClientID struct{ identifier }
type HumanRequestID struct{ identifier }
type HumanRequestDeliveryID struct{ identifier }

func ProjectIDFromBytes(value []byte) (ProjectID, error) {
	id, err := identifierFromBytes(value)
	return ProjectID{id}, err
}

func AgentIDFromBytes(value []byte) (AgentID, error) {
	id, err := identifierFromBytes(value)
	return AgentID{id}, err
}

func AccountIDFromBytes(value []byte) (AccountID, error) {
	id, err := identifierFromBytes(value)
	return AccountID{id}, err
}

func TaskIDFromBytes(value []byte) (TaskID, error) {
	id, err := identifierFromBytes(value)
	return TaskID{id}, err
}

func IncarnationIDFromBytes(value []byte) (IncarnationID, error) {
	id, err := identifierFromBytes(value)
	return IncarnationID{id}, err
}

func ChangeIDFromBytes(value []byte) (ChangeID, error) {
	id, err := identifierFromBytes(value)
	return ChangeID{id}, err
}

func RunIDFromBytes(value []byte) (RunID, error) {
	id, err := identifierFromBytes(value)
	return RunID{id}, err
}

func ResourceIDFromBytes(value []byte) (ResourceID, error) {
	id, err := identifierFromBytes(value)
	return ResourceID{id}, err
}
func TerminalSessionIDFromBytes(value []byte) (TerminalSessionID, error) {
	id, err := identifierFromBytes(value)
	return TerminalSessionID{id}, err
}
func DaemonIDFromBytes(value []byte) (DaemonID, error) {
	id, err := identifierFromBytes(value)
	return DaemonID{id}, err
}
func BootIDFromBytes(value []byte) (BootID, error) {
	id, err := identifierFromBytes(value)
	return BootID{id}, err
}
func BrowserClientIDFromBytes(value []byte) (BrowserClientID, error) {
	id, err := identifierFromBytes(value)
	return BrowserClientID{id}, err
}
func HumanRequestIDFromBytes(value []byte) (HumanRequestID, error) {
	id, err := identifierFromBytes(value)
	return HumanRequestID{id}, err
}
func HumanRequestDeliveryIDFromBytes(value []byte) (HumanRequestDeliveryID, error) {
	id, err := identifierFromBytes(value)
	return HumanRequestDeliveryID{id}, err
}

func (id ProjectID) Bytes() []byte              { return id.bytes() }
func (id AgentID) Bytes() []byte                { return id.bytes() }
func (id AccountID) Bytes() []byte              { return id.bytes() }
func (id TaskID) Bytes() []byte                 { return id.bytes() }
func (id IncarnationID) Bytes() []byte          { return id.bytes() }
func (id ChangeID) Bytes() []byte               { return id.bytes() }
func (id RunID) Bytes() []byte                  { return id.bytes() }
func (id ResourceID) Bytes() []byte             { return id.bytes() }
func (id TerminalSessionID) Bytes() []byte      { return id.bytes() }
func (id DaemonID) Bytes() []byte               { return id.bytes() }
func (id BootID) Bytes() []byte                 { return id.bytes() }
func (id BrowserClientID) Bytes() []byte        { return id.bytes() }
func (id HumanRequestID) Bytes() []byte         { return id.bytes() }
func (id HumanRequestDeliveryID) Bytes() []byte { return id.bytes() }

func (id ProjectID) MarshalText() ([]byte, error)              { return []byte(id.String()), nil }
func (id AgentID) MarshalText() ([]byte, error)                { return []byte(id.String()), nil }
func (id TaskID) MarshalText() ([]byte, error)                 { return []byte(id.String()), nil }
func (id IncarnationID) MarshalText() ([]byte, error)          { return []byte(id.String()), nil }
func (id ChangeID) MarshalText() ([]byte, error)               { return []byte(id.String()), nil }
func (id RunID) MarshalText() ([]byte, error)                  { return []byte(id.String()), nil }
func (id ResourceID) MarshalText() ([]byte, error)             { return []byte(id.String()), nil }
func (id TerminalSessionID) MarshalText() ([]byte, error)      { return []byte(id.String()), nil }
func (id DaemonID) MarshalText() ([]byte, error)               { return []byte(id.String()), nil }
func (id BootID) MarshalText() ([]byte, error)                 { return []byte(id.String()), nil }
func (id BrowserClientID) MarshalText() ([]byte, error)        { return []byte(id.String()), nil }
func (id HumanRequestID) MarshalText() ([]byte, error)         { return []byte(id.String()), nil }
func (id HumanRequestDeliveryID) MarshalText() ([]byte, error) { return []byte(id.String()), nil }

type digest struct {
	b [DigestBytes]byte
}

func digestFromBytes(value []byte) (digest, error) {
	if len(value) != DigestBytes {
		return digest{}, fmt.Errorf("%w: digest is %d bytes, want %d", ErrInvalidValue, len(value), DigestBytes)
	}
	var result digest
	copy(result.b[:], value)
	return result, nil
}

func (d digest) Bytes() []byte {
	value := make([]byte, DigestBytes)
	copy(value, d.b[:])
	return value
}

type AttemptDigest struct{ digest }
type ResultProofDigest struct{ digest }
type TreeDigest struct{ digest }
type BirthDigest struct{ digest }

func AttemptDigestFromBytes(value []byte) (AttemptDigest, error) {
	d, err := digestFromBytes(value)
	return AttemptDigest{d}, err
}

func ResultProofDigestFromBytes(value []byte) (ResultProofDigest, error) {
	d, err := digestFromBytes(value)
	return ResultProofDigest{d}, err
}

func TreeDigestFromBytes(value []byte) (TreeDigest, error) {
	d, err := digestFromBytes(value)
	return TreeDigest{d}, err
}

func BirthDigestFromBytes(value []byte) (BirthDigest, error) {
	d, err := digestFromBytes(value)
	return BirthDigest{d}, err
}

func (d AttemptDigest) Bytes() []byte     { return d.digest.Bytes() }
func (d ResultProofDigest) bytes() []byte { return d.digest.Bytes() }
func (d TreeDigest) Bytes() []byte        { return d.digest.Bytes() }
func (d BirthDigest) Bytes() []byte       { return d.digest.Bytes() }

type BrowserChallengeDigest struct{ digest }

func BrowserChallengeDigestFromBytes(value []byte) (BrowserChallengeDigest, error) {
	d, err := digestFromBytes(value)
	return BrowserChallengeDigest{d}, err
}
func (d BrowserChallengeDigest) Bytes() []byte { return d.digest.Bytes() }

type UnixMillis struct{ value int64 }

func NewUnixMillis(value int64) (UnixMillis, error) {
	if value < 0 {
		return UnixMillis{}, fmt.Errorf("%w: negative timestamp", ErrInvalidValue)
	}
	return UnixMillis{value: value}, nil
}

func (value UnixMillis) Int64() int64 { return value.value }
func (value UnixMillis) MarshalJSON() ([]byte, error) {
	return strconv.AppendInt(nil, value.value, 10), nil
}

type Revision struct{ value int64 }

func NewRevision(value int64) (Revision, error) {
	if value < 1 {
		return Revision{}, fmt.Errorf("%w: revision must be positive", ErrInvalidValue)
	}
	return Revision{value: value}, nil
}

func (value Revision) Int64() int64 { return value.value }
func (value Revision) MarshalJSON() ([]byte, error) {
	return strconv.AppendInt(nil, value.value, 10), nil
}

type EventSequence struct{ value int64 }

func NewEventSequence(value int64) (EventSequence, error) {
	if value < 0 {
		return EventSequence{}, fmt.Errorf("%w: event sequence must not be negative", ErrInvalidValue)
	}
	return EventSequence{value: value}, nil
}

func (value EventSequence) Int64() int64 { return value.value }
func (value EventSequence) MarshalJSON() ([]byte, error) {
	return strconv.AppendInt(nil, value.value, 10), nil
}

type AgentRole uint8

const (
	RoleOrchestrator AgentRole = iota + 1
	RoleWorker
)

func parseAgentRole(value string) (AgentRole, error) {
	switch value {
	case "orchestrator":
		return RoleOrchestrator, nil
	case "worker":
		return RoleWorker, nil
	default:
		return 0, corruptControl("agent role", value)
	}
}

func (value AgentRole) String() string {
	switch value {
	case RoleOrchestrator:
		return "orchestrator"
	case RoleWorker:
		return "worker"
	default:
		return ""
	}
}
func (value AgentRole) valid() bool {
	return value == RoleOrchestrator || value == RoleWorker
}

type Provider uint8

const (
	ProviderClaudeCode Provider = iota + 1
	ProviderCodex
	ProviderShell
)

// ParseProvider accepts the complete public provider vocabulary.
func ParseProvider(value string) (Provider, error) {
	switch value {
	case "claude_code":
		return ProviderClaudeCode, nil
	case "codex":
		return ProviderCodex, nil
	case "shell":
		return ProviderShell, nil
	default:
		return 0, corruptControl("provider", value)
	}
}

func (value Provider) String() string {
	switch value {
	case ProviderClaudeCode:
		return "claude_code"
	case ProviderCodex:
		return "codex"
	case ProviderShell:
		return "shell"
	default:
		return ""
	}
}
func (value Provider) valid() bool {
	return value == ProviderClaudeCode || value == ProviderCodex || value == ProviderShell
}

type TaskStatus uint8

const (
	TaskQueued TaskStatus = iota + 1
	TaskRunning
	TaskBlocked
	TaskSucceeded
	TaskFailed
	TaskCancelled
)

func parseTaskStatus(value string) (TaskStatus, error) {
	switch value {
	case "queued":
		return TaskQueued, nil
	case "running":
		return TaskRunning, nil
	case "blocked":
		return TaskBlocked, nil
	case "succeeded":
		return TaskSucceeded, nil
	case "failed":
		return TaskFailed, nil
	case "cancelled":
		return TaskCancelled, nil
	default:
		return 0, corruptControl("task status", value)
	}
}

func (value TaskStatus) String() string {
	switch value {
	case TaskQueued:
		return "queued"
	case TaskRunning:
		return "running"
	case TaskBlocked:
		return "blocked"
	case TaskSucceeded:
		return "succeeded"
	case TaskFailed:
		return "failed"
	case TaskCancelled:
		return "cancelled"
	default:
		return ""
	}
}

type EntityKind uint8

const (
	EntityFactory EntityKind = iota + 1
	EntityProject
	EntityAgent
	EntityTask
	EntityChange
	EntityRun
	EntityHumanRequest
	EntityAccount
)

func parseEntityKind(value string) (EntityKind, error) {
	switch value {
	case "factory":
		return EntityFactory, nil
	case "project":
		return EntityProject, nil
	case "agent":
		return EntityAgent, nil
	case "task":
		return EntityTask, nil
	case "change":
		return EntityChange, nil
	case "run":
		return EntityRun, nil
	case "human_request":
		return EntityHumanRequest, nil
	case "account":
		return EntityAccount, nil
	default:
		return 0, corruptControl("entity kind", value)
	}
}

func (value EntityKind) String() string {
	switch value {
	case EntityFactory:
		return "factory"
	case EntityProject:
		return "project"
	case EntityAgent:
		return "agent"
	case EntityTask:
		return "task"
	case EntityChange:
		return "change"
	case EntityRun:
		return "run"
	case EntityHumanRequest:
		return "human_request"
	case EntityAccount:
		return "account"
	default:
		return ""
	}
}

type FactoryConfig struct {
	DispatchEnabled bool
	Capacity        uint16
}

func (config FactoryConfig) validate() error {
	if config.Capacity < 1 || config.Capacity > MaxFactoryCapacity {
		return fmt.Errorf("%w: capacity %d outside 1..%d", ErrInvalidValue, config.Capacity, MaxFactoryCapacity)
	}
	return nil
}

func (config FactoryConfig) normalized() (FactoryConfig, error) {
	if config.Capacity == 0 {
		config.Capacity = 1
	}
	return config, config.validate()
}

type NewProject struct {
	ID                 ProjectID
	Name               string
	Root               string
	VerificationPolicy VerificationPolicy
}

type NewAgent struct {
	ID              AgentID
	ProjectID       ProjectID
	Name            string
	Role            AgentRole
	Provider        Provider
	Model           string
	ReasoningEffort string
	// AccountID selects one linked provider login. The zero identity means
	// the provider's own default configuration directory, which is what every
	// agent created before accounts existed keeps using.
	AccountID       AccountID
	ToolBudgetLimit uint64
}

// NewAccount registers one CLI login that already exists on this machine.
// Home is that CLI's own configuration directory: CLAUDE_CONFIG_DIR for
// claude_code, CODEX_HOME for codex. Nothing secret is durable here.
type NewAccount struct {
	ID       AccountID
	Provider Provider
	Home     string
	Label    string
}

type Account struct {
	ID        AccountID
	Provider  Provider
	Home      string
	Label     string
	Revision  Revision
	CreatedAt UnixMillis
	UpdatedAt UnixMillis
}

type NewTask struct {
	ID              TaskID
	ProjectID       ProjectID
	AssignedAgentID AgentID
	IncarnationID   IncarnationID
	Title           string
	Body            string
	Priority        int64
}

type FactoryState struct {
	DaemonID        DaemonID
	DispatchEnabled bool
	Capacity        uint16
	Revision        Revision
	Head            EventSequence
	Floor           EventSequence
	updatedAt       UnixMillis
}

type Project struct {
	ID                 ProjectID
	Name               string
	Root               string
	VerificationPolicy VerificationPolicy
	Revision           Revision
	CreatedAt          UnixMillis
	UpdatedAt          UnixMillis
}

type Agent struct {
	ID              AgentID
	ProjectID       ProjectID
	Name            string
	Role            AgentRole
	Provider        Provider
	Model           string
	ReasoningEffort string
	AccountID       AccountID
	Paused          bool
	ToolBudgetLimit uint64
	ToolCallsUsed   uint64
	Revision        Revision
	CreatedAt       UnixMillis
	UpdatedAt       UnixMillis
}

type Task struct {
	ID              TaskID
	ProjectID       ProjectID
	AssignedAgentID AgentID
	IncarnationID   IncarnationID
	WorkRevision    Revision
	Title           string
	Body            string
	Status          TaskStatus
	Priority        int64
	BlockedReason   string
	Result          string
	CompletedAt     *UnixMillis
	Revision        Revision
	CreatedAt       UnixMillis
	UpdatedAt       UnixMillis
}

type ProjectSummary struct {
	ID       ProjectID
	Name     string
	Revision Revision
}

type AgentSummary struct {
	ID        AgentID
	ProjectID ProjectID
	Name      string
	Role      string
	// Provider is a served public fact (the console's C/X glyphs derive
	// from it) and never changes after creation, so it is always fresh
	// under revision-keyed invalidation. Live activity facts (busy,
	// needs-you) are deliberately NOT summary fields: they change without
	// bumping the agent revision, so a served copy would go stale on the
	// wire; clients derive them from task and human-request state, which
	// invalidates correctly.
	Provider string
	Paused   bool
	// Model and ReasoningEffort are public launch controls by owner decision
	// on 5 September 2026: the console displays and edits them.
	Model           string
	ReasoningEffort string
	// AccountID is the linked provider login this agent launches with; the
	// zero identity means the provider default.
	AccountID AccountID
	Revision  Revision
}

// AccountSummary is the served account fact: which login it is and where its
// configuration directory lives. Tokens have no field here.
type AccountSummary struct {
	ID       AccountID
	Provider string
	Home     string
	Label    string
	Revision Revision
}

type TaskSummary struct {
	ID              TaskID
	ProjectID       ProjectID
	AssignedAgentID AgentID
	Title           string
	Status          string
	Priority        int64
	Revision        Revision
}

type FactorySummary struct {
	DispatchEnabled bool
	Capacity        uint16
	ActiveRuns      uint16
	Revision        Revision
}

type DashboardSnapshot struct {
	Head          EventSequence
	Factory       FactorySummary
	Projects      []ProjectSummary
	Agents        []AgentSummary
	Tasks         []TaskSummary
	HumanRequests []HumanRequestProjection
}

func byteLen(value string) int { return len([]byte(value)) }
