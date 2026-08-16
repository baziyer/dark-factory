//! Bounded, loopback-only authenticated webhook integrations.

use std::{
    fmt,
    fs::OpenOptions,
    future::Future,
    io::Read,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path as AxumPath, State},
    http::{HeaderMap, Request, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use factory_core::{AgentId, ObserverHealth, ProjectId, TaskId};
use rustix::fs::OFlags;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::net::TcpListener;

use crate::{
    daemon_state::{DaemonState, DaemonStateError},
    store::{
        NewWebhookTask, OperationalTaskStatus, StoreError, WebhookAnswer, WebhookDocument,
        WebhookOpenQuestion, WebhookSnapshot, WebhookSnapshotAgent, WebhookSnapshotTask,
        WebhookTaskPoll,
    },
};

const MAX_BODY_BYTES: usize = 1024 * 1024;
const MAX_SECRET_BYTES: u64 = 512;
const MAX_MESSAGE_CHARS: usize = 100_000;
const MAX_TITLE_CHARS: usize = 240;
const MAX_STORED_TITLE_BYTES: usize = 160;
const MAX_FROM_CHARS: usize = 240;
const MAX_DOCUMENT_BYTES: usize = 128 * 1024;
const ENDPOINT_RATE_LIMIT: u64 = 60;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const LEGACY_SECRET_HEADER: &str = "x-md-webhook-secret";
const LEGACY_TOKEN_HEADER: &str = "x-md-webhook-token";
const MAX_CONFIG_BYTES: u64 = 64 * 1024;

#[derive(Deserialize)]
struct CreateInput {
    message: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    kind: Option<InboundKind>,
    #[serde(default)]
    from: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum InboundKind {
    Directive,
    Communication,
}

#[derive(Deserialize)]
struct AnswerInput {
    #[serde(rename = "taskId")]
    task_id: String,
    answer: String,
}

#[derive(Deserialize)]
struct DocumentInput {
    #[serde(rename = "taskId")]
    task_id: String,
    #[serde(rename = "documentId")]
    document_id: String,
}

struct ValidatedCreate {
    message: String,
    title: String,
}

struct ValidatedAnswer {
    task_id: TaskId,
    answer: String,
}

struct ValidatedDocument {
    task_id: TaskId,
    document_id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationalSnapshot {
    generated_at: String,
    counts: OperationalCounts,
    tasks: Vec<OperationalTask>,
    agents: Vec<OperationalAgent>,
}

#[derive(Serialize)]
struct OperationalCounts {
    todo: u64,
    doing: u64,
    blocked: u64,
    done: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationalTask {
    id: String,
    title: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<String>,
    priority: i32,
    created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    started_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    question: Option<OperationalQuestion>,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationalQuestion {
    text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    asked_at: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    documents: Vec<OperationalDocumentRef>,
}

#[derive(Serialize)]
struct OperationalDocumentRef {
    id: String,
    name: String,
    reference: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OperationalAgent {
    id: String,
    name: String,
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    provider: Option<String>,
    is_orchestrator: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_god: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    breaker: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    usd: Option<f64>,
    last_active_sec_ago: Option<u64>,
    inbox_backlog: u64,
}

/// A validated shared secret. Its formatter is intentionally redacted.
pub struct WebhookSecret(Box<[u8]>);

impl fmt::Debug for WebhookSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebhookSecret([REDACTED])")
    }
}

impl WebhookSecret {
    fn matches(&self, provided: &[u8]) -> bool {
        crate::store::constant_time_eq(&self.0, provided)
    }
}

/// Only one wire profile remains: the `legacy_v1` shape Minerva speaks. The
/// `wireProfile` config key is still required and still validated, so an
/// operator's existing `webhooks.json` keeps loading unchanged; any other
/// value is rejected as invalid configuration.
const SUPPORTED_WIRE_PROFILE: &str = "legacy_v1";

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawEndpointConfig {
    id: String,
    wire_profile: String,
    secret_file: PathBuf,
    project_id: ProjectId,
    orchestrator_agent_id: AgentId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RawWebhookConfig {
    version: u16,
    bind: SocketAddr,
    endpoints: Vec<RawEndpointConfig>,
}

struct WebhookEndpoint {
    id: String,
    secret: Arc<WebhookSecret>,
    project_id: ProjectId,
    orchestrator_agent_id: AgentId,
}

/// Fully validated, privacy-sensitive webhook listener configuration. Exactly
/// one endpoint is supported.
pub struct WebhookHttpConfig {
    bind: SocketAddr,
    endpoint: WebhookEndpoint,
}

/// A fully validated and already-bound webhook listener awaiting supervision.
///
/// This deliberately has no `Debug` implementation so private router state
/// cannot be formatted by a startup error path.
pub struct WebhookServer {
    listener: TcpListener,
    router: Router,
    local_addr: SocketAddr,
    metrics: Arc<WebhookHttpMetrics>,
}

impl WebhookServer {
    #[must_use]
    pub const fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn serve<Shutdown>(self, shutdown: Shutdown) -> Result<(), WebhookHttpError>
    where
        Shutdown: Future<Output = ()> + Send + 'static,
    {
        let result = axum::serve(self.listener, self.router)
            .with_graceful_shutdown(shutdown)
            .await;
        let stopped_total = self
            .metrics
            .listener_stopped
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        if result.is_ok() {
            tracing::info!(
                target: "factoryd.webhook",
                event = "listener_stopped",
                outcome = "shutdown",
                listener_stopped_total = stopped_total
            );
            Ok(())
        } else {
            tracing::error!(
                target: "factoryd.webhook",
                event = "listener_stopped",
                outcome = "failed",
                listener_stopped_total = stopped_total
            );
            Err(WebhookHttpError::Serve)
        }
    }
}

#[derive(Debug, Error)]
pub enum WebhookHttpError {
    #[error("webhook HTTP must bind to 127.0.0.1")]
    NonLoopbackBind,
    #[error("webhook configuration could not be opened safely")]
    ConfigOpen,
    #[error("webhook configuration is invalid")]
    InvalidConfig,
    #[error("webhook secret file could not be opened safely")]
    SecretOpen,
    #[error("webhook secret file must be a regular non-symbolic-link file")]
    SecretFileType,
    #[error("webhook secret file permissions must be exactly 0600")]
    SecretPermissions,
    #[error("webhook secret file must be owned by the current user")]
    SecretOwner,
    #[error("webhook secret file contents are invalid")]
    SecretContents,
    #[error("webhook HTTP listener could not bind")]
    Bind,
    #[error("webhook HTTP listener failed")]
    Serve,
}

/// Privacy-safe cumulative listener counters for daemon/operator health.
#[derive(Default)]
pub struct WebhookHttpMetrics {
    listener_ready: AtomicU64,
    listener_stopped: AtomicU64,
    authenticated_requests: AtomicU64,
    rate_limited_requests: AtomicU64,
    bounded_rejections: AtomicU64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookHttpMetricsSnapshot {
    pub listener_ready: u64,
    pub listener_stopped: u64,
    pub authenticated_requests: u64,
    pub rate_limited_requests: u64,
    pub bounded_rejections: u64,
}

impl WebhookHttpMetrics {
    #[must_use]
    pub fn snapshot(&self) -> WebhookHttpMetricsSnapshot {
        WebhookHttpMetricsSnapshot {
            listener_ready: self.listener_ready.load(Ordering::Relaxed),
            listener_stopped: self.listener_stopped.load(Ordering::Relaxed),
            authenticated_requests: self.authenticated_requests.load(Ordering::Relaxed),
            rate_limited_requests: self.rate_limited_requests.load(Ordering::Relaxed),
            bounded_rejections: self.bounded_rejections.load(Ordering::Relaxed),
        }
    }
}

#[derive(Clone)]
struct HttpState {
    daemon: DaemonState,
    endpoint: Arc<WebhookEndpoint>,
    limiter: Arc<Mutex<RateLimits>>,
    metrics: Arc<WebhookHttpMetrics>,
}

#[derive(Clone)]
struct RequestEndpoint {
    id: String,
    secret: Arc<WebhookSecret>,
    project_id: ProjectId,
    orchestrator_agent_id: AgentId,
}

impl From<&WebhookEndpoint> for RequestEndpoint {
    fn from(endpoint: &WebhookEndpoint) -> Self {
        Self {
            id: endpoint.id.clone(),
            secret: Arc::clone(&endpoint.secret),
            project_id: endpoint.project_id.clone(),
            orchestrator_agent_id: endpoint.orchestrator_agent_id.clone(),
        }
    }
}

/// A single global fixed-window limiter. There is exactly one loopback-only
/// endpoint, so per-endpoint budgeting adds nothing.
struct RateLimits {
    started: Instant,
    count: u64,
}

impl Default for RateLimits {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            count: 0,
        }
    }
}

impl RateLimits {
    fn allow(&mut self, now: Instant) -> bool {
        if now.duration_since(self.started) >= RATE_WINDOW {
            self.started = now;
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.count <= ENDPOINT_RATE_LIMIT
    }
}

#[derive(Clone, Copy)]
enum Route {
    Create,
    Poll,
    Snapshot,
    Answer,
    Document,
    Unknown,
}

impl Route {
    const fn name(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Poll => "poll",
            Self::Snapshot => "snapshot",
            Self::Answer => "answer",
            Self::Document => "document",
            Self::Unknown => "unknown",
        }
    }
}

/// Open and validate the secret on its final file descriptor, preventing a
/// symbolic-link swap between a path metadata check and the read.
fn load_private_file(path: &Path, maximum: u64) -> Result<Vec<u8>, WebhookHttpError> {
    let nofollow =
        i32::try_from(OFlags::NOFOLLOW.bits()).map_err(|_| WebhookHttpError::ConfigOpen)?;
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nofollow)
        .open(path)
        .map_err(|_| WebhookHttpError::ConfigOpen)?;
    let metadata = file.metadata().map_err(|_| WebhookHttpError::ConfigOpen)?;
    if !metadata.file_type().is_file() {
        return Err(WebhookHttpError::SecretFileType);
    }
    if metadata.mode() & 0o7777 != 0o600 {
        return Err(WebhookHttpError::SecretPermissions);
    }
    if metadata.uid() != rustix::process::getuid().as_raw() {
        return Err(WebhookHttpError::SecretOwner);
    }
    if metadata.len() == 0 || metadata.len() > maximum {
        return Err(WebhookHttpError::SecretContents);
    }

    let mut contents = Vec::with_capacity(metadata.len() as usize);
    file.take(maximum + 1)
        .read_to_end(&mut contents)
        .map_err(|_| WebhookHttpError::SecretContents)?;
    if contents.len() as u64 != metadata.len() || contents.len() as u64 > maximum {
        return Err(WebhookHttpError::SecretContents);
    }
    Ok(contents)
}

pub fn load_webhook_secret(path: &Path) -> Result<WebhookSecret, WebhookHttpError> {
    let mut contents = load_private_file(path, MAX_SECRET_BYTES)?;
    if contents.last() == Some(&b'\n') {
        contents.pop();
        if contents.last() == Some(&b'\r') {
            contents.pop();
        }
    }
    if contents.is_empty()
        || contents
            .iter()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
    {
        return Err(WebhookHttpError::SecretContents);
    }
    Ok(WebhookSecret(contents.into_boxed_slice()))
}

fn valid_endpoint_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 64
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'-'
        })
}

/// Load one strict owner-only configuration and all endpoint secrets before bind.
pub fn load_webhook_config(path: &Path) -> Result<WebhookHttpConfig, WebhookHttpError> {
    if !path.is_absolute() {
        return Err(WebhookHttpError::InvalidConfig);
    }
    let bytes = load_private_file(path, MAX_CONFIG_BYTES)?;
    let raw: RawWebhookConfig =
        serde_json::from_slice(&bytes).map_err(|_| WebhookHttpError::InvalidConfig)?;
    if raw.version != 1 || raw.bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(WebhookHttpError::InvalidConfig);
    }
    let [endpoint] = <[RawEndpointConfig; 1]>::try_from(raw.endpoints)
        .map_err(|_| WebhookHttpError::InvalidConfig)?;
    if !valid_endpoint_id(&endpoint.id)
        || endpoint.wire_profile != SUPPORTED_WIRE_PROFILE
        || !endpoint.secret_file.is_absolute()
    {
        return Err(WebhookHttpError::InvalidConfig);
    }
    let configured_metadata = std::fs::symlink_metadata(&endpoint.secret_file)
        .map_err(|_| WebhookHttpError::SecretOpen)?;
    if configured_metadata.file_type().is_symlink() {
        return Err(WebhookHttpError::SecretFileType);
    }
    let canonical = endpoint
        .secret_file
        .canonicalize()
        .map_err(|_| WebhookHttpError::SecretOpen)?;
    let secret = load_webhook_secret(&canonical)?;
    Ok(WebhookHttpConfig {
        bind: raw.bind,
        endpoint: WebhookEndpoint {
            id: endpoint.id,
            secret: Arc::new(secret),
            project_id: endpoint.project_id,
            orchestrator_agent_id: endpoint.orchestrator_agent_id,
        },
    })
}

/// Build the dynamic router for tests or an already-bound supervised listener.
pub async fn webhook_router(
    daemon: DaemonState,
    config: WebhookHttpConfig,
    metrics: Arc<WebhookHttpMetrics>,
) -> Result<Router, WebhookHttpError> {
    let project_id = config.endpoint.project_id.clone();
    let orchestrator_agent_id = config.endpoint.orchestrator_agent_id.clone();
    daemon
        .with_store(move |store| {
            store.webhook_snapshot(&project_id, &orchestrator_agent_id, unix_time_ms())
        })
        .await
        .map_err(|_| WebhookHttpError::InvalidConfig)?;
    let state = HttpState {
        daemon,
        endpoint: Arc::new(config.endpoint),
        limiter: Arc::new(Mutex::new(RateLimits::default())),
        metrics,
    };
    Ok(Router::new()
        .route("/{endpoint}", get(poll_task).post(create_task))
        .route("/{endpoint}/snapshot", get(read_snapshot))
        .route("/{endpoint}/answer", post(answer_question))
        .route("/{endpoint}/document", post(read_document))
        .fallback(unknown_route)
        .method_not_allowed_fallback(method_not_allowed)
        .with_state(state))
}

/// Load the configuration, enforce loopback binding, and bind before the daemon
/// advertises readiness. The returned server is then supervised by `main`.
pub async fn bind_webhooks(
    daemon: DaemonState,
    config: WebhookHttpConfig,
    metrics: Arc<WebhookHttpMetrics>,
) -> Result<WebhookServer, WebhookHttpError> {
    if config.bind.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
        return Err(WebhookHttpError::NonLoopbackBind);
    }
    let bind = config.bind;
    let router = webhook_router(daemon, config, Arc::clone(&metrics)).await?;
    let listener = TcpListener::bind(bind)
        .await
        .map_err(|_| WebhookHttpError::Bind)?;
    let local = listener.local_addr().map_err(|_| WebhookHttpError::Bind)?;
    let ready_total = metrics.listener_ready.fetch_add(1, Ordering::Relaxed) + 1;
    tracing::info!(
        target: "factoryd.webhook",
        event = "listener_ready",
        transport = "loopback",
        port = local.port(),
        listener_ready_total = ready_total
    );
    Ok(WebhookServer {
        listener,
        router,
        local_addr: local,
        metrics,
    })
}

fn rate_limited(state: &HttpState, route: Route) -> Option<Response> {
    let allowed = state
        .limiter
        .lock()
        .map(|mut limiter| limiter.allow(Instant::now()))
        .unwrap_or(false);
    if allowed {
        return None;
    }
    state
        .metrics
        .rate_limited_requests
        .fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        target: "factoryd.webhook",
        event = "request_rejected",
        route = route.name(),
        outcome = "rate_limited",
        status = 429_u16
    );
    Some(json_response(
        StatusCode::TOO_MANY_REQUESTS,
        json!({ "ok": false, "error": "rate limited" }),
    ))
}

fn endpoint(state: &HttpState, id: &str) -> Option<RequestEndpoint> {
    (state.endpoint.id == id).then(|| RequestEndpoint::from(state.endpoint.as_ref()))
}

fn authenticate(endpoint: &RequestEndpoint, headers: &HeaderMap) -> bool {
    headers
        .get(LEGACY_SECRET_HEADER)
        .is_some_and(|provided| endpoint.secret.matches(provided.as_bytes()))
}

fn unauthorized() -> Response {
    json_response(
        StatusCode::UNAUTHORIZED,
        json!({ "ok": false, "error": "unauthorized" }),
    )
}

async fn bounded_json(request: Request<Body>, state: &HttpState) -> Result<Value, Response> {
    let bytes = to_bytes(request.into_body(), MAX_BODY_BYTES)
        .await
        .map_err(|_| {
            state
                .metrics
                .bounded_rejections
                .fetch_add(1, Ordering::Relaxed);
            json_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({ "ok": false, "error": "too large" }),
            )
        })?;
    serde_json::from_slice(&bytes).map_err(|_| {
        json_response(
            StatusCode::BAD_REQUEST,
            json!({ "ok": false, "error": "bad json" }),
        )
    })
}

fn json_response(status: StatusCode, value: Value) -> Response {
    (status, Json(value)).into_response()
}

fn validate_create(value: Value) -> Option<ValidatedCreate> {
    let input: CreateInput = serde_json::from_value(value).ok()?;
    let message = input.message.trim();
    if message.is_empty()
        || message.chars().count() > MAX_MESSAGE_CHARS
        || message.len() > MAX_MESSAGE_CHARS
    {
        return None;
    }
    let requested_title = input.title.as_deref().unwrap_or_default().trim();
    if requested_title.chars().count() > MAX_TITLE_CHARS {
        return None;
    }
    if input
        .from
        .as_deref()
        .is_some_and(|value| value.trim().chars().count() > MAX_FROM_CHARS)
    {
        return None;
    }
    let _kind = input.kind;
    let title_source = if requested_title.is_empty() {
        message
    } else {
        requested_title
    };
    let title = bounded_title(title_source);
    Some(ValidatedCreate {
        message: message.to_owned(),
        title,
    })
}

fn validate_answer(value: Value) -> Option<ValidatedAnswer> {
    let input: AnswerInput = serde_json::from_value(value).ok()?;
    let task_id = TaskId::try_from(input.task_id.trim()).ok()?;
    let answer = input.answer.trim();
    if answer.is_empty()
        || answer.chars().count() > MAX_MESSAGE_CHARS
        || answer.len() > MAX_MESSAGE_CHARS
    {
        return None;
    }
    Some(ValidatedAnswer {
        task_id,
        answer: answer.to_owned(),
    })
}

fn bounded_title(value: &str) -> String {
    if value.chars().count() <= 80 && value.len() <= MAX_STORED_TITLE_BYTES {
        return value.to_owned();
    }
    const ELLIPSIS: char = '…';
    let ellipsis_bytes = ELLIPSIS.len_utf8();
    let mut title = String::with_capacity(MAX_STORED_TITLE_BYTES);
    for character in value.chars().take(79) {
        if title.len() + character.len_utf8() + ellipsis_bytes > MAX_STORED_TITLE_BYTES {
            break;
        }
        title.push(character);
    }
    title.push(ELLIPSIS);
    title
}

fn validate_document(value: Value) -> Option<ValidatedDocument> {
    let input: DocumentInput = serde_json::from_value(value).ok()?;
    let task_id = TaskId::try_from(input.task_id.trim()).ok()?;
    let document_id = input.document_id.trim();
    if document_id.len() != 24
        || !document_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return None;
    }
    Some(ValidatedDocument {
        task_id,
        document_id: document_id.to_owned(),
    })
}

fn random_hex<const N: usize>() -> Result<String, getrandom::Error> {
    let mut bytes = [0_u8; N];
    getrandom::fill(&mut bytes)?;
    let mut encoded = String::with_capacity(N * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    Ok(encoded)
}

fn token_hash(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn unix_time_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
        })
}

fn task_status_name(status: OperationalTaskStatus) -> &'static str {
    match status {
        OperationalTaskStatus::Todo => "todo",
        OperationalTaskStatus::Doing => "doing",
        OperationalTaskStatus::Blocked => "blocked",
        OperationalTaskStatus::Done => "done",
    }
}

fn serialized_label<T: Serialize>(value: T) -> String {
    match serde_json::to_value(value) {
        Ok(Value::String(value)) => value,
        _ => "unknown".to_owned(),
    }
}

fn provider_label<T: Serialize>(value: Option<T>) -> Option<String> {
    value
        .map(serialized_label)
        .map(|value| match value.as_str() {
            "claude_code" => "claude".to_owned(),
            _ => value,
        })
}

fn operational_snapshot(snapshot: WebhookSnapshot) -> OperationalSnapshot {
    OperationalSnapshot {
        generated_at: rfc3339_millis(snapshot.generated_at_ms),
        counts: OperationalCounts {
            todo: u64::try_from(snapshot.counts.todo).unwrap_or(0),
            doing: u64::try_from(snapshot.counts.doing).unwrap_or(0),
            blocked: u64::try_from(snapshot.counts.blocked).unwrap_or(0),
            done: u64::try_from(snapshot.counts.done).unwrap_or(0),
        },
        tasks: snapshot.tasks.into_iter().map(operational_task).collect(),
        agents: snapshot.agents.into_iter().map(operational_agent).collect(),
    }
}

fn operational_task(task: WebhookSnapshotTask) -> OperationalTask {
    OperationalTask {
        id: task.id.to_string(),
        title: task.title,
        status: task_status_name(task.status),
        assignee: task.assignee,
        priority: task.priority,
        created_at: rfc3339_millis(task.created_at_ms),
        started_at: task.started_at_ms.map(rfc3339_millis),
        completed_at: task.completed_at_ms.map(rfc3339_millis),
        question: task.question.map(operational_question),
        result: task.result,
    }
}

fn operational_question(question: WebhookOpenQuestion) -> OperationalQuestion {
    OperationalQuestion {
        text: question.text,
        asked_at: question.asked_at_ms.map(rfc3339_millis),
        documents: question
            .documents
            .into_iter()
            .map(|document| OperationalDocumentRef {
                id: document.id,
                name: document.name,
                reference: document.reference,
            })
            .collect(),
    }
}

fn operational_agent(agent: WebhookSnapshotAgent) -> OperationalAgent {
    let breaker = match agent.observer_health {
        ObserverHealth::Degraded => "degraded".to_owned(),
        ObserverHealth::Healthy => "healthy".to_owned(),
        ObserverHealth::Unknown => "unknown".to_owned(),
    };
    OperationalAgent {
        id: agent.id.to_string(),
        name: agent.name,
        role: agent.role,
        provider: provider_label(agent.provider),
        is_orchestrator: agent.is_orchestrator,
        is_god: Some(agent.is_orchestrator),
        breaker: Some(breaker),
        tokens: Some(0),
        usd: Some(0.0),
        last_active_sec_ago: agent
            .last_active_sec_ago
            .and_then(|seconds| u64::try_from(seconds).ok()),
        inbox_backlog: u64::try_from(agent.inbox_backlog).unwrap_or(0),
    }
}

fn poll_json(poll: WebhookTaskPoll) -> Value {
    json!({
        "ok": true,
        "status": task_status_name(poll.status),
        "title": poll.title,
        "result": poll.result
    })
}

fn document_json(document: WebhookDocument) -> Value {
    json!({
        "ok": true,
        "document": {
            "id": document.id,
            "name": document.name,
            "reference": document.reference,
            "revision": document.revision,
            "content": document.content
        }
    })
}

fn valid_document(document: &WebhookDocument) -> bool {
    document.id.len() == 24
        && document
            .id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && document.revision.len() == 12
        && document
            .revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && document.reference.to_ascii_lowercase().ends_with(".md")
        && document.content.len() <= MAX_DOCUMENT_BYTES
}

fn rfc3339_millis(timestamp_ms: i64) -> String {
    let seconds = timestamp_ms.div_euclid(1_000);
    let milliseconds = timestamp_ms.rem_euclid(1_000);
    let days = seconds.div_euclid(86_400);
    let seconds_in_day = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_date_from_unix_days(days);
    let hour = seconds_in_day / 3_600;
    let minute = seconds_in_day % 3_600 / 60;
    let second = seconds_in_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}.{milliseconds:03}Z")
}

// Gregorian conversion derived from the public-domain civil calendar
// algorithm by Howard Hinnant. It avoids a heavyweight time dependency on this
// small compatibility surface.
fn civil_date_from_unix_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn latency_category(elapsed: Duration) -> &'static str {
    if elapsed < Duration::from_millis(25) {
        "fast"
    } else if elapsed < Duration::from_millis(250) {
        "normal"
    } else {
        "slow"
    }
}

fn authenticated_response(
    state: &HttpState,
    route: Route,
    started: Instant,
    status: StatusCode,
    outcome: &'static str,
    value: Value,
) -> Response {
    record_authenticated(state, route, started, status, outcome);
    json_response(status, value)
}

fn record_authenticated(
    state: &HttpState,
    route: Route,
    started: Instant,
    status: StatusCode,
    outcome: &'static str,
) {
    state
        .metrics
        .authenticated_requests
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        target: "factoryd.webhook",
        event = "authenticated_request",
        route = route.name(),
        outcome,
        status = status.as_u16(),
        latency_category = latency_category(started.elapsed())
    );
}

// Handler bodies are kept below the transport/security helpers so their store
// calls remain a small, auditable compatibility seam.
async fn create_task(
    State(state): State<HttpState>,
    AxumPath(endpoint_id): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let route = Route::Create;
    if let Some(response) = rate_limited(&state, route) {
        return response;
    }
    let started = Instant::now();
    let Some(endpoint) = endpoint(&state, &endpoint_id) else {
        return unauthorized();
    };
    if !authenticate(&endpoint, request.headers()) {
        return unauthorized();
    }
    let value = match bounded_json(request, &state).await {
        Ok(value) => value,
        Err(response) => return record_body_rejection(&state, route, started, response),
    };
    let Some(input) = validate_create(value) else {
        return authenticated_response(
            &state,
            route,
            started,
            StatusCode::BAD_REQUEST,
            "invalid_input",
            json!({ "ok": false, "error": "message required" }),
        );
    };
    let (token, task_id) = match (random_hex::<24>(), random_hex::<8>()) {
        (Ok(token), Ok(task_suffix)) => {
            let task_id = TaskId::try_from(format!("webhook-{task_suffix}"));
            match task_id {
                Ok(task_id) => (token, task_id),
                Err(_) => {
                    return internal_error(
                        &state,
                        route,
                        started,
                        "create_failed",
                        "could not create task",
                    );
                }
            }
        }
        _ => {
            return internal_error(
                &state,
                route,
                started,
                "create_failed",
                "could not create task",
            );
        }
    };
    let task = NewWebhookTask {
        id: task_id,
        project_id: endpoint.project_id,
        orchestrator_agent_id: endpoint.orchestrator_agent_id,
        endpoint_id: endpoint.id,
        title: input.title,
        body: input.message,
        token_sha256: token_hash(&token),
        created_at_ms: unix_time_ms(),
    };
    let created = match state
        .daemon
        .commit_and_publish(move |store| store.create_webhook_task(task))
        .await
    {
        Ok(created) => created,
        Err(_) => {
            return internal_error(
                &state,
                route,
                started,
                "create_failed",
                "could not create task",
            );
        }
    };
    authenticated_response(
        &state,
        route,
        started,
        StatusCode::OK,
        "created",
        json!({
            "ok": true,
            "token": token,
            "taskId": created.task_id
        }),
    )
}

async fn poll_task(
    State(state): State<HttpState>,
    AxumPath(endpoint_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let route = Route::Poll;
    if let Some(response) = rate_limited(&state, route) {
        return response;
    }
    let Some(endpoint) = endpoint(&state, &endpoint_id) else {
        return json_response(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "not found" }),
        );
    };
    let Some(token) = headers
        .get(LEGACY_TOKEN_HEADER)
        .and_then(|header| header.to_str().ok())
        .map(str::trim)
        .filter(|token| !token.is_empty())
    else {
        return json_response(
            StatusCode::UNAUTHORIZED,
            json!({ "ok": false, "error": "token required" }),
        );
    };
    let started = Instant::now();
    let hash = token_hash(token);
    let project_id = endpoint.project_id;
    let endpoint_id = endpoint.id;
    let poll = state
        .daemon
        .with_store(move |store| store.poll_webhook_task(&endpoint_id, &project_id, &hash))
        .await;
    match poll {
        Ok(Some(poll)) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::OK,
            "found",
            poll_json(poll),
        ),
        Ok(None) => json_response(
            StatusCode::NOT_FOUND,
            json!({ "ok": false, "error": "not found" }),
        ),
        Err(_) => json_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({ "ok": false, "error": "lookup failed" }),
        ),
    }
}

async fn read_snapshot(
    State(state): State<HttpState>,
    AxumPath(endpoint_id): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let route = Route::Snapshot;
    if let Some(response) = rate_limited(&state, route) {
        return response;
    }
    let started = Instant::now();
    let Some(endpoint) = endpoint(&state, &endpoint_id) else {
        return unauthorized();
    };
    if !authenticate(&endpoint, &headers) {
        return unauthorized();
    }
    let project_id = endpoint.project_id;
    let orchestrator_agent_id = endpoint.orchestrator_agent_id;
    let now_ms = unix_time_ms();
    let snapshot = state
        .daemon
        .with_store(move |store| {
            store.webhook_snapshot(&project_id, &orchestrator_agent_id, now_ms)
        })
        .await;
    match snapshot {
        Ok(snapshot) => {
            let response = json!({ "ok": true, "snapshot": operational_snapshot(snapshot) });
            if serde_json::to_vec(&response).is_ok_and(|encoded| encoded.len() <= MAX_BODY_BYTES) {
                authenticated_response(&state, route, started, StatusCode::OK, "read", response)
            } else {
                state
                    .metrics
                    .bounded_rejections
                    .fetch_add(1, Ordering::Relaxed);
                authenticated_response(
                    &state,
                    route,
                    started,
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "snapshot_too_large",
                    json!({ "ok": false, "error": "snapshot unavailable" }),
                )
            }
        }
        Err(_) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            json!({ "ok": false, "error": "snapshot unavailable" }),
        ),
    }
}

async fn answer_question(
    State(state): State<HttpState>,
    AxumPath(endpoint_id): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let route = Route::Answer;
    if let Some(response) = rate_limited(&state, route) {
        return response;
    }
    let started = Instant::now();
    let Some(endpoint) = endpoint(&state, &endpoint_id) else {
        return unauthorized();
    };
    if !authenticate(&endpoint, request.headers()) {
        return unauthorized();
    }
    let value = match bounded_json(request, &state).await {
        Ok(value) => value,
        Err(response) => return record_body_rejection(&state, route, started, response),
    };
    let Some(input) = validate_answer(value) else {
        return authenticated_response(
            &state,
            route,
            started,
            StatusCode::BAD_REQUEST,
            "invalid_input",
            json!({ "ok": false, "error": "valid taskId and answer required" }),
        );
    };
    let notification_task_id = match random_hex::<8>()
        .ok()
        .and_then(|suffix| TaskId::try_from(format!("webhook-answer-{suffix}")).ok())
    {
        Some(task_id) => task_id,
        None => {
            return internal_error(
                &state,
                route,
                started,
                "answer_failed",
                "answer could not be recorded",
            );
        }
    };
    let answer = WebhookAnswer {
        project_id: endpoint.project_id,
        task_id: input.task_id,
        answer: input.answer,
        orchestrator_agent_id: endpoint.orchestrator_agent_id,
        notification_task_id,
        answered_at_ms: unix_time_ms(),
    };
    let result = state
        .daemon
        .commit_and_publish(move |store| {
            let mutation = store.answer_webhook_question(answer)?;
            Ok((mutation.open_question_remains, mutation.events))
        })
        .await;
    match result {
        Ok(open_question_remains) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::OK,
            if open_question_remains {
                "answered_more_open"
            } else {
                "answered"
            },
            json!({ "ok": true }),
        ),
        Err(DaemonStateError::Store(StoreError::WebhookTaskNotFound)) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::NOT_FOUND,
            "task_not_found",
            json!({ "ok": false, "error": "not found" }),
        ),
        Err(DaemonStateError::Store(StoreError::WebhookQuestionNotOpen)) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::CONFLICT,
            "question_not_open",
            json!({ "ok": false, "error": "question is not open" }),
        ),
        Err(_) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "answer_failed",
            json!({ "ok": false, "error": "answer could not be recorded" }),
        ),
    }
}

async fn read_document(
    State(state): State<HttpState>,
    AxumPath(endpoint_id): AxumPath<String>,
    request: Request<Body>,
) -> Response {
    let route = Route::Document;
    if let Some(response) = rate_limited(&state, route) {
        return response;
    }
    let started = Instant::now();
    let Some(endpoint) = endpoint(&state, &endpoint_id) else {
        return unauthorized();
    };
    if !authenticate(&endpoint, request.headers()) {
        return unauthorized();
    }
    let value = match bounded_json(request, &state).await {
        Ok(value) => value,
        Err(response) => return record_body_rejection(&state, route, started, response),
    };
    let Some(input) = validate_document(value) else {
        return authenticated_response(
            &state,
            route,
            started,
            StatusCode::BAD_REQUEST,
            "invalid_input",
            json!({ "ok": false, "error": "valid taskId and documentId required" }),
        );
    };
    let project_id = endpoint.project_id;
    let document = state
        .daemon
        .with_store(move |store| {
            store.webhook_document(&project_id, &input.task_id, &input.document_id)
        })
        .await;
    match document {
        Ok(Some(document)) if valid_document(&document) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::OK,
            "found",
            document_json(document),
        ),
        Ok(None) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::NOT_FOUND,
            "not_found",
            json!({ "ok": false, "error": "not found" }),
        ),
        Ok(Some(_)) | Err(_) => authenticated_response(
            &state,
            route,
            started,
            StatusCode::INTERNAL_SERVER_ERROR,
            "unavailable",
            json!({ "ok": false, "error": "document could not be read" }),
        ),
    }
}

fn record_body_rejection(
    state: &HttpState,
    route: Route,
    started: Instant,
    response: Response,
) -> Response {
    let status = response.status();
    let outcome = if status == StatusCode::PAYLOAD_TOO_LARGE {
        "too_large"
    } else {
        "bad_json"
    };
    record_authenticated(state, route, started, status, outcome);
    response
}

fn internal_error(
    state: &HttpState,
    route: Route,
    started: Instant,
    outcome: &'static str,
    error: &'static str,
) -> Response {
    authenticated_response(
        state,
        route,
        started,
        StatusCode::INTERNAL_SERVER_ERROR,
        outcome,
        json!({ "ok": false, "error": error }),
    )
}

async fn unknown_route(State(state): State<HttpState>) -> Response {
    if let Some(response) = rate_limited(&state, Route::Unknown) {
        return response;
    }
    json_response(
        StatusCode::NOT_FOUND,
        json!({ "ok": false, "error": "not found" }),
    )
}

async fn method_not_allowed(State(state): State<HttpState>) -> Response {
    if let Some(response) = rate_limited(&state, Route::Unknown) {
        return response;
    }
    StatusCode::METHOD_NOT_ALLOWED.into_response()
}
