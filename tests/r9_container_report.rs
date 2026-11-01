//! R9 report-present legs (the seam the unit pins deliberately leave dark): a real
//! load through the scheduler actor against [`MockRuntime`], whose `container_report`
//! answers with a seeded startup `MemorySummary`. The in-module pins only call the
//! fold directly, so a regression that swallows the one-shot between the runtime and
//! the actor — the engine gate, the readiness call site, the parse, the message
//! plumbing — is invisible there and fatal here.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::llama::LlamaAdapter;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::ninfer::NinferAdapter;
use simplehar_se::engine::strata::StrataAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{Reply, Scheduler, SchedulerHandle};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const GIB: u64 = 1_073_741_824;

/// The engine's own startup record, in the shape the built-commit clones emit
/// (`environment.total_device_memory_bytes` + `memory.available_after_startup_bytes`):
/// a 72 GiB card with 31.2 GiB reserved after startup.
fn memory_summary(total_gib: u64, reserved_gib_tenths: u64) -> String {
    let total = total_gib * GIB;
    let available = total - reserved_gib_tenths * GIB / 10;
    format!(
        "ninfer-serve: engine ready | weights 8.16 GiB\n\
         {{\"event\":\"startup\",\"memory\":{{\"weights_bytes\":8589934592,\
         \"available_after_startup_bytes\":{available}}},\
         \"environment\":{{\"total_device_memory_bytes\":{total}}}}}\n"
    )
}

/// One model yaml: name, engine, category, declared VRAM, extra raw lines.
type Spec = (
    &'static str,
    &'static str,
    CategoryId,
    Option<f64>,
    &'static str,
);

fn write_modelconf(specs: &[Spec]) -> PathBuf {
    let dir = simplehar_se::testutil::temp_dir("r9-report");
    for (name, engine, category, vram_gb, extra) in specs {
        let mut yaml = format!("name: {name}\nengine: {engine}\ncategory: {category}\n");
        if let Some(vram) = vram_gb {
            yaml.push_str(&format!("vram_gb: {vram}\n"));
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

/// The scheduler_actor harness shape — the REAL adapters (launch validation must
/// come from production code) over the faked runtime — but handing back the catalog
/// directory, because these legs assert on what the fold persisted to the yaml.
fn harness(specs: &[Spec]) -> (SchedulerHandle, MockRuntime, PathBuf) {
    let dir = write_modelconf(specs);
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    adapters.insert(
        ContainerMockAdapter::ENGINE.to_string(),
        Arc::new(ContainerMockAdapter {
            engine: ContainerMockAdapter::ENGINE,
            container: "ninfer-mock",
        }) as Arc<dyn EngineAdapter>,
    );
    for adapter in [
        Arc::new(StrataAdapter::default()) as Arc<dyn EngineAdapter>,
        Arc::new(NinferAdapter::new("Ubuntu", "ops")),
        Arc::new(LlamaAdapter::new(PathBuf::from("llama-server.exe"))),
    ] {
        adapters.insert(adapter.engine_id().to_string(), adapter);
    }
    let handle = Scheduler::start(
        AppConfig {
            load_timeout_s: 5,
            ..AppConfig::default()
        },
        dir.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    );
    (handle, runtime, dir)
}

/// A `MockAdapter` that installs a container name on the spec under its own engine
/// id. Without it, the sampler's engine gate is unreachable defence-in-depth —
/// stock mock specs carry `container: None`, so `report_container` returned at the
/// container check before the engine check ever ran, and the "non-ninfer report is
/// ignored" pin passed under any mutation of the gate (review 2026-10-31, pin
/// honesty). A spec WITH a container under a non-ninfer engine is precisely the
/// state the gate exists to refuse.
#[derive(Debug, Clone)]
struct ContainerMockAdapter {
    engine: &'static str,
    container: &'static str,
}

impl ContainerMockAdapter {
    /// The engine id the failure-leg rows carry; registered under this id in
    /// [`harness`].
    const ENGINE: &'static str = "mock-carry";
}

impl EngineAdapter for ContainerMockAdapter {
    fn engine_id(&self) -> &str {
        self.engine
    }

    fn spec(
        &self,
        model: &simplehar_se::model::ModelEntry,
        port: u16,
    ) -> Result<simplehar_se::engine::LaunchSpec, simplehar_se::engine::EngineError> {
        let mut spec = MockAdapter.spec(model, port)?;
        spec.container = Some(self.container.to_string());
        Ok(spec)
    }
}

/// The same nine lines every ninfer row needs, spelled at the call site because
/// `Spec` keeps its strings `'static` for the table-literal harness calls.
macro_rules! ninfer_extra {
    ($name:literal, $container:literal, $extra:literal) => {
        concat!(
            "ninfer_autostart: /opt/",
            $name,
            ".sh\nninfer_container: ",
            $container,
            "\n",
            $extra
        )
    };
}

/// Polls the model's yaml until it contains every needle, or fails with the last
/// bytes seen (the persist is async behind the load reply, so this is a wait, not
/// a sleep-and-hope).
async fn await_yaml(yaml: &Path, needles: &[&str]) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let text = std::fs::read_to_string(yaml).unwrap_or_default();
        if needles.iter().all(|needle| text.contains(needle)) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "needles {needles:?} never appeared in {}; last bytes:\n{text}",
            yaml.display()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// A load must be a `Loaded` receipt for the named model.
fn expect_loaded(reply: Reply, model: &str) {
    match reply {
        Reply::Loaded(receipt) => assert_eq!(receipt.model, model),
        other => panic!("expected Loaded for {model}, got {other:?}"),
    }
}

/// The whole R9 chain, end to end: readiness fetches the container report through
/// the runtime seam, the ninfer parse turns the engine's own startup record into a
/// reserved figure, the actor books it, and the fold persists pair + provenance +
/// profile to the spawn-era yaml. The stale restart record ahead of the live one
/// pins "newest startup wins".
#[tokio::test]
async fn a_container_memory_summary_books_engine_report_end_to_end() {
    let (handle, runtime, dir) = harness(&[(
        "r9-live",
        "ninfer",
        CategoryId::Work,
        Some(20.0),
        ninfer_extra!("r9-live", "ninfer-r9", ""),
    )]);
    let stale = memory_summary(72, 500); // an earlier, larger launch of the same container
    runtime.set_container_report(
        "ninfer-r9",
        &format!("{stale}{}\n", memory_summary(72, 312)),
    );

    let reply = handle.swap("r9-live", CategoryId::Work).await;
    expect_loaded(reply.expect("load succeeds"), "r9-live");

    let text = await_yaml(
        &dir.join("r9-live.yaml"),
        &[
            "measured_gb: 31.2",
            "measured_gb_min: 31.2",
            "measured_attribution: engine_report",
            "measured_profile: b",
        ],
    )
    .await;
    assert!(
        !text.contains("50.0"),
        "the stale restart record must lose to the newest one:\n{text}"
    );
    let saved = runtime.saved_container_logs();
    assert!(
        saved
            .iter()
            .any(|(model, body)| model == "r9-live"
                && body.contains("available_after_startup_bytes")),
        "the readiness snapshot must reach the forensic store: {saved:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The fallback shape through the REAL sampler: no report seeded, so the one-shot
/// finds nothing and the ledger-only witness mints nothing — the yaml stays clean
/// of a booked pair rather than gaining a guessed one.
#[tokio::test]
async fn a_container_without_a_report_books_nothing() {
    let (handle, _runtime, dir) = harness(&[(
        "r9-silent",
        "ninfer",
        CategoryId::Work,
        Some(20.0),
        ninfer_extra!("r9-silent", "ninfer-silent", ""),
    )]);
    let reply = handle.swap("r9-silent", CategoryId::Work).await;
    expect_loaded(reply.expect("load succeeds"), "r9-silent");

    // The load is served and some time has passed for a one-shot; absence is then
    // the pinned fact (a booked pair would have landed on the same fast path as
    // the report-present leg above).
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = std::fs::read_to_string(dir.join("r9-silent.yaml")).expect("seed yaml");
    assert!(
        !text.contains("measured_gb"),
        "no report, no booking, no stamp:\n{text}"
    );
    assert!(
        runtime_saved_nothing(&_runtime),
        "a report the seam never had cannot be snapshotted"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn runtime_saved_nothing(runtime: &MockRuntime) -> bool {
    runtime.saved_container_logs().is_empty()
}

/// Fail-closed on garbage: neither a record missing the environment fields nor the
/// engine's `required_to_free_gb`-only wish (the parser demands the byte pair it can
/// independently check) may book, even though both are fetched, snapshotted, and
/// parsed on the way to `None`.
#[tokio::test]
async fn an_unparseable_or_incomplete_report_never_books() {
    let (handle, runtime, dir) = harness(&[
        (
            "r9-junk",
            "ninfer",
            CategoryId::Work,
            Some(20.0),
            ninfer_extra!("r9-junk", "ninfer-junk", ""),
        ),
        (
            "r9-partial",
            "ninfer",
            CategoryId::Work,
            Some(20.0),
            ninfer_extra!("r9-partial", "ninfer-partial", ""),
        ),
    ]);
    runtime.set_container_report("ninfer-junk", "engine ready\n{not json at all\n{{{\n");
    runtime.set_container_report(
        "ninfer-partial",
        "{\"memory\":{\"required_to_free_gb\":40.0}}",
    );

    expect_loaded(
        handle
            .swap("r9-junk", CategoryId::Work)
            .await
            .expect("load"),
        "r9-junk",
    );
    expect_loaded(
        handle
            .swap("r9-partial", CategoryId::Work)
            .await
            .expect("load"),
        "r9-partial",
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    for name in ["r9-junk", "r9-partial"] {
        let text = std::fs::read_to_string(dir.join(format!("{name}.yaml"))).expect("seed yaml");
        assert!(
            !text.contains("measured_gb") && !text.contains("measured_attribution"),
            "a figure the parser refused cannot reach the row: {name}:\n{text}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Declared wins unconditionally (ruling 15 (ii) bound): the report may PERSIST its
/// honest pair next to a declared row — the forensic value stands — while the slot
/// keeps charging the operator's declared figure.
#[tokio::test]
async fn a_declared_row_persists_the_report_but_charges_declared() {
    let (handle, runtime, dir) = harness(&[(
        "r9-declared",
        "ninfer",
        CategoryId::Work,
        Some(8.0),
        "ninfer_autostart: /opt/r9-declared.sh\nninfer_container: ninfer-declared\n",
    )]);
    runtime.set_container_report("ninfer-declared", &memory_summary(72, 312));

    expect_loaded(
        handle
            .swap("r9-declared", CategoryId::Work)
            .await
            .expect("load"),
        "r9-declared",
    );
    let text = await_yaml(
        &dir.join("r9-declared.yaml"),
        &["measured_gb: 31.2", "measured_attribution: engine_report"],
    )
    .await;
    let status = handle.status().await.expect("actor alive");
    let need = status
        .slots
        .iter()
        .find(|s| s.category == CategoryId::Work)
        .and_then(|s| s.need_gib);
    assert_eq!(
        need,
        Some(8.0),
        "the declared figure wins the charge no matter what the engine reserved:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The sampler's engine gate, REACHABLY: the row spawns with a container installed
/// (via [`ContainerMockAdapter`]) under a non-ninfer engine id, so `report_container`
/// must refuse at the engine check — deleting that check lands the seeded report on
/// the mock row and fails this pin (the first cut drove it with a container-less
/// mock spec, where the earlier container check hid the gate from every mutation).
#[tokio::test]
async fn a_report_for_a_non_ninfer_engine_is_ignored() {
    let (handle, runtime, dir) = harness(&[(
        "r9-mock",
        ContainerMockAdapter::ENGINE,
        CategoryId::Work,
        Some(4.0),
        "",
    )]);
    runtime.set_container_report("ninfer-mock", &memory_summary(72, 312));

    expect_loaded(
        handle
            .swap("r9-mock", CategoryId::Work)
            .await
            .expect("load"),
        "r9-mock",
    );
    tokio::time::sleep(Duration::from_millis(300)).await;
    let text = std::fs::read_to_string(dir.join("r9-mock.yaml")).expect("seed yaml");
    assert!(
        !text.contains("measured_gb"),
        "only the ninfer sampler may book a container report:\n{text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The failure leg of the forensic contract (GOTCHAS `[proc]`: an engine log dies
/// with `docker rm -f`): a load that never reaches readiness must still put the
/// container's bytes on the Windows side, and the in-flight snapshot mark must
/// release afterwards — a second failure re-snapshots, because a wedged mark that
/// silently blinded every later failure had zero pins (review 2026-10-31, L9).
#[tokio::test]
async fn a_failed_launch_snapshots_its_container_log_and_can_snapshot_again() {
    // A row under the container-carrying never-ready engine: harness-built ad hoc,
    // because `harness` maps only the stock ids.
    let dir = write_modelconf(&[(
        "r9-doomed",
        "mock-hold-never",
        CategoryId::Work,
        Some(4.0),
        "",
    )]);
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock-hold-never".to_string(),
        Arc::new(ContainerMockAdapter {
            engine: "mock-hold-never",
            container: "ninfer-doomed",
        }) as Arc<dyn EngineAdapter>,
    );
    let handle = Scheduler::start(
        AppConfig {
            load_timeout_s: 1,
            ..AppConfig::default()
        },
        dir.clone(),
        adapters,
        Arc::new(runtime.clone()) as Arc<dyn EngineRuntime>,
        Arc::new(simplehar_se::budget::LedgerOnlyWitness),
    );
    runtime.set_container_report("ninfer-doomed", "engine crashed on startup\n");

    for leg in 1..=2 {
        let failure = handle.swap("r9-doomed", CategoryId::Work).await;
        assert!(
            failure.is_err(),
            "leg {leg}: this load must fail: {failure:?}"
        );
        let saved = await_saved(&runtime, leg).await;
        assert!(
            saved
                .iter()
                .any(|(model, body)| model == "r9-doomed"
                    && body.contains("engine crashed on startup")),
            "leg {leg}: the failed launch's container bytes must be snapshotted: {saved:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Polls the runtime's forensic store until at least `count` snapshots landed.
async fn await_saved(runtime: &MockRuntime, count: usize) -> Vec<(String, String)> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let saved = runtime.saved_container_logs();
        if saved.len() >= count {
            return saved;
        }
        assert!(
            Instant::now() < deadline,
            "only {} of {count} snapshots landed: {saved:?}",
            saved.len()
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
