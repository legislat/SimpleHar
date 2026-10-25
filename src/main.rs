//! Entry point: boot order per DESIGN §0.2 — config first, listeners bound before any
//! engine work (there is none: preload is scrapped), zero engine spawns at boot.

use anyhow::Context;
use simplehar_se::api::{category_router, control_router, ApiState};
use simplehar_se::config::{AppConfig, ConfigError};
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::unimplemented::UnimplementedRuntime;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::observability;
use simplehar_se::scheduler::Scheduler;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

/// Loads and validates boot config from beside the executable.
///
/// A missing file yields defaults ("no config file" is usable, §0.1); an invalid file
/// or any validation failure is fatal and every problem is logged (never silently
/// defaulted — defect D1).
fn boot_config() -> Result<AppConfig, Vec<ConfigError>> {
    let path = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("simplehar.json")));
    let config = match &path {
        Some(path) if path.exists() => AppConfig::load(path).map_err(|e| vec![e])?,
        _ => {
            tracing::info!("no simplehar.json found; using built-in defaults");
            AppConfig::default()
        }
    };
    let errors = config.validate();
    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    observability::init();
    let config = match boot_config() {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors {
                tracing::error!(error = %error, "invalid configuration");
            }
            return ExitCode::FAILURE;
        }
    };
    // The catalog is empty until the ModelConf scan lands (Step 9); the mock adapter
    // is registered so operator-supplied mock entries work out of the box.
    let catalog = Vec::new();
    let mut adapters = std::collections::BTreeMap::new();
    let mock: Arc<dyn EngineAdapter> = Arc::new(MockAdapter);
    adapters.insert("mock".to_string(), mock);
    let runtime: Arc<dyn EngineRuntime> = Arc::new(UnimplementedRuntime);
    let scheduler = Scheduler::start(config.clone(), catalog.clone(), adapters, runtime);
    let state = Arc::new(ApiState::new(config, scheduler, catalog));

    match run(state).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "harness stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Binds every category listener (the control one hosts `/har/*`); a failed
/// *category* bind is reported and skipped — partial availability beats a dead
/// process (G11). A failed *control* bind is fatal: without it there is no control
/// plane.
async fn run(state: Arc<ApiState>) -> anyhow::Result<()> {
    tracing::info!(
        control = %state.control_addr,
        "simplehar-se booting: listeners before engines, zero engine spawns (§0.2)"
    );
    let host = state.config.internal_host;
    let mut servers = Vec::new();
    for category in state.config.categories.clone() {
        let addr = SocketAddr::new(host, category.public_port);
        let router = if category.control {
            control_router(Arc::clone(&state))
        } else {
            category_router(Arc::clone(&state), category.id)
        };
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                tracing::info!(%addr, category = %category.id, "listener bound");
                servers.push(tokio::spawn(async move {
                    let _ = axum::serve(listener, router).await;
                }));
            }
            Err(error) if category.control => {
                return Err(anyhow::anyhow!(error)).with_context(|| {
                    format!("cannot bind control listener {addr} (G11: name the holder and refuse)")
                });
            }
            Err(error) => {
                tracing::error!(%addr, category = %category.id, error = %error,
                    "listener bind failed; continuing on the listeners we have (G11)");
            }
        }
    }
    shutdown_signal().await;
    tracing::info!("shutdown requested; stopping listeners");
    for server in servers {
        server.abort();
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}

/// Resolves on Ctrl+C; a signal-receive error is logged, not fatal.
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %error, "ctrl_c listener failed; continuing until process exit");
    }
}
