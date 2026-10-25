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
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::HashMap;
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

/// `GET /har/status`: boot info + slot table + jobs, one call (G13).
async fn har_status(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    Ok(Json(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "control_addr": state.control_addr,
        "categories": state.config.categories,
        "listeners": state.listeners.snapshot(),
        "mem": {
            "total_gb": state.config.vram_total_gb,
            "reserve_gb": state.config.vram_reserve_gb,
        },
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
            let default = api
                .config
                .categories
                .iter()
                .find(|cat| cat.id == listener.category)
                .and_then(|cat| cat.default_model.clone());
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
        .route("/har/jobs/{id}", get(har_job))
        .route("/har/load", post(har_load))
        .route("/har/unload", post(har_unload))
        .route("/har/rescan", post(har_rescan));
    Router::new()
        .merge(category_router(Arc::clone(&api), control_category))
        .merge(har.with_state(api))
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
