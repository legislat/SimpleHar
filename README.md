---
title: SimpleHar SE user guide
owner: operator
status: active
audience: [user, contributor]
source_of_truth_for: [product overview, quick start, safety, navigation]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release
---

# SimpleHar SE

Windows tray harness for local LLM inference engines: one app, one GPU, many models.

Current release: **v1.1.0**. Windows 10/11, one NVIDIA GPU, single user.

<!-- Screenshot: add docs/assets/control-page.png after scrubbing model names, paths, tokens, and LAN addresses. -->

<details>
<summary>Contents</summary>

- [What it is](#what-it-is) | [Scope and non-goals](#scope-and-non-goals) | [Who this is for](#who-this-is-for)
- [Quick start](#quick-start) | [Concepts](#concepts) | [Requirements](#requirements)
- [Categories and ports](#categories-and-ports) | [Control page](#control-page) | [Tray](#tray)
- [Rescan](#rescan) | [Configuration](#configuration) | [Models](#models)
- [HTTP API](#http-api) | [Troubleshooting](#troubleshooting)
- [Security](#security) | [Docs, help, and license](#docs-help-and-license)

</details>

## What it is

SimpleHar SE starts, monitors, and stops your engine processes: llama.cpp `llama-server`,
strata Python servers, and ninfer GPU containers. One scheduler swaps models between four
category listeners and is the only writer of scheduler state. The harness admits loads on
the reading the card itself gives (the witness), minus a reserve, refuses to overcommit,
and names its reasons. No engine starts at boot: an engine starts only when a model loads.

## Scope and non-goals

- Not a model downloader. You place model files on disk yourself.
- Not a multi-user server. One user, one machine, one GPU.
- Windows only (10/11). No Linux or macOS host support.
- Not an inference engine. It supervises llama.cpp, strata, and ninfer.

## Who this is for

You run local LLMs on one NVIDIA GPU. You want:

- Several models that swap quickly, each on its own OpenAI-compatible endpoint, with no
  console windows to manage.
- One safe budget manager. It admits loads against what the card reports, never force-loads
  onto a full GPU, and names the blockers and numbers on every refusal.
- Declarative configuration: one JSON file for the harness and one flat YAML file per model.
  Both hot-reload, and every edit on the page writes back to those exact files.

## Quick start

### Use the release package

The v1.1.0 release package contains `simplehar-se.exe`, `simplehar-se.json`, `ModelConf/`,
and `ninfer-autostart-template.sh`. Public release assets are not published on GitHub yet,
so if you do not have the package, build from source below.

1. Extract the package to `<install-dir>`. If its `simplehar-se.json` sets `internal_host`
   to `0.0.0.0`, set `control_token` first, or change it to `127.0.0.1`.
2. Add one model row: create `ModelConf/example.yaml` (see below).
3. Run `<install-dir>\simplehar-se.exe` from that folder.
4. Open `http://127.0.0.1:8708/`.

### Build from source

Install the Rust toolchain, then:

```powershell
cargo build --release
# create a ModelConf\ folder next to the exe and drop one flat .yaml per model
target\release\simplehar-se.exe
start http://127.0.0.1:8708/
```

No config file is required: first start probes the GPU total (NVML) and `llama-server.exe`
on PATH and writes the authoritative `simplehar-se.json` beside the exe.

### Add one model

Create `ModelConf/example.yaml`:

```yaml
model: <path-to-model>.gguf
category: WORK
vram_gb: 8.0
cost: fixed
context: 8192
```

The file stem (`example`) is the model identity; schema in [MODELS.md](docs/reference/MODELS.md).

### Verify

The control page opens and shows four category tabs. No engine started at boot. Load one
model on the page: the harness starts its engine and the slot turns loaded.

## Concepts

<details>
<summary>Terms</summary>

- **harness**: this application, SimpleHar SE.
- **engine**: an inference server the harness starts and stops (`llama`, `strata`, `ninfer`).
- **category**: one of the four model groups (WORK, RP, AUX, EMB).
- **listener**: the public HTTP port that serves one category.
- **slot**: one model position inside a category.
- **witness**: the live VRAM reading taken from the GPU.
- **admission**: the decision that allows or refuses a model load.
- **scheduler**: the single component that changes slot and budget state.
- **catalog**: the set of model rows built from the `ModelConf` files.

</details>

## Requirements

| Engine | What runs | Stack required on the machine |
|---|---|---|
| `llama` (default) | a native `llama-server.exe` process per model, serving a GGUF file | [llama.cpp](https://github.com/ggml-org/llama.cpp) `llama-server.exe` (default expected at `C:\Inference\llama.cpp\llama-server.exe`, configurable) |
| `strata` | `python -m serve.server --engine strata` in a Python venv, on a harness-rendered profile | A strata install with its virtualenv (default `C:\Strata\.venv\Scripts\python.exe`, cwd `C:\Strata`). The model row's yaml carries the profile fields, and the harness renders the JSON. |
| `ninfer` | a GPU container inside WSL2. The harness renders and stages the autostart script, starts the container, and reaps it by container name. | A WSL2 distro and Docker, plus the ninfer container image. Set `wsl_distro` / `wsl_user` in the config. |

- Windows 10/11 with an NVIDIA GPU; GPU memory is read through NVML with `nvidia-smi` fallback.
- WSL2 and Docker only for `ninfer`; the Rust toolchain only for source builds.
- One GGUF file per `llama` model, or the engine artifact for `strata` and `ninfer`.

## Categories and ports

| Category | Public port | Slots | Purpose | Evicted by |
|---|---|---|---|---|
| WORK | 8708 | 1 | coding/agent work; also hosts the control page + `/har/*` | nobody |
| RP | 8709 | 1 | roleplay | WORK |
| AUX | 8710 | 1-6 | utility models | WORK, RP |
| EMB | 8711 | 1 | embeddings | WORK, RP |

The full port plan and rules live in [docs/reference/CONFIG.md](docs/reference/CONFIG.md).

- Same-slot replacement is always allowed: loading a different model into a category evicts
  that category's own occupant, whatever the matrix says.
- The "Evicted by" column is a one-way hard filter: a category yields only to the categories
  that list it. Nothing evicts WORK, only WORK evicts RP, and WORK and RP evict AUX and EMB.
- When several victims would cover the deficit, the planner takes the cheapest sufficient
  set; on a tie it frees the most disposable category first. A refusal that cannot free
  enough VRAM names the blockers, for example `only WORK can evict RP`.

## Control page

Open `http://127.0.0.1:8708/`. The page shows slot cards and memory bars (observed VRAM,
the reserve as a hatched band, in-flight bookings, host RAM), category tabs, per-model rows
(Load / Set default, state dot, category, CPU checkbox, Engine, Ctx, Cost, Files), recent
jobs, and a collapsible harness log. A loaded row turns amber while its file differs from
the bytes its launch used; unload and reload to apply. Updates arrive over SSE with a
polling fallback, and the theme adapts (light/dark) with a toggle.

## Tray

The tray shows green loaded, yellow swap in flight, gray idle, amber no verdict (reason in
the tooltip), or red listener down. Menu: Open Web UI, Unload All or per-category, Exit.

## Rescan

After you edit files, press **Rescan** or send `POST /har/rescan`. A rescan re-reads the
catalog only: a serving model keeps the launch it was spawned with, so an edit to a live
model's yaml takes effect on its next load. The row reads amber while the file differs.
A queued load whose yaml moved is retired with a `superseded by rescan` note; re-queue it.
Two files whose names collapse to one identity are both reported as `duplicate_name`;
loads of that name stay refused until you rename one file.

## Configuration

The authoritative `simplehar-se.json` sits beside the exe; page edits persist to it. Core keys:

- `internal_host`: bind address. Default `127.0.0.1`; `0.0.0.0` publishes to the LAN, so
  set `control_token` first. See Security.
- `control_token`: when set, every route except the page assets requires it.
  `SIMPLEHAR_TOKEN` overrides it and keeps the secret out of the file.
- `categories`: per-category id, port, slots, default model, and eviction.
- `vram_total_gb`: admission ceiling. First start normally detects it from the card.
- `modelconf_dir`: catalog directory. Default `<exe dir>\ModelConf`.

The full field table is in [docs/reference/CONFIG.md](docs/reference/CONFIG.md).

## Models

Each model is one flat `key: value` YAML file in `ModelConf`; the file stem is the model
identity. A row picks its engine, category, and admission cost source (`fixed`, `greedy`,
or `exact`). The full schema and engine knobs: [docs/reference/MODELS.md](docs/reference/MODELS.md).

## HTTP API

Each public listener answers the OpenAI-style endpoints:

- `/v1/models` (filtered by category)
- `/v1/chat/completions`
- `/v1/completions`
- `/v1/embeddings`

A request that names no model goes to the loaded model of that category, or to its
`default_model` when nothing is loaded. The control page, the `/har/*` routes, memory
provenance, and error codes are in [docs/reference/API.md](docs/reference/API.md).

## Troubleshooting

- **A port will not bind.** A Hyper-V/WSL2 reservation may cover it. List ranges with
  `netsh interface ipv4 show excludedportrange protocol=tcp`. To reserve your port
  permanently, use the full command in [docs/reference/TROUBLESHOOTING.md](docs/reference/TROUBLESHOOTING.md).
- **A model file is missing at boot.** The scan drops the row and reports
  `! <file>.yaml: model file not found: <path>`.
- **Refusals.** Every refusal names the blockers and the numbers, in `/har/status` and in
  `last_error`. Nothing over-admits, so unload something; the witness is the budget truth.
- **`launch floor not met`** (code `below_launch_floor`): the card holds too little free
  VRAM, so the harness refused the load before spawn. Free VRAM, or lower the model's
  `launch_only_with_at_least_vram_free`.
- **A second instance.** The single-instance guard makes a duplicate exit immediately.

Logs and the full list: [TROUBLESHOOTING.md](docs/reference/TROUBLESHOOTING.md).

## Security

Set `control_token`, or set `SIMPLEHAR_TOKEN`, which wins so the secret stays out of the file.
**Every route then requires the token**, on `X-Har-Token` or `Authorization: Bearer`: `/har/*`,
each listener's `/v1/*`, and `GET /health` too. Only the page and its assets answer
unauthenticated. The structural guards run **before** the token check: a cross-site `Origin`
on a POST gets `403`, and a missing `application/json` content-type gets `415`, whatever the
token. The harness never logs the token and never accepts it from a URL.

With no token set the harness has **no authentication**. `internal_host: 0.0.0.0` (Publish
to LAN) then trusts every machine on the segment to load and unload models and to drive
the GPUs; it warns loudly at boot, and the default is loopback only. The token guards
only the harness's own listeners: engine ports have no harness auth, so the harness forces
llama and strata engines to loopback regardless of `internal_host`, and every ninfer
launch publishes loopback-only.

## Docs, help, and license

| Topic | Where |
|---|---|
| Architecture overview | [docs/architecture/OVERVIEW.md](docs/architecture/OVERVIEW.md) |
| Decision ledger | [docs/architecture/DECISIONS.md](docs/architecture/DECISIONS.md) |
| Engineering standards | [docs/process/ENGINEERING.md](docs/process/ENGINEERING.md) |
| Documentation map | [docs/DOCS_README.md](docs/DOCS_README.md) |
| Configuration reference | [docs/reference/CONFIG.md](docs/reference/CONFIG.md) |
| Model schema reference | [docs/reference/MODELS.md](docs/reference/MODELS.md) |
| HTTP API reference | [docs/reference/API.md](docs/reference/API.md) |
| Operations reference | [docs/reference/OPERATIONS.md](docs/reference/OPERATIONS.md) |
| Troubleshooting reference | [docs/reference/TROUBLESHOOTING.md](docs/reference/TROUBLESHOOTING.md) |
| Contributing | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Changelog | [CHANGELOG.md](CHANGELOG.md) |
| License | [LICENSE](LICENSE) (Apache-2.0) |
| Issues | [legislat/SimpleHar/issues](https://github.com/legislat/SimpleHar/issues) |
