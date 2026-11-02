---
title: Current architecture
owner: operator
status: active
audience: [contributor, agent, architect]
source_of_truth_for: [current architecture]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release, new-decision
---

# OVERVIEW.md

Up: `docs/README.md`. This file describes the architecture as shipped,
verified against `src/` and `Cargo.toml` on 2026-11-02. Binding rules live in
`docs/architecture/DOCTRINE.md`; rulings and rationale live in
`docs/architecture/DECISIONS.md`. This file summarizes and links; it does not
duplicate either ledger. User-facing setup and the full HTTP and model-schema
tables live in `README.md`.

## 1. System context

- SimpleHar SE is a Windows tray harness: one Rust crate
  (`simplehar-se`, `Cargo.toml`), one process, one NVIDIA GPU, several local
  LLM inference engines (`llama`, `strata`, `ninfer`, plus a test-only `mock`).
- It owns engine processes as its children, swaps models between four
  category listeners through one single-writer scheduler, and admits loads
  against the card's live reading. It refuses with named blockers rather than
  overcommit (`docs/architecture/DOCTRINE.md`).
- The harness is the only writer of engine configuration. Engine binds,
  ports, and keys are harness-forced, never read from engine-side files.
- Key runtime dependencies (`Cargo.toml`): `tokio` (async runtime), `axum`
  with `tower-http` (HTTP), `reqwest` (proxy and readiness probes),
  `nvml-wrapper` (GPU witness), `tray-icon` plus the `windows` crate (tray,
  Job Objects, single-instance mutex), `tracing` (logging), `serde`
  (config and API), `base64` (ninfer script staging).
- Engine software itself (llama.cpp, strata, the ninfer container) is
  external. The harness launches and supervises it; it does not contain it.

## 2. Process and actor model

One harness process, one tokio runtime, plus OS-level children:

- `src/main.rs` - entry point. Boot order: config first, single-instance
  guard, listeners bound before any engine work, zero engine spawns at boot
  (`process::SingleInstanceGuard::acquire` rejects a second instance).
- Scheduler actor - `src/scheduler/mod.rs` (`Scheduler`, `SchedulerHandle`).
  One task owns all slot and budget state; callers send
  `scheduler::vocab::Command` values and hold a oneshot waiter. The actor
  never performs engine I/O inline: each step runs as a spawned task whose
  result returns as an event, so status and cancel calls never queue behind
  a load.
- HTTP tasks - per-category public listeners plus one control listener, all
  served from `src/api/mod.rs` over axum routers (`api::ApiState`).
- Engine children - spawned by `src/process.rs` (`ProcessRuntime`). Each
  owned tree lives in a Win32 Job Object with `KILL_ON_JOB_CLOSE`, so a
  harness death kills the tree (the crash-proof orphan guarantee). Ninfer is
  the documented carve-out: the child is a short-lived `wsl.exe` client; the
  container is reaped by name with `docker rm -f` and reconciled pre-spawn.
- Tray - `src/tray.rs`. The tray is a view: it renders the scheduler snapshot
  and the listener registry and sends the same commands the HTTP handlers
  send. No tray-only state.

## 3. Scheduler and job lifecycle

- Commands (`src/scheduler/vocab.rs`, `Command`): `Swap`, `Restart`,
  `Cancel`, `Unload`, `UnloadModel`, `UnloadAll`, `Reload`.
- Job state machine (`vocab::SwapState`, LOCKED graph, checked by
  `transition_allowed`):
  `Idle -> Admit -> Quiesce -> Dispose -> Spawn -> Ready -> Install ->
  Serving`, with `AdmitFail`, `Failed`, `Reap`, `CancelRequested` as the exit
  paths. No skips: a job may not jump to `Spawn` (that is how double spawns
  happened, defect F-6).
- `Dispose -> Spawn` is a confirmation gate, not a delay: the edge is legal
  only once release is proven (handle signaled plus TCP-refused proof); an
  unproven release within `release_timeout_s` fails into `Reap` and refuses.
- `SwapState::reserves_vram` marks the states that hold GiB booked but not
  yet card-visible; the admission gate debits exactly that set and the page
  shows the same figure as "booked".
- Same-slot policy is supersede: a same-category load of a different model
  cancels the running job and starts when the slot returns to `Idle`. Other
  requests queue FIFO. Cancel is cooperative: the current step completes,
  cleanup runs, then `Reap`.
- Eviction follows the `evictable_by` matrix from the config
  (`config::CategoryConfig`): WORK is evicted by nobody, RP only by WORK,
  AUX and EMB by both; same-slot replacement is always allowed. The planner
  takes the cheapest sufficient victim set (`budget::VictimInfo`).

## 4. Admission and the witness

- Witness (`src/witness.rs`, `NvmlWitness`; trait `budget::VramWitness`):
  in-process NVML primary, `nvidia-smi` fallback, a 2 s success TTL
  (`SAMPLE_TTL`) and a 1 s failure cache. When neither answers, `sample()`
  errors and the caller refuses - never a stale guess. Only
  `budget::LedgerOnlyWitness` (no-GPU mode) budgets from the ledger; a dead
  witness on real hardware fails closed.
- Budget arithmetic (`src/budget.rs`): admission computes free space as
  `total - reserve - observed_used`, credits the same-slot victim, and
  debits in-flight bookings (`SwapState::reserves_vram`). `need_gb` costs a
  load as declared `vram_gb`, else GGUF size x 1.15 + 2, else
  `AdmissionError::UnderSpecified`.
- Refusals name facts, not just numbers: `BudgetFacts`, `VictimInfo`, and
  `BlockerInfo` carry `unreleasable`, `used_gib`, and `booked_total` into the
  refusal text and the job ledger (`GET /har/jobs`).
- Cost classes (`src/model/mod.rs`, `CostClass`): `fixed` (size binds),
  `greedy` (offload; a launch floor is mandatory), `exact` (engine-computed
  number required). The launch floor `launch_only_with_at_least_vram_free`
  refuses before spawn; when no release the harness can perform would reach
  the floor the refusal reads "No releases allow for the VRAM floor. Check
  foreign residents." (in `budget.rs`). Foreign occupancy never counts
  toward meeting a floor.
- Cost sources (`model::CostSource`, `MeasuredAttribution`): a measured
  figure books only for trusted provenance (`engine_report`, `per_process`);
  a `total_diff` figure displays but never books (`MeasuredAttribution::
  bookable`). The llama Fit class (`src/engine/fit.rs`) asks the engine's own
  `llama-fit-params` before spawn; a Fit figure is never persisted and is
  recomputed per proven change; the scheduler owns the cache.
- Fold gate: `budget::grade_card_occupancy` grades the movement of
  unaccounted memory across a serve; a `ForeignDominated` window mints no
  `total_diff` pair.

## 5. Engine adapters

- Contract (`src/engine/mod.rs`): `EngineAdapter` builds a `LaunchSpec`
  (argv, env, cwd) from the model row; `EngineRuntime` spawns, probes
  readiness (`ReadinessSpec`, typed `Probe`), and releases. `default_adapters`
  builds the map. `LaunchSpec` must be constructible from data alone.
- Policy table (`src/engine/policy.rs`, `EnginePolicy::all`): the compiled-in
  single-source table behind L1/L2/L3 - one row per engine, `forced_fields`,
  forbidden words, and word contracts. One mechanism, four instances.
- `llama` (`src/engine/llama.rs`): native `llama-server.exe` per GGUF. The
  flag map is data, parity-tested against the operator's start script. The
  harness forces `--host 127.0.0.1` (`LLAMA_ENGINE_HOST`) and the assigned
  `--port`. `cpu_offload` forces `-ngl 0` (llama only). `extra_args` is a
  closed allow-list, not a deny-list. The optional Fit probe is the R11
  booking source (section 4).
- `strata` (`src/engine/strata.rs`): template mode is the current and only
  mode. The row yaml owns the profile fields (`strata_*` keys); the harness
  renders the full profile JSON at every load under `.simplehar\
  strata-profiles` (`process::strata_profiles_dir`, content-addressed,
  atomic, keep-N prune) and launches only its own staged copy
  (`python -m serve.server --engine strata` with forced `--host 127.0.0.1`
  and `--port`). The external `strata_config` mode is superseded: a row that
  declares it is refused at scan (`model/scan.rs`) and at the adapter
  (`superseded_sentence`) naming `simplehar-se import strata <profile.json>`.
  Readiness needs `/health` 200 and the JSON `loaded` flag (strata answers
  200 while weights stream).
- `ninfer` (`src/engine/ninfer.rs`): GPU container inside WSL2. Template
  mode (row declares `ninfer_image`) renders the canonical autostart script
  (`CANONICAL_NINFER_SCRIPT` in `engine/policy.rs`), stages it into the
  distro content-addressed with checksum verification, and launches the
  staged copy: `--restart no`, loopback-only publish
  (`--publish 127.0.0.1:...`), bounded container logs, name-scoped pre-reap.
  The legacy `ninfer_autostart` path (operator script) is deprecated with a
  scan warning; removal rides a green Wave C (ledger row 18). Unload reaps
  by container name; the claim ledger is `ownership::ContainerClaims`
  (`ninfer-claims.json`), and `ownership::boot_reap_decision` is the single
  boot-reclaim rule.
- `mock` (`src/engine/mock.rs`): test-facing adapter. Production boot
  registers it only when config `allow_mock_engine` is true; integration
  tests build their own adapter maps.
- Custody: `src/custody.rs` (`CustodyLedger`) records what the harness
  launched for custodied engines (strata) and reconciles by pid identity at
  boot; engine-side custody is untrusted.

## 6. Configuration layers L1/L2/L3

- L1 import: engine-native artifacts (strata profile JSON, operator scripts)
  are copied in by an explicit operator verb, then forgotten. The one
  sanctioned reader is the import bridge (`src/import.rs`, `strata_yaml`;
  CLI in `src/main.rs`): `simplehar-se import strata <profile.json>` emits a
  yaml knob block and touches neither the row nor the profile. Harness-forced
  fields found in L1 are reported as dropped; the serve path never reads an
  L1 file.
- L2 intent: the hand-authored per-model records in `ModelConf/*.yaml`.
  `src/model/scan.rs` builds the typed catalog (`model::ModelEntry`): only
  regular non-symlink `*.yaml` files, identity is the file stem, a problem
  line never publishes. `src/catalog.rs` resolves names and v3.3 aliases.
  `src/model/writeback.rs` writes page edits back byte-preserving: ordering,
  comments, and unknown keys survive.
- L3 launch spec: generated from L2 plus `EnginePolicy` plus scrubbed L1;
  the harness launches only from L3 (rendered strata profile, rendered
  ninfer script, rendered llama argv). The invariant is byte-reproducibility
  from L2 + policy + L1 (see `docs/architecture/DOCTRINE.md` item 11).
- Harness config: `src/config/mod.rs` (`AppConfig`, `simplehar-se.json`,
  `SIMPLEHAR_CONFIG`-aware, shared `default_config_path`). A present-but-
  invalid file fails boot per field; a missing file is materialized at first
  start by real probes (`src/config/firstrun.rs`, never overwriting an
  existing file). `AppConfig::port_claim` is the single port-namespace
  decision (DOCTRINE item 13). `control_token` is optional;
  `SIMPLEHAR_TOKEN` beats it.

## 7. Public listeners and control surface

- Four categories (`config::CategoryId`, defaults `four_defaults`): WORK
  (8708, also the control listener), RP (8709), AUX (8710, up to six slots),
  EMB (8711). Engine ports are assigned sequentially from 18109 in category
  order.
- Listener health (`src/listeners.rs`, `ListenerRegistry`): every category
  carries a verdict - `Pending`, `Serving`, or `Down` - from registry
  construction. A bind failure or dead listener marks the category `Down`,
  the harness keeps running, the tray turns red, and `main` retries the
  bind.
- HTTP surface (`src/api/mod.rs`): each category listener serves the
  OpenAI-style `POST /v1/chat/completions`, `/v1/completions`,
  `/v1/embeddings` and `GET /v1/models` (category-filtered, deliberately
  minimal - no local paths). A body naming no model targets the category's
  occupant or its `default_model`. Proxy responses stream byte-for-byte,
  SSE included.
- Control listener: `/har/status`, `/har/events` (SSE), `/har/jobs`,
  `/har/log`, `/har/models`, `POST /har/load`, `/har/unload`, `/har/rescan`,
  and the config write endpoints (full table in `README.md`).
- Request guards run in fixed order (DOCTRINE item 7): Host/Origin/content-
  type refusals (`403`, `415`) precede the token check (`401`); only the
  page assets are unauthenticated; `GET /health` is token-gated. Error codes
  come from the `ApiError` type, never from message text.
- Control page (`src/webui/mod.rs`): Pico CSS plus vanilla JavaScript
  embedded with `include_str!`; the page renders only `/har/*` JSON and
  every action is an API call.
- Tray states (`src/tray.rs`, `tray_state`): precedence red (listener down)
  > amber (no verdict after boot grace) > yellow (swap in flight) > green
  (loaded) > gray (idle). Per-job errors ride the tooltip, never the color.

## 8. Module map

| Module | Responsibility |
|---|---|
| `src/main.rs` | boot order, wiring, first-start bind supervisor, `import` CLI |
| `src/config/` | typed `simplehar-se.json` schema, validation, `port_claim`, first-run probes (`firstrun.rs`) |
| `src/model/` | typed yaml rows (`mod.rs`), Fork-C scan (`scan.rs`), byte-preserving write-back (`writeback.rs`) |
| `src/catalog.rs` | model identity and alias resolution |
| `src/scheduler/` | single-writer actor and job state machine (`mod.rs`, `vocab.rs`) |
| `src/budget.rs` | admission arithmetic, `AdmissionError`, witnesses' types |
| `src/witness.rs` | NVML witness with `nvidia-smi` fallback, TTL caches |
| `src/engine/` | adapters (`llama.rs`, `strata.rs`, `ninfer.rs`, `mock.rs`), `EnginePolicy` table (`policy.rs`), Fit probe (`fit.rs`) |
| `src/process.rs` | Job-Object supervision, script staging, single-instance guard, log files |
| `src/ownership.rs` | `SpawnTicket` provenance, `ContainerClaims` ledger, boot-reap rule |
| `src/custody.rs` | harness-side custody sidecar for strata launches |
| `src/import.rs` | strata profile import bridge (emits yaml, applies nothing) |
| `src/listeners.rs` | listener health registry and verdicts |
| `src/api/` | public and control HTTP surface, guards, proxy |
| `src/webui/` | embedded control page assets |
| `src/tray.rs` | tray view and menu |
| `src/observability.rs` | tracing init, in-memory ring, on-disk harness log |
| `src/testutil.rs` | shared test helpers |

## 9. Data flow: one load or swap

1. A client request, a page action, or a tray command arrives; the handler
   resolves the model (`catalog::resolve`) and sends a scheduler `Command`.
2. The actor moves the slot `Idle -> Admit`: it samples the witness, costs
   the load (cost ladder, section 4), and compares free VRAM minus reserve
   against the need plus in-flight bookings.
3. If short, the planner selects victims through the `evictable_by` matrix;
   a refusal names the blockers (`AdmissionError` -> `AdmitFail` -> `Idle`).
4. `Quiesce -> Dispose`: victims release one at a time; each release needs
   the evidence pair (handle signaled, TCP refused); settle re-samples the
   card before the gate re-checks.
5. `Spawn`: the adapter builds the L3 `LaunchSpec`; the runtime launches it
   under its Job Object with scrubbed environment and log capture.
6. `Ready`: typed readiness probes (`Probe`) poll the engine - `/health`,
   `/v1/models`, and engine-specific flags such as strata's `loaded`.
7. `Install -> Serving`: the occupant enters the slot table, measured
   footprints fold back toward the model row, SSE pushes the new state, and
   proxy traffic streams through to the engine.

## 10. Key invariants

The binding list is `docs/architecture/DOCTRINE.md` (18 items as of
2026-11-02). Architecturally load-bearing among them: the witness is budget
truth; the scheduler is the single writer; engines obey only harness-owned
configuration; the harness launches only from L3; reaping is ownership-
scoped; a stale `(port, generation)` ticket is refused; card evidence
belongs to an operator-run window.

## 11. Where to read next

- Binding invariants: `docs/architecture/DOCTRINE.md`.
- Rulings and rationale: `docs/architecture/DECISIONS.md`.
- User-facing behavior, config keys, HTTP table: `README.md`.
- Historical design record (not a current spec): `DESIGN.md`.
- Coding and documentation standards: `docs/process/ENGINEERING.md`.

## 12. Verification notes

Claims here were checked against `src/` and `Cargo.toml`. Two classes are
Unverified in this repository because the code lives outside it: the strata
server's flag precedence over profile keys (documented in `README.md`), and
engine-side log formats the ninfer parser reads (documented in `README.md`,
exercised by tests against captured fixtures). Machine-local facts (live
services, protected ports, card windows) live in `HANDOFF.md` and are out
of scope for a durable document.
