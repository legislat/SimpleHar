//! Scheduler-actor integration tests against the in-process mock engine (Step 19).
//! Each test pins one v3.3 defect class: F-1 (client abort), F-6 (double load),
//! T3.4 (rapid A->B), T8 (cancel mid-load), A1 (control plane behind a load), plus
//! the admission and supersede contracts.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::model::ModelEntry;
use simplehar_se::scheduler::{
    JobId, Reply, Scheduler, SchedulerError, SchedulerHandle, SwapState,
};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn config(load_timeout_s: u64) -> AppConfig {
    AppConfig {
        load_timeout_s,
        ..AppConfig::default()
    }
}

fn entry(name: &str, engine: &str, category: CategoryId, vram_gb: Option<f64>) -> ModelEntry {
    ModelEntry {
        name: name.to_string(),
        engine: engine.to_string(),
        category,
        vram_gb,
        model_ref: None,
        file: None,
        raw: BTreeMap::new(),
    }
}

fn harness(cfg: AppConfig, catalog: Vec<ModelEntry>) -> (SchedulerHandle, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters = BTreeMap::new();
    let adapter: Arc<dyn simplehar_se::engine::EngineAdapter> = Arc::new(MockAdapter);
    adapters.insert("mock".to_string(), adapter);
    for engine in ["mock-slow", "mock-dies", "mock-never"] {
        adapters.insert(
            engine.to_string(),
            Arc::new(MockAdapter) as Arc<dyn simplehar_se::engine::EngineAdapter>,
        );
    }
    let handle = Scheduler::start(
        cfg,
        catalog,
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn simplehar_se::engine::EngineRuntime>,
    );
    (handle, runtime)
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
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(5), catalog);

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
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(5), catalog);

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
    let catalog = vec![entry("qwen3", "mock-slow", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(10), catalog);

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
    let catalog = vec![
        entry("model-a", "mock", CategoryId::Work, Some(4.0)),
        entry("model-b", "mock", CategoryId::Work, Some(4.0)),
    ];
    let (handle, runtime) = harness(config(5), catalog);

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
    let catalog = vec![
        entry("model-a", "mock-slow", CategoryId::Work, Some(4.0)),
        entry("model-b", "mock", CategoryId::Work, Some(4.0)),
    ];
    let (handle, runtime) = harness(config(10), catalog);

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
    let catalog = vec![entry("qwen3", "mock-slow", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(10), catalog);

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
    let catalog = vec![entry("qwen3", "mock-slow", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(10), catalog);

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
    let catalog = vec![
        entry("big-a", "mock", CategoryId::Work, Some(50.0)),
        entry("big-b", "mock", CategoryId::Rp, Some(50.0)),
    ];
    let (handle, runtime) = harness(config(5), catalog);

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
    let catalog = vec![entry("ghost", "mock", CategoryId::Work, None)];
    let (handle, _runtime) = harness(config(5), catalog);

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
    let catalog = vec![entry("qwen3", "mock", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(5), catalog);

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
async fn control_plane_answers_while_a_slow_load_runs() {
    // A1: v3.3 held a global lock through wait_ready; SE must not.
    let catalog = vec![
        entry("slow", "mock-slow", CategoryId::Work, Some(4.0)),
        entry("quick", "mock", CategoryId::Rp, Some(4.0)),
    ];
    let (handle, _runtime) = harness(config(10), catalog);

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
    let catalog = vec![entry("broken", "mock-never", CategoryId::Work, Some(4.0))];
    let (handle, runtime) = harness(config(1), catalog);

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
