//! Typed per-model configuration from `ModelConf/*.yaml` (DESIGN Steps 9-10).
//!
//! The v3.3 files are flat `key: value` documents. This module owns the typed entry and
//! the documented alias rules; byte-preserving round-trip write-back arrives with the
//! scan/write milestone (Step 9).

pub mod scan;
pub mod writeback;

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::config::CategoryId;
use thiserror::Error;

/// Problems building a [`ModelEntry`] from raw yaml keys; collected per file at scan
/// time and exposed at `/har/status` — a bad file is skipped and named, never fatal.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ModelError {
    /// The `name:` key is absent or empty; the harness name is the canonical id (Step 10).
    #[error("missing or empty `name` key")]
    MissingName,
    /// `category:` is not one of WORK|RP|AUX|EMB.
    #[error("bad `category` value `{value}`")]
    BadCategory {
        /// The unparsable value as it appeared in the file.
        value: String,
    },
    /// `is_embedding_model: true` on a model whose explicit category is not EMB.
    #[error("category `{value}` conflicts with `is_embedding_model: true` (must be EMB)")]
    EmbeddingCategoryConflict {
        /// The explicit, non-EMB category value.
        value: String,
    },
    /// A numeric key could not be parsed.
    #[error("field `{field}` is not a number: `{value}`")]
    BadNumber {
        /// Field name.
        field: String,
        /// Raw value.
        value: String,
    },
    /// `cost:` is not one of fixed|greedy|exact.
    #[error("bad `cost` value `{value}` (fixed|greedy|exact)")]
    BadCost {
        /// The unparsable value as it appeared in the file.
        value: String,
    },
}

/// How a model's declared size participates in admission (operator design note,
/// 2026-10-26: engines grade differently — some load everything into VRAM,
/// some offload shards to RAM/SSD greedily).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CostClass {
    /// Nominal size is binding: `need > free` evicts or refuses (the classic gate).
    Fixed,
    /// Greedy offloader (strata-style): nominal size never refuses — the engine
    /// spills to RAM/SSD. Protected at launch by the VRAM floor gate: spawning a
    /// huge quant onto an already-busy card thrashes RAM/SSD for minutes (G-admission note).
    Greedy,
    /// The engine's computed requirement (ninfer-style): binding like `fixed`,
    /// but never estimated — a missing `vram_gb` is refused rather than guessed
    /// (the size×1.15+2 fallback would be a precision claim with no source).
    /// A harness measurement VERIFIES this class at install time (a diverging
    /// `measured_gb` logs a warning) instead of overriding it as it does for
    /// `fixed`.
    Exact,
}

/// One model from `ModelConf/`, typed (Step 10: harness name is canonical).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelEntry {
    /// Canonical harness name (`name:`); the `model` field clients send.
    pub name: String,
    /// Engine id (`engine:`), defaulting to `llama`; may name a template (Step 7).
    pub engine: String,
    /// Slot category after alias resolution.
    pub category: CategoryId,
    /// Declared admission cost in GiB (Step 4 rule 1); `None` falls back to the
    /// GGUF-size formula.
    pub vram_gb: Option<f64>,
    /// GiB size of the backing GGUF file, recorded by the scanner (None for
    /// sentinels, missing files, and synthetic entries) — the estimate input at
    /// admission (DESIGN Step 4 rule 1).
    pub gguf_size_gb: Option<f64>,
    /// How `vram_gb` participates in admission (`cost:` key, default `fixed`).
    pub cost: CostClass,
    /// `launch_only_with_at_least_vram_free:` — refuse the load **before spawn**
    /// unless VRAM free at decision time reaches this floor in GiB. The guard for
    /// greedy offloaders: launching a huge quant onto a busy card thrashes RAM/SSD.
    pub launch_floor_gb: Option<f64>,
    /// Per-model readiness deadline in seconds (`load_timeout:`); `None` uses the
    /// config default. Strata-class cold starts run for many minutes and would
    /// otherwise be cut off by the global gate (v3.3 parity).
    pub load_timeout_s: Option<u64>,
    /// Engine footprint measured by the harness at its last successful load
    /// (`measured_gb:`, written back after Install). The page shows it as truth
    /// for every cost class; admission consults it only for `fixed` models with
    /// no declared `vram_gb` (greedy keeps nominal semantics; exact refuses
    /// without its declared precision claim).
    pub measured_gb: Option<f64>,
    /// Raw `model:` value — GGUF path or the engine-backed `scheme://` sentinel.
    /// Informational for managed models; used for stem/path aliases (Step 10).
    pub model_ref: Option<String>,
    /// The id the yaml claims the engine reports (`serve_model:`); verified by
    /// discovery at readiness — a mismatch is a recorded warning, never trusted
    /// (F-4 fix, Step 10).
    pub serve_model: Option<String>,
    /// File the entry came from; `None` for synthetic entries (tests, mock).
    pub file: Option<PathBuf>,
    /// Every raw `key: value` from the source file, verbatim — the passthrough the
    /// llama adapter builds argv from (Step 9 raw map; write-back keeps it honest).
    pub raw: BTreeMap<String, String>,
}

/// v3.3-compatible boolean-ish reading of a flat yaml value. Exact parity with the
/// baseline `truthy` table is pinned by a unit test when the scan milestone ports it.
pub(crate) fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// The one GiB parser for every size-valued yaml key. Rust's `f64` parse accepts
/// `nan`/`inf`/`1e400`, and every NaN comparison is false — an unchecked value
/// would sail through `< 0.0` guards and silently disable the gates it feeds
/// (review, 2026-10-26). Finite and non-negative, or it is a scan error.
fn parse_gib(field: &str, raw: &str) -> Result<f64, ModelError> {
    let v = raw
        .trim()
        .parse::<f64>()
        .map_err(|_| ModelError::BadNumber {
            field: field.to_string(),
            value: raw.to_string(),
        })?;
    if !v.is_finite() || v < 0.0 {
        return Err(ModelError::BadNumber {
            field: field.to_string(),
            value: raw.to_string(),
        });
    }
    Ok(v)
}

/// Which figure admission will book for an entry. Decided once, here, and read by
/// both the gate and the page's Cost column — two derivations of this precedence is
/// how the page could show a number the gate ignored (duplication review, 2026-10-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// The declared `vram_gb`.
    Declared,
    /// The harness-measured footprint of an earlier run (`fixed` only).
    Measured,
    /// GGUF size × 1.15 + 2.
    Estimate,
    /// `exact` with nothing declared: refused, never guessed.
    Undeclared,
}

impl ModelEntry {
    /// The booking precedence: declared → measured (`fixed` only) → GGUF estimate,
    /// with `exact`-without-`vram_gb` refused.
    #[must_use]
    pub fn cost_source(&self) -> CostSource {
        if self.vram_gb.is_some() {
            return CostSource::Declared;
        }
        if self.cost == CostClass::Exact {
            return CostSource::Undeclared;
        }
        if self.cost == CostClass::Fixed && self.measured_gb.is_some() {
            return CostSource::Measured;
        }
        CostSource::Estimate
    }
}

impl ModelEntry {
    /// Builds an entry from a flat `key: value` map (the v3.3 yaml shape), applying the
    /// documented aliases: `is_embedding_model: true` forces EMB, `sticky: true` maps
    /// to AUX when no explicit `category:` is present, and the default is WORK
    /// (Step 5, Step 9).
    ///
    /// # Errors
    ///
    /// - [`ModelError::MissingName`] when `name` is absent/empty.
    /// - [`ModelError::BadCategory`] / [`ModelError::EmbeddingCategoryConflict`] on an
    ///   unusable category combination.
    /// - [`ModelError::BadNumber`] when `vram_gb` is not a number.
    pub fn from_flat(keys: &BTreeMap<String, String>) -> Result<Self, ModelError> {
        let name = keys.get("name").map(String::as_str).unwrap_or_default();
        if name.trim().is_empty() {
            return Err(ModelError::MissingName);
        }
        let embedding = keys.get("is_embedding_model").is_some_and(|v| truthy(v));
        let category = match keys.get("category") {
            Some(raw) => {
                let parsed: CategoryId = raw
                    .parse()
                    .map_err(|_| ModelError::BadCategory { value: raw.clone() })?;
                if embedding && parsed != CategoryId::Emb {
                    return Err(ModelError::EmbeddingCategoryConflict { value: raw.clone() });
                }
                parsed
            }
            None if embedding => CategoryId::Emb,
            None if keys.get("sticky").is_some_and(|v| truthy(v)) => CategoryId::Aux,
            None => CategoryId::Work,
        };
        let vram_gb = match keys.get("vram_gb") {
            Some(raw) => Some(parse_gib("vram_gb", raw)?),
            None => None,
        };
        let cost = match keys.get("cost") {
            Some(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "fixed" => CostClass::Fixed,
                "greedy" => CostClass::Greedy,
                "exact" => CostClass::Exact,
                other => {
                    return Err(ModelError::BadCost {
                        value: other.to_string(),
                    })
                }
            },
            None => CostClass::Fixed,
        };
        let launch_floor_gb = match keys.get("launch_only_with_at_least_vram_free") {
            Some(raw) => Some(parse_gib("launch_only_with_at_least_vram_free", raw)?),
            None => None,
        };
        // u64 parse rejects nan/inf/1e400 outright; the ceiling guards a typo that
        // would park a load job in readiness forever.
        let load_timeout_s = match keys.get("load_timeout") {
            Some(raw) => {
                let v = raw
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| ModelError::BadNumber {
                        field: "load_timeout".to_string(),
                        value: raw.to_string(),
                    })?;
                if v == 0 || v > 86_400 {
                    return Err(ModelError::BadNumber {
                        field: "load_timeout".to_string(),
                        value: raw.to_string(),
                    });
                }
                Some(v)
            }
            None => None,
        };
        Ok(ModelEntry {
            name: name.trim().to_string(),
            engine: keys
                .get("engine")
                .cloned()
                .unwrap_or_else(|| "llama".to_string()),
            category,
            vram_gb,
            gguf_size_gb: None,
            cost,
            launch_floor_gb,
            load_timeout_s,
            measured_gb: match keys.get("measured_gb") {
                Some(raw) => {
                    let v = parse_gib("measured_gb", raw)?;
                    if v < 0.05 {
                        // Below the writer's 0.1 quantum: a footprint this small
                        // means nothing was attributed — absence, not a measurement.
                        return Err(ModelError::BadNumber {
                            field: "measured_gb".to_string(),
                            value: raw.to_string(),
                        });
                    }
                    Some(v)
                }
                None => None,
            },
            model_ref: keys.get("model").cloned(),
            serve_model: keys.get("serve_model").filter(|v| !v.is_empty()).cloned(),
            file: None,
            // v3.3's `external: true` is scrapped (operator 2026-10-26): skipped here
            // so a copied-back v3.3 yaml cannot keep advertising a meaning nothing has,
            // and cannot resurrect it through the raw passthrough on write-back. The
            // operator's file bytes are left alone — only the parsed entry drops it.
            raw: keys
                .iter()
                .filter(|(key, _)| key.as_str() != "external")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn defaults_to_work_and_llama() {
        let entry = ModelEntry::from_flat(&flat(&[("name", "qwen3"), ("model", "F:/m.gguf")]))
            .expect("valid entry");
        assert_eq!(entry.category, CategoryId::Work);
        assert_eq!(entry.engine, "llama");
        assert_eq!(entry.model_ref.as_deref(), Some("F:/m.gguf"));
    }

    #[test]
    fn cost_source_mirrors_the_booking_precedence() {
        let declared =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("vram_gb", "10")])).expect("valid");
        assert_eq!(declared.cost_source(), CostSource::Declared);
        // `fixed` honours a measured footprint; `greedy` never does (a post-spill
        // measurement is not a nominal size).
        let measured = ModelEntry::from_flat(&flat(&[
            ("name", "m"),
            ("cost", "fixed"),
            ("measured_gb", "7"),
        ]))
        .expect("valid");
        assert_eq!(measured.cost_source(), CostSource::Measured);
        let greedy = ModelEntry::from_flat(&flat(&[
            ("name", "m"),
            ("cost", "greedy"),
            ("measured_gb", "7"),
        ]))
        .expect("valid");
        assert_eq!(greedy.cost_source(), CostSource::Estimate);
        // `exact` without a declared number is refused, never estimated.
        let exact =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("cost", "exact")])).expect("valid");
        assert_eq!(exact.cost_source(), CostSource::Undeclared);
    }

    #[test]
    fn retired_external_key_is_skipped_from_the_raw_map() {
        // v3.3 `external: true` is scrapped: it must not survive into the raw
        // passthrough, where it would advertise a meaning nothing has and be written
        // back out as if it were live config. Every other key still passes through.
        let entry = ModelEntry::from_flat(&flat(&[("name", "m"), ("external", "true")]))
            .expect("valid entry");
        assert!(!entry.raw.contains_key("external"));
        let kept =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("custom", "x")])).expect("valid entry");
        assert_eq!(kept.raw.get("custom").map(String::as_str), Some("x"));
    }

    #[test]
    fn measured_gb_parses_and_rejects_garbage() {
        let e = ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "70.5")]))
            .expect("valid entry");
        assert_eq!(e.measured_gb, Some(70.5));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "abc")]))
                .expect_err("garbage"),
            ModelError::BadNumber { .. }
        ));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0")]))
                .expect_err("zero is not a measurement"),
            ModelError::BadNumber { .. }
        ));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0.04")]))
                .expect_err("sub-quantum is not a measurement"),
            ModelError::BadNumber { .. }
        ));
        assert_eq!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0.05")]))
                .expect("one quantum is")
                .measured_gb,
            Some(0.05)
        );
    }

    #[test]
    fn load_timeout_parses_and_bounds() {
        let entry = ModelEntry::from_flat(&flat(&[("name", "s"), ("load_timeout", "1200")]))
            .expect("valid entry");
        assert_eq!(entry.load_timeout_s, Some(1200));
        for bad in ["0", "86401", "abc", "-5", "nan", "1e400"] {
            let err = ModelEntry::from_flat(&flat(&[("name", "s"), ("load_timeout", bad)]))
                .expect_err("{bad} must be refused");
            assert!(
                matches!(err, ModelError::BadNumber { ref field, .. } if field == "load_timeout"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn aliases_sticky_to_aux_and_embedding_to_emb() {
        let sticky = ModelEntry::from_flat(&flat(&[("name", "a"), ("sticky", "true")]))
            .expect("valid entry");
        assert_eq!(sticky.category, CategoryId::Aux);

        let embed = ModelEntry::from_flat(&flat(&[("name", "b"), ("is_embedding_model", "true")]))
            .expect("valid entry");
        assert_eq!(embed.category, CategoryId::Emb);
    }

    #[test]
    fn explicit_category_wins_and_embedding_conflicts_are_errors() {
        let rp = ModelEntry::from_flat(&flat(&[("name", "a"), ("category", "RP")]))
            .expect("valid entry");
        assert_eq!(rp.category, CategoryId::Rp);

        let err = ModelEntry::from_flat(&flat(&[
            ("name", "b"),
            ("category", "WORK"),
            ("is_embedding_model", "true"),
        ]))
        .expect_err("embedding model must live in EMB");
        assert!(matches!(err, ModelError::EmbeddingCategoryConflict { .. }));
    }

    #[test]
    fn missing_name_and_bad_numbers_are_errors() {
        assert_eq!(
            ModelEntry::from_flat(&flat(&[])),
            Err(ModelError::MissingName)
        );
        let err = ModelEntry::from_flat(&flat(&[("name", "a"), ("vram_gb", "big")]))
            .expect_err("vram_gb must be numeric");
        assert!(matches!(err, ModelError::BadNumber { .. }));
    }

    #[test]
    fn cost_classes_and_launch_floor_parse() {
        // Operator design note 2026-10-26: graded admission per engine kind.
        let entry = ModelEntry::from_flat(&flat(&[
            ("name", "g"),
            ("cost", "Greedy"),
            ("launch_only_with_at_least_vram_free", "6"),
        ]))
        .expect("greedy parses");
        assert_eq!(entry.cost, CostClass::Greedy);
        assert_eq!(entry.launch_floor_gb, Some(6.0));

        let exact = ModelEntry::from_flat(&flat(&[("name", "e"), ("cost", "exact")]))
            .expect("exact parses");
        assert_eq!(exact.cost, CostClass::Exact);

        let fixed = ModelEntry::from_flat(&flat(&[("name", "f")])).expect("default cost");
        assert_eq!(fixed.cost, CostClass::Fixed);
        assert_eq!(fixed.launch_floor_gb, None);

        let bad = ModelEntry::from_flat(&flat(&[("name", "x"), ("cost", "hoarder")]))
            .expect_err("unknown cost class is a scan error, never a silent default");
        assert!(matches!(bad, ModelError::BadCost { .. }), "{bad}");

        for value in ["eight", "-2", "nan", "inf", "1e400"] {
            let err = ModelEntry::from_flat(&flat(&[
                ("name", "x"),
                ("launch_only_with_at_least_vram_free", value),
            ]))
            .expect_err("floor must be a finite non-negative number");
            assert!(matches!(err, ModelError::BadNumber { .. }), "{err}");
            let err = ModelEntry::from_flat(&flat(&[("name", "x"), ("vram_gb", value)]))
                .expect_err("vram_gb must be a finite non-negative number");
            assert!(matches!(err, ModelError::BadNumber { .. }), "{err}");
        }
    }
}
