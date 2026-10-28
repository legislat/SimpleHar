//! Discovery and crash-retry tests (Step 10 F-4, T10.1, A9): the engine's
//! self-reported id wins for proxy rewriting with a recorded warning on yaml
//! mismatch; a dead engine's restart installs a fresh, correctly-identified
//! engine; a genuine upstream error is never retried. The end-to-end mid-request
//! retry (proxy send → connect failure → restart → retry) is asserted in
//! `tests/api_surface.rs` (`dead_engine_connect_failure_restarts_exactly_once`),
//! where the request is really made.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::budget::LedgerOnlyWitness;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime, LaunchSpec, ReadinessSpec, Spawned};
use simplehar_se::ownership::{ResourceKind, SpawnTicket};
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Writes model yamls (with optional serve_model) into a temp ModelConf dir.
fn write_modelconf(
    specs: &[(&'static str, &'static str, CategoryId, f64, Option<&str>)],
) -> PathBuf {
    let dir = simplehar_se::testutil::temp_dir("discover");
    for (name, engine, category, vram, serve_model) in specs {
        let mut yaml =
            format!("name: {name}\nengine: {engine}\ncategory: {category}\nvram_gb: {vram}\n");
        if let Some(serve) = serve_model {
            yaml.push_str(&format!("serve_model: {serve}\n"));
        }
        std::fs::write(dir.join(format!("{name}.yaml")), yaml).expect("model yaml");
    }
    dir
}

fn harness(
    cfg: AppConfig,
    specs: &[(&'static str, &'static str, CategoryId, f64, Option<&str>)],
) -> (SchedulerHandle, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    for engine in ["mock-dies", "mock-error", "mock-nodiscovery"] {
        adapters.insert(engine.to_string(), Arc::new(MockAdapter));
    }
    // FINDINGS-20 [6]: two engines with DISTINCT self-reported ids, so a serve_id
    // crossed between slots is visible instead of hiding behind one constant.
    for (id, serve_id) in [("mock-id-a", "engine-alpha"), ("mock-id-b", "engine-beta")] {
        adapters.insert(
            id.to_string(),
            Arc::new(mock_runtime::MockIdAdapter {
                engine: id,
                serve_id,
            }) as Arc<dyn EngineAdapter>,
        );
    }
    let handle = Scheduler::start(
        cfg,
        write_modelconf(specs),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(LedgerOnlyWitness),
    );
    (handle, runtime)
}

fn config() -> AppConfig {
    AppConfig {
        load_timeout_s: 5,
        ..AppConfig::default()
    }
}

async fn post(
    handle: &SchedulerHandle,
    model: &str,
    category: CategoryId,
) -> simplehar_se::scheduler::SwapReceipt {
    match handle.swap(model, category).await.expect("swap succeeds") {
        Reply::Loaded(receipt) => receipt,
        other => panic!("expected Loaded, got {other:?}"),
    }
}

#[tokio::test]
async fn discovery_adopts_the_engines_real_id() {
    // The mock reports its own id `mock-model` at /v1/models; with no yaml claim,
    // the receipt and slot carry the discovered id (F-4 fixed).
    let (handle, _runtime) = harness(config(), &[("m1", "mock", CategoryId::Work, 4.0, None)]);
    let receipt = post(&handle, "m1", CategoryId::Work).await;
    assert_eq!(receipt.serve_id.as_deref(), Some("mock-model"));

    let status = handle.status().await.expect("actor alive");
    let slot = status
        .slots
        .iter()
        .find(|slot| slot.model.as_deref() == Some("m1"))
        .expect("occupant");
    assert_eq!(slot.serve_id.as_deref(), Some("mock-model"));
}

#[tokio::test]
async fn serve_model_mismatch_is_a_recorded_warning_and_discovery_wins() {
    // The yaml claims `wrong-id`; the engine reports `mock-model`: discovery
    // wins, the mismatch lands in the job's warnings (never trusted, F-4).
    let (handle, _runtime) = harness(
        config(),
        &[("m2", "mock", CategoryId::Work, 4.0, Some("wrong-id"))],
    );
    let _ = post(&handle, "m2", CategoryId::Work).await;

    let status = handle.status().await.expect("actor alive");
    let job = status
        .jobs
        .iter()
        .find(|job| job.model == "m2")
        .expect("job");
    assert!(
        job.warnings
            .iter()
            .any(|warning| warning.contains("wrong-id") && warning.contains("mock-model")),
        "warnings: {:?}",
        job.warnings
    );
    let slot = status
        .slots
        .iter()
        .find(|slot| slot.model.as_deref() == Some("m2"))
        .expect("occupant");
    assert_eq!(slot.serve_id.as_deref(), Some("mock-model"));
}

#[tokio::test]
async fn restart_after_death_installs_a_new_engine() {
    // T10.1's scheduler half. FINDINGS-20 [13]: this used to be named
    // `dead_engine_mid_request_is_restarted_and_retried_once` while sending NO
    // request — a test named for the retry contract that cannot see a retry. The
    // end-to-end mid-request retry (send → connect failure → restart → retry) is
    // asserted where the request is really made, in
    // `api_surface::dead_engine_connect_failure_restarts_exactly_once`; the name and
    // header here now say what THIS test proves: the dead engine is replaced by a
    // fresh one that is correctly re-identified.
    let (handle, runtime) = harness(
        config(),
        &[("dies", "mock-dies", CategoryId::Work, 4.0, None)],
    );
    let first = post(&handle, "dies", CategoryId::Work).await;
    assert_eq!(runtime.spawn_count(), 1, "one engine to die");

    // Wait past the mock's 300 ms self-death window.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let reply = handle
        .restart("dies", CategoryId::Work)
        .await
        .expect("restart succeeds");
    let Reply::Loaded(second) = reply else {
        panic!("expected Loaded, got {reply:?}");
    };

    // FINDINGS-20 [8]: the old `assert_ne!(first.port, second.port)` could not fail
    // (the fake binds ephemeral OS ports the scheduler reads back verbatim,
    // src/scheduler/mod.rs `(Spawn, Spawned(Ok))` overwrites the ticket port) and
    // COULD flake (the OS may hand the same ephemeral port back). The honest pins
    // are the ones consumers read: the receipt names exactly the port the slot
    // reports, the corpse really stopped answering, the replacement really started,
    // and it re-identified itself.
    let status = handle.status().await.expect("actor alive");
    let slot = status
        .slots
        .iter()
        .find(|slot| slot.model.as_deref() == Some("dies"))
        .expect("the restarted model serves again");
    assert_eq!(
        slot.port,
        Some(second.port),
        "the receipt carries the port the slot actually serves — a stale-port \
         restart hands the client a receipt pointing somewhere else"
    );
    assert_eq!(
        slot.serve_id.as_deref(),
        Some("mock-model"),
        "the fresh engine re-identified itself (post-restart identity)"
    );
    assert_eq!(
        second.serve_id.as_deref(),
        Some("mock-model"),
        "the receipt carries that same discovered id"
    );
    assert!(runtime.is_dead(first.port).await, "the corpse is dead");
    assert_eq!(
        runtime.spawn_count(),
        2,
        "restart really spawned a second engine"
    );
}

#[tokio::test]
async fn restart_of_a_missing_model_falls_back_to_a_plain_load() {
    // The idle sweep may have reaped the dead engine before the retry arrives:
    // Restart must degrade to a normal load, not error.
    let (handle, _runtime) = harness(
        config(),
        &[("phantom", "mock", CategoryId::Work, 4.0, None)],
    );
    let reply = handle
        .restart("phantom", CategoryId::Work)
        .await
        .expect("restart ok");
    assert!(matches!(reply, Reply::Loaded(_)));
}

#[tokio::test]
async fn two_loaded_engines_carry_distinct_serve_ids() {
    // FINDINGS-20 [6]: with one fixed id for every engine, a product bug that
    // forwarded slot A's `serve_id` for slot B — the crash-retry staleness class
    // fixed in b540c30 — compared equal against the same constant. Each engine here
    // reports its own id; the slot and receipt must carry THEIR OWN engine's id.
    let mut cfg = config();
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = 2;
    let (handle, _runtime) = harness(
        cfg,
        &[
            ("a1", "mock-id-a", CategoryId::Aux, 4.0, None),
            ("a2", "mock-id-b", CategoryId::Aux, 4.0, None),
        ],
    );
    let ra = post(&handle, "a1", CategoryId::Aux).await;
    let rb = post(&handle, "a2", CategoryId::Aux).await;
    assert_eq!(ra.serve_id.as_deref(), Some("engine-alpha"));
    assert_eq!(rb.serve_id.as_deref(), Some("engine-beta"));
    assert_ne!(
        ra.serve_id, rb.serve_id,
        "identity is per-engine, not one constant"
    );

    let status = handle.status().await.expect("actor alive");
    for (model, expect) in [("a1", "engine-alpha"), ("a2", "engine-beta")] {
        let slot = status
            .slots
            .iter()
            .find(|slot| slot.model.as_deref() == Some(model))
            .unwrap_or_else(|| panic!("{model} installed"));
        assert_eq!(
            slot.serve_id.as_deref(),
            Some(expect),
            "{model}'s slot must carry {model}'s own engine id"
        );
    }
}

#[tokio::test]
async fn a_silent_engine_adopts_no_id_and_records_no_false_warning() {
    // FINDINGS-20 [6], the `serve_id == None` branch: an engine that 404s
    // `/v1/models` yields no discovered id. `install` only records a mismatch
    // warning when BOTH the yaml claim and discovery produced an id, so a claimed
    // id beside a silent engine must leave the slot with NO id (never the claimed
    // one laundered into truth) and no bogus warning — the pre-fix shape where a
    // `Some("claimed-id")` here would forward an id the engine never reported.
    let (handle, _runtime) = harness(
        config(),
        &[(
            "silent",
            "mock-nodiscovery",
            CategoryId::Work,
            4.0,
            Some("claimed-id"),
        )],
    );
    let receipt = post(&handle, "silent", CategoryId::Work).await;
    assert_eq!(
        receipt.serve_id, None,
        "discovery found nothing; nothing is adopted"
    );

    let status = handle.status().await.expect("actor alive");
    let slot = status
        .slots
        .iter()
        .find(|slot| slot.model.as_deref() == Some("silent"))
        .expect("the silent engine still installs");
    assert_eq!(slot.serve_id, None, "no id was discovered, none is claimed");
    let job = status
        .jobs
        .iter()
        .find(|job| job.model == "silent")
        .expect("job");
    assert!(
        job.warnings.is_empty(),
        "the yaml claim beside a silent engine must not fabricate a mismatch \
         comparison it never made: {:?}",
        job.warnings
    );
}

#[tokio::test]
async fn a_self_dying_engine_stops_answering_before_the_fake_calls_it_gone() {
    // FINDINGS-20 [7]: `kill` awaited the aborted serve future but `schedule_death`
    // (the `mock-dies` path) did not, so the map-only `release_confirmed` could
    // answer `true` while the corpse's listener was still completing handshakes —
    // exactly the pre-0fa70eb condition that let a connect hang past a release
    // deadline. The fake's word must be the port's word: from the instant
    // `release_confirmed` says gone, the port must refuse. 8 rounds is one per
    // death cycle; the pre-fix window opened exactly once per death, and on the
    // current-thread runtime the reverted ordering was observable only as a slow
    // connect, never a hard failure — this is the structural invariant pin
    // (FINDINGS-20 [7]), not a race-catcher.
    let runtime = MockRuntime::new();
    let spec = LaunchSpec {
        argv: vec!["mock-engine".to_string()],
        cwd: PathBuf::from("."),
        env: std::collections::BTreeMap::from([(
            "MOCK_ENGINE".to_string(),
            "mock-dies".to_string(),
        )]),
        log_name: "discover-selfdeath".to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
    };
    for round in 0..8u32 {
        let ticket = SpawnTicket {
            generation: u64::from(round) + 1,
            kind: ResourceKind::ChildProcess,
            pid: 0,
            port: 0,
        };
        let spawned: Spawned = runtime
            .spawn(&spec, ticket.clone())
            .await
            .expect("mock engine spawns");
        // The scheduler reads the bound port back onto the ticket (mod.rs
        // `(Spawn, Spawned(Ok))`); mirror that here, the fake keys evidence by port.
        let ticket = SpawnTicket {
            port: spawned.port,
            ..ticket
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            if runtime
                .release_confirmed(&ticket)
                .await
                .expect("bookkeeping check")
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "round {round}: the engine must self-die"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            runtime.is_dead(spawned.port).await,
            "round {round}: release_confirmed said gone but the port still answers — \
             the fake's word is not the port's word"
        );
    }
}
