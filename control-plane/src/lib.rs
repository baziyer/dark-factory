//! Hosted GitHub authority boundary. This standalone service never links to
//! `factoryd`; its initial surface only authenticates and journals deliveries.

#[cfg(feature = "development-sqlite")]
use std::path::Path;

use axum::{Router, extract::DefaultBodyLimit, response::IntoResponse as _, routing::get};

#[cfg(feature = "development-sqlite")]
mod journal;
pub mod maintainer;

#[cfg(feature = "development-sqlite")]
use journal::DeliveryJournal;
#[cfg(feature = "development-sqlite")]
use maintainer::{MAX_BODY_BYTES, MaintainerState, SecretRevision, WebhookSecret};

#[derive(Clone)]
pub struct BrokerState {
    #[cfg(feature = "development-sqlite")]
    maintainer: Option<MaintainerState>,
}

impl BrokerState {
    #[must_use]
    pub const fn inactive() -> Self {
        Self {
            #[cfg(feature = "development-sqlite")]
            maintainer: None,
        }
    }

    /// Opens the development-only SQLite delivery journal. A serverless deploy
    /// must replace this constructor with a durable shared journal before its
    /// webhook or readiness route can be active.
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
            maintainer: Some(MaintainerState::development(
                webhook_secret,
                secret_revision,
                expected_target_id,
                DeliveryJournal::open(database).map_err(|_| OpenError::Journal)?,
            )),
        })
    }
}

pub fn app(state: BrokerState) -> Router {
    let router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready));
    #[cfg(feature = "development-sqlite")]
    let router = if state.maintainer.is_some() {
        router
            .route(
                maintainer::WEBHOOK_PATH,
                axum::routing::post(maintainer::receive),
            )
            .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
    } else {
        router
    };
    #[cfg(not(feature = "development-sqlite"))]
    let router = router.layer(DefaultBodyLimit::max(0));
    router.with_state(state)
}

#[cfg(feature = "development-sqlite")]
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("expected GitHub App installation target ID must be positive")]
    InvalidExpectedTargetId,
    #[error("development delivery journal could not be opened")]
    Journal,
}

async fn health() -> impl axum::response::IntoResponse {
    ([("content-type", "application/json")], r#"{"status":"ok"}"#).into_response()
}

async fn ready() -> impl axum::response::IntoResponse {
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        [("content-type", "application/json")],
        r#"{"status":"inactive","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
    )
        .into_response()
}
