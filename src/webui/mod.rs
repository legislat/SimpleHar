//! Control page (DESIGN Step 14, operator mockup 2026-10-25): Pico CSS + vanilla
//! JavaScript, assets embedded with `include_str!` — the page only renders `/har/*`
//! JSON and every action is a headless API call. The header is persistent (the
//! merged slot/tab strip and the memory bars never scroll away, compacted
//! 2026-11-01); the model table lives below it.

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
/// The operator's tri-color Browser_Tab icon (asset 2026-11-05), served as-is.
const FAVICON_PNG: &[u8] = include_bytes!("favicon.png");

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

async fn favicon() -> Response {
    ([(header::CONTENT_TYPE, "image/png")], FAVICON_PNG).into_response()
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
        .route("/favicon.png", get(favicon))
        // Browsers probe `/favicon.ico` whether or not the page links an icon; the
        // same PNG bytes under that name answer the probe instead of a 404 (the
        // sniffing decoders read a PNG served as `.ico`).
        .route("/favicon.ico", get(favicon))
        .fallback(not_found)
}

#[cfg(test)]
mod tests {
    use super::{APP_JS, FAVICON_PNG, INDEX_HTML, STYLE_CSS};
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

    /// Axis 2 (operator ruling 2026-10-29): the row dot's amber reads the SERVER's
    /// boolean directly (`model.stale_config === true`, the exact-compare the K-round
    /// listener-verdict rule demands — `undefined` must not slip through as truthy),
    /// carries the ruling's tooltip wording, and prints `model.launched_at` verbatim
    /// with no client-side date math (the only `new Date(` the page may keep is the
    /// log panel's pre-existing `entry.ts_ms` formatting — asserted scoped, not
    /// blanket-absent). The amber rule's existence in the served stylesheet is pinned
    /// at the byte level because CSS is untestable in the DOM stub (GOTCHAS [ui]).
    ///
    /// Nature, declared: a source-content assertion over the exact `include_str!`
    /// bytes `/app.js` and `/style.css` answer (not a render test — the render legs
    /// are `tests/webui/webui_dom_test.mjs`).
    ///
    /// # Falsifier
    ///
    /// Drop the `=== true` and the exact-compare assert fails; re-derive staleness
    /// page-side from file times (a `Date` around `launched_at`) and the scoped
    /// absence asserts fail; delete the CSS rule and its assert fails.
    #[test]
    fn served_app_js_renders_the_stale_bool_and_launch_stamp_as_given() {
        assert!(
            APP_JS.contains("model.stale_config === true"),
            "the amber must read the server's own boolean exactly"
        );
        assert!(
            APP_JS.contains("\"data-stale\": \"amber\""),
            "the serving row's stale marker must carry the attribute the stylesheet paints"
        );
        assert!(
            APP_JS.contains(
                "title: \"config file changed since launch \\u2014 unload + reload to apply\""
            ),
            "the tooltip must be the ruling's true sentence, byte-exact"
        );
        assert!(
            APP_JS.contains("model.launched_at ? [text(` \\u00b7 launched ${model.launched_at}`)]"),
            "the launch stamp must render verbatim from the server's string"
        );
        for derived in ["new Date(model", "Date.parse(model", "launched_at.toLocale"] {
            assert!(
                !APP_JS.contains(derived),
                "no date math around the launch field: found `{derived}`"
            );
        }
        assert!(
            STYLE_CSS.contains(".sh-dot[data-stale=\"amber\"]") && STYLE_CSS.contains("--sh-warn"),
            "the amber needs a rule in the served stylesheet or it renders as the green dot"
        );
    }

    /// FINDINGS-16 [4]: the header badge reads the two verdicts `/har/status` computes
    /// and no longer decodes the serde shape of `listeners` in the browser. The
    /// stranded-reservation badge (R5) rides the same rule: it reads
    /// `status.stranded_containers` and names only what the harness lists.
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
            "status.stranded_containers",
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
            2,
            "two amber badges: the listener no-verdict state (the tray's colour, page-side) \
             and the reservations the harness reports stranded"
        );
        assert!(
            STYLE_CSS.contains(".sh-badge[data-tone=\"warn\"]") && STYLE_CSS.contains("--sh-warn"),
            "the amber badge needs a rule in the served stylesheet or it renders as the \
             default green badge"
        );
    }

    /// Cost provenance is colour, not prose (operator 2026-11-01): the served
    /// page paints a gate-validated figure green and an unchecked one yellow,
    /// and the CSS rules both colours need exist in the served stylesheet —
    /// untestable in the DOM stub (GOTCHAS [ui][test]), so pinned byte-level.
    ///
    /// Nature: a source-content assertion over the `include_str!` bytes.
    /// Falsifier: dropping either rule, or colouring both states from one token,
    /// fails.
    #[test]
    fn served_assets_colour_the_cost_cell_by_validation() {
        assert!(
            APP_JS.contains("sh-cost-\" + (costChecked ? \"checked\" : \"unchecked\")"),
            "the cost cell must carry exactly the checked/unchecked class token"
        );
        assert!(
            STYLE_CSS.contains(".sh-cost-checked { color: var(--sh-accent-solid); }")
                && STYLE_CSS.contains(".sh-cost-unchecked { color: var(--sh-warn); }"),
            "green must be the accent and yellow the warn token, each with its own rule"
        );
    }

    /// The `releasing` slot state (operator ruling 2026-11-04): both the tab
    /// pill and the model row read the server's `slots[].state` string
    /// directly — the page never re-derives the release (same rule as the
    /// stale amber and the fault dot) — and `releasing` gets its own
    /// stylesheet treatment, distinct from `loading`'s amber. CSS application
    /// is untestable in the DOM stub (GOTCHAS [ui][test]), so the rule's
    /// existence is pinned at the byte level, like the amber before it; the
    /// render legs live in `tests/webui/webui_dom_test.mjs`.
    ///
    /// Nature, declared: a source-content assertion over the exact
    /// `include_str!` bytes `/app.js` and `/style.css` answer.
    ///
    /// # Falsifier
    ///
    /// Replace a server read with a page-side derivation (`loading_job` or
    /// `model` alone) and one of the two reads disappears: the count assert
    /// fails. Delete the `releasing` rule from the stylesheet and its assert
    /// fails while the cost asserts stay green.
    #[test]
    fn served_assets_render_the_server_releasing_state() {
        const READ: &str = "slot.state === \"releasing\"";
        assert_eq!(
            APP_JS.matches(READ).count(),
            2,
            "the tab pill AND the model row must read the server's `state` string, \
             never re-derive it"
        );
        assert!(
            APP_JS.contains("text(\"releasing\u{2026}\")"),
            "the pill must say releasing, not restate the occupant"
        );
        assert!(
            APP_JS.contains("text(\"Releasing\u{2026}\")"),
            "the releasing row parks on an honest disabled button"
        );
        assert!(
            STYLE_CSS.contains(".sh-dot[data-state=\"releasing\"]")
                && STYLE_CSS.contains("--sh-releasing"),
            "the releasing dot needs its own rule and token or it renders as \
             another state's colour"
        );
    }

    /// The working-model dot, made bright (operator 2026-11-05): the matte
    /// olive served-dot read as nothing at 9px, so serving takes the bright
    /// green `--sh-dot-live` (#22c55e) with a 1px ring of its deep shade
    /// `--sh-dot-live-ring` (#14532d) - a non-text graphic held to >= 3:1 on
    /// its ring/fill combination (measured: light ring 7.6:1 on the surface,
    /// fill 4.0:1 on the ring; dark fill 7.6:1 on the card, ring a subtle
    /// 1.9:1 edge). Every other state keeps its hue and the same ring
    /// treatment; dark drops the other rings to a near-black tint. The
    /// selected-tab olive accent must stay exactly as it was.
    ///
    /// Nature, declared: a source-content assertion over the exact
    /// `include_str!` bytes `/style.css` answers.
    ///
    /// Falsifier: reverting the serving dot to the accent (or dropping a ring)
    /// fails the rule asserts; recoloring the tab accent fails its pins in
    /// `served_assets_join_the_unload_entity_and_colour_it_amber`.
    #[test]
    fn served_assets_brighten_the_working_dot() {
        assert!(
            STYLE_CSS.contains(
                ".sh-dot[data-state=\"serving\"] { background: var(--sh-dot-live); \
                 box-shadow: 0 0 0 1px var(--sh-dot-live-ring); }"
            ),
            "the serving dot is the bright live green with its deep ring"
        );
        assert!(
            !STYLE_CSS
                .contains(".sh-dot[data-state=\"serving\"] { background: var(--sh-accent-solid)"),
            "the matte olive is no longer the working indicator"
        );
        assert!(
            STYLE_CSS.contains("--sh-dot-live: #22c55e")
                && STYLE_CSS.matches("--sh-dot-live:").count() == 1
                && STYLE_CSS.contains("--sh-dot-live-ring: #14532d")
                && STYLE_CSS.matches("--sh-dot-live-ring:").count() == 1,
            "the live fill and its ring are declared once and shared by both themes"
        );
        assert!(
            STYLE_CSS.contains("background: var(--pico-muted-color);\n  box-shadow: 0 0 0 1px var(--sh-dot-idle-ring);")
                && STYLE_CSS.contains("--sh-dot-warn-ring: #7c4a03")
                && STYLE_CSS.contains("--sh-dot-releasing-ring: #5c3804")
                && STYLE_CSS.contains("--sh-dot-bad-ring: #7f1d12")
                && STYLE_CSS.contains("--sh-dot-idle-ring: #3d372c"),
            "idle gets its ring in the base rule and every light state ring is \
             one dark step of its own hue"
        );
        assert!(
            STYLE_CSS.contains("--sh-dot-warn-ring: #241703")
                && STYLE_CSS.contains("--sh-dot-releasing-ring: #1d1101")
                && STYLE_CSS.contains("--sh-dot-bad-ring: #260a06")
                && STYLE_CSS.contains("--sh-dot-idle-ring: #05070a")
                && STYLE_CSS.matches("--sh-dot-live-ring:").count() == 1,
            "dark drops the non-green rings to a near-black tint and keeps the \
             same green ring as light"
        );
    }

    /// The checked box on warm paper (operator 2026-11-05): Pico's tick is a
    /// hardcoded-white SVG glyph and the switch knob is white, both painted on
    /// `--pico-primary-background` / `--pico-switch-checked-background-color` -
    /// the warm control fill in light, a 1.24:1 vanishing tick. Light-only,
    /// both tokens take the matte olive on the checkbox element: white glyph
    /// and knob read at 5.8:1. Dark must stay exactly as Pico paints it
    /// (white on its blue is 5.1:1), and the radio shares neither token.
    ///
    /// Nature, declared: a source-content assertion over the exact
    /// `include_str!` bytes `/style.css` answers.
    ///
    /// Falsifier: scoping the override to dark or deleting it fails the
    /// asserts; the dark-uniformity legs keep the dark block honest.
    #[test]
    fn served_assets_darken_the_checked_box_in_light() {
        assert!(
            STYLE_CSS.contains(":root:not([data-theme=\"dark\"]) input[type=\"checkbox\"]")
                && STYLE_CSS.contains("html[data-theme=\"light\"] input[type=\"checkbox\"]")
                && STYLE_CSS.contains("--pico-primary-background: var(--sh-accent-solid);")
                && STYLE_CSS
                    .contains("--pico-switch-checked-background-color: var(--sh-accent-solid);")
                && STYLE_CSS
                    .matches("--pico-switch-checked-background-color:")
                    .count()
                    == 1,
            "the checked checkbox and switch fill is the olive, declared light-only \
             and once - the white tick and knob need >= 4.5:1 on it (5.8:1 measured)"
        );
        assert!(
            !STYLE_CSS.contains("html[data-theme=\"dark\"] input[type=\"checkbox\"]")
                && !STYLE_CSS.contains(":root[data-theme=\"dark\"] input[type=\"checkbox\"]"),
            "dark's checked box stays Pico's blue, byte-identical"
        );
    }

    /// The unload entity and its amber (operator ruling 2026-11-05): the three
    /// unload gestures — tab-side, row Unload, Unload all — carry the one
    /// `sh-unload` class in the served page bytes, the served stylesheet
    /// carries the amber class rule and the warm-parchment light palette the
    /// ruling names, and no rule may repaint the joined side when its tab is
    /// selected: the shape joins, the amber action stays amber. Rule
    /// existence is pinned byte-level like the amber tones before it; the
    /// matte-olive accent (bright #2ea043 retired), the table-cell actions
    /// wrapper (seam fix), the shared unload plate and the warm Pico-primary
    /// scheme ride the same served bytes; the cascade legs (joined edges
    /// computed from the served stylesheet, the class on each rendered
    /// button) live in `tests/webui/webui_dom_test.mjs`.
    ///
    /// Nature, declared: a source-content assertion over the exact
    /// `include_str!` bytes `/app.js` and `/style.css` answer, so an
    /// `include_str!`-stale exe cannot hide a page bug (GOTCHAS [ui]).
    ///
    /// # Falsifier
    ///
    /// Drop `sh-unload` from any one call site and that count assert fails;
    /// delete the amber class rule or the parchment values, reintroduce a
    /// selected-side accent override, the bright #2ea043, a flex on the
    /// actions td, a transparent unload plate, a renamed-away `Default`
    /// label, a row-action block that is not compact, equal-width and
    /// left-aligned, light body cells painting their own surface instead of
    /// the panel's, a light block falling back to the card hex instead of
    /// the page parchment, or a wrapper without the warm scrollbar rule, and
    /// their asserts fail while the releasing and cost asserts stay green.
    #[test]
    fn served_assets_join_the_unload_entity_and_colour_it_amber() {
        assert_eq!(
            APP_JS.matches("\"sh-mini sh-tab-side sh-unload\"").count(),
            1,
            "the tab-side Unload carries the amber class on its joined pill"
        );
        assert_eq!(
            APP_JS.matches("\"sh-mini sh-unload\"").count(),
            2,
            "the row Unload and the Unload all carry the amber class"
        );
        assert!(
            STYLE_CSS.contains(".sh-unload {") && STYLE_CSS.contains("var(--sh-warn-ink)"),
            "the amber unload class needs its rule and its text-ink token in \
             the served stylesheet"
        );
        assert!(
            !STYLE_CSS.contains(".sh-tab[aria-selected=\"true\"] + .sh-tab-side"),
            "a selected tab must not repaint the joined side - the amber \
             action stays amber in every state"
        );
        assert!(
            APP_JS.contains("class: \"sh-row-actions\"")
                && APP_JS.contains("el(\"td\", { class: \"sh-actions\" }, actions)")
                && STYLE_CSS.contains("table.sh-models td.sh-actions")
                && !STYLE_CSS.contains("td.sh-row-actions"),
            "the actions cell must stay a table-cell wrapping a flexed span \
             (the display:flex td opened a card-white seam beside it)"
        );
        assert!(
            STYLE_CSS.contains("--sh-accent: #596B4770")
                && STYLE_CSS.contains("--sh-accent-solid: #596B47")
                && STYLE_CSS.contains("--sh-accent-solid: #8fae6f")
                && !STYLE_CSS.contains("#2ea043"),
            "the accent is the matte olive: shared wash #596B4770, matte \
             solid in light, the same hue lightened in dark; the bright \
             green #2ea043 is retired from the served stylesheet"
        );
        assert!(
            STYLE_CSS.contains(".sh-unload {")
                && STYLE_CSS.contains("background: var(--sh-tab-side-bg)"),
            "every unload button sits on the shared tab-side plate"
        );
        assert!(
            STYLE_CSS.contains("--pico-primary-background: #efe6d6")
                && STYLE_CSS.matches("--pico-primary-background:").count() == 2
                && STYLE_CSS.contains("--pico-muted-border-color: #d6c5a8")
                && STYLE_CSS.matches("--pico-muted-border-color:").count() == 1,
            "the served stylesheet carries the warm Pico-primary scheme and \
              the warm muted-border once, in the light block only (dark keeps \
              Pico blue and Pico's cool muted border); the second \
              --pico-primary-background is the light checked-box olive override"
        );
        assert!(
            APP_JS.contains("text(\"Default\")") && !APP_JS.contains("text(\"Set default\")"),
            "the row action is labelled Default - only the label renamed, \
             the handler still posts the default it always posted"
        );
        assert!(
            STYLE_CSS.contains("flex: 0 0 auto")
                && STYLE_CSS.contains("min-width: 4rem")
                && STYLE_CSS.contains("padding: 0.25rem 0.6rem")
                && STYLE_CSS.contains("justify-content: flex-start")
                && !STYLE_CSS.contains("justify-content: flex-end"),
            "row action buttons are word-width compact (small overhead, small \
             equalising floor, no grow) and left-aligned in the served stylesheet"
        );
        assert!(
            STYLE_CSS.contains("--sh-panel-bg: #f2eadc")
                && STYLE_CSS.contains("--sh-header-bg: #f2eadc")
                && STYLE_CSS.contains("--pico-table-border-color: #e2d5be")
                && STYLE_CSS.matches("--pico-table-border-color:").count() == 1
                && STYLE_CSS.contains("color: var(--sh-head-ink, var(--pico-color))")
                && STYLE_CSS.contains("--pico-primary-border: #015c8f")
                && STYLE_CSS
                    .contains("background: var(--sh-panel-bg, var(--pico-card-background-color))")
                && STYLE_CSS
                    .contains("background: var(--sh-header-bg, var(--pico-card-background-color))",)
                && STYLE_CSS.contains("html[data-theme=\"light\"] .sh-unload:not(:hover)")
                && !STYLE_CSS.contains("[data-theme=\"dark\"] .sh-unload"),
            "the approved ramp ships: surface #f2eadc (panel, header, table head - \
             no band), separator #e2d5be, blue buttons get a darker blue edge in \
             dark, and the light strip carries no plate behind any unload"
        );
        assert!(
            STYLE_CSS.contains("table.sh-models tbody td {")
                && STYLE_CSS.contains("border-bottom: 0;\n  background: transparent;")
                && !STYLE_CSS.contains("[data-theme=\"light\"] table.sh-models tbody td")
                && !STYLE_CSS.contains("[data-theme=\"dark\"] table.sh-models tbody td")
                && !STYLE_CSS.contains(":root:not([data-theme=\"dark\"]) table.sh-models tbody td"),
            "body cells paint nothing in both themes - one unscoped rule, the \
             panel token shows through every gap, dark mirrors light exactly"
        );
        assert!(
            STYLE_CSS
                .contains("scrollbar-color: var(--pico-card-border-color) var(--sh-chrome-bg)")
                && STYLE_CSS.contains(":root:not([data-theme=\"dark\"]) .sh-tablewrap"),
            "the table wrapper scrollbar carries the warm light tokens: its \
             white trough was the white \"border\" under the table"
        );
        for ramp in ["#f7f1e6", "#f2eadc", "#efe6d6", "#d6c5a8", "#e2d5be"] {
            assert!(
                STYLE_CSS.contains(ramp),
                "the warm ramp needs {ramp} in the served stylesheet"
            );
        }
    }

    /// A unit-handler state: the favicon handlers read no state at all, so the
    /// dummy seam `api`'s unit pins use is the honest here too.
    fn page_state() -> std::sync::Arc<crate::api::ApiState> {
        std::sync::Arc::new(crate::api::ApiState::new(
            crate::config::AppConfig::default(),
            crate::scheduler::SchedulerHandle::test_dummy(),
            crate::listeners::ListenerRegistry::new(),
            std::sync::Arc::new(crate::budget::LedgerOnlyWitness),
            crate::api::BootTimings::default(),
        ))
    }

    /// The favicon (operator asset 2026-11-05): both names answer 200 `image/png`
    /// with exactly the embedded PNG bytes — the `.ico` alias exists so the
    /// browser's default probe lands on the logo instead of the 404 fallback.
    ///
    /// Nature, declared: an in-process route test over `router()` itself, the page
    /// surface without the guard layer (`control_router` composes it in `api`);
    /// the guard's public-path ledger is doctrine-locked and untouched here.
    ///
    /// Falsifier: drop a route and that leg fails with the fallback 404; serve
    /// anything other than `FAVICON_PNG`, or a content-type other than
    /// `image/png`, and that leg's assert fails.
    #[tokio::test]
    async fn the_favicon_routes_answer_the_embedded_png() {
        use axum::body::Body;
        use axum::http::{header, Request, StatusCode};
        use tower::ServiceExt;
        for path in ["/favicon.png", "/favicon.ico"] {
            let response = super::router(page_state())
                .oneshot(
                    Request::get(path)
                        .body(Body::empty())
                        .expect("favicon request"),
                )
                .await
                .expect("in-process favicon read");
            assert_eq!(response.status(), StatusCode::OK, "{path} must answer");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .expect("favicon content-type"),
                "image/png",
                "{path} must answer as a PNG"
            );
            let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
                .await
                .expect("favicon body");
            assert_eq!(
                &bytes[..],
                FAVICON_PNG,
                "{path} must answer the embedded PNG bytes"
            );
        }
    }

    /// The page head links the favicon, so a browser uses the link instead of
    /// probing only `/favicon.ico`. Nature, declared: a source-content assertion
    /// over the exact `include_str!` bytes `/` answers, the served-asset style of
    /// every pin in this module.
    ///
    /// Falsifier: remove the link from `index.html` and the assert fails.
    #[test]
    fn served_index_links_the_favicon() {
        assert!(
            INDEX_HTML.contains("<link rel=\"icon\" type=\"image/png\" href=\"/favicon.png\" />"),
            "the page head must link the favicon: an unlinked page probes only \
             the .ico path"
        );
    }
}
