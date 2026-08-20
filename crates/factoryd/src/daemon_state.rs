//! Shared durable state and post-commit event publication.

use std::sync::{Arc, Mutex};

use factory_core::EventEnvelope;
use thiserror::Error;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard, broadcast};

use crate::store::{Store, StoreError};

const EVENT_CHANNEL_CAPACITY: usize = 256;

#[derive(Clone)]
pub struct DaemonState {
    store: Arc<Mutex<Store>>,
    events: broadcast::Sender<EventEnvelope>,
    /// Assignment mutations are infrequent and must be serialized with the
    /// owner delivery barrier.
    assignment_gate: Arc<AsyncMutex<()>>,
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
            assignment_gate: Arc::new(AsyncMutex::new(())),
        }
    }

    /// Serializes all assignment mutations, including moves between workers
    /// and moves to the project backlog.
    pub async fn lock_assignment_slot(&self) -> OwnedMutexGuard<()> {
        Arc::clone(&self.assignment_gate).lock_owned().await
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
