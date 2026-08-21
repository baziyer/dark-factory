//! Per-agent presentation state: the five-way [`AgentState`] shown as glyph color everywhere on
//! the board, the bounded [`RingBuffer`] the announcements log is built from, and the per-agent
//! [`ActivitySeries`]/braille sparkline. Moved out of the pre-Track-6c `model.rs` unchanged in
//! behavior.

use std::collections::VecDeque;

use factory_core::{RunOutcome, RunPhase, RunSnapshot};

// ---------------------------------------------------------------------------------------------
// Agent state
// ---------------------------------------------------------------------------------------------

/// The five states a glyph on the fortress floor, a WORKSHOP agent-tree row, or a unit list can
/// show for an agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentState {
    Idle,
    Working,
    Waiting,
    Stopped,
    Failed,
}

/// Whether a presentation value came directly from durable state or is an
/// intentionally inferred fallback.
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
}

impl AgentState {
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Waiting => "waiting",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// Maps one durable attempt onto the board's compact presentation state.
#[must_use]
pub fn agent_state_from_run(run: Option<&RunSnapshot>) -> AgentState {
    let Some(run) = run else {
        return AgentState::Idle;
    };
    match (&run.phase, &run.outcome) {
        (RunPhase::Admitted | RunPhase::Running, _) => AgentState::Working,
        (RunPhase::Finalizing, Some(RunOutcome::Blocked { .. })) => AgentState::Waiting,
        (RunPhase::Finalizing, _) => AgentState::Working,
        (RunPhase::Terminal, Some(RunOutcome::Blocked { .. })) => AgentState::Waiting,
        (RunPhase::Terminal, Some(RunOutcome::Failed { .. })) => AgentState::Failed,
        (RunPhase::Terminal, Some(RunOutcome::Cancelled { .. })) => AgentState::Stopped,
        (RunPhase::Terminal, Some(RunOutcome::Succeeded) | None) => AgentState::Idle,
    }
}

// ---------------------------------------------------------------------------------------------
// Ring buffer
// ---------------------------------------------------------------------------------------------

/// A fixed-capacity FIFO. Pushing past capacity drops the oldest item. Used for the
/// announcements log.
#[derive(Debug)]
pub struct RingBuffer<T> {
    items: VecDeque<T>,
    capacity: usize,
}

impl<T> RingBuffer<T> {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            items: VecDeque::with_capacity(capacity.min(1024)),
            capacity: capacity.max(1),
        }
    }

    pub fn push(&mut self, item: T) {
        if self.items.len() >= self.capacity {
            self.items.pop_front();
        }
        self.items.push_back(item);
    }

    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> {
        self.items.iter()
    }
}

// ---------------------------------------------------------------------------------------------
// Activity sparklines
// ---------------------------------------------------------------------------------------------

/// Width of one short-horizon activity bucket. Durable hook/tool events land in this series;
/// time-only rolling never adds activity.
pub const ACTIVITY_BUCKET_MS: i64 = 5_000;
/// How many five-second buckets of history each agent's sparkline retains.
const ACTIVITY_WINDOW: usize = 12;
/// Number of recent buckets shown in BUILDING. At five seconds per bucket this is a 40-second
/// visible horizon, while the series retains another four buckets for smooth aging at the edge.
/// A rolling five-second event count for one agent, rendered as a braille sparkline. A stand-in
/// for a real tokens/turns series until attempt-level accounting exists.
#[derive(Debug, Default)]
pub struct ActivitySeries {
    /// `(bucket_start_ms, count)`, oldest first, one entry per five seconds.
    buckets: VecDeque<(i64, u64)>,
}

impl ActivitySeries {
    /// Advances the window so the newest bucket covers `at_ms`, without incrementing anything.
    /// Called on the UI's ordinary elapsed-time tick so idle agents' sparklines age honestly.
    /// Large idle gaps are collapsed rather than iterated through one bucket at a time.
    pub fn roll_to(&mut self, at_ms: i64) {
        let bucket_start = at_ms.div_euclid(ACTIVITY_BUCKET_MS) * ACTIVITY_BUCKET_MS;
        match self.buckets.back() {
            None => self.buckets.push_back((bucket_start, 0)),
            Some(&(last_start, _)) if bucket_start > last_start => {
                let steps = bucket_start
                    .saturating_sub(last_start)
                    .checked_div(ACTIVITY_BUCKET_MS)
                    .unwrap_or(ACTIVITY_WINDOW as i64);
                if steps >= ACTIVITY_WINDOW as i64 {
                    self.buckets.clear();
                    self.buckets.push_back((bucket_start, 0));
                } else {
                    for step in 1..=steps {
                        self.buckets
                            .push_back((last_start + step * ACTIVITY_BUCKET_MS, 0));
                    }
                }
            }
            _ => {}
        }
        while self.buckets.len() > ACTIVITY_WINDOW {
            self.buckets.pop_front();
        }
    }

    /// Records one durable event at `at_ms`, rolling the window forward first if needed. An event whose
    /// timestamp lands before the current bucket (clock skew, replayed history) is folded into
    /// the newest bucket rather than rolling backward.
    pub fn record(&mut self, at_ms: i64) {
        self.roll_to(at_ms);
        if let Some(last) = self.buckets.back_mut() {
            last.1 = last.1.saturating_add(1);
        }
    }

    /// Bucket counts, oldest first, suitable for a sparkline.
    #[must_use]
    pub fn counts(&self) -> Vec<u64> {
        self.buckets.iter().map(|&(_, count)| count).collect()
    }
}

/// Braille fill levels from empty to full, matching the gradient the owner's design note uses
/// (`⣀⣠⣤⣴⣶⣾⣿`) plus a true-empty glyph for zero-count buckets.
pub const BRAILLE_LEVELS: [char; 8] = ['\u{2800}', '⣀', '⣠', '⣤', '⣴', '⣶', '⣾', '⣿'];

/// Renders `counts` (oldest first) as a fixed-width braille sparkline, right-aligned to the most
/// recent `width` buckets. Pure integer math throughout (no float precision-loss lint dodging
/// needed): each bar is `round(count * 7 / max)` levels tall.
#[must_use]
pub fn braille_sparkline(counts: &[u64], width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let start = counts.len().saturating_sub(width);
    let slice = &counts[start..];
    let max = slice.iter().copied().max().unwrap_or(0);
    let mut out = String::with_capacity(width);
    for _ in 0..width.saturating_sub(slice.len()) {
        out.push(BRAILLE_LEVELS[0]);
    }
    for &count in slice {
        let level = if max == 0 {
            0
        } else {
            ((u128::from(count) * 7 + u128::from(max) / 2) / u128::from(max)).min(7) as usize
        };
        out.push(BRAILLE_LEVELS[level]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::{
        AgentId, ObserverHealth, ProjectId, Provider, RunFailureReason, RunId, TaskId,
    };

    fn run(phase: RunPhase, outcome: Option<RunOutcome>) -> RunSnapshot {
        RunSnapshot {
            id: RunId::try_from("run").unwrap(),
            project_id: ProjectId::try_from("project").unwrap(),
            agent_id: AgentId::try_from("agent").unwrap(),
            task_id: TaskId::try_from("task").unwrap(),
            provider: Provider::Shell,
            phase,
            outcome,
            runner_instance_id: None,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_execution_mode: None,
            runtime_control_mode: None,
            activity: None,
            wait_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_reason: None,
            admitted_at_ms: 1,
            started_at_ms: None,
            phase_since_ms: 1,
            updated_at_ms: 1,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
        }
    }

    #[test]
    fn attempt_phase_and_outcome_are_the_only_agent_state_inputs() {
        let cases = [
            (run(RunPhase::Admitted, None), AgentState::Working),
            (run(RunPhase::Running, None), AgentState::Working),
            (
                run(
                    RunPhase::Finalizing,
                    Some(RunOutcome::Failed {
                        reason: RunFailureReason::Process,
                    }),
                ),
                AgentState::Working,
            ),
            (
                run(
                    RunPhase::Terminal,
                    Some(RunOutcome::Blocked {
                        reason: "operator decision".into(),
                    }),
                ),
                AgentState::Waiting,
            ),
            (
                run(
                    RunPhase::Terminal,
                    Some(RunOutcome::Failed {
                        reason: RunFailureReason::Process,
                    }),
                ),
                AgentState::Failed,
            ),
            (
                run(RunPhase::Terminal, Some(RunOutcome::Succeeded)),
                AgentState::Idle,
            ),
        ];
        for (run, expected) in cases {
            assert_eq!(agent_state_from_run(Some(&run)), expected);
        }
        assert_eq!(agent_state_from_run(None), AgentState::Idle);
    }
}
