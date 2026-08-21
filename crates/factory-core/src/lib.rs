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
/// Durable event envelopes retain their own stored schema version and may be
/// older than this outer frame version during an upgrade.
pub const PROTOCOL_VERSION: u16 = 4;
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
id_type!(ChangeId);
id_type!(LegacySourceId);

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
pub enum RunPhase {
    Admitted,
    Running,
    Finalizing,
    Terminal,
}

/// Project policy applied when a worker requests a successful attempt outcome.
/// Non-success outcomes never require a verification build.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletionVerification {
    #[default]
    None,
    RustWorkspaceTest,
}

/// Durable lifecycle of one daemon-owned writable source tree.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangePhase {
    Provisioning,
    Available,
    Removing,
    Removed,
}

/// Bounded operator projection of one retained Change. Filesystem identity is
/// deliberately private to factoryd; callers select only this typed ID and an
/// exact revision when requesting removal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeSnapshot {
    pub id: ChangeId,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub task_incarnation_id: String,
    pub phase: ChangePhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_oid: Option<String>,
    pub revision: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub removed_at_ms: Option<i64>,
}

/// One coherent storage projection for either a project or the whole factory.
/// Measurements are absent unless every retained Change in the scope is
/// quiescent and measured.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeStorageSnapshot {
    pub retained_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at_ms: Option<i64>,
    pub active_leases: u64,
    pub complete: bool,
}

/// Metadata-only record for a source path retained from the pre-kernel
/// architecture. Factoryd never adopts, stats, measures, leases, renames, or
/// deletes this path; an operator may only forget the database record by ID.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacySourceSnapshot {
    pub id: LegacySourceId,
    pub project_id: ProjectId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub former_agent_id: Option<AgentId>,
    pub source_path: String,
    pub retained_reason: String,
    pub recorded_at_ms: i64,
}

impl RunPhase {
    /// Whether the durable phase transition is permitted by the attempt
    /// state machine. Repeating the current phase is not a transition.
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Admitted, Self::Running | Self::Finalizing)
                | (Self::Running, Self::Finalizing)
                | (Self::Finalizing, Self::Terminal)
        )
    }

    #[must_use]
    pub const fn grants_attempt_authority(self) -> bool {
        matches!(self, Self::Running)
    }
}

/// Outcome proposed by the transition to [`RunPhase::Finalizing`] and selected
/// by the finalizer for [`RunPhase::Terminal`]. A successful proposal may
/// become `Failed(Unverifiable)` when the project's completion check fails;
/// blocked, failed, and cancelled proposals remain exact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RunOutcome {
    Succeeded,
    Blocked { reason: String },
    Failed { reason: RunFailureReason },
    Cancelled { reason: String },
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

/// The provider hook event accepted by `factoryctl hook`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHookEvent {
    PreToolUse,
}

impl ProviderHookEvent {
    /// The exact event name Claude Code and Codex use in their own hook
    /// wire protocols and configuration files — independent of this enum's own
    /// `snake_case` wire serialization used inside `LocalRequest::
    /// ProviderHook`. This is what `factoryctl hook <Event>` accepts as its
    /// positional argument and what generated provider hook commands are
    /// invoked with.
    #[must_use]
    pub const fn provider_event_name(self) -> &'static str {
        "PreToolUse"
    }

    /// Parses the exact provider event name back into this enum. Inverse of
    /// [`Self::provider_event_name`]. Returns `None` for anything else,
    /// including this enum's own `snake_case` serialization.
    #[must_use]
    pub fn parse_provider_event_name(value: &str) -> Option<Self> {
        (value == "PreToolUse").then_some(Self::PreToolUse)
    }
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
    #[serde(default)]
    pub completion_verification: CompletionVerification,
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
    /// The immutable attempt block reason projected by the finalizer when
    /// `snapshot.status` is [`TaskStatus::Blocked`]; `None` otherwise.
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
    /// Durable operator hold: while `true`, the daemon does not admit new
    /// work for this agent.
    #[serde(default)]
    pub paused: bool,
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
    pub task_id: TaskId,
    pub provider: Provider,
    pub phase: RunPhase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<RunOutcome>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_instance_id: Option<RunnerInstanceId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_reasoning_effort: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_control_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_reason: Option<String>,
    #[serde(default)]
    pub observer_health: ObserverHealth,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observer_reason: Option<String>,
    pub admitted_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at_ms: Option<i64>,
    pub phase_since_ms: i64,
    pub updated_at_ms: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_signal: Option<i32>,
}

impl RunSnapshot {
    /// During finalization `outcome` is the immutable requested outcome. At
    /// terminal it is the daemon-selected actual outcome after any completion
    /// verification. It is absent while the attempt still has effect authority.
    #[must_use]
    pub const fn has_valid_phase_outcome(&self) -> bool {
        matches!(
            (self.phase, self.outcome.is_some()),
            (RunPhase::Admitted | RunPhase::Running, false)
                | (RunPhase::Finalizing | RunPhase::Terminal, true)
        )
    }
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
        run_id: RunId,
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
    /// Decode-only compatibility for repository audit events written before
    /// the Stage 1 repository execution surface was removed.
    #[serde(rename = "repository_operation")]
    LegacyRepositoryOperation {
        project_id: ProjectId,
        agent_id: AgentId,
        run_id: RunId,
        operation: String,
        phase: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        success: Option<bool>,
        #[serde(skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
    },
    /// Decode-only compatibility for the deleted project repository-authority
    /// setting. Stage 2 has no constructor for this historical event.
    #[serde(rename = "repository_authority_changed")]
    LegacyRepositoryAuthorityChanged {
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
        run: Box<RunSnapshot>,
    },
    /// A daemon-owned source Change was reserved or durably advanced.
    ChangeChanged {
        change: ChangeSnapshot,
    },
    /// Metadata-only removal of one quarantined pre-kernel source record.
    LegacySourceForgotten {
        project_id: ProjectId,
        legacy_source_id: LegacySourceId,
    },
    /// Decode-only projection for `run_changed` events written before the
    /// attempt-kernel wire change. The store derives these identities from
    /// the event index rather than reviving the old resident-session run
    /// model or exposing its worktree and process metadata.
    LegacyRunChanged {
        project_id: ProjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        task_id: Option<TaskId>,
        agent_id: AgentId,
        run_id: RunId,
    },
    /// Decode-only projection for historical resident-session events. The
    /// store derives these identities from the event index and never parses
    /// the obsolete session/process snapshot.
    LegacySessionChanged {
        project_id: ProjectId,
        agent_id: AgentId,
        #[serde(skip_serializing_if = "Option::is_none")]
        run_id: Option<RunId>,
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
    fn provider_event_name_round_trips() {
        let event = ProviderHookEvent::PreToolUse;
        assert_eq!(
            ProviderHookEvent::parse_provider_event_name(event.provider_event_name()),
            Some(event)
        );
    }

    #[test]
    fn provider_event_name_is_exact_pascal_case_not_this_enums_own_snake_case_wire_form() {
        assert_eq!(
            ProviderHookEvent::PreToolUse.provider_event_name(),
            "PreToolUse"
        );
        assert_eq!(
            ProviderHookEvent::parse_provider_event_name("pre_tool_use"),
            None
        );
        assert_eq!(ProviderHookEvent::parse_provider_event_name("stop"), None);
        assert_eq!(ProviderHookEvent::parse_provider_event_name(""), None);
    }
}
