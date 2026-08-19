//! Shared durable state and post-commit event publication.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use factory_core::{AgentId, EventEnvelope};
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast};

use crate::store::{Store, StoreError};

const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct DaemonState {
    store: Arc<Mutex<Store>>,
    events: broadcast::Sender<EventEnvelope>,
    operator_token: Arc<str>,
    /// One outstanding-delivery slot per agent, shared by every delivery
    /// path (`execution.rs`'s `deliver_pending`, `Handle::start_task`, and
    /// `stop_hook_reply`): whichever composes a delivery first holds the
    /// agent's slot until it acks/times out/commits, so a second delivery
    /// attempt racing the same agent (the dispatcher's tick-driven
    /// `deliver_pending` racing a `Stop` hook's own inline
    /// `stop_hook_reply` -- both see the session go `idle` at the same
    /// instant, since `record_hook_event`'s `Stop` arm always flips state
    /// to `idle` *before* `local_api.rs` calls `stop_hook_reply`) finds the
    /// slot held and skips instead of independently recomposing and
    /// redelivering the same task/messages (this track's item 1; see
    /// `Store::open_run_episode`'s `has_open` check for why a *second*
    /// task delivery is merely rejected rather than silently duplicated,
    /// and why that alone was not enough -- an agent inbox message has no
    /// equivalent guard, so two concurrent `compose_delivery` calls could
    /// each read the same undelivered messages and each type/reply them
    /// before either committed).
    ///
    /// Never pruned: bounded by the number of agents that have ever
    /// attempted a delivery in this daemon's lifetime, which is small
    /// relative to daemon uptime memory budgets and never grows within one
    /// agent's lifetime (one `Arc<AsyncMutex<()>>` reused for every
    /// delivery attempt).
    delivery_slots: Arc<Mutex<HashMap<AgentId, Arc<AsyncMutex<()>>>>>,
    /// Assignment mutations are infrequent and must be serialized with the
    /// owner delivery barrier. A bounded gate is simpler and safer than a
    /// registry keyed by an unbounded stream of task IDs.
    assignment_gate: Arc<AsyncMutex<()>>,
    /// All repository mutations pass through one daemon-owned committer.
    /// Read-only status/diff operations share this lock too, so their output
    /// is never captured halfway through a commit or push.
    repository_slot: Arc<AsyncMutex<()>>,
}

/// Held for the duration of one delivery attempt (compose through commit,
/// or compose through a timed-out ack); dropping it (however the holder
/// returns -- success, ack timeout, or error) frees the agent's slot for
/// the next attempt. See [`DaemonState::try_delivery_slot`].
pub struct DeliverySlot {
    _guard: OwnedMutexGuard<()>,
}

#[derive(Debug, Error)]
pub enum DaemonStateError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("store lock was poisoned")]
    StoreLockPoisoned,
    #[error("store worker failed")]
    StoreWorkerFailed,
}

impl DaemonState {
    #[must_use]
    pub fn new(store: Store) -> Self {
        Self::with_operator_token(store, "test-operator-token".to_owned())
    }

    #[must_use]
    pub fn with_operator_token(store: Store, operator_token: String) -> Self {
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            store: Arc::new(Mutex::new(store)),
            events,
            operator_token: Arc::from(operator_token),
            delivery_slots: Arc::new(Mutex::new(HashMap::new())),
            assignment_gate: Arc::new(AsyncMutex::new(())),
            repository_slot: Arc::new(AsyncMutex::new(())),
        }
    }

    #[must_use]
    pub fn operator_token_matches(&self, provided: &str) -> bool {
        crate::store::constant_time_eq(self.operator_token.as_bytes(), provided.as_bytes())
    }

    pub async fn repository_slot(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.repository_slot).lock_owned().await
    }

    /// Attempts to claim `agent_id`'s single pending-delivery slot,
    /// returning `None` (skip this delivery attempt) if another one is
    /// already in flight. Non-blocking by design: a delivery attempt that
    /// loses the race must not queue up behind the winner (that would
    /// still redeliver once it got its turn, since the winner's commit
    /// already satisfied the same pending work) -- it simply defers to
    /// whatever the current holder is doing, exactly like a wake trigger
    /// that arrives while the dispatcher is already busy with that agent.
    #[must_use]
    pub fn try_delivery_slot(&self, agent_id: &AgentId) -> Option<DeliverySlot> {
        let lock = self.delivery_lock(agent_id);
        lock.try_lock_owned()
            .ok()
            .map(|guard| DeliverySlot { _guard: guard })
    }

    /// Waits for the owner-side delivery barrier. Assignment changes use
    /// this blocking form so a task cannot be moved while the old worker's
    /// delivery is composing, typing, awaiting its hook, or committing.
    pub async fn lock_delivery_slot(&self, agent_id: &AgentId) -> DeliverySlot {
        DeliverySlot {
            _guard: self.delivery_lock(agent_id).lock_owned().await,
        }
    }

    /// Serializes all assignment mutations, including moves between workers
    /// and moves to the project backlog.
    pub async fn lock_assignment_slot(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.assignment_gate).lock_owned().await
    }

    fn delivery_lock(&self, agent_id: &AgentId) -> Arc<AsyncMutex<()>> {
        let mut slots = self
            .delivery_slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            slots
                .entry(agent_id.clone())
                .or_insert_with(|| Arc::new(AsyncMutex::new(()))),
        )
    }

    /// `true` if another delivery attempt currently holds `agent_id`'s
    /// pending-delivery slot -- a non-claiming peek, immediately releasing
    /// the slot again if it happens to be free (see [`Self::try_delivery_slot`]).
    ///
    /// Used by `execution::commit_pending_delivery_on_prompt` to decide
    /// whether a `UserPromptSubmit` hook is plausibly the ack for an
    /// in-flight PTY-typed delivery (slot held by `deliver_pending`/
    /// `Handle::start_task`, so yes, safe to commit whatever they just
    /// typed right now) versus an operator's or provider's own unrelated
    /// prompt with no delivery attempt behind it at all (slot free -- a
    /// pending task must not be silently auto-attached to whatever the
    /// operator happened to be typing; that task's own real delivery
    /// still goes through the ordinary idle-dispatch or Stop-hook
    /// block-reply path, untouched by this).
    #[must_use]
    pub fn delivery_in_flight(&self, agent_id: &AgentId) -> bool {
        self.try_delivery_slot(agent_id).is_none()
    }

    async fn run_with_store<T, F>(&self, operation: F) -> Result<T, DaemonStateError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<T, StoreError> + Send + 'static,
    {
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let mut store = store
                .lock()
                .map_err(|_| DaemonStateError::StoreLockPoisoned)?;
            operation(&mut store).map_err(DaemonStateError::Store)
        })
        .await
        .map_err(|_| DaemonStateError::StoreWorkerFailed)?
    }

    pub async fn with_store<T, F>(&self, operation: F) -> Result<T, DaemonStateError>
    where
        T: Send + 'static,
        F: FnOnce(&Store) -> Result<T, StoreError> + Send + 'static,
    {
        self.run_with_store(move |store| operation(store)).await
    }

    pub async fn commit_and_publish<T, F>(&self, operation: F) -> Result<T, DaemonStateError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Store) -> Result<(T, Vec<EventEnvelope>), StoreError> + Send + 'static,
    {
        let events = self.events.clone();
        self.run_with_store(move |store| {
            let (value, committed) = operation(store)?;
            if let (Some(first), Some(last)) = (committed.first(), committed.last()) {
                tracing::info!(
                    target: "factoryd.state",
                    event = "durable_events_committed",
                    event_count = committed.len(),
                    first_sequence = first.sequence,
                    last_sequence = last.sequence
                );
            }
            for event in committed {
                let _ = events.send(event);
            }
            Ok(value)
        })
        .await
    }

    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use factory_core::AgentId;

    #[tokio::test]
    async fn reassignment_barrier_waits_for_old_owner_delivery() {
        let state = DaemonState::new(Store::open_in_memory().unwrap());
        let old_owner = AgentId::try_from("worker-1").unwrap();
        let delivery = state.try_delivery_slot(&old_owner).unwrap();
        let assignment = state.lock_assignment_slot().await;

        let state_for_move = state.clone();
        let old_owner_for_move = old_owner.clone();
        let waiter = tokio::spawn(async move {
            let _assignment = state_for_move.lock_assignment_slot().await;
            state_for_move.lock_delivery_slot(&old_owner_for_move).await
        });
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished(), "move crossed the in-flight delivery");
        drop(assignment);
        // The move still cannot pass until the old worker's prompt/commit
        // has released its delivery barrier.
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(delivery);
        waiter.await.unwrap();
    }

    #[test]
    fn operator_cleanup_secret_is_exact_and_constant_time_compared() {
        let state = DaemonState::with_operator_token(
            Store::open_in_memory().unwrap(),
            "operator-secret".into(),
        );
        assert!(state.operator_token_matches("operator-secret"));
        assert!(!state.operator_token_matches("wrong-secret"));
        assert!(!state.operator_token_matches("operator-secret-extra"));
    }
}
