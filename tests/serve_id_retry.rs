//! FINDINGS-06 [8] falsifier: a `serve_id` discovery miss at readiness is
//! neither silent nor permanent. The install must WARN (naming the model and
//! what was tried), the load must still succeed, the first forward must keep
//! the documented harness-name fallback, and the next resolve must re-run the
//! discovery so a later forward carries the engine's real id. Pre-fix shape:
//! no line was logged, `serve_id` stayed `None` for the occupant's whole life,
//! and every proxied request carried the harness name.

mod mock_runtime;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use mock_runtime::{MockLateIdAdapter, MockRuntime};
use serde_json::Value;
use simplehar_se::api::{category_router, control_router, ApiState, BootTimings};
use simplehar_se::budget::LedgerOnlyWitness;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::listeners::ListenerRegistry;
use simplehar_se::scheduler::Scheduler;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, OnceLock};
use tower::ServiceExt;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::Layer;

/// Process-global line capture. Installed before this binary's first scheduler
/// exists: `tracing` caches callsite `Interest` on the first touch (the D9
/// flake in `GOTCHAS.md`), so the capture must stand before any emission on
/// the install-warn callsite — this file owns that callsite as its only test.
#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<String>>>);

struct LineVisitor<'a> {
    parts: &'a mut Vec<String>,
}

impl tracing::field::Visit for LineVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.parts.push(format!("{}={:?}", field.name(), value));
    }
}

impl<S: Subscriber> Layer<S> for Capture {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let mut parts = Vec::new();
        event.record(&mut LineVisitor { parts: &mut parts });
        self.0.lock().expect("capture lock").push(parts.join(" "));
    }
}

fn capture_lines() -> Arc<Mutex<Vec<String>>> {
    static STORE: OnceLock<Arc<Mutex<Vec<String>>>> = OnceLock::new();
    STORE
        .get_or_init(|| {
            let store = Arc::new(Mutex::new(Vec::new()));
            let _ = tracing_subscriber::registry()
                .with(Capture(store.clone()))
                .try_init();
            store
        })
        .clone()
}

/// Control and WORK listeners over one `late` model whose engine misses the
/// first `/v1/models` probe and reports `engine-late` on every probe after.
fn app() -> (Router, Router, std::path::PathBuf) {
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock-late-discovery".to_string(),
        Arc::new(MockLateIdAdapter {
            engine: "mock-late-discovery",
            serve_id: "engine-late",
        }),
    );
    let dir = simplehar_se::testutil::temp_dir("serve-id-retry");
    std::fs::write(
        dir.join("late.yaml"),
        "name: late\nengine: mock-late-discovery\ncategory: WORK\nvram_gb: 4.0\n",
    )
    .expect("model yaml");
    let config = AppConfig {
        source_path: Some(dir.join("simplehar-se.json")),
        ..AppConfig::default()
    };
    let scheduler = Scheduler::start(
        config.clone(),
        dir.clone(),
        adapters,
        Arc::new(MockRuntime::new()) as Arc<dyn EngineRuntime>,
        Arc::new(LedgerOnlyWitness),
    );
    let state = Arc::new(ApiState::new(
        config,
        scheduler,
        ListenerRegistry::new(),
        Arc::new(LedgerOnlyWitness),
        BootTimings::default(),
    ));
    (
        control_router(Arc::clone(&state)),
        category_router(state, CategoryId::Work),
        dir,
    )
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
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
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn a_discovery_miss_warns_and_the_next_resolve_carries_the_landed_id() {
    let lines = capture_lines();
    let (control, work, _dir) = app();

    // The load itself is never punished for the engine's silence.
    let (status, body) = post_json(&control, "/har/load", r#"{"model":"late"}"#).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a discovery miss never unmakes a load: {body}"
    );
    assert_eq!(body["status"], "loaded");
    assert!(
        body["receipt"]["serve_id"].is_null(),
        "the readiness discovery found nothing, and nothing is invented: {body}"
    );

    // The miss is loud: the install warn names the model AND what was tried.
    // Pre-fix this line did not exist — the finding's "nothing is logged".
    let warned = lines.lock().expect("capture lock").iter().any(|line| {
        line.contains("engine reported no model id")
            && line.contains("GET /v1/models")
            && line.contains("late")
    });
    assert!(
        warned,
        "no discovery-failure warn naming the model and the probe: {:?}",
        lines.lock().expect("capture lock")
    );

    // First proxy request: the documented harness-name fallback still rides
    // the wire, and the resolve arms the re-discovery.
    let (status, echo) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"late","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "proxy: {echo}");
    assert_eq!(
        echo["body"]["model"].as_str(),
        Some("late"),
        "the first forward keeps the harness-name fallback: {echo}"
    );

    // The retry lands off the request path: the slot gains the engine's real
    // id (pre-fix, `None` was the occupant's life sentence).
    let mut landed = None;
    for _ in 0..200 {
        let (_, status_body) = get_json(&control, "/har/status").await;
        if let Some(id) = status_body["slots"]
            .as_array()
            .and_then(|slots| slots.iter().find(|slot| slot["model"] == "late"))
            .and_then(|slot| slot["serve_id"].as_str())
        {
            landed = Some(id.to_string());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert_eq!(
        landed.as_deref(),
        Some("engine-late"),
        "the re-discovery never landed the engine id on the occupant"
    );

    // Second proxy request: the engine's own id, not the harness name.
    let (status, echo) = post_json(
        &work,
        "/v1/chat/completions",
        r#"{"model":"late","messages":[]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "proxy: {echo}");
    assert_eq!(
        echo["body"]["model"].as_str(),
        Some("engine-late"),
        "the forward still carries the harness name after the id landed: {echo}"
    );
}
