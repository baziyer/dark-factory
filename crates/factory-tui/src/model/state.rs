//! Per-agent presentation state: the five-way [`AgentState`] shown as glyph color everywhere on
//! the board, the bounded [`RingBuffer`] the announcements log is built from, and the per-agent
//! [`ActivitySeries`]/braille sparkline. Moved out of the pre-Track-6c `model.rs` unchanged in
//! behavior; only [`agent_state`] gained the session-precedence rule described in its own doc
//! comment.

use std::collections::VecDeque;

use factory_core::{RunStatus, SessionState};

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

/// Maps a session's durable state onto [`AgentState`]. The session-state half of the precedence
/// rule described on [`crate::model::Board::agent_state`]: called only once a session is known to
/// exist for the agent.
#[must_use]
pub const fn agent_state_from_session(state: SessionState) -> AgentState {
    match state {
        SessionState::Starting | SessionState::Idle => AgentState::Idle,
        SessionState::Working => AgentState::Working,
        SessionState::WaitingForInput => AgentState::Waiting,
        SessionState::Stopped => AgentState::Stopped,
        SessionState::Failed => AgentState::Failed,
    }
}

/// Maps a run's status onto [`AgentState`] — the pre-sessions fallback, used when an agent has no
/// session yet. Preserved verbatim from the pre-Track-6c board (see its README for the original
/// judgment calls: no run/succeeded reads as idle; a run that failed or was stopped keeps showing
/// that outcome, rather than reverting to idle, until retried; `RunStatus::Paused` folds into
/// `Waiting` as the closest fit).
#[must_use]
pub const fn agent_state_from_run(status: Option<RunStatus>) -> AgentState {
    match status {
        None | Some(RunStatus::Succeeded) => AgentState::Idle,
        Some(RunStatus::Starting | RunStatus::Running) => AgentState::Working,
        Some(RunStatus::Waiting | RunStatus::Blocked | RunStatus::Paused) => AgentState::Waiting,
        Some(RunStatus::Failed) => AgentState::Failed,
        Some(RunStatus::Stopped) => AgentState::Stopped,
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

    #[must_use]
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.items.len()
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
pub const ACTIVITY_VISIBLE_BUCKETS: usize = 8;

/// A rolling five-second event count for one agent, rendered as a braille sparkline. A stand-in
/// for a real tokens/turns series until session-level per-turn accounting exists.
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
            ((count.saturating_mul(7) + max / 2) / max).min(7) as usize
        };
        out.push(BRAILLE_LEVELS[level]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_state_from_run_maps_no_run_to_idle() {
        assert_eq!(agent_state_from_run(None), AgentState::Idle);
    }

    #[test]
    fn agent_state_from_run_maps_all_statuses() {
        let cases = [
            (RunStatus::Starting, AgentState::Working),
            (RunStatus::Running, AgentState::Working),
            (RunStatus::Waiting, AgentState::Waiting),
            (RunStatus::Blocked, AgentState::Waiting),
            (RunStatus::Paused, AgentState::Waiting),
            (RunStatus::Succeeded, AgentState::Idle),
            (RunStatus::Failed, AgentState::Failed),
            (RunStatus::Stopped, AgentState::Stopped),
        ];
        for (status, expected) in cases {
            assert_eq!(agent_state_from_run(Some(status)), expected, "{status:?}");
        }
    }

    #[test]
    fn agent_state_from_session_maps_all_states() {
        let cases = [
            (SessionState::Starting, AgentState::Idle),
            (SessionState::Idle, AgentState::Idle),
            (SessionState::Working, AgentState::Working),
            (SessionState::WaitingForInput, AgentState::Waiting),
            (SessionState::Stopped, AgentState::Stopped),
            (SessionState::Failed, AgentState::Failed),
        ];
        for (state, expected) in cases {
            assert_eq!(agent_state_from_session(state), expected, "{state:?}");
        }
    }

    #[test]
    fn ring_buffer_drops_oldest_past_capacity() {
        let mut buf = RingBuffer::new(3);
        for value in 0..5 {
            buf.push(value);
        }
        assert_eq!(buf.len(), 3);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2, 3, 4]);
    }

    #[test]
    fn ring_buffer_capacity_is_at_least_one() {
        let mut buf: RingBuffer<i32> = RingBuffer::new(0);
        buf.push(1);
        buf.push(2);
        assert_eq!(buf.iter().copied().collect::<Vec<_>>(), vec![2]);
    }

    #[test]
    fn activity_series_buckets_events_per_five_seconds() {
        let mut series = ActivitySeries::default();
        series.record(0);
        series.record(4_999);
        series.record(5_000);
        let counts = series.counts();
        assert_eq!(counts, vec![2, 1]);
    }

    #[test]
    fn activity_series_rolls_forward_on_idle_ticks() {
        let mut series = ActivitySeries::default();
        series.record(0);
        series.roll_to(3 * ACTIVITY_BUCKET_MS);
        let counts = series.counts();
        assert_eq!(counts, vec![1, 0, 0, 0]);
    }

    #[test]
    fn activity_series_collapses_long_idle_gaps() {
        let mut series = ActivitySeries::default();
        series.record(0);
        series.roll_to(1_000_000_000);
        assert_eq!(series.counts(), vec![0]);
    }

    #[test]
    fn activity_series_caps_window_length() {
        let mut series = ActivitySeries::default();
        for bucket in 0..(ACTIVITY_WINDOW as i64 + 10) {
            series.record(bucket * ACTIVITY_BUCKET_MS);
        }
        assert_eq!(series.counts().len(), ACTIVITY_WINDOW);
    }

    #[test]
    fn activity_series_saturates_high_event_rates() {
        let mut series = ActivitySeries::default();
        series.buckets.push_back((0, u64::MAX));
        series.record(0);
        assert_eq!(series.counts(), vec![u64::MAX]);
    }

    #[test]
    fn braille_sparkline_quantizes_and_pads() {
        let counts = [0, 1, 2, 4];
        let rendered = braille_sparkline(&counts, 6);
        assert_eq!(rendered.chars().count(), 6);
        assert_eq!(rendered.chars().next(), Some(BRAILLE_LEVELS[0]));
        assert_eq!(rendered.chars().last(), Some(BRAILLE_LEVELS[7]));
    }

    #[test]
    fn braille_sparkline_all_zero_stays_empty() {
        let counts = [0, 0, 0];
        let rendered = braille_sparkline(&counts, 3);
        assert!(rendered.chars().all(|glyph| glyph == BRAILLE_LEVELS[0]));
    }
}
