//! Entry point: boot order per DESIGN §0.2 — config first, listeners before any engine
//! work (there is none: preload is scrapped), zero engine spawns at boot.

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use simplehar_se::api::{router, ApiState};
use simplehar_se::config::{AppConfig, ConfigError};
use simplehar_se::observability;

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
    let state = Arc::new(ApiState::from(config));
    tracing::info!(
        control = %state.control_addr,
        categories = 4,
        "simplehar-se booting: control listener first, zero engine spawns (§0.2)"
    );
    match run(state).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "harness stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Binds the control listener and serves until Ctrl+C.
///
/// # Errors
///
/// Propagates bind/serve failures with context (the port and address are named,
/// matching the G11 failure contract).
async fn run(state: Arc<ApiState>) -> anyhow::Result<()> {
    let addr: std::net::SocketAddr = state
        .control_addr
        .parse()
        .context("control address must be a valid socket address")?;
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!("cannot bind control listener {addr} (name the holder and continue — G11)")
    })?;
    tracing::info!(%addr, "control listener bound");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("control listener stopped with an error")
}

/// Resolves on Ctrl+C; a signal-receive error is logged, not fatal.
async fn shutdown_signal() {
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::warn!(error = %error, "ctrl_c listener failed; continuing until process exit");
    }
}
