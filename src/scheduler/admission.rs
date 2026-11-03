//! Actor-side load registration, booking, and admission: the ordinary load path,
//! the ledger-row registration, the booking-need decision, the admit arming and
//! decision paths, and the victim planner. The state stays owned by `mod.rs`.

use crate::budget::{self, AdmissionError, BudgetFacts, Gibibytes};
use crate::config::CategoryId;
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

    /// The Admit decision, once the witness sample returns. The card's own reading
    /// is the budget (operator 2026-10-26): `total - reserve - observed_used`,
    /// credited for the occupant this job is about to release and debited for what
    /// other loads have already committed. The ledger survives only as the `budget`
    /// figure a refusal quotes, and as the whole basis of `ledger_only` witness mode.
    pub(super) fn finish_admit(
        &mut self,
        job_id: JobId,
        sample: Result<crate::budget::MemorySample, String>,
    ) {
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
        let (greedy, model_floor) = {
            let job = &self.jobs[&job_id];
            (
                job.cost == crate::model::CostClass::Greedy,
                job.launch_floor,
            )
        };
        // Launch floor: the model's key, else the global greedy default — a
        // greedy quant spawned onto a card already busy with foreign processes
        // thrashes RAM/SSD for minutes before serving (operator note 2026-10-26).
        let floor = model_floor.or(if greedy {
            Some(self.cfg.greedy_launch_min_free_gb)
        } else {
            None
        });
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
            match self.plan_victims(job_id, deficit) {
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
            self.spawn_kill_confirm(job_id);
        } else {
            self.begin_spawn(job_id);
        }
    }

    /// Step 6 planner: among occupants evictable by this job's category, pick the
    /// cheapest sufficient set (exhaustive over the ≤9 slots; tie-break: fewer
    /// victims). Hard filters first — a non-evictable occupant becomes a named
    /// blocker, never a candidate. Release order: least-protected first, LRU
    /// within a category.
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
            let allowed_list = evictable_by(slot.category);
            if allowed_list.contains(&load_category) {
                candidates.push((index, occupant.need.0, allowed_list.len()));
                considered.push(crate::budget::VictimInfo {
                    model: occupant.model.clone(),
                    category: slot.category.to_string(),
                    need_gib: occupant.need.0,
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
                let (index, _, _) = candidates[bit];
                let occupant = self.slots[index]
                    .occupant
                    .as_ref()
                    .expect("invariant: candidates come from live occupants");
                Victim {
                    model: occupant.model.clone(),
                    ticket: occupant.ticket.clone(),
                    slot: index,
                    evicted: true,
                    cost: occupant.need.0,
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
