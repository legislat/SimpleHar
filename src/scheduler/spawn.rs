//! Actor-side spawn lifecycle: the post-release settle window, the launch
//! preflight, the spawn, the readiness probe, the ownership-scoped reap, and
//! the bounded late-release watch. The state and the actor loop stay owned by
//! `mod.rs`.

use crate::engine::{LaunchSpec, Probe, SpawnError};
use crate::model::ModelEntry;
use crate::ownership::{ResourceKind, SpawnTicket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::measured::MeasuredAnchor;
use super::{
    ActorMsg, ActorState, Event, JobId, SchedulerError, SwapState, Victim, PROBE_INTERVAL,
    RELEASE_POLL,
};

impl ActorState {
    /// §0.3 memory evidence: after a confirmed release, wait until the card has
    /// *physically* freed the gate target **plus the reserve guard band** — the same
    /// `vram_reserve_gb` the direct-admit gate subtracts (DOCTRINE item 4: the gate is
    /// `total - reserve - observed_used + credits`, and this window is the physical
    /// half of that same decision) — within the witness tolerance before the spawn
    /// fires. This is where a replacement load proves the card truly emptied (operator
    /// 2026-10-26: the stopper is observed VRAM, judged after release). The compare
    /// stays physical free against `max(need, launch floor) + vram_reserve_gb` with the
    /// ruled 2026-10-26 one-sided tolerance: no ledger baseline is consulted — an
    /// unattributable desktop floor is not a leak — but the config guard band is, so a
    /// swap cannot reach the card state the no-release path refuses (R2-01-01).
    ///
    /// A witness that dies mid-window fails **closed**, exactly as at admission: a
    /// confirmed release proves the port closed, never that the VRAM came back, and
    /// for a same-slot replacement this window is the only physical check the load
    /// ever gets. Only a deliberately silent (`ledger_only`) witness is waved
    /// through (review, 2026-10-26).
    pub(super) fn spawn_settle_task(&mut self, job_id: JobId) {
        let tolerance = self.cfg.witness_tolerance_gb;
        let reserve = self.cfg.vram_reserve_gb;
        let gate = self.jobs[&job_id].gate_target.unwrap_or(0.0);
        // The figure the window actually compares: the planned gate plus the reserve
        // band the admit-time gate subtracted, so the swap path keeps the same guard
        // band as the no-release path.
        let required = gate + reserve;
        let model = self.jobs[&job_id].model.clone();
        let deadline = Duration::from_secs(self.cfg.settle_timeout_s);
        let ledger_only = self.witness.ledger_only();
        let witness = Arc::clone(&self.witness);
        let tx = self.tx.clone();
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            loop {
                match witness.sample().await {
                    Ok(sample) => {
                        let observed_free = sample.free_gib();
                        if sample.freed_for(required, tolerance) {
                            if crate::observability::extended_logging() {
                                tracing::info!(
                                    used_gib = sample.used_gib,
                                    total_gib = sample.total_gib,
                                    free_gib = observed_free,
                                    gate_gib = gate,
                                    reserve_gib = reserve,
                                    required_gib = required,
                                    "settle evidence (extended)"
                                );
                            }
                            let _ = tx.send(ActorMsg::Event {
                                job: job_id,
                                event: Event::Settled(Ok(())),
                            });
                            return;
                        }
                    }
                    Err(reason) => {
                        if ledger_only {
                            tracing::debug!(reason = %reason,
                                "no reading to settle against (ledger-only witness); proceeding");
                            let _ = tx.send(ActorMsg::Event {
                                job: job_id,
                                event: Event::Settled(Ok(())),
                            });
                            return;
                        }
                        tracing::warn!(reason = %reason,
                            "witness lost during settle; holding the spawn until it answers");
                    }
                }
                if started.elapsed() >= deadline {
                    let numbers = match witness.sample().await {
                        Ok(sample) => {
                            let observed_free = sample.free_gib();
                            // G13: the figures the window actually compared, reserve
                            // band named inside the required figure.
                            let msg = format!(
                                "{:.1} GiB free after release, {:.1} GiB required incl. {:.1} GiB reserve (used {:.1} of {:.1})",
                                observed_free, required, reserve, sample.used_gib, sample.total_gib
                            );
                            // The load that never spawned must still be readable
                            // in the ring (§0.3 evidence, operator 2026-10-26).
                            tracing::warn!(
                                %job_id,
                                %model,
                                used_gib = sample.used_gib,
                                total_gib = sample.total_gib,
                                free_gib = observed_free,
                                gate_gib = gate,
                                reserve_gib = reserve,
                                required_gib = required,
                                "settle timeout: the card never freed the required space"
                            );
                            msg
                        }
                        Err(reason) => {
                            tracing::warn!(%job_id, %model, reason = %reason, gate_gib = gate,
                                reserve_gib = reserve, required_gib = required,
                                "settle timeout: the witness never answered again");
                            format!("witness lost during settle: {reason}")
                        }
                    };
                    let _ = tx.send(ActorMsg::Event {
                        job: job_id,
                        event: Event::Settled(Err(numbers)),
                    });
                    return;
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            }
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// The engine port the slot plan assigned to this job's slot (Step 8/11).
    fn slot_engine_port(&self, job_id: JobId) -> u16 {
        self.jobs
            .get(&job_id)
            .and_then(|job| job.slot)
            .map_or(0, |slot| self.slots[slot].engine_port)
    }

    /// Read-only launch preparation for a live job: adapter lookup, catalog lookup,
    /// the slot's engine port, and the adapter's own validation of the entry.
    ///
    /// A pure function of state — it mutates nothing, kills nothing and spawns
    /// nothing — so the actor can settle every launch-time refusal (chunk 10 [2])
    /// while the resident model and the planned victims are still alive. `begin_spawn`
    /// calls the same helper, so the pre-eviction refusal and the spawn cannot drift.
    ///
    /// # Arguments
    ///
    /// * `job_id` - a live job that has passed admission, in `Admit` or later.
    ///
    /// # Returns
    ///
    /// The [`LaunchSpec`] to hand the runtime, with the slot's engine port embedded,
    /// and the [`ModelEntry`] it was built from — the pair, never the spec alone, so
    /// the spawn's `MeasuredAnchor` provably comes from the row the argv came from.
    ///
    /// # Errors
    ///
    /// * [`SchedulerError::Spawn`] - no adapter is registered for the job's engine, or
    ///   the model vanished from the catalog mid-job.
    /// * [`SchedulerError::Engine`] - the adapter refused the entry (`MissingField` for
    ///   a key it requires, `Rejected` for an illegal or absent value).
    pub(super) fn preflight_spec(
        &self,
        job_id: JobId,
    ) -> Result<(LaunchSpec, ModelEntry), SchedulerError> {
        let (engine, model) = {
            let job = self
                .jobs
                .get(&job_id)
                .expect("invariant: callers hold live job ids");
            (job.engine.clone(), job.model.clone())
        };
        let Some(adapter) = self.adapters.get(&engine).cloned() else {
            return Err(SchedulerError::Spawn(format!(
                "no adapter registered for engine `{engine}`"
            )));
        };
        let Some(entry) = self.catalog.iter().find(|m| m.name == model).cloned() else {
            return Err(SchedulerError::Spawn(format!(
                "model `{model}` vanished from the catalog mid-job"
            )));
        };
        let engine_port = self.slot_engine_port(job_id);
        adapter
            .spec(&entry, engine_port)
            .map(|spec| (spec, entry))
            .map_err(SchedulerError::from)
    }

    /// `Dispose -> Spawn` edge (G12): only called after release is confirmed, or when
    /// there was nothing to release.
    pub(super) fn begin_spawn(&mut self, job_id: JobId) {
        self.step(job_id, SwapState::Spawn);
        let engine_port = self.slot_engine_port(job_id);
        let spawn_error = |reason: String| Event::Spawned(Err(SpawnError::SpawnFailed { reason }));
        let (spec, entry) = match self.preflight_spec(job_id) {
            Ok(launch) => launch,
            Err(error) => {
                // The `Spawn` arm already reads like a spawn failure; anything else
                // (an adapter refusal) is quoted verbatim, as before.
                let reason = match &error {
                    SchedulerError::Spawn(reason) => reason.clone(),
                    other => other.to_string(),
                };
                self.on_event(job_id, spawn_error(reason));
                return;
            }
        };
        // The provenance type carries the container identity (review, 2026-10-26):
        // the runtime reaps by the ticket's label, never by re-reading the spec.
        let kind = match &spec.container {
            Some(name) => ResourceKind::Container { name: name.clone() },
            None => ResourceKind::ChildProcess,
        };
        let ticket = SpawnTicket {
            generation: self.next_gen,
            kind,
            pid: 0,
            port: engine_port,
        };
        self.next_gen += 1;
        {
            let job = self
                .jobs
                .get_mut(&job_id)
                .expect("invariant: caller holds a live job id");
            job.spawned = Some(ticket.clone());
            job.readiness = Some(spec.readiness.clone());
            job.container = spec.container.clone();
            // The measured anchor is taken from the same entry the argv above was
            // built from, in the same actor turn: what the fold may later say about
            // this process is fixed here, and no `reload()` can move it.
            job.measured = Some(MeasuredAnchor::at_spawn(&entry));
        }
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let handle = tokio::spawn(async move {
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Spawned(runtime.spawn(&spec, ticket).await),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Readiness loop as a task; sends exactly one terminal event (Step 7 + §0.3).
    pub(super) fn spawn_probe_task(&mut self, job_id: JobId, port: u16) {
        let readiness = self.jobs[&job_id]
            .readiness
            .clone()
            .expect("invariant: readiness is set when a spawn exists");
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let container = self.jobs[&job_id].container.clone();
        let deadline = Duration::from_secs(
            self.jobs[&job_id]
                .load_timeout_s
                .unwrap_or(self.cfg.load_timeout_s),
        );
        let handle = tokio::spawn(async move {
            let started = Instant::now();
            let mut discovered = None;
            let terminal = loop {
                match runtime.probe(port, &readiness).await {
                    Probe::Ready => {
                        // Step 10 (F-4): the upstream id is discovered, not asserted.
                        discovered = runtime.discover_model_id(port).await;
                        break Probe::Ready;
                    }
                    Probe::Failed { reason } => break Probe::Failed { reason },
                    Probe::NotYet { reason } => {
                        if started.elapsed() >= deadline {
                            break Probe::Failed {
                                reason: format!(
                                    "readiness deadline {deadline:?} exceeded; last: {reason}"
                                ),
                            };
                        }
                    }
                }
                tokio::time::sleep(PROBE_INTERVAL).await;
            };
            // Ruling 1(b): evidence-only annotation on a decided failure. A
            // container-backed spawn gets one look at `docker port`, because
            // "the container is up but published on another port" is precisely
            // what a port probe cannot see; the verdict is not touched — the
            // ready path never reaches here, and the annotation only ever
            // rides an already-Failed `reason`.
            let terminal = match terminal {
                Probe::Failed { reason } => {
                    let reason = match container.as_deref() {
                        Some(name) => match runtime.published_elsewhere(name, port).await {
                            Some(evidence) => format!("{reason}; {evidence}"),
                            None => reason,
                        },
                        None => reason,
                    };
                    Probe::Failed { reason }
                }
                ready => ready,
            };
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Probed(terminal, discovered),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Whether a reap error on one target should abort the whole reap.
    ///
    /// A stale ticket means the process this job meant is already gone — there is nothing
    /// left to reap for that target, and aborting here would strand the job's OTHER
    /// target, its own live engine (chunk 2, 2026-10-27). Any other failure is real and
    /// must surface.
    pub(super) fn reap_should_continue(error: &SpawnError) -> bool {
        matches!(error, SpawnError::StaleTicket { .. })
    }

    /// Kill every process this job owns (its pending victim and its own spawn) and
    /// confirm release with evidence, inside `release_timeout_s` (G12).
    pub(super) fn spawn_kill_confirm(&mut self, job_id: JobId) {
        // Both targets, never one chosen by preference. `job.victim` is only ever
        // assigned (when a victim is queued or dequeued) and never cleared, so the old
        // `match job.victim.as_ref()` re-killed a victim that had already been released
        // and never killed the engine this job had itself started — leaving it holding
        // the card and its port while the slot read empty (review CRITICAL, 2026-10-27).
        let targets: Vec<Victim> = {
            let job = &self.jobs[&job_id];
            let mut targets: Vec<Victim> = Vec::with_capacity(2);
            if let Some(victim) = job.victim.clone() {
                targets.push(victim);
            }
            if let Some(ticket) = job.spawned.clone() {
                if !targets.iter().any(|t| t.ticket == ticket) {
                    targets.push(Victim {
                        model: job.model.clone(),
                        ticket,
                        // Unused on this path: a job reaping its own spawn has its slot
                        // cleared by its own transition, not by the victim-slot check.
                        slot: job.slot.unwrap_or(0),
                        evicted: false,
                        cost: 0.0,
                    });
                }
            }
            targets
        };
        if targets.is_empty() {
            tracing::warn!(
                %job_id,
                "nothing to reap: release phase with neither a victim nor a spawn"
            );
            return;
        }
        let tx = self.tx.clone();
        let runtime = Arc::clone(&self.runtime);
        let timeout = Duration::from_secs(self.cfg.release_timeout_s);
        let handle = tokio::spawn(async move {
            let holder = targets
                .iter()
                .map(|t| t.model.clone())
                .collect::<Vec<_>>()
                .join("+");
            let outcome = match tokio::time::timeout(timeout, async {
                for target in &targets {
                    if let Err(error) = runtime.kill(&target.ticket).await {
                        if !Self::reap_should_continue(&error) {
                            return Err(error);
                        }
                        tracing::warn!(
                            port = target.ticket.port,
                            generation = target.ticket.generation,
                            model = %target.model,
                            "stale reap target: its process is already gone, reaping the rest"
                        );
                    }
                }
                for target in &targets {
                    loop {
                        match runtime.release_confirmed(&target.ticket).await {
                            Ok(true) => break,
                            Ok(false) => tokio::time::sleep(RELEASE_POLL).await,
                            Err(error) if Self::reap_should_continue(&error) => {
                                tracing::warn!(
                                    port = target.ticket.port,
                                    "stale reap target: nothing left to confirm"
                                );
                                break;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                }
                Ok(())
            })
            .await
            {
                Ok(inner) => inner,
                Err(_) => Err(SpawnError::KillFailed {
                    holder,
                    reason: "release not confirmed within release_timeout_s".to_string(),
                }),
            };
            let _ = tx.send(ActorMsg::Event {
                job: job_id,
                event: Event::Released(outcome),
            });
        });
        self.jobs
            .get_mut(&job_id)
            .expect("invariant: caller holds a live job id")
            .tasks
            .push(handle);
    }

    /// Whether the late-release watch of R2-02-02 has spent its bound: exactly one
    /// `release_timeout_s` window measured from the watch start — the same budget the
    /// job's own reap already had. Pure so the bound is pinned by a unit test without
    /// wall-clock waits; the production window is never shortened for a test.
    pub(super) fn late_release_expired(elapsed: Duration, window: Duration) -> bool {
        elapsed >= window
    }

    /// Whether the late-release watch of R2-02-02 may name this ticket (REV8-01).
    ///
    /// The watch exists for the **uninstalled spawn**: an engine the scheduler no longer
    /// owns and can never reap again. Where the ticket *is* the slot's installed occupant
    /// — a cancel that reached `Serving` and whose own reap could not prove the death —
    /// G12 deliberately keeps that occupant installed, `/har/status` shows the model
    /// serving, and a later restart or swap re-owns the ticket; a watch there would warn
    /// about a runtime entry the operator is being told is serving, so it stays out and
    /// the installed occupant remains the record. An occupant carrying a DIFFERENT ticket
    /// (a restart's predecessor, a cross-category victim still in its own slot) does not
    /// blind the watch: that other engine is what the slot reports, not this orphan.
    ///
    /// Identity is `(port, generation)` — what the runtime table keys on, and exactly what
    /// [`ActorState::install`] copies out of `job.spawned`.
    pub(super) fn late_watch_names_an_orphan(
        installed: Option<&SpawnTicket>,
        ticket: &SpawnTicket,
    ) -> bool {
        !installed.is_some_and(|occupant| {
            occupant.port == ticket.port && occupant.generation == ticket.generation
        })
    }

    /// R2-02-02 option B (owner ruling 2026-11-03): one bounded, ticket-scoped watch on
    /// a release the job terminalized without proving.
    ///
    /// A job that started an engine but never installed it and then lost its reap to the
    /// release deadline leaves the truth "the holder is still alive" (DOCTRINE 5)
    /// recorded nowhere the scheduler owns: the slot reads empty, the row is terminal,
    /// `apply_event` refuses late events for a terminal job, and no kill path can name
    /// the ticket again. The card's physical reading still protects admission, so the
    /// residue is one unaccounted harness-owned resident plus one misleading readiness
    /// failure on that port's next load. This watch is the notice, not the fix: it
    /// observes only. It never re-kills, never touches a successor, and never changes
    /// slot, occupant, job, or ledger state — every exit is a single `warn`.
    ///
    /// Exactly one watch per ticket follows from the funnel: `finish` terminalizes a job
    /// once and `apply_event` drops every later event, and a ticket belongs to one job.
    ///
    /// # Arguments
    ///
    /// * `job_id` — the job that is terminalizing on an unconfirmed release. Nothing is
    ///   watched when the job never spawned (it owes no release) and when its ticket is
    ///   the slot's installed occupant (REV8-01, [`ActorState::late_watch_names_an_orphan`]).
    ///
    /// The task is deliberately NOT parked on `job.tasks`: `finish` aborts those on the
    /// very step that terminalizes the job, which is the instant this watch starts (the
    /// same reason as the R9 C1 snapshot). Its bound is one `release_timeout_s` window
    /// polled at `RELEASE_POLL`, and REV8-03 wraps the loop in a hard outer timeout of
    /// the same window plus one poll, so even a `release_confirmed` that never returns
    /// cannot hold the task past its bound: no unbounded task exists.
    pub(super) fn spawn_late_release_watch(&self, job_id: JobId) {
        let Some((ticket, model, installed)) = self.jobs.get(&job_id).and_then(|job| {
            let ticket = job.spawned.clone()?;
            let installed = job
                .slot
                .and_then(|slot| self.slots.get(slot))
                .and_then(|slot| slot.occupant.as_ref())
                .map(|occupant| occupant.ticket.clone());
            Some((ticket, job.model.clone(), installed))
        }) else {
            return;
        };
        if !Self::late_watch_names_an_orphan(installed.as_ref(), &ticket) {
            // REV8-01: this ticket is what the slot still reports as installed. The
            // occupant is the operator-visible record, the runtime entry must not be
            // talked about as gone while the page says serving, and the next restart or
            // swap owns this ticket again — so no watch, and no new warning either.
            return;
        }
        let runtime = Arc::clone(&self.runtime);
        let window = Duration::from_secs(self.cfg.release_timeout_s);
        tokio::spawn(async move {
            let started = tokio::time::Instant::now();
            // The inner loop is the ordinary path: it returns `Err(())` only when the
            // window is spent with nobody warned yet, and `Ok(())` when it already named
            // its own outcome. REV8-03: the outer timeout is the hard backstop for a
            // `release_confirmed` that never returns — the bound cannot be outlived from
            // inside the runtime call either. Both bound exits share one warning below,
            // so exactly one warn is ever emitted for the spent window.
            let spent = tokio::time::timeout(window + RELEASE_POLL, async {
                loop {
                    match runtime.release_confirmed(&ticket).await {
                        Ok(true) => {
                            // REV8-02: a state claim, never a causation claim. `Ok(true)`
                            // is also what the runtime answers for a port it never owned
                            // and for an entry already replaced, so this watch cannot
                            // credit itself with clearing anything.
                            tracing::warn!(
                                port = ticket.port,
                                generation = ticket.generation,
                                model = %model,
                                "late release evidence reads confirmed: no runtime entry for \
                                 this generation remains and no claim of ours stands on it"
                            );
                            return Ok(());
                        }
                        Ok(false) => {
                            if Self::late_release_expired(started.elapsed(), window) {
                                return Err(());
                            }
                            tokio::time::sleep(RELEASE_POLL).await;
                        }
                        Err(error) if Self::reap_should_continue(&error) => {
                            tracing::warn!(
                                port = ticket.port,
                                generation = ticket.generation,
                                model = %model,
                                error = %error,
                                "late release watch stopped: this ticket was superseded, and \
                                 the engine now on that port is not this watch's to touch"
                            );
                            return Ok(());
                        }
                        Err(error) => {
                            tracing::warn!(
                                port = ticket.port,
                                generation = ticket.generation,
                                model = %model,
                                error = %error,
                                "late release watch stopped: release evidence could not be read"
                            );
                            return Ok(());
                        }
                    }
                }
            })
            .await;
            if matches!(spent, Err(_) | Ok(Err(()))) {
                tracing::warn!(
                    port = ticket.port,
                    generation = ticket.generation,
                    model = %model,
                    "release still unconfirmed after one more release_timeout_s: the harness \
                     owns a resident nobody can reap; the next load on this port or the boot \
                     reclaim must reap it"
                );
            }
        });
    }
}
