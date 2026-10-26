# SimpleHar SE

A Windows tray harness for local LLM inference engines: one app, one GPU, many
models. It owns llama.cpp `llama-server` processes (more engines planned), swaps
models between four category listeners with a typed, single-writer scheduler,
admits loads against a real memory ledger, and refuses — with reasons — rather
than overcommitting.

Rewrite of SimpleHar v3.3; the design rationale lives in `DESIGN.md`.

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

Engine processes bind loopback 18109–18119 (never exposed). All ports are
declarative — see `categories` in the config.

Each public listener answers OpenAI-style `/v1/models` (category-filtered),
`/v1/chat/completions`, `/v1/embeddings`. A request naming no model (or the
engine's default id) loads that category's `default_model` instead of erroring.

## Configuration: `simplehar.json`

Lives beside the executable; override the path with the `SIMPLEHAR_CONFIG`
environment variable. **The config file is the truth**: `default_model` changes
made on the page persist back to the exact file boot read (atomic rewrite),
partial files are completed, an invalid file is fatal with every problem logged
— never silently defaulted.

| Field | Default | Meaning |
|---|---|---|
| `internal_host` | `127.0.0.1` | Bind address. `0.0.0.0` publishes to the LAN — **no auth**, see Security |
| `categories` | four defaults | Per-category `id`, `public_port`, `max_models`, `default_model`, `evictable_by`, `idle_timeout_s` (0 = never), `control` (exactly one), `engine_ports` |
| `vram_total_gb` | `72.0` | Admission ceiling |
| `vram_reserve_gb` | `2.0` | Kept unreachable by admission |
| `witness_tolerance_gb` | `2.0` | NVML/ledger disagreement band; also the settle allowance |
| `load_timeout_s` | `120` | Readiness deadline per load |
| `release_timeout_s` | `60` | Kill-confirmation deadline (G12) |
| `settle_timeout_s` | `30` | Memory-settle deadline after release |
| `protected_ports` | v3.3 ecosystem set | Ports the harness must never bind or touch |
| `llama_server_exe` | `C:\Inference\llama.cpp\llama-server.exe` | Engine binary |
| `modelconf_dir` | `<exe dir>\ModelConf` | Catalog directory |
| `tray_enabled` | `true` | Tray icon at boot |

## Model files: `ModelConf/*.yaml` (or `.yml`)

One flat `key: value` file per model (comments via `#` preserved on write-back):

| Key | Notes |
|---|---|
| `name` | Canonical harness name; also matched by file-stem/path aliases |
| `model` | GGUF path (or `scheme://` engine sentinel). **Required for llama** |
| `engine` | `llama` (default) or `mock` |
| `category` | `WORK`/`RP`/`AUX`/`EMB` |
| `vram_gb` | Declared admission cost; falls back to `file size × 1.15 + 2` |
| `cpu_offload` | `true`/`1`/`on` → forces `-ngl 0` |
| `serve_model` | Expected engine-reported id; mismatch = recorded warning, never trusted |
| `is_embedding_model` | Truthy forces category `EMB` (conflicts with another explicit category) |
| `sticky` | Legacy alias → `AUX` |
| `context`, `extra_args`, … | Unknown keys pass through verbatim into the llama argv |

After editing files, press **Rescan** (or `POST /har/rescan`).

## Control page

`http://127.0.0.1:8708/` — slot cards + memory bars (VRAM ledger/reserve/observed,
host RAM), category tabs, per-model rows (state dot, Ctx, Cost, Files, category
select, CPU checkbox, Load / Set default), recent jobs, collapsible harness log.
Updates push over SSE (`/har/events`) and degrade to polling; adaptive light/dark
theme with a toggle.

## Tray

Four states: green = loaded, yellow = swap in flight, gray = idle, **red =
listener down**. Left-click does nothing; the menu offers Open Web UI, Unload
All / per-category, Exit.

## HTTP control API (control listener, 8708)

| Endpoint | Body / meaning |
|---|---|
| `GET /health` | Liveness |
| `GET /har/status` | Full snapshot incl. `mem`, `jobs`, `last_error`, boot timings |
| `GET /har/events` | SSE state pushes |
| `GET /har/jobs`, `GET /har/jobs/{id}` | Job ledger |
| `GET /har/log?limit=n` | Ring buffer of harness events |
| `GET /har/models` | Full catalog (page view; includes paths) |
| `POST /har/load` | `{model}` — loads into the model's own category (from its yaml) |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all). Unloading a *loading* slot cancels that swap (cleanup still completes) |
| `POST /har/rescan` | `{}` catalog rescan |
| `POST /har/config/default` | `{category, model \| null}` — persists to the config file |
| `POST /har/config/model` | `{model, category?, cpu?}` — writes back to the model yaml |

Errors carry both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope.

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
- **Second instance** — a single-instance guard exits duplicates immediately.
- Logs: `logs/` next to the exe (harness + captured engine logs, pruned).

## Security note

The harness has **no authentication**. `internal_host: 0.0.0.0` (Publish to LAN)
trusts every machine on the segment to load/unload models and drive the GPUs;
it exists for LAN boxes and warns loudly at boot. Default is loopback only.
