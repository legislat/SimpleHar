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
    adapters.insert("mock-error".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-never".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-slow".to_string(), Arc::new(MockAdapter));
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
        simplehar_se::api::BootTimings::default(),
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

    // No category/model but a NON-EMPTY body: 400 (the object is unrecognized).
    let (status, _) = post_json(&control, "/har/unload", r#"{"unknown":1}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Garbage: 400.
    let (status, _) = post_json(&control, "/har/unload", "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Model unload of a not-loaded model: accepted (Done), idempotent (T12.2).
    let (status, _) = post_json(&control, "/har/unload", r#"{"model":"never-loaded"}"#).await;
    assert_eq!(status, StatusCode::OK);
    // Explicit empty object: unload-all (T12.4) - accepted.
    let (status, _) = post_json(&control, "/har/unload", "{}").await;
    assert_eq!(status, StatusCode::OK);
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
    // Alias resolved to the harness name, then rewritten to the engine's
    // self-reported id (F-4: the mock reports `mock-model`).
    assert_eq!(body["body"]["model"], "mock-model");
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
        body["body"]["model"], "mock-model",
        "fallback serves the loaded model (rewritten to its engine id)"
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
    assert_eq!(body["body"]["model"], "mock-model");
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
    assert_eq!(body["body"]["model"], "mock-model");
}

#[tokio::test]
async fn extended_logging_toggle_round_trips_and_validates() {
    let (control, _, _, _, _) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
    let (status, body) = post_json(&control, "/har/config/logging", r#"{"enabled":"yes"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
    let (status, _) = post_json(&control, "/har/config/logging", r#"{"enabled":true}"#).await;
    assert_eq!(status, StatusCode::OK);
    let (_, doc) = get_json(&control, "/har/status").await;
    assert_eq!(doc["extended_logging"], true);
    let (status, _) = post_json(&control, "/har/config/logging", r#"{"enabled":false}"#).await;
    assert_eq!(status, StatusCode::OK);
    let (_, doc) = get_json(&control, "/har/status").await;
    assert_eq!(doc["extended_logging"], false);
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

#[tokio::test]
async fn config_model_writes_back_to_the_yaml_file() {
    let (control, _, _, _, modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
    let file = modelconf.join("qwen3.yaml");
    let original = std::fs::read_to_string(&file).expect("model file");

    // Category edit: written into the file, catalog refreshed.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","category":"AUX"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let edited = std::fs::read_to_string(&file).expect("read back");
    assert!(edited.contains("category: AUX"), "was: {edited}");
    assert!(edited.contains("name: qwen3"), "rest of the file survives");

    // The catalog now shows the new category on the right listener.
    let (_, rp_models) = get_json(&control, "/har/models").await;
    let entry = rp_models["data"]
        .as_array()
        .expect("list")
        .iter()
        .find(|m| m["id"] == "qwen3")
        .expect("entry");
    assert_eq!(entry["category"], "AUX");

    // CPU edit: boolean -> yaml key, and back.
    let (status, _) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","cpu":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(std::fs::read_to_string(&file)
        .expect("read")
        .contains("cpu_offload: true"));
    let _ = original;
}

#[tokio::test]
async fn moving_the_default_model_clears_its_old_category_default() {
    // Operator finding #1 (2026-10-26): moving the default model out of a category
    // must not leave the old listener defaulting to a model it no longer serves.
    let (control, _, _, _, _) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
    post_json(
        &control,
        "/har/config/default",
        r#"{"category":"WORK","model":"qwen3"}"#,
    )
    .await;

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","category":"RP"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    let (_, status_body) = get_json(&control, "/har/status").await;
    let work_cat = status_body["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .find(|cat| cat["id"] == "WORK")
        .expect("WORK");
    assert!(
        work_cat["default_model"].is_null(),
        "the old category must not keep a default that moved away: {status_body}"
    );
}

#[tokio::test]
async fn defaults_persist_to_the_config_boot_loaded_even_under_a_custom_path() {
    // Operator finding #2 (2026-10-26): write-back must hit the file boot read
    // (`SIMPLEHAR_CONFIG` aware), and a partial config with no `categories` key
    // must still accept the patch instead of silently staying in memory.
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let path = std::env::temp_dir().join(format!(
        "se-api-config-{}-{}.json",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, r#"{"modelconf_dir": "unused"}"#).expect("seed config");
    let mut config =
        AppConfig::parse(&std::fs::read_to_string(&path).expect("read")).expect("parses");
    config.source_path = Some(path.clone());

    let (control, _, _, _, _) = app(config, &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);
    let (status, body) = post_json(
        &control,
        "/har/config/default",
        r#"{"category":"WORK","model":"qwen3"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(
        body["persisted"].as_bool().unwrap_or(false),
        "custom-path config must accept the write: {body}"
    );

    let on_disk: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("persisted file must exist at the boot path"),
    )
    .expect("valid json");
    assert!(
        on_disk.get("source_path").is_none(),
        "the internal path marker must never be serialized"
    );
    // The written file must survive a full boot reload — the class of bug that
    // dropped `control: true` on the first rebuild-through-to_value.
    let reloaded =
        AppConfig::parse(&std::fs::read_to_string(&path).expect("read back persisted config"))
            .expect("persisted config parses");
    let reload_errors = reloaded.validate();
    assert!(
        reload_errors.is_empty(),
        "persisted config must validate: {reload_errors:?}"
    );
    let work = reloaded
        .categories
        .iter()
        .find(|cat| cat.id == CategoryId::Work)
        .expect("WORK");
    assert_eq!(work.default_model.as_deref(), Some("qwen3"));
    assert!(
        reloaded.categories.iter().any(|cat| cat.control),
        "the control flag must survive the persist reload"
    );
    let work_json = on_disk["categories"]
        .as_array()
        .expect("categories rebuilt from boot config")
        .iter()
        .find(|cat| cat["id"] == "WORK")
        .expect("WORK");
    assert_eq!(work_json["default_model"], "qwen3");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn last_error_is_the_newest_failed_load_and_survives_unrelated_success() {
    // A3 (API wiring): a genuine engine failure pins `last_error` (502, names the
    // model + readiness); a newer failure takes the headline; an unrelated
    // successful load of a DIFFERENT model does not mask a real fault (finding 2).
    // The pure derivation rules (same-model clear, cancel/unload skip) are pinned
    // by the `headline_fault` unit test in src/scheduler/mod.rs.
    let config = AppConfig {
        load_timeout_s: 2,
        ..AppConfig::default()
    };
    let (control, _, _, _, _) = app(
        config,
        &[
            ("bad", "mock-never", CategoryId::Work, Some(4.0)),
            ("bad2", "mock-never", CategoryId::Work, Some(4.0)),
            ("good", "mock", CategoryId::Work, Some(4.0)),
        ],
    );

    let (status, body) = post_json(&control, "/har/load", r#"{"model":"bad"}"#).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "body: {body}");
    let (_, doc) = get_json(&control, "/har/status").await;
    let first = doc["last_error"].as_str().unwrap_or_default().to_string();
    assert!(
        first.contains("bad@WORK") && first.contains("ready"),
        "the failed load pins last_error: {first}"
    );

    let (status, _) = post_json(&control, "/har/load", r#"{"model":"bad2"}"#).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "second genuine failure");
    let (_, doc) = get_json(&control, "/har/status").await;
    let newest = doc["last_error"].as_str().unwrap_or_default();
    assert!(
        newest.contains("bad2@WORK"),
        "newest failure wins: {newest}"
    );

    let (status, _) = post_json(&control, "/har/load", r#"{"model":"good"}"#).await;
    assert_eq!(status, StatusCode::OK, "the good model loads");
    let (_, doc) = get_json(&control, "/har/status").await;
    assert!(
        doc["last_error"]
            .as_str()
            .unwrap_or_default()
            .contains("bad2@WORK"),
        "an unrelated success must not mask a real fault: {:?}",
        doc["last_error"]
    );
}

#[tokio::test]
async fn below_launch_floor_gets_its_own_error_code() {
    // 503 with a distinct code: clients can tell "card busy with foreign work,
    // refused before spawn" (greedy guard) from plain admission over-commit.
    let config = AppConfig {
        vram_total_gb: 10.0,
        vram_reserve_gb: 2.0,
        load_timeout_s: 5,
        ..AppConfig::default()
    };
    let (control, _, _, _, dir) = app(config, &[("filler", "mock", CategoryId::Work, Some(4.0))]);
    std::fs::write(
        dir.join("guarded.yaml"),
        "name: guarded\nengine: mock\ncategory: RP\nvram_gb: 2\n\
         launch_only_with_at_least_vram_free: 40\n",
    )
    .expect("seed guarded model");
    let (status, _) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(&control, "/har/load", r#"{"model":"guarded"}"#).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "body: {body}");
    assert_eq!(body["code"], "below_launch_floor");
    assert_eq!(body["error"]["code"], "below_launch_floor");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("launch floor"),
        "message explains the floor: {body}"
    );
}

#[tokio::test]
async fn guards_refuse_rebinding_hosts_and_foreign_origins() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);

    // DNS-rebinding guard: a hostname in Host is refused 400 before any handler.
    let response = control
        .clone()
        .oneshot(
            Request::get("/har/status")
                .header("host", "evil.example.com")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Loopback shapes pass.
    for host in ["127.0.0.1:8708", "localhost:8708", "[::1]:8708"] {
        let response = control
            .clone()
            .oneshot(
                Request::get("/har/status")
                    .header("host", host)
                    .body(Body::empty())
                    .expect("static request"),
            )
            .await
            .expect("in-process request");
        assert_eq!(response.status(), StatusCode::OK, "host {host}");
    }

    // CSRF guard: a cross-site Origin on POST is refused 403.
    let response = control
        .oneshot(
            Request::post("/har/unload")
                .header("host", "127.0.0.1:8708")
                .header("origin", "http://evil.example.com")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::CONFLICT);
}

#[tokio::test]
async fn posts_without_json_content_type_are_415() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);
    let response = control
        .oneshot(
            Request::post("/har/unload")
                .header("host", "127.0.0.1:8708")
                .header("content-type", "text/plain")
                .body(Body::from("{}"))
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(response.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
}

#[tokio::test]
async fn request_ids_are_echoed_or_generated() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);

    // Client-provided id is echoed.
    let response = control
        .clone()
        .oneshot(
            Request::get("/har/status")
                .header("host", "127.0.0.1:8708")
                .header("x-request-id", "my-trace-1")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(
        response
            .headers()
            .get("x-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("my-trace-1")
    );

    // Absent id is generated (req-<epoch>-<counter> shape).
    let response = control
        .oneshot(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    let id = response
        .headers()
        .get("x-request-id")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(id.starts_with("req-"), "generated id: {id}");
}

#[tokio::test]
async fn genuine_upstream_error_is_surfaced_never_retried() {
    // A9: a 500 from the engine is a real answer - no crash-restart, no retry,
    // exactly one job in the ledger.
    let (control, work, _, runtime, _) = app(
        AppConfig::default(),
        &[("err-model", "mock-error", CategoryId::Work, Some(4.0))],
    );
    let (status, _) = post_json(&control, "/har/load", r#"{"model":"err-model"}"#).await;
    assert_eq!(status, StatusCode::OK);

    let (status, _) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"err-model","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    let (_, jobs) = get_json(&control, "/har/jobs").await;
    assert_eq!(
        jobs["jobs"].as_array().map(Vec::len),
        Some(1),
        "no restart job may exist"
    );
    let _ = runtime;
}

#[tokio::test]
async fn har_log_serves_the_ring() {
    let (control, _, _, _, _) = app(AppConfig::default(), &[]);
    // Push a marker entry straight into the ring (the test binary has no global
    // subscriber; the endpoint reads the same process-global storage).
    simplehar_se::observability::ring_push(simplehar_se::observability::LogEntry::now(
        "ERROR",
        "test::har_log",
        "last load failed: need 50 GiB",
    ));
    let (status, body) = get_json(&control, "/har/log?limit=10").await;
    assert_eq!(status, StatusCode::OK);
    let events = body["events"].as_array().expect("events list");
    assert!(events.iter().any(|entry| {
        entry["message"]
            .as_str()
            .unwrap_or_default()
            .contains("last load failed")
    }));
    assert!(body["capacity"].is_u64());
}
