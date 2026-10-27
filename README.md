# SimpleHar SE

A Windows tray harness for local LLM inference engines: one app, one GPU, many
models. It owns llama.cpp `llama-server` processes (more engines planned), swaps
models between four category listeners with a typed, single-writer scheduler,
admits loads against a real memory ledger, and refuses — with reasons — rather
than overcommitting.

Current release: **1.0** — `cargo build --release` produces
`target\release\simplehar-se.exe`.

## Requirements

- Windows 10/11 with an NVIDIA GPU (NVML witness; `nvidia-smi` fallback).
- Rust toolchain (`cargo build --release`) for building.
- `llama-server.exe` from [llama.cpp](https://github.com/ggml-org/llama.cpp)
  (default expected at `C:\Inference\llama.cpp\llama-server.exe`, configurable).

## Quick start

```powershell
cargo build --release
# place simplehar-se.exe anywhere; create a ModelConf\ folder next to it
# and drop one flat .yaml per model (see "Model files" below)
target\release\simplehar-se.exe
# open the control page
start http://127.0.0.1:8708/
```

No config file is required — defaults are usable. Boot spawns **zero engines**:
models load on demand (first request, or from the page).

## Categories & ports

| Category | Public port | Slots | Purpose | Evicted by |
|---|---|---|---|---|
| WORK | 8708 | 1 | coding/agent work; also hosts the control page + `/har/*` | nobody |
| RP | 8709 | 1 | roleplay | WORK |
| AUX | 8710 | 1–6 | utility models | WORK, RP |
| EMB | 8711 | 1 | embeddings | WORK, RP |

llama engines are **forced** to `--host 127.0.0.1` by the harness; strata and ninfer
take their bind address from their own external config (strata profile `host`,
ninfer autostart script) — keep those loopback unless you mean to publish. One
port per slot allocated sequentially from
18109 (`18109`–`18112` with the default four single-slot categories, up to `18117`
with six AUX slots). All ports are declarative — see `categories` in
the config.

Each public listener answers OpenAI-style `/v1/models` (category-filtered),
`/v1/chat/completions`, `/v1/completions` and `/v1/embeddings`. A request naming no
model (or the engine's default id) loads that category's `default_model` instead of
erroring.

## Configuration: `simplehar-se.json`

Lives beside the executable; override the path with the `SIMPLEHAR_CONFIG`
environment variable. It is SE's own file: the older app's `simplehar.json` is read
once, to migrate, and never written. **The config file is the truth**:
`default_model` changes
made on the page persist back to the exact file boot read (atomic rewrite),
partial files are completed, an invalid file is fatal with every problem logged
— never silently defaulted.

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN — set `control_token` first, see Security |
| `control_token` | *unset* | When set, every route but the page assets demands it (`X-Har-Token` or `Authorization: Bearer`); `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models`, `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), `engine_ports` |
| `vram_total_gb` | `72.0` | Admission ceiling |
| `vram_reserve_gb` | `2.0` | Kept unreachable by admission |
| `witness_tolerance_gb` | `2.0` | NVML/ledger disagreement band; also the settle allowance |
| `greedy_launch_min_free_gb` | `4.0` | Launch floor applied to `cost: greedy` models without their own `launch_only_with_at_least_vram_free` |
| `load_timeout_s` | `120` | Readiness deadline per load; per-model override via the `load_timeout:` yaml key |
| `measured_sample_s` | `60` | Seconds between witness samples while a model serves; builds the `measured_gb`/`measured_gb_min` range |
| `release_timeout_s` | `60` | Kill-confirmation deadline (G12) |
| `settle_timeout_s` | `30` | Memory-settle deadline after release |
| `protected_ports` | `8003-8005`, `8009`, `8016-8017`, `8030`, `18203-18209` | Ports the harness must never bind or touch |
| `wsl_distro` / `wsl_user` | `Ubuntu` / *(set this)* | WSL coordinates for ninfer containers |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary |
| `modelconf_dir` | `<exe dir>\ModelConf` | Catalog directory |
| `tray_enabled` | `true` | Tray icon at boot |

## Model files: `ModelConf/*.yaml` (or `.yml`)

One flat `key: value` file per model (comments via `#` preserved on write-back):

| Key | Notes |
|---|---|
| `name` | Canonical harness name; also matched by file-stem/path aliases |
| `model` | GGUF path (or `scheme://` engine sentinel). **Required for llama** |
| `engine` | `llama` (default), `strata`, `ninfer`, or `mock` |
| `category` | `WORK`/`RP`/`AUX`/`EMB` |
| `vram_gb` | Declared admission cost in GiB; `fixed` falls back to `file size × 1.15 + 2`, `exact` refuses without it |
| `cost` | `fixed` (default): size binds admission · `greedy`: offloads to RAM/SSD, size never refuses but a launch floor is mandatory · `exact`: engine-computed number, required and binding |
| `launch_only_with_at_least_vram_free` | GiB. Refuse the load **before spawn** unless the physically free card reaches this floor (reserve deducted; foreign processes counted whenever per-process attribution is available) — the thrash guard for big/greedy quants. Greedy models without it inherit `greedy_launch_min_free_gb`; set `0` per-model to opt out explicitly |
| `cpu_offload` | `true`/`1`/`on` → forces `-ngl 0` |
| `serve_model` | Expected engine-reported id; mismatch = recorded warning, never trusted |
| `measured_gb` | High end of the footprint the harness measured from the witness while this model served (`total - used` diff against the free memory its load was admitted at, sampled every `measured_sample_s`). Written back after a load; `fixed` admission books it when `vram_gb` is absent, and a large divergence from `vram_gb` logs a verification warning |
| `measured_gb_min` | Low end of that range: the smallest total-diff seen while serving. The spread is the KV cache growing during a session, which is why one number would be false precision. Must not exceed `measured_gb` |
| `measured_offload` | The `cpu_offload` setting the pair was measured under, written with it. An offloaded launch has no comparable footprint, so flipping `cpu_offload` voids both values at scan while leaving the stamp visible on the page |
| `is_embedding_model` | Truthy forces category `EMB` (conflicts with another explicit category) |
| `context`, `batch`, `flash_attn`, … | Mapped to the corresponding `llama-server` flags |
| `load_timeout` | Per-model readiness deadline in seconds (llama default 120; strata/ninfer cold starts use 600–1200) |
| `strata_config` | **Required for strata.** Profile JSON path; existence-checked before spawn. Weights/quant/context all live in this file, never in argv |
| `strata_python` | Interpreter (default `C:\Strata\.venv\Scripts\python.exe`); existence-checked before spawn |
| `strata_cwd` | Working directory (default `C:\Strata`) |
| `extra_args` | Whitespace-split words appended to the argv; words naming a harness-owned flag (`-m`, `--model`, `--host`, `-h`, `--port`, `-p`, `-ngl`, `--n-gpu-layers`, `--jinja`, `--mmproj`, `=` forms included) are refused with the launch. The forced `--host 127.0.0.1`, `--port`, and `cpu_offload` `-ngl 0` are appended after it, so they win under any parser that honors last occurrence. All other unknown keys are preserved in the catalog for reference but never launched |
| `ninfer_autostart` | **Required for ninfer.** Path (inside WSL) of the operator's autostart script; invoked as `bash <script> <profile> <key\|nokey> <port>` via `wsl.exe` |
| `ninfer_container` | **Required for ninfer.** Container name; reaped with `docker rm -f` on unload — strictly scoped to the name this harness claimed at spawn (`ninfer-claims.json` beside the harness exe). Must be docker-legal and must not look like a container ID (4+ hex chars): docker would resolve such an argument as an id prefix and delete someone else's container |
| `ninfer_profile` | Profile passed as the script's `$1` (default `b`) |
| `ninfer_key` | `true` sends `key` as `$2`; anything else sends `nokey` |

### Ninfer engines

Ninfer models (`engine: ninfer`) are GPU containers in WSL: the harness spawns a
short-lived `wsl.exe -d <wsl_distro> -u <wsl_user> -- bash <ninfer_autostart>
<profile> <key|nokey> <assigned-port>` client, then waits for `/health` 200
**then** `/v1/models` 200. Because the container outlives the client, unload
reaps it by name (`docker rm -f <ninfer_container>`); the release evidence is
the port closing after the reaping, not the client's exit. Every ninfer model the
catalog carries is managed; manual
servers the harness did not spawn are never registered or monitored, only
subtracted from VRAM via the witness. A harness crash can orphan a container (the job object only reaps
the client); the next load of that name reconciles by removing the stale
container before autostart runs. At boot an orphan is reclaimed only when all
three ownership gates hold — a `ninfer` model declares the name, the name is
legal, and the claim ledger proves this harness started it — otherwise the
harness warns and leaves the container alone: boot reclaim never destroys on
suspicion, never by prefix, never by container-ID prefix. Duplicate
`ninfer_container` names across models are refused at scan — reaping is by
name. As with strata: no preload, booting spawns nothing.

### Strata engines

Strata models (`engine: strata`) launch `python -m serve.server --engine strata
--config <strata_config> --port <assigned>` in `strata_cwd`; the harness-assigned
engine port always wins over anything in the profile. Readiness requires `/health`
200 **and** the JSON `loaded` flag not being `false` (strata answers 200 while
weights stream in). There is no preload anywhere in this harness: a strata engine
starts only when an operator loads it (or a client request names it through the
proxy); booting the harness spawns nothing.

After editing files, press **Rescan** (or `POST /har/rescan`).

## Control page

`http://127.0.0.1:8708/` — slot cards + memory bars (VRAM ledger with the reserve
marked as a hatched band, observed, host RAM), category tabs, per-model rows
(Load / Set default, state dot + name, category select, CPU checkbox, Engine,
Ctx, Cost, Files), recent jobs, collapsible harness log.
Updates push over SSE (`/har/events`) and degrade to polling; adaptive light/dark
theme with a toggle.

## Tray

Four states: green = loaded, yellow = swap in flight, gray = idle, **red =
listener down**. Left-click does nothing; the menu offers Open Web UI, Unload
All / per-category, Exit.

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
| `POST /har/config/logging` | `{enabled: bool}` — toggles the verbose harness log (answers `extended_logging`) |
| `POST /har/config/model` | `{model, category?, cpu?}` — writes back to the model yaml |

Errors carry both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope, with the same values in both. The `code` is
one of: `not_found` (404), `bad_request` (400), `unauthorized` (401, control token),
`forbidden` (403, cross-site `Origin` on a POST), `conflict` (409, cancelled /
superseded / port still held), `unsupported_media_type` (415), `unavailable` (503,
admission refused with the numbers and blockers), `below_launch_floor` (503, refused
before spawn), `witness_unavailable` (503, card unreadable), `upstream` (502),
`not_implemented` (501), `internal` (500). Refusals that carry numbers do so in
`message` and in the job ledger, never in a bare status.

## Autostart

`autostart.vbs` (adapt paths), registered per-user:

```vb
' runs the harness hidden at logon, once (WMI guards double-launch)
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
- **"model file MISSING" at boot** — the banner (`simplehar_se::boot` lines in
  the log panel) flags models whose GGUF is gone before the first load tries it.
- **Refusals** — every rejection names the blockers and the numbers
  (`/har/status` jobs + `last_error`); nothing over-admits, so a refusal means
  unload something or raise nothing — the ledger is the truth.
- **`launch floor not met`** (code `below_launch_floor`) — the card holds too
  little physically free VRAM for that model to launch sanely (foreign
  processes). The load was refused **before spawn**, so nothing thrashed; free
  VRAM or lower the model's `launch_only_with_at_least_vram_free` (the printed
  `free` is the number the gate compared — reserve deducted, same-slot
  replacement credited).
- **Second instance** — a single-instance guard exits duplicates immediately.
- Logs: `logs/` next to the exe (harness + captured engine logs, pruned).

## Security note

Set `control_token` (or `SIMPLEHAR_TOKEN`, which wins so the secret can stay out of
the file) and **every route requires it** on `X-Har-Token` or `Authorization: Bearer` —
`/har/*`, each listener's `/v1/*`, and `GET /health` too, so a liveness probe must carry
the token or it will read 401 and look like a dead listener. Only the page and the assets
it links — `/`, `/index.html`, `/app.js`, `/style.css` and `/pico.min.css` — answer
unauthenticated, so the page can load and ask. The structural guards run **before** the
token check: a cross-site `Origin` on a POST is `403` whether or not a token is presented,
and a missing `application/json` content-type is `415` even with a valid one — so the
token decides *who*, never *whether* the request shape is acceptable. The token is
never logged and never accepted from a URL. With no token set the harness has **no authentication**:
`internal_host: 0.0.0.0` (Publish to LAN) then trusts every machine on the segment to
load/unload models and drive the GPUs; it exists for LAN boxes and warns loudly at
boot. Default is loopback only. The token guards only the harness's own listeners —
engine ports have no harness auth, so a published strata profile (`host` non-loopback)
needs its own `api_key`, and llama is forced loopback regardless of `internal_host`.
