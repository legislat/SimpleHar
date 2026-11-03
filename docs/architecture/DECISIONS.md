---
title: Decision ledger (tracked copy)
owner: operator
status: active
audience: [architect, agent]
source_of_truth_for: [settled and deferred decisions]
last_reviewed: 2026-11-02
review_trigger: new-decision, release
---

# DECISIONS - settled doctrine and rulings (binding; do not re-open without reason)

This is the tracked ledger and the single home. `handoff/DECISIONS.md` is a pointer
stub; it holds no rulings and is safe to delete.

Three sections: **LOCKED** = active doctrine every brief must carry; **DEFERRED** = decided
later, with the tripwire that re-opens them; **ARCHIVED** = one-line tombstones (the ruling,
the date, the commit - detail lives in the commit body / FINDINGS file named).
Unresolved questions live ONLY in `DECISIONS-PENDING.md`. This file only ever *gains* lines.

## LOCKED (operator doctrine - binding on all agents)

- Scrapped by operator: bonsai class, engine adoption, preload at boot, parallel soak,
  state restore across restarts. v3.3 `external: true` inert; **`sticky:`->AUX alias deleted
  from the parser** (0 uses in either catalog; a yaml with `sticky:` and no `category:` now
  lands in WORK). `protected_containers` scrapped - reaping strictly ownership-scoped;
  **no hardcoded third-party software names in code/config defaults** (name-free rule) -
  scoped (operator 2026-10-28 audit): documented defaults of *supported engines* are exempt
  (strata/llama/ninfer paths, WSL distro `Ubuntu`); never names of software outside the
  harness's engine scope (other apps, other people's containers).
- **`bak:` yaml node = silent forensic, manual only** (operator 2026-10-28): never read,
  warned, logged, surfaced or written back (stripped from parsed `raw` like `external`);
  one line; a second is a problem line; the harness rewrites a present node only inside the
  incident `.yaml.bak` copy, never appends one.
- Admission is the product: refuse with named blockers, never overcommit; evidence-based
  kill/release (handle signaled + TCP-refused proof); witness 2 s TTL, no stale guesses.
  A job reaps **every** process it owns (victim + own spawn); engine kill/release require
  `(port, generation)` - a stale ticket is refused, never honoured.
- Config file is king (read *and* write-back target `AppConfig.source_path`,
  `SIMPLEHAR_CONFIG`-aware; SE's own `simplehar-se.json`, legacy read-only, one shared
  `config::default_config_path()`). Optional `control_token`, `SIMPLEHAR_TOKEN` beats the
  file. Tray red = listener down, not per-job errors.
- Guard order: structural refusals (Host, cross-site `Origin` -> **403**, content-type -> 415)
  precede the token (401); only `/`, `/index.html`, `/app.js`, `/style.css`,
  `/pico.min.css` are unauthenticated, and `GET /health` **is** token-gated.
- Client-facing `/v1/models` stays minimal (no local paths); control surface owns detail.
  Cost verdicts are data (operator 2026-10-28 audit): strata yamls declare `cost: greedy`
  with their launch floors (currently 60/65 in `ModelConf/`, not code); ninfer defaults to
  `fixed` at declared vram. The
  harness never registers/monitors what it did not spawn.
- **External engines obey only harness-owned configuration** (operator 2026-10-27): a
  child's whole config set - strata profile, ninfer autostart inputs, engine env - lives
  under SE control (like `ModelConf/`), validated at scan/spawn; policy fields (`host`,
  `port`, key) harness-forced, never obeyed from engine-side files. Enforcement build =
  chunk 10 (see HANDOFF queue); `C:\Strata\*.json` migrate copy-in, HANDOFF Section 3 protects
  serving profiles till switched.
- **Ledger demoted** (operator 2026-10-26): budget = `total - reserve - observed_used +
  same-slot victim credit - in-flight bookings`; the ledger sum left the gate *and* the UI,
  surviving only in `LedgerOnlyWitness` (no-GPU mode; dead witness on real HW fails closed).
  Measured `need` = witness total-diff per 60 s as `min-max` (invalidated by `cpu_offload`).
- **Scan accepts only `*.yaml`** (regular, non-symlink, case-insensitive; `.yml` and
  everything else silently inert; a problem line never publishes) - Fork C doctrine
  "better not publish anything blindly" (operator 2026-10-28, `e042b52`).
- **Config layers = L1/L2/L3, adopted 2026-10-29 (target architecture; chunk 10 is its first
  build).** **L1 Import**: engine-native artifacts (strata profile JSON, ninfer autostart
  script, llama invocation) are *fossils of a manual bring-up*, copied in by an explicit
  operator verb and then **forgotten** - after import the harness never reads or writes the
  original (this is what makes the whole area Section 3-safe). Re-import under the same model name
  rewrites the copy. Scan never pulls. **L2 Intent**: the small hand-authored per-model record
  the operator edits. **L3 Launch spec**: generated from L2 + `EnginePolicy` + scrubbed L1,
  and **the harness launches only from L3**. Binding invariant: *L3 must be byte-reproducible
  from L2 + policy + L1* - if anyone needs to hand-edit L3, the design has failed.
  Dangerous L1 parameters (`--host 0.0.0.0`, port, foreign keys, `log` paths) are scrubbed
  **twice with different jobs**: at import for auditability (record what was dropped and why),
  and at render for authority (never trust that an artifact is still clean).
  "Config is not a database" (`DESIGN.md` Section 0.4) still stands: files in a folder, per-model
  isolation ("a problem line never publishes"), are a property worth more here than
   queryability. Full ruling + rejected alternatives in ARCHIVED.
- **Measured-footprint provenance (operator 2026-10-30; refines the "Ledger demoted"
  measured clause).** A measured figure carries a **provenance** and books only if trusted.
  `MemorySample` gains `attribution: {PerProcess, Unavailable}` - witness-local, `PerProcess`
  iff the compute-process list is non-empty **and** >=1 entry reports `used>0`, else
  `Unavailable` (an idle card is `Unavailable` by design; idleness is `used_gib`'s job, never
  overloaded onto attribution). A model's booked measured figure carries a strict yaml stamp
  `measured_attribution: {engine_report | per_process | total_diff}`, written by `on_measured`
  beside `measured_gb`/`measured_gb_min`/`measured_offload` and read by `read_measured`.
  **Booking trust:** `cost_source()` books a measured figure only for `engine_report`/
  `per_process`; a `total_diff` figure **never books** (falls to `Declared`/`Estimate`) but
  stays in the payload labelled untrusted - one accessor `bookable_measured()` feeds booking so
  gate and page share a source (2026-10-26 duplication rule). Per-model provenance derives from
  `own_gib(own_pids)` (`Some`->`per_process`, `None`->`total_diff`); the ninfer startup
  `MemorySummary` log-parse yields `engine_report`. **Void on comparability change** (mirrors
  `measured_offload`): a `cpu_offload` flip, or for ninfer a **`ninfer_profile`/`--kv-capacity`
  change** since the figure was taken, voids the pair; provenance is never upgraded post-hoc
  (only a fresh serve re-measures). **Ruling 11 = metadata-only:** R1 never touches the physical
  gate - the stopper stays observed `used_gib` (LOCKED witness-is-budget-truth). **Ruling 15 =
  (ii):** an `engine_report` figure MAY book (for `fixed`-without-`vram_gb` + eviction ranking),
  bounded by declared `vram_gb` still winning when present, clamp `[0, total]`, and `exact`
   engines only verify (`model/mod.rs:71-73`). `CardOccupancy`/"foreign-dominated" is **deferred
   to R3** (it needs the own-vs-foreign `unreleasable_gib` split). Full design + evidence:
   `research/ninfer-telemetry-2026-10-30.md`.
   **AMENDED 2026-11-01 (operator: "Cost = max booking cost"):** the `vram_gb`-wins clause holds
   only as a FLOOR - a guarded (profile-stamped, un-voided) `engine_report` figure ABOVE the
   declared value now books instead of it (`engine_report_outranks_declared`); a declared value
   at-or-above the report still stands, witness attributions (`per_process`/`total_diff`) never
   overshadow a declaration, and `greedy` is untouched. Mid-session the raise is not retroactive
   (the resident keeps the charge it was armed with; the next admit reads the yaml pair). R9 e2e
   re-pinned: `a_declared_row_below_the_report_raises_the_next_admit`.
- **R10 - container-internal ports join the harness/manual separation** (operator
   2026-11-01: "8003-8005 are manual only"; separation is NEXT after the booking/UI
   commit): the DESIGN Section 21 separation covers numbers, not only host binds. The
   container inside is a **third namespace**: `ninfer_internal_port` becomes optional,
   defaulting to the SE internal number **8808** - the harness's 87xx family at a
   +100 offset, amended the same day on the operator's "8080 - bad, closer to the
   harness ports"; a row naming a `protected_ports`,
   public-listener or engine-plan number is refused at arm time
   (`AdmissionError::PortClaimed`) and named by the rescan (`apply_catalog` twin) -
   `AppConfig::port_claim` is the single decision. The canonical template's
   `PUBLISH="${3:-$PORT}"` fallback (the last path that could host-publish a manual
   number) is replaced by `PUBLISH="${3:?...}"`: a missing publish argument fails the
    script, never borrows the host namespace. Live yamls drop the borrowed 8004/8005.
- **The card window belongs to the handoff, never to a served session** (operator, restated
   2026-11-02 after the `-Card` smoke: "YOU WILL NEVER RUN CARD tests YOURSELF - you are the
   process occupying 99% VRAM"): the agent's own session is served by engines on this box, so
   no agent may start harness instances, load models, or execute `verify.ps1 -Card` - card
   evidence is produced ONLY by the `handoff/wave-verify/` package run in an operator-spawned
   session. When a card capture needs changed behavior, the fix lands in the PACKAGE
   (selftested cardless) and the operator runs the window. Cardless work in-tree (fmt, clippy,
   `cargo test`, DOM stub) stays allowed - it execs no engine.

## DEFERRED (decided later - tripwire in parentheses re-opens it)

- **13[10] typed `WritebackError`**: ~~lands with the first consumer that branches on the error
  class~~ - **TRIPPED + DELIVERED** 2026-10-28 (see ARCHIVED `b5bf0c1` line).
- **FIXTRACK [5]** stale line-cites in frozen `t_switching` -> post-tag.
- **Share gate-honesty harness with `t_switching`** -> post-tag.
- **`name:` key** tolerate-iff-equal now -> hard-reject post-tag (frozen suites write matching
  pairs).

## ARCHIVED (tombstones: ruling | date | commit)

- 13[4] Fork C strict scan + persist-reconcile, `commit_verified` blanket kept | 2026-10-28 | `e042b52`
- 13[5] Fork B restore temp+verify+rename + single-slot `.yaml.bak` + strict `.yaml` gate +
  symlink skip | 2026-10-28 | `e042b52`
- same-stem/temp-shape property pins ruled **vacuous** (single dir + full-name temps) - do not
  pin; re-open only if an arbitrary-path write-back caller appears | 2026-10-28 | `951023f`
- 13[8] Fork B comment policy: identical-value write = byte-verbatim pass-through, real change
  drops the inline comment | 2026-10-28 | `7fdf99c`
- 13[9] parser charset = `[A-Za-z_][A-Za-z0-9_-]*` (hyphen legal) | 2026-10-28 | `e042b52`
- 13[11] gate pins: hammer + mixed-entry + residue on all paths + `write_synced`
  flush-before-publish (inspection-tripwire, declared) | 2026-10-28 | `951023f`
- 13[12] `EDITABLE_FLAGS` deleted (dead code) | 2026-10-28 | `951023f`
- Review "lib 215 (208+7)" finding [2] **rejected**: per-round counts were true in uncommitted
  states - decompositions must name their round | 2026-10-28
- Editor race (harness patch vs operator save) = **acceptable doctrine**: edit yamls while
  harness idle / Rescan after; no CAS guard until doctrine fails | 2026-10-28
- 19[5] duplicate-parked-load dedup sub-claim = **WONTFIX, supersede-is-by-design**
  (`on_swap` supersedes the queued twin deliberately) | 2026-10-28
- 22[1] strata policy-field claim downgraded to "decided, enforcement owed to chunk 10";
  22[2] README witness-is-truth wording | 2026-10-28 | in tree (`DESIGN.md`, `README.md`)
- DESIGN Section 20 item 7 yaml write-back formally CLOSED (measured triple persists with pins) |
  2026-10-28 | `DESIGN.md:823`
- **14[7]+15[2] tri-state = "A + amber"**: `Pending` seeded at registry construction
  (absence unrepresentable), tray precedence red>amber>yellow>green>gray with 2 s boot
  grace, no-verdict-is-one-word (Pending and status-fail both amber), ready gate never
  calls no-verdict resolved; landed with 15[1]/[3]/[7]/[10] | 2026-10-28 | `a149070`
- **Section 20-9 engine schema = Fork C** (operator 2026-10-28, recommendation approved):
  compiled-in adapters + single-source internal `EnginePolicy` table (forced_fields /
  forbidden_words / custody_roots / stop_grace / cost_default / markers) with ONE generic
  validator - chunk 10 lands as "one mechanism, four instances". Sub-decisions 1+2 fold in:
  per-engine settings (strata python/cwd) move to the harness config `engines:` section,
  per-model yaml keeps only references; knob single-source generation parity test (llama
  FLAG_TABLE <-> yaml <-> webui). Operator-declared `engines.toml` (Fork B) REJECTED on
  doctrine (harness-owned-config is the inverse of openness-posture); Fork A is what C
  prevents; Concept-set adoption (sub-decision 3) deferred. Build lands when promoted.
- **`cpu_offload` engine scoping = B+C+D** (operator 2026-10-28, per
  `research/cpu-offload-scoping-2026-10-28.md`): llama-only via `OFFLOAD_ENGINES` const
  (first `EnginePolicy` row; mock OUT), warn-only `ScanKind::InertKey` on engines that never
  read it, stamp/void engine-gated (strata/ninfer measured pairs no longer silently voided),
  page + API both refuse non-llama cpu edits. Sibling llama knobs (`ngl/fit/...`) knowingly
  unscoped until chunk 10 generalizes the table | `c6ee754`
- **15[9] token redaction = manual `Debug`** (operator blanket GO on recorded rec):
  exhaustive-destructure guard (E0027 on new fields), `REDACTED(present/absent)`,
  secrecy declined (rationale on the impl); ring pin race-proofed after 28I | `ae71883`
- **Reload-drift RULED + SHIPPED** (operator 2026-10-29): Axis 1 = 1b queued-intent
  retirement by fingerprint on rescan (`57c5ff6`); Axis 2 = B + launch-time -
  server-owned `stale_config` amber (spawn fingerprint; stats on install/reload turns
  only, double-layer zero-stat proof on read paths) + `launched_at` ISO-8601 rendered
  verbatim by the page; C (auto-free) rejected. Profile-layer variant -> chunk 10
  (custody shrinks it) | `270fd16`
- **extra_args = closed ALLOW-list** (`EXTRA_ARGS_ALLOWED`, llama.rs; 11 reviewed words:
  embedding/pooling/sampling/warmup family): the 10-name deny-list was a doctrine bypass
  (8/8 smuggles by review F2-0910; 13/13 blocked post-fix, 28N). Full `EnginePolicy`
  custody enforcement stays owed to chunk 10 | 2026-10-28 | `7e99679`+engine round
- **15[4]/15[8] ruled GO** (tooltip error-first under truncation; SIMPLEHAR_LOG strict +
  reported fallback) and shipped in the R2 batch | 2026-10-28 | `7e99679`
- **13[10] typed errors DELIVERED via tripped tripwire** (operator autonomous approval):
  8 thiserror variants, String-site 1:1, Display byte-identical (review-verified against raw
  HEAD), api classes 400/409-naming-file/500; chunk 13 fully closed | 2026-10-28 | `b5bf0c1`
- **Quoted-value seam = auto-quote + refuse-unquotable** (operator 2026-10-28, rec approved):
  `emit_form` defined solely through `split_value_comment` - as-is if faithful (live
  vocabulary byte-identical), one-pair quote-wrap if that round-trips, refuse naming
  key+value otherwise; quote-wrapped incoming words are literal data, never directives.
  Review 28G ACCEPT; its LOW closed pre-commit | `47f4119`
- **`ninfer_profile` default `"b"` = LEAVE AS-IS** (operator 2026-10-28): the autostart
  scripts already default to profile `b`, so the harness copy matches reality - neither warn
  nor refuse; FINDINGS-10 [10] closed without a code change.
- **Ninfer published-port = evidence-only diagnosis, option (b)** (operator 2026-10-29): the
  `$3` **offer/contract** stance (FINDINGS-10 [5]) stands - the harness does NOT enforce that
  the script published where it was told. On readiness failure for a container-backed engine
  it runs the ownership-scoped `docker ps --filter publish=<assigned>` / `docker port <name>`
  and, if the container is running but published elsewhere, names it in the refusal instead of
  returning a bare `load_timeout`. Report never gate; strictly the claimed name (no enumerating
  foreign containers). Sub-choice **failure-path only** adopted with the recommendation (zero
  happy-path WSL round-trip). Lands inside chunk 10 (it is `port`-policy custody enforcement,
  already owed per the 22[1] line + `extra_args` line). Trigger: both `ninfer-swift*` scripts
  echo `$PORT` (internal 8004, a *protected* port) while `--publish` honours `$3`, so the sole
  on-disk artifact of a launch misdirected a whole post-mortem.
- **Chunk 10 (EnginePolicy / strata-custody, Fork C) PROMOTED to priority 1**
  (operator 2026-10-29): build first, ahead of the cardless Wave A measurement cluster.
  Cancels "Build lands when promoted" on the Section 20-9 line. It is the enforcement build for the
  LOCKED "external engines obey only harness-owned configuration" rule and retires five debts
  already recorded here: full `EnginePolicy` custody enforcement, `extra_args` full
  enforcement, `cpu_offload` sibling-knob scoping, the reload-drift profile-layer variant, and
  the strata policy-field claim.
- Test-honesty batch 18/19/20 committed (14 items, mock_runtime rebuild verdict FAITHFUL,
  KILL_ON_CLOSE finally tested) | 2026-10-28 | `a718149`
- Docs-truth batch 22[1]/22[2] committed | 2026-10-28 | `301f5d0`
- RULED (empirical, manual test 2026-11-01): ninfer serve args ride the SPACE form - both shipped
  engine parsers reject `--flag=value`; `valid_serve_arg` admits bare value tokens, refuses
  single-dash options and every denylist name/prefix. An override key naming a built-in profile
  (`ninfer_profile_b`) is dead config and is now refused at launch, not ignored.

- RULED 2026-11-01 (amends 15): the engine own word is read AT READY, not at startup. The ninfer
  booking is max(startup MemorySummary reserve, post-ready `weights ready`+`runtime` lines from the
  same docker-logs scrape); both figures remain the container own word, host-KV lines can never
  join. Question A (swap credit) SUPERSEDED: with truthful bookings the same-slot credit at
  finish_admit is honest by construction - no witness-measured credit was built. All live yamls
  cleared of stale measured_* (estimate fallback until fresh folds); card is ECC-enabled (witness
  total reads 70, not 72) - operator to flip after session. Upstream ask drafted:
  handoff/upstream-ninfer-post-ready-memory-summary.md. Committed a7f1623.

- RULED 2026-11-01 (supersedes the "Question A SUPERSEDED" closure above; operator: "(ii) - ok.
  but we will leave an option to revise it later"): **same-slot swaps are release-then-resample**.
  When the admit gate is short and the planner cannot cover the deficit, a job with a same-slot
  victim sets `release_first`, destroys the resident it was already replacing, and lets the
  SETTLE window - which already re-samples the physical card - act as the re-gate; refusal then
  happens against truth (the victim is gone: accepted cost, REVISABLE ruling). Cross-slot victims
  are never queued on a paper decision; preflight (chunk 10 [2]) still refuses before any kill.
  The paper deficit is real even under truthful ninfer bookings: booked reserve vs actually freed
  differ in BOTH directions (round-2(A) credited 32, freed 44.1 - refused a fitting swap;
  round-2(B) debited a stale 62.9 against 44 actual).

- BUILT 2026-11-01 (operator "approved to go"): **R3**. Refusals name `unreleasable`
  (= physical used - same-slot victim - every evictable candidate), `used_gib` and `booked_total`
  via `BudgetFacts` (boxed for clippy); zero/absent figures keep the one-sentence refusal.
  NO new error code (ruling row 8). Ruling-10 trust grading landed as the FOLD gate:
  `grade_card_occupancy` grades the MOVEMENT of unaccounted memory between admit and sample
  against the engine's own growth - static foreign holders are already discounted by the baseline
  (a share-based grade would refuse every honest co-serve fold, pinned); a ForeignDominated
  window mints no `total_diff` pair (warn, labelled blank). Gate 562/0/24.

- RULED 2026-11-01: stale measurements are BLANKED, not annotated. The qwen-turbo post-ECC row's
  23.8-59.3 total_diff quartet was stripped from both ModelConf dirs; the row books at the GGUF
  estimate (27.7) until a Wave C quiescent serve re-mints. Default absence chain stands:
  Declared vram_gb -> bookable Measured -> Estimate (gguf x 1.15 + 2) -> `exact` with nothing
  = Undeclared refusal. (Under-booking vs the ~44-59 real footprint is protected only by the
  physical gate at load - re-measure is the fix, and R3 now keeps the re-mint honest.)

- RULED 2026-11-01 (R5, row 9; operator principle): **the harness operates only on resources
  available to it** - a foreign/unclaimed load's VRAM is never counted as the harness's own
  (never credited, never evictable, already occupied in the witness reading), and loads
  refused on an OBSERVED insufficient card is correct behaviour. Foreign memory freeing or
  re-claiming mid-session is NORMAL - handled gracefully (settle-window re-samples, no
  permanent refusal on a foreign reading, nothing foreign is ever killed). R5 lands as
  **report-only** reconcile: boot/rescan names a running catalog `ninfer_container` with no
  claim in log + `/har/status` (+ webui); a refusal may NAME the unclaimed container beside
  its `unreleasable` figure (display rides facts). Never gate on a foreign footprint guess -
  per-PID is `[N/A]` box-wide, so any zombie-size estimate is a guess; the physical gate
  already refuses what does not fit. Report only - never kill (G4 unchanged).

- RULED 2026-11-01 (row 10, operator): **launch-floor achievability guard = refuse early, name
  it** - when the model's `launch_only_with_at_least_vram_free` exceeds what NO release the
  harness can perform would reach, arm-time refusal: "No releases allow for the VRAM floor.
  Check foreign residents." The floor's free is defined as **truly available to the harness**:
  currently free + everything the harness may release (its own booked, evictable residents) -
  foreign/unclaimed occupancy is never counted toward meeting a floor (R5's principle, same
  arithmetic as R3's `unreleasable`). Values KEEP: 60/65 in the yamls stand - they are the
  operator's dedicated-card intent statement, and the guard makes "intent unmet" visible
  instead of a numeric shrug; the guard NEVER auto-relaxes a floor.

- RULED 2026-11-01 (row 17): **template denylist confirmed as-is; profile-override lines are
  THE capacity-shaping mechanism.** `ninfer_serve_args` keeps refusing the four harness-owned
  capacity flags (`--max-context`/`--kv-capacity`/`--max-concurrency`/`--kv-dtype`) plus every
  strict prefix; no serve-args allowlist is added (an escape hatch beside the denylist breaks
  booking-derivability - rejected). Capacity shaping rides `ninfer_profile_<name>` override
  lines only, which the live cards already use. One-time confirmation: capture
  `ninfer-serve --help` in the Wave C window and verify the four flag names verbatim (a
  renamed flag would silently widen the denylist's blind spot).

- RULED 2026-11-01 (row 18, option b): **legacy `ninfer_autostart` is deprecated now, removed
  after a green Wave C.** Retirement trigger: the Wave C window passes with template mode on
  the operator's cards - until then the legacy path is the deliberate escape hatch (it has
  zero live yaml users today; the operator's manual `installation-guide/*/autostart.sh` are
  manual launches and survive the removal). Interim: a scan warning on any legacy row. With
  the   trigger met, the divergent path (fail-closed internal-port rule, its R9/log quirks,
  profile-gate differences) is deleted - one mechanism restored.
  EXECUTED 2026-11-02 after green Wave C (`805c7d0`, TASK-DEBT-LEFTOVERS): scan
  (`model/scan.rs`) and adapter (`ninfer::legacy_removed_sentence`) refuse a declaring row
  with the migration sentence naming `ninfer_image`, `ninfer_model_name`, `ninfer_artifact`,
  and `ninfer_model_roots`; the row never publishes. The legacy launch branch, its
  internal-port consequence, the R9 profile-gate warn, and the stale display path are
  deleted. Template mode is the only ninfer launch path. The operator's manual
  `installation-guide/*/autostart.sh` scripts remain manual launches outside the harness.

- RULED 2026-11-01 (operator approved Search#40): **llama books from `llama-fit-params
  --fit-print on`** - a new **Fit** cost class computed PRE-SPAWN from the engine's own
  calculator (sibling of config `llama_server_exe`), stdout `NAME model ctx compute` MiB,
  VRAM = sum of non-Host rows + `mmproj_surcharge_gb` (vision rows) + `fit_margin_gb` per
  active device (Wave-C-calibrated constants). **Never persisted** - recomputed each admit
  (args-hash cache is the void identity), so the stale-`measured_*` failure class cannot
  exist for Fit; version-locked because the calculator IS the sibling of the binary about
  to serve. Max-truth ladder mirrors the amended 15(ii): Fit above declared `vram_gb` books
  instead of it; `greedy` launch-floor input reads Fit; `exact`-without-`vram_gb` verifies;
  webui amber "engine-projected" until a first witness cross-check promotes green for the
  session. Live-proved 2026-11-01 on the foreign-saturated card (turbo row: 31.0 GiB device
  projection vs 27.7 estimate vs 36 observed) - contamination-proof. Server-log own word
  (variant A) = Wave-C probe for display/audit only, never booking (post-spawn, trigger
  unverified b11669); hand-written GGUF formula REJECTED (upstream ships the calculator);
  quiescent `total_diff` re-mint demoted to the Wave-C cross-check that calibrates the
  margin. Full design: `D:\WORKSPACE-VS\research\Search#40-llama-vram-booking-own-word.md`.
  **AMENDED same session (operator approved):** cache policy = warm **all** llama rows at
  boot/rescan (deferred serial queue, never near `first_bind`, cancelled+redone on rescan,
  ~0.66 s/row measured live - cells patch in via the polling-quiet path), NOT only the
  unmeasured ones (max-truth needs the fit figure even where declared/measured stands);
  launch re-probes on any PROVEN change - tool/server version mismatch or row/model-file
  change (args_hash+size/mtime miss) - a provably-current cache entry is the same
   deterministic computation, re-running it buys nothing. No persisted numbers remains
   absolute; the cache is in-process only.

- RULED 2026-11-02 (row 19, operator: "go. adopting current live profiles afterwards"):
  **strata template mode SHIPPES; the external `strata_config` path is SUPERSEDED at once.**
  (a) GO the ninfer-style L3 render: the row yaml owns the fields (`strata_exe`, cwd, the gguf
  paths, quant/kv/context/spec knobs, the vision block, `gpus_asked`), the harness renders the
  full profile JSON at load and launches ONLY its own rendered copy. (b) The rendered-profile
  root is `.simplehar/strata-profiles` beside the strata custody sidecar (content-addressed
  `<model>-<hash>.json`, keep-N prune, atomic write) - the spawn rewrites `--config` to exactly
  the staged path, so the child can never be pointed at another file. (c) YES:
  `simplehar-se import strata <profile.json>` (one-shot: read profile -> emit yaml, delete
  nothing) replaces the held Section 20-9 copy-in slice; the operator adopts the live profiles
  afterwards. Consequences ruled with the GO: the bind is harness-forced `--host 127.0.0.1`
  argv (server.py obeys the flag over any config, and the flag's own default is loopback), the
  port is the harness-assigned `--port` (server.py never binds the profile's `port` key), so a
  rendered profile carries NO `host`/`port`/`key` - 22[1] stays clean by construction. A row
  still declaring `strata_config` is refused at scan with the migration sentence (name the
  import command); `C:\Strata` stays the manual-launch home the harness never reads on the
  serve path (the import CLI is the one sanctioned reader). No compat path: unlike ninfer's
   operator scripts, no live consumer of external mode survives the adoption.
- BUILT same day (`b9c4745` + `3e37c4d`, gate 651/0/24 + DOM 101). **Row 19b - faithfulness
  addendum, found by the first adoption imports:** the nvfp4 profile's top-level `sampling`
  defaults (`temperature 1.0, top_p 0.95, top_k 20`) and `uncensored` switch (this fork's
  honest name for the projection default) ARE consumed by server.py's
  `sampling_defaults_from_config`, and the first knob set did not carry them - the import
  honestly printed them as `# not mapped`, but adopting that row would silently change the
  served engine. "The row yaml owns the fields" (19a) covers them, so `strata_sampling`
  (whitespace `k=v` number/bool tokens, source numeric types round-trip) and
  `strata_uncensored` (strict bool - an explicit `false` is written intent, distinct from
  absent) became render knobs and the import maps them. The import header also now reminds
  that `strata_python:` is a row key no profile carries (the nvfp4 row's alternate venv must
  be copied at adoption). No further unmapped keys survive in any of the three live profiles.
- **Row 19c - the import bridge, RULED 2026-11-02 (the ruling `src/import.rs` cites):**
  `simplehar-se import strata <profile.json>` is the ONE sanctioned reader of a manual
  profile and the only door off the superseded external mode. It EMITS, never applies: it
  prints the knob block - scheduling keys and `strata_python:` named as row-side merges,
  every key it cannot carry reported, harness-forced fields reported as dropped - and
  touches neither the row nor the profile file. Adoption is therefore an operator-owned
  paste of the emitted block (2026-11-02 Q&A: "why still a paste" - because a profile
  cannot answer for row-only keys, and an auto-rewrite could silently drop the nvfp4 venv).
  An auto-merge `--apply` was offered and is NOT ruled (DECISIONS-PENDING, Needs a GO).
- **Row 19d - vision faithfulness, RULED 2026-11-02 (operator, option 2 of the
  FINDINGS-25 [1] item):** 19b's closing claim held only for ROOT keys - the nvfp4 profile
  carries nested `vision.threads: 16`, and server.py DOES consume it (the encoder spawns
  with `--threads <n>`, `serve.server` at server.py:1941-1942). The `strata_vision` closed
  key set mirrors the vision spawn's FULL cfg contract (server.py:1936-1946): unsigned
  `threads` and `min_tokens` (server's own hand-edited key, #767) join the tokens. The
  import maps both and reports a nested number it cannot carry as
  `vision.<key> (present, wrong type)`; keys outside the server contract are reported
  `# not mapped: vision.<key>` as since `d7e3858`. The golden import->render round-trip
  now carries both keys in every gate run, so the knob set cannot silently drift from
  the engine contract again.

- Governance D1-D11 adopted 2026-11-02: durable core tracked, live state local, DESIGN
  historical, decisions moved to `docs/architecture/DECISIONS.md`, maximum two working
  subagents, narrative commits, cardless CI, local gotchas/findings, live-layer backup,
  version/changelog coupling, and ASD-STE100 for durable docs. N2 closed by this ruling.
  | 2026-11-02 | `1bd78c8`

- CALIBRATED 2026-11-02 (TASK-R11-FIT, C1 capture at `805c7d0`,
  `handoff/wave-verify/logs/`): llama Fit constants stay margin 3 / surcharge 2 /
  session slack 2.0. The C1 row books 31,717 MiB / 1024 (CUDA0 only; Host excluded)
  + surcharge 2.0 (mmproj present) + margin 3.0 x 1 active device = 35.9736328125 GiB.
  NVML observed_used_gb was 35.61735153198242, so the projection is 0.35628128051758
  GiB above the observation: conservative on one sample. A numeric reduction needs
  multi-sample card-window evidence. Pins: `src/engine/fit.rs` C1 arithmetic test and
  the scheduler tolerance test. Deferred: the `engines.llama.fit_validate_slack_gib`
  knob, which owes the llama policy row's `config_knobs` claim and the parity literal.
  | 2026-11-02 | `TASK-R11-FIT`

- ACCEPTED 2026-11-03 (12-P): the park-path queue fingerprint stays one
  `fs::metadata` on the yaml file the just-applied scan read. The scan is zero-fs by
  design; offloading the stat costs a second read or couples queue registration to scan
  internals, and a deferred stat races the KEEP-on-absence rule that must excuse an
  unobserved capture. Cost: one metadata call per parked load; no engine, no volume.
  Reopen only if a profile shows the park path as a hot spot.
  | 2026-11-03 | `TASK-DEBT-LEFTOVERS`

- FIXED 2026-11-03 (R2 chunk 01/03): the post-release settle gate is reserve-aware.
  `freed_for(gate_target + vram_reserve_gb, witness_tolerance_gb)` matches the
  direct-admit guard band; the 2026-10-26 one-sided tolerance is unchanged. This is
  intentionally stricter: a swap must reach `gate + reserve`, so the settle-timeout
  surface grows on a card that runs near the reserve. The same-slot holder credit is
  counted once: the ledger term is `free_at_admit + (released_cost -
  holder_released_cost)`, while the observed term keeps the full release cost. The
  shipped test that pinned the inflated skip is renamed and re-pinned at 55 < 56.
  | 2026-11-03 | `TASK-DEBT-LEFTOVERS`

- RULED 2026-11-03 (R2-14-04/REV6-02): the legacy `simplehar.json` read-only rule
  scopes to auto-discovery and migration only. An explicit `SIMPLEHAR_CONFIG` path is
  an operator instruction and the config write-back target even when it carries the
  legacy name (config file is king). Boot honors the selection and warns once: every
  write-back rewrites the v3.3 file, and v3.3 reads `protected_ports` and may exit on
  its own port-namespace check (the rollback-strand hazard). Pins: the `boot_config`
  warn, `main.rs` tests `explicit_legacy_named_config_warns_about_the_rollback_strand`
  and `legacy_named_file_is_a_write_target_only_when_explicitly_selected`, and
  `api::tests::config_write_path_follows_boot_and_never_the_legacy_file_on_fallback`
  (fallback never legacy; stamped explicit selection wins).
  | 2026-11-03 | `TASK-DEBT-LEFTOVERS`

- RULED 2026-11-03 (R2-02-02, option B): a job that terminalizes with an unconfirmed
  release and a spawned ticket gets one bounded late-release watcher. The watcher is
  ticket- and generation-scoped, observes only, runs for one additional
  `release_timeout_s`, warns once, and never re-kills. `Ok(true)` clears the same
  generation; `StaleTicket` means a successor exists and the watch stops without
  touching it. Next load or boot reclaim remains the recovery. This makes the residue
  visible without adding live kill authority.
  | 2026-11-03 | `TASK-DEBT-LEFTOVERS`

- RULED 2026-11-03 (N3): approve the scheduler cohesion split in principle. Phase 1
  (free-cluster relocation) runs after the chunked R2 review and after release 1.1.0,
  in a dedicated no-scheduler-edits window. Phase 2 (split `impl ActorState`) runs only
  after a green wave-verify. Phase 3 (API split) is deferred. Single-writer semantics
  stay LOCKED; pure moves only; cardless gate after each module.
  | 2026-11-03 | `TASK-N3-SPLIT`

- RULED 2026-11-03 (N7): approve dependency hygiene. Remove the unused `reqwest`
  blocking feature, narrow `tokio` features to what the crate uses, and dedupe
  `base64`/`syn` where a compatible upgrade exists. No new dependency; report any
  duplicate that cannot be removed without an upgrade.
  | 2026-11-03 | `TASK-N7-DEPS`

- RULED 2026-11-03 (release): release **1.1.0** after the chunked R2 review is
  complete. Version, changelog, README release claim, tag, deploy, and artifact hash
  move together.
  | 2026-11-03 | `TASK-RELEASE-NEXT`

- RULED 2026-11-03 (import): GO for `simplehar-se import strata --apply <row.yaml>`.
  Merge the emitted block into the live row, keep scheduling keys and `strata_python`,
  refuse on conflict instead of guessing, and delete `strata_config`.
  | 2026-11-03 | `TASK-IMPORT-APPLY`

- RULED 2026-11-03 (priority): after release 1.1.0, Basalt is priority 1 and Berth
  borrows are priority 2. Neither starts before the release.
  | 2026-11-03 | `TASK-BASALT`, `TASK-BERTH-BORROWS`
