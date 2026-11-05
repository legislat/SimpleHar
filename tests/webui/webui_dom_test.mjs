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
  replaceChildren(...kids) {
    this.children = kids;
    // A real browser loses the scroll when the whole document body is replaced;
    // emulate it on the root so a deleted restore line cannot pass both ways (F2).
    if (this.tag === "#sh-main") globalThis.window.scrollY = 0;
  }
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

// R2-16-03: the unreachable-header alert names `location.host`, so the page
// global exists here as it does in a browser.
globalThis.location = { hostname: "127.0.0.1", host: "127.0.0.1:8708" };
globalThis.window = {
  location: { hostname: "127.0.0.1", host: "127.0.0.1:8708" },
  matchMedia: () => ({ matches: false }),
  localStorage: storage,
  scrollY: 0,
  scrollTo: (x, y) => { globalThis.window.scrollY = y; },
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

// R2-16-03 switch: when set, every fetch rejects with this error, so a leg can
// watch what the page keeps showing while the harness is unreachable.
globalThis.fetchFails = null;
globalThis.fetch = async (path, init) => {
  if (globalThis.fetchFails !== null) throw globalThis.fetchFails;
  if (path === "/har/status") globalThis.statusFetches += 1;
  const sent = (init && init.headers && init.headers["x-har-token"]) || "";
  globalThis.sentTokens.push(sent);
  if (globalThis.requireToken !== null) {
    if (sent !== globalThis.requireToken) {
      return { ok: false, status: 401, json: async () => ({ error: { code: "unauthorized" } }) };
    }
  }
  if (init && init.method === "POST") {
    (globalThis.posts ??= []).push({ path, body: (init && init.body) || "" });
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
  const hasCls = (c) => (c.attrs.class || "").split(/\s+/).includes(cls);
  const walk = (n) => {
    for (const c of n.children ?? []) {
      if (c instanceof El) {
        if (c.tag === tag && (cls === undefined || hasCls(c))) out.push(c);
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
// R2-16-01 static legs read the page assets from the repo's own src/webui: the
// page is a repo file, and the [appJs] override swaps only the logic bundle.
globalThis.servedPage = {
  app: appSrc,
  index: readFileSync(join(HERE, "..", "..", "src", "webui", "index.html"), "utf8"),
  css: readFileSync(join(HERE, "..", "..", "src", "webui", "style.css"), "utf8"),
  pico: readFileSync(join(HERE, "..", "..", "src", "webui", "pico.min.css"), "utf8"),
};
const driver = `
;(async () => {
  const results = [];
  const check = (name, ok, extra) => results.push([name, !!ok, extra ?? ""]);
  // ---- R2-16-01: the page is self-contained — no font CDN (static leg) ----
  {
    const page = globalThis.servedPage;
    const hit = Object.keys(page).filter(function (k) {
      return /fonts\.googleapis|fonts\.gstatic/.test(page[k]);
    });
    check("no Google Fonts reference in any served asset",
      hit.length === 0, "hits in " + hit.join(","));
    check("the page loads no stylesheet from off-harness",
      !/<link[^>]+href=["']https?:/i.test(page.index),
      (page.index.match(/<link[^>]*>/gi) || []).join(" "));
    check("style.css keeps a documented system font stack",
      /font-family:[^;]*system-ui[^;]*sans-serif/.test(page.css) &&
        !/font-family:[^;]*"(Inter|Sora)"/.test(page.css) &&
        page.css.indexOf("R2-16-01") >= 0,
      (page.css.match(/font-family:[^;]+/g) || []).join(" | "));
  }
  await refresh();
  const main = document.getElementById("sh-main");
  const header = document.getElementById("sh-header");
  // 2026-11-01 compaction: the tabs ARE the slot cards and they sit in the sticky
  // header strip; the model table lives alone in main.
  const tabs = findAll(header, "button", "sh-tab");
  check("four tabs rendered", tabs.length === 4, "got " + tabs.length);
  // The whole pill is the tab (2026-11-01): the button text carries category, port
  // and occupant - the category must LEAD the clickable target, not hide in a link.
  check("tab labels lead their pills",
    tabs.map((b) => textOf(b).replace(/[^A-Z].*$/, "")).join(",") === "WORK,RP,AUX,EMB",
    "labels=[" + tabs.map(textOf).join("|") + "]");
  check("tabs moved out of main",
    findAll(main, "button", "sh-tab").length === 0);
  const pills = findAll(header, "span", "sh-tab-item");
  check("one slot pill per category", pills.length === 4, "got " + pills.length);
  check("pill carries the category port",
    textOf(pills[0]).includes(":8708"), textOf(pills[0]));
  check("the port is inside the clickable tab, never a lone anchor",
    findAll(pills[0], "a").length === 0 && textOf(tabs[0]).includes(":8708"),
    textOf(pills[0]));
  check("occupied pill carries the occupant and an Unload",
    textOf(pills[0]).includes("strata-fp4local-bf16ple") &&
      findAll(pills[0], "button").some((b) => textOf(b) === "Unload"),
    textOf(pills[0]));
  check("pill carries the explicit Unload all scope",
    findAll(header, "button").some((b) => textOf(b) === "Unload all"));
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

  const rpTab = findAll(document.getElementById("sh-header"), "button", "sh-tab")[1];
  rpTab.dispatch("click", {});
  check("tab click switches", state.activeTab === "RP");
  const rpPill = findAll(document.getElementById("sh-header"), "span", "sh-tab-item")[1];
  check("RP pill shows its port", textOf(rpPill).includes(":8709"), textOf(rpPill));
  check("panel head shows the active tab's default",
    textOf(document.getElementById("sh-main")).includes("default: starlight-synthesis-31b"),
    "no default text in main");
  // Bars above the tab strip (operator 2026-11-01): headline, memory, controls.
  const hdrWrap = document.getElementById("sh-header").children[0];
  check("memory bars sit above the tab strip",
    (hdrWrap.children[1].attrs.class || "") === "sh-mem"
      && (hdrWrap.children[2].attrs.class || "") === "sh-controls",
    hdrWrap.children.map((c) => c.attrs.class).join("|"));

  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  check("SSE full-doc render ok", !document.getElementById("sh-flash").textContent);

  // Memory-bar honesty (2026-10-26 review + ledger demotion): one bar, the card's
  // own reading, labeled with its source; no ledger bar, no fabricated zero.
  // Compaction (2026-11-01): the label carries only the number; the honesty
  // sentence lives in the label's tooltip - same words, asserted here.
  const fullText = (l) => (l.attrs.title || "") + " " + textOf(l);
  const barLabels = findAll(document.getElementById("sh-header"), "label");
  const vramEls = barLabels.filter(function (l) {
    return textOf(l).indexOf("VRAM ") === 0;
  });
  const vramLabel = vramEls.length ? fullText(vramEls[0]) : "";
  const mem = fixtures["/har/status"].mem;
  check("exactly one VRAM bar", vramEls.length === 1, barLabels.map(textOf).join(" || "));
  check("bar label itself is compact",
    vramEls.length === 1 && !textOf(vramEls[0]).includes("("),
    vramEls.length ? textOf(vramEls[0]) : "missing");
  check("ledger bar is gone",
    barLabels.every(function (l) {
      return textOf(l).indexOf("ledger") < 0 && fullText(l).indexOf("what admission budgets") < 0;
    }),
    barLabels.map(textOf).join(" || "));
  check("observed bar rendered", vramLabel.length > 0, barLabels.map(textOf).join(" || "));
  check("vram bar names its source", vramLabel.indexOf("(" + mem.source + ";") >= 0, vramLabel);
  check("vram bar states attribution",
    mem.attributed_gb == null
      ? vramLabel.indexOf("per-process attribution unavailable") >= 0
      : vramLabel.indexOf("attributed " + mem.attributed_gb.toFixed(1) + " GiB") >= 0,
    vramLabel);
  // R1/R9: the bar reads the witness's own attribution verdict and the sum of what
  // the loaded containers reported reserving — both server-computed, never inferred.
  check("vram bar uses the server attribution verdict",
    vramLabel.indexOf("(" + mem.source + "; attributed " + mem.attributed_gb.toFixed(1) + " GiB") >= 0,
    vramLabel);
  check("vram bar names the container-reported reservation",
    vramLabel.indexOf("containers report " + mem.container_reported_gb.toFixed(1) + " GiB reserved") >= 0,
    vramLabel);
  {
    const degraded = JSON.parse(JSON.stringify(fixtures["/har/status"]));
    degraded.mem.attributed_gb = null;
    degraded.mem.attribution = "unavailable";
    degraded.mem.container_reported_gb = null;
    applyState({ ...degraded, models: fixtures["/har/models"].data });
    const dl = findAll(document.getElementById("sh-header"), "label").filter(function (l) {
      return textOf(l).indexOf("VRAM ") === 0;
    })[0];
    check("unavailable attribution is stated, not inferred from absence",
      fullText(dl).indexOf("per-process attribution unavailable") >= 0, fullText(dl));
    check("a container that never reported says nothing, not zero",
      fullText(dl).indexOf("containers report") < 0, fullText(dl));
    const idle = JSON.parse(JSON.stringify(degraded));
    idle.mem.attribution = "per_process";
    applyState({ ...idle, models: fixtures["/har/models"].data });
    const il = findAll(document.getElementById("sh-header"), "label").filter(function (l) {
      return textOf(l).indexOf("VRAM ") === 0;
    })[0];
    check("a capable but idle card is not called unavailable",
      fullText(il).indexOf("per-process capable (card idle)") >= 0, fullText(il));
    applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  }
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
  const workTab = findAll(document.getElementById("sh-header"), "button", "sh-tab")[0];
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

  // Booking truth (operator 2026-11-01): a loaded row's Cost IS the live slot's
  // booked figure - the container's own word beats the declared one - and the
  // page remembers it for the session, so the truth survives the unload.
  {
    const bookedModel = {
      id: "bookedone", engine: "ninfer", category: "WORK", vram_gb: 34, cost: "fixed",
      cost_basis: "declared", launch_floor_gb: null, measured_gb: null,
      measured_min_gb: null, context: "262144", model_ref: "ninfer://box:8004/served",
      serve_model: null, cpu_offload: false,
    };
    const bookedSlot = {
      category: "WORK", index: 0, model: "bookedone", port: 18109, pid: 1,
      need_gib: 44, idle_s: 0, serve_id: null, loading_job: null,
      launched_at: "2026-11-01T04:48:19Z", stale_config: false,
      container_reported_gb: 44.0, state: "serving",
    };
    const rowCost = (id) => {
      for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
        const tds = findAll(tr, "td");
        const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
        if (strong && textOf(strong) === id) {
          return { text: textOf(tds[6]), cls: tds[6].attrs.class || "", title: tds[6].attrs.title || "" };
        }
      }
      return { text: "row missing", cls: "", title: "" };
    };
    applyState({ ...fixtures["/har/status"], slots: [bookedSlot], models: [bookedModel] });
    check("the loaded row books the slot's live figure, checked",
      rowCost("bookedone").text === "44.0 GiB" &&
        rowCost("bookedone").cls.includes("sh-cost-checked") &&
        !rowCost("bookedone").title.includes("until reload"), JSON.stringify(rowCost("bookedone")));
    // F3: the container says 44 but the gate still books its armed 40 (a config
    // edit lands after launch) - the cell shows the container's word verbatim
    // and names the gate's booking in the tooltip, until a reload re-reads.
    const driftSlot = { ...bookedSlot, need_gib: 40 };
    applyState({ ...fixtures["/har/status"], slots: [driftSlot], models: [bookedModel] });
    check("a reported/booked mismatch names both figures",
      rowCost("bookedone").text === "44.0 GiB" &&
        rowCost("bookedone").title.includes("booked 40.0 GiB until reload"),
      JSON.stringify(rowCost("bookedone")));
    // Under the 0.1 GiB display quantum the two numbers ARE the same figure
    // on screen - no note for a divergence no cell could show.
    const nearSlot = { ...bookedSlot, need_gib: 44.04 };
    applyState({ ...fixtures["/har/status"], slots: [nearSlot], models: [bookedModel] });
    check("a sub-quantum divergence earns no note",
      rowCost("bookedone").text === "44.0 GiB" &&
        !rowCost("bookedone").title.includes("until reload"),
      JSON.stringify(rowCost("bookedone")));
    const unloadedSlot = { ...bookedSlot, model: null, container_reported_gb: null };
    applyState({ ...fixtures["/har/status"], slots: [unloadedSlot], models: [bookedModel] });
    const after = rowCost("bookedone");
    check("the session keeps the last loaded figure in the tooltip",
      after.text === "34 GiB" && after.cls.includes("sh-cost-unchecked")
        && after.title.includes("44.0 GiB on last load"), JSON.stringify(after));
    applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  }

  // Effective context (operator ruling 2026-11-02): the Ctx column shows what
  // each engine really serves - the llama row key, the strata argv
  // --max-context, the ninfer profile max-context, and nothing (em dash) where
  // the engine speaks none. The API derives the number; the page renders it.
  {
    const ctxModels = [
      { id: "llamax", engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: "32768",
        model_ref: "F:/l.gguf", serve_model: null, cpu_offload: false },
      { id: "stratactx", engine: "strata", category: "WORK", vram_gb: 55, cost: "greedy",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: "262144",
        model_ref: "strata://s", serve_model: null, cpu_offload: false },
      { id: "ninferctx", engine: "ninfer", category: "WORK", vram_gb: 26, cost: "fixed",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: "262144",
        model_ref: "ninfer://box:8004/served", serve_model: null, cpu_offload: false },
      { id: "mockctx", engine: "mock", category: "WORK", vram_gb: 4, cost: "fixed",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "mock://m", serve_model: null, cpu_offload: false },
    ];
    applyState({ ...fixtures["/har/status"], models: ctxModels });
    const ctxCell = (id) => {
      for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
        const tds = findAll(tr, "td");
        const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
        if (strong && textOf(strong) === id) return textOf(tds[5]);
      }
      return "row missing";
    };
    const ctxOf = (id) => id + "=" + ctxCell(id);
    check("Ctx shows each engine's effective served context",
      ctxCell("llamax") === "32768" && ctxCell("stratactx") === "262144" &&
        ctxCell("ninferctx") === "262144" && ctxCell("mockctx") === "\u2014",
      ["llamax", "stratactx", "ninferctx", "mockctx"].map(ctxOf).join(" "));
    applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  }

  // Unload all is the explicit {} scope (T12.4), never an implicit one.
  {
    globalThis.posts = [];
    const unloadAll = findAll(document.getElementById("sh-header"), "button")
      .find((b) => textOf(b) === "Unload all");
    check("strip offers Unload all", !!unloadAll);
    unloadAll.dispatch("click", {});
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));
    const hit = (globalThis.posts || []).some(
      (p) => p.path === "/har/unload" && p.body === "{}",
    );
    check("Unload all posts the explicit {} scope", hit, JSON.stringify(globalThis.posts));
  }

  // Operator 2026-11-05: the unload family is ONE amber, and the tab-side Unload
  // is one entity with its tab. No browser here, so the joined edges are
  // COMPUTED by a mini cascade over the served stylesheet: class-only rules
  // matched against the element's classes and resolved by specificity then
  // source order - exactly how a browser breaks those ties (Pico paints its
  // bases through :where(), specificity 0, so any class rule beats them; the
  // attribute and pseudo rules are checked directly for the states they name).
  // The stub never resolves custom properties (UR-05), so the side's
  // background is pinned twice: as the dedicated token name here, and as the
  // per-theme hex that token carries in the palette legs - Pico re-scopes
  // --pico-background-color to its button blue on every button, and the stub
  // structurally cannot see that (UR-01 landed exactly there).
  {
    const page = globalThis.servedPage;
    const unloadHits = (page.app.match(/sh-unload/g) || []).length;
    check("the page carries sh-unload at exactly the three unload call sites",
      unloadHits === 3, "found " + unloadHits);
    const clsOf = function (b) { return b ? " " + (b.attrs.class || "") + " " : ""; };
    const hdr = document.getElementById("sh-header");
    const hdrPills = findAll(hdr, "span", "sh-tab-item");
    const sideBtn = function (pill) {
      return findAll(pill, "button").find(function (b) {
        return (b.attrs.class || "").indexOf("sh-tab-side") >= 0;
      });
    };
    const occupied = hdrPills.filter(function (p) { return !!sideBtn(p); });
    check("the side Unload renders only on pills that carry a loaded occupant",
      occupied.length === 1 && hdrPills.filter(function (p) {
        return findAll(p, "button").some(function (b) { return textOf(b) === "Unload"; });
      }).length === 1, "side pills: " + occupied.length);
    const pillKids = occupied[0].children;
    const side = sideBtn(occupied[0]);
    check("the side Unload shares the tab container, never a wrapper of its own",
      pillKids.length === 2 && pillKids[0] === findAll(occupied[0], "button")[0] &&
        pillKids[1] === side &&
        (pillKids[0].attrs.class || "").indexOf("sh-tab-part") >= 0 &&
        pillKids[0].getAttribute("aria-selected") === "true",
      "kids=" + pillKids.map(function (k) { return k.attrs.class; }).join("|"));
    const allBtn = findAll(hdr, "button").find(function (b) {
      return textOf(b) === "Unload all";
    });
    const rowBtn = function (label) {
      return findAll(document.getElementById("sh-main"), "button")
        .find(function (b) { return textOf(b) === label; });
    };
    check("tab-side and Unload all both carry sh-unload",
      clsOf(side).indexOf(" sh-unload ") >= 0 &&
        clsOf(allBtn).indexOf(" sh-unload ") >= 0,
      "side=" + (side && side.attrs.class) + " all=" + (allBtn && allBtn.attrs.class));
    check("Rescan, Load and Default never take the amber class",
      clsOf(findAll(hdr, "button").find(function (b) { return textOf(b) === "Rescan"; }))
        .indexOf(" sh-unload ") < 0 &&
        clsOf(rowBtn("Load")).indexOf(" sh-unload ") < 0 &&
        clsOf(rowBtn("Default")).indexOf(" sh-unload ") < 0);
    // The fixture's slot occupant is no catalog row, so the serving row is
    // synthesised the same way the booking leg does: one loaded row, its
    // Unload amber, its Default not; unloaded, the Load stays plain.
    const amberModel = {
      id: "amberone", engine: "ninfer", category: "WORK", vram_gb: 30, cost: "fixed",
      cost_basis: "declared", launch_floor_gb: null, measured_gb: null,
      measured_min_gb: null, context: null, model_ref: "ninfer://box:8004/served",
      serve_model: null, cpu_offload: false,
    };
    const amberSlot = {
      category: "WORK", index: 0, model: "amberone", port: 18119, pid: 7,
      need_gib: 30, idle_s: 0, serve_id: null, loading_job: null,
      launched_at: "2026-11-05T00:00:00Z", stale_config: false,
      container_reported_gb: null, state: "serving",
    };
    applyState({ ...fixtures["/har/status"], slots: [amberSlot], models: [amberModel] });
    check("the serving row Unload carries sh-unload and its Default never does",
      clsOf(rowBtn("Unload")).indexOf(" sh-unload ") >= 0 &&
        clsOf(rowBtn("Default")).indexOf(" sh-unload ") < 0,
      "unload=" + (rowBtn("Unload") && rowBtn("Unload").attrs.class) +
        " set=" + (rowBtn("Default") && rowBtn("Default").attrs.class));
    applyState({ ...fixtures["/har/status"],
      slots: [{ ...amberSlot, model: null, container_reported_gb: null }],
      models: [amberModel] });
    check("the unloaded row's Load stays uncoloured and no Unload renders",
      !rowBtn("Unload") && clsOf(rowBtn("Load")).indexOf(" sh-unload ") < 0,
      "load=" + (rowBtn("Load") && rowBtn("Load").attrs.class));
    applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
    const strip = function (src) {
      let s = src;
      for (;;) {
        const a = s.indexOf("/*");
        if (a < 0) break;
        const b = s.indexOf("*/", a + 2);
        if (b < 0) { s = s.slice(0, a); break; }
        s = s.slice(0, a) + " " + s.slice(b + 2);
      }
      return s;
    };
    const css = strip(page.css);
    const wsRe = new RegExp("[" + String.fromCharCode(9, 10, 13, 32) + "]+", "g");
    const norm = function (s) {
      return String(s === null || s === undefined ? "" : s).replace(wsRe, " ").trim();
    };
    const rules = [];
    const ruleAt = function (from) {
      const open = css.indexOf("{", from);
      if (open < 0) return null;
      const close = css.indexOf("}", open);
      if (close < 0) return null;
      return { sel: norm(css.slice(from, open)), body: css.slice(open + 1, close), next: close + 1 };
    };
    let at = 0;
    let scanned;
    while ((scanned = ruleAt(at))) {
      for (const one of scanned.sel.split(",")) {
        rules.push({ sel: one, body: scanned.body, spec: one.split(".").length - 1 });
      }
      at = scanned.next;
    }
    const pureSel = function (sel) {
      if (sel.length === 0 || sel.charAt(0) !== "." || sel.indexOf(" ") >= 0) return false;
      for (const part of sel.slice(1).split(".")) {
        if (!/^[A-Za-z][A-Za-z0-9-]*$/.test(part)) return false;
      }
      return true;
    };
    const applies = function (sel, classes) {
      if (!pureSel(sel)) return false;
      return sel.slice(1).split(".").every(function (c) {
        return classes.indexOf(c) >= 0;
      });
    };
    const computed = function (classes) {
      const out = {};
      for (const r of rules) {
        if (!applies(r.sel, classes)) continue;
        for (const raw of r.body.split(";")) {
          const line = norm(raw);
          if (!line || line.indexOf(":") < 0) continue;
          const prop = line.slice(0, line.indexOf(":")).trim();
          const value = norm(line.slice(line.indexOf(":") + 1));
          const set = function (p, v) {
            if (out[p] === undefined || r.spec >= out[p + "|spec"]) {
              out[p] = v;
              out[p + "|spec"] = r.spec;
            }
          };
          const widthOf = function (v) { return (v.match(/[0-9.]+[a-z%]*/) || ["medium"])[0]; };
          if (prop === "border") {
            const w = widthOf(value);
            const col = value.split(" ").pop();
            set("border-left-width", w); set("border-right-width", w);
            set("border-top-width", w); set("border-bottom-width", w);
            if (col.indexOf("var(") === 0 || col.charAt(0) === "#") set("border-color", col);
          } else if (prop === "border-left") {
            set("border-left-width", widthOf(value));
          } else if (prop === "border-right") {
            set("border-right-width", widthOf(value));
          } else if (prop === "border-radius") {
            set("border-top-left-radius", value); set("border-top-right-radius", value);
            set("border-bottom-left-radius", value); set("border-bottom-right-radius", value);
          } else if (prop === "border-left-width" || prop === "border-right-width" ||
              prop === "border-top-left-radius" || prop === "border-top-right-radius" ||
              prop === "border-bottom-left-radius" || prop === "border-bottom-right-radius" ||
              prop === "background" || prop === "background-color" ||
              prop === "color" || prop === "border-color") {
            set(prop, value);
          }
        }
      }
      return out;
    };
    const sideCss = computed(["sh-mini", "sh-tab-side", "sh-unload"]);
    check("computed: the joined side has no border on the shared edge",
      sideCss["border-left-width"] === "0",
      "border-left-width=" + sideCss["border-left-width"]);
    check("computed: the joined side paints the dedicated background token, never a Pico button-scope var",
      sideCss["background"] === "var(--sh-tab-side-bg)",
      "background=" + sideCss["background"]);
    check("computed: the side rounds only its right corners (mirrored radius)",
      sideCss["border-top-left-radius"] === "0" &&
        sideCss["border-bottom-left-radius"] === "0" &&
        sideCss["border-top-right-radius"] === "var(--sh-radius)" &&
        sideCss["border-bottom-right-radius"] === "var(--sh-radius)",
      "tl=" + sideCss["border-top-left-radius"] + " tr=" + sideCss["border-top-right-radius"] +
        " bl=" + sideCss["border-bottom-left-radius"] + " br=" + sideCss["border-bottom-right-radius"]);
    check("computed: the joined side keeps the amber ink and amber border of the release family",
      sideCss["color"] === "var(--sh-warn-ink)" && sideCss["border-color"] === "var(--sh-warn)",
      "color=" + sideCss["color"] + " border-color=" + sideCss["border-color"]);
    const tabCss = computed(["sh-tab", "sh-tab-part"]);
    check("computed: the tab gives its right edge to the pair (no border, no radius)",
      tabCss["border-right-width"] === "0" &&
        tabCss["border-top-right-radius"] === "0" &&
        tabCss["border-bottom-right-radius"] === "0",
      "brw=" + tabCss["border-right-width"] + " tr=" + tabCss["border-top-right-radius"]);
    const bodyOf = function (needle) {
      const found = css.indexOf(needle);
      if (found < 0) return null;
      const open = css.indexOf("{", found);
      const close = css.indexOf("}", open);
      if (open < 0 || close < 0) return null;
      return norm(css.slice(open + 1, close));
    };
    const selBody = bodyOf('.sh-tab[aria-selected="true"] + .sh-tab-side');
    check("a selected tab never repaints the joined side: no selected-side override ships",
      selBody === null, selBody === null ? "clean" : selBody);
    const amberBody = bodyOf(".sh-unload {");
    check("sh-unload paints the amber border and ink of the release family",
      !!amberBody && amberBody.indexOf("var(--sh-warn)") >= 0 &&
        amberBody.indexOf("var(--sh-warn-ink)") >= 0,
      amberBody || "missing");
    check("amber stays amber: the unload class never uses the fault red",
      !!amberBody && amberBody.indexOf("--sh-bad") < 0 &&
        amberBody.indexOf("#c0392b") < 0,
      amberBody || "missing");
    const hoverBody = bodyOf(".sh-unload:hover");
    check("sh-unload fills with the amber tint on hover",
      !!hoverBody && hoverBody.indexOf("var(--sh-warn-tint)") >= 0,
      hoverBody || "missing");
    const lightBody = bodyOf('html[data-theme="light"]');
    check("the light theme is the warm parchment palette",
      !!lightBody && lightBody.indexOf("--pico-background-color: #f7f1e6") >= 0 &&
        lightBody.indexOf("--pico-card-background-color: #f2eadc") >= 0 &&
        lightBody.indexOf("--pico-card-border-color: #d6c5a8") >= 0 &&
        lightBody.indexOf("--pico-muted-color: #6b6152") >= 0,
      lightBody || "missing");
    // The warm palette is the DEFAULT: its selector list must match an html
    // with NO theme attribute (:root:not([data-theme="dark"])). A bare ":root"
    // branch is the forbidden form: same specificity as the dark defaults but
    // later order, so it would also land on data-theme="dark" and drag the
    // deep light ink onto the dark card (2.8:1 there).
    const warmAt = css.indexOf("--pico-background-color: #f7f1e6");
    const warmOpen = warmAt < 0 ? -1 : css.lastIndexOf("{", warmAt);
    const warmSel = warmOpen < 0 ? ""
      : norm(css.slice(css.lastIndexOf("}", warmOpen) + 1, warmOpen));
    let bareRoot = false;
    for (const branch of warmSel.split(",")) {
      if (norm(branch) === ":root") bareRoot = true;
    }
    check("the warm palette is the default: a theme-attribute-less html matches it",
      warmAt >= 0 && warmSel.indexOf(':root:not([data-theme="dark"])') >= 0 && !bareRoot,
      "selector=[" + warmSel + "]");
    // Which element paints the visible background: Pico sets it on the root
    // (:where(:root) gets background-color:var(--pico-background-color)), so
    // the no-attribute parchment lives on html, and body stays transparent.
    // The first literal hit for that string is inside Pico's theme rule as
    // part of --pico-card-background-color, so scan every occurrence until a
    // candidate selector names the root.
    let rootPaint = false;
    let paintScan = 0;
    for (;;) {
      const paintAt = page.pico.indexOf("background-color:var(--pico-background-color)", paintScan);
      if (paintAt < 0) break;
      const paintOpen = page.pico.lastIndexOf("{", paintAt);
      const paintSel = page.pico.slice(page.pico.lastIndexOf("}", paintOpen) + 1, paintOpen);
      if (paintSel.indexOf(":where(:root)") >= 0) { rootPaint = true; break; }
      paintScan = paintAt + 1;
    }
    check("the visible page background is painted on the root element itself",
      rootPaint, "no :where(:root) rule carries the page background-color");
    check("no theme attribute computes the parchment background (head-root hex)",
      warmAt >= 0 && warmSel.indexOf(':root:not([data-theme="dark"])') >= 0 &&
        !!lightBody && lightBody.indexOf("--pico-background-color: #f7f1e6") >= 0,
      "sel=[" + warmSel + "]");
    check("the light amber text ink is the deeper readable amber",
      !!lightBody && lightBody.indexOf("--sh-warn-ink: #805a00") >= 0,
      lightBody || "missing");
    const darkBody = bodyOf('html[data-theme="dark"]');
    check("the dark theme keeps its machine-room palette",
      !!darkBody && darkBody.indexOf("#10141a") >= 0 &&
        darkBody.indexOf("#161c24") >= 0 && darkBody.indexOf("#232c38") >= 0 &&
        darkBody.indexOf("#7d8896") >= 0,
      darkBody || "missing");
    // UR-01: the side's computed background equals the dedicated token, and
    // BOTH themes carry that token as a plain hex - the card surface the pill
    // sits on - never a custom property Pico re-scopes to its button blue.
    check("both themes resolve the side's background token to the card hex (UR-01)",
      !!lightBody && lightBody.indexOf("--sh-tab-side-bg: #fffaf3") >= 0 &&
        !!darkBody && darkBody.indexOf("--sh-tab-side-bg: #161c24") >= 0 &&
        css.indexOf("--sh-tab-side-bg: var(--pico-") < 0,
      "light=" + (lightBody ? lightBody.indexOf("--sh-tab-side-bg") : -1) +
        " dark=" + (darkBody ? darkBody.indexOf("--sh-tab-side-bg") : -1));
    // Final ruling (2026-11-05): the occupied tab that is also selected is the
    // common view, and the Unload there must stay amber. The rendered fixture
    // pill IS a selected occupied tab (see the aria-selected leg above), so
    // the cascade values are that tab's side; the ink must resolve in both
    // themes: deep amber #805a00 on the warm paper, plain amber on the card.
    check("the selected occupied side stays amber-inked on its card token in both themes",
      selBody === null &&
        sideCss["background"] === "var(--sh-tab-side-bg)" &&
        sideCss["color"] === "var(--sh-warn-ink)" &&
        !!lightBody && lightBody.indexOf("--sh-warn-ink: #805a00") >= 0 &&
        css.indexOf("--sh-warn-ink: var(--sh-warn)") >= 0 &&
        css.indexOf("--sh-warn: #d89b1d") >= 0,
      "selOverride=" + (selBody === null ? "none" : "present") +
        " bg=" + sideCss["background"] + " ink=" + sideCss["color"]);
    // UR-02: the amber label on the hover fill is the intent - dark composites
    // to 5.2:1 only while the fill's alpha stays at or under 0x2e, so pin it.
    const tintAt = darkBody ? darkBody.indexOf("--sh-warn-tint: #d89b1d") : -1;
    const darkTintAlpha = tintAt >= 0
      ? parseInt(darkBody.slice(tintAt + 23, tintAt + 25), 16) : 255;
    check("the dark hover fill stays deep enough for the amber label (UR-02)",
      darkTintAlpha <= 0x2e, "dark hover alpha=" + darkTintAlpha);
    check("the light hover fill keeps its measured 25 % amber (4.9:1)",
      !!lightBody && lightBody.indexOf("--sh-warn-tint: #d89b1d40") >= 0,
      lightBody || "missing");
    // Depth (operator screenshot 2026-11-05): in light the chrome must read
    // DEEPER than the parchment - borders, table rules, table head, panel
    // head and the unselected pills. Every depth token is light-only: a
    // consumer falls back to the pre-fix value, so dark computes identical
    // (the dark/root absence half of this leg is the dark-unchanged guard).
    const rootBody = bodyOf(":root {");
    check("light ramp tokens exist (table rules, muted rules, control fill, idle tab, head rule)",
      !!lightBody && lightBody.indexOf("--pico-card-border-color: #d6c5a8") >= 0 &&
        lightBody.indexOf("--pico-table-border-color: #e2d5be") >= 0 &&
        lightBody.indexOf("--pico-muted-border-color: #d6c5a8") >= 0 &&
        lightBody.indexOf("--sh-chrome-bg: #efe6d6") >= 0 &&
        lightBody.indexOf("--sh-head-ink: #6b6152") >= 0 &&
        lightBody.indexOf("--sh-tab-idle-bg: var(--sh-chrome-bg)") >= 0 &&
        lightBody.indexOf("--sh-panel-head-rule: 1px solid var(--pico-card-border-color)") >= 0,
      lightBody || "missing");
    check("no depth token leaks to dark or the :root defaults",
      !!darkBody && darkBody.indexOf("--sh-chrome-bg") < 0 &&
        darkBody.indexOf("--pico-table-border-color") < 0 &&
        darkBody.indexOf("--sh-tab-idle-bg") < 0 &&
        darkBody.indexOf("--sh-panel-head-rule") < 0 &&
        !!rootBody && rootBody.indexOf("--sh-chrome-bg") < 0 &&
        rootBody.indexOf("--pico-table-border-color") < 0 &&
        rootBody.indexOf("--sh-tab-idle-bg") < 0 &&
        rootBody.indexOf("--sh-panel-head-rule") < 0,
      "dark=" + (darkBody || "missing") .slice(0, 60));
    const idleTabBody = bodyOf('.sh-tab:not([aria-selected="true"])');
    check("the unselected pill takes the light-only chrome token, falling back to the old background",
      !!idleTabBody &&
        idleTabBody.indexOf("background: var(--sh-tab-idle-bg, var(--pico-background-color))") >= 0,
      idleTabBody || "missing");
    const headBody = bodyOf(".sh-panel-head {");
    check("the panel head sits on the surface token with a light-only bottom rule",
      !!headBody && headBody.indexOf("background: var(--sh-panel-bg, transparent)") >= 0 &&
        headBody.indexOf("border-bottom: var(--sh-panel-head-rule, 0)") >= 0,
      headBody || "missing");
    const thBody = bodyOf("table.sh-models thead th {");
    check("the table head IS the surface (no band) with muted light ink",
      !!thBody &&
        thBody.indexOf("background: var(--sh-panel-bg, var(--pico-card-background-color))") >= 0 &&
        thBody.indexOf("color: var(--sh-head-ink, var(--pico-color))") >= 0 &&
        thBody.indexOf("--sh-chrome-bg") < 0,
      thBody || "missing");
    // Luminance truth: the chrome and border hexes must sit BELOW the page
    // (darker), not above it - the exact defect the screenshot showed.
    const LUM_HEX = "0123456789abcdef";
    const lumOf = function (hex) {
      const h = hex.slice(1);
      const lin = [];
      for (let k = 0; k < 3; k++) {
        const raw = LUM_HEX.indexOf(h.charAt(k * 2)) * 16 + LUM_HEX.indexOf(h.charAt(k * 2 + 1));
        let v = raw / 255;
        v = v <= 0.04045 ? v / 12.92 : Math.pow((v + 0.055) / 1.055, 2.4);
        lin.push(v);
      }
      return 0.2126 * lin[0] + 0.7152 * lin[1] + 0.0722 * lin[2];
    };
    check("the light ramp hexes compute in the approved darker order",
      lumOf("#f2eadc") < lumOf("#f7f1e6") &&
        lumOf("#efe6d6") < lumOf("#f2eadc") &&
        lumOf("#e2d5be") < lumOf("#f2eadc") &&
        lumOf("#d6c5a8") < lumOf("#efe6d6") &&
        lumOf("#d6c5a8") < lumOf("#f2eadc") &&
        lumOf("#6f5b36") < lumOf("#f7f1e6") && lumOf("#e6dac3") < lumOf("#f7f1e6"),
      "page=" + lumOf("#f7f1e6").toFixed(4) + " surface=" + lumOf("#f2eadc").toFixed(4) +
        " fill=" + lumOf("#efe6d6").toFixed(4) + " sep=" + lumOf("#e2d5be").toFixed(4) +
        " border=" + lumOf("#d6c5a8").toFixed(4));
    // Remaining Pico-primary controls (operator 2026-11-05: "apply this color
    // scheme to the remaining blue buttons"): the recolor rides Pico's own
    // primary variables inside the light block, once for every button,
    // outline, link, checkbox accent and focus ring.
    check("light Pico-primary variables resolve to the warm control scheme",
      !!lightBody && lightBody.indexOf("--pico-primary-background: #efe6d6") >= 0 &&
        lightBody.indexOf("--pico-primary-border: #d6c5a8") >= 0 &&
        lightBody.indexOf("--pico-primary-hover-background: #e6dac3") >= 0 &&
        lightBody.indexOf("--pico-primary-hover-border: #cdba9d") >= 0 &&
        lightBody.indexOf("--pico-primary-inverse: #373c44") >= 0 &&
        lightBody.indexOf("--pico-primary: #6f5b36") >= 0 &&
        lightBody.indexOf("--pico-primary-hover: #5a4a2b") >= 0 &&
        lightBody.indexOf("--pico-primary-focus: rgba(111, 91, 54, 0.45)") >= 0,
      lightBody || "missing");
    check("light form fields take the warm ink, chrome rest plate and border",
      !!lightBody && lightBody.indexOf("--pico-form-element-color: #373c44") >= 0 &&
        lightBody.indexOf("--pico-form-element-background-color: var(--sh-chrome-bg)") >= 0 &&
        lightBody.indexOf("--pico-form-element-border-color: var(--pico-card-border-color)") >= 0,
      lightBody || "missing");
    check("no primary or form recolor leaks to dark beyond the approved button edge",
      !!darkBody && darkBody.split("--pico-primary").length - 1 === 2 &&
        darkBody.indexOf("--pico-primary-border: #015c8f") >= 0 &&
        darkBody.indexOf("--pico-primary-hover-border: #01699e") >= 0 &&
        darkBody.indexOf("--pico-form-element") < 0 &&
        darkBody.indexOf("--pico-muted-border-color") < 0 &&
        !!rootBody && rootBody.indexOf("--pico-primary") < 0 &&
        rootBody.indexOf("--pico-form-element") < 0 &&
        rootBody.indexOf("--pico-muted-border-color") < 0,
      "dark=" + (darkBody || "missing").slice(0, 60));
    const selectedTabBody = bodyOf('.sh-tab[aria-selected="true"] {');
    check("the selected tab keeps the green accent, outside the warm primary scheme",
      !!selectedTabBody && selectedTabBody.indexOf("var(--sh-accent)") >= 0 &&
        selectedTabBody.indexOf("var(--sh-accent-solid)") >= 0 &&
        selectedTabBody.indexOf("--pico-primary") < 0,
      selectedTabBody || "missing");
    // Matte olive (operator 2026-11-05): the bright #2ea043 is retired. The
    // shared wash is the matte at 44 %; the solid is matte in light and the
    // dark block lightens it to stay readable (a graphic/text step of only
    // 2.95:1 on the dark card would break the >= 4.5:1 rule).
    check("the accent is the matte olive, dark lightens only the solid",
      !!rootBody && rootBody.indexOf("--sh-accent: #596B4770") >= 0 &&
        rootBody.indexOf("--sh-accent-solid: #596B47") >= 0 &&
        !!darkBody && darkBody.indexOf("--sh-accent-solid: #8fae6f") >= 0 &&
        darkBody.indexOf("--sh-accent: ") < 0 &&
        css.indexOf("#2ea043") < 0,
      "root=" + (rootBody || "missing").slice(0, 40) + " dark=" + (darkBody || "").slice(0, 40));
    const sideJoinBody = bodyOf(".sh-mini.sh-tab-side");
    // One plate for every unload (operator 2026-11-05): the row Unload sat on
    // transparent over the cell, the tab-side on the card token - two looks,
    // one control. Both take --sh-tab-side-bg now; hover stays the amber tint.
    check("both unload plates sit on the shared tab-side token",
      !!amberBody && amberBody.indexOf("background: var(--sh-tab-side-bg)") >= 0 &&
        !!sideJoinBody && sideJoinBody.indexOf("background: var(--sh-tab-side-bg)") >= 0 &&
        !!hoverBody && hoverBody.indexOf("var(--sh-warn-tint)") >= 0,
      "unload=" + (amberBody || "missing") + " side=" + (sideJoinBody || "missing"));
    // The white seam (operator 2026-11-05): display:flex on the actions td
    // replaced the table-cell box, so the browser painted a card-white
    // anonymous cell beside it. The flex is now on an inner span; the td
    // stays a table-cell and paints the row colour.
    const actionsTdBody = bodyOf("table.sh-models td.sh-actions");
    check("the actions flex moved off the td: no display-bearing rule on a cell selector",
      css.indexOf("td.sh-row-actions") < 0 &&
        !!actionsTdBody && actionsTdBody.indexOf("width: max-content") >= 0,
      actionsTdBody || "no td.sh-actions rule");
    // Row separator continuity (operator 2026-11-05): one collapsed border on
    // the row spans every column (actions included - it declares no border of
    // its own); the body cells suppress their bottom border uniformly, never
    // one cell individually.
    const trSepBody = bodyOf("table.sh-models tbody tr {");
    const tdSepBody = bodyOf("table.sh-models tbody td {");
    check("row separators are one row-level rule and no body cell draws its own",
      !!trSepBody &&
        trSepBody.indexOf("border-bottom: 1px solid var(--pico-table-border-color)") >= 0 &&
        !!tdSepBody && tdSepBody.indexOf("border-bottom: 0") >= 0 &&
        !!actionsTdBody && actionsTdBody.indexOf("border") < 0 &&
        css.indexOf("tbody td.sh") < 0,
      "tr=" + (trSepBody || "missing") + " td=" + (tdSepBody || "missing"));
    // Tidy row actions (operator 2026-11-05): a compact equal-width pair,
    // flush-left and never stretched by the table's spare width - no grow,
    // one shared floor, compact padding - on a flush first column.
    const rowActBody = bodyOf(".sh-row-actions {");
    const rowBtnBody = bodyOf(".sh-row-actions button {");
    check("row actions are a left-aligned compact non-stretching pair",
      !!rowActBody && rowActBody.indexOf("display: flex") >= 0 &&
        rowActBody.indexOf("justify-content: flex-start") >= 0 &&
        rowActBody.indexOf("flex-end") < 0 &&
        !!rowBtnBody && rowBtnBody.indexOf("flex: 0 0 auto") >= 0 &&
        rowBtnBody.indexOf("min-width: 4rem") >= 0 &&
        rowBtnBody.indexOf("padding: 0.25rem 0.6rem") >= 0 &&
        css.indexOf("flex: 1 1 6rem") < 0 &&
        css.indexOf("justify-content: flex-end") < 0,
      "span=" + (rowActBody || "missing") + " btn=" + (rowBtnBody || "missing"));
    const flushBody = bodyOf("table.sh-models :is(th, td):first-child:not(.sh-empty)");
    check("the first column drops its start padding so the buttons stick to the table corner",
      !!flushBody && flushBody.indexOf("padding-left: 0") >= 0,
      flushBody || "missing");
    // One surface through the table block in BOTH themes (operator
    // 2026-11-05): body cells painted the page token while their own panel
    // sat one token lighter - light #F7F1E6 over #FFFAF3, dark #10141A over
    // #161C24. One unscoped rule now makes every body cell paint nothing, so
    // the panel token shows through and dark mirrors light exactly.
    const mirrorTdBody = bodyOf("table.sh-models tbody td {");
    check("body cells paint nothing in both themes - one unscoped mirror rule",
      !!mirrorTdBody && mirrorTdBody.indexOf("background: transparent") >= 0 &&
        mirrorTdBody.indexOf("border-bottom: 0") >= 0 &&
        css.indexOf('[data-theme="light"] table.sh-models tbody td') < 0 &&
        css.indexOf('[data-theme="dark"] table.sh-models tbody td') < 0 &&
        css.indexOf(':root:not([data-theme="dark"]) table.sh-models tbody td') < 0 &&
        !!darkBody && darkBody.indexOf("table.sh-models tbody td") < 0,
      mirrorTdBody || "missing");
    // Warm block (operator 2026-11-05): no near-white card hex survives inside
    // the light table block - the panel takes the page parchment (light-only
    // token with the card fallback for dark) and the unload plate in the
    // table goes transparent. The header pill keeps its plate.
    const panelBody = bodyOf(".sh-tabpanel {");
    const headerCssBody = bodyOf(".sh-header {");
    check("the light strip sits on the surface token, dark keeps the card",
      !!lightBody && lightBody.indexOf("--sh-panel-bg: #f2eadc") >= 0 &&
        lightBody.indexOf("--sh-header-bg: #f2eadc") >= 0 &&
        !!panelBody &&
        panelBody.indexOf("background: var(--sh-panel-bg, var(--pico-card-background-color))") >= 0 &&
        !!headerCssBody &&
        headerCssBody.indexOf("background: var(--sh-header-bg, var(--pico-card-background-color))") >= 0 &&
        css.indexOf(':root:not([data-theme="dark"]) .sh-unload:not(:hover)') >= 0 &&
        css.indexOf('html[data-theme="light"] .sh-unload:not(:hover)') >= 0 &&
        !!rootBody && rootBody.indexOf("--sh-panel-bg") < 0 &&
        rootBody.indexOf("--sh-header-bg") < 0 &&
        !!darkBody && darkBody.indexOf("--sh-panel-bg") < 0 &&
        darkBody.indexOf("--sh-header-bg") < 0,
      "panel=" + (panelBody || "missing") + " header=" + (headerCssBody || "missing"));
    // Equal unload plates (operator #5, made consistency-proof 2026-11-05):
    // the token plate still carries both unloads in dark; light overrides it
    // with transparent for every unload, so row, side and Unload-all always
    // share one background.
    const lightPlateBody = bodyOf('html[data-theme="light"] .sh-unload:not(:hover)');
    check("light paints no plate behind any unload and dark keeps the shared token plate",
      !!lightPlateBody && lightPlateBody.indexOf("background: transparent") >= 0 &&
        css.indexOf('[data-theme="dark"] .sh-unload') < 0 &&
        !!sideJoinBody && sideJoinBody.indexOf("background: var(--sh-tab-side-bg)") >= 0,
      "lightPlate=" + (lightPlateBody || "missing"));
    // The white "border" (operator 2026-11-05): no computed border owned it -
    // the wrapper's horizontal scrollbar trough drew white under the table.
    // Light paints the bar warm; dark keeps the platform scrollbar.
    check("the table wrapper scrollbar takes the warm light tokens, not the white trough",
      css.indexOf("scrollbar-color: var(--pico-card-border-color) var(--sh-chrome-bg)") >= 0 &&
        css.indexOf(':root:not([data-theme="dark"]) .sh-tablewrap') >= 0 &&
        css.indexOf('html[data-theme="light"] .sh-tablewrap') >= 0,
      "no warm scrollbar rule");
    // No border declaration ships a hex lighter than the page (the operator's
    // "table border is still white" class of defect): every hex named in a
    // border-bearing declaration sits below the page luminance.
    const pageLum = lumOf("#f7f1e6");
    let lightBorder = "";
    for (const d0 of css.split(";")) {
      const d = d0.toLowerCase();
      if (d.indexOf("border") < 0) continue;
      const h = d.indexOf("#");
      if (h < 0) continue;
      const hex = "#" + d.slice(h + 1, h + 7);
      if (hex.length !== 7) continue;
      if (lumOf(hex) > pageLum) lightBorder += " [" + d.trim().slice(0, 60) + "]";
    }
    check("no border-bearing declaration uses a hex lighter than the page",
      lightBorder === "", lightBorder.trim() || "clean");
    // The approved ramp (operator 2026-11-05): panel/header/table-head all
    // read the ONE surface token --sh-panel-bg (#f2eadc in light, card
    // fallback in dark) - the chrome band is retired from the header rules.
    check("panel head and table head share the single surface token (no band)",
      !!headBody && headBody.indexOf("background: var(--sh-panel-bg") >= 0 &&
        !!thBody && thBody.indexOf("var(--sh-panel-bg") >= 0 &&
        !!lightBody && lightBody.indexOf("--sh-panel-bg: #f2eadc") >= 0,
      "head=" + (headBody || "missing").slice(0, 40) + " th=" + (thBody || "missing").slice(0, 40));
  }

  const withFault = { ...fixtures["/har/status"], models: fixtures["/har/models"].data, last_error: "broken@WORK: never became ready" };
  applyState(withFault);
  const errJobs = findAll(document.getElementById("sh-main"), "div", "sh-errline");
  check("last fault renders beside the job ledger",
    errJobs.length === 1 && textOf(errJobs[0]).includes("broken@WORK"),
    errJobs.map(textOf).join(" | "));
  check("the sticky header no longer carries the fault line",
    findAll(document.getElementById("sh-header"), "div", "sh-errline").length === 0);
  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  check("errline absent when last_error null",
    findAll(document.getElementById("sh-main"), "div", "sh-errline").length === 0 &&
      findAll(document.getElementById("sh-header"), "div", "sh-errline").length === 0);

  const extBoxes = findAll(document.getElementById("sh-main"), "input")
    .filter((i) => i.attrs["aria-label"] === "extended logging");
  check("Extended Logging lives beside the harness log, not the strip",
    extBoxes.length === 1 && extBoxes[0].attrs.type === "checkbox"
      && findAll(header, "input").length === 0, String(extBoxes.length));

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

  // B6 admission rung (the Berth cost-ladder borrow): the jobs strip prints the
  // server-owned path beside an admitted row, and prints nothing for a row
  // that carries no rung. The fixture is the live document: three of its last
  // five rows are admitted loads, two are unloads.
  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });
  {
    const strip = findAll(document.getElementById("sh-main"), "div", "sh-jobs")
      .find((d) => findAll(d, "h3").map(textOf).includes("Recent jobs"));
    const lines = strip ? findAll(strip, "div", "sh-job").map(textOf) : [];
    const rungs = ["FitsNow", "ReleaseFirst", "Drain"];
    const rungOf = (t) => rungs.find((rung) => t.endsWith("· " + rung));
    const runged = lines.filter((t) => rungOf(t));
    check("jobs strip prints the admission rung on admitted rows",
      lines.length === 5 && runged.length === 3, lines.join(" | "));
    const plain = lines.filter((t) => t.includes("ninfer-bonsai-heretic"));
    check("a row without a path renders no rung",
      plain.length === 1 && !rungs.some((rung) => plain[0].endsWith("· " + rung)),
      plain.join(" | "));
  }
  applyState({
    ...fixtures["/har/status"],
    models: [
      { id: "greedyone", engine: "strata", category: "WORK", vram_gb: 55, cost: "greedy",
        cost_basis: "declared",
        // The Ctx the server derives from the row's argv (ruling 2026-11-02):
        // the strata_args --max-context token, not the (deleted) inert key.
        launch_floor_gb: 65, measured_gb: null, measured_min_gb: null, context: "262144",
        model_ref: "strata://g", serve_model: null, cpu_offload: false },
      { id: "measuredone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "measured", measured_attribution: "per_process",
        launch_floor_gb: null, measured_gb: 29.9, measured_min_gb: null, est_vram_gb: 29.4,
        context: null, model_ref: "F:/x.gguf", serve_model: null, cpu_offload: false },
      { id: "rangeone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "measured", measured_attribution: "per_process",
        launch_floor_gb: null, measured_gb: 41.7, measured_min_gb: 24.3, est_vram_gb: null,
        context: null, model_ref: "F:/y.gguf", serve_model: null, cpu_offload: false },
      { id: "totaldiffone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "estimate", measured_attribution: "total_diff",
        launch_floor_gb: null, measured_gb: 2.4, measured_min_gb: null, est_vram_gb: 27.4,
        context: null, model_ref: "F:/t.gguf", serve_model: null, cpu_offload: false },
      { id: "engineone", engine: "ninfer", category: "WORK", vram_gb: null, cost: "exact",
        cost_basis: "measured", measured_attribution: "engine_report",
        launch_floor_gb: null, measured_gb: 31.2, measured_min_gb: 31.2, est_vram_gb: null,
        context: "262144", model_ref: "ninfer://box:8004/served", serve_model: null, cpu_offload: false },
      { id: "ninferone", engine: "ninfer", category: "WORK", vram_gb: 26, cost: "fixed",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: "262144",
        model_ref: "ninfer://box:8004/served", serve_model: null, cpu_offload: false },
      { id: "mockone", engine: "mock", category: "WORK", vram_gb: 4, cost: "fixed",
        cost_basis: "declared",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "mock://m", serve_model: null, cpu_offload: false },
      { id: "fitone", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "fit", fit_gb: 31.0,
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/f.gguf", serve_model: null, cpu_offload: false },
      { id: "fitverified", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "fit", fit_gb: 28.4, fit_validated: true,
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/v.gguf", serve_model: null, cpu_offload: false },
      { id: "fitfalse", engine: "llama", category: "WORK", vram_gb: null, cost: "fixed",
        cost_basis: "fit", fit_gb: 30.0, fit_validated: 1,
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/x.gguf", serve_model: null, cpu_offload: false },
      // A verdict the server never pairs with this basis (model_json gates it on
      // cost_source == Fit): the page must still refuse to borrow the green.
      { id: "declaredfit", engine: "llama", category: "WORK", vram_gb: 24, cost: "fixed",
        cost_basis: "declared", fit_gb: 20.0, fit_validated: true,
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/d.gguf", serve_model: null, cpu_offload: false },
    ],
  });
  const costs = {};
  for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
    const tds = findAll(tr, "td");
    const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
    if (strong && tds[6]) {
      costs[textOf(strong)] = {
        text: textOf(tds[6]),
        cls: tds[6].attrs.class || "",
        title: tds[6].attrs.title || "",
      };
    }
  }
  // Colour is the provenance (2026-11-01): the cell carries ONLY the number;
  // green = the gate books a validated figure, yellow = unchecked, sentence in
  // the tooltip.
  check("greedy row is unchecked floor, number only",
    costs["greedyone"].text === "65 GiB" &&
      costs["greedyone"].cls.includes("sh-cost-unchecked") &&
      costs["greedyone"].title.includes("launch floor - dedicated-card intent") &&
      costs["greedyone"].title.includes("65 GiB"), JSON.stringify(costs["greedyone"]));
  check("measured row is checked, number only",
    costs["measuredone"].text === "29.9 GiB" &&
      costs["measuredone"].cls.includes("sh-cost-checked"), JSON.stringify(costs["measuredone"]));
  check("measured row shows the range when a low end exists",
    costs["rangeone"].text === "24.3-41.7 GiB" &&
      costs["rangeone"].cls.includes("sh-cost-checked"), JSON.stringify(costs["rangeone"]));
  // R9: the Cost column shows exactly what the gate books (cost_basis), and says
  // whose number a measured figure is. A total-diff never books, so its row shows
  // the estimate the gate really charges; an engine-reported figure books and is
  // labelled — colour now says whose number it is, prose lives in the tooltip.
  check("a total-diff never shows as the booking basis",
    costs["totaldiffone"].text === "27.4 GiB" &&
      costs["totaldiffone"].cls.includes("sh-cost-unchecked"), JSON.stringify(costs["totaldiffone"]));
  check("an engine-reported figure books and is labelled",
    costs["engineone"].text === "31.2 GiB" &&
      costs["engineone"].cls.includes("sh-cost-checked") &&
      costs["engineone"].title.includes("engine-reported"), JSON.stringify(costs["engineone"]));
  check("a declared row is unchecked with its sentence in the tooltip",
    costs["ninferone"].text === "26 GiB" &&
      costs["ninferone"].cls.includes("sh-cost-unchecked") &&
      costs["ninferone"].title.includes("declared 26 GiB"), JSON.stringify(costs["ninferone"]));
  // R11: a Fit booking is the engine calculator's pre-spawn projection - amber
  // until this session's launch cross-checks it, then green. The gate's basis
  // alone picks the figure: a declared row that happens to carry a smaller
  // fit_gb still shows what the gate actually books.
  check("an unvalidated fit row is amber projection, source named",
    costs["fitone"].text === "31.0 GiB" &&
      costs["fitone"].cls.includes("sh-cost-unchecked") &&
      costs["fitone"].title.includes("engine calculator, pre-spawn") &&
      !costs["fitone"].title.includes("confirmed"), JSON.stringify(costs["fitone"]));
  check("a session-validated fit row is green and says so",
    costs["fitverified"].text === "28.4 GiB" &&
      costs["fitverified"].cls.includes("sh-cost-checked") &&
      costs["fitverified"].title.includes("this session's launch confirmed it"),
    JSON.stringify(costs["fitverified"]));
  check("a declared row shows the booked declared figure, never the fit one",
    costs["declaredfit"].text === "24 GiB" &&
      !costs["declaredfit"].title.includes("engine calculator"),
    JSON.stringify(costs["declaredfit"]));
  // Green ONLY on a true session verdict (app.js checks === true): a truthy
  // non-true flag is not a verdict — 1 is the falsifier that separates the
  // strict compare from a bare truthiness test. And a verdict cannot ride a
  // row the gate does not book from Fit.
  check("a non-true fit verdict never turns the row green",
    costs["fitfalse"].text === "30.0 GiB" &&
      costs["fitfalse"].cls.includes("sh-cost-unchecked") &&
      !costs["fitfalse"].title.includes("confirmed"), JSON.stringify(costs["fitfalse"]));
  check("a declared row never borrows the fit verdict",
    costs["declaredfit"].cls.includes("sh-cost-unchecked") &&
      !costs["declaredfit"].title.includes("confirmed"),
    JSON.stringify(costs["declaredfit"]));
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

  // Axis 2 (ruling 2026-10-29): the amber and the launch stamp are SERVER verdicts
  // rendered as given — stale_config === true paints the dot amber with the true
  // tooltip, launched_at prints verbatim, and an unloaded row (no field) prints
  // neither.
  const launchedStamp = "2026-10-29T03:55:12Z";
  applyState({
    ...fixtures["/har/status"],
    slots: [{ category: "WORK", index: 0, model: "staleone", port: 8111, pid: 1,
      need_gib: 4.0, idle_s: 0, serve_id: null, loading_job: null,
      launched_at: launchedStamp, stale_config: true, state: "serving" }],
    models: [
      { id: "staleone", engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/s.gguf", serve_model: null, cpu_offload: false,
        launched_at: launchedStamp, stale_config: true },
      { id: "cleanone", engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/c.gguf", serve_model: null, cpu_offload: false,
        launched_at: "2026-10-29T04:00:00Z", stale_config: false },
      { id: "unloadedone", engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
        launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
        model_ref: "F:/u.gguf", serve_model: null, cpu_offload: false },
    ],
  });
  const staleMarks = {};
  const nameCells = {};
  for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
    const tds = findAll(tr, "td");
    const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
    if (!strong) continue;
    const cell = tds.find((td) => td.attrs.class === "sh-name");
    const dot = findAll(cell, "span", "sh-dot")[0];
    staleMarks[textOf(strong)] = dot && dot.attrs["data-stale"] === "amber"
      ? dot.attrs.title : null;
    nameCells[textOf(strong)] = cell ? textOf(cell) : "";
  }
  check("stale resident dot is amber with the true tooltip",
    staleMarks["staleone"] === "config file changed since launch \u2014 unload + reload to apply",
    String(staleMarks["staleone"]));
  check("a row the server did not mark stale has no amber marker",
    staleMarks["cleanone"] === null && staleMarks["unloadedone"] === null,
    JSON.stringify(staleMarks));
  check("launch stamp prints verbatim on the loaded rows",
    nameCells["staleone"].includes(launchedStamp)
      && nameCells["cleanone"].includes("2026-10-29T04:00:00Z"),
    nameCells["staleone"] + " | " + nameCells["cleanone"]);
  check("unloaded row prints no launch stamp",
    !nameCells["unloadedone"].includes("launched"),
    nameCells["unloadedone"]);

  // Releasing (operator ruling 2026-11-04): the server's slots[].state wins
  // where present. A committed release renders the releasing pill and row dot
  // with honest text; a payload without a state field (an older harness)
  // keeps today's derivation. Falsifier: page-side derivation only (drop the
  // server read in app.js) and the releasing checks stay "serving"; delete
  // the fallback and the legacy-row checks lose their dot state.
  {
    const relModel = {
      id: "relone", engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
      launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
      model_ref: "F:/r.gguf", serve_model: null, cpu_offload: false,
    };
    const relSlot = { category: "WORK", index: 0, model: "relone", port: 8122, pid: 1,
      need_gib: 4.0, idle_s: 0, serve_id: null, loading_job: 9,
      launched_at: "2026-11-04T00:00:00Z", stale_config: false, state: "releasing" };
    applyState({ ...fixtures["/har/status"], slots: [relSlot], models: [relModel] });
    const relPill = findAll(document.getElementById("sh-header"), "span", "sh-tab-item")[0];
    const relPillDot = findAll(relPill, "span", "sh-dot")[0];
    check("the pill dot follows the server's releasing state",
      !!relPillDot && relPillDot.attrs["data-state"] === "releasing",
      relPillDot ? JSON.stringify(relPillDot.attrs) : "no pill dot");
    check("the releasing pill says releasing, not serving",
      textOf(relPill).includes("releasing"), textOf(relPill));
    const relRowDot = findAll(document.getElementById("sh-main"), "span", "sh-dot")[0];
    check("the releasing model row paints its dot releasing",
      !!relRowDot && relRowDot.attrs["data-state"] === "releasing",
      relRowDot ? JSON.stringify(relRowDot.attrs) : "no row dot");
    const parked = findAll(document.getElementById("sh-main"), "button")
      .find(function (b) { return textOf(b) === "Releasing…"; });
    check("the parked releasing button keeps the plain disabled unload behavior",
      !!parked && parked.attrs.disabled === "" &&
      (parked.attrs.class || "").indexOf("sh-unload") < 0,
      parked ? JSON.stringify(parked.attrs) : "missing");
    // Seam fix, rendered truth (operator 2026-11-05): the actions td is a
    // plain table-cell (class sh-actions, no flex class) and the flexed span
    // inside it carries the buttons - the cell paints, the span lays out.
    const parkedRow = findAll(document.getElementById("sh-main"), "tr")
      .find(function (tr) { return findAll(tr, "button").indexOf(parked) >= 0; });
    const parkedTds = parkedRow ? findAll(parkedRow, "td") : [];
    const parkedSpan = parkedTds.length > 0 ? findAll(parkedTds[0], "span", "sh-row-actions")[0] : null;
    check("the actions cell is a table-cell with the flexed span inside",
      !!parkedRow && parkedTds.length > 0 &&
        (parkedTds[0].attrs.class || "") === "sh-actions" && !!parkedSpan &&
        findAll(parkedSpan, "button").indexOf(parked) >= 0,
      parkedTds.length ? parkedTds[0].attrs.class : "no actions td");
    const relSide = findAll(relPill, "button").find(function (b) {
      return (b.attrs.class || "").indexOf("sh-tab-side") >= 0;
    });
    check("the releasing pill keeps its joined side Unload", !!relSide &&
      (relSide.attrs.class || "").indexOf("sh-unload") >= 0,
      relSide ? relSide.attrs.class : "no side on the releasing pill");
    const legacy = { ...relSlot };
    delete legacy.state;
    applyState({ ...fixtures["/har/status"], slots: [legacy], models: [relModel] });
    const legacyPillDot =
      findAll(findAll(document.getElementById("sh-header"), "span", "sh-tab-item")[0],
        "span", "sh-dot")[0];
    const legacyRowDot = findAll(document.getElementById("sh-main"), "span", "sh-dot")[0];
    check("a payload without state keeps today's derivation",
      !!legacyPillDot && legacyPillDot.attrs["data-state"] === "serving" &&
        !!legacyRowDot && legacyRowDot.attrs["data-state"] === "serving",
      JSON.stringify([legacyPillDot && legacyPillDot.attrs, legacyRowDot && legacyRowDot.attrs]));
  }

  // B3R-02 (review 2026-11-04): a multi-slot category with one serving and one
  // releasing slot keeps the releasing DOT, but the pill TEXT must still name
  // the serving occupant - "releasing…" alone is honest only when no serving
  // occupant remains. Falsifier: restore the releasing-first text and the
  // naming check goes red.
  {
    const mixModel = (id, ref) => ({
      id, engine: "llama", category: "WORK", vram_gb: 4, cost: "fixed",
      launch_floor_gb: null, measured_gb: null, measured_min_gb: null, context: null,
      model_ref: ref, serve_model: null, cpu_offload: false,
    });
    const mixReleasing = { category: "WORK", index: 0, model: "reltwo", port: 8123,
      pid: 1, need_gib: 4.0, idle_s: 0, serve_id: null, loading_job: null,
      launched_at: "2026-11-04T00:00:00Z", stale_config: false, state: "releasing" };
    const mixServing = { ...mixReleasing, index: 1, model: "keepone", port: 8124,
      pid: 2, state: "serving" };
    applyState({ ...fixtures["/har/status"],
      slots: [mixReleasing, mixServing], models: [mixModel("reltwo", "F:/t.gguf"), mixModel("keepone", "F:/k.gguf")] });
    const mixPill = findAll(document.getElementById("sh-header"), "span", "sh-tab-item")[0];
    const mixPillDot = findAll(mixPill, "span", "sh-dot")[0];
    check("the mixed-slot pill keeps the releasing dot winning",
      !!mixPillDot && mixPillDot.attrs["data-state"] === "releasing",
      mixPillDot ? JSON.stringify(mixPillDot.attrs) : "no pill dot");
    const mixPillText = textOf(mixPill);
    check("the mixed-slot pill names the serving occupant, not releasing",
      mixPillText.includes("keepone") && !mixPillText.includes("releasing"), mixPillText);
  }

  applyState({ ...fixtures["/har/status"], models: fixtures["/har/models"].data });

  // ---- N6 (2026-11-02): the Cost cell is a FUNCTION of the server verdicts ----
  // The legs above assert one rendered snapshot per fixture; this leg drives the
  // SAME row through verdict flips (one field changed per applyState) and asserts
  // the rendering moves with each flip: Fit amber -> green on the validated
  // verdict, the basis flip moving the cell to the booked measured figure and
  // shedding the calculator sentence, the attribution label riding ONLY the
  // engine-spoken verdicts (the 2026-11-04 engine_report/engine_status pair,
  // never the witness per_process sum), and a declared basis owning neither
  // sentence. A render keyed on the row id or a fixture string instead of the
  // verdict fields fails here on the second flip.
  {
    const trackModel = (over) => ({
      id: "trackone", engine: "llama", category: "WORK", vram_gb: 12, cost: "fixed",
      cost_basis: "fit", fit_gb: 12.0, fit_validated: false,
      launch_floor_gb: null, measured_gb: 9.5, measured_min_gb: null,
      measured_attribution: null, est_vram_gb: null, context: null,
      model_ref: "F:/track.gguf", serve_model: null, cpu_offload: false, ...over,
    });
    const trackCost = () => {
      for (const tr of findAll(document.getElementById("sh-main"), "tr")) {
        const tds = findAll(tr, "td");
        const strong = tds.map((td) => findAll(td, "strong")[0]).find(Boolean);
        if (strong && textOf(strong) === "trackone" && tds[6]) {
          return {
            text: textOf(tds[6]),
            cls: tds[6].attrs.class || "",
            title: tds[6].attrs.title || "",
          };
        }
      }
      return { text: "row missing", cls: "", title: "" };
    };
    const trackState = (over) =>
      applyState({ ...fixtures["/har/status"], models: [trackModel(over)] });
    trackState({});
    const unvalidated = trackCost();
    trackState({ fit_validated: true });
    const validated = trackCost();
    trackState({ fit_validated: false, cost_basis: "measured" });
    const measured = trackCost();
    trackState({ cost_basis: "measured", measured_attribution: "engine_report" });
    const reported = trackCost();
    trackState({ cost_basis: "measured", measured_attribution: "engine_status" });
    const statusReported = trackCost();
    trackState({ cost_basis: "measured", measured_attribution: "per_process" });
    const treeSum = trackCost();
    trackState({ cost_basis: "declared", measured_attribution: null });
    const declared = trackCost();
    check("the cost cell follows the fit verdict flip on one row",
      unvalidated.cls.includes("sh-cost-unchecked") &&
        unvalidated.text === "12.0 GiB" &&
        !unvalidated.title.includes("confirmed") &&
        validated.cls.includes("sh-cost-checked") &&
        validated.title.includes("this session's launch confirmed it") &&
        validated.text === unvalidated.text,
      JSON.stringify([unvalidated, validated]));
    check("a basis flip moves the cell to the booked measured figure",
      measured.text === "9.5 GiB" && measured.cls.includes("sh-cost-checked") &&
        measured.title.includes("measured 9.5 GiB") &&
        !measured.title.includes("engine calculator"),
      JSON.stringify(measured));
    check("the attribution label rides only the engine-spoken verdicts",
      !measured.title.includes("(engine-reported)") &&
        reported.text === "9.5 GiB" &&
        reported.title.includes("(engine-reported)") &&
        statusReported.text === "9.5 GiB" &&
        statusReported.title.includes("(engine-reported)") &&
        !treeSum.title.includes("(engine-reported)"),
      JSON.stringify([measured, reported, statusReported, treeSum]));
    check("a declared basis owns neither the fit nor the measured sentence",
      declared.text === "12 GiB" && declared.cls.includes("sh-cost-unchecked") &&
        declared.title.includes("declared 12 GiB") &&
        !declared.title.includes("engine calculator") &&
        !declared.title.includes("the gate books it"),
      JSON.stringify(declared));
  }

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

  // ---- R5: stranded reservations ride the same server-verdict badge rule ----
  // The harness decides which reservations no live container renews; the page
  // only lists status.stranded_containers verbatim. Absent or empty means the
  // badge does not exist — an empty list is clean, not a zero-count badge.
  const strandedBadges = () =>
    findAll(document.getElementById("sh-header"), "span", "sh-badge")
      .filter((b) => textOf(b).indexOf("stranded") === 0)
      .map((b) => b.attrs["data-tone"] + "=" + textOf(b));
  const strandedDoc = (names) => ({
    ...fixtures["/har/status"],
    models: fixtures["/har/models"].data,
    ...(names === undefined ? {} : { stranded_containers: names }),
  });
  applyState(strandedDoc(undefined));
  check("clean fixture shows no stranded badge", strandedBadges().length === 0,
    strandedBadges().join(" | "));
  applyState(strandedDoc(["r5-api-a", "r5-api-b"]));
  check("stranded containers render the amber badge, named",
    strandedBadges().join("|") === "warn=stranded: r5-api-a, r5-api-b",
    strandedBadges().join(" | "));
  applyState(strandedDoc([]));
  check("an empty stranded list renders no badge", strandedBadges().length === 0,
    strandedBadges().join(" | "));
  applyState(strandedDoc(undefined));

  // ---- FINDINGS-16 [3]: a refused config-default write flashes AND re-reads ----
  const flashBox = document.getElementById("sh-flash");
  const quietFlash = () => { flashBox.textContent = ""; flashBox.hidden = true; };
  const rowButton = (label) =>
    findAll(document.getElementById("sh-main"), "button").find((b) => textOf(b) === label);
  quietFlash();
  const quote = String.fromCharCode(96);
  const refusal = "model " + quote + "qwen3.5-4b" + quote + " belongs to RP, not WORK";
  globalThis.failNextPost = refusal;
  const setDefault = rowButton("Default");
  check("row offers Default", !!setDefault);
  const beforeSet = globalThis.statusFetches;
  setDefault.dispatch("click", {});
  await flush();
  check("refused Default flashes the server's reason",
    flashBox.hidden === false && String(flashBox.textContent) === refusal,
    JSON.stringify(flashBox.textContent));
  check("refused Default re-reads the server", globalThis.statusFetches > beforeSet,
    beforeSet + " -> " + globalThis.statusFetches);
  const clearDefault = findAll(document.getElementById("sh-main"), "button")
    .find((b) => textOf(b) === "\u2715");
  check("panel head offers Clear default on the default chip", !!clearDefault);
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
  check("accepted Default re-reads the server",
    globalThis.statusFetches > beforeAccepted && flashBox.hidden !== false,
    beforeAccepted + " -> " + globalThis.statusFetches + " flash=" + JSON.stringify(flashBox.textContent));

  // ---- R2-16-02: a refused Extended Logging flip re-reads and unflips ----
  // The POST is refused (token, restart, cross-site). No har_state push follows
  // a config-only write ([3]), so only the handler's own refresh can bring the
  // checkbox back to the server's extended_logging. The live doc moved under
  // the flip here (the WORK default retagged from another tab — the same race
  // the Clear-default comment names), and that is what makes the re-read
  // repaint: the box must then re-render from the re-read value.
  {
    const sd = fixtures["/har/status"];
    const originalExtLog = sd.extended_logging;
    const originalWorkDefault = sd.categories[0].default_model;
    sd.extended_logging = false;
    applyState({ ...sd, models: fixtures["/har/models"].data });
    const extBox = findAll(document.getElementById("sh-main"), "input")
      .find((i) => i.attrs["aria-label"] === "extended logging");
    check("refusal leg starts with the box off per the server",
      !!extBox && extBox.attrs.checked === undefined && !extBox.checked,
      extBox ? JSON.stringify(extBox.attrs) : "missing");
    quietFlash();
    globalThis.failNextPost = "harness refused the logging change";
    const beforeExt = globalThis.statusFetches;
    extBox.checked = true; // a browser flips the box before the change fires
    extBox.dispatch("change", { target: extBox });
    sd.categories[0].default_model = "r2-16-02-race-retag";
    await flush();
    check("refused Extended Logging flashes the server's reason",
      flashBox.hidden === false &&
        String(flashBox.textContent) === "harness refused the logging change",
      JSON.stringify(flashBox.textContent));
    check("refused Extended Logging re-reads the server",
      globalThis.statusFetches > beforeExt, beforeExt + " -> " + globalThis.statusFetches);
    const afterExt = findAll(document.getElementById("sh-main"), "input")
      .find((i) => i.attrs["aria-label"] === "extended logging");
    check("refused Extended Logging unflips to the server value",
      !!afterExt && afterExt !== extBox &&
        afterExt.attrs.checked === undefined && !afterExt.checked,
      afterExt
        ? "checked=" + JSON.stringify(afterExt.checked) +
            " attrs=" + JSON.stringify(afterExt.attrs)
        : "missing");
    globalThis.failNextPost = null;
    sd.categories[0].default_model = originalWorkDefault;
    sd.extended_logging = originalExtLog;
    quietFlash();
    await refresh();
  }

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

  // Polling noise (operator 2026-11-01): while the operator scrolls or selects,
  // a tick that only moved the sampled memory numbers must not repaint the page.
  await refresh();
  const mainNow = document.getElementById("sh-main");
  mainNow.children[0].__keeper = "still-mine";
  globalThis.window.scrollY = 4321;
  const statusDoc = globalThis.fixtures["/har/status"];
  const originalEmbDefault = statusDoc.categories[3].default_model;
  statusDoc.mem.observed_used_gb = 41.5;
  await refresh();
  check("volatile-only tick does not repaint the page",
    mainNow.children.length > 0 && mainNow.children[0].__keeper === "still-mine",
    "repainted: " + (mainNow.children[0] && mainNow.children[0].__keeper));
  check("volatile-only tick leaves the scroll alone",
    globalThis.window.scrollY === 4321, "scrollY=" + globalThis.window.scrollY);
  check("the bar still shows the new number",
    textOf(document.getElementById("sh-header")).indexOf("41.5") >= 0,
    textOf(document.getElementById("sh-header")).slice(0, 90));
  // A real change repaints - and the operator's scroll comes back to them.
  const keeper2 = mainNow.children[0];
  globalThis.window.scrollY = 777;
  statusDoc.categories[3].default_model = "zzz-emb";
  await refresh();
  check("a structural change repaints",
    mainNow.children[0] !== keeper2 && !mainNow.children[0].__keeper);
  check("the repaint restores the scroll",
    globalThis.window.scrollY === 777, "scrollY=" + globalThis.window.scrollY);
  statusDoc.categories[3].default_model = originalEmbDefault;
  // F1: attributed_gb drifts every sample wherever per-process attribution works;
  // it must be volatile like its siblings, while its presence stays structural.
  statusDoc.mem.attributed_gb = null;
  await refresh();
  const beforeAttr = mainNow.children[0];
  statusDoc.mem.attributed_gb = 3.2;
  await refresh();
  check("attributed bytes appearing repaints",
    mainNow.children[0] !== beforeAttr,
    "same node after presence change");
  const attrKeeper = mainNow.children[0];
  statusDoc.mem.attributed_gb = 5.6;
  await refresh();
  check("a drifting attributed_gb does not repaint",
    mainNow.children[0] === attrKeeper,
    "repainted: " + (mainNow.children[0] && mainNow.children[0].__keeper));
  statusDoc.mem.attributed_gb = 12.3;
  // F3: sample presence transitions are page changes, not volatile ticks - the
  // bar must vanish and return through a real repaint, never a silent patch miss.
  const vanishKeeper = mainNow.children[0];
  statusDoc.mem.observed_used_gb = null;
  await refresh();
  check("losing the sample repaints and drops the bar",
    mainNow.children[0] !== vanishKeeper && !mainNow.children[0].__keeper &&
      textOf(document.getElementById("sh-header")).indexOf("VRAM") < 0,
    "header=" + textOf(document.getElementById("sh-header")).slice(0, 90));
  statusDoc.mem.observed_used_gb = 41.5;
  await refresh();
  check("the bar returns when the sample returns",
    textOf(document.getElementById("sh-header")).indexOf("41.5") >= 0,
    "header=" + textOf(document.getElementById("sh-header")).slice(0, 90));

  // Catalog notices live at the PAGE BOTTOM (operator 2026-11-02): a long wall of
  // hygiene notices must never push the model table down. Header stays clean, the
  // block sits between the harness log row and the footer, and it vanishes with
  // the errors.
  statusDoc.catalog_errors = [
    { file: "ninfer-a.yaml", reason: "context does nothing here" },
    { file: "strata-b.yaml", reason: "profile declares host" },
  ];
  await refresh();
  const hdrNotices = findAll(document.getElementById("sh-header"), "div", "sh-catalog-errors");
  const mainNotices = findAll(document.getElementById("sh-main"), "div", "sh-catalog-errors");
  const kids = mainNow.children;
  const noticeText = mainNotices.length ? textOf(mainNotices[0]) : "";
  check("catalog notices sit at the page bottom above the footer",
    hdrNotices.length === 0 && mainNotices.length === 1 &&
      kids.indexOf(mainNotices[0]) === kids.length - 2 &&
      kids[kids.length - 1].attrs.class === "sh-footer" &&
      noticeText.indexOf("ninfer-a.yaml") >= 0 && noticeText.indexOf("strata-b.yaml") >= 0,
    "hdr=" + hdrNotices.length + " main=" + mainNotices.length +
      " idx=" + kids.indexOf(mainNotices[0]) + " last=" + (kids[kids.length - 1] || {}).attrs);
  statusDoc.catalog_errors = [];
  await refresh();
  check("no catalog errors, no notice block",
    findAll(document.getElementById("sh-main"), "div", "sh-catalog-errors").length === 0);

  // ---- R2-16-03: a dead harness must not leave a live-looking header ----
  {
    const headerEl = document.getElementById("sh-header");
    const headerText = () => textOf(headerEl);
    quietFlash();
    check("the leg starts on a real header",
      headerText().indexOf("loaded") >= 0, headerText().slice(0, 90));
    // A TypeError is the browser's own "never reached the harness" shape.
    globalThis.fetchFails = new TypeError("connect ECONNREFUSED 127.0.0.1:8708");
    await refresh();
    const deadBadges = findAll(headerEl, "span", "sh-badge")
      .map((b) => textOf(b) + "@" + b.getAttribute("data-tone"));
    const deadText = headerText();
    check("unreachable fetch stamps the header instead of the stale state",
      deadText.indexOf("harness unreachable") >= 0 &&
        deadBadges.indexOf("harness unreachable@none") >= 0 &&
        deadText.indexOf("loaded") < 0 && deadText.indexOf("serving") < 0,
      "header=" + deadText.slice(0, 110) + " badges=" + deadBadges.join("|"));
    check("the unreachable stamp keeps the alert",
      flashBox.hidden === false && String(flashBox.textContent).indexOf("cannot reach") >= 0,
      JSON.stringify(flashBox.textContent));
    // Recovery with the SAME payload as before the failure: the render gate was
    // voided by the stamp, so the unchanged key still forces a full repaint.
    globalThis.fetchFails = null;
    quietFlash();
    await refresh();
    const backText = headerText();
    check("the next same-key success restores the real header",
      backText.indexOf("loaded") >= 0 && backText.indexOf("unreachable") < 0 &&
        findAll(headerEl, "button", "sh-tab").length === 4,
      "header=" + backText.slice(0, 110));
    // A non-network fetch failure admits only "state not confirmed" — the page
    // cannot claim the harness is down, only that it could not confirm the state.
    globalThis.fetchFails = new Error("/har/status -> 500");
    await refresh();
    const softText = headerText();
    check("a non-network fetch error stamps state not confirmed",
      softText.indexOf("state not confirmed") >= 0 &&
        findAll(headerEl, "span", "sh-badge").some(
          (b) => b.getAttribute("data-tone") === "none"),
      "header=" + softText.slice(0, 110));
    globalThis.fetchFails = null;
    quietFlash();
    await refresh();
    check("the header recovers again after the soft failure",
      headerText().indexOf("loaded") >= 0, headerText().slice(0, 90));
  }

  for (const [name, ok, extra] of results)
    console.log((ok ? "PASS " : "FAIL ") + name + (ok || !extra ? "" : " :: " + extra));
  process.exit(results.every((r) => r[1]) ? 0 : 1);
})();
`;
(0, eval)(appSrc + "\n" + driver);
