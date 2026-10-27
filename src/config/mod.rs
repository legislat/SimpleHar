//! Typed config schema (`simplehar-se.json`), defaults, and per-field validation
//! (DESIGN Step 9/11).
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

/// The accepted category spellings, quoted wherever a message asks for one — the
/// vocabulary lives with the type, not in copies of it (duplication review).
pub const CATEGORY_HINT: &str = "WORK|RP|AUX|EMB";

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
            other => Err(format!("unknown category `{other}` ({CATEGORY_HINT})")),
        }
    }
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

fn default_settle_timeout() -> u64 {
    30
}

fn default_witness_tolerance() -> f64 {
    2.0
}

fn default_greedy_min_free() -> f64 {
    4.0
}

fn default_measured_sample_s() -> u64 {
    60
}

fn default_wsl_distro() -> String {
    "Ubuntu".to_string()
}

/// Strips surrounding whitespace from a WSL coordinate at parse time. A stray space
/// would otherwise reach `wsl.exe` as part of its own argument and fail as "distribution
/// not found" — and both coordinates address the same container, so they must be
/// normalized the same way (review, 2026-10-26).
fn trimmed<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    String::deserialize(deserializer).map(|raw| raw.trim().to_string())
}

/// [`trimmed`] for an optional coordinate. A blank spelling survives as `Some("")`, so
/// [`AppConfig::validate`] can still tell a typo from an absent key.
fn trimmed_opt<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<String>::deserialize(deserializer)?.map(|raw| raw.trim().to_string()))
}

fn default_modelconf_dir() -> std::path::PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("ModelConf")))
        .unwrap_or_else(|| std::path::PathBuf::from("ModelConf"))
}

fn default_llama_server_exe() -> std::path::PathBuf {
    std::path::PathBuf::from(r"C:\Inference\llama.cpp\llama-server.exe")
}

/// First port of the sequential engine-port allocation (HANDOFF §3 SE port plan).
const ENGINE_PORT_BASE: u16 = 18_109;

fn default_release_timeout() -> u64 {
    60
}

fn default_protected_ports() -> BTreeSet<u16> {
    // The live v3.3 ecosystem on this box: its public ports, engine ports, and the
    // manual namespace. SE defaults use 8708-8711 so the two never collide (HANDOFF §3).
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
    /// Idle auto-unload threshold in seconds; 0 disables (Step 5: WORK defaults
    /// to 0 and never idles out; set it explicitly to make WORK idle — config is king).
    #[serde(default)]
    pub idle_timeout_s: u64,
    /// This category's listener also serves `/har/*` and the control page;
    /// exactly one category sets this (Step 11). Always serialized: the old
    /// `skip_serializing_if = "is_true"` was inverted (skipped the only value
    /// that matters), so a config rebuilt through `to_value` lost its control
    /// category and refused to boot (found by the live restart test, 2026-10-26).
    #[serde(default)]
    pub control: bool,
    /// Internal engine ports for this category's slots (loopback only). When empty
    /// or short, ports are assigned sequentially from 18109 in category order
    /// (HANDOFF §3 port plan: WORK 18109, RP 18110, AUX 18111.., EMB next).
    #[serde(default)]
    pub engine_ports: Vec<u16>,
}

impl CategoryConfig {
    /// The four shipped categories on the operator port set 8708-8711 (WORK control;
    /// operator decision 2026-10-25).
    ///
    /// Eviction privileges reproduce v3.3 semantics: WORK is evicted by nobody, RP by
    /// WORK, AUX and EMB by WORK and RP (DESIGN Steps 5-6).
    pub fn four_defaults() -> Vec<CategoryConfig> {
        vec![
            CategoryConfig {
                id: CategoryId::Work,
                public_port: 8708,
                default_model: None,
                max_models: 1,
                evictable_by: vec![],
                idle_timeout_s: 0,
                control: true,
                engine_ports: vec![],
            },
            CategoryConfig {
                id: CategoryId::Rp,
                public_port: 8709,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work],
                idle_timeout_s: 0,
                control: false,
                engine_ports: vec![],
            },
            CategoryConfig {
                id: CategoryId::Aux,
                public_port: 8710,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work, CategoryId::Rp],
                idle_timeout_s: 0,
                control: false,
                engine_ports: vec![],
            },
            CategoryConfig {
                id: CategoryId::Emb,
                public_port: 8711,
                default_model: None,
                max_models: 1,
                evictable_by: vec![CategoryId::Work, CategoryId::Rp],
                idle_timeout_s: 0,
                control: false,
                engine_ports: vec![],
            },
        ]
    }
}

fn default_categories() -> Vec<CategoryConfig> {
    CategoryConfig::four_defaults()
}

/// The file SE boots by default, beside the executable. Deliberately *not* v3.3's
/// name: SE re-serializes the whole struct on every persist, and sharing a file
/// means our write can strand the old exe — v3.3 reads `protected_ports` and exits
/// when it clashes with its port bases (deploy review, 2026-10-26).
pub const CONFIG_FILE: &str = "simplehar-se.json";

/// v3.3's config name. Read once, to migrate; **never** written by SE, so the
/// rollback path keeps the file it expects.
pub const LEGACY_CONFIG_FILE: &str = "simplehar.json";

/// Root of [`CONFIG_FILE`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Address every listener binds on (operator note 2026-10-25): a loopback
    /// address (default 127.0.0.1) or `0.0.0.0` to publish to the local net.
    /// Any other address is refused by validation. Note: there is no auth —
    /// publishing to 0.0.0.0 trusts every machine on the segment (§0.4).
    #[serde(default = "default_internal_host")]
    pub internal_host: IpAddr,
    /// The four category definitions (Step 5); validation requires all four exactly once.
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryConfig>,
    /// GiB kept unreachable by admission (Step 4 rule 2).
    #[serde(default = "default_vram_reserve")]
    pub vram_reserve_gb: f64,
    /// Total VRAM of the card in GiB; the ceiling the budget is measured against
    /// (Step 4).
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
    /// Interval between witness samples while a model serves, which build the
    /// `measured_gb`/`measured_gb_min` range (§0.4 measured cost). One minute by
    /// default: the KV cache grows over a session, so a faster cadence only buys a
    /// wider max, while a slower one can miss the peak entirely.
    #[serde(default = "default_measured_sample_s")]
    pub measured_sample_s: u64,
    /// Whether the tray icon starts with the harness (Step 15; loopback tool knob, G9).
    #[serde(default = "default_tray_enabled")]
    pub tray_enabled: bool,
    /// Path to `llama-server.exe` (A.7: the operator's script is the source of truth
    /// for flags; the binary path is config).
    #[serde(default = "default_llama_server_exe")]
    pub llama_server_exe: std::path::PathBuf,
    /// Directory of per-model flat yaml files (Step 9); missing dir = empty catalog.
    #[serde(default = "default_modelconf_dir")]
    pub modelconf_dir: std::path::PathBuf,
    /// G12 memory-settle deadline after a confirmed release (§0.3).
    #[serde(default = "default_settle_timeout")]
    pub settle_timeout_s: u64,
    /// Witness tolerance in GiB: disagreement within this band is not a re-sample
    /// trigger, and settle accepts `baseline + tolerance` (Step 4 rule 4).
    #[serde(default = "default_witness_tolerance")]
    pub witness_tolerance_gb: f64,
    /// Launch floor applied to `cost: greedy` models that carry no explicit
    /// `launch_only_with_at_least_vram_free`: greedy loads are never unguarded
    /// (operator design note, 2026-10-26).
    #[serde(default = "default_greedy_min_free")]
    pub greedy_launch_min_free_gb: f64,
    /// WSL distro hosting ninfer containers (`wsl.exe -d <this>`; v3.3 parity).
    #[serde(default = "default_wsl_distro", deserialize_with = "trimmed")]
    pub wsl_distro: String,
    /// WSL user owning the ninfer containers and their autostart scripts. **No
    /// default**: it names a personal account, so a value shipped in code would be
    /// wrong for everyone but its author. Omit it when no ninfer model is used; while
    /// it is unset the `ninfer` adapter refuses to launch and boot reclaim stands
    /// down, both by name rather than by a silent empty `-u` argument.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "trimmed_opt"
    )]
    pub wsl_user: Option<String>,
    /// The file this config was loaded from (`SIMPLEHAR_CONFIG` or beside the exe),
    /// stamped by the boot loader. `#[serde(skip)]` so it never enters the persisted
    /// JSON. `default_model` write-back targets this exact path — read and write must
    /// be the same file, or a `SIMPLEHAR_CONFIG` run silently loses its defaults on
    /// restart (operator finding, 2026-10-26).
    #[serde(skip, default)]
    pub source_path: Option<std::path::PathBuf>,
    /// Optional shared secret for the API and proxy routes, enforced by the guard via
    /// `X-Har-Token` or `Authorization: Bearer`. Absent means open — safe only on
    /// loopback, because publishing `internal_host` to a segment otherwise hands every
    /// machine there load/unload/config power (deploy review, 2026-10-26).
    /// `SIMPLEHAR_TOKEN` overrides it so the secret can stay out of the file entirely;
    /// the value is never logged and never accepted in a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<String>,
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
            llama_server_exe: default_llama_server_exe(),
            modelconf_dir: default_modelconf_dir(),
            settle_timeout_s: default_settle_timeout(),
            witness_tolerance_gb: default_witness_tolerance(),
            greedy_launch_min_free_gb: default_greedy_min_free(),
            measured_sample_s: default_measured_sample_s(),
            wsl_distro: default_wsl_distro(),
            wsl_user: None,
            control_token: None,
            source_path: None,
        }
    }
}

impl AppConfig {
    /// Parses config text: either [`CONFIG_FILE`] or a legacy [`LEGACY_CONFIG_FILE`]
    /// read for the one-time migration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] naming the file root (with the serde location in
    /// the message) when the text is not valid JSON for the schema. Per-field semantic
    /// problems are *not* raised here; call [`AppConfig::validate`] for those.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        serde_json::from_str(text).map_err(|e| ConfigError::Invalid {
            field: CONFIG_FILE.to_string(),
            reason: e.to_string(),
        })
    }

    /// Loads a config from `path`.
    ///
    /// # Errors
    ///
    /// - [`ConfigError::Invalid`] if the file cannot be read or parsed. A *missing* file
    ///   is not an error at this layer — callers decide policy (boot uses defaults).
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Invalid {
            field: CONFIG_FILE.to_string(),
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

    /// Resolves the internal engine port per slot (Step 8/11 port plan).
    ///
    /// Categories with an explicit `engine_ports` of the right length keep theirs;
    /// the rest are allocated sequentially from [`ENGINE_PORT_BASE`] in category
    /// order. Validation checks the resolved plan for collisions.
    pub fn effective_engine_ports(&self) -> BTreeMap<CategoryId, Vec<u16>> {
        let mut cursor = ENGINE_PORT_BASE;
        let mut plan = BTreeMap::new();
        for category in &self.categories {
            let explicit_ok = category.engine_ports.len() == category.max_models;
            let ports: Vec<u16> = if explicit_ok {
                category.engine_ports.clone()
            } else {
                (0..category.max_models)
                    .map(|offset| cursor + offset as u16)
                    .collect()
            };
            cursor = cursor.saturating_add(category.max_models as u16);
            plan.insert(category.id, ports);
        }
        plan
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

    /// The WSL account to address containers with: `None` when the key is absent or
    /// blank, so callers branch on "unset" instead of passing an empty `-u` to
    /// `wsl.exe`. [`AppConfig::validate`] rejects the blank-but-present spelling.
    #[must_use]
    pub fn wsl_account(&self) -> Option<&str> {
        self.wsl_user
            .as_deref()
            .map(str::trim)
            .filter(|user| !user.is_empty())
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
        // A blank token must not silently disable auth: the guard reads an empty value
        // as "no token", which would fail open on a published listener. Refuse to boot
        // instead (fail closed).
        if self
            .control_token
            .as_deref()
            .is_some_and(|token| token.trim().is_empty())
        {
            errors.push(ConfigError::Invalid {
                field: "control_token".to_string(),
                reason: "must not be blank; omit the key for an open (loopback-only) control plane"
                    .to_string(),
            });
        }
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
        if !(self.greedy_launch_min_free_gb.is_finite() && self.greedy_launch_min_free_gb > 0.0) {
            errors.push(ConfigError::Invalid {
                field: "greedy_launch_min_free_gb".to_string(),
                reason:
                    "must be a finite number greater than 0 (greedy loads are never unguarded; \
                         opt out per-model with launch_only_with_at_least_vram_free: 0)"
                        .to_string(),
            });
        }
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
        // WSL coordinates are load-bearing for ninfer spawn AND container reap
        // (review, 2026-10-26): a blank distro/user must be a boot error, not a
        // silently ineffective string.
        if self.wsl_distro.trim().is_empty() {
            errors.push(ConfigError::Invalid {
                field: "wsl_distro".to_string(),
                reason: "must not be blank (ninfer spawn and reap both address it)".to_string(),
            });
        }
        if self
            .wsl_user
            .as_deref()
            .is_some_and(|user| user.trim().is_empty())
        {
            errors.push(ConfigError::Invalid {
                field: "wsl_user".to_string(),
                reason: "must not be blank (ninfer spawn and reap both address it); omit the \
                         key when no ninfer model is used"
                    .to_string(),
            });
        }
        if self.load_timeout_s == 0 {
            errors.push(ConfigError::Invalid {
                field: "load_timeout_s".to_string(),
                reason: "must be positive (readiness deadline, Step 3)".to_string(),
            });
        }
        if self.measured_sample_s == 0 {
            errors.push(ConfigError::Invalid {
                field: "measured_sample_s".to_string(),
                reason:
                    "must be positive (a zero cadence would spin the witness; 60 is the default)"
                        .to_string(),
            });
        }
        // Engine ports: unique among themselves and disjoint from both namespaces.
        let mut engine_ports_seen = BTreeSet::new();
        for (category, ports) in self.effective_engine_ports() {
            for port in ports {
                if !engine_ports_seen.insert(port) {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{category}.engine_ports"),
                        reason: format!("engine port {port} appears more than once"),
                    });
                }
                if self.protected_ports.contains(&port) {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{category}.engine_ports"),
                        reason: format!("engine port {port} is in protected_ports"),
                    });
                }
                if self.categories.iter().any(|c| c.public_port == port) {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{category}.engine_ports"),
                        reason: format!("engine port {port} collides with a public listener"),
                    });
                }
            }
        }
        if !(self.internal_host.is_loopback() || self.internal_host.is_unspecified()) {
            errors.push(ConfigError::Invalid {
                field: "internal_host".to_string(),
                reason: format!(
                    "must be a loopback address or 0.0.0.0 (local net, no auth - §0.4); got {}",
                    self.internal_host
                ),
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
        assert_eq!(cfg.control_category().public_port, 8708);
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
        cfg.categories_mut(CategoryId::Emb).public_port = 8709;
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
    fn unicast_lan_host_is_refused_but_unspecified_is_allowed() {
        // A specific LAN address is refused: publish with 0.0.0.0 instead.
        let cfg = AppConfig {
            internal_host: IpAddr::from([192, 168, 1, 10]),
            ..AppConfig::default()
        };
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.to_string().contains("0.0.0.0")));

        // The operator option: 0.0.0.0 publishes to the local net.
        let cfg = AppConfig {
            internal_host: IpAddr::from([0, 0, 0, 0]),
            ..AppConfig::default()
        };
        assert!(cfg.validate().is_empty(), "0.0.0.0 must validate");
    }

    #[test]
    fn measured_cadence_defaults_to_a_minute_and_refuses_zero() {
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert_eq!(absent.measured_sample_s, 60);
        assert!(absent.validate().is_empty());
        let zero = AppConfig::parse("{\"measured_sample_s\": 0}")
            .expect("parses")
            .validate();
        assert!(
            zero.iter()
                .any(|e| e.to_string().contains("measured_sample_s")),
            "a zero cadence would spin the witness instead of sampling it"
        );
    }

    #[test]
    fn wsl_coordinates_are_trimmed_at_parse_so_a_stray_space_cannot_reach_wsl() {
        // Both halves address the same container, so a typo that survives in one and is
        // corrected in the other is a launch-vs-teardown divergence.
        let cfg = AppConfig::parse("{\"wsl_distro\": \" Ubuntu \", \"wsl_user\": \"  ops  \"}")
            .expect("parses");
        assert_eq!(cfg.wsl_distro, "Ubuntu", "the distro is normalized too");
        assert_eq!(cfg.wsl_user.as_deref(), Some("ops"));
        assert_eq!(cfg.wsl_account(), Some("ops"));
        assert!(cfg.validate().is_empty());
    }

    #[test]
    fn wsl_user_ships_no_default_and_is_only_rejected_when_blank() {
        // It names a personal account, so a shipped default would be wrong for every
        // other machine: absent is legal (the adapter refuses by name), blank is a typo.
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert_eq!(absent.wsl_user, None);
        assert_eq!(absent.wsl_account(), None);
        assert!(absent.validate().is_empty());
        assert!(
            !serde_json::to_string(&absent)
                .expect("serializes")
                .contains("wsl_user"),
            "an unconfigured coordinate must not be stamped into the operator's file"
        );
        let set = AppConfig::parse("{\"wsl_user\": \"  ops  \"}").expect("parses");
        assert_eq!(
            set.wsl_account(),
            Some("ops"),
            "surrounding space is not part of an account"
        );
        assert!(set.validate().is_empty());
        let blank = AppConfig::parse("{\"wsl_user\": \"   \"}")
            .expect("parses as json")
            .validate();
        assert!(
            blank.iter().any(|e| e.to_string().contains("wsl_user")),
            "a blank spelling must fail closed, not disable reaping silently"
        );
    }

    #[test]
    fn control_token_is_optional_never_stamped_when_absent_and_never_blank() {
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert_eq!(absent.control_token, None);
        assert!(absent.validate().is_empty());
        assert!(
            !serde_json::to_string(&absent)
                .expect("serializes")
                .contains("control_token"),
            "an unconfigured secret must not be stamped into the operator's file"
        );
        let set = AppConfig::parse("{\"control_token\": \"s3cret\"}").expect("token parses");
        assert_eq!(set.control_token.as_deref(), Some("s3cret"));
        assert!(set.validate().is_empty());
        let blank = AppConfig::parse("{\"control_token\": \"   \"}")
            .expect("parses as json")
            .validate();
        assert!(
            blank
                .iter()
                .any(|e| e.to_string().contains("control_token")),
            "a blank token would fail open: {blank:?}"
        );
    }

    #[test]
    fn unknown_field_is_an_error_not_a_silent_default() {
        let err = AppConfig::parse("{\"wrok_port\": 9}").expect_err("deny_unknown_fields");
        assert!(err.to_string().contains(CONFIG_FILE));
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

    #[test]
    fn greedy_launch_floor_defaults_and_validates() {
        // Greedy loads are never unguarded: the global default gives every
        // `cost: greedy` model a launch floor unless its yaml overrides it.
        let cfg = AppConfig::default();
        assert_eq!(cfg.greedy_launch_min_free_gb, 4.0);
        let bad = AppConfig {
            greedy_launch_min_free_gb: -1.0,
            ..AppConfig::default()
        };
        assert!(
            bad.validate()
                .iter()
                .any(|e| e.to_string().contains("greedy_launch_min_free_gb")),
            "negative floor is refused at boot"
        );
        let zero = AppConfig {
            greedy_launch_min_free_gb: 0.0,
            ..AppConfig::default()
        };
        assert!(
            zero.validate()
                .iter()
                .any(|e| e.to_string().contains("greedy_launch_min_free_gb")),
            "a zero global default would leave every greedy load unguarded"
        );
        let huge = AppConfig {
            greedy_launch_min_free_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(
            huge.validate()
                .iter()
                .any(|e| e.to_string().contains("greedy_launch_min_free_gb")),
            "inf must be refused, not locked in as a permanent refusal generator"
        );
    }
}
