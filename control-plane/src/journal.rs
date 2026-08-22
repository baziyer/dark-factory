#[cfg(feature = "development-sqlite")]
use std::{path::Path, sync::Arc, time::Duration};

#[cfg(feature = "development-sqlite")]
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
use serde::Deserialize;
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
use sha2::{Digest as _, Sha256};
#[cfg(target_arch = "wasm32")]
use worker::{
    DurableObject, Env, Method, ObjectNamespace, Request, RequestInit, Response, SqlStorage,
    SqlStorageValue, State, durable_object,
};

use crate::maintainer::{Delivery, Disposition};

#[cfg(target_arch = "wasm32")]
pub(crate) const NAMESPACE_BINDING: &str = "DARK_FACTORY_MAINTAINER_DELIVERIES";
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
const MIGRATION_COMPONENT: &str = "maintainer_webhook";
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
const MIGRATION_REVISION: &str = "0001";
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
const MIGRATION_SQL: &str = include_str!("../migrations/0001_maintainer_deliveries.sql");
#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
const MIGRATION_TABLE_SQL: &str = "CREATE TABLE IF NOT EXISTS control_plane_migrations (
    component TEXT PRIMARY KEY,
    revision TEXT NOT NULL,
    digest TEXT NOT NULL CHECK (
        length(digest) = 64 AND digest NOT GLOB '*[^0-9a-f]*'
    )
) STRICT;";

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[cfg(target_arch = "wasm32")]
    #[error("Durable Object delivery journal is unavailable")]
    Cloudflare(#[from] worker::Error),
    #[cfg(target_arch = "wasm32")]
    #[error("Durable Object delivery journal message is invalid")]
    Json(#[from] serde_json::Error),
    #[cfg(feature = "development-sqlite")]
    #[error("development delivery journal is unavailable")]
    Sqlite(#[from] rusqlite::Error),
    #[cfg(feature = "development-sqlite")]
    #[error("development delivery journal worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
    #[error("delivery journal contains an invalid disposition")]
    InvalidDisposition,
    #[cfg(target_arch = "wasm32")]
    #[error("delivery journal lost a conflicting row")]
    MissingConflict,
    #[error("delivery journal schema differs from the reviewed migration")]
    InvalidSchema,
}

#[derive(Clone)]
pub(crate) enum DeliveryJournal {
    #[cfg(target_arch = "wasm32")]
    Cloudflare(CloudflareJournal),
    #[cfg(feature = "development-sqlite")]
    Sqlite(SqliteJournal),
    #[cfg(all(not(target_arch = "wasm32"), not(feature = "development-sqlite")))]
    #[allow(dead_code)]
    Unavailable,
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Record {
    New,
    Replay(Disposition),
    Conflict,
}

#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
#[derive(Deserialize)]
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

#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
impl StoredDelivery {
    fn matches(&self, delivery: &Delivery) -> bool {
        self.hook_id == delivery.hook_id
            && self.target_id == delivery.target_id
            && self.target_type == delivery.target_type
            && self.event == delivery.event
            && self.action == delivery.action
            && self.body_digest == delivery.body_digest
            && self.secret_revision == delivery.secret_revision
            && self.disposition == delivery.disposition.as_str()
    }

    fn replay(&self, delivery: &Delivery) -> Result<Record, Error> {
        if self.matches(delivery) {
            Ok(Record::Replay(
                Disposition::from_database(&self.disposition).ok_or(Error::InvalidDisposition)?,
            ))
        } else {
            Ok(Record::Conflict)
        }
    }
}

impl DeliveryJournal {
    #[cfg(target_arch = "wasm32")]
    pub(crate) const fn cloudflare(namespace: ObjectNamespace, app_id: i64) -> Self {
        Self::Cloudflare(CloudflareJournal { namespace, app_id })
    }

    #[cfg(feature = "development-sqlite")]
    pub(crate) fn open_development(database: &Path) -> Result<Self, Error> {
        Ok(Self::Sqlite(SqliteJournal::open(database)?))
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) async fn ready(&self) -> Result<(), Error> {
        match self {
            Self::Cloudflare(journal) => journal.ready().await,
        }
    }

    pub(crate) async fn record(&self, _delivery: &Delivery) -> Result<Record, Error> {
        match self {
            #[cfg(target_arch = "wasm32")]
            Self::Cloudflare(journal) => journal.record(_delivery).await,
            #[cfg(feature = "development-sqlite")]
            Self::Sqlite(journal) => {
                let journal = journal.clone();
                let delivery = _delivery.clone();
                tokio::task::spawn_blocking(move || journal.record(&delivery)).await?
            }
            #[cfg(all(not(target_arch = "wasm32"), not(feature = "development-sqlite")))]
            Self::Unavailable => Err(Error::InvalidSchema),
        }
    }
}

#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
fn migration_digest() -> String {
    hex::encode(Sha256::digest(MIGRATION_SQL.as_bytes()))
}

#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
fn normalized_sql(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(any(target_arch = "wasm32", feature = "development-sqlite"))]
fn expected_stored_sql(sql: &str) -> String {
    normalized_sql(
        sql.trim()
            .trim_end_matches(';')
            .replacen(" IF NOT EXISTS ", " ", 1)
            .as_str(),
    )
}

#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub(crate) struct CloudflareJournal {
    namespace: ObjectNamespace,
    app_id: i64,
}

#[cfg(target_arch = "wasm32")]
impl CloudflareJournal {
    async fn ready(&self) -> Result<(), Error> {
        let name = format!("maintainer:{}:ready", self.app_id);
        let stub = self.namespace.get_by_name(&name)?;
        let response = stub
            .fetch_with_str("https://journal.internal/ready")
            .await?;
        if response.status_code() == 200 {
            Ok(())
        } else {
            Err(Error::InvalidSchema)
        }
    }

    async fn record(&self, delivery: &Delivery) -> Result<Record, Error> {
        let stub = self
            .namespace
            .get_by_name(&delivery_shard_name(self.app_id, &delivery.delivery_id))?;
        let mut init = RequestInit::new();
        init.with_method(Method::Post)
            .with_body(Some(serde_json::to_string(delivery)?.into()));
        let request = Request::new_with_init("https://journal.internal/record", &init)?;
        let mut response = stub.fetch_with_request(request).await?;
        if response.status_code() != 200 {
            return Err(Error::InvalidSchema);
        }
        Ok(response.json().await?)
    }
}

#[cfg(target_arch = "wasm32")]
fn delivery_shard_name(app_id: i64, delivery_id: &str) -> String {
    // A sha256-derived byte avoids UUID time-prefix hotspots while keeping
    // every replay identity for one App on the same Durable Object.
    let sha256 = Sha256::digest(delivery_id.as_bytes());
    format!("maintainer:{app_id}:{:02x}", sha256[0])
}

#[cfg(target_arch = "wasm32")]
#[durable_object]
pub struct MaintainerDeliveryJournal {
    sql: SqlStorage,
}

#[cfg(target_arch = "wasm32")]
impl DurableObject for MaintainerDeliveryJournal {
    fn new(state: State, _env: Env) -> Self {
        let sql = state.storage().sql();
        Self { sql }
    }

    async fn fetch(&self, mut request: Request) -> worker::Result<Response> {
        if initialize_cloudflare_schema(&self.sql).is_err() {
            return Response::error("journal unavailable", 503);
        }
        match (request.method(), request.path().as_str()) {
            (Method::Get, "/ready") => Response::ok("ready"),
            (Method::Post, "/record") => {
                let result = async {
                    let delivery: Delivery = request.json().await?;
                    record_cloudflare(&self.sql, &delivery)
                }
                .await;
                match result {
                    Ok(record) => Response::from_json(&record),
                    Err(_) => Response::error("journal unavailable", 503),
                }
            }
            _ => Response::error("not found", 404),
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn initialize_cloudflare_schema(sql: &SqlStorage) -> Result<(), Error> {
    sql.exec(MIGRATION_TABLE_SQL, None)?;
    sql.exec(MIGRATION_SQL, None)?;
    sql.exec(
        "INSERT INTO control_plane_migrations (component, revision, digest)
         VALUES (?, ?, ?) ON CONFLICT(component) DO NOTHING",
        vec![
            MIGRATION_COMPONENT.into(),
            MIGRATION_REVISION.into(),
            migration_digest().into(),
        ],
    )?;
    audit_cloudflare_schema(sql)
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct SchemaRow {
    sql: String,
}

#[cfg(target_arch = "wasm32")]
#[derive(Deserialize)]
struct MigrationRow {
    revision: String,
    digest: String,
}

#[cfg(target_arch = "wasm32")]
fn audit_cloudflare_schema(sql: &SqlStorage) -> Result<(), Error> {
    let delivery_schema = sql
        .exec(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'maintainer_deliveries'",
            None,
        )?
        .to_array::<SchemaRow>()?;
    let migration_schema = sql
        .exec(
            "SELECT sql FROM sqlite_schema
             WHERE type = 'table' AND name = 'control_plane_migrations'",
            None,
        )?
        .to_array::<SchemaRow>()?;
    let migration = sql
        .exec(
            "SELECT revision, digest FROM control_plane_migrations WHERE component = ?",
            vec![MIGRATION_COMPONENT.into()],
        )?
        .to_array::<MigrationRow>()?;
    let exact = delivery_schema.len() == 1
        && normalized_sql(&delivery_schema[0].sql) == expected_stored_sql(MIGRATION_SQL)
        && migration_schema.len() == 1
        && normalized_sql(&migration_schema[0].sql) == expected_stored_sql(MIGRATION_TABLE_SQL)
        && migration.len() == 1
        && migration[0].revision == MIGRATION_REVISION
        && migration[0].digest == migration_digest();
    exact.then_some(()).ok_or(Error::InvalidSchema)
}

#[cfg(target_arch = "wasm32")]
fn record_cloudflare(sql: &SqlStorage, delivery: &Delivery) -> Result<Record, Error> {
    let insert = sql.exec(
        "INSERT INTO maintainer_deliveries (
            delivery_id, hook_id, target_id, target_type, event, action,
            body_digest, disposition, secret_revision
         ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(delivery_id) DO NOTHING",
        vec![
            delivery.delivery_id.as_str().into(),
            delivery.hook_id.to_string().into(),
            delivery.target_id.to_string().into(),
            delivery.target_type.as_str().into(),
            delivery.event.as_str().into(),
            SqlStorageValue::from(delivery.action.clone()),
            delivery.body_digest.as_str().into(),
            delivery.disposition.as_str().into(),
            delivery.secret_revision.as_str().into(),
        ],
    )?;
    if insert.rows_written() == 1 {
        return Ok(Record::New);
    }
    let stored = sql
        .exec(
            "SELECT hook_id, target_id, target_type, event, action,
                    body_digest, secret_revision, disposition
             FROM maintainer_deliveries WHERE delivery_id = ?",
            vec![delivery.delivery_id.as_str().into()],
        )?
        .to_array::<StoredDelivery>()?;
    match stored.as_slice() {
        [stored] => stored.replay(delivery),
        _ => Err(Error::MissingConflict),
    }
}

#[cfg(feature = "development-sqlite")]
#[derive(Clone)]
pub(crate) struct SqliteJournal {
    database: Arc<Path>,
}

#[cfg(feature = "development-sqlite")]
impl SqliteJournal {
    fn open(database: &Path) -> Result<Self, Error> {
        let journal = Self {
            database: Arc::from(database),
        };
        let connection = journal.connection()?;
        connection.execute_batch(MIGRATION_TABLE_SQL)?;
        connection.execute_batch(MIGRATION_SQL)?;
        connection.execute(
            "INSERT INTO control_plane_migrations (component, revision, digest)
             VALUES (?1, ?2, ?3) ON CONFLICT(component) DO NOTHING",
            params![MIGRATION_COMPONENT, MIGRATION_REVISION, migration_digest()],
        )?;
        audit_sqlite_schema(&connection)?;
        Ok(journal)
    }

    fn record(&self, delivery: &Delivery) -> Result<Record, Error> {
        let mut connection = self.connection()?;
        audit_sqlite_schema(&connection)?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing: Option<StoredDelivery> = transaction
            .query_row(
                "SELECT hook_id, target_id, target_type, event, action,
                        body_digest, secret_revision, disposition
                 FROM maintainer_deliveries WHERE delivery_id = ?1",
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
            let result = stored.replay(delivery)?;
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
        connection.busy_timeout(Duration::from_secs(5))?;
        Ok(connection)
    }
}

#[cfg(feature = "development-sqlite")]
fn audit_sqlite_schema(connection: &Connection) -> Result<(), Error> {
    let schema = |name: &str| -> Result<Option<String>, rusqlite::Error> {
        connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
                [name],
                |row| row.get(0),
            )
            .optional()
    };
    let migration: Option<(String, String)> = connection
        .query_row(
            "SELECT revision, digest FROM control_plane_migrations WHERE component = ?1",
            [MIGRATION_COMPONENT],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let exact = schema("maintainer_deliveries")?
        .is_some_and(|sql| normalized_sql(&sql) == expected_stored_sql(MIGRATION_SQL))
        && schema("control_plane_migrations")?
            .is_some_and(|sql| normalized_sql(&sql) == expected_stored_sql(MIGRATION_TABLE_SQL))
        && migration.is_some_and(|(revision, digest)| {
            revision == MIGRATION_REVISION && digest == migration_digest()
        });
    exact.then_some(()).ok_or(Error::InvalidSchema)
}
