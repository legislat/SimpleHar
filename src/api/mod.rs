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
    /// The engine could not be started or never became ready — `502`.
    #[error("upstream failure: {reason}")]
    Upstream { reason: String },
    /// The request conflicts with live state (cancelled, superseded, holder on port).
    #[error("conflict: {reason}")]
    Conflict { reason: String },
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
            ApiError::Upstream { .. } => "upstream",
            ApiError::Conflict { .. } => "conflict",
            ApiError::NotImplemented { .. } => "not_implemented",
            ApiError::Internal { .. } => "internal",
        }
    }

    /// HTTP status for this error kind — typed mapping, no text matching.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
            ApiError::Upstream { .. } => StatusCode::BAD_GATEWAY,
            ApiError::Conflict { .. } => StatusCode::CONFLICT,
            ApiError::NotImplemented { .. } => StatusCode::NOT_IMPLEMENTED,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = json!({ "code": self.code(), "message": self.to_string() });
        (self.status(), Json(body)).into_response()
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
            SchedulerError::Admission(reason) => ApiError::Unavailable {
                reason: reason.to_string(),
            },
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
    /// Runtime `default_model` overrides from `/har/config/default`, layered over
    /// the boot config (Step 9: the yaml/json stays the source of truth; overrides
    /// persist back to it on every set).
    default_overrides: Arc<Mutex<BTreeMap<CategoryId, Option<String>>>>,
    /// Last-use hints per model (fallback MRU for AUX x N; eviction LRU later).
    usage: Arc<Mutex<HashMap<String, Instant>>>,
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
    ) -> Self {
        let control_addr = format!(
            "{}:{}",
            config.internal_host,
            config.control_category().public_port
        );
        ApiState {
            control_addr,
            scheduler,
            listeners,
            witness,
            default_overrides: Arc::new(Mutex::new(BTreeMap::new())),
            usage: Arc::new(Mutex::new(HashMap::new())),
            proxy: reqwest::Client::builder()
                .timeout(Duration::from_secs(600))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            config,
        }
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
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("simplehar.json")))
        .unwrap_or_else(|| std::path::PathBuf::from("simplehar.json"));
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(error = %error, "simplehar.json unreadable; rewriting from boot config");
            serde_json::to_value(&*config).expect("invariant: config serializes")
        }),
        Err(_) => serde_json::to_value(config).expect("invariant: config serializes"),
    };
    let id = category.to_string();
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
    if let Err(error) = std::fs::rename(&tmp, &path) {
        tracing::error!(error = %error, "cannot persist default_model: rename failed");
        return false;
    }
    tracing::info!(category = %id, model = ?model, "default_model persisted to simplehar.json");
    true
}

/// `GET /har/status`: boot info + slot table + jobs, one call (G13).
async fn har_status(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "control_addr": state.control_addr,
        "categories": categories,
        "listeners": state.listeners.snapshot(),
        "mem": mem_section(&state).await,
        "slots": scheduler.slots,
        "jobs": scheduler.jobs,
        "catalog_errors": scheduler.catalog_errors,
        "config_errors": [],
    })))
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
        } => {
            json!({ "status": "already", "model": model, "category": category, "port": port })
        }
        Reply::Done => json!({ "status": "done" }),
    }))
}

/// `POST /har/unload {"category": "WORK"}` — strict body: `{}` or garbage is `400`,
/// never an implicit unload-all (v3.3 CRITICAL fix).
async fn har_unload(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with a `category` field: {rejection}"),
    })?;
    let Some(raw) = body.get("category").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: "missing required field `category` (WORK|RP|AUX|EMB); unload-all is deliberate and not implicit".to_string(),
        });
    };
    let category: CategoryId = raw.parse().map_err(|_| ApiError::BadRequest {
        reason: format!("unknown category `{raw}` (WORK|RP|AUX|EMB)"),
    })?;
    state
        .scheduler
        .unload(category)
        .await
        .map_err(ApiError::from)?;
    Ok(Json(json!({ "status": "unloading", "category": category })))
}

/// The merged document every `har_state` event carries: one push replaces the
/// page's per-panel polling (Step 14).
async fn merged_state(state: &ApiState) -> Result<Value, ApiError> {
    let status = state.scheduler.status().await.map_err(ApiError::from)?;
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            json!({
                "id": entry.name,
                "object": "model",
                "owned_by": "simplehar",
                "engine": entry.engine,
                "category": entry.category,
                "vram_gb": entry.vram_gb,
                "cpu_offload": entry.raw.get("cpu_offload").is_some_and(|v| v == "true"),
            })
        })
        .collect();
    Ok(json!({
        "status": status,
        "models": models,
        "listeners": state.listeners.snapshot(),
    }))
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

/// `GET /har/models` — the full unfiltered catalog for the control page (G5: the
/// per-listener `/v1/models` stays category-filtered).
async fn har_models(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            json!({
                "id": entry.name,
                "object": "model",
                "owned_by": "simplehar",
                "engine": entry.engine,
                "category": entry.category,
                "vram_gb": entry.vram_gb,
                "cpu_offload": entry.raw.get("cpu_offload").is_some_and(|v| v == "true"),
            })
        })
        .collect();
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
            reason: "missing required field `category` (WORK|RP|AUX|EMB)".to_string(),
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
    if let Some(raw) = body.get("category").and_then(Value::as_str) {
        let category: CategoryId = raw.parse().map_err(|_| ApiError::BadRequest {
            reason: format!("unknown category `{raw}` (WORK|RP|AUX|EMB)"),
        })?;
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

    // A loaded model whose category changed must move slots, not lie in place.
    let category_changed = edits.iter().any(|(key, _)| *key == "category");
    if category_changed && old_category != entry.category {
        // old_category was the entry's category before the edit; the fresh entry
        // (post-reload) holds the new one, resolved below from the new catalog.
        let catalog = state.catalog().await?;
        if let Some(fresh) = catalog.iter().find(|m| m.name == entry.name) {
            if fresh.category != old_category {
                let _ = state.scheduler.unload(old_category).await;
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

/// The `/har/status` memory block: declared config numbers plus what the witness
/// actually sees, labeled with its source (F-7 lesson: never present a number
/// without saying whose number it is).
async fn mem_section(state: &ApiState) -> Value {
    let ledger: f64 = state
        .scheduler
        .status()
        .await
        .map(|status| status.slots.iter().filter_map(|slot| slot.need_gib).sum())
        .unwrap_or_default();
    match state.witness.sample().await {
        Ok(sample) => json!({
            "total_gb": sample.total_gib,
            "reserve_gb": state.config.vram_reserve_gb,
            "ledger_gb": ledger,
            "observed_used_gb": sample.used_gib,
            "external_gb": sample.external_gib(&[]),
            "source": "witness",
        }),
        Err(reason) => json!({
            "total_gb": state.config.vram_total_gb,
            "reserve_gb": state.config.vram_reserve_gb,
            "ledger_gb": ledger,
            "observed_used_gb": null,
            "external_gb": null,
            "source": format!("unavailable ({reason})"),
        }),
    }
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
async fn ensure_target(listener: &ListenerState, body: &Value) -> Result<(String, u16), ApiError> {
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
            let port = swap_for_port(api, &entry.name, entry.category).await?;
            Ok((entry.name.clone(), port))
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
                return Ok((model.clone(), *port));
            }
            let default = api.default_model(listener.category);
            match default {
                Some(model) => {
                    let port = swap_for_port(api, &model, listener.category).await?;
                    Ok((model, port))
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

/// Ensures `model` is serving in `category` and returns its engine port.
async fn swap_for_port(api: &ApiState, model: &str, category: CategoryId) -> Result<u16, ApiError> {
    let reply = api
        .scheduler
        .swap(model, category)
        .await
        .map_err(ApiError::from)?;
    let port = match reply {
        Reply::Loaded(receipt) => receipt.port,
        Reply::Already { port, .. } => port,
        Reply::Done => 0,
    };
    api.touch(model);
    Ok(port)
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
    let (model, port) = ensure_target(&listener, &body).await?;
    let path = uri.path().to_string();
    body["model"] = Value::String(model);
    let url = format!("http://127.0.0.1:{port}{path}");
    let upstream = listener
        .api
        .proxy
        .post(&url)
        .json(&body)
        .send()
        .await
        .map_err(|error| ApiError::Upstream {
            reason: format!("engine on :{port} unreachable: {error}"),
        })?;
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
        .route("/har/jobs/{id}", get(har_job))
        .route("/har/load", post(har_load))
        .route("/har/unload", post(har_unload))
        .route("/har/rescan", post(har_rescan))
        .route("/har/models", get(har_models))
        .route("/har/config/default", post(har_config_default))
        .route("/har/config/model", post(har_config_model));
    Router::new()
        .merge(category_router(Arc::clone(&api), control_category))
        .merge(har.with_state(Arc::clone(&api)))
        .merge(crate::webui::router(api))
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
    }
}
