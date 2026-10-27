//! Witness integration tests: the card's own reading is the budget (observed free,
//! credited for a pending same-slot release and debited for loads still in flight),
//! the settle-after-release gate blocks a spawn until the bytes are physically back,
//! and a real witness that cannot answer is a refusal rather than a guess. Driven by
//! a scripted `FakeWitness` against the mock engine; plus pure parser tests and one
//! skip-safe live NVML sample.

mod mock_runtime;

use async_trait::async_trait;
use mock_runtime::MockRuntime;
use simplehar_se::budget::{parse_smi_sample, MemorySample, VramWitness};
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scripted witness: samples come off the queue (the last one repeats), and an
/// explicit override always wins — tests flip behavior mid-flight, e.g. when the
/// released engine's memory "returns" to the driver.
#[derive(Clone, Default)]
struct FakeWitness {
    queue: Arc<Mutex<VecDeque<MemorySample>>>,
    override_now: Arc<Mutex<Option<MemorySample>>>,
    /// Once set, every sample errors: models a driver wedging mid-window (§0.3).
    dead: Arc<AtomicBool>,
}

impl FakeWitness {
    fn with(samples: Vec<MemorySample>) -> Self {
        FakeWitness {
            queue: Arc::new(Mutex::new(samples.into())),
            override_now: Arc::new(Mutex::new(None)),
            dead: Arc::new(AtomicBool::new(false)),
        }
    }

    fn set(&self, sample: MemorySample) {
        *self.override_now.lock().expect("witness override lock") = Some(sample);
    }

    fn kill(&self) {
        self.dead.store(true, Ordering::Relaxed);
    }
}

#[async_trait]
impl VramWitness for FakeWitness {
    fn source(&self) -> String {
        "fake".to_string()
    }

    async fn sample(&self) -> Result<MemorySample, String> {
        if self.dead.load(Ordering::Relaxed) {
            return Err("nvml died".to_string());
        }
        if let Some(sample) = self
            .override_now
            .lock()
            .expect("witness override lock")
            .clone()
        {
            return Ok(sample);
        }
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

fn sample(total: f64, used: f64, processes: Vec<(u32, f64)>) -> MemorySample {
    MemorySample {
        total_gib: total,
        used_gib: used,
        processes,
    }
}

fn harness(
    cfg: AppConfig,
    specs: &[(&'static str, CategoryId)],
    witness: Arc<dyn VramWitness>,
) -> SchedulerHandle {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let dir = simplehar_se::testutil::temp_dir("wit");
    for (name, category) in specs {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: mock\ncategory: {category}\nvram_gb: 4\n"),
        )
        .expect("model yaml");
    }
    Scheduler::start(
        cfg,
        dir,
        adapters,
        Arc::new(runtime) as Arc<dyn EngineRuntime>,
        witness,
    )
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
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[tokio::test]
async fn admission_refuses_when_the_card_has_no_room() {
    // The card is 72 GiB and reports 70 used — whatever holds it (a foreign process
    // appears here as per-process data, but the physical reading alone suffices), a
    // 4 GiB load must refuse with the numbers (G13: the F-7 lie is gone).
    let witness = FakeWitness::with(vec![sample(72.0, 70.0, vec![(900, 70.0)])]);
    let cfg = AppConfig {
        settle_timeout_s: 2,
        ..AppConfig::default()
    };
    let handle = harness(cfg, &[("qwen3", CategoryId::Work)], Arc::new(witness));

    let error = handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect_err("external holder must block the load");
    let message = error.to_string();
    assert!(message.contains("need 4.0 GiB"), "was: {message}");
    assert!(message.contains("free 0.0 GiB"), "was: {message}");

    let status = handle.status().await.expect("actor alive");
    assert!(status.slots.iter().all(|slot| slot.model.is_none()));
}

#[tokio::test]
async fn settle_blocks_spawn_while_the_card_has_not_freed() {
    // Load model A, then swap to B while the card still reports the released
    // engine's bytes: the settle gate refuses with the numbers and never spawns B
    // (G12: no double-spawn). The gate is physical free vs the gate target — a
    // leftover that still leaves room is no reason to refuse (operator 2026-10-26:
    // the ledger-baseline form timed out every swap against a ~3 GiB desktop floor
    // that had nothing to do with the release).
    let witness = FakeWitness::with(vec![sample(72.0, 0.0, vec![])]);
    let cfg = AppConfig {
        settle_timeout_s: 1,
        ..AppConfig::default()
    };
    let (handle, _dir) = harness_models(
        cfg,
        &[("a", CategoryId::Work, 8.0), ("b", CategoryId::Work, 8.0)],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
    // A's memory never comes back: 68 of 72 used leaves 4.0 GiB free — under the
    // 8.0 GiB gate even with the witness tolerance — while B's own admit fits,
    // because the victim's 8 GiB are credited to it.
    witness.set(sample(72.0, 68.0, vec![]));
    let error = handle
        .swap("b", CategoryId::Work)
        .await
        .expect_err("settle must refuse while the card is still full");
    let message = error.to_string();
    assert!(message.contains("settle timeout"), "was: {message}");
    assert!(
        message.contains("4.0 GiB free after release, 8.0 GiB required"),
        "was: {message}"
    );

    let status = handle.status().await.expect("actor alive");
    assert!(
        status.slots.iter().all(|slot| slot.model.is_none()),
        "the replaced occupant is gone and B was never installed"
    );
}

#[tokio::test]
async fn settle_passes_once_the_card_frees() {
    // Same start, but the card is genuinely full while B waits; mid-settle the
    // memory returns: B loads and serves.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0, vec![])]);
    let cfg = AppConfig {
        settle_timeout_s: 5,
        ..AppConfig::default()
    };
    let (handle, _dir) = harness_models(
        cfg,
        &[("a", CategoryId::Work, 8.0), ("b", CategoryId::Work, 8.0)],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
    // Hold the card full so the settle gate has a real reason to wait: 4.0 GiB
    // free against an 8.0 GiB gate, tolerance included.
    witness.set(sample(72.0, 68.0, vec![]));
    let awaiting_b = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b", CategoryId::Work).await })
    };
    // Wait until the release is confirmed and the settle gate is holding (job b in
    // Dispose, slot empty), then let the memory return.
    wait_until(&handle, 5, |status| {
        status.slots.iter().all(|slot| slot.model.is_none())
            && status.jobs.iter().any(|job| {
                job.model == "b" && job.state == simplehar_se::scheduler::SwapState::Dispose
            })
    })
    .await;
    witness.set(sample(72.0, 20.0, vec![]));
    let reply = awaiting_b
        .await
        .expect("task alive")
        .expect("b loads after settle");
    assert!(matches!(reply, Reply::Loaded(_)), "got {reply:?}");
}

#[tokio::test]
async fn settle_fails_closed_when_the_witness_dies_mid_window() {
    // The card is held full so the settle window is genuinely waiting, then the
    // witness dies. A confirmed release proves the port closed, never that the VRAM
    // came back — and for a same-slot replacement this window is the load's only
    // physical check — so the spawn must not fire blind (review, 2026-10-26).
    let witness = FakeWitness::with(vec![sample(72.0, 0.0, vec![])]);
    let cfg = AppConfig {
        settle_timeout_s: 1,
        ..AppConfig::default()
    };
    let (handle, _dir) = harness_models(
        cfg,
        &[("a", CategoryId::Work, 8.0), ("b", CategoryId::Work, 8.0)],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
    witness.set(sample(72.0, 68.0, vec![]));
    let awaiting_b = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("b", CategoryId::Work).await })
    };
    wait_until(&handle, 5, |status| {
        status
            .jobs
            .iter()
            .any(|job| job.model == "b" && job.state == simplehar_se::scheduler::SwapState::Dispose)
    })
    .await;
    witness.kill();

    let error = awaiting_b
        .await
        .expect("task alive")
        .expect_err("a dead witness must not spawn blind");
    let message = error.to_string();
    assert!(message.contains("settle timeout"), "was: {message}");
    assert!(
        message.contains("nvml died"),
        "the lost witness must be named: {message}"
    );
    let status = handle.status().await.expect("actor alive");
    assert!(
        status.slots.iter().all(|slot| slot.model.is_none()),
        "b was never installed"
    );
}

#[tokio::test]
async fn unreadable_card_reading_refuses_rather_than_silently_fitting() {
    // `nvidia-smi` can hand back `NaN`, which parses as a number; every comparison
    // against NaN is false, so a gate that just compares would read "fits".
    let witness = FakeWitness::with(vec![sample(f64::NAN, f64::NAN, vec![])]);
    let cfg = AppConfig {
        settle_timeout_s: 1,
        ..AppConfig::default()
    };
    let handle = harness(cfg, &[("qwen3", CategoryId::Work)], Arc::new(witness));

    let error = handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect_err("NaN is not a reading");
    let message = error.to_string();
    assert!(message.contains("witness unavailable"), "was: {message}");
    assert!(
        message.contains("unreadable witness reading"),
        "was: {message}"
    );
    let status = handle.status().await.expect("actor alive");
    assert!(
        status.slots.iter().all(|slot| slot.model.is_none()),
        "refused before spawn"
    );
}

#[tokio::test]
async fn serving_window_folds_total_diff_into_the_yaml_measured_range() {
    // §0.4 item c: while a model serves, the witness total-diff is sampled on
    // `measured_sample_s` and persisted as `measured_gb` (the high end `fixed` books),
    // `measured_gb_min` (the low end the page shows) and the `cpu_offload` setting the
    // pair was taken under, so a later flip voids it at scan.
    // A foreign process already holds 30 GiB. The baseline is the free memory this load
    // was admitted at, so the tenant must never be folded into the range — the
    // guarantee the old pid-attributed sample gave, kept on the new basis.
    let witness = FakeWitness::with(vec![sample(72.0, 30.0, vec![])]);
    let cfg = AppConfig {
        measured_sample_s: 1,
        ..AppConfig::default()
    };
    let (handle, dir) = harness_models(
        cfg,
        &[("a", CategoryId::Work, 8.0)],
        Arc::new(witness.clone()),
    );
    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
    // The card filling by 10 then 20 GiB is this model's own footprint growing — the
    // KV cache spread the range exists to record.
    witness.set(sample(72.0, 40.0, vec![]));
    await_measured(&handle, "a", (Some(10.0), Some(10.0))).await;
    witness.set(sample(72.0, 50.0, vec![]));
    await_measured(&handle, "a", (Some(20.0), Some(10.0))).await;

    let yaml = std::fs::read_to_string(dir.join("a.yaml")).expect("yaml readable");
    assert!(yaml.contains("measured_gb: 20.0"), "was: {yaml}");
    assert!(yaml.contains("measured_gb_min: 10.0"), "was: {yaml}");
    assert!(
        yaml.contains("measured_offload: false"),
        "the pair must carry the setting it was measured under: {yaml}"
    );
    assert!(
        yaml.contains("engine: mock") && yaml.contains("vram_gb: 8"),
        "the write-back preserves the rest of the file: {yaml}"
    );
}

/// The measured pair currently on a catalog entry.
async fn measured_of(handle: &SchedulerHandle, model: &str) -> (Option<f64>, Option<f64>) {
    let catalog = handle.catalog().await.expect("actor alive");
    catalog
        .models
        .iter()
        .find(|m| m.name == model)
        .map_or((None, None), |m| (m.measured_gb, m.measured_min_gb))
}

async fn await_measured(handle: &SchedulerHandle, model: &str, want: (Option<f64>, Option<f64>)) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = measured_of(handle, model).await;
        if got == want {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "range for {model} never reached {want:?}: got {got:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Asserts the pair holds for `hold` — long enough for several sampler ticks — so a
/// later contaminating reading cannot slip in unnoticed.
async fn await_stable(
    handle: &SchedulerHandle,
    model: &str,
    want: (Option<f64>, Option<f64>),
    hold: Duration,
) {
    let until = Instant::now() + hold;
    while Instant::now() < until {
        assert_eq!(
            measured_of(handle, model).await,
            want,
            "{model}'s measured range moved when nothing of its own changed"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn replacement_is_measured_against_the_post_release_baseline() {
    // The admit reading still counts the victim's memory as occupied. Using it as the
    // baseline would measure a replacement short by the victim's whole footprint — and
    // once that diff goes negative, never measure it at all (review, 2026-10-27).
    let witness = FakeWitness::with(vec![sample(72.0, 30.0, vec![])]);
    let mut cfg = AppConfig {
        measured_sample_s: 1,
        ..AppConfig::default()
    };
    cfg.categories[0].max_models = 1; // one slot: the second load must have a victim
    let (handle, dir) = harness_models(
        cfg,
        &[
            ("victim", CategoryId::Work, 30.0),
            ("next", CategoryId::Work, 20.0),
        ],
        Arc::new(witness.clone()),
    );
    handle
        .swap("victim", CategoryId::Work)
        .await
        .expect("victim loads on a card showing its own 30 GiB");
    let Reply::Loaded(_) = handle
        .swap("next", CategoryId::Work)
        .await
        .expect("next replaces it")
    else {
        panic!("expected Loaded");
    };

    // The card now holds only the new engine: 10 GiB of 72, with the victim gone. The
    // baseline must be the free memory *after* the release (72), so the reading gives
    // the engine's true 10 GiB rather than a negative diff that is never recorded.
    witness.set(sample(72.0, 10.0, vec![]));
    await_measured(&handle, "next", (Some(10.0), Some(10.0))).await;
    let yaml = std::fs::read_to_string(dir.join("next.yaml")).expect("yaml readable");
    assert!(yaml.contains("measured_gb: 10.0"), "was: {yaml}");
}

#[tokio::test]
async fn a_tenant_that_loaded_later_is_not_charged_to_the_model() {
    // The regression behind the deleted `measured_charges_only_the_installed_engine`:
    // another category's engine coming up mid-window must not land on this model's
    // high end, which only ever grows (review, 2026-10-27).
    let witness = FakeWitness::with(vec![sample(72.0, 0.0, vec![])]);
    let cfg = AppConfig {
        measured_sample_s: 1,
        ..AppConfig::default()
    };
    let (handle, _dir) = harness_models(
        cfg,
        &[
            ("target", CategoryId::Work, 10.0),
            ("decoy", CategoryId::Aux, 20.0),
        ],
        Arc::new(witness.clone()),
    );
    handle
        .swap("target", CategoryId::Work)
        .await
        .expect("target loads on an idle card");
    witness.set(sample(72.0, 10.0, vec![]));
    await_measured(&handle, "target", (Some(10.0), Some(10.0))).await;

    // The decoy comes up alongside: the card now shows 30 GiB used, 20 of it someone
    // else's. The total-diff sees all of it, so the ledger's booking is what keeps the
    // target's range honest.
    witness.set(sample(72.0, 30.0, vec![]));
    handle
        .swap("decoy", CategoryId::Aux)
        .await
        .expect("decoy loads alongside");
    await_stable(
        &handle,
        "target",
        (Some(10.0), Some(10.0)),
        Duration::from_secs(3),
    )
    .await;
}

#[tokio::test]
async fn a_nonsense_witness_reading_is_never_persisted() {
    // `free_gib()` clamps, so a NaN reading would otherwise arrive as "this model took
    // the whole card" and, because the high end only grows, be written forever.
    let witness = FakeWitness::with(vec![sample(72.0, 0.0, vec![])]);
    let cfg = AppConfig {
        measured_sample_s: 1,
        ..AppConfig::default()
    };
    let (handle, dir) = harness_models(
        cfg,
        &[("model", CategoryId::Work, 10.0)],
        Arc::new(witness.clone()),
    );
    handle
        .swap("model", CategoryId::Work)
        .await
        .expect("model loads");
    witness.set(sample(72.0, 10.0, vec![]));
    await_measured(&handle, "model", (Some(10.0), Some(10.0))).await;

    witness.set(sample(72.0, f64::NAN, vec![]));
    await_stable(
        &handle,
        "model",
        (Some(10.0), Some(10.0)),
        Duration::from_secs(3),
    )
    .await;
    let yaml = std::fs::read_to_string(dir.join("model.yaml")).expect("yaml readable");
    assert!(
        !yaml.contains("measured_gb: 72"),
        "a wedged driver must not become the model's footprint: {yaml}"
    );
}

/// `harness` with a per-model `vram_gb` (the shared one pins 4 GiB for everything).
/// Returns the catalog directory too: write-back tests read the yaml back from it.
fn harness_models(
    cfg: AppConfig,
    specs: &[(&'static str, CategoryId, f64)],
    witness: Arc<dyn VramWitness>,
) -> (SchedulerHandle, std::path::PathBuf) {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    let catalog_dir = simplehar_se::testutil::temp_dir("wit-models");
    for (name, category, vram) in specs {
        std::fs::write(
            catalog_dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: mock\ncategory: {category}\nvram_gb: {vram}\n"),
        )
        .expect("model yaml");
    }
    (
        Scheduler::start(
            cfg,
            catalog_dir.clone(),
            adapters,
            Arc::new(runtime) as Arc<dyn EngineRuntime>,
            witness,
        ),
        catalog_dir,
    )
}

/// A real witness that cannot answer and does not claim ledger mode: the card is
/// unreadable, so there is no honest number to budget against.
struct DeadWitness;

#[async_trait]
impl VramWitness for DeadWitness {
    fn source(&self) -> String {
        "dead".to_string()
    }

    async fn sample(&self) -> Result<MemorySample, String> {
        Err("nvml absent and nvidia-smi failed".to_string())
    }
}

#[tokio::test]
async fn unreadable_card_refuses_rather_than_guesses() {
    // F-3/F-7: on a GPU box a dead witness is a named refusal, never a fallback
    // onto bookkeeping — that is exactly how v3.3 over-committed onto a saturated
    // card (operator 2026-10-26: the ledger left the gate).
    let cfg = AppConfig {
        settle_timeout_s: 1,
        ..AppConfig::default()
    };
    let handle = harness(cfg, &[("qwen3", CategoryId::Work)], Arc::new(DeadWitness));

    let error = handle
        .swap("qwen3", CategoryId::Work)
        .await
        .expect_err("no sample, no load");
    let message = error.to_string();
    assert!(message.contains("witness unavailable"), "was: {message}");
    assert!(
        message.contains("nvidia-smi failed"),
        "the witness's own reason must travel: {message}"
    );

    let status = handle.status().await.expect("actor alive");
    assert!(
        status.slots.iter().all(|slot| slot.model.is_none()),
        "refused before spawn"
    );
    assert!(status
        .jobs
        .iter()
        .any(|job| job.state == simplehar_se::scheduler::SwapState::AdmitFail));
}

#[tokio::test]
async fn concurrent_loads_cannot_double_spend_the_same_bytes() {
    // An 18 GiB load sits in the settle window (its victim's bytes have not come
    // back), so the card still reads 58 of 72 used. A 4 GiB load in another
    // category must be refused: those 18 GiB are committed even though the card
    // cannot have accounted for them yet. The retired ledger counted installed
    // occupants only, so it admitted both and over-committed (operator 2026-10-26).
    let witness = FakeWitness::with(vec![sample(72.0, 58.0, vec![])]);
    let cfg = AppConfig {
        settle_timeout_s: 30,
        vram_reserve_gb: 2.0,
        ..AppConfig::default()
    };
    let (handle, _dir) = harness_models(
        cfg,
        &[
            ("a0", CategoryId::Work, 10.0),
            ("a1", CategoryId::Work, 18.0),
            ("b", CategoryId::Rp, 4.0),
        ],
        Arc::new(witness.clone()),
    );

    handle
        .swap("a0", CategoryId::Work)
        .await
        .expect("a0 loads (12.0 GiB available)");
    let parked = {
        let handle = handle.clone();
        tokio::spawn(async move { handle.swap("a1", CategoryId::Work).await })
    };
    wait_until(&handle, 10, |status| {
        status.jobs.iter().any(|job| {
            job.model == "a1" && job.state == simplehar_se::scheduler::SwapState::Dispose
        })
    })
    .await;

    let error = handle
        .swap("b", CategoryId::Rp)
        .await
        .expect_err("the 18 GiB still in flight must be debited");
    let message = error.to_string();
    assert!(message.contains("insufficient VRAM"), "was: {message}");
    assert!(message.contains("need 4.0 GiB"), "was: {message}");
    // The debit must not become a permanent lockout: once the card frees, a1
    // installs, and b then fits what a1's victim released.
    witness.set(sample(72.0, 40.0, vec![]));
    let Reply::Loaded(receipt) = parked
        .await
        .expect("task alive")
        .expect("a1 loads once the card frees")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "a1");
    let Reply::Loaded(receipt) = handle
        .swap("b", CategoryId::Rp)
        .await
        .expect("b loads after a1 installed")
    else {
        panic!("expected Loaded");
    };
    assert_eq!(receipt.model, "b");
}

#[test]
fn smi_parser_handles_csv_shapes() {
    let parsed =
        parse_smi_sample("24576, 69120\n", "900, 2048\n901, 512 MiB\njunk\n").expect("parses");
    assert_eq!(parsed.total_gib, 24.0);
    assert!((parsed.used_gib - 67.5).abs() < 1e-9);
    assert_eq!(parsed.processes.len(), 2);
    assert_eq!(parsed.processes[0], (900, 2.0));
    assert_eq!(parsed.own_gib(&[900]), Some(2.0));
    assert_eq!(parsed.process_total_gib(), 2.5);
    assert_eq!(
        parsed.free_gib(),
        0.0,
        "used (67.5) above total (24) still reads as zero free, never negative"
    );
    // The settle gate is *physical free*, not a baseline: this sample (total 24,
    // used 67.5) has nothing free, so only a zero target clears the tolerance.
    assert!(parsed.freed_for(0.0, 0.1));
    assert!(!parsed.freed_for(1.0, 0.1));
    let roomy = parse_smi_sample("73728, 30720\n", "").expect("parses");
    assert!(
        roomy.freed_for(40.0, 0.1),
        "42 GiB free clears a 40 GiB gate"
    );
    assert!(!roomy.freed_for(45.0, 0.1));
    assert!(parse_smi_sample("", "").is_err());
}

#[tokio::test]
async fn live_nvidia_witness_samples_or_skips() {
    let witness = simplehar_se::witness::NvmlWitness::new();
    match witness.sample().await {
        Ok(sample) => {
            assert!(sample.total_gib > 0.0, "a GPU reports a positive total");
            eprintln!(
                "live witness: total {:.1} GiB, used {:.1} GiB, {} processes",
                sample.total_gib,
                sample.used_gib,
                sample.processes.len()
            );
        }
        Err(reason) => eprintln!("no NVML/nvidia-smi on this machine ({reason}); skipping"),
    }
}
