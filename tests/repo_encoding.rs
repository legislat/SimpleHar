//! Encoding tripwire: no scanned source file may contain a character from the
//! Cyrillic Unicode block (U+0400-U+04FF). A double-encoded UTF-8 byte pair
//! decodes to such characters, so the block is a reliable mojibake marker.
//! Local record directories are not scanned because they are not pinned.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

/// Extensions the tripwire scans. Anything else is not source text here.
const SCAN_EXTENSIONS: [&str; 11] = [
    "rs", "js", "mjs", "css", "html", "md", "yaml", "yml", "ps1", "toml", "json",
];

/// Directory names the walk never enters.
const SKIPPED_DIRS: [&str; 5] = ["target", ".git", ".kilo", "node_modules", "handoff"];

/// Top-level directories the walk starts from.
const SCAN_DIRS: [&str; 5] = ["src", "tests", "scripts", "docs", "ModelConf"];

fn is_scanned_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| SCAN_EXTENSIONS.contains(&ext))
}

/// Appends every scannable file under `dir` to `files`. Missing directories
/// contribute nothing, so an absent optional tree never breaks the test.
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
        } else if is_scanned_extension(&path) {
            files.push(path);
        }
    }
}

fn collect_top_level_files(root: &Path, files: &mut Vec<PathBuf>) {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap_or(OsStr::new(""));
        let is_markdown = name.to_string_lossy().ends_with(".md");
        let is_manifest = name == OsStr::new("Cargo.toml");
        if is_markdown || is_manifest {
            files.push(path);
        }
    }
}

fn cyrillic_hits(file: &Path, display: &Path) -> Vec<String> {
    let bytes =
        fs::read(file).unwrap_or_else(|err| panic!("cannot read {}: {err}", file.display()));
    let text = String::from_utf8_lossy(&bytes);
    let mut hits = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.chars().any(|c| ('\u{0400}'..='\u{04FF}').contains(&c)) {
            let trimmed: String = line.trim().chars().take(120).collect();
            hits.push(format!("{}:{}: {}", display.display(), index + 1, trimmed));
        }
    }
    hits
}

#[test]
fn no_double_encoded_utf8_in_source_tree() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    for dir in SCAN_DIRS {
        collect_files(&root.join(dir), &mut files);
    }
    collect_top_level_files(&root, &mut files);
    files.sort();
    files.dedup();

    let mut violations: Vec<String> = Vec::new();
    for file in &files {
        let relative = file.strip_prefix(&root).unwrap_or(file.as_path());
        for hit in cyrillic_hits(file, relative) {
            violations.push(hit);
        }
    }

    assert!(
        violations.is_empty(),
        "found {} Cyrillic-block character line(s) (U+0400-U+04FF):\n{}\n\
         hint: double-encoded UTF-8 (mojibake); write files with UTF-8 without BOM",
        violations.len(),
        violations.join("\n")
    );
}
