# SimpleHar SE — Handoff

Session date: 2026-10-24. Phase: **design, item-by-item brainstorm**. Item 1 (Goals) is done;
later the same day the operator scrapped bonsai / adoption / preload (§4 item 9) and revised
the category model to WORK/RP/AUX/EMB with multi-AUX (§4 item 10), then set one listener
per category with no-model default fallback (§4 item 11), then locked the v0 boundary and
the page layout (§4 item 12), then approved NVML and fixed the AUX header rows (§4 item 13),
then approved Job Objects (§4 item 14), then scaffolded and gated the crate (§4 item 15).
Start the next session by reading this file, then `DESIGN.md`, then pick the next item —
**one item per turn, no batched multi-item proposals** (operator's standing instruction).

## 1. What this project is

A from-scratch rewrite of `C:\Inference\SimpleHar` (v3.3.0) in `C:\Inference\SimpleHar-SE`.
The old tree is archived for reference only; nothing is ported by copy-paste.

Main goal (operator, verbatim): *"Have a single app for managing different inference engines
with simple and reliable adjustable swap mechanism for local LLM models."*

Resolved term readings and all goals: `DESIGN.md` §0, §0.1-§0.4.

## 2. Filesystem / repo state (verify with `git status --short` before anything)

`C:\Inference\SimpleHar-SE`
- `AGENTS.md` (operator-edited between sessions — reread it, do not trust a cached copy),
  `GIT_RULES.md`, `AUX-RULES.md` (renamed from `AUX.md` 2026-10-25: `AUX` is a reserved
  Windows device name git cannot index) — the three governing instruction files.
  `AUX-RULES.md` currently carries
  one rule that matters: **only one subagent active at a time**, and use subagents consistently.
- `DESIGN.md` — the working design doc: goals + 21 numbered steps (options, +/-, choice, why)
  + Appendix A (source-verified defect register with `file:line`).
- `HANDOFF.md` — this file.
- `LICENSE` — from GitHub.
- `base-archive/` — v3.3.0 baseline: `src/` (9 files, 3 170 lines), `Cargo.toml`, `Cargo.lock`,
  `ModelConf/` (35 yamls), `index.html`, `simplehar.json`, `autostart.vbs`, `PLAN.md`,
  `README.md`, `TEST-SWITCHING.md`, `TEST-RESULTS-20261023-1042.md`, `state.json.v22-removed`.
  **Git-ignored, never built.**
- `.gitignore` = `/target`, `/base-archive`, `/logs`, `/.env`.
- **Scaffolded 2026-10-25 (§4 item 15):** `Cargo.toml` (deps in use only; nvml-wrapper /
  windows / reqwest / yaml crate land with their modules) and `src/` — `lib.rs`, `main.rs`,
  `config/`, `model/`, `catalog.rs`, `ownership.rs`, `budget.rs`, `scheduler.rs`,
  `engine/{mod,mock}.rs`, `api/`, `observability.rs`, plus `tests/smoke.rs`. **Not created
  (G14, land with their milestones): `process.rs`, `engine/llama.rs`, `webui/`** (`tray.rs`
  landed 2026-10-25, §4 item 18).

Git: branch `main` tracks `origin/main` = `git@github.com:legislat/SimpleHar.git`
(deploy key, verified: `ssh -T git@github.com` → `Hi legislat/SimpleHar!`).
`origin/main` has one commit, `eaec783 Initial commit` (LICENSE only).
Untracked: `.gitignore`, `AGENTS.md`, `GIT_RULES.md`, `DESIGN.md`, `HANDOFF.md`.
**Nothing has been committed or pushed by me, and per `GIT_RULES.md` do not commit or push
without an explicit operator ask.**

## 3. Hard safety rules for this machine

- `C:\Inference\SimpleHar\simplehar.exe` **is running** (was PID 2348, listening 127.0.0.1:8009
  and 8030). Treat `C:\Inference\SimpleHar` as **read-only reference**. Do not write there, do
  not overwrite its exe, do not restart it.
- A **Strata server is live on 127.0.0.1:18203** and may be serving the agent session driving
  this work (the v3.3 test run was halted by exactly this). Never stop, unload, or replace
  anything on 8003/8004/8005/18203/18204 without explicit operator go-ahead.
- Any SE test run must use its **own port set** — public 8708-8711, one per category
  listener (operator-set 2026-10-25), internal 18109-18119 covering AUX ×6, engine
  18303+ — the config is declarative, so this is free.
- GPU: RTX PRO 5000 Blackwell 72 GB (73 415 MiB), driver 616.92, currently ~69-71 GB in use by
  the live engines. RAM 95 GB, ~93 GB in use. Do not launch big models to "test" things.
- WSL2 `Ubuntu`, user `legislat`, running. `mara-lab` container up ~10 h — protected, never rm.
- Toolchain: `cargo 1.98.1`, `rustc 1.98.1`, clippy 0.1.98. Git 2.55. Shell is **Windows
  PowerShell 5.1** (no `&&`, no `Out-String -NoNewline`, robocopy exit codes 0-7 are success,
  POST JSON via `--data @file` — inline `--data` gets its quotes eaten).

## 4. Decisions already locked (do not re-open without a reason)

Full text in `DESIGN.md`; the load-bearing ones:

1. **Ownership by provenance type** (`SpawnTicket`), disjoint harness/manual port namespaces
   with a startup collision check. "Never kill what it did not start" is a type, not a check.
2. **Scheduler actor** (single-writer, command queue + oneshot waiters) replaces the global
   `tokio::Mutex` + `ensure()` inside the request future. Fixes F-1 (client abort → orphan +
   empty slot) and F-6 (double load → leaked engines, wiped slot table, no error). Reads never
   probe and never reap. Control-plane calls must not queue behind an in-flight load.
3. **Swap state machine** with one exit funnel: install or refuse, never neither.
4. **G12 release-confirmed handoff** (`DESIGN.md` §0.3): prove death (handle signaled / job tree
   empty / container gone + port closed), reconcile ledger with NVML, then spawn immediately.
   Unprovable release = `release_timeout` refusal naming the holder, never a double spawn.
5. **Admission = declared ledger + NVML witness + settle-only-after-release** (kills F-3/F-7).
6. **Engine adapters are data-first** (`DESIGN.md` §7 amendment): engine template in config;
   Rust trait is the escape hatch. Local serving only — no remote/hosted engines.
   Future engines named by operator: **vLLM (WSL), Basalt inference, Tabby API**.
7. **Config**: typed serde schema, per-field errors reported (never silently default the whole
   file — v3.3 bug D1), atomic write-back that preserves comments/CRLF, hot reload via watcher.
8. **AGENTS.md compliance is a gate**, not a stretch goal: 0 `.unwrap()` on I/O paths,
   `thiserror` per layer, `tracing` (no `println!`), `spawn_blocking` for all blocking work,
   `clippy -- -D warnings` clean (the baseline fails it with 8 errors), `cargo fmt --check`,
   unit tests for every public item, params ≤ 5, doc comments on public items, no emoji, 4
   spaces, 100 cols, assume the reader is a Python expert and Rust novice.
9. **Three features scrapped by the operator (2026-10-24, later session), applied to
   `DESIGN.md` in place:** **no bonsai class** (§10, §11 — three default listeners, no
   `bonsai:` key, defects D7 obsolete), **no adoption** (§1, §13 — a slot occupant is always
   harness-spawned; register/unregister are a display list only; defects A3 and C4
   obsolete), **no preload** (§0.2 — boot performs zero engine spawns, slots start Empty,
   no `preload:` key, checkbox, or config flag).
10. **Category model revised (operator, 2026-10-24, category note): four categories —
    WORK, RP, AUX, EMB.** AUX is multi-slot, capacity adjustable 1-6 (default 1; each
    occupant its own singleton slot/port). EMB makes embeddings a first-class category
    (`is_embedding_model: true` → implied-EMB alias). UI: one tab per category, 4 total.
    Consequence: Step 5 is LOCKED declarative (§20 item 2 settled); Step 6 tie-breaks
    extended to AUX ×N.
11. **One listener per category + no-model default fallback (operator, 2026-10-24, listener
    note):** four public listeners (WORK/RP/AUX/EMB), each listing exactly its own
    category's models — replaces the v3.3 main/embeddings/engine class split. A no-`model`
    request on a listener serves the category's loaded model (MRU if AUX ×N), else loads
    the category's `default_model`, else `400`. Settles §20 items 3 and 4. `/har/*` and the
    control page live on one designated category listener (default WORK).
12. **v0 boundary locked (2026-10-25): §0.4 rewritten in place** — 12-row In-v0/Deferred
    table (boot contract, actor, ownership, llama+mock, four categories, per-category
    listeners + fallback, admission, proxy surface, typed config, control page, gates) plus
    a new non-goal: no state restore across harness restarts. **Control-page layout
    clarified on the mockup (same day): persistent header always visible** (title +
    loaded count, endpoint line, one slot card per category, GPU/RAM bars — v3.3 look),
    **model tabs below** (WORK/RP/AUX/EMB; AUX card summarizes its N occupants; v3.3 Cat
    dropdown and Emb/Preload columns gone, CPU checkbox stays). Scaffolding is unblocked
    (§6 step 2), pending items 5/6 folding in at `Cargo.toml` time.
13. **NVML approved + AUX header layout (operator, 2026-10-25):** item 5 locked —
    `nvml-wrapper` in-process as the primary VRAM witness, automatic `nvidia-smi` fallback
    only when the NVML DLL is absent (logged loudly once). The AUX header card at ×N
    **splits horizontally into one-line rows** (model name + own Unload button each),
    replacing the earlier "summary card" reading (DESIGN §14).
14. **Job Objects approved (operator, 2026-10-25):** item 6 locked — one Win32 Job Object
    per owned engine tree, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the single documented
    `unsafe`, contained in `process.rs` (§18). Recorded boundary: job objects cover Windows
    child trees only — for `ninfer` the container label-reap (Step 8 D) is the primary
    guarantee, startup reap mandatory, docker invoked via `wsl -d … docker …`.
    All `Cargo.toml`-blocking items (5, 6) are now settled; the scaffold (§6 step 2) is next.
15. **Scaffolded and gated (2026-10-25):** crate builds and boots. Landed: typed config
    with per-field `validate()` + SE defaults (ports 8109-8112, WORK control, v3.3
    ecosystem protected), `ModelEntry` with `sticky`/`is_embedding_model` alias rules,
    catalog alias resolution, `SpawnTicket`/`Provenance` ownership types, budget need
    formula + admission arithmetic, `Command`/`SwapState` with the §3 transition graph,
    `EngineAdapter`/`LaunchSpec`/typed `ReadinessSpec` + mock adapter, control router
    (`/health`, `/har/status`, typed `ApiError` status mapping, trace/compress/timeout
    layers), tracing init. Gates: `cargo fmt --check` clean, `clippy --all-targets
    -- -D warnings` clean, **32/32 tests pass**; binary booted on 127.0.0.1:8109,
    answered `/health` + `/har/status`, then killed (test instance only). Next: §6
    step 3 — the scheduler actor + state machine against the mock engine.
16. **Scheduler actor milestone (2026-10-25, committed):** the product core runs.
    `scheduler/` split into `vocab.rs` (commands, `SwapState`, transition graph) and
    `mod.rs` (single-writer actor: mpsc commands + oneshot waiters, event-driven steps
    — every engine step is a spawned task, so `status`/`cancel`/`unload` never queue
    behind a load; waiter-list dedup for identical loads; supersede with pending
    restart for same-category loads; jobs ledger with per-job timings and terminal
    records; kill+confirm release flow with `release_timeout_s`). `EngineRuntime`
    trait (async-trait) + `SpawnError`; mock runtime in `tests/mock_runtime.rs`
    (real axum servers on ephemeral ports, modes mock/mock-slow/mock-dies/mock-never);
    12 integration tests pin F-1, F-6, F-4-class already-loaded, T3.4, T8, A1,
    admission numbers, under-specified, unload, deadline-failure. **Graph amendment:**
    unload completes `Dispose → Reap → Idle` (DESIGN §3 note). Deps added:
    async-trait, reqwest. Gates: fmt clean, clippy `-D warnings` clean, **45/45
    tests**. Not yet wired into `main`/HTTP (next: API surface — `/har/load`,
    `/har/unload`, `/har/jobs`, per-category listeners + no-model fallback).
17. **API surface milestone (2026-10-25, committed): the harness is headless-usable.**
    Typed `SchedulerError → ApiError` mapping (E1: 404/400/503/502/409/501/500 by type,
    admission numbers travel in the message). Control routes on the designated listener:
    `/har/load` (waits through the load, idempotent via waiter attach), strict
    `/har/unload {"category"}` (400, never implicit all), `/har/jobs` + `/har/jobs/{id}`,
    extended `/har/status` (categories + slots + jobs + mem config). Four per-category
    listeners: filtered `/v1/models`, wrong-category/unknown `404` **before** any
    ensure, no-model fallback (loaded occupant MRU → `default_model` load → `400`
    naming the listener), proxy pass-through of chat/completions/embeddings with
    `model` rewrite and byte-streamed responses (SSE-capable; 1 GiB body limit).
    `main.rs` binds all listeners (category bind failure → log + skip, G11 partial;
    control bind failure → fatal). `engine::unimplemented` placeholder runtime for the
    binary: boots clean, loads refuse with "process runtime lands with Step 8" —
    `process.rs` is the next milestone, then the ModelConf scan (catalog is empty
    until then). Gates: fmt clean, clippy `-D warnings` clean, **56/56 tests**;
    binary boot-verified: 4 listeners on 8109-8112, typed 400/404 bodies confirmed
    by curl, test instance killed. Known gaps deferred: Host/Origin guard, request
    ids, SSE *events* stream for the page, retry-once-on-crash, `/har/rescan`.
18. **Tray milestone (2026-10-25, operator spec, built): `src/tray.rs`.** Four states —
    green (model loaded), yellow (load/unload in flight), gray (nothing loaded),
    **red (error requiring attention: the latest terminal job errored; its message
    rides in the tooltip; a later success clears it — flagged reading, operator may
    prefer sticky-until-clicked)**. Tooltip lists per-category occupants. Left click
    does nothing. Right-click menu: Open Web UI / Unload All Models / Unload WORK /
    Unload RP / Unload All AUX / Unload EMB / Exit — same scheduler commands as HTTP,
    no tray-only path. Runtime-generated circle icons (no assets); Win32 message pump
    on one dedicated thread (the module's documented `unsafe`, plus an ABI shim for
    `DefWindowProcW`); 2 s refresh; `CREATE_NO_WINDOW` browser open (E5); Exit +
    Ctrl+C share one `watch` channel; `tray_enabled` config key (default on).
    Deps: tray-icon 0.21, windows 0.62 (pump; Job Objects later). DESIGN §15 rewritten
    with the spec; §0.4 tray row amended. Gates: fmt clean, clippy `-D warnings`
    clean, **58/58 tests** (2 new tray-state tests); binary boot-verified with the
    tray up (icon visible, listeners clean, killed after check). **Uncommitted** —
    say the word for a commit.
19. **Red state redefined (operator, 2026-10-25, clarifying note; supersedes item 18's
    flagged reading): red = anything that breaks full functionality with the harness
    still running** — a listener that cannot bind (port busy) or died and cannot be
    restarted. Built: `src/listeners.rs` health registry (`Serving`/`Down{reason}`,
    port named); `main.rs` runs one supervisor per listener (bind → serve → rebind
    every 5 s on failure; **no bind failure is fatal anymore, including the control
    listener** — §0.2 "partial availability beats a dead process" now literal);
    `/har/status` exposes `listeners`; tray red = `any_down()`; per-job errors are NOT
    red (tooltip/status only). Live-verified both directions: held port 8112 → EMB
    `Down` with reason in `/har/status` (others serving, tray red condition); freed →
    supervisor rebound → `Serving`, red clears. Gates: fmt clean, clippy `-D warnings`
    clean, **61/61 tests** (3 registry + updated tray tests). Still uncommitted with
    item 18.
20. **Process milestone (2026-10-25, built; A10 included): real engines run.**
    `src/process.rs`: hidden spawns (`CREATE_NO_WINDOW`) with stdout/stderr →
    `logs/<name>-<epoch>.log`, one Win32 Job Object per engine tree
    (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, the crash-proof orphan guarantee —
    documented `unsafe` with per-block invariants), kill = terminate job + child, G12
    evidence = child handle signaled AND port refuses TCP (entry cleaned on confirm),
    probe = shared `http_probe`. `SingleInstanceGuard` (A10): named mutex — verified
    live: second instance refuses with exit 1. `engine/llama.rs`: A.7 flag table as
    data + `cpu_offload`/forced `--host`/`--port` extensions; **parity test re-reads
    `C:\Inference\llama.cpp\start.ps1` and requires the table to match** (passes).
    `EngineAdapter::spec(model, port)` now takes the assigned port; config gained
    `engine_ports` per category (sequential plan from 18109: WORK 18109 / RP 18110 /
    AUX 18111.. / EMB next; validated unique + disjoint) and `llama_server_exe`
    (default `C:\Inference\llama.cpp\llama-server.exe`). Scheduler assigns each slot
    its engine port. `engine/unimplemented.rs` deleted. Integration test spawns a
    real python HTTP child under a Job Object: probes ready over HTTP, log file
    verified, kill → release confirmed, table cleaned. Gates: fmt clean, clippy
    `-D warnings` clean, **70/70 tests**; boot + second-instance refusal verified
    live. **Uncommitted** — next: ModelConf scan (Step 9) to feed the catalog, then
    the first real load of the smoke-test pair.
21. **Listener port set fixed by the operator (2026-10-25): public listeners are
    WORK 8708 / RP 8709 / AUX 8710 / EMB 8711** (control = WORK 8708). Defaults,
    tests, and HANDOFF §3 updated; engine-port plan unchanged (18109+). Still
    uncommitted with items 18-20.
22. **ModelConf scan milestone (2026-10-25, committed): the catalog is real files —
    and the FIRST REAL ENGINE LOAD happened.** `model/scan.rs`: flat yaml parser
    with quote/inline-comment handling (D2), invalid/duplicate keys *reported* not
    dropped (D3), `scan_dir` → `Catalog{models, errors}` (bad file skipped + named,
    missing dir = one error, never fatal). The **scheduler owns the catalog** (single
    writer): `Scheduler::start(cfg, models_dir, …)` scans at boot; `Command::Reload`
    rescans; `handle.catalog()` snapshot; `SchedulerStatus.catalog_errors`;
    `default_model` cross-checked against the catalog. API: catalog fetched from the
    actor, `POST /har/rescan` (counts + errors), `/har/status` carries
    `catalog_errors`. Config: `modelconf_dir` (default `<exe>/ModelConf`). All test
    harnesses now write real temp ModelConf dirs (actor + api + smoke). Gates: fmt
    clean, clippy `-D warnings` clean, **78/78 tests**. **Live demo (all on the SE
    port set):** boot → `/v1/models` lists `qwen3-0.6b` from yaml; `/har/load`
    spawned the real hidden `llama-server.exe` under a Job Object (log captured);
    chat completion through the proxy answered (Qwen3-0.6B Q8, cpu_offload —
    exactly the v3.3 smoke-test pair); unload → slot emptied, port 18109 refuses,
    zero orphan processes. A first attempt with a wrong model path was **refused
    with the engine's own reason in the job record** — the funnel held. Known
    refinement (next turn): probe should fast-fail when the child has already
    exited (a dead engine currently eats the full `load_timeout`).
23. **Page + defaults + fast-fail (2026-10-25, built): the control page is real.**
    (a) `process.rs` probe now fast-fails when the tracked child already exited
    ("engine process exited early") — a dead engine no longer eats `load_timeout`
    (test: python child that exits immediately). (b) `POST /har/config/default`
    {category, model|null}: validates against the catalog (model must belong to the
    category), takes effect immediately via in-memory overrides, and persists to
    `simplehar.json` atomically (parse-patch-temp-rename); `/har/status` shows
    effective defaults; the no-model fallback honors overrides. (c) `GET
    /har/models` — full unfiltered catalog for the page (per-listener `/v1/models`
    stays category-filtered). (d) **`src/webui/`**: the Step 14 page — vendored
    Pico 2.1.1 + `style.css` + vanilla `app.js` + `index.html` via `include_str!`;
    persistent header (title + loaded badge + listener-down badge, endpoint line +
    Rescan, one slot card per category with Unload — the AUX card as one-line
    occupant rows — and labeled memory bars: ledger honestly marked "NVML witness
    pending"); category tabs below with per-model Load / Set-default, listener port
    and effective default per tab; recent-jobs strip with errors; adaptive
    light/dark with a sticky toggle (localStorage), Sora/Inter typography. Gates:
    fmt clean, clippy `-D warnings` clean, **82/82 tests**. Live: page assets on
    8708; rescan → set-default (persisted to simplehar.json, verified) → a bare
    no-model chat **loaded the real Qwen3-0.6B on demand and answered** (operator
    rule 4 end-to-end). **Uncommitted.** Next candidates: NVML witness (item 5),
    SSE events stream replacing the 2 s polling, write-back for per-model yaml
    flags, T1..T14 port.
24. **NVML witness milestone (2026-10-25, built): Step 4 choice D is real — F-3/F-7
    closed.** `src/witness.rs`: `NvmlWitness` (nvml-wrapper in-process, total +
    used + per-process compute list, 2 s TTL cache) with the approved
    `nvidia-smi` fallback (`--query-gpu` + `--query-compute-apps`, spawn_blocking
    with timeout, logged loudly once); errors mean "unavailable" — admission runs
    ledger-only, never a stale guess. `budget.rs`: `MemorySample` (external_gib by
    pid attribution, settled check), `VramWitness` trait, `LedgerOnlyWitness` for
    tests. Actor: admission is now a witness-driven async step (Event::Admitted →
    `finish_admit`: external reservations subtract via per-process attribution with
    own pids from `Spawned{port,pid}`); **settle-after-release** gates
    Dispose→Spawn (Event::Settled): externals frozen at admit so a dead own
    engine's leftover cannot masquerade as a foreign holder; timeout refuses with
    `settle timeout: U GiB used vs E GiB expected` (SchedulerError::SettleTimeout
    → 503) and never double-spawns. Config: `settle_timeout_s` (30),
    `witness_tolerance_gb` (2.0). `/har/status` mem now carries
    ledger/observed/external/source; the page shows a third "Observed" bar with the
    witness label. Tests: 5 witness-suite tests (scripted FakeWitness: external
    blocks admission with numbers; settle blocks spawn while released memory is
    visible; settle passes once memory returns; smi CSV parsing; skip-safe live
    NVML sample). Gates: fmt clean, clippy `-D warnings` clean, **87/87 tests**.
    Live: `/har/status` shows real card numbers (71.69 GiB total, observed used,
    source witness). **Uncommitted** — say the word.
25. **SSE events stream (2026-10-25, built): the page polls nothing.** The actor
    owns a `broadcast::Sender<SchedulerStatus>` and **publishes a snapshot after
    every mutation** (job transitions, swap/unload/reload; reads never publish);
    `SchedulerHandle::subscribe()`. `GET /har/events` (control listener): one SSE
    stream that sends the merged state doc (status + models + listeners) as
    `har_state` events — the current state immediately on connect, then a push per
    mutation, keepalives by the SSE layer, laggy subscribers resync via the next
    full-state merge. The page connects one `EventSource` on load; on stream drop
    it degrades to the 2 s polling fallback and retries the stream after 10 s
    (Step 14: "one SSE stream with a polling fallback"). Dep: tokio-stream. New
    test: content-type + immediate first frame. Gates: fmt clean, clippy
    `-D warnings` clean, **88/88 tests**. Live: 7 `har_state` frames observed
    during a real Qwen3-0.6B load+unload cycle. One incident, transparently: a
    failed python edit truncated `src/api/mod.rs` to zero bytes mid-turn; recovered
    from the `4074f67` commit via `git checkout --` and re-applied the one-line
    `-0.0` cosmetic fix (the only uncommitted change lost; recovered before any
    other damage). **Uncommitted.**
26. **Yaml write-back milestone (2026-10-25, built): page edits persist to the model
    files (Step 9 remainder).** `model/writeback.rs`: read-modify-write of the raw
    lines — comments survive (a `key:` inside a commented block is never
    clobbered), CRLF endings survive (split on `\n` only, `\r` rides per-line;
    appended lines match the file's dominant ending), unknown keys untouched,
    removal deletes the line; atomic replace (temp + rename) with pre-write parse
    validation and post-write re-check that restores the original on failure (the
    D3/D4 fixes as behavior). `POST /har/config/model` {model, category?, cpu?}:
    edits the file, hot-rescans the catalog, and frees the slot when a loaded
    model's category changed so the next use reloads it on the right listener; the
    reply echoes the edits and the fresh entry state. Page: per-row Cat select and
    CPU checkbox posting to it (refresh on success, revert + error flash on
    failure); `/har/models` + merged SSE state carry `cpu_offload`. Gates: fmt
    clean, clippy `-D warnings` clean, **95/95 tests** (6 writeback unit + 1
    integration). Live: category+cpu edit against the real binary — header comment
    and the commented-out old `# category: AUX` line survived verbatim, real key
    rewritten in place, `cpu_offload: true` appended. **Uncommitted.**
27. **Cross-category eviction milestone (2026-10-25, built): Step 6 complete — the
    swap product's last core behavior.** When admission is short after the
    same-slot replacement, `plan_victims` covers the deficit from *evictable*
    occupants: hard filters first (`evictable_by` per victim category; a
    non-evictable occupant becomes a named blocker with "only WORK can evict
    WORK"-style reasons), then the cheapest sufficient set by exhaustive search
    over the ≤9 slots (tie-break: fewer victims), release order least-protected
    first / LRU within. Victims release **one at a time** with a gate re-check
    after each confirmed release (`released_cost` accumulates on the job) — the
    loop stops the moment the load fits, so no more is killed than needed; the
    final release still runs the §0.3 witness settle before the spawn. Refusal
    contract (G6): `Insufficient` now carries `victims_considered` +
    `blockers` and renders them ("blocked by: work50 (nothing may evict WORK)");
    `JobStatus.victims_released` records evictions in the ledger. `Victim` gained
    slot/cost/evicted. Tests: `tests/scheduler_eviction.rs` — WORK evicts AUX,
    cheapest-set minimality (12+20 loaded, deficit 15 → only 20 dies),
    RP-cannot-evict-WORK refusal with the blocker contract, AUX-cannot-evict
    refusal keeping both occupants. Two early test failures were my wrong
    premises (deficit arithmetic; a same-slot replacement that legitimately fit),
    not code bugs. One flake observed once under full parallel load
    (`settle_blocks_spawn`, 1 s deadline), green on re-run and in two consecutive
    full runs — flake-watch per the T-policy. Gates: fmt clean, clippy
    `-D warnings` clean, **99/99 tests**. **Uncommitted.**
28. **Idle timeout + boot measures (2026-10-25, built): Step 5's last knob and
    §0.2's observability.** Idle sweep: the actor owns last-use (proxy paths call
    the fire-and-forget `SchedulerHandle::touch`, a 2 s `ActorMsg::Tick` scans
    slots); a category with `idle_timeout_s > 0` unloads occupants unused for the
    threshold (outcome note "idle timeout" in the ledger; `SlotStatus.idle_s`
    exposes the idle clock). Config rename: `idle_timeout_min` →
    **`idle_timeout_s`** (SE-native schema, no compat burden yet; defaults 0 =
    off, WORK ships 0 — set it explicitly to make WORK idle, config is king).
    Boot measures (§0.2): `BootTimings {config_ms, scan_ms, bind_ms}` — measured
    in main (config load, scheduler scan, and bounded wait until every listener
    is Serving/Down), exposed at `/har/status` `boot`. Tests: `scheduler_idle.rs`
    (idle unloads RP after threshold with the outcome note; 0 disables; activity
    resets the clock), smoke asserts the boot section. Gates: fmt clean, clippy
    `-D warnings` clean, **102/102 tests**. Live: `boot` shows real numbers
    (config 1 ms, scan 14 ms, bind 30 ms). **Uncommitted.**
29. **Guards + request ids (2026-10-25, built): Step 11/12 security and
    correlation invariants.** `api::guard` middleware, applied **inside** the
    router builders (category_router + the control router's /har and webui
    surfaces) so every consumer gets them by default: (a) **Host** must be
    loopback-shaped (IP literal or localhost, v6 bracketed ok) — a hostname is a
    DNS-rebinding attempt, `400` before any handler; (b) **Origin** (when sent)
    must match the request's Host authority on POST — cross-site `403` (CSRF);
    (c) POST bodies must declare `application/json` — else **`415`**
    (`UnsupportedMedia`, new ApiError variant, code `unsupported_media_type`);
    (d) **request id**: client `x-request-id` echoed, else generated
    (`req-<epoch:x>-<counter:x>`), set on every response header and logged at
    request completion with method/path/status (G6 correlation anchor). Tests:
    rebinding host 400 + loopback shapes pass, foreign Origin 403, 415, id
    echo/generation. One structural note: the guard was first applied in `main`;
    tests then showed routers without it — moved into the builders as the
    default. Gates: fmt clean, clippy `-D warnings` clean, **105/105 tests**.
    **Uncommitted.** Next: serve_model discovery (F-4) + retry-once-on-crash
    (T10.1), then /har/log ring (Step 16), then the T1..T14 port.
30. **Publish-to-LAN option (2026-10-25, operator note, built): the bind address
    is a config choice.** `internal_host` accepts a loopback address (default
    127.0.0.1) or `0.0.0.0` (local net); any other (unicast) address is refused
    by validation naming the two options. When publishing beyond loopback,
    `main` logs a loud warning once: no authentication, every device on the
    segment can drive the GPUs (§0.4 as adjusted). Display nuance: `0.0.0.0` is
    a bind address, not a connect address — `/har/status` `control_addr` and the
    tray's Open-Web-UI link show 127.0.0.1 when unspecified. Guards unchanged:
    LAN clients connect by IP (verified live: `http://<lan-ip>:8708/health`
    answers); hostname Host headers stay refused 400 by the rebinding guard.
    DESIGN §0.4 non-goal amended with the note. Test: unicast refused but
    0.0.0.0 validates. Gates: fmt clean, clippy `-D warnings` clean, **105/105
    tests**; live 0.0.0.0 boot verified (warning + serving + display). Also
    live-verified incidentally: the single-instance guard refused a second
    instance during this turn's testing. **Uncommitted.**
31. **Discovery + crash-retry milestone (2026-10-25, built): F-4 and T10.1/A9
    closed.** (a) **serve_model discovery (F-4, Step 10):** at readiness the
    probe task fetches the engine's `GET /v1/models` first entry
    (`EngineRuntime::discover_model_id`); the occupant carries `serve_id`, the
    proxy rewrites `model` to it (harness name as fallback), and a yaml
    `serve_model` that disagrees becomes a recorded warning in the job ledger
    (`JobStatus.warnings`) — never trusted. `SlotStatus`/`SwapReceipt`/
    `Reply::Already` carry serve_id. (b) **Crash-retry (T10.1):** new
    `Command::Restart {model, category}` — kills the (dead) occupant and loads
    fresh through the standard replacement path (release + settle + spawn); the
    proxy retries a connection-level send failure exactly once via Restart,
    re-resolving the (new) port; never on a genuine upstream status (A9) and
    never mid-stream (bytes start past send()). Tests: discovery adopts the
    engine id; mismatch → warning + discovery wins; restart of a missing model
    degrades to a plain load; genuine 500 surfaced with exactly one ledger job.
    Mock now always reports its own id `mock-model` at `/v1/models` (an earlier
    design echoed the yaml's claim back, making mismatch untestable — fixed) and
    gained a `mock-error` mode. Live (real llama-server): a yaml claiming
    `serve_model: wrong-id-on-purpose` loaded fine, the slot's serve_id is the
    engine's real GGUF path, and the job ledger carries the warning verbatim.
    Gates: fmt clean, clippy `-D warnings` clean, **110/110 tests**.
    **Uncommitted.** Remaining to v1.0: /har/log ring (Step 16), the T1..T14
    port, the parallel soak, the cut.
32. **Log ring + hygiene milestone (2026-10-25, built): Step 16 done.**
    (a) `observability.rs`: a tracing `Layer` captures every filtered event into
    a process-global ring (capacity 500, oldest dropped, drop counter kept);
    `LogEntry {ts_ms, level, target, message, fields}`; `ring_push`/`ring_tail`
    public for the endpoint and tests; `init()` installs the ring layer next to
    the console formatter (two EnvFilters, SIMPLEHAR_LOG). (b) `GET /har/log
    ?limit=N` (control router): the last N events + dropped + capacity — why did
    the last load fail, without opening files. (c) Log hygiene: `process.rs`
    prunes engine logs at spawn to the newest 50 files (T14's logs/ budget).
    (d) Page: a collapsible "Harness log" section fetching /har/log on open.
    Tests: ring cap/drops/limit tail; /har/log serves a pushed marker; prune
    keeps the newest. Flake note: the idle suite's 5 s load timeout failed once
    under full parallel load — widened to 30 s (flake policy: widen, never game).
    Gates: fmt clean, clippy `-D warnings` clean, **113/113 tests**.
    **Uncommitted.** Remaining to v1.0: the T1..T14 port, the parallel soak, the
    cut.

## 5. Open items — pick one per turn and brainstorm it

| # | Item | Note |
|---|---|---|
| A | **Item 2 of the design walkthrough: Step 1 Ownership** | the walkthrough order is `DESIGN.md` §1→§19, one step per session-turn, brainstorm style |
| 4 | F-2: no-`model` request → proxy to that listener's slot, or `400` | **settled 2026-10-24** — fallback with default-model semantics (DESIGN §11); v3.3 `400` silently killed the documented behavior |
| 5 | ~~NVML dependency (`nvml-wrapper`) vs spawning `nvidia-smi`~~ | **settled 2026-10-25** — NVML in-process, `nvidia-smi` fallback only if the DLL is absent |
| 6 | ~~Win32 Job Objects (small documented `unsafe`)~~ | **settled 2026-10-25** — approved; the only crash-proof orphan guarantee, one contained `unsafe` |
| 7 | yaml write-back stays the truth for page-edited flags | recommend yes |
| 8 | SE replaces `SimpleHar` after the T1-T14 gate, or soaks in parallel on a second port set | recommend parallel soak; preload is scrapped, so SE boots with empty slots and zero VRAM — the soak can run control-plane-only next to the live v3.3 and load only on command |
| 9 | **Engine template schema** (`DESIGN.md` §7): placeholders, validation, per-model overrides, one file vs directory | blocking for the "add an engine = add a template" goal |
| 10 | ~~Is the v0 boundary in §0.4 right (llama + mock only; tray, externals, ninfer deferred)~~ | **settled 2026-10-25** — §0.4 rewritten + layout clarified (§4 item 12); scaffolding unblocked |
| 11 | Which of vLLM / Basalt / Tabby is the first real second engine | decide before freezing #9 |

## 6. Next concrete steps, in order

1. Walk the remaining design steps with the operator (§1→§19), one at a time, updating
   `DESIGN.md` in place. Do not start coding before the v0 boundary (#10) is agreed.
2. Scaffold the crate per `DESIGN.md` §18: `src/{main,config/,model/,catalog,engine/,process,
   budget,scheduler,ownership,api/,webui/,tray,observability}.rs` + `tests/`, `Cargo.toml` with
   documented version constraints (`axum 0.8`, `tokio 1 full`, `tower` + `tower-http`
   (timeout/trace/compression), `tracing` + `tracing-subscriber`, `thiserror`, `anyhow`,
   `serde`/`serde_json`, serde yaml round-trip crate, `reqwest`, `tray-icon`, `windows`, NVML crate (item 5 approved, 2026-10-25)). Prefer crates
over hand-rolled code (AGENTS.md).
3. Implement the actor + state machine + jobs ledger **against the mock engine first**, with
   unit tests for every transition and every refusal reason (this is the product — G1/G2).
4. `llama` template adapter: reproduce the flag map in `DESIGN.md` Appendix A.7 exactly, plus a
   parity test against `C:\Inference\llama.cpp\start.ps1:45-63` (currently byte-equivalent —
   keep it that way).
5. Budget module: ledger + NVML + settle-after-release, with property-style tests
   ("no accepted sequence exceeds budget", "settle only ever follows a release").
6. API surface (Step 12): typed `ApiError` → status mapping (never substring matching, which is
   what v3.3 does today), `/har/jobs`, request ids, unified error body, SSE stream for status.
7. Control page (Pico CSS + vanilla JS, separate CSS, adaptive theme — AGENTS.md web rules).
8. Then Strata / NInfer templates, externals/register, tray, metrics.

## 7. Source material worth reading before arguing about behavior

- `DESIGN.md` — the current thinking; Appendix A is the defect list with `file:line`.
- `base-archive/README.md` — the best single description of v3.3 behavior (ports, slots,
  endpoints, tray, autostart, build).
- `base-archive/PLAN.md` — v1→v3.3 design history, §16-§21 + FIX LOG. **The file on disk is
  mojibake-corrupted UTF-8** (`â€"`, `Â·`); read around it.
- `base-archive/TEST-SWITCHING.md` — test plan T1..T14 with the invariants each asserts.
- `base-archive/TEST-RESULTS-20261023-1042.md` — the **NO-GO** run: F-1 client-abort orphan,
  F-2 dead no-model fallback, F-3/F-7 stale VRAM, F-4 engine-name mismatch, F-5 UI loading
  state, F-6 double-load leak. The rewrite exists to kill these five classes.
- Prior digests from this session (full text, worth recalling rather than regenerating):
  docs digest and source inventory — available via `kilo_local_recall` in this project.

## 8. Working style the operator expects

One item at a time, brainstorm style: options with pros/cons and a recommendation, then their
call. Prefer starting small with explicit `TBD` seams over pre-building for every future case.
**Subagents: max one active at a time (`AUX-RULES.md`)** — launch the next only after the previous
returns; digest the two existing ones via `kilo_local_recall` instead of re-running them.
Report validation honestly — do not claim tested/formatted/lint-free unless the check ran
(`GIT_RULES.md`). Never touch the live harness or its ports.
