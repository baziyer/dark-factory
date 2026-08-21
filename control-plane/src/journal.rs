use std::{str::FromStr as _, time::Duration};

#[cfg(feature = "development-sqlite")]
use std::{path::Path, sync::Arc};

#[cfg(feature = "development-sqlite")]
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use sqlx::{
    ConnectOptions as _, PgPool, Row as _,
    postgres::{PgConnectOptions, PgPoolOptions, PgSslMode},
};
use url::Url;

use crate::maintainer::{Delivery, Disposition};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("delivery journal is unavailable")]
    Postgres(#[from] sqlx::Error),
    #[cfg(feature = "development-sqlite")]
    #[error("development delivery journal is unavailable")]
    Sqlite(#[from] rusqlite::Error),
    #[cfg(feature = "development-sqlite")]
    #[error("development delivery journal worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("delivery journal contains an invalid disposition")]
    InvalidDisposition,
    #[error("delivery journal lost a conflicting row")]
    MissingConflict,
    #[error("delivery journal lacks required privileges")]
    MissingPrivileges,
    #[error("control-plane schema migration conflicts with the expected revision")]
    MigrationConflict,
}

const MIGRATION_COMPONENT: &str = "maintainer_webhook";
const MIGRATION_REVISION: &str = "0001";
const MIGRATION_SQL: &str = include_str!("../migrations/0001_maintainer_deliveries.sql");

#[derive(Clone)]
pub(crate) enum DeliveryJournal {
    Postgres(PgPool),
    #[cfg(feature = "development-sqlite")]
    Sqlite(SqliteJournal),
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
    pub(crate) fn postgres(database_url: &str) -> Result<Self, sqlx::Error> {
        Ok(Self::Postgres(postgres_pool(database_url)?))
    }

    #[cfg(feature = "development-sqlite")]
    pub(crate) fn open_development(database: &Path) -> Result<Self, Error> {
        Ok(Self::Sqlite(SqliteJournal::open(database)?))
    }

    pub(crate) async fn ready(&self) -> Result<(), Error> {
        match self {
            Self::Postgres(pool) => postgres_ready(pool).await,
            #[cfg(feature = "development-sqlite")]
            Self::Sqlite(_) => Err(Error::MissingPrivileges),
        }
    }

    pub(crate) async fn record(&self, delivery: &Delivery) -> Result<Record, Error> {
        match self {
            Self::Postgres(pool) => postgres_record(pool, delivery).await,
            #[cfg(feature = "development-sqlite")]
            Self::Sqlite(journal) => {
                let journal = journal.clone();
                let delivery = delivery.clone();
                tokio::task::spawn_blocking(move || journal.record(&delivery)).await?
            }
        }
    }
}

pub(crate) fn postgres_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let url = Url::parse(database_url).map_err(sqlx::Error::config)?;
    let explicit = matches!(url.scheme(), "postgres" | "postgresql")
        && url.host_str().is_some()
        && !url.username().is_empty()
        && url.password().is_some()
        && url.path().len() > 1;
    if !explicit {
        return Err(sqlx::Error::Configuration(
            "DATABASE_URL must contain an explicit PostgreSQL host, user, password, and database"
                .into(),
        ));
    }
    let options = PgConnectOptions::from_str(database_url)?.disable_statement_logging();
    if !matches!(
        options.get_ssl_mode(),
        PgSslMode::Require | PgSslMode::VerifyCa | PgSslMode::VerifyFull
    ) {
        return Err(sqlx::Error::Configuration(
            "DATABASE_URL must require TLS".into(),
        ));
    }
    let pool = PgPoolOptions::new()
        .min_connections(0)
        .max_connections(2)
        .acquire_timeout(Duration::from_millis(750))
        .idle_timeout(Duration::from_secs(30))
        .connect_lazy_with(options);
    Ok(pool)
}

pub(crate) async fn migrate(pool: &PgPool) -> Result<(), Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(441684976339)")
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public.dark_factory_schema_migrations (
            component TEXT PRIMARY KEY,
            revision TEXT NOT NULL,
            digest TEXT NOT NULL CHECK (digest ~ '^[0-9a-f]{64}$'),
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
    )
    .execute(&mut *transaction)
    .await?;
    let existing = sqlx::query(
        "SELECT revision, digest
         FROM public.dark_factory_schema_migrations
         WHERE component = $1",
    )
    .bind(MIGRATION_COMPONENT)
    .fetch_optional(&mut *transaction)
    .await?;
    let digest = migration_digest();
    if let Some(row) = existing {
        let revision: String = row.try_get("revision")?;
        let stored_digest: String = row.try_get("digest")?;
        if revision != MIGRATION_REVISION || stored_digest != digest {
            return Err(Error::MigrationConflict);
        }
        transaction.commit().await?;
        return Ok(());
    }

    sqlx::raw_sql(MIGRATION_SQL)
        .execute(&mut *transaction)
        .await?;
    sqlx::query(
        "INSERT INTO public.dark_factory_schema_migrations (
            component, revision, digest
         ) VALUES ($1, $2, $3)",
    )
    .bind(MIGRATION_COMPONENT)
    .bind(MIGRATION_REVISION)
    .bind(digest)
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(())
}

fn migration_digest() -> String {
    hex::encode(Sha256::digest(MIGRATION_SQL.as_bytes()))
}

async fn postgres_ready(pool: &PgPool) -> Result<(), Error> {
    sqlx::query(
        "SELECT delivery_id, hook_id, target_id, target_type, event, action,
                body_digest, disposition, secret_revision, received_at
         FROM public.maintainer_deliveries
         WHERE FALSE",
    )
    .fetch_optional(pool)
    .await?;
    let migration = sqlx::query(
        "SELECT revision, digest
         FROM public.dark_factory_schema_migrations
         WHERE component = $1",
    )
    .bind(MIGRATION_COMPONENT)
    .fetch_optional(pool)
    .await?
    .ok_or(Error::MigrationConflict)?;
    let revision: String = migration.try_get("revision")?;
    let digest: String = migration.try_get("digest")?;
    if revision != MIGRATION_REVISION || digest != migration_digest() {
        return Err(Error::MigrationConflict);
    }
    let allowed: bool = sqlx::query_scalar(
        "SELECT has_table_privilege(
            current_user,
            'public.maintainer_deliveries',
            'SELECT,INSERT'
        )",
    )
    .fetch_one(pool)
    .await?;
    if !allowed {
        return Err(Error::MissingPrivileges);
    }
    Ok(())
}

async fn postgres_record(pool: &PgPool, delivery: &Delivery) -> Result<Record, Error> {
    let mut transaction = pool.begin().await?;
    let inserted = sqlx::query(
        "INSERT INTO public.maintainer_deliveries (
            delivery_id, hook_id, target_id, target_type, event, action,
            body_digest, disposition, secret_revision
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         ON CONFLICT (delivery_id) DO NOTHING",
    )
    .bind(&delivery.delivery_id)
    .bind(delivery.hook_id)
    .bind(delivery.target_id)
    .bind(&delivery.target_type)
    .bind(&delivery.event)
    .bind(&delivery.action)
    .bind(&delivery.body_digest)
    .bind(delivery.disposition.as_str())
    .bind(&delivery.secret_revision)
    .execute(&mut *transaction)
    .await?
    .rows_affected();
    if inserted == 1 {
        transaction.commit().await?;
        return Ok(Record::New);
    }

    let row = sqlx::query(
        "SELECT hook_id, target_id, target_type, event, action,
                body_digest, secret_revision, disposition
         FROM public.maintainer_deliveries
         WHERE delivery_id = $1",
    )
    .bind(&delivery.delivery_id)
    .fetch_optional(&mut *transaction)
    .await?
    .ok_or(Error::MissingConflict)?;
    let stored = StoredDelivery {
        hook_id: row.try_get("hook_id")?,
        target_id: row.try_get("target_id")?,
        target_type: row.try_get("target_type")?,
        event: row.try_get("event")?,
        action: row.try_get("action")?,
        body_digest: row.try_get("body_digest")?,
        secret_revision: row.try_get("secret_revision")?,
        disposition: row.try_get("disposition")?,
    };
    let result = stored.replay(delivery)?;
    transaction.commit().await?;
    Ok(result)
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
                    disposition IN ('ping', 'policy_rejected', 'payload_rejected')
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

    fn record(&self, delivery: &Delivery) -> Result<Record, Error> {
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
