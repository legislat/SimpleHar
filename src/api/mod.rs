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
use crate::scheduler::{JobId, Reply, SchedulerError, SchedulerHandle, SchedulerStatus};
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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// A request refused for protection, not for state: the CSRF guard's cross-site
    /// `Origin` (`403`). Deliberately distinct from [`ApiError::Conflict`], whose
    /// live-state clashes (cancelled, superseded, port held) invite a retry; a security
    /// refusal must tell the client to stop. The guard's own contract and T13 both
    /// specify 403.
    #[error("forbidden: {reason}")]
    Forbidden { reason: String },
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
            ApiError::Forbidden { .. } => "forbidden",
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
            ApiError::Forbidden { .. } => StatusCode::FORBIDDEN,
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

/// One §0.2 boot measure that can only complete AFTER [`ApiState`] is shared — a
/// listener binds, the container reclaim returns — so it is interior-mutable: every
/// holder of a clone writes the same cell, first-write-wins.
///
/// The stored integer is biased by 1 so the atomic's `0` keeps its "never happened"
/// meaning. `Instant::elapsed` truncates, so an unbiased `0` is what a fast first
/// bind actually reports; without the bias the honest measurement and the sentinel
/// would be the same value and the key would silently stay out of
/// `/har/status.boot`.
#[derive(Debug, Clone, Default)]
pub struct BootMsStamp(Arc<AtomicU64>);

impl BootMsStamp {
    /// Records `ms` unless an earlier stamp already stands.
    ///
    /// # Returns
    ///
    /// `true` when this call won the race, `false` when a previous value holds —
    /// every supervisor stamps and only the first bind is the measure.
    pub fn stamp(&self, ms: u64) -> bool {
        self.0
            .compare_exchange(0, ms.saturating_add(1), Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    /// The stamped value, or `None` while the measured event has not happened.
    #[must_use]
    pub fn get(&self) -> Option<u64> {
        match self.0.load(Ordering::Acquire) {
            0 => None,
            biased => Some(biased - 1),
        }
    }

    /// `skip_serializing_if` predicate: the key is absent, not null, until stamped.
    fn unset(&self) -> bool {
        self.get().is_none()
    }

    /// Serializes as the `Option<u64>` the API promises, un-biasing the raw cell.
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.get() {
            None => serializer.serialize_none(),
            Some(ms) => serializer.serialize_some(&ms),
        }
    }
}

/// Boot measures (§0.2): the numbers that make "the harness is up" observable.
/// Every one of them is **milliseconds since the single `boot_started` epoch**
/// `main` captures before the guard is taken, including the two that complete after
/// [`ApiState`] is shared and therefore live in [`BootMsStamp`] cells.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct BootTimings {
    /// Config load + validation duration.
    pub config_ms: u64,
    /// Time to construct + spawn the scheduler; the initial catalog scan runs on
    /// the actor task and is NOT measured here — 4389a0a.
    pub scheduler_spawn_ms: u64,
    /// Wall time until every listener was Serving or Down (set by `main`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bind_ms: Option<u64>,
    /// ms-since-start of the FIRST successful bind across all category supervisors
    /// (§0.2 step 5): when the harness first became reachable, as opposed to when
    /// every category had a verdict. Absent from `/har/status.boot` until it happens.
    #[serde(
        skip_serializing_if = "BootMsStamp::unset",
        serialize_with = "BootMsStamp::serialize"
    )]
    pub first_bind_at: BootMsStamp,
    /// Wall ms of the boot container reclaim, from its dispatch (after `boot ready`,
    /// §0.2 step 4 "off the bind path") to completion. Absent until the reclaim
    /// returns, which needs docker/WSL to have any owned container to talk about.
    #[serde(
        skip_serializing_if = "BootMsStamp::unset",
        serialize_with = "BootMsStamp::serialize"
    )]
    pub reap_ms: BootMsStamp,
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

    /// Handle to the §0.2 `first_bind_at` cell, for the supervisor that first binds.
    /// Cloning the cell out of the lock keeps the stamp off the bind path: no writer
    /// ever holds that lock across an `.await`.
    #[must_use]
    pub fn first_bind_stamp(&self) -> BootMsStamp {
        self.boot
            .read()
            .expect("boot timings lock poisoned by a writer panic")
            .first_bind_at
            .clone()
    }

    /// Handle to the §0.2 `reap_ms` cell, for the boot reclaim to fill on completion.
    #[must_use]
    pub fn reap_stamp(&self) -> BootMsStamp {
        self.boot
            .read()
            .expect("boot timings lock poisoned by a writer panic")
            .reap_ms
            .clone()
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
    /// `simplehar-se.json` atomically (temp file + rename). In-memory wins even when
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

/// Where a config write-back belongs: the file boot actually loaded (`SIMPLEHAR_CONFIG`
/// included), else the default beside the executable — taken from
/// [`crate::config::default_config_path`] so the file SE reads and the file SE writes can
/// never drift apart.
///
/// The final `PathBuf::from(CONFIG_FILE)` is a last resort for a process that cannot
/// resolve its own executable path: `boot_config` always stamps `source_path`, so this is
/// reached in practice only by a hand-built `AppConfig`. It used to name `simplehar.json`,
/// the older app's file, which SE promises never to write and never reads — a latent
/// hazard in a nearly unreachable branch, not a live bug (README review, 2026-10-27).
fn config_write_path(config: &AppConfig) -> std::path::PathBuf {
    config
        .source_path
        .clone()
        .or_else(crate::config::default_config_path)
        .unwrap_or_else(|| std::path::PathBuf::from(crate::config::CONFIG_FILE))
}

/// Writes `category.default_model` into the config file atomically: parse the
/// on-disk file (or the boot config when absent), patch the one field, temp+rename.
///
/// The whole read-patch-write-rename runs inside the model write-back's gate and onto
/// a per-process unique temp: two handlers reach this function (`/har/config/default`
/// and `/har/config/model`'s dangling-default clear) on separate workers, and an
/// ungated read-modify-write onto one fixed temp lost one of their fields while both
/// answered `persisted: true` (FINDINGS-07 [3]).
fn persist_default_model(config: &AppConfig, category: CategoryId, model: Option<&str>) -> bool {
    // One critical section with every yaml rewrite (FINDINGS-07 [3]): the second
    // writer can no longer read the ORIGINAL while the first one is between its
    // patch and its rename, which is how a persisted field used to vanish from disk.
    let _gate = crate::model::writeback::acquire_write_gate();
    let path = config_write_path(config);
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(error = %error, path = %path.display(), "config unreadable; rewriting from boot config");
            serde_json::to_value(&*config).expect("invariant: config serializes")
        }),
        Err(error) => {
            // Silent before: the only difference between this arm and the one above
            // is that the file could not be read at all, and both rewrite the whole
            // file from the boot snapshot, so both must say so (FINDINGS-07 [2]'s
            // unlogged arm).
            tracing::warn!(error = %error, path = %path.display(), "config file missing or unreadable; writing from boot config");
            serde_json::to_value(config).expect("invariant: config serializes")
        }
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
        tracing::error!(path = %path.display(), "cannot persist default_model: no categories in the config file");
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
    // Unique per (process, sequence), mirroring the yaml temp in `model::writeback`:
    // on the fixed name a second writer truncated the first one's temp, so one
    // rename failed while the other's content landed (FINDINGS-07 [3]).
    let tmp = path.with_extension(format!(
        "json.{}.{}.sh.tmp",
        std::process::id(),
        crate::model::writeback::next_temp_seq()
    ));
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
        // The temp is ours alone; leaving it behind turned a failed write into
        // permanent litter (writeback does the same on its rename failure).
        let _ = std::fs::remove_file(&tmp);
        tracing::error!(error = %error, "cannot persist default_model: rename failed");
        return false;
    }
    tracing::info!(category = %id, model = ?model, path = %path.display(), "default_model persisted to the config file");
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

/// The `409` for a catalog that claims one identity twice (FINDINGS-11 [4]).
///
/// Names every colliding entry's yaml so the operator can fix the catalog without
/// guessing which file the harness picked: a wrong-quantization serve, a 404 pointing
/// at another category's port, and a write-back landing on the first match are all the
/// same defect, and none of them is the client's to resolve.
///
/// # Arguments
///
/// * `query` - the `model` string as the client sent it
/// * `entries` - every entry that matched the same resolution tier
fn ambiguous_catalog_error(query: &str, entries: &[&ModelEntry]) -> ApiError {
    let files = entries
        .iter()
        .map(|entry| match entry.file.as_deref() {
            Some(path) => path.display().to_string(),
            None => format!("<synthetic entry named `{}`>", entry.name),
        })
        .collect::<Vec<_>>()
        .join(", ");
    ApiError::Conflict {
        reason: format!(
            "model `{query}` is ambiguous: {} catalog entries claim it ({files}); \
             rename one or remove the duplicate",
            entries.len()
        ),
    }
}

/// Resolves a client-supplied model name, refusing an ambiguous catalog instead of
/// picking the first match.
///
/// `Found` returns the entry the caller was always given; `None` is mapped through
/// `not_found` so each route keeps its exact error text and status; `Ambiguous` becomes
/// the named-entries `409`. There is deliberately no convenience that hands out
/// `Ambiguous`'s first entry — a caller that must not guess cannot be written.
///
/// # Arguments
///
/// * `query` - the `model` string as the client sent it
/// * `catalog` - the catalog to resolve against
/// * `not_found` - builds the caller's own error for the no-match case
///
/// # Errors
///
/// [`ApiError::Conflict`] (`409`) for an ambiguous query **or** a uniquely-found
/// entry whose `name` is duplicated in the catalog (the actor re-resolves by
/// name find-first, so a shared name must never reach it), else whatever
/// `not_found` returns for an unknown one.
fn resolve_unique<'a, F>(
    query: &str,
    catalog: &'a [ModelEntry],
    not_found: F,
) -> Result<&'a ModelEntry, ApiError>
where
    F: FnOnce(&str) -> ApiError,
{
    match crate::catalog::resolve(query, catalog) {
        crate::catalog::Resolution::Found(entry) => {
            // A uniquely-resolved entry can still hand its `name` to the actor,
            // which re-resolves name→entry find-first (scheduler/mod.rs). Two
            // entries sharing a `name:` therefore serve the wrong twin under the
            // requested identity (one GGUF, one booking) with a green 200 even
            // when the client's own query matched exactly one of them (review
            // FINDINGS-FIXTRACK [1]). Refuse any Found whose name is duplicated
            // in the catalog, naming every colliding file.
            let twins: Vec<&ModelEntry> = catalog
                .iter()
                .filter(|other| other.name == entry.name)
                .collect();
            if twins.len() > 1 {
                return Err(ambiguous_catalog_error(&entry.name, &twins));
            }
            Ok(entry)
        }
        crate::catalog::Resolution::None => Err(not_found(query)),
        crate::catalog::Resolution::Ambiguous(entries) => {
            Err(ambiguous_catalog_error(query, &entries))
        }
    }
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
    let entry = resolve_unique(model, &catalog, |query| ApiError::NotFound {
        what: format!("model `{query}`; known models are listed at each listener's /v1/models"),
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
        "measured_min_gb": entry.measured_min_gb,
        "measured_offload": entry.measured_offload,
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
///
/// `pub` so the token test iterates this list rather than its own copy of it: when the
/// two were separate, a stylesheet added to the page but not here shipped an unstyled
/// control page while the test stayed green, because each asserted a private truth.
pub const PUBLIC_PATHS: [&str; 5] = [
    "/",
    "/index.html",
    "/app.js",
    "/style.css",
    // The page links Pico's base stylesheet, so it must answer before a token exists;
    // omitting it left the control page rendering unstyled whenever `control_token` was
    // set (README review, 2026-10-27).
    "/pico.min.css",
];

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
    // The page polls /har/status continuously and opens the log panel repeatedly;
    // those lines would evict real evidence from the ring during long boots — and
    // reading the log must not be an event that pushes the log's own evidence out
    // (FINDINGS-15 [10]). Extended logging restores them (operator, 2026-10-26).
    let routine_poll =
        method == axum::http::Method::GET && matches!(path.as_str(), "/har/status" | "/har/log");
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
                return Some(ApiError::Forbidden {
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
/// persisted to `simplehar-se.json` atomically.
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
        let entry = resolve_unique(model, &catalog, |query| ApiError::BadRequest {
            reason: format!("unknown model `{query}`"),
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
    let entry = resolve_unique(model_name, &catalog, |query| ApiError::NotFound {
        what: format!("model `{query}`"),
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
        // FINDINGS-12 [1]: a surviving `is_embedding_model: true` beside the new
        // non-EMB `category:` is an `EmbeddingCategoryConflict` that makes the next
        // scan DROP the whole file — one dropdown click deleted the model while the
        // endpoint answered success. The two facts are one fact, so the removal
        // rides the same atomic rewrite as the category write. EMB keeps the key:
        // alias + EMB is the legal shape, and removing it would destroy an
        // operator-authored declaration for no gain.
        if category != CategoryId::Emb {
            edits.push(("is_embedding_model", None));
        }
    }
    if let Some(cpu) = body.get("cpu").and_then(Value::as_bool) {
        edits.push(("cpu_offload", Some(cpu.to_string())));
    }
    if edits.is_empty() {
        return Err(ApiError::BadRequest {
            reason: "nothing to edit: send `category` and/or `cpu`".to_string(),
        });
    }
    let refs: Vec<(&str, Option<&str>)> = edits
        .iter()
        .map(|(key, value)| (*key, value.as_deref()))
        .collect();
    // One rewrite for every edit: calling `set_flag_on_file` per key would leave
    // the file half-edited (category moved, alias still there) if a write failed
    // between the two — 1g's non-atomic defect, not replicated here.
    crate::model::writeback::set_flags_on_file(&path, &refs)
        .map_err(|reason| ApiError::Internal { reason })?;
    // The catalog must reflect the file the moment the operator looks again.
    state.scheduler.reload().await.map_err(ApiError::from)?;

    // A model whose category actually changed must move slots, not lie in place
    // (v3.3 §-behavior, kept). Only ITS OWN slot: `unload(old_category)` freed every
    // occupant of the old category, so retagging one AUX model killed the other
    // five's engines and whatever WORK held, in-flight generations included
    // (FINDINGS-07 [1]). A retag of a never-loaded model is the idempotent `Done`
    // the actor already guarantees for `unload_model` (pinned by scheduler_actor's
    // T12.2 test), which is why the old inner catalog-name test — meant to skip
    // that no-op — is gone: it never asked whether the moved model was loaded, only
    // whether SOME entry shared its name in a different category, so the duplicate
    // name of chunk 7 [9] was enough to trigger the collateral kill. If the moved
    // model was its old category's default, that default now dangles — clear it too
    // (operator finding #1, 2026-10-26).
    if let Some(new_cat) = requested_category.filter(|c| *c != old_category) {
        let _ = state.scheduler.unload_model(&entry.name).await;
        let catalog = state.catalog().await?;
        if let Some(default) = state.default_model(old_category) {
            // Only an UNAMBIGUOUS resolution proves the category's default is the
            // model that just moved. An ambiguous one is refused here rather than
            // accepted on a guess: clearing the wrong default would rewrite the
            // operator's config file for a model that never moved.
            match crate::catalog::resolve(&default, &catalog) {
                crate::catalog::Resolution::Found(d) if d.name == entry.name => {
                    state.set_default_model(old_category, None);
                    tracing::info!(
                        model = %entry.name,
                        from = %old_category,
                        to = %new_cat,
                        "cleared dangling category default"
                    );
                }
                crate::catalog::Resolution::Ambiguous(clients) => tracing::warn!(
                    model = %entry.name,
                    category = %old_category,
                    entries = clients.len(),
                    "left the category default alone: its name is ambiguous in the catalog"
                ),
                crate::catalog::Resolution::Found(_) | crate::catalog::Resolution::None => {}
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

/// Resolves a catalog NAME (not an engine id) to `(name, port, serve_id)`, loading it
/// into `listener`'s category when it is not serving yet.
///
/// This is deliberately a separate entry point from [`ensure_target`]: `proxy` rewrites
/// `body["model"]` to the engine's self-reported id (F-4), so anything re-resolving a
/// request after that rewrite must pass the NAME it captured beforehand. The T10.1
/// crash-retry used to re-resolve from the rewritten body and therefore looked the
/// engine's id up in the catalog, which 404s every llama engine (its id is the gguf
/// path) - the restart succeeded and the request still failed.
///
/// # Errors
///
/// [`ApiError::NotFound`] for an unknown model or one belonging to another category,
/// [`ApiError::Conflict`] for a name more than one catalog entry claims.
async fn ensure_named(
    listener: &ListenerState,
    model: &str,
) -> Result<(String, u16, Option<String>), ApiError> {
    let api = &listener.api;
    let catalog = api.catalog().await?;
    let entry = resolve_unique(model, &catalog, |query| ApiError::NotFound {
        what: format!(
            "model `{query}` on the {} listener; its /v1/models lists the ids it serves",
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
        Some(model) => ensure_named(listener, model).await,
        None => {
            // F-2 rule (operator, 2026-10-24): "use this category".
            let status = api.scheduler.status().await.map_err(ApiError::from)?;
            if let Some((model, port, serve_id)) =
                pick_loaded_occupant(api, &status, listener.category)
            {
                api.touch(&model);
                api.scheduler.touch(&model);
                return Ok((model, port, serve_id));
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

/// Picks the serving occupant for a no-`model` request: MRU by last proxy use,
/// with untouched occupants sorted by their ACTUAL load time (FINDINGS-06 [1]:
/// `slot.loaded_at`, never a fabricated instant that underflows on a young
/// process). `None` when the category serves nothing.
fn pick_loaded_occupant(
    api: &ApiState,
    status: &SchedulerStatus,
    category: CategoryId,
) -> Option<(String, u16, Option<String>)> {
    let mut occupants: Vec<(String, u16, Instant)> = status
        .slots
        .iter()
        .filter(|slot| slot.category == category)
        .filter_map(|slot| match (&slot.model, slot.port) {
            (Some(model), Some(port)) if port != 0 => Some((
                model.clone(),
                port,
                api.last_used(model).unwrap_or(slot.loaded_at),
            )),
            _ => None,
        })
        .collect();
    occupants.sort_by_key(|&(_, _, used)| std::cmp::Reverse(used)); // MRU first
    occupants.first().map(|(model, port, _)| {
        let serve_id = status
            .slots
            .iter()
            .find(|slot| slot.model.as_deref() == Some(model.as_str()))
            .and_then(|slot| slot.serve_id.clone());
        (model.clone(), *port, serve_id)
    })
}

/// What the proxy does when a forward fails before any response head arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAction {
    /// The request never got a response and nothing listens on the port: the
    /// engine class of failure that justifies exactly one reap + reload + retry.
    Restart,
}

/// Classifies a `send()` failure for the crash-retry arm (FINDINGS-06 [2]).
///
/// Restart-and-retry ONLY when the request never got a response AND the engine is
/// not listening — reqwest's `is_connect()` (refused/reset before response head).
/// A timeout (`is_timeout()`) is a live-but-slow engine (long non-streamed
/// generation, huge prefill): reaping it would discard a healthy KV cache and
/// re-forward the same slow request, so it answers `502` naming the class
/// instead. Any other send error is likewise never treated as death.
///
/// # Arguments
///
/// * `error` - the error returned by `reqwest::Client::send()`
/// * `port` - the engine port the forward targeted, for the operator-facing reason
///
/// # Errors
///
/// [`ApiError::Upstream`] for every non-connect class.
///
/// Public only so the integration suite can feed it real reqwest error values;
/// the proxy loop is its sole caller in production code.
pub fn classify_send_failure(error: &reqwest::Error, port: u16) -> Result<RetryAction, ApiError> {
    if error.is_connect() {
        return Ok(RetryAction::Restart);
    }
    let reason = if error.is_timeout() {
        format!("engine on :{port} timed out; not restarting a live engine")
    } else {
        format!("forward failed: {error}")
    };
    Err(ApiError::Upstream { reason })
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

    // T10.1: a connection-level failure (engine died before any response) triggers
    // exactly one reap + reload + retry. Only `is_connect()` classifies as death
    // (FINDINGS-06 [2]): a timeout means a live engine is still thinking, and
    // reaping it discards a healthy KV cache. A genuine upstream HTTP status is
    // never retried (A9), and a death mid-stream surfaces in the body stream,
    // past this loop.
    let mut port = port;
    let mut attempts = 0;
    let upstream = loop {
        let url = format!("http://127.0.0.1:{port}{path}");
        match listener.api.proxy.post(&url).json(&body).send().await {
            Ok(upstream) => break upstream,
            Err(error) => {
                classify_send_failure(&error, port)?;
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
                // Re-resolve from the NAME, never from `body`: `body["model"]` holds the
                // engine id written above, and an engine id is not a catalog key. The
                // fresh occupant may serve on a different port and may report a
                // different id, so both come from the new receipt; when discovery found
                // no id, fall back to the harness name exactly as the first forward did
                // rather than keeping the dead engine's.
                let (_, new_port, new_serve_id) = ensure_named(&listener, &model).await?;
                port = new_port;
                body["model"] = Value::String(new_serve_id.unwrap_or_else(|| model.clone()));
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
    use crate::scheduler::SlotStatus;
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn config_write_path_follows_boot_and_never_the_legacy_file() {
        // Write-back must land where boot reads. Comparing the WHOLE path is what makes
        // this bite: every fallback branch ends in CONFIG_FILE, so a filename-only
        // assertion would pass even if the default directory moved under it.
        let no_source = AppConfig {
            source_path: None,
            ..AppConfig::default()
        };
        let expected = crate::config::default_config_path().expect("test process has an exe path");
        assert_eq!(
            config_write_path(&no_source),
            expected,
            "with no stamped path, write to boot's own default location"
        );
        assert_ne!(
            config_write_path(&no_source).file_name(),
            Some(std::ffi::OsStr::new(crate::config::LEGACY_CONFIG_FILE)),
            "SE never writes the older app's config file"
        );

        let loaded = std::path::PathBuf::from(r"C:\Configs\lan.json");
        let stamped = AppConfig {
            source_path: Some(loaded.clone()),
            ..AppConfig::default()
        };
        assert_eq!(
            config_write_path(&stamped),
            loaded,
            "the file boot actually loaded always wins"
        );
    }

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
            measured_min_gb: None,
            measured_offload: None,
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
            measured_min_gb: None,
            measured_offload: None,
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

    fn unit_state() -> ApiState {
        // The selection and classification logic never issues a scheduler command,
        // so a dummy handle (no actor) is the honest seam here.
        ApiState::new(
            AppConfig::default(),
            SchedulerHandle::test_dummy(),
            crate::listeners::ListenerRegistry::new(),
            Arc::new(crate::budget::LedgerOnlyWitness),
            BootTimings::default(),
        )
    }

    /// A state with a LIVE actor: `status_document` asks the scheduler for its
    /// snapshot, so [`unit_state`]'s dummy handle cannot serve a `/har/status` test.
    /// The empty catalog dir keeps the actor's scan trivial and engine-free.
    fn live_state(boot: BootTimings) -> ApiState {
        let config = AppConfig::default();
        let mut adapters: BTreeMap<String, Arc<dyn crate::engine::EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(crate::engine::mock::MockAdapter) as Arc<dyn crate::engine::EngineAdapter>,
        );
        let scheduler = crate::scheduler::Scheduler::start(
            config.clone(),
            crate::testutil::temp_dir("api-boot"),
            adapters,
            Arc::new(crate::process::ProcessRuntime::new())
                as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(crate::budget::LedgerOnlyWitness),
        );
        ApiState::new(
            config,
            scheduler,
            crate::listeners::ListenerRegistry::new(),
            Arc::new(crate::budget::LedgerOnlyWitness),
            boot,
        )
    }

    #[test]
    fn boot_stamp_is_first_write_wins() {
        // FINDINGS-14 [4]: the two late measures are written by whoever finishes the
        // work, so the cell must admit exactly one writer. A plain store would let
        // the last supervisor to bind overwrite the first one's timestamp.
        let stamp = BootMsStamp::default();
        assert_eq!(
            stamp.get(),
            None,
            "an unstamped measure is absent, never reported as 0"
        );
        assert!(stamp.stamp(3), "the first writer wins");
        assert!(
            !stamp.stamp(9_999),
            "a later writer must be ignored, not overwrite the measure"
        );
        assert_eq!(stamp.get(), Some(3));
        let zero = BootMsStamp::default();
        assert!(zero.stamp(0), "a sub-millisecond event is still an event");
        assert_eq!(
            zero.get(),
            Some(0),
            "0 ms is a measurement, not the 'never happened' sentinel"
        );
        assert!(
            !zero.stamp(5),
            "and it still closes the cell to later writers"
        );
    }

    #[test]
    fn concurrent_boot_stamps_admit_exactly_one_writer() {
        // The same rule under a real race: three supervisors released together, one
        // value lands and it is one of the three that were offered.
        let stamp = BootMsStamp::default();
        let barrier = std::sync::Barrier::new(3);
        let winners = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for ms in [11_u64, 22, 33] {
                let stamp = stamp.clone();
                let barrier = &barrier;
                let winners = &winners;
                scope.spawn(move || {
                    barrier.wait();
                    if stamp.stamp(ms) {
                        winners.lock().expect("winners lock").push(ms);
                    }
                });
            }
        });
        let won = winners.lock().expect("winners lock").clone();
        assert_eq!(won.len(), 1, "exactly one writer may land; got {won:?}");
        assert_eq!(
            stamp.get(),
            Some(*won.first().expect("one winner")),
            "the value `/har/status.boot` reports is the winner's"
        );
    }

    #[tokio::test]
    async fn status_boot_reports_the_five_measures_as_they_complete() {
        // FINDINGS-14 [4]: `/har/status.boot` must carry the whole §0.2 measure set.
        // The three boot-thread fields are present from the start; the two that
        // complete after `ApiState` is shared are ABSENT (skipped, not null) until
        // stamped, then present with exactly the stamped value — the same shape
        // `bind_ms` has always had.
        let api = live_state(BootTimings {
            config_ms: 4,
            scheduler_spawn_ms: 6,
            ..BootTimings::default()
        });
        let before = status_document(&api).await.expect("status document");
        assert_eq!(before["boot"]["config_ms"], json!(4));
        assert_eq!(before["boot"]["scheduler_spawn_ms"], json!(6));
        assert!(
            before["boot"].get("bind_ms").is_none(),
            "bind_ms keeps its skip-when-unset shape: {before}"
        );
        assert!(
            before["boot"].get("first_bind_at").is_none(),
            "an unstamped first bind is absent from the document: {before}"
        );
        assert!(
            before["boot"].get("reap_ms").is_none(),
            "an unstamped reclaim is absent from the document: {before}"
        );

        api.first_bind_stamp().stamp(1);
        api.reap_stamp().stamp(4_321);
        api.boot.write().expect("boot timings lock").set_bound(20);

        let after = status_document(&api).await.expect("status document");
        assert_eq!(after["boot"]["first_bind_at"], json!(1), "ms-since-start");
        assert_eq!(after["boot"]["reap_ms"], json!(4_321));
        assert_eq!(after["boot"]["bind_ms"], json!(20));
        assert!(
            !api.first_bind_stamp().stamp(500),
            "the shared cell behind the lock is the one the route serializes"
        );
        let again = status_document(&api).await.expect("status document");
        assert_eq!(
            again["boot"]["first_bind_at"],
            json!(1),
            "and it is still first-write-wins through the API handle"
        );
    }

    /// FINDINGS-14 [7] + the 2026-10-28 "A + amber" ruling: a category whose
    /// supervisor never spoke must be PRESENT in the `listeners` object and LABELED
    /// `Pending`, so the page can never read "no verdict" as "no such category" (the
    /// old shape made the two indistinguishable). Falsifier: return the registry to an
    /// unseeded `new()` and the four `Pending` assertions fail on a missing key.
    #[tokio::test]
    async fn a_category_with_no_verdict_is_present_and_labeled_pending() {
        let api = live_state(BootTimings::default());
        let fresh = status_document(&api).await.expect("status document");
        assert_eq!(
            fresh["listeners"].as_object().map(|m| m.len()),
            Some(4),
            "the passthrough serializes every configured category, not only the marked ones: {fresh}"
        );
        for name in ["WORK", "RP", "AUX", "EMB"] {
            assert_eq!(
                fresh["listeners"][name],
                json!("Pending"),
                "a never-marked category is present AND labeled: {fresh}"
            );
        }

        api.listeners.mark_serving(CategoryId::Work);
        api.listeners.mark_down(
            CategoryId::Emb,
            "cannot bind 127.0.0.1:8711: address in use".to_string(),
        );
        let after = status_document(&api).await.expect("status document");
        assert_eq!(after["listeners"]["WORK"], json!("Serving"));
        assert_eq!(
            after["listeners"]["RP"],
            json!("Pending"),
            "neighbours keep their no-verdict label while the boot is incomplete"
        );
        assert_eq!(
            after["listeners"]["EMB"]["Down"]["reason"],
            json!("cannot bind 127.0.0.1:8711: address in use"),
            "the four verdicts stay three distinct shapes"
        );
        assert!(
            after["listeners"].get("AUX").is_some(),
            "a category nobody marked is never the same as a removed one: {after}"
        );
        assert!(
            after["listeners"].get("NOSUCH").is_none(),
            "an absent category is a MISSING key — which seeding makes unrepresentable"
        );
    }

    /// FINDINGS-15 [10]: reading the ring must not append to the ring, or the act of
    /// asking "why did the last load fail" evicts the answer. Falsifier: drop
    /// `/har/log` from the routine-poll exemption and the two-reads assertion fails
    /// with two `request complete` lines.
    #[tokio::test]
    async fn reading_the_log_never_appends_to_the_log() {
        // The switch is process-global and off at boot; state it explicitly so this
        // pin means "exempted", not "someone turned the logging off".
        crate::observability::set_extended_logging(false);
        let api = Arc::new(unit_state());
        let session = crate::tray::testlog::Session::start();
        for _ in 0..2 {
            let response = control_router(Arc::clone(&api))
                .oneshot(
                    Request::get("/har/log")
                        .body(Body::empty())
                        .expect("log request"),
                )
                .await
                .expect("in-process log read");
            assert_eq!(response.status(), StatusCode::OK, "/har/log must answer");
        }
        // And a GET that is not a routine poll still reports, so the exemption is
        // scoped rather than an accidental mute of the whole request line.
        let other = control_router(Arc::clone(&api))
            .oneshot(
                Request::get("/health")
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("in-process health read");
        assert_eq!(other.status(), StatusCode::OK, "/health must answer");
        let lines = session.stop();
        let log_lines = lines
            .iter()
            .filter(|line| line.contains("request complete") && line.contains("/har/log"))
            .count();
        assert_eq!(
            log_lines, 0,
            "the log's own reads may not evict its own evidence: {lines:?}"
        );
        assert_eq!(
            lines
                .iter()
                .filter(|line| line.contains("request complete") && line.contains("/health"))
                .count(),
            1,
            "non-routine GETs stay in the ring (the exemption is scoped): {lines:?}"
        );
    }

    fn occupied_slot(model: &str, port: u16, loaded_at: Instant) -> SlotStatus {
        SlotStatus {
            category: CategoryId::Aux,
            index: 0,
            model: Some(model.to_string()),
            port: Some(port),
            pid: Some(1),
            need_gib: Some(4.0),
            idle_s: Some(0),
            serve_id: Some("mock-model".to_string()),
            loading_job: None,
            loaded_at,
        }
    }

    #[test]
    fn no_fabricated_instants_survive_short_uptime() {
        // FINDINGS-06 [1]: the removed fallback sorted untouched occupants by a
        // fabricated `Instant::now() - 3600s`, which panics outright on a host
        // younger than an hour. Fixture instants are FORWARD offsets of `now`
        // (`Instant + Duration` cannot underflow) so the test never panics on a
        // young host — the same panic the finding condemned. The pin is the
        // REAL picker's winner: pre-fix both slots collapse to the fabricated
        // hour and the last-evaluated slot (`older`, snapshot order reversed
        // against load order) wins the jitter tie; post-fix the actual
        // `loaded_at` ordering makes `newer` win, on any uptime.
        let api = unit_state();
        let now = Instant::now();
        // 20-minute separation is far above QueryPerformanceCounter jitter.
        let t_older = now + Duration::from_secs(600);
        let t_newer = now + Duration::from_secs(1_200);
        let status = SchedulerStatus {
            slots: vec![
                occupied_slot("newer", 9002, t_newer),
                occupied_slot("older", 9001, t_older),
            ],
            jobs: Vec::new(),
            catalog_errors: Vec::new(),
        };
        let picked = pick_loaded_occupant(&api, &status, CategoryId::Aux)
            .expect("an occupied category must resolve");
        assert_eq!(
            picked.0, "newer",
            "untouched occupants sort by ACTUAL load time"
        );
        assert_eq!(picked.1, 9002);
        assert_eq!(picked.2.as_deref(), Some("mock-model"));
    }

    #[test]
    fn mru_touch_wins_over_a_fresher_load() {
        let api = unit_state();
        // Load stamps sit NEAR now (not in the future: an in-process future stamp
        // would outrank a touch landing at ~now, inverting reality — real loads
        // always precede their touches). `hot` loaded last, yet the touch on
        // `cold` must outrank it. Backward offsets stay under a minute so the
        // fixtures are safe on any host uptime; the touch lands after them.
        let status = SchedulerStatus {
            slots: vec![
                occupied_slot("cold", 9001, Instant::now()),
                occupied_slot(
                    "hot",
                    9002,
                    Instant::now()
                        .checked_sub(Duration::from_millis(150))
                        .expect("invariant: process start precedes first fixture stamp"),
                ),
            ],
            jobs: Vec::new(),
            catalog_errors: Vec::new(),
        };
        api.touch("cold");
        let picked = pick_loaded_occupant(&api, &status, CategoryId::Aux)
            .expect("an occupied category must resolve");
        assert_eq!(picked.0, "cold", "a proxied model outranks a fresher load");
    }

    #[test]
    fn other_category_and_dead_ports_resolve_to_nothing() {
        let api = unit_state();
        let now = Instant::now();
        let foreign = SlotStatus {
            category: CategoryId::Work,
            ..occupied_slot("elsewhere", 9003, now)
        };
        let portless = SlotStatus {
            port: None,
            ..occupied_slot("unbound", 9004, now)
        };
        let zero = SlotStatus {
            port: Some(0),
            ..occupied_slot("zombie", 0, now)
        };
        let empty = SlotStatus {
            category: CategoryId::Aux,
            index: 1,
            model: None,
            port: None,
            pid: None,
            need_gib: None,
            idle_s: None,
            serve_id: None,
            loading_job: None,
            loaded_at: now,
        };
        let status = SchedulerStatus {
            slots: vec![foreign, portless, zero, empty],
            jobs: Vec::new(),
            catalog_errors: Vec::new(),
        };
        assert!(
            pick_loaded_occupant(&api, &status, CategoryId::Aux).is_none(),
            "wrong-category, portless, and port-0 slots must not resolve"
        );
    }

    #[tokio::test]
    async fn connect_failure_is_the_only_restart_class() {
        // FINDINGS-06 [2] evidence for the connect class: the T10.1 mock death is
        // a `handle.abort()` that drops the listener socket, so the port REFUSES
        // and `send()` yields `is_connect()` — exactly what this classifier maps
        // to the single sanctioned reap + reload + retry.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
        let port = listener.local_addr().expect("local addr").port();
        drop(listener);
        let error = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&json!({"messages": []}))
            .send()
            .await
            .expect_err("nothing accepts on the released port");
        assert!(
            error.is_connect(),
            "dead port must be connect-class: {error}"
        );
        let action = classify_send_failure(&error, port).expect("connect-class retries once");
        assert_eq!(action, RetryAction::Restart);
    }

    #[tokio::test]
    async fn timeout_of_a_live_engine_never_restarts() {
        // A server that accepts but never answers produces a real reqwest
        // timeout — the class that used to mean "engine died" and reaped a
        // healthy engine mid-generation. It must now be a typed 502 instead.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind free port");
        let port = listener.local_addr().expect("local addr").port();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(100))
            .build()
            .expect("plain-HTTP client with a timeout");
        let error = client
            .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
            .json(&json!({"messages": []}))
            .send()
            .await
            .expect_err("the socket is accepted but never answered");
        assert!(
            error.is_timeout(),
            "hanging server must be timeout-class: {error}"
        );
        let err = classify_send_failure(&error, port).expect_err("timeouts never restart");
        assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
        let reason = match &err {
            ApiError::Upstream { reason } => reason.clone(),
            other => panic!("expected Upstream, got {other:?}"),
        };
        assert!(
            reason.contains(&format!("engine on :{port} timed out"))
                && reason.contains("not restarting a live engine"),
            "reason must name the class and the refusal to reap: {reason}"
        );
        drop(listener);
    }

    #[tokio::test]
    async fn non_connect_errors_are_upstream_never_death() {
        // A redirect loop is the finding's named non-death trigger ("a redirect
        // loop or a request-body error"): the engine was alive and answering, so
        // the class must never be read as death and must never restart. Raw
        // sockets speak just enough HTTP/1.1 to bounce the client forever.
        let looping = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect loop server binds");
        let port = looping.local_addr().expect("loop addr").port();
        let server = tokio::spawn(async move {
            let reply = format!(
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/loop\r\n\
                 Content-Length: 0\r\nConnection: keep-alive\r\n\r\n"
            );
            while let Ok((mut stream, _)) = looping.accept().await {
                let reply = reply.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 4096];
                    while let Ok(n) = stream.read(&mut buf).await {
                        if n == 0 {
                            break;
                        }
                        if stream.write_all(reply.as_bytes()).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        let error = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::limited(4))
            .build()
            .expect("plain-HTTP client")
            .post(format!("http://127.0.0.1:{port}/loop"))
            .json(&json!({"messages": []}))
            .send()
            .await
            .expect_err("the redirect loop must exhaust the policy");
        assert!(
            !error.is_connect(),
            "a live answering server is not death: {error}"
        );
        let err = classify_send_failure(&error, port).expect_err("non-connect never restarts");
        match err {
            ApiError::Upstream { reason } => assert!(
                reason.starts_with("forward failed:"),
                "reason must name the class: {reason}"
            ),
            other => panic!("expected Upstream, got {other:?}"),
        }
        server.abort();
    }

    #[test]
    fn concurrent_default_writes_never_lose_a_field_or_leave_a_temp() {
        // FINDINGS-07 [3]: the config write-back was an UNGATED read-modify-write onto
        // one FIXED temp name, and two handlers reach it (`/har/config/default` and
        // `/har/config/model`'s dangling-default clear) on separate workers. Interleaved,
        // the second writer renamed its copy of the ORIGINAL, so the first writer's
        // `default_model` vanished from disk while both responses said `persisted: true`;
        // the shared temp added a second failure mode — one writer truncating the other's
        // tmp made its rename fail although the other's content had landed. Six threads
        // over the four categories, a fresh value per round: under the gate plus the
        // unique temp every write answers true and no category can fall back to its seed.
        const ROUNDS: usize = 24;
        let dir = crate::testutil::temp_dir("api-persist-race");
        let path = dir.join("simplehar-se.json");
        let mut config = AppConfig::default();
        for category in &mut config.categories {
            category.default_model = Some(format!("seed-{}", category.id));
        }
        std::fs::write(
            &path,
            serde_json::to_string_pretty(&config).expect("seed serializes"),
        )
        .expect("seed config");
        config.source_path = Some(path.clone());

        let plan: [(CategoryId, &str); 6] = [
            (CategoryId::Work, "work"),
            (CategoryId::Work, "work"),
            (CategoryId::Rp, "rp"),
            (CategoryId::Rp, "rp"),
            (CategoryId::Aux, "aux"),
            (CategoryId::Emb, "emb"),
        ];
        let writers = plan
            .into_iter()
            .enumerate()
            .map(|(index, (category, prefix))| {
                let per_writer = config.clone();
                std::thread::spawn(move || {
                    (0..ROUNDS)
                        .map(|round| {
                            let name = format!("{prefix}-t{index}-r{round}");
                            persist_default_model(&per_writer, category, Some(&name))
                        })
                        .collect::<Vec<bool>>()
                })
            })
            .collect::<Vec<_>>();
        let outcomes = writers
            .into_iter()
            .map(|writer| writer.join().expect("a writer thread never panics"))
            .collect::<Vec<_>>();
        assert!(
            outcomes.iter().flatten().all(|persisted| *persisted),
            "every writer must be able to answer honestly, not `persisted: true` \
             for bytes that a concurrent rename undid: {outcomes:?}"
        );

        let text = std::fs::read_to_string(&path).expect("the config file is always present");
        let on_disk: Value =
            serde_json::from_str(&text).expect("temp + rename never expose a partial file");
        let categories = on_disk["categories"].as_array().expect("categories array");
        let persisted_default = |category: CategoryId| -> String {
            let id = category.to_string();
            categories
                .iter()
                .find(|entry| entry["id"].as_str() == Some(id.as_str()))
                .and_then(|entry| entry["default_model"].as_str())
                .unwrap_or_else(|| panic!("category {id} lost its default_model: {on_disk}"))
                .to_string()
        };
        for &(category, prefix) in &plan {
            let value = persisted_default(category);
            assert!(
                value.starts_with(prefix),
                "{}'s default is a lost update — no writer ever named {value:?}: {on_disk}",
                category
            );
        }
        // A single-writer category is deterministic under the gate: every later commit
        // re-reads the committed file, so its last round can never be rolled back.
        assert_eq!(
            persisted_default(CategoryId::Aux),
            format!("aux-t4-r{}", ROUNDS - 1),
            "the last AUX write must still be on disk"
        );
        assert_eq!(
            persisted_default(CategoryId::Emb),
            format!("emb-t5-r{}", ROUNDS - 1),
            "the last EMB write must still be on disk"
        );

        let leftovers = std::fs::read_dir(&dir)
            .expect("read temp dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "a failed rename must not leave its temp behind: {leftovers:?}"
        );
    }
}
