//! HTTP surface (DESIGN Steps 11-12): per-category listeners, the control plane, and
//! the proxy.
//!
//! Locked rules implemented here:
//! - status codes come from types, never from message text (defect E1);
//! - wrong-category and unknown-model requests `404` **before** any ensure (G5);
//! - a no-`model` body on a category listener means "use this category": the loaded
//!   occupant (MRU among AUX x N), else the category's `default_model` loaded as a
//!   normal job, else `400` (operator rule 2026-10-24, resolves F-2);
//! - `/har/unload` bodies are strict: unparseable is `400`, both scopes at once is
//!   `400`, never an implicit unload-all (v3.3 CRITICAL fix); the reply reports what
//!   the actor actually did, never a blanket `unloading` (FINDINGS-07 \[5]/\[6]);
//! - proxy responses stream through byte-for-byte, SSE included;
//! - `/har/*` lives on the one control listener; the control router has no request
//!   timeout because `/har/load` deliberately waits through the load (Step 12
//!   idempotency: a double-click attaches to the same job).

use crate::config::{AppConfig, CategoryId, ConfigError};
use crate::model::writeback::WritebackError;
use crate::model::ModelEntry;
use crate::scheduler::{JobId, Reply, SchedulerError, SchedulerHandle};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;
use tokio_stream::StreamExt;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};

mod guards;

use guards::resolve_control_token;
pub use guards::{guard, PUBLIC_PATHS, ROUTINE_POLL_PATHS};

#[cfg(test)]
use guards::{
    loopback_host, presented_token, split_host, token_matches, REQUEST_ID_MAX_CHARS, X_REQUEST_ID,
};

mod status_render;

use status_render::{har_models, har_status, merged_state, v1_models};

#[cfg(test)]
use status_render::{
    container_reported_gb, loaded_rows, model_json, render_status_document, status_document,
    LoadedRow,
};

mod config_persist;

use config_persist::{har_config_default, har_config_logging, har_config_model};

#[cfg(test)]
use config_persist::{
    arm_config_read_back_fault, config_write_path, persist_default_model,
    release_config_read_back_fault, PersistOutcome,
};

mod proxy;

pub use proxy::{category_router, classify_send_failure, RetryAction};

use proxy::ListenerState;

#[cfg(test)]
use proxy::{classify_send_failure_flags, pick_loaded_occupant, wakes_slot, SendFailureClass};

#[cfg(test)]
use crate::scheduler::SchedulerStatus;

#[cfg(test)]
use axum::http::Method;

/// The largest request body the proxy accepts. A chat/embeddings request is text
/// plus, at most, a few base64 images; 64 MiB is generous for that and keeps the
/// buffered `Json<Value>` path from turning one POST into several GiB
/// (FINDINGS-06 [7]).
const PROXY_BODY_LIMIT: usize = 64 * 1024 * 1024;

/// The largest body any `/har/*` control route accepts. Every control body is a
/// small JSON object, so the proxy's ceiling must not be reachable from the control
/// surface (FINDINGS-06 [7]).
const CONTROL_BODY_LIMIT: usize = 64 * 1024;

/// One value on the SSE bus. `Closed` is the terminal sentinel the pump publishes
/// when it stops for any reason (scheduler gone, panic): every attached stream ends
/// so the page falls back to polling instead of staying silently frozen behind
/// keepalives (review 2026-11-02, MED).
#[derive(Clone)]
enum StateEvent {
    /// No document has been published yet.
    Empty,
    /// A complete merged state document.
    Doc(Arc<Value>),
    /// The pump is gone; end every stream.
    Closed,
}

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
    /// A memory refusal may additionally carry the decision's wait hint
    /// ([`crate::budget::WaitHint`]), rendered as flat envelope fields.
    #[error("service unavailable: {reason}")]
    Unavailable {
        reason: String,
        wait_hint: Option<crate::budget::WaitHint>,
    },
    /// The model's `launch_only_with_at_least_vram_free` floor was not met — the
    /// load is refused before spawn (greedy-offload thrash guard). Distinct code
    /// so clients can tell "card busy with foreigners" from plain over-commit.
    /// Carries the same optional wait hint as [`Self::Unavailable`].
    #[error("service unavailable: {reason}")]
    BelowLaunchFloor {
        reason: String,
        wait_hint: Option<crate::budget::WaitHint>,
    },
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
    /// The path exists but the HTTP method does not — `405`. The router raises
    /// this class for every wrong-method request; the envelope middleware carries
    /// it in the same JSON shape as every other class (R2-05-03).
    #[error("method not allowed: {reason}")]
    MethodNotAllowed { reason: String },
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
            ApiError::MethodNotAllowed { .. } => "method_not_allowed",
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
            ApiError::MethodNotAllowed { .. } => StatusCode::METHOD_NOT_ALLOWED,
            ApiError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }

    /// The wait hint a client may branch on, present only on the three memory
    /// refusals (`below_launch_floor`, insufficient, whole machine busy) that
    /// a refusal decision produced one for. `Some` here
    /// says the harness can name a queue place or quote an estimate — the
    /// per-field `None`s still stand (no place, or no release sample).
    pub fn wait_hint(&self) -> Option<crate::budget::WaitHint> {
        match self {
            ApiError::Unavailable { wait_hint, .. }
            | ApiError::BelowLaunchFloor { wait_hint, .. } => *wait_hint,
            _ => None,
        }
    }

    /// Classifies a model-file write-back failure (FINDINGS-13 [10], released from
    /// DEFERRED by the change-side quote ruling — the ruling added a failure mode
    /// that is a caller error, and it was surfacing as `500 internal`).
    ///
    /// The module's own sentence is never re-worded. For the two **config-state**
    /// classes the caller still owes what the module deliberately does not carry —
    /// *which* file refused — so `file` is prepended here, as a **prefix**: the
    /// reason becomes `<file>: <module sentence>` with the sentence intact to the
    /// last byte (that is what the endpoint's `contains`/`ends_with` legs prove
    /// unedited). Every other class answers `reason == error.to_string()` exactly,
    /// byte-identical to what the endpoint answered before typing.
    ///
    /// | writeback class | api | reason | reading |
    /// |---|---|---|---|
    /// | `UnrepresentableValue`, `KeyNotInFile` | `400 bad_request` | the module's sentence, verbatim | the request itself is not writable — fix the request |
    /// | `WouldBreakFile`, `EmptyFile` | `409 conflict` | `<file>: ` + the module's sentence | well-formed request, refused by *this* file's current state (Fork C blanket: the edited file is one the scanner would never publish) |
    /// | `Unreadable`, `WriteFailed`, `RenameFailed`, `PostWriteValidation` | `500 internal` | the module's sentence, verbatim | the harness or the volume failed; `PostWriteValidation`'s message still tells the truth about where the original bytes are |
    ///
    /// Pinned class-by-class (including the prefix/non-prefix split) by
    /// `writeback_errors_map_to_their_http_class_without_rewording` (all eight, in
    /// crate) and, for the classes the endpoint can genuinely reach, by the
    /// `/har/config/model` tests in `tests/api_surface.rs`.
    ///
    /// `reason` is what the *variant* carries and what the ring receives; the
    /// `500 internal` sentence reaches the client body only through `error_body`,
    /// which withholds the OS text of the two harness-failure classes (FINDINGS-05
    /// [6]) — the verbatim `to_string()` comparisons above and beside this table
    /// are statements about `reason`, unchanged by that withholding.
    pub(crate) fn from_writeback(file: &std::path::Path, error: WritebackError) -> Self {
        match error {
            WritebackError::WouldBreakFile { .. } | WritebackError::EmptyFile => {
                ApiError::Conflict {
                    reason: format!("{}: {error}", file.display()),
                }
            }
            WritebackError::UnrepresentableValue { .. } | WritebackError::KeyNotInFile { .. } => {
                ApiError::BadRequest {
                    reason: error.to_string(),
                }
            }
            WritebackError::Unreadable { .. }
            | WritebackError::WriteFailed { .. }
            | WritebackError::RenameFailed { .. }
            | WritebackError::PostWriteValidation { .. } => ApiError::Internal {
                reason: error.to_string(),
            },
        }
    }
}

/// The dual envelope every JSON error body uses (A2), built from a decided
/// `code` and `message`. `error_body` and the router refusals share it, so one
/// shape lives in exactly one place.
fn router_error_body(code: &'static str, message: String) -> Value {
    json!({
        "code": code,
        "message": message,
        "error": { "message": message, "type": "simplehar_error", "code": code },
    })
}

/// The single JSON error body (A2): SE-native flat `code`/`message` plus the
/// OpenAI-compatible `error` envelope with the same values.
///
/// The two harness-failure classes carry OS-level failure text — absolute paths
/// into model yamls, log files, the engine executable (FINDINGS-05 [6]). That
/// text is the operator's diagnostic, not the client's: it goes to
/// `tracing::error!` (ring + log file) and the client receives a stable
/// sentence that keeps the class prefix and points at the harness log. The
/// machine-readable class was always `code`; the numbers a client branches on
/// live in the *request-side* classes (`unavailable`, `below_launch_floor`),
/// whose messages pass through here unchanged, exactly like every other variant.
fn error_body(error: &ApiError) -> Value {
    let message = match error {
        ApiError::Internal { reason } => {
            tracing::error!(detail = %reason, code = %error.code(), "harness failure; OS detail stays in the harness log");
            "internal error: the harness could not complete this operation; the full reason is in the harness log"
                .to_string()
        }
        ApiError::Upstream { reason } => {
            tracing::error!(detail = %reason, code = %error.code(), "engine failure; OS detail stays in the harness log");
            "upstream failure: the engine could not complete this operation; the full reason is in the harness log"
                .to_string()
        }
        other => other.to_string(),
    };
    let code = error.code();
    let mut body = router_error_body(code, message);
    // A refusal that knows about waiting adds its two structured fields at the
    // flat envelope top level, beside `code`/`message`. `queue_position` is an
    // integer or an explicit null (null: the refusal got no queue place).
    // `expected_release_s` is seconds, one decimal, and absent when the
    // harness has no measured release to base it on — never a fabricated
    // default. The OpenAI `error` sub-object keeps exactly today's shape.
    if let Some(hint) = error.wait_hint() {
        body["queue_position"] = json!(hint.queue_position);
        if let Some(seconds) = hint.expected_release_s {
            body["expected_release_s"] = json!((seconds * 10.0).round() / 10.0);
        }
    }
    body
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
            SchedulerError::Admission(admission) => {
                // The message keeps carrying every number (the pins below and in
                // `tests/api_surface.rs` prove it); the structured wait hint rides
                // beside it on the memory refusals that compute one.
                let reason = admission.to_string();
                match admission {
                    crate::budget::AdmissionError::BelowLaunchFloor { wait_hint, .. } => {
                        ApiError::BelowLaunchFloor {
                            reason,
                            wait_hint: Some(wait_hint),
                        }
                    }
                    crate::budget::AdmissionError::Insufficient { wait_hint, .. } => {
                        ApiError::Unavailable {
                            reason,
                            wait_hint: Some(wait_hint),
                        }
                    }
                    // The RAM refusal (operator ruling 2026-11-05) is the same
                    // wait posture as plain over-commit on its own resource:
                    // the client retries, the hint rides, and every number
                    // stays in the message.
                    crate::budget::AdmissionError::InsufficientRam { wait_hint, .. } => {
                        ApiError::Unavailable {
                            reason,
                            wait_hint: Some(wait_hint),
                        }
                    }
                    // The whole-machine refusal (ruling 2026-11-04) is the same wait
                    // posture as plain over-commit: the client retries, and the B2
                    // hint rides it. `below_launch_floor` would quote a `min_free`
                    // figure the `whole` value never had.
                    crate::budget::AdmissionError::WholeMachineBusy { wait_hint, .. } => {
                        ApiError::Unavailable {
                            reason,
                            wait_hint: Some(wait_hint),
                        }
                    }
                    crate::budget::AdmissionError::WitnessUnavailable { .. } => {
                        ApiError::WitnessUnavailable { reason }
                    }
                    _ => ApiError::Unavailable {
                        reason,
                        wait_hint: None,
                    },
                }
            }
            SchedulerError::Engine(reason) => ApiError::Unavailable {
                reason: reason.to_string(),
                wait_hint: None,
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
                wait_hint: None,
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

/// The guard's evidence classes (FINDINGS2-0507 [3]): the four refusals that used to
/// leave no ring entry at all, each deduplicated per path since the last successful
/// command re-armed it.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum GuardRefusal {
    /// DNS-rebinding-shaped `Host`.
    Host,
    /// Cross-site `Origin`.
    Origin,
    /// POST without `application/json`.
    ContentType,
    /// Missing or wrong control token.
    Token,
}

impl GuardRefusal {
    /// The class a refusal status belongs to; `None` for anything the guard does not
    /// raise (a handler's own 400/409 is not guard evidence).
    fn from_status(status: StatusCode) -> Option<Self> {
        match status {
            StatusCode::BAD_REQUEST => Some(Self::Host),
            StatusCode::FORBIDDEN => Some(Self::Origin),
            StatusCode::UNSUPPORTED_MEDIA_TYPE => Some(Self::ContentType),
            StatusCode::UNAUTHORIZED => Some(Self::Token),
            _ => None,
        }
    }

    /// The wire label, so a reader of `/har/log` sees which wall answered.
    fn label(self) -> &'static str {
        match self {
            Self::Host => "host",
            Self::Origin => "origin",
            Self::ContentType => "content-type",
            Self::Token => "token",
        }
    }
}

/// Per-`(class, path)` guard-refusal evidence, one per [`ApiState`] (so per process,
/// and fresh at boot: the first refusal after a restart is always reported).
///
/// The two maps are bounded by construction (FINDINGS-BATCH-2026-10-28M [2]): at most
/// [`GUARD_TRACKED_PATHS_PER_CLASS`] keys per class in `reported` and at most one entry
/// per class in `overflow`, so neither a repeat flood nor a distinct-path flood can grow
/// this state or the ring past a constant per re-arm window.
#[derive(Default)]
struct GuardTrace {
    /// `(class, path)` already reported, with how many repeats were swallowed since.
    /// Absent means armed: the next refusal of that pair reports.
    reported: BTreeMap<(GuardRefusal, String), u64>,
    /// Per class: distinct paths refused past the tracking cap, and whether that class
    /// has already spent its one fold line this window. One entry per class, ever.
    overflow: BTreeMap<GuardRefusal, GuardOverflow>,
}

/// The beyond-the-cap tally for one class (FINDINGS-BATCH-2026-10-28M [2]).
#[derive(Default, Clone, Copy)]
struct GuardOverflow {
    /// Distinct refused paths that were collapsed instead of tracked.
    paths: u64,
    /// The fold line is spent once per window, not once per collapsed path.
    folded: bool,
}

/// How many distinct paths one class may name per re-arm window.
///
/// The bound has to be on cardinality, not just repeats: `reject_reason` runs ahead of
/// `auth_reason` and consults neither the token nor route membership, so the three
/// structural classes are reachable **pre-auth on arbitrary paths** — dedup by
/// `(class, path)` alone turned 1000 bogus paths into 1000 ring lines and 1000 map
/// entries (FINDINGS-BATCH-2026-10-28M [2], Attack 1). Past this cap a class emits one
/// fold line and counts the rest.
const GUARD_TRACKED_PATHS_PER_CLASS: usize = 8;

/// How much of an attacker-chosen path the evidence keeps, in chars. The path is
/// attacker-chosen, so the stored key and the logged field are capped together: a flood
/// of kilobyte-long paths must not become an allocation channel, and a key must agree
/// with the line printed for it. The logged refusal `method` and `reason` share this cap (R2-05-02):
/// its text is built from the attacker's own path, `Host` or `Origin`, and one line may
/// not become attacker-sized (hyper's head bound alone is ~408 KiB per request).
const GUARD_TRACKED_PATH_CHARS: usize = 96;

/// The path as the guard evidence stores and prints it.
fn tracked_path(path: &str) -> String {
    if path.chars().count() <= GUARD_TRACKED_PATH_CHARS {
        return path.to_string();
    }
    let kept: String = path.chars().take(GUARD_TRACKED_PATH_CHARS - 3).collect();
    format!("{kept}...")
}

/// The `reason` as the guard evidence prints it (R2-05-02): capped exactly like
/// [`tracked_path`], because the message embeds the attacker-chosen path, `Host` or
/// `Origin` — the 500-line ring and the log file must not become an attacker-sized
/// allocation channel. The client wire body keeps the full text on purpose: it is the
/// caller's own data, never the operator's post-mortem channel. Extended Logging
/// prints the raw path by its declared purpose and stays uncapped.
fn tracked_reason(reason: &ApiError) -> String {
    let text = reason.to_string();
    if text.chars().count() <= GUARD_TRACKED_PATH_CHARS {
        return text;
    }
    let kept: String = text.chars().take(GUARD_TRACKED_PATH_CHARS - 3).collect();
    format!("{kept}...")
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
    /// Per-field verdict on the boot config file as of the last landing harness
    /// write-back (07[11]: the `/har/status` `config_errors` producer). A boot that
    /// validates dirty never serves a status (§0.2 step 1 fails the boot), so the
    /// starting state is honestly empty; the only way to non-empty is a write-back
    /// that published bytes the next boot would refuse. Every boot-file mutation
    /// path must feed its landed bytes through [`ApiState::record_config_errors`] —
    /// pinned by
    /// `a_write_back_that_lands_a_boot_invalid_file_names_it_in_config_errors`.
    config_errors: Arc<Mutex<Vec<ConfigError>>>,
    /// Last-use hints per model (fallback MRU for AUX x N; eviction LRU later).
    usage: Arc<Mutex<HashMap<String, Instant>>>,
    /// Shared secret for API/proxy routes when configured (`control_token` or
    /// `SIMPLEHARN_TOKEN`). Private: only the guard reads it, and it is never logged.
    control_token: Option<String>,
    /// Guard-refusal evidence state, shared by every clone of this state (one per
    /// process).
    ///
    /// The TRUE bound, stated exactly (FINDINGS-BATCH-2026-10-28M [2] killed the earlier
    /// "a refusal flood costs the ring a bounded number of lines", which was false — the
    /// structural classes fire pre-auth on arbitrary paths, so path cardinality has to be
    /// bounded too): with Extended Logging **off** (the shipped default) one re-arm
    /// window costs the ring at most
    /// `4 classes × (GUARD_TRACKED_PATHS_PER_CLASS + 1 fold) + 1 recovery line`
    /// = 37 lines, whatever the number of refused requests, and holds at most
    /// `4 × GUARD_TRACKED_PATHS_PER_CLASS` keys plus 4 fold counters in memory.
    /// Extended Logging is the operator's explicit record-everything switch and lifts
    /// the line bound (and the state writes with it), as it does for `request complete`.
    /// Pinned by `a_distinct_path_flood_costs_a_constant_number_of_lines`.
    guard_trace: Arc<Mutex<GuardTrace>>,
    proxy: reqwest::Client,
    /// The latest SSE bus value, published once per scheduler push by
    /// [`ApiState::ensure_state_pump`]. A `watch` channel is the coalescing bound:
    /// a slow SSE client sees the latest value, never a backlog (FINDINGS-OPEN N1).
    state_events: Arc<tokio::sync::watch::Sender<StateEvent>>,
    /// One-shot guard for the state pump: the first SSE connection starts it, and
    /// every later connection reuses it.
    state_pump_started: Arc<std::sync::atomic::AtomicBool>,
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
        let (state_events, _) = tokio::sync::watch::channel(StateEvent::Empty);
        ApiState {
            control_addr,
            scheduler,
            listeners,
            witness,
            boot: Arc::new(std::sync::RwLock::new(boot)),
            default_overrides: Arc::new(Mutex::new(BTreeMap::new())),
            config_errors: Arc::new(Mutex::new(Vec::new())),
            usage: Arc::new(Mutex::new(HashMap::new())),
            control_token,
            guard_trace: Arc::new(Mutex::new(GuardTrace::default())),
            proxy: reqwest::Client::builder()
                // A total `timeout` covers the response body, so it cut a long SSE
                // generation mid-stream after the 200 (FINDINGS-06 [6]). `read_timeout`
                // is per-read: a live engine that keeps producing bytes is never
                // reaped, while a stalled socket still fails in bounded time.
                .connect_timeout(Duration::from_secs(10))
                .read_timeout(Duration::from_secs(600))
                .build()
                .expect("invariant: plain-HTTP client with timeouts needs no configuration"),
            state_events: Arc::new(state_events),
            state_pump_started: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            config,
        }
    }

    /// Whether API/proxy routes demand the control token (boot messaging; the secret
    /// itself stays private and is never logged).
    #[must_use]
    pub fn requires_token(&self) -> bool {
        self.control_token.is_some()
    }

    /// Starts the one state pump that builds the merged document once per scheduler
    /// push and publishes it to every SSE client (FINDINGS-OPEN N1). Idempotent: the
    /// first caller wins, and a caller with no Tokio runtime (a sync unit test) leaves
    /// the flag unset so a later async caller can start it.
    fn ensure_state_pump(self: &Arc<Self>) {
        if self
            .state_pump_started
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            self.state_pump_started.store(false, Ordering::Release);
            return;
        };
        let state = Arc::clone(self);
        handle.spawn(async move {
            // Supervisor: a panic inside the pump must not leave every stream parked
            // behind keepalives with no `har_state` ever landing again. Await the
            // pump, log any exit, then publish the terminal sentinel so every client
            // ends its stream and the page falls back to polling.
            let pump = tokio::spawn(pump_state(Arc::clone(&state)));
            match pump.await {
                Ok(()) => tracing::warn!("state pump ended; SSE clients fall back to polling"),
                Err(error) => {
                    tracing::error!(%error, "state pump died; SSE clients fall back to polling");
                }
            }
            let _ = state.state_events.send_replace(StateEvent::Closed);
        });
    }

    /// Reports a guard refusal to the ring (FINDINGS2-0507 [3]) — the refusals that
    /// used to answer and vanish, while benign 200s filled the same ring.
    ///
    /// Threat model, stated plainly, because the ring is the operator's only
    /// post-mortem and it holds 500 lines: these four refusals are exactly what an
    /// attacker produces **in a flood** (token guessing → 401, cross-site POST probe →
    /// 403, DNS rebinding → 400, content-type probe → 415), and three of them are
    /// reachable pre-auth on any path. So the flood is bounded on both axes:
    ///
    /// * **repeats** of one `(class, path)` cost no further line — the counter rides the
    ///   recovery line;
    /// * **distinct paths** past [`GUARD_TRACKED_PATHS_PER_CLASS`] cost no further line
    ///   either, only the class's single fold line (FINDINGS-BATCH-2026-10-28M [2]).
    ///
    /// The window re-arms on the next successful *command* — a 2xx that is not one of
    /// [`ROUTINE_POLL_PATHS`] and not a [`PUBLIC_PATHS`] page or asset (R2-05-01),
    /// because the page's 2 s polls would otherwise re-arm a flood into a line every
    /// tick and a free public-page 200 would otherwise re-arm it at request rate — and
    /// Extended Logging (the switch whose doc promises "the full request ring") reports
    /// every refusal outright and keeps no state at all.
    /// Never logged: the token, in any form (15[9] posture); the non-extended
    /// `reason` field is capped to [`GUARD_TRACKED_PATH_CHARS`] (R2-05-02).
    fn note_guard_refusal(
        &self,
        reason: &ApiError,
        method: &axum::http::Method,
        path: &str,
        request_id: &str,
    ) {
        let Some(class) = GuardRefusal::from_status(reason.status()) else {
            return;
        };
        let status = reason.status();
        if crate::observability::extended_logging() {
            // The record-everything mode is defined by its name: one line per refusal,
            // and no evidence map to grow.
            tracing::warn!(
                %request_id,
                method = %method,
                path = %path,
                status = status.as_u16(),
                refusal = class.label(),
                reason = %reason,
                "guard refused"
            );
            return;
        }
        let tracked = tracked_path(path);
        let key = (class, tracked.clone());

        enum Action {
            Report,
            Fold(u64),
            Silent,
        }
        let action = {
            let mut trace = self
                .guard_trace
                .lock()
                .expect("guard trace lock poisoned by a panic");
            if let Some(count) = trace.reported.get_mut(&key) {
                *count = count.saturating_add(1);
                Action::Silent
            } else if trace
                .reported
                .keys()
                .filter(|(tracked, _)| *tracked == class)
                .count()
                < GUARD_TRACKED_PATHS_PER_CLASS
            {
                trace.reported.insert(key, 0);
                Action::Report
            } else {
                let entry = trace.overflow.entry(class).or_default();
                entry.paths = entry.paths.saturating_add(1);
                if entry.folded {
                    Action::Silent
                } else {
                    entry.folded = true;
                    Action::Fold(entry.paths)
                }
            }
        };
        match action {
            Action::Report => tracing::warn!(
                %request_id,
                method = %tracked_path(method.as_str()),
                path = %tracked,
                status = status.as_u16(),
                refusal = class.label(),
                reason = %tracked_reason(reason),
                "guard refused"
            ),
            Action::Fold(paths) => tracing::warn!(
                %request_id,
                method = %tracked_path(method.as_str()),
                status = status.as_u16(),
                refusal = class.label(),
                tracked = GUARD_TRACKED_PATHS_PER_CLASS,
                folded = paths,
                "guard refusal window full: further new-path refusals are counted, not logged"
            ),
            Action::Silent => {}
        }
    }

    /// One line naming what the guard refused since the last successful command, then
    /// re-arms every class. Cheap by construction: it fires only when something was
    /// actually refused, so a quiet harness never sees it.
    fn note_guard_recovery(&self, request_id: &str) {
        let mut trace = self
            .guard_trace
            .lock()
            .expect("guard trace lock poisoned by a panic");
        if trace.reported.is_empty() && trace.overflow.is_empty() {
            return;
        }
        let drained = std::mem::take(&mut trace.reported);
        let folded = std::mem::take(&mut trace.overflow);
        drop(trace);
        let mut parts: Vec<String> = drained
            .iter()
            .map(|((class, path), suppressed)| {
                if *suppressed == 0 {
                    format!("{} on {path}", class.label())
                } else {
                    format!("{} on {path} (+{suppressed} suppressed)", class.label())
                }
            })
            .collect();
        // The collapsed paths are reported here rather than one line each, which is the
        // whole point of the cap (FINDINGS-BATCH-2026-10-28M [2]).
        for (class, entry) in &folded {
            if entry.paths > 0 {
                parts.push(format!(
                    "{}: +{} further paths folded",
                    class.label(),
                    entry.paths
                ));
            }
        }
        let summary = parts.join(", ");
        tracing::info!(
            %request_id,
            refusals = %summary,
            "guard refusals cleared by a successful command"
        );
    }

    /// The refusals this state has reported since the last successful command, as
    /// `(class, path, suppressed)`. Test-only: this bookkeeping is what the ring line is
    /// rendered from, and reading it directly keeps those assertions independent of
    /// which subscriber happens to be installed on which thread.
    #[cfg(test)]
    pub(crate) fn guard_report(&self) -> Vec<(String, String, u64)> {
        self.guard_trace
            .lock()
            .expect("guard trace lock poisoned by a panic")
            .reported
            .iter()
            .map(|((class, path), suppressed)| {
                (class.label().to_string(), path.clone(), *suppressed)
            })
            .collect()
    }

    /// The per-class collapsed-path tallies since the last re-arm: `(class, paths)`.
    /// Test-only companion of [`ApiState::guard_report`] for the cardinality bound.
    #[cfg(test)]
    pub(crate) fn guard_folds(&self) -> Vec<(String, u64)> {
        self.guard_trace
            .lock()
            .expect("guard trace lock poisoned by a panic")
            .overflow
            .iter()
            .map(|(class, entry)| (class.label().to_string(), entry.paths))
            .collect()
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

    /// Records the per-field validation verdict on the boot config file, replacing
    /// the previous one. Call only after a write-back's landed bytes have been
    /// re-validated — an empty vector is the claim "the file on disk now boots clean"
    /// and must be earned the same way (07\[11]).
    pub fn record_config_errors(&self, errors: Vec<ConfigError>) {
        *self
            .config_errors
            .lock()
            .expect("config errors lock poisoned by a writer panic") = errors;
    }

    /// The recorded per-field config problems: the only thing `/har/status`
    /// `config_errors` renders. A cached-state read by design — the status render path
    /// never touches the filesystem (pinned by the structural scan in this file).
    #[must_use]
    pub fn config_errors(&self) -> Vec<ConfigError> {
        self.config_errors
            .lock()
            .expect("config errors lock poisoned by a writer panic")
            .clone()
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

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
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
        Reply::Done | Reply::Unloaded { .. } => json!({ "status": "done" }),
    }))
}

/// `POST /har/unload` — `{"category": "WORK"}` one category, `{"model": name}`
/// one slot (T12.1), `{}` every occupied slot (T12.4, the explicit unload-all),
/// garbage `400` (the v3.3 CRITICAL fix was about *unparseable* bodies, never an
/// implicit unload-all), and both scopes at once `400` rather than a silently
/// wider category kill (FINDINGS-07 [6]). Unloading a loading slot cancels the
/// load (T8). The reply reports what the actor actually did — `unloading`,
/// `cancelled` or `nothing_unloaded` (FINDINGS-07 [5]).
async fn har_unload(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON: {rejection}"),
    })?;
    if body.get("model").is_some() && body.get("category").is_some() {
        return Err(ApiError::BadRequest {
            reason: "send exactly one of `model` or `category`, never both: \
                     `{\"model\": …}` unloads one model, `{\"category\": …}` every \
                     occupant of the category"
                .to_string(),
        });
    }
    if let Some(raw) = body.get("category").and_then(Value::as_str) {
        let category: CategoryId = raw
            .parse()
            .map_err(|reason| ApiError::BadRequest { reason })?;
        let reply = state
            .scheduler
            .unload(category)
            .await
            .map_err(ApiError::from)?;
        return Ok(Json(unload_reply(("category", json!(category)), reply)?));
    }
    if let Some(model) = body.get("model").and_then(Value::as_str) {
        let reply = state
            .scheduler
            .unload_model(model)
            .await
            .map_err(ApiError::from)?;
        return Ok(Json(unload_reply(("model", json!(model)), reply)?));
    }
    if body.as_object().is_some_and(|map| map.is_empty()) {
        let reply = state.scheduler.unload_all().await.map_err(ApiError::from)?;
        return Ok(Json(unload_reply(("scope", json!("all")), reply)?));
    }
    Err(ApiError::BadRequest {
        reason: "unrecognized body: send {} (all), {\"category\": …} or {\"model\": …}".to_string(),
    })
}

/// The honest reply shape for `/har/unload` (FINDINGS-07 [5]): `unloading` names
/// slots whose release the actor committed to (the release itself still lands on
/// the job ledger, GOTCHAS: a 200 is an acceptance, never a release), `cancelled`
/// names a command that only cancelled in-flight or parked loads, and
/// `nothing_unloaded` names a no-op — a never-loaded model or an empty harness
/// may not answer like a scope that moved. An unload command answered with any
/// other variant is an invariant violation, refused as one rather than dressed up
/// as a status.
fn unload_reply(scope: (&str, Value), reply: Reply) -> Result<Value, ApiError> {
    let (unloading, jobs_cancelled) = match reply {
        Reply::Unloaded {
            unloading,
            jobs_cancelled,
        } => (unloading, jobs_cancelled),
        other => {
            return Err(ApiError::Internal {
                reason: format!("the scheduler answered an unload command with {other:?}"),
            })
        }
    };
    let status = if unloading > 0 {
        "unloading"
    } else if jobs_cancelled > 0 {
        "cancelled"
    } else {
        "nothing_unloaded"
    };
    let (key, value) = scope;
    Ok(json!({
        "status": status,
        "unloading": unloading,
        "jobs_cancelled": jobs_cancelled,
        key: value,
    }))
}

/// The pump body: one merged document per scheduler push, published to every SSE
/// subscriber. Skipping the build while nobody is attached keeps an idle page from
/// costing an actor round-trip per tick (review 2026-11-02, LOW).
async fn pump_state(state: Arc<ApiState>) {
    let mut pushes = state.scheduler.subscribe();
    publish_state(&state).await;
    loop {
        match pushes.recv().await {
            Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                if state.state_events.receiver_count() > 0 {
                    publish_state(&state).await;
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

/// Builds the merged document and publishes it; a build failure is logged, never
/// silent, because the page's next paint is the only place the operator sees it.
async fn publish_state(state: &ApiState) {
    match merged_state(state).await {
        Ok(doc) => {
            let _ = state
                .state_events
                .send_replace(StateEvent::Doc(Arc::new(doc)));
        }
        Err(error) => {
            tracing::warn!(error = %error, "state pump could not build the status document");
        }
    }
}

/// `GET /har/events` — one SSE stream: the current state immediately, then a push
/// after every scheduler mutation (keepalives by the SSE layer); a dropped stream
/// degrades to the page's polling fallback (Step 12 / Step 14).
///
/// The document is built ONCE per scheduler push by [`ApiState::ensure_state_pump`]
/// and published through a `watch` channel; `WatchStream` is the coalescing bound, so
/// a slow reader sees the latest value and never queues documents (FINDINGS-OPEN N1).
/// The pump's `Closed` sentinel ends the stream, which is the page's cue to poll.
async fn har_events(
    State(state): State<Arc<ApiState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, std::convert::Infallible>>> {
    state.ensure_state_pump();
    let rx = state.state_events.subscribe();
    let stream = tokio_stream::wrappers::WatchStream::new(rx)
        .take_while(|event| !matches!(event, StateEvent::Closed))
        .filter_map(|event| match event {
            StateEvent::Doc(doc) => Some(Ok(state_event(&doc))),
            StateEvent::Empty | StateEvent::Closed => None,
        });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// One `har_state` event; a serialization failure is impossible for a `Value`, but
/// the fallback keeps the stream alive rather than dropping the client.
fn state_event(doc: &Value) -> Event {
    Event::default()
        .event("har_state")
        .json_data(doc)
        .unwrap_or_else(|_| Event::default().event("har_state").data("{}"))
}

/// The routes whose bodies come from the engine, not from the harness. A `404`
/// here is the engine's own answer. The client must receive it byte-for-byte
/// (proxy pass-through, FINDINGS-06 [9]), so the envelope rewrite below never
/// touches these three paths.
const PROXY_PASSTHROUGH_PATHS: [&str; 3] =
    ["/v1/chat/completions", "/v1/completions", "/v1/embeddings"];

/// Rewrites the router's own refusals to the typed JSON envelope (R2-05-03).
///
/// An unknown route answers `404` from the router fallback, and a wrong method
/// answers `405` from the method router. Both are plain-text answers, and a
/// client could not branch on a missing `code` — the same contract FINDINGS-05
/// [3] fixed for body rejections. Only those two statuses are rewritten, and
/// only while the body is still axum's (no content type, or `text/plain`). A
/// `404` on a proxy pass-through path is the engine's answer, not the harness's,
/// so it is never touched. Every other status, and every response that is
/// already JSON, passes through unchanged.
async fn envelope_router_errors(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let mut response = next.run(request).await;
    let status = response.status();
    let harness_refusal = status == StatusCode::METHOD_NOT_ALLOWED
        || (status == StatusCode::NOT_FOUND && !PROXY_PASSTHROUGH_PATHS.contains(&path.as_str()));
    if !harness_refusal {
        return response;
    }
    let plain_text = match response.headers().get(axum::http::header::CONTENT_TYPE) {
        None => true,
        Some(value) => value
            .to_str()
            .is_ok_and(|value| value.starts_with("text/plain")),
    };
    if !plain_text {
        return response;
    }
    let (code, message) = if status == StatusCode::METHOD_NOT_ALLOWED {
        (
            "method_not_allowed",
            format!("method_not_allowed: {method} {path} is not accepted"),
        )
    } else {
        (
            "not_found",
            format!("not_found: no route for {method} {path}"),
        )
    };
    // The body swap takes its framing headers with it: the old bytes may carry
    // an inner layer's gzip, and `content-length` would then lie about the new
    // JSON. Every other header survives — `x-request-id` from the guard, the
    // `Allow` list of a `405`, and anything a later layer adds.
    let headers = response.headers_mut();
    headers.remove(axum::http::header::CONTENT_TYPE);
    headers.remove(axum::http::header::CONTENT_LENGTH);
    headers.remove(axum::http::header::CONTENT_ENCODING);
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    *response.body_mut() = Body::from(router_error_body(code, message).to_string());
    response
}

/// `GET /har/log?limit=N` — the last N harness log events from the in-memory
/// ring (Step 16): why did the last load fail, without opening files.
///
/// A query the extractor rejects answers `400 bad_request` in the project
/// envelope, never axum's plain-text rejection (R2-05-03). The branch is
/// defensive: the 0.8 form decoder is lossy and the map type accepts every
/// pair, so a query rarely rejects on this surface today.
async fn har_log(
    query: Result<
        axum::extract::Query<std::collections::HashMap<String, String>>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Json<Value>, ApiError> {
    let axum::extract::Query(query) = query.map_err(|rejection| ApiError::BadRequest {
        reason: rejection.to_string(),
    })?;
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
        .route("/har/config/model", post(har_config_model))
        // `/har/*` bodies are small JSON objects; the proxy's ceiling must not be
        // reachable from the control surface (FINDINGS-06 [7]).
        .layer(DefaultBodyLimit::max(CONTROL_BODY_LIMIT));
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
            // proxy ceiling applies to the merged router; the `/har/*` sub-router
            // narrows it to CONTROL_BODY_LIMIT above.
            DefaultBodyLimit::max(PROXY_BODY_LIMIT),
        ))
        // Outermost, over the merged surface: the merged fallback and every
        // sub-router's own 404/405 answer the project envelope here, request id
        // and `Allow` headers included (R2-05-03). The category surface already
        // normalizes inside `category_router`; this is what catches the `/har/*`
        // method refusals and the page fallback.
        .layer(axum::middleware::from_fn(envelope_router_errors))
}

#[cfg(test)]
mod tests;
