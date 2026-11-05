//! The log-file half of process supervision: where `logs/` lives, the three
//! log classes, and the per-model and harness-boot-log retention pruners (R9).

use std::path::PathBuf;

/// The directory every harness log file lands in: `logs/` beside the current
/// executable, falling back to the working directory. Shared by the captured engine
/// children and [`crate::observability`]'s boot log, so the two can never disagree
/// about where the operator's `logs/` lives.
#[must_use]
pub fn harness_logs_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("logs")))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// Whether a path is the harness's own boot log rather than a captured engine child.
/// Matched on the file stem, so the rolled `harness-<epoch>.log.1`, `.log.2`, … (whose
/// extension is a generation number) still count as harness logs and cannot leak out of
/// the harness budget into the engine one.
pub(super) fn is_harness_log(path: &std::path::Path) -> bool {
    path.file_stem()
        .and_then(|stem| stem.to_str())
        .is_some_and(|stem| stem.starts_with(crate::observability::HARNESS_LOG_PREFIX))
}

pub(super) fn is_engine_log(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()) == Some("log") && !is_harness_log(path)
}

/// Deletes the oldest files beyond `keep` in `dir` that `wants` claims. `kind` names
/// them in the one warning this can emit, so a failure says which budget broke.
fn prune_matching(
    dir: &std::path::Path,
    keep: usize,
    kind: &str,
    wants: impl Fn(&std::path::Path) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| wants(path))
        .filter_map(|path| {
            let modified = std::fs::metadata(&path)
                .and_then(|meta| meta.modified())
                .ok()?;
            Some((modified, path))
        })
        .collect();
    if files.len() <= keep {
        return;
    }
    files.sort_by_key(|(modified, _)| *modified); // oldest first
    let excess = files.len() - keep;
    for (_, path) in files.into_iter().take(excess) {
        if let Err(error) = std::fs::remove_file(&path) {
            tracing::warn!(error = %error, path = %path.display(), "cannot prune old {kind} log");
        }
    }
}

/// Whether a path is one of the R9 container-log snapshots (`<model>-<epoch>.container.log`),
/// as opposed to a client capture (`<model>-<epoch>.log`).
fn is_container_log(path: &std::path::Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.ends_with(".container.log"))
        && !is_harness_log(path)
}

/// Per-model forensic retention (R9): keep the newest `keep` client captures **and**
/// the newest `keep` container snapshots for one model, deleting the older ones of
/// each class. Scoped to one model so a swap-heavy model can never evict another
/// model's forensic log — the defect of the old global budget. The scope is the exact
/// minted shape `{model}-{epoch}[.container].log` — epoch all digits — NOT a bare
/// `{model}-` prefix: model names legally contain `-`, so a prefix match let model
/// `swift`'s keep-N claim (and delete) `swift-7b`'s files. Harness boot logs are
/// excluded from both classes by [`is_harness_log`]. The caller passes the ONE
/// forensic pool key — [`sanitize_slug`] of the model, the same stem both mint
/// sites use (R2-08-03) — never a raw name or a private spelling.
pub(super) fn prune_model_logs(dir: &std::path::Path, prefix: &str, keep: usize) {
    let owned_prefix = format!("{prefix}-");
    let matches_model = |path: &std::path::Path| {
        path.file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| {
                let Some(rest) = name.strip_prefix(owned_prefix.as_str()) else {
                    return false;
                };
                // `{epoch}.log` or `{epoch}.container.log`, epoch all digits.
                let epoch = rest
                    .strip_suffix(".container.log")
                    .or_else(|| rest.strip_suffix(".log"))
                    .unwrap_or(rest);
                !epoch.is_empty() && epoch.chars().all(|c| c.is_ascii_digit())
            })
    };
    prune_matching(dir, keep, "engine", |path| {
        is_engine_log(path) && !is_container_log(path) && matches_model(path)
    });
    prune_matching(dir, keep, "container", |path| {
        is_container_log(path) && matches_model(path)
    });
}

/// Keeps the newest `keep` harness boot logs (rotations included). Called by
/// [`crate::observability::attach_file_sink`] at boot.
pub fn prune_harness_log_files(dir: &std::path::Path, keep: usize) {
    prune_matching(dir, keep, "harness", is_harness_log);
}
