#![cfg(feature = "development-sqlite")]

use std::path::Path;

use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{
    BrokerState, app,
    maintainer::{SecretRevision, WebhookSecret, verify_signature},
};
use rusqlite::Connection;
use tempfile::TempDir;
use tower::ServiceExt as _;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

#[test]
fn hmac_matches_githubs_documented_vector() {
    assert!(verify_signature(
        b"It's a Secret to Everybody",
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
        b"Hello, World!",
    ));
    assert!(!verify_signature(
        b"It's a Secret to Everybody",
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e10",
        b"Hello, World!",
    ));
}

#[tokio::test]
async fn liveness_is_separate_from_fixed_inactive_readiness() {
    let (router, _temporary, _database) = test_router();
    let response = router
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(body.as_ref(), br#"{"status":"ok"}"#);

    let response = router
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    assert_eq!(
        body.as_ref(),
        br#"{"status":"inactive","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#
    );
}

#[tokio::test]
async fn exact_signed_ping_is_durable_and_idempotent_across_restart() {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let body = br#"{"zen":"Approachable is better than simple."}"#;
    let delivery = "0c8a5c44-7f1f-11f0-952e-acde48001122";

    let first = app(state(&database, "maintainer-v1"))
        .oneshot(signed_request("ping", delivery, body))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(delivery_count(&database), 1);

    let replay = app(state(&database, "maintainer-v1"))
        .oneshot(signed_request("ping", delivery, body))
        .await
        .unwrap();
    assert_eq!(replay.status(), StatusCode::OK);
    assert_eq!(delivery_count(&database), 1);

    let conflict = app(state(&database, "maintainer-v1"))
        .oneshot(signed_request("ping", delivery, br#"{"zen":"different"}"#))
        .await
        .unwrap();
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert_eq!(delivery_count(&database), 1);
}

#[tokio::test]
async fn schema_drift_fails_closed_before_acknowledgement() {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let router = app(state(&database, "maintainer-v1"));
    Connection::open(&database)
        .unwrap()
        .execute(
            "ALTER TABLE maintainer_deliveries ADD COLUMN unreviewed TEXT",
            [],
        )
        .unwrap();

    let response = router
        .oneshot(signed_request(
            "ping",
            "0d8a5c44-7f1f-11f0-952e-acde48001122",
            br#"{"zen":"schema first"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(delivery_count(&database), 0);
}

#[tokio::test]
async fn concurrent_exact_deliveries_collapse_to_one_row() {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let router = app(state(&database, "maintainer-v1"));
    let body = br#"{"zen":"concurrent replay"}"#;
    let delivery = "9c8a5c44-7f1f-11f0-952e-acde48001122";
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let router = router.clone();
        let request = signed_request("ping", delivery, body);
        tasks.push(tokio::spawn(async move {
            router.oneshot(request).await.unwrap().status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
    assert_eq!(delivery_count(&database), 1);
}

#[tokio::test]
async fn concurrent_conflicting_delivery_has_one_winner() {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let router = app(state(&database, "maintainer-v1"));
    let delivery = "ac8a5c44-7f1f-11f0-952e-acde48001122";
    let first = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .oneshot(signed_request("ping", delivery, br#"{"zen":"first"}"#))
                .await
                .unwrap()
                .status()
        }
    });
    let second = tokio::spawn(async move {
        router
            .oneshot(signed_request("ping", delivery, br#"{"zen":"second"}"#))
            .await
            .unwrap()
            .status()
    });
    let mut statuses = [first.await.unwrap(), second.await.unwrap()];
    statuses.sort_unstable();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
    assert_eq!(delivery_count(&database), 1);
}

#[tokio::test]
async fn replay_binds_hook_target_and_secret_revision() {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let body = br#"{"zen":"bind every delivery field"}"#;
    let delivery = "5c8a5c44-7f1f-11f0-952e-acde48001122";

    let first = app(state(&database, "maintainer-v1"))
        .oneshot(signed_request("ping", delivery, body))
        .await
        .unwrap();
    assert_eq!(first.status(), StatusCode::OK);

    let changed_hook = app(state(&database, "maintainer-v1"))
        .oneshot(signed_request_with_bindings(
            "ping", delivery, "9999", "5678", body,
        ))
        .await
        .unwrap();
    assert_eq!(changed_hook.status(), StatusCode::CONFLICT);

    let changed_target = app(state_with_target(&database, "maintainer-v1", 9999))
        .oneshot(signed_request_with_bindings(
            "ping", delivery, "1234", "9999", body,
        ))
        .await
        .unwrap();
    assert_eq!(changed_target.status(), StatusCode::CONFLICT);

    let changed_revision = app(state(&database, "maintainer-v2"))
        .oneshot(signed_request("ping", delivery, body))
        .await
        .unwrap();
    assert_eq!(changed_revision.status(), StatusCode::CONFLICT);
    assert_eq!(delivery_count(&database), 1);
}

#[tokio::test]
async fn bad_signature_never_reaches_the_journal() {
    let (router, _temporary, database) = test_router();
    let request = Request::post("/v1/github/maintainer/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", "ping")
        .header("x-github-delivery", "0c8a5c44-7f1f-11f0-952e-acde48001122")
        .header("x-github-hook-id", "1234")
        .header("x-github-hook-installation-target-id", "5678")
        .header("x-github-hook-installation-target-type", "integration")
        .header("x-hub-signature-256", format!("sha256={}", "0".repeat(64)))
        .body(Body::from(r#"{"zen":"untrusted"}"#))
        .unwrap();

    let response = router.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(delivery_count(&database), 0);
}

#[tokio::test]
async fn signed_installation_lifecycle_is_policy_rejected() {
    let (router, _temporary, database) = test_router();
    let response = router
        .oneshot(signed_request(
            "installation",
            "1c8a5c44-7f1f-11f0-952e-acde48001122",
            br#"{"action":"created"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(delivery_count(&database), 1);
    assert_eq!(stored_disposition(&database), "policy_rejected");
}

#[tokio::test]
async fn unknown_events_and_lifecycle_actions_fail_closed() {
    let (router, _temporary, database) = test_router();
    let event = router
        .clone()
        .oneshot(signed_request(
            "issues",
            "2c8a5c44-7f1f-11f0-952e-acde48001122",
            br#"{"action":"opened"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(event.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let action = router
        .oneshot(signed_request(
            "installation",
            "3c8a5c44-7f1f-11f0-952e-acde48001122",
            br#"{"action":"invented"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(action.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(delivery_count(&database), 2);
}

#[tokio::test]
async fn malformed_or_oversized_requests_are_rejected_before_journaling() {
    let (router, _temporary, database) = test_router();
    let malformed_delivery = Request::post("/v1/github/maintainer/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", "ping")
        .header("x-github-delivery", "../../not-a-delivery")
        .header("x-hub-signature-256", signature(SECRET, b"{}"))
        .body(Body::from("{}"))
        .unwrap();
    let response = router.clone().oneshot(malformed_delivery).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let oversized = vec![b'x'; 65 * 1024];
    let response = router
        .oneshot(signed_request(
            "ping",
            "4c8a5c44-7f1f-11f0-952e-acde48001122",
            &oversized,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(delivery_count(&database), 0);
}

#[tokio::test]
async fn arbitrary_target_type_and_wrong_configured_target_fail_closed() {
    let (router, _temporary, database) = test_router();
    let body = br#"{"zen":"scope first"}"#;
    let mut wrong_type = signed_request("ping", "6c8a5c44-7f1f-11f0-952e-acde48001122", body);
    wrong_type.headers_mut().insert(
        "x-github-hook-installation-target-type",
        "organization".parse().unwrap(),
    );
    let response = router.clone().oneshot(wrong_type).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let wrong_target = signed_request_with_bindings(
        "ping",
        "7c8a5c44-7f1f-11f0-952e-acde48001122",
        "1234",
        "9999",
        body,
    );
    let response = router.oneshot(wrong_target).await.unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert_eq!(delivery_count(&database), 0);

    let too_large_for_worker_sqlite = signed_request_with_bindings(
        "ping",
        "7d8a5c44-7f1f-11f0-952e-acde48001122",
        "9007199254740992",
        "5678",
        body,
    );
    let response = app(state(&database, "maintainer-v1"))
        .oneshot(too_large_for_worker_sqlite)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(delivery_count(&database), 0);
}

#[tokio::test]
async fn duplicate_security_headers_are_rejected_before_journaling() {
    let (router, _temporary, database) = test_router();
    let body = br#"{"zen":"one value per binding"}"#;
    for name in [
        "x-hub-signature-256",
        "x-github-delivery",
        "x-github-event",
        "x-github-hook-id",
        "x-github-hook-installation-target-id",
        "x-github-hook-installation-target-type",
    ] {
        let mut request = signed_request("ping", "8c8a5c44-7f1f-11f0-952e-acde48001122", body);
        let duplicate = request.headers().get(name).unwrap().clone();
        request.headers_mut().append(name, duplicate);
        let response = router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{name}");
    }
    assert_eq!(delivery_count(&database), 0);
}

#[tokio::test]
async fn future_product_and_operator_routes_are_not_exposed() {
    let (router, _temporary, _database) = test_router();
    for path in ["/v1/github/product/webhook", "/v1/operator/needs-you"] {
        let response = router
            .clone()
            .oneshot(Request::post(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}

fn test_router() -> (Router, TempDir, std::path::PathBuf) {
    let temporary = TempDir::new().unwrap();
    let database = temporary.path().join("broker.sqlite3");
    let router = app(state(&database, "maintainer-v1"));
    (router, temporary, database)
}

fn state(database: &Path, secret_revision: &str) -> BrokerState {
    state_with_target(database, secret_revision, 5678)
}

fn state_with_target(database: &Path, secret_revision: &str, target_id: i64) -> BrokerState {
    BrokerState::open_development(
        database,
        WebhookSecret::new(SECRET.to_vec()).expect("32-byte secret"),
        SecretRevision::new(secret_revision.to_owned()).expect("valid secret revision"),
        target_id,
    )
    .unwrap()
}

fn signed_request(event: &str, delivery: &str, body: &[u8]) -> Request<Body> {
    signed_request_with_bindings(event, delivery, "1234", "5678", body)
}

fn signed_request_with_bindings(
    event: &str,
    delivery: &str,
    hook_id: &str,
    target_id: &str,
    body: &[u8],
) -> Request<Body> {
    Request::post("/v1/github/maintainer/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header("x-github-hook-id", hook_id)
        .header("x-github-hook-installation-target-id", target_id)
        .header("x-github-hook-installation-target-type", "integration")
        .header("x-hub-signature-256", signature(SECRET, body))
        .body(Body::from(body.to_vec()))
        .unwrap()
}

fn signature(secret: &[u8], body: &[u8]) -> String {
    format!("sha256={}", hex::encode(reference_hmac(secret, body)))
}

fn reference_hmac(secret: &[u8], body: &[u8]) -> [u8; 32] {
    use sha2::{Digest as _, Sha256};

    let mut key = [0_u8; 64];
    if secret.len() > key.len() {
        key[..32].copy_from_slice(&Sha256::digest(secret));
    } else {
        key[..secret.len()].copy_from_slice(secret);
    }
    let mut inner_key = key;
    let mut outer_key = key;
    for byte in &mut inner_key {
        *byte ^= 0x36;
    }
    for byte in &mut outer_key {
        *byte ^= 0x5c;
    }
    let inner = Sha256::new()
        .chain_update(inner_key)
        .chain_update(body)
        .finalize();
    Sha256::new()
        .chain_update(outer_key)
        .chain_update(inner)
        .finalize()
        .into()
}

fn delivery_count(database: &Path) -> i64 {
    Connection::open(database)
        .unwrap()
        .query_row("SELECT COUNT(*) FROM maintainer_deliveries", [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn stored_disposition(database: &Path) -> String {
    Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT disposition FROM maintainer_deliveries LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
}
