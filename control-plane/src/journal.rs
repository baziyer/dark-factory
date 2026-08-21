use std::{str::FromStr as _, time::Duration};

#[cfg(feature = "development-sqlite")]
use std::{path::Path, sync::Arc};

#[cfg(feature = "development-sqlite")]
use rusqlite::{Connection, OptionalExtension as _, TransactionBehavior, params};
use sha2::{Digest as _, Sha256};
use sqlx::{
    ConnectOptions as _, PgConnection, PgPool, Row as _,
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
    #[error("delivery journal schema differs from the reviewed migration")]
    InvalidSchema,
    #[error("control-plane schema migration conflicts with the expected revision")]
    MigrationConflict,
    #[cfg(feature = "provision-runtime")]
    #[error("runtime database role has unexpected memberships or ownership")]
    RuntimeRoleDrift,
    #[cfg(feature = "provision-runtime")]
    #[error("PostgreSQL 17 or newer is required")]
    UnsupportedPostgres,
    #[cfg(feature = "provision-runtime")]
    #[error("database is not the expected Neon project and branch")]
    InvalidNeonIdentity,
}

const MIGRATION_COMPONENT: &str = "maintainer_webhook";
const MIGRATION_REVISION: &str = "0001";
const MIGRATION_SQL: &str = include_str!("../migrations/0001_maintainer_deliveries.sql");
pub(crate) const RUNTIME_ROLE: &str = "dark_factory_broker_runtime";
#[cfg(feature = "provision-runtime")]
const RUNTIME_ROLE_COMMENT: &str = "dark-factory-control-plane managed runtime role v1";
// SQLx reads these process-global settings before it parses a URL and exposes
// no API for clearing them. Managed Postgres connection aliases are safe
// because the required URL fields overwrite them below; these optional fields
// are not aliases and must remain explicit URL authority.
const UNSUPPORTED_SQLX_ENV: [&str; 4] = ["PGOPTIONS", "PGSSLCERT", "PGSSLKEY", "PGSSLROOTCERT"];

#[cfg(feature = "provision-runtime")]
pub(crate) struct NeonIdentity {
    pub(crate) project_id: String,
    pub(crate) branch_id: String,
}

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
    if UNSUPPORTED_SQLX_ENV
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err(sqlx::Error::Configuration(
            "process-global PostgreSQL options and certificates are unsupported".into(),
        ));
    }
    let url = Url::parse(database_url).map_err(sqlx::Error::config)?;
    let mut ssl_mode = false;
    let mut channel_binding = false;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "sslmode"
                if !ssl_mode
                    && matches!(value.as_ref(), "require" | "verify-ca" | "verify-full") =>
            {
                ssl_mode = true;
            }
            "channel_binding" if !channel_binding && value == "require" => {
                channel_binding = true;
            }
            _ => {
                return Err(sqlx::Error::Configuration(
                    "database URL contains an unsupported connection parameter".into(),
                ));
            }
        }
    }
    let explicit = matches!(url.scheme(), "postgres" | "postgresql")
        && url.host_str().is_some()
        && !url.username().is_empty()
        && url.password().is_some_and(|password| !password.is_empty())
        && url.path().len() > 1
        && !url.path()[1..].contains('/')
        && url.fragment().is_none()
        && ssl_mode;
    if !explicit {
        return Err(sqlx::Error::Configuration(
            "database URL must contain an explicit PostgreSQL host, user, password, and database"
                .into(),
        ));
    }
    let options = PgConnectOptions::from_str(database_url)?
        // SQLx initially reads libpq aliases. Reapply every field which may
        // otherwise survive when its ordinary URL spelling uses a default.
        .port(url.port().unwrap_or(5432))
        .application_name("dark-factory-control-plane")
        // `channel_binding` in Marketplace URLs is tolerated above but SQLx
        // 0.8 does not enforce it. Every actual connection therefore upgrades
        // independently to authenticated certificate and hostname checking.
        .ssl_mode(PgSslMode::VerifyFull)
        .disable_statement_logging();
    if !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull) {
        return Err(sqlx::Error::Configuration(
            "database URL must use verified TLS".into(),
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

#[cfg(feature = "provision-runtime")]
pub(crate) fn neon_owner_pool(database_url: &str) -> Result<PgPool, sqlx::Error> {
    let url = Url::parse(database_url).map_err(sqlx::Error::config)?;
    let host = url
        .host_str()
        .ok_or_else(|| sqlx::Error::Configuration("Neon owner URL must contain a host".into()))?;
    if host.len() <= ".neon.tech".len() || !host.ends_with(".neon.tech") {
        return Err(sqlx::Error::Configuration(
            "Neon owner URL host is not a Neon service host".into(),
        ));
    }
    postgres_pool(database_url)
}

#[cfg(feature = "provision-runtime")]
pub(crate) async fn verify_runtime(database_url: &str) -> Result<(), Error> {
    DeliveryJournal::postgres(database_url)?.ready().await
}

#[cfg(feature = "provision-runtime")]
async fn migrate(transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>) -> Result<(), Error> {
    let supported: bool =
        sqlx::query_scalar("SELECT current_setting('server_version_num')::integer >= 170000")
            .fetch_one(&mut **transaction)
            .await?;
    if !supported {
        return Err(Error::UnsupportedPostgres);
    }
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS public.dark_factory_schema_migrations (
            component TEXT NOT NULL,
            revision TEXT NOT NULL,
            digest TEXT NOT NULL,
            applied_at TIMESTAMPTZ NOT NULL DEFAULT now(),
            CONSTRAINT dark_factory_schema_migrations_pkey PRIMARY KEY (component),
            CONSTRAINT dark_factory_schema_migrations_digest_format
                CHECK (digest ~ '^[0-9a-f]{64}$')
        )",
    )
    .execute(&mut **transaction)
    .await?;
    let existing = sqlx::query(
        "SELECT revision, digest
         FROM public.dark_factory_schema_migrations
         WHERE component = $1",
    )
    .bind(MIGRATION_COMPONENT)
    .fetch_optional(&mut **transaction)
    .await?;
    let digest = migration_digest();
    if let Some(row) = existing {
        let revision: String = row.try_get("revision")?;
        let stored_digest: String = row.try_get("digest")?;
        if revision != MIGRATION_REVISION || stored_digest != digest {
            return Err(Error::MigrationConflict);
        }
        return Ok(());
    }

    sqlx::raw_sql(MIGRATION_SQL)
        .execute(&mut **transaction)
        .await?;
    sqlx::query(
        "INSERT INTO public.dark_factory_schema_migrations (
            component, revision, digest
         ) VALUES ($1, $2, $3)",
    )
    .bind(MIGRATION_COMPONENT)
    .bind(MIGRATION_REVISION)
    .bind(digest)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

#[cfg(feature = "provision-runtime")]
pub(crate) async fn prepare_runtime(
    pool: &PgPool,
    expected_project_id: &str,
) -> Result<NeonIdentity, Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(441684976339)")
        .execute(&mut *transaction)
        .await?;
    let (project_id, branch_id): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT current_setting('neon.project_id', true),
                current_setting('neon.branch_id', true)",
    )
    .fetch_one(&mut *transaction)
    .await?;
    let (Some(project_id), Some(branch_id)) = (project_id, branch_id) else {
        return Err(Error::InvalidNeonIdentity);
    };
    if project_id != expected_project_id
        || !neon_identifier_is_valid(&project_id)
        || !neon_identifier_is_valid(&branch_id)
    {
        return Err(Error::InvalidNeonIdentity);
    }
    migrate(&mut transaction).await?;
    let owner: String = sqlx::query_scalar("SELECT current_user")
        .fetch_one(&mut *transaction)
        .await?;
    if owner == RUNTIME_ROLE {
        return Err(Error::RuntimeRoleDrift);
    }

    let existing_role = sqlx::query(
        "SELECT rolcanlogin, rolinherit, rolsuper, rolcreatedb, rolcreaterole,
                rolreplication, rolbypassrls,
                pg_catalog.shobj_description(oid, 'pg_authid') AS comment
         FROM pg_catalog.pg_roles
         WHERE rolname = $1",
    )
    .bind(RUNTIME_ROLE)
    .fetch_optional(&mut *transaction)
    .await?;
    if let Some(role) = existing_role {
        let exact = !role.try_get::<bool, _>("rolinherit")?
            && !role.try_get::<bool, _>("rolsuper")?
            && !role.try_get::<bool, _>("rolcreatedb")?
            && !role.try_get::<bool, _>("rolcreaterole")?
            && !role.try_get::<bool, _>("rolreplication")?
            && !role.try_get::<bool, _>("rolbypassrls")?
            && role.try_get::<Option<String>, _>("comment")?.as_deref()
                == Some(RUNTIME_ROLE_COMMENT);
        if !exact {
            return Err(Error::RuntimeRoleDrift);
        }
    } else {
        let create_role: String = sqlx::query_scalar(
            "SELECT format(
                 'CREATE ROLE %I NOLOGIN NOINHERIT CONNECTION LIMIT -1 VALID UNTIL %L PASSWORD NULL',
                 $1, 'infinity'
             )",
        )
        .bind(RUNTIME_ROLE)
        .fetch_one(&mut *transaction)
        .await?;
        sqlx::raw_sql(&create_role)
            .execute(&mut *transaction)
            .await?;
        sqlx::raw_sql(
            "COMMENT ON ROLE dark_factory_broker_runtime IS 'dark-factory-control-plane managed runtime role v1'",
        )
        .execute(&mut *transaction)
        .await?;
    }

    let role_drift: bool = sqlx::query_scalar(
        "WITH runtime AS (
             SELECT oid
             FROM pg_catalog.pg_roles
             WHERE rolname = $1
         ),
         provisioning_owner AS (
             SELECT oid
             FROM pg_catalog.pg_roles
             WHERE rolname = current_user
         ),
         database_row AS (
             SELECT datdba
             FROM pg_catalog.pg_database
             WHERE datname = current_database()
         )
         SELECT provisioning_owner.oid <> database_row.datdba
             OR (
                 SELECT count(*)
                 FROM pg_catalog.pg_auth_members membership
                 WHERE membership.member = runtime.oid
                    OR membership.roleid = runtime.oid
             ) <> 1
             OR NOT EXISTS (
                 SELECT 1
                 FROM pg_catalog.pg_auth_members membership
                 WHERE membership.roleid = runtime.oid
                   AND membership.member = provisioning_owner.oid
                   AND membership.admin_option
                   AND NOT membership.inherit_option
                   AND NOT membership.set_option
             )
             OR EXISTS (
                 SELECT 1
                 FROM pg_catalog.pg_shdepend dependency
                 WHERE dependency.refclassid = 'pg_authid'::regclass
                   AND dependency.refobjid = runtime.oid
                   AND dependency.deptype = 'o'
             )
         FROM runtime, provisioning_owner, database_row",
    )
    .bind(RUNTIME_ROLE)
    .fetch_one(&mut *transaction)
    .await?;
    if role_drift {
        return Err(Error::RuntimeRoleDrift);
    }

    let normalize_role: String = sqlx::query_scalar(
        "SELECT format(
             'ALTER ROLE %I RESET ALL; ALTER ROLE %I IN DATABASE %I RESET ALL; ALTER ROLE %I WITH NOLOGIN PASSWORD NULL NOINHERIT CONNECTION LIMIT -1 VALID UNTIL %L',
             $1, $1, current_database(), $1, 'infinity'
         )",
    )
    .bind(RUNTIME_ROLE)
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::raw_sql(&normalize_role)
        .execute(&mut *transaction)
        .await?;

    normalize_neon_provider_database_acl(&mut transaction).await?;

    let database_acl: String = sqlx::query_scalar(
        "SELECT format(
             'REVOKE CONNECT, TEMPORARY ON DATABASE %I FROM PUBLIC; REVOKE ALL ON DATABASE %I FROM dark_factory_broker_runtime; GRANT CONNECT ON DATABASE %I TO dark_factory_broker_runtime',
             current_database(), current_database(), current_database()
         )",
    )
    .fetch_one(&mut *transaction)
    .await?;
    sqlx::raw_sql(&database_acl)
        .execute(&mut *transaction)
        .await?;
    let unexpected_public_functions: bool = sqlx::query_scalar(
        "SELECT EXISTS (
             SELECT 1
             FROM pg_catalog.pg_proc procedure
             JOIN pg_catalog.pg_namespace namespace ON namespace.oid = procedure.pronamespace
             WHERE namespace.nspname = 'public'
         )",
    )
    .fetch_one(&mut *transaction)
    .await?;
    if unexpected_public_functions {
        return Err(Error::RuntimeRoleDrift);
    }

    sqlx::raw_sql(
        "REVOKE ALL ON SCHEMA public FROM PUBLIC;
         REVOKE ALL ON ALL TABLES IN SCHEMA public FROM PUBLIC;
         REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM PUBLIC;
         REVOKE ALL ON SCHEMA public FROM dark_factory_broker_runtime;
         REVOKE ALL ON ALL TABLES IN SCHEMA public FROM dark_factory_broker_runtime;
         REVOKE ALL ON ALL SEQUENCES IN SCHEMA public FROM dark_factory_broker_runtime;
         GRANT USAGE ON SCHEMA public TO dark_factory_broker_runtime;
         GRANT SELECT, INSERT ON TABLE public.maintainer_deliveries TO dark_factory_broker_runtime;
         GRANT SELECT ON TABLE public.dark_factory_schema_migrations TO dark_factory_broker_runtime;
         ALTER DEFAULT PRIVILEGES REVOKE ALL ON TABLES FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES REVOKE ALL ON SEQUENCES FROM PUBLIC;
         ALTER DEFAULT PRIVILEGES REVOKE ALL ON TABLES FROM dark_factory_broker_runtime;
         ALTER DEFAULT PRIVILEGES REVOKE ALL ON SEQUENCES FROM dark_factory_broker_runtime",
    )
    .execute(&mut *transaction)
    .await?;
    transaction.commit().await?;
    Ok(NeonIdentity {
        project_id,
        branch_id,
    })
}

#[cfg(feature = "provision-runtime")]
pub(crate) async fn activate_runtime(pool: &PgPool, identity: &NeonIdentity) -> Result<(), Error> {
    let mut transaction = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(441684976339)")
        .execute(&mut *transaction)
        .await?;
    let (project_id, branch_id): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT current_setting('neon.project_id', true),
                current_setting('neon.branch_id', true)",
    )
    .fetch_one(&mut *transaction)
    .await?;
    if project_id.as_deref() != Some(&identity.project_id)
        || branch_id.as_deref() != Some(&identity.branch_id)
    {
        return Err(Error::RuntimeRoleDrift);
    }
    normalize_neon_provider_database_acl(&mut transaction).await?;
    if !runtime_role_is_exact_for_activation(&mut transaction).await? {
        return Err(Error::RuntimeRoleDrift);
    }
    postgres_contract_is_exact(&mut transaction, false, false).await?;
    if !default_privileges_are_exact(&mut transaction).await? {
        return Err(Error::RuntimeRoleDrift);
    }
    sqlx::query("ALTER ROLE dark_factory_broker_runtime LOGIN")
        .execute(&mut *transaction)
        .await?;
    transaction.commit().await?;
    Ok(())
}

#[cfg(feature = "provision-runtime")]
async fn normalize_neon_provider_database_acl(connection: &mut PgConnection) -> Result<(), Error> {
    let provider = sqlx::query(
        "SELECT count(*) AS row_count,
            COALESCE(bool_and(
                NOT provider.rolcanlogin
                AND provider.rolinherit
                AND NOT provider.rolsuper
                AND provider.rolcreatedb
                AND provider.rolcreaterole
                AND provider.rolreplication
                AND provider.rolbypassrls
                AND provider.rolconnlimit = -1
                AND provider.rolvaliduntil IS NULL
                AND provider.oid <> owner.oid
            ), false) AS role_shape_is_exact,
            COALESCE(bool_and(
                NOT membership.admin_option
                AND membership.inherit_option
                AND membership.set_option
                AND grantor.rolname = 'cloud_admin'
            ), false) AS owner_membership_is_exact
         FROM pg_catalog.pg_roles provider
         JOIN pg_catalog.pg_roles owner ON owner.rolname = current_user
         JOIN pg_catalog.pg_auth_members membership
           ON membership.roleid = provider.oid
          AND membership.member = owner.oid
         JOIN pg_catalog.pg_roles grantor ON grantor.oid = membership.grantor
         WHERE provider.rolname = 'neon_superuser'",
    )
    .fetch_one(&mut *connection)
    .await?;
    if !neon_provider_identity_is_expected(
        provider.try_get("row_count")?,
        provider.try_get("role_shape_is_exact")?,
        provider.try_get("owner_membership_is_exact")?,
    ) {
        return Err(Error::InvalidNeonIdentity);
    }
    let revoke: String = sqlx::query_scalar(
        "SELECT format(
             'REVOKE ALL ON DATABASE %I FROM neon_superuser',
             current_database()
         )",
    )
    .fetch_one(&mut *connection)
    .await?;
    sqlx::raw_sql(&revoke).execute(&mut *connection).await?;
    Ok(())
}

#[cfg(feature = "provision-runtime")]
const fn neon_provider_identity_is_expected(
    row_count: i64,
    role_shape_is_exact: bool,
    owner_membership_is_exact: bool,
) -> bool {
    row_count == 1 && role_shape_is_exact && owner_membership_is_exact
}

#[cfg(feature = "provision-runtime")]
async fn runtime_role_is_exact_for_activation(
    transaction: &mut sqlx::Transaction<'_, sqlx::Postgres>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH runtime AS (
             SELECT oid, rolcanlogin, rolinherit, rolsuper, rolcreatedb,
                    rolcreaterole, rolreplication, rolbypassrls, rolconnlimit,
                    rolvaliduntil, rolconfig,
                    pg_catalog.shobj_description(oid, 'pg_authid') AS comment
             FROM pg_catalog.pg_roles
             WHERE rolname = $1
         ),
         provisioning_owner AS (
             SELECT oid FROM pg_catalog.pg_roles WHERE rolname = current_user
         ),
         database_row AS (
             SELECT datdba FROM pg_catalog.pg_database WHERE datname = current_database()
         )
         SELECT NOT runtime.rolcanlogin
            AND NOT runtime.rolinherit
            AND NOT runtime.rolsuper
            AND NOT runtime.rolcreatedb
            AND NOT runtime.rolcreaterole
            AND NOT runtime.rolreplication
            AND NOT runtime.rolbypassrls
            AND runtime.rolconnlimit = -1
            AND COALESCE(runtime.rolvaliduntil = 'infinity'::timestamptz, false)
            AND runtime.rolconfig IS NULL
            AND runtime.comment = $2
            AND provisioning_owner.oid = database_row.datdba
            AND (
                SELECT count(*)
                FROM pg_catalog.pg_auth_members membership
                WHERE membership.member = runtime.oid
                   OR membership.roleid = runtime.oid
            ) = 1
            AND EXISTS (
                SELECT 1
                FROM pg_catalog.pg_auth_members membership
                WHERE membership.roleid = runtime.oid
                  AND membership.member = provisioning_owner.oid
                  AND membership.admin_option
                  AND NOT membership.inherit_option
                  AND NOT membership.set_option
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_db_role_setting setting
                WHERE setting.setrole = runtime.oid
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_shdepend dependency
                WHERE dependency.refclassid = 'pg_authid'::regclass
                  AND dependency.refobjid = runtime.oid
                  AND dependency.deptype = 'o'
            )
         FROM runtime, provisioning_owner, database_row",
    )
    .bind(RUNTIME_ROLE)
    .bind(RUNTIME_ROLE_COMMENT)
    .fetch_one(&mut **transaction)
    .await
}

#[cfg(feature = "provision-runtime")]
async fn default_privileges_are_exact(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH runtime AS (
             SELECT oid FROM pg_catalog.pg_roles WHERE rolname = $1
         ),
         provisioning_owner AS (
             SELECT oid FROM pg_catalog.pg_roles WHERE rolname = current_user
         )
         SELECT NOT EXISTS (
             SELECT 1
             FROM pg_catalog.pg_default_acl default_acl
             CROSS JOIN LATERAL pg_catalog.aclexplode(default_acl.defaclacl) acl
             CROSS JOIN runtime
             CROSS JOIN provisioning_owner
             WHERE default_acl.defaclrole = provisioning_owner.oid
               AND default_acl.defaclobjtype IN ('r', 'S')
               AND (acl.grantee = 0 OR acl.grantee = runtime.oid)
         )",
    )
    .bind(RUNTIME_ROLE)
    .fetch_one(&mut *connection)
    .await
}

#[cfg(feature = "provision-runtime")]
fn neon_identifier_is_valid(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 60
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn migration_digest() -> String {
    hex::encode(Sha256::digest(MIGRATION_SQL.as_bytes()))
}

async fn postgres_ready(pool: &PgPool) -> Result<(), Error> {
    let mut connection = pool.acquire().await?;
    postgres_contract_is_exact(&mut connection, true, true).await
}

async fn postgres_contract_is_exact(
    connection: &mut PgConnection,
    expected_login: bool,
    require_runtime_session: bool,
) -> Result<(), Error> {
    if !runtime_privileges_are_exact(connection, expected_login, require_runtime_session).await?
        || !catalog_acls_are_exact(connection).await?
    {
        return Err(Error::MissingPrivileges);
    }
    if !columns_are_exact(
        connection,
        "public.maintainer_deliveries",
        &[
            ("delivery_id", "text", true, ""),
            ("hook_id", "bigint", true, ""),
            ("target_id", "bigint", true, ""),
            ("target_type", "text", true, ""),
            ("event", "text", true, ""),
            ("action", "text", false, ""),
            ("body_digest", "text", true, ""),
            ("disposition", "text", true, ""),
            ("secret_revision", "text", true, ""),
            ("received_at", "timestamp with time zone", true, "now()"),
        ],
    )
    .await?
        || !columns_are_exact(
            connection,
            "public.dark_factory_schema_migrations",
            &[
                ("component", "text", true, ""),
                ("revision", "text", true, ""),
                ("digest", "text", true, ""),
                ("applied_at", "timestamp with time zone", true, "now()"),
            ],
        )
        .await?
        || !not_null_constraints_are_exact(
            connection,
            "public.maintainer_deliveries",
            &[1, 2, 3, 4, 5, 7, 8, 9, 10],
        )
        .await?
        || !not_null_constraints_are_exact(
            connection,
            "public.dark_factory_schema_migrations",
            &[1, 2, 3, 4],
        )
        .await?
        || !delivery_constraints_are_exact(connection).await?
        || !delivery_primary_key_is_exact(connection).await?
        || !migration_constraints_are_exact(connection).await?
        || !migration_primary_key_is_exact(connection).await?
    {
        return Err(Error::InvalidSchema);
    }
    let migration = sqlx::query(
        "SELECT revision, digest
         FROM public.dark_factory_schema_migrations
         WHERE component = $1",
    )
    .bind(MIGRATION_COMPONENT)
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(Error::MigrationConflict)?;
    let revision: String = migration.try_get("revision")?;
    let digest: String = migration.try_get("digest")?;
    if revision != MIGRATION_REVISION || digest != migration_digest() {
        return Err(Error::MigrationConflict);
    }
    Ok(())
}

async fn runtime_privileges_are_exact(
    connection: &mut PgConnection,
    expected_login: bool,
    require_runtime_session: bool,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH me AS (
             SELECT oid, rolcanlogin, rolinherit, rolsuper, rolcreatedb,
                    rolcreaterole, rolreplication, rolbypassrls, rolconnlimit,
                    rolvaliduntil, rolconfig
             FROM pg_catalog.pg_roles
             WHERE rolname = $1
         ),
         target AS (
             SELECT deliveries.oid AS deliveries_oid,
                    deliveries.relowner AS deliveries_owner,
                    deliveries.relkind AS deliveries_kind,
                    deliveries.relpersistence AS deliveries_persistence,
                    deliveries.relrowsecurity AS deliveries_rls,
                    deliveries.relforcerowsecurity AS deliveries_force_rls,
                    migrations.oid AS migrations_oid,
                    migrations.relowner AS migrations_owner,
                    migrations.relkind AS migrations_kind,
                    migrations.relpersistence AS migrations_persistence,
                    migrations.relrowsecurity AS migrations_rls,
                    migrations.relforcerowsecurity AS migrations_force_rls,
                    namespace.oid AS namespace_oid,
                    namespace.nspowner AS namespace_owner
             FROM pg_catalog.pg_class deliveries
             JOIN pg_catalog.pg_namespace namespace ON namespace.oid = deliveries.relnamespace
             JOIN pg_catalog.pg_class migrations
               ON migrations.relnamespace = namespace.oid
              AND migrations.relname = 'dark_factory_schema_migrations'
             WHERE namespace.nspname = 'public'
               AND deliveries.relname = 'maintainer_deliveries'
         )
         SELECT current_setting('server_version_num')::integer >= 170000
            AND current_setting('transaction_read_only') = 'off'
            AND (NOT $3 OR (current_user = $1 AND session_user = current_user))
            AND me.rolcanlogin = $2
            AND NOT me.rolinherit
            AND NOT me.rolsuper
            AND NOT me.rolcreatedb
            AND NOT me.rolcreaterole
            AND NOT me.rolreplication
            AND NOT me.rolbypassrls
            AND me.rolconnlimit = -1
            AND COALESCE(me.rolvaliduntil = 'infinity'::timestamptz, false)
            AND me.rolconfig IS NULL
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_db_role_setting setting
                WHERE setting.setrole = me.oid
            )
            AND (
                SELECT count(*)
                FROM pg_catalog.pg_auth_members membership
                WHERE membership.member = me.oid OR membership.roleid = me.oid
            ) = 1
            AND EXISTS (
                SELECT 1
                FROM pg_catalog.pg_auth_members membership
                JOIN pg_catalog.pg_database database_row
                  ON database_row.datname = current_database()
                WHERE membership.roleid = me.oid
                  AND membership.member = database_row.datdba
                  AND membership.admin_option
                  AND NOT membership.inherit_option
                  AND NOT membership.set_option
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_shdepend dependency
                WHERE dependency.refclassid = 'pg_authid'::regclass
                  AND dependency.refobjid = me.oid
                  AND dependency.deptype = 'o'
            )
            AND (SELECT datdba <> me.oid FROM pg_catalog.pg_database WHERE datname = current_database())
            AND has_database_privilege($1, current_database(), 'CONNECT')
            AND NOT has_database_privilege($1, current_database(), 'CONNECT WITH GRANT OPTION')
            AND NOT has_database_privilege($1, current_database(), 'CREATE')
            AND NOT has_database_privilege($1, current_database(), 'TEMPORARY')
            AND target.namespace_owner <> me.oid
            AND has_schema_privilege($1, target.namespace_oid, 'USAGE')
            AND NOT has_schema_privilege($1, target.namespace_oid, 'USAGE WITH GRANT OPTION')
            AND NOT has_schema_privilege($1, target.namespace_oid, 'CREATE')
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_proc procedure
                WHERE procedure.pronamespace = target.namespace_oid
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_namespace namespace
                WHERE namespace.nspname <> 'public'
                  AND namespace.nspname <> 'information_schema'
                  AND namespace.nspname !~ '^pg_'
                  AND (
                      has_schema_privilege($1, namespace.oid, 'USAGE')
                      OR has_schema_privilege($1, namespace.oid, 'CREATE')
                  )
            )
            AND target.deliveries_owner <> me.oid
            AND target.deliveries_kind = 'r'
            AND target.deliveries_persistence = 'p'
            AND NOT target.deliveries_rls
            AND NOT target.deliveries_force_rls
            AND has_table_privilege($1, target.deliveries_oid, 'SELECT')
            AND has_table_privilege($1, target.deliveries_oid, 'INSERT')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'UPDATE')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'DELETE')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'TRUNCATE')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'REFERENCES')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'TRIGGER')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'MAINTAIN')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'SELECT WITH GRANT OPTION')
            AND NOT has_table_privilege($1, target.deliveries_oid, 'INSERT WITH GRANT OPTION')
            AND NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_attribute
                WHERE attrelid = target.deliveries_oid AND attnum > 0 AND attacl IS NOT NULL
            )
            AND target.migrations_owner <> me.oid
            AND target.migrations_kind = 'r'
            AND target.migrations_persistence = 'p'
            AND NOT target.migrations_rls
            AND NOT target.migrations_force_rls
            AND has_table_privilege($1, target.migrations_oid, 'SELECT')
            AND NOT has_table_privilege($1, target.migrations_oid, 'INSERT')
            AND NOT has_table_privilege($1, target.migrations_oid, 'UPDATE')
            AND NOT has_table_privilege($1, target.migrations_oid, 'DELETE')
            AND NOT has_table_privilege($1, target.migrations_oid, 'TRUNCATE')
            AND NOT has_table_privilege($1, target.migrations_oid, 'REFERENCES')
            AND NOT has_table_privilege($1, target.migrations_oid, 'TRIGGER')
            AND NOT has_table_privilege($1, target.migrations_oid, 'MAINTAIN')
            AND NOT has_table_privilege($1, target.migrations_oid, 'SELECT WITH GRANT OPTION')
            AND NOT EXISTS (
                SELECT 1 FROM pg_catalog.pg_attribute
                WHERE attrelid = target.migrations_oid AND attnum > 0 AND attacl IS NOT NULL
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_inherits inheritance
                WHERE inheritance.inhrelid IN (target.deliveries_oid, target.migrations_oid)
                   OR inheritance.inhparent IN (target.deliveries_oid, target.migrations_oid)
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_trigger trigger_row
                WHERE trigger_row.tgrelid IN (target.deliveries_oid, target.migrations_oid)
                  AND NOT trigger_row.tgisinternal
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_rewrite rule_row
                WHERE rule_row.ev_class IN (target.deliveries_oid, target.migrations_oid)
            )
            AND NOT EXISTS (
                SELECT 1
                FROM pg_catalog.pg_class relation
                JOIN pg_catalog.pg_namespace namespace ON namespace.oid = relation.relnamespace
                WHERE namespace.nspname <> 'information_schema'
                  AND namespace.nspname !~ '^pg_'
                  AND relation.oid NOT IN (target.deliveries_oid, target.migrations_oid)
                  AND (
                      (relation.relkind = 'S' AND (
                          has_sequence_privilege($1, relation.oid, 'USAGE')
                          OR has_sequence_privilege($1, relation.oid, 'SELECT')
                          OR has_sequence_privilege($1, relation.oid, 'UPDATE')
                      ))
                      OR (relation.relkind IN ('r', 'p', 'v', 'm', 'f') AND (
                          has_table_privilege($1, relation.oid, 'SELECT')
                          OR has_table_privilege($1, relation.oid, 'INSERT')
                          OR has_table_privilege($1, relation.oid, 'UPDATE')
                          OR has_table_privilege($1, relation.oid, 'DELETE')
                          OR has_table_privilege($1, relation.oid, 'TRUNCATE')
                          OR has_table_privilege($1, relation.oid, 'REFERENCES')
                          OR has_table_privilege($1, relation.oid, 'TRIGGER')
                          OR has_table_privilege($1, relation.oid, 'MAINTAIN')
                          OR has_any_column_privilege(
                              $1,
                              relation.oid,
                              'SELECT,INSERT,UPDATE,REFERENCES'
                          )
                          OR EXISTS (
                              SELECT 1
                              FROM pg_catalog.pg_attribute attribute
                              WHERE attribute.attrelid = relation.oid
                                AND attribute.attnum > 0
                                AND attribute.attacl IS NOT NULL
                          )
                      ))
                  )
            )
         FROM me, target",
    )
    .bind(RUNTIME_ROLE)
    .bind(expected_login)
    .bind(require_runtime_session)
    .fetch_one(&mut *connection)
    .await
}

async fn catalog_acls_are_exact(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH me AS (
             SELECT oid
             FROM pg_catalog.pg_roles
             WHERE rolname = $1
         ),
         database_row AS (
             SELECT datdba, datacl
             FROM pg_catalog.pg_database
             WHERE datname = current_database()
         ),
         namespace_row AS (
             SELECT oid, nspowner, nspacl
             FROM pg_catalog.pg_namespace
             WHERE nspname = 'public'
         ),
         deliveries AS (
             SELECT oid, relowner, relacl
             FROM pg_catalog.pg_class
             WHERE oid = 'public.maintainer_deliveries'::regclass
         ),
         migrations AS (
             SELECT oid, relowner, relacl
             FROM pg_catalog.pg_class
             WHERE oid = 'public.dark_factory_schema_migrations'::regclass
         ),
         acl_rows AS (
             SELECT 'database' AS object_type, database_row.datdba AS owner_oid,
                    acl.grantee, acl.privilege_type, acl.is_grantable
             FROM database_row
             CROSS JOIN LATERAL pg_catalog.aclexplode(
                 COALESCE(database_row.datacl, pg_catalog.acldefault('d', database_row.datdba))
             ) acl
             UNION ALL
             SELECT 'schema', namespace_row.nspowner,
                    acl.grantee, acl.privilege_type, acl.is_grantable
             FROM namespace_row
             CROSS JOIN LATERAL pg_catalog.aclexplode(
                 COALESCE(namespace_row.nspacl, pg_catalog.acldefault('n', namespace_row.nspowner))
             ) acl
             UNION ALL
             SELECT 'deliveries', deliveries.relowner,
                    acl.grantee, acl.privilege_type, acl.is_grantable
             FROM deliveries
             CROSS JOIN LATERAL pg_catalog.aclexplode(
                 COALESCE(deliveries.relacl, pg_catalog.acldefault('r', deliveries.relowner))
             ) acl
             UNION ALL
             SELECT 'migrations', migrations.relowner,
                    acl.grantee, acl.privilege_type, acl.is_grantable
             FROM migrations
             CROSS JOIN LATERAL pg_catalog.aclexplode(
                 COALESCE(migrations.relacl, pg_catalog.acldefault('r', migrations.relowner))
             ) acl
         )
         SELECT NOT EXISTS (
                    SELECT 1
                    FROM acl_rows, me
                    WHERE NOT (
                        (acl_rows.grantee = acl_rows.owner_oid
                         AND NOT acl_rows.is_grantable
                         AND (
                             (acl_rows.object_type = 'database'
                              AND acl_rows.privilege_type IN ('CONNECT', 'CREATE', 'TEMPORARY'))
                             OR (acl_rows.object_type = 'schema'
                                 AND acl_rows.privilege_type IN ('USAGE', 'CREATE'))
                             OR (acl_rows.object_type IN ('deliveries', 'migrations')
                                 AND acl_rows.privilege_type IN (
                                     'SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE',
                                     'REFERENCES', 'TRIGGER', 'MAINTAIN'
                                 ))
                         ))
                        OR (acl_rows.grantee = me.oid
                            AND NOT acl_rows.is_grantable
                            AND (
                                (acl_rows.object_type = 'database'
                                 AND acl_rows.privilege_type = 'CONNECT')
                                OR (acl_rows.object_type = 'schema'
                                    AND acl_rows.privilege_type = 'USAGE')
                                OR (acl_rows.object_type = 'deliveries'
                                    AND acl_rows.privilege_type IN ('SELECT', 'INSERT'))
                                OR (acl_rows.object_type = 'migrations'
                                    AND acl_rows.privilege_type = 'SELECT')
                            ))
                    )
                )
            AND (SELECT count(*) FROM acl_rows WHERE object_type = 'database') = 4
            AND (SELECT count(*) FROM acl_rows WHERE object_type = 'schema') = 3
            AND (SELECT count(*) FROM acl_rows WHERE object_type = 'deliveries') = 10
            AND (SELECT count(*) FROM acl_rows WHERE object_type = 'migrations') = 9",
    )
    .bind(RUNTIME_ROLE)
    .fetch_one(&mut *connection)
    .await
}

async fn columns_are_exact(
    connection: &mut PgConnection,
    relation: &str,
    expected: &[(&str, &str, bool, &str)],
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT attribute.attname,
                pg_catalog.format_type(attribute.atttypid, attribute.atttypmod) AS type,
                attribute.attnotnull,
                COALESCE(pg_catalog.pg_get_expr(default_value.adbin, default_value.adrelid), '') AS default_value
         FROM pg_catalog.pg_attribute attribute
         LEFT JOIN pg_catalog.pg_attrdef default_value
           ON default_value.adrelid = attribute.attrelid
          AND default_value.adnum = attribute.attnum
         WHERE attribute.attrelid = to_regclass($1)
           AND attribute.attnum > 0
           AND NOT attribute.attisdropped
         ORDER BY attribute.attnum",
    )
    .bind(relation)
    .fetch_all(&mut *connection)
    .await?;
    if rows.len() != expected.len() {
        return Ok(false);
    }
    for (row, (name, data_type, not_null, default_value)) in rows.iter().zip(expected) {
        if row.try_get::<String, _>("attname")? != *name
            || row.try_get::<String, _>("type")? != *data_type
            || row.try_get::<bool, _>("attnotnull")? != *not_null
            || row.try_get::<String, _>("default_value")? != *default_value
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn not_null_constraints_are_exact(
    connection: &mut PgConnection,
    relation: &str,
    expected_attnums: &[i16],
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "WITH not_null_constraints AS (
             SELECT convalidated, condeferrable, condeferred, conislocal,
                    coninhcount, connoinherit, contypid, conindid,
                    conparentid, conkey
             FROM pg_catalog.pg_constraint
             WHERE conrelid = to_regclass($1)
               AND contype = 'n'
         )
         SELECT CASE
             WHEN current_setting('server_version_num')::integer >= 180000 THEN
                 (SELECT count(*) = cardinality($2::smallint[])
                      AND COALESCE(bool_and(
                              convalidated
                              AND NOT condeferrable
                              AND NOT condeferred
                              AND conislocal
                              AND coninhcount = 0
                              AND NOT connoinherit
                              AND contypid = 0
                              AND conindid = 0
                              AND conparentid = 0
                              AND cardinality(conkey) = 1
                          ), false)
                      AND COALESCE(
                              array_agg(conkey[1] ORDER BY conkey[1]),
                              ARRAY[]::smallint[]
                          ) = $2::smallint[]
                  FROM not_null_constraints)
             ELSE NOT EXISTS (SELECT 1 FROM not_null_constraints)
         END",
    )
    .bind(relation)
    .bind(expected_attnums.to_vec())
    .fetch_one(&mut *connection)
    .await
}

async fn delivery_constraints_are_exact(
    connection: &mut PgConnection,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT conname, contype::text AS type, convalidated,
                pg_catalog.pg_get_constraintdef(oid, false) AS definition
         FROM pg_catalog.pg_constraint
         WHERE conrelid = 'public.maintainer_deliveries'::regclass
           AND contype <> 'n'
         ORDER BY conname",
    )
    .fetch_all(&mut *connection)
    .await?;
    let expected = [
        (
            "maintainer_deliveries_action_format",
            "c",
            "CHECK (((action IS NULL) OR (((octet_length(action) >= 1) AND (octet_length(action) <= 64)) AND (action ~ '^[a-z_]+$'::text))))",
        ),
        (
            "maintainer_deliveries_body_digest_format",
            "c",
            "CHECK ((body_digest ~ '^[0-9a-f]{64}$'::text))",
        ),
        (
            "maintainer_deliveries_delivery_id_format",
            "c",
            "CHECK ((delivery_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'::text))",
        ),
        (
            "maintainer_deliveries_disposition_values",
            "c",
            "CHECK ((disposition = ANY (ARRAY['ping'::text, 'policy_rejected'::text, 'payload_rejected'::text])))",
        ),
        (
            "maintainer_deliveries_event_format",
            "c",
            "CHECK ((event ~ '^[a-z_]+$'::text))",
        ),
        (
            "maintainer_deliveries_event_length",
            "c",
            "CHECK (((octet_length(event) >= 1) AND (octet_length(event) <= 64)))",
        ),
        (
            "maintainer_deliveries_hook_id_positive",
            "c",
            "CHECK ((hook_id > 0))",
        ),
        (
            "maintainer_deliveries_pkey",
            "p",
            "PRIMARY KEY (delivery_id)",
        ),
        (
            "maintainer_deliveries_secret_revision_format",
            "c",
            "CHECK ((secret_revision ~ '^[a-z0-9_-]+$'::text))",
        ),
        (
            "maintainer_deliveries_secret_revision_length",
            "c",
            "CHECK (((octet_length(secret_revision) >= 1) AND (octet_length(secret_revision) <= 64)))",
        ),
        (
            "maintainer_deliveries_target_id_positive",
            "c",
            "CHECK ((target_id > 0))",
        ),
        (
            "maintainer_deliveries_target_type_format",
            "c",
            "CHECK ((target_type ~ '^[a-zA-Z0-9_-]+$'::text))",
        ),
        (
            "maintainer_deliveries_target_type_length",
            "c",
            "CHECK (((octet_length(target_type) >= 1) AND (octet_length(target_type) <= 64)))",
        ),
    ];
    if rows.len() != expected.len() {
        return Ok(false);
    }
    for (row, (expected_name, expected_type, expected_definition)) in rows.iter().zip(expected) {
        let name: String = row.try_get("conname")?;
        let constraint_type: String = row.try_get("type")?;
        let validated: bool = row.try_get("convalidated")?;
        let definition: String = row.try_get("definition")?;
        if name != expected_name
            || constraint_type != expected_type
            || !validated
            || definition != expected_definition
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn delivery_primary_key_is_exact(connection: &mut PgConnection) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) = 1
         FROM pg_catalog.pg_constraint constraint_row
         JOIN pg_catalog.pg_index index_row ON index_row.indexrelid = constraint_row.conindid
         JOIN pg_catalog.pg_attribute attribute
           ON attribute.attrelid = constraint_row.conrelid
          AND attribute.attname = 'delivery_id'
         WHERE constraint_row.conrelid = 'public.maintainer_deliveries'::regclass
           AND constraint_row.conname = 'maintainer_deliveries_pkey'
           AND constraint_row.contype = 'p'
           AND constraint_row.convalidated
           AND index_row.indisunique
           AND index_row.indisvalid
           AND index_row.indisready
           AND index_row.indpred IS NULL
           AND index_row.indexprs IS NULL
           AND constraint_row.conkey = ARRAY[attribute.attnum]::smallint[]",
    )
    .fetch_one(&mut *connection)
    .await
}

async fn migration_constraints_are_exact(
    connection: &mut PgConnection,
) -> Result<bool, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT conname, contype::text AS type, convalidated,
                pg_catalog.pg_get_constraintdef(oid, false) AS definition
         FROM pg_catalog.pg_constraint
         WHERE conrelid = 'public.dark_factory_schema_migrations'::regclass
           AND contype <> 'n'
         ORDER BY conname",
    )
    .fetch_all(&mut *connection)
    .await?;
    let expected = [
        (
            "dark_factory_schema_migrations_digest_format",
            "c",
            "CHECK ((digest ~ '^[0-9a-f]{64}$'::text))",
        ),
        (
            "dark_factory_schema_migrations_pkey",
            "p",
            "PRIMARY KEY (component)",
        ),
    ];
    if rows.len() != expected.len() {
        return Ok(false);
    }
    for (row, (expected_name, expected_type, expected_definition)) in rows.iter().zip(expected) {
        let name: String = row.try_get("conname")?;
        let constraint_type: String = row.try_get("type")?;
        let validated: bool = row.try_get("convalidated")?;
        let definition: String = row.try_get("definition")?;
        if name != expected_name
            || constraint_type != expected_type
            || !validated
            || definition != expected_definition
        {
            return Ok(false);
        }
    }
    Ok(true)
}

async fn migration_primary_key_is_exact(
    connection: &mut PgConnection,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT count(*) = 1
         FROM pg_catalog.pg_constraint constraint_row
         JOIN pg_catalog.pg_index index_row ON index_row.indexrelid = constraint_row.conindid
         JOIN pg_catalog.pg_attribute attribute
           ON attribute.attrelid = constraint_row.conrelid
          AND attribute.attname = 'component'
         WHERE constraint_row.conrelid = 'public.dark_factory_schema_migrations'::regclass
           AND constraint_row.conname = 'dark_factory_schema_migrations_pkey'
           AND constraint_row.contype = 'p'
           AND constraint_row.convalidated
           AND index_row.indisunique
           AND index_row.indisvalid
           AND index_row.indisready
           AND index_row.indpred IS NULL
           AND index_row.indexprs IS NULL
           AND constraint_row.conkey = ARRAY[attribute.attnum]::smallint[]",
    )
    .fetch_one(&mut *connection)
    .await
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

#[cfg(all(test, feature = "provision-runtime"))]
mod tests {
    use sqlx::{Executor as _, Row as _};

    use super::{
        activate_runtime, neon_owner_pool, neon_provider_identity_is_expected, prepare_runtime,
    };

    #[test]
    fn neon_provider_identity_requires_one_exact_role_and_owner_membership() {
        assert!(neon_provider_identity_is_expected(1, true, true));
        assert!(!neon_provider_identity_is_expected(0, true, true));
        assert!(!neon_provider_identity_is_expected(2, true, true));
        assert!(!neon_provider_identity_is_expected(1, false, true));
        assert!(!neon_provider_identity_is_expected(1, true, false));
    }

    #[tokio::test]
    #[ignore = "requires an explicit disposable Neon owner database URL"]
    async fn activation_removes_a_provider_acl_restored_after_preparation() {
        let database_url = std::env::var("DARK_FACTORY_TEST_NEON_OWNER_DATABASE_URL")
            .expect("disposable Neon owner database URL");
        let pool = neon_owner_pool(&database_url).unwrap();
        let expected_project_id: String =
            sqlx::query_scalar("SELECT current_setting('neon.project_id')")
                .fetch_one(&pool)
                .await
                .unwrap();
        let identity = prepare_runtime(&pool, &expected_project_id).await.unwrap();
        let restore_provider_acl: String = sqlx::query_scalar(
            "SELECT format(
                 'GRANT ALL ON DATABASE %I TO neon_superuser',
                 current_database()
             )",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        pool.execute(restore_provider_acl.as_str()).await.unwrap();

        let result = async {
            activate_runtime(&pool, &identity).await?;
            let row = sqlx::query(
                "SELECT role.rolcanlogin,
                        EXISTS (
                            SELECT 1
                            FROM pg_catalog.pg_database database_row
                            CROSS JOIN LATERAL pg_catalog.aclexplode(database_row.datacl) acl
                            JOIN pg_catalog.pg_roles grantee ON grantee.oid = acl.grantee
                            WHERE database_row.datname = current_database()
                              AND grantee.rolname = 'neon_superuser'
                        ) AS provider_acl_exists
                 FROM pg_catalog.pg_roles role
                 WHERE role.rolname = 'dark_factory_broker_runtime'",
            )
            .fetch_one(&pool)
            .await?;
            Ok::<_, super::Error>((
                row.try_get::<bool, _>("rolcanlogin")?,
                row.try_get::<bool, _>("provider_acl_exists")?,
            ))
        }
        .await;
        let cleanup = prepare_runtime(&pool, &expected_project_id).await;
        let (runtime_can_login, provider_acl_exists) = result.unwrap();
        cleanup.unwrap();
        assert!(runtime_can_login);
        assert!(!provider_acl_exists);
    }
}
