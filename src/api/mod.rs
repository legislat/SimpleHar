//! HTTP surface (DESIGN Steps 11-12): per-category listeners, the control plane, and
//! the proxy.
//!
//! Locked rules implemented here:
//! - status codes come from types, never from message text (defect E1);
//! - wrong-category and unknown-model requests `404` **before** any ensure (G5);
//! - a no-`model` body on a category listener means "use this category": the loaded
//!   occupant (MRU among AUX x N), else the category's `default_model` loaded as a
//!   normal job, else `400` (operator rule 2026-10-24, resolves F-2);
//! - `/har/unload` bodies are strict: unparseable is `400`, never an implicit
//!   unload-all (v3.3 CRITICAL fix);
//! - proxy responses stream through byte-for-byte, SSE included;
//! - `/har/*` lives on the one control listener; the control router has no request
//!   timeout because `/har/load` deliberately waits through the load (Step 12
//!   idempotency: a double-click attaches to the same job).

use crate::config::{AppConfig, CategoryId};
use crate::model::ModelEntry;
use crate::scheduler::{JobId, Reply, SchedulerError, SchedulerHandle};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, KeepAliveStream, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};

/// Proxy request body ceiling kept from v3.3 (G5 clients send big prompts).
const PROXY_BODY_LIMIT: usize = 1024 * 1024 * 1024;

/// Typed API error: the single place where the taxonomy maps to status codes (E1 fix).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ApiError {
    /// Unknown model on this listener, wrong category, or unknown job — `404`.
    #[error("not found: {what}")]
    NotFound { what: String },
    /// Malformed request (no `model` where no default is configured, bad body).
    #[error("bad request: {reason}")]
    BadRequest { reason: String },
    /// Admission refusal or under-specified model; the message carries the numbers.
    #[error("service unavailable: {reason}")]
    Unavailable { reason: String },
    /// The model's `launch_only_with_at_least_vram_free` floor was not met — the
    /// load is refused before spawn (greedy-offload thrash guard). Distinct code
    /// so clients can tell "card busy with foreigners" from plain over-commit.
    #[error("service unavailable: {reason}")]
    BelowLaunchFloor { reason: String },
    /// The card could not be read at all, so admission refused rather than guessed
    /// (503). Distinct from `unavailable` (over-commit) so a client can tell a dead
    /// witness from a busy card.
    #[error("service unavailable: {reason}")]
    WitnessUnavailable { reason: String },
    /// The engine could not be started or never became ready — `502`.
    #[error("upstream failure: {reason}")]
    Upstream { reason: String },
    /// The request conflicts with live state (cancelled, superseded, holder on port).
    #[error("conflict: {reason}")]
    Conflict { reason: String },
    /// A configured control token is missing or wrong — `401` (deploy review,
    /// 2026-10-26: publishing the control plane to a segment without a secret).
    #[error("unauthorized: {reason}")]
    Unauthorized { reason: String },
    /// A POST body lacks the required `application/json` content type — `415`.
    #[error("unsupported media type: {reason}")]
    UnsupportedMedia { reason: String },
    /// Real command, later milestone — `501`.
    #[error("not implemented: {reason}")]
    NotImplemented { reason: String },
    /// Harness-side failure — `500`.
    #[error("internal error: {reason}")]
    Internal { reason: String },
}

impl ApiError {
    /// Stable machine code mirrored in the JSON body (`code` field).
    pub fn code(&self) -> &'static str {
        match self {
            ApiError::NotFound { .. } => "not_found",
            ApiError::BadRequest { .. } => "bad_request",
            ApiError::Unavailable { .. } => "unavailable",
            ApiError::BelowLaunchFloor { .. } => "below_launch_floor",
            ApiError::WitnessUnavailable { .. } => "witness_unavailable",
            ApiError::Upstream { .. } => "upstream",
            ApiError::Conflict { .. } => "conflict",
            ApiError::Unauthorized { .. } => "unauthorized",
            ApiError::UnsupportedMedia { .. } => "unsupported_media_type",
            ApiError::NotImplemented { .. } => "not_implemented",
            ApiError::Internal { .. } => "internal",
        }
    }

    /// HTTP status for this error kind — typed mapping, no text matching.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::Unavailable { .. }
            | ApiError::BelowLaunchFloor { .. }
            | ApiError::WitnessUnavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            ApiError::Conflict { .. } => StatusCode::CONFLICT,
            ApiError::Unauthorized { .. } => StatusCode::UNAUTHORIZED,
            ApiError::UnsupportedMedia { .. } => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            ApiError::NotImplemented { .. } => StatusCode::NOT_IMPLEMENTED,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

/// The single JSON error body (A2): SE-native flat `code`/`message` plus the
/// OpenAI-compatible `error` envelope with the same values.
fn error_body(error: &ApiError) -> Value {
    let message = error.to_string();
    let code = error.code();
    json!({
        "code": code,
        "message": message,
        "error": { "message": message, "type": "simplehar_error", "code": code },
    })
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status(), Json(error_body(&self))).into_response()
    }
}

impl From<SchedulerError> for ApiError {
    fn from(error: SchedulerError) -> Self {
        match error {
            SchedulerError::UnknownModel(model) => ApiError::NotFound {
                what: format!("model `{model}`"),
            },
            SchedulerError::UnknownJob(job) => ApiError::NotFound {
                what: format!("job {job}"),
            },
            SchedulerError::Admission(reason) => {
                if matches!(
                    reason,
                    crate::budget::AdmissionError::BelowLaunchFloor { .. }
                ) {
                    ApiError::BelowLaunchFloor {
                        reason: reason.to_string(),
                    }
                } else if matches!(
                    reason,
                    crate::budget::AdmissionError::WitnessUnavailable { .. }
                ) {
                    ApiError::WitnessUnavailable {
                        reason: reason.to_string(),
                    }
                } else {
                    ApiError::Unavailable {
                        reason: reason.to_string(),
                    }
                }
            }
            SchedulerError::Engine(reason) => ApiError::Unavailable {
                reason: reason.to_string(),
            },
            SchedulerError::Spawn(reason) | SchedulerError::EngineFailed(reason) => {
                ApiError::Upstream { reason }
            }
            SchedulerError::ReleaseTimeout { holder } => ApiError::Conflict {
                reason: format!("release timeout: {holder} still holds the slot"),
            },
            SchedulerError::Cancelled(reason) => ApiError::Conflict {
                reason: format!("cancelled: {reason}"),
            },
            SchedulerError::SettleTimeout { .. } => ApiError::Unavailable {
                reason: error.to_string(),
            },
            SchedulerError::ActorGone => ApiError::Internal {
                reason: "scheduler is not running".to_string(),
            },
            SchedulerError::Unsupported(reason) => ApiError::NotImplemented { reason },
        }
    }
}

/// Boot measures (§0.2): the numbers that make "the harness is up" observable.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BootTimings {
    /// Config load + validation duration.
    pub config_ms: u64,
    /// Catalog scan duration (measured at scheduler start).
    pub scan_ms: u64,
    /// Wall time until every listener was Serving or Down (set by `main`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_ms: Option<u64>,
}

impl BootTimings {
    /// Records the bind resolution once all supervisors have verdicts.
    pub fn set_bound(&mut self, ms: u64) {
        self.bind_ms = Some(ms);
    }
}

/// Shared application state (AGENTS.md: layered extractors, no globals).
#[derive(Clone)]
pub struct ApiState {
    /// Validated boot config.
    pub config: AppConfig,
    /// Address the control listener binds on.
    pub control_addr: String,
    /// The scheduler actor.
    pub scheduler: SchedulerHandle,
    /// Listener health, written by the supervisors in `main` (red state, §0.2).
    pub listeners: crate::listeners::ListenerRegistry,
    /// The VRAM witness, shared with the scheduler (Step 4).
    pub witness: Arc<dyn crate::budget::VramWitness>,
    /// Boot measures (§0.2), filled by `main` as boot progresses.
    pub boot: Arc<std::sync::RwLock<BootTimings>>,
    /// Runtime `default_model` overrides from `/har/config/default`, layered over
    /// the boot config (Step 9: the yaml/json stays the source of truth; overrides
    /// persist back to it on every set).
    default_overrides: Arc<Mutex<BTreeMap<CategoryId, Option<String>>>>,
    /// Last-use hints per model (fallback MRU for AUX x N; eviction LRU later).
    usage: Arc<Mutex<HashMap<String, Instant>>>,
    /// Shared secret for API/proxy routes when configured (`control_token` or
    /// `SIMPLEHAR_TOKEN`). Private: only the guard reads it, and it is never logged.
    control_token: Option<String>,
    proxy: reqwest::Client,
}

impl ApiState {
    /// Builds handler state from a validated config, a live scheduler, the catalog,
    /// and the listener health registry.
    pub fn new(
        config: AppConfig,
        scheduler: SchedulerHandle,
        listeners: crate::listeners::ListenerRegistry,
        witness: Arc<dyn crate::budget::VramWitness>,
        boot: BootTimings,
    ) -> Self {
        // 0.0.0.0 is a bind address, not a connect address: display and tray links
        // use loopback when the harness publishes to the local net.
        let display_host = if config.internal_host.is_unspecified() {
            std::net::IpAddr::from([127, 0, 0, 1])
        } else {
            config.internal_host
        };
        let control_addr = format!("{}:{}", display_host, config.control_category().public_port);
        let control_token = resolve_control_token(&config);
        ApiState {
            control_addr,
            scheduler,
            listeners,
            witness,
            boot: Arc::new(std::sync::RwLock::new(boot)),
            default_overrides: Arc::new(Mutex::new(BTreeMap::new())),
            usage: Arc::new(Mutex::new(HashMap::new())),
            control_token,
            proxy: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            config,
        }
    }

    /// Whether API/proxy routes demand the control token (boot messaging; the secret
    /// itself stays private and is never logged).
    #[must_use]
    pub fn requires_token(&self) -> bool {
        self.control_token.is_some()
    }

    /// Catalog snapshot from the actor (Step 2: one writer, snapshot reads).
    ///
    /// # Errors
    ///
    /// [`ApiError::Internal`] when the scheduler actor is gone.
    pub async fn catalog(&self) -> Result<Vec<ModelEntry>, ApiError> {
        Ok(self
            .scheduler
            .catalog()
            .await
            .map_err(ApiError::from)?
            .models)
    }

    /// The effective `default_model` for a category: runtime override, else boot
    /// config (Step 11 fallback rule).
    pub fn default_model(&self, category: CategoryId) -> Option<String> {
        let overrides = self
            .default_overrides
            .lock()
            .expect("default override lock poisoned by a writer panic");
        match overrides.get(&category) {
            Some(override_value) => override_value.clone(),
            None => self
                .config
                .categories
                .iter()
                .find(|cat| cat.id == category)
                .and_then(|cat| cat.default_model.clone()),
        }
    }

    /// Sets (or clears, with `None`) the category default and persists it to
    /// `simplehar.json` atomically (temp file + rename). In-memory wins even when
    /// persistence fails (logged); the response says which happened.
    pub fn set_default_model(
        &self,
        category: CategoryId,
        model: Option<&str>,
    ) -> (Option<String>, bool) {
        self.default_overrides
            .lock()
            .expect("default override lock poisoned by a writer panic")
            .insert(category, model.map(str::to_string));
        let persisted = persist_default_model(&self.config, category, model);
        (model.map(str::to_string), persisted)
    }

    fn touch(&self, model: &str) {
        self.usage
            .lock()
            .expect("usage lock poisoned by a writer panic")
            .insert(model.to_string(), Instant::now());
    }

    fn last_used(&self, model: &str) -> Option<Instant> {
        self.usage
            .lock()
            .expect("usage lock poisoned by a writer panic")
            .get(model)
            .copied()
    }
}

/// One category listener's view of the shared state.
#[derive(Clone)]
struct ListenerState {
    api: Arc<ApiState>,
    category: CategoryId,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Writes `category.default_model` into `simplehar.json` atomically: parse the
/// on-disk file (or the boot config when absent), patch the one field, temp+rename.
fn persist_default_model(config: &AppConfig, category: CategoryId, model: Option<&str>) -> bool {
    // Write-back must target the file boot actually loaded (`SIMPLEHAR_CONFIG`
    // included), or a test/LAN config silently loses its defaults (finding #2).
    let path = config.source_path.clone().unwrap_or_else(|| {
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(|dir| dir.join("simplehar.json")))
            .unwrap_or_else(|| std::path::PathBuf::from("simplehar.json"))
    });
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "simplehar.json unreadable; rewriting from boot config");
            serde_json::to_value(&*config).expect("invariant: config serializes")
        }),
        Err(_) => serde_json::to_value(config).expect("invariant: config serializes"),
    };
    let id = category.to_string();
    // A partial config file (e.g. only `modelconf_dir` set) has no categories to
    // patch — rebuild the tree from the boot config, which carries all four.
    if root
        .get("categories")
        .and_then(serde_json::Value::as_array)
        .is_none()
    {
        root = serde_json::to_value(config).expect("invariant: config serializes");
    }
    let Some(categories) = root.get_mut("categories").and_then(|c| c.as_array_mut()) else {
        tracing::error!("cannot persist default_model: no categories in simplehar.json");
        return false;
    };
    let mut patched = false;
    for entry in categories.iter_mut() {
        if entry.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) {
            let map = match entry.as_object_mut() {
                Some(map) => map,
                None => continue,
            };
            match model {
                Some(name) => {
                    map.insert("default_model".to_string(), Value::String(name.to_string()));
                }
                None => {
                    map.insert("default_model".to_string(), Value::Null);
                }
            }
            patched = true;
            break;
        }
    }
    if !patched {
        tracing::error!(category = %id, "cannot persist default_model: category not found");
        return false;
    }
    let text = serde_json::to_string_pretty(&root).expect("invariant: JSON serializes");
    let tmp = path.with_extension("json.tmp");
    if let Err(error) = std::fs::write(&tmp, text.as_bytes()) {
        tracing::error!(error = %error, "cannot persist default_model: write failed");
        return false;
    }
    // Keep the previous version beside the live file: schema additions are a
    // one-way door for `deny_unknown_fields` (an old binary refuses a file a new
    // one wrote), so rollback needs the pre-write copy (review, 2026-10-26).
    if path.exists() {
        let bak = path.with_extension("json.bak");
        if let Err(error) = std::fs::copy(&path, &bak) {
            tracing::warn!(error = %error, "could not write config backup; continuing");
        }
    }
    if let Err(error) = std::fs::rename(&tmp, &path) {
        tracing::error!(error = %error, "cannot persist default_model: rename failed");
        return false;
    }
    tracing::info!(category = %id, model = ?model, "default_model persisted to simplehar.json");
    true
}

/// `GET /har/status`: boot info + slot table + jobs, one call (G13).
async fn har_status(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(status_document(&state).await?))
}

/// The full `/har/status` document: config categories, mem, boot, listener
/// health, slots, jobs, and catalog errors — one call (G13). Also the body of
/// every SSE `har_state` event (Step 14: one push replaces the page's state).
async fn status_document(state: &ApiState) -> Result<Value, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let categories: Vec<Value> = state
        .config
        .categories
        .iter()
        .map(|cat| {
            let mut value = serde_json::to_value(cat).expect("invariant: category serializes");
            value["default_model"] = match state.default_model(cat.id) {
                Some(model) => Value::String(model),
                None => Value::Null,
            };
            value
        })
        .collect();
    let boot = state
        .boot
        .read()
        .expect("boot timings lock poisoned by a writer panic")
        .clone();
    // A3: the sticky headline fault, derived once by the scheduler crate
    // (`headline_fault`: newest failed LOAD, cancels excluded, cleared only by a
    // later successful load of the same model+category) — page and tray share it.
    let last_error = crate::scheduler::headline_fault(&scheduler.jobs).and_then(|job| {
        job.error
            .as_ref()
            .map(|reason| format!("{}@{}: {reason}", job.model, job.category))
    });
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "control_addr": state.control_addr,
        "extended_logging": crate::observability::extended_logging(),
        "boot": boot,
        "categories": categories,
        "listeners": state.listeners.snapshot(),
        "mem": mem_section(state, &scheduler).await,
        "slots": scheduler.slots,
        "jobs": scheduler.jobs,
        "last_error": last_error,
        "catalog_errors": scheduler.catalog_errors,
        "config_errors": [],
    }))
}

/// `POST /har/load {"model": name}` — waits through the load and returns the receipt;
/// a repeat while in flight attaches to the same job (Step 12 idempotency).
async fn har_load(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with a `model` field: {rejection}"),
    })?;
    let Some(model) = body.get("model").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: "missing required field `model`".to_string(),
        });
    };
    let catalog = state.catalog().await?;
    let entry = crate::catalog::resolve(model, &catalog).ok_or_else(|| ApiError::NotFound {
        what: format!("model `{model}`; known models are listed at each listener's /v1/models"),
    })?;
    let reply = state
        .scheduler
        .swap(&entry.name, entry.category)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(match reply {
        Reply::Loaded(receipt) => json!({ "status": "loaded", "receipt": receipt }),
        Reply::Already {
            model,
            category,
            port,
            serve_id,
        } => {
            json!({ "status": "already", "model": model, "category": category, "port": port, "serve_id": serve_id })
        }
        Reply::Done => json!({ "status": "done" }),
    }))
}

/// `POST /har/unload` — `{"category": "WORK"}` one category, `{"model": name}`
/// one slot (T12.1), `{}` every occupied slot (T12.4, the explicit unload-all),
/// garbage `400` (the v3.3 CRITICAL fix was about *unparseable* bodies, never an
/// implicit unload-all). Unloading a loading slot cancels the load (T8).
async fn har_unload(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON: {rejection}"),
    })?;
    if let Some(raw) = body.get("category").and_then(Value::as_str) {
        let category: CategoryId = raw
            .parse()
            .map_err(|reason| ApiError::BadRequest { reason })?;
        state
            .scheduler
            .unload(category)
            .await
            .map_err(ApiError::from)?;
        return Ok(Json(json!({ "status": "unloading", "category": category })));
    }
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        state
            .scheduler
            .unload_model(model)
            .await
            .map_err(ApiError::from)?;
        return Ok(Json(json!({ "status": "unloading", "model": model })));
    }
    if body.as_object().is_some_and(|map| map.is_empty()) {
        state.scheduler.unload_all().await.map_err(ApiError::from)?;
        return Ok(Json(json!({ "status": "unloading", "scope": "all" })));
    }
    Err(ApiError::BadRequest {
        reason: "unrecognized body: send {} (all), {\"category\": …} or {\"model\": …}".to_string(),
    })
}

/// The one harness-facing model-listing shape: OpenAI fields plus the extras the
/// page's table renders (A5: `context`, `serve_model`, `model_ref`). Single
/// builder for `merged_state` and `/har/models` so they cannot drift. The
/// client-facing listener `/v1/models` is deliberately minimal — local file
/// paths stay off the LAN-facing surface.
fn model_json(entry: &ModelEntry) -> Value {
    json!({
        "id": entry.name,
        "object": "model",
        "owned_by": "simplehar",
        "engine": entry.engine,
        "category": entry.category,
        "vram_gb": entry.vram_gb,
        // Shown only when `cost_source` says admission really books the GGUF estimate
        // (x1.15 + 2) — the page can no longer display a cost the gate ignores
        // (duplication review, 2026-10-26).
        "est_vram_gb": (entry.cost_source() == crate::model::CostSource::Estimate).then(|| {
            crate::budget::need_gb(
                None,
                entry.gguf_size_gb.map(crate::budget::Gibibytes),
            )
            .ok()
            .map(|n| n.0)
        }).flatten(),
        "measured_gb": entry.measured_gb,
        "cost": entry.cost,
        "launch_floor_gb": entry.launch_floor_gb,
        "load_timeout_s": entry.load_timeout_s,
        "cpu_offload": entry
            .raw
            .get("cpu_offload")
            .is_some_and(|v| crate::engine::llama::jinja_on(v)),
        "context": entry.raw.get("context"),
        "serve_model": entry.serve_model,
        "model_ref": entry.model_ref,
    })
}

/// The merged document every `har_state` event carries: one push replaces the
/// page's per-panel polling (Step 14).
async fn merged_state(state: &ApiState) -> Result<Value, ApiError> {
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog.iter().map(model_json).collect();
    let mut document = status_document(state).await?;
    document["models"] = Value::Array(models);
    Ok(document)
}

/// `GET /har/events` — one SSE stream: the current state immediately, then a push
/// after every scheduler mutation (keepalives by the SSE layer); a dropped stream
/// degrades to the page's polling fallback (Step 12 / Step 14).
#[allow(clippy::type_complexity)]
async fn har_events(
    State(state): State<Arc<ApiState>>,
) -> Sse<
    KeepAliveStream<
        tokio_stream::wrappers::UnboundedReceiverStream<Result<Event, std::convert::Infallible>>,
    >,
> {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut pushes = state.scheduler.subscribe();
        // The immediate snapshot: the page renders once without waiting for a change.
        if let Ok(doc) = merged_state(&state).await {
            if send_state(&tx, &doc).is_err() {
                return;
            }
        }
        loop {
            match pushes.recv().await {
                Ok(_) => {
                    if let Ok(doc) = merged_state(&state).await {
                        if send_state(&tx, &doc).is_err() {
                            return; // subscriber gone
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Missed pushes are fine: the next merge carries the full state.
                    if let Ok(doc) = merged_state(&state).await {
                        let _ = send_state(&tx, &doc);
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            }
        }
    });
    Sse::new(tokio_stream::wrappers::UnboundedReceiverStream::new(rx))
        .keep_alive(KeepAlive::default())
}

/// Sends one `har_state` event; `Err` means the subscriber disconnected.
fn send_state(
    tx: &tokio::sync::mpsc::UnboundedSender<Result<Event, std::convert::Infallible>>,
    doc: &Value,
) -> Result<(), tokio::sync::mpsc::error::SendError<()>> {
    let event = Event::default()
        .event("har_state")
        .json_data(doc)
        .unwrap_or_else(|_| Event::default().event("har_state").data("{}"));
    tx.send(Ok(event))
        .map_err(|_| tokio::sync::mpsc::error::SendError(()))
}

/// Process-wide request-id counter (mixed with the boot epoch: unique per run).
static REQUEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REQUEST_EPOCH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// `x-request-id` is not in the standard header constants.
const X_REQUEST_ID: axum::http::HeaderName = axum::http::HeaderName::from_static("x-request-id");

fn next_request_id() -> String {
    let epoch = *REQUEST_EPOCH.get_or_init(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or_default()
    });
    let counter = REQUEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("req-{epoch:x}-{counter:x}")
}

/// Routes that must answer before a token exists, so the page can load and ask for
/// one. Exact matches only — never a prefix, which would re-open `/har/*`-adjacent
/// paths.
const PUBLIC_PATHS: [&str; 4] = ["/", "/index.html", "/app.js", "/style.css"];

/// The control token, if any: the environment wins so the secret can stay out of the
/// config file entirely (AGENTS.md — secrets live in the environment, not in
/// code/config). Whitespace-only values mean "no token" at read time; `validate`
/// rejects them in the file, so a typo can never silently disable auth.
fn resolve_control_token(config: &AppConfig) -> Option<String> {
    std::env::var("SIMPLEHAR_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            config
                .control_token
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        })
}

/// Whether a request may pass without the control token. The token travels only in a
/// header — never in a URL, where it would land in the request log (review,
/// 2026-10-26).
fn auth_reason(state: &ApiState, request: &axum::extract::Request) -> Option<ApiError> {
    let expected = state.control_token.as_deref()?;
    if PUBLIC_PATHS.contains(&request.uri().path()) {
        return None;
    }
    let headers = request.headers();
    let presented = headers
        .get("x-har-token")
        .or_else(|| headers.get(axum::http::header::AUTHORIZATION))
        .and_then(|value| value.to_str().ok())
        .map(|raw| raw.strip_prefix("Bearer ").unwrap_or(raw).trim());
    match presented {
        Some(given) if given == expected => None,
        _ => Some(ApiError::Unauthorized {
            reason: format!(
                "{} requires the control token (X-Har-Token or Authorization: Bearer)",
                request.uri().path()
            ),
        }),
    }
}

/// The guard + correlation middleware (Step 11 "kept regardless", Step 12):
/// - **Host** must be loopback-shaped (an IP literal or `localhost`) — a hostname
///   means a DNS-rebinding attempt, refused `400` before any handler runs;
/// - **Origin** (when the client sends one — browsers do on cross-site POSTs) must
///   match the request's Host authority, else `403` (CSRF guard);
/// - a **POST** body must declare `application/json`, else `415` (Step 11);
/// - the **request id** comes from the client's `x-request-id` or is generated,
///   echoed in the response header, and logged at request completion (G6).
///
/// Applied in `main` on every listener router, so guards wrap uniformly.
pub async fn guard(
    State(state): State<Arc<ApiState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let request_id = request
        .headers()
        .get(&X_REQUEST_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(next_request_id);

    if let Some(reason) = reject_reason(&request) {
        let response = (reason.status(), Json(error_body(&reason))).into_response();
        return with_request_id(response, &request_id);
    }
    if let Some(reason) = auth_reason(&state, &request) {
        let response = (reason.status(), Json(error_body(&reason))).into_response();
        return with_request_id(response, &request_id);
    }

    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    response = with_request_id(response, &request_id);
    // The page polls /har/status continuously; those lines would evict real
    // evidence from the ring during long boots. Extended logging restores them
    // (operator, 2026-10-26).
    let routine_poll = path == "/har/status" && method == axum::http::Method::GET;
    if !routine_poll || crate::observability::extended_logging() {
        tracing::info!(
            %request_id,
            method = %method,
            path = %path,
            status = response.status().as_u16(),
            "request complete"
        );
    }
    response
}

/// The guard verdict for a request: `Some(reason)` refuses it.
fn reject_reason(request: &axum::extract::Request) -> Option<ApiError> {
    let headers = request.headers();
    if let Some(host) = headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        if !loopback_host(host) {
            return Some(ApiError::BadRequest {
                reason: format!(
                    "Host `{host}` is not a loopback address; SE is a loopback tool and refuses requests addressed by hostname (DNS-rebinding guard)"
                ),
            });
        }
    }
    if request.method() == axum::http::Method::POST {
        if let Some(origin) = headers
            .get(axum::http::header::ORIGIN)
            .and_then(|value| value.to_str().ok())
        {
            let same_authority = headers
                .get(axum::http::header::HOST)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|host| {
                    origin_authority(origin).is_some_and(|authority| authority == host)
                });
            if !same_authority {
                return Some(ApiError::Conflict {
                    reason: format!(
                        "Origin `{origin}` does not match this listener; cross-site POSTs are refused (CSRF guard)"
                    ),
                });
            }
        }
        let json = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .unwrap_or_default()
                    .trim()
                    .eq_ignore_ascii_case("application/json")
            });
        if !json {
            return Some(ApiError::UnsupportedMedia {
                reason: "POST bodies must declare Content-Type: application/json".to_string(),
            });
        }
    }
    None
}

/// `true` for IP literals and `localhost` (with any port, v6 bracketed).
fn loopback_host(host: &str) -> bool {
    let authority = host
        .rsplit_once(':')
        .map_or(host, |(authority, _)| authority);
    let authority = authority.trim_start_matches('[').trim_end_matches(']');
    authority.parse::<std::net::IpAddr>().is_ok() || authority.eq_ignore_ascii_case("localhost")
}

/// The authority (`host[:port]`) of an `Origin` header value.
fn origin_authority(origin: &str) -> Option<&str> {
    origin
        .strip_prefix("http://")
        .or_else(|| origin.strip_prefix("https://"))
}

/// Copies the request id onto the response headers.
fn with_request_id(mut response: Response, request_id: &str) -> Response {
    if let Ok(value) = axum::http::HeaderValue::from_str(request_id) {
        response.headers_mut().insert(&X_REQUEST_ID, value);
    }
    response
}

/// `GET /har/log?limit=N` — the last N harness log events from the in-memory
/// ring (Step 16): why did the last load fail, without opening files.
async fn har_log(
    axum::extract::Query(query): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, ApiError> {
    let limit = query
        .get("limit")
        .and_then(|limit| limit.parse::<usize>().ok())
        .unwrap_or(100)
        .min(crate::observability::RING_CAPACITY);
    let (events, dropped) = crate::observability::ring_tail(limit);
    Ok(Json(
        json!({ "events": events, "dropped": dropped, "capacity": crate::observability::RING_CAPACITY }),
    ))
}

/// `GET /har/models` — the full unfiltered catalog for the control page (G5: the
/// per-listener `/v1/models` stays category-filtered).
async fn har_models(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog.iter().map(model_json).collect();
    Ok(Json(json!({ "object": "list", "data": models })))
}

/// `POST /har/config/default {"category": …, "model": name|null}` — set the
/// category's default model (Step 11 fallback); validated against the catalog and
/// persisted to `simplehar.json` atomically.
async fn har_config_default(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with `category` and optional `model`: {rejection}"),
    })?;
    let Some(raw_category) = body.get("category").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: format!(
                "missing required field `category` ({})",
                crate::config::CATEGORY_HINT
            ),
        });
    };
    let category: CategoryId = raw_category.parse().map_err(|_| ApiError::BadRequest {
        reason: format!("unknown category `{raw_category}`"),
    })?;
    let requested = body.get("model").and_then(Value::as_str);
    if let Some(model) = requested {
        let catalog = state.catalog().await?;
        let entry =
            crate::catalog::resolve(model, &catalog).ok_or_else(|| ApiError::BadRequest {
                reason: format!("unknown model `{model}`"),
            })?;
        if entry.category != category {
            return Err(ApiError::BadRequest {
                reason: format!(
                    "model `{}` belongs to {}, not {}",
                    entry.name, entry.category, category
                ),
            });
        }
    }
    let (effective, persisted) = state.set_default_model(category, requested);
    Ok(Json(json!({
        "status": "ok",
        "category": category,
        "default_model": effective,
        "persisted": persisted,
    })))
}

/// `POST /har/config/model {"model": name, "category"?: WORK|RP|AUX|EMB, "cpu"?: bool}`
/// — write per-model flags into the model's yaml (Step 9: the file stays the
/// source of truth). A category change on a *loaded* model frees the slot so the
/// next use reloads it into the right listener (v3.3 §-behavior, kept).
async fn har_config_model(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with a `model` field: {rejection}"),
    })?;
    let Some(model_name) = body.get("model").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: "missing required field `model`".to_string(),
        });
    };
    let catalog = state.catalog().await?;
    let entry =
        crate::catalog::resolve(model_name, &catalog).ok_or_else(|| ApiError::NotFound {
            what: format!("model `{model_name}`"),
        })?;
    let path = entry.file.clone().ok_or_else(|| ApiError::BadRequest {
        reason: format!(
            "model `{}` has no backing file (synthetic entry)",
            entry.name
        ),
    })?;
    let old_category = entry.category;

    let mut edits: Vec<(&str, Option<String>)> = Vec::new();
    let mut requested_category: Option<CategoryId> = None;
    if let Some(raw) = body.get("category").and_then(Value::as_str) {
        let category: CategoryId = raw
            .parse()
            .map_err(|reason| ApiError::BadRequest { reason })?;
        requested_category = Some(category);
        edits.push(("category", Some(category.to_string())));
    }
    if let Some(cpu) = body.get("cpu").and_then(Value::as_bool) {
        edits.push(("cpu_offload", Some(cpu.to_string())));
    }
    if edits.is_empty() {
        return Err(ApiError::BadRequest {
            reason: "nothing to edit: send `category` and/or `cpu`".to_string(),
        });
    }
    for (key, value) in &edits {
        crate::model::writeback::set_flag_on_file(&path, key, value.as_deref())
            .map_err(|reason| ApiError::Internal { reason })?;
    }
    // The catalog must reflect the file the moment the operator looks again.
    state.scheduler.reload().await.map_err(ApiError::from)?;

    // A model whose category actually changed must move slots, not lie in place
    // (v3.3 §-behavior, kept). The old guard compared `entry.category` against
    // itself and never fired. If the moved model was its old category's default,
    // that default now dangles — clear it too (operator finding #1, 2026-10-26).
    if let Some(new_cat) = requested_category.filter(|c| *c != old_category) {
        let catalog = state.catalog().await?;
        if catalog
            .iter()
            .any(|m| m.name == entry.name && m.category != old_category)
        {
            let _ = state.scheduler.unload(old_category).await;
            if let Some(default) = state.default_model(old_category) {
                if crate::catalog::resolve(&default, &catalog).is_some_and(|d| d.name == entry.name)
                {
                    state.set_default_model(old_category, None);
                    tracing::info!(
                        model = %entry.name,
                        from = %old_category,
                        to = %new_cat,
                        "cleared dangling category default"
                    );
                }
            }
        }
    }
    let catalog = state.catalog().await?;
    let fresh = catalog.iter().find(|m| m.name == entry.name);
    Ok(Json(json!({
        "status": "edited",
        "model": entry.name,
        "edits": edits.iter().map(|(key, value)| json!({
            "key": key,
            "value": value,
        })).collect::<Vec<_>>(),
        "category": fresh.map(|m| m.category),
        "cpu_offload": fresh.and_then(|m| m.raw.get("cpu_offload").cloned()),
    })))
}

/// `POST /har/rescan` — re-scan `ModelConf`; the reply carries the new counts and
/// every catalog error (Step 9: a bad file is named, never fatal).
async fn har_rescan(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    state.scheduler.reload().await.map_err(ApiError::from)?;
    let catalog = state.scheduler.catalog().await.map_err(ApiError::from)?;
    Ok(Json(json!({
        "status": "rescanned",
        "models": catalog.models.len(),
        "catalog_errors": catalog.errors,
    })))
}

/// `POST /har/config/logging {"enabled": bool}` — the page's "Extended Logging"
/// switch: decision-level scheduler lines plus the full request ring. Runtime
/// only; every boot starts with it off.
async fn har_config_logging(
    State(_state): State<Arc<ApiState>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, ApiError> {
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return Err(ApiError::BadRequest {
            reason: "enabled (boolean) is required".to_string(),
        });
    };
    crate::observability::set_extended_logging(enabled);
    Ok(Json(json!({ "status": "ok", "extended_logging": enabled })))
}

/// The `/har/status` memory block: declared config numbers plus what the witness
/// actually sees, labeled with its source (F-7 lesson: never present a number
/// without saying whose number it is).
async fn mem_section(state: &ApiState, scheduler: &crate::scheduler::SchedulerStatus) -> Value {
    // Loads committed but not yet installed: the very set the admission gate debits
    // (`booking_gibs`), so the bar and a refusal can never quote two numbers
    // (review, 2026-10-26). The caller already holds this snapshot — asking the
    // actor for a second one per poll was a duplicated round-trip.
    let booked_gb = crate::scheduler::booking_gibs(&scheduler.jobs);
    let ram = crate::witness::ram_sample();
    let mut section = match state.witness.sample().await {
        Ok(sample) => {
            let attributed = sample.process_total_gib();
            json!({
                "total_gb": sample.total_gib,
                "reserve_gb": state.config.vram_reserve_gb,
                "booked_gb": booked_gb,
                "observed_used_gb": sample.used_gib,
                // What NVML accounted across all processes, or `null` when it
                // answered with no usable per-process figures at all (WDDM
                // degradation): absence, never a fake 0.0 GiB (review 2026-10-26).
                "attributed_gb": (attributed > 0.0).then_some(attributed),
                "source": state.witness.source(),
            })
        }
        Err(reason) => json!({
            "total_gb": state.config.vram_total_gb,
            "reserve_gb": state.config.vram_reserve_gb,
            "booked_gb": booked_gb,
            "observed_used_gb": null,
            "attributed_gb": null,
            "source": format!("unavailable ({reason})"),
        }),
    };
    // A1: host RAM is informational context next to the VRAM bars (§0.4 keeps it
    // out of admission).
    if let Value::Object(fields) = &mut section {
        fields.insert("ram_total_gb".to_string(), json!(ram.map(|r| r.total_gib)));
        fields.insert("ram_used_gb".to_string(), json!(ram.map(|r| r.used_gib)));
    }
    section
}

/// `GET /har/jobs` — the ledger this timekeeping always pretended to be (F-5 fix).
async fn har_jobs(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    Ok(Json(json!({ "jobs": scheduler.jobs })))
}

/// `GET /har/jobs/{id}`.
async fn har_job(
    State(state): State<Arc<ApiState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let id: u64 = id.parse().map_err(|_| ApiError::NotFound {
        what: format!("job id `{id}`"),
    })?;
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let job = scheduler
        .jobs
        .iter()
        .find(|job| job.id == JobId(id))
        .ok_or_else(|| ApiError::NotFound {
            what: format!("job {id}"),
        })?;
    Ok(Json(json!({ "job": job })))
}

/// `GET /v1/models` — exactly this listener's category, never the full catalog
/// (no cross-category disclosure, G5).
async fn v1_models(State(listener): State<Arc<ListenerState>>) -> Result<Json<Value>, ApiError> {
    let catalog = listener.api.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .filter(|entry| entry.category == listener.category)
        .map(|entry| {
            json!({
                "id": entry.name,
                "object": "model",
                "owned_by": "simplehar",
                "engine": entry.engine,
            })
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": models })))
}

/// Resolves the request to a serving port on this listener, or a typed error:
/// wrong-category and unknown models `404` **before** any ensure (Step 11).
/// Resolves the request to a serving target on this listener, or a typed error:
/// wrong-category and unknown models `404` **before** any ensure (Step 11).
///
/// Returns `(harness_name, port, serve_id)`; `serve_id` is the engine's
/// self-reported id (F-4) and is the proxy rewrite target when present.
async fn ensure_target(
    listener: &ListenerState,
    body: &Value,
) -> Result<(String, u16, Option<String>), ApiError> {
    let api = &listener.api;
    let requested = body.get("model").and_then(Value::as_str);
    match requested {
        Some(model) => {
            let catalog = api.catalog().await?;
            let entry =
                crate::catalog::resolve(model, &catalog).ok_or_else(|| ApiError::NotFound {
                    what: format!(
                        "model `{model}` on the {} listener; its /v1/models lists the ids it serves",
                        listener.category
                    ),
                })?;
            if entry.category != listener.category {
                let hint = api
                    .config
                    .categories
                    .iter()
                    .find(|cat| cat.id == entry.category)
                    .map(|cat| cat.public_port)
                    .unwrap_or_default();
                return Err(ApiError::NotFound {
                    what: format!(
                        "model `{model}` is served on the {} listener (port {hint}), not {}",
                        entry.category, listener.category
                    ),
                });
            }
            let (port, serve_id) = swap_for_port(api, &entry.name, entry.category).await?;
            Ok((entry.name.clone(), port, serve_id))
        }
        None => {
            // F-2 rule (operator, 2026-10-24): "use this category".
            let status = api.scheduler.status().await.map_err(ApiError::from)?;
            let mut occupants: Vec<(String, u16, Instant)> = status
                .slots
                .iter()
                .filter(|slot| slot.category == listener.category)
                .filter_map(|slot| match (&slot.model, slot.port) {
                    (Some(model), Some(port)) if port != 0 => Some((
                        model.clone(),
                        port,
                        api.last_used(model).unwrap_or_else(slot_loaded_fallback),
                    )),
                    _ => None,
                })
                .collect();
            occupants.sort_by_key(|&(_, _, used)| std::cmp::Reverse(used)); // MRU first
            if let Some((model, port, _)) = occupants.first() {
                api.touch(model);
                api.scheduler.touch(model);
                let serve_id = status
                    .slots
                    .iter()
                    .find(|slot| slot.model.as_deref() == Some(model.as_str()))
                    .and_then(|slot| slot.serve_id.clone());
                return Ok((model.clone(), *port, serve_id));
            }
            let default = api.default_model(listener.category);
            match default {
                Some(model) => {
                    let (port, serve_id) =
                        swap_for_port(api, &model, listener.category).await?;
                    Ok((model, port, serve_id))
                }
                None => Err(ApiError::BadRequest {
                    reason: format!(
                        "no `model` field and nothing loaded on the {} listener; set the category's `default_model` or send a model name",
                        listener.category
                    ),
                }),
            }
        }
    }
}

fn slot_loaded_fallback() -> Instant {
    // Occupants without a usage hint fall back to "least recently used" semantics by
    // appearing oldest; loaded-time tracking refines this when eviction lands.
    Instant::now() - Duration::from_secs(3_600)
}

/// Ensures `model` is serving in `category`; returns its engine port and the
/// engine's self-reported id (F-4).
async fn swap_for_port(
    api: &ApiState,
    model: &str,
    category: CategoryId,
) -> Result<(u16, Option<String>), ApiError> {
    let reply = api
        .scheduler
        .swap(model, category)
        .await
        .map_err(ApiError::from)?;
    let (port, serve_id) = match reply {
        Reply::Loaded(receipt) => (receipt.port, receipt.serve_id),
        Reply::Already { port, serve_id, .. } => (port, serve_id),
        Reply::Done => (0, None),
    };
    api.touch(model);
    api.scheduler.touch(model);
    Ok((port, serve_id))
}

/// Proxy pass-through: POST /v1/chat/completions | /v1/completions | /v1/embeddings.
/// The response body streams through byte-for-byte (SSE included).
async fn proxy(
    State(listener): State<Arc<ListenerState>>,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(mut body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON: {rejection}"),
    })?;
    let (model, port, serve_id) = ensure_target(&listener, &body).await?;
    let path = uri.path().to_string();
    // F-4 fix: rewrite to the engine's self-reported id when discovery found one;
    // the harness name is the fallback (llama engines ignore the field either way).
    body["model"] = Value::String(serve_id.unwrap_or_else(|| model.clone()));

    // T10.1: a connection-level failure (engine died mid-request) triggers exactly
    // one reap + reload + retry. A genuine upstream HTTP status is never retried
    // (A9), and a death mid-stream surfaces in the body stream, past this loop.
    let mut port = port;
    let mut attempts = 0;
    let upstream = loop {
        let url = format!("http://127.0.0.1:{port}{path}");
        match listener.api.proxy.post(&url).json(&body).send().await {
            Ok(upstream) => break upstream,
            Err(error) => {
                attempts += 1;
                if attempts >= 2 {
                    return Err(ApiError::Upstream {
                        reason: format!("engine on :{port} unreachable after crash-retry: {error}"),
                    });
                }
                tracing::warn!(
                    model = %model,
                    port,
                    error = %error,
                    "engine unreachable mid-request; restarting once (T10.1)"
                );
                listener
                    .api
                    .scheduler
                    .restart(&model, listener.category)
                    .await
                    .map_err(ApiError::from)?;
                // Re-resolve: the fresh occupant may serve on a different port.
                let (_, new_port, _) = ensure_target(&listener, &body).await?;
                port = new_port;
            }
        }
    };
    let status =
        StatusCode::from_u16(upstream.status().as_u16()).map_err(|error| ApiError::Upstream {
            reason: format!("upstream status not representable: {error}"),
        })?;
    let mut response = Response::builder().status(status);
    for (name, value) in upstream.headers() {
        if matches!(
            name.as_str(),
            "transfer-encoding" | "content-length" | "connection" | "content-encoding"
        ) {
            continue;
        }
        if let (Ok(name), Ok(value)) = (
            axum::http::HeaderName::from_bytes(name.as_str().as_bytes()),
            HeaderValue::from_bytes(value.as_bytes()),
        ) {
            response = response.header(name, value);
        }
    }
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|error| ApiError::Internal {
            reason: error.to_string(),
        })
}

/// Builds one category listener: filtered catalog, proxy pass-through, health.
///
/// # Returns
///
/// A router serving exactly `category`'s models; `/har/*` is *not* included (that is
/// [`control_router`], on the designated control listener only).
pub fn category_router(api: Arc<ApiState>, category: CategoryId) -> Router {
    let guard_state = Arc::clone(&api);
    let listener = Arc::new(ListenerState { api, category });
    Router::new()
        .route("/health", get(health))
        .route(
            "/v1/models",
            get(v1_models).with_state(Arc::clone(&listener)),
        )
        .route(
            "/v1/chat/completions",
            post(proxy).with_state(Arc::clone(&listener)),
        )
        .route(
            "/v1/completions",
            post(proxy).with_state(Arc::clone(&listener)),
        )
        .route("/v1/embeddings", post(proxy).with_state(listener))
        .layer((
            TraceLayer::new_for_http(),
            CompressionLayer::new(),
            DefaultBodyLimit::max(PROXY_BODY_LIMIT),
        ))
        .layer(axum::middleware::from_fn_with_state(guard_state, guard))
}

/// Builds the control listener: its own category's surface plus `/har/*`.
pub fn control_router(api: Arc<ApiState>) -> Router {
    let control_category = api.config.control_category().id;
    // `/har/*` handlers take `State<Arc<ApiState>>`; type the router, then strip the
    // state before merging with the (stateless) category surface.
    let har: Router<Arc<ApiState>> = Router::new()
        .route("/har/status", get(har_status))
        .route("/har/jobs", get(har_jobs))
        .route("/har/events", get(har_events))
        .route("/har/log", get(har_log))
        .route("/har/jobs/{id}", get(har_job))
        .route("/har/load", post(har_load))
        .route("/har/unload", post(har_unload))
        .route("/har/rescan", post(har_rescan))
        .route("/har/models", get(har_models))
        .route("/har/config/default", post(har_config_default))
        .route("/har/config/logging", post(har_config_logging))
        .route("/har/config/model", post(har_config_model));
    Router::new()
        .merge(category_router(Arc::clone(&api), control_category))
        .merge(
            har.with_state(Arc::clone(&api))
                .layer(axum::middleware::from_fn_with_state(
                    Arc::clone(&api),
                    guard,
                )),
        )
        .merge(
            crate::webui::router(Arc::clone(&api)).layer(axum::middleware::from_fn_with_state(
                Arc::clone(&api),
                guard,
            )),
        )
        .layer((
            TraceLayer::new_for_http(),
            CompressionLayer::new(),
            // The control listener also carries its category's proxy surface, so the
            // generous limit applies here too; `/har/*` bodies are tiny in practice.
            DefaultBodyLimit::max(PROXY_BODY_LIMIT),
        ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_json_cpu_offload_agrees_with_the_engine() {
        // Review finding 6: one truth set — engine/llama::jinja_on — so the page
        // checkbox can never disagree with whether `-ngl 0` was actually forced.
        let entry = |cpu: &str| ModelEntry {
            name: "m".to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(4.0),
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            model_ref: Some(r"F:\m.gguf".to_string()),
            serve_model: None,
            file: None,
            raw: BTreeMap::from([
                ("cpu_offload".to_string(), cpu.to_string()),
                ("context".to_string(), "262144".to_string()),
            ]),
        };
        for truthy in ["true", "1", "on"] {
            assert_eq!(
                model_json(&entry(truthy))["cpu_offload"],
                json!(true),
                "`{truthy}` is engine-truthy"
            );
        }
        assert_eq!(model_json(&entry("yes"))["cpu_offload"], json!(false));
        assert_eq!(model_json(&entry("false"))["cpu_offload"], json!(false));
        let body = model_json(&entry("true"));
        assert_eq!(body["context"], json!("262144"));
        assert_eq!(body["model_ref"], json!(r"F:\m.gguf"));
        assert_eq!(body["cost"], json!("fixed"));
        assert!(body["launch_floor_gb"].is_null());
        assert!(body["load_timeout_s"].is_null());
        assert!(body["measured_gb"].is_null());
    }

    #[test]
    fn est_vram_gb_only_when_declared_and_measured_are_absent() {
        let mut entry = ModelEntry {
            name: "m".to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Work,
            vram_gb: None,
            gguf_size_gb: Some(23.0),
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            model_ref: Some(r"F:\m.gguf".to_string()),
            serve_model: None,
            file: None,
            raw: BTreeMap::new(),
        };
        let body = model_json(&entry);
        assert_eq!(body["est_vram_gb"], json!(23.0 * 1.15 + 2.0));
        entry.measured_gb = Some(70.5);
        let body = model_json(&entry);
        assert_eq!(body["measured_gb"], json!(70.5));
        assert!(
            body["est_vram_gb"].is_null(),
            "admission books the measurement, not the estimate"
        );
    }

    #[test]
    fn error_body_carries_flat_and_openai_envelope() {
        // A2: both shapes present and consistent.
        let err = ApiError::NotFound {
            what: "model `x`".to_string(),
        };
        let body = error_body(&err);
        assert_eq!(body["code"], "not_found");
        assert_eq!(body["message"], "not found: model `x`");
        assert_eq!(body["error"]["type"], "simplehar_error");
        assert_eq!(body["error"]["code"], "not_found");
        assert_eq!(body["error"]["message"], body["message"]);
    }

    #[test]
    fn status_codes_come_from_types_not_text() {
        // A model literally named "not found" must not change any mapping (E1).
        let err = ApiError::NotFound {
            what: "not found model".to_string(),
        };
        assert_eq!(err.status(), StatusCode::NOT_FOUND);
        assert_eq!(err.code(), "not_found");
        assert_eq!(
            ApiError::BadRequest {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            ApiError::Unavailable {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            ApiError::Upstream {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::BAD_GATEWAY
        );
        assert_eq!(
            ApiError::Conflict {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::CONFLICT
        );
        assert_eq!(
            ApiError::NotImplemented {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::NOT_IMPLEMENTED
        );
        assert_eq!(
            ApiError::Internal {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn scheduler_errors_map_to_typed_statuses() {
        assert_eq!(
            ApiError::from(SchedulerError::UnknownModel("m".to_string())).status(),
            StatusCode::NOT_FOUND
        );
        let admission = SchedulerError::Admission(crate::budget::AdmissionError::Insufficient {
            need: crate::budget::Gibibytes(50.0),
            free: crate::budget::Gibibytes(20.0),
            reserve: crate::budget::Gibibytes(2.0),
            facts: crate::budget::BudgetFacts {
                budget: Some(crate::budget::Gibibytes(20.0)),
                observed_free: None,
            },
            victims_considered: Vec::new(),
            blockers: Vec::new(),
        });
        let mapped = ApiError::from(admission);
        assert_eq!(mapped.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            mapped.to_string().contains("50.0 GiB"),
            "numbers must travel: {mapped}"
        );
        assert_eq!(
            ApiError::from(SchedulerError::Spawn("boom".to_string())).status(),
            StatusCode::BAD_GATEWAY
        );
        // A dead witness is its own client-visible class: "the card is unreadable"
        // must not be mistaken for "the card is full".
        let witness_down =
            SchedulerError::Admission(crate::budget::AdmissionError::WitnessUnavailable {
                reason: "nvidia-smi timed out".to_string(),
            });
        let mapped = ApiError::from(witness_down);
        assert_eq!(mapped.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(mapped.code(), "witness_unavailable");
        assert!(
            mapped.to_string().contains("nvidia-smi timed out"),
            "the witness's reason must travel: {mapped}"
        );
    }
}
