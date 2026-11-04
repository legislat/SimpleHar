//! Typed per-model configuration from `ModelConf/*.yaml` (DESIGN Steps 9-10).
//!
//! The v3.3 files are flat `key: value` documents. This module owns the typed entry and
//! the documented alias rules; the flat parser lives in [`scan`] and the byte-preserving
//! round-trip write-back ships in [`writeback`].

pub mod scan;
pub mod writeback;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::CategoryId;
use thiserror::Error;

/// Problems building a [`ModelEntry`] from raw yaml keys; collected per file at scan
/// time and exposed at `/har/status` — a bad file is skipped and named, never fatal.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ModelError {
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
    /// A harness-written stamp token is neither true nor false.
    #[error("field `{field}` is not a boolean: `{value}`")]
    BadBool {
        /// Field name.
        field: String,
        /// Raw value.
        value: String,
    },
    /// The `measured_attribution:` provenance token is not one this harness wrote.
    #[error("field `{field}` is not one of engine_report|engine_status|per_process|total_diff: `{value}`")]
    BadAttribution {
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
    /// An operator-extra word (`extra_args`, `ninfer_serve_args`) outside the
    /// engine row's closed word surface — refused at validate time (slice 10c,
    /// full `extra_args` enforcement) so the file is reported at rescan instead
    /// of failing at spawn, after admission may have evicted a resident for the
    /// doomed launch. `reason` is the engine's own refusal voice naming the
    /// word and the mechanism that owns the decision.
    #[error("{reason}")]
    ExtraWordsRefused {
        /// The engine's refusal text (from its `EnginePolicy` extra-words check).
        reason: String,
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

/// How a measured footprint figure was obtained, and therefore whether it may book
/// (R1, DECISIONS LOCKED 2026-10-30). Written by `on_measured` as the
/// `measured_attribution:` stamp and read back at scan; never upgraded post-hoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MeasuredAttribution {
    /// The engine's own reserved-footprint self-report (ninfer startup
    /// `MemorySummary`): the authoritative over-reserved figure, books.
    EngineReport,
    /// The process engine's own status endpoint stated the footprint (basalt
    /// `/v1/status`, strata `/status`): the process-engine analog of
    /// `EngineReport` and it books, above the declared value as max truth
    /// (`TASK-ENGINE-MEASUREMENT`, operator ruling 2026-11-04). A
    /// process-engine source: bound to no ninfer profile, so it is valid
    /// without a profile stamp and voids on a ninfer row.
    EngineStatus,
    /// Per-process attribution from the witness (`own_gib` returned bytes): books.
    PerProcess,
    /// A free-memory total-diff (per-process unavailable): displays, **never books**.
    TotalDiff,
}

impl MeasuredAttribution {
    /// Whether a figure stamped with this provenance may be booked for admission.
    /// A `TotalDiff` never books; an absent stamp (a legacy `measured_gb` written
    /// before provenance existed) is grandfathered bookable and is corrected the
    /// next time the model serves (the sampler always stamps from R9 on).
    #[must_use]
    pub fn bookable(self) -> bool {
        !matches!(self, MeasuredAttribution::TotalDiff)
    }

    /// Whether this provenance comes from the engine speaking about itself — its own
    /// reserved budget, the footprint its own status endpoint printed, or concrete
    /// bytes the driver named as its pids. `exact` rows accept only these (ruling 15
    /// (ii), extended to `engine_status` by the 2026-11-04 ruling): a grandfathered
    /// stamp-less pair is not a source for a class that forbids guessing, even though
    /// `fixed` may still charge it.
    #[must_use]
    pub fn self_reported(self) -> bool {
        matches!(
            self,
            MeasuredAttribution::EngineReport
                | MeasuredAttribution::EngineStatus
                | MeasuredAttribution::PerProcess
        )
    }

    /// The yaml/payload token for this provenance — the exact form
    /// [`parse_measured_attribution`] accepts back, so writer and reader share one
    /// spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            MeasuredAttribution::EngineReport => "engine_report",
            MeasuredAttribution::EngineStatus => "engine_status",
            MeasuredAttribution::PerProcess => "per_process",
            MeasuredAttribution::TotalDiff => "total_diff",
        }
    }
}

/// One model from `ModelConf/`, typed (Step 10: the harness name is canonical, and
/// since FINDINGS-13 [3]/[7] it is the yaml file stem, which `scan` supplies as the
/// `name` argument of [`ModelEntry::from_flat`]).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelEntry {
    /// Canonical harness identity: the yaml file stem (a `name:` key that
    /// contradicts it is refused at scan); the `model` field clients send.
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
    /// High end of the engine footprint measured by the harness while this model
    /// served (`measured_gb:`, written back after Install; see
    /// [`ModelEntry::measured_min_gb`] for the low end). The page shows the range as
    /// truth for every cost class; admission consults it when the provenance stamp
    /// says it may book (`fixed` without a declared `vram_gb`, or a guarded
    /// `engine_report` figure — above the declared floor too, per the 2026-11-01
    /// amendment of ruling 15 (ii); `exact` only on a self-reported figure); a
    /// `total_diff` displays but never books (greedy keeps nominal semantics;
    /// exact refuses without a source).
    pub measured_gb: Option<f64>,
    /// Low end of the measured range (`measured_gb_min:`): the smallest witness
    /// total-diff seen while the model served. The spread is what the model actually
    /// varies over — a growing KV cache — not measurement noise, so booking a single
    /// point would be a guess with false precision.
    pub measured_min_gb: Option<f64>,
    /// The `cpu_offload` setting the range was measured under (`measured_offload:`),
    /// written with it on rows whose engine carries the `cpu_offload` knob
    /// ([`crate::engine::llama::offload_live`]) only. An
    /// offloaded launch has no comparable footprint, so a flip between then and now
    /// voids the pair at scan time (measured values verify, they never mislead); on
    /// engines that never read the key the flip cannot describe the launch, so it
    /// voids nothing.
    pub measured_offload: Option<bool>,
    /// How the measured range was obtained (`measured_attribution:`): gates whether it
    /// books. `TotalDiff` never books; absent is grandfathered (see
    /// [`MeasuredAttribution::bookable`]).
    pub measured_attribution: Option<MeasuredAttribution>,
    /// The `ninfer_profile` an `engine_report` figure was taken under
    /// (`measured_profile:`): the reserved KV is bound to the profile's `--kv-capacity`,
    /// so a profile change voids the pair at scan (the ninfer analog of
    /// `measured_offload`). Absent for non-ninfer engines and total-diff figures.
    pub measured_profile: Option<String>,
    /// Raw `model:` value — GGUF path or the engine-backed `scheme://` sentinel.
    /// Informational for managed models; used for stem/path aliases (Step 10).
    pub model_ref: Option<String>,
    /// The id the yaml claims the engine reports (`serve_model:`); verified by
    /// discovery at readiness — a mismatch is a recorded warning, never trusted
    /// (F-4 fix, Step 10).
    pub serve_model: Option<String>,
    /// The R11 Fit projection in GiB — the engine's own pre-spawn word from
    /// `llama-fit-params --fit-print on` (rule 2026-11-01). **In-process
    /// only**: the scanner never fills it, no writer persists it, and it is
    /// absent from `raw`, so a Fit computation can never reach the yaml. The
    /// scheduler patches the current value in after each probe; a stale one
    /// is dropped (set back to `None`), never trusted (the args-hash cache is
    /// the void identity).
    pub fit_gb: Option<f64>,
    /// File the entry came from; `None` for synthetic entries (tests, mock).
    pub file: Option<PathBuf>,
    /// Every raw `key: value` from the source file, verbatim — the passthrough the
    /// llama adapter builds argv from (Step 9 raw map; write-back keeps it honest).
    pub raw: BTreeMap<String, String>,
}

/// v3.3-compatible boolean-ish reading of a flat yaml value: `1|true|yes|on`,
/// ASCII case-insensitive, whitespace-trimmed; anything else is false. Exact
/// parity with the baseline `truthy` table is pinned by the unit test
/// `truthy_pins_the_v3_3_boolean_table` in this module.
pub(crate) fn truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "true" | "1" | "yes" | "on"
    )
}

/// The moved-or-not answer for one file: the pair a reload compares against what a
/// load queued on. Reload-drift Axis 1 (operator ruling 2026-10-29) captures it at
/// row registration and re-stats when a rescan lands; Axis 2 (the `stale_config`
/// companion round) reuses the same primitive at spawn. Typed on purpose: one
/// helper, no stringly timestamps on the job rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    /// Last-modified time as the filesystem reports it.
    pub mtime: std::time::SystemTime,
    /// Length in bytes.
    pub size: u64,
}

/// The fingerprint of one file; `None` when it cannot be stat'ed (absent, locked,
/// volume down). Absence of a fingerprint is the absence of **evidence**, never
/// evidence of change — callers must keep their queue on `None`, never retire on it.
#[must_use]
pub fn fingerprint(path: &Path) -> Option<FileFingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    Some(FileFingerprint {
        mtime: meta.modified().ok()?,
        size: meta.len(),
    })
}

/// Renders a wall-clock instant as the fixed-width UTC second form the page prints
/// verbatim (`2026-10-29T03:55:12Z`, Axis-2 ruling 2026-10-29: the server owns the
/// format, no date math in the browser — the K-round page-must-not-derive rule, and
/// fixed-width UTC also sorts lexicographically by time, which `/har/models`' merge
/// relies on). Civil date from days via Hinnant's `civil_from_days`; sub-second is
/// truncated, which is what a launch stamp means to an operator. An instant before
/// the epoch (a clock set backwards — pathological, never produced by `now()`)
/// saturates to `1970-01-01T00:00:00Z` rather than fabricate a negative day.
#[must_use]
pub fn iso8601_utc(at: std::time::SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = (secs / 86_400) as i64;
    let tod = secs % 86_400;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_097 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    format!(
        "{year:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60
    )
}

/// The one GiB parser for every size-valued yaml key. Rust's `f64` parse accepts
/// `nan`/`inf`/`1e400`, and every NaN comparison is false — an unchecked value
/// would sail through `< 0.0` guards and silently disable the gates it feeds
/// (review, 2026-10-26). Finite and non-negative, or it is a scan error. Individual
/// keys tighten this at their call site: `vram_gb` also refuses 0 (a free booking),
/// while `launch_only_with_at_least_vram_free` deliberately keeps 0 as its opt-out.
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

/// The measured footprint pair plus the launch-describing stamps it is only
/// meaningful alongside: the `cpu_offload` setting and (for an `engine_report` figure)
/// the `ninfer_profile` whose `--kv-capacity` bound the reserved KV.
struct Measured {
    max: Option<f64>,
    min: Option<f64>,
    offload: Option<bool>,
    attribution: Option<MeasuredAttribution>,
    profile: Option<String>,
}

impl Measured {
    /// Drop the range but keep the stamps, so the payload still shows *how* it was
    /// taken even once the number is voided (mirrors the offload-flip behavior).
    fn voided(self) -> Self {
        Self {
            max: None,
            min: None,
            ..self
        }
    }
}

/// Reads the measured footprint pair together with the stamps that make it comparable
/// to the current launch. Two void conditions (measured values verify, they never
/// mislead): a `cpu_offload` flip on an [`crate::engine::llama::offload_live`] engine,
/// and a `ninfer_profile` change on an `engine_report` figure (its reserved KV is bound
/// to the profile's `--kv-capacity`). A void drops the range but keeps the stamps in
/// the entry/payload.
///
/// # Errors
///
/// [`ModelError::BadNumber`] for a non-numeric or sub-quantum size, [`ModelError::BadBool`]
/// for an unreadable `measured_offload`, [`ModelError::BadAttribution`] for an unreadable
/// `measured_attribution`, and a `measured_gb_min` that exceeds its `measured_gb` or
/// appears without one.
fn read_measured(engine: &str, keys: &BTreeMap<String, String>) -> Result<Measured, ModelError> {
    let offload = keys
        .get("measured_offload")
        .map(|raw| parse_bool_stamp("measured_offload", raw))
        .transpose()?;
    let attribution = keys
        .get("measured_attribution")
        .map(|raw| parse_measured_attribution(raw))
        .transpose()?;
    let profile = keys
        .get("measured_profile")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let mut measured = Measured {
        max: None,
        min: None,
        offload,
        attribution,
        profile,
    };
    // A flip voids the pair only on engines whose adapters read `cpu_offload`; an
    // absent `engine:` is llama (the scan default, the one offload engine).
    let offload_now = keys
        .get("cpu_offload")
        .is_some_and(|v| crate::engine::llama::jinja_on(v));
    if crate::engine::llama::offload_live(engine)
        && offload.is_some_and(|stamped| stamped != offload_now)
    {
        return Ok(measured.voided());
    }
    // The engine_report reserved figure is bound to the profile's KV capacity: a
    // profile change since it was taken makes the reservation incomparable. The
    // comparison is the profile's RESOLVED IDENTITY (name plus the `--kv-capacity`
    // a custom override line stands for — `profile_stamp`), not the name alone:
    // editing `ninfer_profile_<name>`'s `kv=` moves the reservation without moving
    // the name, and an unresolvable identity (bad or deleted override line) voids —
    // nothing launch-verified behind it can be trusted to still describe the card.
    // The gate is on the ATTRIBUTION, not on the presence of a stamp: an
    // `engine_report` row whose stamp is missing was written behind an unvalidated
    // (legacy-autostart) profile and has no certifying launch at all, so the
    // mismatch with the identity now — whatever it resolves to, `None` included —
    // voids it (review 2026-10-31, H2: gating on `profile.is_some()` let exactly
    // those unstamped figures survive every comparability change).
    if measured.attribution == Some(MeasuredAttribution::EngineReport) || measured.profile.is_some()
    {
        let identity_now = crate::engine::ninfer::profile_stamp(keys);
        if measured.profile.as_deref() != identity_now.as_deref() {
            return Ok(measured.voided());
        }
    }
    // An `engine:` flip in the same file outranks any stamp: an engine_report
    // reservation describes a container's CUDA reservation, an engine_status
    // figure a status endpoint only a process engine runs, and a per_process
    // figure a driver-attributed child — none crosses the container/process
    // boundary, and a stale ninfer profile on a llama row would otherwise
    // "match" the built-in default silently (review 2026-10-31, M2). A ninfer
    // row claiming per_process is the same impossibility seen from the other
    // side (a container has no harness child), and so is one claiming
    // engine_status: the harness reads `ninfer` through `docker logs`, never
    // through a status endpoint (ruling 2026-11-04: `EngineReport` => !ninfer
    // keeps, `EngineStatus` => ninfer voids — it is a process-engine source).
    // The profile-comparability leg above never fires for an `engine_status`
    // figure: the profile identity is ninfer's alone (ORCHESTRATION §2), so a
    // process-engine figure is valid WITHOUT a profile stamp.
    let ninfer = engine == crate::ownership::NINFER_ENGINE;
    let stamp_describes_the_wrong_engine = match measured.attribution {
        Some(MeasuredAttribution::EngineReport) => !ninfer,
        Some(MeasuredAttribution::EngineStatus | MeasuredAttribution::PerProcess) => ninfer,
        Some(MeasuredAttribution::TotalDiff) | None => false,
    };
    if stamp_describes_the_wrong_engine {
        return Ok(measured.voided());
    }
    let size = |field: &str, raw: &str| -> Result<f64, ModelError> {
        let v = parse_gib(field, raw)?;
        // Below the writer's 0.1 quantum: a footprint this small means nothing was
        // attributed — absence, not a measurement, and persisting it would write the
        // exact token the reader rejects.
        if v < 0.05 {
            return Err(ModelError::BadNumber {
                field: field.to_string(),
                value: raw.to_string(),
            });
        }
        Ok(v)
    };
    let max = keys
        .get("measured_gb")
        .map(|raw| size("measured_gb", raw))
        .transpose()?;
    let min = keys
        .get("measured_gb_min")
        .map(|raw| {
            let v = size("measured_gb_min", raw)?;
            match max {
                Some(max) if v <= max => Ok(v),
                // A lone low end cannot book anything, and an inverted pair is a typo
                // that would silently widen every range read from it.
                Some(_) | None => Err(ModelError::BadNumber {
                    field: "measured_gb_min".to_string(),
                    value: raw.to_string(),
                }),
            }
        })
        .transpose()?;
    measured.max = max;
    measured.min = min;
    Ok(measured)
}

/// Strict reading of the `measured_attribution:` provenance token this harness wrote:
/// only the forms it emits, so a typo is a scan error rather than a silent false.
fn parse_measured_attribution(raw: &str) -> Result<MeasuredAttribution, ModelError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "engine_report" => Ok(MeasuredAttribution::EngineReport),
        "engine_status" => Ok(MeasuredAttribution::EngineStatus),
        "per_process" => Ok(MeasuredAttribution::PerProcess),
        "total_diff" => Ok(MeasuredAttribution::TotalDiff),
        _ => Err(ModelError::BadAttribution {
            field: "measured_attribution".to_string(),
            value: raw.to_string(),
        }),
    }
}

/// Strict reading of a boolean token this harness wrote itself: the engine's truth set
/// plus its explicit negatives, so a typo is a scan error rather than a silent false.
fn parse_bool_stamp(field: &str, raw: &str) -> Result<bool, ModelError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" => Ok(true),
        "false" | "0" | "off" | "no" => Ok(false),
        _ => Err(ModelError::BadBool {
            field: field.to_string(),
            value: raw.to_string(),
        }),
    }
}

/// Which figure admission will book for an entry. Decided once, here, and read by
/// both the gate and the page's Cost column — two derivations of this precedence is
/// how the page could show a number the gate ignored (duplication review, 2026-10-26).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostSource {
    /// The declared `vram_gb` — including where a guarded `engine_report` or a
    /// Fit projection exists but does not exceed it (the declared floor then
    /// stands).
    Declared,
    /// The harness-measured footprint of an earlier run: the booking basis for
    /// `fixed`/`exact` rows without a binding declaration, and the override of a
    /// declared value a guarded `engine_report`/`engine_status` figure exceeds
    /// (the amended-15(ii) max truth, extended to the process engines' status
    /// figures by the 2026-11-04 ruling).
    Measured,
    /// The engine's own pre-spawn projection (`llama-fit-params --fit-print`,
    /// R11): books for `fixed`/`exact` rows without a binding declaration and
    /// overshadows a declared `vram_gb` it exceeds — the same amended-15(ii)
    /// max truth as the container's word. `greedy` never books it as a class
    /// (its launch-floor input does read it); never persisted.
    Fit,
    /// GGUF size × 1.15 + 2.
    Estimate,
    /// `exact` with nothing declared and no self-reported figure or Fit
    /// projection: refused, never guessed.
    Undeclared,
}

impl ModelEntry {
    /// Whether the measured pair is present with a provenance that may book: an
    /// absent stamp is a legacy figure, grandfathered (see
    /// [`MeasuredAttribution::bookable`]); an explicit `total_diff` never books.
    /// This is the *witness* question — `exact` demands more, which is why
    /// [`Self::may_book_measured`] is a separate question from this one.
    #[must_use]
    fn measured_bookable(&self) -> bool {
        self.measured_gb.is_some()
            && self
                .measured_attribution
                .is_none_or(MeasuredAttribution::bookable)
    }

    /// Whether admission may charge this entry's measured pair, per its cost class.
    /// `greedy` never books a measurement: a post-spill footprint is not a nominal
    /// size (the class exists to take the whole card). `exact` is the mirror image —
    /// it demands a real *source*, so only a figure the engine produced about itself
    /// qualifies (`engine_report`/`engine_status`/`per_process`, the middle form
    /// admitted by the 2026-11-04 ruling: a status endpoint is the process engine
    /// speaking, not a witness stand-in); a grandfathered stamp-less pair and a
    /// `total_diff` stand-in are both refused, since the GGUF-size fallback would be
    /// a precision claim with no source. `fixed` accepts any bookable provenance.
    #[must_use]
    fn may_book_measured(&self) -> bool {
        if !self.measured_bookable() {
            return false;
        }
        match self.cost {
            CostClass::Greedy => false,
            CostClass::Exact => self
                .measured_attribution
                .is_some_and(MeasuredAttribution::self_reported),
            CostClass::Fixed => true,
        }
    }

    /// Whether the engine's own guarded report outranks the declared `vram_gb`:
    /// the container certified reserving MORE than the operator declared, and
    /// `read_measured` still certifies the report against the current profile
    /// stamp; since the 2026-11-04 ruling the same max truth covers a process
    /// engine's guarded `engine_status` figure (its own status endpoint spoke,
    /// and `read_measured` keeps it off ninfer rows), no profile stamp being
    /// demanded there. Operator ruling 2026-11-01 (amends the declared-always-wins
    /// leg of the 2026-10-30 R9 design): the booking is the max truth — what
    /// admission must keep free is what the engine actually holds. A declared
    /// value at or above the report stays the operator's chosen floor. The name
    /// predates the extension (`DECISIONS` 2026-11-01 records it); it now answers
    /// for both engine-spoken report provenances.
    #[must_use]
    fn engine_report_outranks_declared(&self) -> bool {
        let (Some(declared), Some(reported)) = (self.vram_gb, self.measured_gb) else {
            return false;
        };
        self.measured_attribution.is_some_and(|attribution| {
            matches!(
                attribution,
                MeasuredAttribution::EngineReport | MeasuredAttribution::EngineStatus
            )
        }) && self.may_book_measured()
            && reported > declared
    }

    /// Whether this row's in-process Fit projection (R11) may book at all:
    /// the figure is present, the row's engine policy row is the fit-capable
    /// one (the llama row alone — never an engine-name match), and the cost
    /// class may charge a projected figure. `greedy` is excluded from the
    /// CLASS exactly as it is for a measured pair (a projection is not a
    /// nominal size; the scheduler's greedy launch-floor input reads the
    /// figure directly); `fixed` and `exact` accept it — the projection is
    /// the engine's own calculator, a real source, which is what `exact`
    /// demands.
    #[must_use]
    fn fit_bookable(&self) -> bool {
        self.fit_gb.is_some()
            && self.cost != CostClass::Greedy
            && crate::engine::policy::fit_capable(&self.engine)
    }

    /// The booking precedence: declared → measured (only where the cost class may
    /// book it, per [`Self::may_book_measured`]) → the R11 Fit projection → GGUF
    /// estimate, with `exact`-without-`vram_gb`-and-without-a-real-source refused.
    /// A guarded engine-spoken report (`engine_report`, or a process engine's
    /// `engine_status` since the 2026-11-04 ruling) ABOVE the declared value books
    /// instead of it ([`Self::engine_report_outranks_declared`]), and so does a Fit projection
    /// above it (rule 2026-11-01, the same amended-15(ii) max truth): the booking
    /// is what the engine itself will take, the declared number is a floor, never
    /// a ceiling.
    #[must_use]
    pub fn cost_source(&self) -> CostSource {
        let fit = self.fit_bookable().then_some(self.fit_gb).flatten();
        if let Some(declared) = self.vram_gb {
            if self.engine_report_outranks_declared() {
                return CostSource::Measured;
            }
            return if fit.is_some_and(|fit| fit > declared) {
                CostSource::Fit
            } else {
                CostSource::Declared
            };
        }
        if self.may_book_measured() {
            // Both real sources present: the higher books (max truth), the
            // lower stands as the display figure it is.
            return match (self.measured_gb, fit) {
                (Some(measured), Some(fit)) if fit > measured => CostSource::Fit,
                _ => CostSource::Measured,
            };
        }
        if fit.is_some() {
            return CostSource::Fit;
        }
        if self.cost == CostClass::Exact {
            return CostSource::Undeclared;
        }
        CostSource::Estimate
    }
}

impl ModelEntry {
    /// Builds an entry from a flat `key: value` map (the v3.3 yaml shape) and the
    /// identity the caller vouches for — at scan that is the file stem (`name:` is
    /// legacy-optional and only legal when it equals the stem, so identity cannot be
    /// a yaml value's claim). Applies the documented alias `is_embedding_model:
    /// true` → EMB; the default category is WORK (Step 5, Step 9). The older app's
    /// `sticky:` alias is gone: a model is placed by its own `category:`, never by
    /// an inherited label.
    ///
    /// # Arguments
    ///
    /// * `keys` - the parsed flat map, verbatim (a `name:` key rides along in
    ///   `raw` but no longer selects the identity).
    /// * `name` - the canonical identity (file stem at scan; synthetic entries pass
    ///   their test name).
    ///
    /// # Errors
    ///
    /// - [`ModelError::BadCategory`] / [`ModelError::EmbeddingCategoryConflict`] on an
    ///   unusable category combination.
    /// - [`ModelError::BadNumber`] when `vram_gb` is not a positive number (0 books
    ///   a model free) or another size key holds a non-number.
    pub fn from_flat(keys: &BTreeMap<String, String>, name: &str) -> Result<Self, ModelError> {
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
            None => CategoryId::Work,
        };
        let vram_gb = match keys.get("vram_gb") {
            Some(raw) => {
                let v = parse_gib("vram_gb", raw)?;
                // A declared 0 is not a declared cost: the declared number books at
                // admission (`cost_source` → Declared; since the 2026-11-01 amendment
                // a guarded report may stand above it, never a zero below it), so
                // `vram_gb: 0` books any model free — gate deficit 0, ledger 0, and
                // the next load spends the same bytes (FINDINGS-12 [2]). Zero stays
                // legal for `launch_only_with_at_least_vram_free` below: a floor of
                // 0 means "no floor", the documented greedy opt-out (README), while
                // a size of 0 claims a footprint no model has. `==` on f64 is
                // IEEE-754, so this catches `-0.0` too (`-0.0 == 0.0` is true; the
                // `< 0.0` guard in `parse_gib` alone would not).
                if v == 0.0 {
                    return Err(ModelError::BadNumber {
                        field: "vram_gb".to_string(),
                        value: raw.to_string(),
                    });
                }
                Some(v)
            }
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
        // Resolved before the measured pair: the `cpu_offload` flip only voids a
        // range on engines whose adapters read the key, and an absent `engine:`
        // means llama (the scan default), so the policy comparison needs this.
        let engine = keys
            .get("engine")
            .cloned()
            .unwrap_or_else(|| "llama".to_string());
        // Full `extra_args` enforcement (slice 10c): a word outside the row's
        // closed operator-word surface refuses the file here, with the engine's
        // own refusal voice (word + owning mechanism) — the adapter keeps the
        // same check at render, so the scan is the early report, not the only
        // door. An engine id with no row has no surface to enforce; the actor's
        // `EngineUnavailable` drop is that file's report.
        if let Some(row) = crate::engine::policy::engine_policy(&engine) {
            if let Some(contract) = row.extra_words {
                if let Some(value) = keys
                    .get(contract.yaml_key)
                    .map(|v| v.trim())
                    .filter(|v| !v.is_empty())
                {
                    if let Err(error) = (contract.check)(&engine, name, value) {
                        return Err(ModelError::ExtraWordsRefused {
                            reason: error.to_string(),
                        });
                    }
                }
            }
        }
        let measured = read_measured(&engine, keys)?;
        Ok(ModelEntry {
            name: name.trim().to_string(),
            engine,
            category,
            vram_gb,
            gguf_size_gb: None,
            cost,
            launch_floor_gb,
            load_timeout_s,
            measured_gb: measured.max,
            measured_min_gb: measured.min,
            measured_offload: measured.offload,
            measured_attribution: measured.attribution,
            measured_profile: measured.profile,
            model_ref: keys.get("model").cloned(),
            serve_model: keys.get("serve_model").filter(|v| !v.is_empty()).cloned(),
            // A hand-written `fit_gb:` in a yaml is inert by construction: the
            // figure is the engine's live word, only ever set in-process by
            // the scheduler's probe, never read from disk.
            fit_gb: None,
            file: None,
            // v3.3's `external: true` is scrapped (operator 2026-10-26): skipped here
            // so a copied-back v3.3 yaml cannot keep advertising a meaning nothing has,
            // and cannot resurrect it through the raw passthrough on write-back. The
            // forensic `bak:` key joins `external` in never reaching the raw map
            // (operator ruling 2026-10-28, "forensic manual only"): its value can
            // neither surface on the page nor be re-emitted by write-back. The
            // operator's file bytes are left alone — only the parsed entry drops it.
            // R11's `fit_gb:` drops for the same reason run deeper: the figure is
            // the engine's live in-process word, and a stale one carried into the
            // write-back source could persist what the ruling forbids persisting.
            raw: keys
                .iter()
                .filter(|(key, _)| {
                    key.as_str() != "external"
                        && key.as_str() != "fit_gb"
                        && !crate::model::scan::SILENT_KEYS.contains(&key.as_str())
                })
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
        let entry =
            ModelEntry::from_flat(&flat(&[("name", "qwen3"), ("model", "F:/m.gguf")]), "qwen3")
                .expect("valid entry");
        assert_eq!(entry.category, CategoryId::Work);
        assert_eq!(entry.engine, "llama");
        assert_eq!(entry.model_ref.as_deref(), Some("F:/m.gguf"));
    }

    /// The v3.3 boolean truth table pinned in its own home (R2-12-01): the four
    /// accepted words hold under ASCII case mixing and surrounding whitespace,
    /// and everything else — `0`/`false`/`no`/`off`, mixed-case negatives, empty,
    /// blank, inner whitespace, arbitrary text — reads false, never a guess.
    /// Falsifier: accepting any rejected word, or refusing any accepted spelling,
    /// fails here before a drift of the alias shared by `is_embedding_model`,
    /// the `strata_*` flags and `ninfer_key` can reach a launch.
    #[test]
    fn truthy_pins_the_v3_3_boolean_table() {
        for value in ["1", "true", "yes", "on"] {
            assert!(truthy(value), "{value} must read true");
        }
        for value in ["TRUE", "True", "tRuE", "yEs", "YES", "ON", "On"] {
            assert!(truthy(value), "{value} must read true (ASCII case folds)");
        }
        for value in [" 1 ", "\ttrue\n", "  yes", "on  ", "\ton\n"] {
            assert!(truthy(value), "{value} must read true (edges trim)");
        }
        for value in [
            "0", "false", "no", "off", "False", "NO", "Off", "", "   ", "\t\n", "truthy", "2",
            "ye", "onn", "y es", "tr ue", "+1", "1.0", "on/off",
        ] {
            assert!(!truthy(value), "{value} must read false");
        }
    }

    /// Full `extra_args` enforcement (slice 10c): a hostile operator-extra
    /// word is refused by `from_flat` itself — the scan therefore reports the
    /// file at rescan, before any load could evict a resident for the doomed
    /// spawn — with the engine's own refusal voice naming the word and the
    /// mechanism that owns the decision. The benign family the live catalog
    /// uses passes through untouched. Falsifier: removing the row's
    /// extra-words check from `from_flat` admits the hostile word here.
    #[test]
    fn a_hostile_extra_word_is_refused_with_the_engine_voice() {
        let err = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "llama"),
                ("model", "F:/m.gguf"),
                ("extra_args", "--gpu-layers 99"),
            ]),
            "m",
        )
        .expect_err("a harness-owned word must refuse the file");
        match err {
            ModelError::ExtraWordsRefused { reason } => {
                assert!(
                    reason.contains("`--gpu-layers`")
                        && reason.contains("`cpu_offload:` is the harness's offload decision")
                        && reason.contains("engine `llama` rejected model `m`"),
                    "{reason}"
                );
            }
            other => panic!("expected ExtraWordsRefused, got {other:?}"),
        }
        let err = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("ninfer_serve_args", "--kv-dtype fp16"),
            ]),
            "m",
        )
        .expect_err("a harness-owned serve flag must refuse the file");
        assert!(
            matches!(&err, ModelError::ExtraWordsRefused { reason } if
                reason.contains("`--kv-dtype`")
                && reason.contains("harness-owned flag")
                && reason.contains("ninfer_profile")),
            "{err}"
        );
        ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("model", "F:/m.gguf"),
                ("extra_args", "--embedding --pooling last"),
            ]),
            "m",
        )
        .expect("the benign family stays admitted");
    }

    /// The Axis 1 primitive of the 2026-10-29 reload-drift ruling: still with the
    /// file, moved with its bytes, and `None` when unreadable — the absence of
    /// evidence every caller must read as "change not proven". Falsifier: a
    /// fingerprint that survives an edit (stale KEEP) or flakes while the file
    /// stands (false RETIRE) fails here first.
    #[test]
    fn a_file_fingerprint_stands_with_the_file_and_moves_with_the_bytes() {
        let dir = crate::testutil::temp_dir("fingerprint");
        let path = dir.join("m.yaml");
        std::fs::write(&path, "vram_gb: 4\n").expect("seed");
        let first = crate::model::fingerprint(&path).expect("a readable file fingerprints");
        assert_eq!(
            first,
            crate::model::fingerprint(&path)
                .expect("the pair is stable while the file stands still")
        );
        std::fs::write(&path, "vram_gb: 400\n").expect("edit");
        let moved = crate::model::fingerprint(&path).expect("an edited file fingerprints");
        assert_ne!(
            first, moved,
            "moved bytes must move the pair (the SIZE moved here; the clock leg is \
             isolated end-to-end by `rescan_supersedes_a_queued_load_when_only_the_clock_moves`, \
             which keeps the size identical and forces the mtime)"
        );
        assert_eq!(
            crate::model::fingerprint(&dir.join("gone.yaml")),
            None,
            "unreadable is None: no evidence either way"
        );
    }

    /// The server-owned format of the launch stamp (Axis 2, ruling 2026-10-29):
    /// the page prints these bytes verbatim, so fixed-width fields, UTC, second
    /// truncation and the leap/century boundaries are all this side's contract.
    /// The vectors were generated against an independent oracle
    /// (`[DateTimeOffset]::FromUnixTimeSeconds`). Falsifier: an off-by-one civil
    /// (era/civic arithmetic), a local-time render, or lost padding fails its
    /// vector; sub-second leakage makes the truncation leg non-reproducible.
    #[test]
    fn iso8601_utc_renders_wall_clock_as_the_page_prints_it() {
        let at = |secs: u64| std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        assert_eq!(crate::model::iso8601_utc(at(0)), "1970-01-01T00:00:00Z");
        assert_eq!(
            crate::model::iso8601_utc(at(1_709_209_845)),
            "2024-02-29T12:30:45Z",
            "leap day"
        );
        assert_eq!(
            crate::model::iso8601_utc(at(951_782_400)),
            "2000-02-29T00:00:00Z",
            "the leap century"
        );
        assert_eq!(
            crate::model::iso8601_utc(at(1_793_232_000)),
            "2026-10-29T00:00:00Z"
        );
        assert_eq!(
            crate::model::iso8601_utc(at(4_107_456_000)),
            "2100-02-28T00:00:00Z",
            "2100 is NOT a leap year"
        );
        assert_eq!(
            crate::model::iso8601_utc(at(4_107_542_400)),
            "2100-03-01T00:00:00Z",
            "and 2100-02-29 never exists"
        );
        assert_eq!(
            crate::model::iso8601_utc(std::time::UNIX_EPOCH - std::time::Duration::from_secs(1)),
            "1970-01-01T00:00:00Z",
            "a pre-epoch instant is a clock pathology: saturate, never fabricate a negative day"
        );
        assert_eq!(
            crate::model::iso8601_utc(at(0) + std::time::Duration::from_millis(999)),
            "1970-01-01T00:00:00Z",
            "sub-second truncates, never rounds"
        );
    }

    #[test]
    fn measured_range_parses_and_books_its_high_end() {
        let entry = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("measured_gb", "41.7"),
                ("measured_gb_min", "24.3"),
                ("measured_offload", "false"),
                ("cpu_offload", "false"),
            ]),
            "m",
        )
        .expect("valid range");
        assert_eq!(entry.measured_gb, Some(41.7));
        assert_eq!(entry.measured_min_gb, Some(24.3));
        assert_eq!(entry.measured_offload, Some(false));
        assert_eq!(entry.cost_source(), CostSource::Measured);
    }

    #[test]
    fn an_inverted_or_orphan_low_end_is_a_scan_error() {
        let inverted = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("measured_gb", "10"),
                ("measured_gb_min", "30"),
            ]),
            "m",
        );
        assert!(
            matches!(inverted, Err(ModelError::BadNumber { ref field, .. }) if field == "measured_gb_min"),
            "an inverted pair would silently widen every range read from it"
        );
        assert!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb_min", "10")]), "m").is_err(),
            "a low end with no high end books nothing"
        );
    }

    #[test]
    fn measured_attribution_parses_each_provenance() {
        for (raw, want) in [
            ("engine_report", MeasuredAttribution::EngineReport),
            ("engine_status", MeasuredAttribution::EngineStatus),
            ("per_process", MeasuredAttribution::PerProcess),
            ("total_diff", MeasuredAttribution::TotalDiff),
            ("ENGINE_REPORT", MeasuredAttribution::EngineReport),
        ] {
            let entry = ModelEntry::from_flat(
                &flat(&[
                    ("name", "m"),
                    ("engine", "llama"),
                    ("measured_gb", "10"),
                    ("measured_attribution", raw),
                ]),
                "m",
            )
            .expect("a written provenance token parses");
            assert_eq!(entry.measured_attribution, Some(want), "raw {raw}");
            if raw.eq_ignore_ascii_case("engine_report") {
                // Review M2's other side: the token PARSES on any engine (a scan
                // error would eat the operator's file), but the pair it stamps is
                // void-on-sight on the wrong engine kind.
                assert_eq!(entry.measured_gb, None, "cross-engine pair voids: {raw}");
            }
        }
    }

    #[test]
    fn an_unreadable_measured_attribution_is_a_scan_error() {
        let err = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("measured_gb", "10"),
                ("measured_attribution", "maybe"),
            ]),
            "m",
        );
        assert!(
            matches!(err, Err(ModelError::BadAttribution { ref field, .. }) if field == "measured_attribution"),
            "a provenance typo is a scan error, never a silent default"
        );
    }

    #[test]
    fn only_total_diff_is_unbookable() {
        assert!(!MeasuredAttribution::TotalDiff.bookable());
        assert!(MeasuredAttribution::EngineReport.bookable());
        // The 2026-11-04 ruling: the process engines' status figure books, and it
        // is a figure the engine produced about itself, so `exact` may charge it.
        assert!(MeasuredAttribution::EngineStatus.bookable());
        assert!(MeasuredAttribution::EngineStatus.self_reported());
        assert!(MeasuredAttribution::PerProcess.bookable());
    }

    #[test]
    fn a_ninfer_profile_change_voids_the_engine_report_range() {
        // The reserved KV is bound to the profile's --kv-capacity: a different profile
        // makes the engine_report figure incomparable, so the range drops (stamp kept).
        let changed = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("measured_gb", "30"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
                ("ninfer_profile", "c"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(changed.measured_gb, None, "profile moved: range voided");
        assert_eq!(
            changed.measured_attribution,
            Some(MeasuredAttribution::EngineReport),
            "the stamp survives the void"
        );
        assert_eq!(changed.measured_profile.as_deref(), Some("b"));
        let same = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("measured_gb", "30"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
                ("ninfer_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            same.measured_gb,
            Some(30.0),
            "profile unchanged: range kept"
        );
        // The name alone is not the identity: a custom profile whose override line
        // moved `kv` reserves a different KV pool under the same name, and the
        // engine_report figure must not survive that.
        let custom = |kv: &str| {
            ModelEntry::from_flat(
                &flat(&[
                    ("name", "m"),
                    ("engine", "ninfer"),
                    ("measured_gb", "30"),
                    ("measured_attribution", "engine_report"),
                    ("measured_profile", "lt:kv32768"),
                    ("ninfer_profile", "lt"),
                    ("ninfer_profile_lt", &format!("ctx=16384 kv={kv} conc=2")),
                ]),
                "m",
            )
            .expect("valid")
        };
        assert_eq!(
            custom("32768").measured_gb,
            Some(30.0),
            "same resolved capacity: the stamp still matches"
        );
        assert_eq!(
            custom("524288").measured_gb,
            None,
            "edited override line: same name, different reservation, voided"
        );
        let mut no_line = vec![
            ("name", "m"),
            ("engine", "ninfer"),
            ("measured_gb", "30"),
            ("measured_attribution", "engine_report"),
            ("measured_profile", "lt:kv32768"),
            ("ninfer_profile", "lt"),
        ];
        let unresolvable = ModelEntry::from_flat(&flat(&no_line), "m").expect("valid");
        assert_eq!(
            unresolvable.measured_gb, None,
            "a deleted override line cannot leave a figure claiming it"
        );
        no_line.push(("ninfer_profile_lt", "ctx=bad kv=bad conc=bad"));
        assert_eq!(
            ModelEntry::from_flat(&flat(&no_line), "m")
                .expect("valid")
                .measured_gb,
            None,
            "an unparseable override line voids, never certifies"
        );
    }

    /// Ruling 2026-11-04 (`TASK-ENGINE-MEASUREMENT`): a process engine's own
    /// status figure above the declared `vram_gb` books instead of it — the same
    /// amended-15(ii) max truth as the container's reservation. The live case:
    /// Basalt declared 65, measured 67.4, and the stale declaration refused the
    /// readmission the evidence recorded.
    #[test]
    fn the_engine_status_books_above_the_declared_value_as_max_truth() {
        let entry = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "basalt"),
                ("vram_gb", "65"),
                ("measured_gb", "67.4"),
                ("measured_gb_min", "67.4"),
                ("measured_attribution", "engine_status"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(entry.measured_gb, Some(67.4));
        assert_eq!(entry.cost_source(), CostSource::Measured);
        assert!(
            entry.measured_profile.is_none(),
            "a status figure mints no profile stamp: profile identity is ninfer's alone"
        );
        let not_above = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "basalt"),
                ("vram_gb", "68"),
                ("measured_gb", "67.4"),
                ("measured_gb_min", "67.4"),
                ("measured_attribution", "engine_status"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            not_above.cost_source(),
            CostSource::Declared,
            "a declared value at or above the report stays the operator's floor"
        );
    }

    /// Ruling 2026-11-04: the profile stamp stays ninfer-only, so an
    /// `engine_status` figure is VALID without one — no profile key may void
    /// what never had a profile to compare against.
    #[test]
    fn the_engine_status_is_valid_without_a_profile_stamp() {
        let kept = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "strata"),
                ("vram_gb", "67.8"),
                ("measured_gb", "68.1"),
                ("measured_gb_min", "68.1"),
                ("measured_attribution", "engine_status"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            kept.measured_gb,
            Some(68.1),
            "there is no ninfer profile on a strata row, and none is demanded"
        );
        assert_eq!(kept.measured_profile, None);
    }

    /// Ruling 2026-11-04: an `engine_status` stamp describes a process engine's
    /// status endpoint, so an `engine:` flip onto the container engine voids the
    /// figure the same way `engine_report` voids when flipped OFF it — the
    /// stamp survives the void as evidence of how the figure was taken.
    #[test]
    fn the_engine_status_voids_when_the_engine_flips_to_ninfer() {
        let flipped = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("cost", "exact"),
                ("measured_gb", "30"),
                ("measured_gb_min", "30"),
                ("measured_attribution", "engine_status"),
            ]),
            "m",
        )
        .expect("the token parses on any engine, a scan error would eat the operator's file");
        assert_eq!(
            flipped.measured_gb, None,
            "an engine_status stamp on a ninfer row must not book: a process-engine source"
        );
        assert_eq!(
            flipped.measured_attribution,
            Some(MeasuredAttribution::EngineStatus),
            "the stamp survives the void"
        );
        assert_eq!(
            flipped.cost_source(),
            CostSource::Undeclared,
            "an `exact` row with the figure voided has no source, and absence is refused"
        );
    }

    /// Ruling 2026-11-04: `exact` demands a real source, and the engine's own
    /// status answer is one (`self_reported`); a `total_diff` stand-in on the
    /// very same row stays refused — provenance decides, never the number.
    #[test]
    fn the_engine_status_is_self_reported_so_exact_books_it() {
        let entry = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "strata"),
                ("cost", "exact"),
                ("measured_gb", "67.8"),
                ("measured_gb_min", "67.8"),
                ("measured_attribution", "engine_status"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            entry.cost_source(),
            CostSource::Measured,
            "the exact class demands a source, and the status endpoint is the engine speaking"
        );
        let stand_in = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "strata"),
                ("cost", "exact"),
                ("measured_gb", "67.8"),
                ("measured_gb_min", "67.8"),
                ("measured_attribution", "total_diff"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            stand_in.cost_source(),
            CostSource::Undeclared,
            "the identical number under total_diff still never books"
        );
    }

    /// Review 2026-10-31 (H2 + M2): the comparability gate keys on the ATTRIBUTION,
    /// not on the presence of a stamp, and a stamp can never cross engines. The
    /// first cut let an unstamped `engine_report` (written behind an unvalidated
    /// legacy-autostart profile) survive every profile change, and let an
    /// `engine:` flip in the same file carry the figure onto the new engine.
    #[test]
    fn an_unstamped_or_cross_engine_report_figure_never_books() {
        let unstamped = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("measured_gb", "30"),
                ("measured_attribution", "engine_report"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            unstamped.measured_gb, None,
            "an engine_report with no resolved profile had no certifying launch"
        );
        let flipped = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "llama"),
                ("measured_gb", "30"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            flipped.measured_gb, None,
            "a container reservation cannot describe a llama process"
        );
        let wrong_engine = |engine: &str, attribution: &str| {
            ModelEntry::from_flat(
                &flat(&[
                    ("name", "m"),
                    ("engine", engine),
                    ("measured_gb", "30"),
                    ("measured_attribution", attribution),
                ]),
                "m",
            )
            .expect("valid")
        };
        assert_eq!(
            wrong_engine("ninfer", "per_process").measured_gb,
            None,
            "a container has no harness child to attribute"
        );
        assert_eq!(
            wrong_engine("llama", "per_process").measured_gb,
            Some(30.0),
            "per_process on its own engine kind survives"
        );
        assert_eq!(
            wrong_engine("ninfer", "total_diff").measured_gb,
            Some(30.0),
            "total_diff crosses engines honestly (it never books anyway)"
        );
    }

    #[test]
    fn a_cpu_offload_flip_voids_the_range_but_keeps_its_stamp() {
        // The pair describes a different launch: an offloaded run has no comparable
        // footprint, so it must be neither booked nor displayed (§0.4 invalidation).
        let entry = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("measured_gb", "41.7"),
                ("measured_gb_min", "24.3"),
                ("measured_offload", "true"),
                ("cpu_offload", "false"),
            ]),
            "m",
        )
        .expect("still a valid entry");
        assert_eq!(entry.measured_gb, None);
        assert_eq!(entry.measured_min_gb, None);
        assert_eq!(
            entry.measured_offload,
            Some(true),
            "the stamp survives in the entry and the /har payload (the page renders \
             the range, not the stamp)"
        );
        assert_eq!(entry.cost_source(), CostSource::Estimate);
        // A flip is judged with the engine's truth set, so `on` is `true` here too —
        // a third reading of the token would let a flip hide (HANDOFF §6).
        let agreed = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("measured_gb", "41.7"),
                ("measured_offload", "true"),
                ("cpu_offload", "on"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(agreed.measured_gb, Some(41.7));
    }

    /// D of the `cpu_offload` scoping ruling: the flip-void rule belongs to the
    /// engines that read the key. A strata (and ninfer, and the engine-less-off
    /// `mock`) row's measured pair describes a launch the key cannot influence, so
    /// an added or flipped `cpu_offload` must neither drop the numbers nor refuse
    /// the file. Falsifier: un-gating the void in `read_measured` (any engine
    /// comparing stamp against the inert key) drops the pair here — the one real
    /// damage the key did off llama.
    #[test]
    fn a_cpu_offload_flip_voids_nothing_for_engines_that_never_read_it() {
        for engine in ["strata", "ninfer", "mock"] {
            assert!(
                !crate::engine::llama::offload_live(engine),
                "{engine} must be outside the offload knob table for this pin to mean anything"
            );
            let entry = ModelEntry::from_flat(
                &flat(&[
                    ("name", "m"),
                    ("engine", engine),
                    ("measured_gb", "41.7"),
                    ("measured_gb_min", "24.3"),
                    ("measured_offload", "false"),
                    ("cpu_offload", "true"),
                ]),
                "m",
            )
            .unwrap_or_else(|e| panic!("{engine}: flip must not refuse the file: {e}"));
            assert_eq!(
                (entry.measured_gb, entry.measured_min_gb),
                (Some(41.7), Some(24.3)),
                "{engine}: an inert key must not void the range"
            );
            assert_eq!(
                entry.cost_source(),
                CostSource::Measured,
                "{engine}: and must not change what admission books"
            );
        }
    }

    #[test]
    fn an_unreadable_offload_stamp_is_a_scan_error_not_a_silent_false() {
        let err =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_offload", "maybe")]), "m");
        assert!(
            matches!(err, Err(ModelError::BadBool { ref field, .. }) if field == "measured_offload")
        );
    }

    #[test]
    fn cost_source_mirrors_the_booking_precedence() {
        let declared =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("vram_gb", "10")]), "m").expect("valid");
        assert_eq!(declared.cost_source(), CostSource::Declared);
        // `fixed` honours a measured footprint; `greedy` never does (a post-spill
        // measurement is not a nominal size).
        let measured = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("cost", "fixed"), ("measured_gb", "7")]),
            "m",
        )
        .expect("valid");
        assert_eq!(measured.cost_source(), CostSource::Measured);
        let greedy = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("cost", "greedy"), ("measured_gb", "7")]),
            "m",
        )
        .expect("valid");
        assert_eq!(greedy.cost_source(), CostSource::Estimate);
        let greedy_reported = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("cost", "greedy"),
                ("measured_gb", "7"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            greedy_reported.measured_gb,
            Some(7.0),
            "the pair survives on its honest engine kind — greedy must refuse a LIVE figure"
        );
        assert_eq!(
            greedy_reported.cost_source(),
            CostSource::Estimate,
            "a self-report is still a footprint, not a nominal size: greedy never books it"
        );
        let legacy_pair = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("cost", "exact"), ("measured_gb", "7")]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            legacy_pair.cost_source(),
            CostSource::Undeclared,
            "a stamp-less legacy pair is grandfathered for `fixed`, but `exact` demands a source"
        );
        // `exact` without a declared number is refused, never estimated.
        let exact =
            ModelEntry::from_flat(&flat(&[("name", "m"), ("cost", "exact")]), "m").expect("valid");
        assert_eq!(exact.cost_source(), CostSource::Undeclared);
    }

    /// R11 (rule 2026-11-01): the fit projection books with the SAME max truth
    /// as the guarded engine_report — declared is a floor, never a ceiling —
    /// it verifies `exact` rows (a real source, not an estimate), it never
    /// labels a `greedy` booking, and it is scoped to the fit-capable engine
    /// row, never to whichever engine happens to answer to a name.
    #[test]
    fn the_fit_projection_books_max_truth_on_the_llama_row_only() {
        let mut row = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "llama"), ("vram_gb", "24")]),
            "m",
        )
        .expect("valid");
        row.fit_gb = Some(31.0);
        assert_eq!(
            row.cost_source(),
            CostSource::Fit,
            "the engine's own word above the declared floor books"
        );
        row.fit_gb = Some(24.0);
        assert_eq!(
            row.cost_source(),
            CostSource::Declared,
            "declared is a floor: equal does not overshadow"
        );
        row.fit_gb = Some(20.0);
        assert_eq!(row.cost_source(), CostSource::Declared);
        // No declaration: the fit pair and a certified measured figure compete
        // the same max-truth way.
        let mut row = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "llama"), ("measured_gb", "28")]),
            "m",
        )
        .expect("valid");
        row.fit_gb = Some(31.0);
        assert_eq!(row.cost_source(), CostSource::Fit);
        row.fit_gb = Some(26.0);
        assert_eq!(row.cost_source(), CostSource::Measured);
        // `exact` without a declaration verifies THROUGH the projection: a fit
        // figure is a real source, so the row is no longer undeclared.
        let mut exact = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "llama"), ("cost", "exact")]),
            "m",
        )
        .expect("valid");
        assert_eq!(exact.cost_source(), CostSource::Undeclared);
        exact.fit_gb = Some(12.0);
        assert_eq!(exact.cost_source(), CostSource::Fit);
        // `greedy` never wears the Fit label (its launch floor reads the figure
        // directly in the scheduler, exactly as for a measured pair).
        let mut greedy = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "llama"), ("cost", "greedy")]),
            "m",
        )
        .expect("valid");
        greedy.fit_gb = Some(12.0);
        assert_eq!(
            greedy.cost_source(),
            CostSource::Estimate,
            "greedy refuses a projection as a CLASS"
        );
        // Row scope: only the llama policy row is fit-capable.
        let mut ninfer = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "ninfer"), ("vram_gb", "24")]),
            "m",
        )
        .expect("valid");
        ninfer.fit_gb = Some(31.0);
        assert_eq!(
            ninfer.cost_source(),
            CostSource::Declared,
            "a figure from a tool no other engine owns is not that engine's word"
        );
    }

    /// Never-persisted, in both directions: a hand-written `fit_gb:` in a yaml
    /// is inert (the figure is only ever the scheduler's live in-process word),
    /// and the typed field never joins `raw` (the write-back source).
    #[test]
    fn fit_gb_is_never_read_from_disk_nor_written_to_it() {
        let entry = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("engine", "llama"), ("fit_gb", "99")]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            entry.fit_gb, None,
            "a stale yaml figure is inert by construction"
        );
        assert!(
            !entry.raw.contains_key("fit_gb"),
            "an unknown key must not survive into the write-back passthrough either"
        );
    }

    /// R1/F1(b) + ruling 15 (ii): the provenance stamp, not the bare presence of a
    /// measured pair, decides what books. `total_diff` displays but never books —
    /// neither for `fixed` (no resurrecting the GGUF estimate either: it stays the
    /// estimate) nor for `exact` (a stand-in is not a source). `engine_report` books
    /// `exact` too: the engine's own reserved budget is the source the class demands.
    /// Falsifier: reading `measured_gb` without consulting `measured_attribution`
    /// books the total-diff rows here.
    #[test]
    fn provenance_decides_booking_for_fixed_and_exact() {
        let fixed_total_diff = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("cost", "fixed"),
                ("measured_gb", "7"),
                ("measured_attribution", "total_diff"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            fixed_total_diff.cost_source(),
            CostSource::Estimate,
            "a total-diff must not book, and must not replace the estimate with a lie either"
        );
        let exact_total_diff = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("cost", "exact"),
                ("measured_gb", "7"),
                ("measured_attribution", "total_diff"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            exact_total_diff.cost_source(),
            CostSource::Undeclared,
            "exact stays refused on a stand-in figure"
        );
        // Each bookable provenance is shown on the engine kind that can honestly
        // carry it (review 2026-10-31, M2): an `engine_report` on a llama row is
        // void-on-sight, and a `per_process` on a container row likewise. The
        // `measured_profile` is the identity a template-path fold really writes —
        // an UNstamped engine_report has no certifying launch and never books
        // (the H2 pin below).
        for (token, engine, profile) in [
            ("engine_report", "ninfer", Some("b")),
            ("per_process", "llama", None),
        ] {
            let mut keys = vec![
                ("name", "m"),
                ("engine", engine),
                ("cost", "exact"),
                ("measured_gb", "7"),
                ("measured_attribution", token),
            ];
            if let Some(profile) = profile {
                keys.push(("measured_profile", profile));
            }
            let exact_reported = ModelEntry::from_flat(&flat(&keys), "m").expect("valid");
            assert_eq!(
                exact_reported.cost_source(),
                CostSource::Measured,
                "{token}: a bookable provenance is a source for exact"
            );
        }
        // A declared value at or above the report is the operator's floor and
        // stands (ruling 2026-11-01 amends only the OVERSHADOWED leg below).
        let declared_wins = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("cost", "exact"),
                ("vram_gb", "26"),
                ("measured_gb", "7"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            declared_wins.cost_source(),
            CostSource::Declared,
            "a report smaller than the declared floor must not shrink the booking"
        );
        // The amendment (operator ruling 2026-11-01): a guarded engine_report that
        // EXCEEDS the declared value books — admission must keep free what the
        // container itself reserved. Only the engine's own word, still certified by
        // its profile stamp, gets to overshadow; a witness `per_process` figure
        // (weaker staleness guard, no profile stamp) stays annotation over a
        // declared row.
        let report_overshadows = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("cost", "fixed"),
                ("vram_gb", "34"),
                ("measured_gb", "44"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            report_overshadows.cost_source(),
            CostSource::Measured,
            "the container's certified reservation IS the booking"
        );
        let witness_does_not = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "llama"),
                ("cost", "fixed"),
                ("vram_gb", "34"),
                ("measured_gb", "44"),
                ("measured_attribution", "per_process"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            witness_does_not.cost_source(),
            CostSource::Declared,
            "a witness fold does not overshadow a declared value"
        );
        // Greedy stays untouched by the amendment: its class may never book a
        // measurement (nominal semantics), so a certified report cannot raise it
        // either — the overshadow leg stops at `may_book_measured`.
        let greedy_untouched = ModelEntry::from_flat(
            &flat(&[
                ("name", "m"),
                ("engine", "ninfer"),
                ("cost", "greedy"),
                ("vram_gb", "34"),
                ("measured_gb", "44"),
                ("measured_attribution", "engine_report"),
                ("measured_profile", "b"),
            ]),
            "m",
        )
        .expect("valid");
        assert_eq!(
            greedy_untouched.cost_source(),
            CostSource::Declared,
            "a greedy row books its declared nominal even under a bigger report"
        );
    }

    #[test]
    fn retired_external_key_is_skipped_from_the_raw_map() {
        // v3.3 `external: true` is scrapped: it must not survive into the raw
        // passthrough, where it would advertise a meaning nothing has and be written
        // back out as if it were live config. Every other key still passes through.
        let entry = ModelEntry::from_flat(&flat(&[("name", "m"), ("external", "true")]), "m")
            .expect("valid entry");
        assert!(!entry.raw.contains_key("external"));
        let kept = ModelEntry::from_flat(&flat(&[("name", "m"), ("custom", "x")]), "m")
            .expect("valid entry");
        assert_eq!(kept.raw.get("custom").map(String::as_str), Some("x"));
    }

    #[test]
    fn reserved_silent_bak_key_is_skipped_from_the_raw_map() {
        // `bak:` is forensic-manual-only (operator ruling 2026-10-28): stripped
        // from the parsed raw map like the retired `external`, so its value can
        // never reach the page or be re-emitted by write-back. Every other key
        // still passes through.
        let entry = ModelEntry::from_flat(
            &flat(&[("name", "m"), ("bak", "snapshot 2026-10-28 pre-quant")]),
            "m",
        )
        .expect("valid entry");
        assert!(!entry.raw.contains_key("bak"));
        let kept = ModelEntry::from_flat(&flat(&[("name", "m"), ("custom", "x")]), "m")
            .expect("valid entry");
        assert_eq!(kept.raw.get("custom").map(String::as_str), Some("x"));
    }

    #[test]
    fn measured_gb_parses_and_rejects_garbage() {
        let e = ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "70.5")]), "m")
            .expect("valid entry");
        assert_eq!(e.measured_gb, Some(70.5));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "abc")]), "m")
                .expect_err("garbage"),
            ModelError::BadNumber { .. }
        ));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0")]), "m")
                .expect_err("zero is not a measurement"),
            ModelError::BadNumber { .. }
        ));
        assert!(matches!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0.04")]), "m")
                .expect_err("sub-quantum is not a measurement"),
            ModelError::BadNumber { .. }
        ));
        assert_eq!(
            ModelEntry::from_flat(&flat(&[("name", "m"), ("measured_gb", "0.05")]), "m")
                .expect("one quantum is")
                .measured_gb,
            Some(0.05)
        );
    }

    #[test]
    fn load_timeout_parses_and_bounds() {
        let entry = ModelEntry::from_flat(&flat(&[("name", "s"), ("load_timeout", "1200")]), "s")
            .expect("valid entry");
        assert_eq!(entry.load_timeout_s, Some(1200));
        for bad in ["0", "86401", "abc", "-5", "nan", "1e400"] {
            let err = ModelEntry::from_flat(&flat(&[("name", "s"), ("load_timeout", bad)]), "s")
                .expect_err("{bad} must be refused");
            assert!(
                matches!(err, ModelError::BadNumber { ref field, .. } if field == "load_timeout"),
                "{bad}: {err}"
            );
        }
    }

    #[test]
    fn embedding_alias_forces_emb_and_no_category_means_work() {
        let embed =
            ModelEntry::from_flat(&flat(&[("name", "b"), ("is_embedding_model", "true")]), "b")
                .expect("valid entry");
        assert_eq!(embed.category, CategoryId::Emb);

        // The removed `sticky:` alias must not quietly keep working: an entry naming no
        // category lands in WORK whatever else it carries.
        let unplaced = ModelEntry::from_flat(&flat(&[("name", "a"), ("sticky", "true")]), "a")
            .expect("unknown keys are preserved, not rejected");
        assert_eq!(unplaced.category, CategoryId::Work);
    }

    #[test]
    fn explicit_category_wins_and_embedding_conflicts_are_errors() {
        let rp = ModelEntry::from_flat(&flat(&[("name", "a"), ("category", "RP")]), "a")
            .expect("valid entry");
        assert_eq!(rp.category, CategoryId::Rp);

        let err = ModelEntry::from_flat(
            &flat(&[
                ("name", "b"),
                ("category", "WORK"),
                ("is_embedding_model", "true"),
            ]),
            "b",
        )
        .expect_err("embedding model must live in EMB");
        assert!(matches!(err, ModelError::EmbeddingCategoryConflict { .. }));
    }

    #[test]
    fn bad_numbers_are_errors() {
        // Identity is the caller's (the scan's stem); `from_flat` no longer reads
        // the `name` key at all — an empty map with a vouched name is a valid
        // default entry, not a missing-name error.
        let defaulted = ModelEntry::from_flat(&flat(&[]), "m").expect("name comes from the caller");
        assert_eq!(defaulted.name, "m");
        assert_eq!(defaulted.category, CategoryId::Work);
        let err = ModelEntry::from_flat(&flat(&[("name", "a"), ("vram_gb", "big")]), "a")
            .expect_err("vram_gb must be numeric");
        assert!(matches!(err, ModelError::BadNumber { .. }));
    }

    #[test]
    fn cost_classes_and_launch_floor_parse() {
        // Operator design note 2026-10-26: graded admission per engine kind.
        let entry = ModelEntry::from_flat(
            &flat(&[
                ("name", "g"),
                ("cost", "Greedy"),
                ("launch_only_with_at_least_vram_free", "6"),
            ]),
            "g",
        )
        .expect("greedy parses");
        assert_eq!(entry.cost, CostClass::Greedy);
        assert_eq!(entry.launch_floor_gb, Some(6.0));

        let exact = ModelEntry::from_flat(&flat(&[("name", "e"), ("cost", "exact")]), "e")
            .expect("exact parses");
        assert_eq!(exact.cost, CostClass::Exact);

        let fixed = ModelEntry::from_flat(&flat(&[("name", "f")]), "f").expect("default cost");
        assert_eq!(fixed.cost, CostClass::Fixed);
        assert_eq!(fixed.launch_floor_gb, None);

        let bad = ModelEntry::from_flat(&flat(&[("name", "x"), ("cost", "hoarder")]), "x")
            .expect_err("unknown cost class is a scan error, never a silent default");
        assert!(matches!(bad, ModelError::BadCost { .. }), "{bad}");

        for value in ["eight", "-2", "nan", "inf", "1e400"] {
            let err = ModelEntry::from_flat(
                &flat(&[
                    ("name", "x"),
                    ("launch_only_with_at_least_vram_free", value),
                ]),
                "x",
            )
            .expect_err("floor must be a finite non-negative number");
            assert!(matches!(err, ModelError::BadNumber { .. }), "{err}");
            let err = ModelEntry::from_flat(&flat(&[("name", "x"), ("vram_gb", value)]), "x")
                .expect_err("vram_gb must be a finite non-negative number");
            assert!(matches!(err, ModelError::BadNumber { .. }), "{err}");
        }
    }

    #[test]
    fn vram_gb_must_be_positive_but_a_zero_launch_floor_is_the_opt_out() {
        // FINDINGS-12 [2]: `vram_gb: 0` wins the booking precedence as Declared and
        // books the model free — the gate deficit, the ledger and the in-flight sum
        // all read 0, so the next load spends the same bytes on top.
        for value in ["0", "0.0", "-0", "-0.0"] {
            let err = ModelEntry::from_flat(&flat(&[("name", "x"), ("vram_gb", value)]), "x")
                .expect_err("a declared 0 GiB is not a declared cost");
            assert!(
                matches!(err, ModelError::BadNumber { ref field, .. } if field == "vram_gb"),
                "{value}: {err}"
            );
        }
        // The sibling opt-out keeps its documented zero (README: "set `0` per-model
        // to opt out explicitly"); killing it here would break greedy launches.
        let opt_out = ModelEntry::from_flat(
            &flat(&[
                ("name", "g"),
                ("cost", "greedy"),
                ("launch_only_with_at_least_vram_free", "0"),
            ]),
            "g",
        )
        .expect("zero is the floor's explicit opt-out, not a typo");
        assert_eq!(opt_out.launch_floor_gb, Some(0.0));
        let full = ModelEntry::from_flat(
            &flat(&[
                ("name", "full"),
                ("engine", "llama"),
                ("category", "RP"),
                ("model", "F:/m.gguf"),
                ("vram_gb", "6.5"),
                ("cost", "fixed"),
                ("launch_only_with_at_least_vram_free", "12"),
                ("load_timeout", "300"),
            ]),
            "full",
        )
        .expect("a full valid yaml still parses under the new rule");
        assert_eq!(full.vram_gb, Some(6.5));
        assert_eq!(full.cost_source(), CostSource::Declared);
    }
}
