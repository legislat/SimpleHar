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

SimpleHar SE is a Windows tray harness for local LLM inference engines.
One app, one GPU, many models. The harness starts, monitors, and stops your
engine processes: llama.cpp `llama-server`, strata Python servers, and ninfer
GPU containers. One scheduler swaps models between four category listeners.
Only that scheduler changes scheduler state, and its commands are typed. The
harness admits loads on the reading the card itself gives (the witness), minus
a reserve, with in-flight bookings counted. The harness refuses to
overcommit, and it names its reasons.

Latest release: **v1.0.1** (`v1.0.1` tag). Repository HEAD may be ahead of
the tag; HEAD is not itself a release. `cargo build --release` produces
`target\release\simplehar-se.exe`, or use the shipped exe directly.

## Terms

- **harness**: this application, SimpleHar SE.
- **engine**: an inference server the harness starts and stops (`llama`,
  `strata`, or `ninfer`).
- **category**: one of the four model groups (WORK, RP, AUX, EMB).
- **listener**: the public HTTP port that serves one category.
- **slot**: one model position inside a category.
- **witness**: the live VRAM reading taken from the GPU.
- **admission**: the decision that allows or refuses a model load.
- **scheduler**: the single component that changes slot and budget state.
- **catalog**: the set of model rows built from the `ModelConf` files.

## Who this is for

You run local LLMs on one NVIDIA GPU. You want:

- Several models that swap quickly: a coding/agent model, a roleplay model,
  and utility and embedding models. Each model keeps its own
  OpenAI-compatible endpoint. You do not manage console windows.
- One safe budget manager. The harness admits loads against what the card
  actually reports. It never force-loads onto a full GPU. Every refusal
  names the blockers and the numbers.
- Declarative configuration: one JSON file for the harness and one flat YAML
  file per model. Both hot-reload. Every edit on the page writes back to
  those exact files.

## Engines and the stack you need

| Engine | What runs | Stack required on the machine |
|---|---|---|
| `llama` (default) | a native `llama-server.exe` process per model, serving a GGUF file | [llama.cpp](https://github.com/ggml-org/llama.cpp) `llama-server.exe` (default expected at `C:\Inference\llama.cpp\llama-server.exe`, configurable) |
| `strata` | `python -m serve.server --engine strata` in a Python venv, on a harness-rendered profile | A strata install with its virtualenv (default `C:\Strata\.venv\Scripts\python.exe`, cwd `C:\Strata`). No per-model profile JSON is needed. The model row's yaml carries the profile fields, and the harness renders the JSON. |
| `ninfer` | a GPU container inside WSL2. The harness renders and stages the autostart script, starts the container, and reaps it by container name. | A WSL2 distro and Docker, plus the ninfer container image. Set `wsl_distro` / `wsl_user` in the config. |

Common requirements:

- Windows 10/11 with an NVIDIA GPU. The harness reads GPU memory through
  NVML, with an `nvidia-smi` fallback.
- The Rust toolchain, only when you build from source.

The harness starts no engine at boot. An engine starts only when a model
loads, from you or from a request that names the model.

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

No config file is required. On the first start, the harness probes the real
system: the GPU total from NVML and `llama-server.exe` on PATH. It creates
`simplehar-se.json` beside the exe with the detected values. From then on,
that file is authoritative, so edit it there. Delete it to have the harness
re-create it. Every boot also compares the stored `vram_total_gb` against
the card. It warns when the two disagree beyond `witness_tolerance_gb`. It
never repairs the value.

## Categories and ports

| Category | Public port | Slots | Purpose | Evicted by |
|---|---|---|---|---|
| WORK | 8708 | 1 | coding/agent work; also hosts the control page + `/har/*` | nobody |
| RP | 8709 | 1 | roleplay | WORK |
| AUX | 8710 | 1-6 | utility models | WORK, RP |
| EMB | 8711 | 1 | embeddings | WORK, RP |

AUX can hold up to six models at once (`max_models` in the config). Each
other category holds exactly one. The harness assigns engine ports per slot,
sequentially from 18109. The default four single-slot categories use
`18109`-`18112`. With six AUX slots the plan reaches `18117`. Every value
here is declarative; see `categories` in the config.

The harness forces every llama engine to `--host 127.0.0.1`. It forces the
same `--host 127.0.0.1` argv for strata engines. strata's server.py resolves
that flag before any profile key, and the harness-rendered profile carries
no `host` key at all. The canonical script the harness renders forces every
template-mode ninfer container to a loopback-only publish
(`--publish 127.0.0.1:...`). A **legacy** ninfer script publishes wherever
its own `docker run` line says. Keep a legacy script on loopback unless you
mean to publish.

Each public listener answers the OpenAI-style endpoints `/v1/models`
(filtered by category), `/v1/chat/completions`, `/v1/completions`, and
`/v1/embeddings`. A request that names no model goes to the loaded model of
that category. At AUX this is the most recently used model. When nothing is
loaded, the harness loads that category's `default_model` instead of
returning an error.

### Unloading priority

The "Evicted by" column is the eviction priority matrix. It is the only rule
that decides which models the scheduler unloads to make room. When a load
does not fit the free VRAM, the scheduler first unloads occupants of the
**other** categories that may evict them. The matrix is a one-way hard
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

## Configuration: `simplehar-se.json`

The file lives beside the executable. Override the path with the
`SIMPLEHAR_CONFIG` environment variable. A file named by that variable is an
instruction, not an absence: boot fails when it does not exist, and
first-start creation never writes there. If a legacy `simplehar.json` sits
beside the exe and no `simplehar-se.json` exists yet, the harness copies it
over once and leaves the old file untouched for rollback. **The config file
is authoritative.** A `default_model` change made on the page persists back
to the exact file boot read, through an atomic rewrite. The harness completes
partial files. An invalid file stops boot and the harness logs every problem;
it never applies silent defaults.

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN. Set `control_token` first; see Security. |
| `control_token` | *unset* | When set, every route except the page assets requires it, on `X-Har-Token` or `Authorization: Bearer`. `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file. |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models` (1-6 for AUX, exactly 1 otherwise), `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), and `engine_ports`. |
| `vram_total_gb` | `72.0` | Admission ceiling. First start normally **detects it** (NVML total, floored to 0.1 GiB). This default survives only a failed card probe. Every boot warns when the stored value and the card disagree beyond `witness_tolerance_gb`. |
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

## Model files: `ModelConf/*.yaml`

Write one flat `key: value` file per model. The harness keeps `#` comments
when it rewrites a file. An unchanged value passes through byte-for-byte.

**What counts as a model file:** a regular file whose extension is `.yaml`
(case-insensitive). The harness does not recognize `.yml`; such a file is
inert, silently and without an error. The harness also ignores everything
else in `ModelConf`, with zero errors: readme files, thumbnails, and backup
copies. The harness skips symlinks and never follows them. Only real
`*.yaml` files are parsed, and only they can report parse errors.

When an atomic write fails its own validation, the harness restores the
original bytes. The error names exactly where every byte ended up. A failed
edit never leaves a half-written model file in use.

| Key | Notes |
|---|---|
| `name` | Optional. The file stem is the model identity. A `name` that mismatches the stem is refused: rename the file or drop the key. |
| `model` | GGUF path (or the engine's model URI). **Required for llama**. |
| `engine` | `llama` (default), `strata`, or `ninfer`. |
| `category` | `WORK`/`RP`/`AUX`/`EMB`. |
| `vram_gb` | Declared admission cost in GiB. `fixed` falls back to `file size x 1.15 + 2`. `exact` refuses without it. |
| `cost` | `fixed` (default): the declared size binds admission. `greedy`: the model offloads to RAM/SSD; the size never refuses, but a launch floor is mandatory. `exact`: an engine-computed number, required and binding. |
| `launch_only_with_at_least_vram_free` | GiB. The harness refuses the load **before spawn** unless free VRAM reaches this floor. The reserve is deducted. Free VRAM that meets the floor counts only what the harness itself can make free: its own releasable residents. Foreign occupancy never counts toward the floor. When no release the harness can perform would reach the floor, the refusal says so: "No releases allow for the VRAM floor. Check foreign residents." This is the thrash guard for large and greedy quants. Greedy models without this key inherit `greedy_launch_min_free_gb`. Set `0` on a model to opt out. |
| `cpu_offload` | `true`/`1`/`on` forces `-ngl 0` (**llama only**). Other engines report an `inert_key` hint, and the page offers no CPU control for them. |
| `serve_model` | The expected engine-reported id. A mismatch records a warning; the harness never trusts it. |
| `measured_gb` / `measured_gb_min` | The footprint range the harness measured while this model served: `measured_gb` is the high end and `measured_gb_min` the low end. The harness writes them back after a load. When `vram_gb` is absent, admission books `measured_gb`. The `measured_attribution` stamp decides whose number it is, and only these attributions book: `engine_report` (the container's own startup reservation) and `per_process` (the witness attributed the bytes to this engine's process). A `total_diff` figure (a free-memory delta) displays but never books. An **absent** stamp is grandfathered and books on `fixed` rows. `greedy` never books a measured figure, whatever the provenance, because it is offloaded by design. Ninfer rows also carry `measured_profile`: an `engine_report` figure is voided when the resolved profile identity changes. The identity is the profile name, plus the resolved `kv` for an override line. An edit to the line's `ctx`/`conc`/`kv_dtype` that leaves `kv` alone keeps the figure, because the reservation follows the KV pool. |
| `is_embedding_model` | A truthy value forces category `EMB`. It conflicts with another explicit category. |
| `context`, `batch`, `flash_attn`, and other flag keys | The harness maps these keys to the corresponding `llama-server` flags. `context` is **llama only**: on other engines it stays an inert key (the scan says so), and the page and API never display it there. The Ctx figure shown for a strata row is the `--max-context` figure in its `strata_args` (the `--max-context` token or the `--max-context=` spelling, digits only). For a ninfer row it is the max-context of the resolved profile. |
| `load_timeout` | Per-model readiness deadline in seconds. The llama default is 120; strata and ninfer cold starts commonly need 600 to 1200. |
| `strata_exe` | **Required for strata** (template mode): the absolute path of the engine binary that the harness-rendered profile names. Its presence selects template mode. `serve_model` is also required, and it becomes the profile's served model name. |
| `strata_tokenizer` | **Required for strata**: the absolute tokenizer path that the rendered profile names. |
| `strata_python` | Interpreter. It must be an existing `.exe` **file**; it is the program the harness launches (default `C:\Strata\.venv\Scripts\python.exe`). |
| `strata_cwd` | Working directory. It must be an existing directory (default `C:\Strata`). |
| `strata_args` | Comma-separated engine argv tokens rendered into the profile's `args`. Put weights, quant, and context here, for example `--native D:\Models\m.gguf`. The harness forces `--host` and `--port` and refuses them in this list. A `strata_vision` block appends the engine's `--vision` switch, so do not write that token here too. |
| `strata_lib_dirs` | Comma-separated absolute directories rendered as the profile's `lib_dirs`. |
| `strata_env` | Whitespace-separated `STRATA_<NAME>=<VALUE>` tokens rendered as the profile's `env`. Only uppercase `STRATA_`-prefixed engine settings pass. |
| `strata_gpu` / `strata_parallel` | Non-negative integers rendered as the profile's `gpu` / `parallel`. |
| `strata_gpus_asked` | A truthy value renders `gpus_asked: true` in the profile. |
| `strata_vision` | Whitespace-separated `k=v` vision block over the closed key set `exe`, `mmproj`, `model`, `gpu`, `max_tokens`, `min_tokens`, `threads`. `exe`, `mmproj`, and `model` are absolute paths and are required together. `gpu` defaults on. Rendering the block also appends the `--vision` switch to `args`. |
| `strata_sampling` | Whitespace-separated `key=value` sampling defaults (number or bool) rendered as the profile's `sampling` dict. |
| `strata_uncensored` | Strict `true`/`false` rendered as the profile's `uncensored` switch. An explicit `false` is written intent and is different from absence. |
| `strata_config` | **Superseded** (operator ruling 2026-11-02, row 19). A row that still declares it is refused at scan, with the migration sentence: run `simplehar-se import strata <profile.json>`, merge its emitted knob block into the row, and delete the `strata_config` line. A row that carries both `strata_exe` and `strata_config` meets the mutual-exclusion refusal; delete the `strata_config` line. |
| `extra_args` | **Closed allow-list, not a denial list.** The harness splits the value on whitespace and appends a word to the argv only when it is a reviewed option: `--embedding`, `--embeddings`, `--pooling <mode>`, `--temp`, `--top-k`, `--top-p`, `--min-p`, `--repeat-penalty`, `--seed`, `--warmup`, `--no-warmup`. The value words a reviewed option consumes are appended too. A value may not be option-shaped, so it cannot smuggle another flag. **Anything else is refused with the launch**, and the message names the word. The model, host, port, gpu-layers, jinja, api-key, lora, and rpc options and every other option are not on the list, so their absence refuses them and there is nothing to dodge. The harness appends its forced `--host 127.0.0.1`, `--port`, and `cpu_offload` `-ngl 0` after these args, and they win. |
| `ninfer_image` | Selects **template mode, the supported path**. Its presence means the harness renders the canonical autostart script from your yaml, stages it into the distro, verifies it by distro-side checksum, and launches exactly **that** script. Exactly one of `ninfer_image`/`ninfer_autostart` must be present. Its required companions are `ninfer_model_name` (the directory that holds the artifact), `ninfer_artifact` (the file inside it), and `ninfer_model_roots` (comma-separated absolute WSL directories, searched in order). Each must be a plain word, and the harness refuses a bad value and names the key. `ninfer_internal_port` (the port the container binds inside its own namespace) is optional and defaults to the SE internal number **8808**. That is the harness's 87xx family plus 100: clear of the manual 80xx world, the 87xx listeners, and the 181xx engine plan. It may not name a `protected_ports`, public-listener, or harness engine port. The manual/harness separation covers container-internal numbers too, so the harness refuses a row that borrows one at load, and the rescan names it. |
| `ninfer_serve_cmd` | The server command inside the container (default `ninfer-serve`). |
| `ninfer_gpu_device` | Comma-separated device selector for `--gpus device=` (default `0`). |
| `ninfer_serve_args` | Extra flags appended to the serve line, as shell-bare tokens. Both shipped engine parsers reject the `--flag=value` form, so write each option with a space and put the value in its own token, for example `--spec dflash2`. Option tokens must be long options that start with `--`. The canonical script reserves these options: `--host`, `--port`, `--api-key`, `--data-dir`, `--data-dir-permissions`, `--model`, `--model-id`, `--log-level`, `--log-dir`, `--uds`, `--ssl-keyfile`, `--ssl-certfile`, `--max-context`, `--kv-capacity`, `--max-concurrency`, `--kv-dtype`. The harness refuses each of them by name and by any abbreviation or extension of it, so `--max-cont` and `--host-context-mib` are refused too. Your args can therefore never resize what the cost estimate measured. |
| `ninfer_model_id` | Optional OpenAI-style served id, passed as `--model-id`. |
| `ninfer_env` | Extra container environment as `NAME=VALUE` tokens. This is an **allow-list**, not a parse rule. The only admissible pair today is `CUDA_MODULE_LOADING=EAGER\|LAZY`, the cold-start flag of the NInfer-all cards. The harness refuses an arbitrary name, such as `LD_PRELOAD` or `PATH`, by name. Growing the list needs a code review, never a yaml edit. |
| `ninfer_key_file` | Template mode only: the place where `ninfer_key: true` reads the API key. The path is exactly `$HOME/.simplehar/keys/<file>.key` in the distro, a bare word with no traversal and no subdirectories. The script `cat`s it into `--api-key`, so an arbitrary path would let the yaml export any distro secret. Default: `$HOME/.simplehar/keys/<ninfer_container>.key`. A legacy script reads its own `$(dirname "$0")/.api_key` instead, and this key is not read there. |
| `ninfer_autostart` | **Legacy path.** The path (inside WSL) of your own autostart script. The harness invokes it as `bash <script> <profile> <key\|nokey> <port>` via `wsl.exe`. It must be a plain absolute path: no spaces, no shell metacharacters, and no leading `~`. It is mutually exclusive with `ninfer_image`. Use the template instead: the harness cannot verify what a hand-written script does to your card or your network. |
| `ninfer_container` | **Required for ninfer.** The container name. The harness reaps it with `docker rm -f` on unload, scoped strictly to the name this harness started (claim ledger: `ninfer-claims.json` beside the harness exe). The name must be docker-legal and must not look like a container ID (4+ hex chars). |
| `ninfer_profile` | Template mode: one of the built-in VRAM profiles `a` to `e` (default `b`). The table is the operator's own: `a` 262k ctx/524k kv/2 conc, `b` 262k/262k/1, `c` 131k/131k/1, `d` 16k/32k/2 nvfp4, `e` 64k/64k/1. Or use a custom lowercase name, resolved by a `ninfer_profile_<name> = ctx=... kv=... conc=... kv_dtype=...` line. `kv_dtype` is one of `fp8`/`nvfp4`/`fp16`/`bf16`/`int8`/`rk8v4`, and `kv` must cover `ctx`. The override line applies to custom names only. The built-in table wins for `a` to `e`, so the harness refuses a `ninfer_profile_b`-style line at launch rather than silently ignoring it. Legacy mode: the value passes to your script verbatim as `$1`. |
| `ninfer_key` | A truthy value (`true`/`1`/`yes`/`on`) sends `key` as `$2`; in template mode the script `cat`s the `ninfer_key_file` into `--api-key`. Anything else sends `nokey`. |

The harness renders strata rows; it never hands the row to the engine. At
every load the harness writes the full profile JSON into
`.simplehar\strata-profiles` beside the exe (content-addressed
`<model>-<hash>.json`, atomic write, keep-N prune) and launches only its own
staged copy. See "Strata engines" below.

The harness keeps unknown keys for reference and reports them as
`unknown_key` warnings. The model still loads. `host` and `port` keys belong
to the harness and are inert: the bind is forced to loopback, and the port
comes from the category config. `fit_gb` is the exception: it is the engine
calculator's projection slot (see below). The harness ignores and strips a
hand-written value and never books it.

**Cost sources** decide which number binds admission, and the strongest
source wins. The sources are an `engine_report` (the ninfer container's own
startup reservation) and, for llama, a **Fit** projection. When
`engines.llama.fit_probe` is on, the harness runs the engine's own
`llama-fit-params` calculator before spawn and books that figure when it
exceeds the declared `vram_gb`. A Fit figure is **never persisted**, so the
stale-measurement failure class cannot exist for it. The harness re-verifies
it against the calculator's exact version on every launch. On the page a Fit
row reads amber "engine-projected" until a witnessed serve confirms it
within the margin, then it turns green. The colour is a confidence signal;
the number never changes between the two.

### Strata engines

Strata models (`engine: strata`) run in **template mode, the supported
path**. The row yaml carries the profile fields (the `strata_*` keys above),
and the harness renders the full profile JSON at every load. It launches
`python -m serve.server --engine strata --host 127.0.0.1 --config <rendered> --port <assigned>`
in `strata_cwd`. The render is staged under `.simplehar\strata-profiles`
beside the exe (content-addressed `<model>-<hash>.json`, atomic write,
keep-N prune). The spawn rewrites `--config` to exactly that file, so the
child can never point at another profile. The bind is the forced
`--host 127.0.0.1` argv, and the port is the harness-assigned `--port`;
server.py resolves both flags before any profile key. The rendered profile
therefore carries no `host`, `port`, or key at all. Readiness needs `/health`
200 **and** the JSON `loaded` flag not being `false`, because strata answers
200 while weights stream in. A strata engine starts only when you load it,
or when a client request names it. Booting the harness starts nothing.

The external-profile mode is superseded (operator ruling 2026-11-02, row
19): a row that declares `strata_config` is refused at scan. The command
`simplehar-se import strata <profile.json>` reads a manual profile and
**emits** the yaml knob block. It never touches the row or the profile file,
so adoption is your own paste of the emitted block. `C:\Strata` stays the
manual-launch home that the harness never reads on the serve path. The
import CLI is the one sanctioned reader.

### Ninfer engines

Ninfer models (`engine: ninfer`) are GPU containers in WSL2. The harness
launches them as a short-lived `wsl.exe` client. The container outlives the
client, so unload reaps it by name (`docker rm -f <ninfer_container>`), and
the release evidence is the port closing, not the client exit. Under
**template mode** (`ninfer_image` present) the client runs the harness's own
canonical script. The harness renders it from your yaml, stages it base64
through a stdin driver to
`~/.simplehar/autostart/<container-slug>-<content-hash>.sh`, verifies it by
checksum on the distro before and after the atomic publish, and prunes to
the newest renders of that container. The rendered launch carries the
incident-review guarantees: `--restart no` (nothing revives on WSL boot
without the harness deciding), a loopback-only publish, and bounded
container logs (`json-file`, 25 MiB x 5). It also carries your profile,
key, and serve args. The engine logs a JSON memory summary at startup into
the container log. The harness reads it from the container's own logs and
books it as the `engine_report` measured figure. A container that never
logs one is measured from free memory instead and labelled `total_diff`:
shown, never booked. Under **legacy mode** (`ninfer_autostart`) the client
runs your script instead, as
`bash <ninfer_autostart> <profile> <key|nokey> <assigned-port>`. Both modes
then wait for `/health` 200 and then for `/v1/models` 200. The harness
manages every ninfer model the catalog carries. It never registers or
monitors servers you started manually; it only subtracts them from VRAM
through the witness.

If a harness crash orphans a container, the next load of that name
reconciles the state: the harness removes the stale container before
autostart runs. At boot, the harness reclaims an orphan only when it can
prove ownership: the name is declared by a `ninfer` model, the name is
legal, and the claim ledger proves this harness started it. Otherwise the
harness warns and leaves the container alone. A running container that
matches a catalog `ninfer_container` but holds no claim is therefore never
killed, and it is never silent either. Boot and every rescan name it in the
log, in `/har/status.stranded_containers`, and in an amber page badge. Its
VRAM stays occupied, and the harness never reclaims it (report only). The
harness refuses duplicate `ninfer_container` names across models at scan,
because reaping is by name.

#### autostart.sh template

The authoritative template is compiled into the harness
(`CANONICAL_NINFER_SCRIPT`), and template mode renders and stages that
template. You never edit it. A readable copy ships as
[ninfer-autostart-template.sh](ninfer-autostart-template.sh) next to this
README (and beside the exe in the release package). Read that one file to
see exactly what a launch does, and start from it when you must write a
**legacy** `ninfer_autostart` script. The legacy contract: the harness
invokes your script inside WSL as
`bash <script> <profile> <key|nokey> <port>`. `$1` selects a resource profile.
`$2` toggles the service's API key. `$3` is the **host** port the harness assigned; map it to the
container's internal port. The script exits once the container is starting,
and the harness then polls `/health` and `/v1/models` itself. Keep the
container name equal to the model's `ninfer_container`.

Notes:

- `--publish 127.0.0.1:...` keeps the container loopback-only. Publishing
  on all interfaces exposes an unauthenticated engine to your LAN. Do that
  only with the `key` profile (and a real `.api_key` file), or not at all.
- `--restart no` is not a preference. A `--restart` policy revives
  containers on WSL boot outside any harness decision, with no ledger entry
  and no reap path (the NINFER-8004 incident). Template mode hard-codes it;
  keep it in a legacy script.
- R10 namespaces: the container's internal port is a **third namespace**,
  disjoint from the manual (`protected_ports`) and harness (listeners,
  engine plan) sets. Keep your legacy script's `INTERNAL_PORT` clear of all
  three; 8808 is the SE internal number. The harness validates the `$3`
  offer it makes, but it never rewrites your script. The harness refuses a
  legacy row that also declares `ninfer_internal_port` at load, for a
  borrowed number, even though the script would ignore the key. So leave
  that key unset on legacy rows.
- The name-scoped `docker rm -f` before `docker run` makes the launch
  idempotent, and it complements the harness's orphan reconciliation.
- The `--log-driver json-file --log-opt` pair bounds container logs.
  Template mode always sets them, so a week of a serving engine never fills
  the distro.
- `serve_model` in the model yaml must match `--model-id`, or the harness
  records a served-id warning.
- Run `chmod +x` on the script. The harness runs it with `bash`, so the
  shebang is documentation, but the executable bit avoids surprises when
  you test by hand.

## Rescan

After you edit files, press **Rescan** or send `POST /har/rescan`. A rescan
re-reads the catalog only. A model that is serving keeps the launch it was
spawned with, so an edit to a live model's yaml takes effect on its next
load. The page marks the row **amber** while the file differs from the
running bytes; unload and reload to apply. A queued load whose yaml moved
is retired with a `superseded by rescan` note; re-queue it. Two files whose
names collapse to one identity are both reported as `duplicate_name`. Loads
of that name stay refused as ambiguous until you rename one file.

## Control page

Open `http://127.0.0.1:8708/`. The page shows slot cards and memory bars:
observed VRAM from the card's own reading, the reserve marked as a hatched
band, in-flight bookings noted, and host RAM. It also shows category tabs,
per-model rows (Load / Set default, state dot and name, category select,
CPU checkbox, Engine, Ctx, Cost, Files), recent jobs, and a collapsible
harness log. A loaded row shows `launched_at` and turns amber while the
model's file differs from the bytes its launch used. The page receives
updates over SSE and degrades to polling. The theme is adaptive
(light/dark) with a toggle.

## Tray

The tray has four states: green means loaded, yellow means a swap is in
flight, gray means idle, and **red means a listener is down**. The menu
offers Open Web UI, Unload All or per-category unload, and Exit.

## HTTP control API (control listener, 8708)

| Endpoint | Body / meaning |
|---|---|
| `GET /health` | Liveness check. Once `control_token` is set, it requires the token like every other route. |
| `GET /har/status` | Full snapshot, including `mem`, `jobs`, `last_error`, and boot timings. |
| `GET /har/events` | SSE state pushes. |
| `GET /har/jobs`, `GET /har/jobs/{id}` | Job ledger. |
| `GET /har/log?limit=n` | Ring buffer of harness events. |
| `GET /har/models` | Full catalog (the page view; includes paths). The `context` field is the **effective served context**, never a stale row key. For llama it is the row `context` knob. For strata it is the context figure in the row's `strata_args` argv (the `--max-context` token or the `--max-context=` spelling, digits only; anything else answers `null`). For ninfer it is the max-context of the profile its launcher resolves (the built-in table or the `ninfer_profile_<name>` line, the identical resolution the spawn uses). Every other engine answers `null`, and so does any row whose effective context does not resolve. |
| `POST /har/load` | `{model}`. Loads the model into its own category, from its yaml. |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all). Never both keys at once (`400`). The reply status is honest: `unloading` (counts `unloading`/`jobs_cancelled`; 200 is an acceptance, and the release lands on the job ledger), `cancelled` (only load jobs were cancelled, and nothing installed was freed; cancelling an in-flight unload is not this case, because its re-armed release is reported under `unloading`), or `nothing_unloaded`. Unloading a *loading* slot cancels that swap, and cleanup still completes. |
| `POST /har/rescan` | `{}` catalog rescan. |
| `POST /har/config/default` | `{category, model \| null}`. `model` is required. A string sets the default, stored as its canonical name, and `null` clears it. The change persists to the config file. |
| `POST /har/config/logging` | `{enabled: bool}`. Toggles the verbose harness log. |
| `POST /har/config/model` | `{model, category?, cpu?}`. Writes back to the model yaml. |

### Memory provenance fields

The status and catalog documents show how the harness obtained a memory figure.
Do not treat a displayed figure as a booked figure unless its provenance allows
booking.

`/har/status` carries these memory fields:

- `mem.attribution`: `per_process` or `unavailable`. It is `null` when the
  witness returned no sample.
- `mem.container_reported_gb`: the sum of the loaded containers' own reserved
  figures. It is `null` when no container reported a figure.
- `slots[].container_reported_gb`: the occupant container's own reserved
  figure. It is present only for a ninfer container whose `engine_report`
  anchored the current launch.

`/har/models` and the SSE `har_state` document carry these per-model fields:

- `cost_basis`: `declared`, `measured`, `fit`, `estimate`, or `undeclared`.
- `measured_attribution`: `engine_report`, `per_process`, `total_diff`, or
  `null`.
- `required_to_free_gb`: the figure the model needs free. It is present only
  when `cost_basis` is `measured`.

A guarded `engine_report` above the declared `vram_gb` books instead of the
declaration (max truth). A declaration at or above the report stands.

Every error carries both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope, with the same values in both. The
`code` is one of: `not_found` (404), `bad_request` (400), `unauthorized`
(401, control token), `forbidden` (403, cross-site `Origin` on a POST),
`conflict` (409, cancelled / superseded / port still held / an edit the
scanner would refuse to publish), `unsupported_media_type` (415),
`unavailable` (503, admission refused with the numbers and blockers),
`below_launch_floor` (503, refused before spawn), `witness_unavailable`
(503, card unreadable), `upstream` (502), `not_implemented` (501),
`internal` (500). A refusal that carries numbers puts them in `message` and
in the job ledger, never in a bare status.

## Autostart

Create `autostart.vbs` at the path the `Run` key below names (adapt the
paths). It runs the harness hidden at logon, once:

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

- **A port will not bind.** The status line and the log name the port. If a
  Hyper-V/WSL2 dynamic reservation covers the port, the harness already
  reports `port N lies in the reserved range`. The command
  `netsh interface ipv4 show excludedportrange protocol=tcp` shows all such
  ranges. Reserve your port permanently with
  `netsh int ipv4 add excludedportrange protocol=tcp startport=N numberofports=1`.
- **A model file is missing at boot.** The scan drops a `llama` row whose
  GGUF is gone and reports `! <file>.yaml: model file not found: <path>`. A
  row that publishes but cannot load leads its log line with `!`. Either
  way you see it before the first load tries it.
- **Refusals.** Every refusal names the blockers and the numbers, in the
  `/har/status` jobs and in `last_error`. Nothing over-admits, so a refusal
  means you must unload something. The witness, the card's own reading, is
  the budget truth.
- **`launch floor not met`** (code `below_launch_floor`). The card holds
  too little physically free VRAM for that model to launch sanely, foreign
  processes included. The harness refused the load **before spawn**, so
  nothing thrashed. Free VRAM, or lower the model's
  `launch_only_with_at_least_vram_free`. The printed `free` is the number
  the gate compared: reserve deducted, same-slot replacement credited.
- **A second instance.** A single-instance guard makes a duplicate exit
  immediately.
- Logs live in `logs/` next to the exe. The harness keeps **its own**
  streaming log, `logs/harness-<boot-epoch>.log`. Each event is one JSON
  line, written unbuffered, so a killed process keeps what it logged. The
  file is never quieter than `INFO`, whatever `SIMPLEHAR_LOG` asks for, and
  it rolls to a new generation at 8 MiB. The last 10 harness log files
  survive, rolled generations included. Engine children keep
  `logs/<model>-<epoch>.log`, pruned to 50 on a separate budget the harness
  log can neither consume nor add to. `GET /har/log` reads the in-memory
  ring. A non-zero `log_write_failures` at `/har/status` means the file
  sink died, and this boot leaves an incomplete artifact.

## Security note

Set `control_token`, or set `SIMPLEHAR_TOKEN`, which wins so the secret can
stay out of the file. **Every route then requires the token**, on
`X-Har-Token` or `Authorization: Bearer`: `/har/*`, each listener's
`/v1/*`, and `GET /health` too. A liveness probe must carry the token, or
it reads 401 and looks like a dead listener. Only the page and the assets
it links answer unauthenticated, so the page can load and ask. The
structural guards run **before** the token check: a cross-site `Origin` on
a POST gets `403` whether or not a token is presented, and a missing
`application/json` content-type gets `415` even with a valid token. The
token decides *who*, never *whether* the request shape is acceptable. The
harness never logs the token and never accepts it from a URL.

With no token set the harness has **no authentication**. The value
`internal_host: 0.0.0.0` (Publish to LAN) then trusts every machine on the
segment to load and unload models and to drive the GPUs. It exists for LAN
boxes and warns loudly at boot. The default is loopback only. The token guards only
the harness's own listeners. Engine ports have no harness auth, so the
harness forces llama and strata engines to loopback regardless of
`internal_host`, a template-mode ninfer publishes loopback-only, and only a
legacy ninfer script that itself chooses to publish can put an
unauthenticated engine on the segment.
