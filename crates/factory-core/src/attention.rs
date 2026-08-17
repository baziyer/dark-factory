//! The one attention taxonomy: what counts as urgent, ranked. `factoryd`
//! uses it to build `factoryctl status`'s attention list and `factory-tui`
//! uses it for fortress badges, announcement ordering, `g`/`G`, and
//! WORKSHOP's `!` filter — the same enum and the same mappings, so the CLI
//! and the board can never quietly disagree about what needs an operator.
//!
//! Session state wins over run-status inference when a session exists
//! (hooks supersede inference); [`Rated`] carries which one a value came
//! from.

use serde::{Deserialize, Serialize};

use crate::{RunSnapshot, RunStatus, SessionSnapshot, SessionState, TaskStatus};

/// How urgently something wants the operator's attention, ranked low to high. `Ord`/`PartialOrd`
/// follow declaration order, so `a.max(b)` and sorting by this enum directly implement "float
/// above routine" without a separate `priority()` lookup table to keep in sync — declaration
/// order *is* the ranking, which is also why every variant is documented with its rank in mind.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    /// Nothing to see: idle, working normally, queued, or otherwise unremarkable.
    Routine,
    /// A positive terminal outcome (task succeeded, run succeeded) — worth a line in the log, but
    /// never worth interrupting an operator's scan for something that's actually stuck.
    Completed,
    /// A negative terminal outcome (task/run/session failed).
    Failed,
    /// Blocked on a human: a session waiting for input, or a task/run blocked/waiting. Ranked
    /// above `Failed` because a `Failed` agent stays failed until retried (nothing new to decide
    /// right now), whereas `NeedsInput` is actively blocking forward progress on live work.
    NeedsInput,
}

impl Attention {
    /// Explicit numeric rank, for call sites that want a number rather than to lean on `Ord`
    /// (e.g. formatting, or a future cross-process wire form). Kept in lock-step with the enum's
    /// declaration order by the `priority_matches_declaration_order` test below.
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Routine => 0,
            Self::Completed => 1,
            Self::Failed => 2,
            Self::NeedsInput => 3,
        }
    }

    /// Whether this level is one an operator should be actively routed toward (`g`/`G`'s filter,
    /// WORKSHOP's `!` toggle). Deliberately excludes `Completed`: a success needs to be seen once
    /// in the log, not hunted down.
    #[must_use]
    pub const fn needs_operator(self) -> bool {
        matches!(self, Self::Failed | Self::NeedsInput)
    }
}

/// Maps a session's durable state onto [`Attention`]. `Starting`/`Idle`/`Working`/`Stopped` are
/// all `Routine` here: `Stopped` is a normal, expected end of a session's life (distinct from
/// `Failed`, which is not), and doesn't need to be chased down the way a stuck or failed agent
/// does.
#[must_use]
pub const fn session_attention(state: SessionState) -> Attention {
    match state {
        SessionState::WaitingForInput => Attention::NeedsInput,
        SessionState::Failed => Attention::Failed,
        SessionState::Starting
        | SessionState::Idle
        | SessionState::Working
        | SessionState::Stopped => Attention::Routine,
    }
}

/// Maps a run's status onto [`Attention]` — the fallback used when an agent has no session yet
/// (see [`agent_attention`]).
#[must_use]
pub const fn run_attention(status: RunStatus) -> Attention {
    match status {
        RunStatus::Failed => Attention::Failed,
        RunStatus::Waiting | RunStatus::Blocked => Attention::NeedsInput,
        RunStatus::Succeeded => Attention::Completed,
        RunStatus::Starting | RunStatus::Running | RunStatus::Paused | RunStatus::Stopped => {
            Attention::Routine
        }
    }
}

/// Maps a task's status onto [`Attention`], for the WORKSHOP task list and announcements.
#[must_use]
pub const fn task_attention(status: TaskStatus) -> Attention {
    match status {
        TaskStatus::Failed => Attention::Failed,
        TaskStatus::Blocked => Attention::NeedsInput,
        TaskStatus::Succeeded => Attention::Completed,
        TaskStatus::Queued | TaskStatus::Running | TaskStatus::Cancelled => Attention::Routine,
    }
}

/// The one precedence rule for an agent's attention, used by `factoryd`'s
/// status responses and by `factory-tui`'s badges alike: a live session's
/// hook-driven state wins (observed); with no live session, a most recent
/// session that ended in failure still counts as failed (observed) until a
/// new session starts; otherwise the most recent run's status is the best
/// guess (inferred), `Routine` when the agent has never run.
/// `latest_session` is the agent's newest session, live or ended.
#[must_use]
pub fn agent_attention(
    latest_session: Option<&SessionSnapshot>,
    latest_run: Option<&RunSnapshot>,
) -> Rated<Attention> {
    match latest_session {
        Some(session) if session.ended_at_ms.is_none() => {
            Rated::observed(session_attention(session.state))
        }
        Some(session) if session.state == SessionState::Failed => {
            Rated::observed(Attention::Failed)
        }
        _ => {
            Rated::inferred(latest_run.map_or(Attention::Routine, |run| run_attention(run.status)))
        }
    }
}

/// An attention level together with whether it was read straight from a hook-reported session
/// (`inferred = false`) or guessed from run/task lifecycle state because no session exists yet
/// (`inferred = true`). The design brief: "mark inferred activity with a `~` prefix in the detail
/// pane" — `inferred` is exactly that bit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Rated<T> {
    pub value: T,
    pub inferred: bool,
}

impl<T> Rated<T> {
    #[must_use]
    pub const fn observed(value: T) -> Self {
        Self {
            value,
            inferred: false,
        }
    }

    #[must_use]
    pub const fn inferred(value: T) -> Self {
        Self {
            value,
            inferred: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_matches_declaration_order() {
        let ranked = [
            Attention::Routine,
            Attention::Completed,
            Attention::Failed,
            Attention::NeedsInput,
        ];
        for window in ranked.windows(2) {
            assert!(
                window[0] < window[1],
                "{:?} should rank below {:?}",
                window[0],
                window[1]
            );
            assert!(
                window[0].priority() < window[1].priority(),
                "priority() should agree with Ord"
            );
        }
    }

    #[test]
    fn needs_operator_excludes_completed_and_routine() {
        assert!(!Attention::Routine.needs_operator());
        assert!(!Attention::Completed.needs_operator());
        assert!(Attention::Failed.needs_operator());
        assert!(Attention::NeedsInput.needs_operator());
    }

    #[test]
    fn session_state_maps_only_waiting_and_failed_above_routine() {
        assert_eq!(
            session_attention(SessionState::WaitingForInput),
            Attention::NeedsInput
        );
        assert_eq!(session_attention(SessionState::Failed), Attention::Failed);
        for routine in [
            SessionState::Starting,
            SessionState::Idle,
            SessionState::Working,
            SessionState::Stopped,
        ] {
            assert_eq!(
                session_attention(routine),
                Attention::Routine,
                "{routine:?}"
            );
        }
    }

    #[test]
    fn run_status_mapping_matches_brief() {
        assert_eq!(run_attention(RunStatus::Failed), Attention::Failed);
        assert_eq!(run_attention(RunStatus::Waiting), Attention::NeedsInput);
        assert_eq!(run_attention(RunStatus::Blocked), Attention::NeedsInput);
        assert_eq!(run_attention(RunStatus::Succeeded), Attention::Completed);
        assert_eq!(run_attention(RunStatus::Starting), Attention::Routine);
        assert_eq!(run_attention(RunStatus::Running), Attention::Routine);
        assert_eq!(run_attention(RunStatus::Paused), Attention::Routine);
        assert_eq!(run_attention(RunStatus::Stopped), Attention::Routine);
    }

    #[test]
    fn task_status_mapping_treats_blocked_as_needs_input() {
        assert_eq!(task_attention(TaskStatus::Blocked), Attention::NeedsInput);
        assert_eq!(task_attention(TaskStatus::Failed), Attention::Failed);
        assert_eq!(task_attention(TaskStatus::Succeeded), Attention::Completed);
        assert_eq!(task_attention(TaskStatus::Queued), Attention::Routine);
        assert_eq!(task_attention(TaskStatus::Running), Attention::Routine);
        assert_eq!(task_attention(TaskStatus::Cancelled), Attention::Routine);
    }

    #[test]
    fn rated_tracks_inferred_bit() {
        let observed = Rated::observed(Attention::Failed);
        let inferred = Rated::inferred(Attention::Failed);
        assert!(!observed.inferred);
        assert!(inferred.inferred);
        assert_eq!(observed.value, inferred.value);
    }
}
