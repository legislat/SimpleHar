//! Process supervision (DESIGN Step 8, LOCKED C+D, item 6 approved).
//!
//! One Win32 Job Object per owned engine tree with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`: if the harness dies for any reason, the
//! kernel kills the whole tree — the crash-proof orphan guarantee for *process*
//! engines. Detached WSL containers (ninfer) are the documented carve-out: the
//! job only reaps the `wsl.exe` client; the container itself is reaped
//! cooperatively (`docker rm -f` in `kill`) and reconciled pre-spawn by removing
//! any stale container of the same name — a harness crash can orphan one until the
//! model loads again.
//! Spawns are hidden (`CREATE_NO_WINDOW`); engine children additionally start
//! suspended and run only after the kill job has claimed them (R2-08-04), so no
//! engine descendant can be born outside the job. stdout/stderr are captured to
//! `logs/<slug>-<epoch>.log`, the stem sanitized by `sanitize_slug` — the one
//! forensic pool key shared with the container snapshots (R2-08-03). Engine
//! children receive exactly an OS baseline plus
//! `LaunchSpec.env` (§5, E3): harness secrets (`SIMPLEHARN_TOKEN`) and Python
//! import-hijack vars (`PYTHONPATH`, `VIRTUAL_ENV`) never leak into them. The
//! harness-internal control commands spawned here (`wsl.exe` docker teardown and
//! `docker ps`) are scrubbed the same way, because they execute *inside the
//! operator's distro*: inheriting there would carry the plaintext token across that
//! very boundary (FINDINGS2-0910 \[4], closing the asymmetry E3 left). Control
//! commands owned by other modules (`netsh` in `main.rs`, the `nvidia-smi` fallback
//! in `witness.rs`, the Web UI opener in `tray.rs`) still inherit — none of them
//! crosses into WSL — and are the smaller remaining half of that rule.
//! G12 release evidence: the child handle is signaled *and* the port refuses TCP
//! (for containers, the closed port after reaping is the evidence).

mod container;
mod control;
mod logs;
mod staging;
mod win32;

pub use container::ContainerCandidate;
pub use logs::{harness_logs_dir, prune_harness_log_files};
pub use staging::script_content_hash;
pub use win32::SingleInstanceGuard;

pub(crate) use staging::{
    rendered_profiles_dir, sanitize_slug, staged_profile_name, strata_profiles_dir,
};

use crate::custody::{self, CustodyLedger};
use crate::engine::{http_probe, EngineRuntime, LaunchSpec, Probe, SpawnError, Spawned};
use crate::ownership::{
    valid_container_name, ContainerClaims, ReapDecision, ResourceKind, SpawnTicket,
};
use async_trait::async_trait;
use container::{
    container_claims_path, container_logs, container_port, docker_ps, docker_ps_quiet, excerpt,
    published_elsewhere_evidence, reap_action, reap_container, reclaim_plan, release_ready,
    should_reap, stranded_plan, strata_custody_path, table_verdict, ReapAction, TableVerdict,
};
use control::{engine_child_env, run_control_command, run_control_command_timeout, ControlOutcome};
use logs::prune_model_logs;
use staging::{
    rewrite_config_slot, rewrite_script_slot, stage_rendered_profile, stage_script,
    staged_script_path,
};
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::process::{Child, Command};
use win32::{
    assign_to_job, create_kill_on_close_job, descendant_pids_of, resume_primary_thread,
    terminate_job, JobHandle, CREATE_NO_WINDOW,
};

#[cfg(test)]
use std::collections::BTreeMap;

#[cfg(test)]
use std::process::Stdio;

#[cfg(test)]
use container::{container_ps_command, container_rm, DockerProbeSeam, ReapSeam, ReleaseGate};

#[cfg(test)]
use control::{child_env_baseline_from, control_child_env, OS_ENV_BASELINE};

#[cfg(test)]
use logs::{is_engine_log, is_harness_log};

#[cfg(test)]
use staging::{
    cksum_expectation, escape_posix_regex, fnv1a64, staged_scripts_root, staging_driver,
    STAGED_PROFILE_KEEP,
};

#[cfg(test)]
use win32::DESCENDANT_CAP;

/// Poll pause between release-evidence checks inside the deadline.
const RELEASE_POLL: Duration = Duration::from_millis(100);
/// Per-model forensic retention (R9, operator 2026-10-31): the newest this many
/// launches' client captures **and** this many container snapshots survive, per model.
/// Replaces the old global-50 budget, which let a swap-heavy model evict another
/// model's forensic log — a failed launch's log must survive the second launch.
const ENGINE_LOG_KEEP: usize = 5;

/// One tracked engine tree: the child plus its Job Object.
struct EngineProc {
    child: Child,
    /// Win32 job handle; closing/terminating it kills the whole tree.
    job: JobHandle,
    /// Set once the kill path ran; the entry stays until release is confirmed.
    terminated: bool,
    /// Detached WSL container backing this engine (ninfer): the job object only
    /// reaps the `wsl.exe` client, so `kill` must additionally `docker rm -f`
    /// this name or the container keeps holding the GPU.
    container: Option<String>,
    /// The generation of the [`SpawnTicket`] that created this entry. Kill and release
    /// must match it: a port names a slot in the port space, not a process, and the
    /// ticket that identified the original child can outlive it — a stale ticket must
    /// never terminate the successor that inherited the port.
    generation: u64,
    /// The launch argv, kept only so the probe fast-fail can say WHICH path the
    /// early-exited engine was launched against (FINDINGS-13 [2]'s companion: an
    /// engine that dies on a deleted model must name it, not just its exit code).
    argv: Vec<String>,
    /// The child's OS pid captured at spawn. `Child::id()` reports `None` once
    /// the child has been reaped, so the release-time custody bookkeeping cannot
    /// ask the child again — the value must ride on the entry.
    pid: u32,
    /// The custody sidecar token written at spawn for a custodied launch
    /// (chunk 10 slice 10b); `release_confirmed` may only clear the entry
    /// bearing this token, never one a later launch wrote under the same pid.
    custody_token: Option<u64>,
    /// The verdict of `kill`'s one-shot container reap duty (R2-08-02): a
    /// container-backed entry may only end its table slot and ownership claim
    /// on a *confirmed* `docker rm -f` — a refusing port alone does not prove
    /// the detached container is gone. Process engines never reap and never
    /// set this, so their release evidence stays exactly the G12 pair.
    reap_confirmed: bool,
}

/// The production `EngineRuntime`: real hidden child processes under Job Objects.
#[derive(Clone)]
pub struct ProcessRuntime {
    engines: Arc<Mutex<HashMap<u16, EngineProc>>>,
    /// Shared so every clone of the runtime reads and writes one claim ledger:
    /// the boot reclaim and the spawn/release paths cannot disagree.
    claims: Arc<Mutex<ContainerClaims>>,
    /// The custody sidecar (slice 10b): recorded at spawn, cleared on
    /// confirmed release, swept at boot — one shared record per process
    /// lifetime, the same single-authority rule as the claim ledger.
    custody: Arc<Mutex<CustodyLedger>>,
    /// Test seam: per-engine overrides of the rendered-profile staging roots
    /// (`TASK-BASALT` stage 1 generalized the strata-only root of ruling 19;
    /// production always resolves the compiled-in row per launch).
    #[cfg(test)]
    profiles_root_overrides: BTreeMap<&'static str, PathBuf>,
    client: reqwest::Client,
    logs_dir: PathBuf,
    wsl_distro: String,
    wsl_user: String,
    /// Test seam: fails the job-object step *after* `command.spawn()` succeeded, so
    /// the post-spawn failure path — the one that must leave no ownership evidence
    /// behind (FINDINGS2-0910 [3]; since R2-08-04 it never has any, because the
    /// claim and custody writes happen only after the job seal succeeds) — is
    /// reachable without a wedged Win32 API.
    #[cfg(test)]
    fail_job_after_spawn: bool,
    /// Test seam: parks this runtime's `release_confirmed` pollers in the gap
    /// between the TCP probe and the final generation re-check of the table
    /// lock (see [`ReleaseGate`]).
    #[cfg(test)]
    release_gate: Option<Arc<ReleaseGate>>,
    /// Test seam: scripts the verdicts `kill`'s container reap duty reports
    /// (see [`ReapSeam`]).
    #[cfg(test)]
    reap_seam: Option<Arc<ReapSeam>>,
    /// Test seam (R5): the R5 reconcile probe goes through this docker door
    /// instead of `wsl.exe`, so pins can script `docker ps` and read back the
    /// docker verbs the reconcile actually asked for — the vocabulary bound the
    /// report-only doctrine promises.
    #[cfg(test)]
    docker_probe: Option<DockerProbeSeam>,
}

impl ProcessRuntime {
    /// Creates the runtime; log files land under `logs/` next to the current exe
    /// (falling back to the working directory).
    pub fn new() -> Self {
        ProcessRuntime {
            engines: Arc::new(Mutex::new(HashMap::new())),
            // One ledger for the whole process lifetime, shared by every clone:
            // claimed at spawn, cleared on confirmed release, consulted at boot.
            claims: Arc::new(Mutex::new(ContainerClaims::load(container_claims_path()))),
            // One custody sidecar per process lifetime, same single-authority
            // rule (slice 10b): recorded when a custodied launch really
            // started, cleared on confirmed release, swept at boot.
            custody: Arc::new(Mutex::new(CustodyLedger::load(strata_custody_path()))),
            #[cfg(test)]
            profiles_root_overrides: BTreeMap::new(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(2))
                .build()
                .expect("invariant: plain-HTTP client with a timeout needs no configuration"),
            logs_dir: harness_logs_dir(),
            // No WSL coordinates before boot: they name a machine-specific distro and a
            // personal account, so nothing is assumed. Boot wires both through `with_wsl`,
            // and the reaping guards refuse loudly rather than invoke `wsl.exe` without
            // them (review, 2026-10-26).
            wsl_distro: String::new(),
            wsl_user: String::new(),
            #[cfg(test)]
            fail_job_after_spawn: false,
            #[cfg(test)]
            release_gate: None,
            #[cfg(test)]
            reap_seam: None,
            #[cfg(test)]
            docker_probe: None,
        }
    }

    /// Sets the WSL coordinates used to address ninfer containers; the harness
    /// wires these from config.
    #[must_use]
    pub fn with_wsl(mut self, wsl_distro: impl Into<String>, wsl_user: impl Into<String>) -> Self {
        self.wsl_distro = wsl_distro.into();
        self.wsl_user = wsl_user.into();
        self
    }

    /// Points this runtime's ownership ledger at `path`. Test-only: the pins for
    /// [3] must neither touch the real `ninfer-claims.json` beside the test exe nor
    /// hand `wsl.exe` the operator's coordinates.
    #[cfg(test)]
    #[must_use]
    pub fn with_claims_ledger(mut self, path: impl Into<PathBuf>) -> Self {
        self.claims = Arc::new(Mutex::new(ContainerClaims::load(path)));
        self
    }

    /// Points this runtime's custody sidecar at `path`. Test-only, the very
    /// rule [`Self::with_claims_ledger`] states: pins must neither touch the
    /// real sidecar beside the test exe nor leave entries for the next run.
    #[cfg(test)]
    #[must_use]
    pub fn with_custody_ledger(mut self, path: impl Into<PathBuf>) -> Self {
        self.custody = Arc::new(Mutex::new(CustodyLedger::load(path)));
        self
    }

    /// Points this runtime's strata rendered-profile root at `path`. Test-only,
    /// the same rule as the ledger seams: pins must neither touch the directory
    /// beside the test exe nor the harness's release folder.
    #[cfg(test)]
    #[must_use]
    pub fn with_profiles_root(mut self, path: impl Into<PathBuf>) -> Self {
        self.profiles_root_overrides.insert("strata", path.into());
        self
    }

    /// Points this runtime's rendered-profile root for `engine` at `path`.
    /// Test-only, the rule [`Self::with_profiles_root`] states, generalized to
    /// the per-engine roots of `TASK-BASALT` stage 1.
    #[cfg(test)]
    #[must_use]
    pub fn with_rendered_profiles_root(
        mut self,
        engine: &'static str,
        path: impl Into<PathBuf>,
    ) -> Self {
        self.profiles_root_overrides.insert(engine, path.into());
        self
    }

    /// Where rendered profiles of `engine` stage for this runtime: the
    /// compiled-in row's custody root, or the test seam's override for it.
    fn profiles_root(&self, engine: &str) -> PathBuf {
        #[cfg(test)]
        if let Some(path) = self.profiles_root_overrides.get(engine) {
            return path.clone();
        }
        rendered_profiles_dir(engine)
    }

    /// Fails the job-object step after the client started. Test-only; see the
    /// [`Self::fail_job_after_spawn`] field.
    #[cfg(test)]
    #[must_use]
    pub fn with_injected_job_failure(mut self) -> Self {
        self.fail_job_after_spawn = true;
        self
    }

    /// Routes this runtime's R5 reconcile probe through a recorded docker seam
    /// answering with `outcome` (`None` = docker cannot run). Test-only, the
    /// rule [`Self::with_claims_ledger`] states: pins must neither touch the
    /// operator's WSL nor their docker. The calls handle is shared, so the test
    /// reads back the docker verbs the reconcile asked for.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn with_docker_probe(
        mut self,
        outcome: Option<Vec<String>>,
        calls: &Arc<Mutex<Vec<String>>>,
    ) -> Self {
        self.docker_probe = Some(DockerProbeSeam {
            outcome,
            calls: Arc::clone(calls),
        });
        self
    }

    /// Parks this runtime's `release_confirmed` pollers in the await gap
    /// before the final generation re-check. Test-only; see [`ReleaseGate`].
    #[cfg(test)]
    #[must_use]
    fn with_release_gate(mut self, gate: Arc<ReleaseGate>) -> Self {
        self.release_gate = Some(gate);
        self
    }

    /// Scripts this runtime's `kill`-carried container reap verdicts (the
    /// seam also reports the names each scripted verdict answered for).
    /// Test-only; see [`ReapSeam`].
    #[cfg(test)]
    #[must_use]
    fn with_reap_seam(mut self, verdicts: &[bool]) -> (Self, Arc<ReapSeam>) {
        let seam = Arc::new(ReapSeam {
            scripted: Mutex::new(verdicts.iter().copied().collect()),
            asked: Mutex::new(Vec::new()),
        });
        self.reap_seam = Some(Arc::clone(&seam));
        (self, seam)
    }

    /// Creates the kill-on-close job for the freshly spawned, still-suspended
    /// child. The `#[cfg(test)]` seam exists so the post-spawn failure path
    /// (which must leave no ownership evidence behind) can be reached without a
    /// wedged Win32 API.
    ///
    /// # Errors
    ///
    /// The Win32 reason the job could not be created.
    fn create_kill_job(&self) -> Result<windows::Win32::Foundation::HANDLE, String> {
        #[cfg(test)]
        if self.fail_job_after_spawn {
            return Err("injected job-object failure (test)".to_string());
        }
        // Safety: the handle is created here and handed to exactly one `JobHandle`,
        // whose `Drop` closes it; job handles are usable from any thread.
        unsafe { create_kill_on_close_job() }
    }

    /// Clears this launch's custody sidecar entry, token-guarded: `None` (the
    /// launch was never recorded) forgets nothing, and a foreign token can
    /// never erase a later launch's record (slice 10b, engine `drop_record`
    /// rule). Used by the confirmed release.
    fn forget_custody(&self, pid: u32, token: Option<u64>) {
        if let Some(token) = token {
            self.custody
                .lock()
                .expect("custody sidecar lock")
                .forget(pid, token);
        }
    }

    /// Runs this kill's reap duty: the scripted verdict while the REV7 test
    /// seam ([`ReapSeam`]) still holds one for it, the real `docker rm -f`
    /// otherwise. The seam replaces only the WSL/docker call, so the duty
    /// rule, the claim gate, the generation guard, and the entry recording
    /// stay the production paths; a runtime without a seam, or once the
    /// script is exhausted, runs [`reap_container`] exactly as before.
    async fn run_reap_duty(&self, name: &str) -> bool {
        #[cfg(test)]
        if let Some(seam) = &self.reap_seam {
            let scripted = seam
                .scripted
                .lock()
                .expect("reap verdict seam lock")
                .pop_front();
            if let Some(verdict) = scripted {
                seam.asked
                    .lock()
                    .expect("reap verdict seam lock")
                    .push(name.to_string());
                return verdict;
            }
        }
        reap_container(&self.wsl_distro, &self.wsl_user, name).await
    }

    /// Boot reconcile (deploy review, 2026-10-26; scoped by chunk 10 \[1], 2026-10-27):
    /// a container this harness started in an earlier lifetime survives a crash holding
    /// tens of GiB, and observed-VRAM admission then refuses every later load with no
    /// pointer to the culprit. A running container is destroyed only when
    /// [`crate::ownership::boot_reap_decision`] passes every ownership gate: declared by a **ninfer**
    /// model, docker-legal under the same validation the spawn path applies, and
    /// positively claimed by this harness's ledger at spawn. Any gate failing — above
    /// all a missing claim — logs a warning naming the container and SKIPs it: the
    /// harness never destroys on suspicion (operator policy). Best-effort: any failure
    /// logs and boot proceeds.
    pub async fn reclaim_at_boot(&self, candidates: &[ContainerCandidate]) {
        if candidates.is_empty() {
            return;
        }
        let declared: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        if self.wsl_user.trim().is_empty() || self.wsl_distro.trim().is_empty() {
            // Name the containers the catalog declares: without them the operator's next
            // load only says "insufficient VRAM", and this line is the sole pointer to the
            // orphan holding the card (review, 2026-10-26).
            tracing::warn!(
                declared = ?declared,
                distro = %self.wsl_distro,
                user = %self.wsl_user,
                "WSL coordinates missing from the config: boot reclaim stands down, an orphaned \
                 container keeps the card occupied"
            );
            return;
        }
        let Some(present) = docker_ps(&self.wsl_distro, &self.wsl_user).await else {
            return;
        };
        let plan = {
            let claims = self.claims.lock().expect("container claim ledger lock");
            reclaim_plan(&present, candidates, &claims)
        };
        let mut reaped: BTreeSet<String> = BTreeSet::new();
        for (candidate, decision) in plan {
            match decision {
                ReapDecision::Skip { reason } => tracing::warn!(
                    container = %candidate.name,
                    engine = %candidate.engine,
                    reason,
                    declared = ?declared,
                    "boot reclaim skipped: no ownership evidence for this container; leaving it running"
                ),
                ReapDecision::Reap => {
                    if !reaped.insert(candidate.name.clone()) {
                        continue; // two catalog entries, one reap
                    }
                    // Re-check the ledger under its lock immediately before destroying:
                    // a release that completed while we deliberated withdraws permission.
                    if !self
                        .claims
                        .lock()
                        .expect("container claim ledger lock")
                        .contains(&candidate.name)
                    {
                        continue;
                    }
                    tracing::warn!(container = %candidate.name, "reclaiming a container left behind by an earlier run");
                    if reap_container(&self.wsl_distro, &self.wsl_user, &candidate.name).await {
                        self.claims
                            .lock()
                            .expect("container claim ledger lock")
                            .unclaim(&candidate.name);
                    }
                }
            }
        }
    }

    /// Test-only read of the ownership ledger behind [3]'s pins.
    #[cfg(test)]
    fn claimed(&self, name: &str) -> bool {
        self.claims
            .lock()
            .expect("container claim ledger lock")
            .contains(name)
    }

    /// Boot custody reconcile + orphan sweep (chunk 10 slice 10b, R8): for
    /// each custody sidecar entry, act only on a process that provably matches
    /// the recorded identity (pid alive **and** start-time token equal); a
    /// non-matching entry is dropped as stale and whatever lives at that pid
    /// now is never touched. The engine's own leftover probe is untrusted —
    /// the release log's `[strata] could not check for engines left by an
    /// earlier run: RuntimeError` proves it can fail while saying nothing
    /// about what still runs — and is never consulted here: the sweep is
    /// driven purely by this harness's own sidecar, through pid-ops that have
    /// no engine channel. A matching orphan is terminated once (identity
    /// re-verified at the instant of the signal) and waited out with the
    /// normal stop-grace; an unconfirmed reap keeps its claim for the next
    /// boot. Best-effort: every failure path warns and boot proceeds.
    pub async fn reclaim_custody_at_boot(&self, grace: Duration) -> custody::ReconcileReport {
        let ledger = Arc::clone(&self.custody);
        if ledger.lock().expect("custody sidecar lock").is_empty() {
            return custody::ReconcileReport::default();
        }
        custody::reconcile(&ledger, &custody::WinCustodyOs, grace).await
    }

    /// Test-only read of the custody sidecar behind slice 10b's pins.
    #[cfg(test)]
    fn custody_entries(&self) -> Vec<custody::CustodyEntry> {
        self.custody
            .lock()
            .expect("custody sidecar lock")
            .entries()
            .cloned()
            .collect()
    }

    /// Number of tracked engines (test/leak assertion helper).
    pub fn engine_count(&self) -> usize {
        self.engines.lock().expect("engine table lock").len()
    }
}

impl Default for ProcessRuntime {
    fn default() -> Self {
        ProcessRuntime::new()
    }
}

#[async_trait]
impl EngineRuntime for ProcessRuntime {
    async fn spawn(&self, spec: &LaunchSpec, ticket: SpawnTicket) -> Result<Spawned, SpawnError> {
        // Template adoption (operator ruling 2026-10-31): a spec carrying a rendered
        // script runs THAT, never whatever file argv names — the bytes are staged
        // content-addressed, verified on disk, and the script position in argv is
        // rewritten to the staged path before anything spawns. A failed staging is a
        // spawn failure, not a fallback to an uncontrolled script.
        let spec = match spec.script.as_deref() {
            None => std::borrow::Cow::Borrowed(spec),
            Some(script) => {
                let slug = match &ticket.kind {
                    ResourceKind::Container { name } => name.clone(),
                    ResourceKind::ChildProcess => spec.log_name.clone(),
                };
                let path = staged_script_path(&slug, script);
                if let Err(reason) =
                    stage_script(&self.wsl_distro, &self.wsl_user, &path, script).await
                {
                    return Err(SpawnError::SpawnFailed {
                        reason: format!(
                            "cannot stage the harness-rendered autostart script: {reason}"
                        ),
                    });
                }
                let argv =
                    match rewrite_script_slot(spec.argv.clone(), path) {
                        Some(rewritten) => rewritten,
                        None => return Err(SpawnError::SpawnFailed {
                            reason:
                                "a staged-script spec must run bash: no script position to rewrite"
                                    .to_string(),
                        }),
                    };
                std::borrow::Cow::Owned(LaunchSpec {
                    argv,
                    ..spec.clone()
                })
            }
        };
        // Rendered-profile mode (operator ruling 2026-11-02 row 19; per-engine
        // roots in `TASK-BASALT` stage 1): a spec carrying a rendered profile
        // reads THAT file — the bytes stage into the harness's own root for
        // the tag's engine, the placeholder in the `--config` slot is rewritten
        // to the staged absolute path, and the name's content address is
        // verified against the bytes before the child can read them. A failed
        // staging is a spawn failure, never a fallback.
        let spec = match spec.rendered_profile.as_ref() {
            None => spec,
            Some(profile) => {
                let staged = stage_rendered_profile(
                    profile.engine,
                    &self.profiles_root(profile.engine),
                    &spec.log_name,
                    &profile.text,
                    spec.argv
                        .iter()
                        .position(|arg| arg == "--config")
                        .and_then(|at| spec.argv.get(at + 1))
                        .map(String::as_str)
                        .unwrap_or(""),
                );
                match staged.and_then(|path| {
                    rewrite_config_slot(spec.argv.clone(), &path.to_string_lossy())
                        .ok_or_else(|| "a rendered-profile spec must carry --config".to_string())
                }) {
                    Ok(argv) => std::borrow::Cow::Owned(LaunchSpec {
                        argv,
                        ..spec.as_ref().clone()
                    }),
                    Err(reason) => {
                        return Err(SpawnError::SpawnFailed {
                            reason: format!(
                                "cannot stage the harness-rendered {} profile: {reason}",
                                profile.engine
                            ),
                        });
                    }
                }
            }
        };
        let Some((program, rest)) = spec.argv.split_first() else {
            return Err(SpawnError::SpawnFailed {
                reason: "empty argv in launch spec".to_string(),
            });
        };
        // Container identity travels on the ticket (the provenance type); the
        // runtime never invents one. Every reap stays ownership-scoped, in two
        // parts: the shared name rule gates the pre-reap below, and the claim
        // ledger — minted only on a spawn that really started — gates the kill
        // reap and the boot reclaim (FINDINGS2-0910 [3]).
        let container = match &ticket.kind {
            ResourceKind::Container { name } => Some(name.clone()),
            ResourceKind::ChildProcess => None,
        };
        // Reconcile a stale container from a crashed run before this spawn: otherwise
        // `docker run` fails on the name conflict (opaque minutes-long readiness
        // failure) or the stale container answers readiness for a model the harness
        // never started.
        if let Some(name) = container.as_deref() {
            // The stale-container pre-reap is gated on the SHARED name rule — the one
            // the adapter, the claim ledger and the boot reclaim all use — so a name
            // docker could resolve as a container-ID prefix can never reach
            // `docker rm -f` from here. It is deliberately NOT a ledger write: the
            // claim that authorizes future reaping is minted only once a client
            // actually started (FINDINGS2-0910 [3]), so a load that failed before the
            // spawn leaves no ownership evidence behind. A refused name reaps nothing.
            if valid_container_name(name) {
                reap_container(&self.wsl_distro, &self.wsl_user, name).await;
            } else {
                tracing::warn!(
                    container = name,
                    port = ticket.port,
                    "not a name this harness may address: the stale container is left alone"
                );
            }
        }
        if let Err(error) = tokio::fs::create_dir_all(&self.logs_dir).await {
            return Err(SpawnError::SpawnFailed {
                reason: format!("cannot create {}: {error}", self.logs_dir.display()),
            });
        }
        // One forensic naming rule for the whole pool (R2-08-03): the client
        // capture and the container snapshot share one key — `sanitize_slug`,
        // the same slug rule the staged profiles use (`prune_staged_profiles`).
        // `spec.log_name` stays RAW as the custody/instance identity; only the
        // file name is filesystem-bare, so keep-N bounds one model's pair of
        // classes and two models cannot prune each other through a raw-vs-clean
        // split. Names that sanitize identically (`a b` / `a_b`) share one pool
        // by design — the accepted rule of the staged profiles (review 25 [3]).
        let pool = sanitize_slug(&spec.log_name);
        prune_model_logs(&self.logs_dir, &pool, ENGINE_LOG_KEEP);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let log_path = self.logs_dir.join(format!("{pool}-{epoch}.log"));
        let log_file = std::fs::File::create(&log_path).map_err(|e| SpawnError::SpawnFailed {
            reason: format!("cannot create {}: {e}", log_path.display()),
        })?;
        let mut command = Command::new(program);
        command
            .args(rest)
            .current_dir(&spec.cwd)
            .stdout(log_file.try_clone().map_err(|e| SpawnError::SpawnFailed {
                reason: format!("cannot duplicate log handle: {e}"),
            })?)
            .stderr(log_file)
            // R2-08-04: hidden AND suspended. The primary thread runs no
            // instruction until the seal below has put this child in its kill
            // job and resumed it, so no engine thread can ever run — and no
            // grandchild ever be born — outside the job.
            .creation_flags(
                CREATE_NO_WINDOW | windows::Win32::System::Threading::CREATE_SUSPENDED.0,
            )
            // §5 (E3): the child sees exactly the OS baseline plus `spec.env`,
            // never the harness environment. The control commands in this module
            // get the same baseline with no spec entries ([`control_child_env`]).
            .env_clear();
        for (key, value) in engine_child_env(std::env::vars(), &spec.env) {
            command.env(key, value);
        }
        let mut child = command.spawn().map_err(|e| SpawnError::SpawnFailed {
            reason: format!("cannot spawn {program}: {e}"),
        })?;
        // R2-08-04 (the escape window, doctrine: no engine descendant may ever
        // escape the job): a grandchild created between `CreateProcess` and
        // `AssignProcessToJobObject` escapes `KILL_ON_JOB_CLOSE` forever, and the
        // claim/custody writes used to sit inside that window and widen it. The
        // child therefore starts SUSPENDED and the order here is fixed: create
        // the kill job, assign the frozen child, resume its primary thread. The
        // first engine instruction runs only after the assignment, so every
        // descendant is born inside the job; a failed seal kills the still
        // frozen child, which never runs and never had a thread to fork.
        let pid = child.id().unwrap_or(0);
        // Safety: the job handle is created and assigned here, on this task,
        // while the child is still suspended and before it is stored;
        // `raw_handle` is valid while the child is alive, and the job is closed
        // exactly once, by `JobHandle::drop`.
        let job = match self.create_kill_job().map(JobHandle) {
            Ok(job) => job,
            Err(reason) => {
                let _ = child.kill().await;
                return Err(SpawnError::SpawnFailed { reason });
            }
        };
        let sealed = match (child.raw_handle(), pid) {
            (Some(handle), pid) if pid != 0 => {
                unsafe { assign_to_job(job.0, handle) }.and_then(|()| resume_primary_thread(pid))
            }
            _ => Err("the spawned client reported no usable handle or pid".to_string()),
        };
        if let Err(reason) = sealed {
            // Nothing is written as ownership evidence before the seal, so a
            // failed seal retracts nothing: the frozen child dies here with no
            // claim, no custody record, and no instruction executed (the
            // success-path-only rule of FINDINGS2-0910 [3], now enforced by
            // ordering alone).
            let _ = child.kill().await;
            return Err(SpawnError::SpawnFailed { reason });
        }
        // The claim is the positive ownership evidence the boot reclaim demands, so it
        // is minted HERE — only once a client really started and stands sealed in its
        // kill job (FINDINGS2-0910 [3]).
        // Minting it before the spawn let a load that failed at `create_dir_all`,
        // the log file or the spawn itself leave `ninfer-claims.json` holding a name
        // this harness never started, which then passed all three boot gates and
        // authorized `docker rm -f` against a container the operator had started by
        // hand under that name. `claim` keeps its own validity rule, so an ID-shaped
        // name is still never recorded.
        if let Some(name) = container.as_deref() {
            self.claims
                .lock()
                .expect("container claim ledger lock")
                .claim(name);
        }
        // Custody (slice 10b): a custodied launch's identity enters the sidecar
        // only on the success path — after every spawn-failure return — so a
        // load the harness reports as failed never leaves evidence for the
        // boot sweep to act on. A child whose start time cannot be read records
        // nothing (warn): an identity that cannot be proven must never
        // authorize a future kill, and a record that cannot be written never
        // stops a launch (the engine-side rule, ported).
        let custody_token = match (spec.custody.as_deref(), pid) {
            (Some(engine), pid) if pid != 0 => child
                .raw_handle()
                .and_then(|raw| custody::start_token_of(windows::Win32::Foundation::HANDLE(raw)))
                .inspect(|&token| {
                    self.custody.lock().expect("custody sidecar lock").record(
                        custody::CustodyEntry {
                            pid,
                            start_token: token,
                            port: ticket.port,
                            instance: spec.log_name.clone(),
                            engine: engine.to_string(),
                            claimed_at_ms: custody::now_ms(),
                        },
                    );
                }),
            (Some(_), _) => {
                tracing::warn!(
                    port = ticket.port,
                    instance = %spec.log_name,
                    "a custodied launch reported no pid: nothing recorded, this engine \
                     will not be reclaimed if this harness dies"
                );
                None
            }
            (None, _) => None,
        };
        tracing::info!(port = ticket.port, pid, log = %log_path.display(),
            "engine spawned under a kill-on-close job object");
        {
            let mut engines = self.engines.lock().expect("engine table lock");
            if let Some(stale) = engines.get(&ticket.port) {
                // Replacing a live entry closes its job handle, which reaps that tree by
                // design (a crash-retry reclaims its own port). Saying so keeps the one
                // case that must never happen — two live engines on one port — visible
                // instead of silent.
                tracing::warn!(
                    port = ticket.port,
                    replaced_generation = stale.generation,
                    new_generation = ticket.generation,
                    "replacing a tracked engine on this port; its job object reaps it"
                );
            }
            engines.insert(
                ticket.port,
                EngineProc {
                    child,
                    job,
                    terminated: false,
                    container,
                    generation: ticket.generation,
                    argv: spec.argv.clone(),
                    pid,
                    custody_token,
                    reap_confirmed: false,
                },
            );
        }
        Ok(Spawned {
            port: ticket.port,
            pid,
        })
    }

    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError> {
        let duty = {
            let mut engines = self.engines.lock().expect("engine table lock");
            match table_verdict(
                engines.get(&ticket.port).map(|entry| entry.generation),
                ticket.generation,
            ) {
                TableVerdict::Unowned => return Ok(()), // never ours: never a foreign kill
                TableVerdict::Stale { found } => {
                    return Err(SpawnError::StaleTicket {
                        port: ticket.port,
                        expected: ticket.generation,
                        found,
                    })
                }
                TableVerdict::Ours => {}
            }
            let engine = engines
                .get_mut(&ticket.port)
                .expect("invariant: the verdict above just read Ours for this port");
            // Safety: the job handle is valid (created in spawn, closed by Drop)
            // and terminating a job is safe from any thread.
            unsafe { terminate_job(engine.job.0) };
            // A confirmed reap retires the duty forever: re-kills (cancel,
            // release-retry) must not re-run `docker rm` (review, 2026-10-26).
            // An UNCONFIRMED reap re-arms on the next explicit kill (REV7-01),
            // so a failed `docker rm` cannot burn the slot until boot; the
            // ledger and the generation verdict keep every retry scoped.
            let duty = should_reap(
                engine.terminated,
                engine.reap_confirmed,
                engine.container.as_ref(),
            );
            engine.terminated = true;
            let _ = engine.child.start_kill();
            duty
        };
        // A detached WSL container survives its client process: reap it by name —
        // but the LEDGER, not the ticket, is the reap authority (FINDINGS2-0910 [3]).
        let action = match duty {
            Some(name) => {
                let claimed = self
                    .claims
                    .lock()
                    .expect("container claim ledger lock")
                    .contains(&name);
                reap_action(Some(name), claimed)
            }
            None => ReapAction::None,
        };
        match action {
            ReapAction::Reap(name) => {
                let reaped = self.run_reap_duty(&name).await;
                // R2-08-02: the reap verdict is the evidence `release_confirmed`
                // needs before it may end the claim, so it rides on the entry.
                // Generation-guarded: a successor that inherited this port
                // while docker deliberated owns the entry now, and no reap of
                // the retired lifetime may speak for it.
                if let Some(engine) = self
                    .engines
                    .lock()
                    .expect("engine table lock")
                    .get_mut(&ticket.port)
                {
                    if engine.generation == ticket.generation {
                        engine.reap_confirmed = reaped;
                    }
                }
            }
            ReapAction::SkipUnclaimed(name) => tracing::warn!(
                container = %name,
                port = ticket.port,
                "kill owed a container reap but the ledger holds no claim: leaving it running"
            ),
            ReapAction::None => {}
        }
        Ok(())
    }

    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        let (exited, is_container) = {
            let mut engines = self.engines.lock().expect("engine table lock");
            match table_verdict(
                engines.get(&ticket.port).map(|entry| entry.generation),
                ticket.generation,
            ) {
                TableVerdict::Unowned => return Ok(true), // never ours: nothing to confirm
                TableVerdict::Stale { found } => {
                    return Err(SpawnError::StaleTicket {
                        port: ticket.port,
                        expected: ticket.generation,
                        found,
                    })
                }
                TableVerdict::Ours => {}
            }
            let engine = engines
                .get_mut(&ticket.port)
                .expect("invariant: the verdict above just read Ours for this port");
            if !engine.terminated {
                return Ok(false);
            }
            let exited = engine
                .child
                .try_wait()
                .map(|status| status.is_some())
                .map_err(|e| SpawnError::KillFailed {
                    holder: format!("port {}", ticket.port),
                    reason: e.to_string(),
                })?;
            (exited, engine.container.is_some())
        };
        // Handle signaled; now the port must refuse TCP (G12 evidence pair).
        let port_closed = tokio::net::TcpStream::connect(("127.0.0.1", ticket.port))
            .await
            .is_err();
        // R2-08-01 pin seam (test-only, inert in production): park this task in
        // the await gap between reading the verdict and re-judging it under the
        // final table lock — the window this function must survive.
        #[cfg(test)]
        if let Some(gate) = &self.release_gate {
            gate.arrived.notify_one();
            gate.departed.notified().await;
        }
        if release_ready(is_container, exited, port_closed) {
            // The verdict above was read under the lock, but the lock was
            // dropped for the TCP await. Two live jobs can poll the SAME victim
            // ticket (the eviction job's victim and a same-slot reload's own
            // reaper — board REV6-03); a sibling poller that completed its
            // removal in this window lets the Dispose->Spawn gate insert the
            // successor before this line runs. Re-judge under the final lock
            // (R2-08-01): only the entry still bearing OUR generation may be
            // removed, and — R2-08-02 — a container entry only on its recorded
            // reap verdict. The bookkeeping below then uses the REMOVED entry,
            // never a capture from before the gap.
            enum ReleaseOutcome {
                /// Our entry, ours to end: its ownership records ride with it.
                Removed(Box<EngineProc>),
                /// A sibling poller already removed our entry, or a newer
                /// spawn inherited the port: the release of OUR child is
                /// proven, and whoever holds the entry now is a different
                /// lifetime this retired ticket may not touch.
                Superseded,
                /// Our entry, but its container reap was never confirmed while
                /// the ledger still holds the claim: the container may still
                /// hold the card, so entry and claim stay (see the return
                /// contract of [`reap_container`]). Carries the held entry's
                /// container name (REV7-04: it is always present here).
                ReapUnconfirmed(String),
            }
            let outcome = {
                let mut engines = self.engines.lock().expect("engine table lock");
                match table_verdict(
                    engines.get(&ticket.port).map(|entry| entry.generation),
                    ticket.generation,
                ) {
                    TableVerdict::Unowned | TableVerdict::Stale { .. } => {
                        ReleaseOutcome::Superseded
                    }
                    TableVerdict::Ours => {
                        // REV7-04: one lookup for the held entry. REV7-02: a
                        // container entry holds the release only while the
                        // ledger still backs it with a claim AND its reap has
                        // not confirmed. With no claim there is no ownership
                        // evidence left to preserve (boot reclaim already
                        // refuses the name), so the plain G12 pair decides and
                        // the SkipUnclaimed shape must not burn the slot until
                        // boot; REV7-01's explicit re-kill is the live retry.
                        let entry = engines
                            .get(&ticket.port)
                            .expect("invariant: the verdict above just read Ours for this port");
                        let unconfirmed_name = match entry.container.as_deref() {
                            Some(name) if !entry.reap_confirmed => Some(name.to_string()),
                            _ => None,
                        };
                        let reap_held = unconfirmed_name.as_deref().is_some_and(|name| {
                            self.claims
                                .lock()
                                .expect("container claim ledger lock")
                                .contains(name)
                        });
                        match unconfirmed_name {
                            Some(name) if reap_held => ReleaseOutcome::ReapUnconfirmed(name),
                            _ => ReleaseOutcome::Removed(Box::new(
                                engines.remove(&ticket.port).expect(
                                    "invariant: the verdict above just read Ours for this port",
                                ),
                            )),
                        }
                    }
                }
            };
            match outcome {
                ReleaseOutcome::Removed(engine) => {
                    if let Some(name) = engine.container.as_ref() {
                        // G12 evidence held AND `kill`'s `docker rm -f`
                        // confirmed the removal: the container is gone, so its
                        // ownership claim ends -- a later hand-started
                        // container reusing the name can never inherit a claim
                        // this harness no longer needs. An entry the ledger
                        // does not hold (the SkipUnclaimed shape, REV7-02)
                        // gets nothing written to its name: `unclaim` persists
                        // only a claim it actually removes.
                        self.claims
                            .lock()
                            .expect("container claim ledger lock")
                            .unclaim(name);
                    }
                    // The custody entry ends with the same evidence, on the
                    // REMOVED entry's own pid and token: token-guarded, so a
                    // release can never erase a later launch's record written
                    // under the same pid (slice 10b) — and a ticket that
                    // removed nothing clears nobody's record (R2-08-01).
                    self.forget_custody(engine.pid, engine.custody_token);
                    // `engine` drops here: `JobHandle::drop` closes the
                    // handle exactly once.
                    tracing::info!(
                        port = ticket.port,
                        "release confirmed: engine gone, port closed"
                    );
                    return Ok(true);
                }
                ReleaseOutcome::Superseded => {
                    tracing::info!(
                        port = ticket.port,
                        "release confirmed for the retired ticket; whoever holds the port now is untouched"
                    );
                    return Ok(true);
                }
                ReleaseOutcome::ReapUnconfirmed(container) => {
                    // R2-08-02: for a detached container the client's exit and
                    // a refusing port prove nothing about the container itself
                    // — `kill`'s `docker rm -f` is that evidence, and it
                    // failed. Stay unconfirmed: the release timeout keeps the
                    // claim, and the next boot reclaim — which keeps it too
                    // until docker confirms — stays the path with authority
                    // over a possibly-live container.
                    tracing::debug!(
                        port = ticket.port,
                        container = %container,
                        "release held: the container reap was not confirmed; keeping the ownership claim, an explicit re-kill re-arms the duty (REV7-01)"
                    );
                }
            }
        }
        tokio::time::sleep(RELEASE_POLL).await;
        Ok(false)
    }

    async fn discover_model_id(&self, port: u16) -> Option<String> {
        let url = format!("http://127.0.0.1:{port}/v1/models");
        let response = self.client.get(&url).send().await.ok()?;
        let body: serde_json::Value = response.json().await.ok()?;
        body["data"][0]["id"].as_str().map(str::to_string)
    }

    async fn engine_status_json(&self, port: u16, path: &str) -> Option<serde_json::Value> {
        // Same posture and the same 2 s client as `discover_model_id`: a
        // loopback GET, JSON in, `None` for any failure. Engine-blind by
        // contract — the per-engine parse lives in the adapter and the
        // sampler decides what absence means (witness fallback, never a
        // guess; TASK-ENGINE-MEASUREMENT ORCHESTRATION §1).
        let url = format!("http://127.0.0.1:{port}{path}");
        let response = self.client.get(&url).send().await.ok()?;
        response.json::<serde_json::Value>().await.ok()
    }

    async fn descendant_pids(&self, pid: u32) -> Vec<u32> {
        descendant_pids_of(pid)
    }

    async fn container_report(&self, container: &str) -> Option<String> {
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        // Ownership gate, same rule as the reap: never address a name this harness
        // could not have spawned (an ID-shaped or hostile name is refused).
        if !valid_container_name(container) {
            tracing::warn!(
                container,
                "not a name this harness may address: container_report skipped"
            );
            return None;
        }
        container_logs(&self.wsl_distro, &self.wsl_user, container).await
    }

    async fn published_elsewhere(&self, container: &str, offered: u16) -> Option<String> {
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        // Ownership gate ruling 1(b) applies strictly: the readiness report only
        // needs a well-shaped name, but this query reads the claim ledger too —
        // a container this harness never claimed is never even asked.
        let claimed = self
            .claims
            .lock()
            .expect("container claim ledger lock")
            .contains(container);
        if !valid_container_name(container) || !claimed {
            return None;
        }
        let (program, args) = container_port(&self.wsl_distro, &self.wsl_user, container);
        match run_control_command(&program, &args).await {
            // Only docker's own success line is evidence; a not-running,
            // wedged, or refused `docker port` keeps the original refusal
            // exactly as the probe wrote it.
            ControlOutcome::Ran(output) if output.status.success() => published_elsewhere_evidence(
                container,
                offered,
                &String::from_utf8_lossy(&output.stdout),
            ),
            ControlOutcome::Ran(output) => {
                tracing::debug!(
                    container,
                    stderr = %excerpt(&output.stderr),
                    "docker port refused publish evidence"
                );
                None
            }
            ControlOutcome::Failed(error) => {
                tracing::debug!(container, error = %error, "docker port cannot run for evidence");
                None
            }
            ControlOutcome::TimedOut => {
                tracing::debug!(container, "docker port timed out; no publish evidence");
                None
            }
        }
    }

    async fn unclaimed_container_names(&self, declared: &[String]) -> Option<BTreeSet<String>> {
        #[cfg(test)]
        if let Some(seam) = &self.docker_probe {
            seam.calls
                .lock()
                .expect("docker probe lock")
                .push("ps".to_string());
            let present = seam.outcome.clone()?;
            let claims = self.claims.lock().expect("container claim ledger lock");
            return Some(stranded_plan(&present, declared, &claims));
        }
        // Blank coordinates never reach `wsl.exe` — and the R5 probe answers
        // them SILENTLY, unlike the boot reclaim's pointer line: report-only
        // doctrine, a failed probe is not a warning.
        if self.wsl_distro.trim().is_empty() || self.wsl_user.trim().is_empty() {
            return None;
        }
        let present = docker_ps_quiet(&self.wsl_distro, &self.wsl_user).await?;
        let claims = self.claims.lock().expect("container claim ledger lock");
        Some(stranded_plan(&present, declared, &claims))
    }

    async fn save_container_log(&self, model: &str, text: &str) {
        // A snapshot with no bytes is forensic noise, not evidence: a container
        // that never exec'd yields an empty `docker logs`, and rotating a model's
        // genuine snapshots against such a file would destroy history to record
        // nothing.
        if text.trim().is_empty() {
            return;
        }
        // One naming rule, the same `sanitize_slug` the capture mint uses at
        // `spawn` and the staged profiles use (R2-08-03): adapters hand
        // `log_name` RAW as the instance identity, so BOTH forensic classes are
        // keyed under the sanitized stem and one per-model keep-N bounds the
        // pair. Names that sanitize identically share one pool by design, the
        // accepted rule of the staged profiles (review 25 [3]).
        let prefix = sanitize_slug(model);
        let epoch = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or_default();
        let path = self
            .logs_dir
            .join(format!("{prefix}-{epoch}.container.log"));
        if let Err(error) = tokio::fs::create_dir_all(&self.logs_dir).await {
            tracing::warn!(error = %error, "cannot create logs dir for a container snapshot");
            return;
        }
        match tokio::fs::write(&path, text.as_bytes()).await {
            Ok(()) => {
                prune_model_logs(&self.logs_dir, &prefix, ENGINE_LOG_KEEP);
                tracing::debug!(path = %path.display(), "container log snapshot written");
            }
            Err(error) => tracing::warn!(
                path = %path.display(),
                error = %error,
                "cannot write container log snapshot"
            ),
        }
    }

    async fn run_probe_command(
        &self,
        program: &std::path::Path,
        args: &[String],
        timeout: Duration,
    ) -> Option<String> {
        // R11: the fit probes ride the ONE control-command builder — hidden,
        // kill-on-drop, scrubbed to the OS baseline — never an engine spawn:
        // no ticket, no port, nothing registered. Every failure path warns and
        // answers `None`; the caller falls down the ladder, there is never an
        // invented number.
        match run_control_command_timeout(&program.to_string_lossy(), args, timeout).await {
            ControlOutcome::Ran(output) if output.status.success() => {
                Some(String::from_utf8_lossy(&output.stdout).into_owned())
            }
            ControlOutcome::Ran(output) => {
                tracing::warn!(
                    program = %program.display(),
                    stderr = %excerpt(&output.stderr),
                    "fit probe command refused; the row keeps its declared/estimate ladder"
                );
                None
            }
            ControlOutcome::Failed(error) => {
                tracing::warn!(
                    program = %program.display(),
                    error = %error,
                    "fit probe command could not start; the row keeps its declared/estimate ladder"
                );
                None
            }
            ControlOutcome::TimedOut => {
                tracing::warn!(
                    program = %program.display(),
                    timeout_s = timeout.as_secs(),
                    "fit probe timed out; the row keeps its declared/estimate ladder"
                );
                None
            }
        }
    }

    async fn probe(&self, port: u16, readiness: &crate::engine::ReadinessSpec) -> Probe {
        // Fast-fail (Step 3 cancel/dead-engine lesson): a child that already exited
        // must not eat the whole readiness deadline — readiness is unreachable.
        // Container-backed engines are exempt: the engine is the detached
        // container, not the short-lived wsl.exe client.
        let exited = {
            let mut engines = self.engines.lock().expect("engine table lock");
            engines.get_mut(&port).and_then(|engine| {
                if engine.container.is_some() {
                    return None;
                }
                engine
                    .child
                    .try_wait()
                    .ok()
                    .flatten()
                    .map(|status| (status.to_string(), engine.argv.clone()))
            })
        };
        if let Some((status, argv)) = exited {
            let (reason, missing) = early_exit_report(&argv, &status);
            if let Some(path) = missing {
                tracing::error!(
                    port,
                    path = %path,
                    "engine process exited early against a model file that is not on disk"
                );
            }
            return Probe::Failed { reason };
        }
        http_probe(&self.client, port, readiness).await
    }
}

/// Whether an argv word is a local path a missing-file diagnosis may name:
/// `scheme://` sentinels name no local file, and a bare module token
/// (`serve.server`) has no path shape at all — checking it would misname the
/// strata module as a missing model.
fn looks_like_local_path(candidate: &str) -> bool {
    !candidate.contains("://")
        && (candidate.contains('/')
            || candidate.contains('\\')
            || candidate.as_bytes().get(1) == Some(&b':'))
}

/// The fast-fail verdict: `(reason, missing path)`. When the launched spec named
/// a local model/profile file that is gone, the reason names it; otherwise the
/// pre-existing text is preserved verbatim. Pure given the filesystem — the stat
/// is its only effect-free input — so the decision is testable without a spawn.
/// The argv positions that carry a model file come from the `EnginePolicy`
/// table's `path_markers` cells (llama `-m <path>`, strata `--config <path>`);
/// a missing file at one of these slots turns the bare exit status of a
/// fast-fail into the file the operator must restore (FINDINGS-13 [2]'s
/// load-path companion); the catalog is never mutated from here (operator
/// ruling — a transient deletion must not silently rewrite the page).
fn early_exit_report(argv: &[String], status: &str) -> (String, Option<String>) {
    let markers = crate::engine::policy::path_markers();
    let missing = argv.windows(2).find_map(|window| {
        (markers.contains(&window[0].as_str())
            && looks_like_local_path(&window[1])
            && std::path::Path::new(&window[1]).metadata().is_err())
        .then(|| window[1].clone())
    });
    match missing {
        Some(path) => (
            format!("model file not found: {path} (engine exited early: {status})"),
            Some(path),
        ),
        None => (format!("engine process exited early ({status})"), None),
    }
}

#[cfg(test)]
mod tests;
