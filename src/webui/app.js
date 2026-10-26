// SimpleHar SE — control page logic (Step 14). Vanilla JS, no framework: the page
// only renders /har/* JSON; every action is an API call that also exists headless.
"use strict";

const CATEGORY_ORDER = ["WORK", "RP", "AUX", "EMB"];
const POLL_MS = 2000;

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

async function getJson(path) {
  const response = await fetch(path);
  if (!response.ok) {
    throw new Error(`${path} -> ${response.status}`);
  }
  return response.json();
}

async function postJson(path, body) {
  const response = await fetch(path, {
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
  const ledger = mem.ledger_gb || 0;
  const ledgerPct = Math.min(100, (ledger / mem.total_gb) * 100);
  const reservePct = (mem.reserve_gb / mem.total_gb) * 100;
  const observed = mem.observed_used_gb;
  const observedPct =
    observed === null || observed === undefined
      ? null
      : Math.min(100, (observed / mem.total_gb) * 100);
  const bars = [
    el(
      "div",
      {},
      el(
        "label",
        {},
        text(
          `VRAM ledger ${ledger.toFixed(1)} / ${mem.total_gb.toFixed(0)} GiB (what admission budgets; hatched = ${mem.reserve_gb.toFixed(1)} GiB reserve)`,
        ),
      ),
      el(
        "div",
        { class: "sh-bar" },
        el("i", { style: `width:${ledgerPct}%` }),
        el("u", {
          class: "sh-reserve",
          style: `left:${(100 - reservePct).toFixed(2)}%;width:${reservePct.toFixed(2)}%`,
        }),
      ),
    ),
  ];
  if (observedPct !== null) {
    bars.push(
      el(
        "div",
        {},
        el(
          "label",
          {},
          text(
            `Observed ${observed.toFixed(1)} / ${mem.total_gb.toFixed(0)} GiB (witness: ${mem.source}; external ${(mem.external_gb || 0).toFixed(1)} GiB)`,
          ),
        ),
        el("div", { class: "sh-bar", "data-tone": "observed" }, el("i", { style: `width:${observedPct}%` })),
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
  const anyDown = Object.values(status.listeners).some(
    (listener) => typeof listener === "object",
  );
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
  const failed = !!last && last.terminal && last.kind === "Load" && !!last.error;
  const dotState = serving ? "serving" : loading ? "loading" : failed ? "failed" : "idle";
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
        onclick: () =>
          postJson("/har/config/default", { category, model: model.id }).catch(() => {}),
      },
      text("Set default"),
    ),
  );
  row.append(actions);
  row.append(
    el(
      "td",
      { class: "sh-name" },
      el("span", { class: "sh-dot", "data-state": dotState }),
      el("strong", {}, text(model.id)),
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

  row.append(el("td", {}, text(model.engine)));
  row.append(el("td", {}, text(model.context ?? "—")));
  row.append(
    el(
      "td",
      {},
      text(
        // Cost honesty (operator 2026-10-26): a greedy model's nominal size is a
        // model-size claim, not a VRAM footprint — measured or floor, never 55.
        model.measured_gb !== null && model.measured_gb !== undefined
          ? `measured ${model.measured_gb.toFixed(1)} GiB${model.cost === "greedy" ? " (greedy)" : ""}`
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
          onclick: () =>
            postJson("/har/config/default", { category, model: null }).catch(() => {}),
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
  refresh(); // first paint without waiting for the stream
  connectEvents();
});
