//! Measured-fold types: the witness reading, the fold plan, the persist payload,
//! the serving-window sampler context, and the spawn-era measured anchor with its
//! pure rules (the anchor derivation and the serve_model mismatch verdict).
//!
//! Data and pure functions only: this module never probes, never mutates, and
//! never touches the card, the filesystem, or an engine. The actor owns the values.

use crate::model::ModelEntry;
use crate::ownership::SpawnTicket;
use std::path::PathBuf;

/// The two witness numbers for one fold: what the card grew by since this spawn's
/// baseline, and what the driver attributed to the engine's own pids (R1: `None`
/// means per-process attribution was unavailable, so the fold may only ever be
/// stamped `total_diff` — which displays but never books).
#[derive(Debug, Clone, Copy)]
pub(super) struct Reading {
    pub(super) total_diff: f64,
    pub(super) own: Option<f64>,
    pub(super) total_gib: f64,
    pub(super) used_gib: f64,
}

/// One figure handed to [`ActorState::fold`], bundled with the launch it belongs to
/// so the fold reads as "this spawn says this number" rather than seven positional
/// arguments. `fold_range` is the second half of the provenance contract: a witness
/// point is averaged into this launch's history, while the container's own reserved
/// figure is static for the life of the process and is never averaged.
#[derive(Debug, Clone)]
pub(super) struct FoldPlan {
    pub(super) model: String,
    pub(super) ticket: SpawnTicket,
    pub(super) high: f64,
    pub(super) low: f64,
    pub(super) attribution: crate::model::MeasuredAttribution,
    pub(super) profile: Option<String>,
    pub(super) fold_range: bool,
}

/// Everything one fold wrote to the catalog row: the pair and the provenance
/// stamps, kept together so a refused persist drains exactly what it wrote
/// (Fork C extended to the R9 stamps — a drain that forgot `measured_attribution`
/// would leave the file and the memory disagreeing about who owns the number).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MeasuredData {
    pub(super) high: f64,
    pub(super) low: f64,
    pub(super) offload: Option<bool>,
    pub(super) attribution: Option<crate::model::MeasuredAttribution>,
    pub(super) profile: Option<String>,
}

impl MeasuredData {
    /// The `(key, value)` edits for one persist: a `None` value removes the key, so
    /// a total-diff fold cleans up the stamps a superseded `engine_report` wrote.
    /// All keys ride in ONE rewrite: split across writes, a failure in between could
    /// leave an orphan pair or a range booked under the wrong provenance (the
    /// scanner rejects the orphan outright — dropping the whole model).
    pub(super) fn edits(&self) -> Vec<(&'static str, Option<String>)> {
        vec![
            ("measured_gb", Some(format!("{:.1}", self.high))),
            ("measured_gb_min", Some(format!("{:.1}", self.low))),
            ("measured_offload", self.offload.map(|on| on.to_string())),
            (
                "measured_attribution",
                self.attribution.map(|a| a.as_str().to_string()),
            ),
            ("measured_profile", self.profile.clone()),
        ]
    }
}

/// Everything the serving-window sampler needs, captured at install so the task
/// never reaches back into actor state: the `(model, ticket)` pair a reading is
/// stamped with, the pids the witness may attribute (empty for a container spawn —
/// its ticket pid is the dead `wsl.exe` client), and the container whose own
/// startup report the sampler fetches once before falling back to the witness (R9).
#[derive(Debug, Clone)]
pub(super) struct SamplerCtx {
    pub(super) model: String,
    pub(super) ticket: SpawnTicket,
    pub(super) own_pids: Vec<u32>,
    pub(super) container: Option<String>,
    pub(super) engine: String,
}

/// What the measured fold is allowed to say about a running engine, and where it may
/// write it. Captured at `begin_spawn` from the catalog row the launch was built from,
/// never re-read at sample time (`research/reload-slot-drift-2026-10-28.md` §1 rows
/// 6-10): `reload()` replaces `self.catalog` wholesale while the process keeps the argv
/// it started with, so a fold that consulted the live catalog would stamp a range for a
/// launch that never happened — and because the flip-void rule compares that very stamp
/// with the same live bytes, it would also disarm the rule built to catch the lie.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MeasuredAnchor {
    /// The yaml that owns this reading, by **file** identity. A reload that renames the
    /// row keeps the bytes and moves the name, and a re-minted twin steals the name, so
    /// the name is not an identity a fold may write to (FINDINGS-13 [3]). `None` = the
    /// row had no backing file when this engine spawned, so nothing on disk owns the
    /// reading and the fold stays closed.
    pub(super) file: Option<PathBuf>,
    /// The `cpu_offload` the engine actually started under, already gated through
    /// [`crate::engine::llama::offload_live`]: an engine that never reads the key keeps
    /// `None` and mints no meaningless stamp (§20-9 Fork C knob-table row 1).
    pub(super) offload: Option<bool>,
    /// The range folded across this spawn's samples, seeded from the row's persisted
    /// pair at spawn. Seeding is what keeps a mid-life reload — which drops the
    /// in-memory pair the moment a stamp no longer matches — from silently re-seeding
    /// an accumulated range from the one process still on the card.
    pub(super) range: Option<(f64, f64)>,
    /// The `serve_model` the row declared when this engine was launched
    /// (FINDINGS-BATCH-2026-10-28L [L-2]). The install-time mismatch verdict is about
    /// the process on the card, so it is judged against these bytes: a reload landing
    /// inside the cold-start window must not manufacture or erase the warning, and a
    /// same-name twin must not lend this one its id.
    pub(super) serve_model: Option<String>,
    /// How the figure this spawn has folded was obtained (R9). `None` until the first
    /// fold lands: a reload's `read_measured` drops the in-memory stamp together with
    /// the range it describes, and the next fold re-mints both from this spawn — the
    /// stamp is never a leftover from a process that is gone.
    pub(super) attribution: Option<crate::model::MeasuredAttribution>,
    /// The ninfer profile identity this engine was launched under: the profile name
    /// plus the `--kv-capacity` a custom override line stands for
    /// (`crate::engine::ninfer::profile_stamp`) — the argument the autostart script
    /// effectively received, default `b` — the same derivation `read_measured`
    /// compares the persisted stamp against. Captured at spawn, never re-read at
    /// sample time, like every other anchor field.
    pub(super) profile: Option<String>,
    /// Whether this spawn is backed by a detached WSL container (ninfer). A container
    /// spawn never claims per-process attribution — its ticket pid is the dead
    /// `wsl.exe` client — so witness readings can only ever be `total_diff` there.
    pub(super) container: bool,
    /// The Fit projection (R11) that actually BOOKED this spawn: the figure
    /// `cost_source()` answered `Fit` with at spawn time. `None` = this load was
    /// not Fit-booked, so the session cross-check has nothing to confirm. Captured
    /// at spawn like every other anchor field: the cross-check must compare the
    /// witness against what this process was reserved by, not against whichever
    /// catalog figure a later rescan patched under the same name.
    pub(super) fit_gb: Option<f64>,
}

impl MeasuredAnchor {
    /// Captures the anchor from the entry a spawn is about to be built from. One
    /// derivation, called by [`ActorState::begin_spawn`] (and by the pins that seat a
    /// spawn), so the stamp can never be read from a different side of the fold than
    /// the engine's argv was.
    #[must_use]
    pub(super) fn at_spawn(entry: &ModelEntry) -> Self {
        Self {
            file: entry.file.clone(),
            offload: crate::engine::llama::offload_live(&entry.engine).then(|| {
                entry
                    .raw
                    .get("cpu_offload")
                    .is_some_and(|value| crate::engine::llama::jinja_on(value))
            }),
            range: match (entry.measured_gb, entry.measured_min_gb) {
                (Some(high), Some(low)) => Some((high, low)),
                // A lone high end is still the floor of what this spawn may book; a
                // lone low end cannot exist (the scanner rejects an orphan pair).
                (Some(high), None) => Some((high, high)),
                (None, _) => None,
            },
            serve_model: entry.serve_model.clone(),
            attribution: None,
            profile: (entry.engine == crate::ownership::NINFER_ENGINE)
                .then(|| crate::engine::ninfer::profile_stamp(&entry.raw))
                .flatten(),
            container: entry.engine == crate::ownership::NINFER_ENGINE,
            fit_gb: (entry.cost_source() == crate::model::CostSource::Fit)
                .then_some(entry.fit_gb)
                .flatten(),
        }
    }
}

/// The serve_model mismatch verdict, decided against the SPAWN-era row
/// (FINDINGS-BATCH-2026-10-28L [L-2]).
///
/// Taking the anchor instead of the catalog is the whole point: the pre-fix site read
/// `self.catalog.iter().find(|m| m.name == job.model)` minutes after `begin_spawn`, so
/// under a mid-serve reload the warning compared the engine's real id against yaml that
/// arrived after the launch (a false warning, or a missed one), and under same-name
/// twins it could read the wrong twin's `serve_model`.
///
/// # Arguments
///
/// * `anchor` - the job's spawn-era anchor, `None` for a job that never spawned.
/// * `discovered` - the id the engine actually reported, `None` when discovery saw
///   nothing.
///
/// # Returns
///
/// The warning sentence, or `None` when there is nothing to report.
#[must_use]
pub(super) fn serve_model_warning(
    anchor: Option<&MeasuredAnchor>,
    discovered: Option<&str>,
) -> Option<String> {
    let declared = anchor?.serve_model.as_deref()?;
    let actual = discovered?;
    (declared != actual).then(|| {
        format!(
            "serve_model `{declared}` is not reported by the engine; using its real id `{actual}`"
        )
    })
}
