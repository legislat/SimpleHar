//! The proxy path of the HTTP surface: the `/v1` dispatch of one category
//! listener, the forwarding handler, the model-name resolution behind it, the
//! swap-to-port call, and the send-failure classes that decide the crash-retry
//! of a forward. The `ApiState` accessors, the catalog resolution shared with
//! the control surface, the envelope middleware, and `control_router` stay in
//! `mod.rs`.

use crate::config::CategoryId;
use crate::scheduler::{Reply, SchedulerStatus};
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, State};
use axum::http::{HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::Value;
use std::sync::Arc;
use std::time::Instant;
use tower_http::{compression::CompressionLayer, trace::TraceLayer};

use super::{
    envelope_router_errors, guard, health, resolve_unique, v1_models, ApiError, ApiState,
    PROXY_BODY_LIMIT,
};

/// One category listener's view of the shared state.
#[derive(Clone)]
pub(super) struct ListenerState {
    pub(super) api: Arc<ApiState>,
    pub(super) category: CategoryId,
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
pub(super) fn pick_loaded_occupant(
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
pub(super) enum SendFailureClass {
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
pub(super) fn classify_send_failure_flags(is_timeout: bool, is_connect: bool) -> SendFailureClass {
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
pub(super) fn wakes_slot(method: &Method, path: &str) -> bool {
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
/// `control_router`, on the designated control listener only).
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
