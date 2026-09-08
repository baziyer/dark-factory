//! Hosted GitHub authority boundary. This standalone service never links to
//! `factoryd`; its bootstrap surface only authenticates and journals ping
//! deliveries.

#![recursion_limit = "256"]

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

#[cfg(any(target_arch = "wasm32", test))]
mod access;
#[cfg(any(target_arch = "wasm32", test))]
mod github_app;
mod journal;
pub mod maintainer;
#[cfg(target_arch = "wasm32")]
mod mcp;

use maintainer::MaintainerState;

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
    #[cfg(target_arch = "wasm32")]
    pub(crate) mcp: Option<mcp::McpState>,
    deployment: Deployment,
}

impl BrokerState {
    #[must_use]
    pub const fn inactive() -> Self {
        Self {
            maintainer: None,
            #[cfg(target_arch = "wasm32")]
            mcp: None,
            deployment: Deployment::Inactive,
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn from_worker_env(env: &worker::Env) -> Result<Self, AuthorityError> {
        let secret = required_secret(env, WEBHOOK_SECRET_BINDING)?;
        let revision = required_secret(env, SECRET_REVISION_BINDING)?;
        let app_id = required_secret(env, APP_ID_BINDING)?;
        let (secret, revision, app_id) = validate_authority(secret, revision, app_id)?;
        let operation_authority = optional_operation_authority(env, app_id)?;
        let namespace = env
            .durable_object(journal::NAMESPACE_BINDING)
            .map_err(|_| AuthorityError::BindingMissing(journal::NAMESPACE_BINDING))?;
        let journal = journal::DeliveryJournal::cloudflare(namespace, app_id);
        Ok(Self {
            maintainer: Some(MaintainerState::new(
                secret,
                revision,
                app_id,
                journal.clone(),
                operation_authority.as_ref().map(|(_, app)| app.clone()),
            )),
            mcp: operation_authority.map(|(access, app)| mcp::McpState::new(access, app, journal)),
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
            #[cfg(target_arch = "wasm32")]
            mcp: None,
            deployment: Deployment::Development,
        })
    }
}

#[cfg(target_arch = "wasm32")]
fn optional_operation_authority(
    env: &worker::Env,
    app_id: i64,
) -> Result<Option<(access::AccessAuthority, github_app::AppAuthority)>, AuthorityError> {
    operation_authority_from_values(
        app_id,
        [
            optional_secret(env, github_app::PRIVATE_KEY_BINDING),
            optional_secret(env, github_app::PERMISSION_REVISION_BINDING),
            optional_secret(env, access::OPERATOR_EMAIL_DIGEST_BINDING),
            optional_secret(env, access::TEAM_DOMAIN_BINDING),
            optional_secret(env, access::AUDIENCE_BINDING),
        ],
        optional_secret(env, access::SERVICE_TOKEN_BINDING),
    )
}

#[cfg(any(target_arch = "wasm32", test))]
fn operation_authority_from_values(
    app_id: i64,
    values: [Option<String>; 5],
    // Separate from the all-or-nothing group above: headless access is opt-in,
    // so its absence must leave the rest of the authority group configured
    // rather than tipping the whole surface inactive.
    service_token_id: Option<String>,
) -> Result<Option<(access::AccessAuthority, github_app::AppAuthority)>, AuthorityError> {
    if values.iter().all(Option::is_none) {
        return Ok(None);
    }
    let [
        Some(private_key),
        Some(permission_revision),
        Some(operator_email_digest),
        Some(team_domain),
        Some(audience),
    ] = values
    else {
        return Err(AuthorityError::AppAuthority);
    };
    let app = github_app::AppAuthority::new(app_id, private_key, permission_revision)
        .map_err(|_| AuthorityError::AppAuthority)?;
    let access = access::AccessAuthority::new(
        operator_email_digest,
        team_domain,
        audience,
        service_token_id,
    )
    .map_err(|_| AuthorityError::AppAuthority)?;
    Ok(Some((access, app)))
}

#[cfg(target_arch = "wasm32")]
fn optional_secret(env: &worker::Env, name: &'static str) -> Option<String> {
    env.secret(name)
        .ok()
        .map(|value| value.to_string())
        .filter(|value| !value.is_empty())
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
    #[cfg(any(target_arch = "wasm32", test))]
    #[error("maintainer App operation authority is invalid")]
    AppAuthority,
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
            axum::routing::post(maintainer::receive)
                .layer(DefaultBodyLimit::max(maintainer::MAX_BODY_BYTES)),
        )
    } else {
        router
    };
    #[cfg(target_arch = "wasm32")]
    let router = if state.mcp.is_some() {
        router.route(
            mcp::PATH,
            // This ceiling is derived above every typed publication allowed by
            // MCP. The old global webhook cap drifted below that contract.
            axum::routing::post(mcp::receive).layer(DefaultBodyLimit::max(mcp::MAX_BODY_BYTES)),
        )
    } else {
        router
    };
    router.with_state(state)
}

async fn health() -> Response {
    json_response(StatusCode::OK, r#"{"status":"ok"}"#)
}

#[cfg_attr(target_arch = "wasm32", worker::send)]
async fn ready(State(state): State<BrokerState>) -> Response {
    match (state.deployment, state.maintainer.as_ref()) {
        #[cfg(target_arch = "wasm32")]
        (Deployment::Cloudflare, Some(maintainer)) if maintainer.ready().await.is_ok() => {
            // Report the operations surface ready only when its own live
            // dependency answers. Gating on configuration alone let a Worker
            // that could not reach Cloudflare Access at all still advertise a
            // working surface while every call returned 401.
            if let Some(mcp) = state.mcp.as_ref() {
                if mcp.ready().await.is_err() {
                    return json_response(
                        StatusCode::SERVICE_UNAVAILABLE,
                        r#"{"status":"unavailable","maintainer_webhook":"inactive","maintainer_operations":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
                    );
                }
                // Name which principals can actually reach the surface. The
                // headless binding is optional and inherited across versions,
                // so a deployment that drops it must be visible here rather
                // than only in the 401 every service-token call then gets.
                json_response(
                    StatusCode::OK,
                    if mcp.headless() {
                        r#"{"status":"ready","maintainer_webhook":"signed_ping_with_app_verification","maintainer_operations":"mcp_installation_bound_operator_and_headless","product_webhook":"inactive","operator_api":"inactive"}"#
                    } else {
                        r#"{"status":"ready","maintainer_webhook":"signed_ping_with_app_verification","maintainer_operations":"mcp_installation_bound_operator_only","product_webhook":"inactive","operator_api":"inactive"}"#
                    },
                )
            } else {
                json_response(
                    StatusCode::OK,
                    r#"{"status":"ready","maintainer_webhook":"bootstrap_ping_only","product_webhook":"inactive","operator_api":"inactive"}"#,
                )
            }
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
    use sha2::Digest as _;

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

    #[test]
    fn operation_authority_group_is_absent_complete_or_inactive() {
        assert!(
            operation_authority_from_values(4_673_420, [None, None, None, None, None], None,)
                .is_ok_and(|value| value.is_none())
        );
        assert_eq!(
            operation_authority_from_values(
                4_673_420,
                [Some("partial".into()), None, None, None, None],
                None,
            )
            .err(),
            Some(AuthorityError::AppAuthority)
        );
        let private_key = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            vec![7_u8; 1_200],
        );
        assert!(
            operation_authority_from_values(
                4_673_420,
                [
                    Some(private_key),
                    Some("maintainer-operations-v6".into()),
                    Some(hex::encode(sha2::Sha256::digest(b"operator@example.com"))),
                    Some("https://dark-factory.cloudflareaccess.com".into()),
                    Some("a".repeat(64)),
                ],
                None,
            )
            .is_ok_and(|value| value.is_some())
        );
    }
}
