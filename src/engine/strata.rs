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
        // the child would otherwise die minutes into a model load).
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
        if !Path::new(config).exists() {
            return Err(rejected(format!("strata_config not found: {config}")));
        }
        let python = model
            .raw
            .get("strata_python")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(
                || self.python.to_string_lossy().into_owned(),
                |v| v.trim().to_string(),
            );
        if !Path::new(&python).exists() {
            return Err(rejected(format!("strata python not found: {python}")));
        }
        let cwd = model
            .raw
            .get("strata_cwd")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| self.default_cwd.clone(), |v| PathBuf::from(v.trim()));
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
            model_ref: Some("strata://test".to_string()),
            serve_model: Some("qwen3.8-flash-next".to_string()),
            file: None,
            raw: raw(pairs),
        }
    }

    /// A real file on disk stands in for the profile JSON and the interpreter:
    /// the adapter existence-checks both before handing a spec to the runtime.
    fn real_file(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("se-strata-{tag}-{}.json", std::process::id()));
        std::fs::write(&path, b"{}").expect("temp profile");
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
        let err = entry(&[("strata_config", config.to_str().expect("utf-8 path"))])
            .raw
            .get("strata_config")
            .map(|_| {
                StrataAdapter::new(
                    PathBuf::from("Z:\\nope\\python.exe"),
                    PathBuf::from(DEFAULT_CWD),
                )
                .spec(
                    &entry(&[("strata_config", config.to_str().expect("utf-8 path"))]),
                    18109,
                )
                .expect_err("must refuse")
            });
        assert!(matches!(&err, Some(EngineError::Rejected { reason, .. })
            if reason.starts_with("strata python not found:")));
        let _ = std::fs::remove_file(config);
    }
}
