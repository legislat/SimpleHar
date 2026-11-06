---
title: Engine setup recipes
owner: operator
status: active
audience: [user, operator, agent]
source_of_truth_for: [minimal engine setup for the harness]
last_reviewed: 2026-11-06
review_trigger: engine-change, release
---

# Engine setup recipes

Minimal recipes for the engines the harness runs, for an installed engine or a
fresh install. Rows go in `ModelConf/`; restart or rescan after you add one.
The harness forces the bind, the port and the key. The YAML file stem is the
model identity: a `name` key that differs from the stem is refused at scan,
so rename the file or drop the key. Ready rows are in
[examples/README.md](../../examples/README.md).

Use `cost: greedy` with a launch floor for an engine that offloads experts;
`cost: fixed` with `vram_gb` for a fixed resident. `load_timeout` is the
cold-load deadline.

## llama.cpp

Upstream: [ggml-org/llama.cpp](https://github.com/ggml-org/llama.cpp) (README,
releases).

**Already installed.** Use the directory that holds `llama-server.exe` and its
DLLs.

```yaml
name: my-llama
engine: llama
model: <models>\<model>.gguf
context: 16384
vram_gb: 24
cost: fixed
load_timeout: 1200
```

**Not installed.** Download the CUDA x64 archive and the matching `cudart`
archive from the releases page. Extract both into one directory, so the DLLs
sit beside `llama-server.exe`. Then use the row above.

`cpu_offload: true` runs the model on host RAM.

**Which binary.** The `llama_server_exe` key in `simpleharn.json` is an
optional global default. First start detects a `llama-server.exe` on PATH and
records it. A row may set `llama_server_exe` to pick a different llama.cpp
fork for that model only; the path must be an absolute path to an existing
`llama-server.exe`. The engine DLLs must sit beside the chosen binary. Two
rows on two forks (placeholders, not machine facts):

```yaml
name: llama-fork-a
engine: llama
model: <models>\<model-a>.gguf
vram_gb: 24
cost: fixed
llama_server_exe: <fork-a-dir>\llama-server.exe
```

```yaml
name: llama-fork-b
engine: llama
model: <models>\<model-b>.gguf
vram_gb: 8
cost: fixed
llama_server_exe: <fork-b-dir>\llama-server.exe
```

## Strata

Upstream: [Niko1221/Strata](https://github.com/Niko1221/Strata) (README install,
[SOURCE_INSTALL.md](https://github.com/Niko1221/Strata/blob/main/docs/SOURCE_INSTALL.md)).

**Already installed.** Use the root that holds `serve/`, `.venv/`,
`<strata-root>\engine\strata.exe` and the engine pack.

```yaml
name: my-strata
engine: strata
model: strata://<pack-name>
strata_exe: <strata-root>\engine\strata.exe
strata_tokenizer: <pack>\tokenizer
strata_args: --pack, <pack>, --native, <dense>.gguf, --ple-gguf, <ple>.gguf, --expert-profile, <expert-profile.bin>, --max-context, 262144, --kv, int8, --ple-io, direct
strata_lib_dirs: <strata-root>\.venv\Lib\site-packages\nvidia\cu13\bin\x86_64
strata_env: STRATA_ROUTE_PRIOR=0 STRATA_KVA=0
serve_model: <engine model_name>
category: WORK
cost: greedy
launch_only_with_at_least_vram_free: 60
load_timeout: 1800
```

**Not installed.** Run the official Windows installer:

```powershell
irm https://github.com/Niko1221/Strata/releases/latest/download/install.ps1 | iex
```

Or build from source per [SOURCE_INSTALL.md](https://github.com/Niko1221/Strata/blob/main/docs/SOURCE_INSTALL.md). The installer prepares the
engine and the Python environment; it can also download a model. Then use the
row above.

Keep engine tuning in `strata_args`.

## Basalt

Upstream: [jesdga95/basalt](https://github.com/jesdga95/basalt) (README quick
start, [model-file.md](https://github.com/jesdga95/basalt/blob/main/docs/model-file.md)).

**Already installed.** Use the root that holds `serve/`, `.venv/`,
`<basalt-root>\bin\basalt.exe` and a `.basalt` pack of the same generation.

```yaml
name: my-basalt
engine: basalt
model: <models>\<pack>.basalt
serve_model: <served id>
basalt_python: <basalt-root>\.venv\Scripts\python.exe
basalt_cwd: <basalt-root>
basalt_exe: <basalt-root>\bin\basalt.exe
category: WORK
cost: greedy
launch_only_with_at_least_vram_free: 60
load_timeout: 1800
```

**Not installed.**

```powershell
git clone https://github.com/jesdga95/basalt && cd basalt
cmake -S . -B build -DCMAKE_BUILD_TYPE=Release
cmake --build build -j
pip install -r requirements.txt
```

Make or copy a `.basalt` pack with [model-file.md](https://github.com/jesdga95/basalt/blob/main/docs/model-file.md); v0.2.x reads pack
version 2. Then use the row above.

## Project Maya

Upstream: [mw00/project-maya](https://github.com/mw00/project-maya) (README
quick start and install).

**Already installed.** Use the root that holds `serve/`, `.venv/`,
`<maya-root>\build\maya.exe` and a model JSON config.

```yaml
name: my-maya
engine: maya
maya_python: <maya-root>\.venv\Scripts\python.exe
maya_cwd: <maya-root>
maya_config: <maya-root>\maya-<model>.json
serve_model: <config model_name>
category: WORK
cost: greedy
launch_only_with_at_least_vram_free: 60
load_timeout: 1800
```

**Not installed.**

```powershell
git clone https://github.com/mw00/project-maya.git && cd project-maya
.\START-MAYA.bat
```

On Linux run the setup script (`setup.sh`). The setup builds the engine, downloads a model and
writes the JSON config. Then use the row above.

The harness runs `serve.server` directly, not `maya.py`. Keep the config
`model_name` equal to `serve_model`; the harness ignores `host`, `port`,
`api_key`, `cors_origins` and `idle_unload_s`.

## NInfer (ninfer)

Upstream (container path): [Neroued/ninfer](https://github.com/Neroued/ninfer)
and [iamwavecut/ninfer-all](https://github.com/iamwavecut/ninfer-all). The native
Windows [NInferno](https://github.com/Brontir/NInferno) build is a separate
manual path; the harness does not launch it.

**Already installed.** The harness path is the container template. Use Docker
in WSL, the model roots, and the `.ninfer` artifact.

```yaml
name: my-ninfer
engine: ninfer
model: ninfer://<container>:8080/<id>
ninfer_image: <image>:local
ninfer_container: <container>
ninfer_model_name: <artifact directory>
ninfer_artifact: <artifact>.ninfer
ninfer_model_roots: <wsl-root-1>,<wsl-root-2>
ninfer_profile: b
serve_model: <id>
category: WORK
vram_gb: 26
load_timeout: 600
```

**Not installed.** Install Docker in WSL and build or pull the `ninfer` or
`ninfer-all` image from those repositories. Put the `.ninfer` artifact under one
of the model roots. Then use the row above.

The native Windows build is a separate manual path; the harness does not launch
it. Use the space form for serve flags (`--flag value`), never `--flag=value`.

## Verify

1. Add the row to `ModelConf/`, then restart the harness or rescan.
2. Load the model and wait for `serving`.
3. Send a chat to the category listener; expect HTTP 200.
4. Unload; the port closes and the card frees.
