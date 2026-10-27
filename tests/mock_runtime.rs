//! In-process fake engine (DESIGN Step 19): real axum servers on ephemeral ports, so
//! the whole scheduler actor is testable without spawning processes or touching the
//! GPU. Behavior modes are selected by the `engine:` id the model carries: `mock`
//! (instant ready), `mock-slow` (3 s readiness delay), `mock-dies` (server stops
//! 300 ms after start), `mock-dies-once` (only the FIRST engine this runtime starts
//! stops, so a crash-retry can be observed end to end), `mock-never` (never ready),
//! `mock-hold-kill` (instant ready, but a kill request parks until the test calls
//! [`MockRuntime::release_held_kills`] — an engine whose terminate has not landed yet,
//! which is what makes a release in flight observable).

use async_trait::async_trait;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use simplehar_se::engine::{EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError, Spawned};
use simplehar_se::ownership::SpawnTicket;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How a mock engine behaves once started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockMode {
    /// `/health` answers immediately.
    Instant,
    /// `/health` sleeps this long before answering.
    DelayMs(u64),
    /// The server stops this long after start (the port refuses afterwards).
    DiesAfterMs(u64),
    /// Only the first engine this runtime starts dies; its replacement lives. This is
    /// what makes the T10.1 crash-retry observable: the corpse is real and the
    /// restart is real, in one request.
    DiesOnce,
    /// `/health` always reports not-loaded.
    Never,
    /// Every proxy target answers 500 (A9: a genuine upstream error is never
    /// retried).
    AlwaysError,
    /// The engine answers normally but a `kill` request parks until the test releases
    /// it: a terminate that has not landed yet, so a release still in flight is
    /// observable rather than a race.
    HoldKill,
}

fn mode_for(engine: &str) -> MockMode {
    match engine {
        "mock-slow" => MockMode::DelayMs(2_500),
        "mock-dies" => MockMode::DiesAfterMs(300),
        "mock-dies-once" => MockMode::DiesOnce,
        "mock-never" => MockMode::Never,
        "mock-error" => MockMode::AlwaysError,
        "mock-hold-kill" => MockMode::HoldKill,
        _ => MockMode::Instant,
    }
}

struct MockServer {
    mode: MockMode,
    handle: tokio::task::JoinHandle<()>,
}

/// Per-engine server state: behavior mode. The mock always reports its own fixed
/// id `mock-model` at `/v1/models`, so discovery and yaml mismatches are testable.
#[derive(Clone)]
struct MockState {
    mode: MockMode,
}

async fn models() -> Json<serde_json::Value> {
    Json(json!({ "object": "list", "data": [{ "id": "mock-model", "object": "model" }] }))
}

/// The test `EngineRuntime`: every "engine" is a real localhost HTTP server.
#[derive(Clone)]
pub struct MockRuntime {
    servers: Arc<Mutex<HashMap<u16, MockServer>>>,
    client: reqwest::Client,
    pid_base: Arc<Mutex<Option<u32>>>,
    spawn_counter: Arc<AtomicU32>,
    /// Latched by the first `DiesOnce` engine, so only the corpse is real.
    dies_once_used: Arc<AtomicBool>,
    /// Zero-permit gate every `HoldKill` kill waits on (see [`MockRuntime::release_held_kills`]).
    kill_gate: Arc<tokio::sync::Semaphore>,
}

impl Default for MockRuntime {
    fn default() -> Self {
        MockRuntime::new()
    }
}

impl MockRuntime {
    /// Creates the runtime with a shared probe client (4 s per attempt, so a delayed
    /// `/health` stays observable as "not yet" instead of erroring forever).
    pub fn new() -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(4_000))
            .build()
            .expect("invariant: a client with a timeout and no TLS needs no configuration");
        MockRuntime {
            servers: Arc::new(Mutex::new(HashMap::new())),
            client,
            pid_base: Arc::new(Mutex::new(None)),
            spawn_counter: Arc::new(AtomicU32::new(0)),
            dies_once_used: Arc::new(AtomicBool::new(false)),
            kill_gate: Arc::new(tokio::sync::Semaphore::new(0)),
        }
    }

    /// Lets every `HoldKill` engine parked inside `kill` actually die, so a test can
    /// hold a release in flight for exactly as long as it needs to observe it.
    #[allow(dead_code)]
    pub fn release_held_kills(&self) {
        self.kill_gate.add_permits(64);
    }

    /// Number of live engines (leak-check helper for F-1 / F-6). Used by the
    /// scheduler_actor test binary; other binaries sharing this module may not call it.
    #[allow(dead_code)]
    pub fn server_count(&self) -> usize {
        self.servers.lock().expect("mock runtime lock").len()
    }

    /// Number of engines this runtime has ever started, including ones that died.
    /// Crash-retry evidence: a real retry restarts, so the count grows while
    /// `server_count` stays at the live set.
    #[allow(dead_code)]
    pub fn spawn_count(&self) -> u32 {
        self.spawn_counter.load(Ordering::Relaxed)
    }

    /// Report unique pids (base, base+1, ...) per spawn instead of the default
    /// pid 0: witness scripts attribute own-engine usage by pid, so the
    /// measured-footprint tests pin attribution with a per-test runtime. Used
    /// by the scheduler_eviction binary only; other binaries see it as dead.
    #[allow(dead_code)]
    pub fn with_unique_pids(base: u32) -> Self {
        let runtime = MockRuntime::new();
        *runtime.pid_base.lock().expect("pid base lock") = Some(base);
        runtime
    }

    /// Whether nothing accepts on `port` anymore (release evidence, G12). Used by the
    /// scheduler_actor test binary; other binaries sharing this module may not call it.
    #[allow(dead_code)]
    pub async fn is_dead(&self, port: u16) -> bool {
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err()
    }
}

async fn health(
    State(state): State<MockState>,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    let mode = state.mode;
    if let MockMode::DelayMs(ms) = mode {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
    let loaded = !matches!(mode, MockMode::Never);
    let status = if loaded {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "status": "ok", "loaded": loaded })))
}

/// Proxy echo: any other path answers 200 with the request shape, so proxy tests can
/// assert what the harness forwarded (including the rewritten `model` field).
async fn echo(
    State(state): State<MockState>,
    uri: axum::http::Uri,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    // A9 test support: this mode is a genuinely-erroring upstream.
    if state.mode == MockMode::AlwaysError {
        return (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            "upstream says no",
        )
            .into_response();
    }
    Json(json!({
        "echo": true,
        "path": uri.path(),
        "body": body.map(|Json(value)| value).unwrap_or(serde_json::Value::Null),
    }))
    .into_response()
}

/// Aborts the server on `port` after `ms`, making the port refuse connections.
fn schedule_death(servers: Arc<Mutex<HashMap<u16, MockServer>>>, port: u16, ms: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        if let Some(server) = servers.lock().expect("mock runtime lock").remove(&port) {
            server.handle.abort();
        }
    });
}

#[async_trait]
impl EngineRuntime for MockRuntime {
    async fn spawn(&self, spec: &LaunchSpec, _ticket: SpawnTicket) -> Result<Spawned, SpawnError> {
        let mode = mode_for(
            spec.env
                .get("MOCK_ENGINE")
                .map(String::as_str)
                .unwrap_or("mock"),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| SpawnError::SpawnFailed {
                reason: e.to_string(),
            })?;
        let port = listener
            .local_addr()
            .map_err(|e| SpawnError::SpawnFailed {
                reason: e.to_string(),
            })?
            .port();
        let app = Router::new()
            .route("/health", get(health))
            .route("/v1/models", get(models))
            .fallback(echo)
            .with_state(MockState { mode: mode.clone() });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.servers.lock().expect("mock runtime lock").insert(
            port,
            MockServer {
                mode: mode.clone(),
                handle,
            },
        );
        let spawns = self.spawn_counter.fetch_add(1, Ordering::Relaxed);
        match mode {
            MockMode::DiesAfterMs(ms) => schedule_death(Arc::clone(&self.servers), port, ms),
            // Only the first engine dies, so the crash-retry has a live replacement to
            // answer from; no later spawn of this runtime is ever the corpse again.
            MockMode::DiesOnce if !self.dies_once_used.swap(true, Ordering::Relaxed) => {
                // Generous on purpose: the timer starts at spawn, before the readiness
                // probe and `/v1/models` discovery have run, and the crash-retry test
                // needs the engine to die after it is serving, not during its load.
                schedule_death(Arc::clone(&self.servers), port, 1_500);
            }
            _ => {}
        }
        let pid = match *self.pid_base.lock().expect("pid base lock") {
            Some(base) => base + spawns,
            None => 0,
        };
        Ok(Spawned { port, pid })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let held = self
            .servers
            .lock()
            .expect("mock runtime lock")
            .get(&ticket.port)
            .is_some_and(|server| server.mode == MockMode::HoldKill);
        if held {
            let _permit = self
                .kill_gate
                .acquire()
                .await
                .expect("invariant: the kill gate is never closed");
        }
        if let Some(server) = self
            .servers
            .lock()
            .expect("mock runtime lock")
            .remove(&ticket.port)
        {
            server.handle.abort();
        }
        Ok(())
    }

    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        if self
            .servers
            .lock()
            .expect("mock runtime lock")
            .contains_key(&ticket.port)
        {
            return Ok(false);
        }
        let port = ticket.port;
        let closed = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_err();
        Ok(closed)
    }

    async fn probe(&self, port: u16, readiness: &ReadinessSpec) -> Probe {
        simplehar_se::engine::http_probe(&self.client, port, readiness).await
    }

    async fn discover_model_id(&self, port: u16) -> Option<String> {
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let response = self.client.get(&url).send().await.ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        body["data"][0]["id"].as_str().map(str::to_string)
    }
}
