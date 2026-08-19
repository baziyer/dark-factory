//! Shared durable state and post-commit event publication.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use factory_core::{AgentId, EventEnvelope};
use thiserror::Error;
use tokio::sync::{broadcast, Mutex as AsyncMutex, OwnedMutexGuard};

use crate::store::{Store, StoreError};

const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct DaemonState {
    store: Arc<Mutex<Store>>,
    events: broadcast::Sender<EventEnvelope>,
    /// One outstanding-delivery slot per agent, shared by every delivery
    /// path (`execution.rs`'s `deliver_pending`, `Handle::start_task`, and
    /// `stop_hook_reply`): whichever composes a delivery first holds the
    /// agent's slot until it acks/times out/commits, so a second delivery
    /// attempt racing the same agent finds the slot held and skips instead
    /// of independently recomposing and redelivering the same work.
    ///
    /// Never pruned: bounded by the number of agents that have ever
    /// attempted a delivery in this daemon's lifetime.
    delivery_slots: Arc<Mutex<HashMap<AgentId, Arc<AsyncMutex<()>>>>>,
    /// Assignment mutations are infrequent and must be serialized with the
    /// owner delivery barrier.
    assignment_gate: Arc<AsyncMutex<()>>,
    /// All repository mutations pass through one daemon-owned committer.
    repository_slot: Arc<AsyncMutex<()>>,
}

/// Held for the duration of one delivery attempt (compose through commit,
/// or compose through a timed-out ack); dropping it frees the agent's slot.
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
        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Self {
            store: Arc::new(Mutex::new(store)),
            events,
            delivery_slots: Arc::new(Mutex::new(HashMap::new())),
            assignment_gate: Arc::new(AsyncMutex::new(())),
            repository_slot: Arc::new(AsyncMutex::new(())),
        }
    }

    pub async fn repository_slot(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::clone(&self.repository_slot).lock_owned().await
    }

    /// Attempts to claim `agent_id`'s single pending-delivery slot,
    /// returning `None` if another attempt is already in flight.
    #[must_use]
    pub fn try_delivery_slot(&self, agent_id: &AgentId) -> Option<DeliverySlot> {
        let lock = self.delivery_lock(agent_id);
        lock.try_lock_owned()
            .ok()
            .map(|guard| DeliverySlot { _guard: guard })
    }

    /// Waits for the owner-side delivery barrier. Assignment changes use
    /// this blocking form so a task cannot move while delivery is composing,
    /// typing, awaiting its hook, or committing.
    pub async fn lock_delivery_slot(&self, agent_id: &AgentId) -> DeliverySlot {
        DeliverySlot {
            _guard: self.delivery_lock(agent_id).lock_owned().await,
        }
    }

    /// Serializes all assignment mutations, including moves to the backlog.
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
    /// pending-delivery slot.
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
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(delivery);
        waiter.await.unwrap();
    }
}
