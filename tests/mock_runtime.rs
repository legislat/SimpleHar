//! In-process fake engine (DESIGN Step 19): real axum servers on ephemeral ports, so
//! the whole scheduler actor is testable without spawning processes or touching the
//! GPU. Behavior modes are selected by the `engine:` id the model carries: `mock`
//! (instant ready), `mock-slow` (2.5 s readiness delay), `mock-dies` (server stops
//! 300 ms after start), `mock-dies-once` (only the FIRST engine this runtime starts
//! stops, so a crash-retry can be observed end to end), `mock-never` (never ready),
//! `mock-hold-kill` (instant ready, but a kill request parks until the test calls
//! [`MockRuntime::release_held_kills`] — an engine whose terminate has not landed yet,
//! which is what makes a release in flight observable), `mock-hold-never` (never ready
//! AND its kill parks: the spawn-failure path with the reap held, so the funnel's
//! `Failed` park is an observable state), and `mock-nodiscovery` (instant ready but
//! `/v1/models` 404s: the silent-engine discovery class), and `mock-late-discovery`
//! (404s the FIRST `/v1/models` probe only, so a discovery miss at readiness is
//! observable recovering on a later resolve — FINDINGS-06 [8]). The id an engine
//! reports at
//! `/v1/models` comes from `spec.env["MOCK_MODEL_ID"]` (default `mock-model`), so a
//! test can give each engine a DISTINCT identity via [`MockIdAdapter`].

use async_trait::async_trait;
use axum::extract::State;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use simplehar_se::engine::{
    EngineAdapter, EngineError, EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError,
    Spawned,
};
use simplehar_se::model::ModelEntry;
use simplehar_se::ownership::SpawnTicket;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
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
    /// `/health` never reports loaded AND the kill parks: a spawn that fails its
    /// readiness with the reap held, so the job's park in `SwapState::Failed` (the
    /// exit funnel's cleanup-pending state) is a state the test observes rather than
    /// a scheduling race past a single step.
    HoldKillNever,
    /// `/health` answers immediately but `/v1/models` 404s: the silent-engine class
    /// where `discover_model_id` finds nothing and the harness must not invent an id.
    NeverDiscover,
    /// `/health` answers immediately and `/v1/models` 404s for the FIRST probe only
    /// (FINDINGS-06 [8]): discovery misses at readiness, and only a later re-probe
    /// sees the id — the shape that makes a serve-id retry observable.
    LateDiscover,
}

fn mode_for(engine: &str) -> MockMode {
    match engine {
        "mock-slow" => MockMode::DelayMs(2_500),
        "mock-dies" => MockMode::DiesAfterMs(300),
        "mock-dies-once" => MockMode::DiesOnce,
        "mock-never" => MockMode::Never,
        "mock-error" => MockMode::AlwaysError,
        "mock-hold-kill" => MockMode::HoldKill,
        "mock-hold-never" => MockMode::HoldKillNever,
        "mock-nodiscovery" => MockMode::NeverDiscover,
        "mock-late-discovery" => MockMode::LateDiscover,
        _ => MockMode::Instant,
    }
}

/// The exact bytes a `stream: true` chat completion answers with. The proxy contract
/// is byte-for-byte pass-through (headers included), so the streaming test declares
/// this same literal and asserts equality on the wire (FINDINGS-18 [4]).
pub const SSE_BODY: &str = "data: {\"chunk\":1}\n\ndata: {\"chunk\":2}\n\n";

struct MockServer {
    mode: MockMode,
    /// `None` while a death is in flight: the entry stays in the table until the
    /// `axum::serve` future has really been dropped, so `release_confirmed` can never
    /// answer `true` while the port still completes handshakes (FINDINGS-20 [7]).
    handle: Option<tokio::task::JoinHandle<()>>,
}

/// One kill currently parked on the kill gate: recorded while it waits and removed
/// when its future ends, by any exit (FINDINGS-19 [4]).
struct ParkedKill {
    parked: Arc<Mutex<Vec<(u16, u64)>>>,
    port: u16,
    generation: u64,
}

impl ParkedKill {
    fn new(parked: Arc<Mutex<Vec<(u16, u64)>>>, port: u16, generation: u64) -> Self {
        parked
            .lock()
            .expect("parked kills lock")
            .push((port, generation));
        ParkedKill {
            parked,
            port,
            generation,
        }
    }
}

impl Drop for ParkedKill {
    fn drop(&mut self) {
        self.parked
            .lock()
            .expect("parked kills lock")
            .retain(|(p, g)| *p != self.port || *g != self.generation);
    }
}

/// Per-engine server state: behavior mode and the id it reports at `/v1/models`
/// (`None` = the historical fixed `mock-model`, so pre-existing assertions are
/// unchanged unless a test plants a distinct id via [`MockIdAdapter`]).
#[derive(Clone)]
struct MockState {
    mode: MockMode,
    model_id: Option<String>,
    /// Remaining `/v1/models` probes that answer 404 before this engine starts
    /// reporting its id (FINDINGS-06 [8]: the `LateDiscover` class).
    discover_failures: Arc<AtomicU32>,
}

async fn models(State(state): State<MockState>) -> axum::response::Response {
    let misses_discovery = state.mode == MockMode::NeverDiscover
        || state
            .discover_failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok();
    if misses_discovery {
        // The silent engine: `/v1/models` answers nothing usable, so discovery
        // yields `None` and the proxy must fall back to the harness name.
        return axum::http::StatusCode::NOT_FOUND.into_response();
    }
    let id = state.model_id.unwrap_or_else(|| "mock-model".to_string());
    Json(json!({ "object": "list", "data": [{ "id": id, "object": "model" }] })).into_response()
}

/// The test `EngineRuntime`: every "engine" is a real localhost HTTP server.
#[derive(Clone)]
pub struct MockRuntime {
    servers: Arc<Mutex<HashMap<u16, MockServer>>>,
    client: reqwest::Client,
    spawn_counter: Arc<AtomicU32>,
    /// Latched by the first `DiesOnce` engine, so only the corpse is real.
    dies_once_used: Arc<AtomicBool>,
    /// Zero-permit gate every `HoldKill` kill waits on (see [`MockRuntime::release_held_kills`]).
    kill_gate: Arc<tokio::sync::Semaphore>,
    /// How many further `HoldKill` kills park on that gate. Unlimited by default, so
    /// every held kill behaves as it always did; a test that needs one release it
    /// cannot prove *followed* by one it can (a cancelled swap, then the operator's
    /// unload of what the cancel left installed) lowers this instead of racing the
    /// release deadline. See [`MockRuntime::hold_next_kills`].
    kill_holds: Arc<AtomicUsize>,
    /// Ports whose kills always park (FINDINGS-19 [4]): hold by TARGET rather than
    /// by count, so a change in how many kills the product issues cannot silently
    /// re-arm or disarm the fake. See [`MockRuntime::hold_kill_of`].
    hold_ports: Arc<Mutex<BTreeSet<u16>>>,
    /// Every kill currently parked: `(port, generation)` of its ticket. The tests
    /// assert this SET (which kills the product really issued and left unproven)
    /// instead of trusting the fake's budget arithmetic (FINDINGS-19 [4]).
    parked: Arc<Mutex<Vec<(u16, u64)>>>,
    /// R9 fake seam: canned `docker logs` text per container name, returned by
    /// [`EngineRuntime::container_report`] (tests seed the startup `MemorySummary`).
    container_reports: Arc<Mutex<HashMap<String, String>>>,
    /// R9 fake seam: every `(model, text)` handed to `save_container_log`, for
    /// asserting a failed launch snapshotted its container log.
    saved_container_logs: Arc<Mutex<Vec<(String, String)>>>,
    /// Ruling 1(b) fake seam: canned publish evidence returned by
    /// [`EngineRuntime::published_elsewhere`], keyed by container name.
    publish_evidence: Arc<Mutex<HashMap<String, String>>>,
    /// Every `(container, offered)` the scheduler asked about, in order: the
    /// pins assert the happy path and the container-less failure never ask.
    publish_calls: Arc<Mutex<Vec<(String, u16)>>>,
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
            spawn_counter: Arc::new(AtomicU32::new(0)),
            dies_once_used: Arc::new(AtomicBool::new(false)),
            kill_gate: Arc::new(tokio::sync::Semaphore::new(0)),
            kill_holds: Arc::new(AtomicUsize::new(usize::MAX)),
            hold_ports: Arc::new(Mutex::new(BTreeSet::new())),
            parked: Arc::new(Mutex::new(Vec::new())),
            container_reports: Arc::new(Mutex::new(HashMap::new())),
            saved_container_logs: Arc::new(Mutex::new(Vec::new())),
            publish_evidence: Arc::new(Mutex::new(HashMap::new())),
            publish_calls: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Seeds the canned `docker logs` text [`EngineRuntime::container_report`] returns
    /// for `name` (R9). Call before driving the scheduler.
    #[allow(dead_code)]
    pub fn set_container_report(&self, name: &str, text: &str) {
        self.container_reports
            .lock()
            .expect("container report lock")
            .insert(name.to_string(), text.to_string());
    }

    /// Every `(model, text)` the scheduler asked to snapshot, in order (R9).
    #[allow(dead_code)]
    pub fn saved_container_logs(&self) -> Vec<(String, String)> {
        self.saved_container_logs
            .lock()
            .expect("saved container lock")
            .clone()
    }

    /// Seeds the canned publish evidence [`EngineRuntime::published_elsewhere`]
    /// returns for `name` (ruling 1(b)). Call before driving the scheduler.
    #[allow(dead_code)]
    pub fn set_publish_evidence(&self, name: &str, evidence: &str) {
        self.publish_evidence
            .lock()
            .expect("publish evidence lock")
            .insert(name.to_string(), evidence.to_string());
    }

    /// Every `(container, offered)` publish diagnosis the scheduler asked for,
    /// in order (ruling 1(b)).
    #[allow(dead_code)]
    pub fn publish_diagnoses(&self) -> Vec<(String, u16)> {
        self.publish_calls
            .lock()
            .expect("publish calls lock")
            .clone()
    }

    /// Lets every `HoldKill` engine parked inside `kill` actually die, so a test can
    /// hold a release in flight for exactly as long as it needs to observe it.
    #[allow(dead_code)]
    pub fn release_held_kills(&self) {
        self.kill_gate.add_permits(64);
    }

    /// Parks at most `n` further `HoldKill` kills; every later one dies normally.
    /// Call it before driving the scheduler, so a test holds an *unprovable release*
    /// as a state rather than as a race against `release_timeout_s`: the kills that
    /// must stay unconfirmed park forever, and the kill the test wants proven is not
    /// held at all and confirms inside its own deadline.
    #[allow(dead_code)]
    pub fn hold_next_kills(&self, n: usize) {
        self.kill_holds.store(n, Ordering::SeqCst);
    }

    /// Parks EVERY further `kill` of `port` until the gate is released, by target
    /// rather than by count (FINDINGS-19 [4]): the fake's parking no longer depends on
    /// how many kills the product happens to issue, so a correct change to the reap
    /// path (dedup, re-order) cannot silently re-arm the scenario.
    #[allow(dead_code)]
    pub fn hold_kill_of(&self, port: u16) {
        self.hold_ports
            .lock()
            .expect("hold ports lock")
            .insert(port);
    }

    /// The `(port, generation)` of every kill currently parked: the SET the test
    /// expects to be held (FINDINGS-19 [4]). An entry is removed when its kill
    /// proceeds, so this names the releases the product left unproven right now.
    #[allow(dead_code)]
    pub fn held_kills(&self) -> Vec<(u16, u64)> {
        self.parked.lock().expect("parked kills lock").clone()
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
    let loaded = !matches!(mode, MockMode::Never | MockMode::HoldKillNever);
    let status = if loaded {
        axum::http::StatusCode::OK
    } else {
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    };
    (status, Json(json!({ "status": "ok", "loaded": loaded })))
}

/// Exact bytes a `mock_encoded_reply: true` request answers with: opaque bytes
/// labelled `content-encoding: gzip`. The proxy decodes nothing, so the pass-through
/// contract demands these same bytes and that header reach the client untouched
/// (FINDINGS-06 [9]).
pub const ENCODED_BODY: &[u8] = b"\x1f\x8b\x08\x00opaque-not-really-gzip\x00\xff";

/// Proxy echo: any other path answers 200 with the request shape, so proxy tests can
/// assert what the harness forwarded (including the rewritten `model` field). The
/// echoed `path` is the full request target INCLUDING the query string (FINDINGS-06
/// [10]): the test asserts what the engine actually received, not what the proxy
/// chose to reveal. A body carrying `"stream": true` is answered with the fixed SSE
/// literal and a `text/event-stream` content-type, so the proxy's pass-through
/// contract (byte-for-byte body + preserved content-type, FINDINGS-18 [4]) has
/// something real to assert. A body carrying `"mock_encoded_reply": true` is answered
/// with [`ENCODED_BODY`] under `content-encoding: gzip`. A body carrying
/// `"mock_hop_by_hop": true` is answered with one of every hop-by-hop response
/// header (plus a `connection` value naming one more) beside an ordinary marker
/// header, so the proxy's engine→client strip list has a real emitter to pin
/// against (R2-06-02).
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
    if body.as_ref().is_some_and(|Json(value)| {
        value.get("stream").and_then(serde_json::Value::as_bool) == Some(true)
    }) {
        return (
            [
                (
                    axum::http::header::CONTENT_TYPE,
                    "text/event-stream; charset=utf-8",
                ),
                (axum::http::header::CACHE_CONTROL, "no-cache"),
            ],
            SSE_BODY,
        )
            .into_response();
    }
    if body.as_ref().is_some_and(|Json(value)| {
        value
            .get("mock_encoded_reply")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }) {
        return (
            [
                (axum::http::header::CONTENT_TYPE, "application/json"),
                (axum::http::header::CONTENT_ENCODING, "gzip"),
            ],
            ENCODED_BODY,
        )
            .into_response();
    }
    // R2-06-02 test support: an engine replying with one of every RFC 9110
    // hop-by-hop response header, a `connection` value enumerating a further
    // header name, and one ordinary header that must survive the harness strip.
    if body.as_ref().is_some_and(|Json(value)| {
        value
            .get("mock_hop_by_hop")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
    }) {
        return (
            [
                ("content-type", "application/json"),
                ("x-engine-marker", "kept"),
                ("keep-alive", "timeout=5"),
                ("upgrade", "websocket"),
                ("te", "trailers"),
                ("trailer", "X-Checksum"),
                ("proxy-authenticate", "Basic realm=\"hop\""),
                ("proxy-authorization", "Basic aG9wOnNlbnRyeQ=="),
                ("connection", "keep-alive, X-Hop-Named"),
                ("x-hop-named", "must-not-reach-the-client"),
            ],
            r#"{"hop_by_hop":true}"#,
        )
            .into_response();
    }
    let target = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string());
    Json(json!({
        "echo": true,
        "path": target,
        "body": body.map(|Json(value)| value).unwrap_or(serde_json::Value::Null),
    }))
    .into_response()
}

/// Aborts the server on `port` after `ms`, making the port refuse connections.
/// Awaits the aborted `axum::serve` future exactly as `kill` does (FINDINGS-20 [7]):
/// until the drop lands the entry stays in the table, so the fake never calls an
/// engine "gone" while its listener is still completing handshakes.
fn schedule_death(servers: Arc<Mutex<HashMap<u16, MockServer>>>, port: u16, ms: u64) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(ms)).await;
        let handle = {
            let mut table = servers.lock().expect("mock runtime lock");
            table.get_mut(&port).and_then(|server| server.handle.take())
        };
        if let Some(handle) = handle {
            handle.abort();
            let _gone = handle.await;
        }
        servers.lock().expect("mock runtime lock").remove(&port);
    });
}

#[async_trait]
impl EngineRuntime for MockRuntime {
    async fn spawn(&self, spec: &LaunchSpec, _ticket: SpawnTicket) -> Result<Spawned, SpawnError> {
        let engine = spec
            .env
            .get("MOCK_ENGINE")
            .map(String::as_str)
            .unwrap_or("mock");
        let mode = mode_for(engine);
        let model_id = spec.env.get("MOCK_MODEL_ID").cloned();
        let discover_failures = Arc::new(AtomicU32::new(
            spec.env
                .get("MOCK_DISCOVERY_FAILURES")
                .and_then(|n| n.parse().ok())
                .unwrap_or(0),
        ));
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
            .with_state(MockState {
                mode: mode.clone(),
                model_id,
                discover_failures,
            });
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        self.servers.lock().expect("mock runtime lock").insert(
            port,
            MockServer {
                mode: mode.clone(),
                handle: Some(handle),
            },
        );
        self.spawn_counter.fetch_add(1, Ordering::Relaxed);
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
        // No pid fiction: the fake owns no process, and nothing attributes by pid
        // since the 20[6] serve-id work — the witness keys on ports and ids.
        let pid = 0;
        Ok(Spawned { port, pid })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let holdkill = self
            .servers
            .lock()
            .expect("mock runtime lock")
            .get(&ticket.port)
            .is_some_and(|server| {
                matches!(server.mode, MockMode::HoldKill | MockMode::HoldKillNever)
            });
        // A `HoldKill` engine parks while the test still holds kills for it: either
        // by TARGET (`hold_kill_of`: every kill of this port parks, so the product's
        // kill COUNT cannot re-arm the fake — FINDINGS-19 [4]) or by the legacy
        // `hold_next_kills` budget a still-unmigrated scenario relies on.
        let held_by_target = holdkill
            && self
                .hold_ports
                .lock()
                .expect("hold ports lock")
                .contains(&ticket.port);
        let holds_one = held_by_target
            || (holdkill
                && self
                    .kill_holds
                    .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                    .is_ok());
        if holds_one {
            // RAII: the entry is dropped from `held_kills` when the kill proceeds OR
            // when the product's reap deadline expires and this whole future is
            // cancelled — the reported set always names the kills parked RIGHT NOW.
            let _parked = ParkedKill::new(Arc::clone(&self.parked), ticket.port, ticket.generation);
            let _permit = self
                .kill_gate
                .acquire()
                .await
                .expect("invariant: the kill gate is never closed");
        }
        // Take the handle under the lock, but abort + await OUTSIDE it (the map lock
        // must not be held across the cancellation). The entry is removed only after
        // the `axum::serve` future has been dropped: a `release_confirmed` of `true`
        // therefore always means the port has really stopped answering, never that
        // the fake merely forgot the engine (FINDINGS-20 [7]). A handle already taken
        // by `schedule_death` means that task owns the removal — the kill of a
        // self-dying engine is a no-op, and the entry disappears with the drop.
        let handle = {
            let mut table = self.servers.lock().expect("mock runtime lock");
            table
                .get_mut(&ticket.port)
                .and_then(|server| server.handle.take())
        };
        if let Some(handle) = handle {
            handle.abort();
            let _gone = handle.await;
            self.servers
                .lock()
                .expect("mock runtime lock")
                .remove(&ticket.port);
        }
        Ok(())
    }

    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        // The runtime's own bookkeeping is the release evidence: `kill` clears the
        // entry only after its server has been aborted and reaped, and a held kill
        // leaves the entry in place — exactly the "terminate has not landed yet" state
        // a test needs to observe (chunk 4 [1]). Re-probing the port here instead made
        // the answer depend on when the runtime happened to drop the listener of a task
        // it had just cancelled: the kernel kept completing handshakes into a backlog
        // nobody serviced, so the connect hung past the release deadline of an engine
        // that was already dead. `is_dead` keeps that TCP check for the tests that want
        // the port-level claim — and the death paths now await the drop BEFORE the
        // entry disappears, so the two claims cannot disagree (FINDINGS-20 [7]).
        let live = self
            .servers
            .lock()
            .expect("mock runtime lock")
            .contains_key(&ticket.port);
        Ok(!live)
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

    async fn container_report(&self, container: &str) -> Option<String> {
        self.container_reports
            .lock()
            .expect("container report lock")
            .get(container)
            .cloned()
    }

    async fn published_elsewhere(&self, container: &str, offered: u16) -> Option<String> {
        self.publish_calls
            .lock()
            .expect("publish calls lock")
            .push((container.to_string(), offered));
        self.publish_evidence
            .lock()
            .expect("publish evidence lock")
            .get(container)
            .cloned()
    }

    async fn save_container_log(&self, model: &str, text: &str) {
        self.saved_container_logs
            .lock()
            .expect("saved container lock")
            .push((model.to_string(), text.to_string()));
    }
}

/// A `MockAdapter` under a caller-chosen engine id that reports a caller-chosen
/// `id` at `/v1/models` (FINDINGS-20 [6]). One fixed id for every engine made every
/// `serve_id` assertion compare the same constant, so a product bug that forwarded
/// slot A's id for slot B's request was invisible; distinct ids per engine make the
/// identity per-slot again.
#[derive(Debug, Clone)]
#[allow(dead_code)] // binaries that never script discovery ids still compile this module
pub struct MockIdAdapter {
    /// The `engine:` id the yaml must carry to select this adapter.
    pub engine: &'static str,
    /// The id this engine reports at `/v1/models`.
    pub serve_id: &'static str,
}

impl EngineAdapter for MockIdAdapter {
    fn engine_id(&self) -> &str {
        self.engine
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let mut spec = simplehar_se::engine::mock::MockAdapter.spec(model, port)?;
        spec.env
            .insert("MOCK_MODEL_ID".to_string(), self.serve_id.to_string());
        Ok(spec)
    }
}

/// A `MockAdapter` whose engine misses the FIRST `/v1/models` discovery and
/// reports `serve_id` on every probe after it (FINDINGS-06 [8]): the readiness
/// discovery yields `None`, and only a re-probe — the kind the scheduler now
/// runs when an occupant resolves with no id — can ever see the id.
#[derive(Debug, Clone)]
#[allow(dead_code)] // binaries that never script a late id still compile this module
pub struct MockLateIdAdapter {
    /// The `engine:` id the yaml must carry to select this adapter.
    pub engine: &'static str,
    /// The id this engine reports at `/v1/models` once it stops missing.
    pub serve_id: &'static str,
}

impl EngineAdapter for MockLateIdAdapter {
    fn engine_id(&self) -> &str {
        self.engine
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let mut spec = simplehar_se::engine::mock::MockAdapter.spec(model, port)?;
        spec.env
            .insert("MOCK_MODEL_ID".to_string(), self.serve_id.to_string());
        spec.env
            .insert("MOCK_DISCOVERY_FAILURES".to_string(), "1".to_string());
        Ok(spec)
    }
}
