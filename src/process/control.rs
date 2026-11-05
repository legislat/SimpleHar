//! The env-scrub and control half: the OS baseline a harness child may keep,
//! the one assembly that builds a child environment over it, and the ONE
//! place a harness-internal control command is built and run. The scrub rule
//! (FINDINGS2-0910 [4]) is owned by the item docs that ride each item below.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use super::CREATE_NO_WINDOW;

/// Windows env var names (case-insensitive) an engine child may keep (§5 locked
/// decision, E3): external engines obey only harness-owned configuration, so a
/// child's environment is this baseline plus what [`LaunchSpec`] declares — never
/// the harness's own environment with its plaintext `SIMPLEHARN_TOKEN`, its
/// `PYTHONPATH`/`VIRTUAL_ENV` (strata is a Python child: import-hijack risk), and
/// every other stray var.
pub(super) const OS_ENV_BASELINE: &[&str] = &[
    "SystemRoot",
    "windir",
    "COMSPEC",
    "Path",
    "PATHEXT",
    "TEMP",
    "TMP",
];

/// Builds the child baseline by copying the [`OS_ENV_BASELINE`] names out of
/// `source` (the harness env at the spawn site), matching names case-insensitively:
/// Windows env names are case-insensitive and [`std::env::vars`] preserves whatever
/// spelling the parent carried, so lookups go through the uppercased key while the
/// output uses the baseline's canonical spelling. A name supplied under several
/// spellings (e.g. `PATH` and `Path`) collapses to one entry, first occurrence
/// wins. `SystemRoot`, `windir`, `COMSPEC` and `PATHEXT` are never invented: absent
/// from `source` they stay absent and the spawn fails loudly if Windows demands
/// one. `Path`, `TEMP` and `TMP` are always present via [`baseline_fallback`].
pub(super) fn child_env_baseline_from<I>(source: I) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let wanted: HashSet<String> = OS_ENV_BASELINE
        .iter()
        .map(|name| name.to_uppercase())
        .collect();
    let mut found: HashMap<String, String> = HashMap::new();
    for (name, value) in source {
        let upper = name.to_uppercase();
        if wanted.contains(&upper) {
            found.entry(upper).or_insert(value);
        }
    }
    OS_ENV_BASELINE
        .iter()
        .filter_map(|name| {
            let upper = name.to_uppercase();
            let value = found
                .get(&upper)
                .cloned()
                .or_else(|| baseline_fallback(&upper))?;
            Some(((*name).to_string(), value))
        })
        .collect()
}

/// The always-present baseline values the harness env may lack: `Path` falls back
/// to the harness's own directory and `TEMP`/`TMP` to [`std::env::temp_dir`];
/// every other name returns `None` and is omitted rather than invented.
///
/// # Panics
///
/// Only when `name_upper` is `"PATH"` and the harness executable path cannot be
/// resolved — there is no honest substitute for `Path`.
fn baseline_fallback(name_upper: &str) -> Option<String> {
    match name_upper {
        "PATH" => Some(
            std::env::current_exe()
                .expect("invariant: the harness exe path resolves; Path has no other fallback")
                .parent()
                .expect("invariant: the harness exe lives in a directory")
                .to_string_lossy()
                .into_owned(),
        ),
        "TEMP" | "TMP" => Some(std::env::temp_dir().to_string_lossy().into_owned()),
        _ => None,
    }
}

/// The complete environment for one engine child: the baseline copied from
/// `source` (the harness env at the spawn site), then every `spec_env` entry
/// applied last so a declared var overrides its baseline twin. Names are
/// case-insensitive on Windows, so a spec entry first removes any baseline pair
/// whose uppercased key equals its own, then appends under its own spelling —
/// the assembly never carries two spellings of one name.
///
/// Applies to `LaunchSpec`-driven engine spawns and, with an empty `spec_env`, to
/// the harness-internal WSL control commands built by [`run_control_command`] —
/// nothing spawned by this module inherits the harness environment any more
/// (FINDINGS2-0910 [4]). Shared by the spawn site and the tests so the tests
/// exercise the real production assembly.
pub(super) fn engine_child_env<I>(
    source: I,
    spec_env: &BTreeMap<String, String>,
) -> Vec<(String, String)>
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut env = child_env_baseline_from(source);
    for (key, value) in spec_env {
        let upper = key.to_uppercase();
        env.retain(|(name, _)| name.to_uppercase() != upper);
        env.push((key.clone(), value.clone()));
    }
    env
}

/// What a harness-internal control command may carry: exactly the OS baseline
/// ([`OS_ENV_BASELINE`]) and nothing else — no `SIMPLEHARN_TOKEN`, no `PYTHONPATH`,
/// no stray harness name (FINDINGS2-0910 [4]).
///
/// The earlier ruling here was "control commands run harness code, not operator
/// config, so they keep inheriting"; that justified the *inbound* direction only.
/// The two WSL control commands execute *outbound* — their whole purpose is to run
/// inside the operator's distro, the very boundary E3 called worse than a Windows
/// child environment — so inheriting was an outbound secret leak. They need nothing
/// but `SystemRoot`/`windir`/`Path`/`PATHEXT`/`COMSPEC`/`TEMP`/`TMP`, i.e. what
/// `wsl.exe` itself requires, so no separate control-only list was invented: one
/// baseline, one rule, nothing to drift.
pub(super) fn control_child_env() -> Vec<(String, String)> {
    engine_child_env(std::env::vars(), &BTreeMap::new())
}

/// How one harness-internal control command finished.
#[derive(Debug)]
pub(super) enum ControlOutcome {
    /// The command ran and reported; the caller judges the exit status and stdout.
    Ran(std::process::Output),
    /// The command could not be started at all (no `wsl.exe`, spawn refused).
    Failed(String),
    /// The command outlived [`CONTROL_TIMEOUT`].
    TimedOut,
}

/// The deadline every control command gets: a wedged `wsl.exe`/docker call must not
/// park a boot reconcile or an unload indefinitely.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);

/// The ONE site through which a harness-internal control command is built and run:
/// hidden (`CREATE_NO_WINDOW`), `kill_on_drop` so a timed-out client cannot
/// accumulate as an orphan, and with the scrubbed [`control_child_env`] rather than
/// the inherited environment (FINDINGS2-0910 [4]). Both WSL control commands go
/// through here; neither may build its own [`Command`], which is what made the
/// asymmetry between engine children and control commands possible in the first
/// place.
pub(super) async fn run_control_command(program: &str, args: &[String]) -> ControlOutcome {
    run_control_command_with_stdin(program, args, &[]).await
}

/// [`run_control_command`] with an explicit deadline — the ONE control-command
/// builder shared by the WSL commands and the R11 fit probes, so a probe gets
/// the same hidden / kill-on-drop / scrubbed-environment posture and only the
/// timeout is the caller's (the fit probes outlive the 10 s WSL default only
/// by operator calibration, never by a new spawn path).
pub(super) async fn run_control_command_timeout(
    program: &str,
    args: &[String],
    timeout: Duration,
) -> ControlOutcome {
    run_control_command_with_stdin_timeout(program, args, &[], timeout).await
}

/// [`run_control_command`] with a stdin payload. The ONLY legitimate use is the
/// staging driver: a program that must reach the distro shell *as data*, never as
/// argv — the login-shell re-tokenization GOTCHAS `[ps]` warns about cannot touch
/// bytes that ride a pipe, and no path (Windows or otherwise) ever appears in the
/// command line. Writing the payload to the child's stdin to EOF precedes waiting,
/// so a child that never reads cannot wedge the control timeout into `TimedOut`
/// without also being reaped by `kill_on_drop`.
pub(super) async fn run_control_command_with_stdin(
    program: &str,
    args: &[String],
    stdin: &[u8],
) -> ControlOutcome {
    run_control_command_with_stdin_timeout(program, args, stdin, CONTROL_TIMEOUT).await
}

/// The body behind [`run_control_command`], [`run_control_command_timeout`] and
/// [`run_control_command_with_stdin`]: the ONE place a control [`Command`] is
/// built, with the deadline as the only varying parameter.
async fn run_control_command_with_stdin_timeout(
    program: &str,
    args: &[String],
    stdin: &[u8],
    timeout: Duration,
) -> ControlOutcome {
    let mut command = Command::new(program);
    command
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .kill_on_drop(true)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in control_child_env() {
        command.env(key, value);
    }
    let run = async {
        let mut child = command.spawn().map_err(|error| error.to_string())?;
        let mut pipe = child
            .stdin
            .take()
            .unwrap_or_else(|| unreachable!("invariant: stdin is piped above"));
        // A stdin write that fails because the child already died (a `bash` that
        // rejects the invocation outright) is not the failure to report: the exit
        // status and stderr below name the real one.
        let _ = pipe.write_all(stdin).await;
        drop(pipe);
        child
            .wait_with_output()
            .await
            .map_err(|error| error.to_string())
    };
    match tokio::time::timeout(timeout, run).await {
        Ok(Ok(output)) => ControlOutcome::Ran(output),
        Ok(Err(error)) => ControlOutcome::Failed(error),
        Err(_) => ControlOutcome::TimedOut,
    }
}
