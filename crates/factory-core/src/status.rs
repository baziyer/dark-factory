//! Fleet and per-agent status projected from durable task and run state.

use serde::{Deserialize, Serialize};

use crate::{
    AgentBudget, AgentId, AgentSnapshot, ProjectId, ProjectSnapshot, RunPhase, RunSnapshot, TaskId,
    TaskSnapshot,
    attention::{Attention, run_attention, task_attention},
    local::AgentDetail,
};

pub const MAX_QUEUE_PREVIEW: usize = 10;
pub const MAX_ATTENTION_SUMMARY_CHARS: usize = 160;
pub const MAX_ATTENTION_EVIDENCE_CHARS: usize = 240;

const fn legacy_event_sequence() -> i64 {
    -1
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FleetStatus {
    pub generated_at_ms: i64,
    #[serde(default = "legacy_event_sequence")]
    pub event_sequence: i64,
    #[serde(default)]
    pub auto_mode: bool,
    pub active_run_cap: u32,
    pub active_runs: u32,
    pub projects: Vec<ProjectStatus>,
    pub attention: Vec<AttentionItem>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectStatus {
    pub project: ProjectSnapshot,
    pub agents: Vec<AgentStatus>,
    pub backlog_depth: u32,
    pub backlog: Vec<TaskSnapshot>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatus {
    pub agent: AgentSnapshot,
    #[serde(default)]
    pub budget: AgentBudget,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pause_reasons: Vec<AgentPauseReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_run: Option<RunSnapshot>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_run: Option<RunSnapshot>,
    pub queue_depth: u32,
    pub queue: Vec<TaskSnapshot>,
    /// Messages waiting to influence a future admitted attempt.
    pub inbox_pending: u32,
    pub attention: Attention,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentPauseReason {
    AgentHold,
    BudgetExhausted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AgentStatusDetail {
    #[serde(default)]
    pub generated_at_ms: i64,
    #[serde(default = "legacy_event_sequence")]
    pub event_sequence: i64,
    pub status: AgentStatus,
    pub detail: AgentDetail,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attention: Vec<AttentionItem>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionReasonKind {
    WorkerBlocked,
    RunFailed,
    FinalizationStalled,
    BudgetExhausted,
    PausedWithWork,
    WaitingForCapacity,
}

impl AttentionReasonKind {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::WorkerBlocked => "worker blocked",
            Self::RunFailed => "run failed",
            Self::FinalizationStalled => "finalization stalled",
            Self::BudgetExhausted => "budget exhausted",
            Self::PausedWithWork => "paused with work",
            Self::WaitingForCapacity => "waiting for capacity",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionAction {
    InspectRun,
    ResetBudget,
    RetryTask,
    ResumeAgent,
    WaitForCapacity,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionReason {
    pub kind: AttentionReasonKind,
    pub summary: String,
    pub action: AttentionAction,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct AttentionItem {
    pub level: Attention,
    pub project_id: ProjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<AgentId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<crate::RunId>,
    pub since_ms: i64,
    pub reason: AttentionReason,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionChoice {
    pub label: String,
    pub action: AttentionAction,
    pub consequence: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AttentionDecision {
    pub cause: String,
    pub evidence: String,
    pub choices: Vec<AttentionChoice>,
    pub recommended: Option<usize>,
}

impl AttentionItem {
    #[must_use]
    pub const fn needs_operator_decision(&self) -> bool {
        matches!(
            self.reason.kind,
            AttentionReasonKind::BudgetExhausted | AttentionReasonKind::PausedWithWork
        )
    }

    #[must_use]
    pub fn decision(&self) -> AttentionDecision {
        let evidence = display_text(&format!(
            "project: {} agent: {} task: {} run: {}",
            self.project_id,
            self.agent_id.as_ref().map_or("-", AgentId::as_str),
            self.task_id.as_ref().map_or("-", TaskId::as_str),
            self.run_id.as_ref().map_or("-", crate::RunId::as_str),
        ));
        let choices = match self.reason.kind {
            AttentionReasonKind::PausedWithWork => vec![AttentionChoice {
                label: "Resume agent".into(),
                action: AttentionAction::ResumeAgent,
                consequence: "allows factoryd to admit queued work".into(),
            }],
            AttentionReasonKind::BudgetExhausted => vec![AttentionChoice {
                label: "Reset budget".into(),
                action: AttentionAction::ResetBudget,
                consequence: "permits a future admitted attempt to use tools".into(),
            }],
            AttentionReasonKind::WorkerBlocked
            | AttentionReasonKind::RunFailed
            | AttentionReasonKind::FinalizationStalled
            | AttentionReasonKind::WaitingForCapacity => Vec::new(),
        };
        AttentionDecision {
            cause: display_text(&self.reason.summary),
            evidence: evidence
                .chars()
                .take(MAX_ATTENTION_EVIDENCE_CHARS)
                .collect(),
            recommended: (!choices.is_empty()).then_some(0),
            choices,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BlockedTaskStatus {
    pub task: TaskSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[must_use]
pub fn attention_items(
    project_id: &ProjectId,
    agents: &[AgentStatus],
    blocked_tasks: &[BlockedTaskStatus],
    at_capacity: bool,
) -> Vec<AttentionItem> {
    let mut items = Vec::new();
    for status in agents {
        let agent = &status.agent;
        let next_queued = status
            .queue
            .iter()
            .find(|task| task.status == crate::TaskStatus::Queued);
        if status.budget.exhausted {
            items.push(item(
                Attention::NeedsInput,
                project_id,
                Some(agent.id.clone()),
                status.current_run.as_ref().map(|run| run.task_id.clone()),
                status.current_run.as_ref().map(|run| run.id.clone()),
                status.budget.updated_at_ms,
                AttentionReasonKind::BudgetExhausted,
                format!("tool-call budget exhausted at {}", status.budget.tool_calls),
                AttentionAction::ResetBudget,
            ));
        }

        if let Some(run) = status.current_run.as_ref()
            && run.phase == RunPhase::Finalizing
            && (run.observer_health == crate::ObserverHealth::Degraded || run.wait_reason.is_some())
        {
            items.push(item(
                Attention::Failed,
                project_id,
                Some(agent.id.clone()),
                Some(run.task_id.clone()),
                Some(run.id.clone()),
                run.phase_since_ms,
                AttentionReasonKind::FinalizationStalled,
                run.wait_reason
                    .clone()
                    .unwrap_or_else(|| "attempt finalization is unresolved".into()),
                AttentionAction::InspectRun,
            ));
        } else if let Some(run) = status.latest_run.as_ref()
            && run_attention(run) == Attention::Failed
        {
            items.push(item(
                Attention::Failed,
                project_id,
                Some(agent.id.clone()),
                Some(run.task_id.clone()),
                Some(run.id.clone()),
                run.phase_since_ms,
                AttentionReasonKind::RunFailed,
                "latest attempt failed".into(),
                AttentionAction::InspectRun,
            ));
        }

        let queued_depth = status
            .queue
            .iter()
            .filter(|task| task.status == crate::TaskStatus::Queued)
            .count();
        if queued_depth > 0 && agent.paused && !status.budget.exhausted {
            items.push(item(
                Attention::NeedsInput,
                project_id,
                Some(agent.id.clone()),
                next_queued.map(|task| task.id.clone()),
                None,
                next_queued.map_or(agent.updated_at_ms, |task| task.created_at_ms),
                AttentionReasonKind::PausedWithWork,
                format!("paused with {queued_depth} queued task(s)"),
                AttentionAction::ResumeAgent,
            ));
        } else if queued_depth > 0 && status.current_run.is_none() && at_capacity {
            items.push(item(
                Attention::Routine,
                project_id,
                Some(agent.id.clone()),
                next_queued.map(|task| task.id.clone()),
                None,
                next_queued.map_or(agent.updated_at_ms, |task| task.updated_at_ms),
                AttentionReasonKind::WaitingForCapacity,
                format!("{queued_depth} queued task(s) but the daemon is at its active-run cap"),
                AttentionAction::WaitForCapacity,
            ));
        }
    }

    for blocked in blocked_tasks {
        if task_attention(blocked.task.status) == Attention::NeedsInput {
            items.push(item(
                Attention::NeedsInput,
                project_id,
                blocked.task.assigned_agent_id.clone(),
                Some(blocked.task.id.clone()),
                None,
                blocked.task.updated_at_ms,
                AttentionReasonKind::WorkerBlocked,
                blocked
                    .reason
                    .clone()
                    .unwrap_or_else(|| format!("task '{}' is blocked", blocked.task.title)),
                AttentionAction::RetryTask,
            ));
        }
    }
    items
}

#[allow(clippy::too_many_arguments)]
fn item(
    level: Attention,
    project_id: &ProjectId,
    agent_id: Option<AgentId>,
    task_id: Option<TaskId>,
    run_id: Option<crate::RunId>,
    since_ms: i64,
    kind: AttentionReasonKind,
    summary: String,
    action: AttentionAction,
) -> AttentionItem {
    AttentionItem {
        level,
        project_id: project_id.clone(),
        agent_id,
        task_id,
        run_id,
        since_ms,
        reason: AttentionReason {
            kind,
            summary: display_text(&summary),
            action,
        },
    }
}

#[must_use]
pub fn display_text(value: &str) -> String {
    let mut output = String::new();
    let mut length = 0;
    let mut pending_space = false;
    let mut truncated = false;
    for character in value.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        if character.is_control() || is_bidi_control(character) {
            continue;
        }
        let separator = usize::from(pending_space && !output.is_empty());
        if length + separator + 1 > MAX_ATTENTION_SUMMARY_CHARS {
            truncated = true;
            break;
        }
        if separator == 1 {
            output.push(' ');
            length += 1;
        }
        output.push(character);
        length += 1;
        pending_space = false;
    }
    if truncated {
        if length == MAX_ATTENTION_SUMMARY_CHARS {
            output.pop();
        }
        output.push('…');
    }
    output
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[must_use]
pub fn age_text(now_ms: i64, since_ms: i64) -> String {
    let seconds = now_ms.saturating_sub(since_ms).max(0) / 1_000;
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

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
    use crate::{AgentRole, Provider, TaskStatus};

    fn agent(paused: bool) -> AgentSnapshot {
        AgentSnapshot {
            id: AgentId::try_from("agent-1").unwrap(),
            project_id: ProjectId::try_from("project-1").unwrap(),
            parent_agent_id: None,
            role: AgentRole::Worker,
            provider: Provider::Shell,
            current_run_id: None,
            paused,
            created_at_ms: 1,
            updated_at_ms: 2,
        }
    }

    fn task(status: TaskStatus) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::try_from("task-1").unwrap(),
            project_id: ProjectId::try_from("project-1").unwrap(),
            parent_task_id: None,
            assigned_agent_id: Some(AgentId::try_from("agent-1").unwrap()),
            title: "task".into(),
            status,
            priority: 0,
            created_at_ms: 3,
            updated_at_ms: 4,
        }
    }

    #[test]
    fn queued_work_uses_run_ownership_for_capacity_and_pause_attention() {
        let project = ProjectId::try_from("project-1").unwrap();
        let status = AgentStatus {
            agent: agent(false),
            budget: AgentBudget::default(),
            pause_reasons: Vec::new(),
            current_run: None,
            latest_run: None,
            queue_depth: 1,
            queue: vec![task(TaskStatus::Queued)],
            inbox_pending: 0,
            attention: Attention::Routine,
        };
        let items = attention_items(&project, &[status], &[], true);
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].reason.kind,
            AttentionReasonKind::WaitingForCapacity
        );
        assert!(items[0].run_id.is_none());
    }

    #[test]
    fn display_text_is_bounded_and_control_safe() {
        let value = format!("hello\n\u{202e}{}", "x".repeat(200));
        let rendered = display_text(&value);
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains('\u{202e}'));
        assert_eq!(rendered.chars().count(), MAX_ATTENTION_SUMMARY_CHARS);
        assert!(rendered.ends_with('…'));
    }

    #[test]
    fn run_outcome_is_used_without_a_session_projection() {
        let _ = crate::RunOutcome::Succeeded;
    }
}
