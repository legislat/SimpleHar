//! HTTP smoke test: the control router answers with default config and a live (but
//! engine-less) scheduler, no port binding needed (`tower::ServiceExt::oneshot`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

use simplehar_se::api::{control_router, ApiState};
use simplehar_se::config::AppConfig;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::Scheduler;

fn app() -> axum::Router {
    let config = AppConfig::default();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let modelconf = simplehar_se::testutil::temp_dir("smoke");
    let scheduler = Scheduler::start(
        config.clone(),
        modelconf,
        adapters,
        Arc::new(simplehar_se::process::ProcessRuntime::new()) as Arc<dyn EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    );
    control_router(Arc::new(ApiState::new(
        config,
        scheduler,
        simplehar_se::listeners::ListenerRegistry::new(),
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
        simplehar_se::api::BootTimings::default(),
    )))
}

#[tokio::test]
async fn health_and_status_answer_on_default_config() {
    let app = app();

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
    assert_eq!(
        status["categories"].as_array().map(Vec::len),
        Some(4),
        "four category listeners"
    );
    assert_eq!(status["control_addr"], "127.0.0.1:8708");
    assert_eq!(status["slots"].as_array().map(Vec::len), Some(4));
    // §0.2 boot measures are observable at /har/status.
    assert!(status["boot"]["config_ms"].is_u64(), "boot: {status}");
    assert!(
        status["boot"]["scheduler_spawn_ms"].is_u64(),
        "boot: {status}"
    );
}

#[tokio::test]
async fn unknown_route_is_typed_not_found() {
    let response = app()
        .oneshot(
            Request::get("/har/nope")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
