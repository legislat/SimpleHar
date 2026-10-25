//! Typed `simplehar.json` schema, defaults, and per-field validation (DESIGN Step 9/11).
//!
//! Boot rule (§0.2 step 1): a present-but-invalid file is reported per field and fails
//! the boot — never silently replaced by defaults (v3.3 defect D1 discarded the whole
//! safety policy over one typo). A *missing* file yields the documented defaults, so
//! "no config file" still produces a usable harness (§0.1 "simple").

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single per-field configuration problem.
///
/// Collected (never panicked on) so boot and `/har/status` can report every issue at
/// once, with the file and key named (Step 9, G6).
#[derive(Debug, Clone, PartialEq, Error, Serialize)]
pub enum ConfigError {
    /// A field has the wrong shape, an unknown name, or an out-of-range value.
    #[error("field `{field}`: {reason}")]
    Invalid {
        /// Dotted path to the offending field.
        field: String,
        /// Human-readable explanation, with the numbers used (G13).
        reason: String,
    },
}

/// The four slot categories (operator decision 2026-10-24; DESIGN Step 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum CategoryId {
    /// Never idles out; protected from eviction.
    Work,
    /// Evictable by WORK only.
    Rp,
    /// Evictable by WORK and RP; up to six occupants (Step 5).
    Aux,
    /// The embeddings surface (operator 2026-10-24).
    Emb,
}

impl fmt::Display for CategoryId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            CategoryId::Work => "WORK",
            CategoryId::Rp => "RP",
            CategoryId::Aux => "AUX",
            CategoryId::Emb => "EMB",
        };
        f.write_str(s)
    }
}

impl CategoryId {
    /// All four categories in canonical (slot-priority) order.
    pub fn all() -> [CategoryId; 4] {
        [
            CategoryId::Work,
            CategoryId::Rp,
            CategoryId::Aux,
            CategoryId::Emb,
        ]
    }
}

/// Parses `WORK | RP | AUX | EMB` case-insensitively.
impl std::str::FromStr for CategoryId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_uppercase().as_str() {
            "WORK" => Ok(CategoryId::Work),
            "RP" => Ok(CategoryId::Rp),
            "AUX" => Ok(CategoryId::Aux),
            "EMB" => Ok(CategoryId::Emb),
            other => Err(format!("unknown category `{other}` (WORK|RP|AUX|EMB)")),
        }
    }
}

fn is_true(flag: &bool) -> bool {
    *flag
}

fn one() -> usize {
    1
}

fn default_internal_host() -> IpAddr {
    IpAddr::from([127, 0, 0, 1])
}

fn default_vram_reserve() -> f64 {
    2.0
}

fn default_vram_total() -> f64 {
    72.0
}

fn default_load_timeout() -> u64 {
    120
}

fn default_tray_enabled() -> bool {
    true
}

fn default_release_timeout() -> u64 {
    60
}

fn default_protected_ports() -> BTreeSet<u16> {
    // The live v3.3 ecosystem on this box: its public ports, engine ports, and the
    // manual namespace. SE defaults use 8109-8112 so the two never collide (HANDOFF §3).
    BTreeSet::from([
        8003, 8004, 8005, 8009, 8016, 8017, 8030, 18203, 18204, 18205, 18206, 18207, 18208, 18209,
    ])
}

/// One declarative category: slot count, listener, and policy as data (DESIGN Step 5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CategoryConfig {
    /// Category identity; also the key referenced by `evictable_by` entries.
    pub id: CategoryId,
    /// Public listener port for this category (Step 11: one listener per category).
    pub public_port: u16,
    /// Model loaded when a no-`model` request finds the category empty (Step 11);
    /// validated against the catalog once it is scanned.
    #[serde(default)]
    pub default_model: Option<String>,
    /// Slot count for this category: 1 for WORK/RP/EMB, 1..=6 for AUX (Step 5).
    #[serde(default = "one")]
    pub max_models: usize,
    /// Categories whose loads may evict occupants of this one (Step 6 hard filter).
    #[serde(default)]
    pub evictable_by: Vec<CategoryId>,
    /// Idle auto-unload in minutes; 0 disables. WORK never idles (Step 5).
    #[serde(default)]
    pub idle_timeout_min: u64,
    /// This category's listener also serves `/har/*` and the control page;
    /// exactly one category sets this (Step 11).
    #[serde(default, skip_serializing_if = "is_true")]
    pub control: bool,
}

impl CategoryConfig {
    /// The four shipped categories on the SE port set 8109-8112 (WORK control).
    ///
    /// Eviction privileges reproduce v3.3 semantics: WORK is evicted by nobody, RP by
    /// WORK, AUX and EMB by WORK and RP (DESIGN Steps 5-6).
    pub fn four_defaults() -> Vec<CategoryConfig> {
        vec![
            CategoryConfig {
                id: CategoryId::Work,
                public_port: 8109,
                default_model: None,
                max_models: 1,
                evictable_by: vec![],
                idle_timeout_min: 0,
                control: true,
            },
            CategoryConfig {
                id: CategoryId::Rp,
                public_port: 8110,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work],
                idle_timeout_min: 0,
                control: false,
            },
            CategoryConfig {
                id: CategoryId::Aux,
                public_port: 8111,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work, CategoryId::Rp],
                idle_timeout_min: 0,
                control: false,
            },
            CategoryConfig {
                id: CategoryId::Emb,
                public_port: 8112,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work, CategoryId::Rp],
                idle_timeout_min: 0,
                control: false,
            },
        ]
    }
}

fn default_categories() -> Vec<CategoryConfig> {
    CategoryConfig::four_defaults()
}

/// Root of `simplehar.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Loopback address every listener binds on; must be loopback (§0.4 non-goals).
    #[serde(default = "default_internal_host")]
    pub internal_host: IpAddr,
    /// The four category definitions (Step 5); validation requires all four exactly once.
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryConfig>,
    /// GiB kept unreachable by admission (Step 4 rule 2).
    #[serde(default = "default_vram_reserve")]
    pub vram_reserve_gb: f64,
    /// Total VRAM of the card in GiB; the ledger's ceiling (Step 4).
    #[serde(default = "default_vram_total")]
    pub vram_total_gb: f64,
    /// Ports the harness must never bind or touch — the manual namespace (Step 1).
    #[serde(default = "default_protected_ports")]
    pub protected_ports: BTreeSet<u16>,
    /// G12 release-confirmation deadline in seconds for one dispose (§0.3).
    #[serde(default = "default_release_timeout")]
    pub release_timeout_s: u64,
    /// Readiness deadline per load in seconds; per-model override lands with the
    /// template schema (Step 3 knobs).
    #[serde(default = "default_load_timeout")]
    pub load_timeout_s: u64,
    /// Whether the tray icon starts with the harness (Step 15; loopback tool knob, G9).
    #[serde(default = "default_tray_enabled")]
    pub tray_enabled: bool,
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            internal_host: default_internal_host(),
            categories: default_categories(),
            vram_reserve_gb: default_vram_reserve(),
            vram_total_gb: default_vram_total(),
            protected_ports: default_protected_ports(),
            release_timeout_s: default_release_timeout(),
            load_timeout_s: default_load_timeout(),
            tray_enabled: default_tray_enabled(),
        }
    }
}

impl AppConfig {
    /// Parses `simplehar.json` text.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] naming the file root (with the serde location in
    /// the message) when the text is not valid JSON for the schema. Per-field semantic
    /// problems are *not* raised here; call [`AppConfig::validate`] for those.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|e| ConfigError::Invalid {
            field: "simplehar.json".to_string(),
            reason: e.to_string(),
        })
    }

    /// Loads `simplehar.json` from `path`.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Invalid`] if the file cannot be read or parsed. A *missing* file
    ///   is not an error at this layer — callers decide policy (boot uses defaults).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Invalid {
            field: "simplehar.json".to_string(),
            reason: format!("cannot read {}: {e}", path.display()),
        })?;
        AppConfig::parse(&text)
    }

    /// Mutable access to one category definition, if present.
    ///
    /// # Returns
    ///
    /// `Some(&mut CategoryConfig)` for the first entry with this id, `None` when the
    /// category is not configured (validation reports duplicates/missing separately).
    pub fn category_mut(&mut self, id: CategoryId) -> Option<&mut CategoryConfig> {
        self.categories.iter_mut().find(|c| c.id == id)
    }

    /// The category whose listener hosts `/har/*` and the control page (Step 11).    ///
    /// # Panics
    ///
    /// Only when the exactly-one-control invariant was not checked by `validate`;
    /// `main` validates before construction. [`ConfigError`] reporting is the path for
    /// user-facing problems; this is a programmer-error guard.
    pub fn control_category(&self) -> &CategoryConfig {
        self.categories
            .iter()
            .find(|c| c.control)
            .expect("invariant: validate() enforces exactly one control category")
    }

    /// Cross-field validation (§0.2 step 2 namespace check). Returns every problem.
    ///
    /// # Errors
    ///
    /// One [`ConfigError::Invalid`] per violation; an empty return means the config is
    /// internally consistent. The `default_model`-names-a-real-model check happens after
    /// catalog scan (Step 9) and is out of scope here.
    pub fn validate(&self) -> Vec<ConfigError> {
        let mut errors = Vec::new();
        let mut seen = BTreeSet::new();
        for cat in &self.categories {
            if !seen.insert(cat.id) {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}", cat.id),
                    reason: "duplicate category entry".to_string(),
                });
            }
            let cap = matches!(cat.id, CategoryId::Aux);
            let bounds_ok = if cap {
                (1..=6).contains(&cat.max_models)
            } else {
                cat.max_models == 1
            };
            if !bounds_ok {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}.max_models", cat.id),
                    reason: format!(
                        "must be {} for {}",
                        if cap { "1..=6" } else { "exactly 1" },
                        cat.id
                    ),
                });
            }
            if let Some(model) = &cat.default_model {
                if model.trim().is_empty() {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{}.default_model", cat.id),
                        reason: "must name a non-empty model".to_string(),
                    });
                }
            }
        }
        for id in CategoryId::all() {
            if !seen.contains(&id) {
                errors.push(ConfigError::Invalid {
                    field: "categories".to_string(),
                    reason: format!("missing category {id}"),
                });
            }
        }
        let control_count = self.categories.iter().filter(|c| c.control).count();
        if control_count != 1 {
            errors.push(ConfigError::Invalid {
                field: "categories.control".to_string(),
                reason: format!(
                    "exactly one category must be the control listener, found {control_count}"
                ),
            });
        }
        let mut ports = BTreeMap::new();
        for cat in &self.categories {
            if let Some(owner) = ports.insert(cat.public_port, cat.id) {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}.public_port", cat.id),
                    reason: format!("port {} already bound by {owner}", cat.public_port),
                });
            }
            if self.protected_ports.contains(&cat.public_port) {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}.public_port", cat.id),
                    reason: format!(
                        "port {} is in protected_ports (harness/manual namespaces must stay disjoint)",
                        cat.public_port
                    ),
                });
            }
        }
        if self.vram_total_gb <= 0.0 {
            errors.push(ConfigError::Invalid {
                field: "vram_total_gb".to_string(),
                reason: "must be positive (the ledger ceiling, Step 4)".to_string(),
            });
        }
        if self.load_timeout_s == 0 {
            errors.push(ConfigError::Invalid {
                field: "load_timeout_s".to_string(),
                reason: "must be positive (readiness deadline, Step 3)".to_string(),
            });
        }
        if !self.internal_host.is_loopback() {
            errors.push(ConfigError::Invalid {
                field: "internal_host".to_string(),
                reason: "must be a loopback address (SE is a loopback tool, §0.4)".to_string(),
            });
        }
        errors
    }
}

/// Test/validate helper: mutable access to one category by id (panics only in tests,
/// where the four-category invariant is established by construction).
impl AppConfig {
    #[cfg(test)]
    fn categories_mut(&mut self, id: CategoryId) -> &mut CategoryConfig {
        self.categories
            .iter_mut()
            .find(|c| c.id == id)
            .expect("invariant: test configs always contain all four categories")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_cover_four_categories_with_se_ports() {
        let cfg = AppConfig::parse("{}").expect("all fields defaulted");
        assert_eq!(cfg, AppConfig::default());
        assert_eq!(cfg.categories.len(), 4);
        assert_eq!(cfg.control_category().id, CategoryId::Work);
        assert_eq!(cfg.control_category().public_port, 8109);
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn aux_capacity_bounds_are_one_to_six() {
        let mut cfg = AppConfig::default();
        for bad in [0usize, 7] {
            cfg.categories_mut(CategoryId::Aux).max_models = bad;
            assert!(
                !cfg.validate().is_empty(),
                "max_models={bad} must be rejected"
            );
        }
        cfg.categories_mut(CategoryId::Aux).max_models = 6;
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn duplicate_listener_port_is_reported() {
        let mut cfg = AppConfig::default();
        cfg.categories_mut(CategoryId::Emb).public_port = 8110;
        let errors = cfg.validate();
        assert!(errors
            .iter()
            .any(|e| e.to_string().contains("already bound")));
    }

    #[test]
    fn protected_port_overlap_is_refused() {
        let mut cfg = AppConfig::default();
        cfg.categories_mut(CategoryId::Work).public_port = 8009;
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.to_string().contains("protected_ports")));
    }

    #[test]
    fn missing_category_and_two_controls_are_reported() {
        let mut cfg = AppConfig::default();
        cfg.categories.retain(|c| c.id != CategoryId::Emb);
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.to_string().contains("missing category")));

        let mut cfg = AppConfig::default();
        cfg.categories_mut(CategoryId::Rp).control = true;
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.to_string().contains("control listener")));
    }

    #[test]
    fn non_loopback_host_is_refused() {
        let cfg = AppConfig {
            internal_host: IpAddr::from([192, 168, 1, 10]),
            ..AppConfig::default()
        };
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.to_string().contains("loopback")));
    }

    #[test]
    fn unknown_field_is_an_error_not_a_silent_default() {
        let err = AppConfig::parse("{\"wrok_port\": 9}").expect_err("deny_unknown_fields");
        assert!(err.to_string().contains("simplehar.json"));
    }

    #[test]
    fn category_round_trips_through_strings() {
        for id in CategoryId::all() {
            let parsed: CategoryId = id.to_string().parse().expect("canonical form parses");
            assert_eq!(parsed, id);
        }
        assert!("WORK".parse::<CategoryId>().is_ok());
        assert!("work".parse::<CategoryId>().is_ok());
        assert!("BONSAI".parse::<CategoryId>().is_err());
    }
}
