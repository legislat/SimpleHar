# SimpleHarn

[![CI](https://img.shields.io/github/actions/workflow/status/legislat/SimpleHarn/ci.yml?branch=main)](https://github.com/legislat/SimpleHarn/actions/workflows/ci.yml)

SimpleHarn is a Windows tray harness for local LLM inference. It runs several inference
engines on one GPU behind one OpenAI-compatible API. One swap scheduler is the only writer
of scheduler state. It admits every model load against the memory the card itself reports,
and it refuses to overcommit. The product name is SimpleHarn, and the repository is
`legislat/SimpleHarn`. The binary is `simpleharn.exe`, the config file is `simpleharn.json`,
and the environment names use the `SIMPLEHARN_` prefix. An existing `simplehar-se.json` or
`simplehar.json` config and the deprecated `SIMPLEHAR_*` environment names keep working.

Current release: **v1.2.0**. Windows 10/11, one NVIDIA GPU, single user.
Prebuilt binaries: see the GitHub Releases page of this repository.

> This is a personal harness, built for my own machine and my own setup. Feel free to try
> it out, give feedback, and contribute.

## Who this is for

You run local LLMs on Windows with one NVIDIA GPU. You want several models that swap
quickly, each on its own OpenAI-compatible endpoint, with no console windows to manage.
You want one safe budget manager that never force-loads onto a full GPU and names the
blockers and the numbers on every refusal.

The `llama` path (llama.cpp) works on any machine that can run `llama-server.exe`. The
`strata`, `basalt`, and `ninfer` paths are supported for this operator's own setup. They
may not fit other machines. Compared with llama-swap, which also swaps models, SimpleHarn
adds a memory-accounting scheduler that refuses to overcommit. What this is not: it is
not multi-user, it is not a router for remote or cloud engines, and it is not a replacement
for llama-swap. It runs and supervises local engines on one machine only.

This software is provided AS-IS, with no warranty of any kind. See
[LICENSE](LICENSE) (Apache-2.0).

## Documentation style

The docs in this repository are written in ASD-STE100 (Simplified Technical English) on
purpose: short sentences, active voice, one idea per sentence. People and software agents
can parse them reliably. The trade is that some pages read terse.

## Quick start

You need the stable Rust toolchain on Windows.

```powershell
cargo build --release
```

1. Create a `ModelConf` folder beside the built exe. Put one flat `.yaml` file per model:

   ```yaml
   engine: llama
   model: <path-to-model>.gguf
   category: WORK
   vram_gb: 8.0
   cost: fixed
   context: 8192
   ```

   The file stem (`example`) is the model identity. The schema is in
   [MODELS.md](docs/reference/MODELS.md).

2. Run `target\release\simpleharn.exe` with the `ModelConf` folder beside it. First run
   probes the GPU total (NVML) and `llama-server.exe` on PATH. It writes the authoritative
   `simpleharn.json` beside the exe, and it binds loopback only. It never overwrites a
   config file that already exists. An existing `simplehar-se.json` keeps working: the
   harness loads it in place and writes back there; an older `simplehar.json` is copied
   over once.

3. Open `http://127.0.0.1:8708/`. Load one model on the page. The harness starts that
   model's engine. No engine runs before a load.

The four category listeners on ports 8708-8711 answer the OpenAI-compatible API.

## How it works

- **Single writer.** The scheduler is the only component that changes slot and budget
  state. Every load and unload passes through one decision point.
- **The witness is the budget truth.** Admission reads the live VRAM figure from the card,
  minus a reserve. The harness never books more than the card reports, so it never
  overcommits.
- **Refusals name their reasons.** Every refusal lists the blockers and the numbers, on
  the page and in the API.
- **Eviction is a one-way matrix.** Nothing evicts WORK. Only WORK evicts RP. WORK and RP
  evict AUX and EMB. Same-slot replacement is always allowed. When several victims would
  cover the deficit, the planner takes the cheapest sufficient set.
- **Config file is king.** One JSON file beside the exe plus one flat YAML file per model.
  Both hot-reload, and every edit on the page writes back to those exact files.

## Categories and ports

| Category | Public port | Slots | Purpose | Evicted by |
|---|---|---|---|---|
| WORK | 8708 | 1 | coding/agent work; also hosts the control page + `/har/*` | nobody |
| RP | 8709 | 1 | roleplay | WORK |
| AUX | 8710 | 1-6 | utility models | WORK, RP |
| EMB | 8711 | 1 | embeddings | WORK, RP |

The full port plan is in [docs/reference/CONFIG.md](docs/reference/CONFIG.md).

## Control page and tray

![The SimpleHarn control page in the light theme](docs/assets/control-page.png)

The page shows slot cards, memory bars (observed VRAM, the reserve, in-flight bookings,
host RAM), per-model rows with Load and Default, recent jobs, and the harness log. A
rescan re-reads the catalog; a serving model keeps the launch it was spawned with, so an
edit to a live model takes effect on its next load. The tray shows green loaded, yellow
swap in flight, gray idle, or red listener down. The menu offers Open Web UI, Unload All,
and Exit.

## Configuration

The authoritative `simpleharn.json` sits beside the exe; page edits persist to it. Core
keys:

- `internal_host`: bind address. Default `127.0.0.1`. Read Security before you change it.
- `control_token`: when set, every route except the page assets requires it. The
  `SIMPLEHARN_TOKEN` environment variable overrides it and keeps the secret out of the
  file. The deprecated `SIMPLEHAR_TOKEN` spelling still works, with one logged warning.
- `categories`: per-category id, port, slots, default model, and eviction.
- `vram_total_gb`: admission ceiling. First run normally detects it from the card.
- `modelconf_dir`: catalog directory. Default: a `ModelConf` folder beside the exe.

The full field table is in [docs/reference/CONFIG.md](docs/reference/CONFIG.md). Each
model is one flat `key: value` YAML file in `ModelConf`; the full schema and the engine
knobs are in [docs/reference/MODELS.md](docs/reference/MODELS.md).

## HTTP API

Each public listener answers the OpenAI-style endpoints `/v1/models`,
`/v1/chat/completions`, `/v1/completions`, and `/v1/embeddings`. A request that names no
model goes to the loaded model of that category, or to its `default_model` when nothing is
loaded. The control routes, memory provenance, and error codes are in
[docs/reference/API.md](docs/reference/API.md).

## Security

The API is unauthenticated by default, and it binds to loopback. Set `SIMPLEHARN_TOKEN`
(or `control_token`) before any non-loopback bind, and never expose the harness to a LAN
without a token.

- With a token set, every route except the page assets requires it, on `X-Har-Token` or
  `Authorization: Bearer`. The harness never logs the token and never accepts it from a
  URL.
- The harness forces every engine it starts onto a loopback listener, regardless of
  `internal_host`. The token guards the harness's own listeners only.
- Structural guards run before the token check: a cross-site `Origin` on a POST gets 403,
  and a missing `application/json` content type gets 415.

## Requirements

| Engine | What runs | What the machine needs |
|---|---|---|
| `llama` (default) | a native `llama-server.exe` process per model, serving a GGUF file | a llama.cpp build; first run prefers a `llama-server.exe` found on PATH, and `llama_server_exe` names it |
| `strata` | a Python server in a virtualenv, on a harness-rendered profile | a strata install with its virtualenv; the interpreter and working directory are config keys |
| `basalt` | a Python server in a virtualenv, on a harness-rendered profile | a Basalt install with its venv and engine binary; the model row names them |
| `ninfer` | a GPU container inside WSL2, staged and reaped by the harness | a WSL2 distro, Docker, and the ninfer image; set `wsl_distro` / `wsl_user` |

GPU memory is read through NVML with an `nvidia-smi` fallback. WSL2 and Docker are needed
only for `ninfer`. One GGUF file is needed per `llama` model.

## Troubleshooting

Common cases: a port that will not bind (a Hyper-V/WSL2 reservation often covers it), a
missing model file at boot (the scan drops the row and reports it), and a repeated
refusal (unload something; the harness never over-admits). The full list, with the
`netsh` fix for reserved ports, is in
[docs/reference/TROUBLESHOOTING.md](docs/reference/TROUBLESHOOTING.md).

## Docs, help, and license

| Topic | Where |
|---|---|
| Agent entry point | [AGENTS.md](AGENTS.md) |
| Binding invariants | [docs/architecture/DOCTRINE.md](docs/architecture/DOCTRINE.md) |
| Architecture overview | [docs/architecture/OVERVIEW.md](docs/architecture/OVERVIEW.md) |
| HTTP API reference | [docs/reference/API.md](docs/reference/API.md) |
| OpenAI compatibility matrix | [docs/reference/COMPATIBILITY.md](docs/reference/COMPATIBILITY.md) |
| Model schema reference | [docs/reference/MODELS.md](docs/reference/MODELS.md) |
| Configuration reference | [docs/reference/CONFIG.md](docs/reference/CONFIG.md) |
| Documentation map | [docs/DOCS_README.md](docs/DOCS_README.md) |
| Contributing | [CONTRIBUTING.md](CONTRIBUTING.md) |
| Changelog | [CHANGELOG.md](CHANGELOG.md) |
| License | [LICENSE](LICENSE) (Apache-2.0) |
| Issues | [legislat/SimpleHarn/issues](https://github.com/legislat/SimpleHarn/issues) |

Found a problem or have an idea? Open an issue. Contributions are welcome; start with
[CONTRIBUTING.md](CONTRIBUTING.md).
