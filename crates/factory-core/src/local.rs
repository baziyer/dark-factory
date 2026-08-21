//! Versioned request/response protocol for the local control socket.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::{
    AgentId, AgentRole, AgentSnapshot, ChangeId, ChangeSnapshot, ChangeStorageSnapshot,
    CompletionVerification, EventEnvelope, LegacySourceId, LegacySourceSnapshot, MessageId,
    PROTOCOL_VERSION, ProjectId, ProjectSnapshot, Provider, ProviderHookEvent, RunId, RunSnapshot,
    TaskDetail, TaskId,
};

/// Private operator-facing configuration. It is deliberately not part of an
/// `AgentSnapshot` or `FactoryEvent`. `instructions` and `memory` are read
/// from (and written to) the agent's guidance files under
/// `$DARK_FACTORY_HOME/projects`; see `AgentDetail::instructions_path` and
/// `AgentDetail::memory_path`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentProfile {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Codex's configured reasoning tier, when explicitly known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Durable policy/operator explanation for the configured model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection_reason: Option<String>,
    /// Provider-scoped permission mode (Claude: `default`/`acceptEdits`/
    /// `plan`; Codex: `on-request`/`never`); `None` is the provider default.
    /// Applied to future provider launches; the resulting runtime value is
    /// recorded separately on each run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub instructions: String,
    pub memory: String,
    pub updated_at_ms: i64,
}

/// The single guidance-file size limit shared by the daemon and local wire
/// projection. Guidance is operator/agent-editable text, not ledger state.
pub const MAX_GUIDANCE_FILE_BYTES: usize = 16 * 1024;

/// The bounded health projection for an agent's file-backed memory. The
/// mechanical agent/status responses carry this even when the active file is
/// too large or not readable, so guidance failure cannot hide durable state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GuidanceHealthState {
    Ok,
    NearLimit,
    Oversized,
    InvalidUtf8,
    PathError,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GuidanceHealth {
    pub state: GuidanceHealthState,
    pub bytes: u64,
    pub max_bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Default for GuidanceHealth {
    fn default() -> Self {
        Self {
            state: GuidanceHealthState::Ok,
            bytes: 0,
            max_bytes: MAX_GUIDANCE_FILE_BYTES as u64,
            detail: None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentDetail {
    pub snapshot: AgentSnapshot,
    pub profile: AgentProfile,
    /// Absolute path to this agent's standing-guidance file, editable
    /// directly with `$EDITOR`.
    pub instructions_path: String,
    /// Bounded health of `instructions.md`; unhealthy content is omitted
    /// from the profile rather than making a mechanical lookup fail.
    #[serde(default)]
    pub instructions_health: GuidanceHealth,
    /// Absolute path to this agent's memory file. The agent itself is
    /// expected to append durable lessons here.
    pub memory_path: String,
    /// Bounded health of the active `memory.md`; unhealthy content is not
    /// copied into this response.
    #[serde(default)]
    pub memory_health: GuidanceHealth,
    /// Absolute path to this agent's project's `PROJECT.md`.
    pub project_guidance_path: String,
}

/// Project-level guidance, file-backed under `$DARK_FACTORY_HOME/projects`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectDetail {
    pub snapshot: ProjectSnapshot,
    pub guidance: String,
    /// Absolute path to this project's `PROJECT.md`, editable directly with
    /// `$EDITOR`.
    pub guidance_path: String,
}

/// Private operator/agent message. Message bodies never enter public events.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentMessage {
    pub id: MessageId,
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sender_agent_id: Option<AgentId>,
    pub recipient_agent_id: AgentId,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_at_ms: Option<i64>,
}

/// Maximum JSON payload size. The newline delimiter is not part of this limit.
pub const MAX_LOCAL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_PROJECT_PAGE_ITEMS: u32 = 1000;
/// Deliberately far lower than the other page caps: a `TaskDetail` carries a
/// full body (up to [`MAX_TASK_BODY_BYTES`]) plus an optional result, so this
/// stays small enough that a page of worst-case tasks still fits one
/// [`MAX_LOCAL_FRAME_BYTES`] frame (see
/// `the_largest_valid_task_page_fits_one_local_frame`).
pub const MAX_TASK_PAGE_ITEMS: u32 = 10;
pub const MAX_AGENT_PAGE_ITEMS: u32 = 1000;
pub const MAX_RUN_PAGE_ITEMS: u32 = 1000;
/// A Change may carry a 4 KiB failure whose JSON escaping expands sixfold.
/// Sixteen worst-case rows stay below [`MAX_LOCAL_FRAME_BYTES`].
pub const MAX_CHANGE_PAGE_ITEMS: u32 = 16;
/// A legacy row may contain two 4 KiB strings whose JSON escaping expands
/// substantially. Sixteen worst-case rows stay below [`MAX_LOCAL_FRAME_BYTES`].
pub const MAX_LEGACY_SOURCE_PAGE_ITEMS: u32 = 16;
/// Durable event pages can carry the same worst-case Change projection.
pub const MAX_EVENT_PAGE_ITEMS: u32 = 16;
pub const MAX_TASK_TITLE_BYTES: usize = 240;
pub const MAX_TASK_BODY_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_REQUEST_CREDENTIAL_BYTES: usize = 1024;

/// Bounded operator projection of the daemon-owned Rust artifact inventory.
/// Missing byte totals mean at least one live artifact has not been measured;
/// counts and policy limits remain exact in that case.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RustStorageSnapshot {
    pub max_cache_count: u64,
    pub max_cache_bytes: u64,
    pub cache_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_bytes: Option<u64>,
    pub protected_count: u64,
    pub reclaimable_count: u64,
    /// Live cache rows whose last external reconciliation attempt failed.
    /// They remain in a recoverable lifecycle and keep totals incomplete.
    pub failed_count: u64,
    pub cache_count_over_limit: bool,
    pub cache_bytes_over_limit: bool,
    pub complete: bool,
}

/// Applies the task-title contract shared by every local and connector write.
#[must_use]
pub fn normalize_task_title(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty() && value.len() <= MAX_TASK_TITLE_BYTES).then_some(value)
}

/// Opaque bearer resolved by factoryd to exactly one operator or attempt
/// principal. Its kind and owned identities are deliberately not caller
/// selected.
#[derive(Clone, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RequestCredential(String);

impl RequestCredential {
    pub fn new(value: String) -> Result<Self, &'static str> {
        if value.is_empty() {
            return Err("request credential must not be empty");
        }
        if value.len() > MAX_REQUEST_CREDENTIAL_BYTES {
            return Err("request credential exceeds the wire limit");
        }
        Ok(Self(value))
    }

    /// Exposes the bearer only to the authentication boundary. Do not log it.
    #[must_use]
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for RequestCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RequestCredential(<redacted>)")
    }
}

impl<'de> Deserialize<'de> for RequestCredential {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub protocol_version: u16,
    /// `None` is valid only for explicitly anonymous requests such as health.
    /// The daemon must never infer operator authority from an absent bearer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<RequestCredential>,
    pub request: LocalRequest,
}

impl RequestEnvelope {
    #[must_use]
    pub const fn new(request: LocalRequest) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            credential: None,
            request,
        }
    }

    #[must_use]
    pub const fn authenticated(request: LocalRequest, credential: RequestCredential) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            credential: Some(credential),
            request,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LocalRequest {
    Health,
    /// Changes the durable factory-wide provider bypass default.
    SetAutoMode {
        enabled: bool,
    },
    /// The whole daemon at one instant (`factoryctl status`): every
    /// project's agents with their runs and queues, plus the attention list
    /// and active-run cap. See [`crate::status`].
    FleetStatus,
    /// Exact persisted Rust artifact inventory and its bounded policy.
    RustStorageStatus,
    /// One agent's live run picture plus its profile (`factoryctl agent status`).
    AgentStatus {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    CreateProject {
        id: ProjectId,
        name: String,
        root: String,
    },
    ListProjects {
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<ProjectId>,
        limit: u32,
    },
    GetProject {
        project_id: ProjectId,
    },
    UpdateProjectGuidance {
        project_id: ProjectId,
        text: String,
    },
    SetProjectCompletionVerification {
        project_id: ProjectId,
        verification: CompletionVerification,
    },
    CreateTask {
        id: TaskId,
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<TaskId>,
        title: String,
        body: String,
        priority: i32,
        /// When present, create-and-assign is one durable transaction. The
        /// daemon wakes only this agent after the commit.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
    },
    CreateAgent {
        id: AgentId,
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_agent_id: Option<AgentId>,
        role: AgentRole,
        provider: Provider,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_selection_reason: Option<String>,
    },
    GetAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    UpdateAgentProfile {
        project_id: ProjectId,
        agent_id: AgentId,
        #[serde(skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_effort: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_selection_reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        permission_mode: Option<String>,
        instructions: String,
        memory: String,
    },
    SetAgentBudget {
        project_id: ProjectId,
        agent_id: AgentId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_tool_calls: Option<u64>,
    },
    ResetAgentBudget {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    SendAgentMessage {
        id: MessageId,
        project_id: ProjectId,
        recipient_agent_id: AgentId,
        body: String,
    },
    ListAgentMessages {
        project_id: ProjectId,
        agent_id: AgentId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<MessageId>,
        limit: u32,
    },
    ListAgents {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<AgentId>,
        limit: u32,
    },
    ListTasks {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<TaskId>,
        /// When present, list only this agent's assigned tasks. The active
        /// queue order is running-first, priority-descending,
        /// creation time, then ID. Pages carry a durable revision so any
        /// mutation makes a cursor explicitly stale instead of skipping work.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_revision: Option<i64>,
        /// Include terminal/history rows. The default is the active queue
        /// only; active work is never hidden by a history view.
        #[serde(default)]
        history: bool,
        limit: u32,
    },
    GetTask {
        project_id: ProjectId,
        task_id: TaskId,
    },
    RetryTask {
        project_id: ProjectId,
        task_id: TaskId,
    },
    CancelTask {
        project_id: ProjectId,
        task_id: TaskId,
    },
    UpdateTask {
        project_id: ProjectId,
        task_id: TaskId,
        #[serde(skip_serializing_if = "Option::is_none")]
        title: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// Queue priority. Updating it is the shared reorder operation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        priority: Option<i32>,
    },
    DeleteTask {
        project_id: ProjectId,
        task_id: TaskId,
    },
    DeleteAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    DeleteProject {
        project_id: ProjectId,
    },
    AssignTask {
        project_id: ProjectId,
        task_id: TaskId,
        #[serde(skip_serializing_if = "Option::is_none")]
        agent_id: Option<AgentId>,
    },
    /// Records operator cancellation intent. The attempt moves to
    /// finalizing; only the durable finalizer may make it terminal.
    CancelRun {
        project_id: ProjectId,
        run_id: RunId,
        grace_ms: u64,
    },
    /// Proposes success for the exact running attempt derived from the outer
    /// credential. Callers cannot name another project, task, or run.
    CompleteAttempt {
        result: String,
    },
    /// Proposes a block for the exact running attempt derived from the outer
    /// credential. Callers cannot name another project, task, or run.
    BlockAttempt {
        reason: String,
    },
    /// Durably holds an agent's queue: the daemon admits no new work for it
    /// until `ResumeAgent`.
    PauseAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ResumeAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    /// One `factoryctl hook` invocation, forwarded verbatim from a
    /// provider's hook subprocess. The outer credential resolves the exact
    /// attempt; `payload` is the hook's JSON body, opaque here and
    /// at most 64 KiB — the daemon extracts whatever fields (provider thread
    /// id, prompt, message, tool name, ...) the event
    /// needs.
    ProviderHook {
        event: ProviderHookEvent,
        payload: serde_json::Value,
    },
    ListRuns {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<RunId>,
        limit: u32,
    },
    ListChanges {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<ChangeId>,
        limit: u32,
    },
    /// Requests identity-safe removal of one retained Change. The caller can
    /// name no source path; a stale inventory revision fails explicitly.
    RemoveChange {
        project_id: ProjectId,
        change_id: ChangeId,
        expected_revision: i64,
    },
    /// Lists quarantined metadata for pre-kernel source paths. These records
    /// are never filesystem ownership claims.
    ListLegacySources {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<LegacySourceId>,
        limit: u32,
    },
    /// Forgets only one legacy metadata row. No filesystem path is accepted
    /// and factoryd performs no filesystem operation for this request.
    ForgetLegacySource {
        project_id: ProjectId,
        legacy_source_id: LegacySourceId,
    },
    EventsAfter {
        sequence: i64,
        limit: u32,
    },
    LatestEventSequence,
    Subscribe {
        after_sequence: i64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    Unauthorized,
    UnsupportedProtocol,
    NotFound,
    Conflict,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LocalResponse {
    /// The resolved, canonical paths `factoryd` is actually using for its
    /// two sibling binaries -- both already confirmed executable at
    /// startup by the daemon's own preflight (see `factoryd::main`'s
    /// `preflight_sibling_binaries`), so a healthy response is always a
    /// guarantee, not just a snapshot of configuration -- and the daemon's
    /// own version (`CARGO_PKG_VERSION`; empty from a daemon older than
    /// this field), so `factoryctl update`/`doctor` can tell whether the
    /// binaries on disk are the ones actually running.
    Health {
        runner_path: String,
        factoryctl_path: String,
        #[serde(default)]
        version: String,
        /// The daemon process identity, used by managed-service reloads to
        /// reject an unrelated manual daemon answering on the same socket.
        #[serde(default)]
        process_id: u32,
    },
    AutoModeSet {
        enabled: bool,
    },
    FleetStatus {
        status: crate::status::FleetStatus,
    },
    RustStorageStatus {
        storage: RustStorageSnapshot,
    },
    AgentStatus {
        status: Box<crate::status::AgentStatusDetail>,
    },
    ProjectCreated {
        project: ProjectSnapshot,
    },
    ProjectCompletionVerificationUpdated {
        project: ProjectSnapshot,
    },
    Projects {
        projects: Vec<ProjectSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<ProjectId>,
    },
    Project {
        project: ProjectDetail,
    },
    ProjectGuidanceUpdated {
        project: ProjectDetail,
    },
    TaskCreated {
        task: TaskDetail,
    },
    Task {
        task: TaskDetail,
    },
    TaskRetried {
        task: TaskDetail,
    },
    TaskCancelled {
        task: TaskDetail,
    },
    TaskUpdated {
        task: TaskDetail,
    },
    TaskDeleted {
        project_id: ProjectId,
        task_id: TaskId,
    },
    AgentDeleted {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ProjectDeleted {
        project_id: ProjectId,
    },
    TaskAssigned {
        task: TaskDetail,
    },
    AgentCreated {
        agent: AgentSnapshot,
    },
    Agent {
        agent: AgentDetail,
    },
    AgentProfileUpdated {
        agent: AgentDetail,
    },
    AgentBudgetUpdated {
        budget: crate::AgentBudget,
    },
    AgentMessageSent {
        message: AgentMessage,
    },
    AgentMessages {
        messages: Vec<AgentMessage>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<MessageId>,
    },
    Agents {
        agents: Vec<AgentSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<AgentId>,
    },
    RunCancelled {
        run_id: RunId,
    },
    /// The requested immutable outcome was accepted and attempt authority was
    /// revoked. The task remains running until any configured completion check
    /// finishes and the daemon-owned finalizer releases every resource; a
    /// requested success can therefore terminalize as unverifiable failure.
    AttemptFinalizing {
        run_id: RunId,
    },
    AgentPaused {
        agent: AgentSnapshot,
    },
    AgentResumed {
        agent: AgentSnapshot,
    },
    /// The `reply` JSON is printed verbatim to stdout by `factoryctl hook`.
    ProviderHookReply {
        reply: serde_json::Value,
    },
    Tasks {
        tasks: Vec<TaskDetail>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<TaskId>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        queue_revision: Option<i64>,
    },
    Runs {
        runs: Vec<RunSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<RunId>,
    },
    Changes {
        changes: Vec<ChangeSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<ChangeId>,
        project_storage: ChangeStorageSnapshot,
        factory_storage: ChangeStorageSnapshot,
        hard_factory_count_cap: u64,
    },
    ChangeRemovalStarted {
        change: ChangeSnapshot,
    },
    LegacySources {
        sources: Vec<LegacySourceSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<LegacySourceId>,
    },
    LegacySourceForgotten {
        project_id: ProjectId,
        legacy_source_id: LegacySourceId,
    },
    Events {
        events: Vec<EventEnvelope>,
    },
    EventHead {
        sequence: i64,
    },
    Subscribed {
        after_sequence: i64,
        replay_through: i64,
    },
    CaughtUp {
        sequence: i64,
    },
    Error {
        code: ErrorCode,
        message: String,
    },
}

/// One newline-delimited frame sent by the daemon.
///
/// `large_enum_variant` is allowed deliberately: these are one-shot control
/// frames on an already-buffered socket write, not a hot allocation path, and
/// boxing individual response/event payloads would ripple through every
/// construction and pattern match across the daemon, CLI, and TUI for no
/// runtime benefit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum ServerFrame {
    Response {
        protocol_version: u16,
        response: LocalResponse,
    },
    Event {
        protocol_version: u16,
        event: EventEnvelope,
    },
}

impl ServerFrame {
    #[must_use]
    pub const fn protocol_version(&self) -> u16 {
        match self {
            Self::Response {
                protocol_version, ..
            }
            | Self::Event {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}
