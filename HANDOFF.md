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
  (G14, land with their milestones): `tray.rs`, `process.rs`, `engine/llama.rs`, `webui/`.**

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
- Any SE test run must use its **own port set** (e.g. public 8109-8112, one per category
  listener, internal 18109-18119 covering AUX ×6, engine 18303+) — the config is
  declarative, so this is free.
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
