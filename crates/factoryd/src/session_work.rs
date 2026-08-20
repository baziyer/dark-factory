//! Pure state machine for one resident session's durable factory-work ownership.
//!
//! Provider lifecycle (`idle`, `working`, `stopped`, ...) is deliberately not
//! represented here. A provider becoming idle cannot release factory work;
//! only one of these exact identity-bound transitions can do that.

use factory_core::{RunId, TaskId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskWork {
    pub task_id: TaskId,
    pub incarnation_id: String,
    pub revision: i64,
    pub run_id: RunId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeliveryLease {
    pub attempt_id: String,
    pub task: Option<TaskWork>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Phase {
    Empty,
    Delivering(DeliveryLease),
    Running(DeliveryLease),
    Uncertain(DeliveryLease),
}

impl Phase {
    #[must_use]
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Delivering(_) => "delivering",
            Self::Running(_) => "running",
            Self::Uncertain(_) => "uncertain",
        }
    }

    #[must_use]
    pub fn lease(&self) -> Option<&DeliveryLease> {
        match self {
            Self::Empty => None,
            Self::Delivering(lease) | Self::Running(lease) | Self::Uncertain(lease) => Some(lease),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionWork {
    pub revision: i64,
    pub phase: Phase,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    Reserve,
    ExternalEffectPossible,
    Acknowledge,
    CancelReservation,
    RecoverTerminal,
    Complete,
    EndSession,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TransitionError {
    #[error("cannot {action:?} session work in {state} state")]
    Invalid { action: Action, state: &'static str },
    #[error("session work identity does not match the requested transition")]
    IdentityMismatch,
    #[error("session work revision overflow")]
    RevisionOverflow,
}

impl SessionWork {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            revision: 0,
            phase: Phase::Empty,
        }
    }

    pub fn reserve(&self, lease: DeliveryLease) -> Result<Self, TransitionError> {
        match self.phase {
            Phase::Empty => self.next(Phase::Delivering(lease)),
            _ => self.invalid(Action::Reserve),
        }
    }

    pub fn external_effect_possible(&self, attempt_id: &str) -> Result<Self, TransitionError> {
        match &self.phase {
            Phase::Delivering(lease) if lease.attempt_id == attempt_id => {
                self.next(Phase::Uncertain(lease.clone()))
            }
            Phase::Delivering(_) => Err(TransitionError::IdentityMismatch),
            _ => self.invalid(Action::ExternalEffectPossible),
        }
    }

    pub fn acknowledge(&self, attempt_id: &str) -> Result<Self, TransitionError> {
        match &self.phase {
            Phase::Uncertain(lease) if lease.attempt_id == attempt_id => {
                let phase = if lease.task.is_some() {
                    Phase::Running(lease.clone())
                } else {
                    Phase::Empty
                };
                self.next(phase)
            }
            Phase::Running(lease) if lease.attempt_id == attempt_id => Ok(self.clone()),
            Phase::Uncertain(_) | Phase::Running(_) => Err(TransitionError::IdentityMismatch),
            _ => self.invalid(Action::Acknowledge),
        }
    }

    pub fn cancel_reservation(&self, attempt_id: &str) -> Result<Self, TransitionError> {
        match &self.phase {
            Phase::Delivering(lease) if lease.attempt_id == attempt_id => self.next(Phase::Empty),
            Phase::Delivering(_) => Err(TransitionError::IdentityMismatch),
            _ => self.invalid(Action::CancelReservation),
        }
    }

    pub fn recover_terminal(&self, attempt_id: &str) -> Result<Self, TransitionError> {
        match &self.phase {
            Phase::Uncertain(lease) if lease.attempt_id == attempt_id => self.next(Phase::Empty),
            Phase::Uncertain(_) => Err(TransitionError::IdentityMismatch),
            _ => self.invalid(Action::RecoverTerminal),
        }
    }

    pub fn complete(&self, run_id: &RunId) -> Result<Self, TransitionError> {
        match &self.phase {
            Phase::Running(lease)
                if lease
                    .task
                    .as_ref()
                    .is_some_and(|task| task.run_id == *run_id) =>
            {
                self.next(Phase::Empty)
            }
            Phase::Running(_) => Err(TransitionError::IdentityMismatch),
            _ => self.invalid(Action::Complete),
        }
    }

    pub fn end_session(&self) -> Result<Self, TransitionError> {
        match self.phase {
            Phase::Empty => Ok(self.clone()),
            _ => self.next(Phase::Empty),
        }
    }

    fn next(&self, phase: Phase) -> Result<Self, TransitionError> {
        Ok(Self {
            revision: self
                .revision
                .checked_add(1)
                .ok_or(TransitionError::RevisionOverflow)?,
            phase,
        })
    }

    fn invalid<T>(&self, action: Action) -> Result<T, TransitionError> {
        Err(TransitionError::Invalid {
            action,
            state: self.phase.name(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(attempt: &str, task: &str, run: &str) -> DeliveryLease {
        DeliveryLease {
            attempt_id: attempt.to_owned(),
            task: Some(TaskWork {
                task_id: TaskId::try_from(task).unwrap(),
                incarnation_id: format!("incarnation:{task}"),
                revision: 3,
                run_id: RunId::try_from(run).unwrap(),
            }),
        }
    }

    #[test]
    fn exact_delivery_lifecycle_is_monotonic_and_identity_bound() {
        let lease = lease(
            "attempt-a",
            "task-a",
            "11111111-1111-4111-8111-111111111111",
        );
        let empty = SessionWork::empty();
        let delivering = empty.reserve(lease.clone()).unwrap();
        let uncertain = delivering.external_effect_possible("attempt-a").unwrap();
        let running = uncertain.acknowledge("attempt-a").unwrap();
        let completed = running
            .complete(&lease.task.as_ref().unwrap().run_id)
            .unwrap();

        assert_eq!(delivering.revision, 1);
        assert_eq!(uncertain.revision, 2);
        assert_eq!(running.revision, 3);
        assert_eq!(completed.revision, 4);
        assert_eq!(completed.phase, Phase::Empty);
        assert_eq!(running.acknowledge("attempt-a").unwrap(), running);
        assert_eq!(
            uncertain.acknowledge("attempt-b"),
            Err(TransitionError::IdentityMismatch)
        );
    }

    #[test]
    fn no_state_can_admit_a_successor_until_exact_release() {
        let first = lease(
            "attempt-a",
            "task-a",
            "11111111-1111-4111-8111-111111111111",
        );
        let successor = lease(
            "attempt-b",
            "task-b",
            "22222222-2222-4222-8222-222222222222",
        );
        let delivering = SessionWork::empty().reserve(first).unwrap();
        let uncertain = delivering.external_effect_possible("attempt-a").unwrap();
        let running = uncertain.acknowledge("attempt-a").unwrap();

        for occupied in [&delivering, &uncertain, &running] {
            assert!(occupied.reserve(successor.clone()).is_err());
        }
        assert!(uncertain.cancel_reservation("attempt-a").is_err());
        assert!(running.end_session().unwrap().reserve(successor).is_ok());
    }

    #[test]
    fn message_only_acknowledgement_returns_directly_to_empty() {
        let lease = DeliveryLease {
            attempt_id: "message-attempt".to_owned(),
            task: None,
        };
        let acknowledged = SessionWork::empty()
            .reserve(lease)
            .unwrap()
            .external_effect_possible("message-attempt")
            .unwrap()
            .acknowledge("message-attempt")
            .unwrap();
        assert_eq!(acknowledged.phase, Phase::Empty);
    }

    #[test]
    fn transition_matrix_has_no_unlisted_release_or_admission_edge() {
        let lease = lease(
            "attempt-a",
            "task-a",
            "11111111-1111-4111-8111-111111111111",
        );
        let run_id = lease.task.as_ref().unwrap().run_id.clone();
        let empty = SessionWork::empty();
        let delivering = empty.reserve(lease.clone()).unwrap();
        let uncertain = delivering.external_effect_possible("attempt-a").unwrap();
        let running = uncertain.acknowledge("attempt-a").unwrap();

        assert!(empty.external_effect_possible("attempt-a").is_err());
        assert!(empty.acknowledge("attempt-a").is_err());
        assert!(empty.cancel_reservation("attempt-a").is_err());
        assert!(empty.complete(&run_id).is_err());

        assert!(delivering.reserve(lease.clone()).is_err());
        assert!(delivering.acknowledge("attempt-a").is_err());
        assert!(delivering.complete(&run_id).is_err());

        assert!(uncertain.reserve(lease.clone()).is_err());
        assert!(uncertain.external_effect_possible("attempt-a").is_err());
        assert!(uncertain.cancel_reservation("attempt-a").is_err());
        assert!(uncertain.complete(&run_id).is_err());

        assert!(running.reserve(lease).is_err());
        assert!(running.external_effect_possible("attempt-a").is_err());
        assert!(running.cancel_reservation("attempt-a").is_err());
        assert_eq!(running.acknowledge("attempt-a").unwrap(), running);

        for occupied in [delivering, uncertain, running] {
            assert!(matches!(
                occupied.end_session().unwrap().phase,
                Phase::Empty
            ));
        }
    }

    #[test]
    fn recover_terminal_only_releases_exact_uncertain_identity() {
        let lease = lease(
            "attempt-a",
            "task-a",
            "11111111-1111-4111-8111-111111111111",
        );
        let empty = SessionWork::empty();
        let delivering = empty.reserve(lease.clone()).unwrap();
        let uncertain = delivering.external_effect_possible("attempt-a").unwrap();
        let running = uncertain.acknowledge("attempt-a").unwrap();

        for (work, state) in [
            (&empty, "empty"),
            (&delivering, "delivering"),
            (&running, "running"),
        ] {
            assert!(matches!(
                work.recover_terminal("attempt-a"),
                Err(TransitionError::Invalid {
                    action: Action::RecoverTerminal,
                    state: actual,
                }) if actual == state
            ));
        }
        assert_eq!(
            uncertain.recover_terminal("attempt-b"),
            Err(TransitionError::IdentityMismatch)
        );
        assert_eq!(
            uncertain.recover_terminal("attempt-a").unwrap().phase,
            Phase::Empty
        );
    }
}
