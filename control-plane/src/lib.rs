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
#[cfg(feature = "provision-runtime")]
use url::Url;

mod journal;
pub mod maintainer;
#[cfg(feature = "provision-runtime")]
mod neon;

use journal::DeliveryJournal;
use maintainer::{MAX_BODY_BYTES, MaintainerState, SecretRevision, WebhookSecret};

pub const DATABASE_URL_ENV: &str = "DATABASE_URL";
pub const RUNTIME_DATABASE_URL_ENV: &str = "DARK_FACTORY_BROKER_DATABASE_URL";
pub const NEON_API_KEY_ENV: &str = "DARK_FACTORY_NEON_API_KEY";
pub const NEON_PROJECT_ID_ENV: &str = "NEON_PROJECT_ID";
pub const WEBHOOK_SECRET_ENV: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET";
pub const SECRET_REVISION_ENV: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION";
pub const APP_ID_ENV: &str = "DARK_FACTORY_MAINTAINER_APP_ID";
const VERCEL_ENV_ENV: &str = "VERCEL_ENV";

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
        if owner_database_environment_is_present() {
            return Err(ProductionOpenError::OwnerDatabaseEnvironment);
        }
        let database_url = required_environment(RUNTIME_DATABASE_URL_ENV)?;
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

fn owner_database_environment_is_present() -> bool {
    std::env::var_os(DATABASE_URL_ENV).is_some()
        || std::env::var_os("DATABASE_URL_UNPOOLED").is_some()
        || std::env::var_os(NEON_PROJECT_ID_ENV).is_some()
        || std::env::var_os(NEON_API_KEY_ENV).is_some()
        || std::env::vars_os().any(|(name, _)| {
            name.to_str()
                .is_some_and(|name| name.starts_with("PG") || name.starts_with("POSTGRES_"))
        })
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProductionOpenError {
    #[error("required production environment is missing: {0}")]
    MissingEnvironment(&'static str),
    #[error("database URL is not a valid PostgreSQL connection URL")]
    InvalidDatabaseUrl,
    #[error("maintainer webhook secret is invalid")]
    InvalidWebhookSecret,
    #[error("maintainer webhook secret revision is invalid")]
    InvalidSecretRevision,
    #[error("maintainer App ID must be a positive integer")]
    InvalidAppId,
    #[error("provider owner database environment must be absent from the runtime")]
    OwnerDatabaseEnvironment,
}

#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum ProvisionError {
    #[error("owner database configuration is unavailable")]
    Configuration,
    #[error("control-plane runtime provisioning failed")]
    Database,
    #[error("Neon runtime credential reset was rejected")]
    Api,
    #[error("Neon runtime credential reset outcome is indeterminate")]
    IndeterminateReset,
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
    if std::env::var(VERCEL_ENV_ENV).as_deref() != Ok("production") {
        return app(BrokerState::inactive());
    }
    let state = BrokerState::from_environment().unwrap_or_else(|_| BrokerState::inactive());
    app(state)
}

#[cfg(feature = "provision-runtime")]
pub async fn provision_runtime_from_env() -> Result<String, ProvisionError> {
    let owner_database_url =
        required_environment(DATABASE_URL_ENV).map_err(|_| ProvisionError::Configuration)?;
    let expected_project_id =
        required_environment(NEON_PROJECT_ID_ENV).map_err(|_| ProvisionError::Configuration)?;
    let api_key =
        required_environment(NEON_API_KEY_ENV).map_err(|_| ProvisionError::Configuration)?;
    let preparation_pool =
        journal::neon_owner_pool(&owner_database_url).map_err(|_| ProvisionError::Configuration)?;
    let api = neon::NeonApi::new(&api_key).map_err(map_neon_error)?;
    let identity = journal::prepare_runtime(&preparation_pool, &expected_project_id)
        .await
        .map_err(|_| ProvisionError::Database)?;
    // Neon may terminate compute connections while rotating a role password.
    // Close the preparation pool before the non-idempotent request, then make
    // activation prove itself over a newly established verified connection.
    preparation_pool.close().await;
    let password = api
        .reset_runtime_password(&identity)
        .await
        .map_err(map_neon_error)?;
    let activation_pool =
        journal::neon_owner_pool(&owner_database_url).map_err(|_| ProvisionError::Configuration)?;
    journal::activate_runtime(&activation_pool, &identity)
        .await
        .map_err(|_| ProvisionError::Database)?;

    let runtime_url = runtime_database_url(&owner_database_url, &password)?;
    journal::verify_runtime(&runtime_url)
        .await
        .map_err(|_| ProvisionError::Database)?;
    Ok(runtime_url)
}

#[cfg(feature = "provision-runtime")]
fn runtime_database_url(
    owner_database_url: &str,
    password: &str,
) -> Result<String, ProvisionError> {
    let mut runtime_url =
        Url::parse(owner_database_url).map_err(|_| ProvisionError::Configuration)?;
    runtime_url
        .set_username(journal::RUNTIME_ROLE)
        .map_err(|_| ProvisionError::Configuration)?;
    runtime_url
        .set_password(Some(password))
        .map_err(|_| ProvisionError::Configuration)?;
    runtime_url
        .query_pairs_mut()
        .clear()
        .append_pair("sslmode", "verify-full");
    Ok(runtime_url.into())
}

#[cfg(feature = "provision-runtime")]
const fn map_neon_error(error: neon::Error) -> ProvisionError {
    match error {
        neon::Error::Configuration => ProvisionError::Configuration,
        neon::Error::IndeterminateReset => ProvisionError::IndeterminateReset,
        neon::Error::Rejected => ProvisionError::Api,
    }
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

    #[tokio::test]
    async fn production_configuration_rejects_partial_or_invalid_values() {
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

        let open = |database_url: &str| {
            BrokerState::open_production(
                database_url,
                vec![b'x'; 32],
                "maintainer-v1".to_owned(),
                42,
            )
        };
        assert!(
            open(
                "postgresql://runtime:password@database.example/control_plane?sslmode=require&channel_binding=require"
            )
            .is_ok()
        );
        for rejected in [
            "postgresql://runtime:password@database.example/control_plane?sslmode=require&options=search_path%3Dpublic",
            "postgresql://runtime:password@database.example/control_plane?sslmode=require&sslmode=require",
            "postgresql://runtime:@database.example/control_plane?sslmode=require",
            "postgresql://runtime:password@database.example/control_plane/extra?sslmode=require",
            "postgresql://runtime:password@database.example/control_plane?sslmode=require#fragment",
        ] {
            assert_eq!(
                open(rejected).err(),
                Some(ProductionOpenError::InvalidDatabaseUrl)
            );
        }
    }

    #[cfg(feature = "provision-runtime")]
    #[test]
    fn provisioned_url_drops_unenforced_channel_binding_and_requires_verified_tls() {
        let url = runtime_database_url(
            "postgresql://owner:owner-password@ep-example.eu-west-2.aws.neon.tech/neondb?sslmode=require&channel_binding=require",
            "runtime password/with punctuation",
        )
        .unwrap();
        let parsed = Url::parse(&url).unwrap();
        assert_eq!(parsed.username(), journal::RUNTIME_ROLE);
        assert_eq!(
            parsed.password(),
            Some("runtime%20password%2Fwith%20punctuation")
        );
        assert_eq!(
            parsed.query_pairs().collect::<Vec<_>>(),
            [("sslmode".into(), "verify-full".into())]
        );
    }
}
