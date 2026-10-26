//! Entry point: boot order per DESIGN §0.2 — config first, listeners bound before any
//! engine work (there is none: preload is scrapped), zero engine spawns at boot.

use simplehar_se::api::{category_router, control_router, ApiState};
use simplehar_se::config::{AppConfig, ConfigError};
use simplehar_se::engine::llama::LlamaAdapter;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::observability;
use simplehar_se::process::{ProcessRuntime, SingleInstanceGuard};
use simplehar_se::scheduler::Scheduler;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// Loads and validates boot config from beside the executable.
///
/// A missing file yields defaults ("no config file" is usable, §0.1); an invalid file
/// or any validation failure is fatal and every problem is logged (never silently
/// defaulted — defect D1).
fn boot_config() -> Result<AppConfig, Vec<ConfigError>> {
    // The test harness (and operators) can point at a config anywhere; otherwise
    // the config lives beside the executable.
    let path = std::env::var("SIMPLEHAR_CONFIG")
        .map(PathBuf::from)
        .ok()
        .or_else(|| {
            std::env::current_exe()
                .ok()
                .and_then(|exe| exe.parent().map(|dir| dir.join("simplehar.json")))
        });
    let mut config = match &path {
        Some(path) if path.exists() => AppConfig::load(path).map_err(|e| vec![e])?,
        _ => {
            tracing::info!("no simplehar.json found; using built-in defaults");
            AppConfig::default()
        }
    };
    // Stamp where the config came from (or where it will live) so `default_model`
    // write-back targets the same file boot read — a `SIMPLEHAR_CONFIG` run must not
    // persist defaults into a file it never loads (operator finding, 2026-10-26).
    config.source_path = path;
    let errors = config.validate();
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let boot_started = std::time::Instant::now();
    observability::init();
    // A10 single-instance guard (§0.2 step 3): a second harness refuses to boot.
    let _guard = match SingleInstanceGuard::acquire() {
        Ok(guard) => guard,
        Err(reason) => {
            tracing::error!(reason = %reason, "refusing to start");
            return ExitCode::FAILURE;
        }
    };
    let config = match boot_config() {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors {
                tracing::error!(error = %error, "invalid configuration");
            }
            return ExitCode::FAILURE;
        }
    };
    let config_ms = boot_started.elapsed().as_millis() as u64;
    let mut adapters = std::collections::BTreeMap::new();
    let mock: Arc<dyn EngineAdapter> = Arc::new(MockAdapter);
    adapters.insert("mock".to_string(), mock);
    let llama: Arc<dyn EngineAdapter> = Arc::new(LlamaAdapter::new(
        config.llama_server_exe.clone(),
        config.internal_host.to_string(),
    ));
    adapters.insert("llama".to_string(), llama);
    let runtime: Arc<dyn EngineRuntime> = Arc::new(ProcessRuntime::new());
    let witness: Arc<dyn simplehar_se::budget::VramWitness> =
        Arc::new(simplehar_se::witness::NvmlWitness::new());
    let scheduler = Scheduler::start(
        config.clone(),
        config.modelconf_dir.clone(),
        adapters,
        runtime,
        Arc::clone(&witness),
    );
    if !config.modelconf_dir.exists() {
        // Friendly default: a missing ModelConf dir is created, not reported —
        // "no config file" still means a usable harness (§0.1); the operator adds
        // yamls and rescans.
        match std::fs::create_dir_all(&config.modelconf_dir) {
            Ok(()) => tracing::info!(
                dir = %config.modelconf_dir.display(),
                "ModelConf dir created (drop model yamls in and Rescan)"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                dir = %config.modelconf_dir.display(),
                "cannot create ModelConf dir; the catalog will report it"
            ),
        }
    }
    let scan_ms = boot_started.elapsed().as_millis() as u64;
    let listeners = simplehar_se::listeners::ListenerRegistry::new();
    let boot = simplehar_se::api::BootTimings {
        config_ms,
        scan_ms,
        ..simplehar_se::api::BootTimings::default()
    };
    let state = Arc::new(ApiState::new(
        config.clone(),
        scheduler.clone(),
        listeners.clone(),
        witness,
        boot,
    ));

    // One shutdown channel for every exit path: Ctrl+C and the tray's Exit item
    // (v3.3 §20 lesson: a single Notify can only wake one waiter).
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    if config.tray_enabled {
        let control_url = format!("http://{}/", state.control_addr);
        simplehar_se::tray::spawn(
            scheduler,
            listeners,
            control_url,
            tokio::runtime::Handle::current(),
            shutdown,
        );
    }

    match run(state, shutdown_rx, boot_started).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "harness stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Pause between listener rebind attempts while a port is unavailable.
const LISTENER_RETRY: Duration = Duration::from_secs(5);

/// Runs one listener under a supervisor: bind, serve, and on any end try to rebind —
/// a busy port marks the category `Down` (tray red, `/har/status` names the port)
/// until a retry succeeds. No bind failure is fatal: partial availability beats a
/// dead process (G11, §0.2 failure contract) — even for the control listener.
async fn supervise_listener(
    state: Arc<ApiState>,
    category: simplehar_se::config::CategoryConfig,
    listeners: simplehar_se::listeners::ListenerRegistry,
) {
    let addr = SocketAddr::new(state.config.internal_host, category.public_port);
    let router = if category.control {
        control_router(Arc::clone(&state))
    } else {
        category_router(Arc::clone(&state), category.id)
    };
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                listeners.mark_serving(category.id);
                tracing::info!(%addr, category = %category.id, "listener serving");
                if let Err(error) = axum::serve(listener, router.clone()).await {
                    tracing::error!(%addr, category = %category.id, error = %error,
                        "listener stopped; attempting rebind");
                }
            }
            Err(error) => {
                listeners.mark_down(category.id, format!("cannot bind {addr}: {error}"));
                tracing::error!(%addr, category = %category.id, error = %error,
                    "listener down (port busy?); retrying; harness continues (G11)");
            }
        }
        tokio::time::sleep(LISTENER_RETRY).await;
    }
}

/// Starts one supervisor per category listener and waits for shutdown.
async fn run(
    state: Arc<ApiState>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    boot_started: std::time::Instant,
) -> anyhow::Result<()> {
    tracing::info!(
        control = %state.control_addr,
        "simplehar-se booting: listeners before engines, zero engine spawns (§0.2)"
    );
    let host = state.config.internal_host;
    if !host.is_loopback() {
        // §0.4, adjusted by the operator note: publishing to the local net trusts
        // every machine on the segment; there is no auth, only the guards.
        tracing::warn!(
            %host,
            "publishing to the local network: the harness has NO authentication; every device on the segment can load models and drive the GPUs"
        );
    }
    let listeners = state.listeners.clone();
    let mut supervisors = Vec::new();
    for category in state.config.categories.clone() {
        supervisors.push(tokio::spawn(supervise_listener(
            Arc::clone(&state),
            category,
            listeners.clone(),
        )));
    }
    // §0.2 "ready": every listener is Serving or Down. Wait (bounded) for the
    // supervisors' first verdict, then record the boot measures.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = state.listeners.snapshot();
        if snapshot.len() >= state.config.categories.len()
            || tokio::time::Instant::now() >= deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    {
        let all = state.listeners.snapshot();
        state
            .boot
            .write()
            .expect("boot timings lock poisoned by a writer panic")
            .set_bound(boot_started.elapsed().as_millis() as u64);
        tracing::info!(
            bind_ms = boot_started.elapsed().as_millis() as u64,
            serving = all
                .values()
                .filter(|s| matches!(s, simplehar_se::listeners::ListenerState::Serving))
                .count(),
            "boot ready (0.2): config validated, catalog scanned, listeners resolved"
        );
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {},
        _ = shutdown_rx.changed() => {},
    }
    tracing::info!("shutdown requested; stopping listeners");
    for supervisor in supervisors {
        supervisor.abort();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
