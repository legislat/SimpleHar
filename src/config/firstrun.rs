//! First-start config materialization (operator ruling 2026-10-29).
//!
//! A fresh install has no `simplehar-se.json`; until now it booted on hardcoded
//! defaults, which fossilize THIS machine (notably `vram_total_gb` = 72.0 — this
//! box's card). The ruling: when no config file exists, the harness probes the real
//! system, materializes the file beside the exe, and from then on the file is the
//! truth. Every probe degrades to the documented default with a loud log line —
//! a wedged driver can neither fail boot nor stall the listeners (FINDINGS-14 [1]
//! bind-before-driver doctrine holds: the probe runs on the blocking pool behind a
//! hard budget, and the bind itself happens later, in `run()`).
//!
//! Two guarantees are absolute:
//! - **never overwrite** — the publish step uses `hard_link`, which fails
//!   `AlreadyExists` rather than replacing: a file the operator (or any external
//!   writer) created between the existence checks is kept, byte-for-byte;
//! - **what is written is what runs** — the same in-memory [`AppConfig`] is handed
//!   to the scheduler and the API after filling, and the file is the pretty
//!   serialization of exactly that config (`source_path` is `#[serde(skip)]`),
//!   flushed and read back before it is published (DECISIONS 13[11]).
//!
//! The cross-check warning runs on EVERY boot (file present or not): a pinned
//! `vram_total_gb` that disagrees with the card's own reading beyond
//! `witness_tolerance_gb` is reported, never silently repaired.

use super::AppConfig;
use std::path::{Path, PathBuf};

/// What the probes learned; `None` means "no trustworthy answer" and the field
/// keeps its documented default.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ProbeResults {
    /// Card total in GiB as the witness reports it (floor-rounded to 0.1 — the
    /// conservative direction for an admission ceiling), or `None` when the probe
    /// failed or answered garbage.
    pub vram_total_gb: Option<f64>,
    /// An existing `llama-server.exe` found on PATH or in the known install dir.
    pub llama_server_exe: Option<PathBuf>,
}

/// Floor to one decimal: an admission ceiling must never be rounded up.
#[must_use]
pub const fn floor_gib(gib: f64) -> f64 {
    (gib * 10.0).floor() / 10.0
}

/// A card reading worth trusting: finite and positive. A driver that answers
/// "device present, 0 bytes" must not become a config the next boot dies on
/// (`validate` refuses a non-positive ceiling) — garbage degrades to the default.
#[must_use]
pub const fn trustworthy(gib: f64) -> bool {
    gib.is_finite() && gib > 0.0
}

/// Applies trustworthy probe results to the boot config and returns the applied
/// `(field, value)` pairs for boot logging. Only values that pass [`trustworthy`]
/// (and an existing exe file) change anything; everything else stays at the
/// documented default the operator can read in the README and edit in the file.
#[must_use]
pub fn fill(cfg: &mut AppConfig, results: &ProbeResults) -> Vec<(String, String)> {
    let mut applied = Vec::new();
    if let Some(total) = results.vram_total_gb.filter(|t| trustworthy(*t)) {
        cfg.vram_total_gb = total;
        applied.push(("vram_total_gb".to_string(), format!("{total}")));
    }
    if let Some(exe) = results
        .llama_server_exe
        .as_ref()
        .filter(|exe| exe.is_file())
    {
        cfg.llama_server_exe = exe.clone();
        applied.push(("llama_server_exe".to_string(), exe.display().to_string()));
    }
    applied
}

/// The PATH entries worth searching: non-empty and absolute only. A relative
/// entry (or the empty one) would resolve `llama-server.exe` against the harness
/// cwd, where a planted exe would silently become the engine this box launches.
#[must_use]
pub fn path_entries(path_var: Option<std::ffi::OsString>) -> Vec<PathBuf> {
    path_var
        .as_deref()
        .map(std::env::split_paths)
        .into_iter()
        .flatten()
        .filter(|dir| !dir.as_os_str().is_empty() && dir.is_absolute())
        .collect()
}

/// Pure core of [`detect_llama_server`]: first directory that holds an existing
/// regular file named `llama-server.exe`. Directories with that name are rejected
/// (an `is_file` check, mirroring the strata interpreter rule: the harness must
/// launch a program, not point at a folder).
#[must_use]
pub fn detect_llama_server_in<'a>(dirs: impl IntoIterator<Item = &'a Path>) -> Option<PathBuf> {
    dirs.into_iter()
        .map(|dir| dir.join("llama-server.exe"))
        .find(|candidate| candidate.is_file())
}

/// Searches PATH (filtered by [`path_entries`]), then the documented install
/// default. The result is written once into the created config, so the search
/// order is the operator's first-run engine choice: their PATH install wins over
/// a leftover convention directory.
#[must_use]
pub fn detect_llama_server() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = path_entries(std::env::var_os("PATH"));
    dirs.push(PathBuf::from(r"C:\Inference\llama.cpp"));
    detect_llama_server_in(dirs.iter().map(PathBuf::as_path))
}

/// A pinned ceiling that disagrees with the card's own reading by more than the
/// tolerance is a finding worth a warning: the operator swapped cards, or the
/// first-run probe once failed and left the documented default in the file.
/// Returns `Some((configured, observed))` when they disagree; `None` when within
/// tolerance or when either number is not finite (validation owns garbage).
#[must_use]
pub fn total_mismatch(configured: f64, observed: f64, tolerance: f64) -> Option<(f64, f64)> {
    if !configured.is_finite() || !observed.is_finite() || !tolerance.is_finite() {
        return None;
    }
    if (configured - observed).abs() > tolerance {
        Some((configured, observed))
    } else {
        None
    }
}

/// Creates the config file beside the exe, atomically, **only** if none exists yet.
///
/// `Ok(true)` = the file is on disk, flushed, read back, and matches the config
/// handed in; `Ok(false)` = the target appeared at any point (the writer wins and
/// their bytes are untouched — `hard_link` refuses to replace); `Err` = the file
/// could not be written — the caller boots on in-memory defaults and warns,
/// because a convenience file must never be a boot gate.
///
/// # Errors
///
/// Any I/O failure from the temp write, its read-back verification, the publish,
/// or the post-publish read-back compare.
pub fn ensure_created(cfg: &AppConfig, path: &Path) -> std::io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    let root = serde_json::to_value(cfg).map_err(std::io::Error::other)?;
    let text = serde_json::to_string_pretty(&root).map_err(std::io::Error::other)?;
    // The same gate and unique-temp discipline the API's config write-back uses
    // (FINDINGS-07 [3]); the publish itself refuses to replace (never-overwrite).
    let _gate = crate::model::writeback::acquire_write_gate();
    if path.exists() {
        return Ok(false);
    }
    let tmp = path.with_extension(format!(
        "json.{}.{}.sh.tmp",
        std::process::id(),
        crate::model::writeback::next_temp_seq()
    ));
    crate::model::writeback::write_synced(&tmp, text.as_bytes())?;
    // Verify the bytes on disk before publishing them under the final name
    // (13[11]: the file that claims to be the truth is read back once, here).
    let on_disk = std::fs::read_to_string(&tmp);
    if !matches!(&on_disk, Ok(read) if read == &text) {
        let _ = std::fs::remove_file(&tmp);
        return Err(std::io::Error::other(format!(
            "temp config bytes did not read back intact: {:?}",
            on_disk.err()
        )));
    }
    let published = publish(&tmp, path)?;
    let _ = std::fs::remove_file(&tmp);
    if !published {
        return Ok(false);
    }
    match std::fs::read_to_string(path) {
        Ok(read) if read == text => Ok(true),
        Ok(_) => Err(std::io::Error::other(
            "the published file does not match the bytes this boot ran; it IS on disk \
             under the final name and the next boot will load it — reconcile before trusting it",
        )),
        Err(error) => Err(std::io::Error::other(format!(
            "published config cannot be read back: {error}"
        ))),
    }
}

/// Refuse-to-replace publish: `hard_link` is `create_new` semantics on the final
/// path (an existing target fails `AlreadyExists` — the operator's bytes are kept
/// and the answer is `false`); any other error removes nothing here because the
/// target was never created, and the caller cleans the temp. `tmp` lives beside
/// `path` (`path.with_extension`), so the link is always same-volume.
fn publish(tmp: &Path, path: &Path) -> std::io::Result<bool> {
    match std::fs::hard_link(tmp, path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::testutil::temp_dir;

    #[test]
    fn probe_results_fill_only_where_trustworthy() {
        let mut cfg = AppConfig::default();
        let applied = fill(
            &mut cfg,
            &ProbeResults {
                vram_total_gb: Some(23.6),
                llama_server_exe: None,
            },
        );
        assert_eq!(cfg.vram_total_gb, 23.6, "the detected ceiling must land");
        assert_eq!(
            applied,
            vec![("vram_total_gb".to_string(), "23.6".to_string())]
        );

        let mut untouched = AppConfig::default();
        let expected = untouched.vram_total_gb;
        let applied = fill(&mut untouched, &ProbeResults::default());
        assert!(applied.is_empty(), "a failed probe reports nothing applied");
        assert_eq!(
            untouched.vram_total_gb, expected,
            "defaults survive probe failure"
        );

        // The BLOCKER-1 leg: a driver answering 0 (or garbage) must never reach the
        // file, because the next boot's validate() would kill it over this number.
        for garbage in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let mut guarded = AppConfig::default();
            let applied = fill(
                &mut guarded,
                &ProbeResults {
                    vram_total_gb: Some(garbage),
                    llama_server_exe: None,
                },
            );
            assert!(applied.is_empty(), "{garbage} must be refused by fill");
            assert_eq!(
                guarded.vram_total_gb, expected,
                "{garbage} must not clobber the default"
            );
        }

        // The llama arm: only an existing file fills; a directory or ghost path does not.
        let root = temp_dir("fill-llama");
        let real = root.join("llama-server.exe");
        std::fs::write(&real, b"MZ").expect("write exe");
        let ghost = root.join("nowhere").join("llama-server.exe");
        for candidate in [&ghost, &root] {
            // `root` is a directory wearing the exe name — not a program.
            let mut cfg = AppConfig::default();
            let applied = fill(
                &mut cfg,
                &ProbeResults {
                    vram_total_gb: None,
                    llama_server_exe: Some(candidate.clone()),
                },
            );
            assert!(applied.is_empty(), "{} must not fill", candidate.display());
            assert_eq!(cfg.llama_server_exe, AppConfig::default().llama_server_exe);
        }
        let mut cfg = AppConfig::default();
        let applied = fill(
            &mut cfg,
            &ProbeResults {
                vram_total_gb: None,
                llama_server_exe: Some(real.clone()),
            },
        );
        assert_eq!(applied.len(), 1, "a real exe fills");
        assert_eq!(cfg.llama_server_exe, real);
    }

    #[test]
    fn ceiling_floor_rounds_down_never_up() {
        // 73415 MiB is 71.69... GiB: the admission ceiling must not round up.
        assert_eq!(floor_gib(73_415.0 / 1024.0), 71.6);
        assert_eq!(floor_gib(24.0), 24.0);
        assert!(trustworthy(71.6) && !trustworthy(0.0));
    }

    #[test]
    fn path_entries_rejects_relative_and_empty() {
        let raw = std::env::join_paths(["C:\\real", "D:\\also", "relative", ""])
            .expect("join PATH entries");
        let kept = path_entries(Some(raw));
        assert_eq!(
            kept,
            vec![PathBuf::from(r"C:\real"), PathBuf::from(r"D:\also")],
            "absolute entries survive; relative and empty do not"
        );
        assert!(
            path_entries(None).is_empty(),
            "a missing PATH yields nothing"
        );
    }

    #[test]
    fn llama_probe_requires_a_file_not_a_directory() {
        let root = temp_dir("llama");
        // A directory wearing the exe name is not a program.
        std::fs::create_dir_all(root.join("llama-server.exe")).expect("fake dir");
        assert_eq!(
            detect_llama_server_in(std::iter::once(root.as_path())),
            None,
            "a directory named llama-server.exe must not be picked"
        );
        let good = root.join("good");
        std::fs::create_dir_all(&good).expect("good dir");
        std::fs::write(good.join("llama-server.exe"), b"MZ").expect("write exe");
        assert_eq!(
            detect_llama_server_in([root.as_path(), good.as_path()]),
            Some(good.join("llama-server.exe")),
            "first directory holding a real file wins"
        );
    }

    #[test]
    fn cross_check_reports_only_real_disagreements() {
        assert_eq!(total_mismatch(72.0, 24.0, 2.0), Some((72.0, 24.0)));
        assert_eq!(
            total_mismatch(24.0, 24.5, 2.0),
            None,
            "within tolerance is silence"
        );
        assert_eq!(
            total_mismatch(f64::NAN, 24.0, 2.0),
            None,
            "garbage is validation's job"
        );
        assert_eq!(total_mismatch(72.0, f64::INFINITY, 2.0), None);
    }

    #[test]
    fn created_file_is_the_truth_and_never_overwrites() {
        let root = temp_dir("create");
        let path = root.join("simplehar-se.json");
        let cfg = AppConfig {
            vram_total_gb: 23.6,
            ..AppConfig::default()
        };
        assert!(
            ensure_created(&cfg, &path).expect("create"),
            "fresh file is created"
        );
        let reloaded = AppConfig::load(&path).expect("created file must load");
        assert!(
            reloaded.validate().is_empty(),
            "the created file must boot: {:?}",
            reloaded.validate()
        );
        assert_eq!(
            reloaded.vram_total_gb, 23.6,
            "what was written is what runs"
        );
        let mut expected = cfg.clone();
        expected.source_path = None;
        let mut actual = reloaded.clone();
        actual.source_path = None;
        assert_eq!(actual, expected, "the whole struct round-trips");

        // Never overwrite: the file an operator (or a race) put there survives untouched.
        std::fs::write(&path, "{\"vram_total_gb\": 1.5}").expect("operator edit");
        assert!(
            !ensure_created(&cfg, &path).expect("second call"),
            "an existing file must never be replaced"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "{\"vram_total_gb\": 1.5}",
            "byte-verbatim survival"
        );

        // A missing parent directory is an Err, not a panic, and writes no file.
        assert!(ensure_created(&cfg, &root.join("gone").join("simplehar-se.json")).is_err());
    }

    #[test]
    fn publish_refuses_to_replace_an_existing_target() {
        // Direct pin of the MEDIUM-1 mechanism: the `AlreadyExists` arm is reached
        // only when the target appears between the pre-check and the hard_link, so
        // the test constructs exactly that — a target that already exists when
        // publish runs. Without the arm, hard_link would error and the caller's
        // whole-suite-green would be a lie (GOTCHAS pass-both-ways).
        let root = temp_dir("publish");
        let target = root.join("simplehar-se.json");
        std::fs::write(&target, "OPERATOR BYTES").expect("pre-existing target");
        let tmp = root.join("staging.tmp");
        std::fs::write(&tmp, "HARNESS BYTES").expect("temp");
        assert!(
            !publish(&tmp, &target).expect("publish"),
            "an existing target must yield false, not replace"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read"),
            "OPERATOR BYTES",
            "the operator's bytes survive the refused publish"
        );

        // The happy arm: no target -> hard link succeeds and reports true.
        let fresh = root.join("fresh.json");
        let tmp2 = root.join("staging2.tmp");
        std::fs::write(&tmp2, "bytes").expect("temp2");
        assert!(
            publish(&tmp2, &fresh).expect("publish fresh"),
            "no target: publish true"
        );
        assert_eq!(std::fs::read_to_string(&fresh).expect("read"), "bytes");
    }
}
