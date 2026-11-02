---
title: SimpleHar SE user guide
owner: operator
status: active
audience: [user, contributor]
source_of_truth_for: [user-facing behavior and setup]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release
---

# SimpleHar SE

A Windows tray harness for local LLM inference engines: one app, one GPU, many
models. It owns your engine processes (llama.cpp `llama-server`, strata Python
servers, ninfer GPU containers), swaps models between four category listeners
with a typed, single-writer scheduler, admits loads on the card's own reading
(the *witness*) minus a reserve, with in-flight bookings — and **refuses, with
reasons**, rather than overcommitting.

Latest release: **v1.0.1** (`v1.0.1` tag). Repository HEAD may be ahead of the
tag; HEAD is not itself a release. `cargo build --release` produces
`target\release\simplehar-se.exe`, or use the shipped exe directly.

## Who this is for

You run local LLMs on one NVIDIA GPU and want:

- several models ready to swap instantly — a coding/agent model, a roleplay
  model, utility and embedding models — each pinned to its own OpenAI-compatible
  endpoint, without juggling console windows;
- one safe budget manager: loads are admitted against what the card actually
  reports, nothing is ever force-loaded onto a full GPU, and every refusal
  names the blockers and the numbers;
- declarative config: a JSON file for the harness, one flat YAML per model —
  hot-reloadable, and every page edit writes back to those exact files.

## Engines and the stack you need

| Engine | What runs | Stack required on the machine |
|---|---|---|
| `llama` (default) | a native `llama-server.exe` process per model, serving a GGUF file | [llama.cpp](https://github.com/ggml-org/llama.cpp) `llama-server.exe` (default expected at `C:\Inference\llama.cpp\llama-server.exe`, configurable) |
| `strata` | `python -m serve.server --engine strata` in a Python venv, on a harness-rendered profile | a strata install with its virtualenv (default `C:\Strata\.venv\Scripts\python.exe`, cwd `C:\Strata`); no per-model profile JSON is needed - the model row's yaml owns the profile fields and the harness renders the JSON |
| `ninfer` | a GPU container inside WSL2, started from a harness-rendered and -staged autostart script and reaped by container name | WSL2 distro + Docker, the ninfer container image; set `wsl_distro` / `wsl_user` in the config |

Common requirements:

- Windows 10/11 with an NVIDIA GPU (memory is read via NVML, `nvidia-smi` fallback).
- The Rust toolchain only if you build from source.

No engine ever starts when the harness boots — engines spawn only when a model
is loaded (by you, or by a request naming it).

## Quick start

```powershell
# from source:
cargo build --release
# place simplehar-se.exe anywhere; create a ModelConf\ folder next to it
# and drop one flat .yaml per model (see "Model files" below)
target\release\simplehar-se.exe
# open the control page
start http://127.0.0.1:8708/
```

No config file is required. On the **first start** the harness probes the real
system (GPU total from NVML, `llama-server.exe` on PATH) and creates
`simplehar-se.json` beside the exe with the detected values — from then on the
file is the truth, so edit it there (delete it to have it re-created). Every
boot also cross-checks the pinned `vram_total_gb` against the card and warns —
never repairs — when they disagree beyond `witness_tolerance_gb`.

## Categories & ports

| Category | Public port | Slots | Purpose | Evicted by |
|---|---|---|---|---|
| WORK | 8708 | 1 | coding/agent work; also hosts the control page + `/har/*` | nobody |
| RP | 8709 | 1 | roleplay | WORK |
| AUX | 8710 | 1–6 | utility models | WORK, RP |
| EMB | 8711 | 1 | embeddings | WORK, RP |

AUX may hold up to six models at once (`max_models` in the config); the others
hold exactly one. Engine ports are allocated per slot sequentially from 18109
(`18109`–`18112` with the default four single-slot categories, up to `18117`
with six AUX slots). Everything here is declarative — see `categories` in the
config.

llama engines are **forced** to `--host 127.0.0.1` by the harness; strata
engines are forced to the same `--host 127.0.0.1` argv (server.py resolves the
flag ahead of any profile key, and the harness-rendered profile carries no
`host` key at all); ninfer containers under template mode are **forced** to a
loopback-only publish (`--publish 127.0.0.1:...`) by the canonical script the
harness renders — a **legacy** ninfer script publishes wherever its own
`docker run` says, so keep that loopback unless you mean to publish.

Each public listener answers OpenAI-style `/v1/models` (category-filtered),
`/v1/chat/completions`, `/v1/completions` and `/v1/embeddings`. A request
naming no model is served by the category's loaded occupant (the most-recently
used one at AUX); when nothing is loaded it loads that category's
`default_model` instead of erroring.

### Unloading priority

The "Evicted by" column is a priority matrix, and it is the only thing that
decides who is unloaded to make room. When a load does not fit the card's free
VRAM, the scheduler unloads a set of **other** categories' occupants before it
refuses; the matrix is a one-way hard filter — a category yields only to the
categories that list it.

- **Same-slot replacement is always allowed**: loading a different model into a
  category evicts that category's own occupant, regardless of the matrix.
- **WORK is the most protected** (evicted by nobody); **AUX and EMB the most
  disposable** (evicted by both WORK and RP); **RP** sits between (evicted only
  by WORK).
- When several victims would cover the deficit, the planner takes the **cheapest
  sufficient set** (lowest total VRAM need); on a tie it releases the **most
  disposable** category first, never by slot order — so RP (costliest to bring
  back) is spared when a cheaper, more disposable AUX/EMB would do.
- A refusal that cannot free enough VRAM names the blockers, e.g.
  `only WORK can evict RP`.

It is data, not code: edit `categories[].evictable_by` in `simplehar-se.json`
and restart to change who yields to whom.

## Configuration: `simplehar-se.json`

Lives beside the executable; override the path with the `SIMPLEHAR_CONFIG`
environment variable — an env-named file is an instruction, not an absence: boot
fails when it does not exist, and first-start creation never writes there. If a
legacy `simplehar.json` sits beside the exe and no `simplehar-se.json` exists yet,
it is copied over once, the old file left untouched for rollback. **The config
file is the truth**: `default_model` changes made on the page persist back to the
exact file boot read (atomic rewrite), partial files are completed, and an
invalid file is fatal with every problem logged — never silently defaulted.

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN — set `control_token` first, see Security |
| `control_token` | *unset* | When set, every route but the page assets demands it (`X-Har-Token` or `Authorization: Bearer`); `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models` (1–6 for AUX, exactly 1 otherwise), `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), `engine_ports` |
| `vram_total_gb` | `72.0` | Admission ceiling. Normally **detected at first start** (NVML total, floored to 0.1 GiB); the default only survives a failed card probe, and every boot warns when the pinned value and the card disagree beyond `witness_tolerance_gb` |
| `vram_reserve_gb` | `2.0` | Kept unreachable by admission |
| `witness_tolerance_gb` | `2.0` | Post-kill driver-lag band: a load may spawn up to this far under the gate (absorbs the lag between a confirmed release and the card's reading catching up); also the settle allowance |
| `greedy_launch_min_free_gb` | `4.0` | Launch floor for `cost: greedy` models without their own `launch_only_with_at_least_vram_free` |
| `load_timeout_s` | `120` | Readiness deadline per load; per-model override via the `load_timeout:` yaml key |
| `measured_sample_s` | `60` | Seconds between witness samples while a model serves |
| `release_timeout_s` | `60` | Kill-confirmation deadline |
| `settle_timeout_s` | `30` | Memory-settle deadline after release |
| `protected_ports` | small reserved set | Ports the harness must never bind or touch — extend it to cover any servers you run manually outside this app |
| `wsl_distro` / `wsl_user` | `Ubuntu` / *(set for ninfer)* | WSL coordinates for ninfer containers |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary. First start prefers a `llama-server.exe` found on PATH (absolute entries only) and records the winner in the created file |
| `engines.strata.python` / `engines.strata.cwd` | documented defaults | Interpreter and working directory for strata launches; per-model `strata_python`/`strata_cwd` still override the section |
| `engines.llama.fit_probe` | `false` | Ask the engine's own `llama-fit-params` calculator (sibling of `llama_server_exe`) before spawn how much a llama row will need; every llama row is projected after boot and rescan, off the hot path |
| `engines.llama.fit_margin_gb` / `engines.llama.mmproj_surcharge_gb` / `engines.llama.fit_probe_timeout_s` | unset | Calibration constants for the projection (added per active GPU; surcharge for vision rows; probe deadline). While the two figures are unset the probe stays silent and rows fall back down the cost ladder — set them once from a calibrated card window |
| `modelconf_dir` | `<exe dir>\ModelConf` | Catalog directory |
| `tray_enabled` | `true` | Tray icon at boot |

## Model files: `ModelConf/*.yaml`

One flat `key: value` file per model. `#` comments are preserved when the
harness rewrites a file, and an unchanged value passes through byte-for-byte.

**What counts as a model file:** a regular file whose extension is `.yaml`
(case-insensitive). `.yml` is not recognized — such a file is silently inert,
not an error. Everything else in `ModelConf` is ignored with zero errors
(readme files, thumbnails, backup copies); symlinks are skipped, never
followed. Only real `*.yaml` files are parsed and can report parse errors.

If an atomic write ever fails its own validation, the harness restores the
original bytes and the error names exactly where every byte ended up — a failed
edit never leaves a half-written model file live.

| Key | Notes |
|---|---|
| `name` | Optional; identity is the file stem, and a mismatching `name` is refused (rename the file or drop the key) |
| `model` | GGUF path (or the engine's model URI). **Required for llama** |
| `engine` | `llama` (default), `strata`, or `ninfer` |
| `category` | `WORK`/`RP`/`AUX`/`EMB` |
| `vram_gb` | Declared admission cost in GiB; `fixed` falls back to `file size × 1.15 + 2`, `exact` refuses without it |
| `cost` | `fixed` (default): size binds admission · `greedy`: offloads to RAM/SSD, size never refuses but a launch floor is mandatory · `exact`: an engine-computed number, required and binding |
| `launch_only_with_at_least_vram_free` | GiB. Refuse the load **before spawn** unless the free card reaches this floor (reserve deducted; floor-meeting free counts only what the harness itself can make free — its own releasable residents — foreign occupancy never counts toward it). When no release the harness can perform would reach the floor, the refusal says so: "No releases allow for the VRAM floor. Check foreign residents." — the thrash guard for big/greedy quants. Greedy models without it inherit `greedy_launch_min_free_gb`; set `0` per-model to opt out |
| `cpu_offload` | `true`/`1`/`on` → forces `-ngl 0` (**llama only** — other engines report an `inert_key` hint and the page offers no CPU control for them) |
| `serve_model` | Expected engine-reported id; mismatch = recorded warning, never trusted |
| `measured_gb` / `measured_gb_min` | The footprint range the harness measured while this model served (high/low end). Written back after a load. Admission books `measured_gb` when `vram_gb` is absent — subject to `measured_attribution`, whose number it is: `engine_report` (the container's own startup reservation) and `per_process` (the witness attributed the bytes to this engine's process) book; `total_diff` (a free-memory delta) displays but never books; an **absent** stamp is grandfathered and books on `fixed` rows. `greedy` never books a measured figure whatever the provenance — it is offloaded by design. Ninfer rows also carry `measured_profile`: an `engine_report` figure is voided when the resolved profile identity changes — the profile name, plus the resolved `kv` for an override line (an edit to the line's `ctx`/`conc`/`kv_dtype` that leaves `kv` alone keeps the figure, since the reservation follows the KV pool) |
| `is_embedding_model` | Truthy forces category `EMB` (conflicts with another explicit category) |
| `context`, `batch`, `flash_attn`, … | Mapped to the corresponding `llama-server` flags. `context` is **llama only** — on other engines it stays an inert key (the scan says so), and the page/API never display it there; the Ctx figure shown for a strata row is its `strata_args` `--max-context` figure (or `--max-context=` spelling; digits only), for a ninfer row the resolved profile's max-context |
| `load_timeout` | Per-model readiness deadline in seconds (llama default 120; strata/ninfer cold starts commonly need 600–1200) |
| `strata_exe` | **Required for strata** (template mode): absolute path of the engine binary the harness-rendered profile names; its presence selects template mode. `serve_model` is required too: it becomes the profile's served model name |
| `strata_tokenizer` | **Required for strata**: absolute tokenizer path the rendered profile names |
| `strata_python` | Interpreter: must be an existing `.exe` **file** — it is the program the harness launches (default `C:\Strata\.venv\Scripts\python.exe`) |
| `strata_cwd` | Working directory; must be an existing directory (default `C:\Strata`) |
| `strata_args` | Comma-separated engine argv tokens rendered into the profile's `args` (weights/quant/context ride here, e.g. `--native D:\Models\m.gguf`). `--host` and `--port` are harness-forced argv and refused in this list; a `strata_vision` block appends the engine's `--vision` switch, so do not spell that token here too |
| `strata_lib_dirs` | Comma-separated absolute directories rendered as the profile's `lib_dirs` |
| `strata_env` | Whitespace-separated `STRATA_<NAME>=<VALUE>` tokens rendered as the profile's `env`; only uppercase `STRATA_`-prefixed engine settings pass |
| `strata_gpu` / `strata_parallel` | Non-negative integers rendered as the profile's `gpu` / `parallel` |
| `strata_gpus_asked` | Truthy renders `gpus_asked: true` in the profile |
| `strata_vision` | Whitespace-separated `k=v` vision block over the closed key set `exe`, `mmproj`, `model`, `gpu`, `max_tokens`, `min_tokens`, `threads` (`exe`/`mmproj`/`model` are absolute paths, required together; `gpu` defaults on); rendering the block also appends the `--vision` switch to `args` |
| `strata_sampling` | Whitespace-separated `key=value` sampling defaults (number or bool) rendered as the profile's `sampling` dict |
| `strata_uncensored` | Strict `true`/`false` rendered as the profile's `uncensored` switch; an explicit `false` is written intent, distinct from absent |
| `strata_config` | **Superseded** (operator ruling 2026-11-02, row 19). A row that still declares it is refused at scan with the migration sentence: run `simplehar-se import strata <profile.json>`, merge its emitted knob block into the row, and delete the `strata_config` line. A row carrying both `strata_exe` and `strata_config` meets the mutual-exclusion refusal - delete the `strata_config` line |
| `extra_args` | **Closed allow-list, not a denial list.** Whitespace-split words are appended to the argv only if each is a reviewed option — `--embedding`, `--embeddings`, `--pooling <mode>`, `--temp`, `--top-k`, `--top-p`, `--min-p`, `--repeat-penalty`, `--seed`, `--warmup`, `--no-warmup` — plus the value words it consumes (a value may not be option-shaped, so it cannot smuggle another flag). **Anything else is refused with the launch**, naming the word: model, host, port, gpu-layers, jinja, api-key, lora, rpc and every other option are admitted by absence, so there is nothing to dodge. The harness's forced `--host 127.0.0.1`, `--port`, and `cpu_offload` `-ngl 0` are appended after it and win |
| `ninfer_image` | Selects **template mode, the supported path** — its presence means the harness renders the canonical autostart script from your yaml, stages it into the distro, verifies it by distro-side checksum, and launches THAT. Exactly one of `ninfer_image`/`ninfer_autostart` must be present. Its required companions: `ninfer_model_name` (the directory holding the artifact), `ninfer_artifact` (the file inside it), `ninfer_model_roots` (comma-separated absolute WSL directories searched in order) — all plain words, refused with the key named. `ninfer_internal_port` (the port the container binds inside its own namespace) is optional and defaults to the SE internal number **8808** (the harness's 87xx family at +100, clear of the manual 80xx world, the 87xx listeners, and the 181xx engine plan); it may not name a `protected_ports`, public-listener, or harness engine port — the manual/harness separation covers container-internal numbers too, so a row that borrows one is refused at load and named by the rescan |
| `ninfer_serve_cmd` | The server command inside the container (default `ninfer-serve`) |
| `ninfer_gpu_device` | Comma-separated device selector for `--gpus device=` (default `0`) |
| `ninfer_serve_args` | Extra flags appended to the serve line: shell-bare tokens. Both shipped engine parsers reject the `--flag=value` form, so spell options with a space and let the value ride as its own token (`--spec dflash2`). Option tokens must be long (`--…`); anything the canonical script owns — `--host`, `--port`, `--api-key`, `--data-dir`, `--data-dir-permissions`, `--model`, `--model-id`, `--log-level`, `--log-dir`, `--uds`, `--ssl-keyfile`, `--ssl-certfile`, `--max-context`, `--kv-capacity`, `--max-concurrency`, `--kv-dtype` — is refused by name AND by any abbreviation or extension of it (`--max-cont`, `--host-context-mib` too), so your args can never resize what the cost estimate was filed from |
| `ninfer_model_id` | Optional OpenAI-style served id, passed as `--model-id` |
| `ninfer_env` | Extra container environment as `NAME=VALUE` tokens — an **allow-list**, not a parse rule: the only admissible pair today is `CUDA_MODULE_LOADING=EAGER\|LAZY` (the NInfer-all cards' cold-start flag). An arbitrary name (`LD_PRELOAD`, `PATH`) is refused by name; growing the list is a code review, never a yaml edit |
| `ninfer_key_file` | Template mode only: where `ninfer_key: true` reads the API key — exactly `$HOME/.simplehar/keys/<file>.key` in the distro, a bare word with no traversal or subdirectories (the script `cat`s it into `--api-key`, so an arbitrary path would let the yaml export any distro secret). Default `$HOME/.simplehar/keys/<ninfer_container>.key`. A legacy script reads its own `$(dirname "$0")/.api_key` instead; this key is not read there |
| `ninfer_autostart` | **Legacy path.** Path (inside WSL) of your own autostart script; invoked as `bash <script> <profile> <key\|nokey> <port>` via `wsl.exe`. Must be a plain absolute path — no spaces, no shell metacharacters, no leading `~`. Mutually exclusive with `ninfer_image`: prefer the template — the harness cannot vouch for what a hand-written script does to your card or your network |
| `ninfer_container` | **Required for ninfer.** Container name; reaped with `docker rm -f` on unload, scoped strictly to the name this harness started (claim ledger: `ninfer-claims.json` beside the harness exe). Must be docker-legal and must not look like a container ID (4+ hex chars) |
| `ninfer_profile` | Template mode: one of the built-in VRAM profiles `a`–`e` (default `b`; the table is the operator's own — `a` 262k ctx/524k kv/2 conc, `b` 262k/262k/1, `c` 131k/131k/1, `d` 16k/32k/2 nvfp4, `e` 64k/64k/1) or a custom lowercase name resolved by a `ninfer_profile_<name> = ctx=… kv=… conc=… kv_dtype=…` line (`kv_dtype` one of `fp8`/`nvfp4`/`fp16`/`bf16`/`int8`/`rk8v4`, `kv` must cover `ctx`). The override line applies to custom names only — the built-in table wins for `a`–`e`, so a `ninfer_profile_b`-style line is refused at launch rather than silently ignored. Legacy mode: passed to your script as `$1` verbatim |
| `ninfer_key` | Truthy (`true`/`1`/`yes`/`on`) sends `key` as `$2` (template mode: `cat`s the `ninfer_key_file` into `--api-key`); anything else sends `nokey` |

Strata rows are rendered, not handed to the engine: at every load the harness
writes the full profile JSON into `.simplehar\strata-profiles` beside the exe
(content-addressed `<model>-<hash>.json`, atomic write, keep-N prune) and
launches only its own staged copy - see "Strata engines" below.

Unknown keys are preserved for reference and reported as `unknown_key`
warnings — the model still loads. `host` and `port` keys are harness-owned and
inert: the bind is forced to loopback and the port is assigned from the
category config. `fit_gb` is the exception: it is the engine calculator's
projection slot (see below), so a hand-written value is ignored and stripped,
never booked.

**Cost sources** decide which number binds admission, strongest word wins:
an `engine_report` (ninfer container's own startup reservation) or, for llama, a
**Fit** projection — when `engines.llama.fit_probe` is on, the harness runs the
engine's own `llama-fit-params` calculator before spawn and books that figure if
it exceeds the declared `vram_gb`. A Fit figure is **never persisted** (the
stale-measurement failure class cannot exist for it) and is re-verified against
the calculator's exact version on every launch. On the page a Fit row reads
amber "engine-projected" until a witnessed serve confirms it within the margin,
then turns green — colour is a confidence signal, the number never changes
between the two.

### Strata engines

Strata models (`engine: strata`) run in **template mode, the supported path**:
the row yaml owns the profile fields (the `strata_*` keys above) and the harness
renders the full profile JSON at every load, launching
`python -m serve.server --engine strata --host 127.0.0.1 --config <rendered> --port <assigned>`
in `strata_cwd`. The render is staged under `.simplehar\strata-profiles` beside
the exe (content-addressed `<model>-<hash>.json`, atomic write, keep-N prune)
and the spawn rewrites `--config` to exactly that file - the child can never
be pointed at another profile. The bind is the forced `--host 127.0.0.1` argv and
the port the harness-assigned `--port` (server.py resolves both flags ahead of
any profile key), so the rendered profile carries no `host`, `port` or key at
all. Readiness requires `/health` 200 **and** the JSON `loaded` flag not being
`false` (strata answers 200 while weights stream in). A strata engine starts
only when you load it (or a client request names it); booting the harness
spawns nothing.

The external-profile mode is superseded (operator ruling 2026-11-02, row 19):
`simplehar-se import strata <profile.json>` reads a manual profile and **emits**
the yaml knob block - it never touches the row or the profile file, so adoption
is your own paste of the emitted block. `C:\Strata` stays the manual-launch
home the harness never reads on the serve path; the import CLI is the one
sanctioned reader.

### Ninfer engines

Ninfer models (`engine: ninfer`) are GPU containers in WSL2, launched as a
short-lived `wsl.exe` client; the container outlives the client, so unload reaps
it by name (`docker rm -f <ninfer_container>`) and the release evidence is the
port closing, not the client's exit. Under **template mode** (`ninfer_image`
present) the client runs the harness's own canonical script: rendered from your
yaml, staged base64 through a stdin driver to
`~/.simplehar/autostart/<container-slug>-<content-hash>.sh`, checksum-verified on the
distro before and after the atomic publish, and pruned to the newest renders of
that container. The rendered launch carries the incident-review guarantees —
`--restart no` (nothing revives on WSL boot without the harness deciding),
loopback-only publish, bounded container logs (`json-file`, 25 MiB × 5) — plus
your profile, key, and serve args. The engine itself logs a JSON memory summary
at startup into the container log; the harness reads it from the container's own
logs and books it as the `engine_report` measured figure — a container that
never logs one is measured from free memory instead and labelled `total_diff`:
shown, never booked. Under **legacy mode**
(`ninfer_autostart`) the client runs your script instead, as
`bash <ninfer_autostart> <profile> <key|nokey> <assigned-port>`. Both modes then
wait for `/health` 200 **then** `/v1/models` 200. Every ninfer model the catalog
carries is managed; servers you started manually are never registered or
monitored — they are only subtracted from VRAM via the witness.

If a harness crash orphans a container, the next load of that name reconciles
by removing the stale container before autostart runs. At boot, an orphan is
reclaimed only when ownership is provable (the name is declared by a `ninfer`
model, the name is legal, and the claim ledger proves this harness started it) —
otherwise the harness warns and leaves it alone. A running container that matches
a catalog `ninfer_container` but holds no claim is therefore never killed and
never silent either: boot and every rescan name it in the log, in
`/har/status.stranded_containers`, and in an amber page badge — its VRAM sits
occupied and the harness will never reclaim it (report only). Duplicate
`ninfer_container` names across models are refused at scan, because reaping is
by name.

#### autostart.sh template

The authoritative template is compiled into the harness
(`CANONICAL_NINFER_SCRIPT`) and is what template mode renders and stages — you
never touch it. A readable copy ships as **`ninfer-autostart-template.sh`** next
to this README (and beside the exe in the release package); read it to see
exactly what a launch does, and start from it when you must write a **legacy**
`ninfer_autostart` script. The legacy contract: the harness invokes your script
as `bash <script> <profile> <key|nokey> <port>` inside WSL — `$1` selects a
resource profile, `$2` toggles the service's API key, `$3` is the **host** port
the harness assigned (map it to the container's internal port), and the script
exits once the container is starting; the harness then polls `/health` and
`/v1/models` itself. Keep the container name equal to the model's
`ninfer_container`.

```bash
#!/usr/bin/env bash
# autostart.sh — start one NInfer service with a VRAM profile: a|b|c|d|e (default b)
set -euo pipefail

NAME="SE-nin-mymodel"                 # == ninfer_container in the model yaml
IMAGE="ninfer-mymodel:local"          # the image your artifact requires
MODEL_NAME="My-Model-NInfer"          # directory name holding the artifact
ARTIFACT="my-model.ninfer"            # file name inside the model directory
MODEL_ID="my-model"                   # OpenAI-style served id (== serve_model in yaml)
INTERNAL_PORT=8808                    # R10: the SE internal number (87xx family +100),
                                      # never a manual (protected 80xx) or harness
                                      # (87xx listener / 181xx engine) port number

# Model directories to search for ARTIFACT (first hit wins).
MODEL_ROOTS=(
  "/mnt/d/Models"
  "/home/you/ai/models"
)
MODEL_DIR=""
for root in "${MODEL_ROOTS[@]}"; do
  if [[ -f "$root/$MODEL_NAME/$ARTIFACT" ]]; then MODEL_DIR="$root/$MODEL_NAME"; break; fi
done
[[ -n "$MODEL_DIR" ]] || { echo "autostart: no '$ARTIFACT' under ${MODEL_ROOTS[*]}" >&2; exit 1; }

CFG="${1:-b}"
WITH_KEY="${2:-}"
case "$CFG" in
  a) MAX_CTX=262144; KV_CAP=524288; CONC=2 ;;  # big context, 2 concurrent
  b) MAX_CTX=262144; KV_CAP=262144; CONC=1 ;;  # default: large single agent context
  c) MAX_CTX=131072; KV_CAP=131072; CONC=1 ;;  # most VRAM-conserving
  d) MAX_CTX=16384;  KV_CAP=32768;  CONC=2 ;;  # short context, 2 concurrent
  e) MAX_CTX=65536;  KV_CAP=65536;  CONC=1 ;;  # mid context, single agent
  *) echo "usage: $0 <a|b|c|d|e> [key]" >&2; exit 1 ;;
esac

if [[ "$WITH_KEY" == "key" ]]; then
  API_KEY="$(cat "$(dirname "$0")/.api_key")"  # keep the secret in a file, never here
  API_KEY_ARGS=(--api-key "$API_KEY")
else
  API_KEY_ARGS=()
fi

PUBLISH="${3:?harness must pass the publish port as arg 3}"  # host port from the harness (required)

# Idempotent: reclaim a stale container of OUR name before starting.
docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" \
  --gpus '"device=0"' \
  --restart no \
  --publish "127.0.0.1:${PUBLISH}:${INTERNAL_PORT}" \
  --volume "$MODEL_DIR:/models:ro" \
  --log-driver json-file --log-opt max-size=25m --log-opt max-file=5 \
  "$IMAGE" \
  serve "/models/$ARTIFACT" \
  --host 0.0.0.0 \
  --port "$INTERNAL_PORT" \
  --model-id "$MODEL_ID" \
  "${API_KEY_ARGS[@]}" \
  --max-context "$MAX_CTX" \
  --kv-capacity "$KV_CAP" \
  --max-concurrency "$CONC"

echo "autostart: '$NAME' profile '$CFG' (ctx=$MAX_CTX kv=$KV_CAP conc=$CONC key=$([[ "$WITH_KEY" == "key" ]] && echo on || echo off)) on port $PUBLISH->$INTERNAL_PORT"
```

Notes:

- `--publish 127.0.0.1:...` keeps the container loopback-only. Publishing on all
  interfaces exposes an unauthenticated engine to your LAN — do it only with the
  `key` profile (and a real `.api_key` file), or not at all.
- `--restart no` is not a preference: a `--restart` policy revives containers on
  WSL boot outside any harness decision, with no ledger entry and no reap path
  (the NINFER-8004 incident). Template mode hard-codes it; keep it in a legacy
  script.
- R10 namespaces: the container's internal port is a **third namespace**, disjoint
  from the manual (`protected_ports`) and harness (listeners, engine plan) sets —
  keep your legacy script's `INTERNAL_PORT` clear of all three (8808 is the SE
  internal number). The harness validates its `$3` offer but never rewrites your
  script; a legacy row that also declares `ninfer_internal_port` is refused at
  load for a borrowed number even though the script would ignore the key, so
  legacy rows should leave that key unset.
- The name-scoped `docker rm -f` before `docker run` makes the launch idempotent
  and complements the harness's orphan reconciliation.
- The `--log-driver json-file --log-opt` pair bounds container logs; template
  mode always sets them, so a week of a serving engine never fills the distro.
- `serve_model` in the model yaml must match `--model-id`, or the harness records
  a served-id warning.
- `chmod +x` the script; the harness runs it with `bash`, so the shebang is
  documentation, but the executable bit avoids surprises when testing by hand.

## Rescan

After editing files, press **Rescan** (or `POST /har/rescan`). A rescan
re-reads the catalog only: a model that is serving keeps the launch it was
spawned with, so an edit to a live model's yaml takes effect on its next load
(the page marks it **amber** while the file differs from the running bytes;
unload + reload to apply). A queued load whose yaml moved is retired with a
`superseded by rescan` note — re-queue it. Two files whose names collapse to
one identity are both reported as `duplicate_name` and loads of that name stay
refused as ambiguous until one is renamed.

## Control page

`http://127.0.0.1:8708/` — slot cards + memory bars (observed VRAM from the
card's own reading, the reserve marked as a hatched band, in-flight bookings
noted, host RAM), category tabs, per-model rows (Load / Set default, state dot
+ name, category select, CPU checkbox, Engine, Ctx, Cost, Files), recent jobs,
collapsible harness log. A loaded row shows `launched_at` and turns amber while
the model's file differs from the bytes its launch used. Updates push over SSE
and degrade to polling; adaptive light/dark theme with a toggle.

## Tray

Four states: green = loaded, yellow = swap in flight, gray = idle,
**red = listener down**. The menu offers Open Web UI, Unload All /
per-category, Exit.

## HTTP control API (control listener, 8708)

| Endpoint | Body / meaning |
|---|---|
| `GET /health` | Liveness — token-gated like every other route once `control_token` is set |
| `GET /har/status` | Full snapshot incl. `mem`, `jobs`, `last_error`, boot timings |
| `GET /har/events` | SSE state pushes |
| `GET /har/jobs`, `GET /har/jobs/{id}` | Job ledger |
| `GET /har/log?limit=n` | Ring buffer of harness events |
| `GET /har/models` | Full catalog (page view; includes paths). The `context` field is the **effective served context**, never a stale row key: llama = the row `context` knob; strata = the context figure riding its `strata_args` argv (`--max-context` token or `--max-context=` spelling, digits only — anything else answers `null`); ninfer = the max-context of the profile its launcher resolves (built-in table or `ninfer_profile_<name>` line — the identical resolution the spawn encodes); every other engine answers `null`, and so does any row whose effective context does not resolve |
| `POST /har/load` | `{model}` — loads into the model's own category (from its yaml) |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all) — never both keys at once (`400`). Reply status is honest: `unloading` (counts `unloading`/`jobs_cancelled`; 200 is an acceptance, release lands on the job ledger), `cancelled` (only load jobs were cancelled, nothing installed was freed — cancelling an in-flight unload is not this: its re-armed release rides `unloading`), `nothing_unloaded`. Unloading a *loading* slot cancels that swap (cleanup still completes) |
| `POST /har/rescan` | `{}` catalog rescan |
| `POST /har/config/default` | `{category, model \| null}` — `model` is required; a string sets the default (stored as its canonical name), `null` clears it. Persists to the config file |
| `POST /har/config/logging` | `{enabled: bool}` — toggles the verbose harness log |
| `POST /har/config/model` | `{model, category?, cpu?}` — writes back to the model yaml |

Errors carry both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope, with the same values in both. The `code`
is one of: `not_found` (404), `bad_request` (400), `unauthorized` (401, control
token), `forbidden` (403, cross-site `Origin` on a POST), `conflict` (409,
cancelled / superseded / port still held / an edit the scanner would refuse to
publish), `unsupported_media_type` (415), `unavailable` (503, admission refused
with the numbers and blockers), `below_launch_floor` (503, refused before
spawn), `witness_unavailable` (503, card unreadable), `upstream` (502),
`not_implemented` (501), `internal` (500). Refusals that carry numbers do so in
`message` and in the job ledger, never in a bare status.

## Autostart

Create `autostart.vbs` at the path the `Run` key below names (adapt paths), to
run the harness hidden at logon, once:

```vb
Option Explicit
Dim dir, exe, sh, wmi, procs
dir = "C:\Inference\SimpleHar-SE\target\release"
exe = dir & "\simplehar-se.exe"
If Not CreateObject("Scripting.FileSystemObject").FileExists(exe) Then WScript.Quit 1
Set sh = CreateObject("WScript.Shell")
Set wmi = GetObject("winmgmts:\\.\root\cimv2")
Set procs = wmi.ExecQuery("Select * from Win32_Process Where Name = 'simplehar-se.exe'")
If procs.Count > 0 Then WScript.Quit 0
sh.CurrentDirectory = dir
sh.Run """" & exe & """", 0, False
```

```powershell
# register / unregister
Set-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name SimpleHarSE -Value 'wscript.exe "C:\Inference\SimpleHar-SE\autostart.vbs"'
Remove-ItemProperty -Path "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run" -Name SimpleHarSE
```

## Troubleshooting

- **Port won't bind** — the status line and log name it; if a Hyper-V/WSL2
  dynamic reservation swallowed the port the harness already reports
  `port N lies in the reserved range` (`netsh interface ipv4 show
  excludedportrange protocol=tcp` shows them all; reserve yours permanently with
  `netsh int ipv4 add excludedportrange protocol=tcp startport=N numberofports=1`).
- **Missing model file at boot** — a `llama` row whose GGUF is gone is dropped
  by the scan and reported as `! <file>.yaml: model file not found: <path>`;
  a row that publishes but cannot load leads its log line with `!`. Either way
  you see it before the first load tries it.
- **Refusals** — every rejection names the blockers and the numbers
  (`/har/status` jobs + `last_error`); nothing over-admits, so a refusal means
  unload something — the card's own reading (the witness) is the truth.
- **`launch floor not met`** (code `below_launch_floor`) — the card holds too
  little physically free VRAM for that model to launch sanely (foreign
  processes included). The load was refused **before spawn**, so nothing
  thrashed; free VRAM or lower the model's
  `launch_only_with_at_least_vram_free` (the printed `free` is the number the
  gate compared — reserve deducted, same-slot replacement credited).
- **Second instance** — a single-instance guard exits duplicates immediately.
- Logs: `logs/` next to the exe. The harness keeps **its own** streaming log,
  `logs/harness-<boot-epoch>.log` — one JSON line per event, written unbuffered so a
  killed process still keeps what it logged, never quieter than `INFO` whatever
  `SIMPLEHAR_LOG` asks for, and rolling to a new generation at 8 MiB (the last 10
  harness log files survive, rolled generations included). Engine children keep
  `logs/<model>-<epoch>.log`, pruned to 50 on a separate budget the harness log can
  neither consume nor be consumed by. `GET /har/log` reads the in-memory ring; a
  non-zero `log_write_failures` at `/har/status` means the file sink died and this boot
  leaves an incomplete artifact.

## Security note

Set `control_token` (or `SIMPLEHAR_TOKEN`, which wins so the secret can stay
out of the file) and **every route requires it** on `X-Har-Token` or
`Authorization: Bearer` — `/har/*`, each listener's `/v1/*`, and `GET /health`
too, so a liveness probe must carry the token or it will read 401 and look like
a dead listener. Only the page and the assets it links answer unauthenticated,
so the page can load and ask. The structural guards run **before** the token
check: a cross-site `Origin` on a POST is `403` whether or not a token is
presented, and a missing `application/json` content-type is `415` even with a
valid one — the token decides *who*, never *whether* the request shape is
acceptable. The token is never logged and never accepted from a URL.

With no token set the harness has **no authentication**: `internal_host:
0.0.0.0` (Publish to LAN) then trusts every machine on the segment to
load/unload models and drive the GPUs; it exists for LAN boxes and warns loudly
at boot. Default is loopback only. The token guards only the harness's own
listeners — engine ports have no harness auth, so llama and strata engines are
forced loopback regardless of `internal_host`, template-mode ninfer publishes
loopback-only, and only a legacy ninfer script that itself chooses to publish
can put an unauthenticated engine on the segment.
