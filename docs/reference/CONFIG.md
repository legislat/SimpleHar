---
title: Configuration reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [harness configuration keys, category eviction rules]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release
---

# Configuration reference

Up: `docs/DOCS_README.md`. This file owns the harness configuration keys and
the category eviction rules. The category and port matrix itself lives in
`README.md` (Categories and ports).

## The config file: `simplehar-se.json`

The file lives beside the executable. Override the path with the
`SIMPLEHAR_CONFIG` environment variable. A file named by that variable is an
instruction, not an absence: boot fails when it does not exist, and
first-start creation never writes there. Delete the default file to have the
next boot re-create it at the default path. A file named by `SIMPLEHAR_CONFIG`
must exist. If a legacy `simplehar.json` sits
beside the exe and no `simplehar-se.json` exists yet, the harness copies it
over once and leaves the old file untouched for rollback. **The config file
is authoritative.** A `default_model` change made on the page persists back
to the exact file boot read, through an atomic rewrite. The harness completes
partial files. An invalid file stops boot and the harness logs every problem;
it never applies silent defaults.

## Fields

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN. Set `control_token` first; see the Security note in `README.md`. |
| `control_token` | *unset* | When set, every route except the page assets requires it, on `X-Har-Token` or `Authorization: Bearer`. `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file. |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models` (1-6 for AUX, exactly 1 otherwise), `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), and `engine_ports`. An omitted `evictable_by` and an empty list mean the same: nobody may evict that category. Boot names every non-WORK category left with no evictor. |
| `vram_total_gb` | `72.0` | Admission ceiling. First start normally **detects it** (NVML total, floored to 0.1 GiB). This default survives only a failed card probe. Every boot warns when the stored value and the card disagree beyond `witness_tolerance_gb`. It never repairs the value. |
| `vram_reserve_gb` | `2.0` | A margin that admission never uses. |
| `witness_tolerance_gb` | `2.0` | Tolerance band for driver lag after a kill. A load may spawn up to this far under the gate. The band absorbs the lag between a confirmed release and the card reading catching up. It is also the settle allowance. |
| `greedy_launch_min_free_gb` | `4.0` | Launch floor for `cost: greedy` models without their own `launch_only_with_at_least_vram_free`. |
| `load_timeout_s` | `120` | Readiness deadline per load. A model can override it with the `load_timeout:` yaml key. |
| `measured_sample_s` | `60` | Seconds between witness samples while a model serves. |
| `release_timeout_s` | `60` | Kill-confirmation deadline. |
| `settle_timeout_s` | `30` | Memory-settle deadline after release. |
| `protected_ports` | small reserved set | Ports the harness must never bind or touch. Extend the set to cover servers you run manually outside this app. |
| `wsl_distro` / `wsl_user` | `Ubuntu` / *(set for ninfer)* | WSL coordinates for ninfer containers. |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary. First start prefers a `llama-server.exe` found on PATH (absolute entries only) and records the winner in the created file. |
| `engines.strata.python` / `engines.strata.cwd` | documented defaults | Interpreter and working directory for strata launches. The per-model `strata_python`/`strata_cwd` keys still override this section. |
| `engines.llama.fit_probe` | `false` | When true, the harness asks the engine's own `llama-fit-params` calculator (a sibling of `llama_server_exe`) before spawn how much a llama row will need. The harness projects every llama row after boot and rescan, off the hot path. |
| `engines.llama.fit_margin_gb` / `engines.llama.mmproj_surcharge_gb` / `engines.llama.fit_probe_timeout_s` | unset | Calibration constants for the projection. The margin is added per active GPU. The surcharge applies to vision rows. The timeout is the probe deadline. While the two figures are unset the probe stays silent and rows fall back down the cost ladder. Set them once from a calibrated card window. |
| `modelconf_dir` | `<exe dir>\ModelConf` | Catalog directory. |
| `tray_enabled` | `true` | Tray icon at boot. |
| `allow_mock_engine` | `false` | Test-only. When true, boot registers the mock engine adapter and warns that it is not for production. A row with `engine: mock` is refused when the grant is false. |

## The `categories` key

Each entry in `categories` declares one category. The keys:

- `id`: the category identity, one of `WORK`, `RP`, `AUX`, `EMB`.
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
- `engine_ports`: the per-slot engine port plan the harness assigns.

Every value is declarative.

## Port plan

The harness assigns engine ports per slot, sequentially from 18109. The
default four single-slot categories use `18109`-`18112`. With six AUX slots
the plan reaches `18117`.

## Eviction rules

The `evictable_by` field is the eviction priority matrix: the "Evicted by"
column of the matrix in `README.md` (Categories and ports). It is the only
rule that decides which models the scheduler unloads to make room. When a
load does not fit the free VRAM, the scheduler first unloads occupants of
the **other** categories that may evict them. The matrix is a one-way hard
filter: a category yields only to the categories that list it.

- **Same-slot replacement is always allowed.** Loading a different model
  into a category evicts that category's own occupant, whatever the matrix
  says.
- WORK is the most protected category, and nothing evicts it. AUX and EMB
  are the most disposable, and both WORK and RP evict them. Only WORK
  evicts RP.
- When several victims would cover the deficit, the planner takes the
  **cheapest sufficient set**: the lowest total VRAM need. On a tie it
  releases the **most disposable** category first, never by slot order. So
  the planner spares RP, the costliest category to bring back, when a
  cheaper and more disposable AUX or EMB model would do.
- A refusal that cannot free enough VRAM names the blockers, for example
  `only WORK can evict RP`.

The matrix is data, not code. Edit `categories[].evictable_by` in
`simplehar-se.json` and restart to change who yields to whom.
