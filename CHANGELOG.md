---
title: Changelog
owner: operator
status: active
audience: [user, maintainer]
source_of_truth_for: [release history]
last_reviewed: 2026-11-05
review_trigger: release
---

# Changelog

Up: `docs/DOCS_README.md`. Release history of SimpleHarn. Older entries keep the product
names they shipped with. The format follows Keep a Changelog. Versions follow Semantic
Versioning.

Rule: no tag without a dated entry here and a matching `Cargo.toml` version.

## Unreleased

### Added

- Project Maya engines: an `engine: maya` row names `maya_python`,
  `maya_cwd`, `maya_config`, and `serve_model`, and it requires a
  `load_timeout` of 300 s or more. The harness launches `serve.server`
  headless (never the `maya.py` launcher) and forces the loopback bind, the
  slot port, and `--idle-unload 0`. It launches only a scrubbed copy of the
  operator config: the harness strips `host`, `port`, `api_key`,
  `idle_unload_s`, and `cors_origins`, forces `model_management` off, and
  stages the bytes content-addressed under `.simpleharn/maya-profiles`.
  `cost: greedy` is the honest class, the launch floor is the gate, and
  `exact` is refused.

## v1.3.1 - 2026-11-05

### Added

- Two container log-rotation knobs the harness renders into every ninfer
  launch script: `ninfer_log_max_size_mib` (default 25, 1..=1024) and
  `ninfer_log_max_files` (default 5, 1..=50). An absent key renders the
  previous bytes.
- `measured_drift_warn_ratio` (default 0.25, 0..=1, 0 silences): the warn-only
  band that compares the measured engine footprint to the declared row. It
  never changes admission.

### Changed

- SimpleHarn is the only name (operator instruction 2026-11-05). The harness
  retires the working-name spellings. Config discovery reads only
  `simpleharn.json` and ignores an old `simplehar-se.json` or
  `simplehar.json`. The harness reads only `SIMPLEHARN_CONFIG`,
  `SIMPLEHARN_TOKEN` and `SIMPLEHARN_LOG` and treats the old names as
  inert. The wire carries `simpleharn_error` and `owned_by: "simpleharn"`.
  The harness writes custody under `.simpleharn/` and holds
  `SimpleHarn-SingleInstance`. Backward compatibility and migration do not
  exist.

### Internal

- Ten opaque numbers became named constants; operator-facing messages
  interpolate them.
- The fingerprint, log-pruning and guard-capture traps are retired by code
  and pins, not by prose.
- The T14 soak log leg now seeds synthetic logs and asserts the per-model
  prune, so it can fail.
- A final chunked review ran over the whole tree; the remaining Medium and
  Low findings are filed for 1.4.

Tag: `v1.3.1`. Commit: `f092b0a` (release prep). Artifact: `simpleharn.exe`,
built 2026-11-05, SHA256
`78599990617DF46876301F26DA6E0441502A08D10917A3E0391D0969ACBA5B80`.


## v1.3.0 - 2026-11-05

### Added

- Category display names are a config option: a `categories[]` entry gains an
  optional `label`. `/har/status` category rows carry the effective `label`
  (the trimmed name, or the id), and the page shows it on the tabs, the pills,
  and the category options. The ids stay the wire vocabulary: request
  payloads, yaml `category:` values, `evictable_by`, and the engine-port plan
  are unchanged. A non-blank label must be at most 24 printable characters;
  absent or blank falls back to the id, and the label survives the config
  write-back.
- The tray paints the operator's logo set: five embedded 16x16 RGBA icons, one per
  tray state, instead of drawn solid circles. The control page carries the operator's
  favicon: `/favicon.png` serves the embedded PNG, `/favicon.ico` answers the same
  bytes, and the page head links the icon.
- A CPU-only llama row (`cpu_offload: true`) books host RAM instead of VRAM. New model
  key `ram_gb` (a GGUF estimate is the floor without it) and config key `ram_reserve_gb`
  (default 8). A RAM shortfall refuses with named blockers and numbers as `unavailable`
  (503). RAM and VRAM keep separate ledgers: a CPU row never debits or blocks the card,
  and a GPU job is never evicted because a CPU row holds RAM. A CPU row can now be
  evicted under RAM pressure, and the RAM planner considers CPU residents only. The
  harness measures a served CPU row's working set, persists the trusted pair as
  `measured_ram_gb` / `measured_ram_gb_min`, and books it above the floor. The surface
  shows it: `/har/status` slot rows carry `booked_ram_gb`, `ram_cost_basis`, and
  `ram_source` (CPU rows only), the `mem` block carries `booked_ram_gb` and
  `ram_reserve_gb`, `/har/models` prices a CPU row on the RAM ladder with its
  `ram_source`, and the page Cost cell shows the booked RAM figure with provenance in
  the tooltip. A GPU row carries no RAM key anywhere: its wire shape is unchanged.
- A per-row `min_turn` guard: admission refuses to evict a model that has served less
  than the configured seconds.
- Memory refusals carry `queue_position` and `expected_release_s` (a rolling median of
  the harness's own measured releases).
- `/har/status` slots carry `state`; a committed release reads `releasing` until the
  driver confirms it.
- `launch_only_with_at_least_vram_free: whole` selects the whole-machine gate, which
  waits for a fully free card and never evicts.
- The job ledger names the admission rung (`jobs[].path`).
- Two cardless chaos legs: engine killed mid-release, harness runtime dropped mid-drain.
- `prefetch_on_release` (experimental, off by default) warms the incoming llama or
  basalt model during the release wait.
- `docs/reference/COMPATIBILITY.md`, copy-ready `examples/` rows, and the control-page
  screenshot.

### Changed

- The light theme is the dark theme with the lightness direction mirrored: warm page,
  one table surface (header equals the table), controls with a darker edge, matte olive
  accent, brighter working dot.
- The row actions are renamed `Default` and sized to their words.
- The local gate runs the documentation checks that CI runs.

### Fixed

- A confirmed victim release now terminalizes the victim's own ledger row
  (`evicted`/`unloaded`) instead of leaving it `Serving`.
- CI on the Windows runner: LF checkout, host-independent tests, and a toolchain-safe
  atomic helper.
- The release check verifies the webui fixture version against `Cargo.toml`.

### Internal

- The scheduler's 7,409-line inline test module moved to `scheduler/tests.rs`
  (move-only; 9,002-line file becomes 1,595).
The release steps live in `docs/process/GIT_WORKFLOW.md`.

Tag: `v1.3.0`. Commit: `011257a` (release prep). Artifact: `simpleharn.exe`,
built 2026-11-05, SHA256
`E4B57FCFE5B9A0B7A1FF0C31D4450DE4D43519ACE0E6A80E3C90F95C93E806A3`.

## v1.2.0 - 2026-11-04

Tag: `v1.2.0`. Commit: `8fcaa82` (release prep). Artifact: `simpleharn.exe`,
built 2026-11-04, SHA256
`CE2CCF1E5721C6006FDB8F8B891ABED1CEC9A532D15DFCC21CEF937F6E4AE1FA`.

- Renamed the project and binary to SimpleHarn; the config file is now
  simpleharn.json; the SIMPLEHAR_TOKEN/SIMPLEHAR_CONFIG names still work with a
  deprecation warning. No behavior change.

## v1.1.3 - 2026-11-04

Tag: `v1.1.3`. Commit: `ba7d3aa` (release prep). Artifact: `simplehar-se.exe`,
built 2026-11-04, SHA256
`125603C84D1522C84B20430EE2ECA426D73DDE4246C186DDD5DF683A136FE7F3`.

### Added

- Strata and Basalt book a measured memory footprint. After readiness the
  harness reads the footprint from the engine's own status snapshot (`/v1/status`
  for Basalt, `/status` for Strata) and stamps it `engine_status`. Above the
  declared `vram_gb` the measured figure books instead of it; the declared
  value stays the floor. The status bodies for Strata and Basalt were later
  captured on real hardware (evidence layer). Strata's automatic
  `engine_status` booking is live-verified (59.8 GiB, High). Basalt's
  automatic booking verification is still owed.
- A process engine that answers no status endpoint is measured from the
  witness over its own process tree: the engine's pid plus its direct
  children. That figure books as `per_process`. A `total_diff` figure stays
  display-only.
- Elastic Basalt and Strata residents keep polling their status while
  serving. The harness re-books the resident when the footprint rises, or
  when it falls past the hysteresis band (1 GiB or 1 percent of the card).

### Changed

- A Basalt row may declare `cost: greedy` (operator ruling 2026-11-04: the
  engine offloads experts to RAM dynamically). A greedy Basalt row may omit
  `vram_gb`: a greedy admit never refuses on the nominal size, and the
  booking takes at most the room at admit; the launch floor is the gate. The
  ruling sets the launch floor at 60 GiB. Note for owners of existing rows:
  a `cost: greedy` edit made before this binary is in place drops the row
  from the catalog, so set the row to greedy together with the new build.
  A `vram_gb` present on a greedy row is the last measured figure:
  it never refuses the admit and books only up to the room at admit; the
  measured pair never books or re-books a greedy row. A `cost: fixed` row
  still requires `vram_gb`, and `cost: exact` still refuses with a message
  naming the two bookable classes.
- A measured fold re-books the loaded resident's slot booking, so same-slot
  release credit and the launch-floor guard read the measured figure. An
  amber or greedy-launched resident is never re-booked.
- A rescan that moves only cost figures re-books the loaded resident without
  a reload. A flag change still marks the resident `stale_config` and needs
  a reload.

### Fixed

- `POST /har/config/default` applies a new category default to memory only
  after its bytes land in the config file. A write that never lands answers
  `persisted: false` and names the previous default, so memory can no longer
  hold a value the file does not.
- The llama fit probe refuses a memory-key value the serve path refuses (a
  blank, option-shaped, or control-bearing token). An option-shaped value can
  no longer reach `llama-fit-params` unvalidated; the refusal names the key,
  the flag, and the value.
- The page header stops claiming "N loaded" when the harness is unreachable.
  A failed status fetch stamps the header `harness unreachable` (or
  `state not confirmed`), and the next success repaints the real header even
  when the state did not change.
- Engine log captures and container snapshots share one forensic pool key: the
  model name sanitized like the staged profiles. A model with a space in its
  name no longer keeps two half-pools, and one model's keep-N prune can no
  longer delete a differently named model's files.
- A spawned engine cannot escape its kill-on-close job: the child starts
  suspended, is assigned to the job, and only then resumes, so no engine
  descendant can be born outside the reaped tree. A seal that fails kills the
  still-frozen child and leaves no ownership evidence behind.

## v1.1.2 - 2026-11-03

Tag: `v1.1.2`. Commit: `2583faf` (release prep). Artifact: `simplehar-se.exe`,
built 2026-11-03, SHA256
`24F38CAE27E203D4A240F4341E3D0AB41E2D023D3511AE9F3CFF597E9628A425`.

### Fixed

- The Basalt child receives `USERPROFILE` and `HOME` so its Python server can
  resolve `Path.home()` for the model cache. The v1.1.1 release harness failed
  Basalt loads without them. This is a regression that v1.1.1 introduced;
  v1.1.2 fixes it.

## v1.1.1 - 2026-11-03

Tag: `v1.1.1`. Commit: `405505e` (release prep). Artifact: `simplehar-se.exe`,
built 2026-11-03, SHA256
`E0B06DFF692B7AEF4406920DE805607B1BA81343FA0D9E449768FB3A828CB8C2`.

### Added

- Basalt engine support: direct Python server launch with a harness-rendered
  server profile, `/health` loaded and `/v1/models` alias readiness, declared
  `vram_gb` booking, and job-object release. The card-window test run passed.

### Changed

- Split the scheduler's `ActorState` method clusters into child modules:
  `admission`, `catalog`, `commands`, `events`, `fit`, `measured`, `occupant`,
  `spawn`, and `status`. The actor loop, command channel, and state owner stay in
  `scheduler/mod.rs`. This completes Phase 1 and Phase 2 of the cohesion split;
  the API split remains deferred. No behavior changes.

### Fixed

- Boot refuses a non-empty `engine_ports` list whose length differs from
  `max_models` and names the required and given counts. Automatic ports now
  follow canonical WORK, RP, AUX, EMB order, not file order.
- Boot refuses `release_timeout_s` and `settle_timeout_s` above 86400 seconds,
  and refuses a blank `llama_server_exe`.
- The idle sweep no longer starts a second unload behind a cross-slot eviction
  victim; the ledger names one cause of death.
- A send timeout is classified before a connect failure, so a slow live engine
  is not reaped; connect-only failures still restart once.
- The catalog documentation no longer claims full-path aliases.

## v1.1.0 - 2026-11-03

Tag: `v1.1.0`. Commit: `f6c6381` (release prep). Artifact: `simplehar-se.exe`,
built 2026-11-03, SHA256
`031D94D827E407AB5A3B1B5E75B651E3FF108BE804B7C869A665EDD76D45A376`.

### Added

- `simplehar-se import strata <profile.json> --apply <row.yaml>` merges the
  emitted knob block into a live row in one atomic write. It preserves the
  scheduling keys and `strata_python`, deletes `strata_config`, and refuses
  conflicts without a partial write.

### Security

- The canonical ninfer launch now passes `--api-key` when `ninfer_key: true`.
  Before this fix the engine ran unauthenticated while the stamp said `key=on`.
  If you relied on the stamp, treat earlier ninfer launches as unprotected.

### Fixed

- Strata rejects `--host=` and `--port=` as well as the space forms.
- Admission settles against `gate + reserve`, and a same-slot holder is
  credited once.
- A late unconfirmed release gets one bounded, observe-only watcher.
- The boot config is flushed before rename; a landed-but-unverified write
  records the real verdict.
- Model files must be regular files; `mmproj` is traced and resolved.
- Log rotation prunes the generation budget; a contended sink cannot wedge
  emitters.
- The control page is self-contained and a refused logging flip unflips.
- The Windows test gate uses an ephemeral foreign port.

### Changed

- Dependencies: the unused `reqwest` blocking feature is removed and `tokio`
  uses the explicit feature set.
- README documents the current logs, tray, model keys, and import contract.

### Removed

- The legacy `ninfer_autostart` launch path. A row that declares it is refused
  with a migration message; template mode is the only ninfer path.

## v1.0.1 - 2026-11-02

Tag: `v1.0.1`. Commit: `c91e179`. The annotated tag was created and pushed on
2026-11-02. The release covers the 12 commits between `446a3d3` and
`c91e179`. The commit log holds the change detail.

## v1.0.0 - 2026-10-29

Tag: `v1.0.0`. Commit: `11b60a1`. First release of SimpleHar SE.

### Added

- A Windows tray harness that supervises local LLM inference engines (llama,
  strata, ninfer, mock) on one GPU.
- A single-writer swap scheduler. It admits loads against live measured memory
  (the witness) and refuses with named blockers. It never overcommits.
- Four category listeners (WORK, RP, AUX, EMB) with an OpenAI-compatible API.
- A control page on the WORK listener.
- Declarative configuration: one JSON config file for the harness and one YAML
  file per model, with write-back.

## Operator notes (local)

Internal release bookkeeping (build staging, deploy steps, and rollback
handling) lives in this project's private handoff layer. It is not part of
this changelog, and this file names no paths or steps for it.
