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
    /// Overrides `used_gib` on *every* reading. An override, not a queue step:
    /// unrelated samples (the serving-window sampler, a status poll) also pop the
    /// queue, so a scripted sequence silently shifts when anyone else asks.
    used: Arc<Mutex<Option<f64>>>,
}

impl FakeWitness {
    fn with(samples: Vec<MemorySample>) -> Self {
        FakeWitness {
            queue: Arc::new(Mutex::new(samples.into())),
            used: Arc::new(Mutex::new(None)),
        }
    }

    fn set_used(&self, used: f64) {
        *self.used.lock().expect("witness used lock") = Some(used);
    }
}

#[async_trait]
impl VramWitness for FakeWitness {
    fn source(&self) -> String {
        "fake".to_string()
    }

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
                    attribution: simplehar_se::budget::AttributionQuality::Unavailable,
                })
            }
        };
        if let Some(used) = *self.used.lock().expect("witness used lock") {
            base.used_gib = used;
        }
        Ok(base)
    }
}

fn sample(total: f64, used: f64) -> MemorySample {
    MemorySample {
        total_gib: total,
        used_gib: used,
        processes: vec![],
        attribution: simplehar_se::budget::AttributionQuality::Unavailable,
    }
}

type Spec = (&'static str, &'static str, CategoryId, f64);

/// Writes model yamls into a fresh temp ModelConf dir (with per-model VRAM).
fn write_modelconf(specs: &[Spec]) -> PathBuf {
    let dir = simplehar_se::testutil::temp_dir("evict");
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
    harness_rt(aux_slots, specs, witness, 5).0
}

/// `harness` with the settle deadline under test control, returning the runtime so a
/// test can count what was really killed: every spawn and abort happens behind the
/// scheduler's `Arc`, and `MockRuntime` shares that state by `Arc` on clone.
///
/// # Arguments
///
/// * `aux_slots` - `max_models` for AUX (the categories that expand).
/// * `specs` - Model yamls to write (name, engine, category, per-model VRAM).
/// * `witness` - The scripted card.
/// * `settle_timeout_s` - How long a released load may wait for the card to report the
///   bytes back. A test that stages the card by hand needs it generous: the refusal,
///   not the deadline, is what a wrong answer must produce.
///
/// # Returns
///
/// The scheduler handle plus the runtime it drives.
fn harness_rt(
    aux_slots: usize,
    specs: &[Spec],
    witness: Arc<dyn VramWitness>,
    settle_timeout_s: u64,
) -> (SchedulerHandle, MockRuntime) {
    let (handle, runtime, _dir) =
        harness_rt_dir(aux_slots, write_modelconf(specs), witness, settle_timeout_s);
    (handle, runtime)
}

/// [`harness_rt`]'s body with the ModelConf dir caller-owned (stale-marker pins
/// edit yaml bytes through it).
fn harness_rt_dir(
    aux_slots: usize,
    dir: PathBuf,
    witness: Arc<dyn VramWitness>,
    settle_timeout_s: u64,
) -> (SchedulerHandle, MockRuntime, PathBuf) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    // `mock-hold-kill` is the engine id whose kill parks; a release-loop test needs it.
    adapters.insert("mock-hold-kill".to_string(), Arc::new(MockAdapter));
    let mut cfg = AppConfig {
        settle_timeout_s,
        ..AppConfig::default()
    };
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = aux_slots;
    let handle = Scheduler::start(
        cfg,
        dir.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        witness,
    );
    (handle, runtime, dir)
}

/// [`harness_rt_dir`] plus the idle-sweep opt-ins the eviction fixtures never
/// needed: RP and AUX `idle_timeout_s` for the R2-20-01 cross-slot-victim test.
/// The other categories keep the never-idle default, so only those two slots can
/// ever be swept.
fn harness_rt_idle(
    aux_slots: usize,
    specs: &[Spec],
    witness: Arc<dyn VramWitness>,
    settle_timeout_s: u64,
    rp_idle_s: u64,
    aux_idle_s: u64,
) -> (SchedulerHandle, MockRuntime) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    adapters.insert("mock-hold-kill".to_string(), Arc::new(MockAdapter));
    let mut cfg = AppConfig {
        settle_timeout_s,
        ..AppConfig::default()
    };
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .max_models = aux_slots;
    cfg.category_mut(CategoryId::Aux)
        .expect("default config has AUX")
        .idle_timeout_s = aux_idle_s;
    cfg.category_mut(CategoryId::Rp)
        .expect("default config has RP")
        .idle_timeout_s = rp_idle_s;
    let handle = Scheduler::start(
        cfg,
        write_modelconf(specs),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        witness,
    );
    (handle, runtime)
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

/// The newest ledger row of `model`'s job.
fn job_row<'a>(
    status: &'a simplehar_se::scheduler::SchedulerStatus,
    model: &str,
) -> &'a simplehar_se::scheduler::JobStatus {
    status
        .jobs
        .iter()
        .rev()
        .find(|job| job.model == model)
        .unwrap_or_else(|| panic!("`{model}` must be in the job ledger: {:?}", status.jobs))
}

/// Every AUX occupant currently installed.
fn aux_models(status: &simplehar_se::scheduler::SchedulerStatus) -> Vec<Option<String>> {
    status
        .slots
        .iter()
        .filter(|slot| slot.category == CategoryId::Aux)
        .map(|slot| slot.model.clone())
        .collect()
}

#[tokio::test]
async fn work_load_evicts_aux_victim_when_short() {
    // Card 72, reserve 2: AUX holds 30 GiB *and the card says so*; WORK wants 50 ->
    // 40 available, deficit 10, the AUX victim (evictable by WORK) is released, then
    // WORK serves. The readings are consistent on purpose: with the ledger gone the
    // card is the budget, and a card reporting 0 GiB used owes no eviction.
    let witness = FakeWitness::with(vec![sample(72.0, 30.0)]);
    let handle = harness(
        1,
        &[
            ("aux30", "mock", CategoryId::Aux, 30.0),
            ("work50", "mock", CategoryId::Work, 50.0),
        ],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle
        .swap("aux30", CategoryId::Aux)
        .await
        .expect("aux loads")
    else {
        panic!("expected Loaded");
    };
    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work50", CategoryId::Work).await })
    };
    // The victim is queued and the gate is holding: 42 GiB free is still 8 short
    // of the 50 GiB this load is gated on.
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work50" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    witness.set_used(0.0);
    let Reply::Loaded(receipt) = loading
        .await
        .expect("task alive")
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
async fn a_stale_resident_is_evicted_like_any_other() {
    // Interlock 2 (Axis 2, ruling 2026-10-29): the amber is INFO, not policy. A
    // resident the marker has judged stale must be evicted exactly like a clean one
    // — same deficit math, same victim record, no sparing and no preference.
    // Falsifier: filter stale occupants out of the victim pool and the WORK load
    // strands into a refusal here; prefer them and the clean-victim pins fail.
    let witness = FakeWitness::with(vec![sample(72.0, 30.0)]);
    let (handle, _runtime, dir) = harness_rt_dir(
        1,
        write_modelconf(&[
            ("stale30", "mock", CategoryId::Aux, 30.0),
            ("work50", "mock", CategoryId::Work, 50.0),
        ]),
        Arc::new(witness.clone()),
        5,
    );
    let Reply::Loaded(_) = handle
        .swap("stale30", CategoryId::Aux)
        .await
        .expect("aux loads")
    else {
        panic!("expected Loaded");
    };
    // Amber it the honest way: edit the bytes, then rescan (the only judge).
    std::fs::write(
        dir.join("stale30.yaml"),
        "name: stale30\nengine: mock\ncategory: AUX\nvram_gb: 30.0\n# operator note\n",
    )
    .expect("edit");
    handle.reload().await.expect("rescan applied");
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        status
            .slots
            .iter()
            .find(|s| s.category == CategoryId::Aux)
            .and_then(|s| s.stale_config),
        Some(true),
        "amber first, then evict"
    );

    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work50", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work50" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    witness.set_used(0.0);
    let Reply::Loaded(receipt) = loading
        .await
        .expect("task alive")
        .expect("work loads after evicting the amber resident")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "work50");
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Aux),
        None,
        "stale did not spare it"
    );
    let swap_job = status
        .jobs
        .iter()
        .find(|job| job.model == "work50")
        .expect("swap job recorded");
    assert_eq!(
        swap_job.victims_released,
        vec!["stale30".to_string()],
        "and the ledger records it like any other eviction"
    );
}

#[tokio::test]
async fn eviction_picks_the_cheapest_sufficient_victim() {
    // Two AUX slots: 12 GiB and 20 GiB loaded (card reports 32 used -> 38 available).
    // WORK 53 -> deficit 15: the 12 alone does not cover, 12+20 overshoots - the 20
    // alone is the cheapest sufficient set, and the 12 must survive (minimality).
    let witness = FakeWitness::with(vec![sample(72.0, 32.0)]);
    let handle = harness(
        2,
        &[
            ("aux12", "mock", CategoryId::Aux, 12.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("work53", "mock", CategoryId::Work, 53.0),
        ],
        Arc::new(witness.clone()),
    );

    handle
        .swap("aux12", CategoryId::Aux)
        .await
        .expect("aux12 loads");
    handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("aux20 loads");
    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work53", CategoryId::Work).await })
    };
    // Held in the settle window until the card reports the victim's bytes back.
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work53" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    witness.set_used(12.0);
    let Reply::Loaded(_) = loading.await.expect("task alive").expect("work loads") else {
        panic!("expected Loaded");
    };

    let status = handle.status().await.expect("actor alive");
    let aux = aux_models(&status);
    assert!(
        aux.contains(&Some("aux12".to_string())),
        "the cheaper insufficient victim must survive: {aux:?}"
    );
    assert!(
        !aux.contains(&Some("aux20".to_string())),
        "the sufficient victim was evicted: {aux:?}"
    );
    let swap_job = status
        .jobs
        .iter()
        .find(|job| job.model == "work53")
        .expect("swap job");
    assert_eq!(swap_job.victims_released, vec!["aux20".to_string()]);
}

#[tokio::test]
async fn equal_cost_victims_evict_the_less_protected_category() {
    // RP and AUX each book 20 GiB (witness reports 40 used) and WORK wants 35: either
    // victim alone covers the deficit, so the choice is a tie - and a tie must fall to
    // disposability. AUX may be evicted by WORK and RP; RP may be evicted by WORK only,
    // so RP is the one that stays. Comparing masks in iteration order evicted RP
    // instead, because its slot index precedes AUX's (T6, 2026-10-27).
    let witness = FakeWitness::with(vec![sample(72.0, 40.0)]);
    let handle = harness(
        1,
        &[
            ("rp20", "mock", CategoryId::Rp, 20.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("work35", "mock", CategoryId::Work, 35.0),
        ],
        Arc::new(witness.clone()),
    );

    handle
        .swap("rp20", CategoryId::Rp)
        .await
        .expect("rp20 loads");
    handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("aux20 loads");
    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work35", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work35" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    witness.set_used(20.0);
    let Reply::Loaded(_) = loading
        .await
        .expect("task alive")
        .expect("work loads after evicting")
    else {
        panic!("expected Loaded");
    };

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work35")
    );
    assert_eq!(
        model_of(&status, CategoryId::Rp).as_deref(),
        Some("rp20"),
        "the more protected category survives an equal-cost tie"
    );
    assert_eq!(
        model_of(&status, CategoryId::Aux),
        None,
        "AUX is the disposable class"
    );
    let swap_job = status
        .jobs
        .iter()
        .find(|job| job.model == "work35")
        .expect("swap job");
    assert_eq!(
        swap_job.victims_released,
        vec!["aux20".to_string()],
        "the ledger must show which victim the tie selected"
    );
}

#[tokio::test]
async fn equal_total_victims_evict_the_fewer_victims() {
    // R2-20-02: the planner's documented `fewer victims` tie-break (the
    // `count < best_count` arm of `plan_victims`, src/scheduler/admission.rs) was
    // pinned by no fixture — every candidate multiset here had either one covering
    // mask or different totals (R2-20 [2] read them all). AUX 12 + 8 and one AUX
    // 20 against a 20 GiB deficit tie at total 20: the count arm must release the
    // single 20, never the pair. Delete that clause and the planner may kill two
    // operator models where one would do, chosen by disposability alone, with
    // every current pin still green.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let handle = harness(
        3,
        &[
            ("aux12", "mock", CategoryId::Aux, 12.0),
            ("aux8", "mock", CategoryId::Aux, 8.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("work50", "mock", CategoryId::Work, 50.0),
        ],
        Arc::new(witness.clone()),
    );
    // The card tracks the ledger load by load, so WORK's admit-time reading is
    // the honest 40 when the 20 GiB deficit (free 30, need 50) is computed.
    handle
        .swap("aux12", CategoryId::Aux)
        .await
        .expect("aux12 loads");
    witness.set_used(12.0);
    handle
        .swap("aux8", CategoryId::Aux)
        .await
        .expect("aux8 loads");
    witness.set_used(20.0);
    handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("aux20 loads");
    witness.set_used(40.0);

    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work50", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work50" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    // The card gives back the single victim's bytes; the 12 and the 8 stay resident.
    witness.set_used(20.0);
    let Reply::Loaded(_) = loading
        .await
        .expect("task alive")
        .expect("work50 loads after the single victim is released")
    else {
        panic!("expected Loaded");
    };

    let status = handle.status().await.expect("actor alive");
    let job = job_row(&status, "work50");
    assert_eq!(
        job.victims_released,
        vec!["aux20".to_string()],
        "the equal-total tie must fall to the FEWER victims, never the 12 + 8 pair: {:?}",
        job.victims_released
    );
    let aux = aux_models(&status);
    assert!(
        aux.contains(&Some("aux12".to_string())),
        "the surviving half of the pair: {aux:?}"
    );
    assert!(
        aux.contains(&Some("aux8".to_string())),
        "the surviving half of the pair: {aux:?}"
    );
    assert!(
        !aux.contains(&Some("aux20".to_string())),
        "the single victim was the release: {aux:?}"
    );
}

#[tokio::test]
async fn rp_load_cannot_evict_work_and_refuses_with_blockers() {
    // WORK holds 50 and a small AUX 8 sits beside it; RP wants 50: free 12,
    // deficit 38. WORK is no candidate (RP cannot evict it) — the blocker — and
    // AUX 8 is a candidate too small to cover. The refusal must carry BOTH arms
    // of the G6 tail (R2-20-03): `considered evicting:` naming the candidate with
    // its GiB figure, and `blocked by:` naming the rule. No executed test ever
    // had a non-empty candidate list before, so the whole `considered evicting`
    // arm in src/budget.rs could be deleted without turning anything red.
    // Release-first is same-slot only: a paper refusal may not kill the candidate.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let handle = harness(
        1,
        &[
            ("work50", "mock", CategoryId::Work, 50.0),
            ("rp50", "mock", CategoryId::Rp, 50.0),
            ("aux8", "mock", CategoryId::Aux, 8.0),
        ],
        Arc::new(witness.clone()),
    );

    handle
        .swap("work50", CategoryId::Work)
        .await
        .expect("work loads");
    // The card tracks the ledger by `used` override, not a queue step: unrelated
    // readings must not shift which phase a sample belongs to (FakeWitness note).
    witness.set_used(50.0);
    handle
        .swap("aux8", CategoryId::Aux)
        .await
        .expect("the small AUX candidate loads");
    witness.set_used(58.0);
    let error = handle
        .swap("rp50", CategoryId::Rp)
        .await
        .expect_err("RP cannot evict WORK");
    let message = error.to_string();
    assert!(message.contains("insufficient VRAM"), "was: {message}");
    assert!(
        message.contains("considered evicting: aux8 (AUX), 8.0 GiB"),
        "the refusal must name the candidate it could not use, with its GiB \
         figure, before the blocker half: {message}"
    );
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
    assert_eq!(
        model_of(&status, CategoryId::Aux).as_deref(),
        Some("aux8"),
        "the insufficient candidate survived the refusal"
    );
}

#[tokio::test]
async fn aux_load_cannot_evict_anything_cross_category() {
    // AUX x2 holds 30 + 20 (ledger 50, free 20); a 55 GiB AUX load replaces its
    // LRU own occupant (allowed) but still falls 5 short - and AUX may evict
    // nobody outside its own slots. Release-first (ruling 2026-11-01, revisable):
    // the same-slot victim is destroyed on the booked credit and the SETTLE window
    // re-gates on the re-sampled card - the card stays full, so the load refuses
    // with the settle numbers. aux20 (a cross-category non-candidate) survives;
    // aux30 is the accepted casualty of a swap that did not fit. Falsifier:
    // refusing before the kill (pre-ruling shape) leaves aux30 serving.
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
        .expect_err("the card never frees what release-first spent");
    let message = error.to_string();
    assert!(message.contains("settle timeout"), "was: {message}");
    assert!(
        message.contains("22.0 GiB free after release, 57.0 GiB required incl. 2.0 GiB reserve"),
        "the re-gate must name the physical truth and its guard band: {message}"
    );
    let status = handle.status().await.expect("actor alive");
    let aux = aux_models(&status);
    assert!(aux.contains(&Some("aux20".to_string())), "{aux:?}");
    assert!(
        !aux.contains(&Some("aux30".to_string())),
        "the same-slot victim was released first: {aux:?}"
    );
    assert!(!aux.contains(&Some("aux55".to_string())), "{aux:?}");
}

#[tokio::test]
async fn an_aux_load_refused_by_an_emb_occupant_names_who_may_evict_emb() {
    // FINDINGS-20 [11], second half: EMB appeared in no eviction test at all, yet
    // EMB refusals are the ones that most often need the blocker sentence (EMB is
    // evictable only by WORK and RP — never by AUX). The wording is built by
    // src/scheduler/mod.rs `plan_victims` from the non-empty evictor list; only the
    // empty-list arm was ever asserted anywhere in the repo.
    let witness = FakeWitness::with(vec![sample(72.0, 30.0)]);
    let handle = harness(
        1,
        &[
            ("emb30", "mock", CategoryId::Emb, 30.0),
            ("aux55", "mock", CategoryId::Aux, 55.0),
        ],
        Arc::new(witness),
    );
    handle
        .swap("emb30", CategoryId::Emb)
        .await
        .expect("the EMB resident loads");
    let error = handle
        .swap("aux55", CategoryId::Aux)
        .await
        .expect_err("AUX may not evict EMB");
    let message = error.to_string();
    assert!(message.contains("insufficient VRAM"), "was: {message}");
    assert!(
        message.contains("only WORK and RP can evict EMB"),
        "the blocker sentence must name EMB's evictors, not claim nobody can: {message}"
    );

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Emb).as_deref(),
        Some("emb30"),
        "a refused AUX load may not touch the embedding engine"
    );
    assert_eq!(model_of(&status, CategoryId::Aux), None);
}

#[tokio::test]
async fn a_successful_eviction_leaves_the_hard_filtered_occupant_untouched() {
    // FINDINGS-20 [12]: every successful-eviction test so far ran on otherwise-empty
    // cards, so the `evictable_by` HARD FILTER was only ever proven in the
    // all-blockers case. Here a protected occupant (WORK: `evictable_by = []`) sits
    // on the card *while an eviction succeeds*: the plan may count only AUX's bytes.
    // A filter inversion that credited the protected WORK's 30 GiB as releasable
    // whenever any candidate existed would either skip the AUX release, kill WORK,
    // or over-admit onto a card nobody freed — all caught by the exact victim list,
    // the WORK-survives pin, and the released port checks below.
    // Card 72, reserve 2. One repeating sample plus explicit `used` overrides: a
    // multi-element queue would shift with every unrelated reading (the measured
    // sampler, the settle re-reads), and which phase a sample belongs to must not
    // depend on how many readings anyone takes. The card tracks the ledger: empty,
    // then 50 once both occupants are installed, then 30 when the victim is gone.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let (handle, runtime) = harness_rt(
        1,
        &[
            ("work30", "mock", CategoryId::Work, 30.0),
            ("aux20", "mock", CategoryId::Aux, 20.0),
            ("rp35", "mock", CategoryId::Rp, 35.0),
        ],
        Arc::new(witness.clone()),
        15,
    );
    let Reply::Loaded(work_port) = handle
        .swap("work30", CategoryId::Work)
        .await
        .expect("the protected occupant loads")
    else {
        panic!("expected Loaded");
    };
    let Reply::Loaded(aux) = handle
        .swap("aux20", CategoryId::Aux)
        .await
        .expect("the candidate victim loads")
    else {
        panic!("expected Loaded");
    };
    // Both occupants really hold their bytes now (the card says so).
    witness.set_used(50.0);

    // 50 booked -> 20 free; RP wants 35, deficit 15. WORK's 30 is filtered out
    // (WORK evicts nobody), AUX's 20 alone covers: the eviction must succeed with
    // WORK still installed.
    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("rp35", CategoryId::Rp).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "rp35" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    // The card gives the victim's bytes back (WORK keeps holding its 30).
    witness.set_used(30.0);
    let Reply::Loaded(_) = loading.await.expect("task alive").expect("rp35 loads") else {
        panic!("expected Loaded");
    };

    let status = handle.status().await.expect("actor alive");
    let job = job_row(&status, "rp35");
    assert_eq!(
        job.victims_released,
        vec!["aux20".to_string()],
        "only the evictable victim is released — the protected occupant's bytes \
         were not counted: {:?}",
        job.victims_released
    );
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work30"),
        "WORK survives untouched alongside a successful eviction"
    );
    assert_eq!(
        model_of(&status, CategoryId::Aux),
        None,
        "the victim is gone"
    );
    assert!(runtime.is_dead(aux.port).await, "the victim's port refuses");
    assert!(
        !runtime.is_dead(work_port.port).await,
        "the protected engine never even got a kill: it still answers"
    );
}

/// ModelConf writer with raw extra lines per model (cost/floor keys).
fn write_modelconf_ext(entries: &[(&str, &[&str])]) -> PathBuf {
    let dir = simplehar_se::testutil::temp_dir("observed");
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
    harness_ext_rt(specs, witness, settle_timeout_s).0
}

/// `harness_ext` with the runtime returned, for tests that must count what was
/// really spawned and killed (a refusal "before any kill" is only falsifiable by
/// the kill count, not by the slot state alone).
fn harness_ext_rt(
    specs: &[(&str, &[&str])],
    witness: Arc<dyn VramWitness>,
    settle_timeout_s: u64,
) -> (SchedulerHandle, MockRuntime) {
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
    let handle = Scheduler::start(
        cfg,
        write_modelconf_ext(specs),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        witness,
    );
    (handle, runtime)
}

#[tokio::test]
async fn greedy_replacement_clears_floor_after_victim_release() {
    // Operator case 2026-10-26: WORK holds a 27.7 GiB llama (card shows 35.4 used
    // by it); a greedy strata with a 65 GiB launch floor must be ALLOWED - the
    // replacement victim dies first, so the floor is judged on the fresh observed
    // free at settle, never on the pre-release reading that still counts the
    // victim's own memory.
    // One repeating sample plus an explicit `used` override: which phase a reading
    // belongs to must not depend on how many readings anyone takes.
    let witness = FakeWitness::with(vec![sample(72.0, 35.4)]);
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
        Arc::new(witness.clone()),
        5,
    );
    handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("holder loads");
    // The victim's release is what empties the card, so the floor is judged here:
    // an override, not a queue step, so no extra reading can shift it.
    witness.set_used(0.0);
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
    let witness = FakeWitness::with(vec![sample(72.0, 35.4)]);
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
        Arc::new(witness.clone()),
        3,
    );
    handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("holder loads");
    // A foreign holder keeps 20 GiB of the card through the release, so the fresh
    // observed free at settle sits under the floor and the load is refused there.
    witness.set_used(20.0);
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
        error.contains("52.0 GiB free after release, 67.0 GiB required incl. 2.0 GiB reserve"),
        "explanatory settle error: {error}"
    );
    // R2-19-03: the settle-refusal ROW was pinned only inside a hand-written webui
    // fixture, which cannot fail. Pin the ledger truth beside the error text: the
    // refused load terminalizes (fail funnel, no spawn: it ends at `Idle`), carries
    // the settle outcome verbatim, and keeps `is_fault` - the headline rule the
    // operator reads. A refactor that terminalizes settle refusals with a different
    // outcome or drops the fault flag goes red here.
    assert!(job.terminal, "{job:?}");
    assert_eq!(
        job.kind,
        simplehar_se::scheduler::JobKind::Load,
        "the refused load stays a load row: {job:?}"
    );
    assert_eq!(
        job.outcome.as_deref(),
        Some("memory did not settle after release"),
        "the row names the settle outcome verbatim: {job:?}"
    );
    assert_eq!(
        job.state,
        simplehar_se::scheduler::SwapState::Idle,
        "the fail funnel (no spawn) walks Failed -> Reap -> Idle: {job:?}"
    );
    assert!(
        job.is_fault,
        "a refused load is the headline fault: {job:?}"
    );
}

#[tokio::test]
async fn an_unreachable_launch_floor_refuses_before_any_release() {
    // R2-19-01 (the Row-10 guard, RULED 2026-11-01): when NO release the harness may
    // perform can reach the launch floor, `finish_admit` refuses BEFORE any kill and
    // the refusal names the guard sentence "No releases allow for the VRAM floor.
    // Check foreign residents". The guard deliberately PRECEDES the release-first
    // arm: destroying the resident is an accepted cost only for a deficit a release
    // could actually fix, never for a floor the card denies. Delete the
    // `!no_releases_reach` condition at src/scheduler/mod.rs:4002 and this very swap
    // first KILLS the resident, then dies in the settle window on the same
    // unreachable floor - a model destroyed on paper, with a `settle timeout` error
    // instead of the guard: every assertion below goes red. The sibling
    // `greedy_replacement_clears_floor_after_victim_release` has a REACHABLE floor
    // (guard false), so no other test drives this guard.
    // Card 72, reserve 2, tolerance 2 (defaults): holder books 20, the card shows 60
    // used (40 GiB of it foreign). free at admit = 72 - 2 - 60 + 20 = 30.0 - the
    // resident's own credit is already inside it, and no cross-category victim
    // exists - so even releasing everything the harness may release leaves
    // 65 - 2 > 30.0 + 0.0: the floor is unreachable.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let (handle, runtime) = harness_ext_rt(
        &[
            (
                "holder",
                &[
                    "name: holder",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 20",
                ],
            ),
            (
                "greedy65",
                &[
                    "name: greedy65",
                    "engine: mock",
                    "category: WORK",
                    "vram_gb: 55",
                    "cost: greedy",
                    "launch_only_with_at_least_vram_free: 65",
                ],
            ),
        ],
        Arc::new(witness.clone()),
        2,
    );
    let Reply::Loaded(holder) = handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("the resident loads onto the empty card")
    else {
        panic!("expected Loaded");
    };
    // The card after install: holder's 20 plus 40 GiB of foreign occupancy the
    // harness may never release.
    witness.set_used(60.0);

    let error = handle
        .swap("greedy65", CategoryId::Work)
        .await
        .expect_err("no release can reach the floor: the guard must refuse");
    let message = error.to_string();
    assert!(
        message.contains("launch floor not met: at least 65.0 GiB free required, free 30.0 GiB"),
        "the refusal is the floor refusal with the compared figure: {message}"
    );
    assert!(
        message.contains("No releases allow for the VRAM floor. Check foreign residents"),
        "the refusal must name the guard, not a bare floor miss: {message}"
    );

    // The ruling's other half: the refusal landed BEFORE any kill. The resident is
    // untouched - same slot, same engine still answering, nothing ever queued or
    // released against it.
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("holder"),
        "the resident a release could not save stays installed"
    );
    let job = job_row(&status, "greedy65");
    assert!(job.terminal, "{job:?}");
    assert_eq!(
        job.outcome.as_deref(),
        Some("admission refused"),
        "refused at the gate, never at settle: {job:?}"
    );
    assert_eq!(job.gate_target_gib, Some(65.0), "the floor was the gate");
    assert!(
        job.victims_released.is_empty(),
        "the guard refuses before any release: {:?}",
        job.victims_released
    );
    assert_eq!(
        runtime.spawn_count(),
        1,
        "the refused greedy load never spawned"
    );
    assert_eq!(
        runtime.server_count(),
        1,
        "only the resident's engine is live"
    );
    assert!(
        !runtime.is_dead(holder.port).await,
        "no kill ever reached the resident's engine"
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
async fn a_load_two_victims_short_releases_both_least_disposable_first() {
    // FINDINGS-20 [3]: every planned-victim assertion in the repo was a single element,
    // so the one-at-a-time release loop, its ordering comparator (most-disposable
    // first = most categories may evict it) and `released_cost` across releases ran
    // entirely untested. Neither victim
    // here covers the deficit alone (12 + 20 against a 24 GiB short), so the load can
    // only be admitted on the SUM of both credits; the card is scripted to stay busy
    // with the unreleased victim's bytes until that victim's own release is proven, so
    // a loop that stops early on an over-credited figure is refused at settle instead
    // of quietly passing.
    let witness = FakeWitness::with(vec![sample(72.0, 32.0)]);
    let (handle, runtime) = harness_rt(
        1,
        &[
            // The first victim's kill parks (`hold_next_kills` below).
            ("aux12", "mock-hold-kill", CategoryId::Aux, 12.0),
            ("rp20", "mock", CategoryId::Rp, 20.0),
            ("work62", "mock", CategoryId::Work, 62.0),
        ],
        Arc::new(witness.clone()),
        15,
    );
    handle
        .swap("aux12", CategoryId::Aux)
        .await
        .expect("aux12 loads");
    handle
        .swap("rp20", CategoryId::Rp)
        .await
        .expect("rp20 loads");
    // Park the FIRST kill only: "one victim released, the second still loaded" becomes a
    // state the test holds rather than a window it races at 25 ms per peek.
    runtime.hold_next_kills(1);

    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work62", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work62" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    assert!(
        job_row(&status, "work62").victims_released.is_empty(),
        "nothing is proven released while the first kill is still in flight: {:?}",
        job_row(&status, "work62").victims_released
    );
    assert_eq!(
        runtime.server_count(),
        2,
        "the loop releases one victim at a time and waits for its evidence"
    );
    assert_eq!(
        runtime.spawn_count(),
        2,
        "and spawns nothing before the gate is met"
    );
    assert_eq!(
        model_of(&status, CategoryId::Rp).as_deref(),
        Some("rp20"),
        "the second victim is untouched"
    );
    // How the card will read once that parked terminate lands: aux12's 12 GiB back,
    // rp20 still holding its 20.
    witness.set_used(20.0);
    runtime.release_held_kills();

    wait_until(&handle, 15, |status| {
        job_row(status, "work62").victims_released == vec!["aux12".to_string(), "rp20".to_string()]
    })
    .await;
    // Only now, with BOTH credits landed, does the card have room for this load.
    witness.set_used(0.0);
    let Reply::Loaded(receipt) = loading
        .await
        .expect("task alive")
        .expect("work62 loads only after both releases")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "work62");

    let status = handle.status().await.expect("actor alive");
    let job = job_row(&status, "work62");
    assert_eq!(
        job.victims_released,
        vec!["aux12".to_string(), "rp20".to_string()],
        "the release ORDER is most-disposable first (most categories may evict it), the \
         planner's `least-protected first`: AUX (WORK and RP may evict it) before RP \
         (WORK only)"
    );
    assert_eq!(job.gate_target_gib, Some(62.0));
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work62"),
        "the single load admits on the summed credit"
    );
    assert!(
        !aux_models(&status).contains(&Some("aux12".to_string())),
        "both victims are gone: {:?}",
        aux_models(&status)
    );
    assert_eq!(model_of(&status, CategoryId::Rp), None);
    assert_eq!(
        runtime.server_count(),
        1,
        "only the admitted load's engine is live"
    );
}

#[tokio::test]
async fn the_release_loop_stops_only_when_the_single_counted_credit_covers_the_gate() {
    // FINDINGS-20 [3] (the early-stop branch, `release sufficient; skipping remaining
    // victims`, src/scheduler/mod.rs) read together with R2-03-01: this load queues
    // three release targets — the same-slot holder and BOTH AUX victims (15 + 15
    // against a 16 GiB deficit, so the planner cannot settle for one). The holder's
    // 2.0 GiB is already inside `free_at_admit` (40.0 = 72 - 2 reserve - 32 used + 2
    // holder credit), so after holder + aux_a the honest ledger reads 40.0 + 15.0 =
    // 55.0 against the 56.0 gate — 1.0 GiB short, so aux_b must die. The double
    // count R2-03-01 filed read 40.0 + 2.0 + 15.0 = 57.0, already over the gate, and
    // the loop stopped with an operator model still loaded; this test pins the honest
    // single count, so it is red on the double count and green on the fix.
    let witness = FakeWitness::with(vec![sample(72.0, 32.0)]);
    let (handle, runtime) = harness_rt(
        2,
        &[
            ("holder", "mock", CategoryId::Work, 2.0),
            ("aux_a", "mock", CategoryId::Aux, 15.0),
            ("aux_b", "mock", CategoryId::Aux, 15.0),
            ("work56", "mock", CategoryId::Work, 56.0),
        ],
        Arc::new(witness.clone()),
        15,
    );
    handle
        .swap("holder", CategoryId::Work)
        .await
        .expect("holder loads");
    let Reply::Loaded(aux_a) = handle
        .swap("aux_a", CategoryId::Aux)
        .await
        .expect("aux_a loads")
    else {
        panic!("expected Loaded");
    };
    let Reply::Loaded(aux_b) = handle
        .swap("aux_b", CategoryId::Aux)
        .await
        .expect("aux_b loads")
    else {
        panic!("expected Loaded");
    };

    let loading = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work56", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work56" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    // Admit already happened (it precedes `Dispose`), so this only moves the card for
    // the settle gate: every byte the loop has released is back, and the window has
    // nothing left to wait for once the last release is proven.
    witness.set_used(0.0);
    let Reply::Loaded(receipt) = loading
        .await
        .expect("task alive")
        .expect("work56 admits only on BOTH victims' credits")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "work56");

    let status = handle.status().await.expect("actor alive");
    let job = job_row(&status, "work56");
    assert_eq!(
        job.victims_released,
        vec!["aux_a".to_string(), "aux_b".to_string()],
        "the honest credit is 1.0 GiB short after the first victim, so the loop may \
         not stop before the second: {:?}",
        job.victims_released
    );
    let free_at_admit = job
        .free_at_admit_gib
        .expect("the admit figure is recorded for post-checks");
    assert_eq!(
        free_at_admit, 40.0,
        "72 - 2 reserve - 32 used + the holder's 2.0 credit"
    );
    assert_eq!(job.gate_target_gib, Some(56.0));
    // Falsifier leg (R2-03-01): the fixture must straddle the gate between the two
    // accounting forms — after aux_a the double-counted ledger term (40.0 + 2.0 +
    // 15.0 = 57.0) clears the 56.0 gate, the single-counted one (40.0 + 15.0 = 55.0)
    // does not. Should the holder's credit ever be counted twice again, this leg goes
    // red before anyone can re-pin the release list to the inflated figure.
    assert!(
        free_at_admit + 2.0 + 15.0 > 56.0 && free_at_admit + 15.0 < 56.0,
        "double-counted 57.0 clears the 56.0 gate while the honest 55.0 does not: \
         free_at_admit {free_at_admit}"
    );
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work56"),
        "the same-slot replacement landed"
    );
    let aux = aux_models(&status);
    assert!(
        !aux.contains(&Some("aux_a".to_string())),
        "the first victim was released: {aux:?}"
    );
    assert!(
        !aux.contains(&Some("aux_b".to_string())),
        "the second victim was required by the honest credit too: {aux:?}"
    );
    assert_eq!(
        runtime.server_count(),
        1,
        "the kill count is the proof: only the admitted load is live"
    );
    assert!(
        runtime.is_dead(aux_a.port).await,
        "the first victim's port refuses"
    );
    assert!(
        runtime.is_dead(aux_b.port).await,
        "the second victim's port refuses"
    );
    assert_eq!(runtime.spawn_count(), 4, "holder, aux_a, aux_b, work56");
}

#[tokio::test]
async fn a_swap_of_a_model_committed_to_eviction_never_answers_already() {
    // R2-02-01: while a WORK load evicts the AUX resident with the kill held, a
    // swap of that same model into AUX answered `Already` — a success receipt
    // for an engine the harness had already committed to kill, which the
    // in-flight eviction then empties. The fix falls through to the queue and
    // replace path, so the swap becomes a real load with a real receipt.
    // Red on the old `Already`: no ledger row is ever registered and the swap
    // answers before the first status poll, so the Dispose-row wait below
    // times out. Green on the fix: a live row of the doomed model in `Dispose`
    // (its own release, also held), no reply while any release is unproven,
    // and a `Loaded` naming a freshly spawned live engine.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let (handle, runtime) = harness_rt(
        1,
        &[
            // The victim's engine parks every kill of its port (`hold_kill_of`).
            ("doom20", "mock-hold-kill", CategoryId::Aux, 20.0),
            ("work55", "mock", CategoryId::Work, 55.0),
        ],
        Arc::new(witness.clone()),
        15,
    );
    let Reply::Loaded(doomed) = handle
        .swap("doom20", CategoryId::Aux)
        .await
        .expect("the victim loads")
    else {
        panic!("expected Loaded");
    };
    // The card tracks the ledger: the victim holds its 20 GiB.
    witness.set_used(20.0);
    // Park EVERY kill of the doomed engine by TARGET (`hold_kill_of`, FINDINGS-19
    // [4]): both pending reaps carry its ticket — the eviction's kill and the
    // reload's own same-slot reap — so the fake's parking never depends on how
    // many kills the product happens to issue, and a reap change (dedup,
    // re-order) cannot silently re-arm or disarm this scenario. Every receipt
    // stays pending as a state the test holds, never a race against the release
    // deadline.
    runtime.hold_kill_of(doomed.port);

    let evicting = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work55", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "work55" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Aux).as_deref(),
        Some("doom20"),
        "the victim is still installed while its release is unproven"
    );
    assert!(
        runtime
            .held_kills()
            .iter()
            .any(|(port, _)| *port == doomed.port),
        "the eviction's kill of the doomed engine is the held one"
    );

    // The double-click: load the doomed model into its own category mid-dispose.
    let reswap = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("doom20", CategoryId::Aux).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "doom20" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;
    assert!(
        !reswap.is_finished(),
        "a doomed engine must not earn any receipt while its release is unproven"
    );
    assert_eq!(
        model_of(
            &handle.status().await.expect("actor alive"),
            CategoryId::Aux
        )
        .as_deref(),
        Some("doom20"),
        "the slot never claims empty before the release is proven (DOCTRINE 5)"
    );

    // Land both releases and give the card back the victim's bytes.
    witness.set_used(0.0);
    runtime.release_held_kills();
    let reply = reswap.await.expect("task alive").expect("reload answers");
    let Reply::Loaded(receipt) = reply else {
        panic!("the swap must end in a receipt for a real engine, got {reply:?}")
    };
    assert_eq!(receipt.model, "doom20");
    let Reply::Loaded(work) = evicting.await.expect("task alive").expect("work loads") else {
        panic!("expected Loaded");
    };
    assert_eq!(work.model, "work55");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        model_of(&status, CategoryId::Aux).as_deref(),
        Some("doom20"),
        "the reload served: the receipt describes a real engine"
    );
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work55")
    );
    assert!(runtime.is_dead(doomed.port).await, "the doomed engine died");
    assert!(
        !runtime.is_dead(receipt.port).await,
        "the receipt's port answers"
    );
    assert_eq!(
        runtime.server_count(),
        2,
        "one live engine per serving model, none left over"
    );
    assert_eq!(runtime.spawn_count(), 3, "doom20, work55, doom20 again");
}

#[tokio::test]
async fn the_idle_sweep_skips_a_cross_slot_eviction_victim() {
    // R2-20-01: the idle sweep's only spare is `slot.job`, which a cross-slot
    // victim slot never carries. With RP opted into `idle_timeout_s = 1`, a
    // WORK load evicts the RP resident and that kill parks in `Dispose`
    // (`hold_kill_of`), the 2 s tick started a second unload for the doomed
    // model: a contradictory `"idle timeout"` ledger row beside the eviction's
    // `victims_released`, plus two kill+confirm loops on one ticket.
    // Falsifier: delete the `is_committed_release_target` skip in `on_tick`
    // (src/scheduler/commands.rs) and the first sweep pass past the 1 s
    // threshold creates an Unload row naming `rp-doomed` while its eviction
    // kill is still parked — the asserts below go red on the spot.
    // The AUX `canary` (idle 1 s as well) is the proof the sweep really fired:
    // the sweep runs on a real `Instant` with no time hook, so its one
    // observable event is what it did to a slot it may lawfully take. That
    // pass necessarily found the older doomed occupant equally due, because
    // both idle clocks stop at the same poll: the touches run until the
    // eviction parks.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0)]);
    let (handle, runtime) = harness_rt_idle(
        1,
        &[
            // The doomed RP resident: instant ready, every kill of its port parks.
            ("rp-doomed", "mock-hold-kill", CategoryId::Rp, 20.0),
            // The sweep canary: an ordinary AUX resident that idles out and
            // thereby signals the tick.
            ("canary", "mock", CategoryId::Aux, 4.0),
            // Card 72, reserve 2: 24 booked -> 46 free, the 55 load is 9 short,
            // and only the RP 20 covers it — the planner never queues the 4.
            ("work-55", "mock", CategoryId::Work, 55.0),
        ],
        Arc::new(witness.clone()),
        15,
        1, // RP idles after 1 s of silence: the sweep under test
        1, // AUX idles after 1 s: the canary's clock
    );
    let Reply::Loaded(doomed) = handle
        .swap("rp-doomed", CategoryId::Rp)
        .await
        .expect("the doomed resident loads")
    else {
        panic!("expected Loaded");
    };
    witness.set_used(20.0);
    handle
        .swap("canary", CategoryId::Aux)
        .await
        .expect("the canary loads");
    witness.set_used(24.0);
    runtime.hold_kill_of(doomed.port);

    let evicting = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("work-55", CategoryId::Work).await })
    };

    // Park the eviction: WORK reaches `Dispose` and its kill of the doomed
    // port parks in the fake. Both idle clocks are reset on every poll until
    // the commitment lands, so the sweep can never lawfully be the first to
    // act on the doomed slot before this scenario holds.
    let park_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        handle.touch("rp-doomed");
        handle.touch("canary");
        let status = handle.status().await.expect("actor alive");
        assert!(
            !status
                .jobs
                .iter()
                .any(|job| job.kind == simplehar_se::scheduler::JobKind::Unload
                    && job.model == "rp-doomed"),
            "the doomed model may never earn an Unload row: {:?}",
            status.jobs
        );
        let parked = status.jobs.iter().any(|job| {
            job.model == "work-55" && job.state == simplehar_se::scheduler::SwapState::Dispose
        }) && runtime
            .held_kills()
            .iter()
            .any(|(port, _)| *port == doomed.port);
        if parked {
            break;
        }
        assert!(
            Instant::now() < park_deadline,
            "the eviction never parked its kill in Dispose; last status: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // Stop touching: both idle clocks now run. Wait for the sweep to take the
    // canary — the signal that a 2 s tick fired past the 1 s threshold. While
    // waiting, demand the doomed model earns no sweep row and stays installed
    // behind the parked eviction (DOCTRINE 5).
    let sweep_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = handle.status().await.expect("actor alive");
        assert!(
            !status
                .jobs
                .iter()
                .any(|job| job.kind == simplehar_se::scheduler::JobKind::Unload
                    && job.model == "rp-doomed"),
            "R2-20-01 falsifier: the idle sweep started its own unload of the \
             doomed model while the eviction's kill of that same engine was \
             parked: {:?}",
            status.jobs
        );
        assert_eq!(
            model_of(&status, CategoryId::Rp).as_deref(),
            Some("rp-doomed"),
            "the victim stays installed while its release is unproven"
        );
        if status.jobs.iter().any(|job| {
            job.kind == simplehar_se::scheduler::JobKind::Unload
                && job.model == "canary"
                && job.outcome.as_deref() == Some("idle timeout")
        }) {
            break;
        }
        assert!(
            Instant::now() < sweep_deadline,
            "the sweep never unloaded the idle canary, so no sweep pass is \
             proven; last status: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let status = handle.status().await.expect("actor alive");
    assert!(
        runtime
            .held_kills()
            .iter()
            .any(|(port, _)| *port == doomed.port),
        "the eviction's kill is still parked at the sweep pass: the skip had a \
         live eviction to honor, not an already-landed release"
    );
    assert!(
        job_row(&status, "work-55").victims_released.is_empty(),
        "nothing is proven released while the kill is parked: {:?}",
        job_row(&status, "work-55").victims_released
    );

    // Land the parked kill: the eviction alone must empty the slot and admit.
    witness.set_used(0.0);
    runtime.release_held_kills();
    let Reply::Loaded(work) = evicting
        .await
        .expect("task alive")
        .expect("the parked eviction completes after the kill lands")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(work.model, "work-55");

    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        job_row(&status, "work-55").victims_released,
        vec!["rp-doomed".to_string()],
        "the eviction is the one and only cause of death"
    );
    assert!(
        !status
            .jobs
            .iter()
            .any(|job| job.kind == simplehar_se::scheduler::JobKind::Unload
                && job.model == "rp-doomed"),
        "no Unload job or ledger row ever named the doomed model: {:?}",
        status.jobs
    );
    for job in status
        .jobs
        .iter()
        .filter(|job| job.outcome.as_deref() == Some("idle timeout"))
    {
        assert_eq!(
            job.model, "canary",
            "the sweep's only idle-timeout row is the canary's, never the \
             eviction victim's: {job:?}"
        );
    }
    assert!(runtime.is_dead(doomed.port).await, "the doomed engine died");
    assert_eq!(
        model_of(&status, CategoryId::Rp),
        None,
        "the RP slot emptied through the eviction only"
    );
    assert_eq!(
        model_of(&status, CategoryId::Aux),
        None,
        "the canary idled out"
    );
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work-55")
    );
    assert_eq!(runtime.server_count(), 1, "only the admitted load is live");
    assert_eq!(runtime.spawn_count(), 3, "rp-doomed, canary, work-55");
}
