//! Entry point: boot order per DESIGN §0.2 — config first, listeners bound before any
//! engine work (there is none: preload is scrapped), zero engine spawns at boot.

use simplehar_se::api::{category_router, control_router, ApiState};
use simplehar_se::config::{AppConfig, ConfigError};
use simplehar_se::engine::llama::LlamaAdapter;
use simplehar_se::engine::mock::MockAdapter;
use simplehar_se::engine::{EngineAdapter, EngineRuntime};
use simplehar_se::observability;
use simplehar_se::process::{ProcessRuntime, SingleInstanceGuard};
use simplehar_se::scheduler::{Scheduler, SchedulerHandle};
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
    let strata: Arc<dyn EngineAdapter> =
        Arc::new(simplehar_se::engine::strata::StrataAdapter::default());
    adapters.insert("strata".to_string(), strata);
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
    // A6: inventory is observability, never a boot gate (review finding 3): the
    // per-model stat loop runs off the critical path so a sleeping or removed
    // drive can never delay the listeners, the tray, or `/har/status`.
    {
        let handle = scheduler.clone();
        let exe = config.llama_server_exe.clone();
        let conf_dir = config.modelconf_dir.clone();
        tokio::spawn(async move {
            log_boot_banner(&handle, &exe, &conf_dir).await;
        });
    }
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

/// A6: v3.3-style boot inventory. Logs one line per catalog model (`marker/category/
/// engine/path`) and warns up front if `llama-server.exe` is absent, so the operator
/// sees load-time landmines at boot instead of at the first request. Spawned off the
/// boot path (never gates bind); the stat loop runs on the blocking pool. Uses
/// `tracing`, never console print (AGENTS.md).
async fn log_boot_banner(
    scheduler: &SchedulerHandle,
    llama_server_exe: &std::path::Path,
    modelconf_dir: &std::path::Path,
) {
    let exe_missing = {
        let exe = llama_server_exe.to_path_buf();
        tokio::task::spawn_blocking(move || !exe.exists())
            .await
            .unwrap_or(false)
    };
    if exe_missing {
        tracing::warn!(
            path = %llama_server_exe.display(),
            "llama-server.exe not found; llama-engine loads will fail until llama_server_exe is configured"
        );
    }
    let catalog = match scheduler.catalog().await {
        Ok(catalog) => catalog,
        Err(error) => {
            tracing::warn!(error = %error, "catalog unreadable at boot; skipping model banner");
            return;
        }
    };
    let entries = catalog.models.clone();
    let conf_dir = modelconf_dir.to_path_buf();
    let markers = tokio::task::spawn_blocking(move || {
        entries
            .iter()
            .map(|entry| banner_marker(entry, &conf_dir))
            .collect::<Vec<_>>()
    })
    .await
    .unwrap_or_else(|_| vec![" "; catalog.models.len()]);
    for (entry, marker) in catalog.models.iter().zip(markers) {
        let path = entry.model_ref.as_deref().unwrap_or("-");
        tracing::info!(
            target: "simplehar_se::boot",
            "{}",
            format!("{marker}/{}/{}/{path}", entry.category, entry.engine),
        );
    }
    for error in &catalog.errors {
        tracing::warn!(
            target: "simplehar_se::boot",
            "{}",
            format!("! {}: {}", error.file, error.reason),
        );
    }
}

/// The banner's per-model marker: `!` when a load is certainly doomed —
/// a plain path that does not exist (resolved against `modelconf_dir` when
/// relative) or a `llama` entry with no `model:` at all (a launch-time
/// `MissingField` refusal, engine/llama.rs). `scheme://` refs are engine-backed
/// sentinels (model/mod.rs:57) and other engines may load file-less, so both
/// print the neutral marker instead of a false alarm.
fn banner_marker(
    entry: &simplehar_se::model::ModelEntry,
    modelconf_dir: &std::path::Path,
) -> &'static str {
    match entry.model_ref.as_deref() {
        None => {
            if entry.engine == "llama" {
                "!"
            } else {
                " "
            }
        }
        Some(p) if p.contains("://") => " ",
        Some(p) => {
            let path = std::path::Path::new(p);
            let full = if path.is_relative() {
                modelconf_dir.join(path)
            } else {
                path.to_path_buf()
            };
            if full.exists() {
                " "
            } else {
                "!"
            }
        }
    }
}

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
    let mut hinted = false;
    let mut hint: Option<String> = None;
    loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                listeners.mark_serving(category.id);
                tracing::info!(%addr, category = %category.id, "listener serving");
                hinted = false;
                hint = None;
                if let Err(error) = axum::serve(listener, router.clone()).await {
                    tracing::error!(%addr, category = %category.id, error = %error,
                        "listener stopped; attempting rebind");
                }
            }
            Err(error) => {
                // Publish the outage BEFORE any child process: a hung netsh must
                // never leave the registry stale-Serving with the tray/UI green
                // over a dead port (G11; review finding, 2026-10-26).
                let mut reason = format!("cannot bind {addr}: {error}");
                if let Some(h) = &hint {
                    reason.push_str("; ");
                    reason.push_str(h);
                }
                listeners.mark_down(category.id, reason);
                tracing::error!(%addr, category = %category.id, error = %error,
                    "listener down (port busy?); retrying; harness continues (G11)");
                // A4: the classic Windows "busy with nothing listening" cause is a
                // Hyper-V/WinNAT-reserved port range — probe once per outage (the
                // resolved hint persists for every retry until a successful bind).
                if !hinted {
                    hinted = true;
                    match tokio::time::timeout(
                        LISTENER_RETRY,
                        reserved_range_hint(category.public_port),
                    )
                    .await
                    {
                        Ok(Some(h)) => {
                            hint = Some(h.clone());
                            tracing::warn!(%addr, category = %category.id, "{h}");
                            listeners.mark_down(
                                category.id,
                                format!("cannot bind {addr}: {error}; {h}"),
                            );
                        }
                        Ok(None) => {}
                        Err(_) => tracing::debug!(%addr, "netsh excludedportrange probe timed out"),
                    }
                }
            }
        }
        tokio::time::sleep(LISTENER_RETRY).await;
    }
}

/// `None`, or a hint naming the excluded range that swallows `port` — the
/// Hyper-V/WSL2 dynamic-port-reservation foot-gun (v3.3-era lesson, item 35 A4).
async fn reserved_range_hint(port: u16) -> Option<String> {
    let output = tokio::process::Command::new("netsh")
        .kill_on_drop(true)
        .args([
            "interface",
            "ipv4",
            "show",
            "excludedportrange",
            "protocol=tcp",
        ])
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let ranges = parse_excluded_ranges(&decode_console_output(&output.stdout));
    ranges
        .into_iter()
        .find(|(start, end)| *start <= port && port <= *end)
        .map(|(start, end)| {
            format!(
                "port {port} lies in the reserved range {start}-{end} \
                 (netsh interface ipv4 show excludedportrange protocol=tcp)"
            )
        })
}

/// Decodes `netsh` output: console tools emit UTF-16LE (BOM) on some locales and
/// UTF-8/OEM on others; numeric rows must survive either way.
fn decode_console_output(bytes: &[u8]) -> String {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c.get(1).copied().unwrap_or_default()]))
            .collect();
        String::from_utf16_lossy(&units)
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

/// Extracts the `(start, end)` rows of `show excludedportrange` output. Headers and
/// locale prose are ignored; a row qualifies when its first two columns are integers
/// and any remaining columns are netsh's row markers (`*` administered, `!` invalid).
/// Accepting those markers matters: the `50000-50059` Hyper-V range is administered,
/// so it carries a `*` and would otherwise be missed.
fn parse_excluded_ranges(text: &str) -> Vec<(u16, u16)> {
    text.lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 2 || cols.len() > 3 {
                return None;
            }
            if cols.len() == 3 && !["*", "!"].contains(&cols[2]) {
                return None;
            }
            let (Ok(start), Ok(end)) = (cols[0].parse::<u16>(), cols[1].parse::<u16>()) else {
                return None;
            };
            Some((start.min(end), start.max(end)))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::{decode_console_output, parse_excluded_ranges};

    const SAMPLE: &str = "
Protocol tcp Port Exclusion Ranges

Start Port    End Port
----------    --------
      1024        1123
      8081        8180
     50000       50059

* - Administered port (exclusion added by an administrator), ! - Invalid port.
";

    #[test]
    fn parses_only_two_integer_rows() {
        let ranges = parse_excluded_ranges(SAMPLE);
        assert_eq!(ranges, vec![(1024, 1123), (8081, 8180), (50_000, 50_059)]);
        assert!(
            ranges.iter().any(|(s, e)| *s <= 8100 && 8100 <= *e),
            "the hint fires for a swallowed port"
        );
        assert!(
            !ranges.iter().any(|(s, e)| *s <= 8708 && 8708 <= *e),
            "a free port must not produce a false hint"
        );
    }

    #[test]
    fn normalizes_reversed_range_columns() {
        assert_eq!(parse_excluded_ranges("  9000  8000  "), vec![(8000, 9000)]);
    }

    #[test]
    fn accepts_netsh_administered_and_invalid_markers() {
        // Real netsh emits `50000 50059 *` (administered) and occasionally `!`.
        let ranges = parse_excluded_ranges("50000 50059 *\n  6000 6099 !\n  80 90 x\n");
        assert_eq!(ranges, vec![(50_000, 50_059), (6_000, 6_099)]);
    }

    #[test]
    fn decodes_utf16le_netsh_output() {
        let utf16: Vec<u16> = SAMPLE.encode_utf16().collect();
        let mut bytes = vec![0xFF, 0xFE];
        for unit in utf16 {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_console_output(&bytes), SAMPLE);
        // Plain UTF-8 passes through untouched.
        assert_eq!(decode_console_output(SAMPLE.as_bytes()), SAMPLE);
    }

    #[test]
    fn banner_marker_is_honest_in_both_directions() {
        use super::banner_marker;
        use simplehar_se::config::CategoryId;
        use simplehar_se::model::ModelEntry;
        let entry = |engine: &str, model_ref: Option<&str>| ModelEntry {
            name: "m".to_string(),
            engine: engine.to_string(),
            category: CategoryId::Work,
            vram_gb: None,
            gguf_size_gb: None,
            cost: simplehar_se::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            model_ref: model_ref.map(str::to_string),
            serve_model: None,
            file: None,
            raw: Default::default(),
        };
        let dir = std::env::temp_dir();
        assert_eq!(
            banner_marker(&entry("llama", None), &dir),
            "!",
            "llama without model: is a launch-time refusal, must flag"
        );
        assert_eq!(
            banner_marker(&entry("mock", None), &dir),
            " ",
            "file-less engines are neutral"
        );
        assert_eq!(
            banner_marker(&entry("strata", Some("strata://models/x")), &dir),
            " ",
            "scheme sentinels are never 'missing'"
        );
        assert_eq!(
            banner_marker(&entry("llama", Some("definitely-missing-se.gguf")), &dir),
            "!",
            "relative refs resolve against modelconf_dir"
        );
        let name = format!("se-banner-{}.gguf", std::process::id());
        std::fs::write(dir.join(&name), b"x").expect("seed file");
        assert_eq!(
            banner_marker(&entry("llama", Some(name.as_str())), &dir),
            " ",
            "relative ref present under modelconf_dir is not missing"
        );
        let _ = std::fs::remove_file(dir.join(&name));
    }
}
