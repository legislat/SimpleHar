//! The llama.cpp adapter (DESIGN Step 7, Appendix A.7).
//!
//! The flag map is *data*, byte-equivalent in semantics to the operator's
//! `C:\Inference\llama.cpp\start.ps1:45-63` — the script is the source of truth, and
//! the parity test below re-reads it so the harness cannot drift. Two harness-level
//! special cases are documented extensions: `cpu_offload` (forces `-ngl 0`,
//! suppressing `ngl`) and the forced `--host`/`--port` pair.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::PathBuf;

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

/// Builds the llama-server argv from the raw yaml map, mirroring `start.ps1:45-63`:
/// `-m` first (required), mapped flags with non-empty values, the forced
/// `--host`/`--port`, `--jinja` when truthy, then `extra_args` split on whitespace.
///
/// # Errors
///
/// - [`EngineError::MissingField`] when `model` is absent or empty — the script
///   refuses to launch without it too.
pub fn build_args(
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
    let mut args = vec!["-m".to_string(), model.to_string()];
    // Script order (start.ps1:45-63): mmproj, then the forced host/port, then the
    // rest of the table, then the two special flags, then extra_args.
    for (key, flag) in FLAG_TABLE {
        if *key != "mmproj" {
            continue;
        }
        if let Some(v) = value(key) {
            args.extend([flag.to_string(), v.to_string()]);
        }
    }
    args.extend(["--host".to_string(), host.to_string()]);
    args.extend(["--port".to_string(), port.to_string()]);
    let cpu_offload = raw.get("cpu_offload").is_some_and(|v| jinja_on(v));
    for (key, flag) in FLAG_TABLE {
        if *key == "mmproj" {
            continue;
        }
        if *key == "ngl" && cpu_offload {
            continue; // A.7: cpu_offload forces -ngl 0 and suppresses the yaml ngl.
        }
        if let Some(v) = value(key) {
            args.extend([flag.to_string(), v.to_string()]);
        }
    }
    if cpu_offload {
        args.extend(["-ngl".to_string(), "0".to_string()]);
    }
    if raw.get("jinja").is_some_and(|v| jinja_on(v)) {
        args.push("--jinja".to_string());
    }
    if let Some(extra) = value("extra_args") {
        args.extend(extra.split_whitespace().map(str::to_string));
    }
    Ok(args)
}

/// llama.cpp adapter: argv from data (A.7), readiness `/health` 200 (v3.3 contract).
#[derive(Debug, Clone)]
pub struct LlamaAdapter {
    /// Path to `llama-server.exe` (config `llama_server_exe`).
    pub exe: PathBuf,
    /// Loopback address engines are forced to (`--host`).
    pub host: String,
}

impl LlamaAdapter {
    /// Creates the adapter for the configured binary and host.
    pub fn new(exe: PathBuf, host: String) -> Self {
        LlamaAdapter { exe, host }
    }
}

impl EngineAdapter for LlamaAdapter {
    fn engine_id(&self) -> &str {
        "llama"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let argv = build_args(&model.raw, &self.host, port, &model.name)?;
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
                "--host",
                "127.0.0.1",
                "--port",
                "18109",
                "-c",
                "4096",
                "-ngl",
                "99",
                "--flash-attn",
                "1",
                "--foo",
                "--bar=2",
            ]
        );
    }

    #[test]
    fn missing_model_refuses_to_launch() {
        let error = build_args(&raw(&[]), "127.0.0.1", 18109, "ghost").expect_err("must refuse");
        assert!(matches!(error, EngineError::MissingField { .. }));
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
