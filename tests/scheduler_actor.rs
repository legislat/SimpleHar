//! Scheduler-actor integration tests against the in-process mock engine (Step 19).
//! Each test pins one v3.3 defect class: F-1 (client abort), F-6 (double load),
//! T3.4 (rapid A->B), T8 (cancel mid-load), A1 (control plane behind a load), plus
//! the admission and supersede contracts.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::llama::LlamaAdapter;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::ninfer::NinferAdapter;
use simplehar_se::engine::strata::StrataAdapter;
use simplehar_se::engine::EngineAdapter;
use simplehar_se::scheduler::{
    JobId, Reply, Scheduler, SchedulerError, SchedulerHandle, SwapState,
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
    // The real adapters, under the ids they themselves serve: a launch validation
    // refusal must come from production code, not from a mock that accepts anything.
    let reals: Vec<Arc<dyn EngineAdapter>> = vec![
        Arc::new(StrataAdapter::default()),
        Arc::new(NinferAdapter::new("Ubuntu", "ops")),
        Arc::new(LlamaAdapter::new(
            PathBuf::from("llama-server.exe"),
            "127.0.0.1".to_string(),
        )),
    ];
    for adapter in reals {
        adapters.insert(adapter.engine_id().to_string(), adapter);
    }
    let handle = Scheduler::start(
        cfg,
        write_modelconf5(specs),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn simplehar_se::engine::EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
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
