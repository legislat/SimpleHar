# SimpleHar SE — Design Decomposition (rewrite)

Status: DRAFT for operator review. Every step lists the decision, the real options with
pros/cons, the choice, and why. Choices marked **OPEN** need an operator answer; choices
marked **LOCKED** follow from a goal or from a recorded defect in v3.3.

Reference material: `base-archive/` holds the v3.3.0 baseline (docs, sources, configs) for
reference only. It is git-ignored and is not built.

---

## 0. Goals

G1 is the operator's statement, verbatim. G2-G9 are proposed from the v3.3 record
(`PLAN.md` §1-§21, FIX LOG, and findings F-1..F-7 from the 2026-10-23 NO-GO test run) and
are **DRAFT** until confirmed.

| # | Goal | Success test |
|---|------|--------------|
| **G1** | **One application that manages different inference engines, with a simple, reliable, adjustable swap mechanism for local LLM models.** | Operator changes which model answers on a port without ever touching a console, and the switch either fully happens or fully refuses — never a half-state. |
| G2 | The swap is *the* product: it must be correct under concurrency, client disconnect, cancel, crash, and OOM. | F-1, F-6, T3.5, T9.1 pass deterministically; zero leaked engine processes/containers after any failure path. |
| G3 | GPU VRAM is a scheduled resource. Admission is authoritative, not advisory, and never acts on stale samples. | No load is refused because of a stale sampler (F-3/F-7); no accepted load OOMs because two loads double-spent the card. |
| G4 | Zero manual juggling, zero collateral damage: the harness never starts, stops, or drops what it did not start. | Manual servers on protected ports and `protected_containers` survive every harness action, including crash recovery and orphan reaping. |
| G5 | Stable OpenAI-compatible surface: one or more base URLs, `model` field addressed by harness name, SSE streaming, no client can bypass the harness. | All catalog/proxy contracts of v3.3 kept; `/v1/models` never blocks behind a load; bypass attempts structurally impossible. |
| G6 | Everything is explainable at 3 a.m.: every decision the scheduler makes is reconstructible from logs plus one status call. | Each swap has one trace id, one record of need/free/victims/decision, and each engine keeps its own log file. |
| G7 | Built to AGENTS.md: no `unwrap()` on production paths, `thiserror` taxonomy, `tracing` not `println!`, no blocking work on the reactor, `clippy -D warnings` and `cargo fmt` clean, unit tests for every new item. | The pre-commit checklist passes; the old baseline failed it on 8 clippy errors, 54 `unwrap()`, 29 `println!`. |
| G8 | A new engine is an adapter plus a config schema, not a core edit. | Adding a hypothetical 4th engine touches one module + one registry entry. |
| G9 | Adjustable without rebuild: slots, priorities, budgets, ports, timeouts are data. | Changing slot count/priority/eviction order is a config edit + reload, no restart. |
| ~~G10~~ | ~~Self-preservation guard~~ — **dropped by operator 2026-10-24.** The harness does not have to protect the resource serving its own operator. | n/a |
| **G11** | Boot-to-ready is a contract, not a hope: from logon, the box reaches a declared, measurable state without intervention (elaborated in §0.2). **No engine is loaded at boot — preload scrapped, operator 2026-10-24.** | `boot_ms` fields at `/har/status`; boot performs zero engine spawns and ends with every configured listener bound and the catalog error report ready; reclaim leftovers are reported, not fatal. |
| **G12** | Swap handoff is **release-confirmed**: the new load starts the moment the old occupant is *verified gone* — never on a guessed sleep, never while it still holds the port or the memory (defined in §0.3). | `release_ms` measured per job; no spawn ever happens while a previous occupant of that port is unconfirmed-dead; a release that cannot be confirmed ends in `release_timeout` naming the PID/container, not in a double-spawn. |
| **G13** | One-glance diagnosability: one status call plus one trace-id-ordered log sequence answers "why", with the numbers that were used. | Every refusal carries `{need, free, reserve, victims, blockers}`; every job carries its timings and decision inputs. |
| **G14** | Start small: the first milestone is a working core over **one** engine, with growth paths left as explicit `TBD` placeholders rather than pre-built abstractions. | v0 runs the actor + state machine + budget against `llama` plus the mock engine; nothing in v0 requires Strata or NInfer. |

### 0.1 G1 resolved definitions (operator brainstorm, 2026-10-24)

| Term in G1 | Chosen reading | Consequence |
|---|---|---|
| *single app* | **B** — one place you never have to leave; page, tray and HTTP are all views of the same core | Every UI action must be an API call that also exists headless; no tray-only or page-only behavior; the harness must be fully correct with no window at all |
| *managing* | **B** — full ownership: lifecycle + config + logs + failure recovery + "why" | Jobs ledger, per-engine log files, structured refusals and config error reports are all in scope, not optional extras |
| *different engines* | Many more engines later, **local serving only** — no remote/hosted endpoint as an "engine". Named candidates: **vLLM (WSL)**, **Basalt inference**, **Tabby API** | Adapters must be additive, and cost/liveness must be describable per engine (a container, a native exe, a python `-m` module, a WSL script). Zero-cost remote engines are a non-goal for now |
| *simple* | **Opinionated defaults that are optional to change** | Every behavior has a working default; every default is a config key. "No config file" must still produce a usable harness |
| *reliable* | **Predictable outcome** | The bar is not "never fails" but "same inputs, same visible result; either it happens or it refuses with a reason" — the state machine and refusals carry this |
| *adjustable* | **Maximum adjustable options in config** | Knobs are first-class product surface, not debug leftovers: slots, priorities, eviction matrix, ports, timeouts, poll intervals, budgets, listener set, per-model overrides |
| *adding an engine* | Ideally **as simple as adding an engine config template + a model config** (operator: "debatable") | Pushes Step 7 toward a **declarative engine template** (argv template, cwd, env, readiness spec, liveness, kill semantics, cost policy) resolved from data, with a Rust trait as the escape hatch for engines that need logic. Template schema is `TBD` |

### 0.2 G11 elaborated — boot-to-ready contract

The v3.3 startup path can block the reactor for up to ~40 s in `reap_orphans` before any
listener binds (`server.rs:116-170`, `main.rs:739`), so "the harness is up" is not observable
while it is deciding what to kill. SE defines boot as an ordered, instrumented sequence with
deadlines, where the control plane is reachable almost immediately and **no engine work
happens at boot at all** (preload scrapped, operator 2026-10-24):

1. **Config** — load and validate `simplehar.json` (typed, per-field errors reported, never
   silently defaulted — Appendix A D1) and the engine templates.
2. **Namespace check** — harness ports ∩ `protected_ports` = ∅, slot port uniqueness, listener
   bind plan built from config (Step 11).
3. **Single-instance guard** — refuse to start a second harness (named mutex / lock file with
   pid + port probe). The 2026-10-23 run recorded a mystery second PID.
4. **Reclaim owned resources** — reap only what provenance says we own (ticket / container
   label, Step 8), **bounded by a deadline and run off the bind path**, so a stuck WSL or
   docker call cannot delay readiness. Unreclaimable leftovers are reported, not fatal.
5. **Bind listeners** — all configured listeners are up before any engine is touched.
   Target: bind within a few hundred ms; `GET /health` and `/har/status` answer during step 4.
6. **Scan catalog** — `ModelConf` parsed once with a full error report exposed at
   `/har/status`; a bad file is skipped and named, never silently dropped.
7. **(removed — preload scrapped, operator 2026-10-24)** No model is loaded at boot. Slots
   start `Empty`; the first load of anything is an explicit operator/client action (page,
   API, tray) executed as a normal swap job. No `preload:` yaml key, no page checkbox, and
   no `/har/config` preload flag exist in SE.

**Definition of "ready"**: steps 1-6 complete — config validated, namespaces clean, single
instance, owned leftovers reclaimed or reported, every configured listener bound, catalog
scanned with its error report exposed. Slots are `Empty`; every load is an on-demand job.
**Measures** (all at `/har/status.boot`): `config_ms`, `reap_ms`, `bind_ms`, `scan_ms`,
`first_bind_at`. Per-model rolling load timings are kept as **load history** (cold vs warm
file cache for a 55 GB Strata pack differs by minutes) — operator data, not a boot concern.
**Failure mode contract**: a boot that cannot bind a listener names the port and the
PID holding it and continues on the listeners it can serve — partial availability beats a dead
process.

### 0.3 G12 defined — release-confirmed swap handoff

v3.3 already had the right instinct ("wait for full exit so VRAM is released before next
spawn", §5a) but implements it as `taskkill` plus `thread::sleep` polling loops at fixed
intervals (`server.rs:64-98`), and F-3/F-7 are what happens when that guess is wrong. SE makes
handoff an explicit confirmation gate with an event, not a sleep:

- **Release evidence required before the next spawn**, per resource kind:
  - child process: the process handle is **signaled** with a captured exit status (not a poll
    interval), and the Job Object reports an empty tree (Step 8);
  - container: `docker ps` no longer lists the name **and** the host-publish port refuses TCP;
  - external: not applicable — the harness never releases what it does not own (Step 1).
- **Then memory evidence**: reconcile the ledger against the NVML witness, with the settle
  window permitted **only after a release** (Step 4 rule 3), so the gate never reads a stale
  "still using 55 GB" value and never sleeps uselessly when memory is already back.
- **Then spawn immediately.** No fixed delay between confirmed release and spawn; the poll
  interval is a fallback, not the mechanism.
- **Deadline**: if release cannot be proven within `release_timeout_s`, the job ends
  `Refused { reason: release_timeout, holder }` naming the PID/container still alive, and the
  scheduler must **not** spawn a second occupant on that port (that is how a leaked
  `strata.exe` and a fresh one end up both holding VRAM).
- **Timings recorded per job**: `queue_ms`, `admit_ms`, `release_ms`, `settle_ms`, `spawn_ms`,
  `ready_ms`, `install_ms` — which also makes the swap latency story (formerly the G12 draft)
  measurable per model instead of argued about.

### 0.4 v0 scope — "start small"

| In v0 | Deferred (`TBD` where the seam is) |
|---|---|
| Boot contract per §0.2 — config → namespace check → single-instance guard → bounded reclaim (off the bind path) → bind all listeners → catalog scan; `boot_ms` measures; **zero engine spawns** | Tuning of partial-availability behavior when a listener cannot bind |
| Actor scheduler + swap state machine + jobs ledger, single-writer rule; reads are snapshots; control plane never queues behind a load | Multi-engine specifics beyond `llama` |
| Ownership by provenance (`SpawnTicket`), child-process supervision and kill-tree; Win32 Job Objects (item 6 approved) | Container label reap and WSL kill plumbing (arrive with `ninfer`, Step 8 D) |
| `llama` engine as a **template** instance, plus the mock engine for tests | `strata`, `ninfer`, vLLM/Basalt/Tabby templates (item 11 picks the first) |
| Declarative **four-category** model — WORK ×1, RP ×1, EMB ×1, AUX ×1-6 (`max_models`, default 1) — with `public_port`, `default_model`, idle policy, eviction matrix as config data (Steps 5/6) | Per-model template overrides; whether categories get retuned routinely |
| **One listener per category** (4): category-filtered catalogs, wrong-category `404` before ensure, **no-model fallback** — loaded model (MRU for AUX ×N) → category `default_model` → `400` (Step 11) | Listeners beyond the four; any `?class=`-style routing (none planned) |
| Admission: declared ledger + NVML-with-fallback witness, settle-only-after-release (item 5 pending: NVML vs `nvidia-smi`) | Learned cost feedback from observed usage |
| Proxy surface: chat/completions/embeddings, byte-for-byte SSE pass-through, `serve_model` rewrite + discovery, retry-once-on-crash, typed `ApiError` statuses, `/har/jobs`, request ids | `/har/events` SSE stream (the page polls in v0); `/har/metrics` |
| Typed config + per-field error report + atomic comment/CRLF-preserving yaml write-back; `sticky`→AUX and `is_embedding_model`→EMB aliases | Engine template schema final form; hot-edit of templates (item 9) |
| Control page: persistent header + one tab per category (WORK/RP/AUX/EMB), read-mostly + load/unload + set-default (Step 14) | Per-model tuning UI, theme polish, log-ring viewer |
| `tracing`, `thiserror`, `clippy -D warnings`, `cargo fmt`, unit + mock-engine tests; T1..T14 ported as `#[ignore]`d operator-gated tests | Tray (its own milestone, Step 15) |
| | Externals/register display list, protected containers (Step 13) — NInfer/maralab territory |


Explicit **non-goals** (kept from v3.x, restated so they are not re-litigated later):

- Not a multi-model concurrency manager: one model per slot, and slots are few (9 at AUX ×6).
- Not a job queue or a cluster scheduler: single machine, single GPU, one user.
- No state restore across harness restarts: Job Objects kill owned engines with the harness
  and boot loads nothing (preload scrapped, §0.2), so a restart always begins with empty
  slots — every reload is on demand.
- Not a gateway to remote or hosted inference (operator decision 2026-10-24): every slot
  occupant is a service served on this machine, so "engine" always implies a local process or
  container and a real VRAM cost. Zero-cost remote adapters are out of scope.
- Not responsible for engines' internal behavior (quantization, KV policy, sampling) — those
  stay in `llama.cpp` / Strata profile JSON / NInfer autostart scripts, which remain the
  source of truth (v3 §16 K6).
- Not a security boundary against a hostile local user: it is a loopback tool with
  DNS-rebinding/CSRF guards, not an auth system.

---

## 1. Step 1 — Ownership model (what the harness may do)

Every later decision depends on this one, so it comes first.

**Decision.** What is the harness's authority over processes, ports, and memory?

| Option | + | - |
|---|---|---|
| A. Universal owner — harness may kill anything on its ports (v3.0/§16 era) | Simple; no orphans ever | Killed the operator's manual servers and `mara-lab`; catastrophic with a live agent serving its own session from :8003 |
| B. Provenance-tagged ownership — harness acts only on what it spawned in this lifetime, recorded per resource (v3.3 K1) | Safe; matches operator reality where manual servers coexist | Leaks are possible if provenance is lost; needs an explicit register/unregister vocabulary |
| C. Namespace isolation on top of B — disjoint port ranges for harness-owned vs manual (v3.3 §21) | Startup can refuse to boot on collision; reaper has a bounded domain | Config surface grows; manual ports are frozen conventions |

**Choice: B + C (LOCKED).** Provenance is mandatory metadata on every owned resource, not an
inferred property, and the two namespaces are disjoint with a startup-time collision check.

**Why.** K1 ("the harness never kills what it did not start") survived three redesigns because
the failure mode is unrecoverable — it takes down the session that is driving the machine.
Inference, not guessing: an owned resource carries `{spawn_ticket, kind, pid/container,
port, generation}`; `external` and `protected_containers` are unkillable types, not
flags checked at the kill site. **Adoption is scrapped (operator, 2026-10-24): a slot
occupant is always harness-spawned — an external is display-only state and can never enter
a slot.** This makes F-6-class damage (slot table wiped, external
registration lost, tens of GB leaked) impossible by construction instead of by care.

**Consequence.** The kill path is one function that takes a `SpawnTicket`; there is no API to
kill "whatever is on port X".

---

## 2. Step 2 — Concurrency model for the scheduler

**Decision.** How is "one swap at a time, but requests keep flowing" implemented?

| Option | + | - |
|---|---|---|
| A. One global `tokio::Mutex` around the whole load (v3.3 `load_lock`) | Trivially prevents double-spawn; 20 lines | A 20-30 min NInfer load holds the lock: status page, second listener, embeddings, and every other slot's load all queue behind it; and a request task can be cancelled *inside* the critical section — this is F-1 |
| B. Per-slot locks + shared budget lock | Better locality | Lock ordering rules, still cancellation-exposed, still races on same-slot double load (F-6) |
| C. Single-owner scheduler actor: one task owns all slot/budget state; everyone else sends commands and holds a oneshot waiter | No shared mutable state at all → races like F-6 unrepresentable; a request disconnect drops the *waiter*, never the in-flight load (fixes F-1); cancel becomes a command, not a dropped task; queue policy (supersede / FIFO / priority) becomes explicit data | More ceremony: command enum, response channels; must be careful never to block the actor (engine probes go to blocking tasks with deadlines) |
| D. Database-style transactional ledger with CAS generations | Auditable | Heavy for a single-user tool |

**Choice: C (LOCKED).** One scheduler task per slot group, owning `Vec<Slot>` and the VRAM
budget; `Swap`, `Cancel`, `Unload`, `Register`, `Reload` commands in, `Result<SwapReceipt>`
out via `oneshot`. `ensure()` is never called from inside a request future — a request submits
`Swap { model, want }`, awaits its waiter, and its death is harmless.

**Why.** G1's word "reliable" plus G2. The three NO-GO findings are all concurrency/cancel
bugs: F-1 (client abort leaves an orphaned container and an empty WORK slot), F-6 (two loads
on one slot → both ack 202, no slot installed, engines leaked, external registration lost),
and F-7 (verification racing the same sampler). None of them are fixable by patching `ensure()`
because they are all "the state has more than one writer, and a writer can die mid-write".
Removing multiple writers removes the bug class. The 358-line `ensure()` becomes a state
machine whose transitions are individually testable.

**Consequences.**
- Loads have identity: `job_id`, observable at `GET /har/jobs` — which also gives the page a
  real per-panel loading state (fixes F-5, a UI symptom of the single global `loading` field).
- Cancellation is cooperative and *completes cleanup*: `Cancel` sets a flag the poll loop
  checks; the actor then kills what it spawned and re-samples, so T8 (cancel mid-load) and
  ninfer container removal are the same code path.
- Queue policy becomes a config value (G9): default `supersede` for same-slot loads
  ("rapid A→B ends with exactly B", T3.4) and `fifo` otherwise.
- Blocking work (NVML, process spawn, `wsl docker`) runs on dedicated blocking tasks with
  per-call deadlines; the scheduler never performs I/O inline.
- **Reads never probe and never mutate (LOCKED).** In v3.3 `slot_infos()` is a "read" that
  TCP-probes every slot and *reaps dead slots as a side effect* (`swap.rs:222-254`), and it is
  called on every proxied request (`main.rs:453,462,465`), every status poll (`main.rs:66`),
  `default_slot` (`main.rs:369`), the reaper (`main.rs:623`), and twice per tray refresh
  (`tray.rs:40,50`). Liveness becomes the scheduler's job on a tick; handlers get immutable
  snapshots. A `GET` that changes state is how "read a status page, lose a slot" happens.
- **Control-plane calls must not queue behind a load (LOCKED).** Today `load_lock` is held for
  the whole of `ensure()` including `wait_ready`, so a `load_timeout: 1200` model freezes
  `/har/unload` for *unrelated* slots, `/har/register`, `/har/unregister`, and the `/har/config`
  reload step for up to 20 minutes — and shutdown awaits `unload_all()` on the same lock
  (`swap.rs:419,991`; `main.rs:845`). Only commands that genuinely conflict with a swap may
  wait; unload-of-other-slot, register, and config edits are handled by the actor without
  waiting on an in-flight engine start.

---

## 3. Step 3 — Swap lifecycle as an explicit state machine

**Decision.** What are the states of a swap, and what must be true on exit from each?

**Chosen states (LOCKED):**

```
Idle -> Admit -> Quiesce -> Dispose -> Spawn -> Ready -> Install -> Serving
              \-> (refuse) AdmitFail          \-> Failed -> Reap -> Idle
Any -> CancelRequested -> (finish current step, then) Reap -> Idle
```

Invariant per transition, and it is enforced by a single `finish()` that no path can skip:
every exit path sets exactly one of {slot installed, `last_error` + `reaped: true`} — never
neither (that is exactly F-6), and never a slot without a live backing resource.

Amendment (2026-10-25, actor milestone): **unload jobs walk the same machine** and complete
`Dispose → Reap → Idle` — release confirmed, teardown done. It is the one edge unload
shares with the failure/cancel paths into `Reap`.

`Dispose → Spawn` is a **confirmation gate, not a delay** (G12, §0.3): the edge is only legal
once release is *proven* for that resource (handle signaled + job tree empty, or container gone +
port refusing), and the spawn fires immediately on that event with `release_ms` recorded. A
release that cannot be proven inside `release_timeout_s` takes the `Failed → Reap` edge and
refuses, naming the holder — it never walks into `Spawn`.

**Why.** G2 + G6. v3.3 has 21 documented fix-log entries around this code, and still ships
"slot vanished at verify" and "no slot installed, no error" states. A named machine with one
exit funnel makes "half-state" unrepresentable, which is what "reliable swap" means.

**Adjustable knobs (G9), all config:** poll interval, per-model `load_timeout`, post-kill
settle policy (Step 5), spawn retries, whether an engine's own `POST /load` is used
(Strata has one) or the harness owns the whole process.

Two fixes the state machine must carry, from the source:
- **Cancel must be observed inside a step, not between steps.** v3.3 checks the cancel flag only
  between readiness attempts (`engine.rs:190`), so an unload waits up to the full 5 s probe
  timeout, and the NInfer launch path has no cancel at all: `server::run_hidden` blocks in
  `.status()` waiting for the WSL script with no timeout and no flag check (`swap.rs:676`,
  `server.rs:174-190`). Every wait in the machine takes a `CancellationToken` and a deadline.
- **Cancelled loads must not clear the error slot they are about to report.** On cancel v3.3
  returns `anyhow!("load cancelled")` *without* `fail()`, so `last_error` is left cleared and
  the page shows nothing about a load that just died (`swap.rs:745-747` vs `:420`).

---

## 4. Step 4 — VRAM admission: from stale `nvidia-smi` to an authoritative budget

**Decision.** How does the harness decide "this fits"?

| Option | + | - |
|---|---|---|
| A. Keep v3.3: cached 5 s `nvidia-smi` sample + `need <= (free − reserve) × 1.15` | Nothing to build | Directly caused F-3 and F-7: a sample taken before CUDA released memory refused a legitimate 26 GB load, and reported 1.2 GB used while a 55 GB Strata was live |
| B. NVML in-process (`nvml-wrapper`) | No process spawn per sample (~30-60 ms and a fork/exec each, called from async code today); per-process used-memory, so the harness can attribute memory to its own children and to known externals | Adds a CUDA-era native dependency (present on this machine: driver 616.92, RTX PRO 5000 72 GB Blackwell); must handle driver-absent gracefully |
| C. Declared-budget ledger only (each model declares `vram_gb`, harness sums) | Fully deterministic, zero sampling, trivially testable | Drifts when declared ≠ real; cannot see what a manual server holds |
| D. B + C combined: ledger is authoritative, NVML is a reconciling witness with a settle window | Deterministic gate + real-world correction; catches "something else grabbed the card" as a distinct error class | Two sources must agree on units and on who wins in a race |

**Choice: D (LOCKED — NVML dependency approved, operator 2026-10-25: `nvml-wrapper`
in-process as the primary witness; automatic `nvidia-smi` fallback only when the NVML DLL
is absent, logged loudly once).**

Rules:
1. `need` = declared `vram_gb` → else `gguf_size × 1.15 + 2` (keep v3.3 formula) → else refuse
   as `under-specified`, and say so in the message (F-7's misleading "blocked by AUX (external)"
   was a message that lied about the reason).
2. `budget` = 72 GiB class total − `vram_reserve_gb` − Σ(harness ledger) − Σ(observed external
   reservations, from NVML per-process or `mem.external_reserved_gb`).
3. Before any gate evaluation there is a **settle** step, and settle is only legal *after a
   release* (the exact rule the test notes hammered: "settle-gates must only apply AFTER an
   unload, never before a load"): poll NVML until `used ≤ expected_baseline + tolerance` with
   deadline; on timeout, refuse with `admission_timeout` and the numbers.
4. The gate is evaluated against the ledger; NVML is used to invalidate the ledger when it
   disagrees beyond tolerance, which re-samples rather than trusting the cache. No decision is
   ever taken on a sample older than one settle window.
5. `vram_guard: false` keeps an escape hatch, logged loudly on every load.

**Why.** G3. The card is 72 GB and the models are 26-56 GB: one wrong gate decision is either a
refused legitimate load or a driver-level OOM that takes the agent session with it. Sampling
alone cannot be authoritative for a device whose release latency is seconds; a ledger alone
cannot see a neighbor. The combination, with settle-before-gate, is what removes F-3/F-7.

---

## 5. Step 5 — Slot model: four categories, AUX multi-slot (LOCKED)

**Decision.** Are WORK/RP/AUX three hardcoded categories, or data?

| Option | + | - |
|---|---|---|
| A. Hardcoded 3 slots (v3.3) | Matches the current mental model and the yaml `category:` values; page has 3 cards | `enum Category {Work,Rp,Aux}` appears in ~10 places, `idx()` mapping, eviction matrix table, preload order, idle rules — adding a 4th is a code change (violates G9, and G8 in spirit) |
| B. Declarative slot list in `simplehar.json`: `{id, public_port, default_model, priority, evictable_by, idle_timeout, ports, max_models}` | Slots become data; the eviction matrix is literally the config; per-slot idle policy already exists as special cases and becomes uniform | Migration of existing yamls; UI must render N cards; the eviction rules need a formal statement to stay safe |
| C. Free-form weighted scheduler (no slots; priority per model) | Maximum flexibility, best big-O packing | "Which model answers on 8009 right now?" stops being obvious, and the singleton-per-slot rule (a genuine invariant) is lost |

**Choice: B — LOCKED (operator, 2026-10-24): four categories, AUX multi-slot.** The default
slot config ships WORK ×1, RP ×1, EMB ×1, AUX ×N — **N adjustable 1-6, default 1** (v3.3
behavior). The option-B schema's `max_models` is exactly this knob: a category with
`max_models: N` materializes as N singleton slots, each occupant with its own port from the
slot's range. Hardcoding (option A) cannot represent this, so B follows by necessity. The
invariant "one model per slot" survives unchanged — AUX is N slots, not one slot with
capacity. What becomes data: the category list, `max_models`, the eviction matrix, and each
category's `public_port` + `default_model` (Step 11).

**Embeddings become the fourth category, EMB (operator, 2026-10-24).** `category: EMB` is
canonical; v3.3's `is_embedding_model: true` is accepted as an implied-EMB alias with a
deprecation note in the config report (Step 9) — the routing class and the slot category are
one fact, stated once (Step 10).

**Why.** G1 says *adjustable* swap; G9 says config not rebuild. The categories encode real
semantics (WORK = never idles, protected; RP = evictable by WORK; AUX = evictable by all,
now several at once; EMB = the embeddings surface), and that policy is exactly the part an
operator wants to retune as the model lineup changes. Option C stays rejected because the
singleton slot is the mechanism that makes behavior predictable, which is what "reliable" buys.

**AUX fill/replace rule (extends v3.3 same-category replacement to capacity):** a load into a
category with a free slot occupies it; when all AUX slots are full, a new AUX load replaces
the LRU AUX occupant. A WORK/RP load evicting AUX picks the LRU AUX occupant (Step 6
tie-breaks).

---

## 6. Step 6 — Eviction policy

**Decision.** Which victims, in which order, when a load does not fit?

| Option | + | - |
|---|---|---|
| A. Fixed order AUX→RP→WORK with per-category `evictable_by` (v3.3) | Predictable, documented in the README table, matches operator intent | Never considers cost: may kill two small AUX models when one 12 GB unload would have sufficed |
| B. Cheapest-sufficient set: choose the minimum-total-GB victim subset that satisfies the gate, tie-broken by (priority, LRU) | Fewer collateral unloads; optimal for the common "just barely doesn't fit" case | Surprising to an operator who expects "AUX first"; needs the ledger (Step 4) to be trustworthy |
| C. B constrained by A's hard rules (never external, never protected, never `evictable_by` violation) | Predictability where it matters, minimality where it is safe | Needs a clear explanation in the refusal message (G6) |

**Choice: C (LOCKED).** Hard constraints from A are non-negotiable filters; among survivors,
pick the cheapest sufficient set with (slot priority, LRU) tie-breaks; release victims one at a
time with re-settle between releases (kept from v3.3, which is correct). With AUX ×N
(Step 5), AUX eviction candidates are all AUX occupants and the same tie-break picks among
them — capacity changes how many are candidates, not who is evictable.

**Why.** G1's "simple and reliable" means the operator can predict it, so A's rules stay
visible; but A alone wastes working sets, which on 26-56 GB models costs minutes of reload.

**Refusal message contract (part of G6):** `503` carries `{need_gb, free_gb, reserve_gb,
victims_considered, blockers:[{name, reason: external|protected|insufficient_privilege}]}` plus
the human line ("only a WORK request can evict WORK", "X is external — stop it yourself").

---

## 7. Step 7 — Engine adapters

**Decision.** How are llama.cpp / Strata / NInfer represented?

| Option | + | - |
|---|---|---|
| A. `enum Engine {Llama,Strata,Ninfer}` with `match` in launch_command / wait_ready / kill (v3.3) | No dynamic dispatch; exhaustive matching catches new variants at compile time | Engine-specific keys and quirks leak into shared modules (`strata_config`, `ninfer_autostart`, `is_bonsai` special cases in registry); adding an engine = editing 5 files (violates G8) |
| B. `trait EngineAdapter { fn spec(&self, &ModelDef) -> Result<LaunchSpec>; async fn probe(...); fn kill(...) }` + a small registry keyed by `engine:` name | Engine knowledge is one module each (G8); unit-testable against a fake; per-engine readiness types stay local | Requires `dyn`/generic plumbing and a decision about async in traits (use `async-trait` or return futures explicitly) |
| C. External plugin (DLL/WASM) | True extensibility | Absurd for one user; ABI/dependency pain |

**Choice: B (LOCKED).** A `LaunchSpec { argv, cwd, env, log_name, readiness, namespace }`
value object, produced by the adapter and consumed by a *single* spawn/kill/reap implementation
that knows nothing about engines. Three built-ins: `llama` (child process, `/health`),
`strata` (`python -m serve.server --config … --port …`, child tree, `/health` + `loaded: true`
when reported), `ninfer` (`wsl -d … -- bash autostart.sh <profile> <publish_port>`, container,
`/health` then `/v1/models` 200).

**Amended 2026-10-24 — the adapter is data-first, code is the escape hatch (G1 "adding an
engine").** The operator's target is "add an engine = add an engine config template + a model
config". So the primary artifact is an **engine template** resolved by serde, not a Rust file:

```toml
[engine.vllm]
launch      = ["wsl", "-d", "{distro}", "-u", "{user}", "--", "bash", "{autostart}", "{profile}", "{publish_port}"]
cwd         = "{cwd}"
env         = { }
liveness    = "container:{container}"          # or "child" for spawned processes
readiness   = { get = "/health", expect = { status = 200 }, then = { get = "/v1/models", expect = { status = 200 } } }
kill        = { container = "docker rm -f {container}", child = "job-object" }
cost        = { declared = "{vram_gb}", fallback = "gguf-size-x1.15+2" }
namespace   = "harness"
log         = "logs/{name}-{epoch}.log"
```

Templates cover vLLM-in-WSL, Basalt, and Tabby without a rebuild; an engine whose readiness or
cost genuinely needs logic registers a Rust adapter for that name (the trait stays, because the
mock/fake engine in tests is exactly such an adapter). **`TBD`: template schema — placeholder
set, validation, whether model yaml can override individual template fields, and where templates
live (one `engines.toml` vs a directory).** Design constraint now, cheap later: `LaunchSpec` must
be constructible from data alone, so no adapter may hide a decision that a template could have
expressed.

**Why.** G8 + G2: the more the shared code is engine-blind, the fewer per-engine special cases
can leak into the scheduler (v3.3's `is_bonsai` substring match is exactly such a leak, and it
had to be fixed twice: first-come labeling, then explicit `bonsai: true`).

**Kept from v3.3 because they are correct:** never duplicate the docker/serve command lines —
run the operator's scripts, so the installation guide stays the source of truth (§16 K6);
never prefix docker publish with `127.0.0.1:` (breaks the WSL→Windows relay); pass the harness
publish port as arg-3 to `autostart.sh` and leave manual `manual-start.sh` untouched.

**Readiness becomes a typed adapter contract, not string heuristics.** v3.3's `wait_ready`
decides readiness with `loaded.is_none() || loaded.as_bool().unwrap_or(true)`
(`engine.rs:208-226`), which means a Strata that answers non-JSON, or answers
`"loaded": "false"` as a *string*, is declared ready; it also spends two `/health` round-trips
per poll on Strata and floors any `load_timeout` below 30 s at 30 s with no upper cap
(`engine.rs:187`). The SE adapter returns `Ready | NotYet {reason} | Failed {reason}` and the
scheduler decides what a missing field means, per engine, in one place — with the reason string
surfaced in the job record (G6).

---

## 8. Step 8 — Process supervision and orphan prevention

**Decision.** How do we guarantee no orphaned `llama-server.exe` / `strata.exe` / container?

| Option | + | - |
|---|---|---|
| A. `std::process::Command` + `taskkill /T` wrappers (v3.3) | Known to work; tree kill via `/T` | Blocking on the reactor today; a harness crash leaves children; taskkill is a spawned process per kill |
| B. `tokio::process` + `kill_on_drop` | Async, no reactor blocking, drop-safety | `kill_on_drop` does not cover a hard process abort, and does not kill child *trees* (Strata spawns `strata.exe` + `strata-vision.exe`) |
| C. B + Win32 **Job Object** per owned engine tree (`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`) | Kernel guarantees the whole tree dies when the harness handle closes, including after a harness crash; no polling needed for liveness | `unsafe` + Windows-specific (contained in one module with a documented invariant; AGENTS.md permits with justification) |
| D. For containers: label every harness-started container (`--label simplehar=<instance_id>`) and reap by label at startup | Exact attribution, no name guessing; makes F-1's orphan container recoverable | Requires passing the label through the operator's `autostart.sh` (arg or env) |

**Choice: C + D (LOCKED — Job Objects approved, operator 2026-10-25).** One `process`
module owns: spawn (hidden, CREATE_NO_WINDOW,
stdout/stderr → `logs/<name>-<epoch>.log`), attach-to-job, kill-tree, liveness, and startup
reap-by-ticket/label. **Coverage boundary (recorded with the approval):** a Job Object kills
the Windows child tree only — for the `ninfer` path, `wsl.exe` dies with the job but a
`docker run -d` container inside the WSL VM survives it, so there the label reap (D) is the
*primary* orphan guarantee and the startup reap is mandatory, with docker control invoked
via `wsl -d <distro> … docker …`, not a Windows-side docker CLI. Blocking calls live on
`tokio::task::spawn_blocking` or dedicated
services — never in a handler (AGENTS.md, and v3.3 violated it: `netstat`/`tasklist`/
`nvidia-smi`/`thread::sleep` inside request paths).

**Why.** G2 + G4: orphan cleanup must not depend on the harness being alive to ask politely,
and reaping must never be able to hit a foreign PID (v3.3's tasklist image-name guard was a
band-aid on name-based attribution).

**Coverage bug this fixes outright.** `reap_orphans` sweeps work/rp/aux (18009-18011) plus
`aux_port+1 ..= aux_port+5` (`server.rs:116-119`) — i.e. the removed v2 resident range — while
`AppConfig::harness_ports` claims ownership of `harness_ninfer_base .. +5` (18204-18209) and
18203, which are **never swept** (`appconf.rs:114-119`). So a leaked `strata.exe` or
harness-started container survives every restart, which is exactly what F-1 left behind. In SE
the owned-resource set is derived from one place (the config's slot/engine port plan plus the
label/ticket registry), so "what we own" and "what we reap" cannot diverge. Port discovery uses
`GetExtendedTcpTable` (or a strictly parsed `netstat` with column matching, never
`line.contains(":18009 ")` + `split_whitespace().last()`), and attribution comes from the ticket
or label, not from an image-name comparison via a substring search in `tasklist` CSV
(`server.rs:100-113,139-149`).

---

## 9. Step 9 — Configuration: schema, truth, and hot reload

**Decision.** Where does state live and how is it edited?

| Option | + | - |
|---|---|---|
| A. Keep hand-rolled flat `key: value` parser + `set_key` writer (v3.3) | Zero deps; byte-compatible with the operator's 35 yamls and with `llama.cpp\start.ps1`, which parses the same file | Silent typos, no validation, "unknown key" invisible; write-back must preserve ordering/comments by hand; F-4-class drift |
| B. `serde` + `serde_yaml`/`serde_json` typed models with `deny_unknown_fields`-style validation, `#[serde(default)]`, and a separate preserved-raw map for passthrough keys | Type-checked at parse; errors reported per file at startup and on rescan (G6); removes 100+ lines of bespoke parsing (AGENTS.md "use the crate") | Must still round-trip unknown llama keys without destroying them, and must not reformat the operator's files |
| C. Move page-editable flags out of yaml into an overlay (`ModelConf/*.state.json`) | Write-back becomes trivial and atomic; operator files never rewritten | Two files per model, and it breaks the v3.3 promise "the yaml is the source of truth" |

**Choice: B (LOCKED) — typed schema + raw passthrough + atomic in-place write.** Keep yaml as
the single per-model truth (G4/G5 expectations, and the operator edits these files by hand).
Write-back uses read-modify-write of the raw lines so comments and ordering survive; the file
is replaced atomically (temp + rename) and revalidated after write, with the old bytes restored
on failure.

**Also LOCKED:**
- Startup and `/har/rescan` produce a `Vec<ConfigError>` (file, key, reason) exposed at
  `/har/status` instead of being logged and forgotten; a bad file is skipped, never fatal.
- Hot reload: keep "re-scan on demand + short TTL", and add a filesystem watcher
  (`notify`) that invalidates the cache — no restart (G4), no per-request `read_dir` storm.
- `simplehar.json` typed with `#[serde(default)]`; startup validates: `internal_host` is a
  numeric IP, harness ports ∩ `protected_ports` = ∅, per-slot port uniqueness, docker port
  conventions.
- `ModelConf` keeps 100% backward compatibility with the existing 35 files; `sticky: true`
  stays accepted as the documented AUX alias and `is_embedding_model: true` as the
  implied-EMB alias (Step 5), each with a deprecation warning in the report.

**Source-verified reasons this step is not optional.** The hand-rolled parser stores quotes and
inline comments literally, so `name: "foo"` breaks every name match and
`context: 4096 # 32k` is passed to `llama-server` as one argv token (`yamlcfg.rs:11-23`);
lines whose key is not `[A-Za-z_][A-Za-z0-9_]*` are dropped **silently** (`:18-23`); the writer
rewrites the whole file on every checkbox click, normalizing CRLF→LF (`:56-59`), it is not
atomic (`fs::write`, `:60`), a `set_key` failure is only `eprintln!`-ed while the handler still
answers 200 (`main.rs:214-217` vs `:236`), and a key inside a commented block is treated as a
real key and clobbered (`:36-47`). Separately, `AppConfig::load` discards the *entire*
`simplehar.json` and runs on built-in defaults when any single field has the wrong shape
(`appconf.rs:155-161`) — a silent downgrade of the whole safety policy (ports, reserve,
protected lists) from one typo. SE fails fast or reports per-field, never silently defaults.

---

## 10. Step 10 — Catalog, identity, and `serve_model` self-heal

**Decision.** How are models addressed and how is the upstream id discovered?

- Harness name (`name:`) is canonical. Accepted aliases kept: GGUF stem, GGUF path,
  case-insensitive (G5, v3.3 §3).
- **The upstream id is discovered, not asserted (fixes F-4).** At `Ready`, the adapter's
  `GET /v1/models` result is compared with `serve_model:`; on mismatch the harness takes the
  engine's real id for rewriting and records the discrepancy in the status report and log.
  A yaml that claims an id the engine never reports becomes a visible warning at load time,
  not a 404 at request time.
- Classification (chat / embedding) comes from one function over the typed model, and
  **embedding is the category EMB** (operator, 2026-10-24): the routing class and the slot
  category are one fact, stated once — `is_embedding_model: true` is an implied-EMB alias
  with a deprecation note. Externals
  are not a class but a display-only registry entry (Step 13). **The bonsai class is scrapped
  (operator, 2026-10-24):** no `bonsai:` yaml key, no bonsai listener, no substring heuristic
  to get wrong (Appendix A D7 becomes obsolete). A former bonsai model, if still used, is an
  engine-class model like any other strata/ninfer entry. The v3.3 §20.1 lesson stands: one
  classification function used by catalog *and* `served_on`.
- `size_gb` / `vram_gb` computed once per scan and cached with the scan, not per request.

**Why.** F-4 cost a FAIL in the test run and was "NOT a race": the registry and the engine
disagreed and nothing noticed. Discovery closes it; the rest is single-source-of-truth hygiene.

---

## 11. Step 11 — Public surface and routing

**Decision.** One port with class filtering, or per-class listeners (v3.3 has 8009 main /
8030 embeddings / 8016 engine / 8017 bonsai)? Historical framing — superseded by the
category-listener decision below.

| Option | + | - |
|---|---|---|
| A. Keep 4 listeners with class-filtered catalogs and 404-before-ensure | Already the documented contract for clients and for `served_on`; wrong-class requests cannot trigger loads | Four bind points × guard × catalog filter; the classification of "which port serves what" is policy duplicated across code and docs |
| B. Single port + `model` field + optional `?class=` / header | One bind, one guard, one catalog | Breaks the existing client setups (embedding base URL is in use) and re-opens "wrong request loads a 55 GB model" |
| C. Listener set generated from slot/port config (data-driven, defaults = today) | Keeps A's compatibility, drops the hardcoding, lets the operator add a listener for a new class without code | Router construction becomes dynamic; needs careful guard tests |

**Choice: C — LOCKED (operator, 2026-10-24, listener note): one listener per category.**
Each category (WORK / RP / AUX / EMB, Step 5) gets its own public listener whose catalog
lists exactly that category's models. The v3.3 class-based split (main / embeddings /
engine, and bonsai before it was scrapped) is **replaced by category filtering** — an
engine-backed model is served on its category's listener like any other. Default port
numbers are config (SE runs its own port set, HANDOFF §3): one `public_port` plus one
`default_model` per category entry (Step 5), validated so the default names a model in that
category's catalog — a per-field config error otherwise (Step 9).

Kept regardless (these are proven invariants, G5/G7):
- wrong-category → `404` with the listener hint **before** any ensure;
- unknown model → `404` listing only that listener's ids (no cross-category disclosure);
- guard middleware: Host/Origin allowlist (DNS-rebinding/CSRF) → `400`/`403`, mutating
  `POST` requires `Content-Type: application/json` → `415`, `/har/*` and the control page on
  one designated category listener (default WORK; a config field on the category entry);
- `/v1/models` and `/har/status` never take the swap lock (actor read path, Step 2);
- body size limit, request timeouts, `tower` middleware (timeout, trace, compression) per
  AGENTS.md;
- SSE pass-through byte-for-byte with backpressure, no JSON round-trip on the stream;
- retry-once-on-crash: reap → reload → retry exactly one time, then surface `502` with the log
  path (T10.1).

**No-model fallback — LOCKED (operator, 2026-10-24; resolves F-2, settles §20 item 4).**
A JSON body without a `model` field on a category listener means "use this category":

1. A model is loaded in the category → serve it. For AUX ×N with several occupants, the
   **most recently used** serves — MRU is already tracked for the LRU tie-breaks (Step 6).
2. Nothing loaded → load the category's **default model** as a normal swap job (admission,
   eviction, `job_id`, idempotent — concurrent no-model calls while empty dedup onto the
   same job, Step 12), then serve.
3. No default configured → `400` naming the listener and how to set a default. Never guess.

v3.3 context: it returned `400` for every no-`model` body, silently killing the documented
"no-model falls back to the slot of that listener" behavior (T5.2/5.3 FAIL = finding F-2).

---

## 12. Step 12 — Control-plane API shape

**Decision.** Keep v3.3's route set, or restructure around jobs?

- Keep every existing route and status code so current clients and the test plan keep working
  (G5).
- Add `GET /har/jobs` and `GET /har/jobs/{id}`: swap jobs with state, trace id, timings,
  need/free numbers, victims, and terminal result — this is what `loading: bool` always
  pretended to be, and it is what makes the page's per-panel state derivable (F-5) and
  incidents explainable (G6).
- Make every mutating call idempotent and safe to double-click: a second identical `Load`
  returns the existing `job_id` (`already_loading`) rather than racing it.
- Add a request id (accepted or generated) echoed in the response header and in every log line
  for that request (G6).
- Strict body parsing everywhere it was learned the hard way: unparseable `POST /har/unload`
  is `400`, never implicit unload-all (v3.3 CRITICAL fix).
- Handlers return `Result<Response, ApiError>` with `ApiError: From<HarError>` — the AGENTS.md
  convention, and the single place where the `thiserror` taxonomy maps to status codes.

**Status codes must come from types, never from message text (LOCKED).** v3.3 picks the code by
substring-matching human-readable strings: `load_error` maps a message containing `"load
failed"`/`"unreachable"` to 502, `"external"` to 409, else 503 (`main.rs:498-507`), and
`register_err` matches `"not answering"`, `"unknown model"`, `"is not registered"` (`:318-327`).
A model literally named `not-answering-model` changes the HTTP status of a request about it.
Related inconsistencies to settle in the same pass: `405` is never produced (a `GET /har/load`
falls through to the 404 handler, `main.rs:486-495`); `413` has two different body shapes
because proxy bodies are limited by hand at 1 GiB while `/har/*` bodies hit axum's 2 MiB
`DefaultBodyLimit` and return plain text (`main.rs:24,389-392`); a response-builder failure
returns `502` with an *empty* body (`proxy.rs:95-98`); the proxy error body omits `code`, unlike
every other error (`proxy.rs:101-110`); and `/har/load` reports `already_loaded` on a name match
while ignoring category, so a model parked in the wrong slot answers "already_loaded" and is
never moved (`main.rs:270` vs `swap.rs:403-405`).

---

## 13. Step 13 — Externals and manual servers (adoption scrapped)

**Adoption is scrapped (operator, 2026-10-24).** v3.3's `Proc::Adopted` — a foreign server
parked in a harness slot, displayed as an occupant, proxied but never killed — is gone, and
with it `adopt_externals`, `moved_adopted`, "Load on an external = register if answering",
and unload-on-external guidance. In SE a slot occupant is always harness-spawned, so
A3-class "registration silently lost" damage is unrepresentable, and a client reaches a
manual server **on its own port, never through a harness slot**.

What remains is display and accounting only:

- `/har/register` + `/har/unregister` manage the **externals display list**: a registered
  external is health-checked once, shown in `/har/status` (and optionally in the catalog with
  `external: true`) with the `serve_model` id it actually reports, may declare `vram_gb` for
  the admission ledger (Step 4 rule 2), and is never started, stopped, reaped, evicted, or
  routed by the harness.
- The `auto_register_externals` watcher, if kept, only adds/removes display entries.
- Provenance is checked by type, not by convention (Step 1); "something already owns this
  harness port" is the distinct named error `stale_engine_on_port` — there is no adopt path
  at all for it to fall back into.

---

## 14. Step 14 — Front end (control page)

Per AGENTS.md web rules (G7): **Pico CSS + vanilla JavaScript, no jQuery/React**, a separate
CSS file, adaptive light/dark with a toggle, modern typography with an explicit header/body
font, no deep computation in JS (the page only renders `/har/*` JSON).

Concrete deltas from v3.3's `index.html`:
- **layout (operator, 2026-10-25, on the mockup): a persistent header that is always
  visible** — title + loaded count, the endpoint line, one slot card per category with its
  occupant and Unload, and the GPU VRAM / RAM / GPU-util bars (the v3.3 look, kept) — with
  the **model tabs below it** (WORK / RP / AUX / EMB); scrolling a model list never hides
  slot state or the memory numbers. At AUX ×N the AUX card **splits horizontally into one
  slim row per occupant** — row height just enough for the model name and its own Unload
  button — instead of one card per slot (operator, 2026-10-25);
- each tab lists exactly its category's models. The v3.3 Cat dropdown and the Emb / Preload
  columns are gone — category is tab membership, EMB is a category, preload is scrapped; the
  CPU checkbox stays;
- each tab names its category's listener port and **default model**, with a "set default"
  control that writes the category entry (Steps 5/9/11);
- per-panel `loading` derived from `/har/jobs`, not from the global flag (F-5);
- the memory bars labeled "observed" vs "budget" (F-7's lesson: numbers that look authoritative
  but are not), with the sample age (`age_s`) shown;
- last refusal rendered from the structured `blockers` list (Step 6), not a free-text string;
- mouse support and correct scroll-offset math for any hit-testing (AGENTS.md);
- polling replaced by one SSE stream (`GET /har/events`) with a polling fallback, so the page
  stops issuing N requests every 5 s while remaining functional if the stream drops.

---

## 15. Step 15 — Tray icon

Keep `tray-icon` (it is the only GUI dependency and it works; no Qt/egui). States gray/amber/
green per slot occupancy; menu Open page / per-slot unload / unload all / Exit.

Change: the tray must not be allowed to become a second source of truth. It renders the same
`/har/status` snapshot the page uses (via an in-process channel, not a re-derivation), and its
menu items send the *same* scheduler commands as the HTTP handlers (Step 2) — no tray-only
code path. Shutdown fan-out uses one `watch::channel<bool>` shared by all listeners (v3.3 §20
lesson: a single `Notify` can only wake one waiter).

---

## 16. Step 16 — Observability

| v3.3 | SE |
|---|---|
| 29 `println!` / `eprintln!`, 0 `tracing` | `tracing` + `trading-subscriber` with `EnvFilter` (`SIMPLEHAR_LOG`), structured fields (`job_id`, `model`, `slot`, `need_gb`), no `println!` (AGENTS.md) |
| per-engine log files only | keep `logs/<name>-<epoch>.log` per engine **and** a bounded in-memory ring of recent harness events exposed at `/har/log?limit=`, so a page reload can show why the last load failed without opening files |
| `last_error` single slot | error history (last N) with trace ids, plus current `last_error` for compatibility |
| no metrics | optional `GET /har/metrics` (Prometheus text) — cheap, and it is the only way to see swap latency in aggregate |

Log hygiene: rotate/cap per file, total `logs/` budget checked by the soak test (T14's
< 50 MB requirement), and never log api keys (`ninfer_key: true` flows must be redacted).

---

## 17. Step 17 — Errors and types

- One `thiserror` enum per layer: `ConfigError`, `RegistryError`, `AdmissionError`,
  `SpawnError`, `ReadyError`, `OwnershipError` (the "you don't own it" refusal gets its own
  type — G4 deserves a distinct class), `ProxyError`; `anyhow` only at `main`.
- Handlers `Result<Response, ApiError>` (AGENTS.md).
- Newtypes instead of loose primitives: `ModelName`, `SlotId`, `Port`, `Gibibytes`, `JobId`,
  `SpawnTicket`, `EpochMs`. This is what turns several v3.3 bug classes into type errors
  (e.g. mixing a harness port with a manual port, or a stale slot generation).
- No `.unwrap()` on any path reachable from I/O; `.expect("invariant: …")` only for
  programmer errors, with a message.

---

## 18. Step 18 — Module layout

```
src/
  main.rs            cli, runtime, startup validation, shutdown
  config/            simplehar.json schema, validation, defaults
  model/             ModelConf schema, typed yaml + raw passthrough, atomic write-back, scan/watch
  catalog.rs         identity, alias resolution, classification, catalog_json
  engine/            trait EngineAdapter + launch spec
    llama.rs  strata.rs  ninfer.rs  (fake used in tests lives in tests/)
  process.rs         spawn/job-object/kill-tree/liveness/reap-by-ticket (the only unsafe)
  budget.rs          ledger + NVML witness + settle
  scheduler.rs       the actor: commands, state machine, jobs, queue policy
  ownership.rs       SpawnTicket, provenance, protected sets
  api/               routers, extractors, guards, handlers, SSE, ApiError mapping
  webui/             index.html + css + js (served via include_str!)
  tray.rs
  observability.rs   tracing init, ring buffer, metrics
tests/               mock-engine harness + integration scenarios T1..T14
```

Rationale: one responsibility per module (AGENTS.md), the 1 057-line `swap.rs` decomposed into
`scheduler` + `budget` + `ownership` + `process`, and the 797-line `main.rs` split into
`api/*` vs startup.

---

## 19. Step 19 — Testing strategy (how we will know G2 is met)

- **Unit**: config schema (bad file → error not panic), yaml round-trip byte-preservation,
  identity resolution, classification, admission arithmetic (property-style: any sequence of
  loads never exceeds budget; settle only follows release), eviction set-minimality,
  `serve_model` mismatch self-heal, state-machine exits (every error path installs or reports).
- **Mock engines**: an in-process axum "fake engine" implementing `/health` (with `loaded`),
  `/v1/models`, SSE chat, a `POST /load`-style slow ready (configurable delay), a
  "wrong id" mode, a "dies after N s" mode, and a "reports wrong VRAM" mode. Everything in
  Steps 2-8 is then testable without touching the 72 GB card, and tests run in parallel with
  no shared state.
- **Fault injection**: client disconnect mid-load (F-1), double `/har/load` (F-6), kill the
  child mid-load, container vanished, NVML disagreement (F-3/F-7), unload-then-immediate-reload.
- **Integration on the real box**: port T1..T14 from `TEST-SWITCHING.md` into `tests/` as
  opt-in `#[ignore]`d tests with an operator gate, keeping the verdict vocabulary
  (PASS/FAIL/FINDING/BLOCKED/SKIP) and the flakiness policy (re-run once, then classify FLAKY
  n/3 by worst observed). The v3.3 run was NO-GO with T6/T7/T8/T10/T11/T14 never executed —
  those are the tests that actually exercise the risky paths, so they become first-class.
- **Soak**: T14-style alternating switch loop asserting zero orphans and bounded `logs/`.
- Gates before any commit: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check`.
- Never benchmark in parallel, never with `RUSTFLAGS` (AGENTS.md).

---

## 20. Open decisions needing the operator

Resolved in the 2026-10-24 brainstorm: goals list (G10 dropped, G11/G12/G13/G14 added and
defined), G1 term readings (§0.1), local-only engines, declarative knobs as product surface,
engine-as-template direction, and "start small" (§0.4).

| # | Question | Recommendation | State |
|---|---|---|---|
| 1 | Goals G2-G9: confirm, edit, or add | confirm; G2/G3 are what G1 costs | **settled** — accepted, plus G11-G14 |
| 2 | Slots hardcoded (WORK/RP/AUX) or declarative with those defaults (Step 5) | declarative, default = today's three | **settled 2026-10-24** — declarative; four categories WORK/RP/AUX/EMB, AUX ×1-6 adjustable (default 1) |
| 3 | Listeners: fixed 8009/8030/8016/8017, or generated from config (Step 11) | generated, defaults = today's four | **settled 2026-10-24** — one listener per category (WORK/RP/AUX/EMB), replacing the class split |
| 4 | F-2 no-model request: proxy to the listener's slot (fallback) or `400`? | fallback | **settled 2026-10-24** — fallback: serve the loaded model, else load the category `default_model`, else `400` |
| 5 | NVML native dependency acceptable (Step 4), or keep spawning `nvidia-smi`? | NVML, with `nvidia-smi` fallback if the DLL is missing | **settled 2026-10-25** — approved as recommended |
| 6 | Win32 Job Objects with a small documented `unsafe` (Step 8)? | yes | **settled 2026-10-25** — approved as recommended |
| 7 | Keep yaml write-back as the truth for page-edited flags (Step 9), or overlay file? | keep yaml write-back | open |
| 8 | Does SE replace `C:\Inference\SimpleHar` when it passes the T1..T14 gate, or run in parallel on a different port set for a soak period? | parallel soak on a second port set | open |
| 9 | **New:** engine template schema — placeholder set, validation depth, per-model overrides, one file vs a directory (Step 7) | one `engines.toml`, per-model override of `launch`/`readiness` only | **TBD** |
| 10 | **New:** is the v0 boundary in §0.4 right — `llama` + mock engine only, tray and externals deferred? | keep as drafted | **settled 2026-10-25** — §0.4 rewritten and locked; page layout clarified: persistent header (slot cards + bars always visible), category tabs below |
| 11 | **New:** which of vLLM / Basalt / Tabby is the first real second engine after v0 | pick one before the template schema is frozen, so it is designed against a real case | **TBD** |

---

## 21. What carries over unchanged (deliberately boring)

The v3.3 invariants that the test record never contradicted: singleton-per-slot; never kill
what it did not start; WORK never idles out; protected containers; harness/manual port
namespace separation with startup collision refusal; wrong-class 404 before ensure;
`serve_model` rewrite after harness routing; single-source-of-truth engine scripts; synchronous
VRAM release before the next spawn (wait for full exit); one automatic reload-and-retry after
a crash; strict unload bodies; Host/Origin/Content-Type guards; control routes main-listener
only; per-engine log files; no restart needed to add/remove models.

---

## Appendix A — Source-verified defects in the v3.3 baseline

From a line-by-line read of `base-archive/src/*.rs` (v3.3.0). "Step" = the design step above
that removes it; "test" = the case in §19 that must pin it. Findings F-1..F-7 are the
2026-10-23 test report's numbering.

### A.1 Concurrency and state (targets Step 2, 3)

| # | Defect | Where | Step / test |
|---|---|---|---|
| A1 | `load_lock` (`tokio::Mutex<()>`) held across the entire `ensure()` body, including `spawn_blocking(...).await` and `wait_ready` up to `load_timeout` (yaml allows 1200 s) → unrelated-slot unload, register/unregister, config reload and shutdown all queue behind it | `swap.rs:419,464,511,608,700-710,740,991,1045`; `main.rs:845` | 2 / T9 |
| A2 | Unload only escapes the lock when the requested **name/category equals the loading one**; unloading any other slot waits for the whole load | `swap.rs:1054-1063,959-971` | 2 / T12 |
| ~~A3~~ | **Obsolete — adoption is scrapped in SE** (operator, 2026-10-24); no external can occupy a slot, so no registration can be dropped. v3.3: `ensure` could return `Ok(())` while a deferred `moved_adopted` external was dropped on the floor | `swap.rs:439,457,480-481` | 13 / — |
| A4 | Same-slot singleton check in `/har/load` matches name only, ignoring category → "already_loaded" while the model sits in the wrong slot | `main.rs:270`; `swap.rs:403-405` | 3 / T3 |
| A5 | Cancellation flag checked only between readiness attempts (up to 5 s lag) and never during the NInfer `wsl` launch, which blocks in `.status()` with no timeout/cancel | `engine.rs:190`; `swap.rs:676`; `server.rs:174-190` | 3 / T8 |
| A6 | Cancel path returns `Err` without `fail()`, leaving `last_error` cleared after a cancelled load | `swap.rs:745-747` | 3, 16 / T8 |
| A7 | `slot_infos()` is a read that probes and **reaps** (writes `None`) as a side effect; called from every proxied request, status, default-slot, reaper, tray | `swap.rs:222-254`; `main.rs:66,369,453,462,465,623`; `tray.rs:40,50` | 2 / T9.4, T10 |
| A8 | Five memory atomics updated and read with `Ordering::Relaxed` as a group, so `/har/status.mem` can mix values from two samples | `swap.rs:296-303,327-331,348-350` | 4 / unit |
| A9 | Retry-once-on-crash triggers on **any** 502, including one the upstream genuinely returned, and can never fire for a mid-stream death | `main.rs:461-471`; `proxy.rs:78-80` | 11 / T10 |
| A10 | No singleton-instance guard (the test run recorded a second `simplehar` PID of unknown origin) | `main.rs` startup | 8 / new |

### A.2 Blocking work on the reactor (targets Step 2, 4, 8)

| # | Defect | Where |
|---|---|---|
| B1 | `reap_orphans()` runs on the async main task before listeners bind: `netstat .output()`, N × `tasklist .output()`, N × `taskkill .status()`, and 8 × `wait_port_free(5 s)` with `thread::sleep(200 ms)` → up to ~40 s of blocked reactor at startup | `main.rs:739`; `server.rs:100-170` |
| B2 | `port_open()` is a blocking `TcpStream::connect_timeout(300 ms)` reached from async through `slot_infos`/`is_alive` on the hot request path | `engine.rs:160-170`; `swap.rs:29,222` |
| B3 | Catalog path does `read_dir` + `read_to_string` per file and `metadata` per entry on request threads, through a 2 s TTL that still misses often; `model_size_gb` uses a process-global `OnceLock<Mutex<HashMap>>` consulted twice per entry | `swap.rs:148-160`; `registry.rs:25,170-179,225-227` |
| B4 | `set_config` does read-modify-write of a yaml file (`fs::read_to_string` + `fs::write`, non-atomic, up to 3× per UI action) inside the handler | `main.rs:214-217`; `yamlcfg.rs:29-62` |
| B5 | `reqwest` built with `default-features = false` (no TLS, no decompression) and **no request timeout** — only a 3 s connect timeout, so a wedged upstream can hold a handler forever; SSE has no idle timeout on either leg | `Cargo.toml:9`; `swap.rs:123-127`; `proxy.rs:91-92` |

### A.3 Ownership, reap, and safety of the kill path (targets Step 1, 8)

| # | Defect | Where |
|---|---|---|
| C1 | Reaper domain ≠ owned domain: sweeps 18009-18011 + 18012-18016 (v2 leftovers) but never 18203 / 18204-18209, so a harness-leaked `strata.exe` or container survives restart | `server.rs:116-119`; `appconf.rs:114-119` |
| C2 | Port→PID attribution by substring: `line.contains(":<port> ")` on `netstat` text, PID = last whitespace token, image name from `tasklist` CSV found by `line.contains(pid)` (matches the PID appearing in a *different* column) | `server.rs:136-149,100-113` |
| C3 | Kill is `taskkill /T [/F]` spawned per kill, with no kernel-level guarantee if the harness itself dies | `server.rs:64-84,152-156` |
| ~~C4~~ | **Obsolete — adoption is scrapped in SE** (operator, 2026-10-24); the `Proc::Adopted` kind does not exist, so the "unkillable by type, not by convention" rule (Step 1) covers externals directly. v3.3: `Proc::Adopted` kill was a no-op by convention at each call site | `swap.rs:33-50` |
| C5 | `public_host` is never validated; bind does `format!("{host}:{port}").parse().expect(...)` — a bad host panics at startup | `main.rs:788-790,820-822` |
| C6 | Four `unsafe` blocks (Win32 message pump, `GlobalMemoryStatusEx`) live in feature/`cfg` code with no documented invariants | `tray.rs:126-137`; `server.rs:236-247` |

### A.4 Config, catalog, and proxy fidelity (targets Step 9, 10, 11)

| # | Defect | Where |
|---|---|---|
| D1 | Any one malformed field makes `AppConfig::load` throw away the whole file and run on defaults, with one warning line | `appconf.rs:155-161` |
| D2 | No quote or inline-comment handling: `name: "foo"` breaks matching; `context: 4096 # 32k` becomes a single argv token | `yamlcfg.rs:11-23`; `:69-76` |
| D3 | Keys failing `[A-Za-z_][A-Za-z0-9_]*` are dropped silently (typos and `key-with-dash:` vanish without a word) | `yamlcfg.rs:18-23` |
| D4 | Write-back is non-atomic, normalizes CRLF→LF, collapses/duplicates-match by first-line-wins, can clobber a key inside a commented block, and its failure is only `eprintln!`-ed while the handler returns 200 | `yamlcfg.rs:34-60`; `main.rs:216` |
| D5 | `cpu_offload: true` silently drops the yaml `ngl`; unknown yaml keys are silently ignored by `build_args`; `extra_args` is split on whitespace so a quoted path with a space cannot be expressed | `yamlcfg.rs:78-119` |
| D6 | Response header copy silently drops any header that fails `HeaderName::from_bytes` — an upstream can lose a header with no trace | `proxy.rs:81-90` |
| ~~D7~~ | **Obsolete — the bonsai class is scrapped in SE** (operator, 2026-10-24); there is no classification heuristic left to get wrong. v3.3: `is_bonsai` fell back to a substring scan over name/container/serve_model/model, flip-flopping a live label | `registry.rs` (`is_bonsai`, `kind_of`); FIX LOG |
| D8 | `serve_model` is trusted from yaml and never verified against the engine until a client 404s (F-4) | `swap.rs:427-431,817-821` |

### A.5 Control plane and UI (targets Step 12, 14, 15)

| # | Defect | Where |
|---|---|---|
| E1 | HTTP status chosen by substring-matching human error text | `main.rs:318-327,498-507` |
| E2 | Two different 413 body shapes (1 GiB hand limit vs axum's 2 MiB default), no 405 anywhere, one 502 with an empty body, proxy error JSON missing `code` | `main.rs:24,389-392,486-495`; `proxy.rs:95-110` |
| E3 | Single global `loading` string cannot describe per-slot loading → the page rendered one loading spinner in two panels (F-5) | `swap.rs:77`; `index.html` |
| E4 | Tray fires unloads and never observes the result (they can sit behind `load_lock`), rebuilds tooltip + probes slots twice per 5 s tick, and leaves the icon painted after its thread breaks out on Exit | `tray.rs:40-74,141-168,175-183` |
| E5 | `cmd /C start <url>` without `CREATE_NO_WINDOW` from the tray thread | `tray.rs:76-80` |
| E6 | Front end is a single 14 KB `include_str!` HTML blob with inline JS doing the derivation work, contrary to the web rules in `AGENTS.md` | `main.rs:23`; `index.html` |

### A.6 AGENTS.md compliance gap (targets Step 16, 17, 18)

| Metric | v3.3 baseline | Required |
|---|---|---|
| `clippy --all-targets -- -D warnings` | **8 errors** (`engine.rs:174,181,316`; `proxy.rs:43`; `registry.rs:67`; `main.rs:384,386,461,543`) | 0 |
| `.unwrap()` | 54 (46 in `swap.rs`) | 0 on I/O paths |
| `println!` / `eprintln!` vs `tracing` | 29 vs **0** | tracing only |
| `thiserror` error taxonomy | absent | per-layer enums |
| Unit tests | 5 test attributes in 9 modules | every public item |
| Largest function | `ensure` 355 lines, `route` 122, `register` 114, `set_config` 113 | single responsibility |
| Largest module | `swap.rs` 1 057 lines, `main.rs` 797 | one responsibility per module |
| `clone()` sites | 91, incl. `SlotInfo` cloned per proxied request and `AppConfig` cloned in 5 places | borrow / snapshot-once |

### A.7 llama flag map, verified against the operator's script

`build_args` (`base-archive/src/yamlcfg.rs:78-119`) is currently **byte-equivalent in semantics**
to `C:\Inference\llama.cpp\start.ps1:45-63`: `model→-m` (missing ⇒ refuse to launch),
`mmproj→--mmproj`, harness-forced `--host`/`--port`, `context→-c`,
`cpu_offload→-ngl 0` **suppressing `ngl`**, `ngl→-ngl`, `batch→-b`, `ubatch→--ubatch-size`,
`flash_attn→--flash-attn`, `cache_type_k→--cache-type-k`, `cache_type_v→--cache-type-v`,
`parallel→--parallel`, `fit→--fit`, `fit_target→--fit-target`, `fit_ctx→--fit-ctx`,
`jinja→--jinja` when truthy, `extra_args` split on whitespace. Empty values are skipped by
`push` (`:69-75`), which also means `ngl: 0`-style falsy-but-valid values survive and
`context: ""` silently vanishes.

Consequence for the llama adapter (Step 7): this table becomes a declarative
`&[(&str yaml_key, &str flag)]` with the two special cases (`cpu_offload`, `jinja`) as data, and
a unit test asserts parity against the parsed text of `start.ps1` so the harness cannot drift
from the script the operator maintains.
