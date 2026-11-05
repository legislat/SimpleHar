---
title: Configuration reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [harness configuration keys, category eviction rules]
last_reviewed: 2026-11-04
review_trigger: behavior-change, release
---

# Configuration reference

Up: `docs/DOCS_README.md`. This file owns the harness configuration keys and
the category eviction rules. The category and port matrix itself lives in
`README.md` (Categories and ports).

## The config file: `simpleharn.json`

The file lives beside the executable. Override the path with the
`SIMPLEHARN_CONFIG` environment variable. The deprecated `SIMPLEHAR_CONFIG`
and `SIMPLEHAR_TOKEN` spellings keep working as fallbacks, with one
deprecation warning in the log; the `SIMPLEHARN_*` spelling wins when both
are set. A file named by that variable is an
instruction, not an absence: boot fails when it does not exist, and
first-start creation never writes there. Delete the default file to have the
next boot re-create it at the default path. A file named by `SIMPLEHARN_CONFIG`
must exist. Auto-discovery order: `simpleharn.json`, then `simplehar-se.json`,
then `simplehar.json`. An existing `simplehar-se.json` is loaded in place and
write-back stays on that file, so a setup written under the old name keeps
working after the rename. If a legacy `simplehar.json` sits
beside the exe and neither newer name exists yet, the harness copies it
over once and leaves the old file untouched for rollback. **The config file
is authoritative.** A `default_model` change made on the page persists back
to the exact file boot read, through an atomic rewrite. The harness completes
partial files. An invalid file stops boot and the harness logs every problem;
it never applies silent defaults.

## Fields

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN. Set `control_token` first; see the Security note in `README.md`. |
| `control_token` | *unset* | When set, every route except the page assets requires it, on `X-Har-Token` or `Authorization: Bearer`. `SIMPLEHARN_TOKEN` overrides it and keeps the secret out of the file. |
| `categories` | four defaults | Per-category `id`, optional `label`, `public_port`, `max_models` (1-6 for AUX, exactly 1 otherwise), `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), and `engine_ports` (empty = automatic plan; a non-empty list must name exactly `max_models` ports). An omitted `evictable_by` and an empty list mean the same: nobody may evict that category. Boot names every non-WORK category left with no evictor. |
| `vram_total_gb` | `72.0` | Admission ceiling. First start normally **detects it** (NVML total, floored to 0.1 GiB). This default survives only a failed card probe. Every boot warns when the stored value and the card disagree beyond `witness_tolerance_gb`. It never repairs the value. |
| `vram_reserve_gb` | `2.0` | Card GiB kept unreachable: the gate subtracts it before it compares (DOCTRINE item 4). |
| `ram_reserve_gb` | `8.0` | Host GiB kept unreachable for CPU-resident loads: the RAM gate subtracts it before it compares. The RAM twin of `vram_reserve_gb` on its own resource (operator ruling 2026-11-05). The OS and the desktop need the headroom, and a refused load costs one retry. |
| `witness_tolerance_gb` | `2.0` | Tolerance band for driver lag after a kill. A load may spawn up to this far under the gate. The band absorbs the lag between a confirmed release and the card reading catching up. It is also the settle allowance. |
| `greedy_launch_min_free_gb` | `4.0` | Launch floor for `cost: greedy` models without their own `launch_only_with_at_least_vram_free`. |
| `load_timeout_s` | `120` | Readiness deadline per load. Range 1-86400 seconds; boot refuses a value outside it. A model can override it with the `load_timeout:` yaml key. |
| `measured_sample_s` | `60` | Seconds between witness samples while a model serves. |
| `release_timeout_s` | `60` | Kill-confirmation deadline. Range 1-86400 seconds; boot refuses a value outside it. |
| `settle_timeout_s` | `30` | Memory-settle deadline after release. Range 1-86400 seconds; boot refuses a value outside it. |
| `protected_ports` | small reserved set | Ports the harness must never bind or touch. Extend the set to cover servers you run manually outside this app. |
| `wsl_distro` / `wsl_user` | `Ubuntu` / *(set for ninfer)* | WSL coordinates for ninfer containers. |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary. Boot refuses a blank value; omit the key to keep the default. First start prefers a `llama-server.exe` found on PATH (absolute entries only) and records the winner in the created file. |
| `engines.strata.python` / `engines.strata.cwd` | documented defaults | Interpreter and working directory for strata launches. The per-model `strata_python`/`strata_cwd` keys still override this section. |
| `engines.llama.fit_probe` | `false` | When true, the harness asks the engine's own `llama-fit-params` calculator (a sibling of `llama_server_exe`) before spawn how much a llama row will need. The harness projects every llama row after boot and rescan, off the hot path. |
| `engines.llama.fit_margin_gb` / `engines.llama.mmproj_surcharge_gb` / `engines.llama.fit_probe_timeout_s` | unset | Calibration constants for the projection. The margin is added per active GPU. The surcharge applies to vision rows. The timeout is the probe deadline. While the two figures are unset the probe stays silent and rows fall back down the cost ladder. Set them once from a calibrated card window. |
| `modelconf_dir` | `<exe dir>\ModelConf` | Catalog directory. |
| `tray_enabled` | `true` | Tray icon at boot. |
| `allow_mock_engine` | `false` | Test-only. When true, boot registers the mock engine adapter and warns that it is not for production. A row with `engine: mock` is refused when the grant is false. |
| `prefetch_on_release` | `false` | Experimental. When true, a load that commits a release reads the weight files the incoming model's launch opens into the OS cache during the release wait, so the reload reads from cache. A llama row warms `model:` and `mmproj:`; a basalt row warms its `model:` pack; other engines open nothing these keys name and warm nothing. The warm is a pure hint: it changes no admission decision, never blocks a step, and a failed warm changes no job outcome. `DEFERRED-IDEAS.md` (section 3) owns the measurement it still waits for. |

## The `categories` key

Each entry in `categories` declares one category. The keys:

- `id`: the category identity, one of `WORK`, `RP`, `AUX`, `EMB`.
- `label`: an optional display name for the category. The page and
  `/har/status` show it; the operator ruling of 2026-11-05 keeps the four
  default names (`WORK`, `RP`, `AUX`, `EMB`) as defaults, not fixed text.
  The id stays the identity: the yaml `category:` values, the `evictable_by`
  lists, the `/har/*` request payloads, and the automatic engine-port plan
  keep using the ids. The page never edits a label. Rules: the harness
  stores the value trimmed. A non-blank label must be at most 24 characters.
  It must be printable (no control characters). A refusal names the
  category id and the broken rule. An absent or blank value falls back to
  the id. An unset key serializes nothing, so an existing file keeps its
  exact shape, and the write-back preserves a set label.
- `public_port`: the public HTTP listener that serves the category. The
  defaults are the matrix in `README.md` (Categories and ports).
- `max_models`: the slot count. 1-6 for AUX, exactly 1 for every other
  category. AUX can hold up to six models at once.
- `default_model`: the model the harness loads for the category when a
  request names no model and nothing is loaded. A change made on the page
  persists back to this file.
- `evictable_by`: the categories that may evict this one. An omitted key and
  an empty list mean the same: nobody may evict that category. Boot names
  every non-WORK category left with no evictor.
- `idle_timeout_s`: idle timeout in seconds; `0` means never.
- `control`: marks the control listener, which hosts the control page and
  the `/har/*` routes. Exactly one category carries it.
- `engine_ports`: the per-slot engine port plan the harness assigns. An empty
  list keeps the automatic plan. A non-empty list must name exactly
  `max_models` ports, one per slot. Boot refuses another length and names the
  required and the given port count.

Every value is declarative.

## Port plan

The harness assigns engine ports per slot, sequentially from 18109. The
automatic plan walks the canonical category order (WORK, RP, AUX, EMB), not
the order of `categories` in the file. The
default four single-slot categories use `18109`-`18112`. With six AUX slots
the plan reaches `18117`.

## Eviction rules

The `evictable_by` field is the eviction priority matrix: the "Evicted by"
column of the matrix in `README.md` (Categories and ports). It is the only
category-level rule that decides which models the scheduler unloads to make
room. When a load does not fit the free VRAM, the scheduler first unloads
occupants of the **other** categories that may evict them. The matrix is a
one-way hard filter: a category yields only to the categories that list it.

- **Same-slot replacement is always allowed.** Loading a different model
  into a category evicts that category's own occupant, whatever the matrix
  says.
- A model row that sets `min_turn` stays unevictable for other categories
  until it has served that many seconds (see `MODELS.md`). The guard never
  blocks same-slot replacement. It covers only the eviction that another
  category's load requests. An idle-timeout unload and an explicit operator
  unload still evict a young holder.
- WORK is the most protected category, and nothing evicts it. AUX and EMB
  are the most disposable, and both WORK and RP evict them. Only WORK
  evicts RP.
- When several victims would cover the deficit, the planner takes the
  **cheapest sufficient set**: the lowest total VRAM need. On a tie it
  releases the **most disposable** category first, never by slot order. So
  the planner spares RP, the costliest category to bring back, when a
  cheaper and more disposable AUX or EMB model would do.
- A refusal that cannot free enough VRAM names the blockers, for example
  `only WORK can evict RP`, or names a model inside its `min_turn` guard.

The matrix is data, not code. Edit `categories[].evictable_by` in
`simpleharn.json` and restart to change who yields to whom.
