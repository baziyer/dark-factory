//! Hosted GitHub authority boundary. This standalone service never links to
//! `factoryd`; its bootstrap surface only authenticates and journals ping
//! deliveries.

#[cfg(feature = "development-sqlite")]
use std::path::Path;

use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::StatusCode,
    response::{IntoResponse as _, Response},
    routing::get,
};

mod journal;
pub mod maintainer;

use journal::DeliveryJournal;
use maintainer::{MAX_BODY_BYTES, MaintainerState, SecretRevision, WebhookSecret};

pub const DATABASE_URL_ENV: &str = "DATABASE_URL";
pub const WEBHOOK_SECRET_ENV: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET";
pub const SECRET_REVISION_ENV: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION";
pub const APP_ID_ENV: &str = "DARK_FACTORY_MAINTAINER_APP_ID";
const AMBIENT_POSTGRES_ENV: [&str; 12] = [
    "PGAPPNAME",
    "PGDATABASE",
    "PGHOST",
    "PGHOSTADDR",
    "PGOPTIONS",
    "PGPASSFILE",
    "PGPASSWORD",
    "PGPORT",
    "PGSSLCERT",
    "PGSSLKEY",
    "PGSSLMODE",
    "PGUSER",
];

#[derive(Clone, Copy)]
enum Deployment {
    Inactive,
    #[cfg(feature = "development-sqlite")]
    Development,
    Production,
}

#[derive(Clone)]
pub struct BrokerState {
    pub(crate) maintainer: Option<MaintainerState>,
    deployment: Deployment,
}

impl BrokerState {
    #[must_use]
    pub const fn inactive() -> Self {
        Self {
            maintainer: None,
            deployment: Deployment::Inactive,
        }
    }

    pub fn open_production(
        database_url: &str,
        webhook_secret: Vec<u8>,
        secret_revision: String,
        expected_app_id: i64,
    ) -> Result<Self, ProductionOpenError> {
        if expected_app_id <= 0 {
            return Err(ProductionOpenError::InvalidAppId);
        }
        let secret = WebhookSecret::new(webhook_secret)
            .map_err(|_| ProductionOpenError::InvalidWebhookSecret)?;
        let revision = SecretRevision::new(secret_revision)
            .map_err(|_| ProductionOpenError::InvalidSecretRevision)?;
        let journal = DeliveryJournal::postgres(database_url)
            .map_err(|_| ProductionOpenError::InvalidDatabaseUrl)?;
        Ok(Self {
            maintainer: Some(MaintainerState::new(
                secret,
                revision,
                expected_app_id,
                journal,
            )),
            deployment: Deployment::Production,
        })
    }

    #[cfg(feature = "development-sqlite")]
    pub fn open_development(
        database: &Path,
        webhook_secret: WebhookSecret,
        secret_revision: SecretRevision,
        expected_target_id: i64,
    ) -> Result<Self, OpenError> {
        if expected_target_id <= 0 {
            return Err(OpenError::InvalidExpectedTargetId);
        }
        Ok(Self {
            maintainer: Some(MaintainerState::new(
                webhook_secret,
                secret_revision,
                expected_target_id,
                DeliveryJournal::open_development(database).map_err(|_| OpenError::Journal)?,
            )),
            deployment: Deployment::Development,
        })
    }

    fn from_environment() -> Result<Self, ProductionOpenError> {
        if AMBIENT_POSTGRES_ENV
            .iter()
            .any(|name| std::env::var_os(name).is_some())
        {
            return Err(ProductionOpenError::AmbientPostgresEnvironment);
        }
        let database_url = required_environment(DATABASE_URL_ENV)?;
        let webhook_secret = required_environment(WEBHOOK_SECRET_ENV)?;
        let secret_revision = required_environment(SECRET_REVISION_ENV)?;
        let expected_app_id = required_environment(APP_ID_ENV)?
            .parse::<i64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or(ProductionOpenError::InvalidAppId)?;
        Self::open_production(
            &database_url,
            webhook_secret.into_bytes(),
            secret_revision,
            expected_app_id,
        )
    }
}

fn required_environment(name: &'static str) -> Result<String, ProductionOpenError> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or(ProductionOpenError::MissingEnvironment(name))
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProductionOpenError {
    #[error("required production environment is missing: {0}")]
    MissingEnvironment(&'static str),
    #[error("DATABASE_URL is not a valid PostgreSQL connection URL")]
    InvalidDatabaseUrl,
    #[error("maintainer webhook secret is invalid")]
    InvalidWebhookSecret,
    #[error("maintainer webhook secret revision is invalid")]
    InvalidSecretRevision,
    #[error("maintainer App ID must be a positive integer")]
    InvalidAppId,
    #[error("ambient PostgreSQL environment is forbidden")]
    AmbientPostgresEnvironment,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum MigrationError {
    #[error("migration database configuration is unavailable")]
    Configuration,
    #[error("control-plane migration failed")]
    Database,
}

#[cfg(feature = "development-sqlite")]
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("expected GitHub App installation target ID must be positive")]
    InvalidExpectedTargetId,
    #[error("development delivery journal could not be opened")]
    Journal,
}

pub fn production_app_from_env() -> Router {
    let state = BrokerState::from_environment().unwrap_or_else(|_| BrokerState::inactive());
    app(state)
}

pub async fn migrate_from_env() -> Result<(), MigrationError> {
    if AMBIENT_POSTGRES_ENV
        .iter()
        .any(|name| std::env::var_os(name).is_some())
    {
        return Err(MigrationError::Configuration);
    }
    let database_url =
        required_environment(DATABASE_URL_ENV).map_err(|_| MigrationError::Configuration)?;
    let pool = journal::postgres_pool(&database_url).map_err(|_| MigrationError::Configuration)?;
    journal::migrate(&pool)
        .await
        .map_err(|_| MigrationError::Database)
}

pub fn app(state: BrokerState) -> Router {
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready));
    let router = if state.maintainer.is_some() {
        router.route(
            maintainer::WEBHOOK_PATH,
            axum::routing::post(maintainer::receive),
        )
    } else {
        router
    };
    router
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn health() -> Response {
    json_response(StatusCode::OK, r#"{"status":"ok"}"#)
}

async fn ready(State(state): State<BrokerState>) -> Response {
    match (state.deployment, state.maintainer.as_ref()) {
        (Deployment::Production, Some(maintainer)) if maintainer.ready().await.is_ok() => {
            json_response(
                StatusCode::OK,
                r#"{"status":"ready","maintainer_webhook":"bootstrap_ping_only","product_webhook":"inactive","operator_api":"inactive"}"#,
            )
        }
        (Deployment::Production, Some(_)) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"unavailable","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
        ),
        #[cfg(feature = "development-sqlite")]
        (Deployment::Development, _) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"inactive","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
        ),
        (Deployment::Inactive | Deployment::Production, _) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"inactive","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
        ),
    }
}

fn json_response(status: StatusCode, body: &'static str) -> Response {
    (
        status,
        [
            ("content-type", "application/json"),
            ("cache-control", "no-store"),
        ],
        body,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_configuration_rejects_partial_or_invalid_values() {
        assert_eq!(
            BrokerState::open_production(
                "not-a-postgres-url",
                vec![b'x'; 32],
                "maintainer-v1".to_owned(),
                42,
            )
            .err(),
            Some(ProductionOpenError::InvalidDatabaseUrl)
        );
        assert_eq!(
            BrokerState::open_production(
                "postgresql://127.0.0.1/control_plane",
                b"short".to_vec(),
                "maintainer-v1".to_owned(),
                42,
            )
            .err(),
            Some(ProductionOpenError::InvalidWebhookSecret)
        );
        assert_eq!(
            BrokerState::open_production(
                "postgresql://127.0.0.1/control_plane",
                vec![b'x'; 32],
                "NOT_VALID".to_owned(),
                42,
            )
            .err(),
            Some(ProductionOpenError::InvalidSecretRevision)
        );
        assert_eq!(
            BrokerState::open_production(
                "postgresql://127.0.0.1/control_plane",
                vec![b'x'; 32],
                "maintainer-v1".to_owned(),
                0,
            )
            .err(),
            Some(ProductionOpenError::InvalidAppId)
        );
    }
}
