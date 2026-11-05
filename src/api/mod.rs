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
use crate::scheduler::{JobId, Reply, SchedulerError, SchedulerHandle, SchedulerStatus};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{HeaderValue, Method, StatusCode};
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

    /// Sets (or clears, with `None`) the category default and persists it to
    /// `simpleharn.json` atomically (temp file + rename). Memory applies only
    /// after the bytes landed: a write that never landed leaves the effective
    /// default at its previous value, so the config file stays king between the
    /// failed call and the next boot (R2-07-03). When `persisted: false` names a
    /// write that never landed, the answer carries the still-effective value.
    ///
    /// A write that lands re-validates the published bytes against the boot schema
    /// (07\[11]): the landed tree is exactly what the next boot will read, so its
    /// per-field verdict is recorded for `/har/status` `config_errors` — the field's
    /// only producer, replacing the last verdict on every write whose bytes reached
    /// the live file. A write that never landed leaves the field alone (the file
    /// still holds the bytes the previous verdict described). A rename that landed
    /// but whose read-back did not confirm the published tree records the verdict of
    /// the bytes that ARE on disk (R2-07-02): the harness just replaced the boot
    /// file, so keeping the old verdict would re-assert a claim about bytes the file
    /// no longer holds.
    ///
    /// The write-back runs on a blocking thread (FINDINGS-07 \[8]): the handler's
    /// async worker must not park on the gated read-patch-copy-rename, while the
    /// `persisted` answer still means what it always meant — the handler waits for
    /// the leg's verdict before replying, so `/har/config/default` keeps its
    /// response honesty (06\[5]). A blocking task that dies (panic/cancel) can only
    /// ever be answered `persisted: false`.
    pub async fn set_default_model(
        &self,
        category: CategoryId,
        model: Option<&str>,
    ) -> (Option<String>, bool) {
        let owned_model = model.map(str::to_string);
        let config = self.config.clone();
        let persist_model = owned_model.clone();
        match tokio::task::spawn_blocking(move || {
            persist_default_model(&config, category, persist_model.as_deref())
        })
        .await
        {
            Ok(outcome) => match outcome {
                PersistOutcome::Landed { verdict } => {
                    self.record_config_errors(verdict);
                    self.default_overrides
                        .lock()
                        .expect("default override lock poisoned by a writer panic")
                        .insert(category, owned_model.clone());
                    (owned_model, true)
                }
                // The rename landed: the live boot file now holds bytes the
                // read-back refused, so the verdict must describe them even
                // though the answer stays `persisted: false` (R2-07-02). The
                // file is king and it holds the NEW bytes, so memory follows
                // them (R2-07-03).
                PersistOutcome::LandedUnverified { verdict } => {
                    self.record_config_errors(verdict);
                    self.default_overrides
                        .lock()
                        .expect("default override lock poisoned by a writer panic")
                        .insert(category, owned_model.clone());
                    (owned_model, false)
                }
                // The bytes never landed: the file still holds the previous
                // default, so memory must not move (R2-07-03). The answer
                // carries the still-effective value, not the refused edit.
                PersistOutcome::NotLanded => (self.default_model(category), false),
            },
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "config persist task died; answering persisted:false"
                );
                (self.default_model(category), false)
            }
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

/// One category listener's view of the shared state.
#[derive(Clone)]
struct ListenerState {
    api: Arc<ApiState>,
    category: CategoryId,
}

async fn health() -> Json<Value> {
    Json(json!({ "status": "ok" }))
}

/// Where a config write-back belongs: the file boot actually loaded (`SIMPLEHARN_CONFIG`
/// included), else the default beside the executable — taken from
/// [`crate::config::default_config_path`] so the file SE reads and the file SE writes can
/// never drift apart.
///
/// The final `PathBuf::from(CONFIG_FILE)` is a last resort for a process that cannot
/// resolve its own executable path: `boot_config` always stamps `source_path`, so this is
/// reached in practice only by a hand-built `AppConfig`. It used to name `simplehar.json`,
/// the older app's file. SE never writes the legacy file unless the operator explicitly
/// selects it as `SIMPLEHARN_CONFIG` (ruling 2026-11-03, `TASK-DEBT-LEFTOVERS`):
/// auto-discovery and migration only read it, and boot warns once about the
/// rollback-strand hazard when the operator selects it (`main.rs::boot_config`). The
/// fallback itself must therefore name [`crate::config::CONFIG_FILE`] — naming the
/// legacy file here would write a file boot never loads (README review, 2026-10-27).
fn config_write_path(config: &AppConfig) -> std::path::PathBuf {
    config
        .source_path
        .clone()
        .or_else(crate::config::default_config_path)
        .unwrap_or_else(|| std::path::PathBuf::from(crate::config::CONFIG_FILE))
}

/// The verdict of one config publish, told in three legs so the caller can keep
/// two separate claims apart: "the bytes landed and read back" (`persisted`) and
/// "this is what the live boot file now says" (`config_errors`). A rename-success/
/// verify-failure separates them — the file changed while the answer must stay
/// `persisted: false` — and conflating the two let `/har/status` keep asserting
/// the previous verdict over a file the harness itself had just replaced
/// (R2-07-02).
#[derive(Debug)]
enum PersistOutcome {
    /// The rename landed and the read-back confirmed the published tree;
    /// `verdict` is the boot-schema validation of the landed bytes (07[11]).
    Landed { verdict: Vec<ConfigError> },
    /// The rename landed but the read-back did not confirm the published tree:
    /// the live file now holds bytes nobody validated. `verdict` is what those
    /// landed bytes say the next boot would refuse — the same authority as a
    /// successful write-back's verdict, recorded even though the answer is
    /// `persisted: false` (R2-07-02).
    LandedUnverified { verdict: Vec<ConfigError> },
    /// Nothing reached the live file (pre-rename failure): the previous verdict
    /// still honestly describes it, so the caller must not record anything.
    NotLanded,
}

#[cfg(test)]
impl PersistOutcome {
    fn is_landed(&self) -> bool {
        matches!(self, Self::Landed { .. })
    }
}

/// Writes `category.default_model` into the config file atomically: parse the
/// on-disk file (or the boot config when absent), patch the one field, temp+rename.
///
/// The whole read-patch-write-rename runs inside the model write-back's gate and onto
/// a per-process unique temp: two handlers reach this function (`/har/config/default`
/// and `/har/config/model`'s dangling-default clear) on separate workers, and an
/// ungated read-modify-write onto one fixed temp lost one of their fields while both
/// answered `persisted: true` (FINDINGS-07 [3]).
///
/// It is the synchronous core by design — [`ApiState::set_default_model`] runs it
/// inside `tokio::task::spawn_blocking`, so the gate is held on a blocking thread
/// where parking is allowed and the reactor never is (FINDINGS-07 [8]).
///
/// Returns a [`PersistOutcome`]: only the `Landed` leg may answer
/// `persisted: true`. `LandedUnverified` means the rename landed and the
/// read-back refused the tree — its verdict describes the bytes that ARE on
/// disk (R2-07-02). `NotLanded` (the pre-rename failures) left the live file
/// untouched, so the previous verdict still describes it and none is returned.
fn persist_default_model(
    config: &AppConfig,
    category: CategoryId,
    model: Option<&str>,
) -> PersistOutcome {
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
        return PersistOutcome::NotLanded;
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
        return PersistOutcome::NotLanded;
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
    // The flushed door the yaml publisher and the first-run config writer use
    // (writeback.rs:605-612, firstrun.rs:158): a rename may never expose a temp
    // whose content the kernel merely buffered, or `persisted: true` can describe
    // bytes the volume was never told to commit and the file that boots can be
    // zero/partial after a crash in the write->rename window (R2-07-01).
    if let Err(error) = crate::model::writeback::write_synced(&tmp, text.as_bytes()) {
        tracing::error!(error = %error, "cannot persist default_model: write failed");
        return PersistOutcome::NotLanded;
    }
    // Keep the previous version beside the live file: schema additions are a
    // one-way door for `deny_unknown_fields` (an old binary refuses a file a new
    // one wrote), so rollback needs the pre-write copy (review, 2026-10-26).
    // The copy goes through the same flushed write as the temp — a rollback reads
    // exactly this file after a bad publish, and the forensic copy in
    // `model::writeback` is flushed for the same reason (R2-07-01).
    if path.exists() {
        let bak = path.with_extension("json.bak");
        let backup = std::fs::read_to_string(&path)
            .map(|previous| crate::model::writeback::write_synced(&bak, previous.as_bytes()))
            .and_then(|written| written);
        if let Err(error) = backup {
            tracing::warn!(error = %error, "could not write config backup; continuing");
        }
    }
    if let Err(error) = std::fs::rename(&tmp, &path) {
        // The temp is ours alone; leaving it behind turned a failed write into
        // permanent litter (writeback does the same on its rename failure).
        let _ = std::fs::remove_file(&tmp);
        tracing::error!(error = %error, "cannot persist default_model: rename failed");
        return PersistOutcome::NotLanded;
    }
    // The symmetric leg the yaml writer has had since D4 and this one lacked
    // (FINDINGS2-0507 [7]): `rename()` returning `Ok` is the OS accepting the move, not
    // proof the bytes landed. This is the one file the harness re-reads at boot under
    // `deny_unknown_fields`, so a silent-short write is a harness that will not start —
    // `persisted: true` may not be cheaper than the truth. Re-read, re-parse, and
    // require it to parse back to exactly the tree that was published.
    // The test arm corrupts the bytes this verification is about to read (valid JSON,
    // different tree), so the `persisted:false` leg of the pin is produced by the real
    // `read_back == root` comparison rather than a hard-set verdict — a rubber-stamped
    // compare has to be caught by the pin (FINDINGS-BATCH-2026-10-28M, item 6 note).
    #[cfg(test)]
    config_corrupt_if_armed(&path);
    let read_back = match std::fs::read_to_string(&path)
        .map_err(|error| format!("read-back failed: {error}"))
        .and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("re-parse failed: {error}"))
        }) {
        Ok(read_back) if read_back == root => read_back,
        // The rename already landed: the live boot file now holds these bytes,
        // whether or not they are the published tree. Refusing the answer is not
        // enough — `/har/status` `config_errors` must describe what is actually on
        // disk, or it would keep asserting the previous verdict over a file the
        // harness itself just replaced (R2-07-02). The verdict is derived with the
        // same authority as a successful write-back's: the boot's own validator
        // over the landed tree.
        Ok(landed) => {
            tracing::error!(
                path = %path.display(),
                "the persisted config file parses to a different tree than the one published; answering persisted:false"
            );
            return PersistOutcome::LandedUnverified {
                verdict: validate_published_config(&landed),
            };
        }
        // Unreadable or unparseable: the next boot would refuse the file root, so
        // one root-naming error is what the field must hold — same shape as
        // `validate_published_config`'s parse leg.
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path.display(),
                "cannot verify the persisted config file; answering persisted:false"
            );
            return PersistOutcome::LandedUnverified {
                verdict: vec![ConfigError::Invalid {
                    field: crate::config::CONFIG_FILE.to_string(),
                    reason: error,
                }],
            };
        }
    };
    tracing::info!(category = %id, model = ?model, path = %path.display(), "default_model persisted to the config file");
    PersistOutcome::Landed {
        verdict: validate_published_config(&read_back),
    }
}

/// Re-validate the tree the boot config file now holds after a landing write-back
/// (07[11]: the `/har/status` `config_errors` producer). The harness cannot fix a file
/// the operator hand-edited while it ran — reload re-reads the catalog and nothing
/// else (Step 9, LOCKED) — but the moment a harness write-back publishes the file it
/// owns every byte the next boot will read, so the verdict the boot would render
/// ([`AppConfig::validate`]) is reported while the harness is still alive to be asked.
/// A tree that no longer parses as the schema (a hand-edited unknown key rides the
/// read-patch-write as `Value` passthrough) yields one error naming the file root —
/// the same shape [`AppConfig::parse`] gives at boot.
fn validate_published_config(root: &Value) -> Vec<ConfigError> {
    match serde_json::from_value::<AppConfig>(root.clone()) {
        Ok(landed) => landed.validate(),
        Err(error) => vec![ConfigError::Invalid {
            field: crate::config::CONFIG_FILE.to_string(),
            reason: error.to_string(),
        }],
    }
}

/// Paths whose post-rename verification read is preceded by a byte corruption
/// (FINDINGS2-0507 [7]); see [`config_corrupt_if_armed`], which is shaped after the
/// write-back module's `corrupt_if_armed`.
#[cfg(test)]
fn config_read_back_faults() -> &'static Mutex<Vec<std::path::PathBuf>> {
    static ARMED: std::sync::OnceLock<Mutex<Vec<std::path::PathBuf>>> = std::sync::OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
pub(crate) fn arm_config_read_back_fault(path: &std::path::Path) {
    config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .push(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn release_config_read_back_fault(path: &std::path::Path) {
    config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .retain(|armed| armed != path);
}

/// The arm's bite (FINDINGS-BATCH-2026-10-28M, item 6 coverage note): between the
/// rename and the verification read, rewrite the file with **different but valid**
/// bytes, so production's own `read_back == root` comparison is what refuses the
/// `persisted: true`. Hard-setting a verdict here instead would have left the
/// always-`true` regression unpinned: the arm proves the compare runs, only a corrupted
/// file proves the compare decides.
#[cfg(test)]
fn config_corrupt_if_armed(path: &std::path::Path) {
    let armed = config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .iter()
        .any(|tracked| tracked == path);
    if !armed {
        return;
    }
    let text = std::fs::read_to_string(path).expect("the file exists in the verify window");
    let mut value: Value = serde_json::from_str(&text).expect("the published file is valid JSON");
    value
        .as_object_mut()
        .expect("a JSON object was published")
        .insert("tampered".to_string(), Value::Bool(true));
    let bytes = serde_json::to_string(&value).expect("invariant: JSON serializes");
    std::fs::write(path, bytes.as_bytes()).expect("the fixture corruption lands");
}

/// `GET /har/status`: boot info + slot table + jobs, one call (G13).
async fn har_status(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(status_document(&state).await?))
}

/// The full `/har/status` document: config categories, mem, boot, listener
/// health (raw verdicts plus the two derived booleans), slots, jobs, catalog
/// errors, and the boot config file's per-field write-back verdict — one call (G13).
/// Also the body of every SSE `har_state` event
/// (Step 14: one push replaces the page's state).
async fn status_document(state: &ApiState) -> Result<Value, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    Ok(render_status_document(state, &scheduler).await)
}

/// The render half of `/har/status`: everything comes from ONE scheduler snapshot
/// already in hand plus the cached witness/config reads — no model-file stat, no
/// second actor round-trip. Keeping the fs-free shape explicit is what makes
/// `/har/status`, the SSE build, and `/har/models` stat-free reads by construction
/// (Axis-2 ruling 2026-10-29's one-stat-per-event rule: `stale_config`'s stats live
/// on install, the reload turn, and harness write-backs — pinned by
/// `the_status_and_models_render_paths_never_touch_the_filesystem` (structural, this
/// file), `the_snapshot_reads_the_cached_verdict_and_never_stats` (scheduler), and
/// the api_surface hidden-file leg of
/// `a_flag_edit_never_ambers_its_own_loaded_model_and_status_never_stats`).
async fn render_status_document(state: &ApiState, scheduler: &SchedulerStatus) -> Value {
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
    // One registry acquisition for the whole badge story (FINDINGS2-0507 [6]):
    // `snapshot()`, `any_down()` and `pending()` each took the lock themselves, so a
    // supervisor flipping `Pending → Down` mid-document could publish `listeners`
    // saying `Serving` next to `listeners_down: true` — the red-over-green
    // contradiction 9c73a39 exists to prevent, re-opened through read timing. The
    // three fields are now three reads of ONE clone, through the registry's own
    // predicates (`is_down`/`is_pending`), never a re-derived match.
    let verdicts = state.listeners.snapshot();
    let mut document = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "control_addr": state.control_addr,
        "extended_logging": crate::observability::extended_logging(),
        // Non-zero means the on-disk post-mortem artifact is incomplete. The sink
        // cannot report itself — the failed write is what would have carried the
        // report — so this counter is the only durable signal that it died.
        "log_write_failures": crate::observability::log_write_failures(),
        "boot": boot,
        "categories": categories,
        "listeners": verdicts,
        // The header badge's two truths, computed here from the registry's own
        // predicates (FINDINGS-16 [4]). `listeners` above is the raw verdict map; a
        // page that decoded "is any listener down" out of its serde shape would be a
        // second brain for the rule the registry owns, and a `Down`/`Pending` that
        // ever changed representation would hide behind a green badge. Total over the
        // four seeded categories: `down` is some supervisor that could not serve a
        // port, `pending` is a category nobody has spoken for (the tray's amber).
        "listeners_down": verdicts.values().any(crate::listeners::ListenerState::is_down),
        "listeners_pending": verdicts.values().any(crate::listeners::ListenerState::is_pending),
        "mem": mem_section(state, scheduler).await,
        "slots": &scheduler.slots,
        "jobs": &scheduler.jobs,
        "last_error": last_error,
        "catalog_errors": &scheduler.catalog_errors,
        // The boot config file's per-field verdict as of the last landing harness
        // write-back (07[11]): cached state only — the boot itself exits on any
        // invalid field (§0.2 step 1), so a served document starts empty and the
        // write-back validator is the only producer.
        "config_errors": state.config_errors(),
    });
    // R5 report-only reconcile (ruling 9): the stranded-container names the
    // reconcile warned about ride the SAME snapshot, so the page and the log can
    // never disagree. Absent — not empty, not null — whenever nothing is
    // reported: the snapshot already normalizes an empty set to `None`.
    if let Some(names) = scheduler
        .stranded_containers
        .as_ref()
        .filter(|names| !names.is_empty())
    {
        document["stranded_containers"] = json!(names);
    }
    document
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

/// The loaded-side facts for one model row, joined server-side from ONE scheduler
/// snapshot (Axis 2, ruling 2026-10-29): the page reads these fields and derives
/// nothing (`stale_config === true` and `launched_at` verbatim are pinned in
/// `webui`). Same-name occupants (one model serving in two categories) merge
/// honestly: `launched_at` is the EARLIEST install among them (fixed-width UTC ISO
/// sorts lexicographically = chronologically), `stale_config` is ANY-of — one
/// resident on stale bytes makes the row amber.
#[derive(Debug, Clone, PartialEq, Eq)]
struct LoadedRow {
    launched_at: String,
    stale_config: bool,
}

fn loaded_rows(slots: &[crate::scheduler::SlotStatus]) -> BTreeMap<String, LoadedRow> {
    let mut rows: BTreeMap<String, LoadedRow> = BTreeMap::new();
    for slot in slots {
        let (Some(model), Some(launched_at)) = (slot.model.as_ref(), slot.launched_at.as_ref())
        else {
            continue;
        };
        let stale = slot.stale_config.unwrap_or(false);
        rows.entry(model.clone())
            .and_modify(|row| {
                if launched_at < &row.launched_at {
                    row.launched_at = launched_at.clone();
                }
                row.stale_config |= stale;
            })
            .or_insert(LoadedRow {
                launched_at: launched_at.clone(),
                stale_config: stale,
            });
    }
    rows
}

/// The one harness-facing model-listing shape: OpenAI fields plus the extras the
/// page's table renders (A5: `context`, `serve_model`, `model_ref`) and the loaded
/// row's server-computed facts (`launched_at`, `stale_config` — absent when the
/// model is not loaded, never null: not-loaded is a state, not an empty value).
/// `fit_validated` is the third joined fact: the actor's session cross-check
/// verdict, present as `true` only while it stands (absent is not false — an
/// unvalidated row keeps its exact unvalidated shape).
/// Single builder for `merged_state` and `/har/models` so they cannot drift. The
/// client-facing listener `/v1/models` is deliberately minimal — local file
/// paths stay off the LAN-facing surface.
fn model_json(entry: &ModelEntry, loaded: Option<&LoadedRow>, fit_validated: bool) -> Value {
    let mut body = json!({
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
        // Which figure admission actually books, stated by the gate's own decision
        // (one derivation; the page renders this and never re-computes the
        // precedence — the duplication-review rule made visible for R9).
        "cost_basis": match entry.cost_source() {
            crate::model::CostSource::Declared => "declared",
            crate::model::CostSource::Measured => "measured",
            crate::model::CostSource::Fit => "fit",
            crate::model::CostSource::Estimate => "estimate",
            crate::model::CostSource::Undeclared => "undeclared",
        },
        // R9/R1: whose number the measured pair is. The page shows it and admits
        // nothing from it — only a `cost_basis: "measured"` row ever renders a
        // provenance tag, which a `total_diff` can never become (it never books),
        // so the token alone is display context, never a booking claim.
        "measured_attribution": entry
            .measured_attribution
            .map(crate::model::MeasuredAttribution::as_str),
        // The displayed number == what admission must have free, shown only when the
        // provenance actually books (the K-round page-must-not-derive rule: the gate's
        // answer is computed here, never reconstructed in the browser). The page's
        // Cost cell renders `measured_gb` under the same condition, so this field is
        // the cross-check the operator's forensic log reads against the gate.
        "required_to_free_gb": (entry.cost_source() == crate::model::CostSource::Measured)
            .then_some(entry.measured_gb)
            .flatten(),
        "cost": entry.cost,
        "launch_floor_gb": entry.vram_gate.floor(),
        "load_timeout_s": entry.load_timeout_s,
        "cpu_offload": entry
            .raw
            .get("cpu_offload")
            .is_some_and(|v| crate::engine::llama::jinja_on(v)),
        // The EFFECTIVE served context (operator ruling 2026-11-02): llama's own
        // row knob, strata's `--max-context` argv token, ninfer's resolved
        // profile max-context — never the inert row key; null when the engine
        // speaks none (the one home is `engine::effective_context`).
        "context": crate::engine::effective_context(entry),
        "serve_model": entry.serve_model,
        "model_ref": entry.model_ref,
    });
    // R11: the Fit projection rides the payload only when it exists — a key
    // that is absent is not a key that is null (not-probed is a state), and a
    // fixture row that has never been probed keeps its byte-identical shape.
    if let Some(fit) = entry.fit_gb {
        body["fit_gb"] = json!(fit);
    }
    // R11 session cross-check: `fit_validated` appears ONLY while the verdict
    // stands — absent, never null, never `false` (an unvalidated row's document
    // is byte-identical to a never-checkable one, the same absent-is-not-null
    // rule as `fit_gb`; the fixture-parity pin rides it). The gate answers the
    // flag: a row the gate no longer books from Fit cannot carry a verdict about
    // a booking that is not happening.
    if fit_validated && entry.cost_source() == crate::model::CostSource::Fit {
        body["fit_validated"] = json!(true);
    }
    if let Some(loaded) = loaded {
        body["launched_at"] = json!(loaded.launched_at);
        body["stale_config"] = json!(loaded.stale_config);
    }
    body
}

/// The merged document every `har_state` event carries: one push replaces the
/// page's per-panel polling (Step 14).
async fn merged_state(state: &ApiState) -> Result<Value, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let loaded = loaded_rows(&scheduler.slots);
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            model_json(
                entry,
                loaded.get(&entry.name),
                scheduler.fit_validated.contains(&entry.name),
            )
        })
        .collect();
    let mut document = render_status_document(state, &scheduler).await;
    document["models"] = Value::Array(models);
    Ok(document)
}

/// The R9 sum for the memory bar: what the running containers said they reserve,
/// summed over the loaded occupants' per-slot `engine_report` figures only (the
/// scheduler snapshot carries each container's own report, so `/har/status` and the
/// SSE push derive the identical number — review 2026-10-31: threading it only
/// through the SSE build made the annotation unreachable on the poll path, which is
/// the ONLY path a token-gated page has). `None` when no loaded container has
/// reported — a container that never spoke must not render as "reserved 0 GiB"
/// next to a card that disagrees. Same-name occupants on two slots are one
/// container: the sum takes the model's figure once.
fn container_reported_gb(scheduler: &SchedulerStatus) -> Option<f64> {
    let mut per_model: BTreeMap<&str, f64> = BTreeMap::new();
    for slot in &scheduler.slots {
        if let (Some(model), Some(gib)) = (&slot.model, slot.container_reported_gb) {
            per_model
                .entry(model.as_str())
                .and_modify(|held| *held = held.max(gib))
                .or_insert(gib);
        }
    }
    let sum: f64 = per_model.values().sum();
    (sum > 0.0).then_some(sum)
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

/// `GET /har/models` — the full unfiltered catalog for the control page (G5: the
/// per-listener `/v1/models` stays category-filtered).
async fn har_models(State(state): State<Arc<ApiState>>) -> Result<Json<Value>, ApiError> {
    let catalog = state.catalog().await?;
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let loaded = loaded_rows(&scheduler.slots);
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            model_json(
                entry,
                loaded.get(&entry.name),
                scheduler.fit_validated.contains(&entry.name),
            )
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": models })))
}

/// `POST /har/config/default {"category": …, "model": name|null}` — set the
/// category's default model (Step 11 fallback); validated against the catalog and
/// persisted to `simpleharn.json` atomically.
///
/// `model` is required: a string sets the default, `null` clears it, and an absent
/// key is a caller mistake (400), never an implicit clear. The persisted value is the
/// catalog entry's canonical `name`, not the caller's spelling: `catalog::resolve`
/// accepts case-insensitive names and GGUF aliases, while the scheduler and the boot
/// check compare `m.name == model` exactly (FINDINGS-06 [5]).
async fn har_config_default(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with `category` and `model`: {rejection}"),
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
    let requested = match body.get("model") {
        None => {
            return Err(ApiError::BadRequest {
                reason: "missing required field `model` (send null to clear)".to_string(),
            })
        }
        Some(Value::Null) => None,
        Some(Value::String(model)) => {
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
            Some(entry.name.clone())
        }
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`model` must be a string or null".to_string(),
            })
        }
    };
    let (effective, persisted) = state
        .set_default_model(category, requested.as_deref())
        .await;
    Ok(Json(json!({
        "status": "ok",
        "category": category,
        "default_model": effective,
        "persisted": persisted,
    })))
}

/// `POST /har/config/model {"model": name, "category"?: WORK|RP|AUX|EMB, "cpu"?: bool}`
/// — write per-model flags into the model's yaml (Step 9: the file stays the
/// source of truth). A `cpu` edit on a model whose engine row lacks the
/// `cpu_offload` knob is refused 400 naming the engine: that key changes no
/// launch there, and the harness never writes inert keys.
/// A category change on a *loaded* model frees the slot so the
/// next use reloads it into the right listener (v3.3 §-behavior, kept). The reply
/// keeps `"status":"edited"` for the yaml edit it did and additionally names each
/// retag follow-up's outcome — `slot_freed` and `default_cleared` (`null` when not
/// attempted), plus `warnings` when either failed (R2-07-05). A wrong-typed
/// `category` is refused 400, not treated as absent (R2-07-04).
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
    // A present-but-non-string `category` read as absent through
    // `and_then(Value::as_str)`: the same defect FINDINGS2-0507 [5] fixed for `cpu`
    // below — a client that asked for two changes got one and an "edited" saying
    // both. The parity rule (07[10]) forbids one field's type bug living one arm
    // from its sibling's fix, so a wrong-typed `category` is a named 400, before
    // any write (R2-07-04).
    match body.get("category") {
        Some(Value::String(raw)) => {
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
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`category` must be a string".to_string(),
            })
        }
        None => {}
    }
    // A wrong-typed `cpu` is a caller mistake, not an absent one: `and_then(as_bool)`
    // dropped `"true"`/`1`/`"yes"` silently and the request still answered
    // `{"status":"edited"}` — a client that asked for two changes got one, reported as
    // success. The in-file sibling `har_config_logging` 400s a non-bool `enabled` for
    // exactly this, and 07[10] established the in-file parity rule
    // (FINDINGS2-0507 [5]). The type is checked before the engine gate, so a malformed
    // body gets the same answer regardless of the row it named.
    let cpu = match body.get("cpu") {
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`cpu` must be a boolean".to_string(),
            })
        }
        None => None,
    };
    if let Some(cpu) = cpu {
        // The key is a knob the row's engine must actually read (same const as the
        // scan hint, the stamp and the webui gate): the page stopped offering the
        // control for non-offload rows, so the API must not let a script mint the
        // dead yaml line the scan would then report. Refused with the named
        // sentence, not silently accepted or dropped.
        if !crate::engine::llama::offload_live(&entry.engine) {
            return Err(ApiError::BadRequest {
                reason: crate::model::scan::inert_key_reason("cpu_offload", &entry.engine),
            });
        }
        edits.push(("cpu_offload", Some(cpu.to_string())));
    }
    if edits.is_empty() {
        return Err(ApiError::BadRequest {
            reason: "nothing to edit: send `category` and/or `cpu`".to_string(),
        });
    }
    // One rewrite for every edit: calling `set_flag_on_file` per key would leave
    // the file half-edited (category moved, alias still there) if a write failed
    // between the two — 1g's non-atomic defect, not replicated here.
    // FINDINGS-07 [8]: the gated rewrite and the fingerprint that feeds the
    // re-baseline note run together on a blocking thread — the request worker
    // never parks on the yaml's read-modify-rename. The order the interlock
    // depends on survives intact: the write completes, the stat reads the bytes
    // the write landed, and only then does this handler send the note (before
    // dispatching the reload), so the actor's mailbox still orders the
    // re-baseline ahead of the reload turn's verdict refresh.
    let write_path = path.clone();
    let write_edits = edits.clone();
    let (written, fingerprint) = tokio::task::spawn_blocking(move || {
        let refs: Vec<(&str, Option<&str>)> = write_edits
            .iter()
            .map(|(key, value)| (*key, value.as_deref()))
            .collect();
        let written = crate::model::writeback::set_flags_on_file(&write_path, &refs);
        let fingerprint = written
            .is_ok()
            .then(|| crate::model::fingerprint(&write_path))
            .flatten();
        (written, fingerprint)
    })
    .await
    .map_err(|error| ApiError::Internal {
        reason: format!("model-yaml write task failed: {error}"),
    })?;
    written.map_err(|error| ApiError::from_writeback(&path, error))?;
    // Axis-2 interlock 5: the harness's own flag edit is not a config change since
    // launch. Re-baseline any resident's spawn-side capture from this file BEFORE
    // the reload.
    if let Some(fingerprint) = fingerprint {
        state
            .scheduler
            .note_model_file_written(&entry.name, &path, fingerprint);
    }
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
    //
    // Both follow-ups are part of the change the operator asked for (README: a
    // category change on a loaded model frees the slot), so their outcome belongs
    // in the reply: the old `let _ =` and dropped tuple answered `"edited"` even
    // when the slot stayed occupied or the default-clear never reached the file
    // (R2-07-05). The 200 stays an acceptance of the yaml edit — it is honest about
    // what it did — and `null` marks a follow-up that was not attempted.
    let mut slot_freed: Option<bool> = None;
    let mut default_cleared: Option<bool> = None;
    let mut warnings: Vec<String> = Vec::new();
    if let Some(new_cat) = requested_category.filter(|c| *c != old_category) {
        slot_freed = Some(match state.scheduler.unload_model(&entry.name).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    model = %entry.name,
                    "retagged model's slot was not freed"
                );
                warnings.push(format!(
                    "slot not freed for `{}`: it may keep serving its old listener \
                     until it is unloaded ({error})",
                    entry.name
                ));
                false
            }
        });
        let catalog = state.catalog().await?;
        if let Some(default) = state.default_model(old_category) {
            // Only an UNAMBIGUOUS resolution proves the category's default is the
            // model that just moved. An ambiguous one is refused here rather than
            // accepted on a guess: clearing the wrong default would rewrite the
            // operator's config file for a model that never moved.
            match crate::catalog::resolve(&default, &catalog) {
                crate::catalog::Resolution::Found(d) if d.name == entry.name => {
                    let (_effective, persisted) = state.set_default_model(old_category, None).await;
                    default_cleared = Some(persisted);
                    if persisted {
                        tracing::info!(
                            model = %entry.name,
                            from = %old_category,
                            to = %new_cat,
                            "cleared dangling category default"
                        );
                    } else {
                        tracing::warn!(
                            model = %entry.name,
                            from = %old_category,
                            to = %new_cat,
                            "dangling category default cleared in memory only; \
                             the config file still names the retagged model"
                        );
                        warnings.push(format!(
                            "the dangling {old_category} default was cleared in memory \
                             only; the config file still names the retagged `{}` as its \
                             default",
                            entry.name
                        ));
                    }
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
        "slot_freed": slot_freed,
        "default_cleared": default_cleared,
        "warnings": warnings,
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
///
/// The body is buffered through the same `Result<Json<Value>, JsonRejection>`
/// wall as every sibling POST (FINDINGS-05 [3]): a bare `Json<Value>` handed
/// axum's default `text/plain` rejection straight to the client, breaking the
/// documented `{code,message}` + OpenAI-envelope contract on this one route.
async fn har_config_logging(
    State(_state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with `enabled`: {rejection}"),
    })?;
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
    let container_reported_gb = container_reported_gb(scheduler);
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
                // R1: the witness's own answer to "could per-process bytes be
                // obtained" — the page's label must come from the witness, never
                // inferred from whether a number happened to be present (an idle
                // card legitimately has `attributed_gb: null` and `per_process`
                // capability at the same time).
                "attribution": sample.attribution.as_str(),
                // R9: what the loaded containers said they reserved, summed over
                // the currently loaded models' `engine_report` figures only —
                // `null` when no loaded model has a self-report. Display and
                // cross-check against `observed_used_gb`; never a gate input.
                "container_reported_gb": container_reported_gb,
                "source": state.witness.source(),
            })
        }
        Err(reason) => json!({
            "total_gb": state.config.vram_total_gb,
            "reserve_gb": state.config.vram_reserve_gb,
            "booked_gb": booked_gb,
            "observed_used_gb": null,
            "attributed_gb": null,
            // No sample, no claim: a witness that cannot answer cannot grade its
            // own attribution — absence, never a fake `unavailable`.
            "attribution": null,
            "container_reported_gb": container_reported_gb,
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

/// What the two reqwest error flags say about a failed forward.
///
/// Kept separate from `reqwest::Error` so the both-flags pair (R2-18-06) is
/// testable through a direct table; the timeout-first rule decides the real
/// pair, not a synthetic error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendFailureClass {
    /// The engine was live and slow, or its backlog never answered: 502, never a reap.
    Timeout,
    /// The port refused or reset before any response head: the death class.
    Connect,
    /// Any other send error: a plain forward failure.
    Other,
}

/// Decides the class from reqwest's two error flags.
///
/// `is_timeout()` is judged before `is_connect()` (R2-18-06): an error that
/// reports both — a connect timeout against a live listener whose backlog
/// blackholes SYNs — describes a live engine, and reaping it is the harm
/// FINDINGS-06 [2] closed. A connect-only failure (refused/reset) is the only
/// restart class.
fn classify_send_failure_flags(is_timeout: bool, is_connect: bool) -> SendFailureClass {
    if is_timeout {
        SendFailureClass::Timeout
    } else if is_connect {
        SendFailureClass::Connect
    } else {
        SendFailureClass::Other
    }
}

/// Classifies a `send()` failure for the crash-retry arm (FINDINGS-06 \[2]).
///
/// Restart-and-retry ONLY when the request never got a response AND the engine is
/// not listening — reqwest's `is_connect()` alone (refused/reset before response
/// head). A timeout (`is_timeout()`) is a live-but-slow engine (long non-streamed
/// generation, huge prefill): reaping it would discard a healthy KV cache and
/// re-forward the same slow request, so it answers `502` naming the class
/// instead. The timeout flag is judged first (R2-18-06), so an error reporting
/// both flags stays the timeout class and never reaps. Any other send error is
/// likewise never treated as death.
///
/// # Arguments
///
/// * `error` - the error returned by `reqwest::Client::send()`
/// * `port` - the engine port the forward targeted, for the operator-facing reason
///
/// # Errors
///
/// [`ApiError::Upstream`] for every class except a connect-only failure.
///
/// Public only so the integration suite can feed it real reqwest error values;
/// the proxy loop is its sole caller in production code.
pub fn classify_send_failure(error: &reqwest::Error, port: u16) -> Result<RetryAction, ApiError> {
    match classify_send_failure_flags(error.is_timeout(), error.is_connect()) {
        SendFailureClass::Timeout => Err(ApiError::Upstream {
            reason: format!("engine on :{port} timed out; not restarting a live engine"),
        }),
        SendFailureClass::Connect => Ok(RetryAction::Restart),
        SendFailureClass::Other => Err(ApiError::Upstream {
            reason: format!("forward failed: {error}"),
        }),
    }
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
        // `on_swap` only ever answers a swap with `Already` or by attaching the waiter
        // to a load that finishes `Loaded(receipt)`; `Done` is the cancel/reload reply
        // and `Unloaded` belongs to the unload commands.
        // Fabricating `0` here handed the proxy `127.0.0.1:0` — a connect failure three
        // layers from the cause, and a port number that belongs to no listener
        // (FINDINGS2-0507 [10]). An answer that names no port is an invariant
        // violation, and it is reported as one.
        Reply::Done | Reply::Unloaded { .. } => {
            return Err(ApiError::Internal {
                reason: format!(
                    "the scheduler acknowledged the swap of `{model}` into {category} without \
                     naming a port; refusing to proxy to an unbound address"
                ),
            })
        }
    };
    api.touch(model);
    api.scheduler.touch(model);
    Ok((port, serve_id))
}

/// Does this request wake its slot — that is, may it trigger admission and load a
/// model?
///
/// The deny-by-default, single source of truth for "reads never wake" (operator
/// ruling 2026-11-04): only the three inference POST paths wake. Everything else
/// — `GET /v1/models`, `/health`, every `/har/*` route, and any non-POST variant
/// — answers `false` and can never load a model. [`category_router`] wires
/// exactly these paths to [`proxy`], and the unit pin
/// `the_wake_set_is_exactly_the_proxy_routes` fails the moment the predicate and
/// the route table diverge, in either direction.
fn wakes_slot(method: &Method, path: &str) -> bool {
    method == Method::POST
        && matches!(
            path,
            "/v1/chat/completions" | "/v1/completions" | "/v1/embeddings"
        )
}

/// Proxy pass-through: POST /v1/chat/completions | /v1/completions | /v1/embeddings.
/// The response body streams through byte-for-byte (SSE included).
async fn proxy(
    State(listener): State<Arc<ListenerState>>,
    method: Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    // The proxy door is the one place that admits (via `ensure_target` below), so
    // the request reaching it must satisfy `wakes_slot`. A violation is not a
    // client error — method routing runs before handlers, so a route arriving here
    // against the predicate is a router wiring bug, never a request a client sent.
    // It is reported as such and never as a new user-visible refusal: the release
    // build logs a structured warning and serves the request exactly as before (no
    // behavior change, Variant A), and the debug build panics, so the falsifier
    // suite catches any future mis-wire at its source.
    if !wakes_slot(&method, uri.path()) {
        tracing::warn!(
            method = %method,
            path = %uri.path(),
            "wiring bug: a route that must never wake reached the proxy door"
        );
        debug_assert!(
            false,
            "wiring bug: {method} {} reached the proxy door but must never wake a slot",
            uri.path()
        );
    }
    let Json(mut body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON: {rejection}"),
    })?;
    let (model, port, serve_id) = ensure_target(&listener, &body).await?;
    // FINDINGS-06 [10]: forward the request target the client actually sent.
    // `uri.path()` alone silently dropped the query string, so an engine-side
    // `?api-key=` or engine-specific flag never arrived. `path_and_query()` is
    // path + `?` + query (an empty `?` included); with no query present its bytes
    // equal `path()`, so the path-join semantics of the URL below (routes match
    // absolute paths starting with `/`; no double slash, no trailing-slash drift)
    // are unchanged. `None` means authority- or asterisk-form (never POSTed to
    // these routes); the `path()` fallback keeps the old behavior for that case.
    let path = uri
        .path_and_query()
        .map(|path_and_query| path_and_query.as_str())
        .unwrap_or_else(|| uri.path())
        .to_string();
    // F-4 fix: rewrite to the engine's self-reported id when discovery found one;
    // the harness name is the fallback (llama engines ignore the field either way).
    body["model"] = Value::String(serve_id.unwrap_or_else(|| model.clone()));

    // T10.1: a connection-level failure (engine died before any response) triggers
    // exactly one reap + reload + retry. Only a connect-only failure classifies as
    // death (FINDINGS-06 [2], R2-18-06): a timeout — even one whose error also
    // reports `is_connect()` — means a live engine is still thinking, and reaping
    // it discards a healthy KV cache. A genuine upstream HTTP status is never
    // retried (A9), and a death mid-stream surfaces in the body stream, past this
    // loop.
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
                // The refused `port` rides the command (R2-06-01): the actor tells
                // the corpse this request witnessed from a successor a previous
                // retry already installed, and never reaps the healthy generation.
                listener
                    .api
                    .scheduler
                    .restart(&model, listener.category, port)
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
    // Engine→client header pass-through. The body streams through `bytes_stream()`
    // undecoded (reqwest is built without gzip/brotli/deflate/zstd), so the wire
    // bytes ARE the engine's bytes and every header describing them must survive —
    // `content-encoding` included (FINDINGS-06 [9]: stripping it labelled a
    // compressed body as identity and the client could not decode it). Dropped are
    // the headers that would LIE about the forwarded body — `transfer-encoding` and
    // `content-length` (hyper re-frames this streamed body and recomputes them) —
    // and the RFC 9110 §7.6.1 hop-by-hop set, which belongs to the engine↔harness
    // hop only (R2-06-02): `connection` itself, the six registered hop-by-hop names
    // (`keep-alive`, `upgrade`, `te`, `trailer`, `proxy-authenticate`,
    // `proxy-authorization`), and every header NAME enumerated by the engine's
    // `connection` value — forwarding a header the dropped `connection` made
    // hop-by-hop would advertise a trailer or protocol switch this harness never
    // re-frames or relays. Client→engine request headers are never copied at all
    // (FINDINGS-06 "verified clean": the forward is `post(url).json(&body)` only),
    // so there is no request-side encoding to strip and no token/host/cookie leak.
    let connection_named: Vec<String> = upstream
        .headers()
        .get_all("connection")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(|name| name.trim().to_ascii_lowercase())
        .filter(|name| !name.is_empty())
        .collect();
    for (name, value) in upstream.headers() {
        if matches!(
            name.as_str(),
            "transfer-encoding"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "upgrade"
                | "te"
                | "trailer"
                | "proxy-authenticate"
                | "proxy-authorization"
        ) || connection_named.iter().any(|n| n.as_str() == name.as_str())
        {
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
        // Outermost: the router's own 404/405 refusals carry the project
        // envelope, the guard's request id included (R2-05-03).
        .layer(axum::middleware::from_fn(envelope_router_errors))
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
