# Search#1 — Basalt engine design

Timestamp: 2026-11-03T19:56:21Z
Recommendation: Launch the Basalt Python server directly against a harness-rendered
JSON profile, force loopback/port/engine binary/GPU on argv, and book a declared
`vram_gb`; keep the existing job-object kill as the safety net.
Builds on: the local Basalt install research referenced by the install guide
(`Search#29-basalt-windows-inference.md`, `Search#7-basalt-vs-strata.md`), not in this
repository.

## 1. Request (broken down)

- Goal: add `engine: basalt` to SimpleHar as a template-mode engine with the same
  custody, readiness, cost, and release guarantees as `strata`, without a scheduler
  change.
- Sub-questions:
  1. Which process should the harness launch, and how do we keep bind, port, model,
     engine binary, and GPU harness-owned?
  2. What is the readiness signal for a server that answers HTTP before weights load?
  3. What is the cost source when the VRAM owner is a grandchild process?
  4. Which paths are custody roots, and which kill path releases the GPU cleanly?
  5. Which row YAML keys and `EnginePolicy` row are required?
- Definition of "done": a design note names the launch spec, readiness, cost source,
  custody roots, and kill semantics; a later implementation can add the adapter and
  prove a card run.

## 2. Setup & Constraints

- Stack/runtime: Windows 11 x64, one Blackwell GPU, 512 GB RAM; Basalt is a native
  C++ engine with a Python OpenAI-compatible server.
- Already present: Basalt source, venv, and `bin\basalt.exe` under `C:\Inference\Basalt`
  (verified locally: source `421063f`, Python 3.14.7, binary present). Upstream `main`
  was `9595ae0` on 2026-11-03 and has no releases or tags.
- Constraints:
  - One GPU only; multi-GPU/peer tier is out of scope.
  - The harness owns the bind, port, config path, engine binary, and GPU choice.
  - The engine must not read an operator profile directly; the harness stages the
    profile, as `strata` does.
  - No API key is required on loopback; Basalt refuses `api-key` in its config file.
  - The existing witness attributes bytes to the direct child; Basalt's VRAM owner is
    the engine grandchild, not the Python server.
  - The task note says "32GB class", while the local install guide says 72 GB. Treat
    VRAM as a declared row value, never a design constant.

## 3. Variants

### Variant A — Direct Python server with a harness-rendered profile

- What it is: launch
  `basalt_python -m serve.server --engine-bin <basalt.exe> --config <staged.json>
  --host 127.0.0.1 --port <slot> --alias <serve_model> --gpu 0` in `basalt_cwd`.
  The harness writes the `{"server": ...}` profile under
  `.simplehar\basalt-profiles`, and appends row-owned engine flags.
- Fit for this setup: **High**. It removes the source-tree launcher, keeps the
  harness in control of every decision, and matches the strata template-mode shape.
- Pros:
  - The direct child is the Python server; the existing job object owns it.
  - The profile path is rewritten to the staged file, so the child cannot read a
    different profile.
  - `--host`, `--port`, `--engine-bin`, and `--gpu` are explicit argv values, not
    config values.
  - The server validates engine flags against the engine binary before load.
- Cons:
  - The harness must own `basalt_python` and `basalt_cwd` in addition to the binary.
  - Basalt's source tree is the Python package root; an installed wheel is not used.
  - Engine flags are argv, not all in the rendered profile; the profile remains the
    server-settings source of truth.
- Source(s): `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/configuration/index.md`,
  `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/config.py`,
  `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/server.py`.

### Variant B — `basalt.exe serve` launcher

- What it is: launch `bin\basalt.exe serve --config <staged.json> --python <venv>` and
  let the launcher start the Python server.
- Fit for this setup: **Medium**. It is the documented CLI, but the launcher is tied
  to the source tree and adds a process layer.
- Pros:
  - One documented entry point; `--model` and `--python` are handled by Basalt.
  - The launcher forwards the remaining arguments to the server.
- Cons:
  - The launcher resolves the Python package from its build-time source tree; a moved
    binary or packaged install is not portable.
  - The harness's direct child is the launcher, not the server; release observation
    and process attribution get one layer worse.
  - The launcher's default Python is the POSIX `.venv/bin/python`; Windows still needs
    `--python`.
- Source(s): `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/configuration/index.md`,
  `https://raw.githubusercontent.com/jesdga95/basalt/main/README.md`.

### Variant C — Reuse the strata profile/import path unchanged

- What it is: treat Basalt as another `serve.server --engine ...` profile and reuse
  the strata renderer and import CLI.
- Fit for this setup: **Low**. It looks similar but the schemas and flags differ.
- Pros:
  - Reuses the staged-profile mechanism and the `--config` rewrite.
- Cons:
  - Basalt has `server`/`engine` sections with `engine-bin` refused in config;
    strata's profile shape is not the same.
  - Basalt's `--gpu`, `--alias`, `--parallel`, and engine flags have different
    ownership rules.
  - The strata import command would produce a misleading, non-working row.
- Source(s): `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/configuration/index.md`,
  `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/config.py`.

### Options considered and rejected

- Containerize the server — rejected: the install is native Windows, the engine needs
  the host GPU, and the task is one GPU.
- Let the row point at an operator `basalt_config.json` — rejected by the strata
  ruling 19 custody model: the harness must render and stage the profile.
- Book `per_process` witness bytes — rejected for v1: the direct child is Python, and
  the VRAM owner is the engine grandchild; the witness would under-report.

## 4. Comparison

| Variant | Fit | Key pro | Key con |
|---------|-----|---------|---------|
| A direct Python | High | Full harness custody; direct child; no launcher layer | Needs `basalt_python` and `basalt_cwd` |
| B `basalt serve` | Medium | Documented entry point | Source-tree launcher; extra layer |
| C strata reuse | Low | Reuses renderer | Different config schema and ownership |

## 5. Recommendation & Decisions

- Chosen: **Variant A**, direct Python server with a harness-rendered profile.
- Why: it matches the strata custody model while avoiding the source-tree launcher;
  it keeps host, port, engine binary, profile path, and GPU in harness-controlled
  argv, and the existing job object can own the server directly.

Decisions:

- Launch spec:
  - Program: `basalt_python` (existing `.exe`).
  - Cwd: `basalt_cwd` (the Basalt source root, for `-m serve.server`).
  - Required argv: `-m serve.server`, `--engine-bin <basalt_exe>`,
    `--config <staged-profile>`, `--host 127.0.0.1`, `--port <slot>`,
    `--alias <serve_model>`, `--gpu 0`.
  - `basalt_parallel` (default 1, 1..8) becomes `--parallel N`; it changes KV cost,
    so the row's declared cost must match it.
  - `basalt_engine_args` is a comma-separated engine-flag list, validated by the
    server against the engine binary; `--host`, `--port`, `--config`, `--model`,
    `--engine-bin`, `--gpu`, `--python`, and `--api-key` are refused there.
  - The rendered profile contains only `server` options: the absolute `.basalt`
    model path, `alias`, and `parallel`. It never contains `host`, `port`, `api-key`,
    `gpu`, `config`, or `engine-bin`.
- Readiness: `/health` must answer 200 and its JSON `loaded` field must not be
  `false`; Basalt answers HTTP while weights stream. Then `/v1/models` must report
  the `serve_model` alias. `load_timeout` should be 1200 seconds for the 482B model
  class; the install guide's measured cold start is ~415 s before the final pack.
- Cost source: **declared `vram_gb` with `cost: fixed`**. Basalt fills the card's
  free VRAM up to its reserve/cache policy; the row must declare the ceiling the
  operator intends. The harness witness will likely classify the reading as
  `total_diff` (free-memory delta) because the direct child is the Python server and
  the engine is a grandchild. `total_diff` is display-only, so the declared figure
  books. If measured booking is required later, add an adapter-owned report hook that
  reads Basalt's `/v1/status` `vram` (or a `VRAM` control response) and feeds the
  existing `engine_report` path; do not call the Python process's `per_process`
  figure "the model".
  - `launch_only_with_at_least_vram_free` should be set to the declared cost floor;
    Basalt needs the card, and a loaded stranger must refuse before spawn.
  - The row's `vram_gb` is hardware-specific. The install guide says 72 GB; the task
    note says 32 GB class. Take the real card and a witnessed run, not either note.
- Custody roots:
  - Rendered profile root: `.simplehar\basalt-profiles` beside the exe,
    content-addressed `<model>-<hash>.json`, atomic write, keep-N prune; add
    `BASALT_RENDERED_PROFILES_ROOT` parallel to the strata root.
  - Executed paths: `basalt_python`, `basalt_cwd`, `basalt_exe`.
  - Data paths: the `.basalt` model file and its embedded tokenizer live under the
    operator's model root; the staged profile is the only config the child reads.
  - No key file and no API key: loopback only, like strata. The engine's own
    `api-key` is command-line-only and is not passed.
- Kill semantics:
  - The harness's job object terminates the Python server. Basalt's Python server puts
    the engine child in its own kill-on-close job, so Python death also terminates the
    engine; this is the orphan guarantee for the crash path.
  - The server's own stop path is `QUIT`, then terminate, then kill if the engine
    refuses; the harness's `TerminateJobObject` bypasses that graceful path. For v1,
    job kill is acceptable because the engine job is kill-on-close, but a card run
    must prove VRAM release and no orphan. If the run shows held VRAM, add a pre-stop
    `POST /unload` hook before job termination as a follow-up.
  - `release_confirmed` must check the server port and the job tree; a card run must
    also show the engine PID gone and VRAM returned.
- Engine policy row:
  - `id: "basalt"`, `supports_fit: false`, forced `--host` literal `127.0.0.1`,
    `--port` slot, and `--config` operator-data; the adapter supplies
    `--engine-bin`, `--alias`, and `--gpu 0`.
  - Forbidden words include `--host`, `--port`, `--config`, `--model`,
    `--engine-bin`, `--gpu`, `--python`, `--api-key`, and every engine-owned flag
    that moves weights or bind; `basalt_engine_args` is the only row-owned engine
    surface.
- Row keys (proposed, documented in `docs/reference/MODELS.md`):
  - `model`: absolute `.basalt` pack.
  - `engine: basalt`.
  - `serve_model`: alias passed as `--alias` and verified via `/v1/models`.
  - `vram_gb` and `cost: fixed`; `launch_only_with_at_least_vram_free`.
  - `basalt_python`, `basalt_cwd`, `basalt_exe`.
  - `basalt_parallel` (default 1).
  - `basalt_engine_args` (closed engine-flag surface).
  - `load_timeout: 1200`.

## 6. Sources

- `https://github.com/jesdga95/basalt` (accessed 2026-11-03, upstream `9595ae0`) —
  repository, license, current `main`.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/README.md` (accessed
  2026-11-03) — C++ engine plus Python server; Windows support; one-GPU limits;
  release/tag status.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/configuration/index.md`
  (accessed 2026-11-03) — `serve` CLI, config sections, `api-key` refusal, precedence,
  engine flags, GPU/context/expert-cache settings.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/running.md` (accessed
  2026-11-03) — load/ready behavior, memory floors, idle sleep, Windows notes.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/docs/platforms.md` (accessed
  2026-11-03) — Windows one-GPU and pinned-arena limits.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/server.py` (accessed
  2026-11-03) — direct `python -m serve.server` path, shutdown path, engine startup.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/config.py` (accessed
  2026-11-03) — server options, `--config` sections, `--engine-bin` refusal in config,
  path resolution.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/handler.py` (accessed
  2026-11-03) — `/health` loaded flag, `/v1/models`, `/unload` ownership check.
- `https://raw.githubusercontent.com/jesdga95/basalt/main/serve/engine/process.py`
  (accessed 2026-11-03) — `QUIT`/terminate/kill stop sequence.
- `D:\WORKSPACE-VS\installation-guide\basalt\Basalt-INSTALL.md` (local, generated
  2026-10-25; local probe 2026-11-03) — installed source, venv, binary, GPU, and the
  `421063f` local SHA.

## 7. Handoff (next work)

- What to build: `src/engine/basalt.rs`, a `BASALT_ROW` policy row, staged-profile
  root, row schema/scan keys, and tests with a fake Python/HTTP server; then a real
  card run.
- Where it lives:
  - `src/engine/basalt.rs` — adapter (`EngineRuntime`, `spec`, preflight, launch,
    readiness, discovery).
  - `src/engine/policy.rs` — row and roots.
  - `src/process.rs` — staged-profile root/staging and `--config` rewrite.
  - `src/model/mod.rs` / `scan.rs` — row keys and inert-key handling.
  - `docs/reference/MODELS.md` — Basalt key reference.
- How to verify:
  - Cardless: `cargo test basalt` and the full `scripts/gate.ps1`; a fake server
    proves `/health` loaded gating, `/v1/models` alias, forced argv, and the staged
    config path.
  - Card: load the 8-bit pack, confirm `/health` loaded and `/v1/models`, then unload
    and confirm the process tree is empty, the port is closed, and VRAM returns.
- Watch: upstream is a moving `main` with no tags; pin a SHA before the card run.
  Check open Windows issues and any change to `serve/config.py` or `--engine-bin`.
- Open questions:
  - The real card is 72 GB in the install guide, 32 GB class in the task note; measure
    `vram_gb` on the target before writing the row.
  - Declared `vram_gb` is the v1 booking source; decide whether a later engine-report
    hook is worth the measured precision.
  - Graceful `/unload` before job kill only if the card run shows held VRAM.
- Suggested first step: add the policy row and a cardless fake-Python adapter test;
  do not load the model until the adapter and config staging pass.
