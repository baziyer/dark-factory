//! Hosted GitHub authority boundary. This standalone service never links to
//! `factoryd`; its initial surface only authenticates and journals deliveries.

use std::path::Path;

use axum::{Router, extract::DefaultBodyLimit, response::IntoResponse as _, routing::get};

mod journal;
pub mod maintainer;
pub mod operator;
pub mod product;

use journal::DeliveryJournal;
use maintainer::{MAX_BODY_BYTES, MaintainerState, SecretRevision, WebhookSecret};

#[derive(Clone)]
pub struct BrokerState {
    maintainer: MaintainerState,
}

impl BrokerState {
    /// Opens the development-only SQLite delivery journal. A serverless deploy
    /// must replace this constructor with a durable shared journal before its
    /// webhook or readiness route can be active.
    pub fn open_development(
        database: &Path,
        webhook_secret: WebhookSecret,
        secret_revision: SecretRevision,
    ) -> Result<Self, OpenError> {
        Ok(Self {
            maintainer: MaintainerState::development(
                webhook_secret,
                secret_revision,
                DeliveryJournal::open(database).map_err(OpenError)?,
            ),
        })
    }
}

pub fn app(state: BrokerState) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route(
            maintainer::WEBHOOK_PATH,
            axum::routing::post(maintainer::receive),
        )
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

#[derive(Debug, thiserror::Error)]
#[error("development delivery journal could not be opened")]
pub struct OpenError(#[source] journal::Error);

async fn health() -> impl axum::response::IntoResponse {
    (
        [("content-type", "application/json")],
        r#"{"status":"development_only","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#,
    )
        .into_response()
}
