//! Versioned request/response protocol for the local control socket.

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentRole, AgentSnapshot, EventEnvelope, PROTOCOL_VERSION, ProjectId, ProjectSnapshot,
    Provider, RunId, RunSnapshot, TaskDetail, TaskId,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionLimitWindow {
    Primary,
    Secondary,
    CurrentSession,
    CurrentWeek,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionFailureCategory {
    Timeout,
    Protocol,
    Process,
    OutputLimit,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum SubscriptionProbeOutcome {
    Observed {
        used_percent: u8,
        limit_window: SubscriptionLimitWindow,
        #[serde(skip_serializing_if = "Option::is_none")]
        resets_at_ms: Option<i64>,
        exhausted: bool,
    },
    Failed {
        category: SubscriptionFailureCategory,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionSeverity {
    Ok,
    Warning,
    Critical,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionProviderStatus {
    pub provider: Provider,
    pub last_attempt_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_window: Option<SubscriptionLimitWindow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exhausted: Option<bool>,
    pub severity: SubscriptionSeverity,
    pub consecutive_failures: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionUsageStatus {
    pub overall_severity: SubscriptionSeverity,
    pub providers: Vec<SubscriptionProviderStatus>,
}

/// Maximum JSON payload size. The newline delimiter is not part of this limit.
pub const MAX_LOCAL_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_PROJECT_PAGE_ITEMS: u32 = 100;
pub const MAX_TASK_PAGE_ITEMS: u32 = 10;
pub const MAX_AGENT_PAGE_ITEMS: u32 = 100;
pub const MAX_RUN_PAGE_ITEMS: u32 = 100;
pub const MAX_EVENT_PAGE_ITEMS: u32 = 100;
pub const MAX_TASK_BODY_BYTES: usize = 64 * 1024;

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
        worktree: String,
    },
    ListTasks {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_id: Option<TaskId>,
        limit: u32,
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
    SubscriptionUsage,
    RecordSubscriptionProbe {
        project_id: ProjectId,
        orchestrator_agent_id: AgentId,
        provider: Provider,
        attempted_at_ms: i64,
        outcome: SubscriptionProbeOutcome,
        notification_task_id: TaskId,
    },
    Subscribe {
        after_sequence: i64,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedProtocol,
    NotFound,
    Conflict,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LocalResponse {
    Health,
    ProjectCreated {
        project: ProjectSnapshot,
    },
    Projects {
        projects: Vec<ProjectSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<ProjectId>,
    },
    TaskCreated {
        task: TaskDetail,
    },
    AgentCreated {
        agent: AgentSnapshot,
    },
    Agents {
        agents: Vec<AgentSnapshot>,
        #[serde(skip_serializing_if = "Option::is_none")]
        next_after_id: Option<AgentId>,
    },
    RunAccepted {
        run_id: RunId,
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
    SubscriptionUsage {
        usage: SubscriptionUsageStatus,
    },
    SubscriptionProbeRecorded {
        provider: Provider,
        severity: SubscriptionSeverity,
        consecutive_failures: u32,
        notification_created: bool,
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
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
