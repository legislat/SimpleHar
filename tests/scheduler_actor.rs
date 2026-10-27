//! Scheduler-actor integration tests against the in-process mock engine (Step 19).
//! Each test pins one v3.3 defect class: F-1 (client abort), F-6 (double load),
//! T3.4 (rapid A->B), T8 (cancel mid-load), A1 (control plane behind a load), plus
//! the admission and supersede contracts.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::budget::{MemorySample, VramWitness};
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::llama::LlamaAdapter;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::ninfer::NinferAdapter;
use simplehar_se::engine::strata::StrataAdapter;
use simplehar_se::engine::EngineAdapter;
use simplehar_se::scheduler::{
    JobId, JobKind, Reply, Scheduler, SchedulerError, SchedulerHandle, SchedulerStatus, SwapState,
};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn config(load_timeout_s: u64) -> AppConfig {
    AppConfig {
        load_timeout_s,
        ..AppConfig::default()
    }
}

type Spec = (&'static str, &'static str, CategoryId, Option<f64>);
type Spec5 = (
    &'static str,
    &'static str,
    CategoryId,
    Option<f64>,
    &'static str,
);

fn to5(specs: &[Spec]) -> Vec<Spec5> {
    specs
        .iter()
        .map(|(name, engine, category, vram)| (*name, *engine, *category, *vram, ""))
        .collect()
}

/// Writes flat model yamls into a fresh temp ModelConf dir; returns its path.
/// The 5th spec element appends raw extra lines (cost:, floor keys...).
fn write_modelconf5(specs: &[Spec5]) -> std::path::PathBuf {
    let dir = simplehar_se::testutil::temp_dir("modelconf");
    for (name, engine, category, vram_gb, extra) in specs {
        let mut yaml = format!(
            "name: {name}
engine: {engine}
category: {category}
"
        );
        if let Some(vram) = vram_gb {
            yaml.push_str(&format!(
                "vram_gb: {vram}
"
            ));
        }
        if !extra.is_empty() {
            yaml.push_str(extra);
            if !extra.ends_with('\n') {
                yaml.push('\n');
            }
        }
        std::fs::write(dir.join(format!("{name}.yaml")), yaml).expect("model yaml");
    }
    dir
}

fn harness5(cfg: AppConfig, specs: &[Spec5]) -> (SchedulerHandle, MockRuntime) {
    harness5_witness(
        cfg,
        specs,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    )
}

/// `harness5` against a caller-supplied witness, for the cases that must control when
/// an admission decision is allowed to land.
fn harness5_witness(
    cfg: AppConfig,
    specs: &[Spec5],
    witness: Arc<dyn VramWitness>,
) -> (SchedulerHandle, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters = BTreeMap::new();
    let adapter: Arc<dyn simplehar_se::engine::EngineAdapter> = Arc::new(MockAdapter);
    adapters.insert("mock".to_string(), adapter);
    for engine in ["mock-slow", "mock-dies", "mock-never", "mock-hold-kill"] {
        adapters.insert(
            engine.to_string(),
            Arc::new(MockAdapter) as Arc<dyn simplehar_se::engine::EngineAdapter>,
        );
    }
    // The real adapters, under the ids they themselves serve: a launch validation
    // refusal must come from production code, not from a mock that accepts anything.
    let reals: Vec<Arc<dyn EngineAdapter>> = vec![
        Arc::new(StrataAdapter::default()),
        Arc::new(NinferAdapter::new("Ubuntu", "ops")),
        Arc::new(LlamaAdapter::new(PathBuf::from("llama-server.exe"))),
    ];
    for adapter in reals {
        adapters.insert(adapter.engine_id().to_string(), adapter);
    }
    let handle = Scheduler::start(
        cfg,
        write_modelconf5(specs),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn simplehar_se::engine::EngineRuntime>,
        witness,
    );
    (handle, runtime)
}

fn harness(cfg: AppConfig, specs: &[Spec]) -> (SchedulerHandle, MockRuntime) {
    harness5(cfg, &to5(specs))
}

/// Polls `cond` against status snapshots until it holds, or fails the test.
async fn wait_until(
    handle: &SchedulerHandle,
    secs: u64,
    cond: impl Fn(&simplehar_se::scheduler::SchedulerStatus) -> bool,
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Submits a load and vanishes: the command reaches the actor, the reply is discarded —
/// the true F-1 client-abort shape (the dropped waiter must not cancel the load).
async fn submit_and_vanish(handle: &SchedulerHandle, model: &str, category: CategoryId) {
    let client = handle.clone();
    let model = model.to_string();
    tokio::spawn(async move {
        let _ = client.swap(&model, category).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
}

fn slot_model(
    status: &simplehar_se::scheduler::SchedulerStatus,
    category: CategoryId,
) -> Vec<Option<String>> {
    status
        .slots
        .iter()
        .filter(|s| s.category == category)
        .map(|s| s.model.clone())
        .collect()
}

fn terminal_jobs(
    status: &simplehar_se::scheduler::SchedulerStatus,
) -> Vec<&simplehar_se::scheduler::JobStatus> {
    status.jobs.iter().filter(|j| j.terminal).collect()
}

#[tokio::test]
async fn loads_model_into_empty_category_and_ledger() {
    let (handle, runtime) = harness(config(5), &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);

    let reply = handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect("load succeeds");
    let Reply::Loaded(receipt) = reply else {
        panic!("expected Loaded, got {reply:?}")
    };
    assert_eq!(receipt.model, "qwen3");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        slot_model(&status, CategoryId::Work),
        vec![Some("qwen3".to_string())]
    );
    let occupant_need = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Work)
        .unwrap()
        .need_gib;
    assert_eq!(occupant_need, Some(4.0));
    assert_eq!(runtime.server_count(), 1, "exactly one engine must be live");
}

#[tokio::test]
async fn same_model_same_category_is_already_and_idempotent() {
    let (handle, runtime) = harness(config(5), &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);

    handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect("first load");
    let second = handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect("second is not an error");
    assert!(matches!(second, Reply::Already { .. }), "got {second:?}");
    assert_eq!(runtime.server_count(), 1, "no second engine may spawn");
}

#[tokio::test]
async fn concurrent_identical_loads_share_one_job_and_one_engine() {
    // F-6: two loads for the same model and category must not race.
    let (handle, runtime) = harness(
        config(10),
        &[("qwen3", "mock-slow", CategoryId::Work, Some(4.0))],
    );

    let (a, b) = tokio::join!(
        handle.swap("qwen3", CategoryId::Work),
        handle.swap("qwen3", CategoryId::Work),
    );
    let receipt_of = |r: Result<Reply, SchedulerError>| match r.expect("both succeed") {
        Reply::Loaded(receipt) => receipt,
        other => panic!("expected Loaded, got {other:?}"),
    };
    let (ra, rb) = (receipt_of(a), receipt_of(b));
    assert_eq!(ra.job, rb.job, "both waiters must ride the same job");
    wait_until(&handle, 5, |s| {
        slot_model(s, CategoryId::Work) == vec![Some("qwen3".to_string())]
    })
    .await;
    assert_eq!(
        runtime.server_count(),
        1,
        "exactly one engine despite two callers"
    );
}

#[tokio::test]
async fn different_model_replaces_occupant_and_kills_previous() {
    let (handle, runtime) = harness(
        config(5),
        &[
            ("model-a", "mock", CategoryId::Work, Some(4.0)),
            ("model-b", "mock", CategoryId::Work, Some(4.0)),
        ],
    );

    let Reply::Loaded(first) = handle
        .swap("model-a", CategoryId::Work)
        .await
        .expect("a loads")
    else {
        panic!("expected Loaded")
    };
    let Reply::Loaded(second) = handle
        .swap("model-b", CategoryId::Work)
        .await
        .expect("b loads")
    else {
        panic!("expected Loaded")
    };
    assert_ne!(first.port, second.port);

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        slot_model(&status, CategoryId::Work),
        vec![Some("model-b".to_string())]
    );
    assert!(
        runtime.is_dead(first.port).await,
        "the replaced engine must be dead (G12)"
    );
    assert_eq!(runtime.server_count(), 1, "no leaked engine after replace");
}

#[tokio::test]
async fn rapid_a_then_b_ends_with_exactly_b() {
    // T3.4: a slow A is superseded by B; the end state is B serving, A's engine dead.
    let (handle, runtime) = harness(
        config(10),
        &[
            ("model-a", "mock-slow", CategoryId::Work, Some(4.0)),
            ("model-b", "mock", CategoryId::Work, Some(4.0)),
        ],
    );

    // Drop the waiter on purpose: the client aborts immediately after submitting A.
    submit_and_vanish(&handle, "model-a", CategoryId::Work).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let Reply::Loaded(receipt) = handle
        .swap("model-b", CategoryId::Work)
        .await
        .expect("b loads")
    else {
        panic!("expected Loaded")
    };
    assert_eq!(receipt.model, "model-b");

    wait_until(&handle, 5, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work) == vec![Some("model-b".to_string())]
    })
    .await;
    assert_eq!(runtime.server_count(), 1, "only B's engine may survive");
}

#[tokio::test]
async fn client_abort_mid_load_leaves_a_serving_slot_and_no_orphan() {
    // F-1: the waiter disappears; the load itself must complete.
    let (handle, runtime) = harness(
        config(10),
        &[("qwen3", "mock-slow", CategoryId::Work, Some(4.0))],
    );

    submit_and_vanish(&handle, "qwen3", CategoryId::Work).await;
    wait_until(&handle, 8, |s| {
        slot_model(s, CategoryId::Work) == vec![Some("qwen3".to_string())]
    })
    .await;
    assert_eq!(
        runtime.server_count(),
        1,
        "the engine survives the aborted client"
    );
}

#[tokio::test]
async fn cancel_mid_load_kills_engine_and_empties_slot() {
    // T8: cancel is observed inside a step and completes cleanup.
    let (handle, runtime) = harness(
        config(10),
        &[("qwen3", "mock-slow", CategoryId::Work, Some(4.0))],
    );

    let job = spawn_and_find_job(&handle).await;
    handle.cancel(job).await.expect("cancel accepted");
    wait_until(&handle, 5, |s| {
        s.jobs.iter().any(|j| j.id == job && j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let job_status = status.jobs.iter().find(|j| j.id == job).unwrap();
    assert_eq!(job_status.state, SwapState::Idle);
    assert!(job_status
        .error
        .as_deref()
        .unwrap_or("")
        .contains("cancelled"));
    assert_eq!(
        runtime.server_count(),
        0,
        "cancelled load must not leak an engine"
    );
}

#[tokio::test]
async fn admission_refusal_carries_the_numbers_and_keeps_the_slot() {
    // 50 + 50 > 72 - 2: the second load must refuse with need/free in the message.
    let (handle, runtime) = harness(
        config(5),
        &[
            ("big-a", "mock", CategoryId::Work, Some(50.0)),
            ("big-b", "mock", CategoryId::Rp, Some(50.0)),
        ],
    );

    handle
        .swap("big-a", CategoryId::Work)
        .await
        .expect("a fits");
    let error = handle
        .swap("big-b", CategoryId::Rp)
        .await
        .expect_err("b must refuse");
    let message = error.to_string();
    assert!(message.contains("need 50.0 GiB"), "message was: {message}");
    assert!(message.contains("free 20.0 GiB"), "message was: {message}");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        slot_model(&status, CategoryId::Rp),
        vec![None],
        "refused load leaves the slot empty"
    );
    assert!(status
        .jobs
        .iter()
        .any(|j| j.state == SwapState::AdmitFail && j.terminal));
    assert_eq!(
        runtime.server_count(),
        1,
        "only the admitted engine is live"
    );
}

#[tokio::test]
async fn under_specified_model_refuses_with_the_real_reason() {
    // F-7 lesson: the refusal must name under-specification, not blame a neighbor.
    let (handle, _runtime) = harness(config(5), &[("ghost", "mock", CategoryId::Work, None)]);

    let error = handle
        .swap("ghost", CategoryId::Work)
        .await
        .expect_err("must refuse");
    assert!(
        error.to_string().contains("under-specified"),
        "was: {error}"
    );
}

#[tokio::test]
async fn unload_frees_slot_ledger_and_engine() {
    let (handle, runtime) = harness(config(5), &[("qwen3", "mock", CategoryId::Work, Some(4.0))]);

    handle.swap("qwen3", CategoryId::Work).await.expect("loads");
    handle
        .unload(CategoryId::Work)
        .await
        .expect("unload accepted");
    wait_until(&handle, 5, |s| {
        slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    assert_eq!(runtime.server_count(), 0, "unload must kill the engine");
    // Idempotent: unloading an empty category is a plain Done.
    assert!(matches!(
        handle.unload(CategoryId::Work).await.expect("idempotent"),
        Reply::Done
    ));
}

#[tokio::test]
async fn unload_model_during_load_cancels_and_kills_engine() {
    // T12.1 + T8: unloading a still-loading model cancels its swap and the
    // cancellation still runs kill + release confirmation.
    let (handle, runtime) = harness(
        config(10),
        &[("qwen3", "mock-slow", CategoryId::Work, Some(4.0))],
    );

    let job = spawn_and_find_job(&handle).await;
    assert!(
        matches!(
            handle.unload_model("qwen3").await.expect("accepted"),
            Reply::Done
        ),
        "unload of a loading slot is a Done (the cancel rides the job)"
    );
    wait_until(&handle, 5, |s| {
        s.jobs.iter().any(|j| j.id == job && j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let job_status = status.jobs.iter().find(|j| j.id == job).unwrap();
    assert!(job_status
        .error
        .as_deref()
        .unwrap_or("")
        .contains("unloaded by request"));
    assert_eq!(
        runtime.server_count(),
        0,
        "cancelled swap must not leak an engine"
    );
    // T12.2: unloading a model that is not loaded is an idempotent Done.
    assert!(matches!(
        handle.unload_model("ghost").await.expect("idempotent"),
        Reply::Done
    ));
}

// --- cancel while a victim release is in flight (chunk 4 [1], 2026-10-27) ---------

/// The resident model of the cancel-mid-release scenarios: its engine parks inside
/// `kill` until the test releases it, so the swap's Dispose release is genuinely still
/// in flight when the operator's cancel lands.
const HOLD_MODEL: &str = "hold-a";
/// The model being swapped in over it.
const INCOMING_MODEL: &str = "model-b";

fn hold_kill_harness(cfg: AppConfig) -> (SchedulerHandle, MockRuntime) {
    harness5(
        cfg,
        &[
            (
                HOLD_MODEL,
                "mock-hold-kill",
                CategoryId::Work,
                Some(4.0),
                "",
            ),
            (INCOMING_MODEL, "mock", CategoryId::Work, Some(4.0), ""),
        ],
    )
}

/// Loads [`HOLD_MODEL`] into WORK and returns its engine's receipt.
async fn serve_hold_model(handle: &SchedulerHandle) -> simplehar_se::scheduler::SwapReceipt {
    let Reply::Loaded(receipt) = handle
        .swap(HOLD_MODEL, CategoryId::Work)
        .await
        .expect("the resident model loads")
    else {
        panic!("expected Loaded, got the resident load")
    };
    receipt
}

/// Drives the same-slot swap of [`INCOMING_MODEL`] over [`HOLD_MODEL`] until the job
/// sits in `Dispose` with the victim's kill unconfirmed, and returns the swap's job id.
async fn swap_parked_in_dispose_over_a_victim(handle: &SchedulerHandle) -> JobId {
    submit_and_vanish(handle, INCOMING_MODEL, CategoryId::Work).await;
    wait_until(handle, 5, |s| {
        s.jobs
            .iter()
            .any(|j| j.model == INCOMING_MODEL && j.state == SwapState::Dispose)
    })
    .await;
    handle
        .status()
        .await
        .expect("actor alive")
        .jobs
        .iter()
        .find(|j| j.model == INCOMING_MODEL)
        .map(|j| j.id)
        .expect("invariant: wait_until established the swap job")
}

#[tokio::test]
async fn cancel_of_a_swap_mid_victim_release_kills_the_victim_and_empties_its_slot() {
    // FINDINGS-04 [1]: Unload during a hung swap is the operator's commonest recovery
    // gesture. Cancelling there must still kill the victim and confirm it: a job that
    // terminalizes over an unconfirmed release leaves a dead-but-installed occupant,
    // which debits the ledger forever and blocks reload with 200 `already`.
    let (handle, runtime) = hold_kill_harness(config(10));
    let victim = serve_hold_model(&handle).await;
    assert_eq!(runtime.server_count(), 1, "the victim engine is live");
    swap_parked_in_dispose_over_a_victim(&handle).await;

    assert!(matches!(
        handle
            .unload(CategoryId::Work)
            .await
            .expect("cancel accepted"),
        Reply::Done
    ));
    assert_eq!(
        runtime.server_count(),
        1,
        "nothing may die behind the operator's back before the cancel's own reap runs"
    );
    runtime.release_held_kills();

    wait_until(&handle, 8, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let job = status
        .jobs
        .iter()
        .find(|j| j.model == INCOMING_MODEL)
        .expect("the cancelled swap is in the ledger");
    assert_eq!(job.outcome.as_deref(), Some("cancelled"), "{job:?}");
    assert!(
        job.error
            .as_deref()
            .unwrap_or("")
            .contains("unloaded by request"),
        "{job:?}"
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "the victim engine must not survive the cancelled swap"
    );
    assert!(
        runtime.is_dead(victim.port).await,
        "the victim's port must stop answering (G12)"
    );
    assert_eq!(
        runtime.spawn_count(),
        1,
        "a swap cancelled in Dispose must never spawn its replacement"
    );
}

#[tokio::test]
async fn second_unload_of_a_slow_unload_kills_the_victim_and_empties_its_slot() {
    // The mirror case of chunk 4 [1]: `begin_unload_slot` sets `victim`, never
    // `spawned`, so the second /har/unload of a slow unload cancels the first while
    // its kill is in flight — and must still finish the job it cancels.
    let (handle, runtime) = hold_kill_harness(config(10));
    let victim = serve_hold_model(&handle).await;

    handle
        .unload(CategoryId::Work)
        .await
        .expect("first unload accepted");
    wait_until(&handle, 5, |s| {
        s.jobs.iter().any(|j| {
            j.model == HOLD_MODEL
                && j.kind == simplehar_se::scheduler::JobKind::Unload
                && j.state == SwapState::Dispose
        })
    })
    .await;
    assert_eq!(
        runtime.server_count(),
        1,
        "the engine is still live when the second unload lands"
    );

    handle
        .unload(CategoryId::Work)
        .await
        .expect("second unload accepted");
    runtime.release_held_kills();

    wait_until(&handle, 8, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let job = status
        .jobs
        .iter()
        .find(|j| j.model == HOLD_MODEL && j.kind == simplehar_se::scheduler::JobKind::Unload)
        .expect("the cancelled unload is in the ledger");
    assert_eq!(job.outcome.as_deref(), Some("cancelled"), "{job:?}");
    assert_eq!(
        runtime.server_count(),
        0,
        "a cancelled unload must leave no live engine"
    );
    assert!(runtime.is_dead(victim.port).await, "and no answering port");
}

#[tokio::test]
async fn cancel_with_an_unconfirmed_release_keeps_the_occupant_and_says_cancelled() {
    // The half the fix must NOT touch: a release that timed out was never confirmed,
    // so the harness may not claim the engine is gone. The slot keeps its occupant (and
    // its ledger debit) and the row stays a plain cancel.
    let cfg = AppConfig {
        release_timeout_s: 2,
        ..config(10)
    };
    let (handle, runtime) = hold_kill_harness(cfg);
    let victim = serve_hold_model(&handle).await;
    swap_parked_in_dispose_over_a_victim(&handle).await;

    handle
        .unload(CategoryId::Work)
        .await
        .expect("cancel accepted");
    wait_until(&handle, 10, |s| s.jobs.iter().all(|j| j.terminal)).await;

    let status = handle.status().await.expect("actor alive");
    let job = status
        .jobs
        .iter()
        .find(|j| j.model == INCOMING_MODEL)
        .expect("the cancelled swap is in the ledger");
    assert_eq!(job.outcome.as_deref(), Some("cancelled"), "{job:?}");
    assert!(
        job.error
            .as_deref()
            .unwrap_or("")
            .contains("unloaded by request"),
        "{job:?}"
    );
    assert_eq!(
        slot_model(&status, CategoryId::Work),
        vec![Some(HOLD_MODEL.to_string())],
        "an unconfirmed release may not clear the slot of an engine nobody proved dead"
    );
    assert_eq!(runtime.server_count(), 1, "the engine was never killed");
    assert!(
        !runtime.is_dead(victim.port).await,
        "and it is still answering on its port"
    );
}

#[tokio::test]
async fn a_second_cancel_of_the_same_swap_still_reaps_the_victim() {
    // The reap decision must not key off the state the cancel itself writes: pressing
    // Unload twice (or Unload then loading a third model) cancels a job that is already
    // `CancelRequested` with its re-issued kill still parked.
    let (handle, runtime) = hold_kill_harness(config(10));
    let victim = serve_hold_model(&handle).await;
    let job = swap_parked_in_dispose_over_a_victim(&handle).await;

    handle
        .unload(CategoryId::Work)
        .await
        .expect("first cancel accepted");
    wait_until(&handle, 5, |s| {
        s.jobs
            .iter()
            .any(|j| j.id == job && j.state == SwapState::CancelRequested)
    })
    .await;

    assert!(matches!(
        handle.cancel(job).await.expect("second cancel accepted"),
        Reply::Done
    ));
    runtime.release_held_kills();

    wait_until(&handle, 8, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    assert_eq!(
        runtime.server_count(),
        0,
        "the second cancel must not abandon the victim's reap"
    );
    assert!(runtime.is_dead(victim.port).await, "and no answering port");
}

#[tokio::test]
async fn unload_all_clears_every_category_and_cancels_inflight() {
    // T12.4 (the tray's Unload All): every occupied slot releases, every
    // in-flight job cancels, nothing leaks.
    let (handle, runtime) = harness(
        config(10),
        &[
            ("work06", "mock", CategoryId::Work, Some(4.0)),
            ("rp06", "mock-slow", CategoryId::Rp, Some(4.0)),
        ],
    );

    handle
        .swap("work06", CategoryId::Work)
        .await
        .expect("loads");
    submit_and_vanish(&handle, "rp06", CategoryId::Rp).await;
    wait_until(&handle, 5, |s| {
        s.slots
            .iter()
            .any(|slot| slot.category == CategoryId::Rp && slot.loading_job.is_some())
    })
    .await;

    assert!(matches!(
        handle.unload_all().await.expect("accepted"),
        Reply::Done
    ));
    wait_until(&handle, 8, |s| {
        s.slots.iter().all(|slot| slot.model.is_none()) && s.jobs.iter().all(|j| j.terminal)
    })
    .await;
    assert_eq!(runtime.server_count(), 0, "unload-all kills every engine");
    assert!(matches!(
        handle.unload_all().await.expect("idempotent"),
        Reply::Done
    ));
}

#[tokio::test]
async fn control_plane_answers_while_a_slow_load_runs() {
    // A1: v3.3 held a global lock through wait_ready; SE must not.
    let (handle, _runtime) = harness(
        config(10),
        &[
            ("slow", "mock-slow", CategoryId::Work, Some(4.0)),
            ("quick", "mock", CategoryId::Rp, Some(4.0)),
        ],
    );

    submit_and_vanish(&handle, "slow", CategoryId::Work).await;
    tokio::time::sleep(Duration::from_millis(150)).await; // let the load get going

    let started = Instant::now();
    let status = handle
        .status()
        .await
        .expect("status must answer during a load");
    assert!(
        started.elapsed() < Duration::from_millis(300),
        "status queued behind the load"
    );
    assert!(
        status.slots.iter().any(|s| s.loading_job.is_some()),
        "the load must be visible as a job"
    );

    let started = Instant::now();
    handle
        .swap("quick", CategoryId::Rp)
        .await
        .expect("other categories load freely");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "RP queued behind WORK's load"
    );
    handle
        .unload(CategoryId::Work)
        .await
        .expect("unload must not queue behind the load");
}

#[tokio::test]
async fn engine_that_never_becomes_ready_fails_within_the_deadline() {
    let (handle, runtime) = harness(
        config(1),
        &[("broken", "mock-never", CategoryId::Work, Some(4.0))],
    );

    let error = handle
        .swap("broken", CategoryId::Work)
        .await
        .expect_err("must fail");
    assert!(error.to_string().contains("ready"), "was: {error}");
    assert_eq!(
        runtime.server_count(),
        0,
        "failed load must reap its engine"
    );
    let status = handle.status().await.expect("actor alive");
    assert!(
        slot_model(&status, CategoryId::Work)
            .iter()
            .all(Option::is_none),
        "failed load must leave the slot empty, got {:?}",
        slot_model(&status, CategoryId::Work)
    );
    assert!(terminal_jobs(&status).iter().any(|j| j.error.is_some()));
}

#[tokio::test]
async fn failed_swap_that_released_its_victim_still_reaps_its_own_engine() {
    // The kill target used to be `job.victim` when one existed, and `job.victim` is never
    // cleared once queued — so a swap that evicted its victim, spawned, and then failed
    // readiness re-killed a corpse and left the engine it had started holding the card and
    // its port while the slot read empty (chunk 3, CRITICAL, 2026-10-27).
    let (handle, runtime) = harness5(
        config(1),
        &[
            ("aux30", "mock", CategoryId::Aux, Some(30.0), ""),
            ("work60", "mock-never", CategoryId::Work, Some(60.0), ""),
        ],
    );
    handle
        .swap("aux30", CategoryId::Aux)
        .await
        .expect("aux30 loads");
    assert_eq!(runtime.server_count(), 1, "the victim engine is live");

    let error = handle
        .swap("work60", CategoryId::Work)
        .await
        .expect_err("the never-ready engine must fail its load");
    assert!(
        error.to_string().contains("ready"),
        "must fail on readiness, not on admission: {error}"
    );

    let status = handle.status().await.expect("actor alive");
    let swap_job = terminal_jobs(&status)
        .into_iter()
        .find(|job| job.model == "work60")
        .expect("the swap is in the ledger");
    assert_eq!(
        swap_job.victims_released,
        vec!["aux30".to_string()],
        "the eviction must really have happened, or this test proves nothing: {swap_job:?}"
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "both the evicted victim and the failed engine must be reaped"
    );
    assert!(
        slot_model(&status, CategoryId::Aux)
            .iter()
            .all(Option::is_none),
        "the evicted slot stays empty"
    );
    assert!(
        slot_model(&status, CategoryId::Work)
            .iter()
            .all(Option::is_none),
        "a failed load must not leave an occupant"
    );
}

/// Submits a slow load without keeping the waiter and returns its job id.
async fn spawn_and_find_job(handle: &SchedulerHandle) -> JobId {
    submit_and_vanish(handle, "qwen3", CategoryId::Work).await;
    wait_until(handle, 5, |s| {
        s.slots.iter().any(|slot| slot.loading_job.is_some())
    })
    .await;
    handle
        .status()
        .await
        .expect("actor alive")
        .slots
        .iter()
        .find_map(|slot| slot.loading_job)
        .expect("invariant: wait_until established a loading job")
}

// --- cost classes + launch floor (operator design note, 2026-10-26) -----------

fn small_card() -> AppConfig {
    AppConfig {
        vram_total_gb: 10.0,
        vram_reserve_gb: 2.0,
        load_timeout_s: 5,
        ..AppConfig::default()
    }
}

#[tokio::test]
async fn greedy_loads_book_the_room_they_grab_not_their_nominal_size() {
    // strata-style: nominal 72 GiB "does not fit" the 10 GiB card, but it
    // offloads to RAM/SSD — the size gate must not refuse, and the ledger
    // books what it will actually grab (room at admit), not 72.
    let (handle, runtime) = harness5(
        small_card(),
        &[
            ("work06", "mock", CategoryId::Work, Some(4.0), ""),
            ("big", "mock", CategoryId::Rp, Some(72.0), "cost: greedy"),
        ],
    );
    handle
        .swap("work06", CategoryId::Work)
        .await
        .expect("work loads");
    // free = 10 - 2 - 4 = 4; greedy default floor 4 -> floor met.
    handle
        .swap("big", CategoryId::Rp)
        .await
        .expect("greedy always fits");
    wait_until(&handle, 5, |s| {
        s.slots
            .iter()
            .any(|sl| sl.category == CategoryId::Rp && sl.model.as_deref() == Some("big"))
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let rp = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Rp)
        .expect("RP slot");
    assert_eq!(
        rp.need_gib,
        Some(4.0),
        "books the room, not the nominal size"
    );
    assert_eq!(runtime.server_count(), 2);
}

#[tokio::test]
async fn greedy_below_the_launch_floor_refuses_before_spawn() {
    // The guard the operator asked for: a greedy load must not spawn onto a
    // card already busy with other work — refused before engine start.
    let cfg = AppConfig {
        greedy_launch_min_free_gb: 6.0,
        ..small_card()
    };
    let (handle, runtime) = harness5(
        cfg,
        &[
            ("work06", "mock", CategoryId::Work, Some(4.0), ""),
            ("big", "mock", CategoryId::Rp, Some(72.0), "cost: greedy"),
        ],
    );
    handle
        .swap("work06", CategoryId::Work)
        .await
        .expect("work loads");
    // free = 10 - 2 - 4 = 4 < floor 6; WORK is not evictable -> refuse.
    let error = handle
        .swap("big", CategoryId::Rp)
        .await
        .expect_err("the floor refuses the greedy load");
    assert!(
        matches!(
            error,
            SchedulerError::Admission(
                simplehar_se::budget::AdmissionError::BelowLaunchFloor { .. }
            )
        ),
        "distinct typed error, got {error}"
    );
    assert!(error.to_string().contains("launch floor"), "{error}");
    assert_eq!(runtime.server_count(), 1, "refused BEFORE spawn, no thrash");
    wait_until(&handle, 5, |s| {
        s.jobs.iter().any(|j| {
            j.model == "big"
                && j.terminal
                && j.error.as_deref().unwrap_or("").contains("launch floor")
        })
    })
    .await;
}

#[tokio::test]
async fn floor_victims_release_in_full_not_until_the_size_fits() {
    // Review CRITICAL (2026-10-26): the floor is planned against plan_victims,
    // but the release-minimality re-check must not skip remaining floor victims
    // just because the (greedy, booked-small) need already fits after one kill.
    let mut cfg = AppConfig {
        vram_total_gb: 16.0,
        vram_reserve_gb: 2.0,
        load_timeout_s: 5,
        ..AppConfig::default()
    };
    if let Some(aux) = cfg
        .categories
        .iter_mut()
        .find(|cat| cat.id == CategoryId::Aux)
    {
        aux.max_models = 3; // two evictable victims must coexist
    }
    cfg.greedy_launch_min_free_gb = 6.0;
    let (handle, _runtime) = harness5(
        cfg,
        &[
            ("work8", "mock", CategoryId::Work, Some(8.0), ""),
            ("aux1", "mock", CategoryId::Aux, Some(3.0), ""),
            ("aux2", "mock", CategoryId::Aux, Some(3.0), ""),
            (
                "greedy72",
                "mock",
                CategoryId::Rp,
                Some(72.0),
                "cost: greedy",
            ),
        ],
    );
    handle
        .swap("work8", CategoryId::Work)
        .await
        .expect("work loads");
    handle
        .swap("aux1", CategoryId::Aux)
        .await
        .expect("aux1 loads");
    handle
        .swap("aux2", CategoryId::Aux)
        .await
        .expect("aux2 loads");
    // Capacity 14 fully booked: free = 0, floor 6 -> BOTH 3 GiB AUX victims
    // must be released (WORK is protected). After only one kill the budget
    // available is 3 < 6: the re-check must continue, not spawn on a half-busy card.
    handle
        .swap("greedy72", CategoryId::Rp)
        .await
        .expect("greedy proceeds once both victims free the floor");
    wait_until(&handle, 8, |s| {
        s.slots
            .iter()
            .any(|sl| sl.category == CategoryId::Rp && sl.model.as_deref() == Some("greedy72"))
    })
    .await;
    let status = handle.status().await.expect("alive");
    let still_there = |cat: CategoryId, name: &str| {
        status
            .slots
            .iter()
            .any(|sl| sl.category == cat && sl.model.as_deref() == Some(name))
    };
    assert!(still_there(CategoryId::Work, "work8"), "WORK is protected");
    assert!(
        !still_there(CategoryId::Aux, "aux1") && !still_there(CategoryId::Aux, "aux2"),
        "BOTH floor victims must be released, not just the first: {status:?}"
    );
    let rp = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Rp)
        .expect("RP slot");
    assert_eq!(
        rp.need_gib,
        Some(6.0),
        "greedy books the room it grabbed (budget + freed victims), never 0"
    );
}

#[tokio::test]
async fn fixed_models_honour_an_explicit_launch_floor_below_their_size() {
    // A model that would fit is still refused when the floor is not met:
    // the floor is a launch guard, not a size calculation.
    let (handle, runtime) = harness5(
        small_card(),
        &[
            (
                "small",
                "mock",
                CategoryId::Work,
                Some(2.0),
                "launch_only_with_at_least_vram_free: 9",
            ),
            ("filler", "mock", CategoryId::Rp, Some(4.0), ""),
        ],
    );
    handle
        .swap("filler", CategoryId::Rp)
        .await
        .expect("filler loads");
    // free = 10 - 2 - 4 = 4 < floor 9 (RP's 4 GiB evictable but insufficient).
    let error = handle
        .swap("small", CategoryId::Work)
        .await
        .expect_err("floor refuses a model that would otherwise fit");
    assert!(error.to_string().contains("launch floor"), "{error}");
    assert_eq!(runtime.server_count(), 1);
}

#[tokio::test]
async fn greedy_with_unknown_size_books_the_room() {
    // Greedy + scheme sentinel: no size to declare, no estimate — still loads.
    let (handle, _runtime) = harness5(
        small_card(),
        &[(
            "ghost",
            "mock",
            CategoryId::Work,
            None,
            "cost: greedy\nmodel: scheme://strata/models/ghost",
        )],
    );
    handle
        .swap("ghost", CategoryId::Work)
        .await
        .expect("greedy needs no size");
    wait_until(&handle, 5, |s| {
        s.slots
            .iter()
            .any(|sl| sl.category == CategoryId::Work && sl.model.as_deref() == Some("ghost"))
    })
    .await;
    let status = handle.status().await.expect("alive");
    let work = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Work)
        .expect("WORK slot");
    assert_eq!(work.need_gib, Some(8.0), "books the whole room (10-2)");
}

#[tokio::test]
async fn exact_refuses_without_a_declared_number() {
    // `exact` claims engine-computed precision — guessing is refused, not estimated.
    let (handle, _runtime) = harness5(
        small_card(),
        &[(
            "faith",
            "mock",
            CategoryId::Work,
            None,
            "cost: exact\nmodel: F:/x.gguf",
        )],
    );
    let error = handle
        .swap("faith", CategoryId::Work)
        .await
        .expect_err("exact without vram_gb is a refusal");
    assert!(error.to_string().contains("exact"), "{error}");
    assert!(
        matches!(
            error,
            SchedulerError::Admission(simplehar_se::budget::AdmissionError::ExactUndeclared)
        ),
        "{error}"
    );
}

#[tokio::test]
async fn exact_with_a_number_gates_like_fixed() {
    let (handle, runtime) = harness5(
        small_card(),
        &[("nine", "mock", CategoryId::Work, Some(9.0), "cost: exact")],
    );
    // free = 10 - 2 = 8 < 9 -> Insufficient (exact numbers are binding).
    let error = handle
        .swap("nine", CategoryId::Work)
        .await
        .expect_err("exact numbers bind the gate");
    assert!(error.to_string().contains("insufficient VRAM"), "{error}");
    assert_eq!(runtime.server_count(), 0);
}

// --- launch validation precedes eviction (chunk 10 [2], 2026-10-27) -----------

/// Pins the refusal of a load the adapter cannot build a spec for: terminal at the
/// admission stage, naming what it refused, and leaving the card exactly as it was —
/// the resident model still serving and nothing evicted.
///
/// # Arguments
///
/// * `status` - the snapshot taken after the refusal.
/// * `bad_model` - the model whose yaml the adapter refuses.
/// * `resident_category` / `resident_model` - the occupant that was still on the card
///   (the same-slot replacement or the planned cross-category victim).
/// * `names` - the fragment of the error that must name the refused key or value.
fn assert_refused_without_damage(
    status: &simplehar_se::scheduler::SchedulerStatus,
    bad_model: &str,
    resident_category: CategoryId,
    resident_model: &str,
    names: &str,
) {
    let job = status
        .jobs
        .iter()
        .find(|j| j.model == bad_model)
        .expect("the refused load is in the job ledger");
    assert!(job.terminal, "the refusal must be terminal: {job:?}");
    assert_eq!(
        slot_model(status, resident_category),
        vec![Some(resident_model.to_string())],
        "the resident model must still hold its slot"
    );
    assert!(
        job.victims_released.is_empty(),
        "a refused launch may evict nobody: {:?}",
        job.victims_released
    );
    assert_eq!(
        job.state,
        SwapState::AdmitFail,
        "a launch that cannot be built is refused before Quiesce/Dispose: {job:?}"
    );
    assert_eq!(job.outcome.as_deref(), Some("launch refused"), "{job:?}");
    let error = job.error.as_deref().unwrap_or_default();
    assert!(
        error.contains(names),
        "the error must name `{names}`, was: {error}"
    );
}

#[tokio::test]
async fn a_strata_model_without_strata_config_is_refused_before_the_victim_dies() {
    // Chunk 10 [2]: `strata_config` is required by the real adapter and existence
    // checked there. 30 (AUX) + 60 (WORK) > 72 - 2, so admission plans aux30 as the
    // victim: without the pre-Dispose preflight the kill runs first and the refusal
    // arrives over a corpse.
    let (handle, runtime) = harness5(
        config(5),
        &[
            ("aux30", "mock", CategoryId::Aux, Some(30.0), ""),
            ("strata-nocfg", "strata", CategoryId::Work, Some(60.0), ""),
        ],
    );
    let Reply::Loaded(resident) = handle
        .swap("aux30", CategoryId::Aux)
        .await
        .expect("the resident model loads")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(runtime.server_count(), 1, "the resident engine is live");

    let error = handle
        .swap("strata-nocfg", CategoryId::Work)
        .await
        .expect_err("a missing strata_config must refuse the load");
    assert!(
        error.to_string().contains("required field `strata_config`"),
        "the refusal must name the missing key, was: {error}"
    );

    let status = handle.status().await.expect("actor alive");
    assert_refused_without_damage(
        &status,
        "strata-nocfg",
        CategoryId::Aux,
        "aux30",
        "required field `strata_config`",
    );
    assert_eq!(runtime.server_count(), 1, "nothing killed, nothing spawned");
    assert!(
        !runtime.is_dead(resident.port).await,
        "the planned victim must still be answering on its engine port"
    );
}

#[tokio::test]
async fn an_illegal_ninfer_container_name_is_refused_before_the_occupant_dies() {
    // The security refusal of chunk 10 [2]: a `ninfer_container` docker would not
    // accept is rejected by the real adapter — and must be rejected while the model
    // this load replaces is still serving.
    let (handle, runtime) = harness5(
        config(5),
        &[
            ("work-a", "mock", CategoryId::Work, Some(4.0), ""),
            (
                "ninfer-badname",
                "ninfer",
                CategoryId::Work,
                Some(8.0),
                "ninfer_autostart: /opt/autostart.sh\nninfer_container: \"bad container\"",
            ),
        ],
    );
    let Reply::Loaded(resident) = handle
        .swap("work-a", CategoryId::Work)
        .await
        .expect("the resident model loads")
    else {
        panic!("expected Loaded");
    };

    let error = handle
        .swap("ninfer-badname", CategoryId::Work)
        .await
        .expect_err("an illegal container name must refuse the load");
    assert!(
        error
            .to_string()
            .contains("ninfer_container must be a docker name"),
        "the refusal must name the illegal value, was: {error}"
    );

    let status = handle.status().await.expect("actor alive");
    assert_refused_without_damage(
        &status,
        "ninfer-badname",
        CategoryId::Work,
        "work-a",
        "ninfer_container must be a docker name",
    );
    assert_eq!(runtime.server_count(), 1, "nothing killed, nothing spawned");
    assert!(
        !runtime.is_dead(resident.port).await,
        "the replaced model must still be answering on its engine port"
    );
}

#[tokio::test]
async fn a_llama_model_without_its_model_key_is_refused_before_the_occupant_dies() {
    // `model:` is llama's one required key (Appendix A.7); scan accepts the file, so
    // the adapter is the first place that can see it — and the first place it may be
    // seen, per chunk 10 [2].
    let (handle, runtime) = harness5(
        config(5),
        &[
            ("aux-a", "mock", CategoryId::Aux, Some(4.0), ""),
            (
                "llama-no-model",
                "llama",
                CategoryId::Aux,
                Some(8.0),
                "context: 4096",
            ),
        ],
    );
    let Reply::Loaded(resident) = handle
        .swap("aux-a", CategoryId::Aux)
        .await
        .expect("the resident model loads")
    else {
        panic!("expected Loaded");
    };

    let error = handle
        .swap("llama-no-model", CategoryId::Aux)
        .await
        .expect_err("a llama entry with no model path must refuse the load");
    assert!(
        error.to_string().contains("required field `model`"),
        "the refusal must name the missing key, was: {error}"
    );

    let status = handle.status().await.expect("actor alive");
    assert_refused_without_damage(
        &status,
        "llama-no-model",
        CategoryId::Aux,
        "aux-a",
        "required field `model`",
    );
    assert_eq!(runtime.server_count(), 1, "nothing killed, nothing spawned");
    assert!(
        !runtime.is_dead(resident.port).await,
        "the replaced model must still be answering on its engine port"
    );
}

// --- unload during a swap, and the load parked behind it (FINDINGS-02 [2], [3]) -----

/// The model that ends up queued on the category's only slot, behind the swap.
const PARKED_MODEL: &str = "model-c";

/// A ledger-only witness the test can park. The admission decision waits on exactly
/// one `sample()` (Step 4), so a gate with `samples` permits answers that many reads
/// and parks every later one: a load frozen in `Admit` is the state in which a cancel
/// can never prove its victim's release, which is the precondition both findings need.
/// Without it these scenarios would be a race against the witness rather than a state
/// the test holds.
struct GatedWitness {
    gate: Arc<tokio::sync::Semaphore>,
}

impl GatedWitness {
    /// Answers `samples` reads, then parks every later one.
    fn new(samples: usize) -> Self {
        GatedWitness {
            gate: Arc::new(tokio::sync::Semaphore::new(samples)),
        }
    }
}

#[async_trait::async_trait]
impl VramWitness for GatedWitness {
    fn source(&self) -> String {
        "gated-ledger".to_string()
    }

    fn ledger_only(&self) -> bool {
        true
    }

    async fn sample(&self) -> Result<MemorySample, String> {
        let permit = self
            .gate
            .acquire()
            .await
            .map_err(|_| "the test gate is closed".to_string())?;
        permit.forget();
        Err("witness unavailable (ledger-only mode)".to_string())
    }
}

/// The three-model, single-slot WORK fixture both findings need: a resident engine
/// whose terminate can be held, a swap that cannot get past `Admit`, and
/// [`PARKED_MODEL`] to queue behind it. The release deadline is short so an
/// unconfirmed release terminalizes quickly rather than at the default.
fn parked_swap_harness() -> (SchedulerHandle, MockRuntime) {
    let cfg = AppConfig {
        release_timeout_s: 2,
        ..config(10)
    };
    let witness = GatedWitness::new(1); // one sample: the resident load's admission
    let (handle, runtime) = harness5_witness(
        cfg,
        &[
            (
                HOLD_MODEL,
                "mock-hold-kill",
                CategoryId::Work,
                Some(4.0),
                "",
            ),
            (INCOMING_MODEL, "mock", CategoryId::Work, Some(4.0), ""),
            (PARKED_MODEL, "mock", CategoryId::Work, Some(4.0), ""),
        ],
        Arc::new(witness),
    );
    (handle, runtime)
}

/// Serves [`HOLD_MODEL`] and submits the swap to [`INCOMING_MODEL`], waiting until it
/// sits frozen in `Admit` (the gate has no sample left for it), so the victim still
/// holds its slot and a live engine when the operator's cancel lands.
async fn serve_and_submit_frozen_swap(
    handle: &SchedulerHandle,
) -> simplehar_se::scheduler::SwapReceipt {
    let victim = serve_hold_model(handle).await;
    submit_and_vanish(handle, INCOMING_MODEL, CategoryId::Work).await;
    wait_until(handle, 5, |s| {
        s.jobs
            .iter()
            .any(|j| j.model == INCOMING_MODEL && j.state == SwapState::Admit)
    })
    .await;
    victim
}

/// Whether `model` is on the ledger as a queued load: registered, non-terminal, still
/// `Idle` because it has never been admitted.
fn is_queued(status: &SchedulerStatus, model: &str) -> bool {
    status.jobs.iter().any(|j| {
        j.model == model && j.kind == JobKind::Load && !j.terminal && j.state == SwapState::Idle
    })
}

/// The job id of `model`'s row, whichever stage it is in.
fn job_of(status: &SchedulerStatus, model: &str) -> JobId {
    status
        .jobs
        .iter()
        .find(|j| j.model == model)
        .unwrap_or_else(|| panic!("`{model}` must be in the job ledger: {:?}", status.jobs))
        .id
}

#[tokio::test]
async fn unload_during_a_swap_frees_the_slot_after_cancelling_the_swap() {
    // FINDINGS-02 [2]: `unload_slots` chose cancel OR unload. A slot with an occupant
    // *and* an in-flight swap is where that choice loses the card: the cancel's reap
    // cannot prove the victim's release (chunk 4 [1] deliberately keeps an engine
    // nobody proved dead installed), so before the fix the operator was told `Done`
    // while the model stayed loaded and serving.
    let (handle, runtime) = parked_swap_harness();
    let victim = serve_and_submit_frozen_swap(&handle).await;
    assert_eq!(
        runtime.server_count(),
        1,
        "only the resident engine is live"
    );
    // One held kill: the cancel's reap of the victim stays unprovable (chunk 4 [1]
    // keeps the engine installed over it) while the unload that must follow gets a
    // terminate it can prove. Holding by count rather than releasing a gate from the
    // test keeps the second release inside its own deadline instead of racing it.
    runtime.hold_next_kills(1);

    assert!(
        matches!(
            handle
                .unload_model(HOLD_MODEL)
                .await
                .expect("unload accepted"),
            Reply::Done
        ),
        "the unload of a swapping slot is accepted"
    );

    wait_until(&handle, 10, |s| {
        s.jobs
            .iter()
            .any(|j| j.model == INCOMING_MODEL && j.terminal)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let swap = status
        .jobs
        .iter()
        .find(|j| j.model == INCOMING_MODEL)
        .expect("the cancelled swap is in the ledger");
    assert_eq!(swap.outcome.as_deref(), Some("cancelled"), "{swap:?}");
    assert!(
        swap.error
            .as_deref()
            .unwrap_or("")
            .contains("unloaded by request"),
        "{swap:?}"
    );
    assert!(
        !swap.is_fault,
        "an operator's unload is not a fault: {swap:?}"
    );
    // The cancel proved nothing about the victim: no release timing, no released
    // victim. Whatever freed the slot therefore cannot be attributed to the swap.
    assert!(
        swap.victims_released.is_empty(),
        "the cancelled swap never proved a release: {:?}",
        swap.victims_released
    );
    assert!(
        swap.timings.release_ms.is_none(),
        "and its release stage never completed: {:?}",
        swap.timings
    );
    assert_eq!(
        runtime.spawn_count(),
        1,
        "a swap cancelled in Admit never spawns"
    );

    // The second half of that one unload command: the slot's own unload job, started
    // when the cancel it triggered terminalized. Without it the card stays spent, so
    // this wait is the falsifier: pre-fix the slot keeps its occupant and live engine.
    wait_until(&handle, 10, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let unload = status
        .jobs
        .iter()
        .find(|j| j.kind == JobKind::Unload && j.model == HOLD_MODEL)
        .expect("one unload must have run for the slot the cancel left occupied");
    assert_eq!(unload.outcome.as_deref(), Some("unloaded"), "{unload:?}");
    assert!(
        unload.timings.release_ms.is_some(),
        "the unload proved its own release: {unload:?}"
    );
    let swap = status
        .jobs
        .iter()
        .find(|j| j.model == INCOMING_MODEL)
        .expect("the cancelled swap is in the ledger");
    assert!(
        unload.id > swap.id,
        "the unload followed the cancel it was waiting on: {:?} then {:?}",
        swap.id,
        unload.id
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "a single unload must leave no live engine behind"
    );
    assert!(
        runtime.is_dead(victim.port).await,
        "and no engine answering on the freed slot's port"
    );
}

#[tokio::test]
async fn unload_replies_done_before_any_release_evidence() {
    // FINDINGS-02 [2], second half: the reply honesty pin. `unload_slots` answers
    // `Reply::Done` as soon as the commands are dispatched, while the kill it
    // triggered is still in flight, so a client cannot tell a freed slot from one
    // whose release is unproven. This pins what the API returns TODAY: making the
    // reply carry release evidence is API surgery outside this finding's approved fix
    // (`Reply::Done` has no payload and `/har/unload` hands back no job id to poll).
    let (handle, runtime) = parked_swap_harness();
    serve_and_submit_frozen_swap(&handle).await;

    handle
        .unload_model(HOLD_MODEL)
        .await
        .expect("unload accepted");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        slot_model(&status, CategoryId::Work),
        vec![Some(HOLD_MODEL.to_string())],
        "the Done the caller already holds arrived with the slot still occupied"
    );
    assert_eq!(
        runtime.server_count(),
        1,
        "and with the engine still live: no release had been proven yet"
    );
}

#[tokio::test]
async fn a_parked_load_is_answered_when_its_predecessor_keeps_the_occupant() {
    // FINDINGS-02 [3], the reviewer's experiment: A serving, swap B frozen in Admit,
    // swap C parked behind it, B cancelled. B terminalizes with A still installed, so
    // C's queue place can never be satisfied: it must be answered, not left hanging,
    // and it must not fire later when the operator finally frees the slot.
    let (handle, runtime) = parked_swap_harness();
    serve_and_submit_frozen_swap(&handle).await;
    // Two held kills: parking C cancels B once ("superseded by a newer load") and the
    // operator's cancel below re-issues that same reap. Both stay unprovable, which is
    // what keeps A installed when B terminalizes; the unload that follows the answer
    // then gets a terminate it can prove.
    runtime.hold_next_kills(2);

    let client = handle.clone();
    let parked = tokio::spawn(async move { client.swap(PARKED_MODEL, CategoryId::Work).await });
    wait_until(&handle, 5, |s| is_queued(s, PARKED_MODEL)).await;

    let swap = job_of(&handle.status().await.expect("actor alive"), INCOMING_MODEL);
    // Parking C already cancelled B; this is the operator's own cancel of the same job,
    // which must still terminalize it.
    handle.cancel(swap).await.expect("cancel accepted");

    let answer = tokio::time::timeout(Duration::from_secs(8), parked)
        .await
        .expect("a parked load's waiter must be answered, not left to hang")
        .expect("the parked load's task must not panic");
    let error = answer.expect_err("a parked load whose slot never freed cannot report a load");
    assert!(
        matches!(error, SchedulerError::Cancelled(_)),
        "the parked load must be told it was cancelled, got {error}"
    );
    assert!(
        error.to_string().contains("did not free the slot"),
        "the answer must name why, so a client can act on it: {error}"
    );

    // Freeing the slot now must not resurrect the load its caller was answered about.
    handle
        .unload_model(HOLD_MODEL)
        .await
        .expect("unload accepted");
    wait_until(&handle, 10, |s| {
        s.jobs.iter().all(|j| j.terminal)
            && slot_model(s, CategoryId::Work).iter().all(Option::is_none)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        slot_model(&status, CategoryId::Work),
        vec![None],
        "the unload must have freed the slot"
    );
    assert!(
        !is_queued(&status, PARKED_MODEL),
        "the answered load must no longer hold the slot's queue place"
    );
    assert_eq!(
        runtime.server_count(),
        0,
        "an abandoned parked load must not start when the slot is finally freed"
    );
    let parked_row = status
        .jobs
        .iter()
        .find(|j| j.model == PARKED_MODEL)
        .expect("the parked load keeps its ledger row");
    assert_eq!(
        parked_row.outcome.as_deref(),
        Some("cancelled"),
        "{parked_row:?}"
    );
    assert!(
        !parked_row.is_fault,
        "a busy card is not an engine fault: {parked_row:?}"
    );
}

#[tokio::test]
async fn a_parked_load_has_a_queued_ledger_row_before_it_starts() {
    // FINDINGS-02 [3], the ledger half: `/har/jobs` is what F-5 exists to make honest,
    // and a parked load used to have no row at all while its request hung. The row it
    // gets at park time is the same one it runs on when it does start, and the one
    // the answered error is recorded against.
    let (handle, _runtime) = parked_swap_harness();
    serve_and_submit_frozen_swap(&handle).await;
    submit_and_vanish(&handle, PARKED_MODEL, CategoryId::Work).await;
    wait_until(&handle, 5, |s| is_queued(s, PARKED_MODEL)).await;

    let status = handle.status().await.expect("actor alive");
    let parked = status
        .jobs
        .iter()
        .find(|j| j.model == PARKED_MODEL)
        .expect("a parked load must have a ledger row while its caller waits");
    assert_eq!(parked.kind, JobKind::Load, "a queued load is a load row");
    assert!(!parked.terminal, "a queued load has not finished");
    assert_eq!(
        parked.state,
        SwapState::Idle,
        "queued: registered, never admitted ({parked:?})"
    );
    assert!(
        parked.error.is_none(),
        "a queued load has failed nothing: {parked:?}"
    );
    let swap = job_of(&status, INCOMING_MODEL);
    assert!(
        status
            .slots
            .iter()
            .any(|s| s.category == CategoryId::Work && s.loading_job == Some(swap)),
        "the slot still runs the swap this load is parked behind: {:?}",
        status.slots
    );

    handle.cancel(swap).await.expect("cancel accepted");
    wait_until(&handle, 8, |s| {
        s.jobs.iter().any(|j| j.model == PARKED_MODEL && j.terminal)
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let parked = status
        .jobs
        .iter()
        .find(|j| j.model == PARKED_MODEL)
        .expect("the parked load's row is kept after it is dropped");
    assert_eq!(parked.outcome.as_deref(), Some("cancelled"), "{parked:?}");
    assert!(
        parked
            .error
            .as_deref()
            .unwrap_or("")
            .contains("did not free the slot"),
        "the row records why the queued load was dropped: {parked:?}"
    );
    assert!(!parked.is_fault, "{parked:?}");
}
