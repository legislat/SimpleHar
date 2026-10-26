//! ModelConf scan (DESIGN Step 9): flat `key: value` yaml files -> typed catalog,
//! with per-file error reports.
//!
//! v3.3 lessons encoded here: quotes and inline comments are handled (D2 — `name:
//! "foo"` and `context: 4096 # 32k` parse correctly), invalid keys are *reported*
//! instead of silently dropped (D3), duplicate keys are reported, and a bad file is
//! skipped and named, never fatal (§0.2 step 6).

use crate::model::ModelEntry;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

/// One scan problem: a file (or `config` for cross-file checks) and the reason.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanError {
    /// File the problem came from, or `"config"` for cross-file checks.
    pub file: String,
    /// Human-readable reason (G13: names the thing, not a stack trace).
    pub reason: String,
}

/// The scan result: every parseable model plus every problem found.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Catalog {
    /// Parsed entries, sorted by harness name for stable listings.
    pub models: Vec<ModelEntry>,
    /// Problems: bad files, bad keys, unresolved `default_model`s.
    pub errors: Vec<ScanError>,
}

impl Catalog {
    /// Empty catalog (a missing ModelConf dir is not fatal — §0.1 "no config still
    /// works").
    pub fn empty() -> Self {
        Self::default()
    }
}

/// Parses one flat yaml document into a key map plus per-line problems.
///
/// Rules: `key: value` with `key` matching `[A-Za-z_][A-Za-z0-9_-]*`; surrounding
/// single/double quotes stripped; ` # ...` inline comments removed outside quotes;
/// blank/comment lines skipped; anything else is reported, never dropped silently.
pub fn parse_flat_yaml(text: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut problems = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = raw_line.trim_start_matches('\u{feff}').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(colon) = line.find(':') else {
            problems.push(format!(
                "line {}: not a `key: value` line: `{line}`",
                index + 1
            ));
            continue;
        };
        let key = line[..colon].trim();
        if key.is_empty()
            || !key
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            problems.push(format!("line {}: invalid key `{key}`", index + 1));
            continue;
        }
        let value = strip_comment(line[colon + 1..].trim());
        if map.contains_key(key) {
            problems.push(format!(
                "line {}: duplicate key `{key}` (first value kept)",
                index + 1
            ));
            continue;
        }
        map.insert(key.to_string(), value);
    }
    (map, problems)
}

/// Removes an inline ` # comment` (outside quotes) and surrounding quote pairs.
fn strip_comment(value: &str) -> String {
    let mut in_quote: Option<char> = None;
    let mut cut = value.len();
    let bytes: Vec<char> = value.chars().collect();
    for (index, window) in bytes.windows(2).enumerate() {
        let (current, next) = (window[0], window[1]);
        match in_quote {
            Some(q) if current == q => in_quote = None,
            Some(_) => {}
            None => {
                if current == '"' || current == '\'' {
                    in_quote = Some(current);
                } else if current == ' ' && next == '#' {
                    cut = index;
                    break;
                }
            }
        }
    }
    let cleaned: String = bytes[..cut].iter().collect();
    let trimmed = cleaned.trim();
    if trimmed.len() >= 2 {
        let first = trimmed.chars().next().expect("non-empty checked");
        let last = trimmed.chars().next_back().expect("non-empty checked");
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return trimmed[1..trimmed.len() - 1].to_string();
        }
    }
    trimmed.to_string()
}

/// Scans a ModelConf directory: every `*.yaml`/`*.yml` becomes an entry or an error;
/// a missing directory is one error, not a panic.
pub fn scan_dir(dir: &Path) -> Catalog {
    let mut catalog = Catalog::empty();
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            catalog.errors.push(ScanError {
                file: dir.display().to_string(),
                reason: format!("cannot read ModelConf dir: {error}"),
            });
            return catalog;
        }
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("yaml") || e.eq_ignore_ascii_case("yml"))
        })
        .collect();
    files.sort();
    let mut seen_containers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for path in files {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                catalog.errors.push(ScanError {
                    file: file_name.clone(),
                    reason: format!("cannot read: {error}"),
                });
                continue;
            }
        };
        let (map, problems) = parse_flat_yaml(&text);
        for problem in &problems {
            catalog.errors.push(ScanError {
                file: file_name.clone(),
                reason: problem.clone(),
            });
        }
        if map.is_empty() && !problems.is_empty() {
            continue; // the line problems already name the file; no redundant report
        }
        match ModelEntry::from_flat(&map) {
            Ok(mut entry) => {
                // One container name may back exactly one model: unload reaps by
                // name, so a duplicate would let either model's release destroy
                // the other's live engine (review, 2026-10-26).
                if entry.engine == "ninfer" {
                    if let Some(name) = map.get("ninfer_container").map(|v| v.trim()) {
                        if !name.is_empty() {
                            if let Some(first) =
                                seen_containers.insert(name.to_string(), file_name.clone())
                            {
                                catalog.errors.push(ScanError {
                                    file: file_name.clone(),
                                    reason: format!(
                                        "duplicate ninfer_container `{name}` (also in {first})"
                                    ),
                                });
                                continue;
                            }
                        }
                    }
                }
                entry.file = Some(path.clone());
                entry.gguf_size_gb = gguf_size_gb(entry.model_ref.as_deref(), dir);
                catalog.models.push(entry);
            }
            Err(error) => {
                catalog.errors.push(ScanError {
                    file: file_name,
                    reason: error.to_string(),
                });
            }
        }
    }
    catalog.models.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// GiB size of the backing GGUF for the admission estimate (DESIGN Step 4 rule 1):
/// recorded at scan (already-blocking file I/O), so the actor never stats during
/// admit. `scheme://` sentinels and unreadable paths yield `None` — the same
/// UnderSpecified refusal as before, never a guessed size.
fn gguf_size_gb(model_ref: Option<&str>, dir: &Path) -> Option<f64> {
    let raw = model_ref?;
    if raw.contains("://") {
        return None;
    }
    let path = Path::new(raw);
    let full = if path.is_relative() {
        dir.join(path)
    } else {
        path.to_path_buf()
    };
    std::fs::metadata(full)
        .ok()
        .map(|meta| meta.len() as f64 / (1024.0_f64).powi(3))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;

    #[test]
    fn quotes_and_inline_comments_are_handled() {
        let (map, problems) =
            parse_flat_yaml("name: \"qwen three\"\ncontext: 4096 # 32k\nengine: 'llama'\nempty:\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.get("name").map(String::as_str), Some("qwen three"));
        assert_eq!(map.get("context").map(String::as_str), Some("4096"));
        assert_eq!(map.get("engine").map(String::as_str), Some("llama"));
        assert_eq!(map.get("empty").map(String::as_str), Some(""));
    }

    #[test]
    fn hash_inside_quotes_is_not_a_comment() {
        let (map, _) = parse_flat_yaml("model: \"F:/a#b.gguf\"\n");
        assert_eq!(map.get("model").map(String::as_str), Some("F:/a#b.gguf"));
    }

    #[test]
    fn invalid_keys_and_duplicates_are_reported_not_dropped() {
        let (_, problems) = parse_flat_yaml("bad-key: 1\ngood: 1\ngood: 2\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("invalid key"));
        assert!(problems[1].contains("duplicate"));
    }

    #[test]
    fn junk_lines_are_reported() {
        let (_, problems) = parse_flat_yaml("just text\n");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("not a `key: value` line"));
    }

    #[test]
    fn scan_dir_collects_models_and_names_bad_files() {
        let dir = std::env::temp_dir().join(format!("se-scan-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("good.yaml"), "name: a\nengine: mock\nvram_gb: 4\n")
            .expect("write");
        std::fs::write(dir.join("bad.yaml"), "oops\n").expect("write");
        std::fs::write(dir.join("ignored.txt"), "name: nope\n").expect("write");

        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "only the yaml counts");
        assert_eq!(catalog.models[0].name, "a");
        assert_eq!(
            catalog.models[0]
                .file
                .as_ref()
                .map(|p| p.file_name().unwrap().to_str().unwrap()),
            Some("good.yaml")
        );
        assert_eq!(catalog.errors.len(), 1);
        assert!(catalog.errors[0].file.contains("bad.yaml"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn duplicate_ninfer_container_names_are_refused() {
        // Unload reaps by name: one container may back exactly one model
        // (review, 2026-10-26).
        let dir = std::env::temp_dir().join(format!("se-scan-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("a.yaml"),
            "name: a\nengine: ninfer\nninfer_autostart: /s.sh\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("b.yaml"),
            "name: b\nengine: ninfer\nninfer_autostart: /t.sh\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("c.yaml"),
            "name: c\nengine: ninfer\nninfer_autostart: /u.sh\nninfer_container: other\n",
        )
        .expect("write");

        let catalog = scan_dir(&dir);
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"a") && names.contains(&"c"), "{names:?}");
        assert!(
            !names.contains(&"b"),
            "the duplicate must be refused: {names:?}"
        );
        assert!(
            catalog
                .errors
                .iter()
                .any(|e| e.file.contains("b.yaml")
                    && e.reason.contains("duplicate ninfer_container")),
            "{:?}",
            catalog.errors
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_dir_is_one_error_not_a_panic() {
        let catalog = scan_dir(Path::new("Z:/definitely-not-here"));
        assert!(catalog.models.is_empty());
        assert!(catalog.errors.len() == 1);
        assert!(catalog.errors[0].reason.contains("cannot read"));
    }

    #[test]
    fn aliases_survive_the_round_trip() {
        let dir = std::env::temp_dir().join(format!("se-scan-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.yaml"), "name: a\nis_embedding_model: true\n").expect("write");
        std::fs::write(dir.join("b.yaml"), "name: b\nsticky: true\n").expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models[0].category, CategoryId::Emb);
        assert_eq!(catalog.models[1].category, CategoryId::Aux);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
