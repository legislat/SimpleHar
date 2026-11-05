//! Status snapshot types for `/har/status` and the SSE document.
//!
//! These are pure data types. They never probe and never mutate. The actor owns
//! the values; this module only defines the shape.

use crate::config::CategoryId;
use crate::model::scan::ScanError;
use serde::Serialize;
use std::collections::BTreeSet;
use std::time::Instant;

use super::{fault_of, jobs::JobStatus, occupant::state_last_used, ActorState, JobId, Slot};

/// Immutable status snapshot (Step 2: reads never probe and never mutate).
#[derive(Debug, Clone, Serialize)]
pub struct SchedulerStatus {
    /// Every materialized slot (AUX expands to `max_models` entries).
    pub slots: Vec<SlotStatus>,
    /// Job ledger, oldest first.
    pub jobs: Vec<JobStatus>,
    /// Catalog problems from the last scan (bad files, unresolved defaults).
    pub catalog_errors: Vec<ScanError>,
    /// R5 report-only reconcile (ruling 9): running containers matching a catalog
    /// `ninfer_container` row while the claim ledger holds no claim — the SAME set
    /// the reconcile warned about, carried on this one snapshot so the page and the
    /// log can never disagree. Absent from `/har/status` until a probe reports a
    /// non-empty set (an empty last-reported set normalizes to `None` at the single
    /// snapshot accessor — the skip predicate's polarity stays `is_none`, the
    /// GOTCHAS inverted-predicate class); a failed probe keeps the last-reported
    /// set, exactly like the warn dedupe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stranded_containers: Option<BTreeSet<String>>,
    /// R11 session cross-check (rule 2026-11-01, display-only): the models whose
    /// Fit booking a usable witness reading confirmed inside the session
    /// tolerance while the current fit epoch stands — the page turns those rows
    /// green. In-memory actor stamps filtered to the epoch, never persisted and
    /// never a gate input; a rescan (epoch moves) or a calculator swap (version
    /// invalidation) empties it. Skip-serialized because the verdict rides the
    /// per-model rows (`/har/models` and the SSE document carry it), never as a
    /// top-level list of `/har/status`.
    #[serde(skip)]
    pub fit_validated: BTreeSet<String>,
}

/// One slot as seen by `/har/status` (Step 12).
#[derive(Debug, Clone, Serialize)]
pub struct SlotStatus {
    /// Category of the slot.
    pub category: CategoryId,
    /// Index within its category (0-based).
    pub index: usize,
    /// Serving model, if any.
    pub model: Option<String>,
    /// Engine port, if occupied.
    pub port: Option<u16>,
    /// Child pid of the occupying engine (0 when the runtime reports none).
    pub pid: Option<u32>,
    /// Admission cost of the occupant in GiB.
    pub need_gib: Option<f64>,
    /// Seconds since the occupant was last used (None when empty; Step 5).
    pub idle_s: Option<u64>,
    /// The engine's self-reported id, when discovered (F-4).
    pub serve_id: Option<String>,
    /// Job currently running for this slot, if any.
    pub loading_job: Option<JobId>,
    /// The slot lifecycle the server derives: `empty`, `loading`, `serving`,
    /// or `releasing` (operator ruling 2026-11-04). `releasing` names a
    /// committed release: the occupant is the target of a live unload or
    /// eviction and stays installed until the driver confirms the release, so
    /// the label covers the whole window from accept to confirmation. A
    /// release that timed out is terminal: the occupant stays on purpose and
    /// the slot reads `serving`, never a stuck `releasing`. The page renders
    /// this value instead of re-deriving the dot.
    pub state: String,
    /// Wall-clock launch of the occupant, server-formatted ISO-8601 UTC
    /// (Axis-2 ruling 2026-10-29: the operator sees when the model launched; the
    /// page prints this string verbatim — no date math in the browser). Empty slot:
    /// absent.
    pub launched_at: Option<String>,
    /// The server-computed `stale_config` verdict for the occupant (Axis 2 = B):
    /// `true` when the model's yaml differs — by the install-time fingerprint —
    /// from the bytes this engine launched from. Refreshed only at install, on the
    /// reload turn, and on harness write-backs; reading it performs no stat.
    /// Empty slot: absent.
    pub stale_config: Option<bool>,
    /// What this container occupant's own startup `MemorySummary` said it reserves
    /// (R9), straight from its spawn anchor — `None` for a child-process spawn, a
    /// container that never reported, or a report the anchor refused. Carried on
    /// the snapshot so BOTH `/har/status` and SSE quote the same number (review
    /// 2026-10-31: threading it only through the SSE build made the memory-bar
    /// annotation unreachable for a token-gated, poll-only page); display only,
    /// never a gate input.
    pub container_reported_gb: Option<f64>,
    /// The host-RAM booking this CPU-resident occupant debits (GiB), straight
    /// from the actor's `ram_need` — the ledger truth, not a re-derivation
    /// (W3, operator ruling 2026-11-05). Emitted only when the catalog row's
    /// ladder figure still agrees with that booking (W3-01): a row that
    /// vanished or moved its rung under the resident proves nothing, and all
    /// three RAM fields then stay absent together — never re-labeled, never
    /// guessed. **Absent on every GPU row and every empty slot**
    /// (`skip_serializing_if`), so their wire shape is exactly what it was
    /// before the RAM surface existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub booked_ram_gb: Option<f64>,
    /// Which rung of the RAM ladder produced this resident's booking:
    /// `declared`, `measured`, or `estimate` — the same vocabulary
    /// `/har/models` `cost_basis` uses, answered by the entry's one ladder
    /// derivation (`ModelEntry::ram_booking`). Present exactly when
    /// `booked_ram_gb` is present: the catalog row still names the resident
    /// and its ladder figure equals the ledger booking (absence is "no
    /// claim", never a re-label of a stale figure).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_cost_basis: Option<&'static str>,
    /// Whose number the booked RAM is: `per_process` when the measured
    /// working set booked (the engine's own process-tree sum — the only
    /// attribution a RAM figure can hold), `declared` when the floor or the
    /// estimate booked. Same presence rule as `ram_cost_basis`: the three
    /// RAM fields stand together or stay absent together. Absent on every
    /// GPU row.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_source: Option<&'static str>,
    /// When the occupant was loaded (copy of the actor's `Occupant::loaded_at`).
    /// Empty slots carry the snapshot time, mirroring the `map_or(Instant::now(),
    /// |o| o.loaded_at)` policy of the scheduler's own LRU picks. Not serialized:
    /// an `Instant` has no wall-clock meaning outside the process, so `/har/status`
    /// keeps exposing `idle_s`. The no-`model` proxy fallback sorts untouched
    /// occupants by this (FINDINGS-06 \[1]: never fabricate an instant instead).
    #[serde(skip)]
    pub loaded_at: Instant,
}

impl ActorState {
    /// What the occupant's own container reported it reserves (R9), read off the
    /// spawn anchor of the job that produced this exact occupant (`model` + `ticket`
    /// — never the model name alone, which a re-minted twin can steal): Some only
    /// for a ninfer container spawn whose anchor folded an `engine_report` figure.
    /// The API's memory-bar sum derives from this so `/har/status` and the SSE push
    /// quote ONE source (review 2026-10-31: threading the sum only through the SSE
    /// build left the poll path — the only path a token-gated page has — forever
    /// null, and a fixture carrying the value hid it).
    /// Whether the catalog row's RAM ladder figure still describes this
    /// occupant's ledger booking (W3-01). Equal means equal within the
    /// project's GiB epsilon — the same band that decides when two figures
    /// are worth naming apart (`budget::DISAGREE_GIB`). NaN never agrees: a
    /// NaN difference fails the band, so a garbage figure silences the
    /// labels instead of lending them credibility.
    fn ram_figures_agree(ladder: f64, booked: f64) -> bool {
        (ladder - booked).abs() < crate::budget::DISAGREE_GIB
    }

    fn container_reported_gb(&self, slot: &Slot) -> Option<f64> {
        let occupant = slot.occupant.as_ref()?;
        let job = self.jobs.iter().find_map(|(_, job)| {
            (job.model == occupant.model && job.spawned.as_ref() == Some(&occupant.ticket))
                .then_some(job)
        })?;
        let anchor = job.measured.as_ref()?;
        (job.engine == crate::ownership::NINFER_ENGINE
            && anchor.container
            && anchor.attribution == Some(crate::model::MeasuredAttribution::EngineReport))
        .then(|| anchor.range.map(|(high, _)| high))
        .flatten()
    }

    pub(super) fn snapshot(&self) -> SchedulerStatus {
        let now = Instant::now();
        SchedulerStatus {
            catalog_errors: self.catalog_errors.clone(),
            // One accessor derives the payload from the dedupe state (empty means
            // absent, never an empty list that would keep the key alive).
            stranded_containers: self
                .last_reported_stranded
                .as_ref()
                .filter(|names| !names.is_empty())
                .cloned(),
            // R11: only stamps earned under the CURRENT epoch answer — the page
            // must never show a green carried across a rescan, and reading this
            // is pure memory (no fs, no clock), like every other snapshot field.
            fit_validated: self
                .fit_stamps
                .iter()
                .filter(|(_, (epoch, ok))| *ok && *epoch == self.fit_gen())
                .map(|(model, _)| model.clone())
                .collect(),
            slots: self
                .slots
                .iter()
                .map(|s| {
                    // The RAM booking story (W3, W3-01 ruling): the figure is
                    // the actor's own `ram_need` (ledger truth) and the labels
                    // are the catalog row's one ladder derivation, joined in
                    // pure memory — the same snapshot-purity rule every other
                    // field obeys. But the ladder only speaks for this
                    // occupant while its figure agrees with the ledger
                    // booking. Legal transitions move the row's rung off a
                    // resident the gate booked differently (a cost-only pair
                    // edit under a loaded row; a same-model twin whose fold
                    // re-booked only the other slot), and re-labelling the
                    // stale figure would print a provenance the gate never
                    // took for it (DOCTRINE 12). A row that vanished or no
                    // longer agrees therefore omits all three fields:
                    // absence is "no claim", never a re-label.
                    let ram = s.occupant.as_ref().and_then(|o| {
                        let booked = o.ram_need?;
                        let (_, basis) = self
                            .catalog
                            .iter()
                            .find(|m| m.name == o.model)
                            .and_then(|entry| entry.ram_booking())
                            .filter(|(figure, _)| Self::ram_figures_agree(*figure, booked.0))?;
                        Some((booked.0, basis))
                    });
                    SlotStatus {
                        category: s.category,
                        index: s.index,
                        model: s.occupant.as_ref().map(|o| o.model.clone()),
                        port: s.occupant.as_ref().map(|o| o.port),
                        pid: s.occupant.as_ref().map(|o| o.ticket.pid),
                        need_gib: s.occupant.as_ref().map(|o| o.need.0),
                        idle_s: s.occupant.as_ref().map(|o| {
                            let last = state_last_used(o, &self.last_used);
                            now.duration_since(last).as_secs()
                        }),
                        serve_id: s.occupant.as_ref().and_then(|o| o.serve_id.clone()),
                        loading_job: s.job,
                        // The one derivation of the slot label (see
                        // `ActorState::slot_state`): a committed release reads
                        // `releasing` until the driver confirms it.
                        state: self.slot_state(s).to_string(),
                        launched_at: s
                            .occupant
                            .as_ref()
                            .map(|o| crate::model::iso8601_utc(o.launched_at_wall)),
                        stale_config: s.occupant.as_ref().map(|o| o.stale_config),
                        container_reported_gb: self.container_reported_gb(s),
                        booked_ram_gb: ram.map(|(figure, _)| figure),
                        ram_cost_basis: ram.map(|(_, basis)| basis.as_str()),
                        ram_source: ram.map(|(_, basis)| basis.source()),
                        loaded_at: s.occupant.as_ref().map_or(now, |o| o.loaded_at),
                    }
                })
                .collect(),
            jobs: self
                .jobs
                .iter()
                .map(|(id, j)| JobStatus {
                    id: *id,
                    kind: j.kind,
                    path: j.path,
                    model: j.model.clone(),
                    category: j.category,
                    state: j.state,
                    terminal: j.terminal,
                    outcome: j.outcome.clone(),
                    error: j.error.as_ref().map(|e| e.to_string()),
                    need_gib: j.need.map(|n| n.0),
                    free_at_admit_gib: j.free_at_admit.map(|n| n.0),
                    observed_at_admit_gib: j.observed_at_admit,
                    gate_target_gib: j.gate_target,
                    victims_released: j.victims_released.clone(),
                    warnings: j.warnings.clone(),
                    is_fault: fault_of(j.terminal, j.kind, j.outcome.as_deref(), j.error.is_some()),
                    timings: j.timings,
                })
                .collect(),
        }
    }
}
