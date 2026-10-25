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
use simplehar_se::scheduler::Scheduler;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type Spec = (&'static str, &'static str, CategoryId, Option<f64>);

fn write_modelconf(specs: &[Spec]) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "se-api-modelconf-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).expect("temp ModelConf dir");
    for (name, engine, category, vram_gb) in specs {
        let mut yaml = format!(
            "name: {name}
engine: {engine}
category: {category}
"
        );
        if let Some(vram) = vram_gb {
            yaml.push_str(&format!(
                "vram_gb: {vram}
"
            ));
        }
        std::fs::write(dir.join(format!("{name}.yaml")), yaml).expect("model yaml");
    }
    dir
}

fn app(
    config: AppConfig,
    specs: &[Spec],
) -> (Router, Router, Router, MockRuntime, std::path::PathBuf) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let dir = write_modelconf(specs);
    let scheduler = Scheduler::start(
        config.clone(),
        dir.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    );
    let state = Arc::new(ApiState::new(
        config,
        scheduler,
        simplehar_se::listeners::ListenerRegistry::new(),
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    ));
    let control = control_router(Arc::clone(&state));
    let work = category_router(Arc::clone(&state), CategoryId::Work);
    let rp = category_router(Arc::clone(&state), CategoryId::Rp);
    (control, work, rp, runtime, dir)
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
    let (control, _work, _rp, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );

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
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"ghost"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn unload_bodies_are_strict() {
    let (control, _, _, _, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
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
    let (_, work, rp, _, _modelconf) = app(
        AppConfig::default(),
        &[
            ("work-model", "mock", CategoryId::Work, Some(4.0)),
            ("embed-model", "mock", CategoryId::Emb, Some(2.0)),
        ],
    );

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
    let (control, _, rp, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("work-model", "mock", CategoryId::Work, Some(4.0))],
    );

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
    let (control, work, _, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
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
    let (control, work, _, _, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
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
    let mut config = AppConfig::default();
    config
        .category_mut(CategoryId::Rp)
        .expect("default config has RP")
        .default_model = Some("default-model".to_string());
    let (control, _, rp, runtime, _modelconf) = app(
        config,
        &[("default-model", "mock", CategoryId::Rp, Some(4.0))],
    );
    let _ = control;

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
    let (_, work, _, _, _) = app(AppConfig::default(), &[]);
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
    let (control, _, rp, _, _) = app(
        AppConfig::default(),
        &[
            ("big-a", "mock", CategoryId::Work, Some(50.0)),
            ("big-b", "mock", CategoryId::Rp, Some(50.0)),
        ],
    );
    post_json(&control, "/har/load", r#"{"model":"big-a"}"#).await;

    let (status, body) = post_json(&rp, "/v1/chat/completions", r#"{"model":"big-b"}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("need 50.0 GiB") && message.contains("free 20.0 GiB"),
        "was: {message}"
    );
}

#[tokio::test]
async fn rescan_picks_up_new_model_files() {
    let (control, work, _, _, modelconf) = app(
        AppConfig::default(),
        &[("first", "mock", CategoryId::Work, Some(4.0))],
    );
    let (status, body) = get_json(&work, "/v1/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().map(Vec::len), Some(1));

    std::fs::write(
        modelconf.join("second.yaml"),
        "name: second
engine: mock
category: WORK
vram_gb: 4
",
    )
    .expect("write second model");
    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["models"], 2);

    let (_, body) = get_json(&work, "/v1/models").await;
    assert_eq!(body["data"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn control_page_serves_assets() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);
    let (status, _) = get_json_html(&control, "/").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_json_html(&control, "/style.css").await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = get_json_html(&control, "/app.js").await;
    assert_eq!(status, StatusCode::OK);
}

async fn get_json_html(app_router: &Router, path: &str) -> (StatusCode, String) {
    let response = app_router
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn har_models_lists_the_full_catalog() {
    let (control, _, _, _, _) = app(
        AppConfig::default(),
        &[
            ("work-model", "mock", CategoryId::Work, Some(4.0)),
            ("embed-model", "mock", CategoryId::Emb, Some(2.0)),
        ],
    );
    let (status, body) = get_json(&control, "/har/models").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn default_model_endpoint_validates_and_takes_effect() {
    let (control, work, _, _, _) = app(
        AppConfig::default(),
        &[
            ("qwen3", "mock", CategoryId::Work, Some(4.0)),
            ("emb-model", "mock", CategoryId::Emb, Some(2.0)),
        ],
    );

    // Wrong category: 400 with the reason.
    let (status, body) = post_json(
        &control,
        "/har/config/default",
        r#"{"category":"RP","model":"qwen3"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");

    // Valid set: ok, and the status reflects it.
    let (status, _) = post_json(
        &control,
        "/har/config/default",
        r#"{"category":"WORK","model":"qwen3"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, status_body) = get_json(&control, "/har/status").await;
    let work_cat = status_body["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .find(|cat| cat["id"] == "WORK")
        .expect("WORK");
    assert_eq!(work_cat["default_model"], "qwen3");

    // The no-model fallback now loads it without touching the boot config file.
    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["body"]["model"], "qwen3");
}

#[tokio::test]
async fn events_stream_serves_sse_with_an_immediate_state() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);
    let response = control
        .oneshot(
            Request::get("/har/events")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        content_type.starts_with("text/event-stream"),
        "{content_type}"
    );

    // The first frame must arrive without waiting for a mutation.
    let mut stream = response.into_body().into_data_stream();
    let first = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        use tokio_stream::StreamExt;
        stream.next().await
    })
    .await
    .expect("first frame within 5s")
    .expect("stream alive")
    .expect("frame is bytes");
    let text = String::from_utf8_lossy(&first);
    assert!(text.contains("har_state"), "frame: {text}");
}
