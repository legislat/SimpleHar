//! Actor-side load registration, booking, and admission: the ordinary load path,
//! the ledger-row registration, the booking-need decision, the admit arming and
//! decision paths, and the victim planner. The state stays owned by `mod.rs`.

use crate::budget::{self, AdmissionError, BudgetFacts, Gibibytes, WaitHint};
use crate::config::CategoryId;
use crate::model::ModelEntry;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    ActorMsg, ActorState, AdmissionPath, Event, Job, JobId, JobKind, JobTimings, SchedulerError,
    SwapState, Victim, Waiter,
};

/// How many recent confirmed releases the wait-estimate ring keeps. Bounded by
/// design (same posture as the log ring): the median answers a refusal, and 32
/// confirmed releases describe recent release behavior as well as a longer
/// window without letting one old swap dominate.
pub(super) const RELEASE_SAMPLE_CAP: usize = 32;

/// Rolling ring of the harness's own measured release durations, in seconds
/// (accept to driver-confirmed). Memory-only by ruling: nothing is persisted,
/// so a fresh process has no sample and quotes no estimate — the honest
/// starting state, not a missing cache (operator ruling 2026-11-04).
pub(super) struct ReleaseSamples {
    pub(super) seconds: std::collections::VecDeque<f64>,
}

impl ReleaseSamples {
    pub(super) fn new() -> Self {
        Self {
            seconds: std::collections::VecDeque::with_capacity(RELEASE_SAMPLE_CAP),
        }
    }

    /// Appends one confirmed release, dropping the oldest sample past the cap.
    pub(super) fn push(&mut self, elapsed: Duration) {
        if self.seconds.len() == RELEASE_SAMPLE_CAP {
            self.seconds.pop_front();
        }
        self.seconds.push_back(elapsed.as_secs_f64());
    }

    /// The median release duration in seconds. Odd count: the middle value.
    /// Even count: the mean of the two middle values (the exact-even rule —
    /// it never invents a sample that was not measured). `None` on an empty
    /// ring: with no confirmed release there is no basis to quote an estimate.
    pub(super) fn median_s(&self) -> Option<f64> {
        if self.seconds.is_empty() {
            return None;
        }
        let mut sorted: Vec<f64> = self.seconds.iter().copied().collect();
        sorted.sort_by(f64::total_cmp);
        let mid = sorted.len() / 2;
        Some(if sorted.len() % 2 == 1 {
            sorted[mid]
        } else {
            (sorted[mid - 1] + sorted[mid]) / 2.0
        })
    }
}

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
            // The rung arrives with the gate decision in `finish_admit`.
            path: None,
            model: entry.name.clone(),
            engine: entry.engine.clone(),
            category: entry.category,
            state: SwapState::Idle,
            terminal: false,
            outcome: None,
            error: None,
            need: None,
            resource: entry.resource(),
            ram_need: None,
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
            vram_gate: entry.vram_gate,
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

    /// The CPU-resident booking ladder (operator ruling 2026-11-05): the
    /// floor first — declared `ram_gb`, else the GGUF-size estimate
    /// `gguf_size_gb * 1.0 + 2.0` GiB, the weights page in, not the card's
    /// `x 1.15 + 2` KV multiplier, because an offloaded row holds the file
    /// and its working set, not VRAM-pinned KV — then the max truth (DOCTRINE
    /// 12 amended ruling): the trusted measured working set books when it
    /// exceeds the floor, and the declared floor is never undercut. The pair
    /// can only carry `per_process` tree truth here: the `cpu_offload` flip
    /// voids it at scan (`read_measured_ram`) and the fold refuses a
    /// `total_diff` reading before it can persist (`on_measured_ram`). The
    /// VRAM cost ladder is never consulted here — a CPU row's `vram_gb`
    /// changes no RAM figure, and a `ram_gb` changes no VRAM figure. No basis
    /// at all (no `ram_gb`, no GGUF size, no trusted pair) is under-specified:
    /// the gate refuses rather than guess the host's footprint. The ladder
    /// itself is the entry's own one derivation ([`ModelEntry::ram_booking`]),
    /// shared with the `/har/models` and `/har/status` surfaces so a page can
    /// never display a basis the gate did not take.
    pub(super) fn ram_booking(entry: &ModelEntry) -> Result<Gibibytes, AdmissionError> {
        entry
            .ram_booking()
            .map(|(need, _)| Gibibytes(need))
            .ok_or(AdmissionError::UnderSpecified)
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
        // A CPU-resident row is skipped: the VRAM calculator answers a
        // question this row's gate never asks (operator ruling 2026-11-05).
        if entry.resource() == crate::model::Resource::Gpu {
            self.refresh_fit_on_admit(&mut entry);
        }
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
        // The gate's figure, by the one decision the page also reads. The
        // two ladders answer separately (operator ruling 2026-11-05): a
        // CPU-resident row books host RAM and zero VRAM; a VRAM row books
        // exactly as before and holds no RAM figure.
        let (booking, ram_booking) = if entry.resource() == crate::model::Resource::Ram {
            match Self::ram_booking(&entry) {
                Ok(ram) => (Some(Gibibytes(0.0)), Some(ram)),
                Err(error) => {
                    let error = SchedulerError::Admission(error);
                    self.slots[slot].job = Some(job_id);
                    self.step(job_id, SwapState::Admit);
                    self.step(job_id, SwapState::AdmitFail);
                    self.log_refusal(job_id, &error, "need");
                    self.finish(job_id, Err(error), "under-specified");
                    return;
                }
            }
        } else {
            match Self::booking_need(&entry) {
                Ok(booking) => (booking, None),
                Err(error) => {
                    let error = SchedulerError::Admission(error);
                    self.slots[slot].job = Some(job_id);
                    self.step(job_id, SwapState::Admit);
                    self.step(job_id, SwapState::AdmitFail);
                    self.log_refusal(job_id, &error, "need");
                    self.finish(job_id, Err(error), "under-specified");
                    return;
                }
            }
        };
        // The figure a confirmed holder release credits (W21-01 ruling, W2 step 2):
        // the bytes this release frees in the resource the drain is planning. A
        // Ram row drains the RAM ledger, so its own-slot figure is the holder's
        // RAM booking — credited once (the drain re-check subtracts the holder leg
        // from the ledger term, R2-03-01, and the physical reading already held
        // it at sample time), and 0.0 for a GPU holder: its host RAM is real but
        // unbooked, so the RAM settle witness reads the freed bytes instead of a
        // credit this gate cannot honestly state. A VRAM row keeps the card
        // figure, unchanged.
        let victim = self.slots[slot].occupant.as_ref().map(|o| Victim {
            model: o.model.clone(),
            ticket: o.ticket.clone(),
            slot,
            evicted: false,
            cost: if entry.resource() == crate::model::Resource::Ram {
                o.ram_need.map_or(0.0, |need| need.0)
            } else {
                o.need.0
            },
        });
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.need = booking;
            job.ram_need = ram_booking;
            job.victim = victim;
        }
        self.slots[slot].job = Some(job_id);
        self.step(job_id, SwapState::Admit);
        // Step 4 (as amended 2026-10-26): the gate is evaluated against the card's own
        // reading, sampled on a task — never inline in the actor, which must not stall
        // behind a witness call. The two ladders sample their own witness (operator
        // ruling 2026-11-05): a CPU-resident row never reads the card (not even to
        // waste a cached sample on it), and a VRAM row never spends the host reading.
        let witness = Arc::clone(&self.witness);
        let ram_witness = Arc::clone(&self.ram_witness);
        let ram_row = self.jobs[&job_id].resource == crate::model::Resource::Ram;
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let event = if ram_row {
                Event::Admitted(None, ram_witness.sample())
            } else {
                Event::Admitted(Some(witness.sample().await), None)
            };
            let _ = tx.send(ActorMsg::Event { job: job_id, event });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// The Admit decision, once the witness sample returns. The card's own reading
    /// is the budget (operator 2026-10-26): `total - reserve - observed_used`,
    /// credited for the occupant this job is about to release and debited for what
    /// other loads have already committed. The ledger survives only as the `budget`
    /// figure a refusal quotes, and as the whole basis of `ledger_only` witness mode.
    ///
    /// The two ladders decide here and share one admitted tail: a RAM row is
    /// dispatched to [`Self::finish_admit_ram`] and the card is never consulted
    /// for it (operator ruling 2026-11-05); the VRAM leg below is unchanged.
    pub(super) fn finish_admit(
        &mut self,
        job_id: JobId,
        card: Option<Result<crate::budget::MemorySample, String>>,
        ram: Option<crate::witness::RamSample>,
    ) {
        if self.jobs[&job_id].resource == crate::model::Resource::Ram {
            self.finish_admit_ram(job_id, ram);
            return;
        }
        // A RAM row never gets here, so the absent-card slot can only mean the
        // dispatcher forgot the reading — named, never silently waved through.
        let sample = card.unwrap_or_else(|| Err("no card sample was dispatched".to_string()));
        // A real witness that cannot answer is a refusal, never a fallback onto a
        // bookkeeping guess (F-3/F-7: this is exactly how v3.3 over-committed).
        if sample.is_err() && !self.witness.ledger_only() {
            let reason = sample
                .as_ref()
                .err()
                .map_or_else(|| "no sample".to_string(), String::clone);
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::WitnessUnavailable { reason });
            self.log_refusal(job_id, &error, "witness");
            self.finish(job_id, Err(error), "witness down");
            return;
        }
        let (total_gib, occupied, observed_free) = match &sample {
            Ok(sample) => (sample.total_gib, sample.used_gib, Some(sample.free_gib())),
            Err(_) => {
                // The declared mode of a witness that never answers (tests, a box
                // without a GPU): the ledger is then the only truth there is.
                tracing::debug!("witness never answers (ledger_only); budgeting the ledger");
                (self.cfg.vram_total_gb, self.ledger_sum().0, None)
            }
        };
        let declared = self.jobs[&job_id].need;
        let need = declared.unwrap_or_default();
        let victim_need = self.jobs[&job_id]
            .slot
            .map(|slot| self.slots[slot].occupant.as_ref().map_or(0.0, |o| o.need.0))
            .unwrap_or(0.0);
        // The bytes this job is about to reclaim are still physically on the card at
        // sample time, so they are credited; what other loads have committed but not
        // yet installed is debited, or two concurrent loads spend the same free GiB.
        let in_flight = self.in_flight_need(job_id);
        let free =
            Gibibytes(total_gib - self.cfg.vram_reserve_gb - occupied + victim_need - in_flight);
        let facts = BudgetFacts {
            budget: Some(budget::free_gb(
                Gibibytes(total_gib),
                Gibibytes(self.cfg.vram_reserve_gb),
                self.ledger_sum(),
            )),
            observed_free: observed_free.map(Gibibytes),
            // R3: filled in the refusal arm from the planner's candidate set;
            // a passing admission never prints them.
            ..Default::default()
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.free_at_admit = Some(free);
            job.observed_at_admit = observed_free;
        }
        self.set_timing(job_id, |t| &mut t.admit_ms);
        let (greedy, vram_gate) = {
            let job = &self.jobs[&job_id];
            (job.cost == crate::model::CostClass::Greedy, job.vram_gate)
        };
        let whole = matches!(vram_gate, crate::model::VramGate::Whole);
        // Launch floor: the model's numeric key, else the global greedy
        // default — a greedy quant spawned onto a card already busy with
        // foreign processes thrashes RAM/SSD for minutes before serving
        // (operator note 2026-10-26). `whole` REPLACES this leg (operator
        // ruling 2026-11-04): the auto floor below is the whole-machine gate
        // expressed in this leg's own units — `free` already nets the
        // reserve, so "the card holds nothing beyond the reserve and the
        // credited same-slot replacement" reads as `free >= total -
        // 2 * reserve` on the same credited witness reading the numeric
        // floor leg compares ([`AdmissionError::WholeMachineBusy`] owns the
        // full "fully free" definition). A greedy `whole` row keeps no
        // numeric floor: the global default never consults it, `whole`
        // replaces the greedy floor leg exactly as it replaces the row one.
        let floor = match vram_gate {
            crate::model::VramGate::Whole => Some(total_gib - 2.0 * self.cfg.vram_reserve_gb),
            crate::model::VramGate::Floor(model_floor) => model_floor.or(if greedy {
                Some(self.cfg.greedy_launch_min_free_gb)
            } else {
                None
            }),
        };
        // The card itself can be misread: `nvidia-smi` parses `NaN` as a number, and
        // every comparison against NaN is false, which would silently read as
        // "fits". An unreadable reading is named as one (fail closed, §0.3).
        if !free.0.is_finite() {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::WitnessUnavailable {
                reason: format!("unreadable witness reading: {} GiB free", free.0),
            });
            self.log_refusal(job_id, &error, "witness");
            self.finish(job_id, Err(error), "witness unreadable");
            return;
        }
        // Defense in depth (review, 2026-10-26): the scan parser rejects
        // non-finite sizes, but no value may ever *silently* pass the gate —
        // every NaN comparison is false, which would read as "fits".
        if need.0.is_nan() || floor.is_some_and(|f| !f.is_finite()) {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::UnderSpecified);
            self.log_refusal(job_id, &error, "bad-size");
            self.finish(job_id, Err(error), "under-specified");
            return;
        }
        // The whole-machine leg (operator ruling 2026-11-04): a WAIT posture
        // on top of the auto floor — refuse naming every holder, never evict
        // to get there. The numeric-deficit machinery below is never reached
        // while the card is not fully free, so the planner is never asked for
        // victims on a `whole` refusal and the B1 `min_turn` interplay is
        // trivially safe. The settle-window re-check posture is unchanged:
        // the auto floor rides `gate_target` like any numeric floor, and an
        // explicit same-slot replacement still re-gates on the physical card
        // after its release ([`ActorState::spawn_settle_task`]).
        if whole {
            let required = floor.expect("invariant: the whole gate always carries its auto floor");
            let holders = self.whole_machine_holders(job_id);
            if free.0 < required || !holders.is_empty() {
                self.step(job_id, SwapState::AdmitFail);
                let mut facts = facts;
                if observed_free.is_some() {
                    // R3 figures beside the named holders: the whole path may
                    // release nothing but the credited same-slot replacement,
                    // so everything else the card holds is unreleasable here
                    // (the reserve band is the operator's allowance for the
                    // driver baseline — see the variant doc).
                    facts.unreleasable = Some(Gibibytes((occupied - victim_need).max(0.0)));
                    facts.used_gib = Some(Gibibytes(occupied));
                    facts.booked_total = Some(self.ledger_sum());
                }
                let error = AdmissionError::WholeMachineBusy {
                    required: Gibibytes(required),
                    free,
                    reserve: Gibibytes(self.cfg.vram_reserve_gb),
                    facts: Box::new(facts),
                    blockers: holders,
                    wait_hint: self.wait_hint(job_id),
                };
                self.log_refusal(job_id, &SchedulerError::Admission(error.clone()), "gate");
                self.finish(
                    job_id,
                    Err(SchedulerError::Admission(error)),
                    "admission refused",
                );
                return;
            }
        }
        // One truth now: `free` already is the physical reading, credited and
        // debited. `facts` carries the originals so a refusal can name which bound.
        let need_deficit = if greedy {
            0.0
        } else {
            (need.0 - free.0).max(0.0)
        };
        let floor_deficit = floor.map(|f| (f - free.0).max(0.0)).unwrap_or(0.0);
        let deficit = need_deficit.max(floor_deficit);
        // What `available` (free_at_admit + released, each credit counted once) must
        // reach before spawn. Comparing only `need` re-admits the bug this review
        // caught: greedy books a clamped-small need, so the re-check would skip the
        // very floor victims the planner queued. The stored figure is the planned
        // target alone: `free_at_admit` already carries the reserve band, and
        // [`ActorState::spawn_settle_task`] adds it to the physical re-gate, so both
        // halves of this decision judge the same guard band (DOCTRINE item 4).
        let gate_target = if greedy {
            floor.unwrap_or(0.0)
        } else {
            need.0.max(floor.unwrap_or(0.0))
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.gate_target = Some(gate_target);
        }
        if deficit > 0.0 {
            // Step 6: try to cover the deficit with evictable cross-category
            // victims before refusing — cheapest sufficient set, hard filters.
            // A `whole` row that stands here has already passed the whole-
            // machine leg, so the planner provably has no candidate to find —
            // and must never be asked: the whole gate never evicts (operator
            // ruling 2026-11-04).
            let planned = if whole {
                Err((Vec::new(), Vec::new()))
            } else {
                self.plan_victims(job_id, deficit)
            };
            match planned {
                Ok(victims) => {
                    if let Some(job) = self.jobs.get_mut(&job_id) {
                        job.victims = victims;
                    }
                    tracing::info!(
                        %job_id,
                        victims = ?self.jobs[&job_id].victims.iter().map(|v| &v.model).collect::<Vec<_>>(),
                        deficit_gib = deficit,
                        "admission short: evicting victims"
                    );
                }
                Err((considered, blockers)) => {
                    // Row-10 guard (RULED 2026-11-01): when NO release the harness
                    // may perform reaches the floor, refuse BEFORE any kill —
                    // "no releases allow for the VRAM floor - check foreign
                    // residents". Releasable = the same-slot victim (already
                    // credited in `free`) plus every evictable candidate; blockers
                    // and foreign holders never count toward meeting a floor
                    // (R5's principle, same arithmetic as R3's `unreleasable`).
                    // The guard precedes the release-first arm: destroying the
                    // resident is an accepted cost only for a deficit a release
                    // could actually fix, never for a floor the card denies.
                    let best_free = free.0 + considered.iter().map(|v| v.need_gib).sum::<f64>();
                    let no_releases_reach =
                        floor.is_some_and(|f| f - self.cfg.witness_tolerance_gb > best_free);
                    // Ruling (operator 2026-11-01, REVISABLE): release-then-resample
                    // for same-slot replacements. The victim's booked need can
                    // understate what it actually holds, and a swap that physically
                    // fits must not die on paper (the round-2(A) 5 GiB paper deficit).
                    // Destroy the victim — it was being replaced by request anyway —
                    // and let the settle window re-gate the load against the card as
                    // physically re-sampled. Cross-slot victims are NOT queued here:
                    // the planner found no sufficient set, and nothing beyond the
                    // already-doomed resident may be killed on a paper decision.
                    if !no_releases_reach
                        && self.jobs[&job_id].victim.is_some()
                        && !self.jobs[&job_id].release_first
                    {
                        if let Some(job) = self.jobs.get_mut(&job_id) {
                            job.release_first = true;
                        }
                        tracing::info!(
                            %job_id,
                            deficit_gib = deficit,
                            "admission short on the booked credit: releasing the same-slot victim first and re-gating on the physical card (release-first)"
                        );
                    } else {
                        self.step(job_id, SwapState::AdmitFail);
                        let reserve = Gibibytes(self.cfg.vram_reserve_gb);
                        // R3 (operator GO 2026-11-01): the refusal must name the
                        // memory no eviction can reclaim — the card minus the
                        // same-slot replacement and every evictable candidate.
                        // Blockers ride inside it (they are exactly the part the
                        // harness may not release) and are named beside it.
                        let mut facts = facts;
                        if observed_free.is_some() {
                            let releasable =
                                victim_need + considered.iter().map(|v| v.need_gib).sum::<f64>();
                            facts.unreleasable = Some(Gibibytes((occupied - releasable).max(0.0)));
                            facts.used_gib = Some(Gibibytes(occupied));
                            facts.booked_total = Some(self.ledger_sum());
                        }
                        // An unreachable floor is always shaped as the floor
                        // refusal: the guard sentence is the actionable fact even
                        // when a (smaller) need deficit also bound.
                        // Decision-time facts ride the refusal as structured
                        // fields beside the named holders (the estimate is a
                        // rolling median of our own measured releases, never a
                        // promise — see [`ActorState::wait_hint`]).
                        let wait_hint = self.wait_hint(job_id);
                        let error = if (floor_deficit > 0.0 && floor_deficit >= need_deficit)
                            || no_releases_reach
                        {
                            AdmissionError::BelowLaunchFloor {
                                min_free: Gibibytes(floor.expect(
                                    "invariant: floor_deficit > 0 implies a configured floor",
                                )),
                                // G13: the figure the gate actually compared.
                                free,
                                reserve,
                                facts: Box::new(facts),
                                victims_considered: considered,
                                blockers,
                                no_releases_reach,
                                wait_hint,
                            }
                        } else {
                            AdmissionError::Insufficient {
                                need,
                                // G13: the compared figure, never the flattering one.
                                free,
                                reserve,
                                facts: Box::new(facts),
                                victims_considered: considered,
                                blockers,
                                wait_hint,
                            }
                        };
                        self.log_refusal(job_id, &SchedulerError::Admission(error.clone()), "gate");
                        self.finish(
                            job_id,
                            Err(SchedulerError::Admission(error)),
                            "admission refused",
                        );
                        return;
                    }
                }
            }
        }
        // A greedy tenant books what it will actually grab — its nominal size if
        // that fits, else the room available *including* the victims this very
        // admit queued for release (their bytes are still counted as used but will
        // be gone before spawn). Never its offload-able nominal size (starves the
        // card) and never zero when it will hold real VRAM (under-books the gate).
        if greedy {
            let planned_room = free.0
                + self.jobs[&job_id]
                    .victims
                    .iter()
                    .map(|v| v.cost)
                    .sum::<f64>();
            let booked = declared
                .map_or(planned_room, |n| n.0.min(planned_room))
                .max(0.0);
            if let Some(job) = self.jobs.get_mut(&job_id) {
                job.need = Some(Gibibytes(booked));
            }
        }
        // The rung derivation and the release-or-spawn commit are the shared
        // admitted tail — the gate leg above only proved the numbers.
        if crate::observability::extended_logging() {
            // Decision-level evidence for rehearsal post-checks (operator,
            // 2026-10-26): the exact numbers the gate compared on the pass path.
            let job = &self.jobs[&job_id];
            tracing::info!(
                %job_id,
                model = %job.model,
                cost = ?job.cost,
                need_gib = job.need.map(|n| n.0),
                floor_gib = floor,
                free_gib = free.0,
                observed_at_admit_gib = job.observed_at_admit,
                budget_gib = facts.budget.map(|budget| budget.0),
                booked_gib = self.in_flight_need(job_id),
                victim = ?job.victim.as_ref().map(|v| v.model.clone()),
                victims = ?job.victims.iter().map(|v| v.model.clone()).collect::<Vec<_>>(),
                gate_gib = gate_target,
                "admission decision (extended)"
            );
        }
        self.finish_admit_tail(job_id);
    }

    /// The shared admitted tail of both ladders: the one rung derivation, at the
    /// single point where a gate has said yes, and the release-or-spawn commit.
    /// Refusal arms never reach here — each one already logs `admission refused`
    /// with its numbers through `log_refusal`, so the rung line names only the
    /// rung (B6: the decision quotes its rung, the numbers stay where they
    /// already live).
    fn finish_admit_tail(&mut self, job_id: JobId) {
        let path = if self.jobs[&job_id].release_first {
            AdmissionPath::ReleaseFirst
        } else if self.jobs[&job_id].victims.is_empty() {
            AdmissionPath::FitsNow
        } else {
            AdmissionPath::Drain
        };
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.path = Some(path);
        }
        tracing::info!(
            %job_id,
            path = ?path,
            model = %self.jobs[&job_id].model,
            category = ?self.jobs[&job_id].category,
            "admission path"
        );
        // Chunk 10 [2]: everything the adapter validates about a launch — required
        // keys, profile/interpreter existence, WSL word and container legality — must
        // be settled *before* the eviction. Run after Dispose it would destroy the
        // resident model and every victim, then refuse, leaving an empty slot and
        // work nobody can un-kill. Read-only, so a refusal here changes nothing.
        if let Err(error) = self.preflight_spec(job_id) {
            self.step(job_id, SwapState::AdmitFail);
            self.log_refusal(job_id, &error, "launch");
            self.finish(job_id, Err(error), "launch refused");
            return;
        }
        self.step(job_id, SwapState::Quiesce);
        self.step(job_id, SwapState::Dispose);
        // Ensure a release target: the same-slot replacement first, else the first
        // queued cross-category victim.
        if self.jobs[&job_id].victim.is_none() {
            self.dequeue_victim(job_id);
        }
        if self.jobs[&job_id].victim.is_some() {
            // The release is committed and its wait is about to open: warm
            // the incoming model's files for the reload that follows
            // (knob-gated, detached, never able to change this job's path).
            self.start_release_prefetch(job_id);
            self.spawn_kill_confirm(job_id);
        } else {
            self.begin_spawn(job_id);
        }
    }

    /// The Admit decision of the RAM ladder (operator ruling 2026-11-05). The
    /// host reading is the budget, with the card's arithmetic and the card's
    /// honesty rules: `ram_total - ram_reserve - observed ram_used`, credited
    /// once for the same-slot RAM resident this job is about to release and
    /// debited for what other loads have already committed to RAM. No launch
    /// floor and no VRAM deficit. A host-RAM shortage asks the same Step 6
    /// planner as the card leg, over Ram residents only (W2 ruling): the plan
    /// runs its victims through the unchanged release path. A GPU resident's
    /// host RAM rides the witness reading and never enters the releasable
    /// set — it is named as an uncontrolled blocker, the same posture as a
    /// foreign holder on the card. Any other named holder rides the refusal
    /// as a blocker and unattributable host RAM rides the `unreleasable`
    /// figure (the correct product outcome of DOCTRINE 3: a refusal that
    /// explains itself). A missing host reading refuses fail-closed, exactly
    /// as a dead card witness does (DOCTRINE 4): there is no ledger-only mode
    /// for RAM.
    fn finish_admit_ram(&mut self, job_id: JobId, ram: Option<crate::witness::RamSample>) {
        let Some(sample) =
            ram.filter(|s| s.total_gib.is_finite() && s.used_gib.is_finite() && s.total_gib > 0.0)
        else {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::WitnessUnavailable {
                reason: "ram witness: no host reading to budget against".to_string(),
            });
            self.log_refusal(job_id, &error, "witness");
            self.finish(job_id, Err(error), "ram witness down");
            return;
        };
        let need = self.jobs[&job_id].ram_need.unwrap_or_default();
        let reserve = Gibibytes(self.cfg.ram_reserve_gb);
        // Defense in depth (the card leg's rule, mirrored): a NaN need would
        // make every comparison false and read as "fits".
        if need.0.is_nan() {
            self.step(job_id, SwapState::AdmitFail);
            let error = SchedulerError::Admission(AdmissionError::UnderSpecified);
            self.log_refusal(job_id, &error, "bad-size");
            self.finish(job_id, Err(error), "under-specified");
            return;
        }
        // The bytes this job is about to reclaim are still physically held at
        // sample time, so they are credited — once, through the RAM ledger
        // (a GPU resident's host RAM is never credited: it is not booked
        // anywhere, it rides the reading). What other loads have committed to
        // RAM but not yet installed is debited, or two concurrent loads spend
        // the same host GiB.
        let same_slot_credit = self.jobs[&job_id].slot.map_or(0.0, |slot| {
            self.slots[slot]
                .occupant
                .as_ref()
                .and_then(|o| o.ram_need)
                .map_or(0.0, |n| n.0)
        });
        let in_flight = self.ram_in_flight_need(job_id);
        let free = Gibibytes(
            sample.total_gib - self.cfg.ram_reserve_gb - sample.used_gib + same_slot_credit
                - in_flight,
        );
        if let Some(job) = self.jobs.get_mut(&job_id) {
            // G13 on both ladders, and the same pair the card drain re-check
            // reads between victim releases (events): the figures this gate
            // actually compared, never the flattering ones.
            job.free_at_admit = Some(free);
            job.observed_at_admit = Some(sample.total_gib - sample.used_gib);
        }
        self.set_timing(job_id, |t| &mut t.admit_ms);
        let gate_target = need.0;
        if let Some(job) = self.jobs.get_mut(&job_id) {
            job.gate_target = Some(gate_target);
        }
        if free.0 < need.0 {
            // The same Step 6 planner as the card leg, over Ram residents
            // only (W2 ruling 2026-11-05): a host-RAM shortage evicts Ram
            // residents through the unchanged release path, never a GPU
            // resident — its host RAM rides the reading, uncontrolled.
            match self.plan_victims(job_id, need.0 - free.0) {
                Ok(victims) => {
                    if let Some(job) = self.jobs.get_mut(&job_id) {
                        job.victims = victims;
                    }
                    tracing::info!(
                        %job_id,
                        victims = ?self.jobs[&job_id].victims.iter().map(|v| &v.model).collect::<Vec<_>>(),
                        deficit_gib = need.0 - free.0,
                        "ram admission short: evicting RAM victims"
                    );
                }
                Err((considered, blockers)) => {
                    self.step(job_id, SwapState::AdmitFail);
                    // R3 mirrored on the RAM leg: the RAM no eviction can
                    // reclaim is the host reading minus the same-slot credit
                    // and every Ram candidate the planner considered.
                    // Blockers — GPU residents, matrix-refused and `min_turn`
                    // holders — never count as releasable (R5's principle).
                    let releasable =
                        same_slot_credit + considered.iter().map(|v| v.need_gib).sum::<f64>();
                    let facts = BudgetFacts {
                        budget: Some(budget::free_gb(
                            Gibibytes(sample.total_gib),
                            reserve,
                            self.ram_ledger_sum(),
                        )),
                        observed_free: Some(Gibibytes(sample.total_gib - sample.used_gib)),
                        unreleasable: Some(Gibibytes((sample.used_gib - releasable).max(0.0))),
                        used_gib: Some(Gibibytes(sample.used_gib)),
                        booked_total: Some(self.ram_ledger_sum()),
                    };
                    let error = AdmissionError::InsufficientRam {
                        need,
                        // G13: the compared figure, never the flattering one.
                        free,
                        reserve,
                        facts: Box::new(facts),
                        victims_considered: considered,
                        blockers,
                        wait_hint: self.wait_hint(job_id),
                    };
                    self.log_refusal(job_id, &SchedulerError::Admission(error.clone()), "gate");
                    self.finish(
                        job_id,
                        Err(SchedulerError::Admission(error)),
                        "admission refused",
                    );
                    return;
                }
            }
        }
        if crate::observability::extended_logging() {
            let job = &self.jobs[&job_id];
            tracing::info!(
                %job_id,
                model = %job.model,
                resource = ?job.resource,
                ram_need_gib = job.ram_need.map(|n| n.0),
                ram_free_gib = free.0,
                ram_used_gib = sample.used_gib,
                ram_total_gib = sample.total_gib,
                reserve_gib = self.cfg.ram_reserve_gb,
                booked_gib = self.ram_in_flight_need(job_id),
                victim = ?job.victim.as_ref().map(|v| v.model.clone()),
                victims = ?job.victims.iter().map(|v| v.model.clone()).collect::<Vec<_>>(),
                gate_gib = gate_target,
                "admission decision (extended)"
            );
        }
        self.finish_admit_tail(job_id);
    }

    /// The card leg under its one-reading shape, for pins that drive the VRAM
    /// gate directly: a VRAM row never carries a host reading (the dispatcher
    /// in [`Self::start_queued_load`] sends it `None` RAM exactly like this),
    /// so this is the real dispatched shape, not a test stand-in.
    #[cfg(test)]
    pub(super) fn finish_admit_card(
        &mut self,
        job_id: JobId,
        sample: Result<crate::budget::MemorySample, String>,
    ) {
        self.finish_admit(job_id, Some(sample), None);
    }

    /// What this refusal honestly knows about waiting, computed at decision
    /// time beside the numbers the gate compared.
    ///
    /// The queue rule, defined once here: `queue_position` is the one-based
    /// place behind the loads already ahead of the refused request — every
    /// other non-terminal `Load` ledger row in the refused job's category,
    /// whether in flight or parked, plus this request itself. `None` when
    /// nothing is ahead: a refusal that got no queue place returns null,
    /// because the harness placed this request nowhere and quoting "position
    /// 1" would describe a queue that does not exist. Unload jobs are not
    /// loads and never count.
    ///
    /// `expected_release_s` is the ring's median release duration (seconds).
    /// `None` until the first driver-confirmed release: the estimate is then
    /// absent from the refusal, never invented from a default.
    pub(super) fn wait_hint(&self, refused: JobId) -> WaitHint {
        let category = self.jobs[&refused].category;
        let ahead = self
            .jobs
            .iter()
            .filter(|(id, job)| {
                **id != refused
                    && job.kind == JobKind::Load
                    && !job.terminal
                    && job.category == category
            })
            .count();
        WaitHint {
            expected_release_s: self.release_samples.median_s(),
            queue_position: (ahead > 0).then(|| ahead as u32 + 1),
        }
    }

    /// Every holder that keeps a `whole` row from the machine: an occupant of
    /// a slot other than the load's own, and every other non-terminal load
    /// that has booked room (`SwapState::reserves_vram`). The request's own
    /// slot is deliberately not a holder there: an explicit same-slot load of
    /// a whole row IS a replacement of that occupant (pinned behavior,
    /// operator ruling 2026-11-04 — replacing the named target of the request
    /// is the request itself, never an eviction to get room; its booked need
    /// is already credited in the compared figure). Each holder rides
    /// [`AdmissionError::WholeMachineBusy`] as a named blocker with the
    /// reason "whole machine requested"; a foreign resident the harness
    /// cannot name rides the `unreleasable` figure instead.
    fn whole_machine_holders(&self, job_id: JobId) -> Vec<crate::budget::BlockerInfo> {
        let own_slot = self.jobs[&job_id].slot;
        let mut holders = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if Some(index) == own_slot {
                continue;
            }
            if let Some(occupant) = &slot.occupant {
                if occupant.ram_need.is_some() {
                    // A CPU-resident holder holds no card bytes: the
                    // whole-machine gate is a card posture (operator ruling
                    // 2026-11-05), and a `whole` GPU request is never blocked
                    // by a host-RAM resident.
                    continue;
                }
                holders.push(crate::budget::BlockerInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    reason: "whole machine requested".to_string(),
                });
            }
        }
        for (id, job) in self.jobs.iter() {
            if *id == job_id
                || job.kind != JobKind::Load
                || job.terminal
                || job.ram_need.is_some()
                || !job.state.reserves_vram()
            {
                continue;
            }
            holders.push(crate::budget::BlockerInfo {
                model: job.model.clone(),
                category: job.category.to_string(),
                reason: "whole machine requested".to_string(),
            });
        }
        holders
    }

    /// Step 6 planner, one code path for both ladders (operator ruling
    /// 2026-11-05, W2): it plans the resource the job row is short on, read
    /// from `Job::resource`. Among occupants evictable by this job's category
    /// in the planned resource, pick the cheapest sufficient set (exhaustive
    /// over the ≤9 slots; tie-break: fewer victims). Hard filters first — a
    /// non-evictable occupant becomes a named blocker, never a candidate, and
    /// so does a holder still inside its `min_turn` guard. The resource
    /// filter is the first hard filter and is strict in both directions: a
    /// VRAM deficit never considers or blames a Ram resident (it holds no
    /// card bytes), and a RAM deficit never spends a GPU occupant — its host
    /// RAM rides the witness reading like a foreign holder on the card, so it
    /// is named as an uncontrolled blocker but never considered. The
    /// `evictable_by` matrix stays category-scoped and resource-agnostic.
    /// Release order: least-protected first, LRU within a category.
    fn plan_victims(
        &self,
        job_id: JobId,
        deficit: f64,
    ) -> Result<
        Vec<Victim>,
        (
            Vec<crate::budget::VictimInfo>,
            Vec<crate::budget::BlockerInfo>,
        ),
    > {
        let load_category = self.jobs[&job_id].category;
        let job_slot = self.jobs[&job_id].slot;
        // The one resource this job is short on: a Ram row deficits its
        // `ram_need`, every other row the card. The filter below is derived
        // from the row, so the two arms of the gate can never ask the planner
        // for the other ladder's bytes.
        let planned = self.jobs[&job_id].resource;
        let evictable_by = |category: CategoryId| {
            self.cfg
                .categories
                .iter()
                .find(|cat| cat.id == category)
                .map(|cat| cat.evictable_by.clone())
                .unwrap_or_default()
        };
        let mut candidates: Vec<(usize, f64, usize)> = Vec::new();
        let mut considered = Vec::new();
        let mut blockers = Vec::new();
        for (index, slot) in self.slots.iter().enumerate() {
            if job_slot == Some(index) {
                continue; // the same-slot replacement is already accounted
            }
            let Some(occupant) = &slot.occupant else {
                continue;
            };
            // The resource filter, first and strict (operator ruling
            // 2026-11-05): a resident of the other resource never competes.
            // A CPU resident holds no VRAM — on the VRAM planner it is never
            // a candidate, never even "considered", and never a named
            // blocker. A GPU occupant on the RAM planner holds real but
            // uncontrolled host RAM — it rides the witness reading like a
            // foreign holder rides the card, so it is named as an
            // uncontrolled holder but never considered and never enters the
            // RAM releasable set.
            let holds_ram = occupant.ram_need.is_some();
            if holds_ram != (planned == crate::model::Resource::Ram) {
                if planned == crate::model::Resource::Ram {
                    blockers.push(crate::budget::BlockerInfo {
                        model: occupant.model.clone(),
                        category: slot.category.to_string(),
                        reason: "a GPU resident's RAM rides the host reading and is never under \
                                 the RAM gate's control"
                            .to_string(),
                    });
                }
                continue;
            }
            // The figure this occupant frees in the planned resource: its RAM
            // booking on the RAM planner, its VRAM booking on the card one.
            let figure = occupant.ram_need.map_or(occupant.need.0, |need| need.0);
            let allowed_list = evictable_by(slot.category);
            if allowed_list.contains(&load_category) {
                // `min_turn` juggle guard (operator ruling 2026-11-04): a holder
                // whose current turn is younger than the row's knob is a named
                // blocker, never a candidate, so an A-B-A-B ping-pong cannot
                // spend a measured 14-45 s swap on every hop. The knob is read
                // from the catalog at decision time, never copied onto the job
                // row: parked and superseded rows outlive their yaml edits, and
                // only the catalog entry is what the operator last wrote. A
                // name absent from the catalog has no evidence of a guard, so
                // it behaves exactly as `None` (absence of evidence is never
                // evidence of a guard). The turn clock is `Occupant::loaded_at`,
                // the install instant — read here, inside the actor, the only
                // place allowed to consult wall-clock state (no new state, no
                // arithmetic outside the actor).
                let guard = self
                    .catalog
                    .iter()
                    .find(|entry| entry.name == occupant.model)
                    .and_then(|entry| entry.min_turn_s);
                if let Some(min_turn) = guard {
                    let served = occupant.loaded_at.elapsed().as_secs();
                    if served < min_turn {
                        blockers.push(crate::budget::BlockerInfo {
                            model: occupant.model.clone(),
                            category: slot.category.to_string(),
                            reason: format!("served {served}s of its {min_turn}s min_turn guard"),
                        });
                        continue;
                    }
                }
                candidates.push((index, figure, allowed_list.len()));
                considered.push(crate::budget::VictimInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    need_gib: figure,
                });
            } else {
                let reason = if allowed_list.is_empty() {
                    format!("nothing may evict {}", slot.category)
                } else {
                    format!(
                        "only {} can evict {}",
                        allowed_list
                            .iter()
                            .map(|c| c.to_string())
                            .collect::<Vec<_>>()
                            .join(" and "),
                        slot.category
                    )
                };
                blockers.push(crate::budget::BlockerInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    reason,
                });
            }
        }
        let n = candidates.len();
        if n == 0 {
            return Err((considered, blockers));
        }
        // Exhaustive cheapest covering subset (n ≤ 9 slots → ≤512 masks).
        //
        // Ties break by disposability — how many categories may evict a victim — and
        // never by slot order. RP lists only WORK as an evictor while AUX lists WORK
        // and RP, so when two victims cost the same and either would do, the more
        // disposable category must be the one released. Comparing masks in iteration
        // order evicted RP, the costlier model to bring back, purely because its slot
        // index precedes AUX's — which contradicted the release ordering below (T6).
        let mut best: Option<u32> = None;
        let mut best_sum = f64::INFINITY;
        let mut best_count = usize::MAX;
        let mut best_disposability = usize::MIN;
        for mask in 1u32..(1 << n) {
            let (mut sum, mut count, mut disposability) = (0.0, 0, 0);
            for (bit, (_, cost, protection)) in candidates.iter().enumerate() {
                if mask & (1 << bit) != 0 {
                    sum += cost;
                    count += 1;
                    disposability += protection;
                }
            }
            if sum + 1e-9 >= deficit
                && (sum < best_sum
                    || (sum <= best_sum + 1e-9
                        && (count < best_count
                            || (count == best_count && disposability > best_disposability))))
            {
                best_sum = sum;
                best_count = count;
                best_disposability = disposability;
                best = Some(mask);
            }
        }
        let Some(mask) = best else {
            return Err((considered, blockers));
        };
        // Release order: least-protected first (more categories may evict it),
        // LRU within the same protection level.
        let mut chosen: Vec<usize> = (0..n).filter(|bit| mask & (1 << bit) != 0).collect();
        chosen.sort_by(|&a, &b| {
            let (slot_a, slot_b) = (candidates[a].0, candidates[b].0);
            let prot = |index: usize| evictable_by(self.slots[index].category).len();
            prot(slot_b).cmp(&prot(slot_a)).then_with(|| {
                let lru = |index: usize| {
                    self.slots[index]
                        .occupant
                        .as_ref()
                        .map_or(Instant::now(), |o| o.loaded_at)
                };
                lru(slot_a).cmp(&lru(slot_b))
            })
        });
        let victims = chosen
            .into_iter()
            .map(|bit| {
                let (index, freed, _) = candidates[bit];
                let occupant = self.slots[index]
                    .occupant
                    .as_ref()
                    .expect("invariant: candidates come from live occupants");
                Victim {
                    model: occupant.model.clone(),
                    ticket: occupant.ticket.clone(),
                    slot: index,
                    evicted: true,
                    cost: freed,
                }
            })
            .collect();
        Ok(victims)
    }

    /// Moves the next queued victim into the release slot; `false` when the queue
    /// is empty (Step 6: one at a time).
    pub(super) fn dequeue_victim(&mut self, job_id: JobId) -> bool {
        let Some(job) = self.jobs.get_mut(&job_id) else {
            return false;
        };
        if job.victims.is_empty() {
            return false;
        }
        let next = job.victims.remove(0);
        job.victim = Some(next);
        true
    }
}
