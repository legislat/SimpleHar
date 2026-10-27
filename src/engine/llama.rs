//! The llama.cpp adapter (DESIGN Step 7, Appendix A.7).
//!
//! The flag map is *data*, byte-equivalent in semantics to the operator's
//! `C:\Inference\llama.cpp\start.ps1:45-63` — the script is the source of truth, and
//! the parity test below re-reads it so the harness cannot drift. Two harness-level
//! special cases are documented extensions: `cpu_offload` (forces `-ngl 0`,
//! suppressing `ngl`) and the forced `--host`/`--port` pair.
//!
//! Harness-owned decisions are enforced twice (FINDINGS-09 [1]/[2]): any
//! whitespace-separated `extra_args` word naming a harness-owned flag is refused
//! with [`EngineError::Rejected`], and the forced flags (`--host`, `--port`, and
//! `-ngl 0` under `cpu_offload`) are appended at the *tail* of the argv so they win
//! under any parser that honors last occurrence. `--host` is always the
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

/// Flags the harness owns; an `extra_args` word equal to one of these, or equal to
/// one of them followed by `=` (e.g. `--port=8080`), is a refused launch rather
/// than a silent override (FINDINGS-09 [2]). Includes llama.cpp's alias spellings
/// (`-h`, `-p`, `--n-gpu-layers`) so the refusal does not rest on the tail-forcing
/// last-occurrence assumption alone. Case-sensitive exact matching, no heuristics.
const HARNESS_OWNED_FLAGS: &[&str] = &[
    "-m",
    "--model",
    "--host",
    "-h",
    "--port",
    "-p",
    "-ngl",
    "--n-gpu-layers",
    "--jinja",
    "--mmproj",
];

/// Rejects any `extra_args` word that names a harness-owned flag.
///
/// # Errors
///
/// [`EngineError::Rejected`] naming the offending token.
fn check_extra_args(extra: &str, model_name: &str) -> Result<(), EngineError> {
    for word in extra.split_whitespace() {
        let owned = HARNESS_OWNED_FLAGS.iter().any(|flag| {
            word == *flag
                || word.starts_with(*flag) && word.as_bytes().get(flag.len()) == Some(&b'=')
        });
        if owned {
            return Err(EngineError::Rejected {
                engine: "llama".to_string(),
                model: model_name.to_string(),
                reason: format!("extra_args may not override harness-owned flag `{word}`"),
            });
        }
    }
    Ok(())
}

/// yaml key -> llama-server flag, in `start.ps1` order. Empty values are skipped,
/// exactly like the script's `if ($v)`.
const FLAG_TABLE: &[(&str, &str)] = &[
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

/// `jinja` truthiness per `start.ps1`: only `true`, `1`, `on` (case-sensitive
/// membership in the script's `-in 'true','1','on'`; note `yes` is *not* included).
/// Also the canonical read of `cpu_offload` — the engine's behavior, so the
/// control page's CPU checkbox uses THIS (not `model::truthy`) and can never
/// disagree with whether `-ngl 0` was actually forced (review finding, 2026-10-26).
#[must_use]
pub fn jinja_on(value: &str) -> bool {
    matches!(value, "true" | "1" | "on")
}

/// Builds the llama-server argv from the raw yaml map, mirroring `start.ps1:45-63`
/// (the deliberate deviation: the forced flags move to the tail): `-m` first
/// (required), `--mmproj`, the remaining mapped flags with non-empty
/// values, `--jinja` when truthy, then `extra_args` split on whitespace, and the
/// harness-forced flags at the tail (`--host`, `--port`, and `-ngl 0` when
/// `cpu_offload` is on) so forced values win under any parser that honors last
/// occurrence. Any `extra_args` word naming a harness-owned flag is refused
/// outright (see [`HARNESS_OWNED_FLAGS`]).
///
/// Crate-internal: `host` is only ever [`LLAMA_ENGINE_HOST`] in production (from
/// [`LlamaAdapter::spec`]); a public host knob would be the struct-literal bypass
/// `ff235e6` deleted from the adapter, back in parameter form.
///
/// # Errors
///
/// - [`EngineError::MissingField`] when `model` is absent or empty — the script
///   refuses to launch without it too.
/// - [`EngineError::Rejected`] when an `extra_args` word matches a harness-owned
///   flag, naming the offending token.
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
    if let Some(extra) = value("extra_args") {
        check_extra_args(extra, model_name)?;
    }
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
    if let Some(extra) = value("extra_args") {
        args.extend(extra.split_whitespace().map(str::to_string));
    }
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
                ("extra_args", "--foo  --bar=2"),
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
                "--foo",
                "--bar=2",
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
        // key is not in FLAG_TABLE, and an extra_args override is refused.
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

    #[test]
    fn extra_args_naming_a_harness_owned_flag_is_refused() {
        for (extra, token) in [
            ("-m D:/evil.gguf", "-m"),
            ("--model D:/evil.gguf", "--model"),
            ("--host 0.0.0.0", "--host"),
            ("-h 0.0.0.0", "-h"),
            ("--port 8080", "--port"),
            ("-p 8080", "-p"),
            ("-ngl 99", "-ngl"),
            ("--n-gpu-layers 99", "--n-gpu-layers"),
            ("--jinja", "--jinja"),
            ("--mmproj D:/evil.gguf", "--mmproj"),
            ("--port=8080", "--port"),
            ("--host=0.0.0.0", "--host"),
            ("-ngl=99", "-ngl"),
            ("--n-gpu-layers=99", "--n-gpu-layers"),
        ] {
            let error = build_args(
                &raw(&[("model", "m"), ("extra_args", extra)]),
                "127.0.0.1",
                18109,
                "t",
            )
            .expect_err("harness-owned flag must be refused");
            let EngineError::Rejected {
                engine,
                model,
                reason,
            } = &error
            else {
                panic!("expected Rejected for {extra}, got {error:?}");
            };
            assert_eq!(engine, "llama");
            assert_eq!(model, "t");
            assert!(reason.contains(token), "reason must name {token}: {reason}");
            assert!(
                error.to_string().contains(token),
                "Display must name {token}: {error}"
            );
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
    fn near_miss_words_are_not_refused() {
        // The `=` matcher requires the byte right after the flag, so look-alikes
        // of harness-owned flags must pass (review finding [1] false-positive guard).
        let args = build_args(
            &raw(&[
                ("model", "m"),
                ("extra_args", "-ngld 5 --model-draft D:/d.gguf --portolan 1"),
            ]),
            "127.0.0.1",
            18109,
            "t",
        )
        .expect("near-miss words must be accepted");
        assert!(args.windows(2).any(|w| w == ["-ngld", "5"]));
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
