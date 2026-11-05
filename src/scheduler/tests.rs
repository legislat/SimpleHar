use super::{
    gather_drift_stats, headline_fault, missing_binary_error, serve_model_warning, ActorMsg,
    ActorState, AdmissionPath, DriftStats, FitEntry, Gibibytes, Job, JobId, JobKind, JobStatus,
    JobTimings, MeasuredAnchor, Occupant, Reply, ResourceKind, ScanKind, Scheduler, Slot,
    SpawnTicket, SwapState, RELEASE_POLL,
};
use crate::budget::LedgerOnlyWitness;
use crate::config::CategoryId;
use crate::engine::mock::MockAdapter;
use crate::engine::EngineAdapter;
use crate::model::ModelEntry;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Process-global warn capture for the persist-drift test: `tracing`'s default
/// dispatcher is thread-local, and the persist warn fires on a blocking-pool
/// thread, so the capturing layer goes in as the global default exactly once
/// (no lib test installs one; `observability::init` runs only in `main`).
type CapturedWarn = (String, BTreeMap<String, String>);
static WARN_CAPTURE: OnceLock<Arc<Mutex<Vec<CapturedWarn>>>> = OnceLock::new();

struct CaptureVisitor<'a> {
    message: &'a mut String,
    fields: &'a mut BTreeMap<String, String>,
}

impl tracing::field::Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.message = format!("{value:?}");
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }
}

struct CaptureLayer(Arc<Mutex<Vec<CapturedWarn>>>);

impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CaptureLayer {
    fn on_event(
        &self,
        event: &tracing::Event<'_>,
        _ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut message = String::new();
        let mut fields = BTreeMap::new();
        event.record(&mut CaptureVisitor {
            message: &mut message,
            fields: &mut fields,
        });
        self.0.lock().expect("capture lock").push((message, fields));
    }
}

fn install_warn_capture() -> Arc<Mutex<Vec<CapturedWarn>>> {
    WARN_CAPTURE
        .get_or_init(|| {
            let store = Arc::new(Mutex::new(Vec::new()));
            let _ = tracing_subscriber::registry()
                .with(CaptureLayer(store.clone()))
                .try_init();
            store
        })
        .clone()
}

/// Constructor-only stand-in for the never-called runtime half of an
/// [`ActorState`]: the boot-thread pin builds state and reads its catalog field,
/// it never spawns, kills or probes, so any call reaching here is a test bug.
struct NeverRuntime;

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for NeverRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the boot-thread pin must never call the runtime");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        panic!("the boot-thread pin must never call the runtime");
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        panic!("the boot-thread pin must never call the runtime");
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the boot-thread pin must never call the runtime");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the boot-thread pin must never call the runtime");
    }
}

struct BootWiring {
    adapters: BTreeMap<String, Arc<dyn EngineAdapter>>,
    runtime: Arc<dyn crate::engine::EngineRuntime>,
    witness: Arc<dyn crate::budget::VramWitness>,
    tx: tokio::sync::mpsc::UnboundedSender<super::ActorMsg>,
}

fn boot_stub_wiring() -> BootWiring {
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    BootWiring {
        adapters,
        runtime: Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        witness: Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx: tokio::sync::mpsc::unbounded_channel().0,
    }
}

fn seed_one_model(dir: &std::path::Path, name: &str) {
    std::fs::write(
        dir.join(format!("{name}.yaml")),
        format!("name: {name}\nengine: mock\ncategory: work\n"),
    )
    .expect("model yaml");
}

/// Test double for the R5 reconcile probe (ruling 9): canned `docker ps`
/// answers consumed one per probe (`None` = docker could not answer) and
/// the declared-name lists it was asked about. Everything else keeps the
/// NeverRuntime posture: a load reaching this runtime is a test bug.
struct ReconcileRuntime {
    answers: Mutex<std::collections::VecDeque<Option<BTreeSet<String>>>>,
    asked: Mutex<Vec<Vec<String>>>,
}

impl ReconcileRuntime {
    fn new(answers: Vec<Option<BTreeSet<String>>>) -> Self {
        ReconcileRuntime {
            answers: Mutex::new(answers.into()),
            asked: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for ReconcileRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the R5 reconcile pin must never load");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        panic!("the R5 reconcile pin must never load");
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        panic!("the R5 reconcile pin must never load");
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the R5 reconcile pin must never load");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the R5 reconcile pin must never load");
    }
    async fn unclaimed_container_names(&self, declared: &[String]) -> Option<BTreeSet<String>> {
        self.asked
            .lock()
            .expect("asked lock")
            .push(declared.to_vec());
        self.answers
            .lock()
            .expect("answers lock")
            .pop_front()
            .unwrap_or(None)
    }
}

/// State shaped for the R5 pins: `on_reconcile_done` is called directly, so
/// the runtime is never reached and the actor channel never read.
fn reconcile_state() -> ActorState {
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    ActorState::empty(
        crate::config::AppConfig::default(),
        crate::testutil::temp_dir("r5-state"),
        adapters,
        Arc::new(ReconcileRuntime::new(Vec::new())) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    )
}

/// Stranded-container warns captured for one container name: the message
/// half pins the report-only wording, the field half the named container.
fn stranded_warns(capture: &Arc<Mutex<Vec<CapturedWarn>>>, container: &str) -> usize {
    capture
        .lock()
        .expect("capture lock")
        .iter()
        .filter(|(message, fields)| {
            message.contains("unclaimed running container")
                && message.contains("no claim in the ledger")
                && fields
                    .get("container")
                    .is_some_and(|name| name.contains(container))
        })
        .count()
}

#[tokio::test]
async fn the_reconcile_warns_once_and_an_unchanged_set_never_re_warns() {
    let capture = install_warn_capture();
    // Warm the reporting callsite and PROVE delivery before the measured
    // window (the D9 rule): a sentinel name no assertion below reads about.
    let mut warm = reconcile_state();
    warm.on_reconcile_done(Some(["r5-warmup".to_string()].into_iter().collect()));
    assert!(
        stranded_warns(&capture, "r5-warmup") == 1,
        "the reporting callsite was warmed but never delivered"
    );

    let mut state = reconcile_state();
    let stranded: BTreeSet<String> = ["r5-unit-a".to_string()].into_iter().collect();
    state.on_reconcile_done(Some(stranded.clone()));
    assert_eq!(
        stranded_warns(&capture, "r5-unit-a"),
        1,
        "the first report names the stranded container once"
    );
    state.on_reconcile_done(Some(stranded.clone()));
    assert_eq!(
        stranded_warns(&capture, "r5-unit-a"),
        1,
        "repeated rescans of the same stranded set must not spam the ring"
    );
    state.on_reconcile_done(None);
    state.on_reconcile_done(Some(stranded));
    assert_eq!(
        stranded_warns(&capture, "r5-unit-a"),
        1,
        "a docker that briefly could not answer must not re-warn what the next probe re-confirms"
    );
}

/// The `/har/status` rider of the R5 report: the SAME dedupe state backs the
/// payload, so page and log can never disagree. Polarity pinned both ways
/// (the inverted-`skip_serializing_if` GOTCHAS class): an empty last-reported
/// set normalizes to `None` and the key is ABSENT from the serialized
/// status, never `null` or `[]`; a failed probe keeps the last-reported set
/// exactly like the warn dedupe.
#[tokio::test]
async fn the_status_snapshot_carries_the_stranded_set_and_drops_it_when_empty() {
    let mut state = reconcile_state();
    assert!(
        state.snapshot().stranded_containers.is_none(),
        "no probe yet: the key has no shape to leak"
    );
    state.on_reconcile_done(None);
    assert!(
        state.snapshot().stranded_containers.is_none(),
        "a failed probe invents nothing for the page either"
    );
    let stranded: BTreeSet<String> = ["r5-payload".to_string()].into_iter().collect();
    state.on_reconcile_done(Some(stranded.clone()));
    let reported = state.snapshot();
    assert_eq!(reported.stranded_containers, Some(stranded));
    assert_eq!(
        serde_json::to_value(&reported).expect("status serializes")["stranded_containers"],
        serde_json::json!(["r5-payload"]),
        "the warned names ride /har/status verbatim"
    );
    state.on_reconcile_done(None);
    assert!(
            state.snapshot().stranded_containers.is_some(),
            "a docker that briefly could not answer keeps the last-reported set (same rule as the warn)"
        );
    state.on_reconcile_done(Some(BTreeSet::new()));
    let cleared = state.snapshot();
    assert!(cleared.stranded_containers.is_none());
    assert!(
        serde_json::to_value(&cleared)
            .expect("status serializes")
            .get("stranded_containers")
            .is_none(),
        "empty means ABSENT, not null or []: the skip predicate must not invert"
    );
}

#[tokio::test]
async fn a_failed_or_empty_reconcile_never_warns() {
    let capture = install_warn_capture();
    let mut state = reconcile_state();
    state.on_reconcile_done(None);
    state.on_reconcile_done(Some(BTreeSet::new()));
    assert_eq!(
        stranded_warns(&capture, "r5-quiet"),
        0,
        "a failed probe and an empty stranded set are both silence"
    );
    let stranded: BTreeSet<String> = ["r5-quiet".to_string()].into_iter().collect();
    state.on_reconcile_done(Some(stranded.clone()));
    assert_eq!(stranded_warns(&capture, "r5-quiet"), 1);
    state.on_reconcile_done(Some(BTreeSet::new()));
    assert_eq!(
        stranded_warns(&capture, "r5-quiet"),
        1,
        "the stranded container dying is a changed set with nothing left to name"
    );
    state.on_reconcile_done(Some(stranded));
    assert_eq!(
        stranded_warns(&capture, "r5-quiet"),
        2,
        "reappearing is a changed set again: one fresh warn"
    );
}

#[tokio::test]
async fn a_rescan_reports_a_stranded_container_once_across_the_actor() {
    // End-to-end: the boot scan and every rescan trigger the reconcile; the
    // warn lands once for the first sighting, the unchanged rescan is quiet,
    // and a CHANGED set warns again — through the live actor loop, not a
    // hand-called handler.
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("r5-e2e");
    std::fs::write(
            dir.join("r5-model.yaml"),
            "name: r5-model\nengine: ninfer\ncategory: work\nvram_gb: 2\nninfer_image: ninfer:local\nninfer_container: r5-e2e-a\n",
        )
        .expect("seed ninfer yaml");
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    adapters.insert(
        "ninfer".to_string(),
        Arc::new(crate::engine::ninfer::NinferAdapter::new("Ubuntu", "ops"))
            as Arc<dyn EngineAdapter>,
    );
    let stranded_a: BTreeSet<String> = ["r5-e2e-a".to_string()].into_iter().collect();
    let stranded_b: BTreeSet<String> = ["r5-e2e-b".to_string()].into_iter().collect();
    let probe = Arc::new(ReconcileRuntime::new(vec![
        Some(stranded_a.clone()),
        Some(stranded_a.clone()),
        Some(stranded_b.clone()),
    ]));
    let handle = Scheduler::start(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        probe.clone() as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    while stranded_warns(&capture, "r5-e2e-a") < 1 {
        assert!(
            Instant::now() < deadline,
            "the boot-scan reconcile never reported the stranded container"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        probe
            .asked
            .lock()
            .expect("asked lock")
            .iter()
            .any(|names| names.iter().any(|name| name == "r5-e2e-a")),
        "the probe must be asked with the catalog's declared `ninfer_container` names"
    );
    let status = handle.status().await.expect("actor alive");
    assert_eq!(status.stranded_containers, Some(stranded_a));
    handle.reload().await.expect("actor alive");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        stranded_warns(&capture, "r5-e2e-a"),
        1,
        "a rescan of an unchanged stranded set must not re-warn"
    );
    std::fs::write(
            dir.join("r5-model.yaml"),
            "name: r5-model\nengine: ninfer\ncategory: work\nvram_gb: 2\nninfer_image: ninfer:local\nninfer_container: r5-e2e-b\n",
        )
        .expect("edit container name");
    handle.reload().await.expect("actor alive");
    while stranded_warns(&capture, "r5-e2e-b") < 1 {
        assert!(
            Instant::now() < deadline,
            "the changed stranded set was never reported"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        stranded_warns(&capture, "r5-e2e-a"),
        1,
        "the departed name is not re-warned by the changed set"
    );
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        status.stranded_containers,
        Some(stranded_b),
        "the page rides the same replaced set the log warned about"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn actor_state_construction_never_scans_the_catalog() {
    // FINDINGS-14 [1], boot-thread pin: the constructor that `Scheduler::start`
    // funnels through — including the `ActorState::empty` path it delegates to —
    // must return against a dir holding a valid model with the catalog still
    // empty. Re-introducing the scan into the constructor (the pre-bind stall)
    // fails this assertion. Deterministic: no task has run, so nothing can have
    // scanned yet.
    let dir = crate::testutil::temp_dir("bootscan");
    seed_one_model(&dir, "boot-stub");
    let wiring = boot_stub_wiring();
    let state = ActorState::new(
        crate::config::AppConfig::default(),
        dir.clone(),
        wiring.adapters,
        wiring.runtime,
        wiring.witness,
        wiring.tx,
    );
    assert!(
        state.catalog.is_empty(),
        "the constructor must not scan: a pre-bind catalog read is the \
             FINDINGS-14 [1] stall; {:?}",
        state
            .catalog
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>()
    );
    let wiring = boot_stub_wiring();
    let state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        wiring.adapters,
        wiring.runtime,
        wiring.witness,
        wiring.tx,
    );
    assert!(
        state.catalog.is_empty(),
        "ActorState::empty must not scan either; {:?}",
        state
            .catalog
            .iter()
            .map(|entry| entry.name.clone())
            .collect::<Vec<_>>()
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn actor_task_performs_the_initial_scan() {
    // FINDINGS-14 [1], actor-side pin: the scan `ActorState::new` no longer runs
    // must still happen — as the first thing the actor task does. A command sent
    // immediately after `start()` (here the catalog read) queues behind the scan
    // on the unbounded channel and observes its result; no sleep or timing
    // assumption is involved, only a bounded poll for the eventual occupancy of
    // the seeded model.
    let dir = crate::testutil::temp_dir("bootscan");
    seed_one_model(&dir, "boot-model");
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let handle = Scheduler::start(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let catalog = handle.catalog().await.expect("actor alive");
        if catalog
            .models
            .iter()
            .any(|entry| entry.name == "boot-model")
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the actor-side initial scan never populated the catalog: {:?}",
            catalog
                .models
                .iter()
                .map(|entry| entry.name.clone())
                .collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_command_during_an_in_flight_rescan_is_answered() {
    // FINDINGS-13 [6]: the reload scan parks on a test-only seam; while it is
    // in flight the actor must keep answering commands, and the reload reply
    // may resolve only when the new catalog is APPLIED (never when the scan
    // was dispatched — the /har/rescan contract). Pre-fix (the inline scan),
    // the parked scan holds the actor thread and the status poll below times
    // out — which is exactly why this test runs on a multi-thread runtime:
    // the timer has to survive a thread that a blocking scan has parked.
    let dir = crate::testutil::temp_dir("reloadresp");
    seed_one_model(&dir, "resident");
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let handle = Scheduler::start(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
    );
    // The boot scan must have landed before the seam is armed: the seam is
    // single-shot for this dir and belongs to the reload alone.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let catalog = handle.catalog().await.expect("actor alive");
        if catalog.models.iter().any(|entry| entry.name == "resident") {
            break;
        }
        assert!(Instant::now() < deadline, "the boot scan never landed");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    std::fs::write(
        dir.join("late.yaml"),
        "engine: mock\ncategory: work\nvram_gb: 2\n",
    )
    .expect("seed late model");

    let (started, release) = crate::model::scan::arm_scan_pause(&dir);
    let client = handle.clone();
    let reload = tokio::spawn(async move { client.reload().await });
    let grabbed = tokio::time::timeout(
        Duration::from_secs(5),
        tokio::task::spawn_blocking(move || started.recv().is_ok()),
    )
    .await;
    assert!(
        matches!(grabbed, Ok(Ok(true))),
        "the reload scan never parked on the seam (is scan_dir off-thread?)"
    );

    let answered = tokio::time::timeout(Duration::from_secs(2), handle.status()).await;
    assert!(
        matches!(answered, Ok(Ok(_))),
        "the actor stopped answering while a rescan was in flight"
    );
    assert!(
        !reload.is_finished(),
        "the reload reply resolved before the scan even completed"
    );

    drop(release);
    let reply = tokio::time::timeout(Duration::from_secs(10), reload)
        .await
        .expect("the reload resolves once released")
        .expect("join")
        .expect("reply");
    assert_eq!(reply, Reply::Done);
    let catalog = handle.catalog().await.expect("actor alive");
    assert!(
        catalog.models.iter().any(|entry| entry.name == "late"),
        "the reply meant applied: the new model must be in the catalog"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn an_unavailable_engine_is_dropped_and_named() {
    // The audit verdict shipped with this batch (FINDINGS-13 [3] follow-up): no
    // fixture publishes an engine its adapter map lacks, and such an entry can
    // only ever die later as `no adapter registered` — so the actor refuses it
    // at apply time, DROPPING it with a typed, page-visible reason.
    let dir = crate::testutil::temp_dir("engineunavail");
    std::fs::write(
        dir.join("ghost.yaml"),
        "engine: phantom\ncategory: work\nvram_gb: 2\n",
    )
    .expect("seed ghost");
    seed_one_model(&dir, "resident");
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let handle = Scheduler::start(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let catalog = loop {
        let catalog = handle.catalog().await.expect("actor alive");
        let reported = catalog
            .errors
            .iter()
            .any(|e| e.kind == crate::model::scan::ScanKind::EngineUnavailable);
        let settled = catalog.models.iter().any(|entry| entry.name == "resident");
        assert!(
            Instant::now() < deadline,
            "scan never reported: {catalog:?}"
        );
        if reported && settled {
            break catalog;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    assert!(
        !catalog.models.iter().any(|entry| entry.name == "ghost"),
        "an engine with no adapter must be dropped from the catalog"
    );
    let error = catalog
        .errors
        .iter()
        .find(|e| e.kind == crate::model::scan::ScanKind::EngineUnavailable)
        .expect("the drop must be reported");
    assert_eq!(error.file, "ghost.yaml");
    assert!(
        error.reason == "engine \"phantom\" is not available to this harness (known: mock)",
        "{}",
        error.reason
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Seats a spawn exactly as the actor does: the terminal load row that owns the
/// ticket — carrying the spawn-era [`MeasuredAnchor`] built by the very function
/// [`ActorState::begin_spawn`] calls — plus the occupant that holds its sampler.
/// A pin that folds a reading must go through this: with no spawn row there is no
/// anchor, and an unanchored reading has neither an honest stamp to mint nor a
/// file it may write to.
fn seat_spawn(state: &mut ActorState, entry: &ModelEntry, ticket: &SpawnTicket, port: u16) {
    let job_id = JobId(state.next_job);
    state.next_job += 1;
    state.jobs.insert(
        job_id,
        Job {
            kind: JobKind::Load,
            // Seated directly, not admitted: the row never took a rung.
            path: None,
            model: entry.name.clone(),
            engine: entry.engine.clone(),
            category: CategoryId::Work,
            state: SwapState::Serving,
            terminal: true,
            outcome: Some("loaded".to_string()),
            error: None,
            need: Some(Gibibytes(2.0)),
            resource: entry.resource(),
            ram_need: None,
            cost: entry.cost,
            vram_gate: entry.vram_gate,
            load_timeout_s: entry.load_timeout_s,
            gate_target: Some(2.0),
            free_at_admit: Some(Gibibytes(20.0)),
            observed_at_admit: Some(20.0),
            release_first: false,
            container: None,
            timings: JobTimings::default(),
            t0: Instant::now(),
            waiters: Vec::new(),
            tasks: Vec::new(),
            spawned: Some(ticket.clone()),
            readiness: None,
            measured: Some(MeasuredAnchor::at_spawn(entry)),
            victim: None,
            victims: Vec::new(),
            victims_released: Vec::new(),
            released_cost: 0.0,
            holder_released_cost: 0.0,
            other_need_at_admit: 0.0,
            note: Some("loaded".to_string()),
            warnings: Vec::new(),
            slot: Some(0),
            queue_fingerprint: None,
        },
    );
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: port,
        occupant: Some(Occupant {
            model: entry.name.clone(),
            need: Gibibytes(2.0),
            ram_need: None,
            port,
            ticket: ticket.clone(),
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: None,
            stale_config: false,
            serve_id: None,
            measure: None,
        }),
        job: Some(job_id),
    }];
}

impl ActorState {
    /// The pin seam for one witness reading, exactly as the serving-window sampler
    /// sends it (`ActorMsg::Measured`).
    fn witness(&mut self, model: &str, ticket: &SpawnTicket, attributed: f64) {
        self.on_measured(
            model,
            ticket,
            super::Reading {
                total_diff: attributed,
                own: Some(attributed),
                total_gib: 24.0,
                used_gib: attributed + 0.1,
            },
        );
    }

    /// The pin seam for the container's one-shot startup `MemorySummary` figure
    /// (`ActorMsg::MeasuredEngineReport`). The card is the operator's 72 GiB: a
    /// reserved figure may legitimately exceed the 24 GiB the witness fold pins
    /// use, because it counts KV reserved at *declared* capacity, not bytes the
    /// card was observed carrying.
    fn engine_report(&mut self, model: &str, ticket: &SpawnTicket, required_gb: f64) {
        self.on_engine_report(model, ticket, required_gb, 72.0);
    }

    /// The pin seam for the process engines' one-shot measurement
    /// (`ActorMsg::MeasuredProcessReport`), on the same 72 GiB card the
    /// `engine_report` seam assumes: a status figure counts what the
    /// engine holds, not only bytes the witness was sampling at one tick.
    fn process_measurement(
        &mut self,
        model: &str,
        ticket: &SpawnTicket,
        measurement: crate::engine::EngineMeasurement,
    ) {
        self.on_process_measurement(model, ticket, measurement, 72.0);
    }

    /// The pin seam for the persist-failed reconcile: the disk refused exactly this
    /// figure, so the drain must land on exactly these values. Pair-only rows pass
    /// `attribution: None, profile: None` — the legacy shape a pre-provenance
    /// persist carried.
    fn fail_persist(
        &mut self,
        model: &str,
        data: super::MeasuredData,
        file: Option<std::path::PathBuf>,
    ) {
        self.on_measured_persist_failed(model, &data, file);
    }
}

/// A persist the disk refuses must not leave memory booking what disk denied
/// (Fork C, closing FINDINGS-13 [4]'s warn-only tail): the warn fires AND the
/// catalog entry's measured triple drains to its none state. The entry is a
/// `llama` row so the persist really carries the stamped triple — non-offload
/// engines never mint the stamp (`only_offload_engines_stamp_measured_offload`).
/// The wedged-drive handle is Windows-only, so the test is gated like its
/// writeback sibling (FINDINGS-BATCH-2026-10-28 [7]).
#[cfg(windows)]
#[tokio::test]
async fn a_refused_measured_persist_drains_the_triple_from_memory() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("measured-persist");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("drift.yaml");
    std::fs::write(&yaml, "engine: llama\ncategory: work\nvram_gb: 2\n").expect("write");

    // Refuse the validated rename the way a wedged drive does: hold the target
    // open with delete-sharing off. Reads still succeed (the batch must be
    // able to read the file), the replace cannot.
    let _lock = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE (no DELETE)
            .open(&yaml)
            .expect("exclusive handle")
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let (map, problems) =
        crate::model::scan::parse_flat_yaml("engine: llama\ncategory: work\nvram_gb: 2\n");
    assert!(problems.is_empty(), "{problems:?}");
    let mut entry = ModelEntry::from_flat(&map, "drift").expect("entry");
    entry.file = Some(yaml.clone());
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 18_991,
    };
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 18_991);

    state.witness("drift", &ticket, 3.7);
    assert_eq!(
        state.catalog[0].measured_gb,
        Some(3.7),
        "the reading books in memory first"
    );

    let msg = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("the persist failure reaches the actor")
        .expect("channel open");
    let ActorMsg::MeasuredPersistFailed { model, data, file } = msg else {
        panic!("expected the persist-failed reconcile message");
    };
    assert_eq!(model, "drift");
    assert_eq!((data.high, data.low, data.offload), (3.7, 3.7, Some(false)));
    assert_eq!(
        data.attribution,
        Some(crate::model::MeasuredAttribution::PerProcess),
        "concrete bytes for the engine's own pid stamp the reading per_process"
    );
    assert_eq!(
        file.as_deref(),
        Some(yaml.as_path()),
        "the reconcile names the spawn-era row it refused to write"
    );

    state.fail_persist(&model, data, file);
    let entry = &state.catalog[0];
    assert_eq!(entry.measured_gb, None);
    assert_eq!(entry.measured_min_gb, None);
    assert_eq!(entry.measured_offload, None);
    assert_eq!(
        entry.measured_attribution, None,
        "a range the disk refused must not keep the provenance that would book it"
    );

    let warns = capture.lock().expect("capture lock");
    assert!(
        warns.iter().any(|(message, fields)| {
            message.contains("cannot persist the measured range")
                && fields.get("model").is_some_and(|m| m.contains("drift"))
        }),
        "the warn must have fired: {warns:?}"
    );
    // And the file really never gained the keys the memory had booked.
    let text = std::fs::read_to_string(&yaml).expect("read back");
    assert!(
        !text.contains("measured_gb"),
        "disk refused the triple: {text}"
    );

    drop(_lock);
    let _ = std::fs::remove_dir_all(&dir);
}

/// FINDINGS2-04 [1]: the fold books the row that owns the spawn-era FILE, while the
/// drain keyed `name && file`. Wherever the file's row answers to a different name
/// than the spawn-era string, the drain missed the very row it exists to clear:
/// memory kept booking a footprint the disk refused for the whole serve window
/// (later samples reproduce the same high/low against the updated anchor range, so
/// `changed` stays false and no retry persist ever fires — it healed only on
/// rescan). Scan derives identity from the file stem today, so the state is built by
/// hand: it is the vocabulary the DEFERRED `name:`-key hard-reject opens up, and the
/// drain must not be the line that turns it into a leak. Falsifier: restore the
/// `name && file` key and the renamed row keeps `Some(3.7)` after the drain.
#[cfg(windows)]
#[tokio::test]
async fn the_refused_persist_drain_follows_the_file_the_fold_booked() {
    let _capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("measured-drain-file");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("renamed.yaml");
    std::fs::write(&yaml, "engine: llama\ncategory: work\nvram_gb: 2\n").expect("write");
    let _lock = {
        use std::os::windows::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .share_mode(0x3)
            .open(&yaml)
            .expect("exclusive handle")
    };

    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let (map, problems) =
        crate::model::scan::parse_flat_yaml("engine: llama\ncategory: work\nvram_gb: 2\n");
    assert!(problems.is_empty(), "{problems:?}");
    let mut spawn_era = ModelEntry::from_flat(&map, "renamed").expect("entry");
    spawn_era.file = Some(yaml.clone());
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4243,
        port: 18_997,
    };
    state.catalog = vec![spawn_era.clone()];
    seat_spawn(&mut state, &spawn_era, &ticket, 18_997);

    // The reload the finding names: the same bytes on disk, a row that answers to a
    // different name. The engine is still serving the spawn-era string.
    state.catalog = vec![ModelEntry {
        name: "moved".to_string(),
        ..spawn_era.clone()
    }];

    state.witness("renamed", &ticket, 3.7);
    assert_eq!(
        state.catalog[0].measured_gb,
        Some(3.7),
        "the fold reaches the row by file, whatever it is called now"
    );

    let msg = tokio::time::timeout(Duration::from_secs(15), rx.recv())
        .await
        .expect("the persist failure reaches the actor")
        .expect("channel open");
    let ActorMsg::MeasuredPersistFailed { model, data, file } = msg else {
        panic!("expected the persist-failed reconcile message");
    };
    assert_eq!(
        model, "renamed",
        "the message carries the spawn-era name, which is what used to defeat the drain"
    );
    assert_eq!(file.as_deref(), Some(yaml.as_path()));

    state.fail_persist(&model, data, file);
    let entry = &state.catalog[0];
    assert_eq!(
        entry.measured_gb, None,
        "memory may not book what the disk refused, whichever name the row answers to"
    );
    assert_eq!(entry.measured_min_gb, None);
    assert_eq!(entry.measured_offload, None);

    drop(_lock);
    let _ = std::fs::remove_dir_all(&dir);
}

/// FINDINGS2-04 [3]: a spawn whose anchor carries `file: None` passed the serving
/// and anchor checks and returned early WITHOUT retiring the sampler — the one
/// reading class that left the task walking the witness every `measured_sample_s`
/// until its 24 h deadline while the actor discarded every reading it produced.
/// `apply_catalog` keeps file-None rows whose engine has an adapter, so the door is
/// config-inert, not type-inert. Falsifier: delete the `retire_sampler` call from
/// the file-None arm and the sampler is still attached.
#[tokio::test]
async fn a_synthetic_anchor_retires_the_sampler_that_has_nowhere_to_write() {
    let capture = install_warn_capture();
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        crate::testutil::temp_dir("synthetic-anchor"),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let (map, problems) = crate::model::scan::parse_flat_yaml("engine: mock\n");
    assert!(problems.is_empty(), "{problems:?}");
    // `from_flat` is the only producer of a `file: None` row: the synthetic shape.
    let entry = ModelEntry::from_flat(&map, "synthetic").expect("entry");
    assert_eq!(entry.file, None, "the shape under test");
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4343,
        port: 18_998,
    };
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 18_998);
    let sampler = tokio::spawn(tokio::time::sleep(Duration::from_secs(3600)));
    state
        .slots
        .first_mut()
        .and_then(|slot| slot.occupant.as_mut())
        .expect("a seated occupant")
        .measure = Some(sampler);

    state.witness("synthetic", &ticket, 3.7);

    let measure = &state
        .slots
        .first()
        .and_then(|slot| slot.occupant.as_ref())
        .expect("a seated occupant")
        .measure;
    assert!(
        measure.is_none(),
        "a reading with no operator file may not leave a sampler polling for a day"
    );
    let warns = capture.lock().expect("capture lock");
    assert!(
        warns
            .iter()
            .any(|(message, _)| message.contains("no operator file")),
        "the retirement has to be visible: {warns:?}"
    );
}

/// FINDINGS-BATCH-2026-10-28L [L-2]: the install-time `serve_model` verdict read the
/// LIVE catalog by name, minutes after `begin_spawn`. Under a mid-serve reload that
/// rewrites the row's `serve_model`, the warning was computed against yaml the
/// running process was never launched from — a false warning, or a missed one — and
/// under same-name twins it could quote the other twin's id. The verdict now rides
/// the spawn-era anchor. Falsifier: judge from the live row instead of the anchor
/// and the `declared-A`/`declared-B` legs fail.
#[test]
fn the_serve_model_verdict_is_judged_against_the_spawn_era_row() {
    let (map, problems) =
        crate::model::scan::parse_flat_yaml("engine: mock\nserve_model: declared-A\n");
    assert!(problems.is_empty(), "{problems:?}");
    let spawn_era = ModelEntry::from_flat(&map, "served").expect("entry");
    let anchor = MeasuredAnchor::at_spawn(&spawn_era);

    let warning = serve_model_warning(Some(&anchor), Some("actual"))
        .expect("the yaml claims an id the engine does not report");
    assert!(warning.contains("declared-A"), "{warning}");

    // The reload the finding names: the same model, a different id on disk now.
    let (reloaded_map, _) =
        crate::model::scan::parse_flat_yaml("engine: mock\nserve_model: declared-B\n");
    let reloaded = ModelEntry::from_flat(&reloaded_map, "served").expect("entry");
    assert_eq!(reloaded.serve_model.as_deref(), Some("declared-B"));
    assert!(
        !warning.contains("declared-B"),
        "yaml that arrived after the launch may not rewrite what counted as a \
             mismatch: {warning}"
    );

    assert!(
        serve_model_warning(Some(&anchor), Some("declared-A")).is_none(),
        "an id the engine really reports is not a mismatch"
    );
    assert!(
        serve_model_warning(Some(&anchor), None).is_none(),
        "no discovered id is nothing to compare against"
    );
    assert!(
        serve_model_warning(None, Some("actual")).is_none(),
        "a job that never spawned holds no spawn-era claim"
    );

    // And the site may not quietly go back to describing the process from live
    // catalog bytes.
    let source = include_str!("events.rs");
    let site = source
        .split("let serve_id = discovered_id;")
        .nth(1)
        .expect("the install verdict site is still here")
        .split("if let Some(slot) = slot")
        .next()
        .expect("the site ends where the slot work begins");
    assert!(
        !site.contains("self.catalog"),
        "the install verdict must come from the anchor, not the live catalog: {site}"
    );
}

/// FINDINGS2-04 [2]: `apply_catalog` ran `self.cfg.llama_server_exe.exists()` inside
/// the single writer's turn — the last synchronous `std::fs` touch on the actor.
/// The exe path is operator config, so that stat can park the actor behind a
/// sleeping USB or a disconnected NAS volume with the listener already bound, and
/// every command, `/har/status` poll and tray tick queues behind it (the A1-class
/// stall FINDINGS-14 [1]/13 [6] moved the scans off-thread for). The verdict now
/// travels in from the blocking leg. Falsifier: restore the stat inside
/// `apply_catalog` and the first leg fails (an absent binary reports itself even
/// when the scan leg said nothing), and the file-wide count leg fails.
#[test]
fn the_actor_applies_the_binary_verdict_it_is_handed_and_never_stats_for_one() {
    let source = include_str!("catalog.rs");
    // Counted over the production half only, and with a needle built at compile
    // time so this assertion's own text cannot feed on itself.
    let production = source.split("mod tests").next().unwrap_or(source);
    let needle = concat!(".exists", "()");
    assert_eq!(
        production.match_indices(needle).count(),
        1,
        "the actor half must hold exactly one synchronous stat — the one inside \
             `missing_binary_error`, which runs on a blocking thread"
    );

    let dir = crate::testutil::temp_dir("binary-verdict");
    let absent = dir.join("llama-server.exe");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let config = crate::config::AppConfig {
        llama_server_exe: absent.clone(),
        ..crate::config::AppConfig::default()
    };
    let mut state = ActorState::empty(
        config,
        dir.clone(),
        BTreeMap::new(),
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );

    assert!(
        missing_binary_error(&absent).is_some(),
        "the fixture binary really is absent"
    );
    state.apply_catalog(
        crate::model::scan::Catalog::empty(),
        None,
        &DriftStats::default(),
    );
    assert!(
        !state
            .catalog_errors
            .iter()
            .any(|error| error.kind == ScanKind::EngineBinaryMissing),
        "the actor may not answer from a stat of its own: {:?}",
        state.catalog_errors
    );

    state.apply_catalog(
        crate::model::scan::Catalog::empty(),
        missing_binary_error(&absent),
        &DriftStats::default(),
    );
    let report = state
        .catalog_errors
        .iter()
        .find(|error| error.kind == ScanKind::EngineBinaryMissing)
        .expect("the carried verdict is published");
    assert!(
        report.file == "config"
            && report.reason.contains(&absent.display().to_string())
            && report.reason.contains("llama loads will fail"),
        "the page-visible sentence keeps naming the path and the consequence: {:?}",
        report.reason
    );

    // The other polarity: a binary that is there is nothing to report.
    let present = dir.join("present.exe");
    std::fs::write(&present, b"").expect("fixture binary");
    assert!(missing_binary_error(&present).is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

/// The reconcile must not drain a triple that a newer reading already replaced
/// (the stale-failure race: two persist tasks land out of order), and the
/// exact-match guard covers the `Option<bool>` shape dimension itself
/// (FINDINGS-BATCH-2026-10-28F [2]): equal `(high, low)` with a mismatched
/// stamp shape drains nothing, in either direction. Falsifier: neutering the
/// `entry.measured_offload == offload` comparison fails the shape legs below.
#[test]
fn a_stale_persist_failure_never_drains_a_superseding_triple() {
    let dir = crate::testutil::temp_dir("measured-stale");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let (map, _) = crate::model::scan::parse_flat_yaml("engine: llama\nvram_gb: 2\n");
    let mut entry = ModelEntry::from_flat(&map, "fresh").expect("entry");
    entry.measured_gb = Some(6.0);
    entry.measured_min_gb = Some(4.0);
    entry.measured_offload = Some(false);
    let fresh_yaml = dir.join("fresh.yaml");
    entry.file = Some(fresh_yaml.clone());
    state.catalog = vec![entry];

    state.fail_persist(
        "fresh",
        super::MeasuredData {
            high: 5.0,
            low: 5.0,
            offload: Some(false),
            attribution: None,
            profile: None,
        },
        Some(fresh_yaml.clone()),
    );
    assert_eq!(
        state.catalog[0].measured_gb,
        Some(6.0),
        "the newer (possibly persisted) triple survives a stale failure"
    );
    state.fail_persist(
        "fresh",
        super::MeasuredData {
            high: 6.0,
            low: 4.0,
            offload: None,
            attribution: None,
            profile: None,
        },
        Some(fresh_yaml.clone()),
    );
    assert_eq!(
        state.catalog[0].measured_gb,
        Some(6.0),
        "a None-shaped (strata-style) failure must not drain a stamped \
             (llama-style) triple: equal pair values with the wrong stamp shape are \
             not the triple that failed"
    );
    state.fail_persist(
        "fresh",
        super::MeasuredData {
            high: 6.0,
            low: 4.0,
            offload: Some(false),
            attribution: None,
            profile: None,
        },
        Some(fresh_yaml.clone()),
    );
    assert_eq!(
        state.catalog[0].measured_gb, None,
        "the matching triple drains"
    );

    let (map2, _) = crate::model::scan::parse_flat_yaml("engine: strata\nvram_gb: 2\n");
    let mut paironly = ModelEntry::from_flat(&map2, "paironly").expect("entry");
    paironly.measured_gb = Some(6.0);
    paironly.measured_min_gb = Some(4.0);
    // measured_offload stays None: the shape a non-offload engine's persist carries.
    let paironly_yaml = dir.join("paironly.yaml");
    paironly.file = Some(paironly_yaml.clone());
    state.catalog.push(paironly);
    state.fail_persist(
        "paironly",
        super::MeasuredData {
            high: 6.0,
            low: 4.0,
            offload: Some(false),
            attribution: None,
            profile: None,
        },
        Some(paironly_yaml.clone()),
    );
    assert_eq!(
        state.catalog[1].measured_gb,
        Some(6.0),
        "the reverse leg: a Some-shaped (llama-style) failure must not drain a \
             pair-only (strata-style) triple"
    );
    state.fail_persist(
        "paironly",
        super::MeasuredData {
            high: 6.0,
            low: 4.0,
            offload: None,
            attribution: None,
            profile: None,
        },
        Some(paironly_yaml.clone()),
    );
    assert_eq!(
        state.catalog[1].measured_gb, None,
        "the shape-matching failure drains the pair"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Poll a persisted yaml until every needle appears. The measured persist
/// runs on the blocking pool and lands through one atomic temp+rename, so a
/// successful write publishes the whole batch in one step.
async fn await_yaml_keys(path: &std::path::Path, keys: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(path).expect("persisted yaml readable");
        if keys.iter().all(|k| text.contains(k)) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "keys {keys:?} never reached the disk: {text}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// D of the `cpu_offload` scoping ruling: the measured PAIR mints for every
/// engine (witness truth is engine-agnostic), but the `measured_offload` stamp
/// is minted only for engines carrying the `cpu_offload` knob — in memory AND on disk: both
/// entries carry a real backing yaml, so the persist runs to completion and
/// the bytes are read back. Falsifier (disk leg): deleting the stamp push in
/// the persist edits makes this the only failing test — a llama yaml that
/// silently stops carrying `measured_offload` (killing the flip-void rule's
/// stamp source) cannot ship unnoticed; the mock leg then stays green (its
/// contract is pair-only). Also catches: dropping the `offload_live` gate in
/// `on_measured` (stamps the mock row) or tightening it past llama (unmints).
#[tokio::test]
async fn only_offload_engines_stamp_measured_offload() {
    let dir = crate::testutil::temp_dir("measured-stamp");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 18_992,
    };
    let entry = |name: &str, engine: &str| -> ModelEntry {
        let (map, _) = crate::model::scan::parse_flat_yaml(&format!(
            "engine: {engine}\ncpu_offload: true\nvram_gb: 2\n"
        ));
        ModelEntry::from_flat(&map, name).expect("entry")
    };
    let mut stampy = entry("stampy", "llama");
    // `on` is in the engine truth set: the stamp must follow it, not a
    // third reading of the token (HANDOFF §6).
    stampy
        .raw
        .insert("cpu_offload".to_string(), "on".to_string());
    let mut nostamp = entry("nostamp", "mock");
    // Real backing files: the persist runs to completion (NOT the wedged-drive
    // sibling) so the stamped triple is pinned as written bytes, not just as
    // in-memory fields.
    let stampy_path = dir.join("stampy.yaml");
    std::fs::write(&stampy_path, "engine: llama\nvram_gb: 2\ncpu_offload: on\n").expect("seed");
    let nostamp_path = dir.join("nostamp.yaml");
    std::fs::write(
        &nostamp_path,
        "engine: mock\nvram_gb: 2\ncpu_offload: true\n",
    )
    .expect("seed");
    stampy.file = Some(stampy_path.clone());
    nostamp.file = Some(nostamp_path.clone());
    state.catalog = vec![stampy.clone(), nostamp.clone()];

    seat_spawn(&mut state, &stampy, &ticket, 18_992);
    state.witness("stampy", &ticket, 4.2);
    assert_eq!(
        (
            state.catalog[0].measured_gb,
            state.catalog[0].measured_min_gb
        ),
        (Some(4.2), Some(4.2)),
        "the pair mints on the offload engine"
    );
    assert_eq!(
        state.catalog[0].measured_offload,
        Some(true),
        "the stamp mints on the engine that reads the key"
    );
    let written = await_yaml_keys(
        &stampy_path,
        &[
            "measured_gb: 4.2",
            "measured_gb_min: 4.2",
            "measured_offload: true",
        ],
    )
    .await;
    assert!(
        written.contains("measured_offload: true"),
        "a genuine llama yaml must gain all three stamped keys on disk — if the \
             persist stops pushing the stamp, this is the only failing test: {written}"
    );

    seat_spawn(&mut state, &nostamp, &ticket, 18_992);
    state.witness("nostamp", &ticket, 4.3);
    assert_eq!(
        (
            state.catalog[1].measured_gb,
            state.catalog[1].measured_min_gb
        ),
        (Some(4.3), Some(4.3)),
        "witness truth is engine-agnostic: the pair still mints on mock"
    );
    assert_eq!(
        state.catalog[1].measured_offload, None,
        "no stamp for an engine whose adapter never reads cpu_offload"
    );
    let written =
        await_yaml_keys(&nostamp_path, &["measured_gb: 4.3", "measured_gb_min: 4.3"]).await;
    assert!(
        !written.contains("measured_offload"),
        "a non-offload yaml must not gain the meaningless stamp on disk: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Actor state for the fold pins: the fold consults no adapter, runtime or
/// witness (each pin drives [`ActorState::on_measured`] with its own reading), so
/// the never-called stand-ins keep the setup to one line per test.
fn fold_state(dir: &std::path::Path) -> ActorState {
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    ActorState::empty(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    )
}

/// The ticket a fold pin seats its spawn under. Any generation works: the fold's
/// `(model, ticket)` guard is satisfied by `seat_spawn` writing the same value.
fn fold_ticket(port: u16) -> SpawnTicket {
    SpawnTicket {
        generation: 7,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port,
    }
}

/// Reads one yaml of the temp dir back through the real parser, as `reload()`
/// would: `read_measured` runs here, so the flip-void rule fires exactly as it
/// does on a rescan — the pins cannot fake the catalog into any shape the scan
/// would not have produced.
fn rescan_entry(path: &std::path::Path, name: &str) -> ModelEntry {
    let text = std::fs::read_to_string(path).expect("fixture yaml readable");
    let (map, problems) = crate::model::scan::parse_flat_yaml(&text);
    assert!(problems.is_empty(), "{problems:?}");
    let mut entry = ModelEntry::from_flat(&map, name).expect("entry");
    entry.file = Some(path.to_path_buf());
    entry
}

/// The `cpu_offload` flip that starts this whole investigation
/// (`research/reload-slot-drift-2026-10-28.md` §1 rows 6, 7, 9): the knob changes
/// in the yaml while the engine still serves under the old argv. The stamp must
/// keep describing the launch that ran — a stamp rewritten to agree with the new
/// bytes silently disarms the flip-void rule, which compares the range with those
/// same live bytes — and the accumulated range must survive the rescan that drops
/// the in-memory pair. Falsifiers: computing `offload` from `entry.raw` inside the
/// fold (the pre-fix shape) flips the stamp leg to `Some(true)`; seeding the fold
/// from the live entry's pair instead of the spawn's drops the low end to `6.0`;
/// each fails here and nowhere else in the suite.
#[tokio::test]
async fn a_mid_serve_offload_flip_stamps_the_launch_that_ran() {
    let dir = crate::testutil::temp_dir("fold-flip");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("flip.yaml");
    std::fs::write(&yaml, "engine: llama\nvram_gb: 20\ncpu_offload: false\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "flip");
    let ticket = fold_ticket(19_001);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_001);

    state.witness("flip", &ticket, 5.0);
    await_yaml_keys(&yaml, &["measured_gb: 5.0", "measured_offload: false"]).await;

    // The operator flips the knob (one line, the measured keys stay) and the
    // catalog is rescanned underneath the serving engine.
    let flipped = std::fs::read_to_string(&yaml)
        .expect("read")
        .replace("cpu_offload: false", "cpu_offload: true");
    std::fs::write(&yaml, flipped).expect("flip");
    state.catalog = vec![rescan_entry(&yaml, "flip")];
    assert_eq!(
        state.catalog[0].measured_gb, None,
        "the rescan's void rule must have fired, or the leg below proves nothing"
    );

    state.witness("flip", &ticket, 6.0);
    let entry = &state.catalog[0];
    assert_eq!(
        entry.measured_offload,
        Some(false),
        "the stamp describes the spawn (`-ngl 99`), not the yaml that arrived after it"
    );
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(6.0), Some(5.0)),
        "the range keeps the history this model built, not a re-seed from the one live process"
    );
    let written = await_yaml_keys(
        &yaml,
        &[
            "measured_gb: 6.0",
            "measured_gb_min: 5.0",
            "measured_offload: false",
        ],
    )
    .await;
    assert!(
        written.contains("cpu_offload: true"),
        "the flip really is on disk, so the contradiction below is real: {written}"
    );
    // And the rule that was supposed to catch it still works: a stamp that cannot
    // match its knob voids the pair, instead of agreeing with it forever.
    let after = rescan_entry(&yaml, "flip");
    assert_eq!(
        (after.measured_gb, after.measured_min_gb),
        (None, None),
        "the flip-void rule is restored, not disarmed by the fold"
    );
    assert_eq!(after.measured_offload, Some(false));
    let _ = std::fs::remove_dir_all(&dir);
}

/// R9 (ruling 15 (ii)): the container's own reserved-footprint report books for an
/// `exact` row that declared nothing — the one class the GGUF fallback must never
/// guess for — fixes BOTH ends of the pair (`measured_gb_min` never understates the
/// declared KV), carries the launch's profile stamp, and outranks every later
/// witness reading: the reserved figure is static, so a noisy total-diff could only
/// corrupt it. Falsifiers: dropping the anchor's `EngineReport` guard lets the
/// witness clobber the range; stamping without the profile leaves the scan's
/// profile-void rule without an anchor to compare.
#[tokio::test]
async fn an_engine_report_books_an_exact_row_and_the_witness_never_clobbers_it() {
    let dir = crate::testutil::temp_dir("fold-engine-report");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("er.yaml");
    std::fs::write(&yaml, "engine: ninfer\ncost: exact\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "er");
    assert_eq!(
        spawned.cost_source(),
        crate::model::CostSource::Undeclared,
        "exact without its declared precision claim starts refused, never estimated"
    );
    let ticket = fold_ticket(19_010);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_010);

    state.engine_report("er", &ticket, 31.0);
    let entry = &state.catalog[0];
    assert_eq!(entry.measured_gb, Some(31.0));
    assert_eq!(
        entry.measured_min_gb,
        Some(31.0),
        "the reserved figure is static: both ends are the reservation"
    );
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::EngineReport)
    );
    assert_eq!(entry.measured_profile.as_deref(), Some("b"));
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Measured,
        "the engine's own reserved budget is a source: exact may book it"
    );

    state.witness("er", &ticket, 9.0);
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(31.0), Some(31.0)),
        "the total-diff cannot widen what the engine itself reported"
    );
    await_yaml_keys(
        &yaml,
        &[
            "measured_gb: 31.0",
            "measured_attribution: engine_report",
            "measured_profile: b",
        ],
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The per-slot carrier behind the API's R9 memory-bar sum: the figure reaches
/// the snapshot ONLY through the spawn anchor of the very job that produced this
/// occupant — engine ninfer, container-backed, attribution `engine_report` — so
/// `/har/status` and the SSE push quote the identical number (review
/// 2026-10-31: threading the sum only through the SSE build made the annotation
/// dead on the poll path). Falsifier: loosening any one of the three anchor
/// gates (the engine check, `anchor.container`, the attribution filter — the
/// very filter with zero coverage before this pin) makes a total-diff free-memory
/// delta or a child-process figure reach the line the operator reads as
/// container-reserved.
#[tokio::test]
async fn only_an_engine_report_anchor_reaches_the_snapshot_slot() {
    let dir = crate::testutil::temp_dir("fold-slot-report");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join("logs")).expect("logs dir");
    let yaml = dir.join("sb.yaml");
    std::fs::write(&yaml, "engine: ninfer\ncost: exact\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "sb");
    let ticket = fold_ticket(19_015);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_015);

    // Witness first: a container total-diff must NOT populate the slot carrier.
    state.on_measured(
        "sb",
        &ticket,
        super::Reading {
            total_diff: 12.0,
            own: None,
            total_gib: 72.0,
            used_gib: 13.0,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].container_reported_gb,
        None,
        "a free-memory delta is not a container reservation"
    );

    // The engine's own record: the same fold that books is what the slot carries.
    state.engine_report("sb", &ticket, 31.0);
    assert_eq!(
        state.snapshot().slots[0].container_reported_gb,
        Some(31.0),
        "the engine_report fold is the ONLY route to the bar's reserved line"
    );

    // A twin under a non-ninfer engine, anchored like a report but with the
    // engine gate's answer changed: the engine check must keep it out.
    let twin = ModelEntry {
        engine: "llama".to_string(),
        ..spawned.clone()
    };
    let twin_ticket = fold_ticket(19_016);
    state.catalog = vec![twin.clone()];
    seat_spawn(&mut state, &twin, &twin_ticket, 19_016);
    state.engine_report("sb", &twin_ticket, 31.0);
    assert_eq!(
        state.snapshot().slots[0].container_reported_gb,
        None,
        "a child-process spawn never reserves container memory, whoever reported it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A report equal to the freshly-persisted stamp must still anchor THIS launch:
/// the persisted pair proves an earlier launch spoke, not this one. The fold's
/// row-idempotence used to return before the anchor promotion, so the live
/// container never displayed as self-reported (card runs 2026-11-02: C2 polled
/// `container_reported_gb` for 150 s through a ninfer serve whose 43.8 GiB
/// report matched the stamp `ninfer-swift-fp8.yaml` already carried).
/// Falsifier: the pre-fix row-only idempotence guard in `fold`.
#[tokio::test]
async fn a_report_equal_to_the_persisted_stamp_still_promotes_the_anchor() {
    let dir = crate::testutil::temp_dir("fold-stamp-promote");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join("logs")).expect("logs dir");
    let yaml = dir.join("sb.yaml");
    std::fs::write(
        &yaml,
        "engine: ninfer\ncost: exact\nmeasured_gb: 31.0\nmeasured_gb_min: 31.0\n\
             measured_attribution: engine_report\nmeasured_profile: b\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "sb");
    assert_eq!(
        spawned.measured_attribution,
        Some(crate::model::MeasuredAttribution::EngineReport),
        "the scanner kept the provenance stamp the live row carries"
    );
    let ticket = fold_ticket(19_017);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_017);
    assert_eq!(
        state.snapshot().slots[0].container_reported_gb,
        None,
        "the persisted stamp is display, not this launch's anchor"
    );

    state.engine_report("sb", &ticket, 31.0);
    assert_eq!(
        state.snapshot().slots[0].container_reported_gb,
        Some(31.0),
        "a fresh report matching the persisted stamp still anchors this launch"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R9 (§5.2) + F1(b): a container that never yielded a parsable memory record
/// still gets measured — but the reading is stamped `total_diff`, and a
/// `total_diff` displays without ever booking. When the report does land (a later
/// startup), it supersedes the witness figure outright. Falsifier: minting the
/// witness stamp as `None` (the pre-R9 shape) books the 12.0 GiB total-diff as if
/// it were a footprint.
#[tokio::test]
async fn a_total_diff_fold_displays_but_never_books_until_the_report_lands() {
    let dir = crate::testutil::temp_dir("fold-total-diff");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("td.yaml");
    std::fs::write(&yaml, "engine: ninfer\ncost: exact\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "td");
    let ticket = fold_ticket(19_011);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_011);

    state.on_measured(
        "td",
        &ticket,
        super::Reading {
            total_diff: 12.0,
            own: None,
            total_gib: 24.0,
            used_gib: 13.0,
        },
    );
    let entry = &state.catalog[0];
    assert_eq!(
        entry.measured_gb,
        Some(12.0),
        "the page still shows the reading"
    );
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::TotalDiff)
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Undeclared,
        "exact never books a total-diff: absence of a source is still absence"
    );

    state.engine_report("td", &ticket, 31.0);
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(31.0), Some(31.0)),
        "the engine's own figure replaces the noisy stand-in rather than widening it"
    );
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::EngineReport)
    );
    assert_eq!(entry.cost_source(), crate::model::CostSource::Measured);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scripted witness for the sampler pins: one canned sample answers every
/// `sample()`. No engine, no driver — the SE fake-everything rule.
struct ScriptedWitness(crate::budget::MemorySample);

#[async_trait::async_trait]
impl crate::budget::VramWitness for ScriptedWitness {
    async fn sample(&self) -> Result<crate::budget::MemorySample, String> {
        Ok(self.0.clone())
    }
    fn source(&self) -> String {
        "scripted".to_string()
    }
}

/// Scripted runtime for the sampler pins: a canned status-snapshot answer
/// (or silence) and a canned direct-children list. Everything else keeps
/// the NeverRuntime posture — a load reaching this runtime is a test bug.
struct StatusRuntime {
    status: Option<serde_json::Value>,
    children: Vec<u32>,
}

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for StatusRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the measurement sampler pin must never load");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        panic!("the measurement sampler pin must never load");
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        panic!("the measurement sampler pin must never load");
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the measurement sampler pin must never load");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the measurement sampler pin must never load");
    }
    async fn engine_status_json(&self, _port: u16, _path: &str) -> Option<serde_json::Value> {
        self.status.clone()
    }
    async fn descendant_pids(&self, _pid: u32) -> Vec<u32> {
        self.children.clone()
    }
}

/// Fake engine metrics JSON on an ephemeral port, served at the path the
/// sampler's path map reads (`/metrics`; the 2026-11-04 capture proved
/// the status routes carry no footprint). The SE fake-server rule, same
/// posture as `serve_health` in `engine/mod.rs`.
async fn serve_status_json(body: &'static str) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral");
    let port = listener.local_addr().expect("addr").port();
    let app =
        axum::Router::new().route("/metrics", axum::routing::get(move || async move { body }));
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    port
}

fn sampler_ctx(model: &str, engine: &str, ticket: SpawnTicket) -> super::measured::SamplerCtx {
    super::measured::SamplerCtx {
        model: model.to_string(),
        ticket,
        own_pids: Vec::new(),
        container: None,
        engine: engine.to_string(),
    }
}

/// The witness shape the sampler pins share: a healthy 72 GiB card, one
/// scripted per-process table.
fn scripted_sample(processes: Vec<(u32, f64)>) -> ScriptedWitness {
    let attribution = crate::budget::attribution_from_processes(&processes);
    ScriptedWitness(crate::budget::MemorySample {
        total_gib: 72.0,
        used_gib: 40.0,
        processes,
        attribution,
    })
}

/// Phase-1 pin, kept honest by the metric parse (`TASK-ENGINE-MEASUREMENT`,
/// ORCHESTRATION §3): the fake metrics server answers with a body the
/// Basalt parse MUST refuse — `vram` is a scalar riding a foreign
/// wrapper, and the parse requires `engine.expert_cache_mib` from the
/// captured `/metrics` engine block (pinned in
/// `engine::basalt::tests::the_scalar_vram_body_stays_refused`) — so the
/// one-shot must NOT consume the sampler: the process-tree fallback has
/// no per-process bytes to sum here either, `report_measurement` returns
/// `None`, and the witness loop keeps standing in. A body the parse
/// ACCEPTS is pinned elsewhere (basalt fake-server chain); this pin
/// exists only for the fallback leg and must never be weakened.
#[tokio::test]
async fn an_unparsable_status_shape_falls_back_to_the_witness_loop() {
    let port = serve_status_json(r#"{"engine":{"info":{"vram":67.4}}}"#).await;
    let runtime =
        Arc::new(crate::process::ProcessRuntime::new()) as Arc<dyn crate::engine::EngineRuntime>;
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let witness = Arc::new(scripted_sample(Vec::new())) as Arc<dyn crate::budget::VramWitness>;
    let ctx = sampler_ctx(
        "stub",
        "basalt",
        SpawnTicket {
            generation: 1,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port,
        },
    );
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &ctx, &witness).await,
        None,
        "the parse refuses the scalar-vram JSON, and the unattributable witness \
             gives the fallback nothing: the sampler must stay alive for the witness loop"
    );
    assert!(
        rx.try_recv().is_err(),
        "a refused one-shot sends no MeasuredProcessReport"
    );
}

/// The engine gate and the refused-figure gate of the same one-shot: a
/// non-process engine is never touched, and figures
/// `measurable_figure` would drop (sub-quantum, larger than the card)
/// return `None` WITHOUT a message — the M1 doctrine that a consumed
/// one-shot silently skips the witness stand-in (review 2026-10-31).
#[tokio::test]
async fn a_refused_measurement_never_consumes_the_sampler() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let runtime = Arc::new(StatusRuntime {
        status: None,
        children: Vec::new(),
    }) as Arc<dyn crate::engine::EngineRuntime>;
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 19_020,
    };
    // A llama row never gets a process-engine one-shot at all.
    let llama_witness =
        Arc::new(scripted_sample(vec![(4242, 30.0)])) as Arc<dyn crate::budget::VramWitness>;
    let llama_ctx = sampler_ctx("plain", "llama", ticket.clone());
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &llama_ctx, &llama_witness).await,
        None,
        "the one-shot belongs to basalt/strata only"
    );
    // A sub-quantum tree sum: absence, never a fake number and never a
    // consumed sampler.
    let tiny = Arc::new(scripted_sample(vec![(4242, 0.02)])) as Arc<dyn crate::budget::VramWitness>;
    let strata_ctx = sampler_ctx("tiny", "strata", ticket.clone());
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &strata_ctx, &tiny).await,
        None,
        "a sub-quantum figure is refused here so the witness loop keeps standing in"
    );
    // A figure larger than the card: same fall-through.
    let giant =
        Arc::new(scripted_sample(vec![(4242, 80.0)])) as Arc<dyn crate::budget::VramWitness>;
    let giant_ctx = sampler_ctx("giant", "strata", ticket.clone());
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &giant_ctx, &giant).await,
        None,
        "a footprint beyond the card is nonsense, not a booking"
    );
    assert!(
        rx.try_recv().is_err(),
        "no refused figure may reach the actor as a MeasuredProcessReport"
    );
}

/// When the status endpoint stays silent but the driver attributes concrete
/// bytes, the one-shot sums the engine pid plus its DIRECT children only
/// (the 9999 twin belongs to nobody here) and sends the `ProcessTreeNvml`
/// measurement ONCE with the live card total the witness just read
/// (ORCHESTRATION §1/§3, doctrine 4).
#[tokio::test]
async fn the_tree_fallback_sums_the_engine_and_its_direct_children() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let runtime = Arc::new(StatusRuntime {
        status: None,
        children: vec![4243],
    }) as Arc<dyn crate::engine::EngineRuntime>;
    let witness = Arc::new(scripted_sample(vec![
        (4242, 20.0),
        (4243, 10.0),
        (9999, 5.0),
    ])) as Arc<dyn crate::budget::VramWitness>;
    let ctx = sampler_ctx(
        "tree",
        "strata",
        SpawnTicket {
            generation: 3,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port: 19_021,
        },
    );
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &ctx, &witness).await,
        // Phase 4: the landing now reports the booked figure it seeded the
        // actor with (the same rounded value the fold will land), so the
        // elastic refresh can judge its hysteresis against the truth — the
        // tree sum of the engine plus its direct children, 20 + 10.
        Some(30.0)
    );
    let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .expect("the measurement reaches the actor")
        .expect("channel open");
    let ActorMsg::MeasuredProcessReport {
        model,
        ticket,
        measurement,
        total_gib,
    } = msg
    else {
        panic!("expected the process-measurement message");
    };
    assert_eq!(model, "tree");
    assert_eq!(ticket.generation, 3);
    assert_eq!(
        measurement.gpu_gib, 30.0,
        "20.0 + 10.0, the foreign twin excluded"
    );
    assert_eq!(
        measurement.source,
        crate::engine::MeasurementSource::ProcessTreeNvml
    );
    assert_eq!(
        measurement.confidence,
        crate::engine::MeasurementConfidence::Low,
        "the tree sum is a footprint read from outside the engine: Low confidence"
    );
    assert_eq!(
        total_gib, 72.0,
        "the message carries the live witness card total"
    );
}

/// Ruling 2026-11-04 (`TASK-ENGINE-MEASUREMENT`): a process engine's
/// `EngineStatus` figure books with `EngineReport`-class trust — it REPLACES
/// the spawn's persisted range (no averaging: a status figure is static for
/// the launch) and, as max truth, overrides the declared `vram_gb` it
/// exceeds. After it lands, witness readings can no longer fold into the
/// range (the guard `engine_report` always had, extended). Falsifiers: the
/// old single-variant witness guard (the 9.0 total-diff would clobber the
/// 67.0), and `engine_report_outranks_declared` without `EngineStatus`
/// (the declared 55 would keep booking).
#[tokio::test]
async fn an_engine_status_measurement_books_over_the_declared_value() {
    let dir = crate::testutil::temp_dir("fold-engine-status");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("es.yaml");
    std::fs::write(
        &yaml,
        "engine: strata\ncost: fixed\nvram_gb: 55\nmeasured_gb: 50.0\nmeasured_gb_min: 40.0\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "es");
    let ticket = fold_ticket(19_022);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_022);

    state.process_measurement(
        "es",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 67.0,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(67.0), Some(67.0)),
        "both ends are the footprint: the persisted 40.0 low end is replaced, not averaged"
    );
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::EngineStatus)
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Measured,
        "the engine's own status figure above the declared floor books instead of it"
    );
    assert!(
        entry.measured_profile.is_none(),
        "no ninfer profile on a strata row"
    );

    state.witness("es", &ticket, 9.0);
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(67.0), Some(67.0)),
        "a total-diff cannot widen what the engine itself reported"
    );
    let written = await_yaml_keys(
        &yaml,
        &[
            "measured_gb: 67.0",
            "measured_gb_min: 67.0",
            "measured_attribution: engine_status",
        ],
    )
    .await;
    assert!(
        !written.contains("measured_profile"),
        "the status fold mints no profile stamp: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The `ProcessTreeNvml` source folds as the existing `PerProcess`
/// footprint — persisted under the old token (ORCHESTRATION §2: no new yaml
/// keys), replacing the spawn's range outright, and NOT overriding the
/// declared value (only the engine-spoken reports get max truth).
#[tokio::test]
async fn a_process_tree_measurement_lands_as_per_process_and_replaces_the_range() {
    let dir = crate::testutil::temp_dir("fold-tree-sum");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("ts.yaml");
    std::fs::write(
        &yaml,
        "engine: strata\nvram_gb: 55\nmeasured_gb: 50.0\nmeasured_gb_min: 40.0\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "ts");
    let ticket = fold_ticket(19_023);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_023);

    state.process_measurement(
        "ts",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 30.0,
            ram_gib: None,
            source: crate::engine::MeasurementSource::ProcessTreeNvml,
            confidence: crate::engine::MeasurementConfidence::Low,
        },
    );
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(30.0), Some(30.0)),
        "a tree sum is this launch's footprint: it fixes both ends instead of averaging"
    );
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::PerProcess),
        "the tree sum persists as the existing per_process provenance"
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Declared,
        "below the declared floor the operator's figure stays the booking"
    );
    await_yaml_keys(
        &yaml,
        &["measured_gb: 30.0", "measured_attribution: per_process"],
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The actor-side twin of the sampler's M1 gate: a figure
/// `measurable_figure` refuses (negative, beyond the card) and a reading
/// that lost its uncontested window are both visible drops that book
/// nothing — the one-shot does not retry, so silence here would be a lost
/// measurement (review 2026-10-31, M1 doctrine).
#[tokio::test]
async fn a_nonsense_process_measurement_books_nothing_and_names_itself() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fold-nonsense-meas");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("nm.yaml");
    std::fs::write(&yaml, "engine: strata\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "nm");
    let ticket = fold_ticket(19_024);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_024);

    let measurement = |gpu_gib: f64| crate::engine::EngineMeasurement {
        gpu_gib,
        ram_gib: None,
        source: crate::engine::MeasurementSource::EngineStatus,
        confidence: crate::engine::MeasurementConfidence::High,
    };
    state.process_measurement("nm", &ticket, measurement(-5.0));
    state.process_measurement("nm", &ticket, measurement(80.0));
    // A reading for a ticket that is not the live occupant's: dropped, and named.
    state.process_measurement("nm", &fold_ticket(19_099), measurement(12.0));
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_attribution),
        (None, None),
        "nonsense never books and never persists"
    );

    let warns = capture.lock().expect("capture lock");
    assert!(
        warns
            .iter()
            .any(|(message, _)| message.contains("process measurement refused")),
        "the refused figures must be named once: {warns:?}"
    );
    assert!(
        warns
            .iter()
            .any(|(message, _)| message.contains("process measurement dropped")),
        "the race-loss drop must be named once: {warns:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R3 ruling 10: a total-diff read on a foreign-dominated card mints nothing.
/// The card holds far more than this engine plus the other occupants can
/// account for, so the delta is someone else's story — absence, labelled by
/// the warn, never a 23.8–59.3 stand-in. Falsifier: deleting the
/// `ForeignDominated` guard persists the 12.0 GiB reading to the yaml.
#[tokio::test]
async fn a_foreign_dominated_card_mints_no_total_diff() {
    let dir = crate::testutil::temp_dir("fold-foreign-dominated");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("fd.yaml");
    std::fs::write(&yaml, "engine: ninfer\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "fd");
    let ticket = fold_ticket(19_021);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_021);
    // The admit baseline: the card was near-empty when this load was admitted,
    // so everything it holds beyond this engine at sample time is memory that
    // MOVED during the window — 26 GiB of foreign story against a 12 GiB delta.
    if let Some((_, job)) = state
        .jobs
        .iter_mut()
        .find(|(_, j)| j.spawned.as_ref() == Some(&ticket))
    {
        job.observed_at_admit = Some(50.0);
    }

    state.on_measured(
        "fd",
        &ticket,
        super::Reading {
            total_diff: 12.0,
            own: None,
            total_gib: 72.0,
            used_gib: 60.0,
        },
    );
    assert_eq!(
        state.catalog[0].measured_gb, spawned.measured_gb,
        "a foreign-dominated reading must not mint a range"
    );
    assert_eq!(state.catalog[0].measured_attribution, None);
    let written = std::fs::read_to_string(&yaml).expect("file readable");
    assert!(
        !written.contains("measured_gb"),
        "nothing reaches the yaml: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// R1 + R9: a detached container's ticket pid is the dead `wsl.exe` client, so a
/// container spawn may NEVER claim `per_process` — even a witness reading that
/// carries concrete bytes for that pid is a lie waiting to be stamped. The
/// falsifier is minting the stamp from `own` alone: the container row would book
/// 12.0 GiB as its own from a number the card never attributed to the engine.
#[tokio::test]
async fn a_container_reading_may_never_claim_per_process_attribution() {
    let dir = crate::testutil::temp_dir("fold-container-own");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join("logs")).expect("logs dir");
    let yaml = dir.join("cn.yaml");
    std::fs::write(&yaml, "engine: ninfer\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "cn");
    let ticket = fold_ticket(19_012);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_012);

    state.witness("cn", &ticket, 12.0);
    let entry = &state.catalog[0];
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::TotalDiff),
        "a container's pid is the dead client: concrete bytes for it attribute nothing"
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Declared,
        "the declared number still books; the total-diff never competes"
    );
    // The persist must carry the honest stamp: a file left with a bare
    // `measured_gb` would grandfather the stand-in back into booking on the next
    // scan, which is exactly the lie this pin exists to keep out.
    let written = await_yaml_keys(&yaml, &["measured_attribution: total_diff"]).await;
    assert!(
        !written.contains("measured_profile"),
        "a witness range carries no profile stamp: {written}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The counterpart leg: a child-process engine whose card DID report concrete
/// bytes for the engine's own pid earns `per_process` — a provenance that books,
/// written to the file alongside the pair. Falsifier: stamping every witnessed
/// fold `total_diff` breaks the booking leg here while the container leg above
/// stays green.
#[tokio::test]
async fn a_per_process_fold_books_and_stamps_the_file() {
    let dir = crate::testutil::temp_dir("fold-per-process");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::create_dir_all(dir.join("logs")).expect("logs dir");
    let yaml = dir.join("pp.yaml");
    std::fs::write(&yaml, "engine: llama\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "pp");
    let ticket = fold_ticket(19_013);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_013);

    state.witness("pp", &ticket, 7.4);
    let entry = &state.catalog[0];
    assert_eq!(
        entry.measured_attribution,
        Some(crate::model::MeasuredAttribution::PerProcess)
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Measured,
        "a driver-attributed footprint is a real booking source for a fixed row"
    );
    await_yaml_keys(
        &yaml,
        &["measured_gb: 7.4", "measured_attribution: per_process"],
    )
    .await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Fork C extended to the provenance stamps: a range the disk refused must not
/// keep the stamp that would book it. With the grandfathering rule (an UNSTAMPED
/// measured pair books), a drain that cleared only the pair and left
/// `measured_attribution` behind would resurrect the refused figure as bookable on
/// the next rescan — this pin is the falsifier for exactly that half-drain.
#[test]
fn a_refused_persist_drains_the_provenance_stamps_with_the_pair() {
    let dir = crate::testutil::temp_dir("fold-drain-stamps");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("ds.yaml");
    std::fs::write(&yaml, "engine: ninfer\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let mut entry = rescan_entry(&yaml, "ds");
    entry.measured_gb = Some(30.0);
    entry.measured_min_gb = Some(30.0);
    entry.measured_attribution = Some(crate::model::MeasuredAttribution::EngineReport);
    entry.measured_profile = Some("b".to_string());
    state.catalog = vec![entry];

    state.fail_persist(
        "ds",
        super::MeasuredData {
            high: 30.0,
            low: 30.0,
            offload: None,
            attribution: Some(crate::model::MeasuredAttribution::EngineReport),
            profile: Some("b".to_string()),
        },
        Some(yaml.clone()),
    );
    let entry = &state.catalog[0];
    assert_eq!(entry.measured_gb, None);
    assert_eq!(
        entry.measured_attribution, None,
        "an orphaned provenance stamp would grandfather the refused pair back to bookable"
    );
    assert_eq!(entry.measured_profile, None);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sampler's fallback shape (R9 §5.2): with no container report seeded, the
/// one-shot finds nothing, the witness loop still runs, and what lands is stamped
/// `total_diff` — displayed, never booked. A regression that swallows the report
/// path entirely looks exactly like this pin from the outside, which is why the
/// report-present legs live in `tests/r9_container_report.rs` against the faked
/// runtime seam rather than being asserted only here.
#[tokio::test]
async fn a_container_without_a_report_falls_back_to_the_witness_loop() {
    let dir = crate::testutil::temp_dir("fold-report-fallback");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("fb.yaml");
    std::fs::write(&yaml, "engine: ninfer\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "fb");
    let ticket = fold_ticket(19_014);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_014);

    state.on_measured(
        "fb",
        &ticket,
        super::Reading {
            total_diff: 18.0,
            own: None,
            total_gib: 24.0,
            used_gib: 19.0,
        },
    );
    assert_eq!(
        state.catalog[0].measured_attribution,
        Some(crate::model::MeasuredAttribution::TotalDiff),
        "the stand-in keeps its honest provenance"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// FINDINGS-13 [3]'s fold half: two rows answering to one identity. The reading
/// belongs to the file the engine was spawned from, and the reconcile of a refused
/// persist may only drain that same row — never the twin whose bytes the refused
/// write never touched. The catalog order puts the twin FIRST so any name-keyed
/// target (the pre-fix `find(|m| m.name == model)`, in the fold and in the drain)
/// picks the wrong file and both legs below fail.
#[tokio::test]
async fn a_twin_name_folds_only_into_the_spawn_era_file() {
    let dir = crate::testutil::temp_dir("fold-twin");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mine = dir.join("twin.yaml");
    let twin = dir.join("twin .yaml");
    std::fs::write(&mine, "engine: llama\nvram_gb: 20\n").expect("seed mine");
    std::fs::write(&twin, "engine: llama\nvram_gb: 20\n").expect("seed twin");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&mine, "twin");
    let ticket = fold_ticket(19_002);
    // Catalog order deliberately the other way round: the twin owns the first
    // slot, so a name lookup answers with the row this engine never came from.
    state.catalog = vec![rescan_entry(&twin, "twin"), spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_002);

    state.witness("twin", &ticket, 4.4);
    let folded = await_yaml_keys(&mine, &["measured_gb: 4.4"]).await;
    assert!(
        folded.contains("measured_gb_min: 4.4"),
        "the spawn-era file gains the pair: {folded}"
    );
    assert_eq!(
        (
            state.catalog[0].measured_gb,
            state.catalog[0].measured_min_gb,
            state.catalog[0].measured_offload
        ),
        (None, None, None),
        "the twin row books nothing for a launch it did not describe"
    );
    let untouched = std::fs::read_to_string(&twin).expect("read twin");
    assert!(
        !untouched.contains("measured_gb"),
        "the twin's bytes are not the resident's measurement sheet: {untouched}"
    );

    // The refused-persist reconcile is held to the same target: equal values on
    // both rows may not make the twin drainable. Both rows wear exactly what the
    // resident's own fold wrote — only the row whose FILE the refused write
    // targeted may drain.
    for index in 0..2 {
        state.catalog[index].measured_gb = Some(4.4);
        state.catalog[index].measured_min_gb = Some(4.4);
        state.catalog[index].measured_offload = Some(false);
        state.catalog[index].measured_attribution =
            Some(crate::model::MeasuredAttribution::PerProcess);
    }
    state.fail_persist(
        "twin",
        super::MeasuredData {
            high: 4.4,
            low: 4.4,
            offload: Some(false),
            attribution: Some(crate::model::MeasuredAttribution::PerProcess),
            profile: None,
        },
        Some(mine.clone()),
    );
    assert_eq!(
        state.catalog[1].measured_gb, None,
        "the row the refused write targeted drains"
    );
    assert_eq!(
        state.catalog[0].measured_gb,
        Some(4.4),
        "the same-name twin keeps the values the refused write never touched"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The row that owned a serving engine vanishes mid-serve (renamed away, turned
/// into a problem line, given an engine with no adapter). Nothing may be minted or
/// extended without it, and the sampler that produced the reading is retired —
/// left alone it walks the witness every `measured_sample_s` for a day with
/// nowhere to put the answer, and says nothing. Falsifiers: dropping the detach
/// `return` (the fold would carry on against whatever row the name still reaches)
/// leaves the sampler armed and the reading booked; dropping the `anchor.file`
/// guard writes a triple to no file at all and clears nothing.
#[tokio::test]
async fn a_vanished_row_fails_the_fold_closed_and_retires_the_sampler() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fold-vanished");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("gone.yaml");
    std::fs::write(&yaml, "engine: llama\nvram_gb: 20\n").expect("seed");
    let mut state = fold_state(&dir);
    let spawned = rescan_entry(&yaml, "gone");
    let ticket = fold_ticket(19_003);
    state.catalog = vec![spawned.clone()];
    seat_spawn(&mut state, &spawned, &ticket, 19_003);
    let sampler = tokio::spawn(async {
        loop {
            tokio::time::sleep(Duration::from_secs(3600)).await;
        }
    });
    state.slots[0]
        .occupant
        .as_mut()
        .expect("seated occupant")
        .measure = Some(sampler);

    // The operator turns the file into a problem line; Fork C never publishes it,
    // so the rescan leaves the serving engine with no row at all.
    std::fs::write(&yaml, "engine: llama\nbad key: x\n").expect("break the row");
    state.catalog = Vec::new();

    state.witness("gone", &ticket, 9.0);
    assert!(
        state.catalog.is_empty(),
        "no row may be booked, created or extended for a vanished launch"
    );
    assert!(
        state.slots[0]
            .occupant
            .as_ref()
            .and_then(|o| o.measure.as_ref())
            .is_none(),
        "the orphaned sampler is retired, not left polling the card"
    );
    let text = std::fs::read_to_string(&yaml).expect("read back");
    assert!(
        !text.contains("measured_gb"),
        "nothing is persisted for the reading that had no row: {text}"
    );
    let warns = capture.lock().expect("capture lock");
    assert!(
        warns.iter().any(|(message, fields)| {
            message.contains("measured reading has no catalog row")
                && fields.get("model").is_some_and(|m| m.contains("gone"))
                && fields.get("file").is_some_and(|f| f.contains("gone.yaml"))
        }),
        "the silence is the bug this leg pins: {warns:?}"
    );

    // Second leg: a spawn with no anchor at all (no `begin_spawn` ever ran for it)
    // folds nothing either — absence, never a number with no provenance.
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut bare = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    bare.catalog = vec![spawned];
    let unanchored = SpawnTicket {
        generation: 8,
        kind: ResourceKind::ChildProcess,
        pid: 4243,
        port: 19_004,
    };
    bare.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 19_004,
        occupant: Some(Occupant {
            model: "gone".to_string(),
            need: Gibibytes(2.0),
            ram_need: None,
            port: 19_004,
            ticket: unanchored.clone(),
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: None,
            stale_config: false,
            serve_id: None,
            measure: None,
        }),
        job: None,
    }];
    bare.witness("gone", &unanchored, 9.0);
    assert_eq!(
        bare.catalog[0].measured_gb, None,
        "an unanchored spawn has no stamp to mint and no file to write"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The Axis 1 predicate table (ruling 2026-10-29), including the
/// unreadable-stat windows the actor path cannot race into existence
/// deterministically (the scan that publishes the bytes rides the same read
/// the stat does), so the `None` legs are manufactured here directly: a
/// missing fingerprint — at registration or at rescan — is the absence of
/// evidence, never evidence of change. Falsifiers: treating `None` as change
/// fails every KEEP leg; disabling the invalidation fails both RETIRE legs;
/// an always-retire predicate fails KEEP 1 (whose actor-path twin is
/// `an_unchanged_rescan_keeps_the_queue_and_the_parked_load_still_loads`).
#[test]
fn a_rescan_keeps_a_queue_whose_fingerprint_cannot_show_a_change() {
    let dir = crate::testutil::temp_dir("rescan-fp");
    let yaml = dir.join("m.yaml");
    let entry_on_disk = |bytes: &str| -> ModelEntry {
        std::fs::write(&yaml, bytes).expect("seed");
        let (map, _) = crate::model::scan::parse_flat_yaml("engine: mock\nvram_gb: 4\n");
        let mut entry = ModelEntry::from_flat(&map, "m").expect("entry");
        entry.file = Some(yaml.clone());
        entry
    };
    let fresh_state = || -> ActorState {
        let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
        let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
        adapters.insert(
            "mock".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        ActorState::empty(
            crate::config::AppConfig::default(),
            dir.clone(),
            adapters,
            Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        )
    };
    let queue_on = |state: &mut ActorState, slot: usize, entry: &ModelEntry| -> JobId {
        state.slots = vec![Slot {
            category: CategoryId::Work,
            index: slot,
            engine_port: 18_100 + slot as u16,
            occupant: None,
            // The parked row does not own the slot: some other job is in
            // flight on it, exactly as parking requires.
            job: Some(JobId(9_999)),
        }];
        let id = state.register_queued_load(slot, entry, Vec::new());
        state.pending.insert(slot, id);
        id
    };
    let apply = |state: &mut ActorState, models: Vec<ModelEntry>| {
        let catalog = crate::model::scan::Catalog {
            models,
            errors: Vec::new(),
        };
        let drift = drift_stats_for(state, &catalog);
        state.apply_catalog(catalog, None, &drift);
    };
    let queued_alive = |state: &ActorState, id: JobId| {
        state.jobs.get(&id).is_some_and(|j| !j.terminal) && state.pending.values().any(|p| *p == id)
    };

    // KEEP 1: same bytes, same clock — the no-change rescan must not drop
    // the queue (budget kept intact, place kept, row non-terminal).
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let id = queue_on(&mut state, 0, &entry);
    apply(&mut state, vec![entry.clone()]);
    assert!(
        queued_alive(&state, id),
        "a no-change rescan must never drop a queue"
    );

    // KEEP 2: the file became unstat'able by apply time (bytes published
    // from the earlier read, the stat window lost the file) — a missing
    // rescan-side fingerprint is not evidence.
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let id = queue_on(&mut state, 0, &entry);
    std::fs::remove_file(&yaml).expect("delete after the scan read");
    apply(&mut state, vec![entry.clone()]);
    assert!(
        queued_alive(&state, id),
        "an unreadable rescan-time stat must not retire"
    );

    // KEEP 3: the row queued on an unreadable file (no registration
    // baseline at all) — even readable-and-changed bytes now prove nothing.
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let id = queue_on(&mut state, 0, &entry);
    state.jobs.get_mut(&id).expect("row").queue_fingerprint = None;
    std::fs::write(&yaml, "vram_gb: 400\n").expect("edit");
    apply(&mut state, vec![entry.clone()]);
    assert!(
        queued_alive(&state, id),
        "a row without a registration baseline survives the rescan"
    );

    // RETIRE: bytes moved against a real baseline — the named reason is on
    // the row and the queue place is released.
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let id = queue_on(&mut state, 0, &entry);
    std::fs::write(&yaml, "vram_gb: 400\n").expect("edit");
    apply(&mut state, vec![entry]);
    assert!(
        state.jobs[&id].terminal,
        "moved bytes must retire the intent"
    );
    assert!(state.pending.is_empty(), "and release the queue place");
    let error = state.jobs[&id]
        .error
        .clone()
        .expect("the reason rides the ledger row")
        .to_string();
    assert!(
        error.contains("superseded by rescan")
            && error.contains("m.yaml")
            && error.contains("changed since queued")
            && error.contains("re-queue to apply"),
        "{error}"
    );

    // RETIRE: the model left the catalog.
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let id = queue_on(&mut state, 0, &entry);
    apply(&mut state, Vec::new());
    assert!(
        state.jobs[&id].terminal,
        "a vanished model retires its intent"
    );
    assert!(
        state.jobs[&id].error.as_ref().is_some_and(|e| e
            .to_string()
            .contains("superseded by rescan (removed from the catalog)")),
        "{:?}",
        state.jobs[&id].error
    );

    // KEEP 4: the dispatch-to-apply corner (FINDINGS-07 [8] fix round [3]):
    // row A parked M at dispatch (its file gathered pre-edit), the scan
    // held, an operator edit landed, and row B parked the same model before
    // the scan landed. B's fingerprint is newer than any stat gathered
    // before its birth — judging it could only invent "changed since
    // queued" — so rows registered after the dispatch are kept
    // unconditionally. Falsifier: drop the `queued_at_dispatch` gate and B
    // lands in the (Some(newer), Some(older)) arm and retires.
    let entry = entry_on_disk("vram_gb: 4\n");
    let mut state = fresh_state();
    let a = queue_on(&mut state, 0, &entry);
    let gathered = drift_stats_for(
        &state,
        &crate::model::scan::Catalog {
            models: vec![entry.clone()],
            errors: Vec::new(),
        },
    );
    let edited = entry_on_disk("vram_gb: 400\n");
    let b = queue_on(&mut state, 1, &edited);
    state.apply_catalog(
        crate::model::scan::Catalog {
            models: vec![edited],
            errors: Vec::new(),
        },
        None,
        &gathered,
    );
    assert!(
        queued_alive(&state, b),
        "a row registered after the reload was dispatched must survive it"
    );
    assert!(
        queued_alive(&state, a),
        "and the gathered evidence shows no change for the row it did see"
    );
}

/// Shared fixture for the Axis-2 resident pins: one `ActorState` with one
/// resident whose `spawn_config` is a real capture of a real yaml (the state
/// `install` produces — same fields, taken from the same helper), the file's
/// fingerprint, and the entry pair the fold would have written.
fn resident_on_a_file() -> (
    ActorState,
    std::path::PathBuf,
    crate::model::FileFingerprint,
) {
    let dir = crate::testutil::temp_dir("stale-resident");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let yaml = dir.join("m.yaml");
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 10.0\nmeasured_gb_min: 8.0\n",
    )
    .expect("seed");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let capture = crate::model::fingerprint(&yaml).expect("seeded file fingerprints");
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 18_431,
    };
    state.catalog = vec![entry_from(&yaml, "m")];
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_431,
        occupant: Some(Occupant {
            model: "m".to_string(),
            need: Gibibytes(4.0),
            ram_need: None,
            port: 18_431,
            ticket,
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: Some((yaml.clone(), capture)),
            stale_config: false,
            serve_id: None,
            measure: None,
        }),
        job: None,
    }];
    (state, yaml, capture)
}

fn entry_from(path: &std::path::Path, name: &str) -> ModelEntry {
    let text = std::fs::read_to_string(path).expect("readable");
    let (map, _) = crate::model::scan::parse_flat_yaml(&text);
    let mut entry = ModelEntry::from_flat(&map, name).expect("entry");
    entry.file = Some(path.to_path_buf());
    entry
}

/// Mirrors the off-actor gather a real reload performs: the resident capture
/// files plus the queued models' new-catalog entry files, stat'ed here on the
/// test thread (production stats on a blocking thread — FINDINGS-07 [8]).
fn drift_stats_for(state: &ActorState, catalog: &crate::model::scan::Catalog) -> DriftStats {
    let queued_at_dispatch: HashSet<JobId> = state.queued_load_candidates().into_iter().collect();
    let queued = state.queued_models_for(&queued_at_dispatch);
    let mut paths = state.drift_capture_files();
    paths.extend(
        catalog
            .models
            .iter()
            .filter(|entry| queued.contains(&entry.name))
            .filter_map(|entry| entry.file.clone()),
    );
    DriftStats {
        queued_at_dispatch,
        ..gather_drift_stats(&paths)
    }
}

/// The reload-turn rescan the pins drive: re-parse the file the operator (or the
/// harness) just wrote and land it through `apply_catalog` — the only judge
/// between install and the next launch.
fn rescan(state: &mut ActorState, yaml: &std::path::Path, name: &str) {
    let catalog = crate::model::scan::Catalog {
        models: vec![entry_from(yaml, name)],
        errors: Vec::new(),
    };
    let drift = drift_stats_for(state, &catalog);
    state.apply_catalog(catalog, None, &drift);
}

/// The yaml fingerprint alone carries the `stale_config` verdict since
/// FINDINGS-25 retired the dormant profile leg: an unchanged row never
/// accuses, and an edit to the resident's own row ambers on the refresh
/// turn with no other capture anywhere — the same amber, the same
/// unload + reload gesture. Falsifier: dropping the yaml leg from
/// `refresh_stale_markers` (amber never fires). The no-capture posture is
/// pinned separately by the L3 leg, which has no capture at all.
#[test]
fn the_yaml_fingerprint_alone_ambers_the_resident_on_a_changed_row() {
    let (mut state, yaml, _capture) = resident_on_a_file();
    state.refresh_stale_markers(&gather_drift_stats(std::slice::from_ref(&yaml)));
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(false),
        "an unchanged fingerprint must not accuse"
    );
    std::fs::write(&yaml, "engine: mock\n").expect("edit row");
    state.refresh_stale_markers(&gather_drift_stats(std::slice::from_ref(&yaml)));
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(true),
        "a changed yaml fingerprint ambers on its own"
    );
}

/// Axis 2 = B (ruling 2026-10-29), reload leg: an edit only ambers when a rescan
/// lands — between the edit and the rescan the status path must not notice
/// (one-stat-per-event: the snapshot never stats). Falsifier: removing
/// `refresh_stale_markers` from `apply_catalog` fails the amber assert; moving
/// the verdict computation into the snapshot (or any status read) fails the
/// pre-rescan assert.
#[test]
fn a_resident_ambers_on_the_reload_that_finds_newer_bytes_than_its_launch() {
    let (mut state, yaml, _) = resident_on_a_file();
    let stamp = state.snapshot().slots[0]
        .launched_at
        .clone()
        .expect("a resident carries its wall-clock launch");
    assert_eq!(state.snapshot().slots[0].stale_config, Some(false));
    let original = std::fs::read_to_string(&yaml).expect("read");
    std::fs::write(&yaml, format!("{original}# operator note\n")).expect("operator edit");
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(false),
        "an edit without a rescan must not move the verdict from a status read: \
             the snapshot never stats"
    );
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].stale_config,
        Some(true),
        "newer bytes than the spawn capture must amber on the reload turn"
    );
    assert_eq!(
        after.slots[0].launched_at.as_deref(),
        Some(stamp.as_str()),
        "the launch stamp rides the install, not the rescan"
    );
}

/// Interlock (4), stated plainly: the comparison rides the SPAWN capture
/// (mtime + size), so re-saving byte-identical content does NOT clear the
/// verdict — the clock moved. The apply gesture is unload + reload, and a
/// respawned engine re-captures cleanly. Falsifier: a verdict that compares
/// content, or that clears on size-equality, takes the final assert from true
/// to false here (the size/mtime probes prove the leg is real either way).
#[test]
fn restoring_identical_bytes_does_not_clear_the_stale_verdict() {
    let (mut state, yaml, capture) = resident_on_a_file();
    let original = std::fs::read(&yaml).expect("read");
    std::fs::write(
        &yaml,
        // The `cpu_offload` flag key (phase 4): a PURE cost edit is now
        // re-booked without a reload, so an amber pin must edit a flag —
        // the launch config the running engine never read — to earn the
        // verdict this pin then defends against a byte-identical restore.
        "engine: mock\ncost: fixed\nmeasured_gb: 444.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("diverge (size moves: no dependence on the clock tick)");
    rescan(&mut state, &yaml, "m");
    assert_eq!(state.snapshot().slots[0].stale_config, Some(true));
    std::fs::write(&yaml, &original).expect("restore the identical bytes");
    // A same-tick rewrite can leave the recorded mtime unmoved (Windows clock
    // granularity), so force the clock forward: the restore is byte-identical,
    // the timestamp factually newer — the state a re-save really produces.
    let file = std::fs::OpenOptions::new()
        .write(true)
        .open(&yaml)
        .expect("open for the clock bump");
    file.set_times(
        std::fs::FileTimes::new().set_modified(
            capture
                .mtime
                .checked_add(Duration::from_millis(60_000))
                .expect("mtime in range"),
        ),
    )
    .expect("force the mtime forward");
    drop(file);
    let now = crate::model::fingerprint(&yaml).expect("stat");
    assert_eq!(
        now.size, capture.size,
        "byte-identical restore: the SIZE cannot tell it apart"
    );
    assert_ne!(
        now.mtime, capture.mtime,
        "and the clock moved (pre {:?} vs post {:?}) — this is the leg size-only would miss",
        capture.mtime, now.mtime
    );
    rescan(&mut state, &yaml, "m");
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(true),
        "byte-identical content restored later is STILL stale: honest conservatism, \
             the operator applies with unload + reload"
    );
}

/// Interlock (1): stale is INFO, not policy. An amber resident keeps its
/// measured pair, its cost basis and its slot exactly as a clean one does —
/// the marker never touches what admission books or who holds the card.
/// Falsifier: gating `cost_source`/measured validity (or the slot) on
/// `stale_config` fails this while the verdict pins stay green.
#[test]
fn a_stale_resident_keeps_serving_its_measured_range() {
    let (mut state, yaml, _) = resident_on_a_file();
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 10.0\nmeasured_gb_min: 8.0\n# operator note\n",
    )
    .expect("operator edit that keeps the measured keys intact");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(after.slots[0].stale_config, Some(true), "amber first");
    let entry = state
        .catalog
        .iter()
        .find(|m| m.name == "m")
        .expect("resident's row");
    assert_eq!(
        (entry.measured_gb, entry.measured_min_gb),
        (Some(10.0), Some(8.0)),
        "the measured pair is witness truth: an amber marker does not void it"
    );
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Measured,
        "and admission still books from it — stale never demotes the cost basis"
    );
    let occupant = after.slots[0].clone();
    assert_eq!(occupant.model.as_deref(), Some("m"));
    assert_eq!(occupant.port, Some(18_431), "the resident keeps its slot");
}

/// Interlock (5), actor side: a harness-owned write (the measured fold, the API
/// flag edit) re-baselines the spawn capture — never through the verdict, never
/// over a true one. Falsifiers: no-op-ing `on_model_file_written` makes the
/// unchanged rescan after the write amber (L1 fails); re-baselining regardless
/// of the verdict erases the operator's earned amber (L2's final assert fails).
#[test]
fn the_harness_own_write_rebaselines_a_clean_resident_captured_spawn_side() {
    // L1: clean resident + harness write + carried fingerprint -> the verdict
    // stays false AND survives the next unchanged rescan (the capture moved).
    let (mut state, yaml, _) = resident_on_a_file();
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 12.0\nmeasured_gb_min: 8.0\n",
    )
    .expect("fold write");
    let written = crate::model::fingerprint(&yaml).expect("stat");
    state.on_model_file_written("m", &yaml, written);
    rescan(&mut state, &yaml, "m");
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(false),
        "the harness's own write never ambers its own model"
    );

    // L2: an already-true verdict is never erased: neither by the message nor
    // by the unchanged rescan that follows it (which would go false had the
    // capture been moved under an earned amber).
    std::fs::write(
        &yaml,
        // The `cpu_offload` flag key (phase 4): a pure cost edit is now
        // re-booked without a reload; this leg needs a real amber, earned
        // by a config move the running engine never read.
        "engine: mock\ncost: fixed\nmeasured_gb: 120.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("operator edit (size moves)");
    rescan(&mut state, &yaml, "m");
    assert_eq!(state.snapshot().slots[0].stale_config, Some(true));
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 120.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("fold write over an amber resident");
    let written = crate::model::fingerprint(&yaml).expect("stat");
    state.on_model_file_written("m", &yaml, written);
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(true),
        "a harness write must not clear a verdict the operator's edit earned"
    );
    rescan(&mut state, &yaml, "m");
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(true),
        "and must not re-baseline the capture under it: the unchanged rescan would \
             go false if it had"
    );

    // L3: a resident with no capture stays unaccused through the same message.
    if let Some(occupant) = state.slots[0].occupant.as_mut() {
        occupant.spawn_config = None;
        occupant.stale_config = false;
    }
    state.on_model_file_written("m", &yaml, written);
    rescan(&mut state, &yaml, "m");
    assert_eq!(
        state.snapshot().slots[0].stale_config,
        Some(false),
        "absence of a capture is absence of evidence, never an accusation"
    );
}

// ---------------------------------------------------------------------
// Phase 4 (`TASK-ENGINE-MEASUREMENT`): the booking policy that removes
// the 2026-11-04 manual-reload trap. Two seams: a process measurement
// that folds re-books its slot occupant, and a rescan that moved ONLY
// cost figures re-books (or invalidates) the resident without a reload.
// ---------------------------------------------------------------------

/// Phase-4 seam (`on_process_measurement`): the fold moves the row's max
/// truth, and the slot booking must follow in the same turn — otherwise
/// the release credit and the R3 no-release floor guard keep billing the
/// paper figure (the Strata 55 / 70.4 incident). Falsifiers: dropping the
/// `rebook_after_measurement` call (the first assert stays at the paper
/// 55), and booking the raw measurement instead of the ONE decision
/// (the second fold to 30.0 must floor back UP to the declared 55, not
/// book 30).
#[tokio::test]
async fn a_folded_measurement_rebooks_the_slots_resident_to_the_max_truth() {
    let dir = crate::testutil::temp_dir("fold-rebook");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("rb.yaml");
    std::fs::write(&yaml, "engine: strata\ncost: fixed\nvram_gb: 55\n").expect("seed");
    let mut state = fold_state(&dir);
    let resident = rescan_entry(&yaml, "rb");
    let ticket = fold_ticket(19_040);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 19_040);
    // The incident's shape: the booking rode the paper row.
    state.slots[0].occupant.as_mut().expect("resident").need = Gibibytes(55.0);

    state.process_measurement(
        "rb",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 67.8,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(67.8),
        "the engine's own word above the declared floor re-books the slot"
    );
    // The two persist writes race for the disk: land the first before the
    // second fold, so the final file provably carries the later reading.
    await_yaml_keys(&yaml, &["measured_gb: 67.8"]).await;

    state.process_measurement(
        "rb",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 30.0,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(55.0),
        "the re-book is the ONE decision, not the raw figure: a footprint under \
             the declared floor floors back at the operator's number"
    );
    await_yaml_keys(&yaml, &["measured_gb: 30.0"]).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A scripted status answer for the elastic-refresh pin: one body per poll
/// (`None` = silence), and silence forever once the sequence is spent.
struct SequencedStatusRuntime {
    answers: std::sync::Mutex<std::collections::VecDeque<Option<serde_json::Value>>>,
    children: Vec<u32>,
}

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for SequencedStatusRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the elastic refresh pin must never load");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        panic!("the elastic refresh pin must never load");
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        panic!("the elastic refresh pin must never load");
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the elastic refresh pin must never load");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the elastic refresh pin must never load");
    }
    async fn engine_status_json(&self, _port: u16, _path: &str) -> Option<serde_json::Value> {
        let mut answers = self.answers.lock().expect("lock");
        answers.pop_front().flatten()
    }
    async fn descendant_pids(&self, _pid: u32) -> Vec<u32> {
        self.children.clone()
    }
}

/// A Strata-shaped `/metrics` engine block booking the component sum
/// `expert_cache_mib + conversation_cache_mib + arena_mib`
/// (`measurement_from_metrics`; the conversation and arena components
/// are zeroed so the fixture figure follows the expert cache alone
/// (the captured body's own `arena_mib` is zero too).
fn strata_vram_status(expert_cache_mib: u64) -> serde_json::Value {
    serde_json::json!({
        "engine": {
            "expert_cache_mib": expert_cache_mib,
            "conversation_cache_mib": 0,
            "arena_mib": 0,
        }
    })
}

/// Phase-4 elastic refresh (`ActorState::elastic_refresh`), the asymmetric
/// band: starting booked at 62.0 on the scripted 72 GiB card, the polls
/// answer equal (62.0 — nothing moves), rise (62.5 — re-books at once,
/// under-booking is over-commitment), small fall (62.0 — inside the
/// 1 GiB band, the booking stays), big fall (60.0 — 2.5 GiB past the
/// band, the credit moves down). After the sequence, silence: absence is
/// never a guess, and nothing further ever books. Falsifiers: a symmetric
/// band (the rise skipped), no band (the small fall lands), and consuming
/// the sampler on the first fold (no message after tick one).
#[tokio::test]
async fn the_elastic_refresh_rebooks_rises_at_once_and_falls_only_past_the_band() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let runtime = Arc::new(SequencedStatusRuntime {
        answers: std::sync::Mutex::new(
            vec![
                Some(strata_vram_status(63_488)), // 62.0: equal — silence
                Some(strata_vram_status(64_000)), // 62.5: rise — re-books
                Some(strata_vram_status(63_488)), // 62.0: 0.5 fall — band
                Some(strata_vram_status(61_440)), // 60.0: 2.5 fall — lands
            ]
            .into(),
        ),
        children: Vec::new(),
    }) as Arc<dyn crate::engine::EngineRuntime>;
    let witness = Arc::new(scripted_sample(Vec::new())) as Arc<dyn crate::budget::VramWitness>;
    let ctx = sampler_ctx("elastic", "strata", fold_ticket(19_041));
    let sampler = tokio::spawn(async move {
        ActorState::elastic_refresh(
            &runtime,
            &tx,
            &ctx,
            &witness,
            62.0,
            Duration::from_millis(40),
        )
        .await;
    });
    async fn next_figure(rx: &mut mpsc::UnboundedReceiver<ActorMsg>) -> f64 {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("the expected figure reaches the actor")
            .expect("actor channel open");
        let ActorMsg::MeasuredProcessReport { measurement, .. } = msg else {
            panic!("expected a process-measurement re-book");
        };
        measurement.gpu_gib
    }
    let rise = next_figure(&mut rx).await;
    assert!(
        (rise - 62.5).abs() < 1e-6,
        "the rise re-books at once, band only ever guards down: {rise}"
    );
    let fall = next_figure(&mut rx).await;
    assert!(
        (fall - 60.0).abs() < 1e-6,
        "the small fall stays inside the band; only the 2.5 GiB fall lands: {fall}"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(250), rx.recv())
            .await
            .is_err(),
        "after the sequence, silence re-books nothing (and the loop keeps polling)"
    );
    sampler.abort();
}

/// Review F-EM-403: a resident the TREE landed (the status endpoint silent
/// at readiness — the 2026-11-04 evidence line records exactly this Strata
/// serve lag) stays open to the engine's own word: the refresh loop polls
/// the endpoint every tick regardless of what landed first, and when it
/// answers, that answer is the truth — above the booked tree figure it
/// re-books at once. The landing message stays `ProcessTreeNvml`; the
/// upgrade carries `EngineStatus`. Falsifier: ending the sampler after a
/// tree landing (the upgrade never arrives).
#[tokio::test]
async fn a_tree_landed_resident_upgrades_to_the_engines_own_status_word() {
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let runtime = Arc::new(SequencedStatusRuntime {
        answers: std::sync::Mutex::new(
            // pop 1: the landing — silent, so the tree sum books;
            // pop 2: the first refresh tick — the endpoint came up with
            // 34.0 GiB above the booked 30.0: the rise re-books at once.
            vec![None, Some(strata_vram_status(34_816))].into(),
        ),
        children: vec![4243],
    }) as Arc<dyn crate::engine::EngineRuntime>;
    let witness = Arc::new(scripted_sample(vec![
        (4242, 20.0),
        (4243, 10.0),
        (9999, 5.0),
    ])) as Arc<dyn crate::budget::VramWitness>;
    let ticket = SpawnTicket {
        generation: 7,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 19_042,
    };
    let ctx = sampler_ctx("treeup", "strata", ticket.clone());
    assert_eq!(
        ActorState::report_measurement(&runtime, &tx, &ctx, &witness).await,
        Some(30.0),
        "the status endpoint is silent at readiness: the engine plus all its \
             descendants land the booking, and the sampler is not consumed"
    );
    let sampler = tokio::spawn(async move {
        ActorState::elastic_refresh(
            &runtime,
            &tx,
            &ctx,
            &witness,
            30.0,
            Duration::from_millis(40),
        )
        .await;
    });
    async fn next_report(
        rx: &mut mpsc::UnboundedReceiver<ActorMsg>,
    ) -> crate::engine::EngineMeasurement {
        let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("a measurement reaches the actor")
            .expect("actor channel open");
        let ActorMsg::MeasuredProcessReport { measurement, .. } = msg else {
            panic!("expected a process-measurement report");
        };
        measurement
    }
    let landing = next_report(&mut rx).await;
    assert_eq!(
        (landing.gpu_gib, landing.source, landing.confidence),
        (
            30.0,
            crate::engine::MeasurementSource::ProcessTreeNvml,
            crate::engine::MeasurementConfidence::Low,
        ),
        "the landing is the tree sum: a footprint read, not the engine's word"
    );
    let upgrade = next_report(&mut rx).await;
    assert!(
        (upgrade.gpu_gib - 34.0).abs() < 1e-6
            && upgrade.source == crate::engine::MeasurementSource::EngineStatus
            && upgrade.confidence == crate::engine::MeasurementConfidence::High,
        "the first live status answer supersedes the stand-in reading: {upgrade:?}"
    );
    sampler.abort();
}

/// Review F-EM-401, amber half: a resident the rescan marked `stale_config`
/// keeps its booking through a fold. The fold still records what the
/// process actually holds (the measured pair persists — honest data, the
/// same ruling as accepted F-EM-402), but a row that no longer describes
/// the process may not drive the booking: only unload + reload re-applies,
/// exactly the posture `rebook_cost_only_moves` declares. Falsifier: no
/// `stale_config` leg in `rebook_after_measurement` (the 67.8 walks onto
/// the amber resident's slot).
#[tokio::test]
async fn an_amber_residents_fold_records_the_pair_but_never_rebooks() {
    let dir = crate::testutil::temp_dir("fold-rebook-amber");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("amb.yaml");
    std::fs::write(&yaml, "engine: strata\ncost: fixed\nvram_gb: 55\n").expect("seed");
    let mut state = fold_state(&dir);
    let resident = rescan_entry(&yaml, "amb");
    let ticket = fold_ticket(19_043);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 19_043);
    if let Some(occupant) = state.slots[0].occupant.as_mut() {
        occupant.need = Gibibytes(55.0);
        occupant.stale_config = true;
    }

    state.process_measurement(
        "amb",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 67.8,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    let entry = &state.catalog[0];
    assert_eq!(
        (entry.measured_gb, entry.measured_attribution),
        (
            Some(67.8),
            Some(crate::model::MeasuredAttribution::EngineStatus)
        ),
        "the fold still records what the process holds"
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(55.0),
        "but an amber resident's booking stands: the apply gesture is unload + reload"
    );
    await_yaml_keys(&yaml, &["measured_gb: 67.8"]).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review F-EM-401, greedy half: the refusal reads the SPAWN-TIME cost
/// class the job carries, not the possibly-edited row — an engine launched
/// greedy keeps its card-driven clamped booking whatever the yaml says
/// now (the rescan seam refuses the same resident from its own side).
/// Falsifier: reading only `entry.cost` (the flipped `fixed` row would
/// book the 67.8 onto the greedy launch).
#[tokio::test]
async fn a_greedy_launched_residents_fold_never_rebooks_even_on_a_fixed_row() {
    let dir = crate::testutil::temp_dir("fold-rebook-greedy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("gdy.yaml");
    std::fs::write(&yaml, "engine: strata\ncost: greedy\nvram_gb: 40\n").expect("seed");
    let mut state = fold_state(&dir);
    let resident = rescan_entry(&yaml, "gdy");
    let ticket = fold_ticket(19_044);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 19_044);
    // The card-driven clamp the greedy admit left behind.
    state.slots[0].occupant.as_mut().expect("resident").need = Gibibytes(40.0);
    // The row reads `fixed` now (an operator flipped the class; the rescan
    // seam has its own greedy legs) — the JOB still carries the spawn-era
    // class, and that is what refuses.
    state.catalog[0].cost = crate::model::CostClass::Fixed;

    state.process_measurement(
        "gdy",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 67.8,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(40.0),
        "a greedy launch is never re-booked by a measurement, whatever the row says now"
    );
    await_yaml_keys(&yaml, &["measured_gb: 67.8"]).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// A cost-only rescan (phase 4, `rebook_cost_only_moves`): the loaded
/// resident's booking follows the new bytes WITHOUT a reload, the capture
/// rides with it so the marker round stays green, and the next unchanged
/// rescan confirms the re-baseline. Falsifiers: no re-book (need stays at
/// the fixture's 4.0), no re-baseline (the unchanged rescan ambers), and
/// laundering a flag move (see the sibling pin below).
#[test]
fn a_cost_only_edit_rebooks_the_loaded_resident_without_a_reload() {
    let (mut state, yaml, _) = resident_on_a_file();
    // The fingerprint is (mtime, size): the seed and the edit below have the
    // same byte length, so a same-tick write could leave the stamp unchanged
    // and the rebook would (correctly) refuse a "moved stamp, same bytes".
    // Let the clock tick so the edit is a real byte move on every machine.
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 12.0\nmeasured_gb_min: 8.0\n",
    )
    .expect("operator cost edit");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].need_gib,
        Some(12.0),
        "the moved cost figure re-books the resident's release credit"
    );
    assert_eq!(
        after.slots[0].stale_config,
        Some(false),
        "a booking the new bytes justify is applied, not accused"
    );
    rescan(&mut state, &yaml, "m");
    let again = state.snapshot();
    assert_eq!(again.slots[0].stale_config, Some(false));
    assert_eq!(again.slots[0].need_gib, Some(12.0));
}

/// The boundary stays a boundary: an edit that moves a flag (here together
/// with a cost figure) is a config change the running engine never read —
/// it ambers exactly as before and re-books NOTHING, even though the cost
/// figure alone would have been adopted. Falsifier: classifying any
/// byte-move as cost-only (this goes green with an unearned 99.0).
#[test]
fn a_flag_edit_still_ambers_the_resident_and_never_rebooks_it() {
    let (mut state, yaml, _) = resident_on_a_file();
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 99.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("operator edit moving a flag alongside the cost figure");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].stale_config,
        Some(true),
        "the launch no longer matches the row: the amber gesture (unload + reload) stands"
    );
    assert_eq!(
        after.slots[0].need_gib,
        Some(4.0),
        "a resident whose engine never read the bytes never re-books on them"
    );
}

/// Honesty control for the re-book classifier: bytes moved with NO cost
/// field actually different (a comment note) is not a cost move — the
/// marker round's conservatism keeps the amber and the old booking.
#[test]
fn a_comment_only_edit_is_not_a_cost_move() {
    let (mut state, yaml, _) = resident_on_a_file();
    let original = std::fs::read_to_string(&yaml).expect("read");
    std::fs::write(&yaml, format!("{original}# operator note\n")).expect("comment edit");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(after.slots[0].stale_config, Some(true));
    assert_eq!(
        after.slots[0].need_gib,
        Some(4.0),
        "nothing moved that a booking could follow"
    );
}

/// A cost move that leaves the row unable to answer the gate (an `exact`
/// stripped of every size) is INVALIDATED, never silently kept: the
/// marker round ambers it (the acceptance criterion's other half — update
/// the booking or invalidate it, never keep a number the bytes deny).
#[test]
fn a_cost_move_to_an_unbookable_row_invalidates_instead_of_rebooking() {
    let (mut state, yaml, _) = resident_on_a_file();
    std::fs::write(&yaml, "engine: mock\ncost: exact\n").expect("strip every size");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].stale_config,
        Some(true),
        "the row no longer justifies any booking: unload + reload is the gesture"
    );
    assert_eq!(after.slots[0].need_gib, Some(4.0));
}

/// An amber the operator's flag edit earned is never laundered by a later
/// cost edit: the resident whose launch no longer matches its row may not
/// re-book against bytes its engine never read. Falsifier: dropping the
/// `stale_config` guard in `rebook_cost_only_moves` (the cost edit below
/// would turn the resident green and re-booked).
#[test]
fn an_earned_amber_survives_a_later_cost_only_edit() {
    let (mut state, yaml, _) = resident_on_a_file();
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 10.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("flag edit earns the amber");
    rescan(&mut state, &yaml, "m");
    assert_eq!(state.snapshot().slots[0].stale_config, Some(true));
    std::fs::write(
        &yaml,
        "engine: mock\ncost: fixed\nmeasured_gb: 99.0\nmeasured_gb_min: 8.0\ncpu_offload: true\n",
    )
    .expect("later cost-only edit over the amber");
    rescan(&mut state, &yaml, "m");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].stale_config,
        Some(true),
        "the amber stays until the unload + reload applies it"
    );
    assert_eq!(after.slots[0].need_gib, Some(4.0));
}

/// The DESIGN-5 ledger-boundary pin (review W1-02): a cost-only rescan of a
/// live CPU-resident row must not move it between the ledgers. Adding
/// `vram_gb` to the loaded row is a pure COST_KEYS move, so the re-book
/// classifier fires — and must find nothing to re-book: the VRAM ladder's
/// figure may never be written into a RAM resident's `need`, where the
/// same-slot release credit would bill a GPU replacement GiB the CPU engine
/// never held (W1-01, the stranger's-paper-number class). The bytes are
/// still adopted (a cost edit neither re-books nor ambers a healthy
/// resident), and a later flag edit still earns the amber.
/// Falsifier: with the guard dropped, `booking_need` answers the VRAM ladder
/// for the RAM row and the snapshot shows `need_gib` 12.0 — a booking into
/// the card ledger — and the phantom credit follows into the next
/// replacement.
#[test]
fn a_cost_only_edit_never_moves_a_ram_resident_into_the_vram_ledger() {
    let dir = crate::testutil::temp_dir("rebook-ram-resident");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("dir");
    let yaml = dir.join("cpu.yaml");
    std::fs::write(&yaml, "engine: llama\ncpu_offload: true\nram_gb: 10\n").expect("seed");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.clone(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let capture = crate::model::fingerprint(&yaml).expect("seeded file fingerprints");
    let ticket = SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 4242,
        port: 18_431,
    };
    state.catalog = vec![entry_from(&yaml, "cpu")];
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_431,
        occupant: Some(Occupant {
            model: "cpu".to_string(),
            need: Gibibytes(0.0),
            ram_need: Some(Gibibytes(10.0)),
            port: 18_431,
            ticket,
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: Some((yaml.clone(), capture)),
            stale_config: false,
            serve_id: None,
            measure: None,
        }),
        job: None,
    }];
    // The fingerprint is (mtime, size): let the clock tick so the edit is a
    // real byte move on every machine (the GPU re-book pin's posture).
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(
        &yaml,
        "engine: llama\ncpu_offload: true\nram_gb: 10\nvram_gb: 12\n",
    )
    .expect("operator cost edit");
    rescan(&mut state, &yaml, "cpu");
    let after = state.snapshot();
    assert_eq!(
        state.catalog[0].resource(),
        crate::model::Resource::Ram,
        "a cost edit cannot re-tag the row's resource"
    );
    assert_eq!(
        after.slots[0].need_gib,
        Some(0.0),
        "the VRAM ladder must not book a RAM resident (a pre-fix rebook shows 12.0 here)"
    );
    assert_eq!(
        after.slots[0].stale_config,
        Some(false),
        "a cost move the resident cannot see is adopted, not accused"
    );
    let occupant = state.slots[0].occupant.as_ref().expect("resident");
    assert_eq!(occupant.need, Gibibytes(0.0));
    assert_eq!(
        occupant.ram_need,
        Some(Gibibytes(10.0)),
        "the RAM booking stands: only a reload moves it (wave 2 owns the measured rung)"
    );
    assert_eq!(
        state.ledger_sum(),
        Gibibytes(0.0),
        "the card ledger gained nothing"
    );
    assert_eq!(
        state.ram_ledger_sum(),
        Gibibytes(10.0),
        "the RAM ledger lost nothing"
    );
    // The amber half: a later flag edit still earns the amber — adopting
    // cost bytes launders nothing.
    std::thread::sleep(std::time::Duration::from_millis(50));
    std::fs::write(
        &yaml,
        "engine: llama\ncpu_offload: false\nram_gb: 10\nvram_gb: 12\n",
    )
    .expect("operator flag edit");
    rescan(&mut state, &yaml, "cpu");
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].stale_config,
        Some(true),
        "the launch no longer matches the row: the unload + reload gesture stands"
    );
    assert_eq!(
        after.slots[0].need_gib,
        Some(0.0),
        "and still nothing re-books"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A runtime the readmission pins can safely touch: the kill and the
/// release proof answer immediately (the pins judge only the admit
/// decision and never drive the swap to the end), while everything the
/// pins must never reach keeps the never-called posture.
struct ReadmitRuntime;

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for ReadmitRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        Ok(crate::engine::Spawned {
            port: 18_999,
            pid: 0,
        })
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        Ok(())
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        Ok(true)
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the readmission pin never reaches readiness");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the readmission pin never reaches readiness");
    }
    async fn engine_status_json(&self, _port: u16, _path: &str) -> Option<serde_json::Value> {
        None
    }
    async fn descendant_pids(&self, _pid: u32) -> Vec<u32> {
        Vec::new()
    }
}

/// The 2026-11-04 readmission shape, parameterised: one WORK slot holding
/// a resident booked at `paper_gib` while the scripted 72 GiB card
/// physically carries `held_gib` (under-booked resident, the incident),
/// and no other occupancy — the same-slot replacement is the only release
/// the gate can ever count.
fn readmit_state(held_gib: f64) -> (ActorState, mpsc::UnboundedReceiver<ActorMsg>) {
    let (tx, rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    for engine in ["mock", "basalt", "strata"] {
        adapters.insert(
            engine.to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
    }
    let witness = Arc::new(ScriptedWitness(crate::budget::MemorySample {
        total_gib: 72.0,
        used_gib: held_gib,
        processes: Vec::new(),
        attribution: crate::budget::attribution_from_processes(&[]),
    })) as Arc<dyn crate::budget::VramWitness>;
    let state = ActorState::empty(
        crate::config::AppConfig::default(),
        crate::testutil::temp_dir("readmit"),
        adapters,
        Arc::new(ReadmitRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        witness,
        tx,
    );
    (state, rx)
}

fn incident_sample(used_gib: f64) -> crate::budget::MemorySample {
    crate::budget::MemorySample {
        total_gib: 72.0,
        used_gib,
        processes: Vec::new(),
        attribution: crate::budget::attribution_from_processes(&[]),
    }
}

/// Arms `entry` on slot 0 and hands the gate the card reading — the two
/// synchronous halves of the load path (`start_queued_load` also samples a
/// scripted witness on a task; its message stays unread in the pin's rx).
fn admit_round(state: &mut ActorState, entry: &ModelEntry, used_gib: f64) -> super::JobId {
    let id = state.register_queued_load(0, entry, Vec::new());
    state.start_queued_load(id, 0, entry.clone());
    state.finish_admit_card(id, Ok(incident_sample(used_gib)));
    id
}

/// T1 (the incident, Strata→Basalt): while the release credit books the
/// row's paper 55 and the card physically holds the resident's 70.4, the
/// 67.4 replacement with a 65 GiB launch floor must be REFUSED with the
/// incident's own verdict — no release the harness may perform reaches the
/// floor (54.6 credited free vs a 63 floor after tolerance). Once the
/// engine's own status figure has folded (phase-4 seam), the SAME load on
/// the SAME card names the resident as its victim and arms. The red leg is
/// the falsifier for the whole feature: a booking policy that ignores the
/// engine's word keeps this refusal forever.
#[tokio::test]
async fn a_strata_resident_releases_for_basalt_only_once_its_measured_footprint_is_booked() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("readmit-sb");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let resident_yaml = dir.join("strata-v1.yaml");
    std::fs::write(&resident_yaml, "engine: strata\ncost: fixed\nvram_gb: 55\n").expect("seed");
    let incoming_yaml = dir.join("basalt-v2.yaml");
    std::fs::write(
        &incoming_yaml,
        "engine: basalt\ncost: fixed\nvram_gb: 67.4\nlaunch_only_with_at_least_vram_free: 65\n",
    )
    .expect("seed");
    let (mut state, _rx) = readmit_state(70.4);
    let resident = rescan_entry(&resident_yaml, "strata-v1");
    let ticket = fold_ticket(18_109);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 18_109);
    state.slots[0].occupant.as_mut().expect("resident").need = Gibibytes(55.0);
    let incoming = rescan_entry(&incoming_yaml, "basalt-v2");
    // The gate preflights the launch before it evicts, so the replacement's
    // row must be in the catalog like it would be on a serving box.
    state.catalog.push(incoming.clone());

    let red = admit_round(&mut state, &incoming, 70.4);
    let job = &state.jobs[&red];
    assert!(job.terminal, "the paper-credit gate must refuse, not arm");
    assert!(
        matches!(
            job.error,
            Some(SchedulerError::Admission(
                AdmissionError::BelowLaunchFloor {
                    no_releases_reach: true,
                    ..
                }
            ))
        ),
        "the incident's verdict: {:?}",
        job.error
    );

    state.process_measurement(
        "strata-v1",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 70.4,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(70.4),
        "the fold re-books the slot before any replacement is judged"
    );

    let green = admit_round(&mut state, &incoming, 70.4);
    let job = &state.jobs[&green];
    assert!(
        !job.terminal && job.error.is_none(),
        "the footprint credit covers the need and the floor: {:?}",
        job.error
    );
    assert_eq!(
        job.victim.as_ref().expect("victim").model,
        "strata-v1",
        "the release-first swap names the resident it is freeing"
    );
    await_yaml_keys(&resident_yaml, &["measured_gb: 70.4"]).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// T2 (the mirror, Basalt→Strata): paper 60 held 67.4, the incoming Strata
/// row needs 70.0 with the same 65 floor — red the need deficit (7.4) is
/// even LARGER than the floor deficit (2.4), and the guard still shapes
/// the refusal as the floor verdict, exactly as the 2026-11-01 ruling
/// demands. After the fold (67.4 credited, free 70.0) it admits.
#[tokio::test]
async fn a_basalt_resident_releases_for_strata_only_once_its_measured_footprint_is_booked() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("readmit-bs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let resident_yaml = dir.join("basalt-v1.yaml");
    std::fs::write(&resident_yaml, "engine: basalt\ncost: fixed\nvram_gb: 60\n").expect("seed");
    let incoming_yaml = dir.join("strata-v2.yaml");
    std::fs::write(
        &incoming_yaml,
        "engine: strata\ncost: fixed\nvram_gb: 70.0\nlaunch_only_with_at_least_vram_free: 65\n",
    )
    .expect("seed");
    let (mut state, _rx) = readmit_state(67.4);
    let resident = rescan_entry(&resident_yaml, "basalt-v1");
    let ticket = fold_ticket(18_110);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 18_110);
    state.slots[0].occupant.as_mut().expect("resident").need = Gibibytes(60.0);
    let incoming = rescan_entry(&incoming_yaml, "strata-v2");
    // The gate preflights the launch before it evicts: the row rides.
    state.catalog.push(incoming.clone());

    let red = admit_round(&mut state, &incoming, 67.4);
    assert!(
        matches!(
            state.jobs[&red].error,
            Some(SchedulerError::Admission(
                AdmissionError::BelowLaunchFloor {
                    no_releases_reach: true,
                    ..
                }
            ))
        ),
        "the incident's verdict: {:?}",
        state.jobs[&red].error
    );

    state.process_measurement(
        "basalt-v1",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 67.4,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(state.snapshot().slots[0].need_gib, Some(67.4));

    let green = admit_round(&mut state, &incoming, 67.4);
    let job = &state.jobs[&green];
    assert!(
        !job.terminal && job.error.is_none(),
        "free 70.0 books the 70.0 need and clears the floor: {:?}",
        job.error
    );
    await_yaml_keys(&resident_yaml, &["measured_gb: 67.4"]).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// T3 (the row-cost half, Strata side): no measurement at all — the
/// operator moves the row's declared cost to the footprint the engine was
/// seen holding (55 -> 70.4) and reloads; the rescan re-books the loaded
/// resident green, and the replacement admits against the SAME card that
/// refused it minutes before. This is the trap's manual gesture made
/// automatic: no unload, no reload of the resident.
#[tokio::test]
async fn a_rebooked_row_lets_the_basalt_replacement_in_without_touching_the_resident() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("readmit-row-sb");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let resident_yaml = dir.join("strata-v1.yaml");
    std::fs::write(&resident_yaml, "engine: strata\ncost: fixed\nvram_gb: 55\n").expect("seed");
    let incoming_yaml = dir.join("basalt-v2.yaml");
    std::fs::write(
        &incoming_yaml,
        "engine: basalt\ncost: fixed\nvram_gb: 67.4\nlaunch_only_with_at_least_vram_free: 65\n",
    )
    .expect("seed");
    let (mut state, _rx) = readmit_state(70.4);
    let resident = rescan_entry(&resident_yaml, "strata-v1");
    let ticket = fold_ticket(18_111);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 18_111);
    let capture = crate::model::fingerprint(&resident_yaml).expect("stat");
    if let Some(occupant) = state.slots[0].occupant.as_mut() {
        occupant.need = Gibibytes(55.0);
        // The launch came from this yaml: install captures it, and only a
        // captured resident can have its bytes re-judged by a rescan.
        occupant.spawn_config = Some((resident_yaml.clone(), capture));
    }
    let incoming = rescan_entry(&incoming_yaml, "basalt-v2");
    // The gate preflights the launch before it evicts: the row rides.
    state.catalog.push(incoming.clone());

    let red = admit_round(&mut state, &incoming, 70.4);
    assert!(
        matches!(
            state.jobs[&red].error,
            Some(SchedulerError::Admission(
                AdmissionError::BelowLaunchFloor {
                    no_releases_reach: true,
                    ..
                }
            ))
        ),
        "the red control before the row moved: {:?}",
        state.jobs[&red].error
    );

    std::fs::write(
        &resident_yaml,
        "engine: strata\ncost: fixed\nvram_gb: 70.4\n",
    )
    .expect("operator moves the declared cost to the footprint");
    rescan(&mut state, &resident_yaml, "strata-v1");
    // `rescan` lands a one-row catalog; the replacement's row rides again.
    state.catalog.push(incoming.clone());
    let after = state.snapshot();
    assert_eq!(
        after.slots[0].need_gib,
        Some(70.4),
        "the rescan re-books the loaded resident's credit from the new row"
    );
    assert_eq!(
        after.slots[0].stale_config,
        Some(false),
        "a cost-only move is applied, not accused: no reload trap"
    );

    let green = admit_round(&mut state, &incoming, 70.4);
    let job = &state.jobs[&green];
    assert!(
        !job.terminal && job.error.is_none(),
        "the same card that refused now releases: {:?}",
        job.error
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T4 (the row-cost half, Basalt side, mirror of T3): declared 60 -> 67.4
/// re-books the resident; the 70.0 Strata replacement that the stale row
/// refused (free 62.6 under a 63 floor-after-tolerance) admits against
/// free 70.0.
#[tokio::test]
async fn a_rebooked_row_lets_the_strata_replacement_in_without_touching_the_resident() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("readmit-row-bs");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let resident_yaml = dir.join("basalt-v1.yaml");
    std::fs::write(&resident_yaml, "engine: basalt\ncost: fixed\nvram_gb: 60\n").expect("seed");
    let incoming_yaml = dir.join("strata-v2.yaml");
    std::fs::write(
        &incoming_yaml,
        "engine: strata\ncost: fixed\nvram_gb: 70.0\nlaunch_only_with_at_least_vram_free: 65\n",
    )
    .expect("seed");
    let (mut state, _rx) = readmit_state(67.4);
    let resident = rescan_entry(&resident_yaml, "basalt-v1");
    let ticket = fold_ticket(18_112);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 18_112);
    let capture = crate::model::fingerprint(&resident_yaml).expect("stat");
    if let Some(occupant) = state.slots[0].occupant.as_mut() {
        occupant.need = Gibibytes(60.0);
        // The launch came from this yaml: install captures it, and only a
        // captured resident can have its bytes re-judged by a rescan.
        occupant.spawn_config = Some((resident_yaml.clone(), capture));
    }
    let incoming = rescan_entry(&incoming_yaml, "strata-v2");
    // The gate preflights the launch before it evicts: the row rides.
    state.catalog.push(incoming.clone());

    let red = admit_round(&mut state, &incoming, 67.4);
    assert!(
        matches!(
            state.jobs[&red].error,
            Some(SchedulerError::Admission(
                AdmissionError::BelowLaunchFloor {
                    no_releases_reach: true,
                    ..
                }
            ))
        ),
        "the red control before the row moved: {:?}",
        state.jobs[&red].error
    );

    std::fs::write(
        &resident_yaml,
        "engine: basalt\ncost: fixed\nvram_gb: 67.4\n",
    )
    .expect("operator moves the declared cost to the footprint");
    rescan(&mut state, &resident_yaml, "basalt-v1");
    // `rescan` lands a one-row catalog; the replacement's row rides again.
    state.catalog.push(incoming.clone());
    let after = state.snapshot();
    assert_eq!(after.slots[0].need_gib, Some(67.4));
    assert_eq!(after.slots[0].stale_config, Some(false));

    let green = admit_round(&mut state, &incoming, 67.4);
    let job = &state.jobs[&green];
    assert!(
        !job.terminal && job.error.is_none(),
        "free 70.0 books the 70.0 need and clears the floor: {:?}",
        job.error
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// T5 (the greedy resident, `TASK-BASALT-GREEDY` phase 4): a `cost: greedy`
/// Basalt occupant holds the card (70.4) while its booking stays the room
/// it grabbed at admit (50) - a measured pair may never re-book a greedy
/// row (DOCTRINE 12), so this stale-looking credit is the honest one. A
/// fixed Strata replacement with a 65 floor is judged against the booked
/// room: red while free+credit misses the floor-after-tolerance (49.6 vs
/// 63) with the R3 verdict `no_releases_reach`, exactly as a fixed
/// resident would take; a 70.4 fold changes nothing on the greedy slot;
/// and once the card reading lets the booked room alone carry the floor
/// (free 30 without the victim credit, 80 with it) the replacement admits
/// and names the greedy resident as the victim it frees. Falsifier: a
/// greedy fold that re-books the slot, or a floor guard that stops
/// crediting the same-slot victim, flips the red or the green arm.
#[tokio::test]
async fn a_greedy_basalt_resident_credits_only_its_room_at_admit() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("readmit-greedy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let resident_yaml = dir.join("basalt-greedy.yaml");
    std::fs::write(
        &resident_yaml,
        "engine: basalt\ncost: greedy\nlaunch_only_with_at_least_vram_free: 60\n",
    )
    .expect("seed");
    let incoming_yaml = dir.join("strata-v2.yaml");
    std::fs::write(
        &incoming_yaml,
        "engine: strata\ncost: fixed\nvram_gb: 60\nlaunch_only_with_at_least_vram_free: 65\n",
    )
    .expect("seed");
    let (mut state, _rx) = readmit_state(70.4);
    let resident = rescan_entry(&resident_yaml, "basalt-greedy");
    let ticket = fold_ticket(18_113);
    state.catalog = vec![resident.clone()];
    seat_spawn(&mut state, &resident, &ticket, 18_113);
    // The greedy admit booked the room it grabbed (50), not the 70.4 the
    // card physically carries - the clamp `min(nominal, planned_room)`.
    state.slots[0].occupant.as_mut().expect("resident").need = Gibibytes(50.0);
    let incoming = rescan_entry(&incoming_yaml, "strata-v2");
    // The gate preflights the launch before it evicts: the row rides.
    state.catalog.push(incoming.clone());

    let red = admit_round(&mut state, &incoming, 70.4);
    assert!(
        matches!(
            state.jobs[&red].error,
            Some(SchedulerError::Admission(
                AdmissionError::BelowLaunchFloor {
                    no_releases_reach: true,
                    ..
                }
            ))
        ),
        "free 49.6 with the booked room credited misses the 63 floor: {:?}",
        state.jobs[&red].error
    );

    state.process_measurement(
        "basalt-greedy",
        &ticket,
        crate::engine::EngineMeasurement {
            gpu_gib: 70.4,
            ram_gib: None,
            source: crate::engine::MeasurementSource::EngineStatus,
            confidence: crate::engine::MeasurementConfidence::High,
        },
    );
    assert_eq!(
        state.snapshot().slots[0].need_gib,
        Some(50.0),
        "a measured pair never books or re-books a greedy row (DOCTRINE 12)"
    );

    // The same replacement on a card whose own occupancy released: free 30
    // never reaches the 65 floor, free 80 does - the greedy victim's booked
    // room alone is what admits the load.
    let green = admit_round(&mut state, &incoming, 40.0);
    let job = &state.jobs[&green];
    assert!(
        !job.terminal && job.error.is_none(),
        "the booked room credits the replacement over the floor: {:?}",
        job.error
    );
    assert_eq!(
        job.victim.as_ref().expect("victim").model,
        "basalt-greedy",
        "the replacement names the greedy resident it is freeing"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The hard invariant's STRUCTURAL tier (established `include_str!` seam, same
/// shape as the api's `swap_for_port` tripwire): the `snapshot` body — the one
/// status-path reader of the marker — may never reach for the filesystem or the
/// clock. The ban-list convicts SYNONYMS, not just the lazy spelling: bare
/// `fs::` catches `std::fs::` and `tokio::fs::` alike, `metadata(` catches
/// `std::fs::metadata`, `Path::metadata()` and the `MetadataExt` forms, and
/// `SystemTime::now` is banned because the stamp is stored at install
/// (`launched_at_wall`) for exactly that reason.
///
/// The two-layer contract, stated honestly: what the NET cannot see is a stat
/// whose result is DISCARDED — inert by definition, it cannot move a verdict —
/// and a stat parked behind a helper name outside the scan. What it convicts is
/// every spelling of a reaching fs/clock call inside the read path's own bodies.
/// The BEHAVIOURAL backstop (the hidden-file probe below, and the api-side
/// `/har/models` leg) catches anything whose result reaches the published
/// verdict regardless of spelling: a stat feeding the verdict is convicted by
/// BOTH tiers; a stat whose result is dropped is harmless to the verdict by
/// construction, and the net still keeps the synonym out of the hot path.
/// The tail sentinel asserts the slice actually covers the body's end (a
/// future early column-4 `}` cannot silently shrink what is scanned).
///
/// Falsifier: move the verdict computation into `snapshot` (any spelling) and
/// the net plus both probes fire; inject an inert `Path::new(f).metadata()` and
/// the net fires (FINDINGS-BATCH-2026-10-28P [P-1]'s M-E).
#[test]
fn the_snapshot_reads_the_cached_verdict_and_never_stats() {
    let source = include_str!("status.rs");
    let start = source
        .find("    pub(super) fn snapshot(&self) -> SchedulerStatus {")
        .expect("snapshot is still in this file");
    let body = &source[start
        ..start
            + source[start..]
                .find("\n    }\n")
                .expect("the function closes")];
    assert!(
        body.contains("loaded_at:"),
        "the scan slice must reach the body's tail (early-close fragility)"
    );
    for banned in [
        "fs::",
        "fingerprint(",
        "read_to_string(",
        "metadata(",
        "modified(",
        "try_exists",
        "File::",
        "FileTimes",
        "OpenOptions",
        "SystemTime::now",
        "read_dir",
    ] {
        assert!(
            !body.contains(banned),
            "the snapshot may only read cached state; found `{banned}` in its body"
        );
    }
    let (state, yaml, _) = resident_on_a_file();
    let before = state.snapshot();
    assert_eq!(before.slots[0].stale_config, Some(false));
    std::fs::remove_file(&yaml).expect("hide the file");
    let after = state.snapshot();
    assert_eq!(
        before.slots[0].stale_config, after.slots[0].stale_config,
        "hiding the file from under the status path changes nothing: the verdict \
             is a cache, not a stat (a statting snapshot would flip to Some(true))"
    );
    assert_eq!(before.slots[0].launched_at, after.slots[0].launched_at);
}

/// The production half of a source file: everything before its test module (if
/// no `mod tests` boundary exists, the whole file counts as production — the
/// safe direction).
fn production_half(source: &str) -> &str {
    let cut = [
        "#[cfg(test)]\nmod tests {",
        "#[cfg(test)]\npub(crate) mod tests {",
    ]
    .iter()
    .filter_map(|marker| source.find(marker))
    .min()
    .unwrap_or(source.len());
    &source[..cut]
}

/// FINDINGS-07 [8] structural net: the reload turn's drift rounds in
/// `catalog.rs` judge only pre-gathered evidence. `apply_catalog` (the single
/// writer's turn) and both rounds it runs contain no filesystem token at all —
/// every stat that moves a `stale_config` verdict or a queued-row retirement
/// is gathered off the actor by `gather_drift_stats`, on the reload task's
/// blocking legs, and travels in as [`DriftStats`].
///
/// Falsifier: restore the inline stat in `refresh_stale_markers` or
/// `invalidate_queued_loads` (any synonym spelling) and this is the test that
/// goes red first; the behavioural twins (supersede/amber after
/// `handle.reload()`) stay green either way because they pin semantics, not
/// threading — the same two-tier shape as the snapshot net above.
#[test]
fn the_drift_rounds_judge_only_pre_gathered_evidence() {
    let source = include_str!("catalog.rs");
    for (head, tail) in [
        ("fn apply_catalog(", "spawn_fit_warm"),
        ("fn invalidate_queued_loads(", "finish_queued_load"),
        ("fn refresh_stale_markers(", "occupant.stale_config = "),
    ] {
        let start = source
            .find(head)
            .unwrap_or_else(|| panic!("{head} is still in catalog.rs"));
        let close = source[start..]
            .find("\n    }\n")
            .unwrap_or_else(|| panic!("{head} closes at column 4"));
        let body = &source[start..start + close];
        assert!(
            body.contains(tail),
            "the scan slice must reach {head}'s tail (early-close fragility)"
        );
        for banned in [
            "fs::",
            "fingerprint(",
            "read_to_string(",
            "metadata(",
            "try_exists",
            "read_dir",
            ".exists()",
            "is_file(",
            "is_dir(",
            "File::open",
        ] {
            assert!(
                !body.contains(banned),
                "`{head}` may only judge the gathered `DriftStats`; found \
                     `{banned}` — the actor turn must never reach the filesystem \
                     (FINDINGS-07 [8])"
            );
        }
    }
}

/// P-4 (FINDINGS-BATCH-2026-10-28P): the re-baseline table documented at
/// [`ActorState::on_model_file_written`] claims a CLOSED set of legitimate model-
/// yaml mutators — the measured persist (`scheduler/measured.rs`, its note riding
/// the success path) and `har_config_model`'s flag edit (api, its note sent before the
/// handler's reload). "True today" was grep-only; this pin makes the claim
/// structural, the repo's established `include_str!` style: every scanned source
/// keeps exactly its allowed count of writeback call sites in its production
/// half, and each mutator file must also carry its note. A third caller must
/// register the write in the re-baseline table (send `ModelFileWritten` /
/// `note_model_file_written` with the write) or the amber blames the harness for
/// its own write — the exact lie interlock 5 forbids.
/// Falsifier: inject `let _ = crate::model::writeback::set_flags_on_file(
/// std::path::Path::new("x"), &[]);` into any scanned production body and this
/// is the sole failing test; add the call to api WITHOUT the note and the note
/// leg fires too.
#[test]
fn the_writeback_mutator_set_stays_closed_to_the_rebaseline_table() {
    for (file, source, allowed) in [
        ("scheduler/mod.rs", include_str!("mod.rs"), 0usize),
        ("scheduler/catalog.rs", include_str!("catalog.rs"), 0),
        ("scheduler/handle.rs", include_str!("handle.rs"), 0),
        ("scheduler/measured.rs", include_str!("measured.rs"), 1),
        ("scheduler/messages.rs", include_str!("messages.rs"), 0),
        ("api/mod.rs", include_str!("../api/mod.rs"), 1),
        ("api/guards.rs", include_str!("../api/guards.rs"), 0),
        (
            "api/status_render.rs",
            include_str!("../api/status_render.rs"),
            0,
        ),
        ("main.rs", include_str!("../main.rs"), 0),
        ("catalog.rs", include_str!("../catalog.rs"), 0),
        ("tray.rs", include_str!("../tray.rs"), 0),
        ("model/mod.rs", include_str!("../model/mod.rs"), 0),
        ("engine/mod.rs", include_str!("../engine/mod.rs"), 0),
    ] {
        let production = production_half(source);
        let calls = production.matches("set_flags_on_file(").count()
            + production.matches("set_flag_on_file(").count();
        assert_eq!(
            calls, allowed,
            "{file} carries {calls} writeback mutator call sites; the closed table \
                 allows {allowed} (the measured persist in scheduler/measured.rs, \
                 har_config_model's flag edit in api). A new writer must register in \
                 the re-baseline table — send the ModelFileWritten note with the write \
                 (see ActorState::on_model_file_written) — or the stale marker lies \
                 about the harness's own write: FINDINGS-BATCH-2026-10-28P [P-4]."
        );
        if allowed == 1 {
            let notes = if file.starts_with("api") {
                production.contains("note_model_file_written(")
            } else {
                production.contains("ActorMsg::ModelFileWritten")
            };
            assert!(
                notes,
                "{file} mutates a model yaml but never sends the re-baseline note: \
                     the write would self-amber its resident"
            );
        }
    }
}

#[test]
fn a_stale_reap_target_does_not_abort_the_rest() {
    // Chunk 2: aborting the reap on a stale victim ticket stranded the job's own live
    // engine — the very orphan the multi-target reap exists to prevent.
    use crate::engine::SpawnError;
    assert!(
        ActorState::reap_should_continue(&SpawnError::StaleTicket {
            port: 18_109,
            expected: 3,
            found: 4,
        }),
        "a vanished victim leaves nothing to reap; the next target must still be tried"
    );
    assert!(
        !ActorState::reap_should_continue(&SpawnError::KillFailed {
            holder: "model".to_string(),
            reason: "TerminateProcess failed".to_string(),
        }),
        "a real kill failure must still fail the job"
    );
}

// --- R2-02-02 option B: the one bounded late-release watch ---------------------

/// Whether the late-release watch has spent its bound. The boundary is the whole
/// contract (option B: one `release_timeout_s`, never a second unbounded task), so
/// it is pinned here rather than inferred from a wall-clock wait.
#[test]
fn the_late_release_watch_bounds_at_exactly_one_window() {
    let window = Duration::from_secs(2);
    assert!(
        !ActorState::late_release_expired(Duration::ZERO, window),
        "the watch starts its one window empty"
    );
    assert!(
        !ActorState::late_release_expired(window - RELEASE_POLL, window),
        "the watch may still poll inside its window, on the reap's own cadence"
    );
    assert!(
        ActorState::late_release_expired(window, window),
        "the watch ends the moment the window is spent: the bound is one window, not two"
    );
    assert!(ActorState::late_release_expired(
        window + Duration::from_secs(5),
        window
    ));
}

/// REV8-01: the watch is scoped to the uninstalled spawn. An occupant carrying this
/// very `(port, generation)` is what `/har/status` reports as serving, so the watch
/// stays out; any other occupant (or none) leaves it armed.
#[test]
fn an_installed_occupant_of_the_same_ticket_blinds_the_late_watch() {
    let orphan = SpawnTicket {
        generation: 41,
        kind: ResourceKind::ChildProcess,
        pid: 4_241,
        port: 19_441,
    };
    let mut installed_same = orphan.clone();
    let installed_other = SpawnTicket {
        generation: 42,
        kind: ResourceKind::ChildProcess,
        pid: 4_242,
        port: 19_441,
    };
    let mut other_port = orphan.clone();
    other_port.port = 19_442;

    assert!(
        ActorState::late_watch_names_an_orphan(None, &orphan),
        "no occupant at all is the R2-02-02 shape: nothing but the watch can name it"
    );
    assert!(
        !ActorState::late_watch_names_an_orphan(Some(&installed_same), &orphan),
        "the slot reports THIS ticket as serving; the watch may not call it gone"
    );
    installed_same.pid = 9_999;
    assert!(
        !ActorState::late_watch_names_an_orphan(Some(&installed_same), &orphan),
        "identity is the runtime table's (port, generation), not the pid"
    );
    assert!(
        ActorState::late_watch_names_an_orphan(Some(&installed_other), &orphan),
        "a newer generation in that slot is a different engine: the orphan still needs \
             its watch"
    );
    assert!(
        ActorState::late_watch_names_an_orphan(Some(&other_port), &orphan),
        "another port's occupant says nothing about this ticket"
    );
}

/// What the fake answers once the late window is open.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum LateAnswer {
    /// Death proven late: the runtime answers confirmed for this generation.
    Confirmed,
    /// Still no proof: the watch keeps polling until its own deadline.
    #[default]
    StillHeld,
    /// A newer spawn owns that port now: the ticket is stale.
    Superseded,
    /// REV8-03: the evidence check never returns — the hard outer timeout is the
    /// only thing that can end the watch.
    Wedged,
}

/// Which terminalizing arm a pin drives: the failure funnel's `Failed` or a
/// cancel's `CancelRequested` — the two arms R2-02-02 names.
#[derive(Clone, Copy)]
enum Arm {
    Failed,
    CancelRequested,
}

/// Test double for the R2-02-02 late-release watch. `kill` is recorded and answers
/// `Ok` (the terminate was issued); `release_confirmed` answers `false` while the
/// job's own reap window runs — the state that produces the unconfirmed terminal —
/// and then exactly what the test scripts for the late window. Every answer is
/// recorded with the ticket it named and whether the late window was already open,
/// so a pin can tell the reap's polls from the watch's own and name every
/// generation the watch went looking for. It never kills on its own, so a `kill`
/// count above the reap's single kill is a watch that re-killed.
#[derive(Default)]
struct LateWatchRuntime {
    state: Mutex<LateWatch>,
}

#[derive(Default)]
struct LateWatch {
    late_open: bool,
    late: LateAnswer,
    kills: usize,
    /// `(port, generation, asked after the late window opened, verdict)`.
    polls: Vec<(u16, u64, bool, &'static str)>,
}

impl LateWatchRuntime {
    fn new() -> Self {
        LateWatchRuntime::default()
    }

    /// Opens the late window. From here every `release_confirmed` belongs to the
    /// watch, because the reap task was dropped the moment its deadline expired —
    /// its event is what terminalized the job.
    fn open_late_window(&self, answer: LateAnswer) {
        let mut state = self.state.lock().expect("late watch lock");
        state.late_open = true;
        state.late = answer;
    }

    /// The watch's own polls, in order.
    fn late_polls(&self) -> Vec<(u16, u64, &'static str)> {
        self.state
            .lock()
            .expect("late watch lock")
            .polls
            .iter()
            .filter(|(_, _, late, _)| *late)
            .map(|(port, generation, _, verdict)| (*port, *generation, *verdict))
            .collect()
    }

    /// Every `release_confirmed` this runtime was ever asked about, late window or
    /// not: proves the reap window polled first and that no other generation was.
    fn poll_count(&self) -> usize {
        self.state.lock().expect("late watch lock").polls.len()
    }

    fn generations_polled(&self) -> Vec<u64> {
        self.state
            .lock()
            .expect("late watch lock")
            .polls
            .iter()
            .map(|(_, generation, _, _)| *generation)
            .collect()
    }

    fn kill_count(&self) -> usize {
        self.state.lock().expect("late watch lock").kills
    }
}

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for LateWatchRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the R2-02-02 late-release pin never spawns");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        self.state.lock().expect("late watch lock").kills += 1;
        Ok(())
    }
    async fn release_confirmed(
        &self,
        ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        let (answer, outcome) = {
            let mut state = self.state.lock().expect("late watch lock");
            let late_open = state.late_open;
            let answer = if late_open {
                state.late
            } else {
                LateAnswer::StillHeld
            };
            let (verdict, outcome) = match answer {
                LateAnswer::Confirmed => ("confirmed", Ok(true)),
                LateAnswer::StillHeld => ("pending", Ok(false)),
                LateAnswer::Superseded => (
                    "superseded",
                    Err(crate::engine::SpawnError::StaleTicket {
                        port: ticket.port,
                        expected: ticket.generation,
                        // The successor the table now carries for this port.
                        found: ticket.generation + 1,
                    }),
                ),
                LateAnswer::Wedged => ("wedged", Ok(false)),
            };
            state
                .polls
                .push((ticket.port, ticket.generation, late_open, verdict));
            (answer, outcome)
        };
        if answer == LateAnswer::Wedged {
            // REV8-03: the call is recorded and then never answers. The lock went
            // back first, so the test can still read what this fake recorded while the
            // watch is stuck inside the runtime call.
            std::future::pending::<()>().await;
        }
        outcome
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the R2-02-02 late-release pin never probes");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the R2-02-02 late-release pin never discovers");
    }
}

/// An actor whose release deadline is `window` seconds, driven by the late-watch
/// fake, with the reap's event channel held by the caller.
fn late_watch_state(
    runtime: Arc<LateWatchRuntime>,
    window: u64,
) -> (ActorState, mpsc::UnboundedReceiver<super::ActorMsg>) {
    let (tx, rx) = mpsc::unbounded_channel::<ActorMsg>();
    (
        ActorState::empty(
            crate::config::AppConfig {
                release_timeout_s: window,
                ..crate::config::AppConfig::default()
            },
            crate::testutil::temp_dir("late-release"),
            BTreeMap::new(),
            runtime as Arc<dyn crate::engine::EngineRuntime>,
            Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
            tx,
        ),
        rx,
    )
}

/// Seats a load row that owns a spawn, in the state and slot shape the pin is about
/// to drive. The ticket is live in the runtime either way; `occupant` is what the
/// slot reports.
fn seat_spawn_row(
    state: &mut ActorState,
    name: &str,
    job_state: SwapState,
    ticket: &SpawnTicket,
    occupant: Option<Occupant>,
) -> JobId {
    let job_id = JobId(state.next_job);
    state.next_job += 1;
    state.jobs.insert(
        job_id,
        Job {
            kind: JobKind::Load,
            // Seated directly, not admitted: the row never took a rung.
            path: None,
            model: name.to_string(),
            engine: "mock".to_string(),
            category: CategoryId::Work,
            state: job_state,
            terminal: false,
            outcome: None,
            error: None,
            need: Some(Gibibytes(2.0)),
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
            spawned: Some(ticket.clone()),
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
            slot: Some(0),
            queue_fingerprint: None,
        },
    );
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: ticket.port,
        occupant,
        job: Some(job_id),
    }];
    job_id
}

/// The R2-02-02 shape: a `Ready` load row that owns a spawn while its slot holds NO
/// occupant — the engine started, readiness never landed, so nothing was installed
/// and the slot reads empty while the ticket is still live.
fn uninstalled_spawn_row(state: &mut ActorState, name: &str, ticket: &SpawnTicket) -> JobId {
    seat_spawn_row(state, name, SwapState::Ready, ticket, None)
}

/// An installed occupant as [`ActorState::install`] seats one: the ticket it was
/// spawned with, and nothing else the pins need.
fn seated_occupant(name: &str, ticket: &SpawnTicket) -> Occupant {
    Occupant {
        model: name.to_string(),
        need: Gibibytes(2.0),
        ram_need: None,
        port: ticket.port,
        ticket: ticket.clone(),
        loaded_at: Instant::now(),
        launched_at_wall: std::time::SystemTime::now(),
        spawn_config: None,
        stale_config: false,
        serve_id: None,
        measure: None,
    }
}

/// The REV8-01 shape: a `Serving` row whose OWN spawn is the slot's installed
/// occupant, held non-terminal so a cancel can reach it — the cancel-of-a-serving-
/// model case where G12 keeps the occupant installed if the reap cannot prove the
/// death, and `/har/status` goes on reporting the model as serving.
fn serving_occupant_row(state: &mut ActorState, name: &str, ticket: &SpawnTicket) -> JobId {
    seat_spawn_row(
        state,
        name,
        SwapState::Serving,
        ticket,
        Some(seated_occupant(name, ticket)),
    )
}

/// The slot's reported occupant ticket, as the watch reads it.
fn seated_occupant_ticket(state: &ActorState) -> Option<SpawnTicket> {
    state.slots[0]
        .occupant
        .as_ref()
        .map(|occupant| occupant.ticket.clone())
}

/// Runs one job to terminalization on a release nobody proved, exactly as the actor
/// does: the real `spawn_kill_confirm` withholds its evidence until the real
/// deadline, and the real `Released(Err)` event is applied on the actor's turn. The
/// late window opens between the two, so every poll recorded after the flip belongs
/// to the watch and to nothing else.
async fn drive_to_unproven_terminal(
    state: &mut ActorState,
    runtime: &LateWatchRuntime,
    job_id: JobId,
    arm: Arm,
    answer: LateAnswer,
    rx: &mut mpsc::UnboundedReceiver<super::ActorMsg>,
) {
    use super::SchedulerError;
    match arm {
        Arm::Failed => state.fail_job(
            job_id,
            SchedulerError::EngineFailed("engine never readied".to_string()),
            "engine not ready",
        ),
        Arm::CancelRequested => state.cancel_job(job_id, "unloaded by request"),
    }
    let message = tokio::time::timeout(Duration::from_secs(20), rx.recv())
        .await
        .expect("the reap answers inside the test deadline")
        .expect("the actor channel is open");
    let ActorMsg::Event { job, event } = message else {
        panic!("expected the reap's release event");
    };
    assert_eq!(job, job_id, "the reap answers its own job");
    runtime.open_late_window(answer);
    state.apply_event(job_id, event);
}

/// Waits for the detached watch to record its polls — the only honest seam to a
/// task the actor does not own (that is the point of R2-02-02 option B).
async fn await_late_polls(
    runtime: &LateWatchRuntime,
    want: impl Fn(usize) -> bool,
) -> Vec<(u16, u64, &'static str)> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let polls = runtime.late_polls();
        if want(polls.len()) {
            return polls;
        }
        assert!(
            Instant::now() < deadline,
            "the late watch never polled as expected; last {polls:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Late-release warns for ONE model: the capture is process-global and lib tests
/// run in parallel, so the model name is the quarantine.
fn late_release_warns(capture: &Arc<Mutex<Vec<CapturedWarn>>>, model: &str, needle: &str) -> usize {
    capture
        .lock()
        .expect("capture lock")
        .iter()
        .filter(|(message, fields)| {
            message.contains(needle)
                && fields
                    .get("model")
                    .is_some_and(|m| m.trim_matches('"') == model)
        })
        .count()
}

/// The D9 warm for every wording the pins below read, on sentinel names nothing
/// else looks at. A zero-length release window expires both the reap's deadline and
/// the watch's own bound on the first poll, so proving delivery costs no wall clock.
#[tokio::test]
async fn every_late_release_wording_is_warmed_and_delivered() {
    let capture = install_warn_capture();
    for (name, answer, needle) in [
        (
            "latewarm-confirmed",
            LateAnswer::Confirmed,
            "no runtime entry for this generation remains",
        ),
        (
            "latewarm-superseded",
            LateAnswer::Superseded,
            "ticket was superseded",
        ),
        (
            "latewarm-stillheld",
            LateAnswer::StillHeld,
            "the next load on this port or the boot reclaim must reap it",
        ),
    ] {
        let runtime = Arc::new(LateWatchRuntime::new());
        let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 0);
        let ticket = SpawnTicket {
            generation: 3,
            kind: ResourceKind::ChildProcess,
            pid: 9_003,
            port: 19_400,
        };
        let job_id = uninstalled_spawn_row(&mut state, name, &ticket);
        drive_to_unproven_terminal(&mut state, &runtime, job_id, Arm::Failed, answer, &mut rx)
            .await;
        await_late_polls(&runtime, |count| count >= 1).await;
        assert_eq!(
            late_release_warns(&capture, name, needle),
            1,
            "the {needle:?} callsite was warmed but never delivered"
        );
    }
}

/// The `Failed` arm: a load that failed readiness and whose reap never proved the
/// release terminalizes as it always did, and its ticket gets exactly one bounded
/// late watch that records the confirmation the reap missed and says so once.
/// Falsifier: drop the hook from the `Failed` arm and the late-window poll count
/// stays 0 (the R2-02-02 silent case); re-kill in the watch and the kill count is 2.
#[tokio::test]
async fn a_failure_the_reap_could_not_prove_watches_its_ticket_late() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 11,
        kind: ResourceKind::ChildProcess,
        pid: 7_101,
        port: 19_411,
    };
    let job_id = uninstalled_spawn_row(&mut state, "lategreen-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::Failed,
        LateAnswer::Confirmed,
        &mut rx,
    )
    .await;

    let job = &state.jobs[&job_id];
    assert!(
        job.terminal,
        "an unproven release terminalizes; state {:?}, error {:?}",
        job.state, job.error
    );
    assert_eq!(
        job.state,
        SwapState::Idle,
        "the funnel still walks Failed -> Reap -> Idle"
    );
    assert!(
        runtime.poll_count() > runtime.late_polls().len(),
        "the job's own reap polled for its release first"
    );
    let polls = await_late_polls(&runtime, |count| count >= 1).await;
    assert_eq!(
        polls,
        vec![(19_411, 11, "confirmed")],
        "the watch asks after its own ticket once the reap is gone"
    );
    // One bounded watch: the proof ends it, and no second warn follows.
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(runtime.late_polls(), polls, "the watch stops on the proof");
    assert_eq!(
        runtime.kill_count(),
        1,
        "the job's own reap issued the only kill: the watch never re-kills"
    );
    assert_eq!(
        late_release_warns(
            &capture,
            "lategreen-row",
            "no runtime entry for this generation remains"
        ),
        1,
        "the late confirmation is named once, not silently"
    );
}

/// The `CancelRequested` arm: the same shape reached by an operator's cancel. The
/// row keeps its plain cancel wording (the slot had no occupant to keep installed),
/// and its orphan gets the one late watch.
#[tokio::test]
async fn a_cancel_the_reap_could_not_prove_watches_its_ticket_late() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 12,
        kind: ResourceKind::ChildProcess,
        pid: 7_102,
        port: 19_412,
    };
    let job_id = uninstalled_spawn_row(&mut state, "latecancel-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::CancelRequested,
        LateAnswer::Confirmed,
        &mut rx,
    )
    .await;

    let job = &state.jobs[&job_id];
    assert!(job.terminal, "the cancel terminalizes");
    assert_eq!(job.state, SwapState::Idle);
    assert_eq!(job.outcome.as_deref(), Some("cancelled"));
    assert!(
        job.error
            .as_ref()
            .is_some_and(|error| error.to_string().contains("unloaded by request")),
        "the row keeps the cancel's own error: {:?}",
        job.error
    );
    let polls = await_late_polls(&runtime, |count| count >= 1).await;
    assert_eq!(polls, vec![(19_412, 12, "confirmed")]);
    assert_eq!(runtime.kill_count(), 1, "the cancel's reap: the only kill");
    assert_eq!(
        late_release_warns(
            &capture,
            "latecancel-row",
            "no runtime entry for this generation remains"
        ),
        1
    );
}

/// REV8-01, the shape the first cut's comment got wrong: a cancel that reached
/// `Serving` and whose own reap could not prove the death keeps G12's posture — the
/// occupant stays installed and `/har/status` still reports the model serving — so no
/// late watch is armed on that ticket and no warn contradicts the page. Falsifier:
/// arm the watch on any spawned ticket (the pre-scope rule) and both the late poll
/// and the confirmation warn appear here.
#[tokio::test]
async fn a_cancel_of_an_installed_occupant_arms_no_late_watch() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 51,
        kind: ResourceKind::ChildProcess,
        pid: 7_151,
        port: 19_451,
    };
    let job_id = serving_occupant_row(&mut state, "lateinstalled-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::CancelRequested,
        LateAnswer::Confirmed,
        &mut rx,
    )
    .await;

    let job = &state.jobs[&job_id];
    assert!(job.terminal, "the cancel terminalizes as it always did");
    assert_eq!(job.state, SwapState::Idle);
    assert_eq!(job.outcome.as_deref(), Some("cancelled"));
    let installed = seated_occupant_ticket(&state)
        .expect("an unproven release never clears an installed occupant");
    assert_eq!(
        (installed.port, installed.generation),
        (19_451, 51),
        "the slot keeps reporting THIS spawn as serving, so the watch stays out"
    );
    assert_eq!(
        runtime.kill_count(),
        1,
        "the cancel's reap: still the only kill"
    );
    assert!(
        runtime.poll_count() > 0,
        "the reap polled for its own release inside its window"
    );
    // The late window is open and scripted Confirmed: a watch would poll on its first
    // pass and warn. Neither may happen.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        runtime.late_polls().is_empty(),
        "no late watch on an installed ticket: {:?}",
        runtime.late_polls()
    );
    for needle in [
        "no runtime entry for this generation remains",
        "the next load on this port or the boot reclaim must reap it",
        "ticket was superseded",
    ] {
        assert_eq!(
            late_release_warns(&capture, "lateinstalled-row", needle),
            0,
            "the page says serving; {needle:?} must not be said about it"
        );
    }
}

/// The other half of the REV8-01 scope: an occupant that is NOT this ticket — a
/// restart's predecessor still installed while its replacement died — must not blind
/// the watch. The engine the slot reports is a different spawn; this orphan is still
/// nobody's to reap. Falsifier: blind the watch on ANY occupant and the late poll
/// count here stays 0, which is the R2-02-02 silence again.
#[tokio::test]
async fn a_slot_holding_another_generation_still_arms_the_late_watch() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let dead_spawn = SpawnTicket {
        generation: 61,
        kind: ResourceKind::ChildProcess,
        pid: 7_161,
        port: 19_461,
    };
    let predecessor = SpawnTicket {
        generation: 60,
        kind: ResourceKind::ChildProcess,
        pid: 7_160,
        port: 19_461,
    };
    let job_id = seat_spawn_row(
        &mut state,
        "latetwin-row",
        SwapState::Ready,
        &dead_spawn,
        Some(seated_occupant("latetwin-row", &predecessor)),
    );

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::Failed,
        LateAnswer::Confirmed,
        &mut rx,
    )
    .await;

    let polls = await_late_polls(&runtime, |count| count >= 1).await;
    assert_eq!(
        polls,
        vec![(19_461, 61, "confirmed")],
        "the watch names the generation it owns, never the installed one"
    );
    assert!(
        !runtime.generations_polled().contains(&60),
        "the predecessor's generation is the slot's business, not this watch's: {:?}",
        runtime.generations_polled()
    );
    assert_eq!(runtime.kill_count(), 1, "the job's own reap: the only kill");
    assert_eq!(
        late_release_warns(
            &capture,
            "latetwin-row",
            "no runtime entry for this generation remains"
        ),
        1
    );
}

/// The successor leg: once a newer spawn owns the port, `release_confirmed` reports
/// `StaleTicket` and the watch stops at once. It may not re-kill, may not keep
/// polling, and may never address the newer generation — an unowned kill wearing a
/// harness ticket is the one thing worse than the leak it watches.
#[tokio::test]
async fn a_superseded_ticket_ends_the_late_watch_without_touching_the_successor() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 21,
        kind: ResourceKind::ChildProcess,
        pid: 7_111,
        port: 19_413,
    };
    let job_id = uninstalled_spawn_row(&mut state, "latesupersede-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::Failed,
        LateAnswer::Superseded,
        &mut rx,
    )
    .await;

    let polls = await_late_polls(&runtime, |count| count >= 1).await;
    assert_eq!(
        polls,
        vec![(19_413, 21, "superseded")],
        "the stale answer ends the watch on its first poll"
    );
    assert_eq!(runtime.kill_count(), 1, "the watch never re-kills");
    assert!(
        !runtime.generations_polled().contains(&22),
        "generation 22 is the successor's: the watch may not even ask after it: {:?}",
        runtime.generations_polled()
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        runtime.late_polls(),
        polls,
        "the watch stops on the stale answer"
    );
    assert_eq!(
        late_release_warns(&capture, "latesupersede-row", "ticket was superseded"),
        1,
        "the stop is named once"
    );
    assert_eq!(
        late_release_warns(
            &capture,
            "latesupersede-row",
            "no runtime entry for this generation remains"
        ),
        0,
        "a superseded ticket never claims a release it never saw"
    );
}

/// The bound: a release that is never proven gets exactly one more
/// `release_timeout_s` of watching, then one warn naming port, generation and model
/// and the recovery that is left. Nothing here grows into a second reap task.
#[tokio::test]
async fn an_unproven_release_watches_one_window_and_then_reports_and_stops() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 31,
        kind: ResourceKind::ChildProcess,
        pid: 7_121,
        port: 19_414,
    };
    let job_id = uninstalled_spawn_row(&mut state, "latebound-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::Failed,
        LateAnswer::StillHeld,
        &mut rx,
    )
    .await;

    let started = await_late_polls(&runtime, |count| count >= 2).await;
    assert!(
        started.iter().all(|(_, _, verdict)| *verdict == "pending"),
        "the watch only asks, it never acts: {started:?}"
    );
    let deadline = Instant::now() + Duration::from_secs(10);
    let needle = "the next load on this port or the boot reclaim must reap it";
    while late_release_warns(&capture, "latebound-row", needle) == 0 {
        assert!(
            Instant::now() < deadline,
            "the watch never reported its own deadline"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let stopped = runtime.late_polls().len();
    assert!(stopped > started.len(), "the watch polled to its bound");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        runtime.late_polls().len(),
        stopped,
        "the task is bounded: it stops after one release_timeout_s"
    );
    assert_eq!(runtime.kill_count(), 1, "the watch never re-kills");
    assert_eq!(
        late_release_warns(&capture, "latebound-row", needle),
        1,
        "the residue is named once"
    );
    assert_eq!(
        late_release_warns(
            &capture,
            "latebound-row",
            "no runtime entry for this generation remains"
        ),
        0
    );
}

/// REV8-03: a `release_confirmed` that never returns cannot hold the watch past its
/// one window either. The hard outer timeout (the same window, plus one poll of slack
/// so the in-loop check stays the ordinary exit) ends the task and emits that single
/// bound warning. Falsifier: drop the outer timeout and no warning ever arrives — the
/// task stays parked inside the wedged runtime call forever.
#[tokio::test]
async fn a_wedged_release_check_cannot_outlive_the_bound() {
    let capture = install_warn_capture();
    let runtime = Arc::new(LateWatchRuntime::new());
    let (mut state, mut rx) = late_watch_state(Arc::clone(&runtime), 1);
    let ticket = SpawnTicket {
        generation: 71,
        kind: ResourceKind::ChildProcess,
        pid: 7_171,
        port: 19_471,
    };
    let job_id = uninstalled_spawn_row(&mut state, "latewedged-row", &ticket);

    drive_to_unproven_terminal(
        &mut state,
        &runtime,
        job_id,
        Arm::Failed,
        LateAnswer::Wedged,
        &mut rx,
    )
    .await;

    let polls = await_late_polls(&runtime, |count| count >= 1).await;
    assert_eq!(
        polls,
        vec![(19_471, 71, "wedged")],
        "the watch is stuck inside the single call it made"
    );
    let needle = "the next load on this port or the boot reclaim must reap it";
    // The hard bound is 1 window + 1 poll (1.05 s here); the 25 s ceiling only
    // tolerates parallel-suite scheduling noise, it never weakens the falsifier:
    // without the outer timeout this warning is never emitted at all.
    let deadline = Instant::now() + Duration::from_secs(25);
    while late_release_warns(&capture, "latewedged-row", needle) == 0 {
        assert!(
            Instant::now() < deadline,
            "the hard timeout never fired: the wedged check outlived the bound"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        runtime.kill_count(),
        1,
        "wedged or not, the watch never re-kills"
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        runtime.late_polls().len(),
        1,
        "the task ended with the timeout: no poll past the bound"
    );
    assert_eq!(
        late_release_warns(&capture, "latewedged-row", needle),
        1,
        "the spent window is named exactly once, from the hard timeout"
    );
    assert_eq!(
        late_release_warns(
            &capture,
            "latewedged-row",
            "no runtime entry for this generation remains"
        ),
        0,
        "a wedged check proves nothing about the entry, so nothing is claimed"
    );
}

/// R10 (operator 2026-11-01: "8003-8005 are manual only"): a ninfer row
/// borrowing a manual number as its container-internal port is refused before
/// spawn, the SE internal default (8808) arms like any ordinary row,
/// and the rescan twin names the key on the page before any load tries.
#[tokio::test]
async fn a_borrowed_internal_port_is_refused_at_the_gate_and_reported_at_rescan() {
    use super::{AdmissionError, SchedulerError};
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "ninfer".to_string(),
        Arc::new(crate::engine::ninfer::NinferAdapter::new("Ubuntu", "ops"))
            as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        crate::testutil::temp_dir("r10-port"),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_109,
        occupant: None,
        job: None,
    }];
    let ninfer_row = |port: &str| -> ModelEntry {
        let mut map = BTreeMap::new();
        map.insert("engine".to_string(), "ninfer".to_string());
        map.insert("vram_gb".to_string(), "4".to_string());
        if !port.is_empty() {
            map.insert("ninfer_internal_port".to_string(), port.to_string());
        }
        ModelEntry::from_flat(&map, "nin").expect("entry")
    };
    // A manual number is dead on arrival: a named refusal, no spawn.
    let stolen = ninfer_row("8004");
    let id = state.register_queued_load(0, &stolen, Vec::new());
    state.start_queued_load(id, 0, stolen);
    let job = &state.jobs[&id];
    assert!(job.terminal, "a borrowed port must never reach spawn");
    assert!(
        matches!(
            job.error,
            Some(SchedulerError::Admission(AdmissionError::PortClaimed {
                port: 8004,
                ..
            }))
        ),
        "{:?}",
        job.error
    );
    // The engine's own EXPOSE number arms like any ordinary row.
    let ordinary = ninfer_row("");
    let id = state.register_queued_load(0, &ordinary, Vec::new());
    state.start_queued_load(id, 0, ordinary);
    assert!(
        !state.jobs[&id].terminal,
        "8808 is no namespace's to refuse: {:?}",
        state.jobs[&id].error
    );
    // The page twin: the rescan names the key before any load tries.
    state.apply_catalog(
        crate::model::scan::Catalog {
            models: vec![ninfer_row("8005")],
            errors: Vec::new(),
        },
        None,
        &DriftStats::default(),
    );
    assert!(
        state
            .catalog_errors
            .iter()
            .any(|e| { e.reason.contains("ninfer_internal_port") && e.reason.contains("8005") }),
        "{:?}",
        state.catalog_errors
    );
}

/// Row-10 guard (RULED 2026-11-01): a floor the card denies must refuse BEFORE
/// any kill. The release-first ruling buys the victim's bytes only when the
/// deficit is one a release could actually fix — when freeing everything the
/// harness may release still leaves the floor out of reach, the cost is not
/// bought. Field case (GOTCHAS, R5 2026-11-01): foreign residents hold the
/// card at 70.5/72 and nothing is evictable.
#[tokio::test]
async fn a_floor_no_release_can_reach_refuses_before_the_victim_dies() {
    let dir = crate::testutil::temp_dir("r10-floor");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("greedy.yaml");
    std::fs::write(
            &yaml,
            "engine: mock\ncategory: work\ncost: greedy\nvram_gb: 2\nlaunch_only_with_at_least_vram_free: 60\n",
        )
        .expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "greedy");
    assert_eq!(
        entry.vram_gate.floor(),
        Some(60.0),
        "the floor key must survive the real parser"
    );
    state.catalog = vec![entry.clone()];
    let ticket = fold_ticket(19_049);
    seat_spawn(&mut state, &entry, &ticket, 19_049);
    // Arm the row against its own slot exactly as `start_queued_load` does,
    // then hand the admit the witness sample the armed sample task would send.
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.slots[0].job = Some(job_id);
    if let Some(job) = state.jobs.get_mut(&job_id) {
        job.need = Some(Gibibytes(2.0));
        job.victim = Some(super::Victim {
            model: entry.name.clone(),
            ticket: ticket.clone(),
            slot: 0,
            evicted: false,
            cost: 2.0,
        });
    }
    state.step(job_id, SwapState::Admit);
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 70.5,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(job.terminal, "an unreachable floor must refuse at the gate");
    assert!(
        job.path.is_none(),
        "a refusal never names a rung the gate never accepted: {:?}",
        job.path
    );
    assert!(
        !job.release_first,
        "the guard precedes release-first: no resident is doomed for a floor the card denies"
    );
    let error = job
        .error
        .as_ref()
        .expect("the refusal travels with the row")
        .to_string();
    assert!(
        error.contains("No releases allow for the VRAM floor")
            && error.contains("Check foreign residents"),
        "{error}"
    );
    assert!(error.contains("unreleasable"), "{error}");
    assert!(
        state.slots[0].occupant.is_some(),
        "refusal before kill: the resident the ruling protects is still seated"
    );
    assert!(
        state.slots[0].job.is_none(),
        "the refused row releases its claim on the slot"
    );
}

/// The other half: the guard guards the FLOOR only. A `need`-short swap with a
/// same-slot victim still takes the release-first path (ruling 2026-11-01,
/// REVISABLE): the settle window re-gates it against the physical card.
#[tokio::test]
async fn a_need_short_swap_still_takes_the_release_first_path() {
    let dir = crate::testutil::temp_dir("r10-release-first");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("big.yaml");
    std::fs::write(&yaml, "engine: mock\ncategory: work\nvram_gb: 40\n").expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "big");
    assert_eq!(entry.vram_gate.floor(), None, "this row carries no floor");
    state.catalog = vec![entry.clone()];
    let ticket = fold_ticket(19_050);
    seat_spawn(&mut state, &entry, &ticket, 19_050);
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.slots[0].job = Some(job_id);
    if let Some(job) = state.jobs.get_mut(&job_id) {
        job.need = Some(Gibibytes(40.0));
        job.victim = Some(super::Victim {
            model: entry.name.clone(),
            ticket,
            slot: 0,
            evicted: false,
            cost: 2.0,
        });
    }
    state.step(job_id, SwapState::Admit);
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 70.5,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        job.release_first,
        "a releasable deficit still re-gates on the physical card: {:?}",
        job.error
    );
    assert_eq!(
        job.path,
        Some(AdmissionPath::ReleaseFirst),
        "the release-first door records its own rung"
    );
    assert!(!job.terminal, "release-first proceeds, it does not refuse");
    assert_eq!(job.state, SwapState::Dispose);
}

/// B6 rung pin, door 1: a load the card already holds admits on the
/// `FitsNow` rung — and the rung, once recorded, is what the `/har/jobs`
/// projection serves. A same-slot replacement alone never changes the
/// rung: releasing the occupant the request itself named is the request.
/// Falsifier: derive the rung before the planner arm (every admit reads
/// `FitsNow` and the drain pin goes red) or drop the `job.path`
/// assignment (this pin reads `None`).
#[tokio::test]
async fn a_fit_now_admission_records_the_fits_now_rung() {
    let dir = crate::testutil::temp_dir("rung-fits");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("small.yaml");
    std::fs::write(&yaml, "engine: mock\ncategory: work\nvram_gb: 4\n").expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "small");
    state.catalog = vec![entry.clone()];
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_130,
        occupant: None,
        job: None,
    }];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    assert_eq!(state.jobs[&job_id].path, None, "no decision yet, no rung");
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 2.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !job.terminal,
        "the roomy card admits the load: {:?}",
        job.error
    );
    assert_eq!(job.path, Some(AdmissionPath::FitsNow));
    // The ledger projection is the only source of the wire `path`, so
    // the served row must carry the same rung the decision recorded.
    let served = state
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job_id)
        .expect("the row is on the ledger");
    assert_eq!(served.path, Some(AdmissionPath::FitsNow));
    assert_eq!(
        serde_json::to_value(&served).expect("JobStatus serializes")["path"],
        "FitsNow",
        "the wire name of the rung is the variant name, like every other ledger enum"
    );
}

/// B6 rung pin, door 2: a load short on its need whose room comes from
/// evictable cross-category victims admits on the `Drain` rung. The pin
/// reads the recorded rung, not `job.victims`: `dequeue_victim` moves the
/// planned victim out of the queue before the row is observed, so the
/// rung is the only honest trace left on the row.
/// Falsifier: derive the rung after the `dequeue_victim` step and this
/// row reads `FitsNow` — the pin goes red.
#[tokio::test]
async fn a_drained_admission_records_the_drain_rung() {
    let dir = crate::testutil::temp_dir("rung-drain");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("big.yaml");
    std::fs::write(&yaml, "engine: mock\ncategory: work\nvram_gb: 40\n").expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "big");
    state.catalog = vec![entry.clone()];
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_131,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_132,
            occupant: Some(Occupant {
                model: "aux-resident".to_string(),
                need: Gibibytes(30.0),
                ram_need: None,
                port: 18_132,
                ticket: fold_ticket(18_132),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    // free = 72 - 2 - 40 = 30 < need 40: the deficit is 10, and the AUX
    // occupant (AUX is evictable by WORK in the shipped matrix) covers it.
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 40.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !job.terminal,
        "the planner covers the deficit, so the load admits: {:?}",
        job.error
    );
    assert_eq!(job.path, Some(AdmissionPath::Drain));
    assert!(!job.release_first, "a drain is not the release-first door");
    assert_eq!(
        job.victim.as_ref().map(|v| v.model.as_str()),
        Some("aux-resident"),
        "the planned victim rides the row"
    );
    assert_eq!(job.state, SwapState::Dispose);
}

/// B6R-02 rung pin, the refusal the gate never made: a load the gate
/// admitted but the launch preflight refused still names the rung the
/// gate decided. `finish_admit` derives the rung after every gate
/// refusal arm and before `preflight_spec`, so a `launch refused` row
/// honestly carries refusal AND rung — the rung is the decision record,
/// not an outcome promise. Gate refusals stay the other class: they
/// return before the derivation and carry no rung (pinned by
/// `a_floor_no_release_can_reach_refuses_before_the_victim_dies`).
/// Falsifier: clear `job.path` in the preflight refusal arm, or derive
/// the rung after `preflight_spec` — either way this row reads `None`
/// and the pin goes red.
#[tokio::test]
async fn a_launch_refused_row_keeps_the_rung_the_gate_decided() {
    let dir = crate::testutil::temp_dir("rung-launch-refused");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("small.yaml");
    std::fs::write(&yaml, "engine: mock\ncategory: work\nvram_gb: 4\n").expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "small");
    state.catalog = vec![entry.clone()];
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_133,
        occupant: None,
        job: None,
    }];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    // The launch preflight (chunk 10 [2]) reads the catalog at refusal
    // time, after the gate and the rung derivation: an emptied catalog
    // drives the `SchedulerError::Spawn` refusal exactly like a row
    // deleted between scan and spawn. The roomy card lets the gate
    // pass untouched, so the refusal class is the preflight's alone.
    state.catalog.clear();
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 2.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(job.terminal, "the launch refusal terminalizes the row");
    assert_eq!(job.state, SwapState::AdmitFail);
    assert_eq!(job.outcome.as_deref(), Some("launch refused"));
    let error = job.error.as_ref().expect("the refusal rides the row");
    assert!(
        matches!(error, crate::scheduler::SchedulerError::Spawn(reason)
                if reason.contains("vanished from the catalog mid-job")),
        "the refusal is the preflight's, not a gate refusal: {error}"
    );
    assert_eq!(
        job.path,
        Some(AdmissionPath::FitsNow),
        "the gate decided before the preflight refused: the rung names that decision"
    );
    let served = state
        .snapshot()
        .jobs
        .into_iter()
        .find(|j| j.id == job_id)
        .expect("the row is on the ledger");
    assert_eq!(served.path, Some(AdmissionPath::FitsNow));
    assert!(
        served
            .error
            .as_ref()
            .is_some_and(|reason| reason.contains("vanished from the catalog mid-job")),
        "the served row carries refusal AND rung: {:?}",
        served.error
    );
}

/// B6 log pin, by source scan: the accepted-decision line inside
/// `finish_admit` must be an info event named `admission path` carrying
/// the rung under the field `path`. A `tracing`-capture pin would add a
/// callsite-interest flake to the suite (`GOTCHAS.md` tracing rows, the
/// D9 shape: the first touch wins process-wide), so this scans the
/// source text; the rung VALUES are pinned by the decision pins and the
/// wire shape by the `/har/jobs` integration pin.
/// Falsifier: rename the field (`rung = ?path`), demote the event out of
/// `info!`, or drop the line — each fails the scan.
#[test]
fn the_admission_path_log_line_names_the_rung_field() {
    let source = include_str!("admission.rs");
    let start = source
        .find("fn finish_admit_tail(")
        .expect("the admitted tail is still in this file");
    let body = &source[start..];
    let at = body
        .find("\"admission path\"")
        .expect("the accepted-decision line names the event `admission path`");
    let macro_start = body[..at]
        .rfind("tracing::info!(")
        .expect("the admission path line is an info event");
    let line = &body[macro_start..at];
    assert!(
        line.contains("path = ?path"),
        "the rung must ride the log under the field `path`: {line}"
    );
    assert!(line.contains("model ="), "the line names the model: {line}");
    assert!(
        line.contains("category ="),
        "the line names the category: {line}"
    );
    assert!(
        !line.contains("need_gib") && !line.contains("free_gib"),
        "the rung line quotes no numbers — the refusal and the extended \
             decision lines already carry them: {line}"
    );
}

/// A `whole` row (operator ruling 2026-11-04): admits on a fully-free
/// card — here the card holds exactly the driver baseline and that
/// baseline sits inside the reserve band, so the credited reading reaches
/// the whole budget minus the reserve (68 = 72 - 2 * 2). The settle twin
/// rides `gate_target`: after a release the window must see the card at
/// least `total - reserve` free.
/// Falsifier: drop the `whole` leg's auto floor and a key-less-shaped row
/// gates on need alone — `gate_target` reads 12 and this pin fails.
#[tokio::test]
async fn a_whole_row_admits_on_a_fully_free_card() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("whole-free");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("whole.yaml");
    std::fs::write(
        &yaml,
        "engine: mock\ncategory: work\nvram_gb: 12\nlaunch_only_with_at_least_vram_free: whole\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "whole");
    assert_eq!(
        entry.vram_gate,
        crate::model::VramGate::Whole,
        "the keyword must survive the real parser"
    );
    state.catalog = vec![entry.clone()];
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: 18_120,
        occupant: None,
        job: None,
    }];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 2.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !job.terminal,
        "a fully-free card admits the whole row: {:?}",
        job.error
    );
    assert_eq!(
        job.gate_target,
        Some(68.0),
        "the settle twin of the whole gate is total - 2 * reserve"
    );
    assert!(
        !matches!(
            job.error,
            Some(SchedulerError::Admission(
                AdmissionError::WholeMachineBusy { .. }
            ))
        ),
        "{:?}",
        job.error
    );
}

/// The wait posture (operator ruling 2026-11-04): any other holder and
/// the `whole` row refuses, naming it with the reason
/// "whole machine requested" — and evicts nothing: no victim was planned
/// (the planner is never asked on this path), no unload job is minted,
/// and the holder is still loaded after the refusal.
/// Falsifier: drop the holder scan from the whole leg and the clean-
/// sample leg admits beside the holder; drop the whole leg itself and
/// the refusal arrives as the numeric floor class — both pins go red.
/// (The `plan_victims` skip above is defense-in-depth: the holder scan
/// already guarantees an empty candidate set, so removing only the skip
/// cannot manifest — verified by mutation, suite stayed green.)
#[tokio::test]
async fn a_whole_row_waits_for_every_other_holder_and_evicts_nothing() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("whole-holder");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("whole.yaml");
    std::fs::write(
        &yaml,
        "engine: mock\ncategory: work\nvram_gb: 12\nlaunch_only_with_at_least_vram_free: whole\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "whole");
    state.catalog = vec![entry.clone()];
    let holder_ticket = fold_ticket(18_122);
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_121,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_122,
            occupant: Some(Occupant {
                model: "aux-resident".to_string(),
                need: Gibibytes(6.0),
                ram_need: None,
                port: 18_122,
                ticket: holder_ticket.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 8.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(job.terminal, "any holder must refuse the whole row");
    let error = job
        .error
        .as_ref()
        .expect("the refusal travels with the row");
    let msg = error.to_string();
    assert!(
        msg.contains("whole machine requested") && msg.contains("aux-resident"),
        "{msg}"
    );
    assert!(
        msg.contains("at least 68.0 GiB free required"),
        "the refusal must print the target it needs (B5R-02): {msg}"
    );
    match error {
        SchedulerError::Admission(AdmissionError::WholeMachineBusy {
            blockers,
            wait_hint,
            ..
        }) => {
            assert!(
                blockers
                    .iter()
                    .any(|b| b.model == "aux-resident" && b.reason == "whole machine requested"),
                "the holder must be named with the ruling's reason: {blockers:?}"
            );
            // The B2 hint rides the new refusal like any other memory
            // refusal: nothing ahead quotes no place, and with no
            // confirmed release yet the estimate is absent, never zero.
            assert_eq!(wait_hint.queue_position, None, "{wait_hint:?}");
            assert_eq!(wait_hint.expected_release_s, None, "{wait_hint:?}");
        }
        other => panic!("the wait refusal has its own class: {other:?}"),
    }
    assert!(
        !msg.contains("considered evicting"),
        "the whole path never plans victims: {msg}"
    );
    assert!(
        job.victim.is_none() && job.victims.is_empty() && !job.release_first,
        "no victim of any kind was queued"
    );
    assert!(
        !msg.contains("No releases allow"),
        "the row-10 guard sentence belongs to the floor refusal only: {msg}"
    );
    assert!(
        state.jobs.values().all(|j| j.kind != JobKind::Unload),
        "the refusal names holders; it mints no unload job"
    );
    assert!(
        state.slots[1].occupant.is_some(),
        "the holder the refusal protects is still loaded"
    );
    // The holder scan is a separate reason from the arithmetic: a sample
    // that momentarily shows the card clean (attribution Unavailable —
    // exactly when the numeric floor leg would wave the row through)
    // still refuses while another slot holds a model.
    // Falsifier: drop `!holders.is_empty()` from the whole leg and this
    // leg admits — the pin goes red.
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    state.catalog = vec![entry.clone()];
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_121,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_122,
            occupant: Some(Occupant {
                model: "aux-resident".to_string(),
                need: Gibibytes(6.0),
                ram_need: None,
                port: 18_122,
                ticket: holder_ticket.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 0.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    assert!(
        matches!(
            state.jobs[&job_id].error,
            Some(SchedulerError::Admission(
                AdmissionError::WholeMachineBusy { .. }
            ))
        ),
        "a momentarily-clean sample must not admit beside a holder: {:?}",
        state.jobs[&job_id].error
    );
}

/// The pinned same-slot behavior (operator ruling 2026-11-04, decided
/// for B5): an explicit same-slot load of a `whole` row IS a replacement
/// of its own slot's occupant — the request named that target, and the
/// occupant is never an eviction "to get there". It admits when the rest
/// of the card is free; the moment ANY other slot holds something the
/// same load refuses, still evicting nothing.
/// Falsifier: count the own-slot occupant as a holder and leg 1 goes red;
/// skip the holder scan and leg 2 goes red.
#[tokio::test]
async fn a_whole_row_replaces_its_own_slot_and_waits_for_the_rest() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("whole-sameslot");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("whole.yaml");
    std::fs::write(
        &yaml,
        "engine: mock\ncategory: work\nvram_gb: 40\nlaunch_only_with_at_least_vram_free: whole\n",
    )
    .expect("seed");
    let entry = rescan_entry(&yaml, "whole");
    let seat = |state: &mut ActorState, aux: Option<&str>| {
        state.slots = vec![
            Slot {
                category: CategoryId::Work,
                index: 0,
                engine_port: 18_123,
                occupant: Some(Occupant {
                    model: "old".to_string(),
                    need: Gibibytes(30.0),
                    ram_need: None,
                    port: 18_123,
                    ticket: fold_ticket(18_123),
                    loaded_at: Instant::now(),
                    launched_at_wall: std::time::SystemTime::now(),
                    spawn_config: None,
                    stale_config: false,
                    serve_id: None,
                    measure: None,
                }),
                job: None,
            },
            Slot {
                category: CategoryId::Aux,
                index: 1,
                engine_port: 18_124,
                occupant: aux.map(|name| Occupant {
                    model: name.to_string(),
                    need: Gibibytes(6.0),
                    ram_need: None,
                    port: 18_124,
                    ticket: fold_ticket(18_124),
                    loaded_at: Instant::now(),
                    launched_at_wall: std::time::SystemTime::now(),
                    spawn_config: None,
                    stale_config: false,
                    serve_id: None,
                    measure: None,
                }),
                job: None,
            },
        ];
    };
    let whole_state = |aux: Option<&str>| {
        let mut state = fold_state(&dir);
        state.cfg.vram_total_gb = 72.0;
        state.cfg.vram_reserve_gb = 2.0;
        state.catalog = vec![entry.clone()];
        seat(&mut state, aux);
        state
    };
    // Leg 1: only the replacement target (30) plus the baseline (2) hold
    // the card — the credited reading reaches 68 — so the load admits and
    // disposes its own victim, nothing else.
    let mut state = whole_state(None);
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 32.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !job.terminal,
        "the own slot may be replaced: {:?}",
        job.error
    );
    assert_eq!(job.state, SwapState::Dispose);
    assert_eq!(job.gate_target, Some(68.0));
    assert_eq!(
        job.victim.as_ref().map(|v| v.model.as_str()),
        Some("old"),
        "the same-slot replacement is the only release a whole load may perform"
    );
    assert!(job.victims.is_empty(), "never a cross-slot victim");
    // Leg 2: the same load one Aux holder later refuses naming it, and
    // both occupants survive the refusal. The sample still shows only the
    // own occupant's bytes (attribution Unavailable: the reading alone
    // would pass the numeric auto floor), so this leg pins the holder
    // scan as a SEPARATE reason — a clean-looking sample must not admit
    // a row while another slot holds a model.
    let mut state = whole_state(Some("aux-resident"));
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 32.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        matches!(
            job.error,
            Some(SchedulerError::Admission(
                AdmissionError::WholeMachineBusy { .. }
            ))
        ),
        "one foreign slot is enough to wait: {:?}",
        job.error
    );
    let msg = job.error.as_ref().expect("refusal").to_string();
    assert!(msg.contains("aux-resident"), "{msg}");
    assert!(
        msg.contains("at least 68.0 GiB free required"),
        "the refusal must print the target it needs (B5R-02): {msg}"
    );
    assert!(
        state.slots[0].occupant.is_some() && state.slots[1].occupant.is_some(),
        "the wait posture kills nothing: both holders are still loaded"
    );
    assert!(
        state.jobs.values().all(|j| j.kind != JobKind::Unload),
        "the refusal names holders; it mints no unload job"
    );
}

/// `whole` on a greedy row is legal and REPLACES the greedy floor leg
/// (ruling 2026-11-04, documented in MODELS.md): the global
/// `greedy_launch_min_free_gb` never applies to it. Leg 1 pins the
/// stricter verdict that replacement buys — a driver baseline above the
/// reserve denies the launch although the numeric greedy floor (6) would
/// pass the same reading; leg 2 admits on a card the reserve covers.
/// Falsifier: let `whole` fall through to the greedy default floor and
/// leg 1 admits — the pin goes red.
#[tokio::test]
async fn whole_replaces_the_greedy_floor_leg() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("whole-greedy");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("greedy.yaml");
    std::fs::write(
            &yaml,
            "engine: mock\ncategory: work\ncost: greedy\nvram_gb: 20\nlaunch_only_with_at_least_vram_free: whole\n",
        )
        .expect("seed");
    let entry = rescan_entry(&yaml, "greedy");
    assert_eq!(entry.vram_gate, crate::model::VramGate::Whole);
    let whole_state = || {
        let mut state = fold_state(&dir);
        state.cfg.vram_total_gb = 72.0;
        state.cfg.vram_reserve_gb = 2.0;
        state.cfg.greedy_launch_min_free_gb = 6.0;
        state.catalog = vec![entry.clone()];
        state.slots = vec![Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_125,
            occupant: None,
            job: None,
        }];
        state
    };
    // Leg 1: the card holds only a 4 GiB baseline. The greedy floor of 6
    // would let this through (66 >= 6); the whole gate demands the
    // baseline live inside the 2 GiB reserve band, so the row waits.
    let mut state = whole_state();
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 4.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        matches!(
            job.error,
            Some(SchedulerError::Admission(
                AdmissionError::WholeMachineBusy { .. }
            ))
        ),
        "the greedy floor must NOT stand in for the whole gate: {:?}",
        job.error
    );
    // The holderless refusal (the reviewer's B5R-02 case: a baseline
    // above the reserve, nothing to name) must print the target, so the
    // operator sees why 66.0 waited.
    let msg = job.error.as_ref().expect("refusal").to_string();
    assert!(
        msg.contains("at least 68.0 GiB free required") && msg.contains("free 66.0 GiB"),
        "{msg}"
    );
    // Leg 2: a baseline the reserve covers is a fully-free card.
    let mut state = whole_state();
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 2.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !job.terminal,
        "the reserve-covered card admits the greedy whole row: {:?}",
        job.error
    );
}

fn job(id: u64, kind: JobKind, model: &str, error: Option<&str>, outcome: &str) -> JobStatus {
    JobStatus {
        id: JobId(id),
        kind,
        // The rung is only meaningful with a real admission; these
        // fixtures exercise the fault rule.
        path: None,
        model: model.to_string(),
        category: CategoryId::Work,
        state: SwapState::Serving,
        terminal: true,
        outcome: Some(outcome.to_string()),
        error: error.map(str::to_string),
        // Recomputed here independently of `fault_of`, so a broken rule cannot
        // quietly make its own fixture consistent.
        is_fault: kind == JobKind::Load && error.is_some() && outcome != "cancelled",
        need_gib: None,
        free_at_admit_gib: None,
        observed_at_admit_gib: None,
        gate_target_gib: None,
        victims_released: Vec::new(),
        warnings: Vec::new(),
        timings: Default::default(),
    }
}

#[test]
fn only_a_failed_load_is_a_fault() {
    // The row dot and the headline share one rule: a supersede or
    // unload-during-load must not pin a red row (duplication review, 2026-10-26).
    let cancelled = job(
        7,
        JobKind::Load,
        "m",
        Some("cancelled by unload"),
        "cancelled",
    );
    assert!(!cancelled.is_fault, "a cancel is policy, not a fault");
    assert!(headline_fault(&[cancelled]).is_none());
    let failed = job(8, JobKind::Load, "m", Some("settle timeout"), "failed");
    assert!(failed.is_fault);
    assert!(headline_fault(&[failed]).is_some());
    let unload = job(9, JobKind::Unload, "m", Some("spawn failed"), "failed");
    assert!(
        !unload.is_fault,
        "unload failures live in the jobs strip only"
    );
}

fn failed(id: u64, model: &str) -> JobStatus {
    job(
        id,
        JobKind::Load,
        model,
        Some("engine not ready"),
        "engine not ready",
    )
}

fn ok(id: u64, model: &str) -> JobStatus {
    job(id, JobKind::Load, model, None, "loaded")
}

#[test]
fn newest_failure_wins() {
    let jobs = vec![failed(1, "a"), failed(2, "b")];
    assert_eq!(headline_fault(&jobs).expect("fault").id, JobId(2));
}

#[test]
fn cancels_and_unloads_are_not_faults() {
    // Review findings 1/2: an operator cancel (T8 unload-during-load, supersede)
    // and a failed unload never take the headline away from a real load failure.
    let jobs = vec![
        failed(1, "a"),
        job(
            2,
            JobKind::Load,
            "b",
            Some("cancelled: superseded"),
            "cancelled",
        ),
        job(3, JobKind::Unload, "c", Some("kill refused"), "unloaded"),
    ];
    assert_eq!(headline_fault(&jobs).expect("real fault").id, JobId(1));
}

#[test]
fn only_a_later_same_model_success_clears() {
    assert!(headline_fault(&[failed(1, "a")]).is_some());
    assert!(
        headline_fault(&[failed(1, "a"), ok(2, "b")]).is_some(),
        "a different model's success does not mask the fault"
    );
    assert!(
        headline_fault(&[
            failed(1, "a"),
            job(2, JobKind::Unload, "a", None, "unloaded")
        ])
        .is_some(),
        "an unload never clears"
    );
    assert!(
        headline_fault(&[failed(1, "a"), ok(2, "a")]).is_none(),
        "the same model+category loading successfully clears"
    );
    assert_eq!(
        headline_fault(&[failed(1, "a"), ok(2, "a"), failed(3, "a")])
            .expect("re-faulted")
            .id,
        JobId(3)
    );
}

/// Test double for the R11 fit probe: records every (program, argv) it is
/// asked to run and answers from two canned queues — one per `--version`
/// call, one per row call, in arrival order; an exhausted queue answers
/// `None` (the tool did not answer). It NEVER spawns: `cargo test` must
/// never exec a real `llama-fit-params.exe`, and a load reaching this
/// runtime is a test bug.
struct FitProbeRuntime {
    calls: Mutex<Vec<(String, Vec<String>)>>,
    version: Mutex<std::collections::VecDeque<Option<String>>>,
    row: Mutex<std::collections::VecDeque<Option<String>>>,
}

impl FitProbeRuntime {
    fn new(version: Vec<Option<String>>, row: Vec<Option<String>>) -> Self {
        FitProbeRuntime {
            calls: Mutex::new(Vec::new()),
            version: Mutex::new(version.into()),
            row: Mutex::new(row.into()),
        }
    }
}

#[async_trait::async_trait]
impl crate::engine::EngineRuntime for FitProbeRuntime {
    async fn spawn(
        &self,
        _spec: &crate::engine::LaunchSpec,
        _ticket: crate::ownership::SpawnTicket,
    ) -> Result<crate::engine::Spawned, crate::engine::SpawnError> {
        panic!("the R11 fit pin must never load");
    }
    async fn kill(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<(), crate::engine::SpawnError> {
        panic!("the R11 fit pin must never load");
    }
    async fn release_confirmed(
        &self,
        _ticket: &crate::ownership::SpawnTicket,
    ) -> Result<bool, crate::engine::SpawnError> {
        panic!("the R11 fit pin must never load");
    }
    async fn probe(
        &self,
        _port: u16,
        _readiness: &crate::engine::ReadinessSpec,
    ) -> crate::engine::Probe {
        panic!("the R11 fit pin must never load");
    }
    async fn discover_model_id(&self, _port: u16) -> Option<String> {
        panic!("the R11 fit pin must never load");
    }
    async fn run_probe_command(
        &self,
        program: &std::path::Path,
        args: &[String],
        _timeout: Duration,
    ) -> Option<String> {
        self.calls
            .lock()
            .expect("calls lock")
            .push((program.display().to_string(), args.to_vec()));
        let queue = if args == crate::engine::fit::version_args() {
            &self.version
        } else {
            &self.row
        };
        let mut answers = queue.lock().expect("queue lock");
        answers.pop_front().unwrap_or(None)
    }
}

/// The live probe's captured answer (Search#40 §2): the same fixture the
/// parser pins, so the end-to-end expectation is the ruling's own number.
const FIT_CAPTURED: &str = "CUDA0 21484 8853 1380\nHost 994 0 296\n";

/// Config with the probe switched on (margin 0.5, no surcharge — this row
/// carries no mmproj) and the tool path under `tool_dir`.
fn fit_config(tool_dir: &std::path::Path) -> crate::config::AppConfig {
    let mut cfg = crate::config::AppConfig::parse(
        r#"{"engines": {"llama": {"fit_probe": true, "fit_margin_gb": 0.5}}}"#,
    )
    .expect("valid");
    cfg.llama_server_exe = tool_dir.join("llama-server.exe");
    cfg
}

/// The config sibling must EXIST for a pass to start (absence is a named
/// warn, not a silent skip): a zero-byte stand-in is enough, the mock
/// runtime is what answers, the file is only ever stat'ed.
fn seed_fit_tools(dir: &std::path::Path) {
    std::fs::write(dir.join("llama-server.exe"), b"MZ").expect("server stand-in");
    std::fs::write(dir.join("llama-fit-params.exe"), b"MZ").expect("tool stand-in");
}

// ---------------------------------------------------------------------------
// The RAM ladder (operator ruling 2026-11-05): CPU-resident rows admit on
// host RAM and never touch the card; the two ledgers keep their rows apart.
// ---------------------------------------------------------------------------

/// The never-answers host witness, the RAM mirror of `LedgerOnlyWitness` in
/// shape — but not in privilege: RAM has no ledger mode, its `None` refuses
/// fail-closed (DOCTRINE 4).
struct SilentRamWitness;

impl crate::witness::RamWitness for SilentRamWitness {
    fn sample(&self) -> Option<crate::witness::RamSample> {
        None
    }
}

/// Slots for the RAM pins: an empty WORK slot and an empty AUX slot.
fn ram_slots() -> Vec<Slot> {
    vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_130,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_131,
            occupant: None,
            job: None,
        },
    ]
}

/// The CPU-resident fixture row: llama (the one engine whose knob table reads
/// `cpu_offload`), the truth-set word, and a declared footprint — read back
/// through the real parser like every other pin.
fn write_cpu_row_yaml(dir: &std::path::Path, name: &str, ram_gb: &str) -> std::path::PathBuf {
    let yaml = dir.join(format!("{name}.yaml"));
    std::fs::write(
        &yaml,
        format!("engine: llama\ncategory: work\nram_gb: {ram_gb}\ncpu_offload: true\n"),
    )
    .expect("seed");
    yaml
}

/// Falsifier (1): a CPU-resident row with ample host RAM admits on its own
/// ladder, books ZERO VRAM, and the next GPU admit sees the card exactly as
/// if it had never been asked (operator ruling 2026-11-05).
/// Falsifier: route a Ram row through the VRAM leg (need 10, booked as VRAM)
/// and the second leg's `free_at_admit` drops by 10 — pin goes red.
#[tokio::test]
async fn a_cpu_row_admits_on_ram_and_books_no_vram() {
    let dir = crate::testutil::temp_dir("ram-admit");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cpu_yaml = write_cpu_row_yaml(&dir, "cpu", "10");
    let gpu_yaml = dir.join("gpu.yaml");
    std::fs::write(&gpu_yaml, "engine: mock\ncategory: aux\nvram_gb: 12\n").expect("seed");
    let mut state = fold_state(&dir);
    state.adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let cpu = rescan_entry(&cpu_yaml, "cpu");
    let gpu = rescan_entry(&gpu_yaml, "gpu");
    assert_eq!(cpu.resource(), crate::model::Resource::Ram);
    state.catalog = vec![cpu.clone(), gpu.clone()];
    state.slots = ram_slots();
    let job_id = state.register_queued_load(0, &cpu, Vec::new());
    state.start_queued_load(job_id, 0, cpu.clone());
    assert_eq!(
        state.jobs[&job_id].need,
        Some(Gibibytes(0.0)),
        "a RAM row books zero VRAM"
    );
    assert_eq!(state.jobs[&job_id].ram_need, Some(Gibibytes(10.0)));
    state.finish_admit(
        job_id,
        None,
        Some(crate::witness::RamSample {
            total_gib: 64.0,
            used_gib: 20.0,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(job.error.is_none(), "{:?}", job.error);
    assert_eq!(job.path, Some(AdmissionPath::FitsNow));
    assert_eq!(
        job.state,
        SwapState::Spawn,
        "the admitted tail ran to the spawn commit"
    );
    assert_eq!(
        state.ledger_sum(),
        Gibibytes(0.0),
        "no VRAM was ever debited"
    );
    // The next GPU admit sees the card untouched: the same sample, the same
    // figures — 72 total, 2 reserve, 30 used, nothing else on the ledger or
    // in flight (the RAM row spends none of the card's bytes).
    let gpu_id = state.register_queued_load(1, &gpu, Vec::new());
    state.start_queued_load(gpu_id, 1, gpu.clone());
    state.finish_admit_card(
        gpu_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 30.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let gpu_job = &state.jobs[&gpu_id];
    assert!(gpu_job.error.is_none(), "{:?}", gpu_job.error);
    assert_eq!(
        gpu_job.free_at_admit,
        Some(Gibibytes(40.0)),
        "the CPU row must not have spent a byte of the card"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Falsifier (2): a CPU row over the RAM floor refuses naming its need,
/// free, reserve and blockers with the standard wait fields — the same
/// contract `Insufficient` has, judged on host RAM (operator ruling
/// 2026-11-05). The named blocker here is a GPU resident: its host RAM
/// rides the witness and is never under the RAM gate's control.
#[tokio::test]
async fn a_cpu_row_over_the_ram_floor_refuses_naming_its_numbers() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("ram-refuse");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cpu_yaml = write_cpu_row_yaml(&dir, "cpu", "10");
    let mut state = fold_state(&dir);
    let cpu = rescan_entry(&cpu_yaml, "cpu");
    state.catalog = vec![cpu.clone()];
    let gpu_ticket = fold_ticket(18_131);
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_130,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_131,
            occupant: Some(Occupant {
                model: "gpu-resident".to_string(),
                need: Gibibytes(6.0),
                ram_need: None,
                port: 18_131,
                ticket: gpu_ticket.clone(),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &cpu, Vec::new());
    state.start_queued_load(job_id, 0, cpu.clone());
    // free = 64 - 8 (reserve) - 50 (used) = 6 < need 10.
    state.finish_admit(
        job_id,
        None,
        Some(crate::witness::RamSample {
            total_gib: 64.0,
            used_gib: 50.0,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(job.terminal, "the refusal terminalizes the row");
    let error = job.error.as_ref().expect("the refusal rides the row");
    let SchedulerError::Admission(AdmissionError::InsufficientRam {
        need,
        free,
        reserve,
        blockers,
        wait_hint,
        victims_considered,
        ..
    }) = error
    else {
        panic!("the RAM refusal has its own class: {error:?}");
    };
    assert_eq!(
        (*need, *free, *reserve),
        (Gibibytes(10.0), Gibibytes(6.0), Gibibytes(8.0))
    );
    let msg = error.to_string();
    assert!(
        msg.contains("need 10.0 GiB")
            && msg.contains("free 6.0 GiB")
            && msg.contains("reserve 8.0 GiB"),
        "the refusal quotes the numbers it compared: {msg}"
    );
    assert!(
        blockers
            .iter()
            .any(|b| b.model == "gpu-resident" && b.reason.contains("GPU")),
        "the GPU resident's host RAM is a named blocker: {blockers:?}"
    );
    assert!(
        victims_considered.is_empty(),
        "the RAM gate plans no eviction: {victims_considered:?}"
    );
    assert_eq!(wait_hint.queue_position, None, "{wait_hint:?}");
    assert_eq!(wait_hint.expected_release_s, None, "{wait_hint:?}");
    assert!(
        !msg.contains("card holds"),
        "a RAM refusal never blames the card: {msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Falsifier (3/4, the VRAM side): the card's victim planner never considers
/// — and never even blames — a RAM resident, even when its category would
/// be evictable by the requesting one. A RAM occupant holds no VRAM to take;
/// the refusal is a plain shortage with an empty candidate set (operator
/// ruling 2026-11-05).
#[tokio::test]
async fn the_vram_planner_never_considers_or_blames_a_ram_occupant() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("vram-planner-skip");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let gpu_yaml = dir.join("gpu.yaml");
    std::fs::write(&gpu_yaml, "engine: mock\ncategory: work\nvram_gb: 40\n").expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let gpu = rescan_entry(&gpu_yaml, "gpu");
    state.catalog = vec![gpu.clone()];
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_130,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_131,
            occupant: Some(Occupant {
                model: "aux-cpu".to_string(),
                need: Gibibytes(0.0),
                ram_need: Some(Gibibytes(10.0)),
                port: 18_131,
                ticket: fold_ticket(18_131),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &gpu, Vec::new());
    state.start_queued_load(job_id, 0, gpu.clone());
    // free = 72 - 2 - 40 = 30 < need 40: a 10 GiB deficit, and the only
    // other holder is the RAM resident — the planner must find nothing.
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 40.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    let error = job.error.as_ref().expect("the shortage refuses");
    assert!(
        matches!(
            error,
            SchedulerError::Admission(AdmissionError::Insufficient { .. })
        ),
        "a VRAM shortage stays the VRAM class: {error:?}"
    );
    let msg = error.to_string();
    assert!(
        !msg.contains("aux-cpu"),
        "the RAM resident is neither victim nor blocker: {msg}"
    );
    match error {
        SchedulerError::Admission(AdmissionError::Insufficient {
            victims_considered,
            blockers,
            ..
        }) => {
            assert!(
                victims_considered.is_empty(),
                "no VRAM candidate existed: {victims_considered:?}"
            );
            assert!(
                blockers.is_empty(),
                "no VRAM holder existed to blame: {blockers:?}"
            );
        }
        _ => unreachable!("checked above"),
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Falsifier (5): a `whole` GPU request is never blocked by a RAM resident,
/// and a RAM resident never enters its holder list — the whole-machine gate
/// is a card posture (operator ruling 2026-11-05).
#[tokio::test]
async fn a_whole_gpu_row_never_sees_a_ram_resident_as_a_holder() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("whole-ram-holder");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let yaml = dir.join("whole.yaml");
    std::fs::write(
        &yaml,
        "engine: mock\ncategory: work\nvram_gb: 12\nlaunch_only_with_at_least_vram_free: whole\n",
    )
    .expect("seed");
    let mut state = fold_state(&dir);
    state.cfg.vram_total_gb = 72.0;
    state.cfg.vram_reserve_gb = 2.0;
    let entry = rescan_entry(&yaml, "whole");
    state.catalog = vec![entry.clone()];
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_130,
            occupant: None,
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_131,
            occupant: Some(Occupant {
                model: "aux-cpu".to_string(),
                need: Gibibytes(0.0),
                ram_need: Some(Gibibytes(10.0)),
                port: 18_131,
                ticket: fold_ticket(18_131),
                loaded_at: Instant::now(),
                launched_at_wall: std::time::SystemTime::now(),
                spawn_config: None,
                stale_config: false,
                serve_id: None,
                measure: None,
            }),
            job: None,
        },
    ];
    let job_id = state.register_queued_load(0, &entry, Vec::new());
    state.start_queued_load(job_id, 0, entry.clone());
    state.finish_admit_card(
        job_id,
        Ok(crate::budget::MemorySample {
            total_gib: 72.0,
            used_gib: 0.0,
            processes: Vec::new(),
            attribution: crate::budget::AttributionQuality::Unavailable,
        }),
    );
    let job = &state.jobs[&job_id];
    assert!(
        !matches!(
            job.error,
            Some(SchedulerError::Admission(
                AdmissionError::WholeMachineBusy { .. }
            ))
        ),
        "the RAM resident must not block the whole gate: {:?}",
        job.error
    );
    assert!(job.error.is_none(), "{:?}", job.error);
    assert_eq!(job.path, Some(AdmissionPath::FitsNow));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Falsifier (8): the whole dispatched path fails closed when the host
/// witness never answers — the Admit task sends no card sample for a RAM row
/// at all, a silent host witness answers `None`, and the gate refuses
/// without guessing (DOCTRINE 4, operator ruling 2026-11-05).
#[tokio::test]
async fn a_cpu_row_refuses_closed_when_the_host_witness_is_silent() {
    use super::{AdmissionError, SchedulerError};
    let dir = crate::testutil::temp_dir("ram-witness-down");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let cpu_yaml = write_cpu_row_yaml(&dir, "cpu", "10");
    let (tx, mut rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters = BTreeMap::new();
    adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir.to_path_buf(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    state.ram_witness = Arc::new(SilentRamWitness);
    let cpu = rescan_entry(&cpu_yaml, "cpu");
    state.catalog = vec![cpu.clone()];
    state.slots = ram_slots();
    let job_id = state.register_queued_load(0, &cpu, Vec::new());
    state.start_queued_load(job_id, 0, cpu.clone());
    let msg = rx
        .recv()
        .await
        .expect("the admit task dispatched its reading");
    match msg {
        ActorMsg::Event { job, event } => match event {
            super::Event::Admitted(card, ram) => {
                assert!(card.is_none(), "a RAM row never samples the card");
                assert!(ram.is_none(), "the silent host witness answers nothing");
                state.apply_event(job, super::Event::Admitted(card, ram));
            }
            other => panic!("the admit task sends its sample: {other:?}"),
        },
        _ => panic!("expected an event"),
    }
    let job = &state.jobs[&job_id];
    assert!(
        job.terminal,
        "a missing host reading is a refusal, never a wait"
    );
    let error = job.error.as_ref().expect("the refusal rides the row");
    assert!(
        matches!(
            error,
            SchedulerError::Admission(AdmissionError::WitnessUnavailable { reason })
                if reason.contains("ram witness")
        ),
        "the refusal names the RAM witness: {error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Falsifier (10): the two ledgers keep their rows apart — the VRAM sum
/// never counts a RAM resident or a RAM row's booking, and the RAM sums
/// never count a GPU resident or a GPU row's (operator ruling 2026-11-05).
#[test]
fn the_two_ledgers_keep_their_rows_apart() {
    let dir = crate::testutil::temp_dir("two-ledgers");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fold_state(&dir);
    let seat = |model: &str, need: f64, ram_need: Option<f64>, port: u16| Occupant {
        model: model.to_string(),
        need: Gibibytes(need),
        ram_need: ram_need.map(Gibibytes),
        port,
        ticket: fold_ticket(port),
        loaded_at: Instant::now(),
        launched_at_wall: std::time::SystemTime::now(),
        spawn_config: None,
        stale_config: false,
        serve_id: None,
        measure: None,
    };
    state.slots = vec![
        Slot {
            category: CategoryId::Work,
            index: 0,
            engine_port: 18_130,
            occupant: Some(seat("gpu-r", 12.0, None, 18_130)),
            job: None,
        },
        Slot {
            category: CategoryId::Aux,
            index: 1,
            engine_port: 18_131,
            occupant: Some(seat("cpu-r", 0.0, Some(10.0), 18_131)),
            job: None,
        },
    ];
    assert_eq!(
        state.ledger_sum(),
        Gibibytes(12.0),
        "the RAM resident holds no card bytes"
    );
    assert_eq!(
        state.ram_ledger_sum(),
        Gibibytes(10.0),
        "the GPU resident is not a RAM booking"
    );
    // The in-flight half: one admitted-not-installed row on each ladder.
    let gpu_entry = ModelEntry::from_flat(
        &BTreeMap::from([
            ("engine".to_string(), "mock".to_string()),
            ("vram_gb".to_string(), "12".to_string()),
        ]),
        "gpu-load",
    )
    .expect("valid");
    let cpu_entry = ModelEntry::from_flat(
        &BTreeMap::from([
            ("engine".to_string(), "llama".to_string()),
            ("cpu_offload".to_string(), "true".to_string()),
            ("ram_gb".to_string(), "5".to_string()),
        ]),
        "cpu-load",
    )
    .expect("valid");
    let gpu_row = state.register_queued_load(0, &gpu_entry, Vec::new());
    state.jobs.get_mut(&gpu_row).expect("row").need = Some(Gibibytes(12.0));
    state.step(gpu_row, SwapState::Admit);
    let cpu_row = state.register_queued_load(1, &cpu_entry, Vec::new());
    state.jobs.get_mut(&cpu_row).expect("row").need = Some(Gibibytes(0.0));
    state.jobs.get_mut(&cpu_row).expect("row").ram_need = Some(Gibibytes(5.0));
    state.step(cpu_row, SwapState::Admit);
    assert_eq!(
        state.in_flight_need(cpu_row),
        12.0,
        "the card leg sees only the card row"
    );
    assert_eq!(
        state.ram_in_flight_need(cpu_row),
        0.0,
        "the other row is the RAM leg's own"
    );
    assert_eq!(
        state.in_flight_need(gpu_row),
        0.0,
        "the card leg never spends the RAM row"
    );
    assert_eq!(
        state.ram_in_flight_need(gpu_row),
        5.0,
        "the RAM leg sees only the RAM row"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The same-slot RAM credit and the in-flight RAM debit, each counted once
/// (operator ruling 2026-11-05): raw free 64 - 8 - 22 = 34, +10 the resident
/// this very replacement releases, -5 a RAM load already admitted elsewhere
/// — a row needing exactly 39 admits on the line, and the same host reading
/// refuses a row needing 40.
/// Falsifier: double-counting the credit admits the 40-row; missing the
/// in-flight debit admits it too; missing the credit refuses the 39-row.
#[tokio::test]
async fn a_ram_replacement_credits_its_own_slot_once_and_pays_other_ram_loads() {
    let dir = crate::testutil::temp_dir("ram-credit");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let build = |name: &str, ram_gb: &str| {
        let yaml = write_cpu_row_yaml(&dir, name, ram_gb);
        rescan_entry(&yaml, name)
    };
    let seat_state = |entry: &ModelEntry| {
        let mut state = fold_state(&dir);
        state.adapters.insert(
            "llama".to_string(),
            Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
        );
        state.catalog = vec![entry.clone()];
        state.slots = vec![
            Slot {
                category: CategoryId::Work,
                index: 0,
                engine_port: 18_130,
                occupant: Some(Occupant {
                    model: "old-cpu".to_string(),
                    need: Gibibytes(0.0),
                    ram_need: Some(Gibibytes(10.0)),
                    port: 18_130,
                    ticket: fold_ticket(18_130),
                    loaded_at: Instant::now(),
                    launched_at_wall: std::time::SystemTime::now(),
                    spawn_config: None,
                    stale_config: false,
                    serve_id: None,
                    measure: None,
                }),
                job: None,
            },
            Slot {
                category: CategoryId::Aux,
                index: 1,
                engine_port: 18_131,
                occupant: None,
                job: None,
            },
        ];
        // An admitted-not-installed RAM load elsewhere: its 5 GiB is spent.
        let other = build("other-cpu", "5");
        state.catalog.push(other.clone());
        let other_row = state.register_queued_load(1, &other, Vec::new());
        state.jobs.get_mut(&other_row).expect("row").need = Some(Gibibytes(0.0));
        state.jobs.get_mut(&other_row).expect("row").ram_need = Some(Gibibytes(5.0));
        state.step(other_row, SwapState::Admit);
        state
    };
    let fits = build("fits", "39");
    let state = &mut seat_state(&fits);
    let job_id = state.register_queued_load(0, &fits, Vec::new());
    state.start_queued_load(job_id, 0, fits.clone());
    state.finish_admit(
        job_id,
        None,
        Some(crate::witness::RamSample {
            total_gib: 64.0,
            used_gib: 22.0,
        }),
    );
    assert!(
        state.jobs[&job_id].error.is_none(),
        "{:?}",
        state.jobs[&job_id].error
    );
    assert_eq!(state.jobs[&job_id].path, Some(AdmissionPath::FitsNow));
    let over = build("over", "40");
    let state = &mut seat_state(&over);
    let job_id = state.register_queued_load(0, &over, Vec::new());
    state.start_queued_load(job_id, 0, over.clone());
    state.finish_admit(
        job_id,
        None,
        Some(crate::witness::RamSample {
            total_gib: 64.0,
            used_gib: 22.0,
        }),
    );
    let msg = state.jobs[&job_id]
        .error
        .as_ref()
        .expect("40 > the 39 the host honestly has")
        .to_string();
    assert!(
        msg.contains("free 39.0 GiB"),
        "credit and debit counted once: {msg}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The RAM ladder's no-basis rule: a CPU row with neither `ram_gb` nor a
/// GGUF size is under-specified, not guessed at (the estimate exists only
/// where the weights file's size is known).
#[test]
fn a_ram_row_without_any_basis_is_under_specified() {
    use super::AdmissionError;
    let entry = ModelEntry::from_flat(
        &BTreeMap::from([
            ("engine".to_string(), "llama".to_string()),
            ("cpu_offload".to_string(), "true".to_string()),
        ]),
        "cpu",
    )
    .expect("valid");
    assert_eq!(entry.resource(), crate::model::Resource::Ram);
    assert!(
        matches!(
            super::ActorState::ram_booking(&entry),
            Err(AdmissionError::UnderSpecified)
        ),
        "no `ram_gb`, no GGUF size: the gate refuses rather than guess"
    );
    let sized = ModelEntry {
        gguf_size_gb: Some(20.0),
        ..entry
    };
    assert_eq!(
        super::ActorState::ram_booking(&sized).expect("the estimate has a basis"),
        Gibibytes(22.0),
        "the weights page in, plus the working set: size x 1.0 + 2"
    );
}

fn fit_llama_entry(dir: &std::path::Path, name: &str) -> ModelEntry {
    ModelEntry::from_flat(
        &BTreeMap::from([
            ("name".to_string(), name.to_string()),
            ("engine".to_string(), "llama".to_string()),
            ("category".to_string(), "work".to_string()),
            ("vram_gb".to_string(), "24".to_string()),
            (
                "model".to_string(),
                dir.join("turbo.gguf").display().to_string(),
            ),
            ("context".to_string(), "4096".to_string()),
            ("ngl".to_string(), "99".to_string()),
        ]),
        name,
    )
    .expect("valid row")
}

/// State carrying one fit-capable row and the live calibration, never
/// loading anything.
fn fit_state(
    cfg: crate::config::AppConfig,
    runtime: Arc<dyn crate::engine::EngineRuntime>,
) -> ActorState {
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    ActorState::empty(
        cfg,
        crate::testutil::temp_dir("fit-state"),
        adapters,
        runtime,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    )
}

fn warns_holding(capture: &Arc<Mutex<Vec<CapturedWarn>>>, needle: &str) -> usize {
    capture
        .lock()
        .expect("capture lock")
        .iter()
        .filter(|(message, fields)| {
            message.contains(needle) || fields.values().any(|value| value.contains(needle))
        })
        .count()
}

#[test]
fn a_default_build_emits_zero_fit_probes() {
    // The inert default is total silence: no plan, no queue, nothing to
    // cancel — the live build ships before the operator's card window with
    // zero surprise probes.
    let dir = crate::testutil::temp_dir("fit-inert");
    let mut state = fit_state(
        crate::config::AppConfig::default(),
        Arc::new(FitProbeRuntime::new(Vec::new(), Vec::new()))
            as Arc<dyn crate::engine::EngineRuntime>,
    );
    state.catalog = vec![fit_llama_entry(&dir, "inert-row")];
    assert!(
        state.fit_probe_plan().is_none(),
        "probe off is no plan at all"
    );
    let mut entry = fit_llama_entry(&dir, "inert-row");
    entry.fit_gb = None;
    state.refresh_fit_on_admit(&mut entry);
    assert_eq!(
        entry.fit_gb, None,
        "the ladder is untouched by a disabled pass"
    );
}

/// A fold pin's state carrying the live Fit calibration (margin 0.5): the
/// session cross-check's tolerance exists only because this config names it.
fn fit_fold_state(dir: &std::path::Path) -> ActorState {
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    ActorState::empty(
        fit_config(dir),
        dir.to_path_buf(),
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    )
}

/// A row the gate books from the engine's projection: declared floor 4 GiB,
/// projection 10 GiB (small enough for the witness pins' 24 GiB card), read
/// back through the real parser with its `file` set so the fold's persist
/// leg runs on real yaml.
fn fit_booked_entry(dir: &std::path::Path, name: &str) -> ModelEntry {
    let yaml = dir.join(format!("{name}.yaml"));
    std::fs::write(
        &yaml,
        format!("engine: llama\nvram_gb: 4\nmodel: {name}.gguf\ncontext: 4096\nngl: 99\n"),
    )
    .expect("seed yaml");
    let mut entry = rescan_entry(&yaml, name);
    entry.fit_gb = Some(10.0);
    assert_eq!(
        entry.cost_source(),
        crate::model::CostSource::Fit,
        "the pin must book from Fit, or the cross-check below proves nothing"
    );
    entry
}

/// Cross-check warns for ONE model: the capture is process-global and tests
/// run in parallel, so the model name is the quarantine.
fn fit_verdict_warns(capture: &Arc<Mutex<Vec<CapturedWarn>>>, name: &str, needle: &str) -> usize {
    capture
        .lock()
        .expect("capture lock")
        .iter()
        .filter(|(message, fields)| {
            message.contains(needle)
                && fields
                    .get("model")
                    .is_some_and(|m| m.trim_matches('"') == name)
        })
        .count()
}

/// R11 session cross-check: a witness reading that survived every fold gate
/// and lands inside the projection's session tolerance (margin 0.5 + slack)
/// stamps the row for the session — and the stamp stays in-process: the
/// folded measured pair is the only figure this reading ever writes.
/// Falsifier: drop the tolerance compare and the out-of-tolerance pin fails;
/// persist the stamp and the yaml leg fails.
#[tokio::test]
async fn a_witness_inside_the_fit_projection_stamps_the_row_for_the_session() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-green");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fit_fold_state(&dir);
    let entry = fit_booked_entry(&dir, "fitgreen-row");
    let yaml = entry.file.clone().expect("seeded file");
    let ticket = fold_ticket(19_301);
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 19_301);
    state.witness("fitgreen-row", &ticket, 11.0);
    assert!(
        state.snapshot().fit_validated.contains("fitgreen-row"),
        "11.0 sits 1.0 GiB from the projection, inside margin 0.5 plus slack"
    );
    assert_eq!(
        fit_verdict_warns(&capture, "fitgreen-row", "kept the Fit row amber"),
        0,
        "an in-tolerance stamp is silence"
    );
    await_yaml_keys(&yaml, &["measured_gb: 11.0"]).await;
    let persisted = std::fs::read_to_string(&yaml).expect("written yaml");
    for token in ["fit_validated", "fit_gb"] {
        assert!(
            !persisted.contains(token),
            "the verdict and the projection are in-process only; the yaml learned \
                 about {token}:\n{persisted}"
        );
    }
}

/// The refusal side, warn-once: the first out-of-tolerance reading names its
/// numbers exactly once per epoch (the operator needs them exactly once), a
/// second refusal adds no noise, and a later reading back inside tolerance
/// still earns the stamp — with a warn naming the reversal.
#[tokio::test]
async fn an_out_of_tolerance_reading_warns_once_and_can_still_earn_the_stamp() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-warn");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fit_fold_state(&dir);
    let entry = fit_booked_entry(&dir, "fitwarn-row");
    let ticket = fold_ticket(19_302);
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 19_302);
    state.witness("fitwarn-row", &ticket, 20.0);
    assert!(
        !state.snapshot().fit_validated.contains("fitwarn-row"),
        "20.0 sits 10 GiB from the projection: the row stays amber"
    );
    assert_eq!(
        fit_verdict_warns(&capture, "fitwarn-row", "kept the Fit row amber"),
        1,
        "the first refusal names its numbers"
    );
    state.witness("fitwarn-row", &ticket, 21.0);
    assert_eq!(
        fit_verdict_warns(&capture, "fitwarn-row", "kept the Fit row amber"),
        1,
        "the same verdict repeating is silence, not a log flood"
    );
    state.witness("fitwarn-row", &ticket, 11.0);
    assert!(
        state.snapshot().fit_validated.contains("fitwarn-row"),
        "a later reading inside tolerance earns the stamp"
    );
    assert_eq!(
        fit_verdict_warns(&capture, "fitwarn-row", "fell back inside"),
        1,
        "the reversal is worth one warn"
    );
}

/// R3's `ForeignDominated` grade runs BEFORE the cross-check: memory that
/// moved across the window is someone else's story, so a total-diff reading
/// that the fold itself refuses must never stamp a projection green — the
/// green would vouch for a number the fold refused to mint.
#[tokio::test]
async fn a_contaminated_window_never_stamps_a_fit_row_green() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-contam");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fit_fold_state(&dir);
    let entry = fit_booked_entry(&dir, "fitcontam-row");
    let ticket = fold_ticket(19_303);
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 19_303);
    // own: None keeps this a total-diff (the only reading the grade guards).
    // At admit 52 GiB of the 72 GiB card was unaccounted; now 58 is: 6 GiB
    // moved while the engine grew 2 — foreign-dominated, refused outright.
    state.on_measured(
        "fitcontam-row",
        &ticket,
        super::Reading {
            total_diff: 2.0,
            own: None,
            total_gib: 72.0,
            used_gib: 60.0,
        },
    );
    assert!(
        !state.snapshot().fit_validated.contains("fitcontam-row"),
        "a contaminated window cannot mint a green"
    );
    assert_eq!(
        fit_verdict_warns(&capture, "fitcontam-row", "kept the Fit row amber"),
        0,
        "the guard refuses BEFORE any verdict is attempted"
    );
}

/// An uncalibrated margin is NO defined tolerance: the row stays amber with
/// total silence — a display cross-check may not nag about a tolerance that
/// was never configured.
#[tokio::test]
async fn an_uncalibrated_margin_never_validates_and_never_says_so() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-quiet");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fold_state(&dir);
    let entry = fit_booked_entry(&dir, "fitquiet-row");
    let ticket = fold_ticket(19_304);
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 19_304);
    state.witness("fitquiet-row", &ticket, 11.0);
    assert!(
        !state.snapshot().fit_validated.contains("fitquiet-row"),
        "no calibration, no tolerance, no green"
    );
    for needle in ["kept the Fit row amber", "fell back inside"] {
        assert_eq!(
            fit_verdict_warns(&capture, "fitquiet-row", needle),
            0,
            "silence, not a nag about an absent config key: {needle}"
        );
    }
}

/// TASK-R11-FIT pin: the session tolerance is exactly `fit_margin_gb` +
/// [`FIT_VALIDATE_SLACK_GIB`], that default slack is exactly 2.0 GiB, and
/// an absent or non-finite margin leaves NO defined tolerance (the row
/// never validates, silently). The C1 evidence figures live once, in the
/// calibration pin in `src/engine/fit.rs`.
#[test]
fn the_fit_validation_tolerance_is_the_margin_plus_the_pinned_default_slack() {
    assert_eq!(
        super::FIT_VALIDATE_SLACK_GIB,
        2.0,
        "the calibrated slack default"
    );
    assert_eq!(
        ActorState::fit_validation_tolerance(Some(3.0)),
        Some(5.0),
        "tolerance = fit_margin_gb + slack, at the live margin 3"
    );
    assert_eq!(
        ActorState::fit_validation_tolerance(Some(0.5)),
        Some(2.5),
        "the same composition the fold pins exercise"
    );
    assert_eq!(
        ActorState::fit_validation_tolerance(None),
        None,
        "no margin, no tolerance: never-validate, silent"
    );
    assert!(
        ActorState::fit_validation_tolerance(Some(f64::NAN)).is_none()
            && ActorState::fit_validation_tolerance(Some(f64::INFINITY)).is_none(),
        "a nonsense margin is no tolerance either (the parse_gib doctrine)"
    );
}

/// The stamp's two retirements: a catalog rescan bumps the fit epoch (and
/// clears the map), and a calculator swap clears it outright — a green the
/// page showed described the superseded catalog or the old tool.
#[tokio::test]
async fn a_rescan_or_a_calculator_swap_retires_the_session_stamp() {
    let dir = crate::testutil::temp_dir("fit-retire");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut state = fit_fold_state(&dir);
    let entry = fit_booked_entry(&dir, "fitretire-row");
    let ticket = fold_ticket(19_305);
    state.catalog = vec![entry.clone()];
    seat_spawn(&mut state, &entry, &ticket, 19_305);
    state.on_fit_version(state.fit_gen(), Some("b11669".to_string()));
    state.witness("fitretire-row", &ticket, 11.0);
    assert!(state.snapshot().fit_validated.contains("fitretire-row"));
    state.spawn_fit_warm();
    assert!(
        state.snapshot().fit_validated.is_empty(),
        "the rescan's epoch retires every session stamp"
    );
    state.witness("fitretire-row", &ticket, 11.0);
    assert!(
        state.snapshot().fit_validated.contains("fitretire-row"),
        "the new epoch can earn a fresh stamp"
    );
    state.on_fit_version(state.fit_gen(), Some("b20000".to_string()));
    assert!(
        state.snapshot().fit_validated.is_empty(),
        "a replaced calculator voids verdicts earned under the old one"
    );
}

#[tokio::test]
async fn the_warm_pass_books_the_projection_and_never_touches_the_yaml() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-warm");
    seed_fit_tools(&dir);
    std::fs::write(dir.join("turbo.gguf"), b"weights").expect("model stand-in");
    let yaml = "name: fit-turbo\nengine: llama\ncategory: work\nvram_gb: 24\n\
                    model: turbo.gguf\ncontext: 4096\nngl: 99\n";
    std::fs::write(dir.join("fit-turbo.yaml"), yaml).expect("seed llama yaml");
    let probe = Arc::new(FitProbeRuntime::new(
        vec![Some("b11669".to_string())],
        vec![Some(FIT_CAPTURED.to_string())],
    ));
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "llama".to_string(),
        Arc::new(MockAdapter) as Arc<dyn EngineAdapter>,
    );
    let handle = Scheduler::start(
        fit_config(&dir),
        dir.clone(),
        adapters,
        probe.clone() as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
    );
    let expected = 31_717.0 / 1024.0 + 0.5;
    let deadline = Instant::now() + Duration::from_secs(20);
    let catalog = loop {
        let catalog = handle.catalog().await.expect("actor alive");
        if catalog
            .models
            .iter()
            .any(|m| m.name == "fit-turbo" && m.fit_gb.is_some())
        {
            break catalog;
        }
        assert!(
            Instant::now() < deadline,
            "the warm pass never landed a figure (warns: {:?})",
            capture.lock().expect("capture lock")
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    };
    let row = catalog
        .models
        .iter()
        .find(|m| m.name == "fit-turbo")
        .expect("the row is there");
    assert_eq!(
        row.fit_gb,
        Some(expected),
        "the captured device sum, Host excluded, + one active device's margin"
    );
    let calls = probe.calls.lock().expect("calls lock");
    assert_eq!(calls.len(), 2, "one version leg, one row leg: {calls:?}");
    let program = std::path::Path::new(&calls[0].0);
    assert_eq!(
        program.file_stem().expect("stem"),
        "llama-fit-params",
        "the probe is the config sibling, never the server itself"
    );
    assert_eq!(
        calls[0].1,
        crate::engine::fit::version_args(),
        "the epoch rides first"
    );
    assert_eq!(
        calls[1].1,
        vec![
            "-m",
            "turbo.gguf",
            "-c",
            "4096",
            "-ngl",
            "99",
            "--fit-print",
            "on",
        ],
        "only the memory-relevant flags, --fit-print last, nothing the server forces"
    );
    let row_args = calls[1].1.join(" ");
    for forbidden in ["--host", "--port", "--jinja", "--mmproj"] {
        assert!(
            !row_args.contains(forbidden),
            "fit-params rejects or ignores {forbidden}: {row_args}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.join("fit-turbo.yaml")).expect("yaml readable"),
        yaml,
        "the figure never reaches the operator's bytes: warm booking does not write"
    );
}

#[test]
fn a_replaced_calculator_voids_every_projection() {
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-epoch");
    let mut state = fit_state(
        fit_config(&dir),
        Arc::new(FitProbeRuntime::new(Vec::new(), Vec::new()))
            as Arc<dyn crate::engine::EngineRuntime>,
    );
    let mut row = fit_llama_entry(&dir, "epoch-row");
    row.fit_gb = Some(31.0);
    state.catalog = vec![row];
    state.fit_version = Some("b11669".to_string());
    state.fit_cache.insert(
        7,
        FitEntry {
            gib: 31.0,
            fingerprint: None,
        },
    );
    let gen = state.fit_gen();
    state.on_fit_version(gen, Some("b12000".to_string()));
    assert_eq!(
        state.fit_cache.len(),
        0,
        "a figure belongs to the tool that minted it"
    );
    assert_eq!(
        state.catalog[0].fit_gb, None,
        "a stripped row falls back to the ladder"
    );
    assert_eq!(state.fit_version.as_deref(), Some("b12000"));
    assert!(
        warns_holding(&capture, "the fit calculator changed under us") >= 1,
        "the void must be named, not silent"
    );
}

#[test]
fn an_unanswered_version_check_is_silence_about_this_pass_only() {
    // A momentarily absent tool is no evidence the tool CHANGED: the
    // established epoch and its figures stand, the pass simply goes quiet.
    let capture = install_warn_capture();
    let dir = crate::testutil::temp_dir("fit-silent");
    let mut state = fit_state(
        fit_config(&dir),
        Arc::new(FitProbeRuntime::new(Vec::new(), Vec::new()))
            as Arc<dyn crate::engine::EngineRuntime>,
    );
    let mut row = fit_llama_entry(&dir, "silent-row");
    row.fit_gb = Some(3.0);
    state.catalog = vec![row];
    state.fit_version = Some("b11669".to_string());
    let gen = state.fit_gen();
    state.on_fit_version(gen, None);
    assert_eq!(state.fit_version.as_deref(), Some("b11669"));
    assert_eq!(state.catalog[0].fit_gb, Some(3.0));
    assert!(warns_holding(&capture, "did not answer --version") >= 1);
}

#[test]
fn a_stale_epoch_fit_answer_is_dropped_at_the_door() {
    // A rescan bumps the epoch mid-probe: the answer describes a catalog
    // that no longer exists and may not patch anything.
    let dir = crate::testutil::temp_dir("fit-stale");
    let mut state = fit_state(
        fit_config(&dir),
        Arc::new(FitProbeRuntime::new(Vec::new(), Vec::new()))
            as Arc<dyn crate::engine::EngineRuntime>,
    );
    let entry = fit_llama_entry(&dir, "stale-row");
    let key = ActorState::fit_row_key(&entry).expect("probeable");
    state.catalog = vec![entry];
    let stale = state.fit_gen().wrapping_add(1);
    state.on_fit_row_done(stale, "stale-row", key, Ok((31.0, None)));
    assert_eq!(
        state.catalog[0].fit_gb, None,
        "the superseded epoch's figure is void"
    );
    assert_eq!(state.fit_cache.len(), 0, "and it never seeds the cache");
}

/// One WORK slot holding one occupant of `(port, generation)`, for the
/// serve-id fold-door pins (FINDINGS-11 [1]): fold answers land as direct
/// calls, so the never-called runtime is never reached.
fn serve_id_state(port: u16, generation: u64, serve_id: Option<&str>) -> ActorState {
    let BootWiring {
        adapters,
        runtime,
        witness,
        tx,
    } = boot_stub_wiring();
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        crate::testutil::temp_dir("serve-id-door"),
        adapters,
        runtime,
        witness,
        tx,
    );
    state.slots = vec![Slot {
        category: CategoryId::Work,
        index: 0,
        engine_port: port,
        occupant: Some(Occupant {
            model: format!("door-{generation}"),
            need: Gibibytes(2.0),
            ram_need: None,
            port,
            ticket: SpawnTicket {
                generation,
                kind: ResourceKind::ChildProcess,
                pid: 4242,
                port,
            },
            loaded_at: Instant::now(),
            launched_at_wall: std::time::SystemTime::now(),
            spawn_config: None,
            stale_config: false,
            serve_id: serve_id.map(str::to_string),
            measure: None,
        }),
        job: None,
    }];
    state
}

/// Silence re-discovery warns captured for one model name (FINDINGS-11 [2]).
fn silence_warns(capture: &Arc<Mutex<Vec<CapturedWarn>>>, model: &str) -> usize {
    capture
        .lock()
        .expect("capture lock")
        .iter()
        .filter(|(message, fields)| {
            message.contains("re-discovery still reports nothing")
                && fields.get("model").is_some_and(|name| name.contains(model))
        })
        .count()
}

#[test]
fn a_serve_id_answer_for_a_replaced_generation_is_dropped_at_the_door() {
    // FINDINGS-11 [1], the sibling of `a_stale_epoch_fit_answer_is_dropped_at_the_door`:
    // the occupant on this port was replaced mid-probe, so an answer naming
    // the dead generation describes an engine that no longer exists and may
    // not patch the one that does.
    let mut state = serve_id_state(19_111, 7, None);
    assert!(
        !state.on_serve_id_refreshed(19_111, 6, Some("engine-dead")),
        "an answer for a superseded spawn reports no change"
    );
    let occupant = state.slots[0]
        .occupant
        .as_ref()
        .expect("the current occupant stands");
    assert_eq!(
        occupant.serve_id, None,
        "a replaced generation's id is never laundered onto the current engine"
    );
}

#[test]
fn a_serve_id_answer_is_refused_over_an_occupant_that_already_has_one() {
    // FINDINGS-11 [1], the second guard: an id that landed at install (or
    // an earlier probe) is the engine's word for THIS spawn — a later
    // answer, however fresh, may not overwrite it, and a fold that
    // overwrites would report a publish for a change that never happened.
    let mut state = serve_id_state(19_112, 7, Some("engine-first"));
    assert!(
        !state.on_serve_id_refreshed(19_112, 7, Some("engine-second")),
        "installing over an occupant with an id is refused, not published"
    );
    let occupant = state.slots[0]
        .occupant
        .as_ref()
        .expect("the occupant keeps its seat");
    assert_eq!(
        occupant.serve_id.as_deref(),
        Some("engine-first"),
        "the already-held id is never replaced"
    );
}

#[test]
fn silence_warns_once_per_spawn_and_a_changed_outcome_arms_the_warn_again() {
    // FINDINGS-11 [2], mirroring `the_reconcile_warns_once_and_an_unchanged_set_never_re_warns`:
    // every resolve of an id-less occupant re-runs discovery, so a client
    // hammering a permanently silent engine used to log one warn per landed
    // silence (~0.5/s/slot forever). The warn marks the CHANGE into silence.
    let capture = install_warn_capture();
    // Warm the reporting callsite and PROVE delivery before the measured
    // window (the D9 rule): a sentinel name no assertion below reads about.
    let mut warm = serve_id_state(19_113, 1, None);
    assert!(!warm.on_serve_id_refreshed(19_113, 1, None));
    assert_eq!(
        silence_warns(&capture, "door-1"),
        1,
        "the reporting callsite was warmed but never delivered"
    );

    let mut state = serve_id_state(19_114, 5, None);
    for _ in 0..3 {
        assert!(
            !state.on_serve_id_refreshed(19_114, 5, None),
            "silence installs nothing and publishes nothing"
        );
    }
    assert_eq!(
        silence_warns(&capture, "door-5"),
        1,
        "a hammered silence must name the unchanged state exactly once"
    );

    // A landed id is a NEW outcome: the silence state ends, so a return to
    // silence is news again and may name itself once more.
    assert!(state.on_serve_id_refreshed(19_114, 5, Some("engine-x")));
    assert!(!state.on_serve_id_refreshed(19_114, 5, None));
    assert_eq!(
        silence_warns(&capture, "door-5"),
        2,
        "silence that ended and returned is a changed state, not the old one"
    );

    // A new spawn on the same port is a new silence state: first warn again.
    {
        let occupant = state.slots[0]
            .occupant
            .as_mut()
            .expect("the occupant still holds its seat");
        occupant.ticket.generation = 6;
        occupant.serve_id = None;
    }
    assert!(!state.on_serve_id_refreshed(19_114, 6, None));
    assert_eq!(
        silence_warns(&capture, "door-5"),
        3,
        "the next generation is a fresh silence state and warns once on its own"
    );
}

#[tokio::test]
async fn an_admit_hit_books_the_projection_a_proven_change_voids_it() {
    let dir = crate::testutil::temp_dir("fit-admit");
    seed_fit_tools(&dir);
    let probe = Arc::new(FitProbeRuntime::new(Vec::new(), Vec::new()));
    let mut state = fit_state(
        fit_config(&dir),
        Arc::clone(&probe) as Arc<dyn crate::engine::EngineRuntime>,
    );
    let entry = fit_llama_entry(&dir, "admit-row");
    let key = ActorState::fit_row_key(&entry).expect("probeable");
    state.catalog = vec![entry.clone()];

    // Unstat'able model file (both sides absent): no evidence, figure stands.
    state.fit_cache.insert(
        key,
        FitEntry {
            gib: 31.0,
            fingerprint: None,
        },
    );
    let mut probe_row = entry.clone();
    state.refresh_fit_on_admit(&mut probe_row);
    assert_eq!(probe_row.fit_gb, Some(31.0), "same argv, same tool, KEEP");
    assert_eq!(
        state.catalog[0].fit_gb,
        Some(31.0),
        "the page reads the figure the admit booked"
    );

    // The ruled lock is "version checked at boot AND launch": a booked
    // hit is no exemption — the admit still asks the calculator its
    // version, fire-and-forget (this admit never parks behind it).
    let mut version_checked = false;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let first_call = probe.calls.lock().expect("calls lock").first().cloned();
        if let Some((_, args)) = first_call {
            assert_eq!(
                args,
                crate::engine::fit::version_args(),
                "the FIRST thing a hit admit asks is the tool identity"
            );
            version_checked = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        version_checked,
        "a cache-hit admit never checked the version"
    );

    // A PROVEN model-file change (both fingerprints known, differing) is the
    // only thing that voids the hit — and this admit books the ladder now
    // while the refresh re-projects.
    std::fs::write(dir.join("turbo.gguf"), b"weights").expect("model file");
    let fresh = crate::model::fingerprint(&dir.join("turbo.gguf")).expect("stat");
    let stale_stamp = crate::model::FileFingerprint {
        mtime: fresh.mtime + std::time::Duration::from_secs(60),
        size: fresh.size + 1,
    };
    state.fit_cache.insert(
        key,
        FitEntry {
            gib: 31.0,
            fingerprint: Some(stale_stamp),
        },
    );
    let mut changed_row = entry.clone();
    changed_row.fit_gb = Some(31.0);
    state.refresh_fit_on_admit(&mut changed_row);
    assert_eq!(
        changed_row.fit_gb, None,
        "a rewritten model file is a proven change: the current admit books the ladder"
    );
}

#[test]
fn booking_need_applies_the_max_truth_including_the_greedy_floor() {
    let mut row = fit_llama_entry(std::path::Path::new("."), "booked");
    row.fit_gb = Some(31.0);
    assert_eq!(
        ActorState::booking_need(&row),
        Ok(Some(Gibibytes(31.0))),
        "the projection overshadows the smaller declared floor"
    );
    row.vram_gb = Some(40.0);
    assert_eq!(
        ActorState::booking_need(&row),
        Ok(Some(Gibibytes(40.0))),
        "a declared floor above the projection stands"
    );
    row.vram_gb = None;
    row.gguf_size_gb = None;
    row.fit_gb = Some(31.0);
    row.cost = crate::model::CostClass::Greedy;
    assert_eq!(
        ActorState::booking_need(&row),
        Ok(Some(Gibibytes(31.0))),
        "greedy never wears the Fit class but its launch floor reads the figure"
    );
    row.engine = "ninfer".to_string();
    assert_eq!(
        ActorState::booking_need(&row),
        Ok(None),
        "a projection no other engine owns floors nothing"
    );
    // Operator ruling 2026-11-04 (`TASK-BASALT-GREEDY`): a greedy basalt
    // row is the legal unknown-nominal case too - `Ok(None)` books the
    // room at admit, and the gate judges the row's launch floor (60 on
    // the live row), the same floor-only path the strata spills proved.
    row.engine = "basalt".to_string();
    assert_eq!(
        ActorState::booking_need(&row),
        Ok(None),
        "a greedy basalt row without a nominal books the room at admit"
    );
}

/// The wait-estimate ring quotes exact medians: the middle sample for an odd
/// count, and the mean of the two middle samples for an even count — the
/// estimate a refusal carries is computed over the real set, never rounded
/// to one of its members. Falsifier: make the even branch answer
/// `sorted[mid]` and the last leg goes red.
#[test]
fn release_median_is_the_middle_sample_or_the_mean_of_the_two_middles() {
    let mut ring = super::ReleaseSamples::new();
    assert_eq!(
        ring.median_s(),
        None,
        "no confirmed release: no estimate, never an invented default"
    );
    for secs in [4.0, 1.0, 9.0, 3.0, 7.0] {
        ring.push(Duration::from_secs_f64(secs));
    }
    assert_eq!(ring.median_s(), Some(4.0), "odd count: middle of 1,3,4,7,9");
    ring.push(Duration::from_secs(2));
    assert_eq!(
        ring.median_s(),
        Some(3.5),
        "even count: mean of the two middles of 1,2,3,4,7,9"
    );
}

/// The ring is bounded: a push past the cap drops the oldest sample, and the
/// median then runs over the newest `RELEASE_SAMPLE_CAP` releases only — a
/// swap that stopped long ago must not anchor today's estimate forever.
/// Falsifier: delete the `pop_front` and both legs go red.
#[test]
fn the_release_ring_keeps_only_the_newest_samples() {
    let mut ring = super::ReleaseSamples::new();
    for secs in 1..=super::RELEASE_SAMPLE_CAP as u64 + 1 {
        ring.push(Duration::from_secs(secs));
    }
    assert_eq!(
        ring.seconds.len(),
        super::RELEASE_SAMPLE_CAP,
        "the 33rd sample must evict the first, not grow the ring"
    );
    // Kept: 2..=33 — even count, middles 17 and 18.
    assert_eq!(ring.median_s(), Some(17.5));
}

/// The queue rule as documented on `ActorState::wait_hint`: a one-based
/// place behind the loads really ahead in the refused category, and `None`
/// when nothing is ahead — the harness never quotes a place it did not
/// grant. Falsifiers: drop the `+ 1` and the Work leg goes red; drop the
/// category filter and the RP leg goes red; drop the `> 0` guard and the
/// no-ahead leg goes red.
#[test]
fn a_refusal_quotes_a_place_only_behind_real_loads() {
    let dir = crate::testutil::temp_dir("waithint");
    let (tx, _rx) = mpsc::unbounded_channel::<ActorMsg>();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let mut state = ActorState::empty(
        crate::config::AppConfig::default(),
        dir,
        adapters,
        Arc::new(NeverRuntime) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(LedgerOnlyWitness) as Arc<dyn crate::budget::VramWitness>,
        tx,
    );
    let entry = |name: &str, category: CategoryId| -> ModelEntry {
        let (map, _) = crate::model::scan::parse_flat_yaml("engine: mock\nvram_gb: 2\n");
        let mut entry = ModelEntry::from_flat(&map, name).expect("entry");
        entry.category = category;
        entry
    };
    let refused = state.register_queued_load(0, &entry("refused", CategoryId::Work), Vec::new());
    let ahead_a = state.register_queued_load(0, &entry("ahead-a", CategoryId::Work), Vec::new());
    let ahead_b = state.register_queued_load(1, &entry("ahead-b", CategoryId::Work), Vec::new());
    state.register_queued_load(2, &entry("other", CategoryId::Rp), Vec::new());

    let hint = state.wait_hint(refused);
    assert_eq!(
        hint.queue_position,
        Some(3),
        "two Work loads are ahead: the refusal places itself third"
    );
    assert_eq!(
        hint.expected_release_s, None,
        "no confirmed release yet: the estimate stays absent"
    );

    // A category with nothing ahead got no place: null, never a quoted "1".
    let empty_job =
        state.register_queued_load(3, &entry("refused-emb", CategoryId::Emb), Vec::new());
    assert_eq!(
        state.wait_hint(empty_job).queue_position,
        None,
        "an empty ahead-set is not a queue"
    );

    // A terminal row is not "ahead": finishing the two Work loads empties the
    // set without deleting the rows (the ledger never deletes).
    for id in [ahead_a, ahead_b] {
        state.jobs.get_mut(&id).expect("row").terminal = true;
    }
    assert_eq!(
        state.wait_hint(refused).queue_position,
        None,
        "terminal rows describe history, not a queue"
    );
}
