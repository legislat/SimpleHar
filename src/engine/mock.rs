//! The mock engine adapter (DESIGN Step 19): everything in Steps 2-8 is testable
//! without touching the 72 GB card.
//!
//! Test-facing only. Production boot registers this adapter in the scheduler's map solely
//! when config `allow_mock_engine` is true — §5 E3-adjacent doctrine: engines obey only
//! harness-owned config. The reason it needs an opt-in is the launch spec below: its argv is
//! the bare program name `mock-engine` with cwd `.`, so an ungated "mock" engine would let a
//! stray yaml win admission against a real model and let a planted `mock-engine.exe` beside
//! the harness exe run as a job-ordained child. Integration tests never set the flag; each
//! builds its own adapter map.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// In-process fake engine: a launch spec that names the model and answers `/health`.
#[derive(Debug, Clone, Default)]
pub struct MockAdapter;

impl EngineAdapter for MockAdapter {
    fn engine_id(&self) -> &str {
        "mock"
    }

    fn spec(&self, model: &ModelEntry, _port: u16) -> Result<LaunchSpec, EngineError> {
        if model.name.trim().is_empty() {
            return Err(EngineError::MissingField {
                model: model.name.clone(),
                field: "name".to_string(),
            });
        }
        Ok(LaunchSpec {
            argv: vec![
                "mock-engine".to_string(),
                "--name".to_string(),
                model.name.clone(),
            ],
            cwd: PathBuf::from("."),
            env: BTreeMap::from([("MOCK_ENGINE".to_string(), model.engine.clone())]),
            log_name: format!("mock-{}", model.name),
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
    use crate::config::CategoryId;

    fn entry(name: &str) -> ModelEntry {
        ModelEntry {
            name: name.to_string(),
            engine: "mock".to_string(),
            category: CategoryId::Work,
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
            raw: BTreeMap::new(),
        }
    }

    #[test]
    fn builds_a_launch_spec_naming_the_model() {
        let spec = MockAdapter
            .spec(&entry("qwen3"), 18109)
            .expect("valid model");
        assert_eq!(spec.argv, vec!["mock-engine", "--name", "qwen3"]);
        assert_eq!(spec.log_name, "mock-qwen3");
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200
            }
        );
    }

    #[test]
    fn empty_name_is_a_missing_field_refusal() {
        let err = MockAdapter
            .spec(&entry(" "), 18109)
            .expect_err("blank name");
        assert!(matches!(err, EngineError::MissingField { .. }));
    }
}
