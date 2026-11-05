//! Actor-side command handlers: the swap, cancel, restart, and unload
//! commands, the shared unload path, the idle sweep, and the proxy touch that
//! feeds it. The state, its fields, and the actor loop stay owned by `mod.rs`.

use crate::config::CategoryId;
use crate::ownership::SpawnTicket;
use std::time::{Duration, Instant};

use super::{
    state_last_used, ActorState, Job, JobId, JobKind, JobTimings, Reply, SchedulerError, Slot,
    SwapState, Victim, Waiter,
};

impl ActorState {
    /// Whether `ticket` names an engine a live job has already committed to
    /// release: its ticket is that job's `victim` or one of its queued `victims`
    /// (R2-02-01). The occupant stays installed until that release is confirmed
    /// ([`ActorState::commit_victim_release`] is the window's close, and the same
    /// commit terminalizes the victim's own load row - EVID-01), so an
    /// `Already` receipt for it would promise a slot the harness has already
    /// decided to empty. A terminal job is not a commitment: its unconfirmed
    /// victim stays installed on purpose (G12) and a later swap or restart
    /// re-owns the ticket, so `Already` for it is the point-in-time truth.
    /// This predicate is also the single source of the `releasing` value of
    /// the `/har/status` label below. `O(slots x live jobs)` against the
    /// ledger; both counts are tiny.
    fn is_committed_release_target(&self, ticket: &SpawnTicket) -> bool {
        self.jobs.iter().any(|(_, job)| {
            !job.terminal
                && (job.victim.as_ref().is_some_and(|v| &v.ticket == ticket)
                    || job.victims.iter().any(|v| &v.ticket == ticket))
        })
    }

    /// The single derivation of the `/har/status` `slots[].state` label
    /// (operator ruling 2026-11-04; the only writer is `ActorState::snapshot`).
    /// Rules, in order:
    /// - `releasing`: the slot still shows its occupant and
    ///   [`ActorState::is_committed_release_target`] names that occupant — the
    ///   same predicate the `Already` denial uses, so the label and the
    ///   receipts can never disagree about a committed release. It covers the
    ///   plain unload (the job's own `victim`) and cross-slot eviction victims
    ///   (the loading job's queued `victims`) with one read. The window closes
    ///   at driver-confirmed release; a terminal job is not a commitment
    ///   (G12), so a timed-out release reads `serving`, never a stuck
    ///   `releasing`.
    /// - `serving`: an occupant with no release in flight.
    /// - `loading`: a job attached to a slot that has no occupant yet.
    /// - `empty`: neither an occupant nor a job.
    #[must_use]
    pub(super) fn slot_state(&self, slot: &Slot) -> &'static str {
        match (&slot.occupant, slot.job) {
            (Some(occupant), _) if self.is_committed_release_target(&occupant.ticket) => {
                "releasing"
            }
            (Some(_), _) => "serving",
            (None, Some(_)) => "loading",
            (None, None) => "empty",
        }
    }

    pub(super) fn on_swap(&mut self, model: String, category: CategoryId, reply: Waiter) {
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            let _ = reply.send(Err(SchedulerError::UnknownModel(model)));
            return;
        };
        let slots: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category)
            .map(|(i, _)| i)
            .collect();
        // A4 fix: "already serving" matches name AND category.
        for &s in &slots {
            if let (Some(occupant), None) = (&self.slots[s].occupant, self.slots[s].job) {
                if occupant.model == model {
                    // R2-02-01: an occupant a live job has committed to kill is
                    // not "already serving" — the eviction in flight empties
                    // this slot, so `Already` would be a success receipt for an
                    // engine the harness has decided to destroy (DOCTRINE 3:
                    // never a receipt for the outcome it undid). Fall through:
                    // the queue and replace path below takes this slot's real
                    // place and the reply describes a real engine.
                    if self.is_committed_release_target(&occupant.ticket) {
                        tracing::info!(
                            %model,
                            ?category,
                            port = occupant.port,
                            slot = self.slots[s].index,
                            "swap of an occupant committed to eviction: not answering \
                             Already, taking a real queue place instead"
                        );
                        continue;
                    }
                    // FINDINGS-06 [8]: an occupant that installed with no id is
                    // not sentenced to it — every resolve re-runs discovery
                    // (contract on [`ActorState::install`]). The reply itself
                    // stays the current state: a load never waits on a probe.
                    let retry = occupant.serve_id.is_none().then(|| occupant.ticket.clone());
                    let _ = reply.send(Ok(Reply::Already {
                        model: occupant.model.clone(),
                        category,
                        port: occupant.port,
                        serve_id: occupant.serve_id.clone(),
                    }));
                    if let Some(ticket) = retry {
                        self.retry_serve_id(ticket);
                    }
                    return;
                }
            }
        }
        // F-6 fix: an identical in-flight load gets the waiter attached, not a race.
        for &s in &slots {
            if let Some(job_id) = self.slots[s].job {
                if let Some(job) = self.jobs.get(&job_id) {
                    if job.kind == JobKind::Load && job.model == model && !job.terminal {
                        self.jobs
                            .get_mut(&job_id)
                            .expect("invariant: job exists when its id is on a slot")
                            .waiters
                            .push(reply);
                        return;
                    }
                }
            }
        }
        // Target: an empty slot first, else the LRU occupied one, else supersede.
        let idle = |s: usize| self.slots[s].job.is_none();
        let target = slots
            .iter()
            .copied()
            .find(|&s| idle(s) && self.slots[s].occupant.is_none())
            .or_else(|| {
                slots.iter().copied().filter(|&s| idle(s)).min_by_key(|&s| {
                    self.slots[s]
                        .occupant
                        .as_ref()
                        .map_or(Instant::now(), |o| o.loaded_at)
                })
            });
        match target {
            Some(target) => self.begin_load(target, entry, vec![reply]),
            None => {
                // Every slot runs a job: cancel the oldest, park this load (T3.4).
                let target = slots
                    .into_iter()
                    .min_by_key(|&s| {
                        self.slots[s]
                            .job
                            .and_then(|j| self.jobs.get(&j))
                            .map_or(Instant::now(), |j| j.t0)
                    })
                    .expect("invariant: the category has at least one slot");
                // The parked load gets its ledger row now, not when it starts: a
                // queued load must be visible at `/har/jobs` while its caller waits
                // (FINDINGS-02 [3]). One queue place per slot, so the load it
                // replaces is answered and terminalized rather than left stranded.
                let queued = self.register_queued_load(target, &entry, vec![reply]);
                if let Some(old) = self.pending.insert(target, queued) {
                    self.finish_queued_load(
                        old,
                        Err(SchedulerError::Cancelled(
                            "superseded by a newer load before it started".to_string(),
                        )),
                        "cancelled",
                    );
                }
                if let Some(job_id) = self.slots[target].job {
                    self.cancel_job(job_id, "superseded by a newer load");
                }
            }
        }
    }

    pub(super) fn on_cancel(&mut self, job_id: JobId, reply: Waiter) {
        match self.jobs.get(&job_id) {
            Some(job) if job.terminal => {
                let _ = reply.send(Ok(Reply::Done));
            }
            Some(_) => {
                self.cancel_job(job_id, "cancelled by request");
                let _ = reply.send(Ok(Reply::Done));
            }
            None => {
                let _ = reply.send(Err(SchedulerError::UnknownJob(job_id)));
            }
        }
    }

    /// T10.1 crash-retry primitive: kill the (dead) occupant of `model`'s slot and
    /// load it fresh. Reuses the standard replacement path — the dead occupant
    /// becomes the victim, so release + settle + spawn all run normally.
    ///
    /// R2-06-01: `failed_port` is the engine the caller's forward refused on — the
    /// corpse it witnessed. A second `Restart` for the same model can arrive after
    /// the first retry already reloaded it and installed a successor: when the
    /// occupant serves a port other than the refused one, it is a different, live
    /// generation, and `begin_load` would book it as the victim of a crash that is
    /// already recovered (evicting the successor mid-generation). Answer that
    /// command through the plain swap path — which replies `Already` for the
    /// successor, or queues honestly when a release of it is already committed.
    /// Residual: a port allocator that hands the successor the corpse's exact port
    /// is indistinguishable from a restart of the very generation that died, and
    /// the reload proceeds; closing that corner needs port + generation identity,
    /// which the command does not carry.
    pub(super) fn on_restart(
        &mut self,
        model: String,
        category: CategoryId,
        failed_port: u16,
        reply: Waiter,
    ) {
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            let _ = reply.send(Err(SchedulerError::UnknownModel(model)));
            return;
        };
        let slot = self.slots.iter().position(|s| {
            s.category == category
                && s.job.is_none()
                && s.occupant.as_ref().is_some_and(|o| o.model == model)
        });
        match slot {
            Some(slot) => {
                let occupant_port = self.slots[slot]
                    .occupant
                    .as_ref()
                    .expect("invariant: the matched slot has an occupant")
                    .port;
                if occupant_port != failed_port {
                    tracing::info!(
                        %model,
                        ?category,
                        failed_port,
                        port = occupant_port,
                        "crash-retry for a port the slot no longer serves: a previous \
                         retry already reloaded this model; answering the swap, not \
                         reaping the successor"
                    );
                    self.on_swap(model, category, reply);
                } else {
                    self.begin_load(slot, entry, vec![reply]);
                }
            }
            None => {
                // Already reaped (idle sweep) or never loaded: a plain load covers it.
                self.on_swap(model, category, reply);
            }
        }
    }

    pub(super) fn on_unload(&mut self, category: CategoryId, reply: Waiter) {
        let targets: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.category == category)
            .map(|(i, _)| i)
            .collect();
        self.unload_slots(targets, reply);
    }

    /// T12.1: unload one model's slot; a loading swap for it is cancelled first and
    /// the slot is unloaded after that cancel terminalizes. A loading slot has no
    /// occupant yet — the in-flight job's model is the only way to find it (T8:
    /// unload during load = cancel, cleanup completes). A load queued on the model
    /// counts as a target too, so the row the operator can see is the row the command
    /// removes.
    pub(super) fn on_unload_model(&mut self, model: &str, reply: Waiter) {
        let targets: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| {
                s.occupant.as_ref().is_some_and(|o| o.model == model)
                    || s.job
                        .is_some_and(|j| self.jobs.get(&j).is_some_and(|job| job.model == model))
            })
            .map(|(i, _)| i)
            .collect();
        let queued: Vec<usize> = self
            .pending
            .iter()
            .filter(|(_, queued)| {
                self.jobs
                    .get(queued)
                    .is_some_and(|job| job.model == model && !job.terminal)
            })
            .map(|(slot, _)| *slot)
            .collect();
        let mut targets: Vec<usize> = targets.into_iter().chain(queued).collect();
        targets.sort_unstable();
        targets.dedup();
        self.unload_slots(targets, reply);
    }

    /// T12.4: unload every occupied slot; loading jobs are cancelled, and the slot
    /// they still leave occupied is unloaded when the cancel terminalizes.
    pub(super) fn on_unload_all(&mut self, reply: Waiter) {
        let targets: Vec<usize> = (0..self.slots.len())
            .filter(|&i| self.slots[i].occupant.is_some() || self.slots[i].job.is_some())
            .collect();
        self.unload_slots(targets, reply);
    }

    /// Shared unload path: free slots start an unload job; a slot with an in-flight
    /// job has that job cancelled **and** — when it still holds an occupant — unloaded
    /// the moment the cancellation terminalizes (FINDINGS-02 [2]: an unload means
    /// cancel *and* unload, never either/or). The occupant survives a cancelled swap
    /// whenever the swap's release was never confirmed (chunk 4 [1] keeps an engine
    /// nobody proved dead installed), so cancelling alone can leave the model loaded
    /// and serving while the caller is told `Done`. A load parked on a targeted slot
    /// is dropped here too: an operator's unload must never be followed by the load
    /// they abandoned starting on the slot it just freed (FINDINGS-02 [3]).
    /// The reply reports the facts the command moved (`Reply::Unloaded`,
    /// FINDINGS-07 [5]) — never a blanket acceptance while nothing moved. A
    /// repeat command therefore moves nothing it can report: re-arming a slot
    /// already armed behind the same job is a set no-op (FINDINGS-09 [1]), and
    /// cancelling a job that already carries `CancelRequested` cancels nothing
    /// new. Cancelling an in-flight UNLOAD job is not a `jobs_cancelled` fact
    /// either: it removes an unloading commitment rather than a load, and the
    /// re-armed release rides `unloading` (FINDINGS-09 [2]).
    fn unload_slots(&mut self, slots: Vec<usize>, reply: Waiter) {
        let mut unloading = 0usize;
        let mut jobs_cancelled = 0usize;
        for slot in slots {
            if let Some(queued) = self.pending.remove(&slot) {
                self.finish_queued_load(
                    queued,
                    Err(SchedulerError::Cancelled(
                        "unloaded by request before it started".to_string(),
                    )),
                    "cancelled",
                );
                jobs_cancelled += 1;
            }
            match self.slots[slot].job {
                Some(job_id) => {
                    let (job_live, cancels_a_fresh_load) = self
                        .jobs
                        .get(&job_id)
                        .map(|job| {
                            (
                                !job.terminal,
                                !job.terminal
                                    && job.kind == JobKind::Load
                                    && job.state != SwapState::CancelRequested,
                            )
                        })
                        .unwrap_or((false, false));
                    // Flagged before the cancel: it may terminalize the job inline,
                    // and that finish is exactly where the flag is consumed. The
                    // set's answer IS the count: only a slot not armed until now
                    // gained a commitment from this command (FINDINGS-09 [1]).
                    if job_live
                        && self.slots[slot].occupant.is_some()
                        && self.unload_after_finish.insert(slot)
                    {
                        unloading += 1;
                    }
                    if cancels_a_fresh_load {
                        jobs_cancelled += 1;
                    }
                    self.cancel_job(job_id, "unloaded by request");
                }
                None => {
                    if self.slots[slot].occupant.is_some() {
                        self.begin_unload_slot(slot, "unloaded");
                        unloading += 1;
                    }
                }
            }
        }
        let _ = reply.send(Ok(Reply::Unloaded {
            unloading,
            jobs_cancelled,
        }));
    }

    /// Starts one unload job for a specific slot (`note` becomes the job outcome:
    /// "unloaded" for operator/eviction paths, "idle timeout" for the sweep).
    pub(super) fn begin_unload_slot(&mut self, slot: usize, note: &str) {
        let job_id = JobId(self.next_job);
        self.next_job += 1;
        let category = self.slots[slot].category;
        let victim = {
            let occupant = self.slots[slot]
                .occupant
                .as_ref()
                .expect("invariant: caller checks occupant.is_some");
            Victim {
                model: occupant.model.clone(),
                ticket: occupant.ticket.clone(),
                slot,
                evicted: false,
                cost: occupant.need.0,
            }
        };
        let job = Job {
            kind: JobKind::Unload,
            // An unload row never stands before the admission gate: no rung.
            path: None,
            model: victim.model.clone(),
            engine: String::new(),
            category,
            state: SwapState::Idle,
            terminal: false,
            outcome: None,
            error: None,
            need: None,
            // An unload row books neither resource: it never stands before a
            // gate, and both in-flight sums count `Load` rows only. The tag
            // is the inert default, never a claim about the departing row.
            resource: crate::model::Resource::Gpu,
            ram_need: None,
            cost: crate::model::CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            gate_target: None,
            free_at_admit: None,
            observed_at_admit: None,
            release_first: false,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters: Vec::new(),
            tasks: Vec::new(),
            spawned: None,
            readiness: None,
            measured: None,
            victim: Some(victim),
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            holder_released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: Some(note.to_string()),
            warnings: Vec::new(),
            slot: Some(slot),
            // Unload rows queue against an occupant, not a file: nothing to compare.
            queue_fingerprint: None,
        };
        self.jobs.insert(job_id, job);
        self.slots[slot].job = Some(job_id);
        self.step(job_id, SwapState::Admit);
        self.step(job_id, SwapState::Quiesce);
        self.step(job_id, SwapState::Dispose);
        self.spawn_kill_confirm(job_id);
    }

    /// The idle sweep (Step 5, tick-driven): every occupied, job-free slot whose
    /// category has `idle_timeout_s > 0` and whose model has been unused for the
    /// threshold starts an unload. WORK defaults to 0 (never idles); config is king.
    /// An occupant a live job has already committed to release is skipped: a
    /// cross-slot victim's slot stays job-free while that release runs, and a
    /// second unload behind it would name two causes of death for one model
    /// (R2-20-01).
    pub(super) fn on_tick(&mut self) {
        let now = Instant::now();
        let due: Vec<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                let (Some(occupant), None) = (&slot.occupant, slot.job) else {
                    return false;
                };
                // R2-20-01 (same window [`ActorState::is_committed_release_target`]
                // guards in `on_swap`, R2-02-01): the sweep's `job.is_none()` spare
                // never covers a cross-slot victim, so consult the commitment.
                if self.is_committed_release_target(&occupant.ticket) {
                    return false;
                }
                let Some(timeout) = self
                    .cfg
                    .categories
                    .iter()
                    .find(|cat| cat.id == slot.category)
                    .map(|cat| cat.idle_timeout_s)
                    .filter(|timeout| *timeout > 0)
                else {
                    return false;
                };
                now.duration_since(state_last_used(occupant, &self.last_used))
                    >= Duration::from_secs(timeout)
            })
            .map(|(index, _)| index)
            .collect();
        for slot in due {
            let model = self.slots[slot]
                .occupant
                .as_ref()
                .map(|o| o.model.clone())
                .unwrap_or_default();
            tracing::info!(slot = self.slots[slot].index, category = %self.slots[slot].category, %model,
                "idle timeout reached; unloading");
            self.begin_unload_slot(slot, "idle timeout");
        }
    }

    /// Records a proxy-side use of `model` (last-use input for the idle sweep).
    pub(super) fn touch_model(&mut self, model: &str) {
        self.last_used.insert(model.to_string(), Instant::now());
    }
}
