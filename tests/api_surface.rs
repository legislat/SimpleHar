//! API-surface integration tests (Steps 11-12): control routes, per-category
//! listeners, wrong-category 404-before-ensure, no-model fallback, proxy
//! pass-through — all against the in-process mock engine.

mod mock_runtime;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use mock_runtime::MockRuntime;
use serde_json::Value;
use simplehar_se::api::{category_router, classify_send_failure, control_router, ApiState};
use simplehar_se::config::{AppConfig, CategoryConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::Scheduler;
use std::collections::BTreeMap;
use std::sync::Arc;
use tower::ServiceExt;

type Spec = (&'static str, &'static str, CategoryId, Option<f64>);

/// The per-test stand-in for the boot config file (FINDINGS-18 [2]): `app()` points
/// every `AppConfig` at `<modelconf dir>/simplehar-se.json` so the persisting
/// endpoints (`/har/config/default`, the retag default-clear) write INSIDE the test's
/// own temp dir. Before this they fell through to `default_config_path()` — the exe
/// dir — so `cargo test` wrote a file the product reads at boot, outside any sandbox,
/// and no test could read back what the endpoint had written.
const TEST_CONFIG_NAME: &str = "simplehar-se.json";

fn write_modelconf(specs: &[Spec]) -> std::path::PathBuf {
    let dir = simplehar_se::testutil::temp_dir("api-modelconf");
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

/// [`app`] with extra engine adapters (ids `mock-a`/`mock-b` carrying distinct
/// `/v1/models` ids, `mock-nodiscovery`, …).
#[allow(clippy::type_complexity)]
fn build_app(
    mut config: AppConfig,
    specs: &[Spec],
    extra_adapters: &[(&str, std::sync::Arc<dyn EngineAdapter>)],
) -> (
    Router,
    Router,
    Router,
    Router,
    MockRuntime,
    std::path::PathBuf,
) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-error".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-never".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-slow".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-dies-once".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-dies".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-nodiscovery".to_string(), Arc::new(MockAdapter));
    for (id, adapter) in extra_adapters {
        adapters.insert((*id).to_string(), Arc::clone(adapter));
    }
    let dir = write_modelconf(specs);
    // A hand-stamped `source_path` (the custom-path persist test) is respected;
    // everything else is sandboxed to this test's temp dir (FINDINGS-18 [2]).
    if config.source_path.is_none() {
        config.source_path = Some(dir.join(TEST_CONFIG_NAME));
    }
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
    let aux = category_router(state, CategoryId::Aux);
    (control, work, rp, aux, runtime, dir)
}

fn app(
    config: AppConfig,
    specs: &[Spec],
) -> (
    Router,
    Router,
    Router,
    Router,
    MockRuntime,
    std::path::PathBuf,
) {
    build_app(config, specs, &[])
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
async fn control_token_gates_api_routes_but_never_the_page_itself() {
    // The optional shared secret (`control_token` / `SIMPLEHAR_TOKEN`) is what makes
    // publishing `internal_host` to a segment defensible: without it any LAN host can
    // load, unload, reconfigure and read logs (deploy review, 2026-10-26).
    let config = AppConfig {
        control_token: Some("s3cret".to_string()),
        ..AppConfig::default()
    };
    let (control, work, _rp, _, _runtime, _modelconf) =
        app(config, &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);

    let (status, body) = get_json(&control, "/har/status").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "body: {body}");
    assert_eq!(body["code"], "unauthorized");

    let wrong = control
        .clone()
        .oneshot(
            Request::get("/har/models")
                .header("x-har-token", "nope")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);

    let right = control
        .clone()
        .oneshot(
            Request::get("/har/status")
                .header("x-har-token", "s3cret")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(right.status(), StatusCode::OK);

    let bearer = control
        .clone()
        .oneshot(
            Request::get("/har/models")
                .header("authorization", "Bearer s3cret")
                .body(Body::empty())
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(bearer.status(), StatusCode::OK);

    // A mutating proxy route is gated as well...
    let (status, _body) = post_json(&work, "/v1/chat/completions", r#"{"model":"qwen3"}"#).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // ...while the static page and every asset it links are not: the page has to load
    // before it can ask for a token. `/pico.min.css` was missing from the public list,
    // which meant a token-protected harness served an unstyled page (README review).
    // Iterating the guard's own list, not a copy, so adding an asset to the page without
    // exempting it can never pass here again.
    for path in simplehar_se::api::PUBLIC_PATHS {
        let page = control
            .clone()
            .oneshot(
                Request::get(path)
                    .body(Body::empty())
                    .expect("static request"),
            )
            .await
            .expect("in-process request");
        assert_eq!(page.status(), StatusCode::OK, "public asset {path}");
    }
}

/// Every local asset reference the page itself makes (`href="/..."` / `src="/..."`).
///
/// Absolute URLs (the Google-Fonts link) and `#` anchors are excluded by the leading-slash
/// rule, so the result is the browser's own fetch set — the client's truth, not the guard's
/// copy of it.
///
/// FINDINGS-FIXROUND2 [6]: the scanner matches only the exact double-quoted lowercase
/// forms, so single-quoted, uppercase, spaced, or unquoted attribute forms evade it; the
/// trio floor below keeps today's scan non-vacuous, and hardening is deferred to the card
/// window.
fn linked_assets(html: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for attr in ["href=\"", "src=\""] {
        let mut rest = html;
        while let Some(at) = rest.find(attr) {
            let after = &rest[at + attr.len()..];
            let Some(end) = after.find('"') else {
                break;
            };
            let value = &after[..end];
            if value.starts_with('/') && !value.starts_with("//") {
                found.push(value.to_string());
            }
            rest = after;
        }
    }
    found.sort();
    found.dedup();
    found
}

#[tokio::test]
async fn every_page_linked_asset_is_public() {
    // FINDINGS-16 [1] / FINDINGS-18 [1]: the served-asset set is hand-maintained in three
    // copies (the webui router, PUBLIC_PATHS, index.html's tags) and the loop in the test
    // above asks only for what the guard already exempts — delete an entry there and the
    // request disappears instead of failing. `/pico.min.css` shipped exactly that way: a
    // token-protected deploy served an unstyled page through green gates. Iterating
    // index.html (what the client actually fetches) covers the direction the guard's own
    // list cannot see, so a linked-but-not-exempt asset fails here.
    let config = AppConfig {
        control_token: Some("s3cret".to_string()),
        ..AppConfig::default()
    };
    let (control, _, _, _, _runtime, _modelconf) = app(config, &[]);

    let (status, page) = get_json_html(&control, "/").await;
    assert_eq!(status, StatusCode::OK, "the page must answer tokenless");

    let assets = linked_assets(&page);
    assert!(
        assets.len() >= 3,
        "scanner found only {assets:?} in index.html; a short or empty scan must fail loudly"
    );
    let joined = assets.join(" ");
    assert!(
        joined.contains(".css") && joined.contains(".js"),
        "scanned set {assets:?} holds neither a stylesheet nor a script: the scanner is broken"
    );
    for known in ["/style.css", "/app.js", "/pico.min.css"] {
        assert!(
            assets.iter().any(|path| path == known),
            "index.html links {known}; the scanner missed it (found {assets:?})"
        );
    }

    for path in &assets {
        let (status, body) = get_json_html(&control, path).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "linked asset {path} refused tokenless — is it missing from PUBLIC_PATHS?"
        );
        assert!(!body.is_empty(), "linked asset {path} served an empty body");
    }
}

#[tokio::test]
async fn load_flow_populates_jobs_and_status() {
    let (control, _work, _rp, _, runtime, _modelconf) = app(
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"ghost"}"#).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["code"], "not_found");
}

#[tokio::test]
async fn unload_bodies_are_strict() {
    let (control, _, _, _, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
    post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(runtime.server_count(), 1, "one engine is loaded");

    // No category/model but a NON-EMPTY body: 400 (the object is unrecognized).
    let (status, _) = post_json(&control, "/har/unload", r#"{"unknown":1}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Garbage: 400.
    let (status, _) = post_json(&control, "/har/unload", "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Model unload of a not-loaded model: accepted (Done), idempotent (T12.2).
    let (status, _) = post_json(&control, "/har/unload", r#"{"model":"never-loaded"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        runtime.server_count(),
        1,
        "an idempotent model unload of a never-loaded model frees nothing"
    );
    // Explicit empty object: unload-all (T12.4) — accepted AND it really releases
    // every engine (FINDINGS-18 [5]: the response used to be bound and discarded;
    // a handler that answered 200 while unloading nothing passed).
    let (status, _) = post_json(&control, "/har/unload", "{}").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        await_server_count(&runtime, 0).await,
        0,
        "the empty-object scope actually unloads"
    );

    // Reload, then the scoped form: `{category: WORK}` must free ITS engine, not
    // merely answer 200. `await_server_count` polls because the reply arrives at job
    // START, not when the kill lands (see the helper's note).
    let (status, _) = post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        runtime.server_count(),
        1,
        "the engine is back for the scoped test"
    );
    // Valid: accepted and actually unloads.
    let (status, body) = post_json(&control, "/har/unload", r#"{"category":"WORK"}"#).await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        await_server_count(&runtime, 0).await,
        0,
        "the category unload frees its engine"
    );
    let (_, status) = get_json(&control, "/har/status").await;
    let work_slot = status["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|slot| slot["category"] == "WORK")
        .expect("WORK slot");
    assert!(
        work_slot["model"].is_null(),
        "and the slot reads empty afterwards: {status}"
    );
}

#[tokio::test]
async fn models_are_filtered_per_listener() {
    let (_, work, rp, _, _, _modelconf) = app(
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
    let (control, _, rp, _, runtime, _modelconf) = app(
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
    let (control, work, _, _, runtime, _modelconf) = app(
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

    // FINDINGS-18 [4]: the "streams" half used to assert nothing. The proxy
    // contract (src/api/mod.rs `proxy`: upstream headers copied + the body handed
    // through `Body::from_stream`) is byte-for-byte pass-through — a regression
    // that buffered and re-serialised the JSON, or dropped the upstream
    // `content-type`, passed the assertions above. The mock answers a
    // `"stream": true` request with a fixed SSE literal; this asks for exactly
    // that literal and demands the content-type and the bytes come back
    // untouched.
    let response = work
        .clone()
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"model":"QWEN3","stream":true}"#))
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
        "the upstream content-type must survive the pass-through, got: {content_type}"
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bounded body");
    assert_eq!(
        bytes.as_ref(),
        mock_runtime::SSE_BODY.as_bytes(),
        "the streamed body must arrive byte-for-byte, not re-serialised"
    );
}

#[tokio::test]
async fn proxy_rewrites_each_occupant_to_its_own_engine_id() {
    // FINDINGS-20 [6]: every mock engine used to report the one constant `mock-model`
    // at `/v1/models`, so every serve_id assertion in the suite compared the same
    // string and a product bug that forwarded slot A's id for slot B's request — the
    // crash-retry staleness class fixed in b540c30 — was invisible. These two engines
    // report DISTINCT ids; each request must be rewritten to ITS OWN engine's id.
    let adapters: Vec<(&str, Arc<dyn EngineAdapter>)> = vec![
        (
            "mock-id-a",
            Arc::new(mock_runtime::MockIdAdapter {
                engine: "mock-id-a",
                serve_id: "engine-alpha",
            }),
        ),
        (
            "mock-id-b",
            Arc::new(mock_runtime::MockIdAdapter {
                engine: "mock-id-b",
                serve_id: "engine-beta",
            }),
        ),
    ];
    let mut config = AppConfig::default();
    config
        .category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = 2;
    let (control, _, _, aux, runtime, _modelconf) = build_app(
        config,
        &[
            ("alpha", "mock-id-a", CategoryId::Aux, Some(3.0)),
            ("beta", "mock-id-b", CategoryId::Aux, Some(3.0)),
        ],
        &adapters,
    );
    for name in ["alpha", "beta"] {
        let body = format!(r#"{{"model":"{name}"}}"#);
        let (status, reply) = post_json(&control, "/har/load", &body).await;
        assert_eq!(status, StatusCode::OK, "load {name}: {reply}");
    }
    assert_eq!(runtime.server_count(), 2, "both occupants serve");

    for (model, expect_id) in [("alpha", "engine-alpha"), ("beta", "engine-beta")] {
        let body = format!(r#"{{"model":"{model}","messages":[]}}"#);
        let (status, reply) = post_json(&aux, "/v1/chat/completions", &body).await;
        assert_eq!(status, StatusCode::OK, "proxy {model}: {reply}");
        assert_eq!(
            reply["body"]["model"].as_str(),
            Some(expect_id),
            "{model}'s request must carry {model}'s OWN engine id — a cross-slot \
             serve_id mix-up is exactly what one shared mock id hid: {reply}"
        );
    }
}

#[tokio::test]
async fn a_silent_engine_falls_back_to_the_harness_name_on_the_wire() {
    // FINDINGS-20 [6], the `serve_id == None` half: when an engine answers nothing at
    // `/v1/models`, discovery yields `None` and the proxy forwards the HARNESS NAME
    // (documented `serve_id.unwrap_or_else` fallback at src/api/mod.rs) — previously
    // untestable because no mock mode could make discovery fail. The mock
    // `mock-nodiscovery` 404s `/v1/models`; the rewritten `model` field must be the
    // harness name the operator gave, never an invented engine id.
    let (control, work, _, _, _runtime, _modelconf) = app(
        AppConfig::default(),
        &[("quiet", "mock-nodiscovery", CategoryId::Work, Some(4.0))],
    );
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"quiet"}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a silent engine still loads: {body}"
    );

    let (status, reply) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"quiet","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "proxy: {reply}");
    assert_eq!(
        reply["body"]["model"].as_str(),
        Some("quiet"),
        "no discovered id means the documented harness-name fallback on the wire: {reply}"
    );
}

#[tokio::test]
async fn crash_retry_re_resolves_by_name_after_the_id_rewrite() {
    // T10.1 regression (T-suite Run 4, 2026-10-27). `proxy` rewrites `body["model"]`
    // to the engine's self-reported id - `mock-model` here, the gguf PATH for llama -
    // before forwarding. The crash-retry then re-resolved the target from that
    // rewritten body, so it searched the catalog for a model *named* by engine id and
    // 404'd: the restart succeeded, the request still failed.
    let (control, work, _, _, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock-dies-once", CategoryId::Work, Some(4.0))],
    );
    let (load_status, load_body) = post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(
        load_status,
        StatusCode::OK,
        "the first load must succeed before anything is killed: {load_body}"
    );
    assert_eq!(runtime.spawn_count(), 1, "one engine started");
    let (_, loaded) = get_json(&control, "/har/status").await;
    let port = u16::try_from(
        loaded["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|slot| slot["model"] == "qwen3")
            .and_then(|slot| slot["port"].as_u64())
            .expect("the serving slot reports its engine port"),
    )
    .expect("port fits u16");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !runtime.is_dead(port).await && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(runtime.is_dead(port).await, "the engine must really die");

    let (status, body) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"qwen3","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "crash-retry must answer: {body}");
    assert_eq!(body["echo"], true, "the restarted engine answered");
    assert_eq!(
        body["body"]["model"], "mock-model",
        "the retried request still carries the engine's own id (F-4)"
    );
    assert_eq!(runtime.spawn_count(), 2, "exactly one restart");
    assert_eq!(
        runtime.server_count(),
        1,
        "the corpse is gone, one live engine"
    );
}

#[tokio::test]
async fn no_model_fallback_serves_the_loaded_occupant() {
    let (control, work, _, _, _, _modelconf) = app(
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
    let (control, _, rp, _, runtime, _modelconf) = app(
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
    let (_, work, _, _, _, _) = app(AppConfig::default(), &[]);
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
    let (control, _, rp, _, _, _) = app(
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
    let (control, work, _, _, _, modelconf) = app(
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);
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
    let (control, _, _, _, _, _) = app(
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
    let (control, work, _, _, _, _) = app(
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
    let (control, _, _, _, _, _) = app(
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);
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
    // The CPU leg rides a `llama` row since the FINDINGS-BATCH-2026-10-28F [3]
    // gate: a `cpu` edit is refused for engines outside OFFLOAD_ENGINES (the
    // refusal legs are pinned by
    // `cpu_edit_is_refused_on_models_whose_engine_never_reads_the_key`); the
    // category leg keeps the mock row, proving category writes stay engine-blind.
    let (control, _, _, _, _, modelconf) = build_app(
        AppConfig::default(),
        &[
            ("qwen3", "mock", CategoryId::Work, Some(4.0)),
            ("gguf3", "llama", CategoryId::Work, Some(4.0)),
        ],
        &[("llama", Arc::new(MockAdapter))],
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

    // CPU edit on the offload engine: boolean -> yaml key, and back.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"gguf3","cpu":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(std::fs::read_to_string(modelconf.join("gguf3.yaml"))
        .expect("read")
        .contains("cpu_offload: true"));
    let _ = original;
}

#[tokio::test]
async fn config_model_reports_writeback_refusals_by_http_class() {
    // FINDINGS-13 [10]: the write-back's typed failures decide the status, so a
    // refusal that is the *file's* state is no longer dressed up as a harness
    // fault. Both legs reach the endpoint through the window the editor-race
    // doctrine allows (DECISIONS: the operator edits yamls while the harness is
    // idle) — the catalog still publishes the entry while the file on disk has
    // moved under it. Falsifier: send `WouldBreakFile` back to `Internal` in
    // `ApiError::from_writeback` and the 409 leg answers 500.
    let (control, _, _, _, _, modelconf) = build_app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
        &[],
    );
    let file = modelconf.join("qwen3.yaml");
    // The actor's cached catalog must hold the row before the file is moved under
    // it — that cache is exactly what makes the two legs below reachable (and a
    // request here also waits out the boot scan, so a late scan cannot turn this
    // pin into a silent 404).
    let (_, published) = get_json(&control, "/har/models").await;
    assert!(
        published["data"]
            .as_array()
            .expect("list")
            .iter()
            .any(|entry| entry["id"] == "qwen3"),
        "the row must be published before the file moves: {published}"
    );

    // 409 conflict: the edited document is one the scan gate would never publish,
    // so the blanket refusal is a live-state clash — and the scanner's own problem
    // lines reach the operator, who is the only one who can fix the file.
    std::fs::write(&file, "name: qwen3\nnot a key line\n").expect("mangle");
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","category":"AUX"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["code"], "conflict");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("edit would break the file"),
        "the module's own sentence must survive the mapping: {message}"
    );
    assert!(
        message.contains("not a key line"),
        "the problem lines are the operator's diagnosis: {message}"
    );
    // FINDINGS-BATCH-2026-10-28H [1]: the config-state refusal names the file, as a
    // caller-side PREFIX. The two `contains` legs above already prove the module's
    // sentence survived; these prove it is intact at both seams — the reason opens
    // with the path and the module's sentence is its exact tail. Falsifier: drop
    // the prefix in `ApiError::from_writeback` and the first leg fails; corrupt the
    // sentence inside the composition and the `contains` legs and this tail leg
    // fail while the module's own pins stay green.
    assert!(
        message.starts_with(&format!("conflict: {}: ", file.display())),
        "a config-state refusal must name its file first: {message}"
    );
    assert!(
        message.ends_with(
            "edit would break the file: line 2: not a `key: value` line: `not a key line`"
        ),
        "the module's sentence must be the untouched tail of the prefix: {message}"
    );
    assert_eq!(
        std::fs::read_to_string(&file).expect("read back"),
        "name: qwen3\nnot a key line\n",
        "a refused edit writes nothing at all"
    );

    // 500 internal: the file itself vanished under a still-published entry — that
    // one genuinely is the harness's/environment's problem, and stays 500.
    std::fs::write(&file, "name: qwen3\nengine: mock\n").expect("restore");
    std::fs::remove_file(&file).expect("delete");
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","category":"AUX"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "body: {body}");
    assert_eq!(body["code"], "internal");
    assert!(
        body["message"]
            .as_str()
            .unwrap_or_default()
            .contains("cannot read"),
        "{body}"
    );

    // And the healthy path is untouched by the typing: same endpoint, same 200.
    std::fs::write(&file, "name: qwen3\nengine: mock\ncategory: WORK\n").expect("reseed");
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"qwen3","category":"AUX"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(std::fs::read_to_string(&file)
        .expect("read")
        .contains("category: AUX"));
}

#[tokio::test]
async fn cpu_edit_is_refused_on_models_whose_engine_never_reads_the_key() {
    // FINDINGS-BATCH-2026-10-28F [3]: with B shipped, a scripted `cpu` write to a
    // strata (or mock) row would make the harness itself the #1 producer of the
    // InertKey noise it just invented. The page offers no control there (C) and
    // the API now agrees: 400 naming the key, the row engine and the owning set —
    // while the engine-blind category write and the llama cpu write keep working.
    // Falsifier: removing the OFFLOAD_ENGINES gate in `har_config_model` returns
    // 200 here and lands a dead `cpu_offload` line in the yaml.
    let (control, _, _, _, _, modelconf) = build_app(
        AppConfig::default(),
        &[
            ("strataline", "strata", CategoryId::Work, Some(8.0)),
            ("mockline", "mock", CategoryId::Work, Some(4.0)),
            ("llamaline", "llama", CategoryId::Work, Some(4.0)),
        ],
        &[
            ("strata", Arc::new(MockAdapter)),
            ("llama", Arc::new(MockAdapter)),
        ],
    );
    for (model, engine) in [("strataline", "strata"), ("mockline", "mock")] {
        let (status, body) = post_json(
            &control,
            "/har/config/model",
            &format!(r#"{{"model":"{model}","cpu":true}}"#),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body}");
        assert_eq!(body["code"], "bad_request");
        let message = body["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("cpu_offload")
                && message.contains(engine)
                && message.contains("llama"),
            "the refusal must name key, row engine and the owning set: {message}"
        );
        let text = std::fs::read_to_string(modelconf.join(format!("{model}.yaml"))).expect("read");
        assert!(
            !text.contains("cpu_offload"),
            "a refused edit must leave the yaml free of the dead key: {text}"
        );
    }
    // The refusal is surgical: the category write still works on the same strata row.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"strataline","category":"AUX"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(std::fs::read_to_string(modelconf.join("strataline.yaml"))
        .expect("read")
        .contains("category: AUX"));
    // And the offload engine still accepts the cpu key.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"llamaline","cpu":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(std::fs::read_to_string(modelconf.join("llamaline.yaml"))
        .expect("read")
        .contains("cpu_offload: true"));
}

#[tokio::test]
async fn retagging_an_embedding_model_out_of_emb_keeps_it_in_the_catalog() {
    // FINDINGS-12 [1]: the page's category dropdown wrote only `category:`, so
    // retagging an `is_embedding_model: true` + `category: EMB` file to WORK left
    // the alias; the reload at the end of the same handler hit
    // EmbeddingCategoryConflict and DROPPED the file — the model 404s everywhere,
    // its row vanishes, and the endpoint answered success. The removal must ride
    // the same atomic rewrite as the category write.
    let (control, _, _, _, _, modelconf) = app(
        AppConfig::default(),
        &[("plain", "mock", CategoryId::Work, Some(4.0))],
    );
    let emb_file = modelconf.join("gemma-emb.yaml");
    std::fs::write(
        &emb_file,
        "# operator header\nname: gemma-emb\nengine: mock\nis_embedding_model: true\ncategory: EMB\nvram_gb: 2\n",
    )
    .expect("seed embedding fixture");
    let keep_file = modelconf.join("gemma-keep.yaml");
    std::fs::write(
        &keep_file,
        "name: gemma-keep\nengine: mock\nis_embedding_model: true\ncategory: EMB\n",
    )
    .expect("seed alias-keeper fixture");
    let plain_file = modelconf.join("plain.yaml");
    let plain_before = std::fs::read_to_string(&plain_file).expect("seed plain");

    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["models"], 3, "all fixtures catalogued: {body}");

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"gemma-emb","category":"WORK"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "retag must answer 2xx: {status} {body}"
    );

    let edited = std::fs::read_to_string(&emb_file).expect("read back");
    assert!(edited.contains("category: WORK"), "{edited}");
    assert_eq!(
        edited.matches("category:").count(),
        1,
        "exactly one category line: {edited}"
    );
    assert!(
        !edited.contains("is_embedding_model"),
        "the stale alias survives the retag: {edited}"
    );
    assert!(
        edited.starts_with("# operator header"),
        "operator bytes lost: {edited}"
    );

    // The survival claim, straight from the finding's falsifier: a fresh scan keeps
    // the model in WORK with no catalog error.
    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["models"], 3,
        "the retagged model must survive the scan: {body}"
    );
    assert!(
        body["catalog_errors"].as_array().is_some_and(Vec::is_empty),
        "no conflict after the retag: {body}"
    );
    let (_, models) = get_json(&control, "/har/models").await;
    let entry = models["data"]
        .as_array()
        .expect("list")
        .iter()
        .find(|m| m["id"] == "gemma-emb")
        .expect("model gone from the catalog after one dropdown click");
    assert_eq!(entry["category"], "WORK");

    // Retag back to EMB: survives, no conflict.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"gemma-emb","category":"EMB"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "retag back must answer 2xx: {status} {body}"
    );
    let (_, models) = get_json(&control, "/har/models").await;
    let entry = models["data"]
        .as_array()
        .expect("list")
        .iter()
        .find(|m| m["id"] == "gemma-emb")
        .expect("model must survive the retag back to EMB");
    assert_eq!(entry["category"], "EMB");

    // A retag to EMB must not needlessly strip a present alias: `category: EMB`
    // beside `is_embedding_model: true` is the legal shape.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"gemma-keep","category":"EMB"}"#,
    )
    .await;
    assert!(status.is_success(), "status: {status} {body}");
    let kept = std::fs::read_to_string(&keep_file).expect("read back");
    assert!(
        kept.contains("is_embedding_model: true"),
        "an EMB retag removed the operator's alias: {kept}"
    );

    // A non-embedding model retagged: its file changes only in the category line.
    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"plain","category":"AUX"}"#,
    )
    .await;
    assert!(status.is_success(), "status: {status} {body}");
    assert_eq!(
        std::fs::read_to_string(&plain_file).expect("read back"),
        plain_before.replace("category: WORK", "category: AUX"),
        "a non-embedding model's file must change only where it was asked"
    );
}

#[tokio::test]
async fn retagging_a_bom_prefixed_embedding_model_keeps_it_in_the_catalog() {
    // FINDINGS-13 [1], the finding's falsifier as a test: writeback's key matcher
    // did not strip a UTF-8 BOM while the scan parser does, so on a file whose
    // BOM'd FIRST line is `is_embedding_model: true` the batch retag treated the
    // removal as a no-op, published `category: WORK` beside the surviving alias,
    // answered `{"status":"edited"}` — and the next scan dropped the model with
    // EmbeddingCategoryConflict. Pre-fix this fails: the alias line survives the
    // retag and the rescan reports models=2 with the conflict naming the file.
    let (control, _, _, _, _, modelconf) = app(
        AppConfig::default(),
        &[("plain", "mock", CategoryId::Work, Some(4.0))],
    );
    let bom_file = modelconf.join("gemma-bom.yaml");
    std::fs::write(
        &bom_file,
        "\u{feff}is_embedding_model: true\nname: gemma-bom\nengine: mock\ncategory: EMB\nvram_gb: 2\n",
    )
    .expect("seed BOM'd embedding fixture");

    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["models"], 2, "all fixtures catalogued: {body}");

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"gemma-bom","category":"WORK"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "retag must answer 2xx: {status} {body}"
    );

    let edited = std::fs::read_to_string(&bom_file).expect("read back");
    assert!(
        !edited.contains("is_embedding_model"),
        "the alias on the BOM'd first line survives the retag: {edited}"
    );
    assert!(edited.contains("category: WORK"), "{edited}");
    assert_eq!(
        edited.matches("category:").count(),
        1,
        "exactly one category line: {edited}"
    );

    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["models"], 2,
        "the retagged model must survive the scan: {body}"
    );
    assert!(
        body["catalog_errors"].as_array().is_some_and(Vec::is_empty),
        "no conflict after the retag: {body}"
    );
    let (_, models) = get_json(&control, "/har/models").await;
    let entry = models["data"]
        .as_array()
        .expect("list")
        .iter()
        .find(|m| m["id"] == "gemma-bom")
        .expect("model dropped by the scan after a retag answered success");
    assert_eq!(entry["category"], "WORK");
}

#[tokio::test]
async fn moving_the_default_model_clears_its_old_category_default() {
    // Operator finding #1 (2026-10-26): moving the default model out of a category
    // must not leave the old listener defaulting to a model it no longer serves.
    let (control, _, _, _, _, _) = app(
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

/// Two yamls whose `model:` paths share one GGUF basename in different dirs —
/// the reviewer's `F:\...\MID\X.gguf` vs `F:\...\BIG\X.gguf` shape. Same-name
/// twins became unconstructable with file-stem identity (FINDINGS-13 [3]), so the
/// surviving ambiguity class is the alias tier of `catalog::resolve`: neither file
/// may win the shared GGUF name. The files carry no `name:` key at all.
fn seed_alias_twins(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let work = dir.join("twin-work.yaml");
    let aux = dir.join("twin-aux.yaml");
    std::fs::write(
        &work,
        "engine: mock\ncategory: WORK\nvram_gb: 4\nmodel: F:/MODELS_MID/twin.gguf\n",
    )
    .expect("seed twin-work");
    std::fs::write(
        &aux,
        "engine: mock\ncategory: AUX\nvram_gb: 4\nmodel: F:/MODELS_BIG/twin.gguf\n",
    )
    .expect("seed twin-aux");
    (work, aux)
}

async fn ambiguous_app() -> (
    Router,
    Router,
    Router,
    Router,
    MockRuntime,
    std::path::PathBuf,
) {
    let routers = app(
        AppConfig::default(),
        &[("filler", "mock", CategoryId::Work, Some(4.0))],
    );
    seed_alias_twins(&routers.5);
    // FINDINGS-18 [2]: the twins fixture gets a REAL config file on disk (the `app()`
    // sandbox path), so the "…and not_persisted" test can assert the claim its name
    // makes — the refused 409 left the operator's config bytes untouched — instead of
    // only reading the in-memory `/har/status` categories.
    std::fs::write(
        routers.5.join(TEST_CONFIG_NAME),
        r#"{"seeded": "pre-existing operator bytes"}"#,
    )
    .expect("seed config file");
    let (status, _) = post_json(&routers.0, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "both twins must be in the catalog");
    routers
}

#[tokio::test]
async fn load_of_an_ambiguous_gguf_alias_is_409_naming_both_yamls() {
    // FINDINGS-11 [4] survives stem identity on the alias tier: before the fix
    // this answered 200 having spawned whichever quantization sorted first, under
    // the identity the client asked for.
    let (control, _, _, _, runtime, _) = ambiguous_app().await;
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"twin"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["code"], "conflict", "flat code: {body}");
    assert_eq!(body["error"]["code"], "conflict", "envelope code: {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("ambiguous") && message.contains("`twin`") && message.contains('2'),
        "names the query and the count: {message}"
    );
    assert!(
        message.contains("twin-work.yaml") && message.contains("twin-aux.yaml"),
        "the operator must see BOTH colliding files, not the one that would have won: {message}"
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "an ambiguous alias spawns nothing"
    );
}

#[tokio::test]
async fn named_proxy_on_an_ambiguous_gguf_alias_is_the_same_409_not_a_misdirected_404() {
    // The twins straddle WORK and AUX: the old first-match-wins answer depended on
    // which yaml sorted first, and a wrong-category hit 404'd naming the OTHER
    // entry's public port. The client cannot fix that, so it is a 409.
    let (control, work, _, _, runtime, _) = ambiguous_app().await;
    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"model":"twin"}"#).await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["code"], "conflict");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("twin-work.yaml") && message.contains("twin-aux.yaml"),
        "both files named: {message}"
    );
    assert!(
        !message.contains("not found"),
        "never a misdirection: {message}"
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "an ambiguous alias ensures nothing"
    );

    // A no-`model` request never resolves a name, so the twins cannot affect it.
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"filler"}"#).await;
    assert_eq!(status, StatusCode::OK, "the unique model loads: {body}");
    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(status, StatusCode::OK, "no-model is unaffected: {body}");
}

#[tokio::test]
async fn a_name_contradicting_the_stem_is_dropped_at_scan_and_the_first_file_loads_by_stem() {
    // 6667c84's shared-name-handoff class, relocated by file-stem identity
    // (FINDINGS-13 [3]): a second file carrying the FIRST file's name can no
    // longer reach the catalog at all — the scan drops it with the InvalidName
    // mismatch, and the twin shape never arrives at the API to be picked.
    let (control, _, _, _, runtime, dir) = app(
        AppConfig::default(),
        &[("filler", "mock", CategoryId::Work, Some(4.0))],
    );
    std::fs::write(
        dir.join("twin-work.yaml"),
        "engine: mock\ncategory: WORK\nvram_gb: 4\nmodel: F:/MODELS_MID/twin-q4.gguf\n",
    )
    .expect("seed twin-work");
    std::fs::write(
        dir.join("twin-aux.yaml"),
        "name: twin-work\nengine: mock\ncategory: AUX\nvram_gb: 4\nmodel: F:/MODELS_BIG/twin-q8.gguf\n",
    )
    .expect("seed name-twin");
    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(
        body["models"], 2,
        "filler + the stem-named file only: {body}"
    );
    assert!(
        body["catalog_errors"]
            .as_array()
            .expect("errors")
            .iter()
            .any(|e| e["kind"] == "invalid_name"
                && e["file"]
                    .as_str()
                    .is_some_and(|f| f.contains("twin-aux.yaml"))
                && e["reason"]
                    == "name: \"twin-work\" does not match file twin-aux.yaml \u{2014} identity \
                        is the file stem; rename the file or delete the key"),
        "the second file must be dropped with the exact mismatch reason: {body}"
    );

    // The first file stays individually addressable by its stem, and the wrong
    // twin is gone rather than waiting behind a 409.
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"twin-work"}"#).await;
    assert_eq!(status, StatusCode::OK, "the stem loads: {body}");
    assert_eq!(runtime.server_count(), 1, "exactly one engine");
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"twin-aux"}"#).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "the dropped twin is unknown: {body}"
    );
}

#[tokio::test]
async fn colliding_gguf_file_names_are_409_on_the_alias_tier() {
    // Reviewer's example: distinct stems, one shared gguf file name across two
    // directories, so the alias tier is claimed twice.
    let (control, _, _, _, _, dir) = app(
        AppConfig::default(),
        &[("filler", "mock", CategoryId::Work, Some(4.0))],
    );
    let mid = dir.join("qwen3-mid.yaml");
    let big = dir.join("qwen3-big.yaml");
    std::fs::write(
        &mid,
        "engine: mock\ncategory: WORK\nvram_gb: 4\nmodel: F:/MODELS_MID/Qwen3-0.6B-Q8_0.gguf\n",
    )
    .expect("seed mid");
    std::fs::write(
        &big,
        "engine: mock\ncategory: WORK\nvram_gb: 4\nmodel: F:/MODELS_BIG/Qwen3-0.6B-Q8_0.gguf\n",
    )
    .expect("seed big");
    post_json(&control, "/har/rescan", "{}").await;

    for query in ["Qwen3-0.6B-Q8_0.gguf", "Qwen3-0.6B-Q8_0"] {
        let body = format!(r#"{{"model":"{query}"}}"#);
        let (status, reply) = post_json(&control, "/har/load", &body).await;
        assert_eq!(status, StatusCode::CONFLICT, "`{query}` body: {reply}");
        let message = reply["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("qwen3-mid.yaml") && message.contains("qwen3-big.yaml"),
            "`{query}` names both quantizations: {message}"
        );
    }

    // Each entry stays individually addressable by its own harness name.
    let (status, reply) = post_json(&control, "/har/load", r#"{"model":"qwen3-big"}"#).await;
    assert_eq!(status, StatusCode::OK, "a unique name still loads: {reply}");
}

#[tokio::test]
async fn setting_the_default_model_on_an_ambiguous_alias_is_refused_and_not_persisted() {
    // The write-back half of the finding: `/har/config/default` persists the name it
    // validated, so an ambiguity must stop BEFORE `persist_default_model`.
    // FINDINGS-18 [2]: the name promised a file-level claim the body never checked —
    // it read only the in-memory `/har/status` categories, and a regression that
    // answered 409 *and still rewrote the config file* passed. The fixture now owns a
    // seeded config file in the test's temp dir; the refusal must leave its bytes
    // untouched.
    let (control, _, _, _, _, dir) = ambiguous_app().await;
    let config_path = dir.join(TEST_CONFIG_NAME);
    let before = std::fs::read_to_string(&config_path).expect("seeded config exists");

    let (status, body) = post_json(
        &control,
        "/har/config/default",
        r#"{"category":"WORK","model":"twin"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("twin-work.yaml") && message.contains("twin-aux.yaml"),
        "both files named: {message}"
    );
    let (_, status_body) = get_json(&control, "/har/status").await;
    let work_cat = status_body["categories"]
        .as_array()
        .expect("categories")
        .iter()
        .find(|cat| cat["id"] == "WORK")
        .expect("WORK");
    assert!(
        work_cat["default_model"].is_null(),
        "an ambiguous alias never becomes a category default: {status_body}"
    );
    // The file half of the claim (the pre-fix hole: an in-memory-only refusal test
    // passed while the endpoint rewrote the operator's config).
    assert_eq!(
        std::fs::read_to_string(&config_path).expect("config file must still exist"),
        before,
        "a refused (409) default-set must leave the config file byte-identical"
    );
}

#[tokio::test]
async fn editing_an_ambiguous_alias_refuses_the_writeback_rather_than_picking_a_victim_file() {
    // Chunk 7 [9]: `/har/config/model` resolves a name to choose WHICH yaml to edit.
    // Under ambiguity it must refuse; editing the first match silently rewrites one
    // operator file for a model the client did not necessarily name.
    let (control, _, _, _, _, dir) = ambiguous_app().await;
    let (work_file, aux_file) = (
        std::fs::read_to_string(dir.join("twin-work.yaml")).expect("read twin-work"),
        std::fs::read_to_string(dir.join("twin-aux.yaml")).expect("read twin-aux"),
    );

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"twin","cpu":true}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "body: {body}");
    assert_eq!(body["code"], "conflict");
    assert_eq!(
        std::fs::read_to_string(dir.join("twin-work.yaml")).expect("read twin-work"),
        work_file,
        "the refused write must leave twin-work.yaml byte-identical"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("twin-aux.yaml")).expect("read twin-aux"),
        aux_file,
        "the refused write must leave twin-aux.yaml byte-identical"
    );
}

#[tokio::test]
async fn defaults_persist_to_the_config_boot_loaded_even_under_a_custom_path() {
    // Operator finding #2 (2026-10-26): write-back must hit the file boot read
    // (`SIMPLEHAR_CONFIG` aware), and a partial config with no `categories` key
    // must still accept the patch instead of silently staying in memory.
    let path = simplehar_se::testutil::temp_dir("api-config").join("simplehar-se.json");
    std::fs::write(&path, r#"{"modelconf_dir": "unused"}"#).expect("seed config");
    let mut config =
        AppConfig::parse(&std::fs::read_to_string(&path).expect("read")).expect("parses");
    config.source_path = Some(path.clone());

    let (control, _, _, _, _, _) = app(config, &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);
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
    let (control, _, _, _, _, _) = app(
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
    let (control, _, _, _, _, dir) =
        app(config, &[("filler", "mock", CategoryId::Work, Some(4.0))]);
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);

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

    // CSRF guard: a cross-site Origin on POST is refused 403. This assertion used to
    // read `StatusCode::CONFLICT`, which agreed with the implementation and quietly
    // overruled both the guard's documented contract and this comment; 409 tells a
    // client the state may change so retrying is reasonable, and a security refusal
    // must not say that (T13 is what caught it, 2026-10-27).
    let response = control
        .clone()
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
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("bounded body");
    let body: Value = serde_json::from_slice(&bytes).expect("error body");
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["code"], "forbidden", "flat code: {body}");
    assert_eq!(
        body["error"]["code"], "forbidden",
        "the OpenAI envelope carries the same code"
    );

    // FINDINGS-18 [3]: the same-authority arm. Browsers send `Origin` on SAME-origin
    // non-GET requests too, so a guard that refused every Origin-bearing POST — e.g.
    // comparing Origin against a portless Host — 403s every button on the control
    // page (unload, load, rescan, default picker) while every refusal-direction test
    // stays green. This is the only test anywhere that POSTs with a MATCHING Origin:
    // a valid unload-all (`{}`, nothing loaded) with Origin == Host must be ACCEPTED,
    // pinned to the exact code rather than `!= FORBIDDEN`.
    let same_origin = control
        .oneshot(
            Request::post("/har/unload")
                .header("host", "127.0.0.1:8708")
                .header("origin", "http://127.0.0.1:8708")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .expect("static request"),
        )
        .await
        .expect("in-process request");
    assert_eq!(
        same_origin.status(),
        StatusCode::OK,
        "a same-authority Origin must pass the CSRF guard, not 403 the operator's page"
    );
}

#[tokio::test]
async fn posts_without_json_content_type_are_415() {
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);

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
    let (control, work, _, _, runtime, _) = app(
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
    let (control, _, _, _, _, _) = app(AppConfig::default(), &[]);
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

#[tokio::test]
async fn no_model_fallback_with_fresh_instants_answers_200() {
    // FINDINGS-06 [1] shape check on a young process. Honest scope: at HEAD
    // every in-process `/har/load` goes through `swap_for_port`, which touches
    // the usage map, so the API cannot leave an occupant hint-free and this
    // request passes pre-fix too; the pre-fix panic (a fabricated
    // `Instant::now() - 3600s`, reachable when the ACTOR loads an occupant
    // without an API touch, e.g. a crash-reload after boot) is pinned by the
    // lib unit test `no_fabricated_instants_survive_short_uptime`.
    let (control, work, _, _, _, _modelconf) = app(
        AppConfig::default(),
        &[("qwen3", "mock", CategoryId::Work, Some(4.0))],
    );
    let (status, _) = post_json(&control, "/har/load", r#"{"model":"qwen3"}"#).await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = post_json(&work, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "no-model request must not die: {body}"
    );
    assert_eq!(body["echo"], true);
    assert_eq!(body["body"]["model"], "mock-model");
}

#[tokio::test]
async fn no_model_fallback_prefers_the_most_recently_proxied_occupant() {
    // F-2 MRU contract end to end. Honest scope: `swap_for_port` calls
    // `api.touch` on every in-process load, so the control API cannot leave an
    // occupant usage-hint-free; the untouched fallback branch (the pre-fix
    // panic site, reached in production when the ACTOR reloads an occupant
    // without an API touch) is pinned instead by the lib unit test
    // `no_fabricated_instants_survive_short_uptime`. What is observable here:
    // the genuine proxy touch on `newer` must outrank the later-loaded `older`
    // (whose mock answers 500) — a mis-selection is immediately visible.
    let mut config = AppConfig::default();
    let aux: &mut CategoryConfig = config
        .category_mut(CategoryId::Aux)
        .expect("default config has AUX");
    aux.max_models = 2;
    let (control, _, _, aux_router, runtime, _modelconf) = app(
        config,
        &[
            ("newer", "mock", CategoryId::Aux, Some(4.0)),
            ("older", "mock-error", CategoryId::Aux, Some(4.0)),
        ],
    );
    for name in ["newer", "older"] {
        let body = format!(r#"{{"model":"{name}"}}"#);
        let (status, body) = post_json(&control, "/har/load", &body).await;
        assert_eq!(status, StatusCode::OK, "load {name}: {body}");
    }
    assert_eq!(runtime.server_count(), 2, "both occupants serve");

    let (status, body) = post_json(
        &aux_router,
        "/v1/chat/completions",
        r#"{"model":"newer","messages":[]}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "named warm-up must reach `newer`: {body}"
    );

    let (status, body) = post_json(&aux_router, "/v1/chat/completions", r#"{"messages":[]}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "the touched `newer` must win the MRU sort over the later-loaded `older` \
         (whose mock answers 500): {body}"
    );
    assert_eq!(body["echo"], true);
    assert_eq!(body["body"]["model"], "mock-model");
    assert_eq!(
        runtime.server_count(),
        2,
        "selection must not reap anything"
    );
}

#[tokio::test]
async fn timeout_class_is_502_and_never_restarts() {
    // FINDINGS-06 [2] classifier half: a server that ACCEPTS but never answers
    // produces a real reqwest timeout - the class that used to mean "engine
    // died" and reaped healthy engines. It must map to a typed 502 naming the
    // class, with no restart in sight. The production proxy client's 600 s
    // deadline is hard-coded and not injectable without new public surface, so
    // the end-to-end 600 s wait is out of scope; the classifier is the decision
    // site and is exercised with the real error value here.
    let hanging = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("hang server binds");
    let port = hanging.local_addr().expect("hang addr").port();
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(100))
        .build()
        .expect("plain-HTTP client with a timeout");
    let error = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&serde_json::json!({"messages": []}))
        .send()
        .await
        .expect_err("the hang server never answers");
    assert!(
        error.is_timeout(),
        "accepted-but-silent must be timeout-class: {error:?}"
    );

    let err = classify_send_failure(&error, port).expect_err("timeouts never restart");
    assert_eq!(err.status(), StatusCode::BAD_GATEWAY, "exact class");
    let message = match err {
        simplehar_se::api::ApiError::Upstream { reason } => reason,
        other => panic!("expected Upstream, got {other:?}"),
    };
    assert!(
        message.contains(&format!("engine on :{port} timed out"))
            && message.contains("not restarting a live engine"),
        "reason must name the class and refuse the reap: {message}"
    );
}

#[tokio::test]
async fn dead_engine_connect_failure_restarts_exactly_once() {
    // FINDINGS-06 [2] restart-class proof, verified: the T10.1 mock death aborts
    // the axum serve task so the port REFUSES and `send()` yields
    // `is_connect()` (probe-verified: kind Request, source hyper Connect /
    // ConnectionRefused 10061 - see report). `mock-dies-once` makes only the
    // first engine a corpse and its replacement lives forever, so the crash-retry
    // is fully deterministic. (A 502 "retry exhausted" end-to-end case is
    // unattainable with this runtime: a dies-always engine cannot pass
    // `/har/load` readiness, so the connect arm can never fire twice.)
    let (control, work, _, _, runtime, _modelconf) = app(
        AppConfig::default(),
        &[("doomed", "mock-dies-once", CategoryId::Work, Some(4.0))],
    );
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"doomed"}"#).await;
    assert_eq!(status, StatusCode::OK, "load: {body}");
    assert_eq!(runtime.spawn_count(), 1);

    let (_, loaded) = get_json(&control, "/har/status").await;
    let port = u16::try_from(
        loaded["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|slot| slot["model"] == "doomed")
            .and_then(|slot| slot["port"].as_u64())
            .expect("the serving slot reports its engine port"),
    )
    .expect("port fits u16");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !runtime.is_dead(port).await && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(runtime.is_dead(port).await, "the engine is really dead");

    let (status, body) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"doomed","messages":[]}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "connect-class must crash-retry: {body}"
    );
    assert_eq!(body["echo"], true);
    assert_eq!(
        runtime.spawn_count(),
        2,
        "initial spawn + exactly one restart for the connect class"
    );
}

/// An `AppConfig` whose AUX category holds `slots` occupants (WORK/RP/EMB stay at 1).
fn aux_slots(slots: usize) -> AppConfig {
    let mut config = AppConfig::default();
    config
        .category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = slots;
    config
}

/// Polls until `runtime` reports exactly `want` live engines, and returns the count it
/// ended on. An unload answers `Done` when its job is STARTED, not when the kill lands
/// (`unload_slots` sends the reply before the spawned release completes), so the live
/// count settles shortly after the handler returns.
async fn await_server_count(runtime: &MockRuntime, want: usize) -> usize {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while runtime.server_count() != want && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    runtime.server_count()
}

#[tokio::test]
async fn retagging_one_loaded_aux_model_never_touches_the_other_aux_occupant() {
    // FINDINGS-07 [1]: `/har/config/model` is a single-model edit, but its retag branch
    // called `scheduler.unload(old_CATEGORY)` — "unloads every occupant of `category`".
    // With AUX at max_models 2..6, retagging one AUX model killed the others: in-flight
    // generations lost, VRAM dumped, cold reload for the operator's next request.
    let (control, _, _, aux_router, runtime, _) = app(
        aux_slots(2),
        &[
            ("aux1", "mock", CategoryId::Aux, Some(3.0)),
            ("aux2", "mock", CategoryId::Aux, Some(3.0)),
        ],
    );
    for name in ["aux1", "aux2"] {
        let body = format!(r#"{{"model":"{name}"}}"#);
        let (status, body) = post_json(&control, "/har/load", &body).await;
        assert_eq!(status, StatusCode::OK, "load {name}: {body}");
    }
    assert_eq!(await_server_count(&runtime, 2).await, 2, "both slots serve");

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"aux1","category":"WORK"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a retag never 500s: {body}");
    assert_eq!(body["status"], "edited", "response shape kept: {body}");
    assert_eq!(body["model"], "aux1", "response shape kept: {body}");
    assert_eq!(body["category"], "WORK", "response shape kept: {body}");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let (_, loaded) = get_json(&control, "/har/status").await;
        let slots = loaded["slots"].as_array().expect("slots");
        let survivor = slots
            .iter()
            .any(|slot| slot["model"] == "aux2" && slot["category"] == "AUX");
        let moved_away = !slots.iter().any(|slot| slot["model"] == "aux1");
        if survivor && moved_away {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("aux2 must keep its AUX slot while aux1's leaves it: {loaded}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        await_server_count(&runtime, 1).await,
        1,
        "only the retagged model's engine may die"
    );
    assert_eq!(runtime.spawn_count(), 2, "the survivor was never restarted");

    // The surviving engine is really alive, not merely listed: its listener answers.
    let (status, body) = post_json(
        &aux_router,
        "/v1/chat/completions",
        r#"{"model":"aux2","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "the untouched AUX serves: {body}");
    assert_eq!(body["echo"], true);
}

#[tokio::test]
async fn retagging_an_unloaded_aux_model_leaves_the_loaded_aux_occupant_alive() {
    // FINDINGS-07 [1], second half: the collateral unload fired on the CATEGORY CHANGE,
    // not on the model being loaded, so retagging a third AUX model that was never
    // loaded wiped the AUX occupants that were serving "for nothing".
    let (control, _, _, aux_router, runtime, _) = app(
        aux_slots(2),
        &[
            ("aux1", "mock", CategoryId::Aux, Some(3.0)),
            ("aux2", "mock", CategoryId::Aux, Some(3.0)),
            ("aux3", "mock", CategoryId::Aux, Some(3.0)),
        ],
    );
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"aux1"}"#).await;
    assert_eq!(status, StatusCode::OK, "load aux1: {body}");
    assert_eq!(await_server_count(&runtime, 1).await, 1, "one occupant");

    let (status, body) = post_json(
        &control,
        "/har/config/model",
        r#"{"model":"aux3","category":"WORK"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a retag never 500s: {body}");
    assert_eq!(body["status"], "edited");
    assert_eq!(body["category"], "WORK");

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    let (_, loaded) = get_json(&control, "/har/status").await;
    let slots = loaded["slots"].as_array().expect("slots");
    assert!(
        slots
            .iter()
            .any(|slot| slot["model"] == "aux1" && slot["category"] == "AUX"),
        "an edit to aux3 must not disturb the loaded aux1: {loaded}"
    );
    assert_eq!(
        runtime.server_count(),
        1,
        "the never-loaded retag kills nothing"
    );
    assert_eq!(runtime.spawn_count(), 1, "and restarts nothing");

    let (status, body) = post_json(
        &aux_router,
        "/v1/chat/completions",
        r#"{"model":"aux1","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "aux1 is still answering: {body}");
}

#[tokio::test]
async fn an_unknown_key_warns_on_the_page_and_the_model_still_loads() {
    // FINDINGS-13 [8]: the warning is real (visible in `catalog_errors`, `kind`
    // round-tripping through the JSON) and harmless (README doctrine: preserved
    // for reference — the entry publishes and loads).
    let (control, _, _, _, runtime, dir) = app(
        AppConfig::default(),
        &[("filler", "mock", CategoryId::Work, Some(4.0))],
    );
    std::fs::write(
        dir.join("typo.yaml"),
        "engine: mock\ncategory: WORK\nvram_gb: 4\ncontex: 4096\nhost: 0.0.0.0\nport: 8123\n",
    )
    .expect("seed typo fixture");
    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let errors = body["catalog_errors"].as_array().expect("errors");
    let typo = errors
        .iter()
        .find(|e| {
            e["kind"] == "unknown_key" && e["reason"].as_str().is_some_and(|r| r.contains("contex"))
        })
        .unwrap_or_else(|| panic!("the typo must be reported: {body}"));
    assert!(
        typo["reason"]
            .as_str()
            .is_some_and(|r| r.contains("(did you mean 'context'?)")),
        "one unique edit away: {typo}"
    );
    assert!(
        errors.iter().any(|e| e["kind"] == "unknown_key"
            && e["reason"].as_str()
                == Some("engine host is harness-forced (loopback, \u{a7}5) \u{2014} this key does nothing")),
        "the host policy hint must be exact: {body}"
    );
    assert!(
        errors.iter().any(|e| e["kind"] == "unknown_key"
            && e["reason"].as_str()
                == Some("engine port is harness-assigned from category config \u{2014} this key does nothing")),
        "the port policy hint must be exact: {body}"
    );

    let (status, body) = post_json(&control, "/har/load", r#"{"model":"typo"}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a warning never unmakes a model: {body}"
    );
    assert_eq!(runtime.server_count(), 1);
}

#[tokio::test]
async fn a_bogus_llama_server_exe_is_a_page_visible_engine_binary_missing_error() {
    // FINDINGS-13 [2]'s cross-check twin of the boot banner: the missing binary
    // is not only said once at boot, it is IN `catalog_errors` with its kind on
    // the wire (file `config`, kind `engine_binary_missing`).
    let config = AppConfig {
        llama_server_exe: std::path::PathBuf::from("Z:/definitely-not-here/llama-server.exe"),
        ..AppConfig::default()
    };
    let (control, _, _, _, _, _) = app(config, &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);
    let (status, body) = get_json(&control, "/har/status").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let error = body["catalog_errors"]
        .as_array()
        .expect("errors")
        .iter()
        .find(|e| e["kind"] == "engine_binary_missing")
        .unwrap_or_else(|| panic!("the missing binary must be page-visible: {body}"));
    assert_eq!(error["file"], "config");
    let reason = error["reason"].as_str().unwrap_or_default();
    assert!(
        reason.contains("llama-server.exe not found at Z:/definitely-not-here/llama-server.exe")
            && reason.contains("llama loads will fail"),
        "{reason}"
    );
}

#[tokio::test]
async fn a_missing_gguf_is_dropped_by_the_scan_and_named_on_the_page() {
    // FINDINGS-13 [2] through the API surface: a llama entry pointed at a GGUF
    // that is not on disk never reaches the catalog (so it cannot evict anything),
    // and `/har/rescan` names the file and the path with its kind.
    let (control, _, _, _, _, dir) = app(
        AppConfig::default(),
        &[("filler", "mock", CategoryId::Work, Some(4.0))],
    );
    std::fs::write(
        dir.join("doomed.yaml"),
        "engine: llama\nmodel: Z:/definitely-not-here/doomed.gguf\nvram_gb: 4\n",
    )
    .expect("seed doomed");
    let (status, body) = post_json(&control, "/har/rescan", "{}").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["models"], 1, "only the mock filler survived: {body}");
    let error = body["catalog_errors"]
        .as_array()
        .expect("errors")
        .iter()
        .find(|e| e["kind"] == "model_path_missing")
        .unwrap_or_else(|| panic!("the missing file must be named: {body}"));
    assert!(error["file"]
        .as_str()
        .is_some_and(|f| f.contains("doomed.yaml")));
    assert!(
        error["reason"]
            .as_str()
            .is_some_and(|r| r.starts_with("model file not found: ") && r.contains("doomed.gguf")),
        "{}",
        error["reason"]
    );

    let (status, body) = post_json(&control, "/har/load", r#"{"model":"doomed"}"#).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a dropped entry is unknown: {body}"
    );
}
