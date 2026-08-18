//! The announcements log: one terse Dwarf-Fortress-style line per daemon event
//! (`20:41 bob     task#42 done`), each tagged with an [`Attention`] level so FORTRESS can float
//! failures/needs-input above routine chatter per the design brief ("announcements for failures/
//! completions/spawns/input requests" + "attention-ranked lines float above routine ones").

use factory_core::{EventEnvelope, FactoryEvent, RunStatus, SessionState, TaskStatus};

use factory_core::attention::{Attention, run_attention, session_attention, task_attention};

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

fn run_status_word(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Waiting => "waiting for input",
        RunStatus::Blocked => "blocked",
        RunStatus::Paused => "paused",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Stopped => "stopped",
    }
}

fn session_state_word(state: SessionState) -> &'static str {
    match state {
        SessionState::Starting => "starting",
        SessionState::Idle => "idle",
        SessionState::Working => "working",
        SessionState::WaitingForInput => "waiting for input",
        SessionState::Stopped => "stopped",
        SessionState::Failed => "failed",
    }
}

/// Formats one Dwarf-Fortress-terse announcement line for an event, e.g.
/// `20:41 bob     task#42 done`, tagged with the [`Attention`] level that governs its ordering.
/// Returns `None` for events this board doesn't narrate (project-level bookkeeping isn't
/// agent/task/session activity).
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
                run_status_word(run.status)
            );
            (text, run_attention(run.status))
        }
        FactoryEvent::SessionChanged { session } => {
            let text = format!(
                "{time} {:<10} session {}",
                truncate_id(session.agent_id.as_str(), 10),
                session_state_word(session.state)
            );
            (text, session_attention(session.state))
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
        FactoryEvent::RepositoryOperation {
            agent_id,
            operation,
            phase,
            success,
            ..
        } if phase == "finished" => (
            format!(
                "{time} {:<10} {} {}",
                truncate_id(agent_id.as_str(), 10),
                operation,
                if *success == Some(true) {
                    "done"
                } else {
                    "failed"
                }
            ),
            if *success == Some(true) {
                Attention::Routine
            } else {
                Attention::Failed
            },
        ),
        FactoryEvent::AutoModeChanged { .. }
        | FactoryEvent::PolicyDecision { .. }
        | FactoryEvent::AgentBudgetChanged { .. }
        | FactoryEvent::RepositoryOperation { .. }
        | FactoryEvent::RepositoryAuthorityChanged { .. }
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

/// Ranks announcements for display: highest [`Attention`] first, ties broken by recency (newest
/// first) — "attention-ranked lines float above routine ones" from the design brief. `height` is
/// how many lines the caller has room to show.
#[must_use]
#[cfg(test)]
pub fn ranked<'a>(
    announcements: impl Iterator<Item = &'a Announcement>,
    height: usize,
) -> Vec<&'a Announcement> {
    let mut all: Vec<&Announcement> = announcements.collect();
    all.sort_by(|a, b| {
        b.attention
            .priority()
            .cmp(&a.attention.priority())
            .then_with(|| b.at_ms.cmp(&a.at_ms))
    });
    all.truncate(height);
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{AgentId, ProjectId, TaskId, TaskSnapshot};

    fn task_event(status: TaskStatus, at_ms: i64) -> EventEnvelope {
        EventEnvelope {
            protocol_version: 1,
            sequence: at_ms,
            occurred_at_ms: at_ms,
            event: FactoryEvent::TaskChanged {
                task: TaskSnapshot {
                    id: TaskId::try_from("t1").unwrap(),
                    project_id: ProjectId::try_from("proj").unwrap(),
                    parent_task_id: None,
                    assigned_agent_id: Some(AgentId::try_from("alice").unwrap()),
                    title: "fix bug".into(),
                    status,
                    priority: 0,
                    created_at_ms: at_ms,
                    updated_at_ms: at_ms,
                },
            },
        }
    }

    #[test]
    fn format_event_tags_task_failed_as_failed_attention() {
        let announcement = format_event(&task_event(TaskStatus::Failed, 0)).unwrap();
        assert_eq!(announcement.attention, Attention::Failed);
        assert!(announcement.text.contains("failed"));
    }

    #[test]
    fn format_event_tags_task_succeeded_as_completed() {
        let announcement = format_event(&task_event(TaskStatus::Succeeded, 0)).unwrap();
        assert_eq!(announcement.attention, Attention::Completed);
    }

    #[test]
    fn format_event_ignores_project_bookkeeping() {
        let event = EventEnvelope {
            protocol_version: 1,
            sequence: 1,
            occurred_at_ms: 0,
            event: FactoryEvent::ProjectDeleted {
                project_id: ProjectId::try_from("proj").unwrap(),
            },
        };
        assert!(format_event(&event).is_none());
    }

    #[test]
    fn ranked_puts_needs_input_above_newer_routine_lines() {
        let old_but_urgent = Announcement {
            at_ms: 0,
            text: "old urgent".into(),
            attention: Attention::NeedsInput,
            sequence: 0,
        };
        let new_but_routine = Announcement {
            at_ms: 100,
            text: "new routine".into(),
            attention: Attention::Routine,
            sequence: 1,
        };
        let items = [new_but_routine.clone(), old_but_urgent.clone()];
        let result = ranked(items.iter(), 10);
        assert_eq!(result[0].text, "old urgent");
        assert_eq!(result[1].text, "new routine");
    }

    #[test]
    fn ranked_breaks_ties_by_recency() {
        let a = Announcement {
            at_ms: 0,
            text: "a".into(),
            attention: Attention::Routine,
            sequence: 0,
        };
        let b = Announcement {
            at_ms: 100,
            text: "b".into(),
            attention: Attention::Routine,
            sequence: 1,
        };
        let items = [a, b];
        let result = ranked(items.iter(), 10);
        assert_eq!(result[0].text, "b");
        assert_eq!(result[1].text, "a");
    }

    #[test]
    fn ranked_truncates_to_height() {
        let items: Vec<Announcement> = (0..5)
            .map(|i| Announcement {
                at_ms: i,
                text: format!("{i}"),
                attention: Attention::Routine,
                sequence: i,
            })
            .collect();
        let result = ranked(items.iter(), 2);
        assert_eq!(result.len(), 2);
    }
}
