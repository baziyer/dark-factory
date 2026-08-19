//! Shared domain types and wire protocol for Dark Factory.
//!
//! These types cross process boundaries. Keep them data-only and stable: the
//! daemon owns behavior, while runners, the CLI, and the UI exchange snapshots
//! and events defined here.

use std::{cmp::Ordering, error::Error, fmt};

use serde::{Deserialize, Deserializer, Serialize};

pub mod attention;
pub mod local;
pub mod model_policy;
pub mod paths;
pub mod runner;
pub mod status;

/// Local API wire version. Bump for new request/response variants so an older
/// daemon rejects a newer client explicitly instead of misreading its JSON.
pub const PROTOCOL_VERSION: u16 = 2;
const MAX_ID_LEN: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidId;

impl fmt::Display for InvalidId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("IDs must be 1-128 ASCII letters, digits, hyphens, or underscores")
    }
}

impl Error for InvalidId {}

fn is_valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID_LEN
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

macro_rules! id_type {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl TryFrom<&str> for $name {
            type Error = InvalidId;

            fn try_from(value: &str) -> Result<Self, Self::Error> {
                if is_valid_id(value) {
                    Ok(Self(value.to_owned()))
                } else {
                    Err(InvalidId)
                }
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidId;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                if is_valid_id(&value) {
                    Ok(Self(value))
                } else {
                    Err(InvalidId)
                }
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::try_from(value).map_err(serde::de::Error::custom)
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                self.as_str()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }
    };
}

id_type!(ProjectId);
id_type!(TaskId);
id_type!(AgentId);
id_type!(MessageId);
id_type!(RunId);
id_type!(RunnerInstanceId);
id_type!(SessionId);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    ClaudeCode,
    Codex,
    /// A plain POSIX shell, driven entirely by `factoryctl hook`/`task
    /// done`/`task blocked` calls the launched command makes itself. The
    /// minimal example provider (`crates/factoryd/src/providers/shell.rs`,
    /// `crates/factoryd/tests/fixtures/shell-agent.sh`): no native hooks or
    /// permission prompts to speak of, useful for deterministic lifecycle
    /// tests and as a template for a new provider.
    Shell,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Orchestrator,
    Worker,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Starting,
    Running,
    Waiting,
    Blocked,
    Paused,
    Succeeded,
    Failed,
    Stopped,
}

impl RunStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Stopped)
    }
}

/// Whether the daemon can currently observe an exact runner instance.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverHealth {
    #[default]
    Unknown,
    Healthy,
    Degraded,
}

/// Lifecycle state of one resident interactive provider process (one per
/// agent, PTY-backed, spanning many task-episodes).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Starting,
    Idle,
    Working,
    WaitingForInput,
    Stopped,
    Failed,
}

impl SessionState {
    #[must_use]
    pub const fn is_live(self) -> bool {
        !matches!(self, Self::Stopped | Self::Failed)
    }
}

/// The provider hook event a `factoryctl hook` invocation was called for.
///
/// `PermissionRequest` is Codex-only (added in Codex 0.147.0, alongside the
/// existing events every provider wires): Codex fires it when its own
/// approval prompt is about to block the session on the operator (a shell
/// command, a file edit, ...), before that tool call's own `PreToolUse`.
/// Claude Code has no equivalent event name — its permission prompts
/// already surface through `Notification` — so `PermissionRequest` is
/// wired only into the Codex provider's generated hooks
/// (`providers::codex::hooks_block_toml`), not the shared
/// `providers::hooks::HOOK_EVENTS` both providers iterate. Either way the
/// daemon only observes `PermissionRequest`; auto mode avoids the native
/// prompt, while the separate `PreToolUse` hook is where the daemon answers
/// its own allow/deny policy.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHookEvent {
    SessionStart,
    UserPromptSubmit,
    PreToolUse,
    PermissionRequest,
    PostToolUse,
    Notification,
    Stop,
    SubagentStop,
    SessionEnd,
}

impl ProviderHookEvent {
    /// The exact event name Claude Code and Codex use in their own hook
    /// wire protocols and configuration files (`SessionStart`,
    /// `UserPromptSubmit`, ...) — independent of this enum's own
    /// `snake_case` wire serialization used inside `LocalRequest::
    /// ProviderHook`. This is what `factoryctl hook <Event>` accepts as its
    /// positional argument and what generated provider hook commands are
    /// invoked with.
    #[must_use]
    pub const fn provider_event_name(self) -> &'static str {
        match self {
            Self::SessionStart => "SessionStart",
            Self::UserPromptSubmit => "UserPromptSubmit",
            Self::PreToolUse => "PreToolUse",
            Self::PermissionRequest => "PermissionRequest",
            Self::PostToolUse => "PostToolUse",
            Self::Notification => "Notification",
            Self::Stop => "Stop",
            Self::SubagentStop => "SubagentStop",
            Self::SessionEnd => "SessionEnd",
        }
    }

    /// Parses the exact provider event name back into this enum. Inverse of
    /// [`Self::provider_event_name`]. Returns `None` for anything else,
    /// including this enum's own `snake_case` serialization.
    #[must_use]
    pub fn parse_provider_event_name(value: &str) -> Option<Self> {
        Some(match value {
            "SessionStart" => Self::SessionStart,
            "UserPromptSubmit" => Self::UserPromptSubmit,
            "PreToolUse" => Self::PreToolUse,
            "PermissionRequest" => Self::PermissionRequest,
            "PostToolUse" => Self::PostToolUse,
            "Notification" => Self::Notification,
            "Stop" => Self::Stop,
            "SubagentStop" => Self::SubagentStop,
            "SessionEnd" => Self::SessionEnd,
            _ => return None,
        })
    }
}

/// Why a run (task-episode) was closed.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunClosedBy {
    TaskDone,
    TaskBlocked,
    OperatorCancel,
    OperatorStop,
    SessionEnded,
}

/// A durable, privacy-safe category for a failed run.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunFailureReason {
    Protocol,
    Provider,
    Permission,
    Limit,
    Process,
    Spawn,
    Incomplete,
    Unverifiable,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Queued,
    Running,
    Blocked,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskStatus {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// The one active assigned-queue order shared by the daemon scheduler and
/// every client projection: a currently running task first, then queued
/// work by descending priority and finally creation time/id.
#[must_use]
pub fn active_task_cmp(a: &TaskSnapshot, b: &TaskSnapshot) -> Ordering {
    (a.status != TaskStatus::Running)
        .cmp(&(b.status != TaskStatus::Running))
        .then_with(|| b.priority.cmp(&a.priority))
        .then_with(|| a.created_at_ms.cmp(&b.created_at_ms))
        .then_with(|| a.id.as_str().cmp(b.id.as_str()))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectSnapshot {
    pub id: ProjectId,
    pub name: String,
    pub root: String,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskSnapshot {
    pub id: TaskId,
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_task_id: Option<TaskId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assigned_agent_id: Option<AgentId>,
    pub title: String,
    pub status: TaskStatus,
    pub priority: i32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// A task snapshot together with the instructions supplied to its agent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskDetail {
    pub snapshot: TaskSnapshot,
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    /// Why `factoryctl task blocked` was called, when `snapshot.status` is
    /// [`TaskStatus::Blocked`]; `None` otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

/// A durable agent identity. Process-attempt state belongs to [`RunSnapshot`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentSnapshot {
    pub id: AgentId,
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<AgentId>,
    pub role: AgentRole,
    pub provider: Provider,
    /// The active attempt, or `None` when this agent is idle.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<RunId>,
    /// Durable operator hold: while `true`, the daemon does not deliver new
    /// work into this agent's session.
    #[serde(default)]
    pub paused: bool,
    /// The agent's current resident session, or `None` when it has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_session_id: Option<SessionId>,
    /// Absolute path to the agent's git worktree, once created.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable provider budget. Tool calls are counted from authenticated
/// `PreToolUse` hooks. Providers do not currently expose trustworthy
/// per-agent monetary spend, so it is explicitly unavailable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentBudget {
    pub max_tool_calls: Option<u64>,
    pub tool_calls: u64,
    pub exhausted: bool,
    pub reset_at_ms: i64,
    pub updated_at_ms: i64,
    /// Always `null` on the wire until a provider exposes authoritative
    /// per-agent monetary accounting.
    #[serde(default)]
    pub monetary_spend: Option<u64>,
}

impl Default for AgentBudget {
    fn default() -> Self {
        Self {
            max_tool_calls: Some(1000),
            tool_calls: 0,
            exhausted: false,
            reset_at_ms: 0,
            updated_at_ms: 0,
            monetary_spend: None,
        }
    }
}

/// One process attempt. Terminal runs remain durable after the agent goes idle.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RunSnapshot {
    pub id: RunId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<RunId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    /// The resident session this episode ran inside. `None` only for runs
    /// that predate the sessions migration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub status: RunStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    pub worktree: String,
    #[serde(default)]
    pub observer_health: ObserverHealth,
    #[serde(default)]
    pub observer_health_since_ms: i64,
    pub started_at_ms: i64,
    pub status_since_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_reason: Option<RunFailureReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_by: Option<RunClosedBy>,
}

/// One resident interactive provider process for one agent. Many task
/// episodes ([`RunSnapshot`]) happen inside one session's lifetime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionSnapshot {
    pub id: SessionId,
    pub project_id: ProjectId,
    pub agent_id: AgentId,
    pub provider: Provider,
    /// The exact values Dark Factory could establish for this session at
    /// launch. `None` is deliberately unreported, not a guessed provider
    /// default. These fields are session-owned so ended sessions retain the
    /// values they actually used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_control_mode: Option<String>,
    pub state: SessionState,
    pub state_since_ms: i64,
    pub worktree: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_session_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_run_id: Option<RunId>,
    /// Bounded free-text activity label (e.g. `"tool: Read"`), or `None`
    /// while idle/starting/stopped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    /// Whether `activity` was inferred from a generic hook (`true`, shown
    /// with a `~` by the TUI) rather than naming an exact tool (`false`).
    #[serde(default)]
    pub activity_inferred: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hook_event: Option<ProviderHookEvent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hook_at_ms: Option<i64>,
    /// Bounded operator-facing explanation of why the session is waiting,
    /// e.g. "permission prompt", "delivery unacknowledged"; at most 512
    /// bytes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default)]
    pub observer_health: ObserverHealth,
    #[serde(default)]
    pub observer_health_since_ms: i64,
    pub started_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum FactoryEvent {
    /// Factory-wide autonomy posture changed. Policy decisions are recorded
    /// separately so the event ledger explains both configuration and use.
    AutoModeChanged {
        enabled: bool,
    },
    /// The daemon's answer to one provider `PreToolUse` hook.
    PolicyDecision {
        project_id: ProjectId,
        agent_id: AgentId,
        session_id: SessionId,
        tool_name: String,
        decision: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        rule: Option<String>,
    },
    /// A budget was configured, observed, exhausted, or reset.
    AgentBudgetChanged {
        project_id: ProjectId,
        agent_id: AgentId,
        budget: AgentBudget,
        action: String,
        /// Effective pause after this transition, including independent holds.
        paused: bool,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        pause_reasons: Vec<crate::status::AgentPauseReason>,
    },
    /// Request/result audit for a daemon-owned repository operation. This
    /// deliberately contains neither credentials, commit messages, PR
    /// bodies, nor diff output.
    RepositoryOperation {
        project_id: ProjectId,
        agent_id: AgentId,
        session_id: SessionId,
        operation: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    RepositoryAuthorityChanged {
        project_id: ProjectId,
    },
    ProjectChanged {
        project: ProjectSnapshot,
    },
    TaskChanged {
        task: TaskSnapshot,
    },
    AgentChanged {
        agent: AgentSnapshot,
    },
    RunChanged {
        run: RunSnapshot,
    },
    SessionChanged {
        session: SessionSnapshot,
    },
    /// A task was permanently removed. Unlike `TaskChanged`, there is no
    /// surviving snapshot to publish.
    TaskDeleted {
        project_id: ProjectId,
        task_id: TaskId,
    },
    /// An agent was permanently removed.
    AgentDeleted {
        project_id: ProjectId,
        agent_id: AgentId,
    },
    /// A project and everything scoped to it was permanently removed.
    ProjectDeleted {
        project_id: ProjectId,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventEnvelope {
    pub protocol_version: u16,
    pub sequence: i64,
    pub occurred_at_ms: i64,
    pub event: FactoryEvent,
}

#[cfg(test)]
mod tests {
    use super::ProviderHookEvent;

    #[test]
    fn provider_event_name_round_trips_every_variant() {
        let events = [
            ProviderHookEvent::SessionStart,
            ProviderHookEvent::UserPromptSubmit,
            ProviderHookEvent::PreToolUse,
            ProviderHookEvent::PermissionRequest,
            ProviderHookEvent::PostToolUse,
            ProviderHookEvent::Notification,
            ProviderHookEvent::Stop,
            ProviderHookEvent::SubagentStop,
            ProviderHookEvent::SessionEnd,
        ];
        for event in events {
            let name = event.provider_event_name();
            assert_eq!(
                ProviderHookEvent::parse_provider_event_name(name),
                Some(event)
            );
        }
    }

    #[test]
    fn provider_event_name_is_exact_pascal_case_not_this_enums_own_snake_case_wire_form() {
        assert_eq!(
            ProviderHookEvent::SessionStart.provider_event_name(),
            "SessionStart"
        );
        assert_eq!(
            ProviderHookEvent::SubagentStop.provider_event_name(),
            "SubagentStop"
        );
        assert_eq!(
            ProviderHookEvent::parse_provider_event_name("session_start"),
            None
        );
        assert_eq!(ProviderHookEvent::parse_provider_event_name("stop"), None);
        assert_eq!(ProviderHookEvent::parse_provider_event_name(""), None);
    }
}
