//! Idle-timeout tests (DESIGN Step 5): a category with `idle_timeout_s > 0`
//! unloads unused occupants on the actor's sweep; 0 disables; the in-flight-job
//! guard keeps the sweep off swapping slots; and WORK's never-idle default holds.

mod mock_runtime;

use mock_runtime::MockRuntime;
use simplehar_se::budget::LedgerOnlyWitness;
use simplehar_se::config::{AppConfig, CategoryId};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::scheduler::{JobKind, Scheduler, SchedulerHandle};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn harness(config: AppConfig) -> SchedulerHandle {
    let runtime = MockRuntime::new();
    let mut adapters: BTreeMap<String, Arc<dyn EngineAdapter>> = BTreeMap::new();
    adapters.insert("mock".to_string(), Arc::new(MockAdapter));
    // FINDINGS-20 [9]: the suite used to register ONLY the instant adapter, so no
    // test here could ever have a job in flight — the sweep's `slot.job.is_none()`
    // guard was untested anywhere. `mock-slow` gives a 2.5 s readiness window to
    // collide with the 2 s tick.
    adapters.insert("mock-slow".to_string(), Arc::new(MockAdapter));
    let dir = simplehar_se::testutil::temp_dir("idle");
    for (name, category, engine) in [
        ("idle-me", CategoryId::Rp, "mock"),
        ("stayer", CategoryId::Work, "mock"),
        ("slow-me", CategoryId::Rp, "mock-slow"),
    ] {
        std::fs::write(
            dir.join(format!("{name}.yaml")),
            format!("name: {name}\nengine: {engine}\ncategory: {category}\nvram_gb: 4\n"),
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
    // Generous readiness deadline: this suite runs in parallel with every other
    // test binary; a 5 s probe deadline flakes under load (flake policy: widen,
    // never game).
    let mut cfg = AppConfig {
        load_timeout_s: 30,
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
    // FINDINGS-20 [10]: the WORK `stayer` fixture existed but no test loaded it, so
    // the documented "WORK never idles out" guarantee (DESIGN, and WORK's `0`
    // default) had no test anywhere — it is a DEFAULT, not a rule: nothing
    // validates WORK's timeout, so the behavioural proof is here. The sweep must
    // take only the opted-in RP occupant and leave the actively-listed WORK model
    // installed while RP empties.
    let handle = harness(config(1)); // RP idles out after 1 s; WORK stays at 0
    handle
        .swap("stayer", CategoryId::Work)
        .await
        .expect("WORK loads");
    handle.swap("idle-me", CategoryId::Rp).await.expect("loads");
    wait_until(&handle, 8, |status| {
        status
            .slots
            .iter()
            .filter(|slot| slot.category == CategoryId::Rp)
            .all(|slot| slot.model.is_none())
    })
    .await;
    let status = handle.status().await.expect("actor alive");
    let unload_job = status
        .jobs
        .iter()
        .find(|job| job.outcome.as_deref() == Some("idle timeout"))
        .expect("idle unload recorded");
    assert_eq!(unload_job.model, "idle-me");
    assert_eq!(
        unload_job.kind,
        JobKind::Unload,
        "the sweep records an unload row"
    );
    assert_eq!(
        status
            .slots
            .iter()
            .find(|slot| slot.category == CategoryId::Work)
            .and_then(|slot| slot.model.as_deref()),
        Some("stayer"),
        "WORK never idles out: the sweep took only the category that opted in"
    );
    assert!(
        !status
            .jobs
            .iter()
            .any(|job| job.model == "stayer" && job.kind == JobKind::Unload),
        "the WORK occupant was never even queued for an idle unload"
    );
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

#[tokio::test]
async fn all_four_categories_boot_with_the_never_idle_default() {
    // FINDINGS-20 [10], the canary half: "WORK never idles out" is a DEFAULT, not a
    // validated rule — grep `src/` and nothing pins any category's `idle_timeout_s`
    // to 0. Flip a default (WORK to 600, say) and the operator's actively-used
    // model silently unloads mid-session with an otherwise-green suite; this row
    // fails on the spot, naming the drifted category.
    let cfg = AppConfig::default();
    assert_eq!(cfg.categories.len(), 4, "the four shipped categories");
    for cat in &cfg.categories {
        assert_eq!(
            cat.idle_timeout_s, 0,
            "category {} boots never-idle; a nonzero default would silently unload \
             the operator's active models (the behavioural pin for WORK is \
             idle_category_unloads_after_the_threshold)",
            cat.id
        );
    }
}

#[tokio::test]
async fn a_slot_with_a_job_in_flight_survives_the_idle_sweep() {
    // FINDINGS-20 [9]: the sweep's `slot.job.is_none()` filter is what stops a 2 s
    // tick from starting an unload on a slot mid-Spawn/Ready/Install. Nothing in
    // the repo combined a nonzero `idle_timeout_s` with an in-flight job (this file
    // registered only instant engines, so it never could). Here RP idles at 1 s and
    // a 2.5 s-readiness load straddles the tick: the occupant AND its job must
    // survive every tick that lands mid-swap, then idle out normally after it is
    // serving and untouched. An inverted filter unloads the slot under the live
    // swap — the load never completes, or completes onto a slot the sweep cleared —
    // and one of the two assertions below trips.
    let handle = harness(config(1)); // RP idles after 1 s of silence
    let client = handle.clone();
    let loading = tokio::spawn(async move { client.swap("slow-me", CategoryId::Rp).await });

    // The load must actually be in flight when a sweep tick fires: assert the
    // premise instead of hoping the timing works.
    wait_until(&handle, 5, |status| {
        status
            .slots
            .iter()
            .any(|slot| slot.category == CategoryId::Rp && slot.loading_job.is_some())
    })
    .await;
    // Hold the premise across the first tick (IDLE_TICK is 2 s; the readiness
    // window is 2.5 s): poll while the job is still in flight and demand the
    // occupant not appear AND the job not disappear.
    let premise_deadline = Instant::now() + Duration::from_millis(2_300);
    while Instant::now() < premise_deadline {
        let status = handle.status().await.expect("actor alive");
        let rp = status
            .slots
            .iter()
            .find(|slot| slot.category == CategoryId::Rp)
            .expect("RP slot");
        assert!(
            rp.loading_job.is_some(),
            "the sweep must not cancel or unload the in-flight swap: {rp:?}"
        );
        assert!(
            rp.model.is_none(),
            "the sweep must not install or replace anything mid-swap: {rp:?}"
        );
        assert!(
            !status
                .jobs
                .iter()
                .any(|j| j.kind == JobKind::Unload && j.model == "slow-me"),
            "no unload row may exist for a slot with a live job: {:?}",
            status.jobs
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // The load then completes on its own…
    let reply = tokio::time::timeout(Duration::from_secs(15), loading)
        .await
        .expect("the load must answer")
        .expect("no panic");
    assert!(
        reply.is_ok(),
        "the swapped-under-way load still completes: {reply:?}"
    );
    let status = handle.status().await.expect("actor alive");
    assert_eq!(
        status
            .slots
            .iter()
            .find(|slot| slot.category == CategoryId::Rp)
            .and_then(|slot| slot.model.as_deref()),
        Some("slow-me"),
        "the occupant survived the mid-swap ticks"
    );

    // …and only THEN ages out under the 1 s threshold (touched at load; the sweep
    // runs every 2 s). This half rules out the opposite cheat: a sweep broken so
    // thoroughly that it never fires at all would pass the guard checks above.
    wait_until(&handle, 15, |status| {
        status
            .slots
            .iter()
            .all(|slot| slot.category != CategoryId::Rp || slot.model.is_none())
            && status.jobs.iter().any(|job| {
                job.kind == JobKind::Unload
                    && job.model == "slow-me"
                    && job.outcome.as_deref() == Some("idle timeout")
            })
    })
    .await;
}
