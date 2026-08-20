//! Shared operator-attention taxonomy derived from durable task/run state.

use serde::{Deserialize, Serialize};

use crate::{RunOutcome, RunPhase, RunSnapshot, TaskStatus};

/// How urgently something wants the operator's attention, ranked low to high.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Attention {
    Routine,
    Completed,
    Failed,
    NeedsInput,
}

impl Attention {
    #[must_use]
    pub const fn priority(self) -> u8 {
        match self {
            Self::Routine => 0,
            Self::Completed => 1,
            Self::Failed => 2,
            Self::NeedsInput => 3,
        }
    }

    #[must_use]
    pub const fn needs_operator(self) -> bool {
        matches!(self, Self::Failed | Self::NeedsInput)
    }
}

/// A run is authoritative for its own attention. There is no resident-session
/// state to override or infer it from.
#[must_use]
pub fn run_attention(run: &RunSnapshot) -> Attention {
    match (&run.phase, &run.outcome) {
        (RunPhase::Terminal, Some(RunOutcome::Succeeded)) => Attention::Completed,
        (RunPhase::Terminal, Some(RunOutcome::Blocked { .. })) => Attention::NeedsInput,
        (RunPhase::Terminal, Some(RunOutcome::Failed { .. })) => Attention::Failed,
        (RunPhase::Terminal, Some(RunOutcome::Cancelled { .. })) => Attention::Routine,
        (RunPhase::Finalizing, _) if run.observer_health == crate::ObserverHealth::Degraded => {
            Attention::Failed
        }
        _ => Attention::Routine,
    }
}

#[must_use]
pub const fn task_attention(status: TaskStatus) -> Attention {
    match status {
        TaskStatus::Failed => Attention::Failed,
        TaskStatus::Blocked => Attention::NeedsInput,
        TaskStatus::Succeeded => Attention::Completed,
        TaskStatus::Queued | TaskStatus::Running | TaskStatus::Cancelled => Attention::Routine,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentId, ObserverHealth, ProjectId, Provider, RunFailureReason, RunId, TaskId};

    fn run(phase: RunPhase, outcome: Option<RunOutcome>) -> RunSnapshot {
        RunSnapshot {
            id: RunId::try_from("run-1").unwrap(),
            project_id: ProjectId::try_from("project-1").unwrap(),
            agent_id: AgentId::try_from("agent-1").unwrap(),
            task_id: TaskId::try_from("task-1").unwrap(),
            provider: Provider::Shell,
            phase,
            outcome,
            runner_instance_id: None,
            runtime_model: None,
            runtime_reasoning_effort: None,
            runtime_permission_mode: None,
            runtime_control_mode: None,
            activity: None,
            wait_reason: None,
            observer_health: ObserverHealth::Healthy,
            observer_reason: None,
            admitted_at_ms: 1,
            started_at_ms: Some(2),
            phase_since_ms: 3,
            updated_at_ms: 3,
            ended_at_ms: None,
            exit_code: None,
            exit_signal: None,
        }
    }

    #[test]
    fn only_terminal_outcomes_drive_completed_blocked_or_failed_attention() {
        assert_eq!(
            run_attention(&run(RunPhase::Terminal, Some(RunOutcome::Succeeded))),
            Attention::Completed
        );
        assert_eq!(
            run_attention(&run(
                RunPhase::Terminal,
                Some(RunOutcome::Blocked {
                    reason: "wait".into()
                })
            )),
            Attention::NeedsInput
        );
        assert_eq!(
            run_attention(&run(
                RunPhase::Terminal,
                Some(RunOutcome::Failed {
                    reason: RunFailureReason::Process
                })
            )),
            Attention::Failed
        );
        assert_eq!(
            run_attention(&run(RunPhase::Running, Some(RunOutcome::Succeeded))),
            Attention::Routine
        );
    }

    #[test]
    fn priority_and_operator_routing_share_one_order() {
        let ranked = [
            Attention::Routine,
            Attention::Completed,
            Attention::Failed,
            Attention::NeedsInput,
        ];
        for window in ranked.windows(2) {
            assert!(window[0] < window[1]);
            assert!(window[0].priority() < window[1].priority());
        }
        assert!(!Attention::Routine.needs_operator());
        assert!(!Attention::Completed.needs_operator());
        assert!(Attention::Failed.needs_operator());
        assert!(Attention::NeedsInput.needs_operator());
    }
}
