use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    sync::Arc,
};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use factory_core::{AgentId, AgentRole, ProjectId, Provider};
use factoryd::{
    daemon_state::DaemonState,
    store::{NewAgent, NewProject, Store},
    webhook_http::{WebhookHttpMetrics, load_webhook_config, webhook_router},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

const LEGACY_SECRET: &str = "private-legacy-secret-sentinel";

fn project_id(value: &str) -> ProjectId {
    ProjectId::try_from(value).unwrap()
}

fn agent_id(value: &str) -> AgentId {
    AgentId::try_from(value).unwrap()
}

fn private_write(path: &std::path::Path, contents: impl AsRef<[u8]>) {
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn generic_config(secret_file: &std::path::Path) -> Value {
    json!({"version":1,"bind":"127.0.0.1:0","endpoints":[{
        "id":"monitor","wireProfile":"generic_v1","secretFile":secret_file
    }]})
}

fn signature(secret: &[u8], body: &[u8]) -> String {
    let mut key = [0_u8; 64];
    key[..secret.len()].copy_from_slice(secret);
    let mut inner = key;
    let mut outer = key;
    for byte in &mut inner {
        *byte ^= 0x36;
    }
    for byte in &mut outer {
        *byte ^= 0x5c;
    }
    let digest = Sha256::new()
        .chain_update(inner)
        .chain_update(body)
        .finalize();
    let digest = Sha256::new()
        .chain_update(outer)
        .chain_update(digest)
        .finalize();
    format!("sha256={digest:x}")
}

#[tokio::test]
async fn generic_events_authenticate_are_idempotent_and_reject_unknown_targets() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let secret_file = directory.path().join("secret");
    private_write(&secret_file, b"generic-secret");
    let config_path = directory.path().join("webhooks.json");
    private_write(
        &config_path,
        serde_json::to_vec(&generic_config(&secret_file)).unwrap(),
    );
    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/tmp/factory".into(),
            },
            1,
        )
        .unwrap();
    let router = webhook_router(
        DaemonState::new(store),
        load_webhook_config(&config_path).unwrap(),
        Arc::new(WebhookHttpMetrics::default()),
    )
    .await
    .unwrap();
    let body = serde_json::to_vec(&json!({"version":1,"eventId":"evt-1","projectId":"factory","type":"task","data":{"title":"Imported","body":"Do it"}})).unwrap();
    let call = |signature_value: String| {
        Request::post("/monitor/events")
            .header("x-dark-factory-signature", signature_value)
            .body(Body::from(body.clone()))
            .unwrap()
    };
    assert_eq!(
        router
            .clone()
            .oneshot(call("sha256=00".into()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = router
        .clone()
        .oneshot(call(signature(b"generic-secret", &body)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let response = router
        .clone()
        .oneshot(call(signature(b"generic-secret", &body)))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let value: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap()).unwrap();
    assert_eq!(value["status"], "duplicate");

    let oversized = vec![b'x'; 1024 * 1024 + 1];
    let request = Request::post("/monitor/events")
        .header(
            "x-dark-factory-signature",
            signature(b"generic-secret", &oversized),
        )
        .body(Body::from(oversized))
        .unwrap();
    assert_eq!(
        router.clone().oneshot(request).await.unwrap().status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );

    let unknown = serde_json::to_vec(&json!({"version":1,"eventId":"evt-2","projectId":"missing","type":"task","data":{"title":"Imported","body":"Do it"}})).unwrap();
    let request = Request::post("/monitor/events")
        .header(
            "x-dark-factory-signature",
            signature(b"generic-secret", &unknown),
        )
        .body(Body::from(unknown))
        .unwrap();
    assert_eq!(
        router.oneshot(request).await.unwrap().status(),
        StatusCode::NOT_FOUND
    );
}

#[tokio::test]
async fn configured_targets_are_resolved_per_request_not_at_startup() {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let secret_file = directory.path().join("secret");
    private_write(&secret_file, LEGACY_SECRET);
    let config_path = directory.path().join("webhooks.json");
    private_write(
        &config_path,
        serde_json::to_vec(&config_json("127.0.0.1:0", &secret_file)).unwrap(),
    );
    let config = load_webhook_config(&config_path).unwrap();
    let _router = webhook_router(
        DaemonState::new(Store::open_in_memory().unwrap()),
        config,
        Arc::new(WebhookHttpMetrics::default()),
    )
    .await
    .unwrap();
}

/// The exact shape Minerva's live `webhooks.json` uses today: one endpoint,
/// `legacy_v1`. This must keep loading unchanged.
fn config_json(bind: &str, secret_file: &std::path::Path) -> Value {
    json!({
        "version": 1,
        "bind": bind,
        "endpoints": [
            {
                "id": "minerva",
                "wireProfile": "legacy_v1",
                "secretFile": secret_file,
                "projectId": "factory",
                "orchestratorAgentId": "god"
            }
        ]
    })
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, DaemonState) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let secret_file = directory.path().join("legacy.secret");
    private_write(&secret_file, LEGACY_SECRET);
    let config_path = directory.path().join("webhooks.json");
    private_write(
        &config_path,
        serde_json::to_vec_pretty(&config_json("127.0.0.1:0", &secret_file)).unwrap(),
    );

    let mut store = Store::open_in_memory().unwrap();
    store
        .create_project(
            NewProject {
                id: project_id("factory"),
                name: "Factory".into(),
                root: "/tmp/factory".into(),
            },
            1,
        )
        .unwrap();
    store
        .create_agent(
            NewAgent {
                id: agent_id("god"),
                project_id: project_id("factory"),
                parent_agent_id: None,
                role: AgentRole::Orchestrator,
                provider: Provider::Codex,
            },
            2,
        )
        .unwrap();
    (directory, config_path, DaemonState::new(store))
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

#[tokio::test]
async fn the_minerva_endpoint_authenticates_with_legacy_headers_and_serves_the_full_flow() {
    let (_directory, config_path, state) = fixture();
    let config = load_webhook_config(&config_path).unwrap();
    let router = webhook_router(state, config, Arc::new(WebhookHttpMetrics::default()))
        .await
        .unwrap();

    let created = router
        .clone()
        .oneshot(
            Request::post("/minerva")
                .header("content-type", "application/json")
                .header("x-md-webhook-secret", LEGACY_SECRET)
                .body(Body::from(r#"{"message":"legacy compatible"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(created.status(), StatusCode::OK);
    let created = body_json(created).await;
    let token = created["token"].as_str().unwrap();

    let wrong_secret = router
        .clone()
        .oneshot(
            Request::post("/minerva")
                .header("content-type", "application/json")
                .header("x-md-webhook-secret", "not-the-real-secret")
                .body(Body::from(r#"{"message":"must not authenticate"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_secret.status(), StatusCode::UNAUTHORIZED);

    let polled = router
        .clone()
        .oneshot(
            Request::get("/minerva")
                .header("x-md-webhook-token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(polled.status(), StatusCode::OK);

    let snapshot = router
        .oneshot(
            Request::get("/minerva/snapshot")
                .header("x-md-webhook-secret", LEGACY_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(snapshot.status(), StatusCode::OK);
    let snapshot = body_json(snapshot).await;
    let agent = &snapshot["snapshot"]["agents"][0];
    assert_eq!(agent["isGod"], true);
    assert_eq!(agent["isOrchestrator"], true);
    assert!(agent["role"].is_string());
    assert!(snapshot["snapshot"].get("subscriptionUsage").is_none());
}

#[tokio::test]
async fn a_second_configured_endpoint_can_never_exhaust_the_only_endpoints_request_budget() {
    let (_directory, config_path, state) = fixture();
    let config = load_webhook_config(&config_path).unwrap();
    let router = webhook_router(state, config, Arc::new(WebhookHttpMetrics::default()))
        .await
        .unwrap();

    for _ in 0..60 {
        let response = router
            .clone()
            .oneshot(
                Request::get("/minerva/snapshot")
                    .header("x-md-webhook-secret", LEGACY_SECRET)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let exhausted = router
        .oneshot(
            Request::get("/minerva/snapshot")
                .header("x-md-webhook-secret", LEGACY_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);
}

#[test]
fn config_is_bounded_strict_private_and_non_symbolic() {
    let (directory, config_path, _state) = fixture();
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_webhook_config(&config_path).is_err());
    fs::set_permissions(&config_path, fs::Permissions::from_mode(0o600)).unwrap();

    let linked = directory.path().join("linked.json");
    symlink(&config_path, &linked).unwrap();
    assert!(load_webhook_config(&linked).is_err());

    let mut unknown = fs::read_to_string(&config_path).unwrap();
    unknown = unknown.replacen("\"bind\":", "\"typo\": true,\n  \"bind\":", 1);
    private_write(&config_path, unknown);
    assert!(load_webhook_config(&config_path).is_err());
}

#[test]
fn more_than_one_endpoint_or_a_missing_endpoint_fails_closed() {
    let (directory, config_path, _state) = fixture();
    let secret_file = directory.path().join("legacy.secret");
    let second_secret = directory.path().join("second.secret");
    private_write(&second_secret, "another-private-secret");

    let mut two_endpoints = config_json("127.0.0.1:0", &secret_file);
    two_endpoints["endpoints"]
        .as_array_mut()
        .unwrap()
        .push(json!({
            "id": "second",
            "wireProfile": "legacy_v1",
            "secretFile": second_secret,
            "projectId": "second-project",
            "orchestratorAgentId": "second-agent"
        }));
    private_write(&config_path, serde_json::to_vec(&two_endpoints).unwrap());
    assert!(load_webhook_config(&config_path).is_err());

    let mut no_endpoints = config_json("127.0.0.1:0", &secret_file);
    no_endpoints["endpoints"] = json!([]);
    private_write(&config_path, serde_json::to_vec(&no_endpoints).unwrap());
    assert!(load_webhook_config(&config_path).is_err());
}

#[test]
fn unknown_wire_profiles_are_rejected() {
    let (_directory, config_path, _state) = fixture();
    let mut unsupported = fs::read_to_string(&config_path).unwrap();
    unsupported = unsupported.replace("legacy_v1", "factory_v2");
    private_write(&config_path, unsupported);
    assert!(load_webhook_config(&config_path).is_err());
}

#[test]
fn non_loopback_bind_fails_closed() {
    let (directory, config_path, _state) = fixture();
    let secret_file = directory.path().join("legacy.secret");
    let non_loopback = config_json("0.0.0.0:3849", &secret_file);
    private_write(&config_path, serde_json::to_vec(&non_loopback).unwrap());
    assert!(load_webhook_config(&config_path).is_err());
}

#[test]
fn endpoint_secret_itself_cannot_be_a_symbolic_link() {
    let (directory, config_path, _state) = fixture();
    let real_secret = directory.path().join("real.secret");
    let linked_secret = directory.path().join("linked.secret");
    private_write(&real_secret, "another-private-secret");
    symlink(&real_secret, &linked_secret).unwrap();

    let config = config_json("127.0.0.1:0", &linked_secret);
    private_write(&config_path, serde_json::to_vec(&config).unwrap());
    assert!(load_webhook_config(&config_path).is_err());
}
