use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{BrokerState, app, migrate_from_env};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use tower::ServiceExt as _;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

#[tokio::test]
#[ignore = "requires an explicit disposable TLS Postgres DATABASE_URL"]
async fn migrated_postgres_proves_readiness_concurrent_replay_and_conflict() {
    let database_url = std::env::var("DATABASE_URL").expect("disposable DATABASE_URL");
    let state = || {
        BrokerState::open_production(
            &database_url,
            SECRET.to_vec(),
            "maintainer-v1".to_owned(),
            5678,
        )
        .unwrap()
    };

    let before = app(state())
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(before.status(), StatusCode::SERVICE_UNAVAILABLE);

    migrate_from_env().await.unwrap();
    let router = app(state());
    let ready = router
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(ready.into_body(), 1024).await.unwrap().as_ref(),
        br#"{"status":"ready","maintainer_webhook":"bootstrap_ping_only","product_webhook":"inactive","operator_api":"inactive"}"#
    );

    exact_concurrent_replay(router.clone()).await;
    concurrent_conflict(router).await;
}

async fn exact_concurrent_replay(router: Router) {
    let body = br#"{"zen":"postgres concurrent replay"}"#;
    let delivery = "cc8a5c44-7f1f-11f0-952e-acde48001122";
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let router = router.clone();
        let request = signed_request(delivery, body);
        tasks.push(tokio::spawn(async move {
            router.oneshot(request).await.unwrap().status()
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap(), StatusCode::OK);
    }
}

async fn concurrent_conflict(router: Router) {
    let delivery = "dc8a5c44-7f1f-11f0-952e-acde48001122";
    let first = tokio::spawn({
        let router = router.clone();
        async move {
            router
                .oneshot(signed_request(delivery, br#"{"zen":"first"}"#))
                .await
                .unwrap()
                .status()
        }
    });
    let second = tokio::spawn(async move {
        router
            .oneshot(signed_request(delivery, br#"{"zen":"second"}"#))
            .await
            .unwrap()
            .status()
    });
    let mut statuses = [first.await.unwrap(), second.await.unwrap()];
    statuses.sort_unstable();
    assert_eq!(statuses, [StatusCode::OK, StatusCode::CONFLICT]);
}

fn signed_request(delivery: &str, body: &[u8]) -> Request<Body> {
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET).unwrap();
    mac.update(body);
    Request::post("/v1/github/maintainer/webhook")
        .header("content-type", "application/json")
        .header("x-github-event", "ping")
        .header("x-github-delivery", delivery)
        .header("x-github-hook-id", "1234")
        .header("x-github-hook-installation-target-id", "5678")
        .header("x-github-hook-installation-target-type", "integration")
        .header(
            "x-hub-signature-256",
            format!("sha256={}", hex::encode(mac.finalize().into_bytes())),
        )
        .body(Body::from(body.to_vec()))
        .unwrap()
}
