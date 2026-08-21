//! The announcements log: one terse Dwarf-Fortress-style line per daemon event
//! (`20:41 bob     task#42 done`), each tagged with an [`Attention`] level so FORTRESS can float
//! failures/needs-input above routine chatter per the design brief ("announcements for failures/
//! completions/spawns/input requests" + "attention-ranked lines float above routine ones").

use factory_core::{EventEnvelope, FactoryEvent, RunOutcome, RunPhase, TaskStatus};

use factory_core::attention::{Attention, run_attention, task_attention};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Announcement {
    pub at_ms: i64,
    pub text: String,
    pub attention: Attention,
    /// The originating event's wire sequence — the daemon's unique event id. Used to dedupe the
    /// connect-time replay (`Board::apply_replay`, issue #67) against whatever the live stream
    /// then delivers for the same events.
    pub sequence: i64,
}

fn clock(at_ms: i64) -> String {
    // This clock needs only UTC time-of-day, so a modulus is enough.
    let ms_of_day = at_ms.rem_euclid(86_400_000);
    let hours = ms_of_day / 3_600_000;
    let minutes = (ms_of_day % 3_600_000) / 60_000;
    format!("{hours:02}:{minutes:02}")
}

fn truncate_id(id: &str, max: usize) -> String {
    if id.chars().count() <= max {
        id.to_owned()
    } else {
        let head: String = id.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

pub(crate) fn task_status_word(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Queued => "queued",
        TaskStatus::Running => "started",
        TaskStatus::Blocked => "blocked",
        TaskStatus::Succeeded => "done",
        TaskStatus::Failed => "failed",
        TaskStatus::Cancelled => "cancelled",
    }
}

fn run_status_word(phase: RunPhase, outcome: Option<&RunOutcome>) -> &'static str {
    match (phase, outcome) {
        (RunPhase::Admitted, _) => "admitted",
        (RunPhase::Running, _) => "running",
        (RunPhase::Finalizing, _) => "finalizing",
        (RunPhase::Terminal, Some(RunOutcome::Succeeded)) => "succeeded",
        (RunPhase::Terminal, Some(RunOutcome::Blocked { .. })) => "blocked",
        (RunPhase::Terminal, Some(RunOutcome::Failed { .. })) => "failed",
        (RunPhase::Terminal, Some(RunOutcome::Cancelled { .. })) => "cancelled",
        (RunPhase::Terminal, None) => "terminal",
    }
}

/// Formats one Dwarf-Fortress-terse announcement line for an event, e.g.
/// `20:41 bob     task#42 done`, tagged with the [`Attention`] level that governs its ordering.
/// Returns `None` for events this board doesn't narrate (project-level bookkeeping isn't
/// agent/task/run activity).
#[must_use]
pub fn format_event(event: &EventEnvelope) -> Option<Announcement> {
    let time = clock(event.occurred_at_ms);
    let (text, attention) = match &event.event {
        FactoryEvent::TaskChanged { task } => {
            let actor = task
                .assigned_agent_id
                .as_ref()
                .map_or_else(|| "-".to_owned(), |id| truncate_id(id.as_str(), 10));
            let text = format!(
                "{time} {actor:<10} task#{} {}",
                truncate_id(task.id.as_str(), 12),
                task_status_word(task.status)
            );
            (text, task_attention(task.status))
        }
        FactoryEvent::RunChanged { run } => {
            let text = format!(
                "{time} {:<10} run {}",
                truncate_id(run.agent_id.as_str(), 10),
                run_status_word(run.phase, run.outcome.as_ref())
            );
            (text, run_attention(run))
        }
        FactoryEvent::LegacySessionChanged { .. } | FactoryEvent::LegacyRunChanged { .. } => {
            return None;
        }
        FactoryEvent::AgentChanged { agent } => {
            let text = format!(
                "{time} {:<10} agent updated",
                truncate_id(agent.id.as_str(), 10)
            );
            (text, Attention::Routine)
        }
        FactoryEvent::TaskDeleted { task_id, .. } => {
            let text = format!("{time} task#{} deleted", truncate_id(task_id.as_str(), 12));
            (text, Attention::Routine)
        }
        FactoryEvent::AgentDeleted { agent_id, .. } => {
            let text = format!("{time} {} removed", truncate_id(agent_id.as_str(), 10));
            (text, Attention::Routine)
        }
        FactoryEvent::PolicyDecision {
            agent_id,
            decision,
            rule,
            ..
        } if decision == "deny" => (
            format!(
                "{time} {:<10} denied {}",
                truncate_id(agent_id.as_str(), 10),
                rule.as_deref().unwrap_or("policy")
            ),
            Attention::Routine,
        ),
        FactoryEvent::AgentBudgetChanged {
            agent_id,
            budget,
            action,
            ..
        } if action == "denied" => (
            format!(
                "{time} {:<10} budget exhausted ({}/{})",
                truncate_id(agent_id.as_str(), 10),
                budget.tool_calls,
                budget
                    .max_tool_calls
                    .map_or_else(|| "unlimited".into(), |n| n.to_string())
            ),
            Attention::NeedsInput,
        ),
        FactoryEvent::DispatchPolicyChanged { .. }
        | FactoryEvent::LegacyAutoModeChanged { .. }
        | FactoryEvent::PolicyDecision { .. }
        | FactoryEvent::AgentBudgetChanged { .. }
        | FactoryEvent::LegacyRepositoryOperation { .. }
        | FactoryEvent::LegacyRepositoryAuthorityChanged { .. }
        | FactoryEvent::ChangeChanged { .. }
        | FactoryEvent::LegacySourceForgotten { .. }
        | FactoryEvent::ProjectChanged { .. }
        | FactoryEvent::ProjectDeleted { .. } => return None,
    };
    Some(Announcement {
        at_ms: event.occurred_at_ms,
        text,
        attention,
        sequence: event.sequence,
    })
}
