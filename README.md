# SimpleHar SE

A Windows tray harness for local LLM inference engines: one app, one GPU, many
models. It owns your engine processes (llama.cpp `llama-server`, strata Python
servers, ninfer GPU containers), swaps models between four category listeners
with a typed, single-writer scheduler, admits loads on the card's own reading
(the *witness*) minus a reserve, with in-flight bookings — and **refuses, with
reasons**, rather than overcommitting.

Current release: **v1.0.0** — `cargo build --release` produces
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
| `strata` | `python -m serve.server --engine strata` in a Python venv | a strata install with its virtualenv (default `C:\Strata\.venv\Scripts\python.exe`, cwd `C:\Strata`) and a per-model profile JSON |
| `ninfer` | a GPU container inside WSL2, started by your autostart script and reaped by container name | WSL2 distro + Docker, the ninfer container image, and an operator-written `autostart.sh` per model; set `wsl_distro` / `wsl_user` in the config |

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

llama engines are **forced** to `--host 127.0.0.1` by the harness; strata and
ninfer take their bind address from their own external config (strata profile
`host`, ninfer autostart script) — keep those loopback unless you mean to
publish.

Each public listener answers OpenAI-style `/v1/models` (category-filtered),
`/v1/chat/completions`, `/v1/completions` and `/v1/embeddings`. A request
naming no model is served by the category's loaded occupant (the most-recently
used one at AUX); when nothing is loaded it loads that category's
`default_model` instead of erroring.

## Configuration: `simplehar-se.json`

Lives beside the executable; override the path with the `SIMPLEHAR_CONFIG`
environment variable. **The config file is the truth**: `default_model` changes
made on the page persist back to the exact file boot read (atomic rewrite),
partial files are completed, and an invalid file is fatal with every problem
logged — never silently defaulted.

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN — set `control_token` first, see Security |
| `control_token` | *unset* | When set, every route but the page assets demands it (`X-Har-Token` or `Authorization: Bearer`); `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models` (1–6 for AUX, exactly 1 otherwise), `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), `engine_ports` |
| `vram_total_gb` | `72.0` | Admission ceiling |
| `vram_reserve_gb` | `2.0` | Kept unreachable by admission |
| `witness_tolerance_gb` | `2.0` | Post-kill driver-lag band: a load may spawn up to this far under the gate (absorbs the lag between a confirmed release and the card's reading catching up); also the settle allowance |
| `greedy_launch_min_free_gb` | `4.0` | Launch floor for `cost: greedy` models without their own `launch_only_with_at_least_vram_free` |
| `load_timeout_s` | `120` | Readiness deadline per load; per-model override via the `load_timeout:` yaml key |
| `measured_sample_s` | `60` | Seconds between witness samples while a model serves |
| `release_timeout_s` | `60` | Kill-confirmation deadline |
| `settle_timeout_s` | `30` | Memory-settle deadline after release |
| `protected_ports` | small reserved set | Ports the harness must never bind or touch — extend it to cover any servers you run manually outside this app |
| `wsl_distro` / `wsl_user` | `Ubuntu` / *(set for ninfer)* | WSL coordinates for ninfer containers |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary |
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
| `launch_only_with_at_least_vram_free` | GiB. Refuse the load **before spawn** unless the physically free card reaches this floor (reserve deducted; foreign processes counted whenever per-process attribution is available) — the thrash guard for big/greedy quants. Greedy models without it inherit `greedy_launch_min_free_gb`; set `0` per-model to opt out |
| `cpu_offload` | `true`/`1`/`on` → forces `-ngl 0` (**llama only** — other engines report an `inert_key` hint and the page offers no CPU control for them) |
| `serve_model` | Expected engine-reported id; mismatch = recorded warning, never trusted |
| `measured_gb` / `measured_gb_min` | The footprint range the harness measured from the witness while this model served (high/low end — the spread is the KV cache growing). Written back after a load; the admission books `measured_gb` when `vram_gb` is absent |
| `is_embedding_model` | Truthy forces category `EMB` (conflicts with another explicit category) |
| `context`, `batch`, `flash_attn`, … | Mapped to the corresponding `llama-server` flags |
| `load_timeout` | Per-model readiness deadline in seconds (llama default 120; strata/ninfer cold starts commonly need 600–1200) |
| `strata_config` | **Required for strata.** Profile JSON path; must be an existing **file** (refused up front, before any eviction or spawn). Weights/quant/context live in this file, never in argv |
| `strata_python` | Interpreter: must be an existing `.exe` **file** — it is the program the harness launches (default `C:\Strata\.venv\Scripts\python.exe`) |
| `strata_cwd` | Working directory; must be an existing directory (default `C:\Strata`) |
| `extra_args` | **Closed allow-list, not a denial list.** Whitespace-split words are appended to the argv only if each is a reviewed option — `--embedding`, `--embeddings`, `--pooling <mode>`, `--temp`, `--top-k`, `--top-p`, `--min-p`, `--repeat-penalty`, `--seed`, `--warmup`, `--no-warmup` — plus the value words it consumes (a value may not be option-shaped, so it cannot smuggle another flag). **Anything else is refused with the launch**, naming the word: model, host, port, gpu-layers, jinja, api-key, lora, rpc and every other option are admitted by absence, so there is nothing to dodge. The harness's forced `--host 127.0.0.1`, `--port`, and `cpu_offload` `-ngl 0` are appended after it and win |
| `ninfer_autostart` | **Required for ninfer.** Path (inside WSL) of your autostart script; invoked as `bash <script> <profile> <key\|nokey> <port>` via `wsl.exe`. Must be a plain absolute path — no spaces, no shell metacharacters, no leading `~` |
| `ninfer_container` | **Required for ninfer.** Container name; reaped with `docker rm -f` on unload, scoped strictly to the name this harness started (claim ledger: `ninfer-claims.json` beside the harness exe). Must be docker-legal and must not look like a container ID (4+ hex chars) |
| `ninfer_profile` | Profile passed as the script's `$1` (default `b`) |
| `ninfer_key` | Truthy (`true`/`1`/`yes`/`on`) sends `key` as `$2`; anything else sends `nokey` |

Unknown keys are preserved for reference and reported as `unknown_key`
warnings — the model still loads. `host` and `port` keys are harness-owned and
inert: the bind is forced to loopback and the port is assigned from the
category config.

### Strata engines

Strata models (`engine: strata`) launch
`python -m serve.server --engine strata --config <strata_config> --port <assigned>`
in `strata_cwd`; the harness passes the assigned `--port` last, after the
profile — pin the port outside the strata profile rather than relying on the
engine to arbitrate. Readiness requires `/health` 200 **and** the JSON `loaded`
flag not being `false` (strata answers 200 while weights stream in). A strata
engine starts only when you load it (or a client request names it); booting the
harness spawns nothing.

### Ninfer engines

Ninfer models (`engine: ninfer`) are GPU containers in WSL2: the harness runs
`wsl.exe -d <wsl_distro> -u <wsl_user> -- bash <ninfer_autostart> <profile>
<key|nokey> <assigned-port>` as a short-lived client, then waits for `/health`
200 **then** `/v1/models` 200. The container outlives the client, so unload
reaps it by name (`docker rm -f <ninfer_container>`) and the release evidence is
the port closing, not the client's exit. Every ninfer model the catalog carries
is managed; servers you started manually are never registered or monitored —
they are only subtracted from VRAM via the witness.

If a harness crash orphans a container, the next load of that name reconciles
by removing the stale container before autostart runs. At boot, an orphan is
reclaimed only when ownership is provable (the name is declared by a `ninfer`
model, the name is legal, and the claim ledger proves this harness started it) —
otherwise the harness warns and leaves it alone. Duplicate `ninfer_container`
names across models are refused at scan, because reaping is by name.

#### autostart.sh template

A ready-to-edit copy ships as **`ninfer-autostart-template.sh`** next to this
README (and beside the exe in the release package); copy it into your distro,
rename, `chmod +x`, and fill in the four variables at the top. The contract: the
harness invokes your script as `bash <script> <profile> <key|nokey> <port>`
inside WSL — `$1` selects a resource profile, `$2` toggles the service's API
key, `$3` is the **host** port the harness assigned (map it to the container's
internal port), and the script exits once the container is starting; the
harness then polls `/health` and `/v1/models` itself. Keep the container name
equal to the model's `ninfer_container`.

```bash
#!/usr/bin/env bash
# autostart.sh — start one NInfer service with a VRAM profile: a|b|c|d (default b)
set -euo pipefail

NAME="SE-nin-mymodel"                 # == ninfer_container in the model yaml
IMAGE="ninfer-mymodel:local"          # the image your artifact requires
MODEL_NAME="My-Model-NInfer"          # directory name holding the artifact
ARTIFACT="my-model.ninfer"            # file name inside the model directory
MODEL_ID="my-model"                   # OpenAI-style served id (== serve_model in yaml)
INTERNAL_PORT=8005                    # port the server listens on inside the container

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
  *) echo "usage: $0 <a|b|c|d> [key]" >&2; exit 1 ;;
esac

if [[ "$WITH_KEY" == "key" ]]; then
  API_KEY="$(cat "$(dirname "$0")/.api_key")"  # keep the secret in a file, never here
  API_KEY_ARGS=(--api-key "$API_KEY")
else
  API_KEY_ARGS=()
fi

PUBLISH="${3:-$INTERNAL_PORT}"        # host port assigned by the harness

# Idempotent: reclaim a stale container of OUR name before starting.
docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" \
  --gpus '"device=0"' \
  --restart unless-stopped \
  --publish "127.0.0.1:${PUBLISH}:${INTERNAL_PORT}" \
  --volume "$MODEL_DIR:/models:ro" \
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
- The name-scoped `docker rm -f` before `docker run` makes the launch idempotent
  and complements the harness's orphan reconciliation.
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
| `GET /har/models` | Full catalog (page view; includes paths) |
| `POST /har/load` | `{model}` — loads into the model's own category (from its yaml) |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all). Unloading a *loading* slot cancels that swap (cleanup still completes) |
| `POST /har/rescan` | `{}` catalog rescan |
| `POST /har/config/default` | `{category, model \| null}` — persists to the config file |
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
- Logs: `logs/` next to the exe (harness + captured engine logs, pruned).

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
listeners — engine ports have no harness auth, so a published strata profile
(`host` non-loopback) needs its own `api_key`, and llama is forced loopback
regardless of `internal_host`.
