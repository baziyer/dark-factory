#![cfg(not(feature = "development-sqlite"))]

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use dark_factory_control_plane::{BrokerState, app};
use tower::ServiceExt as _;

#[tokio::test]
async fn default_build_exposes_only_liveness_and_inactive_readiness() {
    let router = app(BrokerState::inactive());
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

    let webhook = router
        .oneshot(
            Request::post("/v1/github/maintainer/webhook")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(webhook.status(), StatusCode::NOT_FOUND);
}
