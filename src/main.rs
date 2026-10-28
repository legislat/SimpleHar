//! Entry point: boot order per DESIGN §0.2 — config first, listeners bound before any
//! engine work (there is none: preload is scrapped), zero engine spawns at boot.

use simplehar_se::api::{category_router, control_router, ApiState, BootMsStamp};
use simplehar_se::config::{self, AppConfig, ConfigError};
use simplehar_se::engine::EngineRuntime;
use simplehar_se::observability;
use simplehar_se::process::{ContainerCandidate, ProcessRuntime, SingleInstanceGuard};
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
/// defaulted — defect D1). `env_config_path` is the effective value of
/// `SIMPLEHAR_CONFIG` (passed in, not read here, so tests never mutate process env):
/// naming a path that is not an existing file is fatal (FINDINGS-14 [3]), and a
/// legacy-migration copy that fails is fatal too — the operator has a config the
/// boot cannot honour (FINDINGS-14 [2]).
fn boot_config(env_config_path: Option<String>) -> Result<AppConfig, Vec<ConfigError>> {
    // The test harness (and operators) can point at a config anywhere; otherwise
    // the config lives beside the executable.
    let explicit = env_config_path.map(PathBuf::from);
    let path = match &explicit {
        Some(path) => Some(path.clone()),
        None => config::default_config_path(),
    };
    // One-time migration from the shared v3.3 name. The legacy file is copied, never
    // moved or rewritten: the rollback exe must still find what it expects there
    // (deploy review, 2026-10-26 — SE's full-struct persist writes `protected_ports`,
    // which makes v3.3 exit on its own port-namespace check).
    if let Some(path) = path.as_ref() {
        if !path.exists()
            && path
                .file_name()
                .is_some_and(|name| name == config::CONFIG_FILE)
        {
            let legacy = path.with_file_name(config::LEGACY_CONFIG_FILE);
            if legacy.exists() {
                if let Err(error) = std::fs::copy(&legacy, path) {
                    // FINDINGS-14 [2]: a failed copy must not warn-and-continue — the
                    // operator has a config and this boot cannot honour it, so serving
                    // built-in defaults would be a security/admission posture nobody
                    // authored. `exists()` (not `is_file()`) so a legacy path that is
                    // present but uncopiable reaches this same fatal arm.
                    return Err(vec![ConfigError::Invalid {
                        field: config::LEGACY_CONFIG_FILE.to_string(),
                        reason: format!(
                            "could not migrate {} to {}: {error}",
                            legacy.display(),
                            path.display()
                        ),
                    }]);
                }
                tracing::warn!(
                    "migrated {} -> {} (the legacy file is left untouched for rollback)",
                    legacy.display(),
                    path.display()
                );
            }
        }
    }
    if let Some(path) = &explicit {
        // FINDINGS-14 [3]: an explicit env path is an instruction, not an absence.
        if !path.is_file() {
            return Err(vec![ConfigError::Invalid {
                field: "SIMPLEHAR_CONFIG".to_string(),
                reason: format!("no configuration file at {}", path.display()),
            }]);
        }
    }
    let mut config = match &path {
        Some(path) if path.exists() => AppConfig::load(path).map_err(|e| vec![e])?,
        _ => {
            tracing::info!("no {} found; using built-in defaults", config::CONFIG_FILE);
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
    let config = match boot_config(std::env::var("SIMPLEHAR_CONFIG").ok()) {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors {
                tracing::error!(error = %error, "invalid configuration");
            }
            return ExitCode::FAILURE;
        }
    };
    let config_ms = boot_started.elapsed().as_millis() as u64;
    let adapters = simplehar_se::engine::default_adapters(&config);
    // Witness/runtime/scheduler are bind-cheap (FINDINGS-14 [1]): the NVML witness
    // defers Nvml::init to its first sample (or the priming task below) and the
    // scheduler defers the catalog scan to its actor task, so no driver or
    // `modelconf_dir` volume can stall the bind below. Accepted pre-bind cost here:
    // ProcessRuntime::new's one local std::fs::read of ninfer-claims.json beside
    // the exe — microseconds, not a stall.
    let runtime = ProcessRuntime::new().with_wsl(
        config.wsl_distro.clone(),
        config.wsl_account().unwrap_or_default(),
    );
    // Same runtime instance for the boot reclaim: one shared engine table and one
    // shared container-claim ledger (chunk 10 [1]).
    let boot_runtime = runtime.clone();
    let runtime: Arc<dyn EngineRuntime> = Arc::new(runtime);
    let witness = simplehar_se::witness::NvmlWitness::new();
    let witness = Arc::new(witness);
    // FINDINGS-FIXROUND2 [3]: pay the lazy `Nvml::init()` on the blocking pool,
    // where it can hold neither an async worker, the witness `refill` guard, nor a
    // OnceLock caller on the first `/har/status`. Concurrent samples share the
    // init via the OnceLock; the normal case leaves no init for the first poll.
    {
        let priming = Arc::clone(&witness);
        tokio::task::spawn_blocking(move || priming.prime());
    }
    let scheduler = Scheduler::start(
        config.clone(),
        config.modelconf_dir.clone(),
        adapters,
        runtime,
        // Unsized coercion of the shared concrete witness to the trait object.
        Arc::clone(&witness) as Arc<dyn simplehar_se::budget::VramWitness>,
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
    let scheduler_spawn_ms = boot_started.elapsed().as_millis() as u64;
    // A6: inventory is observability, never a boot gate (review finding 3): the
    // per-model stat loop runs off the critical path so a sleeping or removed
    // drive can never delay the listeners, the tray, or `/har/status`.
    {
        let handle = scheduler.clone();
        let exe = config.llama_server_exe.clone();
        let conf_dir = config.modelconf_dir.clone();
        let wsl_account = config.wsl_account().map(str::to_string);
        tokio::spawn(async move {
            log_boot_banner(&handle, &exe, &conf_dir, wsl_account.as_deref()).await;
        });
    }
    let listeners = simplehar_se::listeners::ListenerRegistry::new();
    let boot = simplehar_se::api::BootTimings {
        config_ms,
        scheduler_spawn_ms,
        ..simplehar_se::api::BootTimings::default()
    };
    let state = Arc::new(ApiState::new(
        config.clone(),
        scheduler.clone(),
        listeners.clone(),
        Arc::clone(&witness) as Arc<dyn simplehar_se::budget::VramWitness>,
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

    match run(state, shutdown_rx, boot_started, boot_runtime).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            tracing::error!(error = %error, "harness stopped with an error");
            ExitCode::FAILURE
        }
    }
}

/// Pause between listener rebind attempts while a port is unavailable.
const LISTENER_RETRY: Duration = Duration::from_secs(5);

/// Test-only ordering seam (the shape of `model::scan`'s scan pause): `run()` raises
/// `READY_LOGGED` the instant the `boot ready` line is printed, and the boot-reclaim
/// closure records what that flag said as it begins. FINDINGS-14 [5] is precisely
/// this ordering — the reclaim may only run after the boot-ready gate, never on the
/// bind path — and a test cannot observe it from outside without docker and WSL.
/// Compiled out of the product entirely: no branch, no switch, no runtime cost.
#[cfg(test)]
mod boot_order {
    use std::sync::atomic::{AtomicBool, Ordering};

    /// Set by `run()` immediately after the `boot ready` log line.
    pub(super) static READY_LOGGED: AtomicBool = AtomicBool::new(false);
    /// Whether the reclaim closure has begun at all (so a test can tell "not yet"
    /// from "began before ready").
    pub(super) static RECLAIM_DISPATCHED: AtomicBool = AtomicBool::new(false);
    /// What `READY_LOGGED` said the moment the reclaim closure began.
    pub(super) static RECLAIM_SAW_READY: AtomicBool = AtomicBool::new(false);

    /// Clears the three flags; only the single `run()`-driving test calls it, and it
    /// holds no other boot-order test in parallel with itself.
    pub(super) fn reset() {
        for flag in [&READY_LOGGED, &RECLAIM_DISPATCHED, &RECLAIM_SAW_READY] {
            flag.store(false, Ordering::SeqCst);
        }
    }

    pub(super) fn log_ready() {
        READY_LOGGED.store(true, Ordering::SeqCst);
    }

    pub(super) fn reclaim_begins() {
        RECLAIM_SAW_READY.store(READY_LOGGED.load(Ordering::SeqCst), Ordering::SeqCst);
        RECLAIM_DISPATCHED.store(true, Ordering::SeqCst);
    }
}

/// A6: v3.3-style boot inventory. Logs one line per catalog model (`marker/category/
/// engine/path`) and warns up front if `llama-server.exe` is absent or if the catalog
/// holds ninfer models while no WSL account is configured, so the operator sees
/// load-time landmines at boot instead of at the first request. Spawned off the
/// boot path (never gates bind); the stat loop runs on the blocking pool. Uses
/// `tracing`, never console print (AGENTS.md).
///
/// `wsl_account` is [`AppConfig::wsl_account`] resolved at boot: `None` means no
/// ninfer load can be addressed in WSL.
async fn log_boot_banner(
    scheduler: &SchedulerHandle,
    llama_server_exe: &std::path::Path,
    modelconf_dir: &std::path::Path,
    wsl_account: Option<&str>,
) {
    let exe_missing = {
        let exe = llama_server_exe.to_path_buf();
        tokio::task::spawn_blocking(move || !exe.exists())
            .await
            // FINDINGS-14 [8]: fail toward warning — a check that could not run must
            // not report the reassuring "exe is present" verdict.
            .unwrap_or(true)
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
    // Same landmine pattern as the missing-exe warn above: an ninfer catalog with no
    // WSL account can never load, and that must be visible at boot rather than only
    // when the operator clicks Load (review, 2026-10-26).
    if wsl_account.is_none() && entries.iter().any(|entry| entry.engine == "ninfer") {
        tracing::warn!(
            "the catalog holds ninfer models but `wsl_user` is not set: those loads will \
             refuse until the config names the WSL account that owns them"
        );
    }
    let conf_dir = modelconf_dir.to_path_buf();
    let markers = tokio::task::spawn_blocking(move || {
        entries
            .iter()
            .map(|entry| banner_marker(entry, &conf_dir))
            .collect::<Vec<_>>()
    })
    .await
    // FINDINGS-14 [8]: the `!` filler, not the neutral marker: a banner whose stat
    // loop could not run must look like a machine full of landmines, not a clean one.
    .unwrap_or_else(|_| vec!["!"; catalog.models.len()]);
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
    boot_started: std::time::Instant,
    first_bind: BootMsStamp,
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
                // §0.2 `first_bind_at`, stamped BEFORE the verdict is published: the
                // first supervisor to bind wins the cell, and the ready gate below
                // can never observe "every category has a verdict" without this
                // measure already standing. `boot_started` is the same epoch the
                // other four measures use, so `first_bind_at <= bind_ms` holds.
                first_bind.stamp(boot_started.elapsed().as_millis() as u64);
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
                // FINDINGS-14 [6]: a failed probe does NOT set `hinted`, so the next
                // rebind attempt probes again instead of losing the hint for the
                // outage; the retry cadence stays the supervisor's own backoff below.
                if !hinted {
                    let attempt = tokio::time::timeout(
                        LISTENER_RETRY,
                        reserved_range_hint(category.public_port),
                    )
                    .await
                    .map_err(|_| ());
                    let fresh_hint = match &attempt {
                        Ok(Some(h)) => Some(h.clone()),
                        _ => None,
                    };
                    let (now_hinted, resolved) = probe_outcome(hint.clone(), attempt);
                    hinted = now_hinted;
                    hint = resolved;
                    if let Some(h) = fresh_hint {
                        tracing::warn!(%addr, category = %category.id, "{h}");
                        listeners
                            .mark_down(category.id, format!("cannot bind {addr}: {error}; {h}"));
                    } else if !hinted {
                        tracing::debug!(
                            %addr,
                            "netsh excludedportrange probe failed or timed out; retrying on the next attempt"
                        );
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

/// FINDINGS-14 [6]: the hint decision, isolated so it is unit-testable.
///
/// Returns `(hinted, hint)`: `hinted` becomes true only when the probe obtained an
/// answer — `Ok(Some)` or `Ok(None)`; an `Err` (the 5 s timeout or a failed spawn)
/// leaves it false so the next rebind attempt probes again instead of losing the
/// hint for the whole outage. A hint already held survives a failed probe. `()`
/// stands in for the timeout error, whose payload the caller only logs.
fn probe_outcome(
    current_hint: Option<String>,
    attempt: Result<Option<String>, ()>,
) -> (bool, Option<String>) {
    match attempt {
        Ok(answer) => (true, answer.or(current_hint)),
        Err(()) => (false, current_hint),
    }
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
///
/// `boot_runtime` is the same [`ProcessRuntime`] instance the scheduler drives:
/// the container reclaim consults the very ledger that runtime's spawn/release
/// paths maintain, so ownership evidence cannot disagree between the two.
async fn run(
    state: Arc<ApiState>,
    mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    boot_started: std::time::Instant,
    boot_runtime: ProcessRuntime,
) -> anyhow::Result<()> {
    tracing::info!(
        control = %state.control_addr,
        "simplehar-se booting: listeners before engines, zero engine spawns (§0.2)"
    );
    let host = state.config.internal_host;
    if !host.is_loopback() {
        // §0.4, adjusted by the operator note: publishing to the local net is a
        // deliberate trust decision; the guard is the floor, the optional
        // `control_token` is the door (deploy review, 2026-10-26).
        if state.requires_token() {
            tracing::warn!(
                %host,
                "publishing to the local network: API and proxy routes require the control token (static page exempt; SSE is disabled while a token is in use)"
            );
        } else {
            tracing::warn!(
                %host,
                "publishing to the local network with NO authentication: every device on the segment can load models and drive the GPUs — set `control_token` or `SIMPLEHAR_TOKEN`"
            );
        }
    }
    let listeners = state.listeners.clone();
    let first_bind = state.first_bind_stamp();
    let mut supervisors = Vec::new();
    for category in state.config.categories.clone() {
        supervisors.push(tokio::spawn(supervise_listener(
            Arc::clone(&state),
            category,
            listeners.clone(),
            boot_started,
            first_bind.clone(),
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
            "boot ready (0.2): config validated, catalog scan queued, listeners resolved"
        );
        // FINDINGS-14 [5] seam: the reclaim below is only legal AFTER this line.
        #[cfg(test)]
        boot_order::log_ready();
    }
    // Boot reclaim (deploy review, 2026-10-26; scoped per chunk 10 [1], 2026-10-27):
    // a container left by a crashed run keeps the card occupied, and observed-VRAM
    // admission then refuses every load with no pointer to the culprit. Runs after
    // `boot ready` so it can never delay binding (§0.2). The reclaim may touch ONLY
    // catalog-declared container names this harness positively owns — ninfer engine,
    // spawn-path-legal name, live claim in the runtime's ledger — never by prefix,
    // never by container-ID prefix, never a container it did not start; every other
    // running container is warned about and left alone.
    {
        let handle = state.scheduler.clone();
        let reap = state.reap_stamp();
        tokio::spawn(async move {
            // FINDINGS-14 [5] seam: records what `READY_LOGGED` said when this
            // closure began, which is the post-ready ordering the boot contract
            // promises and nothing else can observe without docker or WSL.
            #[cfg(test)]
            boot_order::reclaim_begins();
            // FINDINGS-14 [4]: §0.2 step 4 says the reclaim is "off the bind path",
            // which is only auditable once it can say what it cost. Dispatch to
            // completion, on the wall clock, in the same ms-since-start unit as the
            // other measures — it used to end in silence.
            let dispatched = std::time::Instant::now();
            let Ok(catalog) = handle.catalog().await else {
                return;
            };
            let candidates: Vec<ContainerCandidate> = catalog
                .models
                .iter()
                .filter_map(|entry| {
                    entry
                        .raw
                        .get("ninfer_container")
                        .map(|name| name.trim().to_string())
                        .filter(|name| !name.is_empty())
                        .map(|name| ContainerCandidate {
                            engine: entry.engine.clone(),
                            name,
                        })
                })
                .collect();
            boot_runtime.reclaim_at_boot(&candidates).await;
            let reap_ms = dispatched.elapsed().as_millis() as u64;
            reap.stamp(reap_ms);
            tracing::info!(
                reap_ms,
                candidates = candidates.len(),
                "boot container reclaim finished (§0.2 step 4)"
            );
        });
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
    //! Boot composition lives in `engine::default_adapters` (including the
    //! `allow_mock_engine` gate), where it is pinned by a test. `main` itself is not
    //! unit-reachable — it takes the single-instance guard, binds every listener and
    //! starts the NVML witness — but `run()` and `supervise_listener` are, so the
    //! §0.2 ordering and the boot measures are pinned against the real code paths
    //! (FINDINGS-14 [4]/[5]) with every listener on an ephemeral port.

    use super::{
        boot_config, boot_order, decode_console_output, log_boot_banner, parse_excluded_ranges,
        probe_outcome, run, supervise_listener,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use simplehar_se::api::{control_router, ApiState};
    use simplehar_se::config::{self, AppConfig, CategoryConfig, ConfigError};
    use simplehar_se::engine::EngineRuntime;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::time::Duration;
    use tower::ServiceExt;

    fn error_text(errors: &[ConfigError]) -> String {
        errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn failed_legacy_migration_copy_is_fatal() {
        // FINDINGS-14 [2]: a present-but-uncopiable legacy config must fail the boot,
        // never warn and fall through to built-in defaults. The legacy path is a
        // directory: `exists()` passes but `std::fs::copy` fails deterministically.
        let dir = simplehar_se::testutil::temp_dir("legacy-copy-fail");
        let target = dir.join(config::CONFIG_FILE);
        let legacy = dir.join(config::LEGACY_CONFIG_FILE);
        std::fs::create_dir(&legacy).expect("legacy dir");
        let errors = boot_config(Some(target.display().to_string()))
            .expect_err("an unmigratable legacy config must be fatal, not defaults");
        let text = error_text(&errors);
        assert!(
            text.contains(&legacy.display().to_string()),
            "the error must name the legacy path: {text}"
        );
        assert!(
            text.contains(&target.display().to_string()),
            "the error must name the migration target: {text}"
        );
    }

    #[test]
    fn missing_file_named_by_simplehar_config_is_fatal() {
        // FINDINGS-14 [3]: the env var is an instruction, not an absence.
        let dir = simplehar_se::testutil::temp_dir("env-missing");
        let requested = dir.join("operator-typo.json");
        let errors = boot_config(Some(requested.display().to_string()))
            .expect_err("a missing file named by SIMPLEHAR_CONFIG must be fatal");
        let text = error_text(&errors);
        assert!(
            text.contains(&requested.display().to_string()),
            "the error must name the requested path: {text}"
        );
        assert!(
            !text.contains("simplehar-se.json"),
            "the honest report names the requested path, not the default file: {text}"
        );
    }

    #[test]
    fn missing_default_config_still_defaults() {
        // Regression pin for [2]/[3]: unset env + no file beside the exe keeps the
        // §0.1 defaults path untouched. Guarded: a stray config beside the test
        // exe is not the case under test.
        let Some(default) = config::default_config_path() else {
            return;
        };
        if default.exists() || default.with_file_name(config::LEGACY_CONFIG_FILE).exists() {
            return;
        }
        let loaded = boot_config(None).expect("unset env with no file must boot defaults");
        let expected = AppConfig {
            source_path: Some(default),
            ..AppConfig::default()
        };
        assert_eq!(loaded, expected);
    }

    #[test]
    fn simplehar_config_pointing_at_a_present_file_loads_it() {
        let dir = simplehar_se::testutil::temp_dir("env-present");
        let path = dir.join("custom-name.json");
        std::fs::write(&path, r#"{"vram_total_gb": 42.0}"#).expect("config file");
        let loaded =
            boot_config(Some(path.display().to_string())).expect("a present file must load");
        assert_eq!(loaded.vram_total_gb, 42.0, "the file's value must survive");
        assert_eq!(loaded.source_path, Some(path));
    }

    #[test]
    fn probe_marks_hinted_only_on_an_answer() {
        // FINDINGS-14 [6]: the four outcomes of one probe attempt.
        assert_eq!(
            probe_outcome(None, Ok(Some("range 8000-8100".to_string()))),
            (true, Some("range 8000-8100".to_string())),
            "an answer with a hint marks the outage probed"
        );
        assert_eq!(
            probe_outcome(None, Ok(None)),
            (true, None),
            "an answer of 'nothing reserved' also marks it probed"
        );
        assert_eq!(
            probe_outcome(None, Err(())),
            (false, None),
            "a timeout/failure must NOT mark it probed: the next attempt retries"
        );
        assert_eq!(
            probe_outcome(Some("held".to_string()), Err(())),
            (false, Some("held".to_string())),
            "a hint already held survives a failed probe"
        );
        assert_eq!(
            probe_outcome(Some("stale".to_string()), Ok(Some("fresh".to_string()))),
            (true, Some("fresh".to_string())),
            "a fresh answer replaces the held hint"
        );
    }

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
            measured_min_gb: None,
            measured_offload: None,
            model_ref: model_ref.map(str::to_string),
            serve_model: None,
            file: None,
            raw: Default::default(),
        };
        let dir = simplehar_se::testutil::temp_dir("banner");
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
        let name = "se-banner.gguf".to_string();
        std::fs::write(dir.join(&name), b"x").expect("seed file");
        assert_eq!(
            banner_marker(&entry("llama", Some(name.as_str())), &dir),
            " ",
            "relative ref present under modelconf_dir is not missing"
        );
        let _ = std::fs::remove_file(dir.join(&name));
    }

    /// Constructor-only stand-in for the runtime half of the boot wiring, the shape
    /// of the scheduler's own `NeverRuntime` (4389a0a): the boot side never spawns,
    /// kills, probes or discovers, so a call reaching here IS the defect.
    struct NeverRuntime;

    #[async_trait::async_trait]
    impl EngineRuntime for NeverRuntime {
        async fn spawn(
            &self,
            _spec: &simplehar_se::engine::LaunchSpec,
            _ticket: simplehar_se::ownership::SpawnTicket,
        ) -> Result<simplehar_se::engine::Spawned, simplehar_se::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot spawned an engine before the listeners bound");
        }
        async fn kill(
            &self,
            _ticket: &simplehar_se::ownership::SpawnTicket,
        ) -> Result<(), simplehar_se::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot killed an engine before the listeners bound");
        }
        async fn release_confirmed(
            &self,
            _ticket: &simplehar_se::ownership::SpawnTicket,
        ) -> Result<bool, simplehar_se::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot released an engine slot before the listeners bound");
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &simplehar_se::engine::ReadinessSpec,
        ) -> simplehar_se::engine::Probe {
            panic!("FINDINGS-14 [5]: boot probed an engine port before the listeners bound");
        }
        async fn discover_model_id(&self, _port: u16) -> Option<String> {
            panic!("FINDINGS-14 [5]: boot discovered an engine model before the listeners bound");
        }
    }

    /// A live scheduler and its `ApiState`, over an EMPTY catalog dir and with the
    /// real boot adapter map: no model can be scanned, so nothing on this path can
    /// reach an engine or a container even by accident.
    fn boot_wiring(
        config: &AppConfig,
        runtime: Arc<dyn EngineRuntime>,
    ) -> (simplehar_se::scheduler::SchedulerHandle, Arc<ApiState>) {
        let scheduler = simplehar_se::scheduler::Scheduler::start(
            config.clone(),
            config.modelconf_dir.clone(),
            simplehar_se::engine::default_adapters(config),
            runtime,
            Arc::new(simplehar_se::budget::LedgerOnlyWitness)
                as Arc<dyn simplehar_se::budget::VramWitness>,
        );
        let state = Arc::new(ApiState::new(
            config.clone(),
            scheduler.clone(),
            simplehar_se::listeners::ListenerRegistry::new(),
            Arc::new(simplehar_se::budget::LedgerOnlyWitness),
            simplehar_se::api::BootTimings::default(),
        ));
        (scheduler, state)
    }

    /// A config whose catalog is empty and whose four listeners ask the OS for
    /// ephemeral ports: the whole boot path, without taking 8708-8711 away from a
    /// harness that may be serving, and without ever naming a container.
    fn ephemeral_boot_config(tag: &str) -> AppConfig {
        AppConfig {
            modelconf_dir: simplehar_se::testutil::temp_dir(tag),
            ..AppConfig::default()
        }
    }

    async fn status_json(state: &Arc<ApiState>) -> serde_json::Value {
        let response = control_router(Arc::clone(state))
            .oneshot(
                Request::get("/har/status")
                    .body(Body::empty())
                    .expect("static request"),
            )
            .await
            .expect("in-process request");
        assert_eq!(response.status(), StatusCode::OK, "/har/status must answer");
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("bounded body");
        serde_json::from_slice(&bytes).expect("status is JSON")
    }

    /// FINDINGS-14 [5], zero-engine-spawns pin. Everything `lib` owns on the boot
    /// side — the adapter map, `Scheduler::start`, `ApiState::new`, the A6 banner
    /// inventory, the listener registry and the `/har/status` read the page and tray
    /// live on — runs against a runtime that panics on contact. `main` is not
    /// unit-reachable, so this is the closest honest cover of the claim the boot log
    /// prints ("listeners before engines, zero engine spawns"): whoever moves an
    /// engine touch (spawn, kill, probe, discover) into any of these steps fails
    /// here instead of shipping a boot that can stall or start an engine before the
    /// harness answers. The banner is awaited rather than spawned, so its catalog
    /// read is inside the pin instead of racing it.
    #[tokio::test]
    async fn boot_side_sequence_never_touches_the_engine_runtime() {
        let config = ephemeral_boot_config("boot-no-engine");
        let (scheduler, state) = boot_wiring(&config, Arc::new(NeverRuntime));
        log_boot_banner(
            &scheduler,
            &config.llama_server_exe,
            &config.modelconf_dir,
            None,
        )
        .await;
        for category in config.categories.clone() {
            state.listeners.mark_serving(category.id);
        }
        let doc = status_json(&state).await;
        assert_eq!(
            doc["listeners"].as_object().map(|m| m.len()),
            Some(4),
            "the four configured categories resolved: {doc}"
        );
        assert!(
            doc["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().all(|slot| slot["model"].is_null())),
            "§0.2 step 7: boot leaves every slot Empty, no engine anywhere: {doc}"
        );
        assert!(
            doc["boot"]["config_ms"].is_u64() && doc["boot"]["scheduler_spawn_ms"].is_u64(),
            "the boot measures the page reads are still there: {doc}"
        );
    }

    /// FINDINGS-14 [4]: the real supervisor really binds (port 0, so the OS hands out
    /// an ephemeral port and the test cannot collide with a serving harness), and the
    /// first bind is stamped on the same epoch before the verdict is published — so
    /// the ready gate recording `bind_ms` implies `first_bind_at` already stands, and
    /// the ordering between the two measures holds.
    #[tokio::test]
    async fn the_first_binding_supervisor_stamps_first_bind_at_within_bind_ms() {
        let config = ephemeral_boot_config("boot-first-bind");
        let (_scheduler, state) = boot_wiring(&config, Arc::new(NeverRuntime));
        let boot_started = std::time::Instant::now();
        let first_bind = state.first_bind_stamp();
        let supervisor = tokio::spawn(supervise_listener(
            Arc::clone(&state),
            CategoryConfig {
                public_port: 0,
                ..config.control_category().clone()
            },
            state.listeners.clone(),
            boot_started,
            first_bind.clone(),
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while state.listeners.snapshot().is_empty() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the supervisor never published a bind verdict"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        supervisor.abort();
        let bind_ms = boot_started.elapsed().as_millis() as u64;
        state
            .boot
            .write()
            .expect("boot timings lock")
            .set_bound(bind_ms);
        let first_bind_at = first_bind
            .get()
            .expect("a listener that bound must have stamped first_bind_at");
        assert!(
            first_bind_at <= bind_ms,
            "bind completion implies the first bind happened: {first_bind_at} <= {bind_ms}"
        );
        let doc = status_json(&state).await;
        assert_eq!(
            doc["boot"]["first_bind_at"].as_u64(),
            Some(first_bind_at),
            "and /har/status.boot reports the same value the supervisor stamped"
        );
    }

    /// FINDINGS-14 [5], reclaim ordering pin, chosen over a `probe_outcome`-style
    /// predicate because the decision under test is an ordering across two `tokio`
    /// spawns inside `run()`, not a pure function of any value: extracted as a
    /// predicate it would only restate `assert ready`, which can never fail. This
    /// drives the REAL `run()` (four ephemeral listeners, empty catalog, then the
    /// tray/Ctrl+C channel closing) and reads the test-only seam: the reclaim closure
    /// began, and it began with `boot ready` already logged. It also pins the
    /// `reap_ms` FIELD mechanism on a real boot. The reclaim's VALUE needs
    /// docker/WSL — with no catalog-declared container `reclaim_at_boot` stands down
    /// immediately — so this proves the measure is filled and named, not that a
    /// reaped container would be timed (that half belongs to a card window).
    #[tokio::test]
    async fn boot_reclaim_is_dispatched_only_after_boot_ready() {
        boot_order::reset();
        let mut config = ephemeral_boot_config("boot-reclaim-order");
        for category in &mut config.categories {
            category.public_port = 0;
        }
        let (_scheduler, state) = boot_wiring(&config, Arc::new(NeverRuntime));
        let boot_runtime = simplehar_se::process::ProcessRuntime::new();
        let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
        let running = tokio::spawn(run(
            Arc::clone(&state),
            shutdown_rx,
            std::time::Instant::now(),
            boot_runtime,
        ));
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        while !boot_order::RECLAIM_DISPATCHED.load(Ordering::SeqCst)
            || state.reap_stamp().get().is_none()
        {
            assert!(
                tokio::time::Instant::now() < deadline,
                "run() never dispatched and completed the boot reclaim \
                 (dispatched={}, reap_ms={:?})",
                boot_order::RECLAIM_DISPATCHED.load(Ordering::SeqCst),
                state.reap_stamp().get(),
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            boot_order::RECLAIM_SAW_READY.load(Ordering::SeqCst),
            "CONFIRMED-14[5]: the boot reclaim began before `boot ready` was logged — \
             §0.2 step 4 is off the bind path only if that line comes first"
        );
        assert!(
            !state.listeners.any_down(),
            "every ephemeral listener should have bound: {:?}",
            state.listeners.snapshot()
        );
        let first_bind_at = state
            .first_bind_stamp()
            .get()
            .expect("four listeners bound, so the measure stands");
        let bind_ms = state
            .boot
            .read()
            .expect("boot timings lock")
            .bind_ms
            .expect("the ready gate records bind_ms");
        assert!(
            first_bind_at <= bind_ms,
            "the same boot epoch orders the two measures: {first_bind_at} <= {bind_ms}"
        );
        let doc = status_json(&state).await;
        assert_eq!(
            doc["boot"]["reap_ms"].as_u64(),
            state.reap_stamp().get(),
            "/har/status.boot must carry the measure the reclaim stamped"
        );
        drop(shutdown);
        running.await.expect("run task").expect("run returns Ok");
    }
}
