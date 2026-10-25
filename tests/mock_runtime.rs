//! In-process fake engine (DESIGN Step 19): real axum servers on ephemeral ports, so
//! the whole scheduler actor is testable without spawning processes or touching the
//! GPU. Behavior modes are selected by the `engine:` id the model carries: `mock`
//! (instant ready), `mock-slow` (3 s readiness delay), `mock-dies` (server stops
//! 300 ms after start), `mock-never` (never ready).

use async_trait::async_trait;
use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use simplehar_se::engine::{EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError, Spawned};
use simplehar_se::ownership::SpawnTicket;
use std::collections::HashMap;
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
    /// `/health` always reports not-loaded.
    Never,
}

fn mode_for(engine: &str) -> MockMode {
    match engine {
        "mock-slow" => MockMode::DelayMs(2_500),
        "mock-dies" => MockMode::DiesAfterMs(300),
        "mock-never" => MockMode::Never,
        _ => MockMode::Instant,
    }
}

struct MockServer {
    #[allow(dead_code)]
    mode: MockMode,
    handle: tokio::task::JoinHandle<()>,
}

/// The test `EngineRuntime`: every "engine" is a real localhost HTTP server.
#[derive(Clone)]
pub struct MockRuntime {
    servers: Arc<Mutex<HashMap<u16, MockServer>>>,
    client: reqwest::Client,
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
        }
    }

    /// Number of live engines (leak-check helper for F-1 / F-6). Used by the
    /// scheduler_actor test binary; other binaries sharing this module may not.
    #[allow(dead_code)]
    pub fn server_count(&self) -> usize {
        self.servers.lock().expect("mock runtime lock").len()
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

async fn health(State(mode): State<MockMode>) -> (axum::http::StatusCode, Json<serde_json::Value>) {
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
    uri: axum::http::Uri,
    body: Option<Json<serde_json::Value>>,
) -> Json<serde_json::Value> {
    Json(json!({
        "echo": true,
        "path": uri.path(),
        "body": body.map(|Json(value)| value).unwrap_or(serde_json::Value::Null),
    }))
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
            .fallback(echo)
            .with_state(mode.clone());
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        if let MockMode::DiesAfterMs(ms) = mode {
            let servers = Arc::clone(&self.servers);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                if let Some(server) = servers.lock().expect("mock runtime lock").remove(&port) {
                    server.handle.abort();
                }
            });
        }
        self.servers
            .lock()
            .expect("mock runtime lock")
            .insert(port, MockServer { mode, handle });
        Ok(Spawned { port, pid: 0 })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
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
}
