//! The `EnginePolicy` table (DESIGN §20-9, Fork C; chunk 10 slice 10a).
//!
//! Doctrine: *"external engines obey only harness-owned configuration"*. This is
//! the single-source, compiled-in data table behind the L1/L2/L3 layering — one
//! row per engine (`mock`, `llama`, `strata`, `ninfer`), "one mechanism, four
//! instances". Built as L3's **render+scrub contract**, not a validator: the
//! row says what the harness always writes (`forced_fields`) and what may never
//! be admitted from L1 (`forbidden_words`); the per-engine renderers read the
//! row instead of hiding the same facts in code. The validator falls out as the
//! degenerate case (a hostile L1 renders to a refusal, so its words can never
//! appear in L3).
//!
//! Cell rule (10a): a cell exists only where a real row populates it AND a
//! reader consumes it — `serve_template` for ninfer, `allow_words` and the
//! absence-refused `forbidden_words` for llama, `path_markers` for the runtime's
//! early-exit diagnosis, `custody_data_roots` for the ninfer key file. Cells
//! the §20-9 ruling listed but today's code holds no per-engine fact for
//! (`stop_grace`, `cost_default`) are absent, not stubbed. The custody
//! enforcement the later slices owe (config wiring, strata sidecar, root
//! admission) lands on the populated custody cells without moving the data.

use super::llama::LLAMA_ENGINE_HOST;

/// The in-distro directory a harness-rendered autostart script is staged into.
/// Deliberately outside every model directory and outside the operator's script
/// tree: a staged file can never shadow or be confused with a hand-written one
/// (template adoption, operator ruling 2026-10-31). This is the ninfer row's
/// executed-path custody root: the location the harness itself writes and runs.
pub(crate) const NINFER_STAGED_SCRIPTS_ROOT: &str = ".simplehar/autostart";

/// The one directory a ninfer key file may live in, as the distro shell sees it
/// (`$HOME` expands in the assignment; `~` never does). The canonical script
/// `cat`s this file into `--api-key`, so an arbitrary path would let a yaml
/// export any distro secret (`.ssh/id_rsa`) into `docker run` argv — the key
/// lives in exactly one custody root, `<root>/<file>.key`, with `<file>` a
/// single bare word (chunk-10 custody doctrine: engines obey only
/// harness-owned locations).
pub(crate) const NINFER_KEYS_ROOT: &str = "$HOME/.simplehar/keys";

/// The Windows directory harness-rendered strata profiles stage into, as the
/// tail below the executable's own directory — beside the strata custody
/// sidecar (operator ruling 2026-11-02 row 19). Like the ninfer staged-scripts
/// root, it is outside every engine home: the rendered copy can never shadow or
/// be confused with a manual `C:\Strata` profile, and the spawn rewrites
/// `--config` to exactly the staged file, so the child reads only what the
/// harness wrote. `process::strata_profiles_dir` resolves it to an absolute
/// path from the single source that this cell spells.
pub(crate) const STRATA_RENDERED_PROFILES_ROOT: &str = ".simplehar/strata-profiles";

/// The Windows directory harness-rendered basalt profiles stage into, as the
/// tail below the executable's own directory — the same exe-adjacent custody
/// posture as [`STRATA_RENDERED_PROFILES_ROOT`], one directory per engine so a
/// basalt launch can never stage into strata's pool or the reverse
/// (`TASK-BASALT` stage 1; `process::rendered_profiles_dir` resolves it).
pub(crate) const BASALT_RENDERED_PROFILES_ROOT: &str = ".simplehar/basalt-profiles";

/// The CLOSED allow-list of `extra_args` option words an operator may pass
/// through to llama-server (FINDINGS2-0910 [1]): the inversion of the old
/// `HARNESS_OWNED_FLAGS` denial list, which could only ever name ten of the
/// pinned build's ~200 options and therefore left the harness-owned decisions —
/// which weights serve, the bind host/port, the key, the transport, the served
/// static roots, egress and port reuse — reachable from a single yaml line. A
/// word passes iff it names one of these entries (or, for a value-taking
/// entry, that spelling with `=value` appended); the entry's `arity` following
/// words are its values and pass verbatim as data. Everything else is refused,
/// so the parser is never reached from the catalog.
///
/// Adding a word here is a review decision, and the entry must state which
/// harness-owned decision it cannot move. The set is admitted because every
/// entry is (a) present in the pinned build (checked against the
/// `llama-common.dll` option string table, so an admitted word is one the
/// engine actually parses) and (b) unable to choose weights, bind address,
/// port, authentication, TLS, static roots, download/RPC egress,
/// `--reuse-port`, or to flip a flag the harness itself emits. Deliberately
/// absent — reviewed and refused, along with everything else:
/// `-m`/`--model`/`--model-url`/`--hf-repo`/`--hf-file`/`--docker-repo` (which
/// weights serve), `--models-dir`/`--models-preset`/`--models-autoload` (an
/// engine-side router file), `--mmproj` (a second weights file),
/// `--host`/`--port` and `-ngl` with all its alias spellings (harness-forced),
/// `--api-key*`, `--ssl-*`, `--rpc`, `--path`, `--reuse-port`, `--lora*`,
/// `--control-vector*`, `--override-kv` and `--jinja`/`--no-jinja` (each moves
/// a harness-owned decision or rewrites the template the harness would have
/// emitted).
pub(crate) const LLAMA_EXTRA_ARGS: &[(&str, usize)] = &[
    // Reply shape only: serves embeddings instead of chat. No weights, bind, key,
    // root or egress choice; `/health` still answers, so readiness is unaffected.
    // `--embedding` is the pinned build's legacy alias of the canonical
    // `--embeddings`, and it is the spelling the live catalog uses
    // (ModelConf/embeddinggemma-2-GGUF.yaml).
    ("--embedding", 0),
    ("--embeddings", 0),
    // How an embeddings reply is pooled; the value is a mode name, not a path.
    ("--pooling", 1),
    // Sampling defaults: token choice within the loaded model. Cannot select a
    // model, a bind, a key or a root, and cannot affect `/health`.
    ("--temp", 1),
    ("--top-k", 1),
    ("--top-p", 1),
    ("--min-p", 1),
    ("--repeat-penalty", 1),
    ("--seed", 1),
    // Pre-tokenization timing: moves work between startup and the first request,
    // so it can only ever shift when `/health` turns up, never what it serves;
    // `load_timeout` remains the harness's own deadline either way.
    ("--warmup", 0),
    ("--no-warmup", 0),
];

/// The llama `extra_args` words that must never reach the argv from L1. The
/// enforcement is absence from [`LLAMA_EXTRA_ARGS`], not a name match — this
/// list is the reviewed record of that complement, and the policy acceptance
/// test feeds each word back through `extra_args` to prove the closed set
/// still refuses it (the table cross-checks itself, so neither list can drift
/// into lying about the other). Every spelling that moves a harness-owned
/// decision: weights, bind, offload depth, key/TLS/egress, the jinja pair the
/// harness emits itself, and each `FLAG_TABLE` decision (one yaml key per
/// decision; a second spelling would make that key a dead claim).
pub(crate) const LLAMA_FORBIDDEN_EXTRA_WORDS: &[&str] = &[
    "-m",
    "--model",
    "--host",
    "--port",
    "-ngl",
    "--n-gpu-layers",
    "--gpu-layers",
    "-h",
    "-p",
    "--api-key",
    "--api-key-file",
    "--rpc",
    "--path",
    "--models-dir",
    "--models-preset",
    "--models-autoload",
    "--ssl-key-file",
    "--ssl-cert-file",
    "--model-url",
    "--hf-repo",
    "--hf-file",
    "--docker-repo",
    "--reuse-port",
    "--jinja",
    "--no-jinja",
    "--lora",
    "--override-kv",
    "--mmproj",
    "-c",
    "-b",
    "--ubatch-size",
    "--flash-attn",
    "--cache-type-k",
    "--cache-type-v",
    "--parallel",
    "--fit",
    "--fit-target",
    "--fit-ctx",
];

/// The ninfer serve flags a yaml may never contribute: the ones the canonical
/// script owns for the network bind, the key, the container's lifecycle/log
/// discipline (the incident-review trio plus the auth path), and the capacity
/// figures the VRAM booking is computed from — a serve arg that resized the
/// context or KV pool would silently invalidate every cost estimate the
/// scheduler filed from this model. `ninfer_serve_args` rides LAST in the
/// launch line, and a later duplicate overrides an earlier one — so these
/// names are refused outright, not merely discouraged, and the refusal names
/// the flag.
pub(crate) const NINFER_FORBIDDEN_SERVE_FLAGS: &[&str] = &[
    "--host",
    "--port",
    "--api-key",
    "--data-dir",
    "--data-dir-permissions",
    "--model",
    "--model-id",
    "--log-level",
    "--log-dir",
    "--uds",
    "--ssl-keyfile",
    "--ssl-certfile",
    "--max-context",
    "--kv-capacity",
    "--max-concurrency",
    "--kv-dtype",
];

/// Which harness mechanism owns each flag a refused `ninfer_serve_args` word
/// names (slice 10c): the refusal must tell the operator where the decision
/// actually lives, not only that the word is refused. One entry per
/// [`NINFER_FORBIDDEN_SERVE_FLAGS`] member (the table test fails on drift);
/// abbreviation matches resolve through the same table, so `--max-cont` names
/// the profile override just like the full spelling.
pub(crate) const NINFER_SERVE_ARG_OWNERS: &[(&str, &str)] = &[
    (
        "--host",
        "the canonical script binds loopback itself (\u{a7}5)",
    ),
    (
        "--port",
        "the port is harness-assigned from category config",
    ),
    ("--api-key", "the key rides `ninfer_key`/`ninfer_key_file`"),
    (
        "--data-dir",
        "container lifecycle is the canonical script's",
    ),
    (
        "--data-dir-permissions",
        "container lifecycle is the canonical script's",
    ),
    (
        "--model",
        "`ninfer_model_name`/`ninfer_artifact` name the model",
    ),
    ("--model-id", "`ninfer_model_id` owns the served id"),
    (
        "--log-level",
        "log discipline is fixed in the canonical script",
    ),
    (
        "--log-dir",
        "log discipline is fixed in the canonical script",
    ),
    ("--uds", "the socket path is the canonical script's"),
    ("--ssl-keyfile", "TLS is harness-owned (loopback only)"),
    ("--ssl-certfile", "TLS is harness-owned (loopback only)"),
    (
        "--max-context",
        "the `ninfer_profile` line owns the capacity figures the VRAM booking is computed from",
    ),
    (
        "--kv-capacity",
        "the `ninfer_profile` line owns the capacity figures the VRAM booking is computed from",
    ),
    (
        "--max-concurrency",
        "the `ninfer_profile` line owns the capacity figures the VRAM booking is computed from",
    ),
    (
        "--kv-dtype",
        "the `ninfer_profile` line owns the capacity figures the VRAM booking is computed from",
    ),
];

/// The denylist match a serve-args token fails: the token's *name* collides
/// with a forbidden flag itself or as a unique abbreviation of one (argparse
/// parsers resolve prefixes, so `--max-cont` is `--max-context` to the engine).
/// `None` = the name is not harness-owned.
#[must_use]
pub(crate) fn forbidden_serve_flag(name: &str) -> Option<&'static str> {
    NINFER_FORBIDDEN_SERVE_FLAGS
        .iter()
        .copied()
        .find(|flag| name.starts_with(flag) || flag.starts_with(name))
}

/// Where the decision behind a refused serve-args flag actually lives.
#[must_use]
pub(crate) fn ninfer_serve_arg_owner(flag: &str) -> Option<&'static str> {
    NINFER_SERVE_ARG_OWNERS
        .iter()
        .find(|(name, _)| *name == flag)
        .map(|(_, owner)| *owner)
}

/// The basalt `serve.server` flags a yaml may never contribute through
/// `basalt_engine_args`: the ones the harness writes itself (the bind, the
/// slot port, the staged profile, the weights, the engine binary, the alias,
/// the pool size, the interpreter, the key), the load/unload and memory-budget
/// decisions the scheduler and the witness own, and the request-time/CORS/
/// logging surface the harness keeps off the launch line. Unlike ninfer, the
/// match is EXACT on the option name: the upstream parser runs
/// `allow_abbrev=False`, so a prefix is an unknown option to the server, never
/// an alias (the abbreviation door is closed only where a parser opens it).
pub(crate) const BASALT_FORBIDDEN_SERVER_FLAGS: &[&str] = &[
    "--model",
    "--engine-bin",
    "--alias",
    "--parallel",
    "--host",
    "--port",
    "--config",
    "--gpu",
    "--python",
    "--api-key",
    "--mock-engine",
    "--script",
    "--slot-full-context",
    "--vision",
    "--tokenizer",
    "--open",
    "--fit-max-tokens",
    "--grammar",
    "--no-grammar",
    "--allowed-host",
    "--cors-origin",
    "--debug",
    "--request-lines",
    "--lazy",
    "--idle-unload",
    "--idle-sleep",
    "--temperature",
    "--top-p",
    "--top-k",
    "--min-p",
    "--presence-penalty",
    "--frequency-penalty",
    "--repetition-penalty",
    "--penalty-last-n",
    "--seed",
    "--min-free-vram-mib",
    "--reasoning-budget-tokens",
    "--thinking-loop-tokens",
    "--answer-room",
    "--no-answer-room",
];

/// Which harness mechanism owns each flag a refused `basalt_engine_args` word
/// names: the refusal tells the operator where the decision lives, one entry
/// per [`BASALT_FORBIDDEN_SERVER_FLAGS`] member (the table test fails on drift).
pub(crate) const BASALT_SERVER_ARG_OWNERS: &[(&str, &str)] = &[
    ("--model", "`model:` names the weights"),
    ("--engine-bin", "`basalt_exe:` names the engine binary"),
    ("--alias", "`serve_model:` owns the served id"),
    ("--parallel", "`basalt_parallel:` owns the worker pool"),
    ("--host", "the bind is harness-forced (loopback, \u{a7}5)"),
    (
        "--port",
        "the port is harness-assigned from category config",
    ),
    (
        "--config",
        "the profile is the harness-rendered file staged under the basalt custody root",
    ),
    (
        "--gpu",
        "GPU placement is the harness witness's (one GPU, \u{a7}5)",
    ),
    ("--python", "`basalt_python:` names the interpreter"),
    ("--api-key", "request authentication is harness-owned"),
    (
        "--mock-engine",
        "the launched program is the harness's choice",
    ),
    ("--script", "the launched program is the harness's choice"),
    (
        "--slot-full-context",
        "the served context is a harness capacity decision",
    ),
    ("--vision", "vision files are a harness decision, not argv"),
    (
        "--tokenizer",
        "the tokenizer rides the weights the harness names",
    ),
    (
        "--open",
        "a serving engine never opens a browser on the operator's machine",
    ),
    (
        "--fit-max-tokens",
        "the memory budget is the harness witness's (\u{a7}5)",
    ),
    (
        "--grammar",
        "grammar enforcement is a request-time decision, not launch argv",
    ),
    (
        "--no-grammar",
        "grammar enforcement is a request-time decision, not launch argv",
    ),
    (
        "--allowed-host",
        "bind and host-header policy is harness-forced (\u{a7}5)",
    ),
    (
        "--cors-origin",
        "CORS policy is harness-owned (loopback only)",
    ),
    ("--debug", "log discipline is harness-owned"),
    ("--request-lines", "log discipline is harness-owned"),
    ("--lazy", "load timing is the harness scheduler's decision"),
    (
        "--idle-unload",
        "unload timing is the harness scheduler's decision",
    ),
    (
        "--idle-sleep",
        "unload timing is the harness scheduler's decision",
    ),
    (
        "--temperature",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--top-p",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--top-k",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--min-p",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--presence-penalty",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--frequency-penalty",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--repetition-penalty",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--penalty-last-n",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--seed",
        "sampling defaults are request-time values, not launch argv",
    ),
    (
        "--min-free-vram-mib",
        "the memory budget is the harness witness's (\u{a7}5)",
    ),
    (
        "--reasoning-budget-tokens",
        "generation budgets are request-time values, not launch argv",
    ),
    (
        "--thinking-loop-tokens",
        "generation budgets are request-time values, not launch argv",
    ),
    (
        "--answer-room",
        "generation budgets are request-time values, not launch argv",
    ),
    (
        "--no-answer-room",
        "generation budgets are request-time values, not launch argv",
    ),
];

/// The denylist match a `basalt_engine_args` option name fails: EXACT equality
/// with a server-owned flag (the upstream `allow_abbrev=False` parser resolves
/// no prefixes, so no abbreviation door exists to close). `None` = the name is
/// not harness-owned.
#[must_use]
pub(crate) fn forbidden_basalt_server_flag(name: &str) -> Option<&'static str> {
    BASALT_FORBIDDEN_SERVER_FLAGS
        .iter()
        .copied()
        .find(|flag| *flag == name)
}

/// Where the decision behind a refused `basalt_engine_args` flag actually lives.
#[must_use]
pub(crate) fn basalt_server_arg_owner(flag: &str) -> Option<&'static str> {
    BASALT_SERVER_ARG_OWNERS
        .iter()
        .find(|(name, _)| *name == flag)
        .map(|(_, owner)| *owner)
}

/// The harness mechanism that owns each decision a refused `extra_args` word
/// moves (slice 10c): the refusal names the yaml key or forced line that owns
/// it, so the operator edits the live key instead of fighting the closed list.
/// One entry per [`LLAMA_FORBIDDEN_EXTRA_WORDS`] member (the table test fails
/// on drift); unknown words have no owner and keep the plain list refusal.
pub(crate) const LLAMA_EXTRA_ARG_OWNERS: &[(&str, &str)] = &[
    ("-m", "`model:` names the weights"),
    ("--model", "`model:` names the weights"),
    ("--host", "the bind is harness-forced (loopback, \u{a7}5)"),
    (
        "--port",
        "the port is harness-assigned from category config",
    ),
    ("-ngl", "`cpu_offload:` is the harness's offload decision"),
    (
        "--n-gpu-layers",
        "`cpu_offload:` is the harness's offload decision",
    ),
    (
        "--gpu-layers",
        "`cpu_offload:` is the harness's offload decision",
    ),
    (
        "-h",
        "no harness key carries it (this build's `-h` is help)",
    ),
    (
        "-p",
        "no harness key carries it (not an option in this build)",
    ),
    ("--api-key", "request authentication is harness-owned"),
    ("--api-key-file", "request authentication is harness-owned"),
    ("--rpc", "RPC egress is harness-owned"),
    ("--path", "static roots are harness-owned"),
    (
        "--models-dir",
        "an engine-side model router is not catalog custody",
    ),
    (
        "--models-preset",
        "an engine-side model router is not catalog custody",
    ),
    (
        "--models-autoload",
        "an engine-side model router is not catalog custody",
    ),
    ("--ssl-key-file", "TLS is harness-owned (loopback only)"),
    ("--ssl-cert-file", "TLS is harness-owned (loopback only)"),
    (
        "--model-url",
        "download egress is harness-owned; `model:` names the weights",
    ),
    (
        "--hf-repo",
        "download egress is harness-owned; `model:` names the weights",
    ),
    (
        "--hf-file",
        "download egress is harness-owned; `model:` names the weights",
    ),
    (
        "--docker-repo",
        "download egress is harness-owned; `model:` names the weights",
    ),
    ("--reuse-port", "port reuse is harness-owned"),
    ("--jinja", "`jinja:` is the harness's template decision"),
    ("--no-jinja", "`jinja:` is the harness's template decision"),
    ("--lora", "adapter loading is harness-owned"),
    ("--override-kv", "the emitted KV values are harness-owned"),
    (
        "--mmproj",
        "`mmproj:` is the harness's vision-file decision",
    ),
    ("-c", "`context:` owns the context size"),
    ("-b", "`batch:` owns the batch size"),
    ("--ubatch-size", "`ubatch:` owns the micro-batch size"),
    ("--flash-attn", "`flash_attn:` owns the attention kernel"),
    ("--cache-type-k", "`cache_type_k:` owns the K-cache type"),
    ("--cache-type-v", "`cache_type_v:` owns the V-cache type"),
    ("--parallel", "`parallel:` owns slot concurrency"),
    ("--fit", "`fit:` owns the auto-fit switch"),
    ("--fit-target", "`fit_target:` owns the fit target"),
    ("--fit-ctx", "`fit_ctx:` owns the fit context"),
];

/// Where the decision behind a refused `extra_args` word actually lives.
#[must_use]
pub(crate) fn llama_extra_arg_owner(word: &str) -> Option<&'static str> {
    let name = word.split_once('=').map_or(word, |(name, _)| name);
    LLAMA_EXTRA_ARG_OWNERS
        .iter()
        .find(|(flag, _)| *flag == name)
        .map(|(_, owner)| *owner)
}

/// The yaml keys this engine's harness-side reads consume: the knob table the
/// §20-9 Fork C scoping ruling ends with — the row that `OFFLOAD_ENGINES` was
/// the first cell of (2026-10-28) now owns the whole fact, siblings included.
/// `cpu_offload` on a row without the knob warns via `InertKey` (published,
/// never refused — ruling D stands); every other member of the table is scoped
/// the same way: a key that is live config for a *different* engine's row is
/// inert here, and the page must be able to say so. Harness keys (`category`,
/// `vram_gb`, the `measured_*` triple, ...) are engine-blind reads and belong
/// to no row.
const LLAMA_KNOBS: &[&str] = &[
    "cpu_offload",
    "jinja",
    "extra_args",
    "mmproj",
    "context",
    "ngl",
    "batch",
    "ubatch",
    "flash_attn",
    "cache_type_k",
    "cache_type_v",
    "parallel",
    "fit",
    "fit_target",
    "fit_ctx",
];

const STRATA_KNOBS: &[&str] = &[
    "strata_config",
    "strata_python",
    "strata_cwd",
    // Template mode (operator ruling 2026-11-02 row 19): the row yaml owns the
    // profile's fields and the harness renders the JSON at every load.
    "strata_exe",
    "strata_args",
    "strata_tokenizer",
    "strata_lib_dirs",
    "strata_env",
    "strata_gpu",
    "strata_parallel",
    "strata_gpus_asked",
    "strata_vision",
    "strata_sampling",
    "strata_uncensored",
];

const BASALT_KNOBS: &[&str] = &[
    "basalt_python",
    "basalt_cwd",
    "basalt_exe",
    "basalt_parallel",
    "basalt_engine_args",
];

const NINFER_KNOBS: &[&str] = &[
    "ninfer_autostart",
    "ninfer_container",
    "ninfer_profile",
    "ninfer_key",
    "ninfer_image",
    "ninfer_model_name",
    "ninfer_artifact",
    "ninfer_internal_port",
    "ninfer_model_roots",
    "ninfer_serve_args",
    "ninfer_gpu_device",
    "ninfer_model_id",
    "ninfer_key_file",
    "ninfer_serve_cmd",
    "ninfer_env",
];

/// Whether the row for `engine` reads yaml key `key` at all. An unknown engine
/// id (no row) reads nothing — the actor's `EngineUnavailable` drop is its
/// report, and no knob claim on it is judged.
#[must_use]
pub(crate) fn knob_live(key: &str, engine: &str) -> bool {
    engine_policy(engine).is_some_and(|row| row.knobs.contains(&key))
}

/// The engine ids whose rows read `key` — the owner list the inert-key hint
/// names ("read only by the llama engine(s)").
#[must_use]
pub(crate) fn knob_readers(key: &str) -> Vec<&'static str> {
    POLICIES
        .iter()
        .filter(|row| row.knobs.contains(&key))
        .map(|row| row.id)
        .collect()
}

/// Whether the row for `engine` may book from the R11 Fit projection (the
/// llama row's `supports_fit` cell is the single answer — the ladder and the
/// warm queue ask this instead of matching engine ids). An unknown engine id
/// (no row) is never fit-capable.
#[must_use]
pub(crate) fn fit_capable(engine: &str) -> bool {
    engine_policy(engine).is_some_and(|row| row.supports_fit)
}

/// The canonical ninfer autostart script the harness renders and stages for
/// every ninfer launch (operator ruling 2026-10-31: "we create a valid template
/// we can control and adopt for each ninfer start"). The staged copy is what
/// runs — the operator's hand-written scripts are no longer on the launch path
/// — so the launch is exactly what [`crate::process`] staged and verified.
/// Placeholders are filled by `ninfer::render_canonical` from validated yaml
/// values; nothing here reads the network, the catalog, or the harness
/// environment.
///
/// The three properties the incident review demanded of every launch
/// (`NINFER-8004-INCIDENT-2026-10-31`): `--restart no` (nothing revives on WSL
/// boot without the harness deciding), loopback-only publish (the served port
/// never leaves the machine's loopback interface, §5), and bounded container
/// logs (`json-file`, 25 MiB × 5 — a week of a serving engine never fills the
/// distro). The pre-reap of our own name makes `docker run` idempotent, and the
/// final echo is the machine-readable launch stamp the harness's readiness
/// forensics read.
pub(crate) const CANONICAL_NINFER_SCRIPT: &str = r#"#!/usr/bin/env bash
# simpleharn canonical ninfer autostart — rendered by the harness from its embedded
# template; staged content-addressed and launched INSTEAD of any operator script.
# Do not edit the staged copy: it is rewritten from the model yaml before every load.
# Profile {{PROFILE}} ({{PROFILE_SOURCE}}): ctx={{MAX_CTX}} kv={{KV_CAP}} conc={{CONC}} kv_dtype={{KV_DTYPE}}
set -euo pipefail

NAME="{{NAME}}"
IMAGE="{{IMAGE}}"
MODEL_NAME="{{MODEL_NAME}}"
ARTIFACT="{{ARTIFACT}}"
PORT="{{PORT}}"
SERVE_CMD="{{SERVE_CMD}}"
MODEL_ROOTS=(
{{MODEL_ROOTS}}
)
SERVE_ARGS=({{SERVE_ARGS}})
API_KEY_FILE="{{KEY_FILE}}"
WITH_KEY="{{KEY}}"

MODEL_DIR=""
for root in "${MODEL_ROOTS[@]}"; do
  if [[ -f "$root/$MODEL_NAME/$ARTIFACT" ]]; then MODEL_DIR="$root/$MODEL_NAME"; break; fi
done
if [[ -z "$MODEL_DIR" ]]; then
  echo "autostart: no artifact '$ARTIFACT' for model '$MODEL_NAME' under ${MODEL_ROOTS[*]}" >&2
  exit 1
fi

PUBLISH="${3:?harness must pass the publish port as arg 3}"

API_KEY_ARGS=()
if [[ "$WITH_KEY" == "key" ]]; then
  if [[ ! -s "$API_KEY_FILE" ]]; then
    echo "autostart: '$NAME' requires an API key but '$API_KEY_FILE' is missing or empty" >&2
    exit 1
  fi
  API_KEY_ARGS=(--api-key "$(cat "$API_KEY_FILE")")
fi

docker rm -f "$NAME" >/dev/null 2>&1 || true

docker run -d --name "$NAME" \
  --gpus '"device={{GPU_DEVICE}}"' \
  --restart no \
  --publish "127.0.0.1:${PUBLISH}:${PORT}" \
  --volume "$MODEL_DIR:/models:ro" \
  --log-driver json-file --log-opt max-size=25m --log-opt max-file=5 \
  {{ENV_ARGS}}"$IMAGE" \
  "$SERVE_CMD" "/models/$ARTIFACT" \
  --host 0.0.0.0 \
  --port "$PORT" \
  --max-context "{{MAX_CTX}}" \
  --kv-capacity "{{KV_CAP}}" \
  --max-concurrency "{{CONC}}" \
  --kv-dtype "{{KV_DTYPE}}" \
  ${API_KEY_ARGS[@]+"${API_KEY_ARGS[@]}"} \
  {{MODEL_ID_ARG}}${SERVE_ARGS[@]+"${SERVE_ARGS[@]}"}

echo "autostart: '$NAME' profile '{{PROFILE}}' (ctx={{MAX_CTX}} kv={{KV_CAP}} conc={{CONC}} kvdtype={{KV_DTYPE}} key=$([[ "$WITH_KEY" == key ]] && echo on || echo off)) on port $PUBLISH->$PORT"
"#;

/// One cell of a forced launch token's value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ForcedValue {
    /// A harness literal written verbatim into every launch.
    Literal(&'static str),
    /// The harness-assigned slot port, interpolated at render time.
    SlotPort,
    /// A validated operator path the renderer inserts at this exact position
    /// (the strata profile); the renderer refuses a row rendered without it.
    OperatorData,
}

/// One flag the harness always writes into the engine's argv, and where its
/// value comes from. Rendered in table order by [`render_forced`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ForcedField {
    /// The flag token, e.g. `--host`.
    pub flag: &'static str,
    /// The value source for this flag.
    pub value: ForcedValue,
}

/// The row's operator-extra-words surface: the yaml key the words ride and the
/// row's own refusal voice. Slice 10c (full `extra_args` enforcement): the
/// scan now runs this check when it validates the file, so a hostile word
/// refuses the file at rescan — naming the word and the mechanism that owns
/// the decision — instead of waiting to fail at spawn, after admission may
/// already have evicted a resident for the doomed launch. The adapter keeps
/// running the same check at render (defense in depth: a hand-built entry
/// meets it too), so the L3 guarantee the 10a acceptance test pins stands.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ExtraWordsContract {
    /// The yaml key carrying the operator's extra words.
    pub yaml_key: &'static str,
    /// The engine's refusal voice over one raw value: tokenizes exactly like
    /// the render path tokenizes, and returns the same typed rejection.
    pub check: fn(engine: &str, model: &str, value: &str) -> Result<(), super::EngineError>,
}

// Equality is the key alone: fn-pointer addresses are not guaranteed unique
// across comparisons, so deriving PartialEq over the cell would pin a lie
// (the compiler says so). Two contracts with the same key are one surface.
impl PartialEq for ExtraWordsContract {
    fn eq(&self, other: &Self) -> bool {
        self.yaml_key == other.yaml_key
    }
}

impl Eq for ExtraWordsContract {}

/// One engine's row: every compiled-in fact the harness owns about it. Rows
/// carry only facts real code reads (10a cell rule); the §20-9 cells whose
/// per-engine fact does not exist in the code yet (`stop_grace`, a per-engine
/// `cost_default`) are not stubbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EnginePolicy {
    /// The `engine:` id this row serves.
    pub id: &'static str,
    /// Flags the harness always writes, in argv order; [`render_forced`] emits
    /// them. Ninfer's forces live inside [`CANONICAL_NINFER_SCRIPT`] itself
    /// (the bind, port and capacity lines the template hard-codes), so its row
    /// carries no argv-level cells.
    pub forced_fields: &'static [ForcedField],
    /// Words never admitted from L1. Enforced: the ninfer serve-args prefix
    /// check; recorded (enforced by absence through [`Self::allow_words`]):
    /// the llama `extra_args` complement. Consumed adversarially by the policy
    /// acceptance test, which proves each word absent from a rendered L3.
    pub forbidden_words: &'static [&'static str],
    /// The closed allow-list for an engine's operator-extra words; the closed
    /// set is the enforcement and emptiness means "no extra-args surface".
    /// Read by the llama adapter for `extra_args`.
    pub allow_words: &'static [(&'static str, usize)],
    /// The operator-extra-words surface the scan enforces at validate time
    /// (slice 10c); `None` = the engine admits no operator word at all, so
    /// there is nothing to refuse before render.
    pub extra_words: Option<ExtraWordsContract>,
    /// The yaml keys this engine's harness-side reads consume (the knob table;
    /// see [`LLAMA_KNOBS`]). Drives the `InertKey` sibling-hint: a consumed key
    /// owned by a different row warns here and publishes.
    pub knobs: &'static [&'static str],
    /// The argv flags that mark a model/profile file for the runtime's
    /// early-exit diagnosis (llama `-m`, strata `--config`); the union feeds
    /// `process::early_exit_report`.
    pub path_markers: &'static [&'static str],
    /// Custody root class (a): executed paths — locations the harness renders,
    /// stages and runs. Empty = open (no root admission yet; admission is the
    /// later custody slice).
    pub custody_exec_roots: &'static [&'static str],
    /// Custody root class (b): data paths the engine reads at launch. The
    /// ninfer row's single key root is enforced by the template renderer's
    /// `ninfer_key_file` check today.
    pub custody_data_roots: &'static [&'static str],
    /// The L3 serve-args/script artifact the harness renders for this engine;
    /// `None` where the adapter builds argv directly.
    pub serve_template: Option<&'static str>,
    /// Harness-config keys this engine consumes; the knob-parity test alarms
    /// when the config grows an engine-scoped key that joins neither a row nor
    /// the config-only list.
    pub config_knobs: &'static [&'static str],
    /// Whether this engine has an R11 Fit cost class: its row is the only one
    /// whose launches may book from the `llama-fit-params` projection. The
    /// ladder and the scheduler ask this cell instead of matching engine names
    /// (row-scoped, no new match-arm sprawl).
    pub supports_fit: bool,
}

static MOCK_ROW: EnginePolicy = EnginePolicy {
    id: "mock",
    // The mock argv (`mock-engine --name <model>`) is minted whole by the
    // adapter under the `allow_mock_engine` grant; nothing is force-tailed.
    forced_fields: &[],
    forbidden_words: &[],
    allow_words: &[],
    extra_words: None,
    knobs: &[],
    path_markers: &[],
    custody_exec_roots: &[],
    custody_data_roots: &[],
    serve_template: None,
    config_knobs: &["allow_mock_engine"],
    supports_fit: false,
};

static LLAMA_ROW: EnginePolicy = EnginePolicy {
    id: "llama",
    forced_fields: &[
        ForcedField {
            flag: "--host",
            value: ForcedValue::Literal(LLAMA_ENGINE_HOST),
        },
        ForcedField {
            flag: "--port",
            value: ForcedValue::SlotPort,
        },
    ],
    forbidden_words: LLAMA_FORBIDDEN_EXTRA_WORDS,
    allow_words: LLAMA_EXTRA_ARGS,
    extra_words: Some(ExtraWordsContract {
        yaml_key: "extra_args",
        check: super::llama::check_extra_words_value,
    }),
    knobs: LLAMA_KNOBS,
    path_markers: &["-m"],
    custody_exec_roots: &[],
    custody_data_roots: &[],
    serve_template: None,
    // R11: the Fit probe switch and its calibration constants live in the
    // harness config `engines:` section (slice 10d idiom), claimed here.
    config_knobs: &[
        "llama_server_exe",
        "engines.llama.fit_probe",
        "engines.llama.fit_margin_gb",
        "engines.llama.mmproj_surcharge_gb",
        "engines.llama.fit_probe_timeout_s",
    ],
    supports_fit: true,
};

static STRATA_ROW: EnginePolicy = EnginePolicy {
    id: "strata",
    forced_fields: &[
        ForcedField {
            flag: "-m",
            value: ForcedValue::Literal("serve.server"),
        },
        ForcedField {
            flag: "--engine",
            value: ForcedValue::Literal("strata"),
        },
        ForcedField {
            flag: "--host",
            // Same loopback literal the llama row forces: server.py resolves the
            // flag ahead of any profile `host`, and its own default is loopback,
            // so the bind is harness-forced argv and the rendered profile carries
            // no `host` key at all (22[1] clean by construction, row 19).
            value: ForcedValue::Literal(LLAMA_ENGINE_HOST),
        },
        ForcedField {
            flag: "--config",
            value: ForcedValue::OperatorData,
        },
        ForcedField {
            flag: "--port",
            value: ForcedValue::SlotPort,
        },
    ],
    // No operator word reaches the strata argv (template mode renders the whole
    // profile from validated yaml values — row 19), so there is nothing to scrub
    // by name; the custody cells below govern the locations.
    forbidden_words: &[],
    allow_words: &[],
    extra_words: None,
    knobs: STRATA_KNOBS,
    path_markers: &["--config"],
    custody_exec_roots: &[],
    // The data cell is load-bearing since row 19: the engine's profile is the
    // harness-rendered file under [`STRATA_RENDERED_PROFILES_ROOT`], and the
    // spawn rewrites `--config` to exactly that path — an operator profile is
    // no longer a launch input at all.
    custody_data_roots: &[STRATA_RENDERED_PROFILES_ROOT],
    serve_template: None,
    // The harness-side strata defaults live in the config `engines:` section
    // (slice 10d); the dotted claims are the knob-parity test's pointers into
    // that section, which the test walks in a populated serialization.
    config_knobs: &["engines.strata.python", "engines.strata.cwd"],
    supports_fit: false,
};

static BASALT_ROW: EnginePolicy = EnginePolicy {
    id: "basalt",
    forced_fields: &[
        ForcedField {
            flag: "-m",
            value: ForcedValue::Literal("serve.server"),
        },
        ForcedField {
            flag: "--host",
            // Same loopback literal the llama and strata rows force: the bind
            // is harness-forced argv and the rendered profile carries no
            // `host` key at all (§5, row 19 posture).
            value: ForcedValue::Literal(LLAMA_ENGINE_HOST),
        },
        ForcedField {
            flag: "--config",
            value: ForcedValue::OperatorData,
        },
        ForcedField {
            flag: "--port",
            value: ForcedValue::SlotPort,
        },
    ],
    // Exact-name denylist (the upstream parser resolves no abbreviations);
    // enforced twice: by the row's `extra_words` contract at scan/validate
    // time and by the adapter at render (defense in depth).
    forbidden_words: BASALT_FORBIDDEN_SERVER_FLAGS,
    allow_words: &[],
    extra_words: Some(ExtraWordsContract {
        yaml_key: "basalt_engine_args",
        check: super::basalt::check_engine_args_value,
    }),
    knobs: BASALT_KNOBS,
    path_markers: &["--config"],
    custody_exec_roots: &[],
    // The data cell is load-bearing from the first stage: the engine's profile
    // is the harness-rendered file under [`BASALT_RENDERED_PROFILES_ROOT`],
    // and the spawn rewrites `--config` to exactly that path — mirroring the
    // strata row 19 door, engine-tagged.
    custody_data_roots: &[BASALT_RENDERED_PROFILES_ROOT],
    serve_template: None,
    // No harness-config defaults exist for basalt (the row names every path
    // explicitly); the config-parity test alarms if that ever changes.
    config_knobs: &[],
    supports_fit: false,
};

static NINFER_ROW: EnginePolicy = EnginePolicy {
    id: crate::ownership::NINFER_ENGINE,
    // Every forced line rides inside the canonical script; the argv only
    // carries the WSL prefix and positional contract.
    forced_fields: &[],
    forbidden_words: NINFER_FORBIDDEN_SERVE_FLAGS,
    allow_words: &[],
    extra_words: Some(ExtraWordsContract {
        yaml_key: "ninfer_serve_args",
        check: super::ninfer::check_serve_args_value,
    }),
    knobs: NINFER_KNOBS,
    path_markers: &[],
    custody_exec_roots: &[NINFER_STAGED_SCRIPTS_ROOT],
    custody_data_roots: &[NINFER_KEYS_ROOT],
    serve_template: Some(CANONICAL_NINFER_SCRIPT),
    config_knobs: &["wsl_distro", "wsl_user"],
    supports_fit: false,
};

static POLICIES: &[&EnginePolicy] = &[&MOCK_ROW, &LLAMA_ROW, &STRATA_ROW, &BASALT_ROW, &NINFER_ROW];

/// Every row of the compiled-in table, in table order. The acceptance and
/// parity tests iterate it; production code always resolves its own row by id.
#[cfg(test)]
#[must_use]
pub(crate) fn all() -> &'static [&'static EnginePolicy] {
    POLICIES
}

/// The row for one `engine:` id, or `None` for an id the table does not know.
#[must_use]
pub(crate) fn engine_policy(id: &str) -> Option<&'static EnginePolicy> {
    POLICIES.iter().copied().find(|row| row.id == id)
}

/// The union of every row's path markers: the argv flags whose following
/// token names a model/profile file to the early-exit diagnosis. Pure lookup
/// over compiled-in data; the fast-fail path allocates only this small list.
#[must_use]
pub(crate) fn path_markers() -> Vec<&'static str> {
    POLICIES
        .iter()
        .flat_map(|row| row.path_markers.iter().copied())
        .collect()
}

/// Renders the row's forced launch tokens for one slot assignment: each
/// forced field's flag followed by its value (literal, slot port, or the
/// operator path this engine inserts between forced pairs). The single code
/// path the §20-9 ruling names for what the harness always writes.
///
/// # Panics
///
/// When a row carries an [`ForcedValue::OperatorData`] cell and `operator_data`
/// is `None` — a caller-rendering bug, not operator input.
pub(crate) fn render_forced(
    row: &EnginePolicy,
    port: u16,
    operator_data: Option<&str>,
) -> Vec<String> {
    let mut out = Vec::with_capacity(row.forced_fields.len() * 2);
    for field in row.forced_fields {
        out.push(field.flag.to_string());
        let value = match field.value {
            ForcedValue::Literal(v) => v.to_string(),
            ForcedValue::SlotPort => port.to_string(),
            ForcedValue::OperatorData => operator_data
                .expect("a row with an OperatorData slot must be rendered with its value")
                .to_string(),
        };
        out.push(value);
    }
    out
}

/// Which custody root class a path is admitted against (slice 10d): executed
/// paths are programs and working directories the harness itself launches into;
/// data paths are files the engine only reads at launch. The two classes are
/// deliberately separate cells — a root admitted for one says nothing about the
/// other (chunk 10 custody ruling 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CustodyClass {
    /// Class (a): the executed-path roots.
    Executed,
    /// Class (b): the data roots.
    Data,
}

impl CustodyClass {
    /// The operator-facing noun naming the class inside a refusal.
    const fn noun(self) -> &'static str {
        match self {
            Self::Executed => "executed-path",
            Self::Data => "data",
        }
    }

    /// The row's roots of this class.
    const fn roots(self, row: &EnginePolicy) -> &'static [&'static str] {
        match self {
            Self::Executed => row.custody_exec_roots,
            Self::Data => row.custody_data_roots,
        }
    }
}

/// Lexical custody membership: is `path` at or under one of the row's roots for
/// `class`. An empty cell is OPEN (ruling 5): nothing is refused, because no row
/// populates an exec root the later copy-in migration will fill and a closed
/// empty cell would refuse every legitimate config today.
///
/// The comparison is lexical (`\` → `/`, trailing `/` trimmed, ASCII
/// case-insensitive, component-boundary prefix), not filesystem resolution:
/// these paths are config text judged at spec time, and a `..` component matches
/// no root — the same conservative direction as the ninfer key-file shape check.
#[must_use]
pub(crate) fn admits_under_root(row: &EnginePolicy, class: CustodyClass, path: &str) -> bool {
    let roots = class.roots(row);
    if roots.is_empty() {
        return true;
    }
    let path = normalize_custody_path(path);
    if path.split('/').any(|component| component == "..") {
        // A traversal component proves no containment lexically (its answer
        // depends on resolution the harness does not do): refuse, same
        // conservative direction as the ninfer key-file shape check.
        return false;
    }
    roots.iter().any(|root| {
        let root = normalize_custody_path(root);
        path == root
            || path
                .strip_prefix(&*root)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

fn normalize_custody_path(text: &str) -> String {
    text.replace('\\', "/")
        .to_ascii_lowercase()
        .trim_end_matches('/')
        .to_string()
}

/// [`admits_under_root`] with its refusal: `Ok(())` or a rejection naming the
/// key, the path and the row's roots, so the operator sees which mechanism owns
/// the location rather than only that it was denied.
///
/// # Errors
///
/// [`super::EngineError::Rejected`] when the path lies outside every populated
/// root of that class (an empty cell refuses nothing, ruling 5).
pub(crate) fn admit_custody_path(
    row: &EnginePolicy,
    class: CustodyClass,
    model: &str,
    key: &str,
    path: &str,
) -> Result<(), super::EngineError> {
    if admits_under_root(row, class, path) {
        return Ok(());
    }
    Err(super::EngineError::Rejected {
        engine: row.id.to_string(),
        model: model.to_string(),
        reason: format!(
            "`{key}` names `{path}`, outside the {} custody roots of the `{}` EnginePolicy \
             row (harness-owned locations only): {}",
            class.noun(),
            row.id,
            class.roots(row).join(", ")
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::engine::{EngineAdapter, LaunchSpec};
    use crate::model::{CostClass, ModelEntry};
    use std::collections::{BTreeMap, BTreeSet};

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(engine: &str, pairs: &[(&str, &str)]) -> ModelEntry {
        ModelEntry {
            name: format!("{engine}-policy"),
            engine: engine.to_string(),
            category: CategoryId::Work,
            vram_gb: Some(10.0),
            gguf_size_gb: None,
            cost: CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            fit_gb: None,
            model_ref: None,
            serve_model: None,
            file: None,
            raw: raw(pairs),
        }
    }

    /// One L3 launch artifact flattened to comparable bytes: every field the
    /// binding invariant cares about (argv, cwd, env, log, container, script),
    /// with separators no field value may contain.
    fn l3_bytes(spec: &LaunchSpec) -> String {
        let mut out = spec.argv.join("\u{0}");
        out.push('\u{1}');
        out.push_str(&spec.cwd.to_string_lossy());
        out.push('\u{1}');
        for (key, value) in &spec.env {
            out.push_str(key);
            out.push('=');
            out.push_str(value);
            out.push('\u{2}');
        }
        out.push('\u{1}');
        out.push_str(&spec.log_name);
        out.push('\u{1}');
        out.push_str(spec.container.as_deref().unwrap_or(""));
        out.push('\u{1}');
        out.push_str(spec.script.as_deref().unwrap_or(""));
        out.push('\u{1}');
        match spec.rendered_profile.as_ref() {
            None => {}
            Some(profile) => {
                out.push_str(profile.engine);
                out.push(':');
                out.push_str(&profile.text);
            }
        }
        out.push('\u{1}');
        out.push_str(spec.custody.as_deref().unwrap_or(""));
        out
    }

    /// One cardless, spawn-free render of one pre-built L2 record: the same
    /// entry feeds both renders, so a difference could only come from the
    /// renderer, never from fresh fixture paths.
    fn render(entry: &ModelEntry) -> LaunchSpec {
        match entry.engine.as_str() {
            "mock" => crate::engine::mock::MockAdapter
                .spec(entry, 18109)
                .expect("mock renders"),
            "llama" => crate::engine::llama::LlamaAdapter::new(std::path::PathBuf::from(
                "llama-server.exe",
            ))
            .spec(entry, 18109)
            .expect("llama renders"),
            "strata" => crate::engine::strata::StrataAdapter::new(
                std::env::current_exe().expect("test exe is the interpreter stand-in"),
                std::env::temp_dir(),
            )
            .spec(entry, 18109)
            .expect("strata renders"),
            "basalt" => crate::engine::basalt::BasaltAdapter
                .spec(entry, 18109)
                .expect("basalt renders"),
            "ninfer" => crate::engine::ninfer::NinferAdapter::new("Ubuntu", "ops")
                .spec(entry, 18109)
                .expect("ninfer renders"),
            other => panic!("the table carries no render seam for `{other}`"),
        }
    }

    /// The same L2 record for one engine, built once per test run.
    fn fixture(engine: &str) -> ModelEntry {
        match engine {
            "mock" => entry("mock", &[]),
            "llama" => entry(
                "llama",
                &[
                    ("model", "F:/m.gguf"),
                    ("context", "4096"),
                    ("cpu_offload", "true"),
                    ("extra_args", "--embedding --pooling last"),
                ],
            ),
            "strata" => {
                let dir = crate::testutil::temp_dir("policy");
                let exe = dir.join("strata.exe");
                let tokenizer = dir.join("tokenizer.bin");
                std::fs::write(&exe, b"x").expect("exe fixture");
                std::fs::write(&tokenizer, b"t").expect("tokenizer fixture");
                let mut model = entry(
                    "strata",
                    &[
                        ("strata_exe", exe.to_str().expect("utf-8 path")),
                        ("strata_tokenizer", tokenizer.to_str().expect("utf-8 path")),
                        ("strata_cwd", dir.to_str().expect("utf-8 path")),
                        ("strata_args", "--native"),
                    ],
                );
                model.serve_model = Some("strata-policy".to_string());
                model
            }
            "basalt" => {
                let dir = crate::testutil::temp_dir("policy-basalt");
                let weights = dir.join("m.basalt");
                std::fs::write(&weights, b"w").expect("weights fixture");
                std::fs::write(dir.join("python.exe"), b"p").expect("python fixture");
                std::fs::write(dir.join("basalt-engine.exe"), b"e").expect("engine fixture");
                let mut model = entry(
                    "basalt",
                    &[
                        (
                            "basalt_python",
                            dir.join("python.exe").to_str().expect("utf-8"),
                        ),
                        (
                            "basalt_exe",
                            dir.join("basalt-engine.exe").to_str().expect("utf-8"),
                        ),
                        ("basalt_cwd", dir.to_str().expect("utf-8")),
                    ],
                );
                model.model_ref = Some(weights.to_str().expect("utf-8").to_string());
                model.serve_model = Some("basalt-policy".to_string());
                model
            }
            "ninfer" => entry(
                "ninfer",
                &[
                    ("ninfer_container", "ninfer-policy"),
                    ("ninfer_image", "ninfer:local"),
                    ("ninfer_model_name", "Some-Model"),
                    ("ninfer_artifact", "some_model.ninfer"),
                    ("ninfer_internal_port", "8090"),
                    ("ninfer_model_roots", "/mnt/d/Models"),
                    ("ninfer_serve_args", "--spec dflash2"),
                    ("ninfer_env", "CUDA_MODULE_LOADING=LAZY"),
                ],
            ),
            other => panic!("the table carries no fixture for `{other}`"),
        }
    }

    /// The binding invariant of L1/L2/L3 (`DECISIONS` §20-9): the same L2 +
    /// policy + L1 renders byte-identical artifacts, so nobody ever needs to
    /// hand-edit L3.
    #[test]
    fn every_engine_renders_byte_identical_twice() {
        for row in all() {
            let model = fixture(row.id);
            let first = l3_bytes(&render(&model));
            let second = l3_bytes(&render(&model));
            assert_eq!(
                first, second,
                "engine `{}`: L3 must be byte-reproducible from L2 + policy",
                row.id
            );
        }
    }

    /// A rendered ninfer script's `SERVE_ARGS=(...)` line — the only region
    /// operator serve-args words could enter L3 (the template's own forced
    /// `--host`/`--port` lines are harness writes, not admitted words).
    fn serve_args_line(script: &str) -> &str {
        script
            .lines()
            .find(|line| line.starts_with("SERVE_ARGS=("))
            .expect("the canonical script carries the SERVE_ARGS line")
    }

    /// The §20-9 acceptance rule: every `forbidden_words` entry is provably
    /// ABSENT from L3 even when present in hostile L1 input. Today every
    /// hostile attempt renders to a refusal (the scrub that refuses); the
    /// assertion is on the artifact, so a future change that ADMITS a forbidden
    /// word fails even though it would render successfully.
    #[test]
    fn forbidden_words_never_reach_a_rendered_l3() {
        for row in all() {
            for word in row.forbidden_words {
                match row.id {
                    "llama" => {
                        let spec = crate::engine::llama::LlamaAdapter::new(
                            std::path::PathBuf::from("llama-server.exe"),
                        )
                        .spec(
                            &entry(
                                "llama",
                                &[
                                    ("model", "F:/m.gguf"),
                                    ("extra_args", &format!("{word} victim.gguf")),
                                ],
                            ),
                            18109,
                        );
                        if let Ok(spec) = spec {
                            assert!(
                                !spec.argv.iter().any(|t| t.contains(word)),
                                "llama L3 admitted forbidden `{word}`: {:?}",
                                spec.argv
                            );
                        }
                    }
                    "ninfer" => {
                        let spec = crate::engine::ninfer::NinferAdapter::new("Ubuntu", "ops").spec(
                            &entry(
                                "ninfer",
                                &[
                                    ("ninfer_container", "ninfer-policy"),
                                    ("ninfer_image", "ninfer:local"),
                                    ("ninfer_model_name", "Some-Model"),
                                    ("ninfer_artifact", "some_model.ninfer"),
                                    ("ninfer_model_roots", "/mnt/d/Models"),
                                    ("ninfer_serve_args", &format!("{word} hostile")),
                                ],
                            ),
                            18109,
                        );
                        if let Ok(spec) = spec {
                            let script = spec.script.expect("template mode renders");
                            assert!(
                                !serve_args_line(&script).contains(word),
                                "ninfer L3 admitted forbidden `{word}`: {script}"
                            );
                        }
                    }
                    "basalt" => {
                        // Exact-name denylist: the hostile word rides as
                        // `word=value`, the same token name the server would
                        // parse, and the refusal is demanded (the row's
                        // `extra_words` contract runs at validate and at
                        // render), so absence-from-L3 is proven by the refusal.
                        let dir = crate::testutil::temp_dir("policy-basalt-hostile");
                        std::fs::write(dir.join("python.exe"), b"p").expect("python fixture");
                        std::fs::write(dir.join("basalt-engine.exe"), b"e")
                            .expect("engine fixture");
                        std::fs::write(dir.join("m.basalt"), b"w").expect("weights fixture");
                        let mut hostile = entry(
                            "basalt",
                            &[
                                (
                                    "basalt_python",
                                    dir.join("python.exe").to_str().expect("utf-8"),
                                ),
                                (
                                    "basalt_exe",
                                    dir.join("basalt-engine.exe").to_str().expect("utf-8"),
                                ),
                                ("basalt_cwd", dir.to_str().expect("utf-8")),
                                ("basalt_engine_args", &format!("{word}=victim")),
                            ],
                        );
                        hostile.model_ref =
                            Some(dir.join("m.basalt").to_str().expect("utf-8").to_string());
                        hostile.serve_model = Some("basalt-hostile".to_string());
                        let spec = crate::engine::basalt::BasaltAdapter.spec(&hostile, 18109);
                        match spec {
                            Ok(spec) => assert!(
                                !spec.argv.iter().any(|t| t.starts_with(word)),
                                "basalt L3 admitted forbidden `{word}`: {:?}",
                                spec.argv
                            ),
                            Err(error) => assert!(
                                error.to_string().contains(word),
                                "basalt refused `{word}` without naming it: {error}"
                            ),
                        }
                    }
                    other => {
                        // Rows without a word pass-through surface carry no
                        // forbidden list (nothing to scrub by name), so this
                        // arm only fires if such a row grew one in code but
                        // not in the test — a drift alarm, not dead weight.
                        panic!(
                            "row `{other}` has forbidden_words but the acceptance \
                             test has no admission seam for it — extend the test"
                        );
                    }
                }
            }
        }
    }

    /// Knob parity (sub-decisions 1+2): every engine-scoped key the harness
    /// config exposes answers to a policy-table row's `config_knobs` or to the
    /// config-only list — either as a top-level key or as a dotted claim into a
    /// section (`engines.strata.python`). The scanned side is the real
    /// serialized config TWICE, the unset default and a fully populated one, so
    /// a new `strata_python_default` or a new `engines:`-section sibling alarms
    /// here in both directions; the engine-name side is the table itself — no
    /// hand-copied list either way (the drift class `GOTCHAS` records).
    #[test]
    fn every_engine_scoped_config_knob_has_a_policy_home() {
        // Config-only knobs: engine-adjacent by name, deliberately not policy
        // data. Additions must say why the table must not own them.
        const CONFIG_ONLY_KNOBS: &[&str] = &[];
        let mut covered: Vec<&str> = CONFIG_ONLY_KNOBS.to_vec();
        for row in all() {
            covered.extend(row.config_knobs);
        }
        let engine_names: Vec<String> = all().iter().map(|row| row.id.to_string()).collect();
        // Serialize-side completeness: the None-optional engine keys are skipped
        // when absent, so the populated doc carries every value for the scan.
        // The full struct literals are the forcing function: a new
        // `EnginesConfig`/`StrataEngineConfig` field fails to compile here until
        // this test (and the row's claims) know it.
        let unset = serde_json::to_value(crate::config::AppConfig {
            wsl_user: Some("ops".to_string()),
            ..crate::config::AppConfig::default()
        })
        .expect("config serializes");
        let populated = serde_json::to_value(crate::config::AppConfig {
            wsl_user: Some("ops".to_string()),
            engines: Some(crate::config::EnginesConfig {
                strata: Some(crate::config::StrataEngineConfig {
                    python: Some(std::path::PathBuf::from(
                        "C:/Strata/.venv/Scripts/python.exe",
                    )),
                    cwd: Some(std::path::PathBuf::from("C:/Strata")),
                }),
                llama: Some(crate::config::LlamaEngineConfig {
                    fit_probe: Some(true),
                    fit_margin_gb: Some(2.5),
                    mmproj_surcharge_gb: Some(1.25),
                    fit_probe_timeout_s: Some(20),
                }),
            }),
            ..crate::config::AppConfig::default()
        })
        .expect("a populated engines section serializes");
        for config in [&unset, &populated] {
            let top = config.as_object().expect("config is a JSON object");
            for key in top.keys() {
                let lower = key.to_lowercase();
                let engine_scoped = lower.contains("engine")
                    || lower.contains("wsl")
                    || engine_names.iter().any(|name| lower.contains(name));
                if !engine_scoped {
                    continue;
                }
                assert!(
                    covered
                        .iter()
                        .any(|knob| knob == key || knob.starts_with(&format!("{key}."))),
                    "config key `{key}` is engine-scoped but has no `EnginePolicy` \
                     `config_knobs` cell, no dotted claim into it, and no config-only \
                     registration"
                );
            }
        }
        // The scan must see the knobs the rows claim, in the shape each claim
        // names: plain claims in the unset serialization, dotted claims in the
        // populated one — a renamed config field fails here, not in production.
        for row in all() {
            for knob in row.config_knobs {
                let (doc, label) = match knob.split_once('.') {
                    None => (&unset, "key"),
                    Some((_, rest)) => {
                        let claimed_engine = rest
                            .split('.')
                            .next()
                            .expect("dotted claim names an engine");
                        assert_eq!(
                            claimed_engine, row.id,
                            "dotted claim `{knob}` must name its own row"
                        );
                        (&populated, "section path")
                    }
                };
                let pointer = format!("/{}", knob.replace('.', "/"));
                assert!(
                    doc.pointer(&pointer).is_some(),
                    "row `{}` claims knob `{knob}` but the serialized config has no such \
                     {label}",
                    row.id
                );
            }
        }
        // And the other direction: a populated section may carry no sub-key the
        // rows have not claimed (a silently growing `engines:` object is exactly
        // the un-owned knob this test exists to catch).
        if let Some(engines) = populated.get("engines").and_then(|e| e.as_object()) {
            for (engine, sub) in engines {
                for sub_key in sub.as_object().expect("engine section is an object").keys() {
                    let claim = format!("engines.{engine}.{sub_key}");
                    assert!(
                        covered.iter().any(|knob| *knob == claim),
                        "`engines.{engine}.{sub_key}` is serialized but no row claims it"
                    );
                }
            }
        }
    }

    /// Admission helper (slice 10d): an empty cell is open (ruling 5); a
    /// populated root admits at-or-under and refuses outside, at the component
    /// boundary, and the two classes stay separate (ruling 4) — an exec root
    /// never lets a data path through, or the other way.
    #[test]
    fn custody_admission_is_open_when_empty_and_per_class_when_populated() {
        let open = custody_row(&[], &[]);
        assert!(admits_under_root(
            &open,
            CustodyClass::Executed,
            r"C:\anywhere\python.exe"
        ));
        assert!(admits_under_root(&open, CustodyClass::Data, "/etc/shadow"));

        let row = custody_row(&[r"C:\Strata"], &[NINFER_KEYS_ROOT]);
        assert!(admits_under_root(
            &row,
            CustodyClass::Executed,
            r"C:\Strata\.venv\Scripts\python.exe"
        ));
        assert!(admits_under_root(&row, CustodyClass::Executed, "c:/strata"));
        assert!(
            !admits_under_root(&row, CustodyClass::Executed, r"C:\Strata-nvfp4\x.exe"),
            "a sibling directory sharing the prefix is not under the root"
        );
        assert!(!admits_under_root(
            &row,
            CustodyClass::Executed,
            "/etc/passwd"
        ));
        assert!(admits_under_root(
            &row,
            CustodyClass::Data,
            "$HOME/.simplehar/keys/x.key"
        ));
        assert!(
            !admits_under_root(
                &row,
                CustodyClass::Data,
                "$HOME/.simplehar/keys/../../x.key"
            ),
            "a traversal component matches no root"
        );
        // Exec and data are different custody (ruling 4), never one shared list.
        assert!(!admits_under_root(
            &row,
            CustodyClass::Data,
            r"C:\Strata\.venv\Scripts\python.exe"
        ));
        assert!(!admits_under_root(
            &row,
            CustodyClass::Executed,
            "$HOME/.simplehar/keys/x.key"
        ));
    }

    /// A refusal must teach (slice 10c doctrine applied to custody): the key,
    /// the offending path and the row's roots are all in the sentence; an open
    /// cell refuses nothing.
    #[test]
    fn an_outside_root_path_is_refused_naming_key_path_and_roots() {
        let row = custody_row(&[r"C:\Strata"], &[]);
        let err = admit_custody_path(
            &row,
            CustodyClass::Executed,
            "m",
            "strata_python",
            r"D:\evil\python.exe",
        )
        .expect_err("outside the populated exec roots");
        let text = err.to_string();
        for needle in [
            "`strata_python`",
            r"`D:\evil\python.exe`",
            "executed-path custody roots",
            r"C:\Strata",
        ] {
            assert!(text.contains(needle), "{needle} missing from: {text}");
        }
        assert!(
            admit_custody_path(
                &row,
                CustodyClass::Data,
                "m",
                "strata_config",
                r"D:\a\b.json"
            )
            .is_ok(),
            "an empty data cell denies nothing (ruling 5)"
        );
    }

    /// A table row with only the custody cells varied, for the admission pins.
    fn custody_row(exec: &'static [&'static str], data: &'static [&'static str]) -> EnginePolicy {
        EnginePolicy {
            id: "custody-test",
            custody_exec_roots: exec,
            custody_data_roots: data,
            ..engine_policy("mock")
                .expect("the table ships a mock row")
                .clone()
        }
    }

    /// Custody (slice 10b): only the strata row's launches ride the harness
    /// custody sidecar — the runtime records exactly what the spec declares.
    /// Ninfer's evidence is the container claim ledger; llama and mock ride
    /// the job object's crash guarantee alone. A row growing `custody`
    /// without the sweep knowing its engine id would fail here first.
    #[test]
    fn custody_record_writes_are_the_strata_launches_alone() {
        for row in all() {
            let spec = render(&fixture(row.id));
            if row.id == "strata" {
                assert_eq!(
                    spec.custody.as_deref(),
                    Some("strata"),
                    "the strata launch must be custody-tracked"
                );
                assert!(
                    engine_policy("strata").is_some(),
                    "a custody engine must answer to a compiled-in row"
                );
            } else {
                assert_eq!(
                    spec.custody, None,
                    "engine `{}` may not claim custody",
                    row.id
                );
            }
        }
    }

    /// "One mechanism, five instances": the table and the adapter registry
    /// must know the same engine ids — a new adapter without a row, or a row
    /// without an adapter, is exactly the fork this chunk exists to close.
    #[test]
    fn policy_rows_and_boot_adapters_cover_the_same_engines() {
        let config = crate::config::AppConfig {
            allow_mock_engine: true,
            ..crate::config::AppConfig::default()
        };
        let adapters: BTreeMap<String, std::sync::Arc<dyn EngineAdapter>> =
            crate::engine::default_adapters(&config);
        let mut row_ids: Vec<&str> = all().iter().map(|row| row.id).collect();
        let mut adapter_ids: Vec<&str> = adapters.keys().map(String::as_str).collect();
        row_ids.sort_unstable();
        adapter_ids.sort_unstable();
        assert_eq!(
            row_ids, adapter_ids,
            "EnginePolicy rows and the adapter map must name the same engines"
        );
    }

    /// The knob table (slice 10c generalization of the `cpu_offload` scoping
    /// ruling): the llama row owns exactly `FLAG_TABLE` plus the three keys the
    /// adapter reads outside it (`cpu_offload`, `jinja`, `extra_args`) — a new
    /// flag-table key or a new adapter read must join the row, and
    /// `cpu_offload` answers to the llama row alone (the fact
    /// `OFFLOAD_ENGINES` used to hold). Falsifier: dropping a knob from the row
    /// would warn a live llama yaml's own key as inert at scan.
    #[test]
    fn the_knob_table_owns_every_engine_scoped_yaml_key() {
        let llama = engine_policy("llama").expect("the table ships a llama row");
        for (key, _) in crate::engine::llama::FLAG_TABLE {
            assert!(
                llama.knobs.contains(key),
                "FLAG_TABLE key `{key}` must be a llama knob"
            );
        }
        for key in ["cpu_offload", "jinja", "extra_args"] {
            assert!(llama.knobs.contains(&key), "`{key}` must be a llama knob");
        }
        assert_eq!(
            llama.knobs.len(),
            crate::engine::llama::FLAG_TABLE.len() + 3,
            "the llama knob list must be FLAG_TABLE plus the three documented extras, \
             no stale members"
        );
        assert_eq!(knob_readers("cpu_offload"), ["llama"]);
        assert_eq!(
            crate::engine::llama::OFFLOAD_ENGINES,
            knob_readers("cpu_offload"),
            "the page-parity record must stand with the knob table"
        );
        assert!(!knob_live("cpu_offload", "mock"));
        assert!(knob_live("cpu_offload", "llama"));
        assert!(!knob_live("cpu_offload", "ninfer"));
        // Unknown ids read nothing; the actor's EngineUnavailable drop is their report.
        assert!(!knob_live("cpu_offload", "no-such-engine"));
        // Every engine-scoped key answers to exactly one row (the hint text
        // "read only by the X engine(s)" is only honest one way).
        for row in all() {
            for key in row.knobs {
                assert_eq!(
                    knob_readers(key),
                    [row.id],
                    "knob `{key}` must answer to exactly the `{}` row",
                    row.id
                );
            }
        }
    }

    /// Full `extra_args` enforcement (slice 10c): the scan-facing contract
    /// exists exactly on the rows that admit operator words, each on its own
    /// key — and it refuses the same words the render refuses, so the file
    /// dies at rescan rather than at spawn.
    #[test]
    fn only_the_word_admitting_rows_carry_an_extra_words_contract() {
        let cases = [
            ("llama", "extra_args", "--api-key sekrit"),
            ("ninfer", "ninfer_serve_args", "--max-context 999"),
            ("basalt", "basalt_engine_args", "--host=0.0.0.0"),
        ];
        for (id, key, hostile) in cases {
            let row = engine_policy(id).expect("row");
            let contract = row
                .extra_words
                .unwrap_or_else(|| panic!("`{id}` admits operator words and needs a contract"));
            assert_eq!(contract.yaml_key, key);
            let error = (contract.check)(id, "t", hostile)
                .expect_err("`{hostile}` must refuse at validate time");
            let text = error.to_string();
            assert!(text.contains(id), "refusal names the engine: {text}");
        }
        for id in ["mock", "strata"] {
            assert!(
                engine_policy(id).expect("row").extra_words.is_none(),
                "`{id}` admits no operator word and must carry no contract"
            );
        }
    }

    /// Every refused word's refusal must be able to name the owning mechanism
    /// (slice 10c): the owner tables cover the reviewed forbidden records
    /// exactly — a forbidden word without an owner sentence is a refusal that
    /// cannot teach, and the scan's hostile word would say only "not allowed".
    #[test]
    fn every_forbidden_word_has_an_owner_sentence() {
        for word in LLAMA_FORBIDDEN_EXTRA_WORDS {
            assert!(
                llama_extra_arg_owner(word).is_some(),
                "`extra_args` word `{word}` has no owner sentence"
            );
        }
        for flag in NINFER_FORBIDDEN_SERVE_FLAGS {
            assert!(
                ninfer_serve_arg_owner(flag).is_some(),
                "`ninfer_serve_args` flag `{flag}` has no owner sentence"
            );
        }
        for flag in BASALT_FORBIDDEN_SERVER_FLAGS {
            assert!(
                basalt_server_arg_owner(flag).is_some(),
                "`basalt_engine_args` flag `{flag}` has no owner sentence"
            );
        }
        // Abbreviations resolve through the denylist matcher to their full
        // flag (the parser-resolves-prefixes rule), so they inherit the owner.
        let abbreviated = forbidden_serve_flag("--max-cont").expect("--max-cont is --max-context");
        assert!(ninfer_serve_arg_owner(abbreviated).is_some());
        assert_eq!(forbidden_serve_flag("--spec"), None);
        assert_eq!(llama_extra_arg_owner("--foo"), None);
        // The other direction (the "one entry per member" claim both owner
        // tables make): an owner entry no forbidden word cites is a dead
        // sentence pretending to teach, so the sets must match exactly.
        let owners: BTreeSet<&str> = LLAMA_EXTRA_ARG_OWNERS.iter().map(|(w, _)| *w).collect();
        assert_eq!(
            owners,
            LLAMA_FORBIDDEN_EXTRA_WORDS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            "the llama owner table must carry exactly one entry per forbidden word"
        );
        let owners: BTreeSet<&str> = NINFER_SERVE_ARG_OWNERS.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            owners,
            NINFER_FORBIDDEN_SERVE_FLAGS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            "the ninfer owner table must carry exactly one entry per forbidden flag"
        );
        let owners: BTreeSet<&str> = BASALT_SERVER_ARG_OWNERS.iter().map(|(f, _)| *f).collect();
        assert_eq!(
            owners,
            BASALT_FORBIDDEN_SERVER_FLAGS
                .iter()
                .copied()
                .collect::<BTreeSet<_>>(),
            "the basalt owner table must carry exactly one entry per forbidden flag"
        );
    }

    /// R2/BS-03: the deny-list test above iterates the list itself, so deleting
    /// a member would keep it green. This independent pin names every upstream
    /// `serve.server` option; removing one from the production list, or letting
    /// one past the extra-args check, turns this RED.
    #[test]
    fn basalt_server_owned_flags_are_pinned_by_name() {
        for flag in [
            "--mock-engine",
            "--model",
            "--engine-bin",
            "--alias",
            "--parallel",
            "--slot-full-context",
            "--config",
            "--vision",
            "--host",
            "--script",
            "--port",
            "--gpu",
            "--tokenizer",
            "--open",
            "--fit-max-tokens",
            "--grammar",
            "--no-grammar",
            "--temperature",
            "--top-p",
            "--top-k",
            "--min-p",
            "--presence-penalty",
            "--frequency-penalty",
            "--repetition-penalty",
            "--penalty-last-n",
            "--seed",
            "--api-key",
            "--allowed-host",
            "--cors-origin",
            "--debug",
            "--request-lines",
            "--lazy",
            "--idle-unload",
            "--idle-sleep",
            "--min-free-vram-mib",
            "--reasoning-budget-tokens",
            "--thinking-loop-tokens",
            "--answer-room",
            "--no-answer-room",
        ] {
            assert!(
                BASALT_FORBIDDEN_SERVER_FLAGS.contains(&flag),
                "`{flag}` is a server-owned basalt flag and must stay in the deny-list"
            );
            assert!(
                basalt_server_arg_owner(flag).is_some(),
                "`{flag}` must keep an owner sentence"
            );
            assert!(
                crate::engine::basalt::check_engine_args_value("basalt", "model", flag).is_err(),
                "`{flag}` must be refused through `basalt_engine_args`"
            );
        }
    }
}
