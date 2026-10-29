//! The llama.cpp adapter (DESIGN Step 7, Appendix A.7).
//!
//! The flag map is *data*, byte-equivalent in semantics to the operator's
//! `C:\Inference\llama.cpp\start.ps1:45-63` — the script is the source of truth,
//! and the parity test below re-reads it so the harness cannot drift. Two
//! harness-level special cases are documented extensions: `cpu_offload` (forces
//! `-ngl 0`, suppressing `ngl`) and the forced `--host`/`--port` pair. The one
//! deliberate divergence from the script: `jinja:` is read with a **case-sensitive**
//! truth set ([`jinja_on`]) where the script's PowerShell `-in 'true','1','on'` is
//! case-insensitive — the narrower set is the pinned product rule (`GOTCHAS`), so
//! `jinja: True` enables nothing here while the script would have enabled it. The
//! pinned build also defaults jinja to *enabled*, so `jinja: false` cannot switch it
//! off (`--no-jinja` is not [`EXTRA_ARGS_ALLOWED`], and a harness-emitted `--jinja`
//! must not be flippable from yaml text); that is a known functional gap, not an
//! enforcement claim.
//!
//! `extra_args` is an ALLOW-LIST (FINDINGS2-0910 [1], which inverted the old
//! ten-name `HARNESS_OWNED_FLAGS` denial list — ten exact words over an ~200-option
//! parser were a semantic bypass of the LOCKED harness-owned-config rule, leaving
//! `--api-key`, `--rpc`, `--path`, `--models-dir`, `--ssl-key-file`, `--model-url`,
//! `--hf-repo` and `--gpu-layers` reachable from one yaml line). An operator word
//! reaches the argv only if it is on [`EXTRA_ARGS_ALLOWED`], together with the value
//! tokens its arity consumes; every other word is refused with
//! [`EngineError::Rejected`] naming it. That makes several former separate claims
//! moot-by-construction rather than enforced: the harness-owned flags and *all*
//! their alias spellings (`-ngl`/`--n-gpu-layers`/`--gpu-layers`, `-h`, `-p`,
//! `--host`, `--port`, `-m`/`--model`, `--mmproj`) and the jinja pair (`--jinja`,
//! `--no-jinja`) are refused because they are ABSENT from the list, not because a
//! name matched a deny-list — so no alias can walk through again. The tail-forced
//! flags (`--host`, `--port`, `-ngl 0`) stay as the second enforcement for the three
//! options the harness itself re-emits, under the same last-occurrence assumption
//! (still unverifiable without a load). `--host` is always the
//! [`LLAMA_ENGINE_HOST`] literal, never the config's `internal_host`.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// IPv4 loopback address the harness forces as the engine bind address (`--host`).
///
/// Engines are launched with this literal, never with config `internal_host`:
/// `internal_host` governs only the harness's own public listeners, so a
/// `0.0.0.0` control page never exposes unauthenticated `llama-server` ports on
/// the segment, and the IPv4-only readiness probe always dials what the engine
/// actually bound (FINDINGS-09 [1]).
pub const LLAMA_ENGINE_HOST: &str = "127.0.0.1";

/// The CLOSED allow-list of `extra_args` option words an operator may pass through
/// (FINDINGS2-0910 [1]): the inversion of the old `HARNESS_OWNED_FLAGS` denial list,
/// which could only ever name ten of the pinned build's ~200 options and therefore
/// left the harness-owned decisions — which weights serve, the bind host/port, the
/// key, the transport, the served static roots, egress and port reuse — reachable
/// from a single yaml line. A word passes iff it names one of these entries
/// (or, for a value-taking entry, that spelling with `=value` appended); the
/// entry's `arity` following words are its values and pass verbatim as data.
/// Everything else is refused, so the parser is never reached from the catalog.
///
/// Adding a word here is a review decision, and the entry must state which
/// harness-owned decision it cannot move. The set is admitted because every entry
/// is (a) present in the pinned build (checked against the `llama-common.dll`
/// option string table, so an admitted word is one the engine actually parses) and
/// (b) unable to choose weights, bind address, port, authentication, TLS, static
/// roots, download/RPC egress, `--reuse-port`, or to flip a flag the harness itself
/// emits. Deliberately absent — reviewed and refused, along with everything else:
/// `-m`/`--model`/`--model-url`/`--hf-repo`/`--hf-file`/`--docker-repo` (which
/// weights serve), `--models-dir`/`--models-preset`/`--models-autoload` (an
/// engine-side router file), `--mmproj` (a second weights file), `--host`/`--port`
/// and `-ngl` with all its alias spellings (harness-forced), `--api-key*`,
/// `--ssl-*`, `--rpc`, `--path`, `--reuse-port`, `--lora*`, `--control-vector*`,
/// `--override-kv` and `--jinja`/`--no-jinja` (each moves a harness-owned decision
/// or rewrites the template the harness would have emitted).
const EXTRA_ARGS_ALLOWED: &[(&str, usize)] = &[
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

/// What one `extra_args` word is under [`EXTRA_ARGS_ALLOWED`].
struct AllowedWord {
    /// How many following words this entry consumes as its own values.
    arity: usize,
}

/// Classifies one `extra_args` word, accepting `flag` and `flag=value` for a
/// value-taking entry. `None` (refused) covers an unknown word, an `=` on a
/// valueless entry, an empty or option-shaped inline value, and any harness-owned or
/// alias spelling.
fn classify_extra_word(word: &str) -> Option<AllowedWord> {
    if let Some((name, value)) = word.split_once('=') {
        let (_, arity) = EXTRA_ARGS_ALLOWED.iter().find(|(flag, _)| *flag == name)?;
        // An inline value is the same data as a following token, so it faces the
        // same floor: `--pooling=--api-key` must not be a way to smuggle one.
        if *arity == 0 || !valid_extra_value(value) {
            return None;
        }
        return Some(AllowedWord { arity: 0 });
    }
    let (_, arity) = EXTRA_ARGS_ALLOWED.iter().find(|(flag, _)| *flag == word)?;
    Some(AllowedWord { arity: *arity })
}

/// Whether an `extra_args` value token is safe to hand over as data.
///
/// A value may not be option-shaped: an engine parser that refuses to bind an
/// option-looking token to the preceding flag would then read it as its own option,
/// which is how an allow-list is walked around. A leading `-` is therefore admitted
/// only for a finite number (`--seed -1`), never for a bare `-` or a word.
fn valid_extra_value(value: &str) -> bool {
    !value.is_empty()
        && (!value.starts_with('-') || value.parse::<f64>().is_ok_and(|n| n.is_finite()))
}

/// The refusal voice for every `extra_args` verdict: the typed [`EngineError::Rejected`]
/// naming the offending word, so a refused launch says what to delete (G6). The
/// reason carries the whole list, generated from the const, so the operator-facing
/// text cannot drift from the rule.
fn rejected_extra(model_name: &str, detail: String) -> EngineError {
    let listed = EXTRA_ARGS_ALLOWED
        .iter()
        .map(|(flag, _)| format!("`{flag}`"))
        .collect::<Vec<_>>()
        .join(" ");
    EngineError::Rejected {
        engine: "llama".to_string(),
        model: model_name.to_string(),
        reason: format!(
            "extra_args {detail}; the closed allow-list is {listed} (an allowed flag's own \
             value tokens pass through unchanged, and any other option is refused)"
        ),
    }
}

/// Rejects any `extra_args` word that is not on [`EXTRA_ARGS_ALLOWED`], walking the
/// same whitespace tokenization [`build_args`] appends from so the checked words and
/// the emitted words can never diverge.
///
/// # Errors
///
/// [`EngineError::Rejected`] naming the offending token: an unknown option, a
/// flag whose value is missing, a value that is option-shaped, or an `=` form the
/// entry does not allow.
fn check_extra_args(words: &[&str], model_name: &str) -> Result<(), EngineError> {
    let mut index = 0;
    while index < words.len() {
        let word = words[index];
        index += 1;
        let Some(allowed) = classify_extra_word(word) else {
            return Err(rejected_extra(
                model_name,
                format!("word `{word}` is not allowed"),
            ));
        };
        for _ in 0..allowed.arity {
            let Some(value) = words.get(index) else {
                return Err(rejected_extra(
                    model_name,
                    format!("flag `{word}` is missing its value"),
                ));
            };
            if !valid_extra_value(value) {
                return Err(rejected_extra(
                    model_name,
                    format!("flag `{word}` cannot take option-shaped value `{value}`"),
                ));
            }
            index += 1;
        }
    }
    Ok(())
}

/// yaml key -> llama-server flag, in `start.ps1` order. Empty values are skipped,
/// exactly like the script's `if ($v)`. `pub(crate)` so the scan's consumed-keys
/// const test can assert every table key is a consumed key (FINDINGS-13 [8]): a
/// flag added here without joining `scan::CONSUMED_KEYS` fails that test.
pub(crate) const FLAG_TABLE: &[(&str, &str)] = &[
    ("mmproj", "--mmproj"),
    ("context", "-c"),
    ("ngl", "-ngl"),
    ("batch", "-b"),
    ("ubatch", "--ubatch-size"),
    ("flash_attn", "--flash-attn"),
    ("cache_type_k", "--cache-type-k"),
    ("cache_type_v", "--cache-type-v"),
    ("parallel", "--parallel"),
    ("fit", "--fit"),
    ("fit_target", "--fit-target"),
    ("fit_ctx", "--fit-ctx"),
];

/// Engine ids for which `cpu_offload` is a live concept: this table is
/// **the first row of the §20-9 Fork C `EnginePolicy` knob table** — scoping
/// data, not scattered `if entry.engine == "llama"` tests, so chunk 10 replaces
/// one lookup rather than a scatter. An absent `engine:` key resolves to
/// `"llama"` ([`crate::model::ModelEntry::from_flat`]), the scan default, so it
/// is in scope through that default. `mock` is deliberately absent: it
/// impersonates other engines in tests and its adapter reads no offload knob,
/// so claiming it live would be the engine-neutrality lie this scoping exists
/// to remove; tests that need llama offload semantics name `engine: llama`.
/// Sibling llama knobs (`ngl`, `fit`, `context`, ...) are knowingly unscoped
/// here for now — the full knob table is chunk 10's object.
pub(crate) const OFFLOAD_ENGINES: &[&str] = &["llama"];

/// Whether `engine` reads `cpu_offload` anywhere (spawn, stamp, void). The
/// engine string must already carry the absent-key default (`"llama"`).
#[must_use]
pub(crate) fn offload_live(engine: &str) -> bool {
    OFFLOAD_ENGINES.contains(&engine)
}

/// The harness's own truth set for `jinja` (and the canonical read of
/// `cpu_offload`): exactly `true`, `1`, `on`, **case-sensitively**.
///
/// This is narrower than `start.ps1`, whose `-in 'true','1','on'` is
/// case-INsensitive (PowerShell's `-in` ignores case — measured, round 1 of this
/// review, and the reason the script's wording must not be quoted as a property of
/// this function). The narrower set is the pinned product rule (`GOTCHAS`: the
/// engine's set is `true|1|on`, `model::truthy` also accepts `yes`); the A.7 parity
/// test still asserts the script's literal list, so a script change shows up as a
/// test failure rather than a silent divergence. Because the pinned build defaults
/// jinja to *enabled*, this knob is one-way: only a truthy word changes the argv,
/// and `jinja: false` is inert (see the module header).
///
/// The control page's CPU checkbox uses THIS (not `model::truthy`) and can never
/// disagree with whether `-ngl 0` was actually forced (review finding, 2026-10-26).
#[must_use]
pub fn jinja_on(value: &str) -> bool {
    matches!(value, "true" | "1" | "on")
}

/// Builds the llama-server argv from the raw yaml map, mirroring `start.ps1:45-63`
/// (the deliberate deviation: the forced flags move to the tail): `-m` first
/// (required), `--mmproj`, the remaining mapped flags with non-empty
/// values, `--jinja` when truthy, then the admitted `extra_args` words, and the
/// harness-forced flags at the tail (`--host`, `--port`, and `-ngl 0` when
/// `cpu_offload` is on) so forced values win under any parser that honors last
/// occurrence. `extra_args` is filtered by the closed [`EXTRA_ARGS_ALLOWED`] list,
/// so nothing but a reviewed word can reach the engine at all.
///
/// Crate-internal: `host` is only ever [`LLAMA_ENGINE_HOST`] in production (from
/// [`LlamaAdapter::spec`]); a public host knob would be the struct-literal bypass
/// `ff235e6` deleted from the adapter, back in parameter form.
///
/// # Errors
///
/// - [`EngineError::MissingField`] when `model` is absent or empty — the script
///   refuses to launch without it too.
/// - [`EngineError::Rejected`] when an `extra_args` word is not on the allow-list,
///   naming the offending token.
pub(crate) fn build_args(
    raw: &BTreeMap<String, String>,
    host: &str,
    port: u16,
    model_name: &str,
) -> Result<Vec<String>, EngineError> {
    let Some(model) = raw
        .get("model")
        .map(String::as_str)
        .filter(|v| !v.is_empty())
    else {
        return Err(EngineError::MissingField {
            model: model_name.to_string(),
            field: "model".to_string(),
        });
    };
    let value = |key: &str| raw.get(key).map(String::as_str).filter(|v| !v.is_empty());
    // One tokenization feeds both the check and the argv, so a word cannot survive
    // by being split differently on the two paths (the classic bug class here,
    // FINDINGS2-0910 "Verified clean").
    let extra: Vec<&str> = value("extra_args")
        .map(|extra| extra.split_whitespace().collect())
        .unwrap_or_default();
    check_extra_args(&extra, model_name)?;
    let cpu_offload = raw.get("cpu_offload").is_some_and(|v| jinja_on(v));
    let mut args = vec!["-m".to_string(), model.to_string()];
    // Script order (start.ps1:45-63): mmproj leads the table, then the rest of the
    // table, then --jinja, then extra_args; the forced flags come last of all.
    for (key, flag) in FLAG_TABLE {
        if *key == "ngl" && cpu_offload {
            continue; // A.7: cpu_offload forces -ngl 0 and suppresses the yaml ngl.
        }
        if let Some(v) = value(key) {
            args.extend([flag.to_string(), v.to_string()]);
        }
    }
    if raw.get("jinja").is_some_and(|v| jinja_on(v)) {
        args.push("--jinja".to_string());
    }
    args.extend(extra.iter().map(|word| (*word).to_string()));
    args.extend(["--host".to_string(), host.to_string()]);
    args.extend(["--port".to_string(), port.to_string()]);
    if cpu_offload {
        args.extend(["-ngl".to_string(), "0".to_string()]);
    }
    Ok(args)
}

/// llama.cpp adapter: argv from data (A.7), readiness `/health` 200 (v3.3 contract).
///
/// Engines are forced to `--host` [`LLAMA_ENGINE_HOST`]; config `internal_host`
/// governs only the harness's own public listeners, never the engine bind address.
#[derive(Debug, Clone)]
pub struct LlamaAdapter {
    /// Path to `llama-server.exe` (config `llama_server_exe`).
    pub exe: PathBuf,
}

impl LlamaAdapter {
    /// Creates the adapter for the configured binary. The engine bind address is
    /// not configurable: [`LLAMA_ENGINE_HOST`] is forced into every argv.
    pub fn new(exe: PathBuf) -> Self {
        LlamaAdapter { exe }
    }
}

impl EngineAdapter for LlamaAdapter {
    fn engine_id(&self) -> &str {
        "llama"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let argv = build_args(&model.raw, LLAMA_ENGINE_HOST, port, &model.name)?;
        Ok(LaunchSpec {
            argv: [self.exe.to_string_lossy().into_owned()]
                .into_iter()
                .chain(argv)
                .collect(),
            cwd: self
                .exe
                .parent()
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
            env: BTreeMap::new(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200,
            },
            container: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn argv_mirrors_the_script_order_with_forced_host_and_port() {
        let args = build_args(
            &raw(&[
                ("model", "F:/m.gguf"),
                ("mmproj", "F:/mmproj.gguf"),
                ("context", "4096"),
                ("ngl", "99"),
                ("flash_attn", "1"),
                ("extra_args", "--embedding --pooling last"),
            ]),
            "127.0.0.1",
            18109,
            "test",
        )
        .expect("valid");
        assert_eq!(
            args,
            vec![
                "-m",
                "F:/m.gguf",
                "--mmproj",
                "F:/mmproj.gguf",
                "-c",
                "4096",
                "-ngl",
                "99",
                "--flash-attn",
                "1",
                "--embedding",
                "--pooling",
                "last",
                "--host",
                "127.0.0.1",
                "--port",
                "18109",
            ]
        );
    }

    /// The allow-list inversion must not touch what the harness itself emits: every
    /// `FLAG_TABLE` knob still reaches the argv in script order, byte-for-byte, with
    /// the forced pair still tailing it. (The same spellings are refused *inside*
    /// `extra_args` — see `harness_owned_and_dual_spelling_words_are_refused` — so
    /// each decision keeps exactly one yaml spelling.)
    #[test]
    fn every_flag_table_knob_still_reaches_the_argv() {
        let args = build_args(
            &raw(&[
                ("model", "F:/m.gguf"),
                ("mmproj", "F:/mmproj.gguf"),
                ("context", "4096"),
                ("ngl", "99"),
                ("batch", "512"),
                ("ubatch", "256"),
                ("flash_attn", "1"),
                ("cache_type_k", "q8_0"),
                ("cache_type_v", "q8_0"),
                ("parallel", "2"),
                ("fit", "true"),
                ("fit_target", "0.9"),
                ("fit_ctx", "2048"),
            ]),
            "127.0.0.1",
            18109,
            "test",
        )
        .expect("valid");
        assert_eq!(
            args,
            vec![
                "-m",
                "F:/m.gguf",
                "--mmproj",
                "F:/mmproj.gguf",
                "-c",
                "4096",
                "-ngl",
                "99",
                "-b",
                "512",
                "--ubatch-size",
                "256",
                "--flash-attn",
                "1",
                "--cache-type-k",
                "q8_0",
                "--cache-type-v",
                "q8_0",
                "--parallel",
                "2",
                "--fit",
                "true",
                "--fit-target",
                "0.9",
                "--fit-ctx",
                "2048",
                "--host",
                "127.0.0.1",
                "--port",
                "18109",
            ]
        );
    }

    #[test]
    fn missing_model_refuses_to_launch() {
        let error = build_args(&raw(&[]), "127.0.0.1", 18109, "ghost").expect_err("must refuse");
        assert!(matches!(error, EngineError::MissingField { .. }));
    }

    fn entry(raw_pairs: &[(&str, &str)]) -> ModelEntry {
        ModelEntry {
            name: "t".to_string(),
            engine: "llama".to_string(),
            category: crate::config::CategoryId::Work,
            vram_gb: None,
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            model_ref: None,
            serve_model: None,
            file: None,
            raw: raw(raw_pairs),
        }
    }

    #[test]
    fn spec_always_binds_the_forced_ipv4_loopback_host() {
        let adapter = LlamaAdapter::new(PathBuf::from("llama-server.exe"));
        // Nothing in the yaml can move --host off the loopback const: a `host`
        // key is not in FLAG_TABLE, and no `--host` spelling is on the extra_args
        // allow-list (refused by absence, not by name-matching).
        let model = entry(&[("model", "F:/m.gguf"), ("host", "0.0.0.0")]);
        let spec = adapter.spec(&model, 18109).expect("valid");
        let window = spec
            .argv
            .windows(2)
            .filter(|w| w[0] == "--host")
            .collect::<Vec<_>>();
        assert_eq!(window.len(), 1, "exactly one --host: {:?}", spec.argv);
        assert_eq!(window[0], ["--host", "127.0.0.1"]);
        assert!(!spec.argv.iter().any(|a| a == "0.0.0.0"));
        assert_eq!(
            spec.argv[spec.argv.len() - 4..],
            ["--host", "127.0.0.1", "--port", "18109"],
            "host/port must tail the argv, after every other word"
        );
        let offloaded = entry(&[
            ("model", "F:/m.gguf"),
            ("cpu_offload", "true"),
            ("extra_args", "--embedding --pooling last"),
        ]);
        let spec = adapter.spec(&offloaded, 18109).expect("valid");
        assert_eq!(
            spec.argv[spec.argv.len() - 6..],
            ["--host", "127.0.0.1", "--port", "18109", "-ngl", "0"],
            "forced host/port/-ngl 0 must tail every extra_args word"
        );
        assert_eq!(
            spec.argv[spec.argv.len() - 2..],
            ["-ngl", "0"],
            "forced -ngl 0 must be last of all"
        );
    }

    /// The reason text of the `Rejected` one `extra_args` value earns, or a panic
    /// naming the value that was wrongly admitted.
    fn refused_reason(extra: &str) -> String {
        let error = build_args(
            &raw(&[("model", "m"), ("extra_args", extra)]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect_err("a word outside the allow-list must be refused");
        let text = error.to_string();
        match error {
            EngineError::Rejected {
                engine,
                model,
                reason,
            } => {
                assert_eq!(engine, "llama");
                assert_eq!(model, "t");
                assert!(
                    text.contains(&reason),
                    "Display must carry the reason: {text}"
                );
                reason
            }
            other => panic!("expected Rejected for `{extra}`, got {other:?}"),
        }
    }

    /// FINDINGS2-0910 [1]'s probe, turned into the positive pin: the reviewer fed
    /// this exact line (with `cpu_offload: true`) to `LlamaAdapter::spec` and the
    /// pinned build kept every word. Now the launch dies naming the first policy
    /// word rather than booking a footprint for a server the yaml reconfigured.
    #[test]
    fn the_policy_bypass_probe_is_refused_naming_the_first_word() {
        let payload = "--api-key sekrit --rpc 10.0.0.9:50052 --path C:\\Users \
                       --models-dir D:\\weights --ssl-key-file F:\\k.pem \
                       --model-url http://evil/x.gguf --hf-repo evil/model:Q4_K_M \
                       --gpu-layers 99";
        let model = entry(&[("model", "F:/m.gguf"), ("cpu_offload", "true")]);
        let mut with_payload = model.clone();
        with_payload
            .raw
            .insert("extra_args".to_string(), payload.to_string());
        let error = LlamaAdapter::new(PathBuf::from("llama-server.exe"))
            .spec(&with_payload, 18109)
            .expect_err("no policy option may reach the argv");
        let EngineError::Rejected { reason, .. } = &error else {
            panic!("expected Rejected, got {error:?}");
        };
        assert!(reason.contains("`--api-key`"), "{reason}");
        assert!(
            !reason.contains("sekrit"),
            "the reason names the option, not the secret value: {reason}"
        );
    }

    /// Every word of [1]'s table, refused on its own, each named in the reason.
    #[test]
    fn each_policy_bypass_word_is_refused() {
        for (extra, word) in [
            ("--api-key sekrit", "--api-key"),
            ("--api-key=sekrit", "--api-key"),
            ("--api-key-file F:/k.key", "--api-key-file"),
            ("--rpc 10.0.0.9:50052", "--rpc"),
            ("--path C:\\Users", "--path"),
            ("--models-dir D:\\weights", "--models-dir"),
            ("--models-preset D:/p.ini", "--models-preset"),
            ("--models-autoload true", "--models-autoload"),
            ("--ssl-key-file F:/k.pem", "--ssl-key-file"),
            ("--ssl-cert-file F:/c.pem", "--ssl-cert-file"),
            ("--model-url http://evil/x.gguf", "--model-url"),
            ("--hf-repo evil/model:Q4_K_M", "--hf-repo"),
            ("--hf-file x", "--hf-file"),
            ("--docker-repo evil/x", "--docker-repo"),
            ("--reuse-port", "--reuse-port"),
            ("--gpu-layers 99", "--gpu-layers"),
            ("--no-jinja", "--no-jinja"),
            ("--lora D:/l.gguf", "--lora"),
            ("--override-kv x", "--override-kv"),
        ] {
            let reason = refused_reason(extra);
            assert!(
                reason.contains(word),
                "`{extra}`: reason must name `{word}`: {reason}"
            );
        }
    }

    /// The words the harness itself owns — including every alias spelling the old
    /// deny-list missed or invented — are refused by absence. `[2]`'s alias claims
    /// are therefore moot by construction: measured against the pinned binaries'
    /// option strings, `-ngl` has two long aliases (`--n-gpu-layers`, and
    /// `--gpu-layers`, which the old list missed), `-h` is the *help* alias rather
    /// than a host spelling, and `-p` is not an option in this build at all. A
    /// closed set makes spelling irrelevant: only membership counts.
    #[test]
    fn harness_owned_and_dual_spelling_words_are_refused() {
        for (extra, word) in [
            ("-m D:/evil.gguf", "-m"),
            ("--model D:/evil.gguf", "--model"),
            ("--host 0.0.0.0", "--host"),
            ("--host=0.0.0.0", "--host"),
            ("--port 8080", "--port"),
            ("--port=8080", "--port"),
            ("-h 0.0.0.0", "-h"),
            ("-p 8080", "-p"),
            ("-ngl 99", "-ngl"),
            ("-ngl=99", "-ngl"),
            ("--n-gpu-layers 99", "--n-gpu-layers"),
            ("--n-gpu-layers=99", "--n-gpu-layers"),
            ("--jinja", "--jinja"),
            ("--mmproj D:/evil.gguf", "--mmproj"),
            // FLAG_TABLE spellings: each decision has exactly one yaml key, and a
            // second spelling would make that key a dead claim.
            ("-c 4096", "-c"),
            ("-b 512", "-b"),
            ("--ubatch-size 256", "--ubatch-size"),
            ("--flash-attn 1", "--flash-attn"),
            ("--cache-type-k f16", "--cache-type-k"),
            ("--parallel 2", "--parallel"),
            ("--fit true", "--fit"),
            ("--fit-target 0.9", "--fit-target"),
            ("--fit-ctx 2048", "--fit-ctx"),
        ] {
            let reason = refused_reason(extra);
            assert!(
                reason.contains(word),
                "`{extra}`: reason must name `{word}`: {reason}"
            );
        }
    }

    /// Unknown-but-plausible words — the deny-list's old near-miss survivors and the
    /// look-alikes it deliberately let through — are now refused: absence from a
    /// closed set is the whole rule.
    #[test]
    fn unknown_but_plausible_words_are_refused() {
        for (extra, word) in [
            ("--foo", "--foo"),
            ("--bar=2", "--bar=2"),
            ("-ngld 5", "-ngld"),
            ("--model-draft D:/d.gguf", "--model-draft"),
            ("--portolan 1", "--portolan"),
            ("--pool last", "--pool"),
            ("--embeddingz", "--embeddingz"),
            ("--embeddings-off", "--embeddings-off"),
            ("-e", "-e"),
        ] {
            let reason = refused_reason(extra);
            assert!(
                reason.contains(word),
                "`{extra}`: reason must name `{word}`: {reason}"
            );
        }
    }

    /// An allowed flag still needs the value its arity promises: a dangling flag is
    /// a typo that would otherwise shift every later word by one position.
    #[test]
    fn an_allowed_flag_missing_its_value_is_refused() {
        for (extra, word) in [("--pooling", "--pooling"), ("--temp", "--temp")] {
            let reason = refused_reason(extra);
            assert!(reason.contains(word), "`{extra}`: {reason}");
            assert!(reason.contains("missing its value"), "{reason}");
        }
    }

    /// Values are data, with one floor: an option-shaped value is refused, because a
    /// parser that will not bind `--api-key` to the preceding flag would read it as
    /// its own option — the only way left around the list. A finite number is not
    /// option-shaped, so `--seed -1` (the engine's own default) stays legal.
    #[test]
    fn a_value_cannot_smuggle_another_option() {
        for (extra, word) in [
            ("--pooling --api-key", "--api-key"),
            ("--pooling=--api-key", "--pooling=--api-key"),
            ("--temp=--host", "--temp=--host"),
            ("--temp --host", "--host"),
            ("--seed -x", "-x"),
            ("--pooling=", "--pooling="),
            ("--embedding=1", "--embedding=1"),
        ] {
            let reason = refused_reason(extra);
            assert!(
                reason.contains(word),
                "`{extra}`: reason must name `{word}`: {reason}"
            );
        }
        refused_reason("--seed -");
        for (extra, value) in [
            ("--seed -1", "-1"),
            ("--seed=-1", "--seed=-1"),
            ("--temp -0.5", "-0.5"),
            ("--pooling=last", "--pooling=last"),
        ] {
            let args = build_args(
                &raw(&[("model", "m"), ("extra_args", extra)]),
                "127.0.0.1",
                18109,
                "t",
            )
            .unwrap_or_else(|error| panic!("`{extra}` must be admitted: {error}"));
            assert!(
                args.iter().any(|word| word == value),
                "`{extra}` lost `{value}`: {args:?}"
            );
        }
    }

    /// A refusal teaches the rule instead of hiding it: the reason carries every
    /// allowed word, generated from the const, so the operator-facing text can never
    /// drift from the list (and an accidentally empty list would refuse everything
    /// with an empty list to show for it).
    #[test]
    fn the_refusal_names_the_whole_closed_list() {
        let reason = refused_reason("--api-key sekrit");
        for (flag, _) in EXTRA_ARGS_ALLOWED {
            assert!(reason.contains(flag), "reason must list `{flag}`: {reason}");
        }
    }

    #[test]
    fn benign_extra_args_still_pass_through() {
        // The only real usage in the catalog (ModelConf/embeddinggemma-2-GGUF.yaml).
        let args = build_args(
            &raw(&[("model", "m"), ("extra_args", "--embedding --pooling last")]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("benign extra_args must be accepted");
        let window = args
            .windows(3)
            .any(|w| w == ["--embedding", "--pooling", "last"]);
        assert!(window, "extra_args missing from {args:?}");
    }

    #[test]
    fn cpu_offload_forced_ngl_zero_lands_after_extra_args() {
        let args = build_args(
            &raw(&[
                ("model", "m"),
                ("ngl", "99"),
                ("cpu_offload", "true"),
                ("extra_args", "--embedding --pooling last"),
            ]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("valid");
        let last_ngl = args
            .iter()
            .rposition(|a| a == "-ngl")
            .expect("forced -ngl present");
        assert_eq!(args[last_ngl + 1], "0");
        let extra_pos = args
            .iter()
            .rposition(|a| a == "--pooling")
            .expect("extra_args words present");
        assert!(
            last_ngl > extra_pos,
            "forced -ngl 0 must come after extra_args: {args:?}"
        );
        assert!(!args.iter().any(|a| a == "99"), "yaml ngl suppressed");
    }

    #[test]
    fn cpu_offload_forces_ngl_zero_and_suppresses_ngl() {
        let args = build_args(
            &raw(&[("model", "m"), ("ngl", "99"), ("cpu_offload", "true")]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("valid");
        let joined = args.join(" ");
        assert!(joined.contains("-ngl 0"), "was: {joined}");
        assert!(!joined.contains("99"));
    }

    #[test]
    fn jinja_follows_the_scripts_truth_set_not_generic_truthy() {
        for on in ["true", "1", "on"] {
            let args = build_args(
                &raw(&[("model", "m"), ("jinja", on)]),
                "127.0.0.1",
                18109,
                "t",
            )
            .expect("valid");
            assert!(
                args.contains(&"--jinja".to_string()),
                "{on} must enable jinja"
            );
        }
        // start.ps1: 'yes' is NOT in the truth set.
        let args = build_args(
            &raw(&[("model", "m"), ("jinja", "yes")]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("valid");
        assert!(!args.contains(&"--jinja".to_string()));
    }

    #[test]
    fn empty_values_are_skipped_but_zero_survives() {
        let args = build_args(
            &raw(&[("model", "m"), ("context", ""), ("ngl", "0")]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("valid");
        assert!(!args.contains(&"-c".to_string()));
        assert!(
            args.windows(2).any(|w| w == ["-ngl", "0"]),
            "ngl 0 must survive"
        );
    }

    /// A.7 parity: re-reads the operator's `start.ps1` and requires our table to
    /// match its key->flag lines exactly, so the harness cannot drift from the
    /// script the operator maintains. Skips (with a note) when the script is absent.
    #[test]
    fn flag_table_matches_the_operators_start_script() {
        let script = std::path::Path::new(r"C:\Inference\llama.cpp\start.ps1");
        let Ok(text) = std::fs::read_to_string(script) else {
            eprintln!("start.ps1 not found; skipping parity check");
            return;
        };
        let mut script_pairs: Vec<(String, String)> = Vec::new();
        for line in text.lines() {
            let Some((key, flag)) = extract_pair(line) else {
                continue;
            };
            // model and host are handled explicitly in build_args (-m first, forced
            // --host/--port); everything else must match the table one-to-one.
            if key != "model" && key != "host" {
                script_pairs.push((key, flag));
            }
        }
        let ours: Vec<(String, String)> = FLAG_TABLE
            .iter()
            .map(|(k, f)| (k.to_string(), f.to_string()))
            .collect();
        assert_eq!(script_pairs, ours, "A.7 parity: table must match start.ps1");
        assert!(
            text.contains("'true','1','on'"),
            "jinja truth set moved in start.ps1"
        );
    }

    /// `$v = $pairs['key']; if ($v) { $args += "flag", $v }` -> (key, flag).
    fn extract_pair(line: &str) -> Option<(String, String)> {
        let line = line.trim();
        let key = line
            .strip_prefix("$v = $pairs['")?
            .split("'];")
            .next()?
            .to_string();
        let flag = line
            .split(r#"$args += ""#)
            .nth(1)?
            .split(r#"", $v }"#)
            .next()?
            .to_string();
        if flag.is_empty() {
            return None;
        }
        Some((key, flag))
    }
}
