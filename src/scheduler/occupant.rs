//! Actor-side occupant, sampler, and container-log lifecycle: the residency
//! clear, the released-victim slot clear, the sampler retire, the R9 one-shot
//! container report, and the failed-launch log snapshot. The state and its
//! fields stay owned by `mod.rs`.

use crate::engine::{EngineMeasurement, EngineRuntime, MeasurementConfidence, MeasurementSource};
use crate::ownership::SpawnTicket;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;

use super::measured::SamplerCtx;
use super::{ActorMsg, ActorState, JobId, JobKind, Occupant, SnapshotRelease, SwapState, Victim};

impl ActorState {
    /// The free memory the card had when this job's engine started: the admit reading
    /// corrected for everything the job's own release gave back. The admit reading
    /// alone still counts a victim's memory as occupied, which would under-measure
    /// every replacement load by the victim's whole footprint (review, 2026-10-27).
    pub(super) fn post_release_free(&self, job_id: JobId) -> Option<f64> {
        let job = self.jobs.get(&job_id)?;
        job.observed_at_admit
            .map(|observed| observed + job.released_cost)
    }

    /// Ends a slot's residency and stops its sampler. The sampler belongs to the
    /// engine that was there, not to whatever loads into the slot next, so every path
    /// that drops an occupant has to come through here (review, 2026-10-27).
    pub(super) fn clear_occupant(&mut self, slot: usize) {
        if let Some(occupant) = self.slots[slot].occupant.take() {
            // The occupant's silence state dies with it (FINDINGS-11 [2]):
            // no stale dedupe mark may outlive the spawn it described.
            self.serve_id_silence_warned
                .remove(&(occupant.ticket.port, occupant.ticket.generation));
            if let Some(task) = occupant.measure {
                task.abort();
            }
        }
    }

    /// Commits a driver-confirmed release of `job_id`'s current `victim`: the
    /// victim's own slot goes the way a completed release takes it (same-slot
    /// replacement or a cross-category evictee — never the job's slot blindly),
    /// and only while the occupant there is still the one this ticket owns, and
    /// the victim's own load row is terminalized as unloaded (EVID-01). Shared
    /// by the two arms that accept a confirmed release, so a cancelled swap
    /// cannot leave a reaped engine installed (chunk 4 [1]) or a gone model
    /// still reading `Serving` on the ledger (EVID-01 live proof: slot empty,
    /// job row `Serving`/`loaded`). Ownership resolves HERE and only here —
    /// one writer — see [`ActorState::terminalize_victim_load_row`].
    ///
    /// # Arguments
    ///
    /// * `job_id` - the job whose `victim` has just been released.
    pub(super) fn commit_victim_release(&mut self, job_id: JobId) {
        let Some(victim) = self.jobs.get(&job_id).and_then(|job| job.victim.clone()) else {
            return;
        };
        if self.slots[victim.slot]
            .occupant
            .as_ref()
            .is_some_and(|o| o.ticket == victim.ticket)
        {
            self.clear_occupant(victim.slot);
        }
        self.terminalize_victim_load_row(&victim);
    }

    /// EVID-01: a confirmed release means the victim's engine is gone, so the
    /// ledger may no longer claim its load still serves. The owning row keeps
    /// `terminal` (its swap ended at install; this is a terminal-ledger
    /// correction, not a state-machine transition — `Serving -> Idle` is no
    /// graph edge, which is why the row is rewritten here rather than walked
    /// through `step`), leaves `Serving`, and takes the honest word its release earned:
    /// `evicted` for a cross-category eviction, `unloaded` for the operator/
    /// same-slot path (the `victim.evicted` flag, set once by the eviction
    /// planner, is the one discriminator at this site). No error is set: a
    /// release is not a fault, and `fault_of` stays false.
    ///
    /// Ownership is resolved by the identity the victim record itself carries
    /// — `model` + `slot` name it and the spawn ticket makes it exact, the
    /// same `model` + `ticket` authority
    /// [`ActorState::container_reported_gb`] reads — never by pattern-matching
    /// ledger rows for "a row that looks old": a re-minted twin (same model,
    /// same slot, newer ticket) and the incoming job this very release belongs
    /// to (its own ticket, or none yet) can never match. `Serving` is the only
    /// state rewritten, which makes the commit idempotent: a row that already
    /// went terminal-unloaded here, or never served, stays untouched.
    fn terminalize_victim_load_row(&mut self, victim: &Victim) {
        let outcome = if victim.evicted {
            "evicted"
        } else {
            "unloaded"
        };
        let Some(row) = self.jobs.values_mut().find(|job| {
            job.kind == JobKind::Load
                && job.state == SwapState::Serving
                && job.model == victim.model
                && job.slot == Some(victim.slot)
                && job.spawned.as_ref() == Some(&victim.ticket)
        }) else {
            return;
        };
        row.state = SwapState::Idle;
        row.outcome = Some(outcome.to_string());
        tracing::info!(
            model = %victim.model,
            slot = victim.slot,
            outcome,
            "confirmed release terminalized the victim's own load row (EVID-01)"
        );
    }

    /// Takes and aborts the sampler of the serving occupant identified by
    /// `model`+`ticket`, so a reading with nowhere to go stops being produced rather
    /// than being discarded for 24 h (the fold's own guard list lives on
    /// [`ActorState::on_measured`]). Both anchor classes that cannot persist call this
    /// (FINDINGS2-04 [3]); it is idempotent, and the ticket keeps a re-minted twin's
    /// sampler untouched.
    pub(super) fn retire_sampler(&mut self, model: &str, ticket: &SpawnTicket) {
        let Some(slot) = self.slots.iter_mut().find(|slot| {
            slot.occupant
                .as_ref()
                .is_some_and(|o| o.model == model && &o.ticket == ticket)
        }) else {
            return;
        };
        if let Some(occupant) = slot.occupant.as_mut() {
            if let Some(sampler) = occupant.measure.take() {
                sampler.abort();
            }
        }
    }

    /// The R9 one-shot: fetch `docker logs <container>` at readiness, snapshot it for
    /// forensics, and hand the engine's own startup `MemorySummary` figure to the
    /// actor. `Some(())` means the sampler is finished for the footprint (the reserved
    /// figure is static per launch); `None` means fall back to the witness loop —
    /// whose readings a container spawn can only ever earn as `total_diff`
    /// (displayed, never booked; §5.2: an old build without the memory record, a
    /// container that will not answer, or a figure the actor would refuse all land
    /// here, after the snapshot is kept for forensics). The fetch rides the
    /// ownership-scoped WSL control-command path; the engine-specific parse lives in
    /// [`crate::engine::ninfer::parse_startup_memory`].
    pub(super) async fn report_container(
        runtime: &Arc<dyn EngineRuntime>,
        tx: &mpsc::UnboundedSender<ActorMsg>,
        ctx: &SamplerCtx,
    ) -> Option<()> {
        let container = ctx.container.as_deref()?;
        if ctx.engine != crate::ownership::NINFER_ENGINE {
            return None;
        }
        // The trust-site ownership gate (review-wave leftover): a container's
        // self-reserved figure books only when its name passes the very rule the
        // spawn path applies — [`crate::ownership::valid_container_name`], the one
        // validator the adapter spec, the pre-reap, the boot reclaim, and
        // `ProcessRuntime::container_report` all read, so no path can accept a name
        // another refuses. `ProcessRuntime` already refuses to fetch a hostile name;
        // this is the trust layer's own defense for any runtime implementation that
        // answers with bytes for a name this harness could never have spawned (an
        // ID-shaped name is the dangerous one: docker resolves it as a container-ID
        // prefix). Refusing here falls back to the witness stand-in, so the serve is
        // never harmed — only the self-reported booking is withheld.
        if !crate::ownership::valid_container_name(container) {
            tracing::warn!(
                model = %ctx.model,
                container,
                "container report ignored: not a name this harness may address"
            );
            return None;
        }
        let text = runtime.container_report(container).await?;
        runtime.save_container_log(&ctx.model, &text).await;
        let report = crate::engine::ninfer::parse_startup_memory(&text)?;
        let total_gib = report.total_device_memory_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        // Decide HERE whether the actor could ever book this figure: the one-shot
        // does not retry after it sends, so consuming the sampler on a figure the
        // actor would drop would silently skip the witness stand-in too (review
        // 2026-10-31, M1). A refused figure falls back; the actor still logs the
        // race-loss drop that cannot be predicted here.
        Self::measurable_figure(report.required_to_free_gb, total_gib)?;
        if tx
            .send(ActorMsg::MeasuredEngineReport {
                model: ctx.model.clone(),
                ticket: ctx.ticket.clone(),
                required_to_free_gb: report.required_to_free_gb,
                total_gib,
            })
            .is_err()
        {
            return Some(()); // actor gone
        }
        tracing::info!(
            model = %ctx.model,
            container,
            required_to_free_gb = report.required_to_free_gb,
            "engine reported its reserved VRAM footprint"
        );
        Some(())
    }

    /// The process-engine measurement landing (`TASK-ENGINE-MEASUREMENT`,
    /// ORCHESTRATION §3) for the non-container engines `basalt`/`strata`,
    /// mirroring [`Self::report_container`]: fetch the engine's own metrics
    /// snapshot once at readiness (both engines read their real surface,
    /// `GET /metrics` — the capture 2026-11-04 proved `/status` is liveness
    /// only and `/v1/status` reports no footprint) and hand the parsed
    /// figure to the actor. `Some(figure)` means the
    /// measurement landed; for the elastic `basalt`/`strata` rows the caller
    /// then hands the sampler to [`Self::elastic_refresh`] instead of ending
    /// it (phase 4: an elastic cache keeps moving after the first fold), while
    /// ninfer's `engine_report` one-shot semantics stay untouched. Every
    /// refusal returns `None` so the witness loop stands in rather than
    /// losing the measurement outright (review 2026-10-31 M1). The
    /// fall-through order is exact: the
    /// process-tree fallback is attempted ONLY when the metrics path yielded
    /// no measurement — a body the engine's own parse will not take (phases
    /// 2-5 parse the `/metrics` engine blocks now and answer `None` for
    /// every other shape) or an engine that never answers. A
    /// PARSED figure that [`ActorState::measurable_figure`] would refuse is not
    /// retried through the fallback: the one-shot returns `None` here and the
    /// witness loop takes over, standing in with its own fresh readings. The
    /// fallback sums the witness's per-process bytes over the engine's own pid
    /// plus ALL its descendants ([`EngineRuntime::descendant_pids`] — on the
    /// captured serves the engine hangs two levels below the harness child:
    /// python launcher -> python server -> engine, plus the engine's vision
    /// child) and carries `ProcessTreeNvml`
    /// provenance; the engine's own metrics answer carries `EngineStatus`. The
    /// witness is sampled on both paths because its live reading is the bound
    /// the figure gate and the actor's fold both measure against (doctrine 4);
    /// a witness that never answers is silence here too, and the sampler's own
    /// loop ends on its first witness error, so nothing is silently lost.
    pub(super) async fn report_measurement(
        runtime: &Arc<dyn EngineRuntime>,
        tx: &mpsc::UnboundedSender<ActorMsg>,
        ctx: &SamplerCtx,
        witness: &Arc<dyn crate::budget::VramWitness>,
    ) -> Option<f64> {
        let status_path = match ctx.engine.as_str() {
            "basalt" => "/metrics",
            "strata" => "/metrics",
            _ => return None,
        };
        let sample = witness.sample().await.ok()?;
        let status = runtime
            .engine_status_json(ctx.ticket.port, status_path)
            .await;
        let parsed = status.as_ref().and_then(|body| {
            if ctx.engine == "basalt" {
                crate::engine::basalt::measurement_from_metrics(body)
            } else {
                crate::engine::strata::measurement_from_metrics(body)
            }
        });
        let measurement = match parsed {
            Some(measurement) => measurement,
            None => {
                // The witness-based process-tree sum: the engine's own pid
                // plus every descendant at every depth — the captured
                // engines hang two levels below the harness child, so a
                // one-generation sum misses the process holding the card.
                let mut pids = vec![ctx.ticket.pid];
                pids.extend(runtime.descendant_pids(ctx.ticket.pid).await);
                let own = sample.own_gib(&pids)?;
                EngineMeasurement {
                    gpu_gib: own,
                    ram_gib: None,
                    source: MeasurementSource::ProcessTreeNvml,
                    confidence: MeasurementConfidence::Low,
                }
            }
        };
        // Decide HERE whether the actor could ever book this figure: a
        // consumed landing without a valid figure would silently skip the
        // witness stand-in too (review 2026-10-31, M1 — the same gate
        // `report_container` applies). The rounded figure is what the actor's
        // fold will book (its own gate rounds identically), so it is what the
        // caller seeds the elastic hysteresis with.
        let rounded = Self::measurable_figure(measurement.gpu_gib, sample.total_gib)?;
        if tx
            .send(ActorMsg::MeasuredProcessReport {
                model: ctx.model.clone(),
                ticket: ctx.ticket.clone(),
                measurement,
                total_gib: sample.total_gib,
            })
            .is_err()
        {
            return Some(rounded); // actor gone
        }
        tracing::info!(
            model = %ctx.model,
            engine = %ctx.engine,
            gpu_gib = measurement.gpu_gib,
            source = ?measurement.source,
            confidence = ?measurement.confidence,
            "process engine measurement landed"
        );
        Some(rounded)
    }

    /// The elastic refresh loop (`TASK-ENGINE-MEASUREMENT` phase 4): a
    /// `basalt`/`strata` row whose metrics figure landed keeps its sampler
    /// alive instead of ending it, because an elastic engine's footprint
    /// MOVES after load — the Strata expert cache (`--vram-elastic`) and the
    /// Basalt expert cache grow and shrink against the card while serving —
    /// and a booking frozen at the readiness reading re-creates the
    /// 2026-11-04 readmission incident with fresh numbers. Every `every`-spaced
    /// tick re-reads the engine's own metrics endpoint and re-folds only when
    /// the move crosses the hysteresis band:
    ///
    /// * **Up moves freely.** Any rise above the last booked figure re-folds
    ///   at once: under-booking is over-commitment, and reserving too much is
    ///   the one direction the card forgives.
    /// * **Down moves only past the band.** A fall re-folds when it exceeds
    ///   `max(1.0 GiB, 1% of the card)`. One GiB is the step scale of the
    ///   expert caches (sub-GiB churn must not oscillate the booking, nor
    ///   rewrite the yaml every minute), and 1% scales the band on cards
    ///   bigger than 100 GiB (1.28 GiB at 128). A fall still lands:
    ///   hysteresis delays release credit, it never denies it.
    ///
    /// Silence on any leg — the witness not answering (the same hard stop the
    /// witness loop takes: absence is never a guess, §0.3), a metrics body the
    /// parse refuses, or a figure the actor would drop — is absence: that
    /// tick is skipped and the poll continues, because the last honest figure
    /// stays booked and an engine mid-cache-rebuild answers again next tick.
    /// The loop ends with its occupant (every residency clear aborts the
    /// sampler task, `clear_occupant`) or with the 24 h wall-clock bound the
    /// witness loop has always carried.
    pub(super) async fn elastic_refresh(
        runtime: &Arc<dyn EngineRuntime>,
        tx: &mpsc::UnboundedSender<ActorMsg>,
        ctx: &SamplerCtx,
        witness: &Arc<dyn crate::budget::VramWitness>,
        mut booked: f64,
        every: std::time::Duration,
    ) {
        let status_path = match ctx.engine.as_str() {
            "basalt" => "/metrics",
            "strata" => "/metrics",
            _ => return,
        };
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first interval tick completes at once and is consumed: the fold
        // that handed this loop `booked` IS the reading that tick would take.
        tick.tick().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(86_400);
        loop {
            tick.tick().await;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let Ok(sample) = witness.sample().await else {
                return; // a witness that cannot answer yields no measurement,
                        // never a false one — same posture as the witness loop
            };
            if !sample.total_gib.is_finite() || sample.total_gib <= 0.0 {
                continue;
            }
            let status = runtime
                .engine_status_json(ctx.ticket.port, status_path)
                .await;
            let parsed = status.as_ref().and_then(|body| {
                if ctx.engine == "basalt" {
                    crate::engine::basalt::measurement_from_metrics(body)
                } else {
                    crate::engine::strata::measurement_from_metrics(body)
                }
            });
            let Some(measurement) = parsed else {
                continue;
            };
            let Some(rounded) = Self::measurable_figure(measurement.gpu_gib, sample.total_gib)
            else {
                continue;
            };
            let hysteresis = 1.0f64.max(sample.total_gib * 0.01);
            let rise = rounded > booked;
            let fall = booked - rounded > hysteresis;
            if !(rise || fall) {
                continue;
            }
            if tx
                .send(ActorMsg::MeasuredProcessReport {
                    model: ctx.model.clone(),
                    ticket: ctx.ticket.clone(),
                    measurement,
                    total_gib: sample.total_gib,
                })
                .is_err()
            {
                return; // actor gone
            }
            tracing::info!(
                model = %ctx.model,
                engine = %ctx.engine,
                gpu_gib = rounded,
                was_gib = booked,
                hysteresis_gib = hysteresis,
                "elastic status refresh re-booked the resident footprint"
            );
            booked = rounded;
        }
    }

    /// The CPU-resident re-book band in GiB (`TASK-CPU-LLAMA-EVICTION` W2
    /// step 3): the smallest working-set move worth a fold, a persist, and a
    /// re-book. One GiB matches the elastic card ladder's floor step — page
    /// churn below it must not oscillate the booking or rewrite the yaml —
    /// and unlike [`ActorState::elastic_refresh`]'s asymmetric rule (rise
    /// free, fall banded) this band is SYMMETRIC: the measured working set is
    /// a noisy witness reading, not the engine's own static word, and the
    /// DESIGN ruling names the reserve band the standing tolerance in both
    /// directions, so a rise inside the band is noise too. The fold itself
    /// stays pure bookkeeping (it re-books on any changed figure); the band
    /// keeps the churn out of the SAMPLER.
    pub(super) const RAM_REBOOK_BAND_GIB: f64 = 1.0;

    /// The band predicate, split from the loop so the cadence is testable
    /// without a runtime, a probe, or a clock.
    pub(super) fn ram_rebook_due(current: f64, booked: f64) -> bool {
        (current - booked).abs() > Self::RAM_REBOOK_BAND_GIB
    }

    /// The ONE place working-set bytes become GiB: a binary gibibyte, not the
    /// decimal gigabyte a `MiB/1024`-style parser would sneak in. The probe
    /// answers raw bytes (the Win32 `QUERY_MEMORY_INFORMATION` unit) and the
    /// actor consumes the GiB newtype, so no MiB or byte figure ever crosses
    /// a task or fold boundary (W2 step 3, falsifier-pinned).
    pub(super) fn working_set_to_gib(bytes: u64) -> crate::budget::Gibibytes {
        crate::budget::Gibibytes(bytes as f64 / (1024.0f64 * 1024.0 * 1024.0))
    }

    /// The engine's own pid plus every descendant at every depth, summed as
    /// bytes (saturating — no working-set sum of a real tree overflows a u64,
    /// and a saturating miss is absence, not a wrapped negative). A pid the
    /// probe cannot read is skipped: a child that exited between the pid walk
    /// and the query shrinks the tree honestly. Nothing readable at all is
    /// absence: the caller treats it as the tree being gone and ends its
    /// sampler. The unit stays bytes until [`Self::working_set_to_gib`].
    pub(super) async fn tree_working_set_gib(
        runtime: &Arc<dyn EngineRuntime>,
        probe: &Arc<dyn crate::process::WorkingSetProbe>,
        root_pid: u32,
    ) -> Option<crate::budget::Gibibytes> {
        let mut pids = vec![root_pid];
        pids.extend(runtime.descendant_pids(root_pid).await);
        let mut total: Option<u64> = None;
        for pid in pids {
            if let Some(bytes) = probe.working_set_bytes(pid) {
                total = Some(total.map_or(bytes, |acc| acc.saturating_add(bytes)));
            }
        }
        total.map(Self::working_set_to_gib)
    }

    /// The CPU-resident refresh loop (`TASK-CPU-LLAMA-EVICTION` W2 step 3):
    /// the twin of [`ActorState::elastic_refresh`] for the RAM ladder, with
    /// the engine's metrics endpoint replaced by the process-tree working-set
    /// probe and the asymmetric band replaced by the symmetric
    /// [`Self::ram_rebook_due`] one. `booked` starts at the admit-time RAM
    /// booking — a floor, not a measurement — so the first real sample arrives
    /// one `every` after install (the immediate interval tick is consumed,
    /// the posture every sampler loop here carries: the state the load was
    /// admitted in is not a serve-window reading). A reading crossing the
    /// band is sent to the actor as `per_process` tree truth — the sampler is
    /// the only writer of the `measured_ram_gb` pair and it never mints a
    /// `total_diff` — and the fold it lands on re-books the resident through
    /// the same max-truth ladder a fresh admit walks. Silence is a skip and
    /// the last honest figure stays booked; the tree being gone (no pid
    /// answerable at all) ends the loop, like any sampler whose subject
    /// died; and the loop carries the same 24 h wall-clock bound, because a
    /// serve is not a promise of eternity.
    pub(super) async fn ram_refresh(
        runtime: &Arc<dyn EngineRuntime>,
        tx: &mpsc::UnboundedSender<ActorMsg>,
        ctx: &SamplerCtx,
        probe: &Arc<dyn crate::process::WorkingSetProbe>,
        mut booked: f64,
        every: std::time::Duration,
    ) {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(86_400);
        loop {
            tick.tick().await;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let Some(ram_gib) = Self::tree_working_set_gib(runtime, probe, ctx.ticket.pid).await
            else {
                return; // the tree this sampler belongs to is gone
            };
            if !Self::ram_rebook_due(ram_gib.0, booked) {
                continue;
            }
            if tx
                .send(ActorMsg::MeasuredRam {
                    model: ctx.model.clone(),
                    ticket: ctx.ticket.clone(),
                    ram_gib,
                    attribution: crate::model::MeasuredAttribution::PerProcess,
                })
                .is_err()
            {
                return; // actor gone
            }
            tracing::info!(
                model = %ctx.model,
                ram_gib = ram_gib.0,
                was_gib = booked,
                band_gib = Self::RAM_REBOOK_BAND_GIB,
                "CPU-resident working set crossed the re-book band"
            );
            booked = ram_gib.0;
        }
    }

    /// Snapshots a failed launch's container log (R9, C1): the autostart pre-reap the
    /// NEXT launch runs is `docker rm -f`, un-interceptable, so the engine evidence of
    /// a failed start must be on the Windows side before this job's reap completes —
    /// never on graceful unload, where there is nothing left to forensicate. Best
    /// effort: a container that never came up simply yields no log. The task is
    /// deliberately NOT parked on `job.tasks`: `finish` aborts those the moment the
    /// release lands, which is routinely before a 10 s `docker logs` returns — the
    /// one piece of evidence the pre-reap destroys would be the one thing the harness
    /// cancelled. It is bounded by the control-command timeout instead.
    pub(super) fn snapshot_failed_container(&mut self, job_id: JobId) {
        let Some((container, model)) = self
            .jobs
            .get(&job_id)
            .and_then(|job| job.container.clone().map(|c| (c, job.model.clone())))
        else {
            return;
        };
        if !self.snapshot_in_flight.insert(container.clone()) {
            return; // a fetch for this container is already running
        }
        let runtime = Arc::clone(&self.runtime);
        let tx = self.tx.clone();
        let released = container.clone();
        tokio::spawn(async move {
            // RAII release: the in-flight mark clears on EVERY exit of this task —
            // normal end, early return, or panic — so a wedged mark can never
            // silently blind every later failure of this container name (review
            // 2026-10-31, L9).
            let _guard = SnapshotRelease {
                tx,
                container: released,
            };
            if let Some(text) = runtime.container_report(&container).await {
                runtime.save_container_log(&model, &text).await;
            }
        });
    }
}

/// The occupant's last use: the proxy-side hint when present, else load time.
pub(super) fn state_last_used(
    occupant: &Occupant,
    last_used: &HashMap<String, Instant>,
) -> Instant {
    last_used
        .get(&occupant.model)
        .copied()
        .unwrap_or(occupant.loaded_at)
        .max(occupant.loaded_at)
}
