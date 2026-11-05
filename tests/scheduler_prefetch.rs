//! Pins for the experimental warm-during-release knob (`prefetch_on_release`).
//! The warm fires exactly at the point a load commits its release target and
//! the driver-confirmed wait opens, on a detached blocking task. The pins
//! prove the five claims that keep the spike admission-risk zero: off means
//! never invoked; on means invoked once for the pending model inside the
//! `releasing` window; a failing warm changes no job outcome; a parked warm
//! never blocks the actor; and the selection's path stats run off the
//! actor's thread (review B8R-01), proven by the thread recorded in
//! `simpleharn::prefetch::warm_selections_for_test`. The invocation seam is
//! the process-global warm hook
//! (`simpleharn::prefetch::install_warm_hook_for_test`), so every pin in
//! this binary serializes on one lock: the hook is one static. The incoming
//! rows declare `engine: llama` — the engine class the knob exists to warm
//! (the adapter is still the mock); engines that open no row-key file
//! select no targets and would observe nothing.

mod mock_runtime;

use async_trait::async_trait;
use mock_runtime::MockRuntime;
use simpleharn::budget::{MemorySample, VramWitness};
use simpleharn::config::{AppConfig, CategoryId};
use simpleharn::engine::mock::MockAdapter;
use simpleharn::engine::{EngineAdapter, EngineRuntime};
use simpleharn::prefetch::{self, WarmHook};
use simpleharn::scheduler::{Reply, Scheduler, SchedulerHandle, SwapState};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Serializes the pins that install the process-global warm hook: the hook is
/// one static shared by every test in this binary (same discipline as the
/// ring's `lock_ring_for_test`). An async mutex because pins hold the guard
/// across the awaits they drive while the hook stays installed.
static PREFETCH_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn lock_prefetch_test_serial() -> tokio::sync::MutexGuard<'static, ()> {
    PREFETCH_TEST_LOCK.lock().await
}

/// What the warm hook recorded: `(model, targets)` per invocation.
type Records = Arc<Mutex<Vec<(String, Vec<PathBuf>)>>>;

/// A card that always reports an empty 72 GiB card: the pins stage the
/// window with the held kill, never with memory pressure.
#[derive(Clone, Default)]
struct EmptyWitness;

#[async_trait]
impl VramWitness for EmptyWitness {
    fn source(&self) -> String {
        "fake-empty".to_string()
    }
    async fn sample(&self) -> Result<MemorySample, String> {
        Ok(MemorySample {
            total_gib: 72.0,
            used_gib: 0.0,
            processes: vec![],
            attribution: simpleharn::budget::AttributionQuality::Unavailable,
        })
    }
}

/// One model row: name, engine id, booked GiB, and the optional `model:`
/// weight file the row names.
struct Row {
    name: &'static str,
    engine: &'static str,
    vram_gb: f64,
    weight_file: Option<PathBuf>,
}

/// Writes the row yamls into a fresh temp ModelConf dir.
fn write_modelconf(rows: &[Row]) -> PathBuf {
    let dir = simpleharn::testutil::temp_dir("b8-conf");
    for row in rows {
        let mut yaml = format!(
            "name: {}\nengine: {}\ncategory: WORK\nvram_gb: {}\n",
            row.name, row.engine, row.vram_gb
        );
        if let Some(file) = &row.weight_file {
            yaml.push_str(&format!("model: {}\n", file.display()));
        }
        std::fs::write(dir.join(format!("{}.yaml", row.name)), yaml).expect("model yaml");
    }
    dir
}

/// A harness with the knob under test control and WORK's single slot driven
/// by the mock runtime (`mock-hold-kill` is the engine id whose kill parks —
/// the seam that keeps the release window open for inspection). `llama` is
/// served by the same mock adapter: the pins stage a llama-class row, the
/// class whose launch opens the row's weight keys and the only class the
/// knob warms for a swap.
fn harness(knob: bool, rows: &[Row]) -> (SchedulerHandle, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-hold-kill".to_string(), Arc::new(MockAdapter));
    adapters.insert("llama".to_string(), Arc::new(MockAdapter));
    let cfg = AppConfig {
        prefetch_on_release: knob,
        ..AppConfig::default()
    };
    let handle = Scheduler::start(
        cfg,
        write_modelconf(rows),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(EmptyWitness) as Arc<dyn VramWitness>,
    );
    (handle, runtime)
}

/// A temp file of `bytes` standing in for the incoming model's weights.
fn seed_weights(tag: &str, name: &str, bytes: &[u8]) -> PathBuf {
    let dir = simpleharn::testutil::temp_dir(tag);
    let path = dir.join(name);
    std::fs::write(&path, bytes).expect("seed weights");
    path
}

/// A hook that records every invocation and then answers `Ok(bytes)`.
fn recording_hook(records: &Records, bytes: u64) -> WarmHook {
    let records = Arc::clone(records);
    Arc::new(move |model: &str, targets: &[PathBuf]| {
        records
            .lock()
            .expect("records lock")
            .push((model.to_string(), targets.to_vec()));
        Ok(bytes)
    })
}

/// Polls the records vector until `cond` holds (the only honest seam to a
/// detached blocking task: the pin observes what it recorded, never a sleep).
async fn wait_records(records: &Records, cond: impl Fn(&[(String, Vec<PathBuf>)]) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        {
            let done = cond(&records.lock().expect("records lock"));
            if done {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the warm hook never recorded as expected: {:?}",
            records.lock().expect("records lock")
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_until(
    handle: &SchedulerHandle,
    secs: u64,
    cond: impl Fn(&simpleharn::scheduler::SchedulerStatus) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let status = handle.status().await.expect("actor alive");
        if cond(&status) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {secs}s; last status: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

fn work_slot(
    status: &simpleharn::scheduler::SchedulerStatus,
) -> &simpleharn::scheduler::SlotStatus {
    status
        .slots
        .iter()
        .find(|slot| slot.category == CategoryId::Work)
        .expect("the default config has a WORK slot")
}

fn job_row<'a>(
    status: &'a simpleharn::scheduler::SchedulerStatus,
    model: &str,
) -> &'a simpleharn::scheduler::JobStatus {
    status
        .jobs
        .iter()
        .rev()
        .find(|job| job.model == model)
        .unwrap_or_else(|| panic!("`{model}` must be in the job ledger: {:?}", status.jobs))
}

/// Loads `name` into WORK and asserts the receipt.
async fn load(handle: &SchedulerHandle, name: &'static str) {
    let Reply::Loaded(receipt) = handle
        .swap(name, CategoryId::Work)
        .await
        .unwrap_or_else(|error| panic!("{name} loads: {error}"))
    else {
        panic!("expected Loaded for {name}");
    };
    assert_eq!(receipt.model, name);
}

/// The OFF claim: with the knob off, the release window opens (job in
/// `Dispose`, kill parked, slot labelled `releasing`) and the warm hook is
/// never invoked — not during the window, not after it.
/// Falsifier: drop the knob guard in `start_release_prefetch` and the records
/// are non-empty the moment the release commits (red at the first assertion).
#[tokio::test]
async fn a_disabled_knob_never_asks_for_a_prefetch() {
    let _serial = lock_prefetch_test_serial().await;
    let weights = seed_weights("b8-off", "inc.gguf", b"incoming weights");
    let rows = [
        Row {
            name: "b8off-hold",
            engine: "mock-hold-kill",
            vram_gb: 30.0,
            weight_file: None,
        },
        Row {
            name: "b8off-inc",
            engine: "llama",
            vram_gb: 20.0,
            weight_file: Some(weights),
        },
    ];
    let (handle, runtime) = harness(false, &rows);
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let _hook = prefetch::install_warm_hook_for_test(recording_hook(&records, 0));

    load(&handle, "b8off-hold").await;
    assert!(
        records.lock().expect("records lock").is_empty(),
        "an empty-slot load releases nothing, so it never warms"
    );

    runtime.hold_next_kills(1);
    let swapper = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b8off-inc", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status
            .jobs
            .iter()
            .any(|job| job.model == "b8off-inc" && job.state == SwapState::Dispose)
    })
    .await;
    assert_eq!(
        work_slot(&handle.status().await.expect("actor alive")).state,
        "releasing",
        "the window is open"
    );
    assert!(
        records.lock().expect("records lock").is_empty(),
        "the window is open and the knob is off: no warm"
    );
    runtime.release_held_kills();
    let Reply::Loaded(receipt) = swapper
        .await
        .expect("swapper alive")
        .expect("the replacement loads")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b8off-inc");
    assert!(
        records.lock().expect("records lock").is_empty(),
        "and none arrives late either"
    );
}

/// The ON claim: with the knob on, the warm is invoked exactly once for the
/// incoming model, with the files its row names, inside the release window —
/// observed before the held kill is released, while the slot still reads
/// `releasing`.
/// Falsifier: remove the `start_release_prefetch` call at the commit point,
/// or move it past the release into the settle/spawn region (a move a few
/// lines down in the same commit turn stays inside the held window and
/// would NOT falsify this pin) — the records stay empty while the window is
/// open, timing this pin out.
#[tokio::test]
async fn an_enabled_knob_warms_the_incoming_model_once_inside_the_release_window() {
    let _serial = lock_prefetch_test_serial().await;
    let weights = seed_weights("b8-on", "inc.gguf", b"incoming weights");
    let rows = [
        Row {
            name: "b8on-hold",
            engine: "mock-hold-kill",
            vram_gb: 30.0,
            weight_file: None,
        },
        Row {
            name: "b8on-inc",
            engine: "llama",
            vram_gb: 20.0,
            weight_file: Some(weights.clone()),
        },
    ];
    let (handle, runtime) = harness(true, &rows);
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let _hook = prefetch::install_warm_hook_for_test(recording_hook(&records, 16));

    load(&handle, "b8on-hold").await;
    assert!(
        records.lock().expect("records lock").is_empty(),
        "the empty-slot load committed no release: nothing to warm"
    );

    runtime.hold_next_kills(1);
    let swapper = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b8on-inc", CategoryId::Work).await })
    };
    wait_records(&records, |got| !got.is_empty()).await;
    let recorded = records.lock().expect("records lock").clone();
    assert_eq!(
        recorded.len(),
        1,
        "one committed release opens exactly one warm, not one per poll"
    );
    assert_eq!(
        recorded[0].0, "b8on-inc",
        "the warm warms the INCOMING model"
    );
    assert_eq!(
        recorded[0].1,
        vec![weights],
        "the selector handed the files the row names"
    );
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        work_slot(&status).state,
        "releasing",
        "the invocation landed inside the releasing window"
    );
    assert_eq!(job_row(&status, "b8on-inc").state, SwapState::Dispose);
    runtime.release_held_kills();
    let Reply::Loaded(receipt) = swapper
        .await
        .expect("swapper alive")
        .expect("the replacement loads")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b8on-inc");
    assert_eq!(
        records.lock().expect("records lock").len(),
        1,
        "exactly one invocation for the whole swap"
    );
}

/// The failure-independence claim: when the warm fails (the hook answers the
/// failure class, as a dead volume or a share violation would), the job the
/// warm was supposed to help still completes: loaded, serving, no error, no
/// fault.
/// Falsifier: run the warm inline on the commit path and fail the job on its
/// Err (replace the detached task with a join that feeds the funnel) — the
/// swap below comes back refused or the row shows the error, red here.
#[tokio::test]
async fn a_failing_prefetch_changes_nothing_about_the_job() {
    let _serial = lock_prefetch_test_serial().await;
    let weights = seed_weights("b8-fail", "inc.gguf", b"incoming weights");
    let rows = [
        Row {
            name: "b8fail-hold",
            engine: "mock-hold-kill",
            vram_gb: 30.0,
            weight_file: None,
        },
        Row {
            name: "b8fail-inc",
            engine: "llama",
            vram_gb: 20.0,
            weight_file: Some(weights),
        },
    ];
    let (handle, runtime) = harness(true, &rows);
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let failing: WarmHook = {
        let records = Arc::clone(&records);
        Arc::new(move |model: &str, targets: &[PathBuf]| {
            records
                .lock()
                .expect("records lock")
                .push((model.to_string(), targets.to_vec()));
            Err("open-failed: injected by the pin".to_string())
        })
    };
    let _hook = prefetch::install_warm_hook_for_test(failing);

    load(&handle, "b8fail-hold").await;
    runtime.hold_next_kills(1);
    let swapper = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b8fail-inc", CategoryId::Work).await })
    };
    wait_records(&records, |got| !got.is_empty()).await;
    runtime.release_held_kills();
    let Reply::Loaded(receipt) = swapper
        .await
        .expect("swapper alive")
        .expect("a dead warm must not refuse the load")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b8fail-inc");
    let status = handle.status().await.expect("actor alive");
    let row = job_row(&status, "b8fail-inc");
    assert!(
        row.error.is_none(),
        "the warm failure never reaches the ledger: {:?}",
        row.error
    );
    assert!(!row.is_fault, "and never turns the job into a fault");
    assert_eq!(work_slot(&status).model.as_deref(), Some("b8fail-inc"));
}

/// The actor-stays-free claim: a warm parked forever on the blocking pool
/// cannot hold the actor hostage — status keeps answering within a tight
/// budget, and the swap itself completes while the warm is still parked. The
/// release itself is NOT held here: only the warm is, so every completed
/// action in this window rides around the parked warm.
/// Falsifier: call `warm_files` inline on the actor's commit turn instead of
/// on `spawn_blocking` — the parked hook parks the actor itself, the tight
/// status polls below time out, and this pin fails.
#[tokio::test]
async fn the_actor_answers_status_while_the_prefetch_is_parked() {
    let _serial = lock_prefetch_test_serial().await;
    let weights = seed_weights("b8-park", "inc.gguf", b"incoming weights");
    let rows = [
        Row {
            name: "b8park-a",
            engine: "mock",
            vram_gb: 30.0,
            weight_file: None,
        },
        Row {
            name: "b8park-inc",
            engine: "llama",
            vram_gb: 20.0,
            weight_file: Some(weights),
        },
    ];
    let (handle, _runtime) = harness(true, &rows);
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let (gate_tx, gate_rx): (Sender<()>, Receiver<()>) = mpsc::channel();
    let parked: WarmHook = {
        let records = Arc::clone(&records);
        let gate = Arc::new(Mutex::new(gate_rx));
        Arc::new(move |model: &str, targets: &[PathBuf]| {
            records
                .lock()
                .expect("records lock")
                .push((model.to_string(), targets.to_vec()));
            // Park until the test lets go (channel closed = unblock for good).
            let _ = gate.lock().expect("gate lock").recv();
            Ok(1)
        })
    };
    let _hook = prefetch::install_warm_hook_for_test(parked);

    load(&handle, "b8park-a").await;
    let swapper = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b8park-inc", CategoryId::Work).await })
    };
    wait_records(&records, |got| !got.is_empty()).await;
    // The warm thread is parked inside the hook right now. The actor must
    // answer a status request on every turn of a tight poll — a warm that
    // had been run on the actor's turn would hold this hostage instead.
    for round in 0..5 {
        let answered = tokio::time::timeout(Duration::from_millis(2_000), handle.status())
            .await
            .unwrap_or_else(|_| panic!("the actor stopped answering status while the prefetch was parked (round {round})"))
            .expect("actor alive");
        let _ = work_slot(&answered);
    }
    let Reply::Loaded(receipt) = swapper
        .await
        .expect("swapper alive")
        .expect("the swap completes while the warm is still parked")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b8park-inc");
    drop(gate_tx); // let the parked hook thread finish and the guard clear cleanly
}

/// The stats-location claim (review B8R-01): the selector's path stats run
/// on the blocking pool, never on the actor's commit turn. Each detached
/// selection records its thread (`warm_selections_for_test`), and
/// `#[tokio::test]`'s default current-thread flavor runs the actor on this
/// very thread — so a new record naming any other thread proves the stats
/// left the actor. No race: the detached task stats before it calls the
/// hook, so a recorded hook invocation already implies the record landed.
/// Falsifier: hoist the `spec.targets()` call in `start_release_prefetch`
/// out of the `spawn_blocking` closure onto the commit turn — the new
/// record then names the actor thread and the `assert_ne` fails.
#[tokio::test]
async fn the_warm_selects_and_stats_off_the_actor_thread() {
    let _serial = lock_prefetch_test_serial().await;
    let weights = seed_weights("b8-thread", "inc.gguf", b"incoming weights");
    let rows = [
        Row {
            name: "b8thr-hold",
            engine: "mock-hold-kill",
            vram_gb: 30.0,
            weight_file: None,
        },
        Row {
            name: "b8thr-inc",
            engine: "llama",
            vram_gb: 20.0,
            weight_file: Some(weights),
        },
    ];
    let (handle, runtime) = harness(true, &rows);
    let records: Records = Arc::new(Mutex::new(Vec::new()));
    let baseline = prefetch::warm_selections_for_test().len();
    let _hook = prefetch::install_warm_hook_for_test(recording_hook(&records, 1));

    load(&handle, "b8thr-hold").await;
    assert_eq!(
        prefetch::warm_selections_for_test().len(),
        baseline,
        "an empty-slot load committed no release: no selection ran"
    );

    runtime.hold_next_kills(1);
    let swapper = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b8thr-inc", CategoryId::Work).await })
    };
    wait_records(&records, |got| !got.is_empty()).await;
    let runs = prefetch::warm_selections_for_test();
    assert_eq!(
        runs.len(),
        baseline + 1,
        "exactly one selection run for the committed release"
    );
    let (model, thread) = runs.last().expect("the new run is recorded");
    assert_eq!(
        model, "b8thr-inc",
        "the selection ran for the incoming model"
    );
    assert_ne!(
        *thread,
        std::thread::current().id(),
        "the stats ran on the blocking pool, not on the thread hosting the actor"
    );
    runtime.release_held_kills();
    let Reply::Loaded(receipt) = swapper
        .await
        .expect("swapper alive")
        .expect("the replacement loads")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b8thr-inc");
    assert_eq!(
        prefetch::warm_selections_for_test().len(),
        baseline + 1,
        "and the whole swap selected exactly once"
    );
}
