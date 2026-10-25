//! HTTP surface: router, shared state, and the typed error mapping (DESIGN Steps 11-12).
//!
//! Status codes come from types, never from message text — v3.3 picked codes by
//! substring-matching human strings, so a model literally named `not-answering-model`
//! changed the status of a request about it (defect E1). The proxy surface, guards,
//! and per-category listener construction land with the routing milestone; the
//! scaffold binds the control listener only (§0.4).

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tower_http::{compression::CompressionLayer, timeout::TimeoutLayer, trace::TraceLayer};

use crate::config::{AppConfig, CategoryConfig};

/// Typed API error: the single place where the taxonomy maps to status codes (E1 fix).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ApiError {
    /// Unknown model on this listener, or wrong category — `404` before any ensure.
    #[error("not found: {what}")]
    NotFound { what: String },
    /// Malformed request (e.g. a no-model body where no default is configured).
    #[error("bad request: {reason}")]
    BadRequest { reason: String },
    /// Admission or release refusal; the message carries the numbers (G13).
    #[error("service unavailable: {reason}")]
    Unavailable { reason: String },
    /// Harness-side failure; logged with the request id (Step 12).
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
            ApiError::Internal { .. } => "internal",
        }
    }

    /// HTTP status for this error kind — typed mapping, no text matching.
    pub fn status(&self) -> StatusCode {
        match self {
            ApiError::NotFound { .. } => StatusCode::NOT_FOUND,
            ApiError::BadRequest { .. } => StatusCode::BAD_REQUEST,
            ApiError::Unavailable { .. } => StatusCode::SERVICE_UNAVAILABLE,
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

/// Immutable snapshot shared by handlers (AGENTS.md: shared state, no globals).
#[derive(Debug, Clone)]
pub struct ApiState {
    /// Validated boot config.
    pub config: AppConfig,
    /// Address the control listener is bound on.
    pub control_addr: String,
}

impl ApiState {
    /// Builds handler state from a validated config.
    ///
    /// # Panics
    ///
    /// Only on the validated exactly-one-control invariant; `main` validates first.
    pub fn from(config: AppConfig) -> Self {
        let control: &CategoryConfig = config.control_category();
        ApiState {
            control_addr: format!("{}:{}", config.internal_host, control.public_port),
            config,
        }
    }
}

/// One category as rendered in `/har/status`.
#[derive(Debug, Serialize)]
struct CategoryDto {
    id: String,
    public_port: u16,
    default_model: Option<String>,
    max_models: usize,
    control: bool,
}

/// The `/har/status` document (scaffold subset; grows with jobs, slots, and mem).
#[derive(Debug, Serialize)]
struct StatusDto {
    version: &'static str,
    control_addr: String,
    categories: Vec<CategoryDto>,
    config_errors: Vec<crate::config::ConfigError>,
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn status(State(state): State<Arc<ApiState>>) -> Result<Json<StatusDto>, ApiError> {
    let categories = state
        .config
        .categories
        .iter()
        .map(|c| CategoryDto {
            id: c.id.to_string(),
            public_port: c.public_port,
            default_model: c.default_model.clone(),
            max_models: c.max_models,
            control: c.control,
        })
        .collect();
    Ok(Json(StatusDto {
        version: env!("CARGO_PKG_VERSION"),
        control_addr: state.control_addr.clone(),
        categories,
        config_errors: Vec::new(),
    }))
}

/// Builds the control-plane router with the AGENTS.md middleware set.
///
/// # Returns
///
/// A stateless-over-`ApiState` router: `/health` and `/har/status` never queue behind
/// engine work (Step 2 — reads are snapshots).
pub fn router(state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/har/status", get(status))
        .with_state(state)
        .layer((
            TraceLayer::new_for_http(),
            CompressionLayer::new(),
            TimeoutLayer::with_status_code(StatusCode::GATEWAY_TIMEOUT, Duration::from_secs(30)),
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
            ApiError::Internal {
                reason: "x".to_string()
            }
            .status(),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
