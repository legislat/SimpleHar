//! TASK-NAMING-CLEANUP tripwire: one name everywhere. No code-side scope file
//! (`src`, `tests`, `scripts`, `examples`, `.github`) may carry an older
//! product spelling (TASK-NAMING-CLEANUP, operator ruling 2026-11-05).
//!
//! Two occurrence classes stay legal:
//! - Legacy machine-path bans: names of the operator's old install, backup,
//!   staging and checkout directories. They are publication blocklist and
//!   exemption patterns (they must keep catching leaks of those paths), not
//!   product names — each lives in a script blocklist line that carries one
//!   of those directory shapes.
//! - Lines marked `NAMING-CLEANUP-EXEMPT`: pins that materialize an older
//!   spelling precisely to prove it is inert.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// The scanned scope directories: the code side of the rename.
const SCAN_DIRS: [&str; 5] = ["src", "tests", "scripts", "examples", ".github"];

/// Extensions the tripwire scans. Anything else is not code-side source text.
const SCAN_EXTENSIONS: [&str; 11] = [
    "rs", "js", "mjs", "css", "html", "md", "yaml", "yml", "ps1", "json", "toml",
];

/// Directory names the walk never enters.
const SKIPPED_DIRS: [&str; 2] = ["target", "node_modules"];

/// The line marker that exempts a legacy-name pin (see module docs).
const EXEMPT_MARKER: &str = "NAMING-CLEANUP-EXEMPT";

/// Assembled at runtime so this file carries no contiguous older spelling.
fn legacy_stem() -> String {
    ["simple", "har"].concat()
}

/// Lowercase line fragments that mark a security or publication machine-path
/// ban: the old install/backup/staging/checkout directory names. They keep
/// their text on purpose so the blocklists still catch leaks.
fn machine_path_ban_fragments() -> Vec<String> {
    let stem = legacy_stem();
    vec![
        format!("{stem}-se-backups"),
        format!("{stem}-se_release"),
        format!("{stem}-se_test"),
        format!(r"c:\\inference\\{stem}-se"),
        format!(r"c:/inference/{stem}-se"),
    ]
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if SKIPPED_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files);
        } else if path
            .extension()
            .and_then(OsStr::to_str)
            .is_some_and(|ext| SCAN_EXTENSIONS.contains(&ext))
        {
            files.push(path);
        }
    }
}

/// Returns the offending excerpts of one line: every case-insensitive
/// contiguous occurrence of the old stem that is not the current name, the
/// machine-path bans, or an exempted pin line.
fn offending_occurrences(line: &str) -> Vec<String> {
    let lower = line.to_lowercase();
    let stem = legacy_stem();
    let mut hits = Vec::new();
    if line.contains(EXEMPT_MARKER) {
        return hits;
    }
    let ban_fragments = machine_path_ban_fragments();
    let line_is_machine_path_ban = ban_fragments
        .iter()
        .any(|fragment| lower.contains(fragment));
    let mut from = 0;
    while let Some(offset) = lower[from..].find(&stem) {
        let at = from + offset;
        let after = &lower[at + stem.len()..];
        let is_current = after.starts_with('n');
        let is_machine_path = line_is_machine_path_ban && after.starts_with("-se");
        if !is_current && !is_machine_path {
            let excerpt: String = line.chars().take(120).collect();
            hits.push(excerpt.trim().to_string());
            break;
        }
        from = at + stem.len();
    }
    hits
}

#[test]
fn no_older_product_spelling_survives_in_the_code_side() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in SCAN_DIRS {
        collect_files(&root.join(dir), &mut files);
    }
    files.sort();
    files.dedup();
    assert!(
        files.len() > 80,
        "the scan must actually walk the tree, found {} files",
        files.len()
    );

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let bytes =
            fs::read(file).unwrap_or_else(|err| panic!("cannot read {}: {err}", file.display()));
        let text = String::from_utf8_lossy(&bytes);
        let relative = file.strip_prefix(&root).unwrap_or(file.as_path());
        for (index, line) in text.lines().enumerate() {
            for excerpt in offending_occurrences(line) {
                violations.push(format!("{}:{}: {}", relative.display(), index + 1, excerpt));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "found {} line(s) carrying an older product spelling outside the machine-path \
         bans and the marked inertness pins:\n{}",
        violations.len(),
        violations.join("\n")
    );
}

#[test]
fn the_current_names_are_present_where_the_wire_mutex_and_roots_live() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let read =
        |rel: &str| fs::read_to_string(root.join(rel)).unwrap_or_else(|err| panic!("{rel}: {err}"));

    let api = read("src/api/mod.rs");
    assert!(
        api.contains("simpleharn_error"),
        "the wire error type must carry the current name"
    );
    let render = read("src/api/status_render.rs");
    assert_eq!(
        render.matches(r#""owned_by": "simpleharn""#).count(),
        2,
        "both owned_by renderings must carry the current name"
    );
    let win32 = read("src/process/win32.rs");
    assert!(
        win32.contains(r#"w!("SimpleHarn-SingleInstance")"#),
        "the single-instance mutex must carry the current name"
    );
    let policy = read("src/engine/policy.rs");
    for root_name in [
        ".simpleharn/autostart",
        "$HOME/.simpleharn/keys",
        ".simpleharn/strata-profiles",
        ".simpleharn/basalt-profiles",
    ] {
        assert!(
            policy.contains(root_name),
            "custody root {root_name} must carry the current name"
        );
    }
    let config = read("src/config/mod.rs");
    assert!(
        config.contains("pub const CONFIG_FILE: &str = \"simpleharn.json\";"),
        "the config file constant must name the one current file"
    );
    for name in ["SIMPLEHARN_TOKEN", "SIMPLEHARN_CONFIG", "SIMPLEHARN_LOG"] {
        assert!(
            config.contains(&format!("std::env::var(\"{name}\")")),
            "{name} must be read as the current name"
        );
    }
}
