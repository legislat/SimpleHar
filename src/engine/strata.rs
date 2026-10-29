//! The strata engine adapter (DESIGN Step 7; port of the working v3.3 path).
//!
//! Strata is a Python server (`serve.server`) driven entirely by a profile JSON:
//! the harness passes only the interpreter, the module, the `--config` path and
//! the harness-assigned `--port` — weights, quant, context and cache behavior all
//! live in the profile. Readiness needs the JSON `loaded` flag because strata's
//! `/health` answers 200 while the weights are still streaming onto the card
//! (v3.3 `engine.rs:208-226` parity).
//!
//! There is no preload in this harness (engine work is on-demand only), and the
//! yamls carry no autostart keys: a strata engine spawns only when an operator
//! loads the model by name.
//!
//! What the adapter validates of the three yaml strings it passes on (FINDINGS2-0910
//! [5] floor, 2026-10-29): `strata_config` must be an existing file, `strata_python`
//! an existing `.exe` **file** (it is the launched program), and `strata_cwd` an
//! existing directory. That is "refuse garbage early", not custody: the profile's
//! *contents* are never policy-checked, and the roots that decide which paths are
//! admissible at all are the `custody_roots` column of chunk 10's `EnginePolicy` row
//! with the values moving to the config's `engines:` section (§20-9 Fork C).

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Fallback interpreter when a yaml omits `strata_python:` (v3.3 default).
const DEFAULT_PYTHON: &str = r"C:\Strata\.venv\Scripts\python.exe";
/// Fallback working directory when a yaml omits `strata_cwd:` (v3.3 default).
const DEFAULT_CWD: &str = r"C:\Strata";

/// Strata adapter: argv from data, `/health` 200 **and** `loaded != false`.
#[derive(Debug, Clone)]
pub struct StrataAdapter {
    /// Default interpreter (yaml `strata_python:` overrides per model).
    pub python: PathBuf,
    /// Default working directory (yaml `strata_cwd:` overrides per model).
    pub default_cwd: PathBuf,
}

impl Default for StrataAdapter {
    fn default() -> Self {
        StrataAdapter {
            python: PathBuf::from(DEFAULT_PYTHON),
            default_cwd: PathBuf::from(DEFAULT_CWD),
        }
    }
}

impl StrataAdapter {
    /// Creates the adapter with explicit defaults (tests, alternate installs).
    pub fn new(python: PathBuf, default_cwd: PathBuf) -> Self {
        StrataAdapter {
            python,
            default_cwd,
        }
    }

    /// The argv exactly as v3.3 built it: interpreter, module, engine, profile,
    /// harness-assigned port. Nothing else reaches the child.
    fn argv(&self, python: &str, config: &str, port: u16) -> Vec<String> {
        vec![
            python.to_string(),
            "-m".to_string(),
            "serve.server".to_string(),
            "--engine".to_string(),
            "strata".to_string(),
            "--config".to_string(),
            config.to_string(),
            "--port".to_string(),
            port.to_string(),
        ]
    }
}

impl EngineAdapter for StrataAdapter {
    fn engine_id(&self) -> &str {
        "strata"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let rejected = |reason: String| EngineError::Rejected {
            engine: "strata".to_string(),
            model: model.name.clone(),
            reason,
        };
        // Required profile JSON, existence-checked before spawn (v3.3 parity:
        // the child would otherwise die minutes into a model load). It must also be
        // a FILE: a directory named `x.json` is the same doomed spawn one step
        // later, and the python child's own error would arrive after admission
        // already evicted a resident engine (FINDINGS2-0910 [5] floor).
        let Some(config) = model
            .raw
            .get("strata_config")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
        else {
            return Err(EngineError::MissingField {
                model: model.name.clone(),
                field: "strata_config".to_string(),
            });
        };
        let config_path = Path::new(config);
        if !config_path.is_file() {
            return Err(rejected(if config_path.exists() {
                format!("strata_config is not a file: {config}")
            } else {
                format!("strata_config not found: {config}")
            }));
        }
        // The interpreter is `argv[0]`, so it is the one yaml string that chooses
        // which program CreateProcessW starts. Refuse garbage early rather than
        // start whatever path happens to exist: an existing FILE whose name ends in
        // `.exe`, matching how the llama adapter refuses a missing binary. The
        // rest of this cell — an interpreter under a config-declared root, the
        // profile under a declared profile root, policy keys inside the profile
        // itself — is the `custody_roots` column of chunk 10's `EnginePolicy` row
        // and the `engines:` config section (`DECISIONS` §20-9 Fork C), not here;
        // a batch shim renamed to `.exe` therefore still needs that ruling, which
        // is why this is called a floor and not a control.
        let python = model
            .raw
            .get("strata_python")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(
                || self.python.to_string_lossy().into_owned(),
                |v| v.trim().to_string(),
            );
        let python_path = Path::new(&python);
        if !python_path.exists() {
            return Err(rejected(format!("strata_python not found: {python}")));
        }
        if !python_path.is_file() {
            return Err(rejected(format!(
                "strata_python is not a file (a directory cannot be the launched program): \
                 {python}"
            )));
        }
        if !python_path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err(rejected(format!(
                "strata_python must be an existing .exe file: {python}"
            )));
        }
        // The cwd is handed to `Command::current_dir`, and a missing one makes the
        // spawn fail as an opaque `cannot spawn` (FINDINGS2-0910 [5]): refuse it
        // here, with the key the operator has to edit named.
        let cwd = model
            .raw
            .get("strata_cwd")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| self.default_cwd.clone(), |v| PathBuf::from(v.trim()));
        if !cwd.is_dir() {
            return Err(rejected(format!(
                "strata_cwd is not an existing directory: {}",
                cwd.display()
            )));
        }
        Ok(LaunchSpec {
            argv: self.argv(&python, config, port),
            cwd,
            env: BTreeMap::new(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
            ),
            container: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(pairs: &[(&str, &str)]) -> ModelEntry {
        ModelEntry {
            name: "strata-test".to_string(),
            engine: "strata".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(55.0),
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            model_ref: Some("strata://test".to_string()),
            serve_model: Some("qwen3.8-flash-next".to_string()),
            file: None,
            raw: raw(pairs),
        }
    }

    /// A real file on disk stands in for the profile JSON and the interpreter:
    /// the adapter existence-checks both before handing a spec to the runtime.
    fn real_file(tag: &str) -> PathBuf {
        real_file_with_extension(tag, "json")
    }

    /// As [`real_file`], with the extension under test: the interpreter rule is an
    /// extension rule, so a "file that exists" fixture must be able to be a `.cmd`.
    fn real_file_with_extension(tag: &str, extension: &str) -> PathBuf {
        let path = crate::testutil::temp_dir("strata").join(format!("{tag}.{extension}"));
        std::fs::write(&path, b"{}").expect("temp file");
        path
    }

    #[test]
    fn argv_matches_the_v33_contract() {
        let config = real_file("argv");
        let spec = StrataAdapter::default()
            .spec(
                &entry(&[("strata_config", config.to_str().expect("utf-8 path"))]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv,
            vec![
                DEFAULT_PYTHON,
                "-m",
                "serve.server",
                "--engine",
                "strata",
                "--config",
                config.to_str().expect("utf-8 path"),
                "--port",
                "18109",
            ]
        );
        assert_eq!(spec.cwd, PathBuf::from(DEFAULT_CWD));
        assert_eq!(spec.log_name, "strata-test");
        assert!(spec.env.is_empty());
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
            )
        );
        let _ = std::fs::remove_file(config);
    }

    #[test]
    fn yaml_overrides_interpreter_and_cwd() {
        let config = real_file("override");
        let python = std::env::current_exe().expect("a real interpreter stand-in");
        let cwd = std::env::temp_dir();
        let spec = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", python.to_str().expect("utf-8 path")),
                    ("strata_cwd", cwd.to_str().expect("utf-8 path")),
                ]),
                18110,
            )
            .expect("valid spec");
        assert_eq!(spec.argv[0], python.to_str().expect("utf-8 path"));
        assert_eq!(spec.argv[8], "18110");
        assert_eq!(spec.cwd, cwd);
        let _ = std::fs::remove_file(config);
    }

    /// Pin of the `cpu_offload` engine scoping: the strata adapter reads exactly
    /// `strata_config`/`strata_python`/`strata_cwd` — the key is inert for this
    /// engine, so absent, `true` and `false` must produce a byte-identical
    /// [`LaunchSpec`] (argv, env, readiness, cwd, container alike). Falsifier: if
    /// this adapter starts reading the key, the equality fails here.
    #[test]
    fn cpu_offload_changes_nothing_for_a_strata_launch() {
        let config = real_file("inert-offload");
        let path = config.to_str().expect("utf-8 path");
        let base = StrataAdapter::default()
            .spec(&entry(&[("strata_config", path)]), 18109)
            .expect("absent key");
        for value in ["true", "false"] {
            let spec = StrataAdapter::default()
                .spec(
                    &entry(&[("strata_config", path), ("cpu_offload", value)]),
                    18109,
                )
                .expect("an inert key never refuses a launch");
            assert_eq!(
                spec, base,
                "cpu_offload: {value} must change nothing — if the strata adapter \
                 starts reading the key, this fails"
            );
        }
        let _ = std::fs::remove_file(config);
    }

    #[test]
    fn missing_profile_is_a_missing_field_refusal() {
        let err = StrataAdapter::default()
            .spec(&entry(&[]), 18109)
            .expect_err("must refuse");
        assert!(matches!(
            err,
            EngineError::MissingField { ref field, .. } if field == "strata_config"
        ));
    }

    #[test]
    fn absent_profile_or_interpreter_is_rejected_before_spawn() {
        let err = StrataAdapter::default()
            .spec(
                &entry(&[("strata_config", "Z:\\nope\\missing.json")]),
                18109,
            )
            .expect_err("must refuse");
        assert!(matches!(&err, EngineError::Rejected { reason, .. }
            if reason.starts_with("strata_config not found:")));

        let config = real_file("badpy");
        let err = StrataAdapter::new(
            PathBuf::from("Z:\\nope\\python.exe"),
            PathBuf::from(DEFAULT_CWD),
        )
        .spec(
            &entry(&[("strata_config", config.to_str().expect("utf-8 path"))]),
            18109,
        )
        .expect_err("must refuse");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
            if reason.starts_with("strata_python not found:")),
            "{err}"
        );
        let _ = std::fs::remove_file(config);
    }

    /// FINDINGS2-0910 [5]'s probe, turned into a pin: `argv[0]` is the program
    /// CreateProcessW starts, and an existing batch file is not an interpreter. The
    /// reviewer's `python.cmd` must be refused naming the yaml key.
    #[test]
    fn a_batch_file_is_refused_as_the_interpreter() {
        let config = real_file("cmd-python-config");
        let shim = real_file_with_extension("cmd-python", "cmd");
        let err = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", shim.to_str().expect("utf-8 path")),
                ]),
                18109,
            )
            .expect_err("a .cmd cannot be the launched program");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("strata_python") && reason.contains(".exe")),
            "{err}"
        );
        let _ = std::fs::remove_file(config);
        let _ = std::fs::remove_file(shim);
    }

    /// A directory that exists is not a program either: `Path::exists()` alone would
    /// have accepted it as `strata_python`.
    #[test]
    fn a_directory_is_refused_as_the_interpreter() {
        let config = real_file("dir-python-config");
        let dir = crate::testutil::temp_dir("strata-python-dir");
        std::fs::create_dir_all(&dir).expect("temp dir as interpreter");
        let err = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", dir.to_str().expect("utf-8 path")),
                ]),
                18109,
            )
            .expect_err("a directory cannot be the launched program");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("strata_python") && reason.contains("not a file")),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(config);
    }

    /// The cwd reaches `Command::current_dir`: today a missing one surfaces only as
    /// `cannot spawn` from the runtime (10[6]'s opaque failure, [5]'s companion).
    #[test]
    fn a_missing_or_non_directory_cwd_is_refused_at_spec_time() {
        let config = real_file("cwd-config");
        let path = config.to_str().expect("utf-8 path");
        let as_file = real_file_with_extension("cwd-not-a-dir", "json");
        for (value, expect) in [
            ("Z:\\no-such-dir", "not an existing directory"),
            (file_str(&as_file), "not an existing directory"),
        ] {
            let err = StrataAdapter::default()
                .spec(
                    &entry(&[("strata_config", path), ("strata_cwd", value)]),
                    18109,
                )
                .expect_err("a cwd the child cannot enter must not be launched");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("strata_cwd") && reason.contains(expect)),
                "{value}: {err}"
            );
        }
        let _ = std::fs::remove_file(config);
        let _ = std::fs::remove_file(as_file);
    }

    fn file_str(path: &std::path::Path) -> &str {
        path.to_str().expect("utf-8 path")
    }

    /// The floor refuses garbage, it does not narrow legitimate config: the real
    /// interpreter + profile + cwd trio still produces the exact v3.3 spec.
    #[test]
    fn a_valid_interpreter_profile_and_cwd_still_launch() {
        let config = real_file("valid-trio");
        let python = std::env::current_exe().expect("a real .exe stand-in");
        let spec = StrataAdapter::new(python.clone(), std::env::temp_dir())
            .spec(&entry(&[("strata_config", file_str(&config))]), 18109)
            .expect("a real trio must launch");
        assert_eq!(spec.argv[0], python.to_str().expect("utf-8 path"));
        assert_eq!(spec.cwd, std::env::temp_dir());
        let _ = std::fs::remove_file(config);
    }
}
