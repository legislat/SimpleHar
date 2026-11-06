//! The wire half: the mailbox vocabulary, the step-result events, and the three
//! release guards. Pure data plus Drop: the only reach is the channel send.
//!
//! Owns the release-per-exit rule (R9 C1, R5, FINDINGS-06 [8]): each guard is
//! the sole sender of its `*Done` answer, so every exit of a spawned task lands
//! exactly one release message and an in-flight mark can never wedge.

use crate::engine::{Probe, SpawnError, Spawned};
use crate::model::scan::{Catalog, ScanError};
use crate::ownership::SpawnTicket;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use super::{Command, DriftStats, JobId, MeasuredData, SchedulerStatus, Waiter};

/// Releases a detached failed-launch snapshot's in-flight mark on drop (R9 C1):
/// constructed inside the spawned fetch, dropped by every exit path (normal end,
/// early return, panic unwind), so a wedged mark can never permanently blind the
/// next failure of the same container name (review 2026-10-31, L9).
pub(super) struct SnapshotRelease {
    pub(super) tx: mpsc::UnboundedSender<ActorMsg>,
    pub(super) container: String,
}

impl Drop for SnapshotRelease {
    fn drop(&mut self) {
        let _ = self.tx.send(ActorMsg::SnapshotDone(self.container.clone()));
    }
}

/// R5 reconcile release: every exit of the probe task — normal end, panic,
/// cancellation — lands exactly one [`ActorMsg::ReconcileDone`], so the
/// in-flight mark can never wedge. An outcome the probe never filled reports
/// as a failed probe (silence), never as an empty set.
pub(super) struct ReconcileRelease {
    pub(super) tx: mpsc::UnboundedSender<ActorMsg>,
    pub(super) outcome: Option<Option<BTreeSet<String>>>,
}

impl Drop for ReconcileRelease {
    fn drop(&mut self) {
        let outcome = self.outcome.take().unwrap_or(None);
        let _ = self.tx.send(ActorMsg::ReconcileDone(outcome));
    }
}

/// Serve-id re-discovery release (FINDINGS-06 [8]): every exit of the probe
/// task — normal end, panic, cancellation — lands exactly one
/// [`ActorMsg::ServeIdRefreshed`], so the in-flight mark can never wedge the
/// next resolve into silence without a retry. An answer the probe never
/// produced reports as silence (`None`), the same posture as the reconcile
/// release above.
pub(super) struct ServeIdProbeRelease {
    pub(super) tx: mpsc::UnboundedSender<ActorMsg>,
    pub(super) port: u16,
    pub(super) generation: u64,
    pub(super) outcome: Option<Option<String>>,
}

impl Drop for ServeIdProbeRelease {
    fn drop(&mut self) {
        let discovered = self.outcome.take().flatten();
        let _ = self.tx.send(ActorMsg::ServeIdRefreshed {
            port: self.port,
            generation: self.generation,
            discovered,
        });
    }
}

#[derive(Debug)]
pub(super) enum ActorMsg {
    Command {
        cmd: Command,
        reply: Waiter,
    },
    /// The off-thread scan dispatched by [`Command::Reload`] finished; apply it
    /// with the same wholesale replace as the boot scan and resolve the queued
    /// reload waiters — their reply means "the new catalog is applied", never
    /// "the scan was dispatched" (FINDINGS-13 [6] keeps the API-visible semantics).
    CatalogLoaded {
        catalog: Catalog,
        seq: u64,
        /// The `llama-server.exe` verdict gathered on the same blocking leg as the
        /// scan (FINDINGS2-04 [2]); `None` means nothing to report.
        binary: Option<ScanError>,
        /// The drift stats for the marker/queue rounds this application runs —
        /// gathered on the reload task's blocking legs so the apply turn never
        /// stats (FINDINGS-07 [8]).
        drift: DriftStats,
    },
    /// The blocking scan task itself failed (panic or cancel): the previous
    /// catalog stays in place, the failure is reported, and the reload waiters
    /// are still answered — a wedged scan thread must not strand callers.
    ReloadFailed {
        seq: u64,
        reason: String,
    },
    /// Last-use hint from the proxy paths (fire-and-forget; feeds the idle check).
    Touch {
        model: String,
    },
    /// One witness reading taken while `model` serves (§0.4 measured cost). The
    /// sampler reports only what the card shows; the actor decides what to charge —
    /// preferring the driver's per-process attribution and subtracting tenants that
    /// appeared after this engine was admitted. A reading whose engine has been
    /// replaced is rejected by `ticket`, so a sampler that outlives its engine cannot
    /// fold another model's memory into the old entry (review, 2026-10-27).
    Measured {
        model: String,
        ticket: SpawnTicket,
        total_diff: f64,
        own: Option<f64>,
        total_gib: f64,
        used_gib: f64,
    },
    /// The container's own startup `MemorySummary` figure (R9): one shot per launch,
    /// fetched from `docker logs` at readiness. The reserved footprint is static for
    /// the life of the engine, so it fixes the range outright instead of folding.
    MeasuredEngineReport {
        model: String,
        ticket: SpawnTicket,
        required_to_free_gb: f64,
        total_gib: f64,
    },
    /// A process engine's one-shot measured footprint (`TASK-ENGINE-MEASUREMENT`,
    /// ORCHESTRATION §3): the figure basalt/strata stated at their own status
    /// endpoint, or the witness's process-tree sum of the engine's pid plus its
    /// direct children. One shot per launch like `MeasuredEngineReport`; the
    /// measurement's `source` decides the provenance it folds under. `total_gib`
    /// is the card total of the live witness sample the figure was bounded
    /// against (doctrine 4: the witness is budget truth).
    MeasuredProcessReport {
        model: String,
        ticket: SpawnTicket,
        measurement: crate::engine::EngineMeasurement,
        total_gib: f64,
    },
    /// One CPU-resident RAM reading for a serving occupant
    /// (`TASK-CPU-LLAMA-EVICTION` W2 step 3): the working set summed over the
    /// engine's own pid plus every descendant, converted to the GiB newtype
    /// once at the probe boundary — no MiB or raw-bytes figure crosses here.
    /// The actor decides what folds: `attribution` is the provenance the
    /// reading earned, and a `total_diff` host delta never books, never
    /// persists, and never re-books (DOCTRINE 12). Like every measured
    /// message, the ticket is the guard: a reading whose spawn is gone is
    /// dropped, so a sampler that outlives its engine cannot fold a stranger's
    /// working set into the old row.
    MeasuredRam {
        model: String,
        ticket: SpawnTicket,
        ram_gib: crate::budget::Gibibytes,
        attribution: crate::model::MeasuredAttribution,
    },
    /// A detached failed-launch log snapshot (R9 C1) has finished; release its
    /// in-flight mark so a later failure of the same container name re-snapshots.
    SnapshotDone(String),
    /// An R5 report-only reconcile probe has finished. `Some` = docker answered
    /// with the stranded set (possibly empty); `None` = the probe could not run
    /// — silence, and the last-reported set stands untouched.
    ReconcileDone(Option<BTreeSet<String>>),
    /// A `serve_id` re-discovery (FINDINGS-06 [8]) for a loaded occupant that
    /// installed with no id has finished: `Some` = the id the engine now
    /// reports; `None` = still silence, and the next resolve retries.
    /// `(port, generation)` names the spawn the probe was launched for — an
    /// answer for a replaced occupant is dropped at the door.
    ServeIdRefreshed {
        port: u16,
        generation: u64,
        discovered: Option<String>,
    },
    /// The R11 fit version probe finished (warm pass and every fit admit lead
    /// with it): `Some` = the calculator's version line, `None` = the tool did
    /// not answer and the fit pass stays silent. `gen` is the catalog epoch the
    /// probe was born in — a stale epoch's answer is dropped at the door.
    FitVersionDone {
        gen: u64,
        version: Option<String>,
    },
    /// An R11 fit row probe finished: `Ok` = (booked GiB, the model file's
    /// fingerprint at computation); `Err` = why (warn + ladder, never a
    /// fabricated figure). `key` is the probe argv hash the figure was computed
    /// for — the cache identity.
    FitRowDone {
        gen: u64,
        model: String,
        key: u64,
        outcome: Result<(f64, Option<crate::model::FileFingerprint>), String>,
    },
    /// The blocking write-back of a measured figure was refused by the disk
    /// (Fork C, FINDINGS-13 [4] follow-through): the actor must stop booking in
    /// memory what disk rejected. Carries the exact figure — pair **and provenance
    /// stamps** — the failed write attempted, so a reading that has since been
    /// superseded never drains the newer (possibly persisted) values. `file` is the
    /// spawn-era row the write targeted: the drain may only land on it, never on a
    /// same-name twin.
    MeasuredPersistFailed {
        model: String,
        data: MeasuredData,
        file: Option<PathBuf>,
    },
    /// A harness-owned writer just rewrote a resident model's own yaml (the measured
    /// persist's success path, or `har_config_model`'s edit — the complete list):
    /// re-baseline that resident's spawn-side capture so the harness's own write is
    /// never a "config changed since launch" (Axis-2 interlock 5). Carries the
    /// fingerprint the writer took directly after its atomic rename, off-thread.
    ModelFileWritten {
        model: String,
        path: PathBuf,
        fingerprint: crate::model::FileFingerprint,
    },
    /// Periodic sweep from the tick task: idles out occupants per category policy.
    Tick,
    Event {
        job: JobId,
        event: Event,
    },
    Status {
        reply: oneshot::Sender<SchedulerStatus>,
    },
    Catalog {
        reply: oneshot::Sender<crate::model::scan::Catalog>,
    },
}

/// Result of an async step, sent back to the actor by the spawned step task.
#[derive(Debug)]
pub(super) enum Event {
    /// Kill + release-evidence poll finished (G12). `Ok` carries the measured
    /// accept-to-confirmed duration of that release, stamped by
    /// [`ActorState::spawn_kill_confirm`] and elapsed inside the step task, so
    /// the actor records the sample without ever consulting wall clock itself.
    Released(Result<Duration, SpawnError>),
    /// Engine start finished.
    Spawned(Result<Spawned, SpawnError>),
    /// Readiness loop finished (terminal tri-state only) plus the engine's
    /// self-reported model id when ready and discoverable (Step 10, F-4).
    Probed(Probe, Option<String>),
    /// Witness sample for admission returned (Err = no reading, which refuses unless
    /// the witness declares `ledger_only`). The two slots mirror the two ladders:
    /// a VRAM row carries `Some(card)` and `None` ram (its gate never reads the
    /// host, and the host witness is never spent on it); a RAM row carries
    /// `None` card and the host reading (which refuses fail-closed when absent,
    /// operator ruling 2026-11-05).
    Admitted(
        Option<Result<crate::budget::MemorySample, String>>,
        Option<crate::witness::RamSample>,
    ),
    /// Post-release memory settle finished (Err carries the numbers).
    Settled(Result<(), String>),
}
