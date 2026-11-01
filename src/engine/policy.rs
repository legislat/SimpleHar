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
# simplehar-se canonical ninfer autostart — rendered by the harness from its embedded
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
}

static MOCK_ROW: EnginePolicy = EnginePolicy {
    id: "mock",
    // The mock argv (`mock-engine --name <model>`) is minted whole by the
    // adapter under the `allow_mock_engine` grant; nothing is force-tailed.
    forced_fields: &[],
    forbidden_words: &[],
    allow_words: &[],
    path_markers: &[],
    custody_exec_roots: &[],
    custody_data_roots: &[],
    serve_template: None,
    config_knobs: &["allow_mock_engine"],
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
    path_markers: &["-m"],
    custody_exec_roots: &[],
    custody_data_roots: &[],
    serve_template: None,
    config_knobs: &["llama_server_exe"],
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
            flag: "--config",
            value: ForcedValue::OperatorData,
        },
        ForcedField {
            flag: "--port",
            value: ForcedValue::SlotPort,
        },
    ],
    // No operator word reaches the strata argv (only existence-checked paths
    // do), so there is nothing to scrub by name; the custody slice governs the
    // profile and interpreter paths through the custody cells below.
    forbidden_words: &[],
    allow_words: &[],
    path_markers: &["--config"],
    custody_exec_roots: &[],
    custody_data_roots: &[],
    serve_template: None,
    // `strata_python`/`strata_cwd` defaults are adapter constants today and
    // move to the config `engines:` section with the custody slice; when they
    // do, the knob-parity test starts requiring them here.
    config_knobs: &[],
};

static NINFER_ROW: EnginePolicy = EnginePolicy {
    id: crate::ownership::NINFER_ENGINE,
    // Every forced line rides inside the canonical script; the argv only
    // carries the WSL prefix and positional contract.
    forced_fields: &[],
    forbidden_words: NINFER_FORBIDDEN_SERVE_FLAGS,
    allow_words: &[],
    path_markers: &[],
    custody_exec_roots: &[NINFER_STAGED_SCRIPTS_ROOT],
    custody_data_roots: &[NINFER_KEYS_ROOT],
    serve_template: Some(CANONICAL_NINFER_SCRIPT),
    config_knobs: &["wsl_distro", "wsl_user"],
};

static POLICIES: &[&EnginePolicy] = &[&MOCK_ROW, &LLAMA_ROW, &STRATA_ROW, &NINFER_ROW];

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::engine::{EngineAdapter, LaunchSpec};
    use crate::model::{CostClass, ModelEntry};
    use std::collections::BTreeMap;

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
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
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
                let config = crate::testutil::temp_dir("policy").join("profile.json");
                std::fs::write(&config, b"{}").expect("profile fixture");
                entry(
                    "strata",
                    &[("strata_config", config.to_str().expect("utf-8 path"))],
                )
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
    /// config-only list. The scanned side is the real serialized config (a new
    /// `strata_python_default` or `engines:`-section sibling alarms here), and
    /// the engine-name side is the table itself — no hand-copied list either
    /// way (the drift class `GOTCHAS` records).
    #[test]
    fn every_engine_scoped_config_knob_has_a_policy_home() {
        // Config-only knobs: engine-adjacent by name, deliberately not policy
        // data. Additions must say why the table must not own them.
        const CONFIG_ONLY_KNOBS: &[&str] = &[];
        // Serialize-side completeness: the None-optional engine key is skipped
        // when absent, so the default doc carries a value for the scan.
        let config = crate::config::AppConfig {
            wsl_user: Some("ops".to_string()),
            ..crate::config::AppConfig::default()
        };
        let value = serde_json::to_value(&config).expect("config serializes");
        let engine_names: Vec<String> = all().iter().map(|row| row.id.to_string()).collect();
        let mut covered: Vec<&str> = CONFIG_ONLY_KNOBS.to_vec();
        for row in all() {
            covered.extend(row.config_knobs);
        }
        let top = value.as_object().expect("config is a JSON object");
        let mut seen: Vec<&String> = Vec::new();
        for key in top.keys() {
            let lower = key.to_lowercase();
            let engine_scoped = lower.contains("engine")
                || lower.contains("wsl")
                || engine_names.iter().any(|name| lower.contains(name));
            if !engine_scoped {
                continue;
            }
            seen.push(key);
            assert!(
                covered.iter().any(|knob| knob == key),
                "config key `{key}` is engine-scoped but has no `EnginePolicy` \
                 `config_knobs` cell and no config-only registration"
            );
        }
        // The scan must see the knobs the rows claim — a renamed config field
        // fails here rather than leaving a stale row.
        for row in all() {
            for knob in row.config_knobs {
                assert!(
                    seen.iter().any(|key| key == knob),
                    "row `{}` claims knob `{knob}` but the serialized config has no such key",
                    row.id
                );
            }
        }
    }

    /// "One mechanism, four instances": the table and the adapter registry
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
}
