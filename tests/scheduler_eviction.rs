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
    let message = error.to_string();
    // G13: the message prints the figure the gate compared — the 20.0 GiB ledger
    // free plus the 30.0 GiB same-slot release it was about to reclaim — and names
    // both truths it was derived from (review, 2026-10-26).
    assert!(
        message.contains("insufficient VRAM: need 55.0 GiB, free 50.0 GiB"),
        "was: {message}"
    );
    assert!(
        message.contains("budget 20.0 GiB") && message.contains("observed free 22.0 GiB"),
        "the refusal must name the budget and the observed reading: {message}"
    );
    // FINDINGS-20 [11]: the non-empty-evictor-list blocker arm. The two AUX
    // occupants ARE evictable — just not by AUX (`evictable_by = [WORK, RP]`), so
    // the operator's only guidance on who could free the memory is this sentence.
    // A regression to the `nothing may evict AUX` wording (or to printing no
    // blocker at all) fails here; the `nothing may evict` arm is pinned by
    // `rp_load_cannot_evict_work_and_refuses_with_blockers`.
    assert!(
        message.contains("only WORK and RP can evict AUX"),
        "the refusal must name who COULD evict the blockers: {message}"
    );
    let status = handle.status().await.expect("actor alive");
    let aux = aux_models(&status);
    assert!(aux.contains(&Some("aux30".to_string())), "{aux:?}");
    assert!(aux.contains(&Some("aux20".to_string())), "{aux:?}");
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
        error.contains("52.0 GiB free after release, 65.0 GiB required"),
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
async fn a_load_two_victims_short_releases_both_least_disposable_first() {
    // FINDINGS-20 [3]: every planned-victim assertion in the repo was a single element,
    // so the one-at-a-time release loop, its ordering comparator (least-disposable
    // first) and `released_cost` across releases ran entirely untested. Neither victim
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
        "the release ORDER is least-disposable first: AUX (WORK and RP may evict it) \
         before RP (WORK only)"
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
async fn the_release_loop_stops_as_soon_as_the_credits_already_cover_the_gate() {
    // FINDINGS-20 [3], the early-stop branch (`release sufficient; skipping remaining
    // victims`, src/scheduler/mod.rs): this load queues three release targets — the
    // same-slot holder and BOTH AUX victims (15 + 15 against a 16 GiB deficit, so the
    // planner cannot settle for one) — and the first two credits are what carry the
    // gate from 40 to 57 GiB against a 56 GiB need. Drop any part of the accumulation
    // and the third victim is killed as well, which is exactly the harm the branch
    // exists to prevent (an operator's model dies for nothing).
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
    // the settle gate: the holder's 2 GiB and the first victim's 15 have come back, and
    // the untouched second victim still holds its 15.
    witness.set_used(15.0);
    let Reply::Loaded(receipt) = loading
        .await
        .expect("task alive")
        .expect("work56 admits on the first victim's credit alone")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "work56");

    let status = handle.status().await.expect("actor alive");
    let job = job_row(&status, "work56");
    assert_eq!(
        job.victims_released,
        vec!["aux_a".to_string()],
        "the loop must stop before the third target: the victim nobody needed stays loaded"
    );
    assert_eq!(
        job.free_at_admit_gib,
        Some(40.0),
        "the figure the accumulated credits were compared against"
    );
    assert_eq!(job.gate_target_gib, Some(56.0));
    assert_eq!(
        model_of(&status, CategoryId::Work).as_deref(),
        Some("work56"),
        "the same-slot replacement landed"
    );
    let aux = aux_models(&status);
    assert!(
        !aux.contains(&Some("aux_a".to_string())),
        "the planned victim was released: {aux:?}"
    );
    assert!(
        aux.contains(&Some("aux_b".to_string())),
        "the skipped victim survived: {aux:?}"
    );
    assert_eq!(
        runtime.server_count(),
        2,
        "the kill count is the proof: work56 plus the spared aux_b"
    );
    assert!(
        runtime.is_dead(aux_a.port).await,
        "the released victim's port refuses"
    );
    assert!(
        !runtime.is_dead(aux_b.port).await,
        "the skipped victim still answers"
    );
    assert_eq!(runtime.spawn_count(), 4, "holder, aux_a, aux_b, work56");
}
