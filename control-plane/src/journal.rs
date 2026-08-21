use std::{path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};

use crate::maintainer::{Delivery, Disposition};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("delivery journal failed")]
    Sqlite(#[from] rusqlite::Error),
}

/// Development and causal-test journal only. SQLite is not durable on a
/// serverless filesystem, so no production constructor exposes this adapter.
#[derive(Clone)]
pub(crate) struct DeliveryJournal {
    database: Arc<Path>,
}

pub(crate) enum Record {
    New,
    Replay(Disposition),
    Conflict,
}

struct StoredDelivery {
    hook_id: i64,
    target_id: i64,
    target_type: String,
    event: String,
    action: Option<String>,
    body_digest: String,
    secret_revision: String,
    disposition: String,
}

impl DeliveryJournal {
    pub(crate) fn open(database: &Path) -> Result<Self, Error> {
        let journal = Self {
            database: Arc::from(database),
        };
        let connection = journal.connection()?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS maintainer_deliveries (
                delivery_id TEXT PRIMARY KEY CHECK (
                    length(delivery_id) = 36
                    AND delivery_id NOT GLOB '*[^0-9a-f-]*'
                ),
                hook_id INTEGER NOT NULL CHECK (hook_id > 0),
                target_id INTEGER NOT NULL CHECK (target_id > 0),
                target_type TEXT NOT NULL CHECK (
                    length(CAST(target_type AS BLOB)) BETWEEN 1 AND 64
                    AND target_type NOT GLOB '*[^a-zA-Z0-9_-]*'
                ),
                event TEXT NOT NULL CHECK (
                    length(CAST(event AS BLOB)) BETWEEN 1 AND 64
                    AND event NOT GLOB '*[^a-z_]*'
                ),
                action TEXT CHECK (
                    action IS NULL OR (
                        length(CAST(action AS BLOB)) BETWEEN 1 AND 64
                        AND action NOT GLOB '*[^a-z_]*'
                    )
                ),
                body_digest TEXT NOT NULL CHECK (
                    length(body_digest) = 64
                    AND body_digest NOT GLOB '*[^0-9a-f]*'
                ),
                disposition TEXT NOT NULL CHECK (
                    disposition IN (
                        'ping',
                        'lifecycle_audited',
                        'event_rejected',
                        'payload_rejected'
                    )
                ),
                secret_revision TEXT NOT NULL CHECK (
                    length(CAST(secret_revision AS BLOB)) BETWEEN 1 AND 64
                    AND secret_revision NOT GLOB '*[^a-z0-9_-]*'
                ),
                received_at INTEGER NOT NULL DEFAULT (unixepoch())
                    CHECK (received_at >= 0)
            ) STRICT;",
        )?;
        Ok(journal)
    }

    pub(crate) fn record(&self, delivery: &Delivery) -> Result<Record, Error> {
        let mut connection = self.connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<StoredDelivery> = transaction
            .query_row(
                "SELECT hook_id, target_id, target_type, event, action,
                        body_digest, secret_revision, disposition
                 FROM maintainer_deliveries
                 WHERE delivery_id = ?1",
                [delivery.delivery_id.as_str()],
                |row| {
                    Ok(StoredDelivery {
                        hook_id: row.get(0)?,
                        target_id: row.get(1)?,
                        target_type: row.get(2)?,
                        event: row.get(3)?,
                        action: row.get(4)?,
                        body_digest: row.get(5)?,
                        secret_revision: row.get(6)?,
                        disposition: row.get(7)?,
                    })
                },
            )
            .optional()?;
        if let Some(stored) = existing {
            let exact = stored.hook_id == delivery.hook_id
                && stored.target_id == delivery.target_id
                && stored.target_type == delivery.target_type
                && stored.event == delivery.event
                && stored.action == delivery.action
                && stored.body_digest == delivery.body_digest
                && stored.secret_revision == delivery.secret_revision
                && stored.disposition == delivery.disposition.as_str();
            let result = if exact {
                Record::Replay(Disposition::from_database(&stored.disposition)?)
            } else {
                Record::Conflict
            };
            transaction.commit()?;
            return Ok(result);
        }
        transaction.execute(
            "INSERT INTO maintainer_deliveries (
                delivery_id, hook_id, target_id, target_type, event, action,
                body_digest, disposition, secret_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                delivery.delivery_id,
                delivery.hook_id,
                delivery.target_id,
                delivery.target_type,
                delivery.event,
                delivery.action,
                delivery.body_digest,
                delivery.disposition.as_str(),
                delivery.secret_revision,
            ],
        )?;
        transaction.commit()?;
        Ok(Record::New)
    }

    fn connection(&self) -> Result<Connection, rusqlite::Error> {
        let connection = Connection::open(self.database.as_ref())?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(connection)
    }
}
