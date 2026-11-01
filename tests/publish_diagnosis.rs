//! Ruling 1(b) scheduler wiring, through the shared [`MockRuntime`]: a failed
//! readiness of a container-backed spawn annotates its refusal with the runtime's
//! publish evidence, and NOTHING else changes — a silent diagnosis leaves the
//! refusal exactly as the probe wrote it, a ready spawn never asks, and a
//! failure without a container never asks either (ownership scope: only
//! container-backed spawns can carry the diagnosis at all).

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime, LaunchSpec};
use simplehar_se::model::ModelEntry;
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

/// A `MockAdapter` that optionally installs a container name on the spec under
/// its own engine id — the state ruling 1(b) is scoped to (stock mock specs
/// carry `container: None`, where the diagnosis has nothing to address).
#[derive(Debug, Clone, Copy)]
struct CarryAdapter {
    engine: &'static str,
    container: Option<&'static str>,
}

impl EngineAdapter for CarryAdapter {
    fn engine_id(&self) -> &str {
        self.engine
    }

    fn spec(
        &self,
        model: &ModelEntry,
        port: u16,
    ) -> Result<LaunchSpec, simplehar_se::engine::EngineError> {
        let mut spec = MockAdapter.spec(model, port)?;
        if let Some(container) = self.container {
            spec.container = Some(container.to_string());
        }
        Ok(spec)
    }
}

/// One row: (model name, engine id). Engine ids decide the mode (mock_runtime's
/// table) and the container (`CARRY` below).
fn harness(rows: &[(&str, &'static str)]) -> (SchedulerHandle, MockRuntime, PathBuf) {
    const CARRY: [(&str, Option<&str>); 3] = [
        ("mock-live", Some("ninfer-live")),
        ("mock-hold-never", Some("ninfer-doomed")),
        ("mock-never", None),
    ];
    let dir = simplehar_se::testutil::temp_dir("publish-diagnosis");
    for (name, engine) in rows {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: {engine}\ncategory: work\nvram_gb: 4.0\n"),
        )
        .expect("model yaml");
    }
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    for (engine, container) in CARRY {
        adapters.insert(
            engine.to_string(),
            Arc::new(CarryAdapter { engine, container }) as Arc<dyn EngineAdapter>,
        );
    }
    let handle = Scheduler::start(
        AppConfig {
            load_timeout_s: 1,
            // The never-ready engine's kill parks (mock_runtime's fake seam), so
            // the release phase would otherwise burn the production 30 s window
            // on every failure leg; two seconds exercises the same path.
            release_timeout_s: 2,
            ..AppConfig::default()
        },
        dir.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    );
    (handle, runtime, dir)
}

/// A load must be a `Loaded` receipt for the named model.
fn expect_loaded(reply: Reply, model: &str) {
    match reply {
        Reply::Loaded(receipt) => assert_eq!(receipt.model, model),
        other => panic!("expected Loaded for {model}, got {other:?}"),
    }
}

/// The ruling 1(b) leg: the container is up but published elsewhere, the probe
/// times out, and the operator's refusal says so — the deadline sentence stands
/// with the evidence appended after it.
#[tokio::test]
async fn a_failed_container_load_names_where_the_container_is_published() {
    let (handle, runtime, dir) = harness(&[("pd-evidenced", "mock-hold-never")]);
    runtime.set_publish_evidence(
        "ninfer-doomed",
        "published-port evidence: docker publishes ninfer-doomed as \
         8808/tcp -> 127.0.0.1:18112 (not the harness-offered port 18109)",
    );

    let failure = handle
        .swap("pd-evidenced", CategoryId::Work)
        .await
        .expect_err("this load must fail");
    let text = failure.to_string();
    assert!(text.contains("engine failed to become ready"), "{text}");
    assert!(text.contains("readiness deadline"), "{text}");
    assert!(
        text.contains("18112"),
        "the evidence must reach the refusal: {text}"
    );
    let calls = runtime.publish_diagnoses();
    assert_eq!(
        calls.len(),
        1,
        "one diagnosis per decided failure: {calls:?}"
    );
    assert_eq!(calls[0].0, "ninfer-doomed", "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Evidence-only means evidence-only: with nothing to report the refusal is
/// exactly what the probe wrote, and the ask still happened (so the silent leg
/// pins "asked, found nothing", not "never asked").
#[tokio::test]
async fn a_silent_diagnosis_leaves_the_refusal_exactly_as_the_probe_wrote_it() {
    let (handle, runtime, dir) = harness(&[("pd-silent", "mock-hold-never")]);

    let failure = handle
        .swap("pd-silent", CategoryId::Work)
        .await
        .expect_err("this load must fail");
    let text = failure.to_string();
    assert!(text.contains("readiness deadline"), "{text}");
    assert!(
        !text.contains("published-port evidence"),
        "nothing was published elsewhere: {text}"
    );
    let calls = runtime.publish_diagnoses();
    assert_eq!(calls.len(), 1, "a container failure asks once: {calls:?}");
    assert_eq!(calls[0].0, "ninfer-doomed", "{calls:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The verdict is the probe's, unchanged: a ready container-backed spawn ships
/// the `Loaded` receipt and the diagnosis seam is never touched — the happy
/// path pays no `docker port` round trip.
#[tokio::test]
async fn a_ready_load_never_asks_where_the_container_is_published() {
    let (handle, runtime, dir) = harness(&[("pd-live", "mock-live")]);

    expect_loaded(
        handle
            .swap("pd-live", CategoryId::Work)
            .await
            .expect("load succeeds"),
        "pd-live",
    );
    assert!(
        runtime.publish_diagnoses().is_empty(),
        "evidence is a failure-path diagnosis only"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Ownership scope at the scheduler door: a failure with no container behind it
/// has nothing to address and must not fire the seam at all.
#[tokio::test]
async fn a_containerless_failure_never_asks() {
    let (handle, runtime, dir) = harness(&[("pd-bare", "mock-never")]);

    let failure = handle
        .swap("pd-bare", CategoryId::Work)
        .await
        .expect_err("this load must fail");
    assert!(
        failure.to_string().contains("readiness deadline"),
        "{failure}"
    );
    assert!(
        runtime.publish_diagnoses().is_empty(),
        "no container, no question"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
