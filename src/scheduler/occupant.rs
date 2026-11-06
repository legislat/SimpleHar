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
                // FR-04: a cap-stopped (truncated) walk would under-count the
                // tree, so this measurement is refused outright — the `None`
                // below is the same stand-in posture the module already
                // carries for every refusal: the witness loop keeps the
                // measurement honest instead of losing it.
                let descendants = runtime.descendant_pids(ctx.ticket.pid).await;
                if Self::descendants_truncated(&descendants) {
                    tracing::warn!(
                        model = %ctx.model,
                        engine = %ctx.engine,
                        root_pid = ctx.ticket.pid,
                        cap = Self::DESCENDANT_REPORT_CAP,
                        "process-tree measurement refused: the descendant walk was \
                         truncated; the witness loop stands in"
                    );
                    return None;
                }
                let mut pids = vec![ctx.ticket.pid];
                pids.extend(descendants);
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
        let deadline = tokio::time::Instant::now() + super::SAMPLER_LIFETIME_BOUND;
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
            let hysteresis =
                Self::ELASTIC_FALL_FLOOR_GIB.max(sample.total_gib * Self::ELASTIC_FALL_RELATIVE);
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

    /// Absolute floor of the elastic card ladder's fall band, in GiB (the twin
    /// of [`Self::RAM_REBOOK_BAND_GIB`] for the asymmetric card rule): a measured
    /// fall re-books only past the wider of this and [`Self::ELASTIC_FALL_RELATIVE`]
    /// of the card, so a 1 GiB step keeps page noise off the booking on a small card.
    const ELASTIC_FALL_FLOOR_GIB: f64 = 1.0;

    /// Relative floor of the elastic card ladder's fall band, as a share of the
    /// card's total (one hundredth): a large card gets a proportionally wider fall
    /// band, so a re-book needs a real working-set move, not a rounding step.
    const ELASTIC_FALL_RELATIVE: f64 = 0.01;

    /// The CPU-resident re-book band in GiB (`TASK-CPU-LLAMA-EVICTION` W2
    /// step 3): the smallest working-set move worth a fold, a persist, and a
    /// re-book. One GiB matches the elastic card ladder's floor step — page
    /// churn below it must not oscillate the booking or rewrite the yaml —
    /// and unlike [`ActorState::elastic_refresh`]'s asymmetric rule (rise
    /// free, fall banded) this band is SYMMETRIC: the measured working set is
    /// a noisy witness reading, not the engine's own static word, so a rise
    /// inside the band is noise too. The ruling's standing tolerance for
    /// growth past the booking is the GATE's reserve — the DESIGN ruling
    /// words it "The reserve band is the standing allowance" — not this
    /// sampler band, which only keeps noise out of the fold. The fold itself
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

    /// The TEST-ONLY one-call form of the guarded tree sample (FR-11), driven
    /// by the W2 tree-sum pins: the engine's own pid plus every descendant at
    /// every depth, summed as bytes (saturating — no working-set sum of a real
    /// tree overflows a u64, and a saturating miss is absence, not a wrapped
    /// negative). A pid the probe cannot read is skipped: a child that exited
    /// between the pid walk and the query shrinks the tree honestly. Nothing
    /// readable at all is absence: the caller treats it as the tree being gone
    /// and ends its sampler. A **truncated** walk (FR-04,
    /// [`Self::descendants_truncated`]) is also absence here — a partial tree
    /// is refused, never summed as if complete — but it is *not* the tree
    /// being gone, so production [`Self::ram_refresh`] runs the same two
    /// guarded legs split, letting a truncated tick skip instead of ending the
    /// sampler. The unit stays bytes until [`Self::working_set_to_gib`].
    #[cfg(test)]
    pub(super) async fn tree_working_set_gib(
        runtime: &Arc<dyn EngineRuntime>,
        probe: &Arc<dyn crate::process::WorkingSetProbe>,
        root_pid: u32,
    ) -> Option<crate::budget::Gibibytes> {
        let descendants = runtime.descendant_pids(root_pid).await;
        if Self::descendants_truncated(&descendants) {
            tracing::warn!(
                root_pid,
                cap = Self::DESCENDANT_REPORT_CAP,
                "the descendant walk stopped at its cap: the working-set sample is \
                 refused, never booked as complete"
            );
            return None;
        }
        Self::sum_tree_working_set_gib(root_pid, &descendants, probe)
    }

    /// The descendant-walk bound the Win32 Toolhelp answer is capped at
    /// (`DESCENDANT_CAP` in `src/process/win32.rs`): a walk answer reaching
    /// this length is the walk reporting a **partial** tree (FR-04 — the cap
    /// length is itself the truncation signal, since the walk never returns
    /// more). Callers refuse such an answer rather than treat it as complete;
    /// the bound is conservative in the safe direction (a pathologically
    /// complete tree of exactly cap size is declined too — the cost is one
    /// skipped sample, never a mis-attributed booking). The mirror is kept
    /// honest by `the_descendant_cap_mirror_tracks_the_win32_walk`, which
    /// reads the walk's source and fails on any drift.
    pub(super) const DESCENDANT_REPORT_CAP: usize = 4_096;

    /// Whether a descendant answer reached the cap and must be treated as a
    /// truncated (partial) tree (FR-04).
    #[must_use]
    pub(super) fn descendants_truncated(descendants: &[u32]) -> bool {
        descendants.len() >= Self::DESCENDANT_REPORT_CAP
    }

    /// Sum the working sets of the root pid and a KNOWN-COMPLETE descendant
    /// list (saturating bytes, unreadable pids skipped, one conversion at the
    /// end). The pure sum leg of the guarded tree walk: a caller that has
    /// already checked [`Self::descendants_truncated`] can keep its sampler
    /// alive across a refusal ([`Self::ram_refresh`] skips the tick instead of
    /// reading this `None` as the tree being gone).
    fn sum_tree_working_set_gib(
        root_pid: u32,
        descendants: &[u32],
        probe: &Arc<dyn crate::process::WorkingSetProbe>,
    ) -> Option<crate::budget::Gibibytes> {
        let mut total: Option<u64> = None;
        for pid in std::iter::once(root_pid).chain(descendants.iter().copied()) {
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
        let deadline = tokio::time::Instant::now() + super::SAMPLER_LIFETIME_BOUND;
        loop {
            tick.tick().await;
            if tokio::time::Instant::now() >= deadline {
                return;
            }
            let descendants = runtime.descendant_pids(ctx.ticket.pid).await;
            if Self::descendants_truncated(&descendants) {
                // FR-04: a cap-stopped walk is a partial tree — this sample
                // is refused and logged, never booked; the sampler survives
                // and the last honest figure stays booked (silence posture,
                // like every other skipped tick).
                tracing::warn!(
                    model = %ctx.model,
                    root_pid = ctx.ticket.pid,
                    cap = Self::DESCENDANT_REPORT_CAP,
                    "RAM sample refused: the descendant walk was truncated; skipping \
                     this tick, the last honest figure stays booked"
                );
                continue;
            }
            let Some(ram_gib) = Self::sum_tree_working_set_gib(ctx.ticket.pid, &descendants, probe)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use std::time::Duration;

    /// The cap-length answer the Win32 walk returns when it stops at the bound
    /// (FR-04): the partial tree the callers must refuse.
    fn capped_walk_answer() -> Vec<u32> {
        (1..=ActorState::DESCENDANT_REPORT_CAP as u32).collect()
    }

    /// Runtime stub answering `descendant_pids` from a scripted queue, then a
    /// fixed tail — the stand-in for a walk that truncates on one tick and
    /// answers completely on the next (a 4097-process tree cannot be spawned
    /// in a cardless test, so the caller's refusal is pinned through the
    /// injectable runtime seam, as the finding allows).
    struct ScriptedWalk {
        answers: Mutex<VecDeque<Vec<u32>>>,
        tail: Vec<u32>,
    }

    impl ScriptedWalk {
        fn new(answers: Vec<Vec<u32>>, tail: Vec<u32>) -> Self {
            Self {
                answers: Mutex::new(VecDeque::from(answers)),
                tail,
            }
        }
    }

    #[async_trait::async_trait]
    impl EngineRuntime for ScriptedWalk {
        async fn spawn(
            &self,
            _spec: &crate::engine::LaunchSpec,
            _ticket: SpawnTicket,
        ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
            panic!("the FR-04 pins never spawn")
        }
        async fn kill(&self, _ticket: &SpawnTicket) -> Result<(), crate::engine::SpawnError> {
            panic!("the FR-04 pins never kill")
        }
        async fn release_confirmed(
            &self,
            _ticket: &SpawnTicket,
        ) -> Result<bool, crate::engine::SpawnError> {
            panic!("the FR-04 pins never reach release evidence")
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &crate::engine::ReadinessSpec,
        ) -> crate::engine::Probe {
            panic!("the FR-04 pins never reach readiness")
        }
        async fn discover_model_id(&self, _port: u16) -> Option<String> {
            panic!("the FR-04 pins never reach readiness")
        }
        async fn descendant_pids(&self, _pid: u32) -> Vec<u32> {
            let mut answers = self.answers.lock().expect("script lock");
            answers.pop_front().unwrap_or_else(|| self.tail.clone())
        }
    }

    /// Probe answering every pid with the same working set; counts the queries
    /// so a refusal can be pinned to happen BEFORE any per-pid work.
    struct FlatProbe {
        bytes: u64,
        calls: AtomicUsize,
    }

    impl FlatProbe {
        fn new(bytes: u64) -> Arc<Self> {
            Arc::new(Self {
                bytes,
                calls: AtomicUsize::new(0),
            })
        }
        fn calls(&self) -> usize {
            self.calls.load(Ordering::Relaxed)
        }
    }

    impl crate::process::WorkingSetProbe for FlatProbe {
        fn working_set_bytes(&self, _pid: u32) -> Option<u64> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Some(self.bytes)
        }
    }

    /// Witness stub: a healthy 72 GiB card attributing 5.0 GiB to pid 4242.
    struct StubCard;

    #[async_trait::async_trait]
    impl crate::budget::VramWitness for StubCard {
        async fn sample(&self) -> Result<crate::budget::MemorySample, String> {
            let processes = vec![(4242u32, 5.0f64)];
            Ok(crate::budget::MemorySample {
                total_gib: 72.0,
                used_gib: 40.0,
                attribution: crate::budget::attribution_from_processes(&processes),
                processes,
            })
        }
        fn source(&self) -> String {
            "fr04-stub".to_string()
        }
    }

    fn fr04_ticket() -> SpawnTicket {
        SpawnTicket {
            generation: 7,
            kind: crate::ownership::ResourceKind::ChildProcess,
            pid: 4242,
            port: 19_401,
        }
    }

    fn fr04_ctx(model: &str, engine: &str) -> SamplerCtx {
        SamplerCtx {
            model: model.to_string(),
            ticket: fr04_ticket(),
            own_pids: Vec::new(),
            container: None,
            engine: engine.to_string(),
        }
    }

    /// FR-04: an answer that reached the walk's cap is a partial tree — the
    /// tree-sum refuses it outright, before a single per-pid query, and never
    /// books it; a tree under the cap sums exactly as before. Falsifiers:
    /// drop the `descendants_truncated` guard and the first assert sees a
    /// 4097-pid sum with every probe queried; invert the predicate and the
    /// honest-tree leg refuses.
    #[tokio::test]
    async fn a_truncated_descendant_walk_is_refused_never_summed() {
        let runtime = Arc::new(ScriptedWalk::new(
            vec![capped_walk_answer()],
            capped_walk_answer(),
        )) as Arc<dyn EngineRuntime>;
        let probe = FlatProbe::new(1 << 30);
        let probe_dyn = Arc::clone(&probe) as Arc<dyn crate::process::WorkingSetProbe>;
        assert_eq!(
            ActorState::tree_working_set_gib(&runtime, &probe_dyn, 4242).await,
            None,
            "a cap-length answer is truncation: the sample is refused"
        );
        assert_eq!(
            probe.calls(),
            0,
            "the refusal must precede every per-pid query"
        );
        let runtime =
            Arc::new(ScriptedWalk::new(vec![vec![4243]], Vec::new())) as Arc<dyn EngineRuntime>;
        let probe = FlatProbe::new(1 << 30);
        let probe_dyn = Arc::clone(&probe) as Arc<dyn crate::process::WorkingSetProbe>;
        assert_eq!(
            ActorState::tree_working_set_gib(&runtime, &probe_dyn, 4242).await,
            Some(crate::budget::Gibibytes(2.0)),
            "a complete tree under the cap still sums root plus descendants"
        );
    }

    /// FR-04: a truncated RAM tick is skipped and LOGGED while the sampler
    /// keeps polling — the next honest tick re-books. Falsifiers: read the
    /// refusal as tree-gone (`return` instead of `continue`) and the message
    /// never arrives; send the truncated sample through the band check and
    /// the 4097-pid figure books.
    #[tokio::test]
    async fn a_truncated_ram_tick_is_skipped_and_the_sampler_keeps_polling() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
        let runtime = Arc::new(ScriptedWalk::new(vec![capped_walk_answer()], vec![4243]))
            as Arc<dyn EngineRuntime>;
        let probe = FlatProbe::new(1 << 30);
        let probe_dyn = Arc::clone(&probe) as Arc<dyn crate::process::WorkingSetProbe>;
        let ctx = fr04_ctx("fr04-ram", "llama");
        let sampler = tokio::spawn(async move {
            ActorState::ram_refresh(
                &runtime,
                &tx,
                &ctx,
                &probe_dyn,
                8.0,
                Duration::from_millis(40),
            )
            .await;
        });
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the sampler survives the refused tick and re-books")
            .expect("actor channel open");
        let ActorMsg::MeasuredRam { ram_gib, .. } = msg else {
            panic!("expected a RAM reading");
        };
        assert!(
            (ram_gib.0 - 2.0).abs() < 1e-9,
            "the honest sample after the refusal books whole: {}",
            ram_gib.0
        );
        assert!(
            probe.calls() >= 2,
            "the surviving tick really probed the tree"
        );
        sampler.abort();
    }

    /// FR-04: the process-tree measurement fallback refuses a truncated walk
    /// — the `None` is the module's established stand-in posture (the witness
    /// loop keeps the measurement honest), and nothing reaches the actor.
    /// Below the cap the same leg still lands its figure. Falsifiers: remove
    /// the guard and the truncated leg answers `Some` off a partial tree;
    /// return `Some` on refusal and the stand-in is consumed by a lie.
    #[tokio::test]
    async fn the_process_tree_measurement_refuses_a_truncated_walk() {
        let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
        let witness = Arc::new(StubCard) as Arc<dyn crate::budget::VramWitness>;
        let ctx = fr04_ctx("fr04-meas", "strata");
        let runtime = Arc::new(ScriptedWalk::new(
            vec![capped_walk_answer()],
            capped_walk_answer(),
        )) as Arc<dyn EngineRuntime>;
        assert_eq!(
            ActorState::report_measurement(&runtime, &tx, &ctx, &witness).await,
            None,
            "the witness-tree sum refuses a partial tree outright"
        );
        assert!(
            rx.try_recv().is_err(),
            "a refused measurement never reaches the actor"
        );
        let runtime =
            Arc::new(ScriptedWalk::new(vec![vec![4243]], Vec::new())) as Arc<dyn EngineRuntime>;
        assert_eq!(
            ActorState::report_measurement(&runtime, &tx, &ctx, &witness).await,
            Some(5.0),
            "below the cap the per-process tree figure still lands"
        );
        let msg = rx.recv().await.expect("measurement message");
        assert!(
            matches!(
                msg,
                ActorMsg::MeasuredProcessReport {
                    measurement: EngineMeasurement {
                        source: MeasurementSource::ProcessTreeNvml,
                        confidence: MeasurementConfidence::Low,
                        gpu_gib: 5.0,
                        ..
                    },
                    ..
                }
            ),
            "the landed figure keeps its tree provenance"
        );
    }

    /// FR-04 tripwire: the refusal side mirrors the Win32 walk's own cap
    /// constant. Falsifier: move `DESCENDANT_CAP` in `src/process/win32.rs`
    /// without moving the mirror and this pin goes red on the drift.
    #[test]
    fn the_descendant_cap_mirror_tracks_the_win32_walk() {
        let source = include_str!("../process/win32.rs");
        let tail = source
            .split("DESCENDANT_CAP: usize = ")
            .nth(1)
            .expect("the walk keeps declaring its cap");
        let declared: usize = tail
            .split(';')
            .next()
            .expect("the literal ends")
            .trim()
            .replace('_', "")
            .parse()
            .expect("the cap stays a plain integer literal");
        assert_eq!(
            declared,
            ActorState::DESCENDANT_REPORT_CAP,
            "the caller-side refusal must mirror the walk bound (FR-04)"
        );
    }
}
