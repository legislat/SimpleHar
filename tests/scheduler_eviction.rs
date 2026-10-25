//! Cross-category eviction tests (DESIGN Step 6, choice C): a load that does not
//! fit evicts the cheapest sufficient set of evictable victims, releases them one
//! at a time (stopping as soon as the gate is satisfied), and refuses with the
//! named blockers when nothing evictable can cover the deficit.

mod mock_runtime;

use async_trait::async_trait;
use mock_runtime::MockRuntime;
use simplehar_se::budget::{MemorySample, VramWitness};
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scripted witness: samples come off the queue (the last one repeats).
#[derive(Clone, Default)]
struct FakeWitness {
    queue: Arc<Mutex<VecDeque<MemorySample>>>,
}

impl FakeWitness {
    fn with(samples: Vec<MemorySample>) -> Self {
        FakeWitness {
            queue: Arc::new(Mutex::new(samples.into())),
        }
    }
}

#[async_trait]
impl VramWitness for FakeWitness {
    async fn sample(&self) -> Result<MemorySample, String> {
        let mut queue = self.queue.lock().expect("witness queue lock");
        if queue.len() > 1 {
            Ok(queue.pop_front().expect("len checked"))
        } else {
            Ok(queue.front().cloned().unwrap_or(MemorySample {
                total_gib: 72.0,
                used_gib: 0.0,
                processes: vec![],
            }))
        }
    }
}

fn sample(total: f64, used: f64) -> MemorySample {
    MemorySample {
        total_gib: total,
        used_gib: used,
        processes: vec![],
    }
}

type Spec = (&'static str, &'static str, CategoryId, f64);

/// Writes model yamls into a fresh temp ModelConf dir (with per-model VRAM).
fn write_modelconf(specs: &[Spec]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "se-evict-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp ModelConf dir");
    for (name, engine, category, vram) in specs {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: {engine}\ncategory: {category}\nvram_gb: {vram}\n"),
        )
        .expect("model yaml");
    }
    dir
}

fn harness(aux_slots: usize, specs: &[Spec], witness: Arc<dyn VramWitness>) -> SchedulerHandle {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let mut cfg = AppConfig {
        settle_timeout_s: 5,
        ..AppConfig::default()
    };
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = aux_slots;
    Scheduler::start(
        cfg,
        write_modelconf(specs),
        adapters,
        Arc::new(runtime) as Arc<dyn EngineRuntime>,
        witness,
    )
}

/// Polls status until `cond` holds (used by suites sharing this harness shape;
/// some binaries may not call it).
#[allow(dead_code)]
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

fn model_of(
    status: &simplehar_se::scheduler::SchedulerStatus,
    category: CategoryId,
) -> Option<String> {
    status
        .slots
        .iter()
        .find(|slot| slot.category == category)
        .and_then(|slot| slot.model.clone())
}

#[tokio::test]
async fn work_load_evicts_aux_victim_when_short() {
    // Card 72, reserve 2: AUX holds 30 GiB; WORK wants 50 -> free 40, deficit 10,
    // the AUX victim (evictable by WORK) is released, then WORK serves.
    let witness = FakeWitness::with(vec![sample(72.0, 30.0), sample(72.0, 0.0)]);
    let handle = harness(
        1,
        &[
            ("aux30", "mock", CategoryId::Aux, 30.0),
            ("work50", "mock", CategoryId::Work, 50.0),
        ],
        Arc::new(witness),
    );

    let Reply::Loaded(_) = handle
        .swap("aux30", CategoryId::Aux)
        .await
        .expect("aux loads")
    else {
        panic!("expected Loaded");
    };
    let Reply::Loaded(receipt) = handle
        .swap("work50", CategoryId::Work)
        .await
        .expect("work loads after evicting")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "work50");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work50")
    );
    assert_eq!(
        model_of(&status, CategoryId::Aux),
        None,
        "the victim is gone"
    );
    // The ledger records the eviction (G13 decision inputs).
    let swap_job = status
        .jobs
        .iter()
        .find(|job| job.model == "work50")
        .expect("swap job recorded");
    assert_eq!(swap_job.victims_released, vec!["aux30".to_string()]);
}

#[tokio::test]
async fn eviction_picks_the_cheapest_sufficient_victim() {
    // Two AUX slots: 12 GiB and 20 GiB loaded (ledger 32, free 38). WORK 53 ->
    // deficit 15: the 12 alone does not cover, 12+20 overshoots - the 20 alone is
    // the cheapest sufficient set, and the 12 must survive (minimality).
    let witness = FakeWitness::with(vec![
        sample(72.0, 32.0), // admit: aux12 + aux20 loaded
        sample(72.0, 12.0), // settle after the 20 was released: only aux12 left
    ]);
    let handle = harness(
        2,
        &[
            ("aux12", "mock", CategoryId::Aux, 12.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("work53", "mock", CategoryId::Work, 53.0),
        ],
        Arc::new(witness),
    );

    handle
        .swap("aux12", CategoryId::Aux)
        .await
        .expect("aux12 loads");
    handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("aux20 loads");
    let Reply::Loaded(_) = handle
        .swap("work53", CategoryId::Work)
        .await
        .expect("work loads")
    else {
        panic!("expected Loaded");
    };

    let status = handle.status().await.expect("actor alive");
    let aux_models: Vec<Option<String>> = status
        .slots
        .iter()
        .filter(|slot| slot.category == CategoryId::Aux)
        .map(|slot| slot.model.clone())
        .collect();
    assert!(
        aux_models.contains(&Some("aux12".to_string())),
        "the cheaper insufficient victim must survive: {aux_models:?}"
    );
    assert!(
        !aux_models.contains(&Some("aux20".to_string())),
        "the sufficient victim was evicted: {aux_models:?}"
    );
    let swap_job = status
        .jobs
        .iter()
        .find(|job| job.model == "work53")
        .expect("swap job");
    assert_eq!(swap_job.victims_released, vec!["aux20".to_string()]);
}

#[tokio::test]
async fn rp_load_cannot_evict_work_and_refuses_with_blockers() {
    // WORK holds 50; RP wants 50: free 20, deficit 30, and the only candidate
    // (WORK) is not evictable by RP -> refusal naming the blocker and the rule.
    let witness = FakeWitness::with(vec![sample(72.0, 50.0)]);
    let handle = harness(
        1,
        &[
            ("work50", "mock", CategoryId::Work, 50.0),
            ("rp50", "mock", CategoryId::Rp, 50.0),
        ],
        Arc::new(witness),
    );

    handle
        .swap("work50", CategoryId::Work)
        .await
        .expect("work loads");
    let error = handle
        .swap("rp50", CategoryId::Rp)
        .await
        .expect_err("RP cannot evict WORK");
    let message = error.to_string();
    assert!(message.contains("insufficient VRAM"), "was: {message}");
    assert!(message.contains("blocked by: work50"), "was: {message}");
    // WORK.evictable_by is empty: nothing may evict it.
    assert!(message.contains("nothing may evict WORK"), "was: {message}");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Rp),
        None,
        "refused load leaves the slot empty"
    );
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work50")
    );
}

#[tokio::test]
async fn aux_load_cannot_evict_anything_cross_category() {
    // AUX x2 holds 30 + 20 (ledger 50, free 20); a 55 GiB AUX load replaces its
    // LRU own occupant (allowed) but still falls 5 short - and AUX may evict
    // nobody outside its own slots: refuse, both AUX occupants survive.
    let witness = FakeWitness::with(vec![sample(72.0, 50.0)]);
    let handle = harness(
        2,
        &[
            ("aux30", "mock", CategoryId::Aux, 30.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("aux55", "mock", CategoryId::Aux, 55.0),
        ],
        Arc::new(witness),
    );

    handle
        .swap("aux30", CategoryId::Aux)
        .await
        .expect("aux30 loads");
    handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("aux20 loads");
    let error = handle
        .swap("aux55", CategoryId::Aux)
        .await
        .expect_err("AUX may not evict other categories");
    assert!(error
        .to_string()
        .contains("insufficient VRAM: need 55.0 GiB, free 20.0 GiB"));
    let status = handle.status().await.expect("actor alive");
    let aux_models: Vec<Option<String>> = status
        .slots
        .iter()
        .filter(|slot| slot.category == CategoryId::Aux)
        .map(|slot| slot.model.clone())
        .collect();
    assert!(
        aux_models.contains(&Some("aux30".to_string())),
        "{aux_models:?}"
    );
    assert!(
        aux_models.contains(&Some("aux20".to_string())),
        "{aux_models:?}"
    );
}
