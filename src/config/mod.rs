//! Typed config schema (`simpleharn.json`), defaults, and per-field validation
//! (DESIGN Step 9/11).
//!
//! Boot rule (§0.2 step 1): a present-but-invalid file is reported per field and fails
//! the boot — never silently replaced by defaults (v3.3 defect D1 discarded the whole
//! safety policy over one typo). A *missing* file yields the documented defaults, so
//! "no config file" still produces a usable harness (§0.1 "simple").

pub mod firstrun;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::net::IpAddr;
use std::path::Path;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A single per-field configuration problem.
///
/// Collected (never panicked on) so boot and `/har/status` can report every issue at
/// once, with the file and key named (Step 9, G6). On the wire (`/har/status`
/// `config_errors`) each entry carries a snake_case `kind` beside its `field` and
/// `reason`, matching the `catalog_errors` vocabulary.
#[derive(Debug, Clone, PartialEq, Error, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
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

/// Host RAM kept unreachable by admission for a CPU-resident load
/// (operator ruling 2026-11-05): the OS and the desktop need real headroom,
/// and a refused load costs one retry.
fn default_ram_reserve() -> f64 {
    8.0
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

/// Host GiB a CPU-resident row must exceed before a greedy expert-offload
/// load evicts it (operator ruling 2026-11-06): strata, basalt and maya
/// offload experts to host RAM, and freeing a large CPU resident first beats
/// letting the expert tier spill to SSD. `0` disables the rule.
fn default_expert_offload_evict_cpu_above() -> f64 {
    4.0
}

fn default_measured_sample_s() -> u64 {
    60
}

/// Default measured-vs-declared drift ratio that raises the "measured footprint
/// diverges" warning. `pub(crate)`-free: only serde and validation read it here; the
/// scheduler reads the resolved field off `AppConfig`.
fn default_measured_drift_warn_ratio() -> f64 {
    0.25
}

/// Per-container ninfer log rotation size, in MiB, rendered into the canonical
/// autostart script. `pub(crate)` so the ninfer adapter's standalone default and the
/// config default share one literal (no drift between what a default render and a
/// defaulted config produce). The historical value the harness always rendered.
pub(crate) fn default_ninfer_log_max_size_mib() -> u32 {
    25
}

/// Per-container ninfer log rotation file count, rendered into the canonical
/// autostart script. `pub(crate)` for the same one-literal reason as
/// [`default_ninfer_log_max_size_mib`].
pub(crate) fn default_ninfer_log_max_files() -> u32 {
    5
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

/// Longest accepted category display label, in characters (operator ruling
/// 2026-11-05). The label is display-only — it never travels where an id is
/// required — and the cap keeps one hand-written name from breaking the tab
/// strip. A longer value is refused by [`AppConfig::validate`], naming the
/// category and the cap.
pub const CATEGORY_LABEL_MAX_CHARS: usize = 24;

/// One day in seconds. The single source for the one-day bound every timeout
/// ceiling shares, so a ceiling and its doc comment name the same figure.
const ONE_DAY_SECS: u64 = 86_400;

/// The validation ceiling for every timeout key: the config `*_timeout_s` fields
/// and the per-model `load_timeout` and `min_turn` rows. One day. A `u64` parse
/// already rejects `nan`, `inf`, and `1e400`; this guards the mistyped-digit typo
/// that a readiness, dispose, or settle deadline would otherwise honor for years.
/// Cross-module because `model` validates the per-model rows against the same bound.
pub(crate) const MAX_TIMEOUT_S: u64 = ONE_DAY_SECS;

/// One declarative category: slot count, listener, and policy as data (DESIGN Step 5).
///
/// `deny_unknown_fields` (FINDINGS-11 \[1]): the categories carry all the safety policy and
/// the file is hand-edited, so a key typo must fail the boot like any other invalid value —
/// silently defaulting `"max_model": 6` to `max_models: 1` or `"evictable": ["WORK"]` to an
/// empty list is the D1 defect this file's module doc promises against.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryConfig {
    /// Category identity; also the key referenced by `evictable_by` entries.
    pub id: CategoryId,
    /// Display name for the page and `/har/status` (operator ruling 2026-11-05:
    /// the four shipped names are defaults, not fixed). Absent or blank (after
    /// trim) falls back to the id — [`CategoryConfig::effective_label`]. Trimmed
    /// at parse like the WSL coordinates. The id stays the wire vocabulary
    /// everywhere: this value never replaces it in a request, a yaml `category:`
    /// value, an `evictable_by` list, or the engine-port plan. Unset serializes
    /// nothing, so existing config files keep their exact shape (the
    /// GOTCHAS serialization discipline); a blank `""` round-trips as `""`.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "trimmed_opt"
    )]
    pub label: Option<String>,
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
    /// An omitted key and an explicit `[]` are the same posture here: nobody may
    /// evict this category. The schema cannot tell the two spellings apart, so a
    /// non-WORK entry that lands on the empty list is named by a boot warning
    /// ([`AppConfig::evictable_by_warnings`], R2-11-01); the behavior is unchanged.
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
    /// This category's internal engine ports for its slots (loopback only). An
    /// empty list keeps the automatic plan: ports assigned sequentially from
    /// 18109 in canonical category order (HANDOFF §3 port plan: WORK 18109,
    /// RP 18110, AUX 18111.., EMB next). A non-empty list must name exactly
    /// `max_models` ports, one per slot; another length is refused by
    /// [`AppConfig::validate`] (R2-11-02), so a hand-written plan is never
    /// silently discarded for a length typo.
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
                label: None,
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
                label: None,
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
                label: None,
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
                label: None,
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

    /// The name the page and `/har/status` show: the trimmed `label` when one is
    /// configured, else the id (operator ruling 2026-11-05: display-only
    /// naming, the id stays the wire vocabulary). Re-trims here rather than
    /// trusting the parse, so a hand-built entry (a test, a future API patch)
    /// cannot put stray whitespace on the page.
    #[must_use]
    pub fn effective_label(&self) -> std::borrow::Cow<'_, str> {
        match self
            .label
            .as_deref()
            .map(str::trim)
            .filter(|label| !label.is_empty())
        {
            Some(label) => std::borrow::Cow::Borrowed(label),
            None => std::borrow::Cow::Owned(self.id.to_string()),
        }
    }
}

fn default_categories() -> Vec<CategoryConfig> {
    CategoryConfig::four_defaults()
}

/// The file the harness boots by default, beside the executable. Deliberately *not*
/// v3.3's name: the harness re-serializes the whole struct on every persist, and
/// sharing a file means our write can strand the old exe — v3.3 reads
/// `protected_ports` and exits when it clashes with its port bases (deploy review,
/// 2026-10-26). Since the 2026-11-04 product rename this is the one and only
/// config name: `simpleharn.json`. There is no auto-discovery of any earlier
/// spelling and no migration copy (TASK-NAMING-CLEANUP, operator ruling
/// 2026-11-05): a file that carries an older spelling is ignored by discovery,
/// and only an explicit `SIMPLEHARN_CONFIG` path — an operator instruction — can
/// select any file at all.
pub const CONFIG_FILE: &str = "simpleharn.json";

/// Where [`CONFIG_FILE`] lives when nothing points elsewhere: beside the executable.
///
/// One definition on purpose. Boot reads from here (`main.rs::boot_config`) and config
/// write-back writes to here (`api::config_write_path`); when the two were spelled out
/// separately they drifted by exactly one filename, and a write landed in a file boot
/// never loads. Returns `None` only when the process cannot resolve its own executable
/// path.
#[must_use]
pub fn default_config_path() -> Option<std::path::PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(CONFIG_FILE)))
}

// The environment names the harness reads are `SIMPLEHARN_TOKEN`,
// `SIMPLEHARN_CONFIG` and `SIMPLEHARN_LOG` — and nothing else
// (TASK-NAMING-CLEANUP, operator ruling 2026-11-05). No other spelling is a
// fallback: a variable named any older way is inert — it changes nothing,
// warns nothing, and grants nothing.

/// The control token from the process environment: `SIMPLEHARN_TOKEN`.
#[must_use]
pub fn token_from_env() -> Option<String> {
    std::env::var("SIMPLEHARN_TOKEN").ok()
}

/// The explicit config path from the process environment: `SIMPLEHARN_CONFIG`.
#[must_use]
pub fn config_path_from_env() -> Option<String> {
    std::env::var("SIMPLEHARN_CONFIG").ok()
}

/// The log-level knob from the process environment: `SIMPLEHARN_LOG` (review
/// R1R-01 lineage — this knob is the forensic lever named by FINDINGS-15 \[8\]).
/// The value alone is not enough for its consumer: it is read inside
/// `observability::init`, before a subscriber exists, where a `tracing::warn!`
/// would reach the no-op dispatcher — so an invalid value is reported by the
/// caller after `init`, not here.
#[must_use]
pub fn log_level_from_env() -> Option<String> {
    std::env::var("SIMPLEHARN_LOG").ok()
}

/// Fallback strata interpreter when neither `engines.strata.python` nor a model
/// yaml's `strata_python` names one (v3.3 default; single home since slice 10d —
/// the strata adapter's `DEFAULT_PYTHON` is this constant).
pub const STRATA_PYTHON_DEFAULT: &str = r"C:\Strata\.venv\Scripts\python.exe";

/// Fallback strata working directory when neither `engines.strata.cwd` nor a
/// model yaml's `strata_cwd` names one (v3.3 default; single home since slice
/// 10d).
pub const STRATA_CWD_DEFAULT: &str = r"C:\Strata";

/// Harness-side per-engine defaults (slice 10d). Doctrine (`DECISIONS` LOCKED):
/// *"external engines obey only harness-owned configuration"* — the interpreter
/// and working directory a strata launch runs with are configured here, not in
/// an operator-side engine file. Every key is optional and an unset one stays
/// out of the serialized file (existing configs keep their exact shape, the
/// GOTCHAS serialization discipline), while an unknown key is refused
/// (`deny_unknown_fields`) rather than silently ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnginesConfig {
    /// strata engine defaults; absent means the documented v3.3 fallbacks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strata: Option<StrataEngineConfig>,
    /// llama engine defaults (R11: the Fit probe switch and its Wave-C
    /// calibration constants); absent means fully inert — no probe is ever
    /// run unless `fit_probe` is explicitly turned on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llama: Option<LlamaEngineConfig>,
}

/// strata harness-side defaults. The per-model yaml knobs `strata_python` and
/// `strata_cwd` still override these values for a single model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StrataEngineConfig {
    /// Interpreter binary (`argv[0]`) the strata launch starts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub python: Option<std::path::PathBuf>,
    /// Working directory handed to the child process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<std::path::PathBuf>,
}

/// llama harness-side defaults for the R11 Fit cost class (rule 2026-11-01,
/// Search#40). Every value here is *calibration*, not decoration: the margin
/// and surcharge are meant to be measured in the Wave-C card window, so an
/// unset one is treated as **unconfigured** (warn + declared/estimate ladder)
/// rather than defaulted to a number that would pretend to be calibrated.
/// `fit_probe` itself defaults OFF, so the build lands inert: nothing probes,
/// no warm queue runs, and the ladder behaves exactly as before until the
/// operator turns it on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LlamaEngineConfig {
    /// Master switch for the pre-spawn `llama-fit-params --fit-print on`
    /// probe and its boot/rescan warm queue. Unset/false = zero probes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_probe: Option<bool>,
    /// Per-active-device safety margin in GiB (CUDA context, compute graphs —
    /// outside the projection). Required while the probe is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_margin_gb: Option<f64>,
    /// Vision surcharge in GiB applied to rows carrying `mmproj:` (fit-params
    /// rejects `--mmproj`, so the projection cannot see it). Required for a
    /// vision row while the probe is on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mmproj_surcharge_gb: Option<f64>,
    /// Per-probe deadline in seconds; unset keeps the control-command floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit_probe_timeout_s: Option<u64>,
}

/// Root of [`CONFIG_FILE`].
///
/// `Debug` is hand-written, not derived: [`AppConfig::control_token`] renders as a
/// redaction marker that reports only whether auth is enforced (`REDACTED(present)`
/// / `REDACTED(absent)`) — presence is reportable, the value never is, so no
/// `?config` line can ever put the crown jewel into the `/har/log` ring
/// (FINDINGS-15 \[9]).
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    /// Address every listener binds on (operator note 2026-10-25): a loopback
    /// address (default 127.0.0.1) or `0.0.0.0` to publish to the local net.
    /// Any other address is refused by validation. Posture: the optional
    /// `control_token` (env `SIMPLEHARN_TOKEN` overrides it) gates every route
    /// but the page assets; publishing to 0.0.0.0 with no token set trusts
    /// every machine on the segment.
    #[serde(default = "default_internal_host")]
    pub internal_host: IpAddr,
    /// The four category definitions (Step 5); validation requires all four exactly once.
    #[serde(default = "default_categories")]
    pub categories: Vec<CategoryConfig>,
    /// GiB kept unreachable by admission (Step 4 rule 2).
    #[serde(default = "default_vram_reserve")]
    pub vram_reserve_gb: f64,
    /// Host GiB kept unreachable by admission for CPU-resident rows
    /// (operator ruling 2026-11-05: the RAM ladder's reserve, the twin of
    /// `vram_reserve_gb` on its own resource).
    #[serde(default = "default_ram_reserve")]
    pub ram_reserve_gb: f64,
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
    /// Ratio of a model's declared `vram_gb` beyond which a fresh measured high
    /// figure warns the operator that the measured footprint diverged from the
    /// declared booking. The warning fires past the wider of `declared * ratio` and
    /// the fixed `MEASURED_DRIFT_FLOOR_GB` GiB floor. `0.0` silences the warning
    /// entirely; the floor still applies to any non-zero ratio. Validation accepts
    /// `0.0..=1.0`. Absent means `0.25`, the value the harness always used, so an
    /// unchanged file warns exactly as it did before. This gates a log line only; it
    /// never changes admission, the booking, or the persisted measured range.
    #[serde(default = "default_measured_drift_warn_ratio")]
    pub measured_drift_warn_ratio: f64,
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
    /// Host-RAM reclaim threshold for expert-offload engines (operator
    /// ruling 2026-11-06). When a `cost: greedy` load is admitted on an
    /// engine whose `EnginePolicy` row carries the `expert_offload` cell,
    /// the victim planner also evicts every CPU-resident row (a `cpu_offload`
    /// resident with a host-RAM booking) whose booked footprint is strictly
    /// greater than this figure, largest first, before the spawn. A row at or
    /// below the threshold stays; GPU residents are never victims of this
    /// rule, and a `cost: fixed` row never triggers it. `0` disables the
    /// rule. Validation accepts a finite value greater than or equal to 0;
    /// a bad value fails boot naming this key.
    #[serde(default = "default_expert_offload_evict_cpu_above")]
    pub expert_offload_evict_cpu_above_gb: f64,
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
    /// Per-container ninfer log rotation size, in MiB, rendered into the canonical
    /// autostart script's `--log-opt max-size`. Validation accepts `1..=1024`. Absent
    /// means `25`, the literal the harness always rendered, so an unchanged file
    /// produces byte-identical L3. The harness owns this launch input (DOCTRINE 2),
    /// so it lives in the config and never in an engine-side file.
    #[serde(default = "default_ninfer_log_max_size_mib")]
    pub ninfer_log_max_size_mib: u32,
    /// Per-container ninfer log rotation file count, rendered into the canonical
    /// autostart script's `--log-opt max-file`. Validation accepts `1..=50`. Absent
    /// means `5`, the literal the harness always rendered. Same harness-owned-input
    /// rule as [`AppConfig::ninfer_log_max_size_mib`].
    #[serde(default = "default_ninfer_log_max_files")]
    pub ninfer_log_max_files: u32,
    /// The file this config was loaded from (`SIMPLEHARN_CONFIG` or beside the exe),
    /// stamped by the boot loader. `#[serde(skip)]` so it never enters the persisted
    /// JSON. `default_model` write-back targets this exact path — read and write must
    /// be the same file, or a `SIMPLEHARN_CONFIG` run silently loses its defaults on
    /// restart (operator finding, 2026-10-26).
    #[serde(skip, default)]
    pub source_path: Option<std::path::PathBuf>,
    /// Optional shared secret for the API and proxy routes, enforced by the guard via
    /// `X-Har-Token` or `Authorization: Bearer`. Absent means open — safe only on
    /// loopback, because publishing `internal_host` to a segment otherwise hands every
    /// machine there load/unload/config power (deploy review, 2026-10-26).
    /// `SIMPLEHARN_TOKEN` overrides it so the secret can stay out of the file entirely;
    /// the value is never logged and never accepted in a URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_token: Option<String>,
    /// Gate for the test-facing `mock` engine adapter: production boot registers it **only**
    /// when this is true (chunk 10 \[3]). The adapter's argv names the bare program
    /// `mock-engine`, which `CreateProcessW` resolves through the harness's own directories,
    /// so setting this on a machine where untrusted parties can write beside the harness exe
    /// or its cwd is a code-execution grant, not a debugging knob. Tests never need it: each
    /// builds its own adapter map. Always serialized, like [`CategoryConfig::control`].
    #[serde(default)]
    pub allow_mock_engine: bool,
    /// Experimental warm-during-release knob (off by default). When true, a
    /// load that commits a victim fires a fire-and-forget OS-cache warm of
    /// the incoming model's weight files on the blocking pool, using the
    /// driver-confirmed release wait the reload would otherwise idle through.
    /// The warm is a pure hint: it never blocks the actor, never gates
    /// admission, and its failure changes no job outcome. Always serialized,
    /// like [`AppConfig::allow_mock_engine`], so the file states the posture
    /// explicitly instead of relying on the default.
    #[serde(default)]
    pub prefetch_on_release: bool,
    /// Harness-side per-engine defaults (slice 10d: the strata interpreter and
    /// working directory, §20-9 "engines obey only harness-owned configuration").
    /// Unset serializes nothing — existing config files keep their exact shape —
    /// and the resolved values are read through [`AppConfig::strata_python`] and
    /// [`AppConfig::strata_cwd`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engines: Option<EnginesConfig>,
}

impl fmt::Debug for AppConfig {
    /// Prints every field as-is except `control_token`, which renders
    /// `REDACTED(present)` / `REDACTED(absent)` — the posture the status surface
    /// already takes: report *whether* auth is enforced, never the secret.
    ///
    /// `secrecy` was rejected (operator ruling 2026-10-28, FINDINGS-15 \[9]): the fix
    /// for the class belongs at the owning type plus the guard below, and
    /// `expose_secret()` ceremony at each reader would buy no behavior change for a
    /// new dependency AGENTS.md itself polices.
    ///
    /// The destructuring below is deliberately **exhaustive** (no `..`): adding a
    /// field to `AppConfig` is a compile error here until it is named, so a second
    /// secret can never ride a derived `Debug` into the ring unnoticed. That is
    /// this impl's whole safety story — do not replace it with a derive or a `..`
    /// pattern.
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let AppConfig {
            internal_host,
            categories,
            vram_reserve_gb,
            ram_reserve_gb,
            vram_total_gb,
            protected_ports,
            release_timeout_s,
            load_timeout_s,
            measured_sample_s,
            measured_drift_warn_ratio,
            tray_enabled,
            llama_server_exe,
            modelconf_dir,
            settle_timeout_s,
            witness_tolerance_gb,
            greedy_launch_min_free_gb,
            expert_offload_evict_cpu_above_gb,
            wsl_distro,
            wsl_user,
            ninfer_log_max_size_mib,
            ninfer_log_max_files,
            source_path,
            control_token,
            allow_mock_engine,
            prefetch_on_release,
            engines,
        } = self;
        let presence = if control_token.is_some() {
            "present"
        } else {
            "absent"
        };
        out.debug_struct("AppConfig")
            .field("internal_host", internal_host)
            .field("categories", categories)
            .field("vram_reserve_gb", vram_reserve_gb)
            .field("ram_reserve_gb", ram_reserve_gb)
            .field("vram_total_gb", vram_total_gb)
            .field("protected_ports", protected_ports)
            .field("release_timeout_s", release_timeout_s)
            .field("load_timeout_s", load_timeout_s)
            .field("measured_sample_s", measured_sample_s)
            .field("measured_drift_warn_ratio", measured_drift_warn_ratio)
            .field("tray_enabled", tray_enabled)
            .field("llama_server_exe", llama_server_exe)
            .field("modelconf_dir", modelconf_dir)
            .field("settle_timeout_s", settle_timeout_s)
            .field("witness_tolerance_gb", witness_tolerance_gb)
            .field("greedy_launch_min_free_gb", greedy_launch_min_free_gb)
            .field(
                "expert_offload_evict_cpu_above_gb",
                expert_offload_evict_cpu_above_gb,
            )
            .field("wsl_distro", wsl_distro)
            .field("wsl_user", wsl_user)
            .field("ninfer_log_max_size_mib", ninfer_log_max_size_mib)
            .field("ninfer_log_max_files", ninfer_log_max_files)
            .field("source_path", source_path)
            .field("control_token", &format_args!("REDACTED({presence})"))
            .field("allow_mock_engine", allow_mock_engine)
            .field("prefetch_on_release", prefetch_on_release)
            .field("engines", engines)
            .finish()
    }
}

impl Default for AppConfig {
    fn default() -> Self {
        AppConfig {
            internal_host: default_internal_host(),
            categories: default_categories(),
            vram_reserve_gb: default_vram_reserve(),
            ram_reserve_gb: default_ram_reserve(),
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
            expert_offload_evict_cpu_above_gb: default_expert_offload_evict_cpu_above(),
            measured_sample_s: default_measured_sample_s(),
            measured_drift_warn_ratio: default_measured_drift_warn_ratio(),
            wsl_distro: default_wsl_distro(),
            wsl_user: None,
            ninfer_log_max_size_mib: default_ninfer_log_max_size_mib(),
            ninfer_log_max_files: default_ninfer_log_max_files(),
            control_token: None,
            allow_mock_engine: false,
            prefetch_on_release: false,
            engines: None,
            source_path: None,
        }
    }
}

impl AppConfig {
    /// Parses config text for [`CONFIG_FILE`] (the one config name there is).
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
    /// Boot's file-read path (`main.rs::boot_config`). The only boot-time signal a
    /// hand-written `categories` entry gets for a silently empty `evictable_by` is
    /// [`AppConfig::evictable_by_warnings`], logged here per entry (R2-11-01): the
    /// boot honours the posture, it does not hide it.
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
        let config = AppConfig::parse(&text)?;
        // R2-11-01: an invulnerable category may boot — the operator's file is
        // king — but never silently: name it before the first refusal it causes.
        for warning in config.evictable_by_warnings() {
            tracing::warn!("{warning}");
        }
        Ok(config)
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
    /// Walks [`CategoryId::all`] in canonical order, never the file order of
    /// `categories` (R2-11-03): WORK holds 18109 whatever order the hand-written
    /// array uses, and a category absent from the file is skipped (`validate`
    /// reports missing categories separately). A category with a non-empty
    /// `engine_ports` keeps them — `validate` refuses every other length
    /// (R2-11-02) — and the rest are allocated sequentially from
    /// `ENGINE_PORT_BASE`. Each present category advances the cursor by
    /// `max_models`, explicit or not. Validation checks the resolved plan for
    /// collisions.
    pub fn effective_engine_ports(&self) -> BTreeMap<CategoryId, Vec<u16>> {
        let mut cursor = ENGINE_PORT_BASE;
        let mut plan = BTreeMap::new();
        for id in CategoryId::all() {
            let Some(category) = self.categories.iter().find(|c| c.id == id) else {
                continue;
            };
            let explicit_ok = !category.engine_ports.is_empty()
                && category.engine_ports.len() == category.max_models;
            let ports: Vec<u16> = if explicit_ok {
                category.engine_ports.clone()
            } else {
                (0..category.max_models)
                    .map(|offset| cursor + offset as u16)
                    .collect()
            };
            cursor = cursor.saturating_add(category.max_models as u16);
            plan.insert(id, ports);
        }
        plan
    }

    /// Which host-side namespace already claims a port number: the manual
    /// `protected_ports` set, a category public listener, or a resolved engine
    /// port. R10 (operator 2026-11-01: "8003-8005 are manual only") extends the
    /// harness/manual separation to the container-internal namespace: a ninfer
    /// row may not borrow one of these numbers as `ninfer_internal_port`, even
    /// though the netns makes a real collision impossible — the 8004 post-mortem
    /// turned on exactly this confusion. Returns the claim, or `None` when the
    /// number is free of the host namespaces.
    #[must_use]
    pub fn port_claim(&self, port: u16) -> Option<&'static str> {
        if self.protected_ports.contains(&port) {
            return Some("in `protected_ports` (the manual namespace)");
        }
        if self
            .categories
            .iter()
            .any(|category| category.public_port == port)
        {
            return Some("a category public listener port");
        }
        if self
            .effective_engine_ports()
            .values()
            .flatten()
            .any(|engine| *engine == port)
        {
            return Some("a harness engine port");
        }
        None
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

    /// The interpreter a strata launch uses when the model yaml omits
    /// `strata_python`: `engines.strata.python` when present, else
    /// [`STRATA_PYTHON_DEFAULT`]. The yaml knob is the per-model override the
    /// adapter applies on top of this resolved default.
    #[must_use]
    pub fn strata_python(&self) -> std::path::PathBuf {
        self.engines
            .as_ref()
            .and_then(|engines| engines.strata.as_ref())
            .and_then(|strata| strata.python.clone())
            .unwrap_or_else(|| std::path::PathBuf::from(STRATA_PYTHON_DEFAULT))
    }

    /// The working directory a strata launch uses when the model yaml omits
    /// `strata_cwd`: `engines.strata.cwd` when present, else
    /// [`STRATA_CWD_DEFAULT`].
    #[must_use]
    pub fn strata_cwd(&self) -> std::path::PathBuf {
        self.engines
            .as_ref()
            .and_then(|engines| engines.strata.as_ref())
            .and_then(|strata| strata.cwd.clone())
            .unwrap_or_else(|| std::path::PathBuf::from(STRATA_CWD_DEFAULT))
    }

    /// The llama Fit rows: `engines.llama` when the operator wrote the section.
    /// `None` is the inert default — the probe stays off and the ladder
    /// unchanged (R11: the build lands before the card window with zero
    /// surprise probes).
    #[must_use]
    pub fn llama_fit(&self) -> Option<&LlamaEngineConfig> {
        self.engines
            .as_ref()
            .and_then(|engines| engines.llama.as_ref())
    }

    /// Whether the pre-spawn Fit probe may run at all (R11). Unset is false:
    /// the live build is inert until the operator turns it on.
    #[must_use]
    pub fn llama_fit_probe_enabled(&self) -> bool {
        self.llama_fit()
            .and_then(|llama| llama.fit_probe)
            .unwrap_or(false)
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
        // Same discipline for the engines section (slice 10d): present-but-blank
        // is a mistake that must refuse boot, not silently fall back to the
        // documented default the operator did not choose.
        if let Some(strata) = self
            .engines
            .as_ref()
            .and_then(|engines| engines.strata.as_ref())
        {
            for (field, value) in [
                ("engines.strata.python", strata.python.as_deref()),
                ("engines.strata.cwd", strata.cwd.as_deref()),
            ] {
                if value.is_some_and(|path| path.as_os_str().to_string_lossy().trim().is_empty()) {
                    errors.push(ConfigError::Invalid {
                        field: field.to_string(),
                        reason: "must name a non-blank path; omit the key to keep the \
                                 documented default"
                            .to_string(),
                    });
                }
            }
        }
        // R2-11-06: the same present-but-blank doctrine for the engine binary. A
        // blank exe booted clean and failed every llama load later at spawn with
        // an OS error naming `""`, where boot could have named the key.
        if self
            .llama_server_exe
            .as_os_str()
            .to_string_lossy()
            .trim()
            .is_empty()
        {
            errors.push(ConfigError::Invalid {
                field: "llama_server_exe".to_string(),
                reason: "must name a non-blank path; omit the key to keep the documented \
                         default"
                    .to_string(),
            });
        }
        // R11 Fit calibration: every figure that feeds a VRAM booking must be
        // finite and non-negative (the `parse_gib` doctrine — a NaN comparison
        // is false, and a negative margin would shrink bookings below the
        // projection). A timeout of 0 would refuse the probe outright.
        if let Some(llama) = self.llama_fit() {
            for (field, value) in [
                ("engines.llama.fit_margin_gb", llama.fit_margin_gb),
                (
                    "engines.llama.mmproj_surcharge_gb",
                    llama.mmproj_surcharge_gb,
                ),
            ] {
                if value.is_some_and(|v| !v.is_finite() || v < 0.0) {
                    errors.push(ConfigError::Invalid {
                        field: field.to_string(),
                        reason: "must be a finite non-negative GiB figure (these are Wave-C \
                                 calibrations feeding the VRAM booking)"
                            .to_string(),
                    });
                }
            }
            if llama.fit_probe_timeout_s == Some(0) {
                errors.push(ConfigError::Invalid {
                    field: "engines.llama.fit_probe_timeout_s".to_string(),
                    reason: "must be at least 1 second; omit the key to keep the default"
                        .to_string(),
                });
            }
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
            // R2-11-02: a non-empty explicit plan must name one port per slot.
            // A length typo used to discard the whole hand-picked plan at boot
            // and fail at spawn with a bind conflict; now it fails at boot.
            if !cat.engine_ports.is_empty() && cat.engine_ports.len() != cat.max_models {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}.engine_ports", cat.id),
                    reason: format!(
                        "must name exactly {} port(s), one per slot, got {}; an empty \
                         list keeps the automatic plan",
                        cat.max_models,
                        cat.engine_ports.len()
                    ),
                });
            }
            // Display label (operator ruling 2026-11-05): absent or blank is the
            // documented fallback to the id, so only a non-blank label is judged.
            // The cap bounds the tab strip; the printability rule keeps control
            // characters (a terminal escape, a stray \0) out of the page and the
            // status document, where they corrupt rendering invisibly.
            if let Some(label) = cat
                .label
                .as_deref()
                .map(str::trim)
                .filter(|l| !l.is_empty())
            {
                if label.chars().count() > CATEGORY_LABEL_MAX_CHARS {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{}.label", cat.id),
                        reason: format!(
                            "display label must be at most {CATEGORY_LABEL_MAX_CHARS} \
                             characters, got {}",
                            label.chars().count()
                        ),
                    });
                }
                if label.chars().any(char::is_control) {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{}.label", cat.id),
                        reason: "display label must be printable (no control characters)"
                            .to_string(),
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
            if cat.public_port == 0 {
                errors.push(ConfigError::Invalid {
                    field: format!("categories.{}.public_port", cat.id),
                    reason: "must not be 0 (port 0 binds an OS-chosen ephemeral listener, so \
                             /har/status and the tray report a green listener no client can \
                             address at the documented port; 0 is also the harness's own \
                             \"no port\" sentinel, which hides the slot from the no-`model` \
                             proxy path)"
                        .to_string(),
                });
            }
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
        if !(self.vram_total_gb.is_finite() && self.vram_total_gb > 0.0) {
            errors.push(ConfigError::Invalid {
                field: "vram_total_gb".to_string(),
                reason: "must be a finite number greater than 0 (the ledger ceiling, Step 4): \
                         +inf admits the world onto one card and NaN makes every comparison \
                         false, refusing forever"
                    .to_string(),
            });
        }
        if !(self.vram_reserve_gb.is_finite() && self.vram_reserve_gb >= 0.0) {
            errors.push(ConfigError::Invalid {
                field: "vram_reserve_gb".to_string(),
                reason: "must be a finite number greater than or equal to 0 (the headroom kept \
                         unreachable by admission, Step 4 rule 2; 0 means no reserve, while a \
                         negative reserve makes the budget grow as more reserve is asked for)"
                    .to_string(),
            });
        }
        if !(self.ram_reserve_gb.is_finite() && self.ram_reserve_gb >= 0.0) {
            errors.push(ConfigError::Invalid {
                field: "ram_reserve_gb".to_string(),
                reason: "must be a finite number greater than or equal to 0 (the host headroom \
                         kept unreachable by admission for CPU-resident rows; 0 means no \
                         reserve, while a negative reserve makes the RAM budget grow as more \
                         reserve is asked for)"
                    .to_string(),
            });
        }
        if !(self.witness_tolerance_gb.is_finite() && self.witness_tolerance_gb >= 0.0) {
            errors.push(ConfigError::Invalid {
                field: "witness_tolerance_gb".to_string(),
                reason: "must be a finite number greater than or equal to 0 (the witness \
                         disagreement band, Step 4 rule 4; +inf makes the settle gate's \
                         `baseline + tolerance` test always true, deleting the pre-spawn \
                         physical check)"
                    .to_string(),
            });
        }
        // Expert-offload RAM reclaim (operator ruling 2026-11-06): the same GiB
        // discipline as the reserves — finite and non-negative. `0` is a
        // deliberate disable, not a typo; a NaN comparison is false and would
        // read as "nothing exceeds the threshold" by accident instead of by
        // setting, and +inf would never name a victim whatever the host holds.
        if !(self.expert_offload_evict_cpu_above_gb.is_finite()
            && self.expert_offload_evict_cpu_above_gb >= 0.0)
        {
            errors.push(ConfigError::Invalid {
                field: "expert_offload_evict_cpu_above_gb".to_string(),
                reason: "must be a finite number greater than or equal to 0 (the host-GiB \
                         threshold above which a greedy expert-offload load evicts a \
                         CPU-resident row; 0 disables the rule, while a negative figure \
                         would evict every CPU resident and NaN makes every comparison \
                         false)"
                    .to_string(),
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
        // Same shape as the per-model `load_timeout` key (model/mod.rs): a u64 rejects
        // nan/inf outright, and the ceiling guards a typo that would park a load job in
        // readiness forever (FINDINGS-11 [6]).
        if self.load_timeout_s == 0 || self.load_timeout_s > MAX_TIMEOUT_S {
            errors.push(ConfigError::Invalid {
                field: "load_timeout_s".to_string(),
                reason: format!(
                    "must be 1..={MAX_TIMEOUT_S} (readiness deadline, Step 3): a zero deadline fails every \
                     load instantly and one mistyped digit parks the job in `Loading` forever, \
                     holding its slot, port and VRAM"
                ),
            });
        }
        if self.release_timeout_s == 0 || self.release_timeout_s > MAX_TIMEOUT_S {
            errors.push(ConfigError::Invalid {
                field: "release_timeout_s".to_string(),
                reason: format!(
                    "must be 1..={MAX_TIMEOUT_S} (G12 release-confirmation deadline, §0.3): a zero \
                     timeout kills the victim and then fails its own confirmation, so the \
                     dead model stays installed and the new load never spawns, while one \
                     mistyped digit past a day parks the job in `Dispose` forever if the \
                     release evidence never arrives"
                ),
            });
        }
        if self.settle_timeout_s == 0 || self.settle_timeout_s > MAX_TIMEOUT_S {
            errors.push(ConfigError::Invalid {
                field: "settle_timeout_s".to_string(),
                reason: format!(
                    "must be 1..={MAX_TIMEOUT_S} (G12 memory-settle deadline after a confirmed \
                     release, §0.3): a zero timeout is the same loss one step later, once \
                     the eviction has already run, while one mistyped digit past a day \
                     parks the job past the settle gate forever if the memory never \
                     settles"
                ),
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
        // Drift-warning ratio: a share of the declared figure, so a valid ratio is a
        // fraction. `0.0` is a deliberate silence, not a typo; a NaN reads as outside
        // the range and is refused. It gates only the warning, never a booking.
        if !(0.0..=1.0).contains(&self.measured_drift_warn_ratio) {
            errors.push(ConfigError::Invalid {
                field: "measured_drift_warn_ratio".to_string(),
                reason: "must be 0.0..=1.0 (a fraction of the declared vram_gb; 0.0 silences the \
                         measured-drift warning entirely)"
                    .to_string(),
            });
        }
        // ninfer container log rotation: bounded so one mistyped digit cannot render a
        // `max-size` or `max-file` that outruns the container's own log discipline.
        // Both land byte-identical at the default (25 MiB, 5 files).
        if self.ninfer_log_max_size_mib == 0 || self.ninfer_log_max_size_mib > 1024 {
            errors.push(ConfigError::Invalid {
                field: "ninfer_log_max_size_mib".to_string(),
                reason: "must be 1..=1024 (per-container log rotation size in MiB; 25 is the \
                         default)"
                    .to_string(),
            });
        }
        if self.ninfer_log_max_files == 0 || self.ninfer_log_max_files > 50 {
            errors.push(ConfigError::Invalid {
                field: "ninfer_log_max_files".to_string(),
                reason: "must be 1..=50 (per-container log rotation file count; 5 is the default)"
                    .to_string(),
            });
        }
        // Engine ports: unique among themselves and disjoint from both namespaces.
        let mut engine_ports_seen = BTreeSet::new();
        for (category, ports) in self.effective_engine_ports() {
            for port in ports {
                if port == 0 {
                    errors.push(ConfigError::Invalid {
                        field: format!("categories.{category}.engine_ports"),
                        reason: "must not be 0 (the engine argv gets `--port 0`, so the engine \
                                 binds an OS-chosen ephemeral port while the harness probes :0 \
                                 until the readiness deadline and the kill path keys the port \
                                 the engine does not hold)"
                            .to_string(),
                    });
                }
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
                    "must be a loopback address or 0.0.0.0 (publishing to the local net: \
                     `control_token`/SIMPLEHARN_TOKEN gate every route but the page assets; \
                     with no token set the segment is trusted); got {}",
                    self.internal_host
                ),
            });
        }
        errors
    }

    /// Boot warnings for eviction privileges a written entry left empty (R2-11-01).
    ///
    /// The categories carry the whole safety policy and the file is hand-edited
    /// (module doc, D1), but `#[serde(default)]` yields the same `Vec` for an
    /// omitted `evictable_by` key and for an explicit `[]` — the schema cannot tell
    /// the two spellings apart, so neither can this decision: every warning names
    /// both, the category, and the consequence (a load needing that category's VRAM
    /// is refused by everyone). WORK is exempt: being evicted by nobody is its
    /// published posture (README matrix, [`CategoryConfig::four_defaults`]), so a
    /// WORK warning would fire on every default boot and accuse the documented
    /// value. Pure and side-effect-free: [`AppConfig::load`] logs each entry at
    /// boot, and `validate()` stays silent — it also runs on every write-back
    /// re-check, where a recurring warning would only train the operator to
    /// ignore the log. The warning changes no behavior: an empty list still means
    /// exactly what it means today.
    #[must_use]
    pub fn evictable_by_warnings(&self) -> Vec<String> {
        self.categories
            .iter()
            .filter(|cat| cat.id != CategoryId::Work && cat.evictable_by.is_empty())
            .map(|cat| {
                format!(
                    "category {id}: `evictable_by` is empty (key omitted or `[]`), so nobody \
                     may evict {id} occupants and every load that needs their VRAM is refused; \
                     name the evicting categories on this entry (e.g. `\"evictable_by\": \
                     [\"WORK\"]`) unless an invulnerable category was the intent",
                    id = cat.id,
                )
            })
            .collect()
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

    /// FINDINGS-15 [9] pin (a): the crown jewel never rides `Debug`. Falsifier: put
    /// `Debug` back on the derive list and the sentinel shows up in the text.
    #[test]
    fn the_config_debug_never_prints_the_control_token() {
        const SENTINEL: &str = "TOPSECRET-4b7e1a";
        let cfg = AppConfig {
            control_token: Some(SENTINEL.to_string()),
            ..AppConfig::default()
        };
        let text = format!("{cfg:?}");
        assert!(!text.contains(SENTINEL), "Debug leaked the token: {text}");
        assert!(
            text.contains("control_token: REDACTED(present)"),
            "the marker must report that auth is enforced: {text}"
        );
    }

    /// FINDINGS-15 [9] pin (d): the marker encodes presence, so a constant
    /// placeholder fails here — `None` and `Some` must read differently, and the
    /// unconfigured config must not claim `present`.
    #[test]
    fn the_debug_reports_token_presence_as_information() {
        let set = format!(
            "{:?}",
            AppConfig {
                control_token: Some("SENTINEL-A".to_string()),
                ..AppConfig::default()
            }
        );
        let open = format!(
            "{:?}",
            AppConfig {
                control_token: None,
                ..AppConfig::default()
            }
        );
        assert!(set.contains("control_token: REDACTED(present)"), "{set}");
        assert!(open.contains("control_token: REDACTED(absent)"), "{open}");
        assert!(
            !open.contains("REDACTED(present)"),
            "an open harness must not read as authed: {open}"
        );
        assert_ne!(set, open, "presence is the reportable half of the fact");
    }

    /// FINDINGS-15 [9] pin (c): the redaction is a needle, not a blackout — every
    /// other field still prints, by name and with its value. Falsifier: an impl
    /// that stops listing fields (or redacts more than the token) drops a name, a
    /// value, or trips the single-marker count.
    #[test]
    fn the_config_debug_prints_every_non_secret_field() {
        let cfg = AppConfig {
            vram_reserve_gb: 2.5,
            ram_reserve_gb: 9.5,
            vram_total_gb: 47.5,
            protected_ports: [8003, 18203].into_iter().collect(),
            release_timeout_s: 11,
            load_timeout_s: 22,
            measured_sample_s: 33,
            tray_enabled: true,
            llama_server_exe: std::path::PathBuf::from("F:/bin/llama-server.exe"),
            modelconf_dir: std::path::PathBuf::from("F:/modelconf"),
            settle_timeout_s: 44,
            witness_tolerance_gb: 1.5,
            greedy_launch_min_free_gb: 6.5,
            expert_offload_evict_cpu_above_gb: 5.5,
            wsl_distro: "Ubuntu-24.04".to_string(),
            wsl_user: Some("operator".to_string()),
            source_path: Some(std::path::PathBuf::from("F:/simpleharn.json")),
            control_token: Some("SENTINEL-B".to_string()),
            allow_mock_engine: true,
            prefetch_on_release: true,
            ..AppConfig::default()
        };
        let text = format!("{cfg:?}");
        for (name, value) in [
            ("internal_host", "127.0.0.1"),
            ("categories", "CategoryConfig"),
            ("vram_reserve_gb", "2.5"),
            ("ram_reserve_gb", "9.5"),
            ("vram_total_gb", "47.5"),
            ("protected_ports", "8003"),
            ("release_timeout_s", "11"),
            ("load_timeout_s", "22"),
            ("measured_sample_s", "33"),
            ("tray_enabled", "true"),
            ("llama_server_exe", "llama-server.exe"),
            ("modelconf_dir", "F:/modelconf"),
            ("settle_timeout_s", "44"),
            ("witness_tolerance_gb", "1.5"),
            ("greedy_launch_min_free_gb", "6.5"),
            ("expert_offload_evict_cpu_above_gb", "5.5"),
            ("wsl_distro", "Ubuntu-24.04"),
            ("wsl_user", "operator"),
            ("source_path", "simpleharn.json"),
            ("allow_mock_engine", "true"),
            ("prefetch_on_release", "true"),
        ] {
            assert!(
                text.contains(&format!("{name}:")) && text.contains(value),
                "field {name} (or its value) vanished from Debug: {text}"
            );
        }
        assert_eq!(
            text.matches("REDACTED").count(),
            1,
            "only the token is redacted, nothing else: {text}"
        );
        assert!(
            !text.contains("SENTINEL-B"),
            "the value still must not appear: {text}"
        );
    }

    /// R10 (operator 2026-11-01: "8003-8005 are manual only"): the claim helper the
    /// container-internal check consults names each host-side namespace it owns —
    /// the manual protected set, a category listener, the engine plan — and keeps
    /// the SE internal default (8808) clear of all three.
    #[test]
    fn a_port_claim_names_the_namespace_that_owns_the_number() {
        let cfg = AppConfig::default();
        let manual = cfg.port_claim(8004).expect("8004 is the manual namespace");
        assert!(manual.contains("protected_ports"), "{manual}");
        let listener = cfg.port_claim(8708).expect("8708 is a listener");
        assert!(listener.contains("listener"), "{listener}");
        let engine = cfg
            .port_claim(18109)
            .expect("18109 is the first engine port");
        assert!(engine.contains("engine port"), "{engine}");
        assert_eq!(
            cfg.port_claim(
                crate::engine::ninfer::DEFAULT_INTERNAL_PORT
                    .parse()
                    .expect("the default is a port")
            ),
            None,
            "the SE internal default must stay free of every host namespace"
        );
        // An operator-edited plan is judged by the SAME resolved plan the slots
        // are built from: an explicit engine port and a moved listener claim
        // exactly like the defaults.
        let mut edited = AppConfig::default();
        edited.categories[0].engine_ports = vec![20_500];
        edited.categories[1].public_port = 9_999;
        assert!(
            cfg.port_claim(20_500).is_none(),
            "the default plan does not own an explicit plan's port"
        );
        let owned = edited
            .port_claim(20_500)
            .expect("the edited explicit plan owns its port");
        assert!(owned.contains("engine port"), "{owned}");
        assert!(edited
            .port_claim(9_999)
            .expect("a moved listener owns its number")
            .contains("listener"));
    }

    /// FINDINGS-15 [9] pin (b): the exposure the finding named is the ring, so this
    /// pin runs the ring's own storage path. `RingLayer::on_event` records every
    /// field as `format!("{value:?}")` and hands the entry to `ring_push` — the one
    /// choke point a captured event passes through — so pushing exactly the field a
    /// future `debug!(?config)` would store, and reading it back the way `/har/log`
    /// does, tests real storage rather than a mirrored copy of the layer.
    ///
    /// The ring is a single process-global `static` shared with
    /// `observability::tests::ring_caps_at_capacity_and_reports_drops`, which drains
    /// it and asserts the eviction counter (FINDINGS-BATCH-2026-10-28I [2]: 18/20
    /// co-scheduled runs failed before this was fixed). Two defences, both kept: the
    /// pin takes `lock_ring_for_test` so the two never interleave, and it asserts by
    /// **set difference** over a before/after snapshot — never by total length,
    /// position, or a count of what anyone else wrote, so a future ring writer cannot
    /// make it flaky in either direction.
    /// Falsifier: restore the derived `Debug` and the stored field carries the
    /// sentinel.
    #[test]
    fn the_log_ring_cannot_capture_the_control_token_from_a_config_debug() {
        use crate::observability::{
            lock_ring_for_test, ring_push, ring_tail, LogEntry, RING_CAPACITY,
        };
        use std::collections::BTreeSet;
        const SENTINEL: &str = "RINGSENTINEL-9d2f7c";
        const TARGET: &str = "config_debug_capture_pin";
        const MESSAGE: &str = "config snapshot under review";
        // An entry's identity, timestamp excluded: two entries are the same only if
        // target, level, message and every field match.
        type EntryKey = (String, String, String, Vec<(String, String)>);
        fn key_of(e: &LogEntry) -> EntryKey {
            (
                e.target.clone(),
                e.level.clone(),
                e.message.clone(),
                e.fields.clone().into_iter().collect(),
            )
        }
        let cfg = AppConfig {
            control_token: Some(SENTINEL.to_string()),
            ..AppConfig::default()
        };
        let rendered = format!("{cfg:?}");

        let _serial = lock_ring_for_test();
        let before: BTreeSet<EntryKey> = ring_tail(RING_CAPACITY).0.iter().map(key_of).collect();
        let mut entry = LogEntry::now("DEBUG", TARGET, MESSAGE);
        entry.fields.insert("config".to_string(), rendered);
        ring_push(entry);
        let after = ring_tail(RING_CAPACITY).0;

        // (a) the sentinel appears nowhere in the ring at all — every entry,
        // this pin's and any foreign one, message and every structured field.
        assert!(
            after.iter().all(|e| !e.message.contains(SENTINEL)
                && e.fields.values().all(|v| !v.contains(SENTINEL))),
            "no ring entry may hold the control token"
        );
        // (b) the entry this pin wrote is among the NEW ones (set difference, so
        // neither the ring's total length nor another writer's traffic matters), and
        // what it stores still tells the operator that auth is enforced.
        let mine: Vec<&LogEntry> = after
            .iter()
            .filter(|e| e.target == TARGET && e.message == MESSAGE && !before.contains(&key_of(e)))
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "this pin's own capture must be among the entries that were not there before"
        );
        assert!(
            mine[0]
                .fields
                .values()
                .any(|v| v.contains("REDACTED(present)")),
            "the ring must still report that auth is enforced: {:?}",
            mine[0]
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

    /// TASK-RAM-RECLAIM (operator ruling 2026-11-06): the reclaim threshold
    /// defaults to 4.0 GiB, `0` is the deliberate disable, and a nonsense
    /// figure fails boot naming the key — the same GiB discipline as the
    /// reserve keys (finite and non-negative; serde_json refuses the `1e999`
    /// spelling, so +inf and NaN only arrive hand-built).
    /// Falsifier: drop the validate arm and all three bad figures boot clean;
    /// invert `>= 0.0` to `> 0.0` and the documented disable (`0`) is refused.
    #[test]
    fn expert_offload_evict_threshold_defaults_disables_and_bounds() {
        assert_eq!(default_expert_offload_evict_cpu_above(), 4.0);
        assert_eq!(AppConfig::default().expert_offload_evict_cpu_above_gb, 4.0);
        let disabled =
            AppConfig::parse("{\"expert_offload_evict_cpu_above_gb\": 0.0}").expect("parses");
        assert!(
            disabled.validate().is_empty(),
            "0 disables the rule and is legal: {:?}",
            disabled.validate()
        );
        let negative =
            AppConfig::parse("{\"expert_offload_evict_cpu_above_gb\": -1.0}").expect("parses");
        assert!(fails_with(&negative, "expert_offload_evict_cpu_above_gb"));
        let nan = AppConfig {
            expert_offload_evict_cpu_above_gb: f64::NAN,
            ..AppConfig::default()
        };
        assert!(
            fails_with(&nan, "expert_offload_evict_cpu_above_gb"),
            "NaN makes every threshold comparison false and would silently disable the rule"
        );
        let inf = AppConfig {
            expert_offload_evict_cpu_above_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(
            fails_with(&inf, "expert_offload_evict_cpu_above_gb"),
            "+inf names no victim whatever the host holds"
        );
        assert!(
            AppConfig::parse("{\"expert_offload_evict_cpu_above_gb\": 1e999}").is_err(),
            "the JSON route refuses the overflow spelling too"
        );
    }

    #[test]
    fn mock_engine_adapter_is_off_until_the_config_opts_in() {
        // The `mock` adapter spawns the bare program name `mock-engine`, so boot registers
        // it only on an explicit opt-in: absent means off, and a file written before the
        // key existed must still parse rather than fail the boot (§0.2 step 1).
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert!(
            !absent.allow_mock_engine,
            "a stray `engine: mock` yaml must not find an adapter at boot"
        );
        assert_eq!(absent, AppConfig::default());
        assert!(absent.validate().is_empty());

        let older_file = AppConfig::parse("{\"tray_enabled\": false, \"load_timeout_s\": 300}")
            .expect("a config written before the key existed still parses");
        assert!(!older_file.allow_mock_engine);
        assert!(older_file.validate().is_empty());

        let opted_in = AppConfig::parse("{\"allow_mock_engine\": true}").expect("flag parses");
        assert!(
            opted_in.allow_mock_engine,
            "the key is the only thing that turns the grant on"
        );
        assert!(opted_in.validate().is_empty());
    }

    #[test]
    fn allow_mock_engine_survives_the_config_write_back() {
        // §6 restart guard: the flag decides whether boot hands out a code-execution grant,
        // so the value the write-back stamps must come back through parse+validate unchanged
        // — a restart that quietly flipped it would either revoke an operator's setup or
        // re-open the planted-`mock-engine.exe` path without a config edit.
        let opted_in = AppConfig {
            allow_mock_engine: true,
            ..AppConfig::default()
        };
        let text = write_back_text(&opted_in);
        let reloaded = AppConfig::parse(&text).expect("persisted grant parses");
        let errors = reloaded.validate();
        assert!(
            errors.is_empty(),
            "persisted config must validate: {errors:?}"
        );
        assert!(
            reloaded.allow_mock_engine,
            "an opt-in must survive a write-back and a restart"
        );

        // Pinned serde choice: false is serialized (like `control`), never omitted, so the
        // operator's file states the denial explicitly instead of relying on a default.
        let text = write_back_text(&AppConfig::default());
        let stamped: serde_json::Value = serde_json::from_str(&text).expect("stamped json parses");
        assert_eq!(
            stamped["allow_mock_engine"],
            serde_json::Value::Bool(false),
            "the default must be written, not skipped"
        );
        let reloaded = AppConfig::parse(&text).expect("persisted denial parses");
        assert!(!reloaded.allow_mock_engine);
        assert!(reloaded.validate().is_empty());
    }

    #[test]
    fn warm_during_release_stays_off_until_the_config_opts_in() {
        // The experimental warm knob: absent means off, and a file written
        // before the key existed must still parse rather than fail the boot
        // (§0.2 step 1, same discipline as the mock-engine grant).
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert!(
            !absent.prefetch_on_release,
            "a hint that reads tens of GiB per swap must never be on by default"
        );
        assert_eq!(absent, AppConfig::default());
        assert!(absent.validate().is_empty());

        let older_file = AppConfig::parse("{\"tray_enabled\": false, \"load_timeout_s\": 300}")
            .expect("a config written before the key existed still parses");
        assert!(!older_file.prefetch_on_release);
        assert!(older_file.validate().is_empty());

        let opted_in = AppConfig::parse("{\"prefetch_on_release\": true}").expect("flag parses");
        assert!(
            opted_in.prefetch_on_release,
            "the key is the only thing that turns the warm on"
        );
        assert!(opted_in.validate().is_empty());
    }

    #[test]
    fn prefetch_on_release_survives_the_config_write_back() {
        // §6 restart guard: an operator who turned the spike on must find it
        // still on (and one who never touched it still off) after a
        // write-back and a restart.
        let opted_in = AppConfig {
            prefetch_on_release: true,
            ..AppConfig::default()
        };
        let text = write_back_text(&opted_in);
        let reloaded = AppConfig::parse(&text).expect("persisted grant parses");
        let errors = reloaded.validate();
        assert!(
            errors.is_empty(),
            "persisted config must validate: {errors:?}"
        );
        assert!(
            reloaded.prefetch_on_release,
            "an opt-in must survive a write-back and a restart"
        );

        // Pinned serde choice: false is serialized (like `allow_mock_engine`),
        // never omitted, so the file states the denial explicitly.
        let text = write_back_text(&AppConfig::default());
        let stamped: serde_json::Value = serde_json::from_str(&text).expect("stamped json parses");
        assert_eq!(
            stamped["prefetch_on_release"],
            serde_json::Value::Bool(false),
            "the default must be written, not skipped"
        );
        let reloaded = AppConfig::parse(&text).expect("persisted denial parses");
        assert!(!reloaded.prefetch_on_release);
        assert!(reloaded.validate().is_empty());
    }

    /// Slice 10d discipline (the inverted-`skip_serializing_if` GOTCHAS class):
    /// the new `engines:` section must survive serialize→parse→validate, and the
    /// unset default must not change the serialized file shape at all.
    #[test]
    fn the_engines_section_survives_the_config_round_trip() {
        let pinned = AppConfig {
            engines: Some(EnginesConfig {
                strata: Some(StrataEngineConfig {
                    python: Some(std::path::PathBuf::from(
                        "C:/Strata/.venv/Scripts/python.exe",
                    )),
                    cwd: Some(std::path::PathBuf::from("C:/Strata")),
                }),
                llama: Some(LlamaEngineConfig {
                    fit_probe: Some(true),
                    fit_margin_gb: Some(2.5),
                    mmproj_surcharge_gb: Some(1.25),
                    fit_probe_timeout_s: Some(20),
                }),
            }),
            ..AppConfig::default()
        };
        let text = write_back_text(&pinned);
        let reloaded = AppConfig::parse(&text).expect("a pinned section parses back");
        assert!(
            reloaded.validate().is_empty(),
            "a pinned section must validate: {:?}",
            reloaded.validate()
        );
        assert_eq!(reloaded, pinned, "the section must round-trip unchanged");
        let unset: serde_json::Value =
            serde_json::from_str(&write_back_text(&AppConfig::default())).expect("stamped json");
        assert!(
            unset.get("engines").is_none(),
            "an unset section stays out of the file: existing shape must not change"
        );
        // The R11 half: a half-written llama section round-trips with its
        // absent keys ABSENT, never null (the unset-not-serialized rule).
        let half = AppConfig {
            engines: Some(EnginesConfig {
                strata: None,
                llama: Some(LlamaEngineConfig {
                    fit_probe: Some(true),
                    fit_margin_gb: None,
                    mmproj_surcharge_gb: None,
                    fit_probe_timeout_s: None,
                }),
            }),
            ..AppConfig::default()
        };
        let half_json: serde_json::Value =
            serde_json::from_str(&write_back_text(&half)).expect("half section serializes");
        let llama = &half_json["engines"]["llama"];
        assert_eq!(llama["fit_probe"], serde_json::json!(true));
        for absent in [
            "fit_margin_gb",
            "mmproj_surcharge_gb",
            "fit_probe_timeout_s",
        ] {
            assert!(
                llama.get(absent).is_none(),
                "`{absent}` must stay out of the file, not arrive as null"
            );
        }
        assert_eq!(
            AppConfig::parse(&write_back_text(&half)).expect("half parses back"),
            half
        );
    }

    /// The defaults the `engines:` section resolves to are exactly the values
    /// every strata launch carried before the section existed — the move of the
    /// adapter constants must be invisible to an operator who edits nothing.
    #[test]
    fn the_engines_section_defaults_are_the_documented_fallbacks() {
        let default = AppConfig::default();
        assert_eq!(
            default.strata_python(),
            std::path::PathBuf::from(STRATA_PYTHON_DEFAULT)
        );
        assert_eq!(
            default.strata_cwd(),
            std::path::PathBuf::from(STRATA_CWD_DEFAULT)
        );
        let half = AppConfig {
            engines: Some(EnginesConfig {
                strata: Some(StrataEngineConfig {
                    python: Some(std::path::PathBuf::from("D:/other/python.exe")),
                    cwd: None,
                }),
                llama: None,
            }),
            ..AppConfig::default()
        };
        assert_eq!(
            half.strata_python(),
            std::path::PathBuf::from("D:/other/python.exe")
        );
        assert_eq!(
            half.strata_cwd(),
            std::path::PathBuf::from(STRATA_CWD_DEFAULT),
            "a half-pinned section keeps the documented default for the absent key"
        );
    }

    /// A blank engines path is a mistake that must refuse boot (the
    /// `control_token` doctrine), and a typo — a section for an engine with no
    /// harness-side defaults at all — is refused, not ignored. R11 gave the
    /// llama row a real defaults section, so the refused stand-in moved to an
    /// engine that still owns none.
    #[test]
    fn a_blank_or_unknown_engines_entry_is_refused() {
        let blank =
            AppConfig::parse(r#"{"engines": {"strata": {"python": "   "}}}"#).expect("parses");
        assert!(
            fails_with(&blank, "engines.strata.python"),
            "{:?}",
            blank.validate()
        );
        assert!(
            AppConfig::parse(r#"{"engines": {"strata": {"pyton": "x"}}}"#).is_err(),
            "a misspelled engines subkey must not be silently ignored"
        );
        assert!(
            AppConfig::parse(r#"{"engines": {"llama": {"fit_margin": 2.5}}}"#).is_err(),
            "a misspelled llama subkey must not be silently ignored"
        );
        assert!(
            AppConfig::parse(r#"{"engines": {"ninfer": {}}}"#).is_err(),
            "the harness owns no ninfer-side defaults section to accept"
        );
    }

    /// R2-11-06: a present-but-blank engine binary is the same mistake the other
    /// path fields refuse at boot — absent means the documented default, blank
    /// is a typo that would fail every llama load later at spawn with an OS
    /// error naming `""`.
    /// Falsifier: drop the validate() check and both blank spellings boot clean.
    #[test]
    fn a_blank_llama_server_exe_is_refused_at_boot() {
        for blank in [
            r#"{"llama_server_exe": ""}"#,
            r#"{"llama_server_exe": "   "}"#,
        ] {
            let config = AppConfig::parse(blank).expect("parses as json");
            assert!(
                fails_with(&config, "llama_server_exe"),
                "a blank exe path must name its key at boot, not at spawn: {:?}",
                config.validate()
            );
        }
        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert!(
            absent.validate().is_empty(),
            "an absent key keeps the documented default: {:?}",
            absent.validate()
        );
    }

    /// R11 inertness, pinned: the default config probes NOTHING. The probe
    /// only exists when the operator writes `fit_probe: true`, and even then
    /// the calibration constants stay unset (a warning at use time, never a
    /// silent default that pretends to be measured).
    #[test]
    fn the_llama_fit_probe_is_inert_until_the_operator_turns_it_on() {
        assert!(!AppConfig::default().llama_fit_probe_enabled());
        assert!(AppConfig::default().llama_fit().is_none());
        let off =
            AppConfig::parse(r#"{"engines": {"llama": {"fit_probe": false}}}"#).expect("parses");
        assert!(!off.llama_fit_probe_enabled());
        assert!(off.validate().is_empty());
        let on = AppConfig::parse(
            r#"{"engines": {"llama": {"fit_probe": true, "fit_margin_gb": 2.0}}}"#,
        )
        .expect("parses");
        assert!(on.llama_fit_probe_enabled());
        assert!(on.validate().is_empty(), "{:?}", on.validate());
        assert!(
            on.llama_fit().and_then(|l| l.mmproj_surcharge_gb).is_none(),
            "an unset calibration stays unset: there is no honest default"
        );
        for bad in [
            r#"{"engines": {"llama": {"fit_margin_gb": -1}}}"#,
            r#"{"engines": {"llama": {"mmproj_surcharge_gb": -1}}}"#,
            r#"{"engines": {"llama": {"fit_probe_timeout_s": 0}}}"#,
        ] {
            let config = AppConfig::parse(bad).expect("parses as json");
            assert!(
                !config.validate().is_empty(),
                "a nonsense calibration must refuse boot: {bad}"
            );
        }
    }

    /// True when `validate()` reports at least one problem naming `field`.
    fn fails_with(config: &AppConfig, field: &str) -> bool {
        config
            .validate()
            .iter()
            .any(|e| e.to_string().contains(field))
    }

    /// The write-back document with one key mutated, as the parser sees it. Used for
    /// category-level cases: hand-spelling a category would have to get all eight key names
    /// right just to reach the case under test, and this keeps the field list the
    /// serializer's rather than a copy that can drift from it.
    fn parse_mutated(
        mutation: impl FnOnce(&mut serde_json::Value),
    ) -> Result<AppConfig, ConfigError> {
        let mut root = serde_json::to_value(AppConfig::default()).expect("invariant: serializes");
        mutation(&mut root);
        AppConfig::parse(&root.to_string())
    }

    #[test]
    fn category_typos_are_refused_not_silently_defaulted() {
        // FINDINGS-11 [1], mirroring `unknown_field_is_an_error_not_a_silent_default` one
        // level down: the categories hold the whole safety policy and are hand-edited, so
        // `"max_model": 6` must fail the boot instead of quietly booting a one-slot AUX.
        let err = parse_mutated(|root| root["categories"][2]["max_model"] = serde_json::json!(6))
            .expect_err("`max_model` is not a CategoryConfig key");
        let message = err.to_string();
        assert!(
            message.contains("max_model") && message.contains(CONFIG_FILE),
            "the error must name the unknown field and the file: {message}"
        );

        // Over-rejection guard: every name the serializer writes is a name the parser
        // accepts, so the write-back round trip cannot strand the operator's file.
        let exact = parse_mutated(|_| {}).expect("exact field names must parse");
        assert!(exact.validate().is_empty(), "{:?}", exact.validate());
    }

    /// R2-11-01 pin: the omission route of the D1 posture. `#[serde(default)]` gives
    /// the same `Vec` for an omitted `evictable_by` key and for an explicit `[]`, so
    /// the schema cannot tell the two spellings apart — the boot decision therefore
    /// fires on the effective empty list and the text names both spellings, the
    /// category, and the consequence. Behavior stays: the file still validates and
    /// the list stays empty. WORK stays silent: "evicted by nobody" is its published
    /// posture, so a WORK warning would fire on every default boot.
    /// Falsifier: drop the filter and the omitted/explicit cases go silent; drop the
    /// WORK exemption and the shipped-config case fails.
    #[test]
    fn an_empty_evictable_by_is_named_at_boot_for_both_spellings() {
        // The shipped posture: WORK empty by design, RP/AUX/EMB populated.
        let shipped = AppConfig::parse("{}").expect("defaults parse");
        assert!(shipped.validate().is_empty());
        assert!(
            shipped.evictable_by_warnings().is_empty(),
            "WORK's documented empty list must not warn, and neither may a populated one: {:?}",
            shipped.evictable_by_warnings()
        );

        // The finding's route: a hand-written RP entry that omits the key.
        let mut omitted = parse_mutated(|root| {
            let _ = root["categories"][1]
                .as_object_mut()
                .expect("RP entry is an object")
                .remove("evictable_by");
        })
        .expect("an absent evictable_by key parses to the serde default");
        assert!(
            omitted
                .category_mut(CategoryId::Rp)
                .expect("RP present")
                .evictable_by
                .is_empty(),
            "behavior unchanged: absent still means nobody evicts"
        );
        assert!(
            omitted.validate().is_empty(),
            "a warning is not a refusal: {:?}",
            omitted.validate()
        );
        let warnings = omitted.evictable_by_warnings();
        assert_eq!(warnings.len(), 1, "one named warning for RP: {warnings:?}");
        assert!(
            warnings[0].contains("RP"),
            "the warning must name the category: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("omitted") && warnings[0].contains("[]"),
            "omitted and explicit empty are indistinguishable, so the text must cover both: {}",
            warnings[0]
        );
        assert!(
            warnings[0].contains("refused"),
            "the consequence must be named: {}",
            warnings[0]
        );

        // The explicit `[]` spelling reaches the same effective posture and the
        // same decision.
        let explicit =
            parse_mutated(|root| root["categories"][2]["evictable_by"] = serde_json::json!([]))
                .expect("`[]` parses");
        let warnings = explicit.evictable_by_warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("AUX"), "{warnings:?}");
    }

    /// Operator ruling 2026-11-05: the display name is a config option. A set
    /// label is trimmed at parse (the WSL-coordinate discipline), an absent or
    /// blank label falls back to the id without a refusal, and an unset label
    /// serializes nothing — existing config files keep their exact shape (the
    /// GOTCHAS serialization discipline).
    /// Falsifier: return an empty fallback from `effective_label` and the id
    /// legs fail; invert the `skip_serializing_if` and the unset leg fails.
    #[test]
    fn a_category_label_trims_at_parse_and_falls_back_to_the_id() {
        let mut set =
            parse_mutated(|root| root["categories"][1]["label"] = serde_json::json!("  Theater  "))
                .expect("a label parses");
        assert!(set.validate().is_empty(), "{:?}", set.validate());
        let rp = set.category_mut(CategoryId::Rp).expect("RP present");
        assert_eq!(
            rp.label.as_deref(),
            Some("Theater"),
            "surrounding space is not part of a display name"
        );
        assert_eq!(rp.effective_label(), "Theater");

        let mut blank =
            parse_mutated(|root| root["categories"][2]["label"] = serde_json::json!("   "))
                .expect("a blank label parses");
        assert!(
            blank.validate().is_empty(),
            "blank is the documented fallback, not a refusal: {:?}",
            blank.validate()
        );
        assert_eq!(
            blank
                .category_mut(CategoryId::Aux)
                .expect("AUX present")
                .effective_label(),
            "AUX"
        );
        let stamped: serde_json::Value = serde_json::to_value(&blank).expect("serializes");
        assert_eq!(
            stamped["categories"][2]["label"],
            serde_json::json!(""),
            "the trimmed blank round-trips as the trimmed blank"
        );

        let absent = AppConfig::parse("{}").expect("defaults parse");
        assert_eq!(absent.categories[0].label, None);
        assert_eq!(absent.categories[0].effective_label(), "WORK");
        assert!(
            !serde_json::to_string(&absent)
                .expect("serializes")
                .contains("\"label\""),
            "an unset label must not be stamped into the operator's file"
        );
    }

    /// The cap is 24 characters, inclusive (ruling 2026-11-05), and the refusal
    /// names the category id and the rule. The digits are spelled out here, not
    /// read from the constant, so the pin is the ruling and not a mirror of the
    /// code.
    /// Falsifier: raise the accepted length to 25 and the over-cap leg boots
    /// clean, turning this pin red.
    #[test]
    fn an_over_long_label_is_refused_naming_the_category_and_the_cap() {
        let exactly = "x".repeat(24);
        let legal =
            parse_mutated(|root| root["categories"][1]["label"] = serde_json::json!(exactly))
                .expect("parses");
        assert!(
            legal.validate().is_empty(),
            "24 is the inclusive cap: {:?}",
            legal.validate()
        );
        let over = parse_mutated(|root| {
            root["categories"][1]["label"] = serde_json::json!("x".repeat(25))
        })
        .expect("parses");
        let errors = over.validate();
        assert!(
            errors.iter().any(|e| {
                let text = e.to_string();
                text.contains("categories.RP.label") && text.contains("24")
            }),
            "the refusal must name the category id and the 24-character cap: {errors:?}"
        );
    }

    /// A display label reaches the browser and the status document; a control
    /// character there is invisible damage (escapes, split lines), so a
    /// non-blank label is refused at boot naming the category and the rule.
    /// Falsifier: drop the printability check and every leg boots clean.
    #[test]
    fn a_label_with_control_characters_is_refused_naming_the_category() {
        for (name, bad) in [
            ("bell", "Chat\u{7}room"),
            ("newline", "Work\nBench"),
            ("tab", "Work\tBench"),
        ] {
            let config =
                parse_mutated(|root| root["categories"][0]["label"] = serde_json::json!(bad))
                    .expect("parses as json");
            let errors = config.validate();
            assert!(
                errors.iter().any(|e| {
                    let text = e.to_string();
                    text.contains("categories.WORK.label") && text.contains("printable")
                }),
                "a control character ({name}) must be refused by name: {errors:?}"
            );
        }
    }

    /// The write-back round trip for the label (§6 restart guard, the
    /// inverted-`skip_serializing_if` GOTCHAS class): a page `Default` edit
    /// rewrites the whole file, so the persisted bytes must carry the label back
    /// through parse and validate, while the unset default must not change the
    /// serialized shape at all.
    #[test]
    fn the_label_survives_the_config_write_back() {
        let mut categories = CategoryConfig::four_defaults();
        categories[0].label = Some("Primary".to_string());
        let pinned = AppConfig {
            categories,
            ..AppConfig::default()
        };
        let text = write_back_text(&pinned);
        assert!(
            text.contains("\"label\": \"Primary\""),
            "a set label must be written back, not skipped: {text}"
        );
        let reloaded = AppConfig::parse(&text).expect("the persisted label parses back");
        assert!(
            reloaded.validate().is_empty(),
            "a persisted label must validate: {:?}",
            reloaded.validate()
        );
        assert_eq!(reloaded, pinned, "the label must round-trip unchanged");

        let unset = write_back_text(&AppConfig::default());
        assert!(
            !unset.contains("label"),
            "an unset label keeps the file shape byte-stable for existing configs"
        );
    }

    #[test]
    fn vram_reserve_must_be_finite_and_non_negative() {
        // FINDINGS-11 [2]: the reserve is the headroom kept unreachable by admission. The
        // budget subtracts it from the total, so a negative value *grows* the budget the more
        // reserve the operator asks for. serde_json refuses the `1e999` spelling as "number out
        // of range" (settled here, as FINDINGS-11 [3] asked), so +inf reaches the struct only
        // the way NaN does: hand-built, from a test or a future API-patched config.
        let inf = AppConfig {
            vram_reserve_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(
            fails_with(&inf, "vram_reserve_gb"),
            "+inf must be refused, not accepted as headroom"
        );
        assert!(
            AppConfig::parse("{\"vram_reserve_gb\": 1e999}").is_err(),
            "the JSON route refuses the overflow spelling too"
        );
        let negative = AppConfig::parse("{\"vram_reserve_gb\": -1000.0}").expect("parses");
        assert!(fails_with(&negative, "vram_reserve_gb"));
        // serde_json has no NaN literal, so NaN only arrives from a hand-built struct.
        let nan = AppConfig {
            vram_reserve_gb: f64::NAN,
            ..AppConfig::default()
        };
        assert!(fails_with(&nan, "vram_reserve_gb"));
        let legal = AppConfig::parse("{\"vram_reserve_gb\": 0.0}").expect("parses");
        assert!(
            legal.validate().is_empty(),
            "0 means no reserve and is legal: {:?}",
            legal.validate()
        );
    }

    /// The RAM ladder's reserve, pinned like its VRAM twin (operator ruling
    /// 2026-11-05): the default is the ruling's 8 GiB, a negative reserve
    /// would grow the RAM budget the more reserve is asked for, and 0 means
    /// "no reserve" and stays legal.
    #[test]
    fn ram_reserve_defaults_to_eight_and_stays_finite_and_non_negative() {
        assert_eq!(default_ram_reserve(), 8.0);
        assert_eq!(AppConfig::default().ram_reserve_gb, 8.0);
        let negative = AppConfig::parse("{\"ram_reserve_gb\": -1000.0}").expect("parses");
        assert!(fails_with(&negative, "ram_reserve_gb"));
        let nan = AppConfig {
            ram_reserve_gb: f64::NAN,
            ..AppConfig::default()
        };
        assert!(fails_with(&nan, "ram_reserve_gb"));
        let inf = AppConfig {
            ram_reserve_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(fails_with(&inf, "ram_reserve_gb"));
        let legal = AppConfig::parse("{\"ram_reserve_gb\": 0.0}").expect("parses");
        assert!(
            legal.validate().is_empty(),
            "0 means no reserve and is legal: {:?}",
            legal.validate()
        );
    }

    #[test]
    fn witness_tolerance_must_be_finite_and_non_negative() {
        // FINDINGS-11 [2]: the tolerance is the band of witness disagreement that is not a
        // re-sample trigger. The settle gate accepts `baseline + tolerance`, so an inf
        // tolerance makes that test always true and deletes the last check before a spawn.
        // +inf arrives hand-built, as in `vram_reserve_must_be_finite_and_non_negative`.
        let inf = AppConfig {
            witness_tolerance_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(
            fails_with(&inf, "witness_tolerance_gb"),
            "+inf must be refused, not accepted as a band"
        );
        assert!(
            AppConfig::parse("{\"witness_tolerance_gb\": 1e999}").is_err(),
            "the JSON route refuses the overflow spelling too"
        );
        let negative = AppConfig::parse("{\"witness_tolerance_gb\": -1.0}").expect("parses");
        assert!(fails_with(&negative, "witness_tolerance_gb"));
        let nan = AppConfig {
            witness_tolerance_gb: f64::NAN,
            ..AppConfig::default()
        };
        assert!(fails_with(&nan, "witness_tolerance_gb"));
        let legal = AppConfig::parse("{\"witness_tolerance_gb\": 0.0}").expect("parses");
        assert!(
            legal.validate().is_empty(),
            "0 demands exact witness agreement and is legal"
        );
    }

    #[test]
    fn vram_total_ceiling_must_be_finite_and_positive() {
        // FINDINGS-11 [3]: the old `<= 0.0` form was false for exactly the two values that
        // make the ledger ceiling meaningless. +inf arrives hand-built, as in
        // `vram_reserve_must_be_finite_and_non_negative`.
        let inf = AppConfig {
            vram_total_gb: f64::INFINITY,
            ..AppConfig::default()
        };
        assert!(
            fails_with(&inf, "vram_total_gb"),
            "+inf admits the world onto one card"
        );
        assert!(
            AppConfig::parse("{\"vram_total_gb\": 1e999}").is_err(),
            "the JSON route refuses the overflow spelling too"
        );
        let nan = AppConfig {
            vram_total_gb: f64::NAN,
            ..AppConfig::default()
        };
        assert!(fails_with(&nan, "vram_total_gb"), "NaN refuses forever");
        let negative = AppConfig::parse("{\"vram_total_gb\": -8.0}").expect("parses");
        assert!(fails_with(&negative, "vram_total_gb"));
        let zero = AppConfig::parse("{\"vram_total_gb\": 0.0}").expect("parses");
        assert!(fails_with(&zero, "vram_total_gb"));
        let legal = AppConfig::parse("{\"vram_total_gb\": 72.0}").expect("parses");
        assert!(
            legal.validate().is_empty(),
            "72 GiB is the documented ceiling"
        );
    }

    #[test]
    fn release_timeout_refuses_zero() {
        // FINDINGS-11 [5]: the reaper kills the victim first, so a zero deadline loses the
        // model it meant to free and never spawns the new one.
        let zero = AppConfig::parse("{\"release_timeout_s\": 0}").expect("parses");
        assert!(
            fails_with(&zero, "release_timeout_s"),
            "a zero deadline fails its own release confirmation"
        );
        let legal = AppConfig::parse("{\"release_timeout_s\": 1}").expect("parses");
        assert!(legal.validate().is_empty(), "1 s is legal, however tight");
    }

    #[test]
    fn settle_timeout_refuses_zero() {
        // FINDINGS-11 [5]: same shape as release_timeout_s, one step later — by the time the
        // settle gate runs the eviction has already happened.
        let zero = AppConfig::parse("{\"settle_timeout_s\": 0}").expect("parses");
        assert!(
            fails_with(&zero, "settle_timeout_s"),
            "a zero deadline fails the settle gate after the eviction ran"
        );
        let legal = AppConfig::parse("{\"settle_timeout_s\": 1}").expect("parses");
        assert!(legal.validate().is_empty(), "1 s is legal, however tight");
    }

    /// R2-11-05: the `load_timeout_s` ceiling rationale (one mistyped digit parks
    /// the job forever) applies verbatim to both dispose deadlines, and the
    /// scheduler uses the raw value.
    /// Falsifier: revert either bound to `== 0` and the 86_401 leg goes green.
    #[test]
    fn release_and_settle_timeouts_are_bounded_at_one_day() {
        let day = AppConfig::parse("{\"release_timeout_s\": 86400, \"settle_timeout_s\": 86400}")
            .expect("parses");
        assert!(day.validate().is_empty(), "86_400 is the inclusive ceiling");
        let over = AppConfig::parse("{\"release_timeout_s\": 86401}")
            .expect("parses")
            .validate();
        assert!(
            over.iter().any(|e| {
                e.to_string().contains("release_timeout_s") && e.to_string().contains("1..=86400")
            }),
            "one mistyped digit must not park a job in Dispose for years: {over:?}"
        );
        let over = AppConfig::parse("{\"settle_timeout_s\": 86401}")
            .expect("parses")
            .validate();
        assert!(
            over.iter().any(|e| {
                e.to_string().contains("settle_timeout_s") && e.to_string().contains("1..=86400")
            }),
            "the settle deadline carries the same ceiling: {over:?}"
        );
    }

    /// The ninfer container log rotation keys: absent means the literal the harness
    /// always rendered (25 MiB, 5 files), the inclusive bounds parse, and 0 or a
    /// mistyped digit is refused naming the field. Falsifier: widen a bound past the
    /// test's `1024`/`50` legs (a bound edit to `1..=1025` or `..=51` goes green on the
    /// bad leg) or change a default away from 25/5.
    #[test]
    fn ninfer_log_rotation_keys_default_and_are_bounded() {
        let d = AppConfig::parse("{}").expect("defaults parse");
        assert!(d.validate().is_empty(), "{:?}", d.validate());
        assert_eq!(
            d.ninfer_log_max_size_mib, 25,
            "absent means today's MiB literal"
        );
        assert_eq!(
            d.ninfer_log_max_files, 5,
            "absent means today's file-count literal"
        );

        let hi = parse_mutated(|r| {
            r["ninfer_log_max_size_mib"] = serde_json::json!(1024);
            r["ninfer_log_max_files"] = serde_json::json!(50);
        })
        .expect("inclusive top bounds parse");
        assert!(
            hi.validate().is_empty(),
            "the ceiling is inclusive: {:?}",
            hi.validate()
        );

        let bad = parse_mutated(|r| {
            r["ninfer_log_max_size_mib"] = serde_json::json!(1025);
            r["ninfer_log_max_files"] = serde_json::json!(0);
        })
        .expect("out-of-range parses, validation names it");
        let errs: Vec<String> = bad.validate().iter().map(ToString::to_string).collect();
        assert!(
            errs.iter()
                .any(|e| e.contains("ninfer_log_max_size_mib") && e.contains("1..=1024")),
            "{errs:?}"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("ninfer_log_max_files") && e.contains("1..=50")),
            "{errs:?}"
        );
    }

    /// The measured-drift warning ratio: absent means the fixed `0.25` the harness
    /// always used, `0.0` (a deliberate silence) and the whole fraction `0.0..=1.0`
    /// validate, and a value outside the fraction is refused naming the field.
    /// Falsifier: change the default away from `0.25`, or widen `0.0..=1.0` so the
    /// `1.5` leg goes green.
    #[test]
    fn measured_drift_warn_ratio_defaults_and_is_bounded() {
        let d = AppConfig::parse("{}").expect("defaults parse");
        assert_eq!(
            d.measured_drift_warn_ratio, 0.25,
            "the default is today's fixed literal, so an unchanged file warns as before"
        );
        assert!(d.validate().is_empty(), "{:?}", d.validate());

        let zero = parse_mutated(|r| r["measured_drift_warn_ratio"] = serde_json::json!(0.0))
            .expect("parses");
        assert!(
            zero.validate().is_empty(),
            "0.0 is a valid silence: {:?}",
            zero.validate()
        );

        let over = parse_mutated(|r| r["measured_drift_warn_ratio"] = serde_json::json!(1.5))
            .expect("parses");
        let errs: Vec<String> = over.validate().iter().map(ToString::to_string).collect();
        assert!(
            errs.iter()
                .any(|e| e.contains("measured_drift_warn_ratio") && e.contains("0.0..=1.0")),
            "a non-fraction ratio must be refused: {errs:?}"
        );
    }

    #[test]
    fn load_timeout_is_bounded_below_and_at_one_day() {
        // FINDINGS-11 [6]: the per-model `load_timeout` key already carries this ceiling
        // (model/mod.rs); this field is its default, so it needs the same bound.
        let day = AppConfig::parse("{\"load_timeout_s\": 86400}").expect("parses");
        assert!(day.validate().is_empty(), "86_400 is the inclusive ceiling");
        let over = AppConfig::parse("{\"load_timeout_s\": 86401}").expect("parses");
        assert!(
            fails_with(&over, "load_timeout_s"),
            "one mistyped digit must not park a load job in readiness forever"
        );
        let zero = AppConfig::parse("{\"load_timeout_s\": 0}").expect("parses");
        assert!(fails_with(&zero, "load_timeout_s"));
    }

    #[test]
    fn public_port_zero_is_refused() {
        // FINDINGS-11 [7]: port 0 binds an ephemeral listener, so /har/status and the tray
        // report a green category no client can address at the documented port — and 0 is the
        // harness's own "no port" sentinel, which hides the slot from the no-`model` proxy.
        let zero =
            parse_mutated(|root| root["categories"][3]["public_port"] = serde_json::json!(0))
                .expect("0 parses as a u16");
        let errors = zero.validate();
        assert!(
            errors.iter().any(|e| {
                e.to_string().contains("categories.EMB.public_port")
                    && e.to_string().contains("must not be 0")
            }),
            "an ephemeral public listener must fail the boot: {errors:?}"
        );
    }

    #[test]
    fn engine_port_zero_is_refused() {
        // FINDINGS-11 [7]: `--port 0` hands the engine an OS-chosen ephemeral port while the
        // harness probes :0 until the readiness deadline and the kill path keys the port the
        // engine does not hold. The list length must equal max_models, or the plan
        // auto-assigns and the pinned 0 never reaches validation.
        let zero =
            parse_mutated(|root| root["categories"][0]["engine_ports"] = serde_json::json!([0]))
                .expect("0 parses as a u16");
        let errors = zero.validate();
        assert!(
            errors.iter().any(|e| {
                e.to_string().contains("categories.WORK.engine_ports")
                    && e.to_string().contains("must not be 0")
            }),
            "an ephemeral engine port must fail the boot: {errors:?}"
        );
    }

    /// R2-11-02: a non-empty explicit plan whose length differs from `max_models`
    /// used to be discarded whole at boot — the operator's hand-picked ports gone,
    /// every slot back on the auto range, failure only at spawn. Now the boot
    /// names the field and both counts; an empty list still keeps the plan.
    /// Falsifier: drop the length check and both mismatch legs validate clean.
    #[test]
    fn a_non_empty_engine_ports_list_must_name_one_port_per_slot() {
        let short = parse_mutated(|root| {
            root["categories"][2]["max_models"] = serde_json::json!(3);
            root["categories"][2]["engine_ports"] = serde_json::json!([20_500, 20_501]);
        })
        .expect("parses");
        let errors = short.validate();
        assert!(
            errors.iter().any(|e| {
                let text = e.to_string();
                text.contains("categories.AUX.engine_ports")
                    && text.contains("exactly 3")
                    && text.contains("got 2")
            }),
            "a short explicit plan must name the field and the required/got counts: {errors:?}"
        );
        let long = parse_mutated(|root| {
            root["categories"][0]["engine_ports"] = serde_json::json!([20_500, 20_501]);
        })
        .expect("parses");
        let errors = long.validate();
        assert!(
            errors.iter().any(|e| {
                let text = e.to_string();
                text.contains("categories.WORK.engine_ports")
                    && text.contains("exactly 1")
                    && text.contains("got 2")
            }),
            "a long explicit plan is the same typo: {errors:?}"
        );
        // Empty means the automatic plan, and an exact-length list stays legal.
        let empty =
            parse_mutated(|root| root["categories"][2]["engine_ports"] = serde_json::json!([]))
                .expect("`[]` parses");
        assert!(empty.validate().is_empty(), "{:?}", empty.validate());
        let exact = parse_mutated(|root| {
            root["categories"][2]["engine_ports"] = serde_json::json!([20_500])
        })
        .expect("exact length parses");
        assert!(exact.validate().is_empty(), "{:?}", exact.validate());
    }

    /// R2-11-03: the plan is a per-category contract (WORK 18109, RP 18110,
    /// AUX 18111, EMB 18112), so it must follow the canonical category order,
    /// not the order of the hand-written `categories` array.
    /// Falsifier: iterate `self.categories` and the reversed file hands WORK
    /// EMB's documented number.
    #[test]
    fn engine_ports_follow_canonical_order_not_file_order() {
        let mut cfg = AppConfig::default();
        cfg.categories.reverse();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let plan = cfg.effective_engine_ports();
        assert_eq!(plan[&CategoryId::Work], vec![18_109]);
        assert_eq!(plan[&CategoryId::Rp], vec![18_110]);
        assert_eq!(plan[&CategoryId::Aux], vec![18_111]);
        assert_eq!(plan[&CategoryId::Emb], vec![18_112]);
    }

    /// The serializer the config write-back uses (`api::persist_default_model`):
    /// `to_value` of the whole struct, then pretty JSON to disk.
    fn write_back_text(config: &AppConfig) -> String {
        let root = serde_json::to_value(config).expect("invariant: config serializes");
        serde_json::to_string_pretty(&root).expect("invariant: JSON serializes")
    }

    /// TASK-NAMING-CLEANUP: `default_config_path` is the single discovery point
    /// and it names only [`CONFIG_FILE`] — no sibling-name lookup, no fallback
    /// probe. Falsifier: reintroduce an older-name probe (a `with_file_name` or
    /// `exists` walk) here and this pin goes red.
    #[test]
    fn default_config_path_names_only_the_primary_file() {
        let exe = std::env::current_exe().expect("test process has an exe path");
        let expected = exe.parent().expect("exe directory").join(CONFIG_FILE);
        assert_eq!(default_config_path().as_deref(), Some(expected.as_path()));
        assert_eq!(CONFIG_FILE, "simpleharn.json", "the one config name");
        let source = include_str!("mod.rs");
        let body = source
            .split("pub fn default_config_path")
            .nth(1)
            .expect("default_config_path must stay")
            .split("\n}")
            .next()
            .expect("default_config_path must close");
        assert!(
            body.contains("join(CONFIG_FILE)")
                && !body.contains("with_file_name")
                && !body.contains("exists"),
            "discovery must join the one name, never probe for another: {body}"
        );
    }

    /// RAII: clears one primary env name for the test and restores the previous
    /// value on drop, so a machine-wide `SIMPLEHARN_*` export cannot decide the
    /// outcome of the inertness pin below.
    struct PrimaryEnvCleared(&'static str, Option<String>);

    impl PrimaryEnvCleared {
        fn clear(name: &'static str) -> Self {
            let previous = std::env::var(name).ok();
            std::env::remove_var(name);
            Self(name, previous)
        }
    }

    impl Drop for PrimaryEnvCleared {
        fn drop(&mut self) {
            match &self.1 {
                Some(value) => std::env::set_var(self.0, value),
                None => std::env::remove_var(self.0),
            }
        }
    }

    /// RAII: injects one env name and removes it again on drop. Safe to inject
    /// old spellings here: after TASK-NAMING-CLEANUP no reader in this crate
    /// names them, so no other test in this binary can observe the injection.
    struct LegacyEnvInjected(&'static str);

    impl LegacyEnvInjected {
        fn set(name: &'static str, value: &str) -> Self {
            std::env::set_var(name, value);
            Self(name)
        }
    }

    impl Drop for LegacyEnvInjected {
        fn drop(&mut self) {
            std::env::remove_var(self.0);
        }
    }

    /// TASK-NAMING-CLEANUP pin (b, reader half): the old env names are inert.
    /// With every primary name cleared and every old name set, all three
    /// readers answer `None` — no value, no warning machinery left to fire.
    /// Falsifier: restore a fallback arm in any reader and its `None` assert
    /// goes red with the legacy value in hand.
    #[test]
    fn the_old_env_names_are_inert() {
        // NAMING-CLEANUP-EXEMPT: the old spellings are set here only to prove no reader honors them.
        let _token = LegacyEnvInjected::set("SIMPLEHAR_TOKEN", "legacy-token-never-honored"); // NAMING-CLEANUP-EXEMPT
        let _config = LegacyEnvInjected::set("SIMPLEHAR_CONFIG", r"C:\legacy\never-read.json"); // NAMING-CLEANUP-EXEMPT
        let _log = LegacyEnvInjected::set("SIMPLEHAR_LOG", "trace"); // NAMING-CLEANUP-EXEMPT
        let _clear_token = PrimaryEnvCleared::clear("SIMPLEHARN_TOKEN");
        let _clear_config = PrimaryEnvCleared::clear("SIMPLEHARN_CONFIG");
        let _clear_log = PrimaryEnvCleared::clear("SIMPLEHARN_LOG");
        assert_eq!(
            token_from_env(),
            None,
            "the old token name must not be read"
        );
        assert_eq!(
            config_path_from_env(),
            None,
            "the old config name must not be read"
        );
        assert_eq!(
            log_level_from_env(),
            None,
            "the old log name must not be read"
        );
    }

    /// TASK-NAMING-CLEANUP pin (d, rewritten): each reader names exactly one
    /// env variable, and it is the `SIMPLEHARN_*` spelling — the old
    /// primary/deprecated pairing machinery is gone. Falsifier: reintroduce a
    /// fallback `std::env::var` in a reader and the one-call count goes red;
    /// swap a name and the name assert goes red.
    #[test]
    fn each_env_reader_names_only_its_primary_variable() {
        let source = include_str!("mod.rs");
        let body = |name: &str| {
            source
                .split(format!("pub fn {name}").as_str())
                .nth(1)
                .unwrap_or_else(|| panic!("{name} must stay"))
                .split("\n}")
                .next()
                .unwrap_or_else(|| panic!("{name} must close"))
                .replace(char::is_whitespace, "")
        };
        for (reader, var) in [
            ("token_from_env", "SIMPLEHARN_TOKEN"),
            ("config_path_from_env", "SIMPLEHARN_CONFIG"),
            ("log_level_from_env", "SIMPLEHARN_LOG"),
        ] {
            let body = body(reader);
            let calls: Vec<&str> = body
                .match_indices("env::var")
                .map(|(index, _)| &body[index..])
                .collect();
            assert_eq!(
                calls.len(),
                1,
                "{reader} must read exactly one env variable: {body}"
            );
            assert!(
                calls[0].starts_with(&format!("env::var(\"{var}\")")),
                "{reader} must name {var}: {}",
                calls[0]
            );
        }
    }
}
