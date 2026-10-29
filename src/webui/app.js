// SimpleHar SE — control page logic (Step 14). Vanilla JS, no framework: the page
// only renders /har/* JSON; every action is an API call that also exists headless.
"use strict";

const CATEGORY_ORDER = ["WORK", "RP", "AUX", "EMB"];
const POLL_MS = 2000;

// Engines whose adapter reads `cpu_offload`: the row's checkbox renders only here.
// Mirrors the Rust `OFFLOAD_ENGINES` const (engine/llama.rs, §20-9 Fork C knob
// table row 1); the webui lib test compares these two lists, so they cannot drift.
const OFFLOAD_ENGINES = ["llama"];

// Anything a re-render would lose must live here: render() rebuilds the whole
// DOM on every SSE push, so UI state (open tabs, log panel) is state, not DOM.
let state = {
  status: null,
  models: [],
  activeTab: "WORK",
  logOpen: false,
  logLoaded: false,
  logEntries: null,
};

// ---------- tiny helpers ----------

// Optional control token (server: `control_token` / `SIMPLEHAR_TOKEN`). Stored in
// localStorage so a reload does not re-prompt, and sent as a header only — never in a
// URL, where it would land in the harness request log.
const TOKEN_KEY = "sh-control-token";

// The value behind the harness's last 401. A 401 re-prompts only for a token this
// is not, so a stored-but-wrong secret cannot open the modal on every poll tick
// (FINDINGS-16 [2]); the operator recovers through the header's Sign in control.
let rejectedToken = null;

function controlToken() {
  try {
    return window.localStorage.getItem(TOKEN_KEY) || "";
  } catch (error) {
    return "";
  }
}

function authHeaders(extra) {
  const headers = { ...(extra || {}) };
  const token = controlToken();
  if (token) headers["x-har-token"] = token;
  return headers;
}

// True when the operator supplied a token, so the caller may retry once.
function promptForToken() {
  let entered = null;
  try {
    entered = window.prompt("SE control token");
  } catch (error) {
    return false;
  }
  if (!entered) return false;
  try {
    window.localStorage.setItem(TOKEN_KEY, entered.trim());
  } catch (error) {
    return false;
  }
  return true;
}

// One request with the 401 dance, shared by GET and POST. The retry rebuilds its
// headers so it carries the token entered just now, never the one the first attempt
// went out with, and it skips the prompt when a sibling request on the same tick
// (status and models are fetched together) has already collected one.
async function authorizedFetch(path, init) {
  const sent = controlToken();
  const attempt = () => fetch(path, { ...init, headers: authHeaders(init && init.headers) });
  let response = await attempt();
  if (response.status !== 401) return response;
  if (controlToken() !== sent) return attempt();
  if (rejectedToken === sent) return response;
  rejectedToken = sent;
  if (!promptForToken()) return response;
  return attempt();
}

async function getJson(path) {
  const response = await authorizedFetch(path);
  if (!response.ok) {
    throw new Error(`${path} -> ${response.status}`);
  }
  return response.json();
}

async function postJson(path, body) {
  const response = await authorizedFetch(path, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(body),
  });
  const data = await response.json().catch(() => ({}));
  if (!response.ok) {
    throw new Error(data.message || `${path} -> ${response.status}`);
  }
  return data;
}

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (key === "class") node.className = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else if (value !== null && value !== undefined) node.setAttribute(key, value);
  }
  for (const child of children) {
    if (child === null || child === undefined) continue;
    node.append(child);
  }
  return node;
}

function text(value) {
  return document.createTextNode(String(value));
}

// ---------- theme (adaptive light/dark with a sticky toggle) ----------

function applyTheme(theme) {
  document.documentElement.setAttribute("data-theme", theme);
  localStorage.setItem("sh-theme", theme);
  const toggle = document.getElementById("sh-theme-toggle");
  if (toggle) toggle.textContent = theme === "dark" ? "Light" : "Dark";
}

function initTheme() {
  const stored = localStorage.getItem("sh-theme");
  const prefersDark = window.matchMedia("(prefers-color-scheme: dark)").matches;
  applyTheme(stored || (prefersDark ? "dark" : "light"));
  document.getElementById("sh-theme-toggle").addEventListener("click", () => {
    const current = document.documentElement.getAttribute("data-theme");
    applyTheme(current === "dark" ? "light" : "dark");
  });
}

// ---------- control token entry point (static header control) ----------

// The operator's way out of a mistyped secret (FINDINGS-16 [2]). It sits beside the
// theme toggle in index.html rather than in the rendered header because a page that
// has never completed a `/har/*` read renders no header at all: a control inside
// `renderHeader` would be exactly as unreachable as devtools was.
function paintTokenControl() {
  const control = document.getElementById("sh-token-toggle");
  if (control) control.textContent = controlToken() ? "Clear token" : "Sign in";
}

function signIn() {
  let cleared = true;
  try {
    window.localStorage.removeItem(TOKEN_KEY);
  } catch (error) {
    cleared = false;
  }
  if (!cleared) {
    flashError(new Error("the browser refused to forget the stored control token"));
  }
  // The stored value is gone, so the next 401 is about a different token and may ask.
  rejectedToken = null;
  refresh();
}

function initTokenControl() {
  const control = document.getElementById("sh-token-toggle");
  if (!control) return;
  control.addEventListener("click", signIn);
  paintTokenControl();
}

// ---------- rendering ----------

function slotJobsFor(status, category) {
  return status.jobs.filter((job) => job.category === category && !job.terminal);
}

function renderSlotCard(status, category) {
  const slots = status.slots.filter((slot) => slot.category === category);
  const loading = slotJobsFor(status, category).length > 0;
  const card = el("article", { class: "sh-slot" });
  const head = el("h4", {}, text(category), el("span", {}, text(categoryPort(status, category))));
  card.append(head);
  if (category === "AUX") {
    // The AUX card splits horizontally: one slim row per occupant (operator spec).
    if (slots.length === 0) card.append(el("div", { class: "sh-empty" }, text("no slots")));
    for (const slot of slots) {
      const row = el("div", { class: "sh-occupant-row" });
      if (slot.model) {
        row.append(el("span", { class: "sh-occupant" }, text(slot.model)));
        row.append(unloadButton(category));
      } else if (slot.loading_job !== null) {
        row.append(el("span", { class: "sh-empty" }, text("loading…")));
      } else {
        row.append(el("span", { class: "sh-empty" }, text("—")));
      }
      card.append(row);
    }
    return card;
  }
  const main = el("div", { class: "sh-slot-main" });
  const slot = slots[0];
  if (slot && slot.model) {
    main.append(el("span", { class: "sh-occupant" }, text(slot.model)));
    main.append(unloadButton(category));
  } else if (loading) {
    main.append(el("span", { class: "sh-empty" }, text("loading…")));
  } else {
    main.append(el("span", { class: "sh-empty" }, text("—")));
  }
  card.append(main);
  return card;
}

function unloadButton(category) {
  return el(
    "button",
    {
      class: "sh-mini",
      onclick: () =>
        postJson("/har/unload", { category }).catch((error) => flashError(error)),
    },
    text("Unload"),
  );
}

function categoryPort(status, category) {
  const entry = status.categories.find((cat) => cat.id === category);
  return entry ? `:${entry.public_port}` : "";
}

function renderMemory(status) {
  const mem = status.mem;
  const reservePct = (mem.reserve_gb / mem.total_gb) * 100;
  const observed = mem.observed_used_gb;
  const observedPct =
    observed === null || observed === undefined
      ? null
      : Math.min(100, (observed / mem.total_gb) * 100);
  const bars = [];
  // One bar, one truth: what the card itself reports. The ledger bar left with the
  // ledger (operator 2026-10-26) — nothing budgets against it any more, and two
  // bars showing two "VRAM used" numbers is how the 69.7-vs-1.8 GiB lie survived
  // a release. Hatched = the reserve band admission keeps clear; "booked" = loads
  // committed but not yet installed, which the card cannot have accounted for.
  if (observedPct !== null) {
    const attribution =
      mem.attributed_gb == null
        ? "per-process attribution unavailable"
        : "attributed " + mem.attributed_gb.toFixed(1) + " GiB";
    const booked =
      mem.booked_gb > 0 ? "; booked " + mem.booked_gb.toFixed(1) + " GiB in flight" : "";
    bars.push(
      el(
        "div",
        {},
        el(
          "label",
          {},
          text(
            `VRAM ${observed.toFixed(1)} / ${mem.total_gb.toFixed(0)} GiB (${mem.source}; ${attribution}${booked}; hatched = ${mem.reserve_gb.toFixed(1)} GiB reserve)`,
          ),
        ),
        el(
          "div",
          { class: "sh-bar", "data-tone": "observed" },
          el("i", { style: `width:${observedPct}%` }),
          el("u", {
            class: "sh-reserve",
            style: `left:${(100 - reservePct).toFixed(2)}%;width:${reservePct.toFixed(2)}%`,
          }),
        ),
      ),
    );
  }
  // A1: host RAM context (informational; admission budgets VRAM, never RAM). Omit
  // the whole bar when the platform cannot sample it, rather than show zeros.
  if (mem.ram_total_gb) {
    const ramPct = Math.min(100, (mem.ram_used_gb / mem.ram_total_gb) * 100);
    bars.push(
      el(
        "div",
        {},
        el(
          "label",
          {},
          text(
            `RAM ${mem.ram_used_gb.toFixed(1)} / ${mem.ram_total_gb.toFixed(0)} GiB (host, informational)`,
          ),
        ),
        el("div", { class: "sh-bar", "data-tone": "ram" }, el("i", { style: `width:${ramPct}%` })),
      ),
    );
  }
  return el("div", { class: "sh-mem" }, ...bars);
}

function renderHeader(status) {
  const loaded = status.slots.filter((slot) => slot.model).length;
  // Both verdicts are computed by the harness (`status_document` reads the registry's
  // own `any_down` / `pending`) and read here as booleans. The page used to decode
  // them from the serde shape of `listeners` — a second brain for a rule the server
  // owns, which pinned the badge green the moment a verdict changed representation or
  // a category went missing from the map (FINDINGS-16 [4]).
  const anyDown = status.listeners_down === true;
  const anyPending = status.listeners_pending === true;
  const headline = el(
    "div",
    { class: "sh-headline" },
    el("h1", { class: "sh-title" }, text("SimpleHar SE")),
    el(
      "span",
      { class: "sh-badge", "data-tone": loaded > 0 ? "ok" : "none" },
      text(loaded > 0 ? `${loaded} loaded` : "idle"),
    ),
    anyDown
      ? el("span", { class: "sh-badge", "data-tone": "bad" }, text("listener down"))
      : anyPending
        ? el("span", { class: "sh-badge", "data-tone": "warn" }, text("listener pending"))
        : null,
  );
  const endpoints = el(
    "div",
    { class: "sh-endpoints" },
    ...status.categories.map((cat) =>
      el(
        "a",
        { href: `http://${window.location.hostname}:${cat.public_port}/v1/models` },
        text(`${cat.id} :${cat.public_port}${cat.control ? " (control)" : ""}`),
      ),
    ),
    el(
      "button",
      {
        class: "sh-mini outline",
        onclick: () => postJson("/har/rescan", {}).then(refresh).catch((error) => flashError(error)),
      },
      text("Rescan"),
    ),
    el(
      "label",
      { class: "sh-extlog" },
      el("input", {
        type: "checkbox",
        "aria-label": "extended logging",
        ...(status.extended_logging ? { checked: "" } : {}),
        onchange: (event) =>
          postJson("/har/config/logging", { enabled: event.target.checked }).catch(
            (error) => flashError(error),
          ),
      }),
      text("Extended Logging"),
    ),
  );
  const slots = el(
    "div",
    { class: "sh-slots" },
    ...CATEGORY_ORDER.map((category) => renderSlotCard(status, category)),
  );
  const errors = (status.catalog_errors || []).map(
    (error) => `${error.file}: ${error.reason}`,
  );
  return el(
    "div",
    {},
    headline,
    endpoints,
    slots,
    renderMemory(status),
    status.last_error
      ? el("div", { class: "sh-errline", role: "alert" }, text(`last fault: ${status.last_error}`))
      : null,
    errors.length > 0
      ? el("div", { class: "sh-catalog-errors" }, text(errors.join(" · ")))
      : null,
  );
}

function modelRow(status, model, category) {
  const serving = status.slots.some(
    (slot) => slot.category === category && slot.model === model.id,
  );
  const loading = slotJobsFor(status, category).some((job) => job.model === model.id);
  // The dot follows the newest job for this model only: a fail that a later
  // load+unload superseded must not keep the row red (operator 2026-10-26).
  const last = status.jobs.filter((job) => job.model === model.id).at(-1);
  // The server owns the fault rule (`fault_of`: failed LOAD, cancels excluded) so the
  // row dot and the headline can never disagree (review, 2026-10-26).
  const failed = !!last && last.is_fault === true;
  const dotState = serving ? "serving" : loading ? "loading" : failed ? "failed" : "idle";
  // Axis 2 (operator ruling 2026-10-29): the amber is the server's own boolean read
  // directly — the page never compares file times or dates (the K-round
  // page-must-not-derive rule). It rides an attribute, not `dotState`, so the
  // Unload/Load button logic stays exactly the serving-row logic.
  const stale = serving && model.stale_config === true;
  const row = el("tr", {});
  // Operator layout (2026-10-26): actions first, then the name, then the rest —
  // the buttons stay in view at any width without scrolling. The primary button
  // is state-aware: a serving row offers Unload, a loading row is parked.
  const actions = el("td", { class: "sh-row-actions" });
  if (dotState === "serving") {
    actions.append(
      el(
        "button",
        {
          class: "sh-mini",
          onclick: () =>
            postJson("/har/unload", { model: model.id }).catch((error) => flashError(error)),
        },
        text("Unload"),
      ),
    );
  } else if (dotState === "loading") {
    actions.append(el("button", { class: "sh-mini", disabled: "" }, text("Loading…")));
  } else {
    actions.append(
      el(
        "button",
        {
          class: "sh-mini",
          onclick: () =>
            postJson("/har/load", { model: model.id }).catch((error) => flashError(error)),
        },
        text("Load"),
      ),
    );
  }
  actions.append(
    el(
      "button",
      {
        class: "sh-mini",
        // A refused default (the 400 for a model from another category) must reach
        // the flash, and the tab's `default: …` settles only by re-reading the
        // server: no `har_state` push follows a config-only write (FINDINGS-16 [3]).
        // Same shape as the seven other mutating handlers.
        onclick: () =>
          postJson("/har/config/default", { category, model: model.id })
            .then(refresh)
            .catch((error) => {
              flashError(error);
              refresh();
            }),
      },
      text("Set default"),
    ),
  );
  row.append(actions);
  row.append(
    el(
      "td",
      { class: "sh-name" },
      el(
        "span",
        {
          class: "sh-dot",
          "data-state": dotState,
          ...(stale
            ? {
                "data-stale": "amber",
                title: "config file changed since launch \u2014 unload + reload to apply",
              }
            : {}),
        },
      ),
      el("strong", {}, text(model.id)),
      // The launch stamp the server formatted (Axis 2): printed verbatim, no date
      // math on the page. Absent = not loaded, and nothing renders.
      ...(model.launched_at ? [text(` \u00b7 launched ${model.launched_at}`)] : []),
    ),
  );

  // Category select: page edits write back into the model yaml (Step 9).
  const select = el("select", { class: "sh-mini", "aria-label": "category" });
  for (const option of CATEGORY_ORDER) {
    select.append(
      el("option", { value: option, ...(option === model.category ? { selected: "" } : {}) }, text(option)),
    );
  }
  select.addEventListener("change", () =>
    postJson("/har/config/model", { model: model.id, category: select.value })
      .then(refresh)
      .catch((error) => {
        flashError(error);
        refresh();
      }),
  );
  row.append(el("td", {}, select));

  // CPU checkbox: forces -ngl 0 for llama engines; write-back is the yaml.
  // Only the engines whose adapter reads the key get the control: a click on a
  // strata/ninfer row would mint a dead yaml line and change no argv word, so
  // those rows show a dash (scan reports `inert_key` if the file carries it).
  if (OFFLOAD_ENGINES.includes(model.engine)) {
    const cpu = el("input", { type: "checkbox", "aria-label": "cpu offload" });
    cpu.checked = model.cpu_offload === true;
    cpu.addEventListener("change", () =>
      postJson("/har/config/model", { model: model.id, cpu: cpu.checked })
        .then(refresh)
        .catch((error) => {
          flashError(error);
          refresh();
        }),
    );
    row.append(el("td", {}, cpu));
  } else {
    row.append(el("td", {}, text("—")));
  }

  row.append(el("td", {}, text(model.engine)));
  row.append(el("td", {}, text(model.context ?? "—")));
  row.append(
    el(
      "td",
      {},
      text(
        // Cost honesty (operator 2026-10-26): a greedy model's nominal size is a
        // model-size claim, not a VRAM footprint — measured or floor, never 55.
        // The measured figure is a range (§0.4): the low end is where the session
        // started, the high end is what admission books, so showing only one of
        // them would hide how much of the card the KV cache can take.
        model.measured_gb !== null && model.measured_gb !== undefined
          ? `measured ${
              model.measured_min_gb !== null &&
              model.measured_min_gb !== undefined &&
              model.measured_min_gb < model.measured_gb
                ? model.measured_min_gb.toFixed(1) + "-" + model.measured_gb.toFixed(1)
                : model.measured_gb.toFixed(1)
            } GiB${model.cost === "greedy" ? " (greedy)" : ""}`
          : model.cost === "greedy"
            ? model.launch_floor_gb !== null && model.launch_floor_gb !== undefined
              ? `greedy · floor ${model.launch_floor_gb} GiB`
              : "greedy"
            : model.vram_gb !== null
              ? `${model.vram_gb} GiB`
              : model.est_vram_gb !== null && model.est_vram_gb !== undefined
                ? `${model.est_vram_gb.toFixed(1)} GiB (est.)`
                : "est.",
      ),
    ),
  );
  // A5 (v3.3 parity): the backing GGUF + serve_model id, basename on screen,
  // full path in the tooltip. Least-important column, so it sits rightmost.
  const fileBits = [];
  if (model.model_ref) {
    const base = String(model.model_ref).replace(/\\/g, "/").split("/").pop();
    fileBits.push(base);
  }
  if (model.serve_model) fileBits.push(`id:${model.serve_model}`);
  row.append(
    el(
      "td",
      { class: "sh-files", title: model.model_ref || "" },
      text(fileBits.length > 0 ? fileBits.join(" · ") : "—"),
    ),
  );

  return row;
}

function renderTab(status, category) {
  const entry = status.categories.find((cat) => cat.id === category);
  const models = state.models.filter((model) => model.category === category);
  const panel = el("div", { class: "sh-panel-head" });
  panel.append(text(`listener :${entry ? entry.public_port : "?"} · `));
  panel.append(
    text(entry && entry.default_model ? `default: ${entry.default_model}` : "default: none"),
  );
  if (entry && entry.default_model) {
    panel.append(
      el(
        "button",
        {
          class: "sh-mini",
          // Refused (e.g. the row raced a retag from another tab) it must flash and
          // re-read, exactly like `Set default` (FINDINGS-16 [3]).
          onclick: () =>
            postJson("/har/config/default", { category, model: null })
              .then(refresh)
              .catch((error) => {
                flashError(error);
                refresh();
              }),
        },
        text("Clear default"),
      ),
    );
  }
  const table = el(
    "table",
    { class: "sh-models" },
    el(
      "thead",
      {},
      el(
        "tr",
        {},
        el("th", {}),
        el("th", {}, text("Model")),
        el("th", {}, text("Cat")),
        el("th", {}, text("CPU")),
        el("th", {}, text("Engine")),
        el("th", {}, text("Ctx")),
        el("th", {}, text("Cost")),
        el("th", {}, text("Files")),
      ),
    ),
  );
  const body = el("tbody", {});
  if (models.length === 0) {
    body.append(
      el("tr", {}, el("td", { colspan: "8", class: "sh-empty" }, text("no models in this category — add a yaml to ModelConf and Rescan"))),
    );
  }
  for (const model of models) body.append(modelRow(status, model, category));
  table.append(body);
  // Small windows: the table scrolls inside its own wrapper — the page itself
  // never overflows and the action buttons stay reachable (review, 2026-10-26).
  return el("div", { class: "sh-tabpanel" }, panel, el("div", { class: "sh-tablewrap" }, table));
}

function renderTabs() {
  const tabs = el("div", { class: "sh-tabs", role: "tablist" });
  for (const category of CATEGORY_ORDER) {
    tabs.append(
      el("button", {
        class: "sh-tab",
        role: "tab",
        "aria-selected": String(state.activeTab === category),
        onclick: () => {
          state.activeTab = category;
          render();
        },
      }, text(category)),
    );
  }
  return tabs;
}

function logLine(entry) {
  return el(
    "div",
    { class: "sh-job" },
    text(`${new Date(entry.ts_ms).toLocaleTimeString()} ${entry.level} ${entry.message}`),
    entry.level === "ERROR" || entry.level === "WARN"
      ? el("span", { class: "sh-job-error" }, text(` ${entry.target}`))
      : null,
  );
}

async function toggleLog(details) {
  state.logOpen = details.open;
  if (!details.open || state.logLoaded) return;
  state.logLoaded = true;
  try {
    const data = await getJson("/har/log?limit=80");
    state.logEntries = data.events;
    const body = details.querySelector(".sh-log-body");
    if (body) body.replaceChildren(...state.logEntries.map(logLine));
  } catch (error) {
    state.logLoaded = false;
    flashError(error);
  }
}

function renderJobs(status) {
  const recent = status.jobs.slice(-5).reverse();
  if (recent.length === 0) return null;
  const wrap = el("div", { class: "sh-jobs" });
  wrap.append(el("h3", {}, text("Recent jobs")));
  for (const job of recent) {
    const line = el("div", { class: "sh-job" });
    line.append(text(`${job.id} · ${job.model} · ${job.state}`));
    if (job.error) line.append(el("span", { class: "sh-job-error" }, text(job.error)));
    wrap.append(line);
  }
  return wrap;
}

function flashError(error) {
  const box = document.getElementById("sh-flash");
  box.textContent = String(error.message || error);
  box.hidden = false;
  setTimeout(() => {
    box.hidden = true;
  }, 6000);
}

function render() {
  paintTokenControl();
  const status = state.status;
  if (!status) return;
  const headerHost = document.getElementById("sh-header");
  headerHost.replaceChildren(renderHeader(status));
  const mainHost = document.getElementById("sh-main");
  const jobs = renderJobs(status);
  const logBody = el("div", { class: "sh-log-body" });
  if (state.logEntries) logBody.replaceChildren(...state.logEntries.map(logLine));
  else logBody.append(text("open to load /har/log"));
  const logBox = el(
    "details",
    {
      class: "sh-jobs",
      open: state.logOpen ? "" : null,
      ontoggle: (event) => toggleLog(event.target),
    },
    el("summary", {}, text("Harness log (last events)")),
    logBody,
  );
  mainHost.replaceChildren(
    renderTabs(),
    renderTab(status, state.activeTab),
    ...(jobs ? [jobs] : []),
    logBox,
    el(
      "footer",
      { class: "sh-footer" },
      text("SimpleHar SE — slots, jobs and refusals straight from /har/*"),
    ),
  );
}

// ---------- event stream with polling fallback (Step 14) ----------

let pollTimer = null;
let sse = null;

function applyState(doc) {
  // The SSE document IS the full /har/status body plus `models` — one shape
  // everywhere, no per-panel re-derivation.
  state.status = doc;
  state.models = doc.models || [];
  render();
}

async function refresh() {
  // Polling fallback and the rescan button both use the plain endpoints. The
  // catches are split on purpose: a TypeError from the fetches means the browser
  // never reached the harness, while a render-time error is a contract bug worth
  // seeing raw (same handling as applyState on the SSE path).
  let status;
  let catalog;
  try {
    [status, catalog] = await Promise.all([getJson("/har/status"), getJson("/har/models")]);
  } catch (error) {
    flashError(
      error instanceof TypeError
        ? new Error(`cannot reach the harness at ${location.host} — is it running?`)
        : error,
    );
    // Nothing rendered on this path, so the header control repaints itself: it is the
    // operator's only way out of a token the harness just refused.
    paintTokenControl();
    return;
  }
  state.status = status;
  state.models = catalog.data;
  try {
    render();
  } catch (error) {
    flashError(error);
  }
}

function startPolling() {
  if (pollTimer !== null) return;
  pollTimer = setInterval(refresh, POLL_MS);
}

function stopPolling() {
  if (pollTimer === null) return;
  clearInterval(pollTimer);
  pollTimer = null;
}

function connectEvents() {
  stopPolling();
  if (controlToken()) {
    // `EventSource` cannot send headers, and the token must never travel in a URL
    // (it would be logged): a token-protected harness runs on polling alone.
    startPolling();
    return;
  }
  sse = new EventSource("/har/events");
  sse.addEventListener("har_state", (message) => {
    try {
      applyState(JSON.parse(message.data));
    } catch (error) {
      flashError(error);
    }
  });
  sse.onopen = () => stopPolling();
  sse.onerror = () => {
    // Stream dropped (server restart, sleep): degrade to polling, retry SSE later.
    sse.close();
    sse = null;
    startPolling();
    setTimeout(connectEvents, 10000);
  };
}

document.addEventListener("DOMContentLoaded", () => {
  initTheme();
  initTokenControl();
  refresh(); // first paint without waiting for the stream
  connectEvents();
});
