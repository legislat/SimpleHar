//! Actor-side occupant, sampler, and container-log lifecycle: the residency
//! clear, the released-victim slot clear, the sampler retire, the R9 one-shot
//! container report, and the failed-launch log snapshot. The state and its
//! fields stay owned by `mod.rs`.

use crate::engine::{EngineMeasurement, EngineRuntime, MeasurementSource};
use crate::ownership::SpawnTicket;
use std::sync::Arc;
use tokio::sync::mpsc;

use super::measured::SamplerCtx;
use super::{ActorMsg, ActorState, JobId, SnapshotRelease};

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

    /// Clears a victim's own slot once its release is confirmed (same-slot replacement or
    /// a cross-category evictee — never the job's slot blindly), and only while the
    /// occupant there is still the one this ticket owns. Shared by the two arms that
    /// accept a confirmed release, so a cancelled swap cannot leave a reaped engine
    /// installed (chunk 4 [1]).
    ///
    /// # Arguments
    ///
    /// * `job_id` - the job whose `victim` has just been released.
    pub(super) fn clear_released_victim_slot(&mut self, job_id: JobId) {
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

    /// The one-shot process-engine measurement (`TASK-ENGINE-MEASUREMENT`,
    /// ORCHESTRATION §3) for the non-container engines `basalt`/`strata`,
    /// mirroring [`Self::report_container`]: fetch the engine's own status
    /// snapshot once at readiness (basalt `/v1/status`, strata `/status`) and
    /// hand the parsed figure to the actor; `Some(())` ends the sampler the
    /// way ninfer's `engine_report` ends it. Every refusal returns `None` so
    /// the witness loop stands in rather than losing the measurement outright
    /// (review 2026-10-31 M1). The fall-through order is exact: the
    /// process-tree fallback is attempted ONLY when the status path yielded no
    /// measurement — a body the per-engine stub will not parse yet (phase 1
    /// stubs answer `None` for every body) or an engine that never answers. A
    /// PARSED figure that [`ActorState::measurable_figure`] would refuse is not
    /// retried through the fallback: the one-shot returns `None` here and the
    /// witness loop takes over, standing in with its own fresh readings. The
    /// fallback sums the witness's per-process bytes over the engine's own pid
    /// plus its direct children ([`EngineRuntime::descendant_pids`] — the
    /// strata.exe -> strata-vision family) and carries `ProcessTreeNvml`
    /// provenance; the engine's own status answer carries `EngineStatus`. The
    /// witness is sampled on both paths because its live reading is the bound
    /// the figure gate and the actor's fold both measure against (doctrine 4);
    /// a witness that never answers is silence here too, and the sampler's own
    /// loop ends on its first witness error, so nothing is silently lost.
    pub(super) async fn report_measurement(
        runtime: &Arc<dyn EngineRuntime>,
        tx: &mpsc::UnboundedSender<ActorMsg>,
        ctx: &SamplerCtx,
        witness: &Arc<dyn crate::budget::VramWitness>,
    ) -> Option<()> {
        let status_path = match ctx.engine.as_str() {
            "basalt" => "/v1/status",
            "strata" => "/status",
            _ => return None,
        };
        let sample = witness.sample().await.ok()?;
        let status = runtime
            .engine_status_json(ctx.ticket.port, status_path)
            .await;
        let parsed = status.as_ref().and_then(|body| {
            if ctx.engine == "basalt" {
                crate::engine::basalt::measurement_from_status(body)
            } else {
                crate::engine::strata::measurement_from_status(body)
            }
        });
        let measurement = match parsed {
            Some(measurement) => measurement,
            None => {
                // The witness-based process-tree sum: the engine's own pid plus
                // its direct children (one generation — never grandchildren).
                let mut pids = vec![ctx.ticket.pid];
                pids.extend(runtime.descendant_pids(ctx.ticket.pid).await);
                let own = sample.own_gib(&pids)?;
                EngineMeasurement {
                    gpu_gib: own,
                    ram_gib: None,
                    source: MeasurementSource::ProcessTreeNvml,
                }
            }
        };
        // Decide HERE whether the actor could ever book this figure: the
        // one-shot does not retry after it sends, so consuming the sampler on
        // a figure the actor would drop would silently skip the witness
        // stand-in too (review 2026-10-31, M1 — the same gate
        // `report_container` applies).
        Self::measurable_figure(measurement.gpu_gib, sample.total_gib)?;
        if tx
            .send(ActorMsg::MeasuredProcessReport {
                model: ctx.model.clone(),
                ticket: ctx.ticket.clone(),
                measurement,
                total_gib: sample.total_gib,
            })
            .is_err()
        {
            return Some(()); // actor gone
        }
        tracing::info!(
            model = %ctx.model,
            engine = %ctx.engine,
            gpu_gib = measurement.gpu_gib,
            source = ?measurement.source,
            "process engine measurement landed"
        );
        Some(())
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
