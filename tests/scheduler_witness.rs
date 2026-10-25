//! Witness integration tests (Step 4, choice D): admission subtracts external
//! reservations observed by the witness, and the settle-after-release gate blocks a
//! spawn while the released engine's memory is still visible. Driven by a scripted
//! `FakeWitness` against the mock engine; plus pure parser tests and one skip-safe
//! live NVML sample.

mod mock_runtime;

use async_trait::async_trait;
use mock_runtime::MockRuntime;
use simplehar_se::budget::{parse_smi_sample, MemorySample, VramWitness};
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Scripted witness: samples come off the queue (the last one repeats), and an
/// explicit override always wins — tests flip behavior mid-flight, e.g. when the
/// released engine's memory "returns" to the driver.
#[derive(Clone, Default)]
struct FakeWitness {
    queue: Arc<Mutex<VecDeque<MemorySample>>>,
    override_now: Arc<Mutex<Option<MemorySample>>>,
}

impl FakeWitness {
    fn with(samples: Vec<MemorySample>) -> Self {
        FakeWitness {
            queue: Arc::new(Mutex::new(samples.into())),
            override_now: Arc::new(Mutex::new(None)),
        }
    }

    fn set(&self, sample: MemorySample) {
        *self.override_now.lock().expect("witness override lock") = Some(sample);
    }
}

#[async_trait]
impl VramWitness for FakeWitness {
    async fn sample(&self) -> Result<MemorySample, String> {
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
    let dir = std::env::temp_dir().join(format!("se-wit-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
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
async fn admission_subtracts_witness_externals() {
    // The card is 72 GiB; a foreign process (pid 900) holds 70. A 4 GiB load must
    // refuse with the numbers (Step 4 rule 2; the F-7 lie is gone).
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
async fn settle_blocks_spawn_while_released_memory_is_visible() {
    // Load model A (witness sees 4 GiB under our pid), then swap to B: after the
    // release, the witness still reports 4 GiB — settle must time out, refuse with
    // the numbers, and never spawn B (G12: no double-spawn).
    let witness = FakeWitness::with(vec![
        sample(72.0, 4.0, vec![(0, 4.0)]), // admit A: ours
    ]);
    let cfg = AppConfig {
        settle_timeout_s: 1,
        ..AppConfig::default()
    };
    let handle = harness(
        cfg,
        &[("a", CategoryId::Work), ("b", CategoryId::Work)],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
    // The settle samples now see the leftover as used-above-expected (externals
    // were 0 at admit), so the gate holds until the deadline.
    let error = handle
        .swap("b", CategoryId::Work)
        .await
        .expect_err("settle must refuse while memory is not released");
    let message = error.to_string();
    assert!(message.contains("settle timeout"), "was: {message}");
    assert!(message.contains("4.0 GiB used"), "was: {message}");
    assert!(message.contains("2.0 GiB expected"), "was: {message}");

    let status = handle.status().await.expect("actor alive");
    assert!(
        status.slots.iter().all(|slot| slot.model.is_none()),
        "the replaced occupant is gone and B was never installed"
    );
}

#[tokio::test]
async fn settle_passes_once_memory_returns_to_baseline() {
    // Same start, but mid-settle the witness drops to baseline: B loads and serves.
    let witness = FakeWitness::with(vec![sample(72.0, 4.0, vec![(0, 4.0)])]);
    let cfg = AppConfig {
        settle_timeout_s: 5,
        ..AppConfig::default()
    };
    let handle = harness(
        cfg,
        &[("a", CategoryId::Work), ("b", CategoryId::Work)],
        Arc::new(witness.clone()),
    );

    let Reply::Loaded(_) = handle.swap("a", CategoryId::Work).await.expect("a loads") else {
        panic!("expected Loaded");
    };
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
    witness.set(sample(72.0, 0.0, vec![]));
    let reply = awaiting_b
        .await
        .expect("task alive")
        .expect("b loads after settle");
    assert!(matches!(reply, Reply::Loaded(_)), "got {reply:?}");
}

#[test]
fn smi_parser_handles_csv_shapes() {
    let parsed =
        parse_smi_sample("24576, 69120\n", "900, 2048\n901, 512 MiB\njunk\n").expect("parses");
    assert_eq!(parsed.total_gib, 24.0);
    assert!((parsed.used_gib - 67.5).abs() < 1e-9);
    assert_eq!(parsed.processes.len(), 2);
    assert_eq!(parsed.processes[0], (900, 2.0));
    assert_eq!(parsed.external_gib(&[900]), 0.5);
    assert!(parsed.settled(67.5, 0.1));
    assert!(!parsed.settled(0.0, 0.1));
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
