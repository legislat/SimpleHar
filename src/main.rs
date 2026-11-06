//! Entry point: boot order per DESIGN §0.2 — single-instance guard, then config
//! (the guard is acquired before any file is touched: fail fast), listeners bound
//! before any engine work (there is none: preload is scrapped), zero engine spawns
//! at boot.

use simpleharn::api::{category_router, control_router, ApiState, BootMsStamp};
use simpleharn::budget::VramWitness;
use simpleharn::config::{self, firstrun, AppConfig, ConfigError};
use simpleharn::engine::EngineRuntime;
use simpleharn::observability;
use simpleharn::process::{ContainerCandidate, ProcessRuntime, SingleInstanceGuard};
use simpleharn::scheduler::{Scheduler, SchedulerHandle};
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
/// `SIMPLEHARN_CONFIG` (passed in, not read here, so tests never mutate process
/// env): naming a path that is not an existing file is fatal (FINDINGS-14 [3]).
/// With no env name the harness reads exactly one file — the beside-the-exe
/// `simpleharn.json`. There is no discovery of any earlier config name and no
/// migration copy (TASK-NAMING-CLEANUP, operator ruling 2026-11-05): a file
/// carrying an older spelling is ignored, and only an explicit env path — an
/// operator instruction — selects a file by location alone.
fn boot_config(env_config_path: Option<String>) -> Result<AppConfig, Vec<ConfigError>> {
    // The test harness (and operators) can point at a config anywhere; otherwise
    // the config lives beside the executable. An explicit path is an instruction:
    // it never falls back, a missing one stays fatal (14 [3]).
    let explicit = env_config_path.map(PathBuf::from);
    let path = match &explicit {
        Some(path) => Some(path.clone()),
        None => config::default_config_path(),
    };
    if let Some(path) = &explicit {
        // FINDINGS-14 [3]: an explicit env path is an instruction, not an absence.
        if !path.is_file() {
            return Err(vec![ConfigError::Invalid {
                field: "SIMPLEHARN_CONFIG".to_string(),
                reason: format!("no configuration file at {}", path.display()),
            }]);
        }
    }
    let mut config = match &path {
        Some(path) if path.exists() => {
            // R2-14-01: the fatal line must name the file this boot actually read.
            // `AppConfig::parse` names the schema root (`CONFIG_FILE`) because a bare
            // parse call knows no file; here the path is known, so re-stamp the field
            // with it — an operator typo in a `SIMPLEHARN_CONFIG` file must not send a
            // hunt to the healthy default beside the exe. `parse` itself stays as-is.
            AppConfig::load(path).map_err(|error| {
                let reason = match error {
                    ConfigError::Invalid { reason, .. } => reason,
                };
                vec![ConfigError::Invalid {
                    field: path.display().to_string(),
                    reason,
                }]
            })?
        }
        _ => {
            tracing::info!("no {} found; using built-in defaults", config::CONFIG_FILE);
            AppConfig::default()
        }
    };
    // Stamp where the config came from (or where it will live) so `default_model`
    // write-back targets the same file boot read — a `SIMPLEHARN_CONFIG` run must not
    // persist defaults into a file it never loads (operator finding, 2026-10-26).
    // REV6-04: remember the file that was actually read — a semantic refusal must
    // name it in the fatal output too, the way the parse arm already does.
    let loaded_file = path.as_ref().filter(|p| p.exists()).cloned();
    config.source_path = path;
    let errors = config.validate();
    if errors.is_empty() {
        Ok(config)
    } else {
        // REV6-04: field carries the file this boot read, the dotted key moves into
        // the reason — same re-stamp shape as the R2-14-01 parse arm. Built-in
        // defaults (no file read) keep the bare keys.
        let errors = match &loaded_file {
            Some(path) => errors
                .into_iter()
                .map(|error| {
                    let ConfigError::Invalid { field, reason } = error;
                    ConfigError::Invalid {
                        field: path.display().to_string(),
                        reason: format!("{field}: {reason}"),
                    }
                })
                .collect(),
            None => errors,
        };
        Err(errors)
    }
}

/// The command-line verbs the binary answers instead of booting. Today one:
/// `import strata <profile.json>` (ruling 2026-11-02 row 19c) — the yaml lands
/// on stdout (pipeable bytes, no log decoration), refusals on the tracing
/// stream. `--apply <row.yaml>` (ruling 2026-11-03) merges the block into the
/// live row instead of printing it; the merge refuses loudly and writes
/// nothing. `None` means "not a command line, boot the harness".
fn run_cli(mut words: impl Iterator<Item = String>) -> Option<ExitCode> {
    let first = words.next()?;
    if first != "import" {
        return None;
    }
    let verb = words.next().unwrap_or_default();
    let path = words.next().unwrap_or_default();
    let usage = || {
        tracing::error!(
            "usage: simpleharn import strata <profile.json> [--apply <row.yaml>] \
             (prints the row yaml and writes nothing, or merges the block into the row)"
        );
    };
    if verb != "strata" || path.trim().is_empty() {
        usage();
        return Some(ExitCode::FAILURE);
    }
    let row = match words.next() {
        None => None,
        Some(flag) if flag == "--apply" => {
            let row = words.next().unwrap_or_default();
            if row.trim().is_empty() || words.next().is_some() {
                usage();
                return Some(ExitCode::FAILURE);
            }
            Some(row)
        }
        Some(_) => {
            usage();
            return Some(ExitCode::FAILURE);
        }
    };
    let outcome = match row {
        None => simpleharn::import::strata_yaml(std::path::Path::new(path.trim()))
            .map(|yaml| print!("{yaml}")),
        Some(row) => {
            let outcome = simpleharn::import::apply_strata_row(
                std::path::Path::new(path.trim()),
                std::path::Path::new(row.trim()),
            );
            outcome.map(|report| println!("{report}"))
        }
    };
    match outcome {
        Ok(()) => Some(ExitCode::SUCCESS),
        Err(reason) => {
            tracing::error!(reason = %reason, "import refused");
            Some(ExitCode::FAILURE)
        }
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let boot_started = std::time::Instant::now();
    observability::init();
    // `simpleharn import strata <profile.json>` (operator ruling 2026-11-02
    // row 19c): the one-shot migration tool — read a manual strata profile,
    // print the row yaml, delete nothing. It is a command, not a serve: no
    // guard, no config, no listener, and the only sanctioned reader of a
    // `C:\Strata` profile.
    if let Some(code) = run_cli(std::env::args().skip(1)) {
        return code;
    }
    // The boot identity, emitted before anything can fail: a harness that dies at the
    // single-instance guard or on an invalid config still leaves an artifact that says
    // which build and which process died. Non-secret facts only — the token is
    // reported as presence alone, never its value (the 15[9] posture).
    tracing::info!(
        target: "simpleharn::boot",
        version = env!("CARGO_PKG_VERSION"),
        pid = std::process::id(),
        exe = %std::env::current_exe()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|_| "unknown".to_string()),
        "harness booting"
    );
    // A10 single-instance guard (§0.2 step 3): a second harness refuses to boot.
    let _guard = match SingleInstanceGuard::acquire() {
        Ok(guard) => guard,
        Err(reason) => {
            tracing::error!(reason = %reason, "refusing to start");
            return ExitCode::FAILURE;
        }
    };
    let mut config = match boot_config(config::config_path_from_env()) {
        Ok(config) => config,
        Err(errors) => {
            for error in &errors {
                tracing::error!(error = %error, "invalid configuration");
            }
            return ExitCode::FAILURE;
        }
    };
    let config_ms = boot_started.elapsed().as_millis() as u64;
    // The configuration this boot is acting on, so a refusal in the file can be read
    // against the numbers that produced it without opening the JSON — the coordinates
    // I had to reverse-engineer from a silent release package today.
    tracing::info!(
        target: "simpleharn::boot",
        config = %config
            .source_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(none)".to_string()),
        internal_host = %config.internal_host,
        vram_total_gb = config.vram_total_gb,
        vram_reserve_gb = config.vram_reserve_gb,
        wsl_distro = %config.wsl_distro,
        wsl_user = %config.wsl_user.as_deref().unwrap_or("(unset)"),
        token_configured = config
            .control_token
            .as_deref()
            .is_some_and(|token| !token.trim().is_empty()),
        token_from_env = config::token_from_env()
            .is_some_and(|token| !token.trim().is_empty()),
        "boot configuration"
    );
    let witness = simpleharn::witness::NvmlWitness::new();
    let witness = Arc::new(witness);
    // First-start materialization (operator ruling 2026-10-29): with no config file,
    // derive values from the real system and create the file beside the exe — the
    // documented defaults (notably `vram_total_gb` = 72.0, THIS box's card) must not
    // silently ship as everyone's ceiling. Fires only for the beside-the-exe path;
    // a missing SIMPLEHARN_CONFIG file already failed boot as an instruction (14 [3]).
    let first_run = first_run_target(&config);
    let mut observed_total: Option<f64> = None;
    if let Some(path) = first_run {
        // Rare one-time boot; probe_system caps each leg itself and never touches
        // the driver on this thread (the sample runs on the blocking pool behind
        // FIRST_RUN_PROBE_BUDGET). Steady boots pay nothing inline: witness init
        // stays on the blocking pool (prime below) and the cross-check runs
        // detached (FINDINGS-14 [1] holds).
        let results = probe_system(&witness).await;
        observed_total = results.vram_total_gb;
        for (field, value) in firstrun::fill(&mut config, &results) {
            tracing::info!(field = %field, value = %value, "first start: detected from the system");
        }
        if results.vram_total_gb.is_none() {
            tracing::warn!(
                default = config.vram_total_gb,
                "first start: card probe failed; vram_total_gb stays at the documented default — \
                 edit the created file if this machine's card differs"
            );
        }
        match firstrun::ensure_created(&config, &path) {
            Ok(true) => tracing::info!(
                path = %path.display(),
                "first start: config created beside the exe — the file is now the truth"
            ),
            Ok(false) => tracing::info!(
                path = %path.display(),
                "first start: a config file appeared during boot; the operator's file is kept"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                path = %path.display(),
                "first start: config creation failed or could not be verified; this boot \
                 continues on in-memory defaults — check the path before the next boot"
            ),
        }
    }
    // Cross-check every boot: a pinned ceiling that disagrees with the card's own
    // reading beyond the tolerance is reported, never auto-repaired (config is king).
    // Detached: no boot path waits on this sample.
    {
        let witness = Arc::clone(&witness);
        let configured = config.vram_total_gb;
        let tolerance = config.witness_tolerance_gb;
        let probed = observed_total;
        tokio::spawn(async move {
            let card = match probed {
                Some(total) => Some(total),
                None => match tokio::time::timeout(BOOT_CARD_SAMPLE_BUDGET, witness.sample()).await
                {
                    Ok(Ok(sample)) => Some(firstrun::floor_gib(sample.total_gib)),
                    Ok(Err(reason)) => {
                        tracing::debug!(reason = %reason, "cross-check skipped: the card did not answer");
                        None
                    }
                    Err(_elapsed) => {
                        tracing::debug!(
                            "cross-check skipped: card sample budget ({} s) expired",
                            BOOT_CARD_SAMPLE_BUDGET.as_secs()
                        );
                        None
                    }
                },
            };
            if let Some((configured, observed)) =
                card.and_then(|card| firstrun::total_mismatch(configured, card, tolerance))
            {
                tracing::warn!(
                    configured,
                    observed,
                    tolerance,
                    "config vram_total_gb disagrees with the card's own reading (never auto-repaired): \
                     edit simpleharn.json if the card is right"
                );
            }
        });
    }
    let adapters = simpleharn::engine::default_adapters(&config);
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
        Arc::clone(&witness) as Arc<dyn simpleharn::budget::VramWitness>,
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
            log_boot_banner(&handle, exe.as_deref(), &conf_dir, wsl_account.as_deref()).await;
        });
    }
    let listeners = simpleharn::listeners::ListenerRegistry::new();
    let boot = simpleharn::api::BootTimings {
        config_ms,
        scheduler_spawn_ms,
        ..simpleharn::api::BootTimings::default()
    };
    let state = Arc::new(ApiState::new(
        config.clone(),
        scheduler.clone(),
        listeners.clone(),
        Arc::clone(&witness) as Arc<dyn simpleharn::budget::VramWitness>,
        boot,
    ));

    // One shutdown channel for every exit path: Ctrl+C and the tray's Exit item
    // (v3.3 §20 lesson: a single Notify can only wake one waiter).
    let (shutdown, shutdown_rx) = tokio::sync::watch::channel(false);
    if config.tray_enabled {
        let control_url = format!("http://{}/", state.control_addr);
        simpleharn::tray::spawn(
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

/// Per-leg cap on the rare first-start system probes: each leg (card, engine)
/// waits no longer than this before its `None` degrade. A fully stalled driver
/// therefore delays first boot by ~2x this, not one shared deadline; steady boots
/// (config file present) never run the probes at all.
const FIRST_RUN_PROBE_BUDGET: Duration = Duration::from_secs(3);

/// Budget for the one detached boot cross-check sample of the card's total. The
/// cross-check is a diagnostic, never a boot dependency, so a card that does not
/// answer within this is reported as "not sampled" and boot continues.
const BOOT_CARD_SAMPLE_BUDGET: Duration = Duration::from_secs(12);

/// Budget the §0.2 boot ready gate waits for every listener's first verdict. Past
/// it, listeners still pending are amber (no verdict), never certified healthy.
const BOOT_VERDICT_GATE: Duration = Duration::from_secs(10);

/// Poll cadence inside [`BOOT_VERDICT_GATE`]: a short fixed sleep, not a wake-on-event
/// channel, so the gate adds at most this much latency after the last verdict lands.
const BOOT_VERDICT_POLL: Duration = Duration::from_millis(5);

/// Grace between aborting the listener supervisors and returning from `run`, so a
/// supervisor mid-teardown finishes without racing the process exit.
const SHUTDOWN_DRAIN_GRACE: Duration = Duration::from_millis(100);

/// First-start gate: materialization fires only for the beside-the-exe
/// `simpleharn.json` that is absent — never for a differently named file, and
/// never for a `SIMPLEHARN_CONFIG` path (an explicit env file is an instruction:
/// boot already failed when it was missing, FINDINGS-14 [3]; if it exists it is
/// not a target).
fn first_run_target(config: &AppConfig) -> Option<PathBuf> {
    config.source_path.clone().filter(|path| {
        path.file_name()
            .is_some_and(|name| name == config::CONFIG_FILE)
            && !path.exists()
    })
}

/// One first-start sweep, every leg on the blocking pool behind its own budget:
/// card total via the synchronous NVML-only witness leg (a wedged driver can park
/// one blocking thread, never the boot), engine binary via PATH + the known dir.
/// Each leg answers `None` on failure and logs why; callers degrade to defaults.
async fn probe_system(witness: &Arc<simpleharn::witness::NvmlWitness>) -> firstrun::ProbeResults {
    let probing = Arc::clone(witness);
    let total = match tokio::time::timeout(
        FIRST_RUN_PROBE_BUDGET,
        tokio::task::spawn_blocking(move || probing.sample_boot()),
    )
    .await
    {
        Ok(Ok(Ok(sample))) => {
            let gib = firstrun::floor_gib(sample.total_gib);
            if firstrun::trustworthy(gib) {
                Some(gib)
            } else {
                tracing::warn!(
                    reading = gib,
                    "first start: card answered garbage; keeping the default ceiling"
                );
                None
            }
        }
        Ok(Ok(Err(reason))) => {
            tracing::warn!(reason = %reason, "first start: NVML probe failed; keeping the default ceiling");
            None
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "first start: NVML probe task failed; keeping the default ceiling");
            None
        }
        Err(_elapsed) => {
            tracing::warn!(budget = ?FIRST_RUN_PROBE_BUDGET, "first start: NVML probe timed out; keeping the default ceiling");
            None
        }
    };
    let llama = match tokio::time::timeout(
        FIRST_RUN_PROBE_BUDGET,
        tokio::task::spawn_blocking(firstrun::detect_llama_server),
    )
    .await
    {
        Ok(Ok(found)) => {
            if found.is_none() {
                tracing::info!("first start: no llama-server.exe on PATH; llama_server_exe stays unset — llama rows must each name their own `llama_server_exe`");
            }
            found
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "first start: llama-server probe task failed");
            None
        }
        Err(_elapsed) => {
            tracing::warn!("first start: llama-server probe timed out");
            None
        }
    };
    firstrun::ProbeResults {
        vram_total_gb: total,
        llama_server_exe: llama,
    }
}

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
/// The llama-binary posture the boot banner warns about (PH-RUST-03: the
/// CHOICE is a pure function so the falsifier can pin it without a runtime or
/// a filesystem): unset is a posture, a configured-but-absent binary is a
/// fault, a present binary is silence. `exe_missing` is the stat verdict, only
/// meaningful when a binary is configured.
fn llama_binary_warning(exe: Option<&std::path::Path>, exe_missing: bool) -> Option<&'static str> {
    match exe {
        None => Some(
            "llama_server_exe is not configured: llama loads will refuse unless each row \
             names its own `llama_server_exe`",
        ),
        Some(_) if exe_missing => Some(
            "llama-server.exe not found; llama-engine loads will fail until \
             llama_server_exe is configured",
        ),
        Some(_) => None,
    }
}

async fn log_boot_banner(
    scheduler: &SchedulerHandle,
    llama_server_exe: Option<&std::path::Path>,
    modelconf_dir: &std::path::Path,
    wsl_account: Option<&str>,
) {
    // FR-15: an unset global is legal (rows may each name their own binary),
    // but it is an unsafe posture worth one boot warn.
    match llama_server_exe {
        Some(exe) => {
            let owned = exe.to_path_buf();
            let exe_missing = tokio::task::spawn_blocking(move || !owned.exists())
                .await
                // FINDINGS-14 [8]: fail toward warning — a check that could not run must
                // not report the reassuring "exe is present" verdict.
                .unwrap_or(true);
            if let Some(message) = llama_binary_warning(Some(exe), exe_missing) {
                tracing::warn!(path = %exe.display(), "{message}");
            }
        }
        None => tracing::warn!(
            "{}",
            llama_binary_warning(None, false).expect("the unset arm always names its posture")
        ),
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
            target: "simpleharn::boot",
            "{}",
            format!("{marker}/{}/{}/{path}", entry.category, entry.engine),
        );
    }
    for error in &catalog.errors {
        tracing::warn!(
            target: "simpleharn::boot",
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
    entry: &simpleharn::model::ModelEntry,
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
    category: simpleharn::config::CategoryConfig,
    listeners: simpleharn::listeners::ListenerRegistry,
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
                    // R2-14-02: publish the outage BEFORE the retry sleep, exactly as
                    // the bind-failure arm does — the port is dead now, and the
                    // registry must not stay stale-Serving (tray green over a dead
                    // port, G11; doctrine 17) for the whole retry window.
                    listeners.mark_down(category.id, format!("listener {addr} stopped: {error}"));
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

/// What the §0.2 ready gate concluded from one registry snapshot.
///
/// `Pending` is not a verdict: a category still waiting on its supervisor keeps
/// `bind_ms` unrecorded and stops the boot line from claiming "resolved"
/// (FINDINGS-14 [7]). Extracted from [`run`] so the decision is unit-testable without
/// a live supervisor, and so the gate cannot be quietly re-inverted to counting map
/// entries.
struct GateVerdict {
    /// Categories that are bound and serving.
    serving: usize,
    /// Categories with no verdict yet, in canonical order.
    pending: Vec<simpleharn::config::CategoryId>,
}

impl GateVerdict {
    /// Whether every configured category has a verdict (`Serving` or `Down`).
    fn resolved(&self) -> bool {
        self.pending.is_empty()
    }
}

/// The ready gate's verdict for one snapshot of the listener registry.
fn gate_verdict(
    snapshot: &std::collections::BTreeMap<
        simpleharn::config::CategoryId,
        simpleharn::listeners::ListenerState,
    >,
) -> GateVerdict {
    GateVerdict {
        serving: snapshot
            .values()
            .filter(|state| matches!(state, simpleharn::listeners::ListenerState::Serving))
            .count(),
        pending: snapshot
            .iter()
            .filter(|(_, state)| state.is_pending())
            .map(|(category, _)| *category)
            .collect(),
    }
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
        "simpleharn booting: listeners before engines, zero engine spawns (§0.2)"
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
                "publishing to the local network with NO authentication: every device on the segment can load models and drive the GPUs — set `control_token` or `SIMPLEHARN_TOKEN`"
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
    // §0.2 "ready": every listener carries a verdict (Serving or Down). Wait
    // (bounded) for the supervisors' first verdict, then record the boot measures.
    // The gate counts VERDICTS, not entries: the registry is seeded `Pending` at
    // construction, so `len()` says nothing about readiness (FINDINGS-14 [7]).
    let deadline = tokio::time::Instant::now() + BOOT_VERDICT_GATE;
    loop {
        if state.listeners.pending().is_empty() || tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(BOOT_VERDICT_POLL).await;
    }
    {
        let verdict = gate_verdict(&state.listeners.snapshot());
        let bind_ms = boot_started.elapsed().as_millis() as u64;
        if verdict.resolved() {
            state
                .boot
                .write()
                .expect("boot timings lock poisoned by a writer panic")
                .set_bound(bind_ms);
            tracing::info!(
                bind_ms,
                serving = verdict.serving,
                "boot ready (0.2): config validated, catalog scan queued, listeners resolved"
            );
        } else {
            // The deadline path: a supervisor never published. `bind_ms` stays
            // unrecorded — the measure means "every category has a verdict", and
            // recording it here would certify a boot whose ports are unproven — and
            // the word "resolved" is not printed (FINDINGS-14 [7]).
            let names = verdict
                .pending
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ");
            tracing::error!(
                categories = %names,
                serving = verdict.serving,
                "listeners UNRESOLVED: [{names}] after the {} s boot gate; they are amber (no verdict), not healthy",
                BOOT_VERDICT_GATE.as_secs()
            );
        }
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
        let custody_runtime = boot_runtime.clone();
        let custody_grace = Duration::from_secs(state.config.release_timeout_s);
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
            // Custody sweep (chunk 10 slice 10b, R8): a strata router left by a
            // crashed run is reaped only on proof from the harness's OWN
            // sidecar (pid alive AND start-time token equal); the engine's own
            // leftover probe is untrusted and is never consulted here. The
            // sweep is catalog-independent, so it runs before the catalog read
            // below, which may return early.
            let custody = custody_runtime.reclaim_custody_at_boot(custody_grace).await;
            if !(custody.reaped.is_empty() && custody.stale.is_empty() && custody.stuck.is_empty())
            {
                tracing::info!(
                    sweep_ms = dispatched.elapsed().as_millis() as u64,
                    reaped = ?custody.reaped,
                    stale = ?custody.stale,
                    stuck = ?custody.stuck,
                    "boot strata custody sweep finished (§0.2 step 4 companion)"
                );
            }
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
    // Carries the `simpleharn::shutdown` target so "clean exit or killed?" is
    // answerable from the file alone — the question today's incident could not answer.
    tracing::info!(
        target: "simpleharn::shutdown",
        uptime_s = boot_started.elapsed().as_secs(),
        log_write_failures = observability::log_write_failures(),
        "shutdown requested; stopping listeners"
    );
    for supervisor in supervisors {
        supervisor.abort();
    }
    tokio::time::sleep(SHUTDOWN_DRAIN_GRACE).await;
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
        boot_config, boot_order, decode_console_output, first_run_target, gate_verdict,
        llama_binary_warning, log_boot_banner, parse_excluded_ranges, probe_outcome, run, run_cli,
        supervise_listener,
    };
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use simpleharn::api::{control_router, ApiState};
    use simpleharn::config::{self, AppConfig, CategoryConfig, CategoryId, ConfigError};
    use simpleharn::engine::EngineRuntime;
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

    /// The CLI door of TASK-IMPORT-APPLY: only `import` is answered (anything
    /// else boots the harness), `--apply` takes exactly one row path, and every
    /// malformed shape is refused with a usage error instead of a boot.
    #[test]
    fn run_cli_answers_only_import_and_refuses_malformed_shapes() {
        // Not our verb: the harness boots, the CLI answers nothing.
        for words in [
            vec![],
            vec!["serve".to_string()],
            vec!["status".to_string()],
        ] {
            assert_eq!(
                run_cli(words.clone().into_iter()),
                None,
                "not an import line must boot the harness: {words:?}"
            );
        }
        let dir = simpleharn::testutil::temp_dir("cli-import");
        let profile = dir.join("profile.json");
        std::fs::write(
            &profile,
            r#"{"exe": "C:\\Strata\\strata.exe", "model_name": "m"}"#,
        )
        .expect("profile");
        let profile = profile.display().to_string();
        for words in [
            vec!["import".to_string()],
            vec!["import".to_string(), "strata".to_string()],
            vec!["import".to_string(), "ninfer".to_string(), profile.clone()],
            vec![
                "import".to_string(),
                "strata".to_string(),
                profile.clone(),
                "tail".to_string(),
            ],
            vec![
                "import".to_string(),
                "strata".to_string(),
                profile.clone(),
                "--apply".to_string(),
            ],
            vec![
                "import".to_string(),
                "strata".to_string(),
                profile.clone(),
                "--apply".to_string(),
                "row.yaml".to_string(),
                "extra".to_string(),
            ],
            vec![
                "import".to_string(),
                "strata".to_string(),
                profile.clone(),
                "--merge".to_string(),
                "row.yaml".to_string(),
            ],
        ] {
            assert_eq!(
                run_cli(words.clone().into_iter()),
                Some(std::process::ExitCode::FAILURE),
                "a malformed import line must refuse, never boot: {words:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The end-to-end CLI path (cardless): `--apply` merges the block into the
    /// row on disk and answers success; emit-only prints and creates nothing.
    #[test]
    fn run_cli_apply_merges_the_row_and_emit_only_touches_nothing() {
        let dir = simpleharn::testutil::temp_dir("cli-import-apply");
        let profile = dir.join("profile.json");
        std::fs::write(
            &profile,
            r#"{"exe": "C:\\Strata\\strata.exe", "model_name": "m", "gpu": 0}"#,
        )
        .expect("profile");
        let row = dir.join("row.yaml");
        std::fs::write(
            &row,
            "# keep me\nname: m\nengine: strata\nvram_gb: 24.0\nstrata_config: C:/old.json\n",
        )
        .expect("row");
        // Emit-only: success, and the directory grows nothing.
        assert_eq!(
            run_cli(
                ["import", "strata", &profile.display().to_string()]
                    .into_iter()
                    .map(String::from)
            ),
            Some(std::process::ExitCode::SUCCESS)
        );
        assert_eq!(
            std::fs::read_to_string(&row).expect("row unchanged"),
            "# keep me\nname: m\nengine: strata\nvram_gb: 24.0\nstrata_config: C:/old.json\n"
        );
        // --apply: the block lands, the scheduling bytes survive, the legacy
        // pointer line is deleted.
        assert_eq!(
            run_cli(
                [
                    "import",
                    "strata",
                    &profile.display().to_string(),
                    "--apply",
                    &row.display().to_string()
                ]
                .into_iter()
                .map(String::from)
            ),
            Some(std::process::ExitCode::SUCCESS)
        );
        let text = std::fs::read_to_string(&row).expect("read back");
        assert!(text.starts_with("# keep me\n"), "header lost: {text}");
        assert!(
            text.contains("vram_gb: 24.0"),
            "scheduling key lost: {text}"
        );
        assert!(
            text.contains("serve_model: m") && text.contains("strata_exe: C:\\Strata\\strata.exe"),
            "the block did not merge: {text}"
        );
        assert!(text.contains("strata_gpu: 0"), "emitted key lost: {text}");
        assert!(
            !text.contains("strata_config"),
            "the pointer line survived: {text}"
        );
        assert!(
            !std::fs::read_dir(&dir)
                .expect("read dir")
                .filter_map(Result::ok)
                .any(|e| e.file_name().to_string_lossy().ends_with(".sh.tmp")),
            "the CLI write leaves no temp"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn first_run_target_fires_only_for_a_missing_beside_exe_config() {
        let root = simpleharn::testutil::temp_dir("first-run-target");
        let mut cfg = AppConfig {
            source_path: None,
            ..AppConfig::default()
        };
        assert_eq!(
            first_run_target(&cfg),
            None,
            "nowhere to create is not a target"
        );

        let target = root.join(config::CONFIG_FILE);
        cfg.source_path = Some(target.clone());
        assert_eq!(
            first_run_target(&cfg),
            Some(target.clone()),
            "absent beside the exe: materialize"
        );

        std::fs::write(&target, "{}").expect("write config");
        assert_eq!(
            first_run_target(&cfg),
            None,
            "an existing file is never a target (never overwrite)"
        );

        // An explicit SIMPLEHARN_CONFIG path — existing or not — is an instruction,
        // never a materialization target (FINDINGS-14 [3]).
        let named = root.join("operator-spelled.json");
        std::fs::write(&named, "{}").expect("write env config");
        cfg.source_path = Some(named);
        assert_eq!(
            first_run_target(&cfg),
            None,
            "a differently-named env file is inert"
        );
        cfg.source_path = Some(root.join("missing-env-file.json"));
        assert_eq!(
            first_run_target(&cfg),
            None,
            "a path not named CONFIG_FILE is never a target, existing or not"
        );
    }

    #[test]
    fn missing_file_named_by_simpleharn_config_is_fatal() {
        // FINDINGS-14 [3]: the env var is an instruction, not an absence.
        let dir = simpleharn::testutil::temp_dir("env-missing");
        let requested = dir.join("operator-typo.json");
        let errors = boot_config(Some(requested.display().to_string()))
            .expect_err("a missing file named by SIMPLEHARN_CONFIG must be fatal");
        let text = error_text(&errors);
        assert!(
            text.contains(&requested.display().to_string()),
            "the error must name the requested path: {text}"
        );
        assert!(
            !text.contains("simpleharn.json"),
            "the honest report names the requested path, not the default file: {text}"
        );
    }

    /// Serializes the two tests that observe the beside-the-exe default config:
    /// one materializes probe files in the exe directory, and the other must
    /// not boot while they exist — its `default.exists()` guard only sees what
    /// a same-binary sibling leaves on disk, so the lock keeps the skip honest.
    static EXE_DIR_CONFIG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn missing_default_config_still_defaults() {
        // Regression pin for [3]: unset env + no file beside the exe keeps the
        // §0.1 defaults path untouched. Guarded: a stray config beside the test
        // exe is not the case under test.
        let _serial = EXE_DIR_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(default) = config::default_config_path() else {
            return;
        };
        if default.exists() {
            return;
        }
        let loaded = boot_config(None).expect("unset env with no file must boot defaults");
        let expected = AppConfig {
            source_path: Some(default),
            ..AppConfig::default()
        };
        assert_eq!(loaded, expected);
    }

    /// Probe files that remove themselves on scope exit, including through an
    /// assertion panic.
    struct ProbeFiles(Vec<std::path::PathBuf>);

    impl Drop for ProbeFiles {
        fn drop(&mut self) {
            for path in &self.0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }

    /// TASK-NAMING-CLEANUP pin (a): a config directory that holds both the
    /// primary name and files under older spellings boots on `simpleharn.json`
    /// — bytes and stamp alike. The older files sit in the same directory with
    /// distinct sentinel values; boot must never look at them.
    /// NAMING-CLEANUP-EXEMPT: the older spellings are materialized here only
    /// to prove the boot ignores them.
    /// Falsifier: restore the old in-place fallback discovery in `boot_config`
    /// — with the primary deleted first, the boot stamps the sentinel file and
    /// the asserts below go red.
    #[test]
    fn a_dir_holding_older_config_names_boots_on_the_primary_file() {
        let _serial = EXE_DIR_CONFIG_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(default) = config::default_config_path() else {
            return;
        };
        let dir = default.parent().expect("exe directory").to_path_buf();
        if default.exists() {
            return; // a real deployment config beside the test exe: not the case under test
        }
        // NAMING-CLEANUP-EXEMPT: the two older spellings boot must ignore.
        let previous = dir.join("simplehar-se.json"); // NAMING-CLEANUP-EXEMPT
        let older = dir.join("simplehar.json"); // NAMING-CLEANUP-EXEMPT
        let _probes = ProbeFiles(vec![default.clone(), previous.clone(), older.clone()]);
        std::fs::write(&default, r#"{"vram_total_gb": 41.0}"#).expect("primary config");
        std::fs::write(&previous, r#"{"vram_total_gb": 42.0}"#).expect("previous-name sibling");
        std::fs::write(&older, r#"{"vram_total_gb": 43.0}"#).expect("older sibling");
        let loaded = boot_config(None).expect("boot must read the primary file");
        assert_eq!(
            loaded.vram_total_gb, 41.0,
            "boot must read the `simpleharn.json` bytes, not a sibling's"
        );
        assert_eq!(
            loaded.source_path.as_deref(),
            Some(default.as_path()),
            "the write-back stamp must name the primary file"
        );

        // The other half of the pin: with only older-named files present, boot
        // does NOT load one — it boots on built-in defaults stamped at the
        // primary path (FINDINGS-14 [2]'s copy-forward is retired with the name).
        std::fs::remove_file(&default).expect("remove primary");
        let loaded = boot_config(None).expect("older-named files alone boot on defaults");
        assert_eq!(
            loaded,
            AppConfig {
                source_path: Some(default),
                ..AppConfig::default()
            },
            "an older-spelling file is ignored: no bytes load, defaults stand"
        );
    }

    #[test]
    fn simpleharn_config_pointing_at_a_present_file_loads_it() {
        let dir = simpleharn::testutil::temp_dir("env-present");
        let path = dir.join("custom-name.json");
        std::fs::write(&path, r#"{"vram_total_gb": 42.0}"#).expect("config file");
        let loaded =
            boot_config(Some(path.display().to_string())).expect("a present file must load");
        assert_eq!(loaded.vram_total_gb, 42.0, "the file's value must survive");
        assert_eq!(loaded.source_path, Some(path));
    }

    #[test]
    fn invalid_env_named_config_names_the_file_that_failed() {
        // R2-14-01: a fatal invalid-config boot exits before the `boot configuration
        // config=<path>` line ever runs, so the fatal error is the only failure output
        // — it must name the file this boot loaded, not `simpleharn.json` it never
        // read. Falsifier: revert the `boot_config` path re-stamp and both asserts fail.
        let dir = simpleharn::testutil::temp_dir("env-invalid");
        let path = dir.join("custom-name.json");
        std::fs::write(&path, r#"{"wrok_port": 9}"#).expect("invalid config file");
        let errors = boot_config(Some(path.display().to_string()))
            .expect_err("a present-but-invalid env file must be fatal");
        let text = error_text(&errors);
        assert!(
            text.contains(&path.display().to_string()),
            "the fatal error must name the file this boot loaded: {text}"
        );
        assert!(
            !text.contains(config::CONFIG_FILE),
            "the report must not name the default file this boot never read: {text}"
        );
    }

    #[test]
    fn semantically_invalid_env_config_names_the_file_and_the_key() {
        // REV6-04: a present env file that parses but fails validation must also name
        // the loaded file in the fatal output — the shape-error arm alone was not
        // enough. Falsifier: revert the validate re-stamp and the first assert fails.
        let dir = simpleharn::testutil::temp_dir("env-invalid-semantic");
        let path = dir.join("custom-name.json");
        std::fs::write(&path, r#"{"load_timeout_s": 0}"#).expect("config file");
        let errors = boot_config(Some(path.display().to_string()))
            .expect_err("a zero load deadline refuses the boot");
        let text = error_text(&errors);
        assert!(
            text.contains(&path.display().to_string()),
            "the fatal error must name the loaded file: {text}"
        );
        assert!(
            text.contains("load_timeout_s"),
            "and still name the offending key: {text}"
        );
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

    #[test]
    fn the_ready_gate_never_calls_no_verdict_resolved() {
        // FINDINGS-14 [7]: the §0.2 gate decides on verdicts, never on entry counts.
        // A seeded (all-`Pending`) registry must withhold both `bind_ms` and the word
        // "resolved", and must be named category by category. Falsifier: count
        // `snapshot().len()` instead of `pending()` and the first two assertions fail.
        let registry = simpleharn::listeners::ListenerRegistry::new();
        let verdict = gate_verdict(&registry.snapshot());
        assert!(!verdict.resolved(), "no supervisor has spoken yet");
        assert_eq!(
            verdict.pending,
            CategoryId::all().to_vec(),
            "the UNRESOLVED line must name every silent category"
        );
        assert_eq!(verdict.serving, 0, "nothing may be counted as serving");

        registry.mark_serving(CategoryId::Work);
        registry.mark_serving(CategoryId::Rp);
        registry.mark_serving(CategoryId::Aux);
        let partial = gate_verdict(&registry.snapshot());
        assert!(
            !partial.resolved(),
            "one category without a verdict withholds both bind_ms and 'resolved'"
        );
        assert_eq!(partial.pending, vec![CategoryId::Emb]);
        assert_eq!(partial.serving, 3);

        registry.mark_down(CategoryId::Emb, "cannot bind 127.0.0.1:8711".to_string());
        let resolved = gate_verdict(&registry.snapshot());
        assert!(
            resolved.resolved(),
            "a `Down` verdict is a verdict: the gate closes and bind_ms may stand"
        );
        assert_eq!(
            resolved.serving, 3,
            "and the boot line still says how many ports actually serve"
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
        use simpleharn::config::CategoryId;
        use simpleharn::model::ModelEntry;
        let entry = |engine: &str, model_ref: Option<&str>| ModelEntry {
            name: "m".to_string(),
            engine: engine.to_string(),
            category: CategoryId::Work,
            vram_gb: None,
            ram_gb: None,
            gguf_size_gb: None,
            cost: simpleharn::model::CostClass::Fixed,
            vram_gate: simpleharn::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            measured_ram_gb: None,
            measured_ram_min_gb: None,
            fit_gb: None,
            model_ref: model_ref.map(str::to_string),
            serve_model: None,
            file: None,
            raw: Default::default(),
        };
        let dir = simpleharn::testutil::temp_dir("banner");
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
            _spec: &simpleharn::engine::LaunchSpec,
            _ticket: simpleharn::ownership::SpawnTicket,
        ) -> Result<simpleharn::engine::Spawned, simpleharn::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot spawned an engine before the listeners bound");
        }
        async fn kill(
            &self,
            _ticket: &simpleharn::ownership::SpawnTicket,
        ) -> Result<(), simpleharn::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot killed an engine before the listeners bound");
        }
        async fn release_confirmed(
            &self,
            _ticket: &simpleharn::ownership::SpawnTicket,
        ) -> Result<bool, simpleharn::engine::SpawnError> {
            panic!("FINDINGS-14 [5]: boot released an engine slot before the listeners bound");
        }
        async fn probe(
            &self,
            _port: u16,
            _readiness: &simpleharn::engine::ReadinessSpec,
        ) -> simpleharn::engine::Probe {
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
    ) -> (simpleharn::scheduler::SchedulerHandle, Arc<ApiState>) {
        let scheduler = simpleharn::scheduler::Scheduler::start(
            config.clone(),
            config.modelconf_dir.clone(),
            simpleharn::engine::default_adapters(config),
            runtime,
            Arc::new(simpleharn::budget::LedgerOnlyWitness)
                as Arc<dyn simpleharn::budget::VramWitness>,
        );
        let state = Arc::new(ApiState::new(
            config.clone(),
            scheduler.clone(),
            simpleharn::listeners::ListenerRegistry::new(),
            Arc::new(simpleharn::budget::LedgerOnlyWitness),
            simpleharn::api::BootTimings::default(),
        ));
        (scheduler, state)
    }

    /// A config whose catalog is empty and whose four listeners ask the OS for
    /// ephemeral ports: the whole boot path, without taking 8708-8711 away from a
    /// harness that may be serving, and without ever naming a container.
    fn ephemeral_boot_config(tag: &str) -> AppConfig {
        AppConfig {
            modelconf_dir: simpleharn::testutil::temp_dir(tag),
            ..AppConfig::default()
        }
    }

    async fn status_json(state: &Arc<ApiState>) -> serde_json::Value {
        // The guard's Host gate accepts only the harness's own configured ports
        // (FINDINGS-05 [4]), and these boot tests run on ephemeral configs whose
        // public ports may be 0, so the request declares the port THIS state is
        // configured to answer on rather than a literal.
        let host = format!("127.0.0.1:{}", state.config.control_category().public_port);
        let response = control_router(Arc::clone(state))
            .oneshot(
                Request::get("/har/status")
                    .header("host", host)
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

    /// PH-RUST-03 falsifier: the boot-posture choice is pinned pure. Delete
    /// the `None` arm of `llama_binary_warning` and the unset leg goes red;
    /// answer `Some` for a present binary and the silence leg goes red.
    #[test]
    fn the_boot_banner_picks_the_llama_binary_posture() {
        let unset = llama_binary_warning(None, false).expect("unset always names the posture");
        assert!(
            unset.contains("llama_server_exe is not configured"),
            "{unset}"
        );
        assert!(
            llama_binary_warning(None, true).is_some(),
            "no binary to stat: the unset posture answers either way"
        );
        let path = std::path::Path::new("Z:/nowhere/llama-server.exe");
        let missing =
            llama_binary_warning(Some(path), true).expect("a configured-absent binary is named");
        assert!(missing.contains("not found"), "{missing}");
        assert!(
            llama_binary_warning(Some(path), false).is_none(),
            "a present binary owes no warning"
        );
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
            config.llama_server_exe.as_deref(),
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
        // The registry is seeded `Pending`, so "has an entry" is no longer a wait —
        // this must poll for the actual `Serving` verdict the supervisor publishes.
        while state.listeners.snapshot().get(&CategoryId::Work)
            != Some(&simpleharn::listeners::ListenerState::Serving)
        {
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
        let boot_runtime = simpleharn::process::ProcessRuntime::new();
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
