//! Measured-fold types: the witness reading, the fold plan, the persist payload,
//! the serving-window sampler context, and the spawn-era measured anchor with its
//! pure rules (the anchor derivation and the serve_model mismatch verdict).
//!
//! The data types stay pure: they never probe, never mutate, and never touch the
//! card, the filesystem, or an engine. The `impl ActorState` block below owns the
//! actor-side fold and persist lifecycle, including the measured writeback. The
//! actor owns the values.

use super::{ActorMsg, ActorState};
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

impl ActorState {
    pub(super) fn fold(&mut self, plan: &FoldPlan) {
        let FoldPlan {
            model,
            ticket,
            high,
            low,
            attribution,
            profile,
            fold_range,
        } = plan;
        let (model, high, low) = (model.as_str(), *high, *low);
        // The row that owns this reading is the spawn-era FILE, never whichever entry
        // answers to the name now: a reload that renames the yaml keeps the bytes and
        // moves the name, and a re-minted twin steals it, so only the file the engine
        // was launched from may receive the write (FINDINGS-13 [3]).
        let Some((spawned_job, anchor)) = self
            .jobs
            .iter()
            .find(|(_, job)| job.model == model && job.spawned.as_ref() == Some(ticket))
            .map(|(id, job)| (*id, job.measured.clone()))
        else {
            return;
        };
        let Some(anchor) = anchor else { return };
        let Some(path) = anchor.file.clone() else {
            // Synthetic row at spawn: no operator file exists, so nothing may be
            // written and nothing may be booked against a name nobody owns. The sampler
            // still has to be retired here (FINDINGS2-04 [3]): without this line the
            // task keeps walking the witness every `measured_sample_s` until its 24 h
            // deadline while the actor discards every reading it returns — the same
            // orphan class the row-gone branch below closes, reached from the other
            // unreadable anchor shape.
            tracing::warn!(
                model,
                "measured reading has no operator file: the spawn-era row was synthetic, \
                 so the sampler is retired instead of sampling into a discard"
            );
            self.retire_sampler(model, ticket);
            return;
        };
        let Some(entry) = self
            .catalog
            .iter_mut()
            .find(|m| m.file.as_deref() == Some(path.as_path()))
        else {
            // The row went away while its engine kept serving (renamed file, a new
            // problem line, an unknown engine). Nothing here can own the reading: say
            // so once and retire the sampler, which would otherwise walk the witness
            // every `measured_sample_s` for a day with nowhere to put the answer.
            tracing::warn!(
                model,
                file = %path.display(),
                "measured reading has no catalog row: its yaml went away mid-serve, so the range is neither booked nor persisted"
            );
            self.retire_sampler(model, ticket);
            return;
        };
        // Folded against the SPAWN's range, not the entry's live pair: `read_measured`
        // drops the in-memory copy the moment a stamp stops matching, and a reload
        // that voids it must not silently re-seed an accumulated range from the one
        // process still on the card. An `engine_report` skips the fold: the reserved
        // figure is the engine's whole claim for this launch, static, and not a point
        // to be averaged into noisy witness history.
        let (high, low) = if *fold_range {
            let high = anchor.range.map_or(high, |(h, _)| h.max(high));
            let low = anchor.range.map_or(low, |(_, l)| l.min(low)).min(high);
            (high, low)
        } else {
            (high, low)
        };
        let data = MeasuredData {
            high,
            low,
            offload: anchor.offload,
            attribution: Some(*attribution),
            profile: profile.clone(),
        };
        let entry_same = entry.measured_gb == Some(high)
            && entry.measured_min_gb == Some(low)
            && entry.measured_offload == data.offload
            && entry.measured_attribution == data.attribution
            && entry.measured_profile == data.profile;
        // The row is not the fold's only target: the slot's `container_reported_gb`
        // reads THIS launch's spawn anchor. Row-idempotence alone would hide a
        // lagging anchor whenever a fresh reading equals what an earlier launch
        // persisted — the same engine_report figure across boots is the standing
        // case (card runs 2026-11-02: C2 polled a null `container_reported_gb` for
        // 150 s through a ninfer serve whose 43.8 GiB report matched the stamp).
        let anchor_same =
            anchor.attribution == data.attribution && anchor.range == Some((high, low));
        if entry_same && anchor_same {
            return;
        }
        if !entry_same {
            if let Some(declared) = entry.vram_gb {
                let drift = (high - declared).abs();
                if drift > (declared * 0.25).max(2.0) {
                    tracing::warn!(
                        model,
                        declared,
                        measured = high,
                        "measured footprint diverges from the declared vram_gb"
                    );
                }
            }
        }
        entry.measured_gb = Some(high);
        entry.measured_min_gb = Some(low);
        // The stamps are what this launch actually ran under — minted at spawn
        // through `offload_live` + the engine's truth set, so the void rules keep
        // stamps that describe the process rather than ones rewritten to agree with
        // the yaml that arrived after it.
        entry.measured_offload = data.offload;
        entry.measured_attribution = data.attribution;
        entry.measured_profile = data.profile.clone();
        // The spawn's own accumulator, so the next sample (and any reload in between)
        // still folds against the range and provenance this process produced. A
        // witness fold that follows the container's own report is refused upstream by
        // the anchor's `EngineReport` stamp, so the reserved figure can never be
        // clobbered by a noisy total-diff.
        if let Some(job) = self.jobs.get_mut(&spawned_job) {
            job.measured = Some(MeasuredAnchor {
                range: Some((high, low)),
                attribution: data.attribution,
                ..anchor.clone()
            });
        }
        self.publish();
        if entry_same {
            // The disk already carries exactly these bytes: a rewrite would only
            // churn the file's mtime and the fold's own fingerprint stamp — the
            // promotion above was all that stayed pending.
            return;
        }
        let name = model.to_string();
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            // Every measured key in ONE rewrite: written separately, a failure
            // between them could leave an orphan or inverted pair, or a range booked
            // under the wrong provenance, and the scanner rejects the orphan outright
            // — dropping the whole model (review, 2026-10-27).
            let edits = data.edits();
            let edits: Vec<(&'static str, Option<&str>)> =
                edits.iter().map(|(k, v)| (*k, v.as_deref())).collect();
            match crate::model::writeback::set_flags_on_file(&path, &edits) {
                Ok(()) => {
                    // Axis-2 interlock 5: the fold's own bytes are not a config
                    // change since launch. The fingerprint rides from here —
                    // off-thread, directly after the atomic rename — so the
                    // resident's cached verdict never amber-self over a measurement.
                    if let Some(fingerprint) = crate::model::fingerprint(&path) {
                        let _ = tx.send(ActorMsg::ModelFileWritten {
                            model: name,
                            path,
                            fingerprint,
                        });
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        model = %name,
                        "cannot persist the measured range"
                    );
                    // Warn alone left memory booking a range the file never got
                    // (FINDINGS-13 [4]'s degraded tail): hand the exact refused
                    // figure back to the actor, which drains it from the catalog.
                    // The file rides with it so the drain can only land on the row
                    // this write targeted — never on a same-name twin.
                    let _ = tx.send(ActorMsg::MeasuredPersistFailed {
                        model: name,
                        data,
                        file: Some(path),
                    });
                }
            }
        });
    }

    /// The guards every fold must clear before it can touch the catalog: the engine
    /// this reading claims to describe must still be the one in the slot, and no other
    /// job may be in flight (a swap mid-window would attribute the card's movement to
    /// the wrong model). Shared by the witness and the `engine_report` paths so the two
    /// can never drift apart on who may be believed.
    fn serving_uncontested(&self, model: &str, ticket: &SpawnTicket) -> bool {
        self.slots.iter().any(|slot| {
            slot.occupant
                .as_ref()
                .is_some_and(|o| o.model == model && &o.ticket == ticket)
        }) && !self
            .jobs
            .values()
            .any(|job| !job.terminal && job.model != model)
    }

    /// One fold's figure at the catalog's 0.1 GiB quantum. `None` means the number is
    /// not a measurement at all — nonsense or a sub-quantum wobble, both of which are
    /// absence, and persisting either would write a token the scanner's reader rule
    /// rejects or book a footprint the card never carried.
    pub(super) fn measurable_figure(value: f64, total_gib: f64) -> Option<f64> {
        if !value.is_finite() || value < 0.0 || value > total_gib {
            return None;
        }
        let rounded = (value * 10.0).round() / 10.0;
        (rounded >= 0.05).then_some(rounded)
    }

    /// Folds one serving-window witness reading into the model's measured range and
    /// writes it back to the yaml (§0.4): `measured_gb` keeps the high end — what
    /// admission consults when the provenance lets it book — and `measured_gb_min` the
    /// low end the page shows, plus the stamps that make the figure comparable to a
    /// future launch: the `cpu_offload` setting (the knob table's owners only), and
    /// the `measured_attribution:` provenance this reading really earned (R1/R9).
    /// `own` carrying bytes is the only route to `per_process`; everything the card
    /// could not attribute is stamped `total_diff` — displayed, never booked. A
    /// container spawn can never claim `per_process`: its ticket pid is the dead
    /// `wsl.exe` client (`ninfer.rs`).
    /// Everything it says comes from the spawn's [`MeasuredAnchor`], never from the
    /// live catalog: `reload()` replaces the catalog while the process keeps its argv,
    /// so a stamp read at sample time would describe a launch that never happened and
    /// — because the void rule compares that same stamp with the yaml — disarm the rule
    /// that catches it, and a range read at sample time would be re-seeded from the
    /// one live process. Guards keep the number honest: the sampled engine must still
    /// be the one in the slot; no other job may be in flight; a tenant that appeared
    /// after admit is subtracted, or the driver's own per-process attribution is used
    /// when the card reports it; a figure outside the card is refused, not persisted
    /// (measurement verifies, it never guesses); the spawn must carry an anchor at
    /// all; a spawn whose engine already self-reported refuses witness folds outright;
    /// and the row that owns the reading must still be the file this engine was
    /// launched from — if it has gone, nothing is booked, the reason is named once and
    /// the sampler is retired. A persist the disk refuses reconciles: the figure
    /// drains from memory (Fork C — see [`ActorState::on_measured_persist_failed`]).
    pub(super) fn on_measured(&mut self, model: &str, ticket: &SpawnTicket, reading: Reading) {
        if !self.serving_uncontested(model, ticket) {
            return;
        }
        let Reading {
            total_diff,
            own,
            total_gib,
            used_gib,
        } = reading;
        // One lookup for the spawn this reading belongs to, by the `(model, ticket)`
        // the sampler carries: the same row answers for the tenant subtraction below
        // (`other_need_at_admit`) and for the measured anchor. A reading whose spawn
        // row cannot be found — or was never anchored at spawn — is not folded: the
        // harness cannot say what a launch it cannot identify ran under.
        let Some((at_admit, observed_at_admit, anchor)) = self
            .jobs
            .iter()
            .find(|(_, job)| job.model == model && job.spawned.as_ref() == Some(ticket))
            .map(|(_, job)| {
                (
                    job.other_need_at_admit,
                    job.observed_at_admit,
                    job.measured.clone(),
                )
            })
        else {
            return;
        };
        let Some(anchor) = anchor else { return };
        // The spawn's own verdict on provenance, decided before any arithmetic: a
        // spawn that already folded the engine's own static figure — the
        // container's `engine_report` or a process engine's `engine_status`
        // reading (TASK-ENGINE-MEASUREMENT, ORCHESTRATION §4) — never takes a
        // witness reading into its range again (both figures are static for the
        // launch; the total-diff could only corrupt them).
        if matches!(
            anchor.attribution,
            Some(crate::model::MeasuredAttribution::EngineReport)
                | Some(crate::model::MeasuredAttribution::EngineStatus)
        ) {
            return;
        }
        let booked_others: f64 = self
            .slots
            .iter()
            .filter_map(|slot| slot.occupant.as_ref())
            .filter(|o| o.model != model)
            .map(|o| o.need.0)
            .sum();
        let attributed = own.unwrap_or(total_diff - (booked_others - at_admit).max(0.0));
        let Some(rounded) = Self::measurable_figure(attributed, total_gib) else {
            // No footprint this engine could have — a wedged driver, or a subtraction
            // that overshot — or a sub-quantum wobble. Absence, never a fake number.
            return;
        };
        // The provenance a witness reading may claim (R1 doctrine: metadata only —
        // this never touches the physical gate). Concrete bytes for the engine's own
        // pids are the only route to `per_process`.
        let attribution = match (own.is_some(), anchor.container) {
            (true, false) => crate::model::MeasuredAttribution::PerProcess,
            _ => crate::model::MeasuredAttribution::TotalDiff,
        };
        // R3 ruling 10 (operator GO 2026-11-01): a total-diff is a footprint only
        // while the unaccounted remainder of the card stands still. A static
        // foreign holder is already discounted by the admit baseline; memory that
        // MOVES inside the window is what turned qwen-turbo's range into the
        // 23.8–59.3 noise (blanked by the operator's ruling 2026-11-01). When the
        // unaccounted movement exceeds what this engine's own growth explains,
        // the fold is someone else's story and nothing is minted — a labelled
        // blank beats a stand-in lie. A witness that never answered at admit has
        // no baseline to move from, and keeps the pre-R3 behavior.
        if attribution == crate::model::MeasuredAttribution::TotalDiff {
            if let Some(free_at_admit) = observed_at_admit {
                let foreign_at_admit = (total_gib - free_at_admit - at_admit).max(0.0);
                let foreign_now = (used_gib - (attributed + booked_others).max(0.0)).max(0.0);
                let grade = crate::budget::grade_card_occupancy(
                    foreign_now - foreign_at_admit,
                    attributed,
                    self.cfg.witness_tolerance_gb,
                );
                if grade == crate::budget::CardOccupancy::ForeignDominated {
                    tracing::warn!(
                        model,
                        used_gib,
                        attributed,
                        booked_others,
                        foreign_at_admit,
                        foreign_now,
                        "measured total-diff skipped: unaccounted memory moved more than the engine grew; no range minted from someone else's story"
                    );
                    return;
                }
            }
        }
        // R11 session cross-check (rule 2026-11-01, display-only): a Fit-booked
        // load gets its projection confirmed by THIS reading — it reached here
        // only by surviving every fold gate, including the `ForeignDominated`
        // grade above, so a contaminated window can never stamp it green.
        if self.maybe_validate_fit(model, &anchor, rounded) {
            self.publish();
        }
        self.fold(&FoldPlan {
            model: model.to_string(),
            ticket: ticket.clone(),
            high: rounded,
            low: rounded,
            attribution,
            // The profile stamp belongs to the `engine_report` figure alone (its KV
            // is what the profile's --kv-capacity bound); a witness range carries
            // none, so the void rule never misjudges a total-diff on a profile move.
            profile: (attribution == crate::model::MeasuredAttribution::EngineReport)
                .then(|| anchor.profile.clone())
                .flatten(),
            fold_range: true,
        });
    }

    /// Folds the container's one-shot startup `MemorySummary` figure (R9): the
    /// engine's own reserved footprint — weights + KV at *declared* capacity +
    /// graphs + reservations + slack — as computed by the engine from its own
    /// `cudaMemGetInfo`, with no cross-tenant noise. It books (ruling 15 (ii)),
    /// bounded: declared `vram_gb` still wins, the physical gate stays `used_gib`,
    /// and `exact` rows are verified against it rather than estimated. Both ends of
    /// the pair are the reserved figure: `measured_gb_min` never understates the
    /// declared KV, and the figure is static for the life of the launch.
    pub(super) fn on_engine_report(
        &mut self,
        model: &str,
        ticket: &SpawnTicket,
        required_to_free_gb: f64,
        total_gib: f64,
    ) {
        if !self.serving_uncontested(model, ticket) {
            // The one-shot does not retry — the sampler ends when it sends — so a
            // drop here must be visible (review 2026-10-31, M1: the last silent
            // consumption of this figure rode the witness baseline).
            tracing::warn!(
                model,
                "engine report dropped: another spawn is in flight and this reading lost its uncontested window"
            );
            return;
        }
        // The parser already clamped to `[0, total]`; a claimed reservation of zero is
        // nonsense, refused by the same rule the witness path applies — nonsense never
        // books, and never persists.
        let Some(rounded) = Self::measurable_figure(required_to_free_gb, total_gib) else {
            tracing::warn!(
                model,
                required_to_free_gb,
                total_gib,
                "engine report refused: the figure is nonsense or sub-quantum for this card"
            );
            return;
        };
        let anchor = self
            .jobs
            .iter()
            .find(|(_, job)| job.model == model && job.spawned.as_ref() == Some(ticket))
            .and_then(|(_, job)| job.measured.clone());
        // Ruling 15 demands every booked `engine_report` be launch-verifiable
        // against a resolved profile. Since the ruling-18b legacy removal every
        // ninfer spawn resolved its profile through `profile_override` (a row
        // whose profile does not resolve never spawns), so the spawn anchor
        // carries the certifying identity the fold below persists with.
        let profile = anchor.as_ref().and_then(|a| a.profile.clone());
        self.fold(&FoldPlan {
            model: model.to_string(),
            ticket: ticket.clone(),
            high: rounded,
            low: rounded,
            attribution: crate::model::MeasuredAttribution::EngineReport,
            profile,
            // Static for the life of the launch: never averaged into witness history.
            fold_range: false,
        });
    }

    /// Folds one process-engine measurement (`TASK-ENGINE-MEASUREMENT`,
    /// ORCHESTRATION §4): the footprint basalt/strata stated at their own status
    /// endpoint, or the witness's process-tree sum of the engine's pid plus its
    /// direct children. Same guard order as [`Self::on_engine_report`] — the
    /// reading must still own an uncontested serving window (a drop is visible
    /// because the sampler's one-shot does not retry; review 2026-10-31, M1),
    /// and the figure must be one this card could carry. The two sources keep
    /// their own provenance and never upgrade each other (doctrine 12): the
    /// engine's status answer folds under `EngineStatus` with
    /// `EngineReport`-class trust — above the declared floor as max truth per
    /// the 2026-11-04 ruling — and the tree sum folds as the existing
    /// `PerProcess` footprint. Neither averages into witness history
    /// (`fold_range: false`): each is this launch's footprint, not a noisy
    /// sample. Neither mints a profile stamp: the profile identity is ninfer's
    /// alone (ORCHESTRATION §2). Booking then flows unchanged through
    /// `cost_source()`'s max truth.
    pub(super) fn on_process_measurement(
        &mut self,
        model: &str,
        ticket: &SpawnTicket,
        measurement: crate::engine::EngineMeasurement,
        total_gib: f64,
    ) {
        if !self.serving_uncontested(model, ticket) {
            tracing::warn!(
                model,
                "process measurement dropped: another spawn is in flight and this reading lost its uncontested window"
            );
            return;
        }
        let Some(rounded) = Self::measurable_figure(measurement.gpu_gib, total_gib) else {
            tracing::warn!(
                model,
                gpu_gib = measurement.gpu_gib,
                total_gib,
                "process measurement refused: the figure is nonsense or sub-quantum for this card"
            );
            return;
        };
        let attribution = match measurement.source {
            crate::engine::MeasurementSource::EngineStatus => {
                crate::model::MeasuredAttribution::EngineStatus
            }
            crate::engine::MeasurementSource::ProcessTreeNvml => {
                crate::model::MeasuredAttribution::PerProcess
            }
        };
        self.fold(&FoldPlan {
            model: model.to_string(),
            ticket: ticket.clone(),
            high: rounded,
            low: rounded,
            attribution,
            profile: None,
            // A footprint for this launch, not a point to be averaged into
            // this spawn's witness history.
            fold_range: false,
        });
        // Phase 4 (`TASK-ENGINE-MEASUREMENT`, the 2026-11-04 readmission
        // incident): the fold moved the row's max truth, but the resident's
        // booking lives on the SLOT — the same-slot release credit, the R3
        // no-release floor guard and the eviction planner all read that
        // figure, and an occupant left holding the paper number hands a
        // same-slot replacement the stale credit (Strata booked 55, held
        // 70.4; Basalt refused before release-first). Refresh the booking in
        // this same turn through the SAME single decision a fresh load takes
        // ([`ActorState::booking_need`] over the row the fold just updated),
        // so `max(measured, declared floor)` can never drift from what the
        // gate would book. This stays doctrine 12: the seam receives only
        // the engine's own word — `engine_status` or the `per_process` tree
        // sum — never a `total_diff`. A greedy occupant keeps its card-driven
        // clamped booking (a post-spill footprint is not a nominal size, the
        // same rule `may_book_measured` applies to the gate).
        if self.rebook_after_measurement(model, ticket) {
            self.publish();
        }
    }

    /// Refreshes the slot occupant's booked `need` from the row its process
    /// measurement just folded onto, by the one booking decision
    /// ([`ActorState::booking_need`]) — the doctrine lives on
    /// [`ActorState::on_process_measurement`]. `true` only when the booking
    /// actually moved (the caller publishes). The row is found by the
    /// spawn-era FILE, the same identity the fold books: a row that went away
    /// re-books nothing, and a re-minted twin never re-books a stranger's
    /// occupant because the `(model, ticket)` pair must match the slot.
    /// The resident-side refusals are the rescan seam's ([`Self::rebook_cost_only_moves`]),
    /// stated plainly: an engine launched greedy keeps its card-driven clamped
    /// booking — the SPAWN-TIME class the job carries, not a row an operator
    /// may have flipped since — and an amber resident's row no longer
    /// describes the process, so a measurement of the process may not re-book
    /// against it either.
    fn rebook_after_measurement(&mut self, model: &str, ticket: &SpawnTicket) -> bool {
        let Some(slot) = self.slots.iter().position(|slot| {
            slot.occupant
                .as_ref()
                .is_some_and(|o| o.model == model && &o.ticket == ticket)
        }) else {
            return false;
        };
        let Some((path, spawn_greedy)) = self
            .jobs
            .iter()
            .find(|(_, job)| job.model == model && job.spawned.as_ref() == Some(ticket))
            .map(|(_, job)| {
                (
                    job.measured.as_ref().and_then(|a| a.file.clone()),
                    job.cost == crate::model::CostClass::Greedy,
                )
            })
        else {
            return false;
        };
        // Greedy launched card-driven (its admit-time clamp), never
        // measurement-driven — the same rule `may_book_measured` applies to
        // the gate, on the class the launch actually wore.
        if spawn_greedy {
            return false;
        }
        // An amber the rescan earned says the row and the process diverged:
        // the fold still records what the process holds, but the booking
        // gesture is unload + reload, never a re-read of a stale row.
        if self.slots[slot]
            .occupant
            .as_ref()
            .is_some_and(|o| o.stale_config)
        {
            return false;
        }
        let Some(path) = path else {
            return false;
        };
        let Some(entry) = self
            .catalog
            .iter()
            .find(|m| m.file.as_deref() == Some(path.as_path()))
        else {
            return false;
        };
        if entry.cost == crate::model::CostClass::Greedy {
            return false;
        }
        // A row that cannot answer the gate's question (a stripped `exact`)
        // books nothing new — the fold's own refusal stands, unchanged.
        let Ok(Some(need)) = Self::booking_need(entry) else {
            return false;
        };
        let Some(occupant) = self.slots[slot].occupant.as_mut() else {
            return false;
        };
        if occupant.need == need {
            return false;
        }
        tracing::info!(
            model,
            booked_gib = need.0,
            was_gib = occupant.need.0,
            "measured footprint re-booked the resident's slot need (max of the engine's own word and the declared floor)"
        );
        occupant.need = need;
        true
    }

    /// Reconciles a refused measured persist (Fork C): the in-memory measured pair
    /// and the provenance stamps written with it are drained to their none state so
    /// the page and admission never charge a footprint the disk refused. The drain
    /// only lands when the entry still holds the exact values that failed — a
    /// superseding reading (whose own persist will carry its own reconcile if it too
    /// fails) is left untouched. Draining the stamps with the pair is what keeps the
    /// grandfathering rule honest: a `measured_gb` with no stamp books, so a row the
    /// disk refused must not keep a stamp claiming a provenance it never got.
    ///
    /// The row is found by the same key the fold used, the spawn-era FILE
    /// (FINDINGS2-04 [1]): keying this leg `name && file` while the fold is keyed
    /// file-only made the name clause the single divergence point — a reload that
    /// renames the yaml leaves the fold booking the file's row and this drain missing
    /// it, so memory keeps a triple the disk refused for the whole serve window
    /// (subsequent samples reproduce the same high/low against the updated anchor
    /// range, `changed` stays false, and no retry persist ever fires). `model` is the
    /// fallback only for the hand-built no-file messages; production persist failures
    /// always carry the file (the fold's own `anchor_file`).
    pub(super) fn on_measured_persist_failed(
        &mut self,
        model: &str,
        data: &MeasuredData,
        file: Option<PathBuf>,
    ) {
        let Some(entry) = self.catalog.iter_mut().find(|m| {
            file.as_deref()
                .map_or(m.name == model, |path| m.file.as_deref() == Some(path))
        }) else {
            return;
        };
        if entry.measured_gb == Some(data.high)
            && entry.measured_min_gb == Some(data.low)
            && entry.measured_offload == data.offload
            && entry.measured_attribution == data.attribution
            && entry.measured_profile == data.profile
        {
            entry.measured_gb = None;
            entry.measured_min_gb = None;
            entry.measured_offload = None;
            entry.measured_attribution = None;
            entry.measured_profile = None;
            self.publish();
        }
    }
}
