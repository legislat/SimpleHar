//! Idle-timeout tests (DESIGN Step 5): a category with `idle_timeout_s > 0`
//! unloads unused occupants on the actor's sweep; 0 disables; and the boot
//! measures (§0.2) are observable at `/har/status`.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::budget::LedgerOnlyWitness;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{Scheduler, SchedulerHandle};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn harness(config: AppConfig) -> SchedulerHandle {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let dir = std::env::temp_dir().join(format!("se-idle-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp ModelConf dir");
    for (name, category) in [("idle-me", CategoryId::Rp), ("stayer", CategoryId::Work)] {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: mock\ncategory: {category}\nvram_gb: 4\n"),
        )
        .expect("model yaml");
    }
    Scheduler::start(
        config,
        dir,
        adapters,
        Arc::new(runtime) as Arc<dyn EngineRuntime>,
        Arc::new(LedgerOnlyWitness),
    )
}

fn config(rp_idle_s: u64) -> AppConfig {
    let mut cfg = AppConfig {
        load_timeout_s: 5,
        ..AppConfig::default()
    };
    cfg.category_mut(CategoryId::Rp)
        .expect("default config has RP")
        .idle_timeout_s = rp_idle_s;
    cfg
}

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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn idle_category_unloads_after_the_threshold() {
    let handle = harness(config(1)); // RP idles out after 1 s
    handle.swap("idle-me", CategoryId::Rp).await.expect("loads");
    wait_until(&handle, 8, |status| {
        status.slots.iter().all(|slot| slot.model.is_none())
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let unload_job = status
        .jobs
        .iter()
        .find(|job| job.outcome.as_deref() == Some("idle timeout"))
        .expect("idle unload recorded");
    assert_eq!(unload_job.model, "idle-me");
}

#[tokio::test]
async fn zero_threshold_disables_the_sweep() {
    let handle = harness(config(0)); // RP never idles
    handle.swap("idle-me", CategoryId::Rp).await.expect("loads");
    // Well past two sweep ticks: the occupant must still be there.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let status = handle.status().await.expect("actor alive");
    assert!(
        status
            .slots
            .iter()
            .any(|slot| slot.model.as_deref() == Some("idle-me")),
        "idle_timeout_s = 0 must disable the sweep"
    );
}

#[tokio::test]
async fn activity_resets_the_idle_clock() {
    let handle = harness(config(3)); // RP idles after 3 s of silence
    handle.swap("idle-me", CategoryId::Rp).await.expect("loads");
    // Keep touching through the would-be idle window; the occupant must survive.
    for _ in 0..4 {
        handle.touch("idle-me");
        tokio::time::sleep(Duration::from_millis(1200)).await;
    }
    let status = handle.status().await.expect("actor alive");
    assert!(
        status
            .slots
            .iter()
            .any(|slot| slot.model.as_deref() == Some("idle-me")),
        "activity must reset the idle clock"
    );
}
