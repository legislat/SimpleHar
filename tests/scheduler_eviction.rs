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
/// `live` — when set — overrides the per-process table at sample time, so a
/// test can attribute a pid it only learns after a spawn (install-measure
/// retries) without fighting the queue for pop order.
type Procs = Vec<(u32, f64)>;

#[derive(Clone, Default)]
struct FakeWitness {
    queue: Arc<Mutex<VecDeque<MemorySample>>>,
    live: Arc<Mutex<Option<Procs>>>,
}

impl FakeWitness {
    fn with(samples: Vec<MemorySample>) -> Self {
        FakeWitness {
            queue: Arc::new(Mutex::new(samples.into())),
            live: Arc::new(Mutex::new(None)),
        }
    }

    fn set_procs(&self, procs: Procs) {
        *self.live.lock().expect("witness lock") = Some(procs);
    }
}

#[async_trait]
impl VramWitness for FakeWitness {
    async fn sample(&self) -> Result<MemorySample, String> {
        let mut base = {
            let mut queue = self.queue.lock().expect("witness queue lock");
            if queue.len() > 1 {
                queue.pop_front().expect("len checked")
            } else {
                queue.front().cloned().unwrap_or(MemorySample {
                    total_gib: 72.0,
                    used_gib: 0.0,
                    processes: vec![],
                })
            }
        };
        if let Some(procs) = self.live.lock().expect("witness lock").as_ref() {
            base.processes = procs.clone();
        }
        Ok(base)
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
    // Script tracks the ledger: the card is empty until work50 lands (the
    // observed gate refuses loads onto a card the witness sees as busy).
    let witness = FakeWitness::with(vec![sample(72.0, 0.0), sample(72.0, 50.0)]);
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
    let witness = FakeWitness::with(vec![sample(72.0, 30.0), sample(72.0, 50.0)]);
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

/// ModelConf writer with raw extra lines per model (cost/floor keys).
fn write_modelconf_ext(entries: &[(&str, &[&str])]) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "se-observed-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp ModelConf dir");
    for (name, lines) in entries {
        std::fs::write(dir.join(format!("{name}.yaml")), lines.join("\n")).expect("model yaml");
    }
    dir
}

fn harness_ext(
    specs: &[(&str, &[&str])],
    witness: Arc<dyn VramWitness>,
    settle_timeout_s: u64,
) -> SchedulerHandle {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let mut cfg = AppConfig {
        settle_timeout_s,
        ..AppConfig::default()
    };
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = 1;
    Scheduler::start(
        cfg,
        write_modelconf_ext(specs),
        adapters,
        Arc::new(runtime) as Arc<dyn EngineRuntime>,
        witness,
    )
}

#[tokio::test]
async fn greedy_replacement_clears_floor_after_victim_release() {
    // Operator case 2026-10-26: WORK holds a 27.7 GiB llama (card shows 35.4 used
    // by it); a greedy strata with a 65 GiB launch floor must be ALLOWED - the
    // replacement victim dies first, so the floor is judged on the fresh observed
    // free at settle, never on the pre-release reading that still counts the
    // victim's own memory.
    let witness = FakeWitness::with(vec![
        sample(72.0, 35.4), // holder admit
        sample(72.0, 35.4), // chonky admit: victim planned -> no pre-release clamp
        sample(72.0, 0.0),  // settle: card actually emptied
    ]);
    let handle = harness_ext(
        &[
            (
                "holder",
                &[
                    "name: holder",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 27.7",
                ],
            ),
            (
                "chonky",
                &[
                    "name: chonky",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 55",
                    "cost: greedy",
                    "launch_only_with_at_least_vram_free: 65",
                ],
            ),
        ],
        Arc::new(witness),
        5,
    );
    handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("holder loads");
    let Reply::Loaded(receipt) = handle
        .swap("chonky", CategoryId::Work)
        .await
        .expect("greedy replaces after the release clears the floor")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "chonky");
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("chonky")
    );
    let job = status
        .jobs
        .iter()
        .find(|job| job.model == "chonky")
        .expect("job recorded");
    assert_eq!(job.gate_target_gib, Some(65.0), "the floor was the gate");
    assert!(
        job.observed_at_admit_gib.is_some(),
        "witness evidence recorded for post-checks"
    );
}

#[tokio::test]
async fn settle_refuses_when_observed_card_stays_busy() {
    // Same replacement, but the card never empties (foreign holder keeps 20 GiB):
    // the release is confirmed, yet the FRESH observed free sits below the floor
    // - the load is refused at settle with an explanatory error, engine unspawned.
    let witness = FakeWitness::with(vec![
        sample(72.0, 35.4),
        sample(72.0, 35.4),
        sample(72.0, 20.0),
    ]);
    let handle = harness_ext(
        &[
            (
                "holder",
                &[
                    "name: holder",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 27.7",
                ],
            ),
            (
                "chonky",
                &[
                    "name: chonky",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 55",
                    "cost: greedy",
                    "launch_only_with_at_least_vram_free: 65",
                ],
            ),
        ],
        Arc::new(witness),
        3,
    );
    handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("holder loads");
    handle
        .swap("chonky", CategoryId::Work)
        .await
        .expect_err("card stays busy: the observed gate refuses");
    let status = handle.status().await.expect("actor alive");
    assert_eq!(model_of(&status, CategoryId::Work), None, "nothing serves");
    let job = status
        .jobs
        .iter()
        .find(|job| job.model == "chonky")
        .expect("job recorded");
    let error = job.error.clone().expect("failed job carries the reason");
    assert!(
        error.contains("below the required"),
        "explanatory settle error: {error}"
    );
}

#[tokio::test]
async fn fresh_load_refused_on_physically_full_card() {
    // Ledger says 15 GiB free (55 booked, no external attribution) but the card
    // is observed at 70.5/72: a fixed 10 GiB AUX load has no victim to release,
    // so the observed clamp applies at admit and refuses - over-booking from a
    // flattering ledger is the "lying ledger" case (operator 2026-10-26).
    let witness = FakeWitness::with(vec![sample(72.0, 1.0), sample(72.0, 70.5)]);
    let handle = harness_ext(
        &[
            (
                "resident",
                &[
                    "name: resident",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 55",
                ],
            ),
            (
                "want10",
                &[
                    "name: want10",
                    "engine: mock",
                    "category: AUX",
                    "vram_gb: 10",
                ],
            ),
        ],
        Arc::new(witness),
        5,
    );
    handle
        .swap("resident", CategoryId::Work)
        .await
        .expect("resident loads");
    handle
        .swap("want10", CategoryId::Aux)
        .await
        .expect_err("1.5 GiB physically free cannot host 10");
    let status = handle.status().await.expect("actor alive");
    assert_eq!(model_of(&status, CategoryId::Aux), None);
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("resident")
    );
}

#[tokio::test]
async fn install_records_measured_footprint_and_persists_it() {
    // Operator 2026-10-26: a greedy model's nominal size is not its footprint.
    // The install-time witness sample measures it, the catalog shows the truth,
    // and the model yaml records `measured_gb` byte-preservingly for later boots.
    let dir = write_modelconf_ext(&[(
        "stratoid",
        &[
            "name: stratoid",
            "engine: mock",
            "category: WORK",
            "vram_gb: 55",
            "cost: greedy",
            "launch_only_with_at_least_vram_free: 65",
        ],
    )]);
    // Attribution is live-settled by the witness: a decoy entry keeps the
    // measure task retrying until the test scripts the real pid (no pop-order
    // race, no guessed pid).
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    witness.set_procs(vec![(999_999, 0.0)]);
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let cfg = AppConfig {
        settle_timeout_s: 5,
        ..AppConfig::default()
    };
    let handle = Scheduler::start(
        cfg,
        dir.clone(),
        adapters,
        Arc::new(MockRuntime::with_unique_pids(41_000)) as Arc<dyn EngineRuntime>,
        Arc::new(witness.clone()),
    );
    handle
        .swap("stratoid", CategoryId::Work)
        .await
        .expect("greedy loads on an idle card");
    let status = handle.status().await.expect("actor alive");
    let pid = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Work)
        .and_then(|s| s.pid)
        .expect("occupant pid surfaced");
    witness.set_procs(vec![(pid, 70.5)]);

    let yaml = dir.join("stratoid.yaml");
    let mut written = None;
    for _ in 0..50 {
        if let Ok(text) = std::fs::read_to_string(&yaml) {
            if text.contains("measured_gb:") {
                written = Some(text);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let text = written.expect("measured_gb written back to the yaml");
    assert!(text.contains("measured_gb: 70.5"), "{text}");
    assert!(
        text.contains("cost: greedy"),
        "write-back preserves the file"
    );
    let catalog = handle.catalog().await.expect("catalog");
    let entry = catalog
        .models
        .iter()
        .find(|m| m.name == "stratoid")
        .expect("entry");
    assert_eq!(entry.measured_gb, Some(70.5));
}

#[tokio::test]
async fn measured_charges_only_the_installed_engine() {
    // Review BLOCKER 2026-10-26: measuring the union of all harness pids would
    // charge a co-resident tenant's memory to the new model (45 recorded as 75).
    let dir = write_modelconf_ext(&[
        (
            "decoy",
            &[
                "name: decoy",
                "engine: mock",
                "category: AUX",
                "vram_gb: 30",
            ],
        ),
        (
            "target",
            &[
                "name: target",
                "engine: mock",
                "category: WORK",
                "vram_gb: 30",
            ],
        ),
    ]);
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    witness.set_procs(vec![(999_999, 0.0)]);
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let cfg = AppConfig {
        settle_timeout_s: 5,
        ..AppConfig::default()
    };
    let handle = Scheduler::start(
        cfg,
        dir.clone(),
        adapters,
        Arc::new(MockRuntime::with_unique_pids(41_000)) as Arc<dyn EngineRuntime>,
        Arc::new(witness.clone()),
    );
    handle
        .swap("decoy", CategoryId::Aux)
        .await
        .expect("decoy loads");
    handle
        .swap("target", CategoryId::Work)
        .await
        .expect("target loads");
    let status = handle.status().await.expect("actor alive");
    let pid_of = |category: CategoryId| {
        status
            .slots
            .iter()
            .find(|s| s.category == category)
            .and_then(|s| s.pid)
            .expect("occupant pid surfaced")
    };
    let p_decoy = pid_of(CategoryId::Aux);
    let p_target = pid_of(CategoryId::Work);
    witness.set_procs(vec![(p_decoy, 30.0), (p_target, 30.0)]);

    let yaml = dir.join("target.yaml");
    let mut text = String::new();
    for _ in 0..50 {
        if let Ok(read) = std::fs::read_to_string(&yaml) {
            if read.contains("measured_gb:") {
                text = read;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        text.contains("measured_gb: 30.0"),
        "only the installed engine is charged: {text}"
    );
    assert!(
        !text.contains("60.0"),
        "the decoy is never folded in: {text}"
    );
}
