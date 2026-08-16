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
use tower::ServiceExt;

const LEGACY_SECRET: &str = "private-legacy-secret-sentinel";
const LAB_SECRET: &str = "private-lab-secret-sentinel";
const THIRD_SECRET: &str = "private-third-secret-sentinel";

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

fn config_json(bind: &str, legacy_secret: &std::path::Path, lab_secret: &std::path::Path) -> Value {
    json!({
        "version": 1,
        "bind": bind,
        "endpoints": [
            {
                "id": "minerva",
                "wireProfile": "legacy_v1",
                "secretFile": legacy_secret,
                "projectId": "factory",
                "orchestratorAgentId": "god"
            },
            {
                "id": "lab",
                "wireProfile": "factory_v1",
                "secretFile": lab_secret,
                "projectId": "lab-project",
                "orchestratorAgentId": "foreman"
            }
        ]
    })
}

fn config_json_three(
    bind: &str,
    legacy_secret: &std::path::Path,
    lab_secret: &std::path::Path,
    third_secret: &std::path::Path,
) -> Value {
    let mut config = config_json(bind, legacy_secret, lab_secret);
    config["endpoints"].as_array_mut().unwrap().push(json!({
        "id": "third",
        "wireProfile": "factory_v1",
        "secretFile": third_secret,
        "projectId": "third-project",
        "orchestratorAgentId": "third-agent"
    }));
    config
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, DaemonState) {
    let directory = tempfile::tempdir_in("/tmp").unwrap();
    let legacy_secret = directory.path().join("legacy.secret");
    let lab_secret = directory.path().join("lab.secret");
    let third_secret = directory.path().join("third.secret");
    private_write(&legacy_secret, LEGACY_SECRET);
    private_write(&lab_secret, LAB_SECRET);
    private_write(&third_secret, THIRD_SECRET);
    let config_path = directory.path().join("webhooks.json");
    private_write(
        &config_path,
        serde_json::to_vec_pretty(&config_json_three(
            "127.0.0.1:0",
            &legacy_secret,
            &lab_secret,
            &third_secret,
        ))
        .unwrap(),
    );

    let mut store = Store::open_in_memory().unwrap();
    for (project, name, root, orchestrator) in [
        ("factory", "Factory", "/tmp/factory", "god"),
        ("lab-project", "Lab", "/tmp/lab", "foreman"),
        ("third-project", "Third", "/tmp/third", "third-agent"),
    ] {
        store
            .create_project(
                NewProject {
                    id: project_id(project),
                    name: name.into(),
                    root: root.into(),
                },
                1,
            )
            .unwrap();
        store
            .create_agent(
                NewAgent {
                    id: agent_id(orchestrator),
                    project_id: project_id(project),
                    parent_agent_id: None,
                    role: AgentRole::Orchestrator,
                    provider: Provider::Codex,
                },
                2,
            )
            .unwrap();
    }
    (directory, config_path, DaemonState::new(store))
}

async fn body_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}

#[tokio::test]
async fn one_owner_only_config_routes_two_isolated_endpoint_profiles() {
    let (_directory, config_path, state) = fixture();
    let config = load_webhook_config(&config_path).unwrap();
    let router = webhook_router(state, config, Arc::new(WebhookHttpMetrics::default()))
        .await
        .unwrap();

    let minerva = router
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
    assert_eq!(minerva.status(), StatusCode::OK);
    let minerva = body_json(minerva).await;
    let minerva_token = minerva["token"].as_str().unwrap();

    let lab = router
        .clone()
        .oneshot(
            Request::post("/lab")
                .header("content-type", "application/json")
                .header("x-dark-factory-webhook-secret", LAB_SECRET)
                .body(Body::from(r#"{"message":"generic factory"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(lab.status(), StatusCode::OK);
    let lab = body_json(lab).await;
    let lab_token = lab["token"].as_str().unwrap();

    let wrong_secret = router
        .clone()
        .oneshot(
            Request::post("/lab")
                .header("content-type", "application/json")
                .header("x-dark-factory-webhook-secret", LEGACY_SECRET)
                .body(Body::from(r#"{"message":"must not cross"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(wrong_secret.status(), StatusCode::UNAUTHORIZED);

    let cross_endpoint_token = router
        .clone()
        .oneshot(
            Request::get("/lab")
                .header("x-dark-factory-webhook-token", minerva_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cross_endpoint_token.status(), StatusCode::NOT_FOUND);

    let lab_poll = router
        .oneshot(
            Request::get("/lab")
                .header("x-dark-factory-webhook-token", lab_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(lab_poll.status(), StatusCode::OK);
}

#[tokio::test]
async fn wire_profile_controls_legacy_aliases_without_changing_subscription_usage() {
    let (_directory, config_path, state) = fixture();
    let config = load_webhook_config(&config_path).unwrap();
    let router = webhook_router(state, config, Arc::new(WebhookHttpMetrics::default()))
        .await
        .unwrap();

    let legacy = router
        .clone()
        .oneshot(
            Request::get("/minerva/snapshot")
                .header("x-md-webhook-secret", LEGACY_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(legacy.status(), StatusCode::OK);
    let legacy = body_json(legacy).await;
    let legacy_agent = &legacy["snapshot"]["agents"][0];
    assert_eq!(legacy_agent["isGod"], true);
    assert_eq!(legacy_agent["isOrchestrator"], true);
    assert!(legacy_agent["role"].is_string());
    assert!(legacy["snapshot"]["subscriptionUsage"].is_object());

    let generic = router
        .oneshot(
            Request::get("/lab/snapshot")
                .header("x-dark-factory-webhook-secret", LAB_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(generic.status(), StatusCode::OK);
    let generic = body_json(generic).await;
    let generic_agent = &generic["snapshot"]["agents"][0];
    assert_eq!(generic_agent["isOrchestrator"], true);
    assert!(generic_agent.get("isGod").is_none());
    assert!(generic_agent.get("tokens").is_none());
    assert!(generic_agent.get("usd").is_none());
    assert!(generic["snapshot"]["subscriptionUsage"].is_object());
}

#[tokio::test]
async fn one_endpoint_cannot_exhaust_another_endpoints_request_budget() {
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
        .clone()
        .oneshot(
            Request::get("/minerva/snapshot")
                .header("x-md-webhook-secret", LEGACY_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(exhausted.status(), StatusCode::TOO_MANY_REQUESTS);

    let independent = router
        .oneshot(
            Request::get("/lab/snapshot")
                .header("x-dark-factory-webhook-secret", LAB_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(independent.status(), StatusCode::OK);
}

#[tokio::test]
async fn three_configured_endpoints_keep_independent_request_budgets() {
    let (_directory, config_path, state) = fixture();
    let config = load_webhook_config(&config_path).unwrap();
    let router = webhook_router(state, config, Arc::new(WebhookHttpMetrics::default()))
        .await
        .unwrap();

    for (path, header, secret) in [
        ("/minerva/snapshot", "x-md-webhook-secret", LEGACY_SECRET),
        ("/lab/snapshot", "x-dark-factory-webhook-secret", LAB_SECRET),
    ] {
        for _ in 0..60 {
            let response = router
                .clone()
                .oneshot(
                    Request::get(path)
                        .header(header, secret)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }

    let third = router
        .oneshot(
            Request::get("/third/snapshot")
                .header("x-dark-factory-webhook-secret", THIRD_SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(third.status(), StatusCode::OK);
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
fn duplicate_ids_secrets_and_non_loopback_bind_fail_closed() {
    let (directory, config_path, _state) = fixture();
    let legacy_secret = directory.path().join("legacy.secret");
    let lab_secret = directory.path().join("lab.secret");

    let mut duplicate_id = config_json("127.0.0.1:0", &legacy_secret, &lab_secret);
    duplicate_id["endpoints"][1]["id"] = json!("minerva");
    private_write(&config_path, serde_json::to_vec(&duplicate_id).unwrap());
    assert!(load_webhook_config(&config_path).is_err());

    let duplicate_secret = config_json("127.0.0.1:0", &legacy_secret, &legacy_secret);
    private_write(&config_path, serde_json::to_vec(&duplicate_secret).unwrap());
    assert!(load_webhook_config(&config_path).is_err());

    let mut duplicate_project = config_json("127.0.0.1:0", &legacy_secret, &lab_secret);
    duplicate_project["endpoints"][1]["projectId"] = json!("factory");
    private_write(
        &config_path,
        serde_json::to_vec(&duplicate_project).unwrap(),
    );
    assert!(load_webhook_config(&config_path).is_err());

    let non_loopback = config_json("0.0.0.0:3849", &legacy_secret, &lab_secret);
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

    let legacy_secret = directory.path().join("legacy.secret");
    let config = config_json("127.0.0.1:0", &legacy_secret, &linked_secret);
    private_write(&config_path, serde_json::to_vec(&config).unwrap());
    assert!(load_webhook_config(&config_path).is_err());
}
