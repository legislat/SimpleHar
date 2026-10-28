import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

// Self-locating: the fixtures ship beside this file and the bundle under test is the
// repo's own src/webui/app.js. Both stay overridable:
//   node webui_dom_test.mjs [fixturesDir] [appJs]
const HERE = dirname(fileURLToPath(import.meta.url));
const DIR = process.argv[2] || join(HERE, "fixtures");
const APP = process.argv[3] || join(HERE, "..", "..", "src", "webui", "app.js");
const fixtures = {
  "/har/status": JSON.parse(readFileSync(`${DIR}/status.json`, "utf8")),
  "/har/models": JSON.parse(readFileSync(`${DIR}/models.json`, "utf8")),
  "/har/log?limit=80": JSON.parse(readFileSync(`${DIR}/log.json`, "utf8")),
};

class El {
  constructor(tag) {
    this.tag = tag;
    this.attrs = {};
    this.children = [];
    this.listeners = {};
  }
  set className(v) { this.attrs.class = v; }
  get className() { return this.attrs.class; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return this.attrs[k] ?? null; }
  append(...kids) { this.children.push(...kids); }
  replaceChildren(...kids) { this.children = kids; }
  addEventListener(type, fn) { (this.listeners[type] ??= []).push(fn); }
  dispatch(type, event) { for (const fn of this.listeners[type] ?? []) fn(event); }
  querySelector(sel) {
    const want = sel.replace(/^\./, "");
    const walk = (n) => {
      for (const c of n.children) {
        if (c instanceof El) {
          if (c.attrs.class === want) return c;
          const found = walk(c);
          if (found) return found;
        }
      }
      return null;
    };
    return walk(this);
  }
}

class Tx { constructor(t) { this.__text = String(t); } }

const byId = new Map();
globalThis.document = {
  createElement: (tag) => new El(tag),
  createTextNode: (t) => new Tx(t),
  getElementById: (id) => {
    if (!byId.has(id)) byId.set(id, new El(`#${id}`));
    return byId.get(id);
  },
  documentElement: new El("html"),
  addEventListener: () => {},
};
// The driver below is eval'd in GLOBAL scope, so every switch the legs flip lives on
// globalThis (a module-level `let` would be invisible to it, and a stale local copy
// would let a leg "pass" without ever reaching the code under test).
// A real key/value store, so the token legs can prove what the page removes. It is
// installed as BOTH `localStorage` (what the theme code reads) and
// `window.localStorage` (what the token code reads) — in a browser they are the same
// object, and app.js would otherwise throw into its own catch and never exercise the
// token path at all.
globalThis.store = new Map();
globalThis.removedKeys = [];
globalThis.promptSawKeys = null;
const storage = {
  getItem: (k) => (globalThis.store.has(k) ? globalThis.store.get(k) : null),
  setItem: (k, v) => { globalThis.store.set(k, String(v)); },
  removeItem: (k) => { globalThis.removedKeys.push(k); globalThis.store.delete(k); },
};

globalThis.window = {
  location: { hostname: "127.0.0.1" },
  matchMedia: () => ({ matches: false }),
  localStorage: storage,
  prompt: (message) => {
    globalThis.promptLog.push(message);
    globalThis.promptSawKeys = [...globalThis.store.keys()].join(",");
    return globalThis.promptAnswers.length > 0 ? globalThis.promptAnswers.shift() : null;
  },
};
globalThis.localStorage = storage;

// Harness-side switches for the token and refusal legs: `requireToken` makes every
// /har/* answer 401 unless the header carries that value, `failNextPost` makes POSTs
// answer 400 with the server's refusal message, and `statusFetches` proves that a
// handler re-read the server. `sentTokens` records the `x-har-token` the requests
// ACTUALLY carried (every request, gated or not): a leg that only counts prompts can
// satisfy its assertion with a request that never reached the storage path at all, so
// the token legs must read the wire, not just the dialog.
globalThis.requireToken = null;
globalThis.failNextPost = null;
globalThis.statusFetches = 0;
globalThis.promptLog = [];
globalThis.promptAnswers = [];
globalThis.sentTokens = [];

globalThis.fetch = async (path, init) => {
  if (path === "/har/status") globalThis.statusFetches += 1;
  const sent = (init && init.headers && init.headers["x-har-token"]) || "";
  globalThis.sentTokens.push(sent);
  if (globalThis.requireToken !== null) {
    if (sent !== globalThis.requireToken) {
      return { ok: false, status: 401, json: async () => ({ error: { code: "unauthorized" } }) };
    }
  }
  if (init && init.method === "POST") {
    if (globalThis.failNextPost !== null) {
      const message = globalThis.failNextPost;
      return { ok: false, status: 400, json: async () => ({ message }) };
    }
    return { ok: true, status: 200, json: async () => ({ ok: true }) };
  }
  const doc = fixtures[path];
  if (!doc) throw new Error(`unexpected fetch: ${path}`);
  return { ok: true, status: 200, json: async () => doc };
};
globalThis.EventSource = class { addEventListener() {} close() {} };

function textOf(node) {
  if (node instanceof Tx) return node.__text;
  if (node instanceof El) return node.children.map(textOf).join("");
  return "";
}
function findAll(root, tag, cls) {
  const out = [];
  const walk = (n) => {
    for (const c of n.children ?? []) {
      if (c instanceof El) {
        if (c.tag === tag && (cls === undefined || c.attrs.class === cls)) out.push(c);
        walk(c);
      }
    }
  };
  walk(root);
  return out;
}
globalThis.textOf = textOf;
globalThis.findAll = findAll;
globalThis.fixtures = fixtures;

const appSrc = readFileSync(APP, "utf8");
const driver = `
;(async () => {
  const results = [];
  const check = (name, ok, extra) => results.push([name, !!ok, extra ?? ""]);
  await refresh();
  const main = document.getElementById("sh-main");
  const tabs = findAll(main, "button", "sh-tab");
  check("four tabs rendered", tabs.length === 4, "got " + tabs.length);
  check("tab labels not empty",
    tabs.map(textOf).join(",") === "WORK,RP,AUX,EMB",
    "labels=[" + tabs.map(textOf).join("|") + "]");
  const flash = document.getElementById("sh-flash");
  check("no render error flashed", !flash.textContent && flash.hidden !== false, flash.textContent);

  const details = findAll(main, "details")[0];
  details.open = true;
  details.dispatch("toggle", { target: details });
  await new Promise((r) => setTimeout(r, 0));
  await new Promise((r) => setTimeout(r, 0));
  check("logOpen state true", state.logOpen === true);
  const nlog = fixtures["/har/log?limit=80"].events.length;
  check("log fetched into state", Array.isArray(state.logEntries) && state.logEntries.length === nlog,
    JSON.stringify(state.logEntries?.length));
  const lines = findAll(details, "div", "sh-job");
  check("log body filled", lines.length === nlog, "got " + lines.length);

  render();
  const d2 = findAll(document.getElementById("sh-main"), "details")[0];
  check("details survives re-render open", d2.attrs.open === "", "attrs=" + JSON.stringify(d2.attrs));
  check("re-rendered log still filled", findAll(d2, "div", "sh-job").length === nlog);

  d2.open = false;
  d2.dispatch("toggle", { target: d2 });
  render();
  const d3 = findAll(document.getElementById("sh-main"), "details")[0];
  check("closed stays closed", d3.attrs.open === undefined && state.logOpen === false);

  const rpTab = findAll(document.getElementById("sh-main"), "button", "sh-tab")[1];
  rpTab.dispatch("click", {});
  check("tab click switches", state.activeTab === "RP");
  const head = findAll(document.getElementById("sh-main"), "div", "sh-panel-head")[0];
  check("panel shows RP port", textOf(head).includes(":8709"), textOf(head));

  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  check("SSE full-doc render ok", !document.getElementById("sh-flash").textContent);

  // Memory-bar honesty (2026-10-26 review + ledger demotion): one bar, the card's
  // own reading, labeled with its source; no ledger bar, no fabricated zero.
  const barLabels = findAll(document.getElementById("sh-header"), "label").map(textOf);
  const vramBars = barLabels.filter(function (t) {
    return t.indexOf("VRAM ") === 0;
  });
  const vramLabel = vramBars[0] || "";
  const mem = fixtures["/har/status"].mem;
  check("exactly one VRAM bar", vramBars.length === 1, barLabels.join(" || "));
  check("ledger bar is gone",
    barLabels.every(function (t) {
      return t.indexOf("ledger") < 0 && t.indexOf("what admission budgets") < 0;
    }),
    barLabels.join(" || "));
  check("observed bar rendered", vramLabel.length > 0, barLabels.join(" || "));
  check("vram bar names its source", vramLabel.indexOf("(" + mem.source + ";") >= 0, vramLabel);
  check("vram bar states attribution",
    mem.attributed_gb == null
      ? vramLabel.indexOf("per-process attribution unavailable") >= 0
      : vramLabel.indexOf("attributed " + mem.attributed_gb.toFixed(1) + " GiB") >= 0,
    vramLabel);
  check("vram bar never claims external", vramLabel.indexOf("external") < 0, vramLabel);
  check("vram bar shows the in-flight booking",
    mem.booked_gb > 0
      ? vramLabel.indexOf("booked " + mem.booked_gb.toFixed(1) + " GiB in flight") >= 0
      : vramLabel.indexOf("booked") < 0,
    vramLabel);
  check("reserve hatch is on the VRAM bar",
    vramLabel.indexOf("hatched = ") >= 0 &&
      findAll(document.getElementById("sh-header"), "u", "sh-reserve").length === 1,
    vramLabel);

  // A5: Model Engine Ctx Cost Files Cat CPU actions = 8 columns.
  const workModels = fixtures["/har/models"].data.filter((m) => m.category === "WORK");
  const workTab = findAll(document.getElementById("sh-main"), "button", "sh-tab")[0];
  workTab.dispatch("click", {});
  const rows = findAll(document.getElementById("sh-main"), "tr");
  check("eight column headers", findAll(rows[0], "th").length === 8,
    "got " + findAll(rows[0], "th").length);
  const modelRows = rows.slice(1).filter((tr) => findAll(tr, "td").length === 8);
  check("row td count matches headers", modelRows.length === workModels.length,
    modelRows.length + " vs " + workModels.length);
  if (workModels.length > 0 && modelRows.length === workModels.length) {
    const base = String(workModels[0].model_ref || "").replace(/\\\\/g, "/").split("/").pop();
    const filesCell = findAll(modelRows[0], "td").find((td) => td.attrs.class === "sh-files");
    check("Files cell shows gguf basename", !!filesCell && textOf(filesCell).includes(base),
      filesCell ? textOf(filesCell) + " want " + base : "missing");
  } else {
    check("Files cell shows gguf basename", false, "no WORK models to check");
  }

  const withFault = { ...fixtures["/har/status"], models: fixtures["/har/models"].data, last_error: "broken@WORK: never became ready" };
  applyState(withFault);
  const errline = findAll(document.getElementById("sh-header"), "div", "sh-errline")[0];
  check("last_error renders errline", !!errline && textOf(errline).includes("broken@WORK"),
    errline ? textOf(errline) : "missing");
  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  check("errline absent when last_error null",
    findAll(document.getElementById("sh-header"), "div", "sh-errline").length === 0);

  const extBoxes = findAll(document.getElementById("sh-header"), "input")
    .filter((i) => i.attrs["aria-label"] === "extended logging");
  check("header has Extended Logging checkbox", extBoxes.length === 1
    && extBoxes[0].attrs.type === "checkbox", String(extBoxes.length));

  const mkJob = (model, kind, error) => ({
    id: 9, kind, model, category: "WORK", state: "Idle", terminal: true,
    outcome: error ? "failed" : "ok", error, need_gib: 4, free_at_admit_gib: 40,
    // Restated here (not imported) so the stub emulates what the server's fault_of
    // would emit; the dot now reads this field instead of re-deriving the rule.
    is_fault: kind === "Load" && !!error,
    victims_released: [], warnings: [], timings: {},
  });
  applyState({
    ...fixtures["/har/status"],
    models: fixtures["/har/models"].data,
    jobs: [
      mkJob("qwen3.5-4b", "Load", "launch floor not met"),
      mkJob("qwen3.5-4b", "Load", null),
      mkJob("qwen3.5-4b", "Unload", null),
      mkJob("qwen3.8-27b-turbo-mtp-256k", "Load", "boom"),
    ],
  });
  const dots = {};
  for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
    const tds = findAll(tr, "td");
    const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
    const dot = tds.map((td) => findAll(td, "span", "sh-dot")[0]).find(Boolean);
    if (strong && dot) dots[textOf(strong)] = dot.attrs["data-state"];
  }
  check("superseded fail does not keep row red", dots["qwen3.5-4b"] === "idle",
    JSON.stringify(dots));
  check("latest failed load marks row red",
    dots["qwen3.8-27b-turbo-mtp-256k"] === "failed", JSON.stringify(dots));
  applyState({
    ...fixtures["/har/status"],
    models: [
      { id: "greedyone", engine: "strata", category: "WORK", vram_gb: 55, cost: "greedy",
        launch_floor_gb: 65, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "strata://g", serve_model: null, cpu_offload: false },
      { id: "measuredone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        launch_floor_gb: null, measured_gb: 29.9, measured_min_gb: null, est_vram_gb: 29.4,
        context: null, model_ref: "F:/x.gguf", serve_model: null, cpu_offload: false },
      { id: "rangeone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        launch_floor_gb: null, measured_gb: 41.7, measured_min_gb: 24.3, est_vram_gb: null,
        context: null, model_ref: "F:/y.gguf", serve_model: null, cpu_offload: false },
      { id: "ninferone", engine: "ninfer", category: "WORK", vram_gb: 26, cost: "fixed",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "ninfer://box:8004/served", serve_model: null, cpu_offload: false },
      { id: "mockone", engine: "mock", category: "WORK", vram_gb: 4, cost: "fixed",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "mock://m", serve_model: null, cpu_offload: false },
    ],
  });
  const costs = {};
  for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
    const tds = findAll(tr, "td");
    const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
    if (strong && tds[6]) costs[textOf(strong)] = textOf(tds[6]);
  }
  check("greedy row shows floor, never nominal",
    costs["greedyone"] === "greedy · floor 65 GiB", JSON.stringify(costs));
  check("measured row shows measured",
    costs["measuredone"] === "measured 29.9 GiB", JSON.stringify(costs));
  check("measured row shows the range when a low end exists",
    costs["rangeone"] === "measured 24.3-41.7 GiB", JSON.stringify(costs));
  // cpu_offload engine scoping: the CPU column (td 3) offers its control only
  // for OFFLOAD_ENGINES rows; a strata row gets a dash and NO checkbox.
  const cpuBoxes = {};
  const cpuCellText = {};
  for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
    const tds = findAll(tr, "td");
    const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
    if (strong && tds[3]) {
      cpuBoxes[textOf(strong)] = findAll(tds[3], "input")
        .filter((i) => i.attrs["aria-label"] === "cpu offload").length;
      cpuCellText[textOf(strong)] = textOf(tds[3]);
    }
  }
  check("strata row offers no CPU checkbox (engine-scoped)",
    cpuBoxes["greedyone"] === 0 && cpuCellText["greedyone"] === "\u{2014}",
    JSON.stringify(cpuBoxes));
  check("non-llama rows offer no CPU checkbox (engine-scoped)",
    cpuBoxes["greedyone"] === 0 && cpuBoxes["ninferone"] === 0 && cpuBoxes["mockone"] === 0 &&
      cpuBoxes["measuredone"] === 1 && cpuBoxes["rangeone"] === 1,
    JSON.stringify(cpuBoxes));
  check("llama rows keep their CPU checkbox",
    cpuBoxes["measuredone"] === 1 && cpuBoxes["rangeone"] === 1,
    JSON.stringify(cpuBoxes));
  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });

  // ---- FINDINGS-16 [4]: the header badge reads the server-computed verdicts ----
  // Red when the harness says a listener is down, amber when one is still without a
  // verdict (the tray's precedence, page-side), and never amber over red. The stub sees
  // the tone ATTRIBUTE only: CSS application and stacking cannot be seen from here at
  // all (GOTCHAS [ui][test]) — the amber rule's existence in the served stylesheet is
  // pinned byte-level by served_app_js_reads_the_server_computed_listener_verdicts.
  const flush = async () => {
    for (let i = 0; i < 8; i += 1) await new Promise((r) => setTimeout(r, 0));
  };
  const listenerBadges = () =>
    findAll(document.getElementById("sh-header"), "span", "sh-badge")
      .filter((b) => textOf(b).indexOf("listener") === 0)
      .map((b) => b.attrs["data-tone"] + "=" + textOf(b));
  const verdictDoc = (down, pending) => ({
    ...fixtures["/har/status"],
    models: fixtures["/har/models"].data,
    listeners_down: down,
    listeners_pending: pending,
  });
  check("fixture starts all-clear",
    fixtures["/har/status"].listeners_down === false &&
      fixtures["/har/status"].listeners_pending === false,
    JSON.stringify([fixtures["/har/status"].listeners_down, fixtures["/har/status"].listeners_pending]));
  applyState(verdictDoc(false, false));
  check("all-clear shows no listener badge", listenerBadges().length === 0,
    listenerBadges().join(" | "));
  applyState(verdictDoc(true, false));
  check("listeners_down renders the red badge",
    listenerBadges().join("|") === "bad=listener down", listenerBadges().join(" | "));
  applyState(verdictDoc(false, true));
  check("listeners_pending renders the amber badge",
    listenerBadges().join("|") === "warn=listener pending", listenerBadges().join(" | "));
  applyState(verdictDoc(true, true));
  check("red outranks amber", listenerBadges().join("|") === "bad=listener down",
    listenerBadges().join(" | "));
  applyState(verdictDoc(undefined, undefined));
  check("absent verdicts render no badge (no verdict is not a verdict)",
    listenerBadges().length === 0, listenerBadges().join(" | "));
  applyState(verdictDoc(false, false));

  // ---- FINDINGS-16 [3]: a refused config-default write flashes AND re-reads ----
  const flashBox = document.getElementById("sh-flash");
  const quietFlash = () => { flashBox.textContent = ""; flashBox.hidden = true; };
  const rowButton = (label) =>
    findAll(document.getElementById("sh-main"), "button").find((b) => textOf(b) === label);
  quietFlash();
  const quote = String.fromCharCode(96);
  const refusal = "model " + quote + "qwen3.5-4b" + quote + " belongs to RP, not WORK";
  globalThis.failNextPost = refusal;
  const setDefault = rowButton("Set default");
  check("row offers Set default", !!setDefault);
  const beforeSet = globalThis.statusFetches;
  setDefault.dispatch("click", {});
  await flush();
  check("refused Set default flashes the server's reason",
    flashBox.hidden === false && String(flashBox.textContent) === refusal,
    JSON.stringify(flashBox.textContent));
  check("refused Set default re-reads the server", globalThis.statusFetches > beforeSet,
    beforeSet + " -> " + globalThis.statusFetches);
  const clearDefault = rowButton("Clear default");
  check("panel offers Clear default", !!clearDefault);
  quietFlash();
  const beforeClear = globalThis.statusFetches;
  clearDefault.dispatch("click", {});
  await flush();
  check("refused Clear default flashes and re-reads",
    flashBox.hidden === false && String(flashBox.textContent) === refusal &&
      globalThis.statusFetches > beforeClear,
    JSON.stringify(flashBox.textContent) + " " + beforeClear + " -> " + globalThis.statusFetches);
  globalThis.failNextPost = null;
  quietFlash();
  const beforeAccepted = globalThis.statusFetches;
  setDefault.dispatch("click", {});
  await flush();
  check("accepted Set default re-reads the server",
    globalThis.statusFetches > beforeAccepted && flashBox.hidden !== false,
    beforeAccepted + " -> " + globalThis.statusFetches + " flash=" + JSON.stringify(flashBox.textContent));

  // ---- FINDINGS-16 [2]: a stored-but-wrong token must not re-prompt per tick ----
  const tokenControl = document.getElementById("sh-token-toggle");
  initTokenControl();
  check("no stored token: control reads Sign in", tokenControl.textContent === "Sign in",
    JSON.stringify(tokenControl.textContent));
  globalThis.requireToken = "s3cret";
  globalThis.store.set("sh-control-token", "wrong");
  const wrongSecret = globalThis.store.get("sh-control-token");
  globalThis.promptLog.length = 0;
  globalThis.sentTokens.length = 0;
  globalThis.promptAnswers = ["wrong", "wrong", "wrong", "wrong", "wrong", "wrong"];
  quietFlash();
  await refresh();
  await refresh();
  await refresh();
  // The wire is asserted, not just the dialog: a stub whose window.localStorage
  // install is missing sends no token at all, still prompts exactly once (the dedup
  // guard fires on ""), and would PASS this leg blind (FINDINGS-BATCH-2026-10-28K [K-1]).
  check("wrong token prompts once, not per tick",
    globalThis.promptLog.length === 1 && !!wrongSecret &&
      globalThis.sentTokens.indexOf(wrongSecret) >= 0,
    "prompts=" + globalThis.promptLog.length + " store=" + JSON.stringify(wrongSecret) +
      " sent=" + JSON.stringify(globalThis.sentTokens));
  check("401 is flashed, never swallowed",
    flashBox.hidden === false && String(flashBox.textContent).indexOf("401") >= 0,
    JSON.stringify(flashBox.textContent));
  check("stored token: control reads Clear token", tokenControl.textContent === "Clear token",
    JSON.stringify(tokenControl.textContent));
  globalThis.promptAnswers = ["s3cret"];
  globalThis.removedKeys.length = 0;
  globalThis.promptSawKeys = "unset";
  tokenControl.dispatch("click", {});
  await flush();
  check("clear control removes the stored token key",
    globalThis.removedKeys.indexOf("sh-control-token") >= 0 &&
      globalThis.promptSawKeys === "",
    "removed=" + JSON.stringify(globalThis.removedKeys) +
      " promptSawKeys=" + JSON.stringify(globalThis.promptSawKeys));
  check("the prompt after clearing signs the page back in",
    globalThis.store.get("sh-control-token") === "s3cret",
    JSON.stringify(globalThis.store.get("sh-control-token")));
  globalThis.promptLog.length = 0;
  globalThis.sentTokens.length = 0;
  const beforeSignedIn = globalThis.statusFetches;
  await refresh();
  const signedInSecret = globalThis.store.get("sh-control-token");
  // Same non-vacuity rule as the per-tick leg: the tick must be served by the token
  // the store holds, so a page reading a storage object it was never given cannot
  // "fetch without prompting" its way to green.
  check("signed-in ticks fetch without prompting",
    globalThis.promptLog.length === 0 && globalThis.statusFetches > beforeSignedIn &&
      !!signedInSecret && globalThis.sentTokens.length > 0 &&
      globalThis.sentTokens.every(function (t) { return t === signedInSecret; }),
    "prompts=" + globalThis.promptLog.length + " fetches=" + beforeSignedIn + " -> " +
      globalThis.statusFetches + " store=" + JSON.stringify(signedInSecret) +
      " sent=" + JSON.stringify(globalThis.sentTokens));
  check("a correct token needs no clear-control visit", tokenControl.textContent === "Clear token",
    JSON.stringify(tokenControl.textContent));
  globalThis.requireToken = null;
  globalThis.store.clear();
  quietFlash();

  for (const [name, ok, extra] of results)
    console.log((ok ? "PASS " : "FAIL ") + name + (ok || !extra ? "" : " :: " + extra));
  process.exit(results.every((r) => r[1]) ? 0 : 1);
})();
`;
(0, eval)(appSrc + "\n" + driver);
