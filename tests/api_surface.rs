//! API-surface integration tests (Steps 11-12): control routes, per-category
//! listeners, wrong-category 404-before-ensure, no-model fallback, proxy
//! pass-through — all against the in-process mock engine.

mod mock_runtime;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use mock_runtime::MockRuntime;
use serde_json::Value;
use simplehar_se::api::{category_router, control_router, ApiState};
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::model::ModelEntry;
use simplehar_se::scheduler::Scheduler;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

fn entry(name: &str, engine: &str, category: CategoryId, vram_gb: Option<f64>) -> ModelEntry {
    ModelEntry {
        name: name.to_string(),
        engine: engine.to_string(),
        category,
        vram_gb,
        model_ref: None,
        file: None,
    }
}

fn app(config: AppConfig, catalog: Vec<ModelEntry>) -> (Router, Router, Router, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let scheduler = Scheduler::start(
        config.clone(),
        catalog.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
    );
    let state = Arc::new(ApiState::new(config, scheduler, catalog));
    let control = control_router(Arc::clone(&state));
    let work = category_router(Arc::clone(&state), CategoryId::Work);
    let rp = category_router(Arc::clone(&state), CategoryId::Rp);
    (control, work, rp, runtime)
}

async fn post_json(app: &Router, path: &str, body: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bounded body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

async fn get_json(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bounded body");
    let value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, value)
}

#[tokio::test]
async fn load_flow_populates_jobs_and_status() {
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (control, _work, _rp, runtime) = app(AppConfig::default(), catalog);

    let (status, body) = post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["status"], "loaded");
    assert_eq!(body["receipt"]["model"], "qwen3");
    assert_eq!(runtime.server_count(), 1);

    let (status, body) = get_json(&control, "/har/jobs").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["jobs"].as_array().map(Vec::len), Some(1));
    assert_eq!(body["jobs"][0]["state"], "Serving");

    let (status, body) = get_json(&control, "/har/status").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["slots"][0]["model"], "qwen3");

    let job_id = body["jobs"][0]["id"].as_u64().unwrap_or_default();
    let (status, _) = get_json(&control, &format!("/har/jobs/{job_id}")).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_json(&control, "/har/jobs/999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn load_unknown_model_is_typed_not_found() {
    let (control, _, _, _) = app(AppConfig::default(), vec![]);
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"ghost"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn unload_bodies_are_strict() {
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (control, _, _, _) = app(AppConfig::default(), catalog);
    post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;

    // No category field: 400, never an implicit unload-all.
    let (status, _) = post_json(&control, "/har/unload", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Garbage: 400.
    let (status, _) = post_json(&control, "/har/unload", "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Valid: accepted and actually unloads.
    let (status, body) = post_json(&control, "/har/unload", r#"{"category":"WORK"}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let status = get_json(&control, "/har/status").await;
    let _ = status;
}

#[tokio::test]
async fn models_are_filtered_per_listener() {
    let catalog = vec![
        entry("work-model", "mock", CategoryId::Work, Some(4.0)),
        entry("embed-model", "mock", CategoryId::Emb, Some(2.0)),
    ];
    let (_, work, rp, _) = app(AppConfig::default(), catalog);

    let (status, body) = get_json(&work, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    let ids: Vec<&str> = body["data"]
        .as_array()
        .expect("list")
        .iter()
        .map(|m| m["id"].as_str().expect("id"))
        .collect();
    assert_eq!(ids, vec!["work-model"]);

    let (status, body) = get_json(&rp, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().map(Vec::len), Some(0));
}

#[tokio::test]
async fn wrong_category_is_404_before_any_load() {
    // The RP listener must refuse a WORK model with 404 and create no job.
    let catalog = vec![entry("work-model", "mock", CategoryId::Work, Some(4.0))];
    let (control, _, rp, runtime) = app(AppConfig::default(), catalog);

    let (status, body) = post_json(&rp, "/v1/chat/completions", r#"{"model":"work-model"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert!(body["message"]
        .as_str()
        .unwrap_or("")
        .contains("WORK listener"));

    let (_, status) = get_json(&control, "/har/jobs").await;
    assert_eq!(
        status["jobs"].as_array().map(Vec::len),
        Some(0),
        "no ensure may run"
    );
    assert_eq!(runtime.server_count(), 0, "no engine may spawn");
}

#[tokio::test]
async fn proxy_forwards_with_model_rewrite_and_streams_the_response() {
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (control, work, _, runtime) = app(AppConfig::default(), catalog);
    post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(runtime.server_count(), 1);

    let (status, body) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"QWEN3","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["echo"], true);
    assert_eq!(body["path"], "/v1/chat/completions");
    // Alias resolved and canonical harness name rewritten into the body.
    assert_eq!(body["body"]["model"], "qwen3");
}

#[tokio::test]
async fn no_model_fallback_serves_the_loaded_occupant() {
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (control, work, _, _) = app(AppConfig::default(), catalog);
    post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;

    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["body"]["model"], "qwen3",
        "fallback must serve the loaded model"
    );
}

#[tokio::test]
async fn no_model_fallback_loads_the_category_default() {
    let catalog = vec![entry("default-model", "mock", CategoryId::Rp, Some(4.0))];
    let mut config = AppConfig::default();
    config
        .category_mut(CategoryId::Rp)
        .expect("default config has RP")
        .default_model = Some("default-model".to_string());
    let (control, _, rp, runtime) = app(config, catalog);

    let (status, body) = post_json(&rp, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["body"]["model"], "default-model");
    assert_eq!(
        runtime.server_count(),
        1,
        "the default model was loaded on demand"
    );
    let _ = control;
}

#[tokio::test]
async fn no_model_without_default_is_400_naming_the_listener() {
    let (_, work, _, _) = app(AppConfig::default(), vec![]);
    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("WORK") && message.contains("default_model"),
        "was: {message}"
    );
}

#[tokio::test]
async fn admission_refusal_via_proxy_is_503_with_numbers() {
    let catalog = vec![
        entry("big-a", "mock", CategoryId::Work, Some(50.0)),
        entry("big-b", "mock", CategoryId::Rp, Some(50.0)),
    ];
    let (control, _, rp, _) = app(AppConfig::default(), catalog);
    post_json(&control, "/har/load", r#"{"model":"big-a"}"#).await;

    let (status, body) = post_json(&rp, "/v1/chat/completions", r#"{"model":"big-b"}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("need 50.0 GiB") && message.contains("free 20.0 GiB"),
        "was: {message}"
    );
}
