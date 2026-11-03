//! Actor-side load registration and booking: the ordinary load path, the
//! ledger-row registration, the booking-need decision, and the admit arming
//! path. The state and `finish_admit` stay owned by `mod.rs`.

use crate::budget::{self, AdmissionError, Gibibytes};
use crate::model::ModelEntry;
use std::sync::Arc;
use std::time::Instant;

use super::{
    ActorMsg, ActorState, Event, Job, JobId, JobKind, JobTimings, SchedulerError, SwapState,
    Victim, Waiter,
};

impl ActorState {
    /// The ordinary load path: register the row and arm it in one step.
    pub(super) fn begin_load(&mut self, slot: usize, entry: ModelEntry, waiters: Vec<Waiter>) {
        let job_id = self.register_queued_load(slot, &entry, waiters);
        self.start_queued_load(job_id, slot, entry);
    }

    /// Creates the ledger row of a load that has not started yet — `kind: Load`,
    /// non-terminal, state `Idle`, holding its waiters. Every load begins here, so a
    /// parked one is already on the ledger while its caller waits, instead of being a
    /// request with no row and no answer (FINDINGS-02 [3]).
    ///
    /// # Arguments
    ///
    /// * `slot` - the slot the load is queued on (its `pending` place).
    /// * `entry` - the catalog entry, whose keys pre-fill the booking fields.
    /// * `waiters` - the callers attached to this load (F-6: duplicates share one row).
    ///
    /// # Returns
    ///
    /// The id of the row just registered, which is what `pending` stores.
    pub(super) fn register_queued_load(
        &mut self,
        slot: usize,
        entry: &ModelEntry,
        waiters: Vec<Waiter>,
    ) -> JobId {
        let job_id = JobId(self.next_job);
        self.next_job += 1;
        let job = Job {
            kind: JobKind::Load,
            model: entry.name.clone(),
            engine: entry.engine.clone(),
            category: entry.category,
            state: SwapState::Idle,
            terminal: false,
            outcome: None,
            error: None,
            need: None,
            free_at_admit: None,
            observed_at_admit: None,
            release_first: false,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters,
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            measured: None,
            victim: None,
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            holder_released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: None,
            warnings: Vec::new(),
            slot: Some(slot),
            cost: entry.cost,
            launch_floor: entry.launch_floor_gb,
            load_timeout_s: entry.load_timeout_s,
            gate_target: None,
            // The yaml as the catalog saw it, statted inline once per
            // registration on the park path: `apply_catalog` itself is now
            // zero-fs (FINDINGS-07 [8]), so this is the reload flow's one
            // surviving inline stat. Accepted as-is (FINDINGS-12-P): it rides
            // a file the just-applied scan already read, moves no engine and
            // no volume, and an offload would add a second read or couple
            // queue registration to scan internals for one `metadata` call.
            queue_fingerprint: entry.file.as_deref().and_then(crate::model::fingerprint),
        };
        self.jobs.insert(job_id, job);
        self.set_timing(job_id, |t| &mut t.queue_ms);
        job_id
    }

    /// One decision (`ModelEntry::cost_source`) drives the gate; the page's Cost
    /// column reads the same one, so the two cannot drift. The figure follows
    /// the decision, not the key that happens to exist: a Measured source books
    /// the measured figure - including the guarded engine_report that OVERSHADOWS
    /// a smaller declared `vram_gb` (operator ruling 2026-11-01: reserve what the
    /// container itself reserves), and so does a Fit projection (R11, the same
    /// max truth). Greedy keeps nominal semantics: a post-spill measurement is
    /// not a nominal size and must not shrink the booking (review, 2026-10-26);
    /// `may_book_measured` keeps it off Measured anyway — but a greedy row with
    /// a fit figure floors its booking at what the engine says it will reserve:
    /// max truth is not the measured pair's monopoly, and a projection is not a
    /// post-spill size that could wrongly SHRINK the booking. `Ok(None)` is the
    /// legal unknown-size greedy load (an unknown nominal is allowed because
    /// booking uses the room at admit).
    pub(super) fn booking_need(entry: &ModelEntry) -> Result<Option<Gibibytes>, AdmissionError> {
        let need = match entry.cost_source() {
            // `exact` is a precision claim: the engine's number, not an estimate.
            // No `vram_gb`, no load — the size×1.15+2 fallback would be a guess.
            crate::model::CostSource::Undeclared => Err(AdmissionError::ExactUndeclared),
            crate::model::CostSource::Measured => budget::need_gb(
                entry.measured_gb.map(Gibibytes),
                entry.gguf_size_gb.map(Gibibytes),
            ),
            crate::model::CostSource::Fit => budget::need_gb(
                entry.fit_gb.map(Gibibytes),
                entry.gguf_size_gb.map(Gibibytes),
            ),
            crate::model::CostSource::Declared | crate::model::CostSource::Estimate => {
                budget::need_gb(
                    entry.vram_gb.map(Gibibytes),
                    entry.gguf_size_gb.map(Gibibytes),
                )
            }
        };
        let fit_floor = (entry.cost == crate::model::CostClass::Greedy
            && crate::engine::policy::fit_capable(&entry.engine))
        .then(|| entry.fit_gb.map(Gibibytes))
        .flatten();
        match need {
            Ok(need) => Ok(Some(match fit_floor {
                Some(floor) if floor.0 > need.0 => floor,
                _ => need,
            })),
            Err(_) if entry.cost == crate::model::CostClass::Greedy => Ok(fit_floor),
            Err(error) => Err(error),
        }
    }

    /// Arms a registered load row: takes over its slot, books the need, records the
    /// occupant it must release, and starts the witness sample that drives `Admit`.
    /// Arming clears any unload still queued behind a cancelled swap on that slot
    /// (FINDINGS-02 [2]): a load the operator asked for after pressing Unload is the
    /// later instruction.
    pub(super) fn start_queued_load(&mut self, job_id: JobId, slot: usize, mut entry: ModelEntry) {
        self.unload_after_finish.remove(&slot);
        // R11: the row's live fit figure rides the booking decision — cache
        // hit books the projection (through `cost_source`), a miss books the
        // ladder now and fires the probe fire-and-forget (never parking the
        // admit behind a volume). The version epoch rides every llama admit.
        self.refresh_fit_on_admit(&mut entry);
        // R10 separation (operator 2026-11-01: "8003-8005 are manual only"): a
        // container's internal port is a third namespace; a row claiming a manual
        // or harness number is refused BEFORE spawn, whatever docker would have
        // done with it. `apply_catalog` reports the same verdict at rescan.
        if let Some(port) = crate::engine::ninfer::declared_internal_port(&entry) {
            if let Some(claim) = self.cfg.port_claim(port) {
                let error = SchedulerError::Admission(AdmissionError::PortClaimed { port, claim });
                self.slots[slot].job = Some(job_id);
                self.step(job_id, SwapState::Admit);
                self.step(job_id, SwapState::AdmitFail);
                self.log_refusal(job_id, &error, "config");
                self.finish(job_id, Err(error), "port claimed");
                return;
            }
        }
        // The gate's figure, by the one decision the page also reads.
        let booking = match Self::booking_need(&entry) {
            Ok(booking) => booking,
            Err(error) => {
                let error = SchedulerError::Admission(error);
                self.slots[slot].job = Some(job_id);
                self.step(job_id, SwapState::Admit);
                self.step(job_id, SwapState::AdmitFail);
                self.log_refusal(job_id, &error, "need");
                self.finish(job_id, Err(error), "under-specified");
                return;
            }
        };
        let victim = self.slots[slot].occupant.as_ref().map(|o| Victim {
            model: o.model.clone(),
            ticket: o.ticket.clone(),
            slot,
            evicted: false,
            cost: o.need.0,
        });
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.need = booking;
            job.victim = victim;
        }
        self.slots[slot].job = Some(job_id);
        self.step(job_id, SwapState::Admit);
        // Step 4 (as amended 2026-10-26): the gate is evaluated against the card's own
        // reading, sampled on a task — never inline in the actor, which must not stall
        // behind a witness call.
        let witness = Arc::clone(&self.witness);
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Admitted(witness.sample().await),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }
}
