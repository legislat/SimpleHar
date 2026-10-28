//! Control page (DESIGN Step 14, operator mockup 2026-10-25): Pico CSS + vanilla
//! JavaScript, assets embedded with `include_str!` — the page only renders `/har/*`
//! JSON and every action is a headless API call. The header is persistent (slot
//! cards + memory bars never scroll away); the category tabs live below it.

use crate::api::ApiState;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use std::sync::Arc;

const INDEX_HTML: &str = include_str!("index.html");
const STYLE_CSS: &str = include_str!("style.css");
const APP_JS: &str = include_str!("app.js");
const PICO_CSS: &str = include_str!("pico.min.css");

async fn index() -> Response {
    (
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        INDEX_HTML,
    )
        .into_response()
}

async fn stylesheet() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        STYLE_CSS,
    )
        .into_response()
}

async fn pico() -> Response {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        PICO_CSS,
    )
        .into_response()
}

async fn script() -> Response {
    (
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        APP_JS,
    )
        .into_response()
}

async fn not_found() -> Response {
    StatusCode::NOT_FOUND.into_response()
}

/// The page routes, merged into the control listener (Step 11: `/har/*` and the
/// page live on the designated control listener only).
pub fn router(_state: Arc<ApiState>) -> Router {
    Router::new()
        .route("/", get(index))
        // The guard's PUBLIC_PATHS already names this, and the README advertises it, so
        // a bookmarked `.../index.html` should answer rather than 404 on a path that is
        // documented as public.
        .route("/index.html", get(index))
        .route("/style.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/pico.min.css", get(pico))
        .fallback(not_found)
}

#[cfg(test)]
mod tests {
    use super::{APP_JS, STYLE_CSS};
    use crate::engine::llama::OFFLOAD_ENGINES;

    /// C of the `cpu_offload` scoping ruling: the served page — the exact
    /// `include_str!` bytes `/app.js` answers, so an `include_str!`-stale exe
    /// cannot hide a page bug (GOTCHAS [ui]) — gates the CPU control on the
    /// engine table. Nature, declared: a source-content assertion, not a render
    /// test (cargo tests have no DOM engine; the render check is the node DOM
    /// stub in `tests/webui/`). The gate list is compared against the Rust
    /// `OFFLOAD_ENGINES` const itself, not a second copy of it, closing the
    /// list-drift trap (GOTCHAS [test]). Falsifier: restoring the checkbox
    /// unconditionally drops the gate/branch asserts or duplicates the control.
    #[test]
    fn served_app_js_gates_the_cpu_control_on_the_offload_engine_table() {
        const LIST: &str = "const OFFLOAD_ENGINES = [";
        let at = APP_JS
            .find(LIST)
            .expect("the page must carry the OFFLOAD_ENGINES gate list");
        let tail = &APP_JS[at + LIST.len()..];
        let end = tail.find(']').expect("gate list unterminated");
        let listed: Vec<&str> = tail[..end]
            .split(',')
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .map(|token| token.trim_matches(['"', '\'']))
            .collect();
        assert_eq!(
            listed, OFFLOAD_ENGINES,
            "the page's gate list must equal the Rust OFFLOAD_ENGINES const"
        );
        const GATE: &str = "if (OFFLOAD_ENGINES.includes(model.engine)) {";
        let gate_at = APP_JS
            .find(GATE)
            .expect("the CPU cell must be created only inside the engine gate");
        const CONTROL: &str = "\"aria-label\": \"cpu offload\"";
        assert_eq!(
            APP_JS.matches(CONTROL).count(),
            1,
            "exactly one CPU control exists, inside the gate"
        );
        let cpu_at = APP_JS
            .find(CONTROL)
            .expect("the gated branch must still build the CPU checkbox");
        assert!(
            cpu_at > gate_at,
            "the checkbox must sit after the gate test"
        );
        assert!(
            APP_JS[gate_at..].contains("text(\"\u{2014}\")"),
            "non-offload rows must render a dash, not a control"
        );
    }

    /// FINDINGS-16 [4]: the header badge reads the two verdicts `/har/status` computes
    /// and no longer decodes the serde shape of `listeners` in the browser.
    ///
    /// Nature, declared: a source-content assertion over the exact `include_str!`
    /// bytes `/app.js` and `/style.css` answer, so an `include_str!`-stale exe cannot
    /// hide a page bug (GOTCHAS [ui]); it is not a render test. The render legs — one
    /// per badge state — live in `tests/webui/webui_dom_test.mjs`, and CSS application
    /// and stacking stay untestable there (GOTCHAS [ui][test]), which is why the amber
    /// rule's existence is pinned at the byte level here.
    ///
    /// # Falsifier
    ///
    /// Restore the `Object.values(status.listeners).some((l) => typeof l === "object")`
    /// derivation in `renderHeader` and the two absence asserts fail; read a differently
    /// named field (or drop the `=== true`, letting `undefined` through as truthy) and
    /// the presence asserts fail; delete the amber tone from either asset and its assert
    /// fails while the red one stays green.
    #[test]
    fn served_app_js_reads_the_server_computed_listener_verdicts() {
        const SHAPE_SCAN: &str = "typeof listener === \"object\"";
        const SHAPE_USE: &str = "Object.values(status.listeners)";
        assert!(
            !APP_JS.contains(SHAPE_SCAN),
            "the page must not decide a verdict from the JSON shape of `listeners`"
        );
        assert!(
            !APP_JS.contains(SHAPE_USE),
            "and must not scan that map at all: the verdicts are `/har/status` fields"
        );
        for field in [
            "status.listeners_down === true",
            "status.listeners_pending === true",
        ] {
            assert!(
                APP_JS.contains(field),
                "the header must read the server's own boolean exactly: missing {field}"
            );
        }
        const RED: &str = "\"data-tone\": \"bad\"";
        const AMBER: &str = "\"data-tone\": \"warn\"";
        assert_eq!(
            APP_JS.matches(RED).count(),
            1,
            "one red badge, for a listener the harness reports down"
        );
        assert_eq!(
            APP_JS.matches(AMBER).count(),
            1,
            "one amber badge, for the no-verdict state (the tray's colour, page-side)"
        );
        assert!(
            STYLE_CSS.contains(".sh-badge[data-tone=\"warn\"]") && STYLE_CSS.contains("--sh-warn"),
            "the amber badge needs a rule in the served stylesheet or it renders as the \
             default green badge"
        );
    }
}
