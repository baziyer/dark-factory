//! Versioned request/response protocol for the local control socket.

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, MessageId, PROTOCOL_VERSION, ProjectId,
    ProjectSnapshot, Provider, ProviderHookEvent, RunId, RunSnapshot, SessionId, SessionSnapshot,
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
    /// Provider-scoped permission mode (Claude: `default`/`acceptEdits`/
    /// `plan`; Codex: `on-request`/`never`); `None` is the provider default.
    /// Stored and shown; not yet consumed by launch.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    pub instructions: String,
    pub memory: String,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentDetail {
    pub snapshot: AgentSnapshot,
    pub profile: AgentProfile,
    /// Absolute path to this agent's standing-guidance file, editable
    /// directly with `$EDITOR`.
    pub instructions_path: String,
    /// Absolute path to this agent's memory file. The agent itself is
    /// expected to append durable lessons here.
    pub memory_path: String,
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
pub const MAX_SESSION_PAGE_ITEMS: u32 = 1000;
pub const MAX_EVENT_PAGE_ITEMS: u32 = 1000;
pub const MAX_TASK_TITLE_BYTES: usize = 240;
pub const MAX_TASK_BODY_BYTES: usize = 64 * 1024;
pub const MAX_TERMINAL_OUTPUT_BYTES: usize = 64 * 1024;
pub const MAX_AGENT_MESSAGE_BYTES: usize = 64 * 1024;

/// Applies the task-title contract shared by every local and connector write.
#[must_use]
pub fn normalize_task_title(value: String) -> Option<String> {
    let value = value.trim().to_owned();
    (!value.is_empty() && value.len() <= MAX_TASK_TITLE_BYTES).then_some(value)
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunTerminal {
    pub run_id: RunId,
    pub head_sequence: i64,
    pub output: String,
    pub truncated: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequestEnvelope {
    pub protocol_version: u16,
    pub request: LocalRequest,
}

impl RequestEnvelope {
    #[must_use]
    pub const fn new(request: LocalRequest) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
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
    /// project's agents with their sessions, runs, and queues, plus the
    /// attention list and the live-session cap. See [`crate::status`].
    FleetStatus,
    /// One agent's live picture plus its profile and worktree git state
    /// (`factoryctl agent status`).
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
    SetProjectRepositoryAuthority {
        project_id: ProjectId,
        remote_url: String,
        base_branch: String,
    },
    CreateTask {
        id: TaskId,
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_task_id: Option<TaskId>,
        title: String,
        body: String,
        priority: i32,
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
        /// Overrides the daemon-managed per-agent worktree with an existing
        /// absolute path. Not yet implemented; see local_api.rs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender_agent_id: Option<AgentId>,
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
    StartTask {
        project_id: ProjectId,
        task_id: TaskId,
        agent_id: AgentId,
        #[serde(skip_serializing_if = "Option::is_none")]
        parent_run_id: Option<RunId>,
        /// Defaults to the agent's own worktree once per-agent worktrees
        /// exist; until then a request without one is rejected. Semantics:
        /// "deliver now / put at the front of the agent's queue".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        worktree: Option<String>,
    },
    ListTasks {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<TaskId>,
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
    GetRunTerminal {
        project_id: ProjectId,
        run_id: RunId,
    },
    StopRun {
        project_id: ProjectId,
        run_id: RunId,
        grace_ms: u64,
    },
    /// Closes an open run (task-episode) without touching its session's
    /// process: `runs.status → stopped`, `closed_by = operator_cancel`. For
    /// v1 this does not interrupt an in-flight turn.
    CancelRun {
        project_id: ProjectId,
        run_id: RunId,
    },
    /// Marks a task's episode succeeded from inside its session: the run
    /// closes with `closed_by = task_done`, the task becomes `succeeded`.
    CompleteTask {
        project_id: ProjectId,
        task_id: TaskId,
        /// Bounded like today's persisted task result.
        result: String,
    },
    /// Marks a task's episode blocked from inside its session: the run
    /// closes with `closed_by = task_blocked`, the task becomes `blocked`.
    BlockTask {
        project_id: ProjectId,
        task_id: TaskId,
        /// At most 4096 bytes.
        reason: String,
    },
    /// Durably holds an agent's queue: the daemon stops delivering new work
    /// into its session until `ResumeAgent`.
    PauseAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ResumeAgent {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    ListSessions {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<SessionId>,
        #[serde(skip_serializing_if = "Option::is_none")]
        limit: Option<usize>,
    },
    /// Kills a session's PTY-backed provider process group (graceful, like
    /// `StopRun`); any open run closes with `closed_by = operator_stop`.
    StopSession {
        project_id: ProjectId,
        session_id: SessionId,
        grace_ms: u64,
    },
    /// One `factoryctl hook` invocation, forwarded verbatim from a
    /// provider's hook subprocess. `token` is the contents of the session's
    /// `hook.token` file; `payload` is the hook's JSON body, opaque here and
    /// at most 64 KiB — the daemon extracts whatever fields (session/thread
    /// id, prompt, message, tool name, `stop_hook_active`, ...) the event
    /// needs.
    ProviderHook {
        token: String,
        event: ProviderHookEvent,
        payload: serde_json::Value,
    },
    /// Read-only git state for the calling authenticated session's exact
    /// daemon-managed worktree. `token` is read from the session token file
    /// by `factoryctl`; callers never select an agent, project, or path.
    GitStatus {
        token: String,
    },
    GitDiff {
        token: String,
        staged: bool,
    },
    /// Stage every change in the session worktree and create one commit.
    GitCommit {
        token: String,
        message: String,
    },
    /// Push only the session's current `agent/<id>` branch, without force.
    GitPush {
        token: String,
    },
    /// Open a PR from the session branch to the repository's default base.
    PrOpen {
        token: String,
        title: String,
        body: String,
    },
    /// Update only a PR whose head is the calling session's branch.
    PrUpdate {
        token: String,
        number: u64,
        title: String,
        body: String,
    },
    /// Attaches to a terminal-mode session's retained-then-live PTY output
    /// on this connection. The connection commits to terminal-proxy mode:
    /// after the first `AttachTerminal`, only more `AttachTerminal` requests
    /// are accepted on it (a client may attach several sessions on one
    /// connection). A successful attach produces at least one
    /// `ServerFrame::TerminalOutput` (possibly with zero bytes), then output
    /// frames for every attached session are multiplexed onto it, tagged by
    /// `session_id`.
    /// Detaching happens implicitly on disconnect. `TerminalInput` and
    /// `ResizeTerminal` are independent, ordinary one-shot requests (like
    /// `StopRun`); they do not need to run on an attached connection.
    AttachTerminal {
        project_id: ProjectId,
        session_id: SessionId,
        since_offset: u64,
    },
    /// Writes operator input to a session's PTY. An ordinary one-shot
    /// request: it does not require a prior or concurrent `AttachTerminal`,
    /// on this connection or any other. `bytes` is base64-encoded raw bytes;
    /// opaque to the daemon.
    TerminalInput {
        project_id: ProjectId,
        session_id: SessionId,
        bytes: String,
    },
    /// Resizes a session's PTY. An ordinary one-shot request, like
    /// `TerminalInput`.
    ResizeTerminal {
        project_id: ProjectId,
        session_id: SessionId,
        cols: u16,
        rows: u16,
    },
    ListRuns {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<RunId>,
        limit: u32,
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
    },
    AutoModeSet {
        enabled: bool,
    },
    FleetStatus {
        status: crate::status::FleetStatus,
    },
    AgentStatus {
        status: Box<crate::status::AgentStatusDetail>,
    },
    GitOutput {
        operation: String,
        output: String,
    },
    ProjectCreated {
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
    ProjectRepositoryAuthoritySet {
        project_id: ProjectId,
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
    RunTerminal {
        terminal: RunTerminal,
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
    RunAccepted {
        run_id: RunId,
    },
    RunStopped {
        run_id: RunId,
    },
    RunCancelled {
        run_id: RunId,
    },
    TaskCompleted {
        task: TaskDetail,
    },
    TaskBlocked {
        task: TaskDetail,
    },
    AgentPaused {
        agent: AgentSnapshot,
    },
    AgentResumed {
        agent: AgentSnapshot,
    },
    Sessions {
        sessions: Vec<SessionSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<SessionId>,
    },
    SessionStopped {
        session_id: SessionId,
    },
    /// The `reply` JSON is printed verbatim to stdout by `factoryctl hook`.
    ProviderHookReply {
        reply: serde_json::Value,
    },
    TerminalInputAccepted {
        session_id: SessionId,
    },
    TerminalResized {
        session_id: SessionId,
    },
    Tasks {
        tasks: Vec<TaskDetail>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<TaskId>,
    },
    Runs {
        runs: Vec<RunSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<RunId>,
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
    /// One chunk of retained-then-live PTY output for a session attached on
    /// this connection via `AttachTerminal`. `bytes` is base64-encoded raw
    /// bytes; opaque to every layer between the runner and the client.
    TerminalOutput {
        protocol_version: u16,
        session_id: SessionId,
        offset: u64,
        bytes: String,
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
            }
            | Self::TerminalOutput {
                protocol_version, ..
            } => *protocol_version,
        }
    }
}
