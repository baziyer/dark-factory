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
#[cfg(target_arch = "wasm32")]
use tower::ServiceExt as _;

mod journal;
pub mod maintainer;

use maintainer::{MAX_BODY_BYTES, MaintainerState};

pub const WEBHOOK_SECRET_BINDING: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET";
pub const SECRET_REVISION_BINDING: &str = "DARK_FACTORY_MAINTAINER_WEBHOOK_SECRET_REVISION";
pub const APP_ID_BINDING: &str = "DARK_FACTORY_MAINTAINER_APP_ID";

#[derive(Clone, Copy)]
enum Deployment {
    Inactive,
    #[cfg(feature = "development-sqlite")]
    Development,
    #[cfg(target_arch = "wasm32")]
    Cloudflare,
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

    #[cfg(target_arch = "wasm32")]
    fn from_worker_env(env: &worker::Env) -> Result<Self, AuthorityError> {
        let secret = required_secret(env, WEBHOOK_SECRET_BINDING)?;
        let revision = required_secret(env, SECRET_REVISION_BINDING)?;
        let app_id = required_secret(env, APP_ID_BINDING)?;
        let (secret, revision, app_id) = validate_authority(secret, revision, app_id)?;
        let namespace = env
            .durable_object(journal::NAMESPACE_BINDING)
            .map_err(|_| AuthorityError::BindingMissing(journal::NAMESPACE_BINDING))?;
        Ok(Self {
            maintainer: Some(MaintainerState::new(
                secret,
                revision,
                app_id,
                journal::DeliveryJournal::cloudflare(namespace, app_id),
            )),
            deployment: Deployment::Cloudflare,
        })
    }

    #[cfg(feature = "development-sqlite")]
    pub fn open_development(
        database: &Path,
        webhook_secret: maintainer::WebhookSecret,
        secret_revision: maintainer::SecretRevision,
        expected_target_id: i64,
    ) -> Result<Self, OpenError> {
        if !(1..=maintainer::MAX_EXACT_INTEGER).contains(&expected_target_id) {
            return Err(OpenError::InvalidExpectedTargetId);
        }
        Ok(Self {
            maintainer: Some(MaintainerState::new(
                webhook_secret,
                secret_revision,
                expected_target_id,
                journal::DeliveryJournal::open_development(database)
                    .map_err(|_| OpenError::Journal)?,
            )),
            deployment: Deployment::Development,
        })
    }
}

#[cfg(target_arch = "wasm32")]
fn required_secret(env: &worker::Env, name: &'static str) -> Result<String, AuthorityError> {
    let value = env
        .secret(name)
        .map_err(|_| AuthorityError::BindingMissing(name))?
        .to_string();
    if value.is_empty() {
        Err(AuthorityError::BindingMissing(name))
    } else {
        Ok(value)
    }
}

#[cfg(any(target_arch = "wasm32", test))]
fn validate_authority(
    secret: String,
    revision: String,
    app_id: String,
) -> Result<(maintainer::WebhookSecret, maintainer::SecretRevision, i64), AuthorityError> {
    let secret = maintainer::WebhookSecret::new(secret.into_bytes())
        .map_err(|_| AuthorityError::WebhookSecret)?;
    let revision =
        maintainer::SecretRevision::new(revision).map_err(|_| AuthorityError::SecretRevision)?;
    let app_id = app_id
        .parse::<i64>()
        .ok()
        .filter(|value| (1..=maintainer::MAX_EXACT_INTEGER).contains(value))
        .ok_or(AuthorityError::AppId)?;
    Ok((secret, revision, app_id))
}

#[cfg(any(target_arch = "wasm32", test))]
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
enum AuthorityError {
    #[cfg(target_arch = "wasm32")]
    #[error("required Worker binding is missing: {0}")]
    BindingMissing(&'static str),
    #[error("maintainer webhook secret is invalid")]
    WebhookSecret,
    #[error("maintainer webhook secret revision is invalid")]
    SecretRevision,
    #[error("maintainer App ID must be a positive integer")]
    AppId,
}

#[cfg(feature = "development-sqlite")]
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("expected GitHub App installation target ID must be positive")]
    InvalidExpectedTargetId,
    #[error("development delivery journal could not be opened")]
    Journal,
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

#[cfg_attr(target_arch = "wasm32", worker::send)]
async fn ready(State(state): State<BrokerState>) -> Response {
    match (state.deployment, state.maintainer.as_ref()) {
        #[cfg(target_arch = "wasm32")]
        (Deployment::Cloudflare, Some(maintainer)) if maintainer.ready().await.is_ok() => {
            json_response(
                StatusCode::OK,
                r#"{"status":"ready","maintainer_webhook":"bootstrap_ping_only","product_webhook":"inactive","operator_api":"inactive"}"#,
            )
        }
        #[cfg(target_arch = "wasm32")]
        (Deployment::Cloudflare, Some(_)) => json_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"status":"unavailable","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
        ),
        #[cfg(feature = "development-sqlite")]
        (Deployment::Development, _) => inactive_readiness(),
        (Deployment::Inactive, _) => inactive_readiness(),
        #[cfg(target_arch = "wasm32")]
        (Deployment::Cloudflare, None) => inactive_readiness(),
    }
}

fn inactive_readiness() -> Response {
    json_response(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"status":"inactive","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
    )
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

#[cfg(target_arch = "wasm32")]
#[worker::event(fetch)]
pub async fn fetch(
    request: worker::HttpRequest,
    env: worker::Env,
    _context: worker::Context,
) -> worker::Result<axum::http::Response<axum::body::Body>> {
    let state = BrokerState::from_worker_env(&env).unwrap_or_else(|_| BrokerState::inactive());
    match app(state).oneshot(request).await {
        Ok(response) => Ok(response),
        Err(error) => match error {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_configuration_is_all_or_nothing() {
        assert!(
            validate_authority(
                "0123456789abcdef0123456789abcdef".into(),
                "maintainer-v1".into(),
                "5678".into(),
            )
            .is_ok()
        );
        assert_eq!(
            validate_authority("short".into(), "maintainer-v1".into(), "5678".into()).err(),
            Some(AuthorityError::WebhookSecret)
        );
        assert_eq!(
            validate_authority(
                "0123456789abcdef0123456789abcdef".into(),
                "INVALID".into(),
                "5678".into(),
            )
            .err(),
            Some(AuthorityError::SecretRevision)
        );
        assert_eq!(
            validate_authority(
                "0123456789abcdef0123456789abcdef".into(),
                "maintainer-v1".into(),
                "0".into(),
            )
            .err(),
            Some(AuthorityError::AppId)
        );
    }
}
