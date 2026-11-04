---
title: Model reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [ModelConf schema, engine-specific model keys]
last_reviewed: 2026-11-04
review_trigger: behavior-change, release
---

# Model reference

Up: `docs/DOCS_README.md`. This file owns the ModelConf schema and the
engine-specific model keys.

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
| `model` | GGUF path (or the engine's model URI). **Required for llama**. A basalt row requires an absolute `.basalt` pack path. |
| `engine` | `llama` (default), `strata`, `basalt`, or `ninfer`. |
| `category` | `WORK`/`RP`/`AUX`/`EMB`. |
| `vram_gb` | Declared admission cost in GiB. `fixed` falls back to `file size x 1.15 + 2`. `exact` refuses without it. |
| `cost` | `fixed` (default): the declared size binds admission. `greedy`: the model offloads to RAM/SSD; the size never refuses, but a launch floor is mandatory. `exact`: an engine-computed number, required and binding. |
| `launch_only_with_at_least_vram_free` | GiB. The harness refuses the load **before spawn** unless free VRAM reaches this floor. The reserve is deducted. Free VRAM that meets the floor counts only what the harness itself can make free: its own releasable residents. Foreign occupancy never counts toward the floor. When no release the harness can perform would reach the floor, the refusal says so: "No releases allow for the VRAM floor. Check foreign residents." This is the thrash guard for large and greedy quants. Greedy models without this key inherit `greedy_launch_min_free_gb`. Set `0` on a model to opt out. |
| `cpu_offload` | `true`/`1`/`yes`/`on` forces `-ngl 0` (**llama only**). Other engines report an `inert_key` hint, and the page offers no CPU control for them. |
| `serve_model` | The expected engine-reported id. A mismatch records a warning; the harness never trusts it. |
| `measured_gb` / `measured_gb_min` | The footprint range the harness measured while this model served: `measured_gb` is the high end and `measured_gb_min` the low end. The harness writes them back after a load. When `vram_gb` is absent, admission books `measured_gb`. The `measured_attribution` stamp decides whose number it is, and only these attributions book: `engine_report` (the container's own startup reservation), `engine_status` (a process engine's own status endpoint stated the footprint), and `per_process` (the witness attributed the bytes to the engine's own process tree). A `total_diff` figure (a free-memory delta) displays but never books. An **absent** stamp is grandfathered and books on `fixed` rows. `greedy` never books a measured figure, whatever the provenance, because it is offloaded by design. Ninfer rows also carry `measured_profile`: an `engine_report` figure is voided when the resolved profile identity changes. The identity is the profile name, plus the resolved `kv` for an override line. An edit to the line's `ctx`/`conc`/`kv_dtype` that leaves `kv` alone keeps the figure, because the reservation follows the KV pool. The write-back also records `measured_offload`, the `cpu_offload` state the pair was taken under; a later flip voids the pair. |
| `is_embedding_model` | A truthy value forces category `EMB`. It conflicts with another explicit category. |
| `context`, `batch`, `flash_attn`, and other flag keys | The harness maps these keys to the corresponding `llama-server` flags. `context` is **llama only**: on other engines it stays an inert key (the scan says so), and the page and API never display it there. The Ctx figure shown for a strata row is the `--max-context` figure in its `strata_args` (the `--max-context` token or the `--max-context=` spelling, digits only). For a ninfer row it is the max-context of the resolved profile. |
| `load_timeout` | Per-model readiness deadline in seconds. The llama default is 120; strata and ninfer cold starts commonly need 600 to 1200. For the basalt 482B class, `load_timeout: 1200` is the recommended starting point. |
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
| `strata_config` | **Superseded** (operator ruling 2026-11-02, row 19). A row that still declares it is refused at scan, with the migration sentence: run `simplehar-se import strata <profile.json> --apply <row.yaml>` to merge the emitted knobs and delete the `strata_config` line, or print and paste manually. A row that carries both `strata_exe` and `strata_config` meets the mutual-exclusion refusal; delete the `strata_config` line. |
| `basalt_python` | **Required for basalt**: the interpreter `.exe`. It must be an absolute path to an existing file, and it is the program the harness launches. |
| `basalt_cwd` | **Required for basalt**: the Basalt source-root directory. It must be an absolute path to an existing directory. The `-m serve.server` module resolves from this working directory. |
| `basalt_exe` | **Required for basalt**: the engine binary. It must be an absolute path to an existing file, and the harness passes it as `--engine-bin`. |
| `basalt_parallel` | Optional worker-pool size passed as `--parallel`: a whole number `1..=8`, default `1`. It changes the KV footprint, so the row's declared cost must match it. |
| `basalt_engine_args` | Comma-separated engine flags appended to the launch line; each entry is exactly one word. Long options only (`--flag` or `--flag=value`). The harness refuses a server-owned flag by exact name and says which mechanism owns that decision. |
| `extra_args` | **Closed allow-list, not a denial list.** The harness splits the value on whitespace and appends a word to the argv only when it is a reviewed option: `--embedding`, `--embeddings`, `--pooling <mode>`, `--temp`, `--top-k`, `--top-p`, `--min-p`, `--repeat-penalty`, `--seed`, `--warmup`, `--no-warmup`. The value words a reviewed option consumes are appended too. A value may not be option-shaped, so it cannot smuggle another flag. **Anything else is refused with the launch**, and the message names the word. The model, host, port, gpu-layers, jinja, api-key, lora, and rpc options and every other option are not on the list, so their absence refuses them and there is nothing to dodge. The harness appends its forced `--host 127.0.0.1`, `--port`, and `cpu_offload` `-ngl 0` after these args, and they win. |
| `ninfer_image` | **Required for ninfer**: selects **template mode, the only mode**. The harness renders the canonical autostart script from your yaml, stages it into the distro, verifies it by distro-side checksum, and launches exactly **that** script. Its required companions are `ninfer_model_name` (the directory that holds the artifact), `ninfer_artifact` (the file inside it), and `ninfer_model_roots` (comma-separated absolute WSL directories, searched in order). Each must be a plain word, and the harness refuses a bad value and names the key. `ninfer_internal_port` (the port the container binds inside its own namespace) is optional and defaults to the SE internal number **8808**. That is the harness's 87xx family plus 100: clear of the manual 80xx world, the 87xx listeners, and the 181xx engine plan. It may not name a `protected_ports`, public-listener, or harness engine port. The manual/harness separation covers container-internal numbers too, so the harness refuses a row that borrows one at load, and the rescan names it. |
| `ninfer_serve_cmd` | The server command inside the container (default `ninfer-serve`). |
| `ninfer_gpu_device` | Comma-separated device selector for `--gpus device=` (default `0`). |
| `ninfer_serve_args` | Extra flags appended to the serve line, as shell-bare tokens. Both shipped engine parsers reject the `--flag=value` form, so write each option with a space and put the value in its own token, for example `--spec dflash2`. Option tokens must be long options that start with `--`. The canonical script reserves these options: `--host`, `--port`, `--api-key`, `--data-dir`, `--data-dir-permissions`, `--model`, `--model-id`, `--log-level`, `--log-dir`, `--uds`, `--ssl-keyfile`, `--ssl-certfile`, `--max-context`, `--kv-capacity`, `--max-concurrency`, `--kv-dtype`. The harness refuses each of them by name and by any abbreviation or extension of it, so `--max-cont` and `--host-context-mib` are refused too. Your args can therefore never resize what the cost estimate measured. |
| `ninfer_model_id` | Optional OpenAI-style served id, passed as `--model-id`. |
| `ninfer_env` | Extra container environment as `NAME=VALUE` tokens. This is an **allow-list**, not a parse rule. The only admissible pair today is `CUDA_MODULE_LOADING=EAGER\|LAZY`, the cold-start flag of the NInfer-all cards. The harness refuses an arbitrary name, such as `LD_PRELOAD` or `PATH`, by name. Growing the list needs a code review, never a yaml edit. |
| `ninfer_key_file` | The place where `ninfer_key: true` reads the API key. The path is exactly `$HOME/.simplehar/keys/<file>.key` in the distro, a bare word with no traversal and no subdirectories. The script `cat`s it into `--api-key`, so an arbitrary path would let the yaml export any distro secret. Default: `$HOME/.simplehar/keys/<ninfer_container>.key`. |
| `ninfer_autostart` | **Removed** (ruling 18b, after a green Wave C). The launch path for operator-written autostart scripts no longer exists. A row that declares it is refused at scan and at launch with the migration message, and is never published: declare `ninfer_image`, `ninfer_model_name`, `ninfer_artifact`, and `ninfer_model_roots` (template mode) and delete the `ninfer_autostart` line. |
| `ninfer_container` | **Required for ninfer.** The container name. The harness reaps it with `docker rm -f` on unload, scoped strictly to the name this harness started (claim ledger: `ninfer-claims.json` beside the harness exe). The name must be docker-legal and must not look like a container ID (4+ hex chars). |
| `ninfer_profile` | Template mode: one of the built-in VRAM profiles `a` to `e` (default `b`). The table is the operator's own: `a` 262k ctx/524k kv/2 conc, `b` 262k/262k/1, `c` 131k/131k/1, `d` 16k/32k/2 nvfp4, `e` 64k/64k/1. Or use a custom lowercase name, resolved by a `ninfer_profile_<name> = ctx=... kv=... conc=... kv_dtype=...` line. `kv_dtype` is one of `fp8`/`nvfp4`/`fp16`/`bf16`/`int8`/`rk8v4`, and `kv` must cover `ctx`. The override line applies to custom names only. The built-in table wins for `a` to `e`, so the harness refuses a `ninfer_profile_b`-style line at launch rather than silently ignoring it. |
| `ninfer_key` | A truthy value (`true`/`1`/`yes`/`on`) sends `key` as `$2`; in template mode the script `cat`s the `ninfer_key_file` into `--api-key`. Anything else sends `nokey`. |

The harness renders strata rows; it never hands the row to the engine. At
every load the harness writes the full profile JSON into
`.simplehar\strata-profiles` beside the exe (content-addressed
`<model>-<hash>.json`, atomic write, keep-N prune) and launches only its own
staged copy. See "Strata engines" below.

The harness renders basalt rows the same way. At every load it writes its
`{"server": ...}` profile into `.simplehar\basalt-profiles` beside the exe
and launches only its own staged copy. See "Basalt engines" below.

The harness keeps unknown keys for reference and reports them as
`unknown_key` warnings. The model still loads. `host` and `port` keys belong
to the harness and are inert: the bind is forced to loopback, and the port
comes from the category config. `fit_gb` is the exception: it is the engine
calculator's projection slot (see below). The harness ignores and strips a
hand-written value and never books it. Two reserved keys are stripped
silently, with no warning: `bak` is the forensic backup key, and a retired
`external:` line is consumed and stripped.

**Cost sources** decide which number binds admission, and the strongest
source wins. The sources are an `engine_report` (the ninfer container's own
startup reservation), an `engine_status` figure (a process engine's own status
endpoint: see the Strata and Basalt sections), and, for llama, a **Fit**
projection. A guarded `engine_report` or `engine_status` figure above the
declared `vram_gb` books instead of it; the declared value stays the floor.
When
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

Measurement: after readiness the harness reads `info.vram` from the engine's
own `GET /status` snapshot. It books `expert_cache_mib + reserve_mib` as the
footprint: the bytes the engine took, plus the headroom its fill policy
keeps. The figure carries the `engine_status` stamp. Above the declared
`vram_gb` it books instead of it; the declared value stays the floor. The
harness keeps polling that status while the engine serves, and it re-books
the resident when an expert-cache move crosses the hysteresis band. With no
status answer, the witness sums the bytes of the `strata.exe` engine plus
its direct `strata-vision` child and books the `per_process` tree sum. A
free-memory `total_diff` reading displays and never books. The repo holds no
captured strata status body: the field names are recorded only for the
sibling engine's status snapshot. The card run must capture the real body,
and a differing field set is fixed from that capture, never from a widened
guess.

The external-profile mode is superseded (operator ruling 2026-11-02, row
19): a row that declares `strata_config` is refused at scan. The command
`simplehar-se import strata <profile.json>` reads a manual profile and
**emits** the yaml knob block. Add `--apply <row.yaml>` (operator ruling
2026-11-03) to merge the block into the live row in one atomic write:
strata-owned keys and `serve_model` are replaced; `strata_config` and stale
`strata_*` keys are deleted; `category`, `vram_gb`, `cost`, `load_timeout`,
`launch_only_with_at_least_vram_free`, `context`, `name`, `model`,
`strata_python`, comments, and key order are preserved. A missing,
unreadable, duplicate-key, empty, or non-strata target is refused with a
named reason, and nothing is written. The apply summary counts the row lines
the merge actually changed, not the emitted field count. Without `--apply`,
the command only prints; it never touches the row or the profile file. `C:\Strata` stays the
manual-launch home that the harness never reads on the serve path. The
import CLI is the one sanctioned reader.

### Basalt engines

Basalt models (`engine: basalt`) run the Basalt Python server directly. The
harness launches `python -m serve.server` in `basalt_cwd`. It never uses the
source-tree launcher. The row names the three executed paths: the
interpreter (`basalt_python`), the source root (`basalt_cwd`), and the
engine binary (`basalt_exe`). `serve_model` is the served alias: it is
passed as `--alias` and verified at readiness. The child receives
`USERPROFILE` and `HOME` from the harness for Basalt's model cache; no
other parent environment passes.

The harness renders the profile itself. The JSON carries one `server`
object with three figures only: the `alias`, the absolute `.basalt` model
path, and `parallel`. The render is staged under `.simplehar\basalt-profiles`
beside the exe (content-addressed `<model>-<hash>.json`, atomic write,
keep-N prune). The spawn rewrites `--config` to exactly that file, so the
child can never point at another profile. The profile carries no `host`,
`port`, `api-key`, `gpu`, or `engine-bin`: those decisions ride the forced
argv (`--host 127.0.0.1`, the harness-assigned `--port`, the staged
`--config`), and the adapter supplies `--engine-bin`, `--alias`, `--gpu 0`,
and `--parallel`. `basalt_engine_args` entries append after them, and the
server validates each word against the engine binary before load.

Readiness needs `/health` 200 with the JSON `loaded` flag not being `false`,
because Basalt answers HTTP while weights stream. Then `/v1/models` must
report the `serve_model` alias. For the 482B class, `load_timeout: 1200` is
the recommended starting point for the load floor. A basalt engine starts
only when you load it; booting the harness starts nothing.

Cost starts as the declared `vram_gb` with `cost: fixed`. Both are required:
a `.basalt` pack's bytes never feed the GGUF size estimate, and the engine
offers no Fit projection. After readiness the harness reads
`engine.info.vram` from the engine's own `GET /v1/status` snapshot. It books
`expert_cache_mib + reserve_mib` as the footprint: the bytes the engine took,
plus the slab its reserve policy keeps. The figure carries the
`engine_status` stamp. Above the declared `vram_gb` it books instead of it;
the declared value stays the floor. The harness keeps polling that status
while the engine serves, and it re-books the resident when an expert-cache
move crosses the hysteresis band. With no status answer, the witness sums
the bytes of the engine's own process tree - the Python server plus its
direct children - and books the `per_process` tree sum. The Python parent's
per-process figure alone never books. A free-memory `total_diff` reading
displays and never books. The evidence holds no raw status body; the card
run must capture the real one, and a differing field set is fixed from that
capture. Set `launch_only_with_at_least_vram_free` to the declared cost:
Basalt needs the card, and a loaded stranger must refuse before spawn.

Kill: the harness job object terminates the Python server. Basalt's server
puts the engine child in its own kill-on-close job, so a Python death also
terminates the engine and the crash path cannot orphan it. A card run must
prove VRAM release and no orphan.

Example row (placeholders, not machine facts):

```yaml
model: <models-dir>\basalt\model-482b.basalt
engine: basalt
serve_model: basalt-482b
category: WORK
vram_gb: 40
cost: fixed
launch_only_with_at_least_vram_free: 40
load_timeout: 1200
basalt_python: <basalt-dir>\.venv\Scripts\python.exe
basalt_cwd: <basalt-dir>
basalt_exe: <basalt-dir>\bin\basalt.exe
basalt_parallel: 1
basalt_engine_args: --vram-reserve-mib, 700
```

The `vram_gb` figure and its floor here are illustrative only. Basalt fills
the card's free VRAM up to its reserve policy, so the row must declare the
ceiling you intend. Measure your current card and one witnessed serve, then
write the number; do not copy a figure from a note.

### Ninfer engines

Ninfer models (`engine: ninfer`) are GPU containers in WSL2, and they run in
**template mode, the only mode** (the row declares `ninfer_image`). The
harness launches them as a short-lived
`wsl.exe` client. The container outlives the client, so unload reaps it by
name (`docker rm -f <ninfer_container>`), and the release evidence is the port
closing, not the client exit. The client runs the harness's own canonical
script. The harness renders it from your yaml, stages it base64
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
shown, never booked. The client then waits for `/health` 200 and then for
`/v1/models` 200. The harness
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
(`CANONICAL_NINFER_SCRIPT`), and the launch renders and stages that
template. You never edit it. A readable copy ships as
[ninfer-autostart-template.sh](../../ninfer-autostart-template.sh) at the
repository root, beside `README.md` (and beside the exe in the release
package). Read that one file to
see exactly what a launch does. The harness invokes the staged script inside
WSL as `bash <staged-script> <profile> <key|nokey> <port>`: the profile and
key choice are already rendered into the script, and `$3` is the **host**
publish port the harness assigned. The script demands it: a missing `$3`
fails the launch instead of borrowing another number. The script exits once
the container is starting, and the
harness then polls `/health` and `/v1/models` itself. The rendered container
name is the model's `ninfer_container`.

Notes:

- `--publish 127.0.0.1:...` keeps the container loopback-only. The rendered
  launch never writes anything else; a wider publish would expose an
  unauthenticated engine to your LAN, and only the `key` mode (with a real
  key file under `$HOME/.simplehar/keys/`) would make that safe.
- `--restart no` is not a preference. A `--restart` policy revives
  containers on WSL boot outside any harness decision, with no ledger entry
  and no reap path (the NINFER-8004 incident). The rendered script hard-codes
  it.
- R10 namespaces: the container's internal port is a **third namespace**,
  disjoint from the manual (`protected_ports`) and harness (listeners,
  engine plan) sets. The rendered launch binds `ninfer_internal_port`
  (default **8808**, the SE internal number). The harness refuses a row that
  borrows a protected, listener, or engine-plan number at load, and the
  rescan names it.
- The name-scoped `docker rm -f` before `docker run` makes the launch
  idempotent, and it complements the harness's orphan reconciliation.
- The `--log-driver json-file --log-opt` pair bounds container logs.
  Template mode always sets them, so a week of a serving engine never fills
  the distro.
- `serve_model` in the model yaml must match `--model-id`, or the harness
  records a served-id warning.
- The staged script needs no `chmod` for the harness: it is always run with
  `bash`, so the shebang is documentation.
