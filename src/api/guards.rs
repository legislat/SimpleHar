//! The request guard: the one middleware pipeline every listener router wraps —
//! the structural Host/Origin/content-type refusals, the control-token wall, the
//! request id, and the single pipeline entry that applies them in its fixed
//! order. The refusal-ring evidence stays in `mod.rs` beside the `ApiState`
//! state it mutates, and the router wiring that mounts this middleware stays
//! there too.

use crate::config::AppConfig;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::Json;
use std::sync::Arc;

use super::{error_body, ApiError, ApiState};

/// Process-wide request-id counter (mixed with the boot epoch: unique per run).
static REQUEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static REQUEST_EPOCH: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// `x-request-id` is not in the standard header constants.
pub(super) const X_REQUEST_ID: axum::http::HeaderName =
    axum::http::HeaderName::from_static("x-request-id");

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

/// Ceiling for a client-supplied request id (FINDINGS-05 [7]): the ring is the
/// operator's only "why did the last load fail" channel and the guard is its one
/// request producer, so an echoed id must stay one short line — never a flood of
/// attacker-chosen text that evicts real evidence one request at a time.
pub(super) const REQUEST_ID_MAX_CHARS: usize = 64;

/// Whether a client's `x-request-id` may be echoed and logged verbatim: non-empty,
/// at most [`REQUEST_ID_MAX_CHARS`], printable ASCII only. Rejected ids get a
/// freshly generated one rather than a truncation, deliberately: a truncated
/// attacker string still reads like a harness id in the ring, while a regenerated
/// `req-` id is visibly the harness's own. `HeaderValue::to_str` already limits
/// what can reach `raw` to visible ASCII plus tab, so `len` never over-counts a
/// multibyte character here — and tab is exactly what `is_ascii_graphic` refuses.
fn accepted_request_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= REQUEST_ID_MAX_CHARS
        && raw.bytes().all(|byte| byte.is_ascii_graphic())
}

/// Routes that must answer before a token exists, so the page can load and ask for
/// one. Exact matches only — never a prefix, which would re-open `/har/*`-adjacent
/// paths.
///
/// `pub` so the token test iterates this list rather than its own copy of it: when the
/// two were separate, a stylesheet added to the page but not here shipped an unstyled
/// control page while the test stayed green, because each asserted a private truth.
pub const PUBLIC_PATHS: [&str; 7] = [
    "/",
    "/index.html",
    "/app.js",
    "/style.css",
    // The page links Pico's base stylesheet, so it must answer before a token exists;
    // omitting it left the control page rendering unstyled whenever `control_token` was
    // set (README review, 2026-10-27).
    "/pico.min.css",
    // The page head links the operator's favicon (asset 2026-11-05), and a browser
    // probes `/favicon.ico` itself — neither request can carry a token, so both names
    // must answer before a token exists. Same rule that opened `pico.min.css`.
    "/favicon.png",
    "/favicon.ico",
];

/// The page's routine polls: GETs whose per-tick `request complete` line would evict
/// real evidence from the ring while the operator merely leaves the control page open
/// (FINDINGS-15 \[10] for the log read, FINDINGS2-0507 \[1] for the third poll).
///
/// `pub`, and the guard + its pin both read THIS list, with the pin's teeth coming
/// from an independent third source — the page's own `getJson("…")` literals in
/// `app.js`. The exemption used to be a private `matches!` literal inside the guard
/// while the pin named paths of its own: `/har/models`, which `refresh()` polls every
/// 2 s, sat in neither, so a token-protected harness turned its ring over in ~17
/// minutes with the page open and both suites stayed green.
pub const ROUTINE_POLL_PATHS: [&str; 3] = ["/har/status", "/har/log", "/har/models"];

/// The control token, if any: the environment wins so the secret can stay out of the
/// config file entirely (AGENTS.md — secrets live in the environment, not in
/// code/config). The env name is `SIMPLEHARN_TOKEN`, and it is the only name read
/// (TASK-NAMING-CLEANUP). Whitespace-only values mean "no token" at read time;
/// `validate` rejects them in the file, so a typo can never silently disable auth.
pub(super) fn resolve_control_token(config: &AppConfig) -> Option<String> {
    crate::config::token_from_env()
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
        .map(presented_token);
    match presented {
        Some(given) if token_matches(given.as_bytes(), expected.as_bytes()) => None,
        _ => Some(ApiError::Unauthorized {
            reason: format!(
                "{} requires the control token (X-Har-Token or Authorization: Bearer)",
                request.uri().path()
            ),
        }),
    }
}

/// The credential inside a presented header value: the `Bearer` scheme is stripped
/// ASCII-case-insensitively — RFC 7235 defines auth-schemes as case-insensitive, and
/// `authorization: bearer <token>` from an OpenAI-style client was refused with a
/// message that read as a wrong token (FINDINGS-05 [8]). Any other value is the
/// token itself. The trim matches `resolve_control_token`'s handling of the expected
/// value, as before.
pub(super) fn presented_token(raw: &str) -> &str {
    match raw.split_once(' ') {
        Some((scheme, credential)) if scheme.eq_ignore_ascii_case("bearer") => credential.trim(),
        _ => raw.trim(),
    }
}

/// Constant-time token equality (FINDINGS-05 [8]): every byte of an equal-length
/// pair is examined, so the wall cannot short-circuit on the first differing byte
/// and leak its position through timing. A length mismatch returns early — the
/// length remains observable, which the finding accepts as the honest residual of
/// a single-line fix. No dependency is added: `Cargo.lock` carries no
/// `subtle`/`constant_time_eq` today, and this is nine lines with no allocation.
pub(super) fn token_matches(given: &[u8], expected: &[u8]) -> bool {
    if given.len() != expected.len() {
        return false;
    }
    let mut difference = 0u8;
    for (g, e) in given.iter().zip(expected) {
        difference |= g ^ e;
    }
    difference == 0
}

/// The guard + correlation middleware (Step 11 "kept regardless", Step 12):
/// - **Host** must address a loopback — or any address when the harness publishes
///   on the unspecified bind (`internal_host = 0.0.0.0`, the documented no-auth
///   LAN posture where this guard is the floor) — on one of its own configured
///   ports; a hostname, an address outside the bind, a foreign port or a missing
///   `Host` is refused `400` before any handler runs (DNS-rebinding guard; the
///   gate runs on every request, not only the ones that bother to declare a Host,
///   FINDINGS-05 \[4]\[5], FINDINGS-16 N\[1]);
/// - **Origin** (when the client sends one — browsers do on cross-site POSTs) must
///   match the request's Host authority, else `403` (CSRF guard);
/// - a **POST** body must declare `application/json`, else `415` (Step 11);
/// - the **request id** comes from the client's `x-request-id` only when it is
///   short and printable (see `accepted_request_id`) and is generated otherwise,
///   echoed in the response header, and logged at request completion (G6,
///   FINDINGS-05 \[7]).
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
        .filter(|raw| accepted_request_id(raw))
        .map(str::to_string)
        .unwrap_or_else(next_request_id);

    let method = request.method().clone();
    let path = request.uri().path().to_string();

    // Structural refusals stay strictly BEFORE the token check (DECISIONS LOCKED,
    // README:265-268): a cross-site page that knows nothing about the token must be
    // refused for the security reason, not told the route wants a secret. The order is
    // pinned by `structural_refusals_are_answered_before_the_token_is_consulted`, which
    // is what makes swapping the two blocks below a red suite instead of a green one
    // (FINDINGS2-0507 [2]; the two refusals are also individually pinned in
    // `tests/api_surface.rs`, but only on a token-less app, which cannot see the order).
    if let Some(reason) = reject_reason(&state, &request) {
        state.note_guard_refusal(&reason, &method, &path, &request_id);
        let response = (reason.status(), Json(error_body(&reason))).into_response();
        return with_request_id(response, &request_id);
    }
    if let Some(reason) = auth_reason(&state, &request) {
        state.note_guard_refusal(&reason, &method, &path, &request_id);
        let response = (reason.status(), Json(error_body(&reason))).into_response();
        return with_request_id(response, &request_id);
    }

    let mut response = next.run(request).await;
    response = with_request_id(response, &request_id);
    // The page polls /har/status and /har/models every 2 s and opens the log panel
    // repeatedly; those lines would evict real evidence from the ring during long boots
    // — and reading the log must not be an event that pushes the log's own evidence out
    // (FINDINGS-15 [10], FINDINGS2-0507 [1]; the list is one const, shared with its
    // pin, whose teeth are the page's own fetch literals). Extended logging restores
    // them (operator, 2026-10-26).
    let routine_poll =
        method == axum::http::Method::GET && ROUTINE_POLL_PATHS.contains(&path.as_str());
    // R2-05-01: a public page or asset is not a command. Every `PUBLIC_PATHS` member
    // answers 200 without a token in every posture, so counting them as a re-arm
    // let an attacker alternate refused POSTs with free `GET /` packets and turn
    // the ring over at request rate — the exact eviction flood the cardinality
    // cap was built to end (FINDINGS-BATCH-2026-10-28M [2]). Only a success on a
    // path that needed something of the harness (a command) re-arms the window.
    if response.status().is_success() && !routine_poll && !PUBLIC_PATHS.contains(&path.as_str()) {
        // A real command succeeded: whatever the guard refused since the last one gets
        // one line now, and the refusals become reportable again.
        state.note_guard_recovery(&request_id);
    }
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
fn reject_reason(state: &ApiState, request: &axum::extract::Request) -> Option<ApiError> {
    let headers = request.headers();
    // The Host gate runs on every request, including one that carries no `Host`
    // at all: a conditional floor is not a floor, and the safety of a published
    // `internal_host` without a token may rest only on what the guard asserts,
    // never on an undocumented property of the HTTP stack (FINDINGS-05 [5]).
    match headers
        .get(axum::http::header::HOST)
        .and_then(|value| value.to_str().ok())
    {
        None => {
            return Some(ApiError::BadRequest {
                reason: "request carries no `Host` header; SE is a loopback tool and the DNS-rebinding guard must see the address the request was sent to".to_string(),
            });
        }
        Some(host) if loopback_host(state, host) => {}
        Some(host) => {
            return Some(ApiError::BadRequest {
                reason: format!(
                    "Host `{host}` is not this harness's loopback address on one of its own ports; SE is a loopback tool and refuses requests addressed by hostname or foreign address (DNS-rebinding guard)"
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

/// `true` when `host` addresses this harness. An IP-literal authority passes on
/// either of two rules: the address is loopback, or the configured bind
/// `internal_host` is unspecified — `0.0.0.0` is the publish posture whose
/// documented security floor is this very guard (no-auth LAN by design), so it
/// must answer every address the machine's LAN clients put in `Host`; boot
/// `validate` refuses every other bind, so there is no unicast bound address a
/// legitimate client could ever address (FINDINGS-16 N[1]: the old equality leg
/// against `internal_host` was dead code and 400'd every real LAN client). A
/// non-IP authority passes only as `localhost`: rebinding attacks deliver the
/// attacker's hostname in `Host`, so hostnames stay refused — that is the
/// DNS-rebinding defense. If the value carries a port, it must be one of the
/// harness's own configured ports (FINDINGS-05 [4]: accepting every IP literal
/// left `Host: 203.0.113.9:8708` and any wrong port passing, load-bearing only
/// because the Origin check happened to also compare the port). The allowed
/// ports come from the boot config, and the listener binds derive from it, so
/// the two cannot diverge.
pub(super) fn loopback_host(state: &ApiState, host: &str) -> bool {
    let Some((authority, port)) = split_host(host) else {
        return false;
    };
    let address_ok = match authority.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback() || state.config.internal_host.is_unspecified(),
        Err(_) => authority.eq_ignore_ascii_case("localhost"),
    };
    address_ok
        && port.is_none_or(|port| {
            state
                .config
                .categories
                .iter()
                .any(|category| category.public_port == port)
        })
}

/// Splits a `Host` value into authority and optional port. An IPv6 address may
/// appear bracketed only (RFC 3986 §3.2.2 — the form every browser sends): an
/// unbracketed address contains an inner colon and is refused, and so are
/// malformed authorities or unparseable ports, since none is a `Host` a
/// legitimate client produces.
pub(super) fn split_host(host: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = host.strip_prefix('[') {
        let (inner, suffix) = rest.split_once(']')?;
        let port = match suffix {
            "" => None,
            suffix => Some(suffix.strip_prefix(':')?.parse().ok()?),
        };
        return Some((inner, port));
    }
    match host.split_once(':') {
        None => Some((host, None)),
        Some((authority, port)) if !authority.contains(':') => {
            Some((authority, Some(port.parse().ok()?)))
        }
        Some(_) => None,
    }
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
