//! HTTP smoke test: the control router answers with default config, no port binding
//! needed (`tower::ServiceExt::oneshot`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use tower::ServiceExt;

use simplehar_se::api::{router, ApiState};
use simplehar_se::config::AppConfig;

#[tokio::test]
async fn health_and_status_answer_on_default_config() {
    let state = Arc::new(ApiState::from(AppConfig::default()));
    let app = router(state);

    let response = app
        .clone()
        .oneshot(
            Request::get("/health")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::OK);

    let response = app
        .oneshot(
            Request::get("/har/status")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bounded body");
    let status: serde_json::Value = serde_json::from_slice(&bytes).expect("status is JSON");
    assert_eq!(status["categories"].as_array().map(Vec::len), Some(4));
    assert_eq!(status["control_addr"], "127.0.0.1:8109");
}

#[tokio::test]
async fn unknown_route_is_typed_not_found() {
    let state = Arc::new(ApiState::from(AppConfig::default()));
    let response = router(state)
        .oneshot(
            Request::get("/har/nope")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
