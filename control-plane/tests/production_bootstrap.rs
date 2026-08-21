use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{BrokerState, app};
use hmac::{Hmac, Mac as _};
use sha2::Sha256;
use tower::ServiceExt as _;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn configured_but_unavailable_postgres_never_becomes_ready_or_acknowledges() {
    let state = BrokerState::open_production(
        "postgresql://postgres:postgres@127.0.0.1:1/control_plane?sslmode=require",
        SECRET.to_vec(),
        "maintainer-v1".to_owned(),
        5678,
    )
    .unwrap();
    let router = app(state);

    let health = router
        .clone()
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(
        to_bytes(health.into_body(), 1024).await.unwrap().as_ref(),
        br#"{"status":"ok"}"#
    );

    let ready = router
        .clone()
        .oneshot(Request::get("/readyz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        to_bytes(ready.into_body(), 1024).await.unwrap().as_ref(),
        br#"{"status":"unavailable","maintainer_webhook":"inactive","product_webhook":"inactive","operator_api":"inactive"}"#
    );

    let body = br#"{"zen":"never acknowledge volatile receipt"}"#;
    let webhook = router
        .oneshot(signed_request("bc8a5c44-7f1f-11f0-952e-acde48001122", body))
        .await
        .unwrap();
    assert_eq!(webhook.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        to_bytes(webhook.into_body(), 1024).await.unwrap().as_ref(),
        br#"{"error":"journal_unavailable"}"#
    );
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
