//! Fleet and per-agent status: what `factoryctl status` and `factoryctl
//! agent status` return, built by `factoryd` from durable state in one
//! store transaction so every field is from the same instant. `factory-tui`
//! reads the same responses; it never computes a status the CLI can't
//! show.
//!
//! Bounded in what it carries — live/most-recent sessions, current runs,
//! queued and blocked tasks (the full task/run/session ledgers stay behind
//! the paginated `List*` requests), per-agent queue previews truncated to
//! [`MAX_QUEUE_PREVIEW`] with the true depth alongside — but not paginated:
//! unassigned queues and blocked tasks are listed in full, so a factory with
//! thousands of open tasks would push one frame past
//! `MAX_LOCAL_FRAME_BYTES`; that is where pagination would go.

use serde::{Deserialize, Serialize};

use crate::{
    AgentId, AgentSnapshot, ProjectId, ProjectSnapshot, RunSnapshot, SessionId, SessionSnapshot,
    TaskId, TaskSnapshot,
    attention::{Attention, session_attention, task_attention},
    local::AgentDetail,
};

/// How many queued tasks an [`AgentStatus`]/[`ProjectStatus`] lists in full
/// before falling back to the depth count alone.
pub const MAX_QUEUE_PREVIEW: usize = 10;

/// The whole daemon at one instant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FleetStatus {
    pub generated_at_ms: i64,
    /// Factory-wide provider bypass. Explicit per-agent permission modes
    /// still override this default.
    #[serde(default)]
    pub auto_mode: bool,
    /// `factoryd --max-active-runs`: the daemon-wide cap on live sessions.
    pub live_session_cap: u32,
    /// Sessions that have not ended, across every project.
    pub live_sessions: u32,
    pub projects: Vec<ProjectStatus>,
    /// Everything that needs an operator right now, most urgent first.
    pub attention: Vec<AttentionItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectStatus {
    pub project: ProjectSnapshot,
    pub agents: Vec<AgentStatus>,
    /// Queued tasks not assigned to any agent (the operator's or an
    /// orchestrator's to hand out).
    pub unassigned_queue_depth: u32,
    pub unassigned_queue: Vec<TaskSnapshot>,
}

/// One agent's live picture: its session (if any), current run, and what is
/// waiting for it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatus {
    pub agent: AgentSnapshot,
    /// Current git state of the agent's working directory. Fleet status
    /// includes this so an orchestrator can inspect work before deciding
    /// whether a stopped or blocked worker is safe to recover.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<RunSnapshot>,
    /// Queued tasks assigned to this agent, oldest first (see
    /// [`MAX_QUEUE_PREVIEW`]).
    pub queue_depth: u32,
    pub queue: Vec<TaskSnapshot>,
    /// Inbox messages not yet delivered into a session.
    pub inbox_pending: u32,
    /// The agent's attention level, and whether it was read from its
    /// session (`inferred == false`) or from its run (no session).
    pub attention: Attention,
    pub attention_inferred: bool,
}

/// `factoryctl agent status`: everything [`AgentStatus`] has, plus the
/// profile and guidance paths, and the worktree's git state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatusDetail {
    pub status: AgentStatus,
    pub detail: AgentDetail,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeStatus>,
}

/// `git status --porcelain --branch` of an agent's worktree, summarized.
/// Present whenever the agent has a worktree; if `git` could not report on
/// it (deleted directory, not a repository, ...) `error` says why and the
/// counts are zero — never silently "clean".
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorktreeStatus {
    pub path: String,
    /// `None` on a detached `HEAD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Modified, staged, or untracked entries.
    pub changed_files: u32,
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    /// A session is waiting for a human answer.
    NeedsInput,
    /// A session ended in failure and its agent is not running.
    SessionFailed,
    /// A task was marked blocked by its agent.
    TaskBlocked,
    /// An agent has queued work but is paused.
    PausedWithWork,
    /// An agent has queued work and no session, and the daemon is at its
    /// live-session cap.
    WaitingForCapacity,
}

/// One thing that needs the operator.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionItem {
    pub kind: AttentionKind,
    pub level: Attention,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    /// When this condition began (session state change, task update, ...).
    pub since_ms: i64,
    pub detail: String,
}

/// Derives the attention list for one project's agents and blocked tasks.
/// Pure over snapshots so `factoryd` (for `factoryctl status`) and any
/// client can agree on it. Unsorted; see [`sort_attention`].
#[must_use]
pub fn attention_items(
    project_id: &ProjectId,
    agents: &[AgentStatus],
    blocked_tasks: &[TaskSnapshot],
    at_capacity: bool,
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for status in agents {
        let agent = &status.agent;
        if let Some(session) = &status.session {
            match session_attention(session.state) {
                Attention::NeedsInput => items.push(AttentionItem {
                    kind: AttentionKind::NeedsInput,
                    level: Attention::NeedsInput,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: None,
                    session_id: Some(session.id.clone()),
                    since_ms: session.state_since_ms,
                    detail: session
                        .wait_reason
                        .clone()
                        .unwrap_or_else(|| "session is waiting for input".to_owned()),
                }),
                Attention::Failed => items.push(AttentionItem {
                    kind: AttentionKind::SessionFailed,
                    level: Attention::Failed,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: None,
                    session_id: Some(session.id.clone()),
                    since_ms: session.state_since_ms,
                    detail: session
                        .wait_reason
                        .clone()
                        .unwrap_or_else(|| "session failed".to_owned()),
                }),
                Attention::Routine | Attention::Completed => {}
            }
        }
        if status.queue_depth > 0 {
            if agent.paused {
                items.push(AttentionItem {
                    kind: AttentionKind::PausedWithWork,
                    level: Attention::NeedsInput,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: status.queue.first().map(|task| task.id.clone()),
                    session_id: None,
                    since_ms: status
                        .queue
                        .first()
                        .map_or(agent.updated_at_ms, |task| task.created_at_ms),
                    detail: format!(
                        "paused with {} queued task(s); `factoryctl agent resume`",
                        status.queue_depth
                    ),
                });
            } else if agent.current_session_id.is_none() && at_capacity {
                items.push(AttentionItem {
                    kind: AttentionKind::WaitingForCapacity,
                    level: Attention::Routine,
                    project_id: project_id.clone(),
                    agent_id: Some(agent.id.clone()),
                    task_id: status.queue.first().map(|task| task.id.clone()),
                    session_id: None,
                    since_ms: status
                        .queue
                        .first()
                        .map_or(agent.updated_at_ms, |task| task.updated_at_ms),
                    detail: format!(
                        "{} queued task(s) but the daemon is at its live-session cap",
                        status.queue_depth
                    ),
                });
            }
        }
    }
    for task in blocked_tasks {
        if task_attention(task.status) == Attention::NeedsInput {
            items.push(AttentionItem {
                kind: AttentionKind::TaskBlocked,
                level: Attention::NeedsInput,
                project_id: project_id.clone(),
                agent_id: task.assigned_agent_id.clone(),
                task_id: Some(task.id.clone()),
                session_id: None,
                since_ms: task.updated_at_ms,
                detail: format!("task \"{}\" is blocked", task.title),
            });
        }
    }
    items
}

/// Most urgent first, then oldest first — the order `factoryctl status`
/// prints and the board would triage in.
pub fn sort_attention(items: &mut [AttentionItem]) {
    items.sort_by(|a, b| {
        b.level
            .cmp(&a.level)
            .then_with(|| a.since_ms.cmp(&b.since_ms))
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentRole, ObserverHealth, Provider, SessionState, TaskStatus};

    fn agent(id: &str, paused: bool) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::try_from(id).unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            current_run_id: None,
            paused,
            current_session_id: None,
            worktree: None,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn session(state: SessionState, since: i64) -> SessionSnapshot {
        SessionSnapshot {
            id: SessionId::try_from("s1").unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            agent_id: AgentId::try_from("a").unwrap(),
            provider: Provider::Shell,
            state,
            state_since_ms: since,
            worktree: "/w".to_owned(),
            provider_session_id: None,
            current_run_id: None,
            activity: None,
            activity_inferred: false,
            last_hook_event: None,
            last_hook_at_ms: None,
            wait_reason: None,
            observer_health: ObserverHealth::Unknown,
            observer_health_since_ms: 0,
            started_at_ms: 0,
            updated_at_ms: 0,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
        }
    }

    fn task(id: &str, status: TaskStatus, updated: i64) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::try_from(id).unwrap(),
            project_id: ProjectId::try_from("p").unwrap(),
            parent_task_id: None,
            assigned_agent_id: Some(AgentId::try_from("a").unwrap()),
            title: format!("task {id}"),
            status,
            priority: 0,
            created_at_ms: 0,
            updated_at_ms: updated,
        }
    }

    fn status(
        agent: AgentSnapshot,
        session: Option<SessionSnapshot>,
        queue: Vec<TaskSnapshot>,
    ) -> AgentStatus {
        AgentStatus {
            queue_depth: u32::try_from(queue.len()).unwrap(),
            attention: session
                .as_ref()
                .map_or(Attention::Routine, |s| session_attention(s.state)),
            attention_inferred: session.is_none(),
            agent,
            worktree: None,
            session,
            current_run: None,
            queue,
            inbox_pending: 0,
        }
    }

    #[test]
    fn attention_orders_needs_input_above_failed_and_oldest_first_within_a_level() {
        let project = ProjectId::try_from("p").unwrap();
        let agents = vec![
            status(
                agent("a", false),
                Some(session(SessionState::Failed, 10)),
                vec![],
            ),
            status(
                agent("b", false),
                Some(session(SessionState::WaitingForInput, 30)),
                vec![],
            ),
            status(
                agent("c", true),
                None,
                vec![task("t1", TaskStatus::Queued, 5)],
            ),
            status(
                agent("d", false),
                None,
                vec![task("t2", TaskStatus::Queued, 40)],
            ),
        ];
        let blocked = vec![task("t3", TaskStatus::Blocked, 20)];
        let mut items = attention_items(&project, &agents, &blocked, true);
        sort_attention(&mut items);
        let kinds: Vec<AttentionKind> = items.iter().map(|item| item.kind).collect();
        assert_eq!(
            kinds,
            [
                AttentionKind::PausedWithWork, // NeedsInput, since 0 (t1's created_at_ms)
                AttentionKind::TaskBlocked,    // NeedsInput, since 20
                AttentionKind::NeedsInput,     // NeedsInput, since 30
                AttentionKind::SessionFailed,  // Failed, since 10
                AttentionKind::WaitingForCapacity, // Routine
            ]
        );
        // Not at capacity: no WaitingForCapacity item.
        assert!(
            attention_items(&project, &agents, &blocked, false)
                .iter()
                .all(|item| item.kind != AttentionKind::WaitingForCapacity)
        );
    }

    #[test]
    fn waiting_for_capacity_means_no_live_session_not_no_session_ever() {
        // An agent whose latest session ENDED still has no live session: with
        // queued work at the cap it is waiting for capacity. One whose
        // session is live is not, whatever its state.
        let project = ProjectId::try_from("p").unwrap();
        let mut ended = session(SessionState::Stopped, 5);
        ended.ended_at_ms = Some(6);
        let waiting = status(
            agent("a", false),
            Some(ended),
            vec![task("t1", TaskStatus::Queued, 7)],
        );
        let mut live_agent = agent("b", false);
        live_agent.current_session_id = Some(SessionId::try_from("s1").unwrap());
        let live = status(
            live_agent,
            Some(session(SessionState::Idle, 8)),
            vec![task("t2", TaskStatus::Queued, 9)],
        );
        let kinds: Vec<AttentionKind> = attention_items(&project, &[waiting, live], &[], true)
            .iter()
            .map(|item| item.kind)
            .collect();
        assert_eq!(kinds, [AttentionKind::WaitingForCapacity]);
    }

    #[test]
    fn a_working_session_and_an_empty_queue_need_no_attention() {
        let project = ProjectId::try_from("p").unwrap();
        let agents = vec![status(
            agent("a", false),
            Some(session(SessionState::Working, 1)),
            vec![],
        )];
        assert!(attention_items(&project, &agents, &[], true).is_empty());
    }
}
