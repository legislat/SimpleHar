// SimpleHarn — control page logic (Step 14). Vanilla JS, no framework: the page
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
  // What each container admitted it reserves (R9 post-ready triple), remembered per
  // model for the page session: a row can only be seen booked while it is loaded,
  // and the page forgets on reload. Display memory, never a booking input.
  lastReported: {},
};

// ---------- tiny helpers ----------

// Optional control token (server: `control_token` / `SIMPLEHARN_TOKEN`). Stored in
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

// The tab IS the slot card (operator compaction, 2026-11-01), and the whole pill is
// the click target (operator follow-up, 2026-11-01): dot, category, port, occupant
// live INSIDE the tab button - a port rendered as a bare link read as chrome, not
// as the tab. Only the Unload sits outside the button, as a button cannot nest one.
function renderTabItem(status, category) {
  const entry = status.categories.find((cat) => cat.id === category);
  const slots = status.slots.filter((slot) => slot.category === category);
  const occupants = slots.map((slot) => slot.model).filter(Boolean);
  const loading = slotJobsFor(status, category).length > 0;
  // Local name kept clear of the module-level UI `state` object (shadowing here
  // would send the onclick handler reading a string).
  // The server owns `slots[].state` (ruling 2026-11-04): a committed release
  // shows `releasing` while the occupant is still installed. No `releasing`
  // slot (and no `state` field on an old payload) leaves today's derivation
  // untouched.
  const releasing = slots.some((slot) => slot.state === "releasing");
  // The pill text names the first occupant OUTSIDE the releasing slots
  // (review B3R-02, 2026-11-04): a slot releasing beside a serving one still
  // names the serving model, and the releasing dot carries the in-flight
  // release. A payload without `state` keeps every occupied slot here, so an
  // older harness renders exactly as before.
  const kept = slots
    .filter((slot) => slot.state !== "releasing")
    .map((slot) => slot.model)
    .filter(Boolean);
  const dotState = releasing
    ? "releasing"
    : occupants.length > 0
      ? "serving"
      : loading
        ? "loading"
        : "idle";
  const button = el(
    "button",
    {
      class: occupants.length > 0 ? "sh-tab sh-tab-part" : "sh-tab",
      role: "tab",
      "aria-selected": String(state.activeTab === category),
      title: `${category} listener :${entry ? entry.public_port : "?"}` +
        (occupants.length > 0 ? ` - ${occupants.join(", ")}` : ""),
      onclick: () => {
        state.activeTab = category;
        render();
      },
    },
    el("span", { class: "sh-dot", "data-state": dotState }),
    text(category),
  );
  if (entry) button.append(el("span", { class: "sh-tab-port" }, text(`:${entry.public_port}`)));
  if (releasing && kept.length === 0) {
    button.append(el("span", { class: "sh-tab-model sh-empty" }, text("releasing…")));
  } else if (loading && occupants.length === 0) {
    button.append(el("span", { class: "sh-tab-model sh-empty" }, text("loading…")));
  } else if (kept.length > 0) {
    button.append(el("span", { class: "sh-tab-model" }, text(kept[0])));
    if (kept.length > 1) button.append(text(` +${kept.length - 1}`));
  }
  const item = el("span", { class: "sh-tab-item" }, button);
  if (occupants.length > 0) {
    // The Unload is the tab's second segment (operator 2026-11-01): flush against
    // the pill, shared border - two parts of one entity, not a stray button.
    item.append(
      el(
        "button",
        {
          class: "sh-mini sh-tab-side",
          title: `unload every ${category} slot`,
          onclick: () =>
            postJson("/har/unload", { category }).catch((error) => flashError(error)),
        },
        text("Unload"),
      ),
    );
  }
  return item;
}

// Polling quiet (operator 2026-11-01): the NVML and host-RAM samples move every
// tick, but a memory-only tick is not a page change. Repainting the whole page on
// it cancelled the operator's scroll and text selection mid-read ("polling noise"),
// so an unchanged structural document is now PATCHED in place - the bars get their
// new numbers through the painters registered by the last full render, and no
// other node is touched. A real change still repaints, with the scroll restored.
let volatilePainters = [];
let lastRenderKey = "";

// The render gate's comparison key: the whole document EXCEPT the fields that
// only ever drift between samples (witness/RAM readings, slot idle seconds).
function stateKey(status, models) {
  if (!status) return "";
  const masked = { ...status, mem: { ...(status.mem || {}) }, models: null };
  // Mask the drifting values but KEEP their presence: a bar appearing or vanishing
  // (the card becomes sampleable, RAM stops being) is structural and must repaint.
  masked.mem.observed_used_gb =
    status.mem && status.mem.observed_used_gb == null ? null : 1;
  masked.mem.ram_used_gb =
    status.mem && status.mem.ram_used_gb == null ? null : 1;
  // attributed_gb drifts every sample beside observed_used_gb wherever per-process
  // attribution works (review F1): same volatile class, same presence-masking.
  masked.mem.attributed_gb =
    status.mem && status.mem.attributed_gb == null ? null : 1;
  if (Array.isArray(masked.slots)) {
    masked.slots = masked.slots.map((slot) => (slot ? { ...slot, idle_s: null } : slot));
  }
  return JSON.stringify(masked) + "|" + JSON.stringify(models || []);
}

// Applies a memory-only tick to the already-painted bars (after a full render
// registered the painters; before or without them, nothing is painted).
function patchVolatile(status) {
  const mem = status && status.mem;
  if (!mem) return;
  for (const paint of volatilePainters) paint(mem);
}

function renderMemory(status) {
  const mem = status.mem;
  const bars = [];
  volatilePainters = [];
  // One bar, one truth: what the card itself reports. The ledger bar left with the
  // ledger (operator 2026-10-26) — nothing budgets against it any more, and two
  // bars showing two "VRAM used" numbers is how the 69.7-vs-1.8 GiB lie survived
  // a release. Hatched = the reserve band admission keeps clear; "booked" = loads
  // committed but not yet installed, which the card cannot have accounted for.
  if (mem.observed_used_gb !== null && mem.observed_used_gb !== undefined) {
    // R1: the witness states its own attribution capability; the page prefers that
    // verdict over inferring it from a number happening to be absent (an idle card is
    // capable, a WDDM-degraded card is not — both show no attributed bytes). An
    // token outside the closed set is the UNAVAILABLE sentence, never a crash: the
    // page must degrade one label, not repaint nothing (review 2026-10-31).
    const observedOf = (m) => Math.min(100, (m.observed_used_gb / m.total_gb) * 100);
    const reservePct = (mem.reserve_gb / mem.total_gb) * 100;
    const label = el("label", {});
    const fill = el("i", {});
    // Compaction (operator 2026-11-01): the bar label carries only the number the
    // operator reads; the honesty sentence - source, attribution, booking, reserve
    // hatch - moved to the tooltip, same words, still one VRAM bar and never a
    // fabricated zero. The same paint() words serve the first render and every
    // memory-only patch, so the two can never disagree.
    const paint = (m) => {
      const attribution =
        m.attribution === "unavailable" ||
        (m.attribution == null && m.attributed_gb == null) ||
        (m.attribution !== "per_process" && m.attributed_gb == null)
          ? "per-process attribution unavailable"
          : m.attribution === "per_process" && m.attributed_gb == null
            ? "per-process capable (card idle)"
            : "attributed " + Number(m.attributed_gb).toFixed(1) + " GiB";
      const booked =
        m.booked_gb > 0 ? "; booked " + m.booked_gb.toFixed(1) + " GiB in flight" : "";
      const reported =
        m.container_reported_gb != null && m.container_reported_gb > 0
          ? "; containers report " + m.container_reported_gb.toFixed(1) + " GiB reserved"
          : "";
      label.setAttribute(
        "title",
        `VRAM ${m.observed_used_gb.toFixed(1)} / ${m.total_gb.toFixed(0)} GiB (${m.source}; ${attribution}${booked}${reported}; hatched = ${m.reserve_gb.toFixed(1)} GiB reserve)`,
      );
      label.replaceChildren(
        text(`VRAM ${m.observed_used_gb.toFixed(1)} / ${m.total_gb.toFixed(0)} GiB`),
      );
      fill.setAttribute("style", `width:${observedOf(m)}%`);
    };
    paint(mem);
    volatilePainters.push(paint);
    bars.push(
      el(
        "div",
        {},
        label,
        el(
          "div",
          { class: "sh-bar", "data-tone": "observed" },
          fill,
          el("u", {
            class: "sh-reserve",
            style: `left:${(100 - reservePct).toFixed(2)}%;width:${reservePct.toFixed(2)}%`,
          }),
        ),
      ),
    );
  }
  // A1: host RAM context (informational; admission budgets VRAM, never RAM). Omit
  // the whole bar when the platform cannot sample it, rather than show zeros. The
  // used number gates too: a total without a reading is not a bar (review F5).
  if (mem.ram_total_gb && mem.ram_used_gb != null) {
    const ramLabel = el("label", {});
    const ramFill = el("i", {});
    const paintRam = (m) => {
      ramLabel.setAttribute(
        "title",
        `RAM ${m.ram_used_gb.toFixed(1)} / ${m.ram_total_gb.toFixed(0)} GiB (host, informational)`,
      );
      ramLabel.replaceChildren(
        text(`RAM ${m.ram_used_gb.toFixed(1)} / ${m.ram_total_gb.toFixed(0)} GiB`),
      );
      ramFill.setAttribute(
        "style",
        `width:${Math.min(100, (m.ram_used_gb / m.ram_total_gb) * 100)}%`,
      );
    };
    paintRam(mem);
    volatilePainters.push(paintRam);
    bars.push(
      el("div", {}, ramLabel, el("div", { class: "sh-bar", "data-tone": "ram" }, ramFill)),
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
    el("h1", { class: "sh-title" }, text("SimpleHarn")),
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
    // R5: reservations the harness counts but no live container renews. The
    // verdict is the harness's (`status.stranded_containers`, absent when
    // clean); the page only names who is missing — never a number it invented.
    (status.stranded_containers || []).length > 0
      ? el(
          "span",
          { class: "sh-badge", "data-tone": "warn" },
          text(`stranded: ${status.stranded_containers.join(", ")}`),
        )
      : null,
  );
  const controls = el("div", { class: "sh-controls" });
  for (const category of CATEGORY_ORDER) controls.append(renderTabItem(status, category));
  controls.append(
    el("span", { class: "sh-tab-spacer" }),
    el(
      "button",
      {
        class: "sh-mini",
        // `{}` is the explicit unload-all scope (T12.4) - never an implicit one.
        onclick: () =>
          postJson("/har/unload", {})
            .then(refresh)
            .catch((error) => flashError(error)),
      },
      text("Unload all"),
    ),
    el(
      "button",
      {
        class: "sh-mini outline",
        onclick: () => postJson("/har/rescan", {}).then(refresh).catch((error) => flashError(error)),
      },
      text("Rescan"),
    ),
  );
  // Operator order (2026-11-01): numbers first, tabs under them; the Extended
  // Logging toggle belongs with the harness log at the page bottom, not the strip.
  return el("div", {}, headline, renderMemory(status), controls);
}

// Catalog hygiene notices (inert keys, policy refusals): a long wall of them must
// never push the table down, so they live at the PAGE BOTTOM under the harness log,
// not in the header (operator 2026-11-02: "lengthy error message is still not moved
// to the bottom").
function catalogNotices(status) {
  const errors = (status.catalog_errors || []).map(
    (error) => `${error.file}: ${error.reason}`,
  );
  return errors.length > 0
    ? el("div", { class: "sh-catalog-errors" }, text(errors.join("\n")))
    : null;
}

// The loaded slot whose container reports its reservation (R9 display chain), or
// null. The slot itself travels, not just its figure: the armed `need_gib` is
// the gate's side of the same booking, so a cell can name a divergence (F3).
function slotReportedGb(status, category, modelId) {
  return (
    status.slots.find(
      (s) => s.category === category && s.model === modelId && s.container_reported_gb != null,
    ) ?? null
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
  // The server owns `slots[].state` (ruling 2026-11-04): the dot follows a
  // committed release named on this model's slot. A payload without `state`
  // (an older harness) keeps today's derivation.
  const releasing = status.slots.some(
    (slot) => slot.category === category && slot.model === model.id && slot.state === "releasing",
  );
  const dotState = releasing
    ? "releasing"
    : serving
      ? "serving"
      : loading
        ? "loading"
        : failed
          ? "failed"
          : "idle";
  // Axis 2 (operator ruling 2026-10-29): the amber is the server's own boolean read
  // directly — the page never compares file times or dates (the K-round
  // page-must-not-derive rule). It rides an attribute, not `dotState`, so the
  // Unload/Load/Releasing buttons stay the plain serving-row logic.
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
  } else if (dotState === "releasing") {
    // The release is already committed: a second gesture would only race it,
    // so the row parks on its honest word until the driver confirms.
    actions.append(el("button", { class: "sh-mini", disabled: "" }, text("Releasing…")));
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
  // Cost honesty, server-decided (R9): the gate says via `cost_basis` which figure
  // it books. GREEN cells show that booked figure verbatim; YELLOW cells show
  // context, not a booking — declared or estimate sizes, and for greedy the launch
  // floor (the gate books greedy by nominal/estimate, never the floor). Provenance
  // is colour, not prose (operator 2026-11-01); the full sentence lives in the
  // tooltip. A row the page saw loaded keeps that session memory in the tooltip.
  const reportedSlot = serving ? slotReportedGb(status, category, model.id) : null;
  const slotReported = reportedSlot ? reportedSlot.container_reported_gb : null;
  let costText;
  let costChecked = false;
  let costTitle;
  if (slotReported) {
    costText = `${slotReported.toFixed(1)} GiB`;
    costChecked = true;
    costTitle = `engine-reported: this container reserves ${slotReported.toFixed(1)} GiB`;
  } else if (
    // The engine calculator's pre-spawn projection: amber until this session's
    // launch cross-checks the live booking against it (R11), then green — the
    // same colour-is-provenance rule as every other cell, and the tooltip says
    // whose number it is because a projection is not a measurement.
    model.cost_basis === "fit" &&
    model.fit_gb !== null &&
    model.fit_gb !== undefined
  ) {
    costText = `${model.fit_gb.toFixed(1)} GiB`;
    // Green promotion is the server's verdict, never a client-side compare:
    // fit_validated only rides a live witness reading that landed inside
    // fit_margin_gb + the pinned session slack (FIT_VALIDATE_SLACK_GIB,
    // src/scheduler/mod.rs).
    costChecked = model.fit_validated === true;
    costTitle =
      `engine-projected ${model.fit_gb.toFixed(1)} GiB · engine calculator, pre-spawn, re-verified per launch` +
      (costChecked ? " · this session's launch confirmed it" : "");
  } else if (
    model.cost_basis === "measured" &&
    model.measured_gb !== null &&
    model.measured_gb !== undefined
  ) {
    const range =
      model.measured_min_gb !== null &&
      model.measured_min_gb !== undefined &&
      model.measured_min_gb < model.measured_gb
        ? model.measured_min_gb.toFixed(1) + "-" + model.measured_gb.toFixed(1)
        : model.measured_gb.toFixed(1);
    costText = `${range} GiB`;
    costChecked = true;
    // The tag marks the engine's own word: the container reservation
    // (`engine_report`) and the process-engine status figure (`engine_status`)
    // share it; a witness `per_process` sum is not the engine speaking.
    costTitle = `measured ${range} GiB · the gate books it` +
      (model.measured_attribution === "engine_report" ||
      model.measured_attribution === "engine_status"
        ? " (engine-reported)"
        : "") +
      (model.cost === "greedy" ? " (greedy)" : "");
  } else if (model.cost === "greedy") {
    costChecked = false;
    costText =
      model.launch_floor_gb !== null && model.launch_floor_gb !== undefined
        ? `${model.launch_floor_gb} GiB`
        : "greedy";
    costTitle =
      model.launch_floor_gb !== null && model.launch_floor_gb !== undefined
        ? `greedy: launch floor - dedicated-card intent ${model.launch_floor_gb} GiB, unchecked class`
        : "greedy: no floor declared, unchecked";
  } else if (model.vram_gb !== null) {
    costText = `${model.vram_gb} GiB`;
    costTitle = `declared ${model.vram_gb} GiB · unchecked until a launch measures it`;
  } else if (model.est_vram_gb !== null && model.est_vram_gb !== undefined) {
    costText = `${model.est_vram_gb.toFixed(1)} GiB`;
    costTitle = `GGUF estimate ${model.est_vram_gb.toFixed(1)} GiB · unchecked`;
  } else {
    costText = "—";
    costTitle = "nothing the gate can book yet";
  }
  // F3: when the container's self-report and the gate's armed booking disagree,
  // say both — the cell shows what the container claims while the gate still
  // books its own figure until a reload re-reads the truth. 0.05 GiB sits under
  // the 0.1 display quantum, so only real divergence earns the note.
  if (
    slotReported !== null &&
    reportedSlot.need_gib !== null &&
    reportedSlot.need_gib !== undefined &&
    Math.abs(reportedSlot.need_gib - slotReported) >= 0.05
  ) {
    costTitle += ` · booked ${reportedSlot.need_gib.toFixed(1)} GiB until reload`;
  }
  if (
    !slotReported &&
    !serving &&
    state.lastReported[model.id] !== undefined
  ) {
    costTitle += ` · reserved ${state.lastReported[model.id].toFixed(1)} GiB on last load`;
  }
  row.append(
    el(
      "td",
      {
        class: "sh-cost sh-cost-" + (costChecked ? "checked" : "unchecked"),
        title: costTitle,
      },
      text(costText),
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
  const models = state.models.filter((model) => model.category === category);
  const entry = status.categories.find((cat) => cat.id === category);
  // The default chip lives here, on the panel's own empty head row - it names the
  // tab below, not the strip above (operator 2026-11-01).
  const head = el(
    "div",
    { class: "sh-panel-head" },
    text(entry && entry.default_model ? `default: ${entry.default_model}` : "default: none"),
  );
  if (entry && entry.default_model) {
    head.append(
      el(
        "button",
        {
          class: "sh-mini sh-x",
          title: "clear the default",
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
        text("\u2715"),
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
  // The default chip sits on the panel's own head row (operator 2026-11-01).
  return el("div", { class: "sh-tabpanel" }, head, el("div", { class: "sh-tablewrap" }, table));
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
  // The last fault moved down here from the sticky header (operator 2026-11-01):
  // a stale fault is history, and history belongs beside the job ledger, not
  // above the live controls.
  if (recent.length === 0 && !status.last_error) return null;
  const wrap = el("div", { class: "sh-jobs" });
  wrap.append(el("h3", {}, text("Recent jobs")));
  if (status.last_error) {
    wrap.append(
      el("div", { class: "sh-errline", role: "alert" }, text(`last fault: ${status.last_error}`)),
    );
  }
  for (const job of recent) {
    const line = el("div", { class: "sh-job" });
    line.append(text(`${job.id} · ${job.model} · ${job.state}`));
    if (job.error) line.append(el("span", { class: "sh-job-error" }, text(job.error)));
    wrap.append(line);
  }
  return wrap;
}

// The degraded header for an unreachable harness (R2-16-03). A stale "N
// loaded" badge next to a "cannot reach the harness" alert is the contradiction
// this stamp removes: the header may not claim a live state the page just
// failed to read. It reuses the header row classes and admits only what the
// failure says. The sign-in/token control is a page button, painted by
// `paintTokenControl`, never a child of this host.
function renderUnreachableHeader(error) {
  return el(
    "div",
    {},
    el(
      "div",
      { class: "sh-headline" },
      el("h1", { class: "sh-title" }, text("SimpleHarn")),
      el(
        "span",
        { class: "sh-badge", "data-tone": "none" },
        text(error instanceof TypeError ? "harness unreachable" : "state not confirmed"),
      ),
    ),
  );
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
  // A full repaint replaces every node, so the operator's scroll position is
  // saved across it (text selection cannot survive a real content change and
  // does not need to; memory-only ticks never reach this function at all).
  const savedScroll = window.scrollY || 0;
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
  const notices = catalogNotices(status);
  mainHost.replaceChildren(
    renderTab(status, state.activeTab),
    ...(jobs ? [jobs] : []),
    el(
      "div",
      { class: "sh-logrow" },
      logBox,
      el(
        "label",
        { class: "sh-extlog" },
        el("input", {
          type: "checkbox",
          "aria-label": "extended logging",
          ...(status.extended_logging ? { checked: "" } : {}),
          // A refused logging write must flash AND re-read (R2-16-02): no
          // `har_state` push follows a config-only write (FINDINGS-16 [3]), so
          // only the re-read can return the checkbox to the server's
          // `extended_logging`. Same shape as the six other mutating controls.
          onchange: (event) =>
            postJson("/har/config/logging", { enabled: event.target.checked })
              .catch((error) => {
                flashError(error);
                refresh();
              }),
        }),
        text("Extended Logging"),
      ),
    ),
    ...(notices ? [notices] : []),
    el(
      "footer",
      { class: "sh-footer" },
      text("SimpleHarn — slots, jobs and refusals straight from /har/*"),
    ),
  );
  window.scrollTo(0, savedScroll);
  lastRenderKey = stateKey(status, state.models);
}

// ---------- event stream with polling fallback (Step 14) ----------

let pollTimer = null;
let sse = null;

// Remember every live `container_reported_gb` seen, keyed by model: the truth a
// container speaks exists only while its slot lives, so the page holds it for the
// rest of the session. Recording is display-only, never a gate input.
function recordReportedFootprints(doc) {
  if (!doc || !Array.isArray(doc.slots)) return;
  for (const slot of doc.slots) {
    if (slot && slot.model && slot.container_reported_gb != null) {
      state.lastReported[slot.model] = slot.container_reported_gb;
    }
  }
}

function applyState(doc) {
  // The SSE document IS the full /har/status body plus `models` — one shape
  // everywhere, no per-panel re-derivation.
  state.status = doc;
  state.models = doc.models || [];
  recordReportedFootprints(doc);
  if (stateKey(state.status, state.models) === lastRenderKey) patchVolatile(state.status);
  else render();
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
    // R2-16-03: the header may not keep claiming "N loaded"/serving state it
    // just failed to confirm — stamp it degraded. `state` keeps its last read
    // (a later success must still diff against it), but the render gate is
    // voided so the next success repaints in full even on an unchanged key.
    const headerHost = document.getElementById("sh-header");
    if (headerHost) headerHost.replaceChildren(renderUnreachableHeader(error));
    lastRenderKey = null;
    return;
  }
  state.status = status;
  state.models = catalog.data;
  recordReportedFootprints(status);
  try {
    if (stateKey(state.status, state.models) === lastRenderKey) patchVolatile(state.status);
    else render();
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
