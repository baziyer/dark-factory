#[cfg(feature = "development-sqlite")]
use std::sync::Arc;

#[cfg(feature = "development-sqlite")]
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse as _, Response},
};
use hmac::{Hmac, Mac as _};
#[cfg(feature = "development-sqlite")]
use serde_json::Value;
#[cfg(feature = "development-sqlite")]
use sha2::Digest as _;
use sha2::Sha256;

#[cfg(feature = "development-sqlite")]
use crate::{BrokerState, journal};

pub const WEBHOOK_PATH: &str = "/v1/github/maintainer/webhook";
#[cfg(feature = "development-sqlite")]
pub const MAX_BODY_BYTES: usize = 64 * 1024;
#[cfg(feature = "development-sqlite")]
const MAX_SECRET_BYTES: usize = 1024;
#[cfg(feature = "development-sqlite")]
const MAX_IDENTIFIER_BYTES: usize = 64;

#[cfg(feature = "development-sqlite")]
#[derive(Clone)]
pub struct WebhookSecret(Arc<[u8]>);

#[cfg(feature = "development-sqlite")]
#[derive(Clone)]
pub struct SecretRevision(Arc<str>);

#[cfg(feature = "development-sqlite")]
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum SecretError {
    #[error("webhook secret must contain between 32 and 1024 bytes")]
    InvalidLength,
    #[error("secret revision must contain 1-64 lowercase ASCII identifier bytes")]
    InvalidRevision,
}

#[cfg(feature = "development-sqlite")]
impl WebhookSecret {
    pub fn new(secret: Vec<u8>) -> Result<Self, SecretError> {
        if !(32..=MAX_SECRET_BYTES).contains(&secret.len()) {
            return Err(SecretError::InvalidLength);
        }
        Ok(Self(secret.into()))
    }
}

#[cfg(feature = "development-sqlite")]
impl SecretRevision {
    pub fn new(revision: String) -> Result<Self, SecretError> {
        let valid = !revision.is_empty()
            && revision.len() <= MAX_IDENTIFIER_BYTES
            && revision.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
            });
        if !valid {
            return Err(SecretError::InvalidRevision);
        }
        Ok(Self(revision.into()))
    }
}

#[cfg(feature = "development-sqlite")]
#[derive(Clone)]
pub(crate) struct MaintainerState {
    secret: WebhookSecret,
    secret_revision: SecretRevision,
    expected_target_id: i64,
    journal: journal::DeliveryJournal,
}

#[cfg(feature = "development-sqlite")]
impl MaintainerState {
    pub(crate) fn development(
        secret: WebhookSecret,
        secret_revision: SecretRevision,
        expected_target_id: i64,
        journal: journal::DeliveryJournal,
    ) -> Self {
        Self {
            secret,
            secret_revision,
            expected_target_id,
            journal,
        }
    }
}

#[cfg(feature = "development-sqlite")]
#[derive(Clone, Copy)]
pub(crate) enum Disposition {
    Ping,
    PolicyRejected,
    PayloadRejected,
}

#[cfg(feature = "development-sqlite")]
impl Disposition {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::PolicyRejected => "policy_rejected",
            Self::PayloadRejected => "payload_rejected",
        }
    }

    pub(crate) fn from_database(value: &str) -> Result<Self, rusqlite::Error> {
        match value {
            "ping" => Ok(Self::Ping),
            "policy_rejected" => Ok(Self::PolicyRejected),
            "payload_rejected" => Ok(Self::PayloadRejected),
            other => Err(rusqlite::Error::FromSqlConversionFailure(
                7,
                rusqlite::types::Type::Text,
                format!("invalid maintainer disposition: {other}").into(),
            )),
        }
    }

    const fn response(self) -> (StatusCode, &'static str) {
        match self {
            Self::Ping => (StatusCode::OK, r#"{"status":"ping"}"#),
            Self::PolicyRejected => (
                StatusCode::UNPROCESSABLE_ENTITY,
                r#"{"error":"policy_rejected"}"#,
            ),
            Self::PayloadRejected => (
                StatusCode::UNPROCESSABLE_ENTITY,
                r#"{"error":"payload_not_allowed"}"#,
            ),
        }
    }
}

#[cfg(feature = "development-sqlite")]
pub(crate) struct Delivery {
    pub(crate) delivery_id: String,
    pub(crate) hook_id: i64,
    pub(crate) target_id: i64,
    pub(crate) target_type: String,
    pub(crate) event: String,
    pub(crate) action: Option<String>,
    pub(crate) body_digest: String,
    pub(crate) disposition: Disposition,
    pub(crate) secret_revision: String,
}

#[cfg(feature = "development-sqlite")]
pub(crate) async fn receive(
    State(state): State<BrokerState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Some(maintainer) = state.maintainer.as_ref() else {
        return response(StatusCode::SERVICE_UNAVAILABLE, "inactive");
    };
    match header(&headers, CONTENT_TYPE.as_str()) {
        Ok(value) if value == "application/json" || value.starts_with("application/json;") => {}
        Ok(_) => return response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "invalid_content_type"),
        Err(status) => return response(status, "invalid_headers"),
    }
    let event = match header(&headers, "x-github-event").and_then(validate_identifier) {
        Ok(value) => value,
        Err(status) => return response(status, "invalid_headers"),
    };
    let delivery_id = match header(&headers, "x-github-delivery").and_then(validate_delivery) {
        Ok(value) => value,
        Err(status) => return response(status, "invalid_headers"),
    };
    let hook_id = match numeric_header(&headers, "x-github-hook-id") {
        Ok(value) => value,
        Err(status) => return response(status, "invalid_headers"),
    };
    let target_id = match numeric_header(&headers, "x-github-hook-installation-target-id") {
        Ok(value) => value,
        Err(status) => return response(status, "invalid_headers"),
    };
    let target_type = match header(&headers, "x-github-hook-installation-target-type") {
        Ok("integration") => "integration",
        Ok(_) => return response(StatusCode::BAD_REQUEST, "invalid_headers"),
        Err(status) => return response(status, "invalid_headers"),
    };
    let signature = match header(&headers, "x-hub-signature-256") {
        Ok(value) => value,
        Err(status) => return response(status, "invalid_headers"),
    };
    if !verify_signature(maintainer.secret.0.as_ref(), signature, &body) {
        return response(StatusCode::UNAUTHORIZED, "invalid_signature");
    }
    if target_id != maintainer.expected_target_id {
        return response(StatusCode::FORBIDDEN, "wrong_installation_target");
    }

    let (action, disposition) = disposition(event, &body);
    let delivery = Delivery {
        delivery_id: delivery_id.to_owned(),
        hook_id,
        target_id,
        target_type: target_type.to_owned(),
        event: event.to_owned(),
        action,
        body_digest: hex::encode(Sha256::digest(&body)),
        disposition,
        secret_revision: maintainer.secret_revision.0.to_string(),
    };
    let journal = maintainer.journal.clone();
    let stored = tokio::task::spawn_blocking(move || journal.record(&delivery)).await;
    match stored {
        Ok(Ok(journal::Record::New)) => disposition_response(disposition),
        Ok(Ok(journal::Record::Replay(stored))) => disposition_response(stored),
        Ok(Ok(journal::Record::Conflict)) => response(StatusCode::CONFLICT, "delivery_conflict"),
        Ok(Err(_)) | Err(_) => response(StatusCode::SERVICE_UNAVAILABLE, "journal_unavailable"),
    }
}

#[cfg(feature = "development-sqlite")]
fn disposition(event: &str, body: &[u8]) -> (Option<String>, Disposition) {
    let payload = serde_json::from_slice(body)
        .ok()
        .and_then(|value| match value {
            Value::Object(payload) => Some(payload),
            _ => None,
        });
    let action = payload
        .as_ref()
        .and_then(|payload| payload.get("action"))
        .and_then(Value::as_str)
        .filter(|action| validate_identifier(action).is_ok())
        .map(str::to_owned);
    if event != "ping" {
        return (action, Disposition::PolicyRejected);
    }
    match payload
        .as_ref()
        .and_then(|payload| payload.get("zen"))
        .and_then(Value::as_str)
    {
        Some(_) => (None, Disposition::Ping),
        None => (None, Disposition::PayloadRejected),
    }
}

#[cfg(feature = "development-sqlite")]
fn disposition_response(disposition: Disposition) -> Response {
    let (status, body) = disposition.response();
    (status, [("content-type", "application/json")], body).into_response()
}

#[cfg(feature = "development-sqlite")]
fn response(status: StatusCode, error: &'static str) -> Response {
    (
        status,
        [("content-type", "application/json")],
        format!(r#"{{"error":"{error}"}}"#),
    )
        .into_response()
}

#[cfg(feature = "development-sqlite")]
fn header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, StatusCode> {
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(StatusCode::BAD_REQUEST)?;
    if values.next().is_some() {
        return Err(StatusCode::BAD_REQUEST);
    }
    value.to_str().map_err(|_| StatusCode::BAD_REQUEST)
}

#[cfg(feature = "development-sqlite")]
fn validate_identifier(value: &str) -> Result<&str, StatusCode> {
    let valid = !value.is_empty()
        && value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_');
    valid.then_some(value).ok_or(StatusCode::BAD_REQUEST)
}

#[cfg(feature = "development-sqlite")]
fn validate_delivery(delivery: &str) -> Result<&str, StatusCode> {
    let valid = delivery.len() == 36
        && delivery.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
            }
        });
    valid.then_some(delivery).ok_or(StatusCode::BAD_REQUEST)
}

#[cfg(feature = "development-sqlite")]
fn numeric_header(headers: &HeaderMap, name: &str) -> Result<i64, StatusCode> {
    header(headers, name)?
        .parse::<i64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or(StatusCode::BAD_REQUEST)
}

#[must_use]
pub fn verify_signature(secret: &[u8], signature: &str, body: &[u8]) -> bool {
    let Some(encoded) = signature.strip_prefix("sha256=") else {
        return false;
    };
    if encoded.len() != 64 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return false;
    }
    let mut supplied = [0_u8; 32];
    if hex::decode_to_slice(encoded, &mut supplied).is_err() {
        return false;
    }
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&supplied).is_ok()
}
