//! ModelConf scan (DESIGN Step 9): flat `key: value` yaml files -> typed catalog,
//! with per-file error reports.
//!
//! v3.3 lessons encoded here: quotes and inline comments are handled (D2 — `name:
//! "foo"` and `context: 4096 # 32k` parse correctly), and a file with problem lines
//! is never published; its lines are named (D3, operator doctrine: "better not
//! publish anything blindly"), while the scan itself stays non-fatal (§0.2 step 6).
//!
//! Catalog-trust rules (FINDINGS-13 [2]/[3]/[7]/[8]): identity is the yaml FILE
//! STEM — a `name:` key is legacy-optional and only legal when it equals the stem,
//! because that value is the one component of a yaml that reaches a filesystem path
//! (the engine log name); a llama entry whose local `model:` file is gone, and a
//! strata entry whose profile JSON is unreadable or references a missing absolute
//! path, are refused here with the file named, before admission can evict a resident
//! engine for a doomed spawn; and a key outside [`CONSUMED_KEYS`] is reported
//! (warn-only — README doctrine: unknown keys stay "preserved for reference").
//! This module stays log-free: the actor reports every [`ScanError`] it applies.

use crate::model::ModelEntry;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

#[cfg(test)]
use std::path::PathBuf;

/// What kind of problem a [`ScanError`] reports. The page and the actor's `tracing`
/// lines triage on this; `reason` stays the human sentence (G13). Closed enum:
/// every construction site names one of these, and a new report shape must add a
/// variant so the exhaustiveness audit happens at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanKind {
    /// The model's backing file — or a path its strata profile references — is not
    /// on disk; the entry was dropped.
    ModelPathMissing,
    /// A strata profile JSON could not be read or parsed; the entry was dropped.
    ProfileUnreadable,
    /// A `name:` key contradicts the file stem, or the stem is empty; dropped.
    InvalidName,
    /// A configured engine binary does not exist (a `config` cross-check, raised by
    /// the actor's `apply_catalog`, not by the scan itself).
    EngineBinaryMissing,
    /// The entry names an engine this harness has no adapter for; dropped by the
    /// actor (the scan cannot see the adapter map).
    EngineUnavailable,
    /// A yaml key outside [`CONSUMED_KEYS`] (and outside [`SILENT_KEYS`], the one
    /// reserved forensic class that stays silent by operator ruling); warn-only,
    /// the entry still publishes.
    UnknownKey,
    /// A yaml file carried at least one problem line (Fork C doctrine): the entry
    /// was **never** published and the `reason` names every offending line.
    InvalidYaml,
    /// Every pre-existing report shape: unreadable files or dirs, typed field
    /// rejects, duplicate containers, unresolved `default_model`s.
    Other,
}

/// One scan problem: a file (or `config` for cross-file checks) and the reason.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScanError {
    /// What kind of problem this is (page/triage key).
    pub kind: ScanKind,
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

/// The canonical set of yaml keys the harness reads anywhere: scan identity, the
/// typed entry, the llama flag table and its extra reads, the strata and ninfer
/// adapters, and the measured triple. This const is THE source of truth for the
/// unknown-key report: a key outside it is published with a warn-only
/// [`ScanKind::UnknownKey`] (entry stays loadable — README: "preserved for
/// reference"). The `consumed_keys_cover_every_key_the_code_reads` test fails when
/// the FLAG_TABLE or a documented engine-specific read is added without joining
/// this list, and the golden-yaml test fails when a member here stops being read.
const CONSUMED_KEYS: &[&str] = &[
    "name",
    "engine",
    "model",
    "category",
    "is_embedding_model",
    "vram_gb",
    "cost",
    "launch_only_with_at_least_vram_free",
    "load_timeout",
    "serve_model",
    "cpu_offload",
    "jinja",
    "extra_args",
    "mmproj",
    "context",
    "ngl",
    "batch",
    "ubatch",
    "flash_attn",
    "cache_type_k",
    "cache_type_v",
    "parallel",
    "fit",
    "fit_target",
    "fit_ctx",
    "measured_gb",
    "measured_gb_min",
    "measured_offload",
    "strata_config",
    "strata_python",
    "strata_cwd",
    "ninfer_autostart",
    "ninfer_container",
    "ninfer_profile",
    "ninfer_key",
    "external",
];

/// Reserved forensic keys the operator hand-writes into a yaml: one-shot snapshot
/// notes and relevant comments, for the forensic manual only (operator ruling,
/// 2026-10-28, on the `bak:` node). The harness never reads them, never warns
/// about them, never logs them, and never surfaces them: they are stripped from
/// the parsed raw map in [`crate::model::ModelEntry::from_flat`] exactly like the
/// retired `external`, so the value can neither reach the page nor be re-emitted
/// by write-back — the operator's file bytes are never altered. Deliberately NOT
/// members of [`CONSUMED_KEYS`]: membership there claims a real read, and the
/// golden-yaml test enforces that every member is actually read.
pub(crate) const SILENT_KEYS: &[&str] = &["bak"];

/// The `host:` key is inert by policy, not by accident (FINDINGS-13 [8]): the
/// harness forces the loopback bind itself (FINDINGS-09 [1], §5).
const HOST_POLICY_HINT: &str =
    "engine host is harness-forced (loopback, \u{a7}5) \u{2014} this key does nothing";

/// Same for `port:` — allocated from the category config, never self-chosen.
const PORT_POLICY_HINT: &str =
    "engine port is harness-assigned from category config \u{2014} this key does nothing";

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
            || !key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
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

/// The 13[5] Fork B acceptance gate: a config is exactly a **regular file** whose
/// final extension is `.yaml`, case-insensitively. Never a symlink — the caller
/// passes `DirEntry::file_type()`, which does not follow the link, so a symlink
/// (to a file, a directory, a pipe or a linked tree) is judged as itself: inert,
/// never opened, never stat'd. `.yml` is **deliberately not a config extension**
/// (repo convention): `foo.yml` is inert-not-broken — it earns no error, and
/// renaming it to `.yaml` is the operator's call, not the harness's. Everything
/// else in ModelConf (`README.md`, the single-slot `.yaml.bak` incident copies,
/// `x.yaml.bak`, `Thumbs.db`, even a directory named `x.yaml`) is silently inert:
/// non-candidates are not configs, so they produce **no catalog_error** — the
/// Fork C InvalidYaml voice stays reserved for actual `*.yaml` files.
fn is_accepted_config(file_type: &std::fs::FileType, path: &Path) -> bool {
    file_type.is_file()
        && path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("yaml"))
}

/// Scans a ModelConf directory: every regular (non-symlink) `*.yaml` file becomes
/// an entry or an error; `.yml` and every other name is **silently inert** (the
/// 13[5] Fork B acceptance gate, [`is_accepted_config`]); a missing directory is
/// one error, not a panic.
pub fn scan_dir(dir: &Path) -> Catalog {
    let mut catalog = Catalog::empty();
    // Test-only hang seam: parks this scan (and only a scan of the armed directory)
    // until the test releases it, so a rescan can be held in flight to prove the
    // actor never waits on it. Production never arms it and pays one lock-free skip.
    #[cfg(test)]
    {
        let pause = {
            let mut guard = scan_pause().lock().expect("scan pause lock");
            if guard.as_ref().is_some_and(|p| p.dir == dir) {
                guard.take()
            } else {
                None
            }
        };
        if let Some(pause) = pause {
            let _ = pause.started.send(());
            let _ = pause.release.recv();
        }
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            catalog.errors.push(ScanError {
                kind: ScanKind::Other,
                file: dir.display().to_string(),
                reason: format!("cannot read ModelConf dir: {error}"),
            });
            return catalog;
        }
    };
    let mut files: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            // The acceptance gate reads `DirEntry::file_type()` — never
            // `metadata()`, so a symlink is judged as itself and never followed
            // (or opened): links, pipes and linked trees are inert, not hung.
            entry
                .file_type()
                .ok()
                .is_some_and(|file_type| is_accepted_config(&file_type, &entry.path()))
        })
        .map(|entry| entry.path())
        .collect();
    files.sort();
    let mut seen_containers: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    for path in files {
        let file_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        // Identity is the file stem (FINDINGS-13 [3]/[7]): the stem is the only
        // name-shaped value that reaches a `File::create` path as `log_name`, and
        // tying identity to the file is what makes a same-name twin unconstructable.
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if stem.trim().is_empty() {
            catalog.errors.push(ScanError {
                kind: ScanKind::InvalidName,
                file: file_name,
                reason: "file stem is empty; identity is the file stem \u{2014} rename the file"
                    .to_string(),
            });
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                catalog.errors.push(ScanError {
                    kind: ScanKind::Other,
                    file: file_name.clone(),
                    reason: format!("cannot read: {error}"),
                });
                continue;
            }
        };
        let (map, problems) = parse_flat_yaml(&text);
        if !problems.is_empty() {
            // Fork C doctrine (operator ruling): a file with ANY problem line is
            // never published — "better not publish anything blindly". One report
            // names the file and every offending line; the empty-map case (every
            // line bad) folds into the same shape, so the file is still named.
            catalog.errors.push(ScanError {
                kind: ScanKind::InvalidYaml,
                file: file_name.clone(),
                reason: format!(
                    "yaml not loaded \u{2014} problem lines: {}",
                    problems.join("; ")
                ),
            });
            continue;
        }
        if let Some(claimed) = map.get("name") {
            if claimed != &stem {
                catalog.errors.push(ScanError {
                    kind: ScanKind::InvalidName,
                    file: file_name.clone(),
                    reason: format!(
                        "name: \"{claimed}\" does not match file {file_name} \u{2014} identity \
                         is the file stem; rename the file or delete the key"
                    ),
                });
                continue;
            }
        }
        match ModelEntry::from_flat(&map, &stem) {
            Ok(mut entry) => {
                // One container name may back exactly one model: unload reaps by
                // name, so a duplicate would let either model's release destroy
                // the other's live engine (review, 2026-10-26).
                if entry.engine == crate::ownership::NINFER_ENGINE {
                    if let Some(name) = map.get("ninfer_container").map(|v| v.trim()) {
                        if !name.is_empty() {
                            if let Some(first) =
                                seen_containers.insert(name.to_string(), file_name.clone())
                            {
                                catalog.errors.push(ScanError {
                                    kind: ScanKind::Other,
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
                // Existence traces (FINDINGS-13 [2]): the stat the size needs is
                // the same stat that proves the file is there — take it once and
                // refuse a llama entry launched against nothing, before admission
                // can evict a resident for the doomed spawn. `scheme://` sentinels
                // and every other engine are exempt (their checks live elsewhere).
                let local = entry
                    .model_ref
                    .as_deref()
                    .filter(|r| !r.contains("://"))
                    .map(|raw| {
                        let path = Path::new(raw);
                        if path.is_relative() {
                            dir.join(path)
                        } else {
                            path.to_path_buf()
                        }
                    });
                let stat = local.as_ref().and_then(|p| std::fs::metadata(p).ok());
                if entry.engine == "llama" && stat.is_none() {
                    if let Some(missing) = &local {
                        catalog.errors.push(ScanError {
                            kind: ScanKind::ModelPathMissing,
                            file: file_name.clone(),
                            reason: format!("model file not found: {}", missing.display()),
                        });
                        continue;
                    }
                }
                entry.gguf_size_gb = stat.map(|meta| meta.len() as f64 / (1024.0_f64).powi(3));
                // A strata entry is its profile JSON: unreadable JSON is a dead
                // load, and an absolute path the profile names and the disk lacks
                // is the same doomed spawn one layer deeper. Relative profile
                // paths are exempt (the engine resolves them against the profile
                // dir — chunk 10 backlog); the `log` key is exempt entirely (a log
                // target's existence is not a load precondition).
                if entry.engine == "strata" {
                    if let Some(profile) = map
                        .get("strata_config")
                        .map(|v| v.trim())
                        .filter(|v| !v.is_empty())
                    {
                        let unreadable = || ScanError {
                            kind: ScanKind::ProfileUnreadable,
                            file: file_name.clone(),
                            reason: format!("strata profile not readable: {profile}"),
                        };
                        let Ok(text) = std::fs::read_to_string(profile) else {
                            catalog.errors.push(unreadable());
                            continue;
                        };
                        // A UTF-8 BOM is ignored, not refused (same doctrine as
                        // `parse_flat_yaml`; RFC 8259 permits it, and the strata
                        // engine reads BOM'd profiles today — the live
                        // `strata-qwen-gptq-nvfp4.json` carries one).
                        let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
                        let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
                            catalog.errors.push(unreadable());
                            continue;
                        };
                        if let Some((key, missing)) = missing_profile_path(&root) {
                            catalog.errors.push(ScanError {
                                kind: ScanKind::ModelPathMissing,
                                file: file_name.clone(),
                                reason: format!(
                                    "strata profile references missing path: {key} = {missing}"
                                ),
                            });
                            continue;
                        }
                    }
                }
                // Unknown keys are reported, never fatal (README doctrine), but
                // never silent: an operator's typo'd `contex:` is a config that
                // does nothing, and the page must be able to say so. The one
                // silent class is [`SILENT_KEYS`] — reserved forensic keys
                // ("forensic manual only", operator ruling), which are neither
                // warned nor logged nor surfaced.
                for key in map.keys() {
                    if SILENT_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    if !CONSUMED_KEYS.contains(&key.as_str()) {
                        let reason = match key.as_str() {
                            "host" => HOST_POLICY_HINT.to_string(),
                            "port" => PORT_POLICY_HINT.to_string(),
                            other => format!(
                                "unknown key `{other}` is not read by the harness (preserved \
                                 for reference){}",
                                did_you_mean(other)
                            ),
                        };
                        catalog.errors.push(ScanError {
                            kind: ScanKind::UnknownKey,
                            file: file_name.clone(),
                            reason,
                        });
                    }
                }
                entry.file = Some(path.clone());
                catalog.models.push(entry);
            }
            Err(error) => {
                catalog.errors.push(ScanError {
                    kind: ScanKind::Other,
                    file: file_name,
                    reason: error.to_string(),
                });
            }
        }
    }
    catalog.models.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
}

/// The first (key, path) a strata profile value references that does not exist.
/// Every string under every key is judged; the `log` key is exempt (its target's
/// existence is irrelevant to loading). A trailing `:float` is stripped first:
/// llama's `--control-vector-scaled <path>:1.0` shape carries the scale inside the
/// same word, and it is the path — not `path:1.0` — that must exist.
fn missing_profile_path(root: &serde_json::Value) -> Option<(String, String)> {
    walk_profile(root, "profile")
}

fn walk_profile(value: &serde_json::Value, key: &str) -> Option<(String, String)> {
    match value {
        serde_json::Value::Object(map) => map.iter().find_map(|(inner_key, inner)| {
            if inner_key == "log" {
                None
            } else {
                walk_profile(inner, inner_key)
            }
        }),
        serde_json::Value::Array(items) => items.iter().find_map(|inner| walk_profile(inner, key)),
        serde_json::Value::String(text) => profile_path_candidate(text)
            .filter(|path| std::fs::metadata(path).is_err())
            .map(|path| (key.to_string(), path)),
        _ => None,
    }
}

/// The absolute-path shape this check judges: a Windows drive path (`X:\` or
/// `X:/`) or a UNC (`\\server\share`), never a relative path (exempt: the engine
/// resolves those against the profile dir). A trailing `:float` is the llama
/// control-vector scale and is stripped before the existence check.
fn profile_path_candidate(text: &str) -> Option<String> {
    if !is_absolute_shape(text) {
        return None;
    }
    if let Some((head, tail)) = text.rsplit_once(':') {
        if is_absolute_shape(head) && tail.parse::<f64>().is_ok() {
            return Some(head.to_string());
        }
    }
    Some(text.to_string())
}

/// Drive-letter or UNC absolute-path shape (checked bytewise: the grammar is ASCII).
fn is_absolute_shape(text: &str) -> bool {
    let bytes = text.as_bytes();
    (bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/'))
        || text.starts_with("\\\\")
}

/// The did-you-mean tail of an unknown-key report: only when exactly one consumed
/// key sits one edit away, so the advice is a fact, never a coin flip. The
/// candidate list is [`CONSUMED_KEYS`] alone, so a [`SILENT_KEYS`] member can
/// never surface as a suggestion — reserved-forensic silence means no breadcrumb
/// either.
fn did_you_mean(key: &str) -> String {
    let mut candidate = String::new();
    let mut count = 0usize;
    for consumed in CONSUMED_KEYS {
        if within_one_edit(key, consumed) {
            count += 1;
            candidate = (*consumed).to_string();
        }
    }
    if count == 1 {
        format!(" (did you mean '{candidate}'?)")
    } else {
        String::new()
    }
}

/// True when `a` and `b` differ by at most one insertion, deletion, or
/// substitution. Keys are ASCII by construction (`parse_flat_yaml`'s charset), so
/// a byte walk is exact here.
fn within_one_edit(a: &str, b: &str) -> bool {
    if a == b {
        return false; // not a typo, the key itself
    }
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (long, short) = if a.len() >= b.len() { (a, b) } else { (b, a) };
    if long.len() - short.len() > 1 {
        return false;
    }
    let (mut i, mut j, mut mismatched) = (0usize, 0usize, false);
    while i < long.len() && j < short.len() {
        if long[i] == short[j] {
            i += 1;
            j += 1;
            continue;
        }
        if mismatched {
            return false;
        }
        mismatched = true;
        if long.len() == short.len() {
            i += 1;
            j += 1;
        } else {
            i += 1;
        }
    }
    true
}

/// Test-only: parks `scan_dir` for exactly one armed directory (signal on entry,
/// block until release) so a scheduler unit test can hold a rescan in flight and
/// prove the actor keeps answering. Production never arms it: one uncontended
/// mutex peek is the entire cost.
#[cfg(test)]
pub(crate) struct ScanPause {
    dir: PathBuf,
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
}

#[cfg(test)]
fn scan_pause() -> &'static std::sync::Mutex<Option<ScanPause>> {
    static PAUSE: std::sync::OnceLock<std::sync::Mutex<Option<ScanPause>>> =
        std::sync::OnceLock::new();
    PAUSE.get_or_init(|| std::sync::Mutex::new(None))
}

/// Arms the pause for `dir`; returns (started, release) ends. The next `scan_dir`
/// call for exactly that directory signals `started` and parks until the test
/// drops or sends on the release end; the seam then disarms itself.
#[cfg(test)]
pub(crate) fn arm_scan_pause(
    dir: &Path,
) -> (std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *scan_pause().lock().expect("scan pause lock") = Some(ScanPause {
        dir: dir.to_path_buf(),
        started: started_tx,
        release: release_rx,
    });
    (started_rx, release_tx)
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
        // FINDINGS-13 [9]: `-` is a *valid* key character (the documented charset
        // wins), so the invalid-key half needs a genuinely illegal shape — a space.
        let (map, problems) = parse_flat_yaml("bad key: 1\ngood: 1\ngood: 2\ngpu-layers: 99\n");
        assert_eq!(problems.len(), 2, "{problems:?}");
        assert!(problems[0].contains("invalid key"));
        assert!(problems[1].contains("duplicate"));
        assert!(map.contains_key("gpu-layers"), "a hyphenated key parses");
    }

    /// FINDINGS-13 [9], both directions pinned: the parser accepts exactly the
    /// charset its doc comment promises — `[A-Za-z_][A-Za-z0-9_-]*` — and rejects
    /// everything else, so neither side of the contract can drift again.
    #[test]
    fn the_parser_accepts_exactly_the_documented_key_charset() {
        // The doc claim, literally: every member of the documented classes parses
        // without a problem line.
        let (map, problems) =
            parse_flat_yaml("a: 1\nZ: 1\n_: 1\nA0: 1\na0-Z_9-x-y: 1\nwith-hyphen: 1\nx-a-b-c: 1\n");
        assert!(problems.is_empty(), "{problems:?}");
        for key in ["a", "Z", "_", "A0", "a0-Z_9-x-y", "with-hyphen", "x-a-b-c"] {
            assert!(map.contains_key(key), "`{key}` must parse");
        }
        // And the classes the doc excludes are still reported, never accepted.
        let (_, problems) =
            parse_flat_yaml("9lead: 1\nwith.dot: 1\nwith space: 1\n-hyphenfirst: 1\n");
        assert_eq!(problems.len(), 4, "{problems:?}");
        assert!(
            problems.iter().all(|p| p.contains("invalid key")),
            "{problems:?}"
        );
    }

    /// FINDINGS-13 [9] + Fork C boundary: a hyphenated key is a *key*, not a
    /// problem line — an unconsumed one publishes with the UnknownKey warn.
    /// (`cache_type_k` uses an underscore, so `gpu-layers` is a genuinely unknown
    /// hyphenated key.)
    #[test]
    fn a_hyphenated_key_is_a_key_not_a_problem_line() {
        let dir = crate::testutil::temp_dir("scan-hyphen-key");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("hp.yaml"),
            "engine: mock\nvram_gb: 2\ngpu-layers: 99\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            1,
            "a hyphenated key never blocks publication: {:?}",
            catalog.errors
        );
        assert!(
            !catalog
                .errors
                .iter()
                .any(|e| e.kind == ScanKind::InvalidYaml),
            "the hyphenated key must not be a problem line: {:?}",
            catalog.errors
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::UnknownKey)
            .expect("an unconsumed hyphenated key is warn-only, like any unknown key");
        assert!(error.reason.contains("gpu-layers"), "{}", error.reason);
        assert!(
            catalog.models[0].raw.contains_key("gpu-layers"),
            "README: unknown keys stay preserved for reference"
        );
    }

    /// Operator ruling 2026-10-28 (`bak:`): the reserved forensic key publishes
    /// with ZERO reports — no UnknownKey, no Other, nothing naming `bak` — while
    /// a genuinely unknown key in the same file still reaches `raw` and still
    /// earns its warn (silence is surgical, not a blanket mute). The value
    /// exercises both `strip_comment` paths: a `#` inside quotes survives, an
    /// inline ` # comment` after the quotes is cut.
    #[test]
    fn the_reserved_bak_key_publishes_silently_and_never_reaches_raw() {
        let dir = crate::testutil::temp_dir("scan-bak-silent");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("bak.yaml"),
            "engine: mock\nvram_gb: 2\n\
             bak: \"pre-quant snapshot # keep: F:/bak/x.gguf\" # forensic manual only\n\
             frobnicate: yes\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(
            catalog.errors.len(),
            1,
            "only the genuinely unknown key may be reported: {:?}",
            catalog.errors
        );
        let warn = &catalog.errors[0];
        assert_eq!(warn.kind, ScanKind::UnknownKey);
        assert!(warn.reason.contains("frobnicate"), "{}", warn.reason);
        assert!(
            catalog.errors.iter().all(|e| !e.reason.contains("bak")),
            "no report may mention the bak node: {:?}",
            catalog.errors
        );
        let entry = &catalog.models[0];
        assert!(
            !entry.raw.contains_key("bak"),
            "the forensic value never reaches the raw passthrough: {:?}",
            entry.raw
        );
        assert_eq!(
            entry.raw.get("frobnicate").map(String::as_str),
            Some("yes"),
            "silence must not mute real unknown keys"
        );
    }

    /// Fork C consistency: the reserved word does not exempt a file from the
    /// structural rules. A second `bak:` line is a duplicate-key problem line —
    /// one InvalidYaml report naming it, entry never published.
    #[test]
    fn a_duplicate_bak_line_is_a_problem_line_and_drops_the_file() {
        let dir = crate::testutil::temp_dir("scan-bak-dup");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("twice.yaml"),
            "engine: mock\nvram_gb: 2\nbak: first\nbak: second\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(
            catalog.models.is_empty(),
            "a duplicate bak line must not publish: {:?}",
            catalog.models
        );
        let reports: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::InvalidYaml)
            .collect();
        assert_eq!(reports.len(), 1, "{:?}", catalog.errors);
        assert!(reports[0].file.contains("twice.yaml"));
        assert!(
            reports[0].reason.contains("duplicate key `bak`"),
            "{}",
            reports[0].reason
        );
    }

    /// A key cannot be both read and silent: the two lists must not intersect,
    /// and the silent word must stay out of `did_you_mean`'s candidate source
    /// (CONSUMED_KEYS only), so forensic silence carries no breadcrumb either.
    #[test]
    fn the_silent_and_consumed_key_lists_are_disjoint() {
        for silent in SILENT_KEYS {
            assert!(
                !CONSUMED_KEYS.contains(silent),
                "`{silent}` cannot be both consumed and silent"
            );
            // The one-edit misspelling of a silent key owes no suggestion: the
            // candidate list is CONSUMED_KEYS alone.
            assert_eq!(did_you_mean(&format!("{silent}x")), "");
        }
        assert!(SILENT_KEYS.contains(&"bak"));
    }

    #[test]
    fn junk_lines_are_reported() {
        let (_, problems) = parse_flat_yaml("just text\n");
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("not a `key: value` line"));
    }

    #[test]
    fn scan_dir_collects_models_and_names_bad_files() {
        let dir = crate::testutil::temp_dir("scan-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.yaml"), "engine: mock\nvram_gb: 4\n").expect("write");
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
            Some("a.yaml")
        );
        assert_eq!(catalog.errors.len(), 1);
        assert_eq!(catalog.errors[0].kind, ScanKind::InvalidYaml);
        assert!(catalog.errors[0].file.contains("bad.yaml"));
        assert!(
            catalog.errors[0]
                .reason
                .starts_with("yaml not loaded \u{2014} problem lines: "),
            "{}",
            catalog.errors[0].reason
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fork C doctrine, pinned to its exact shape: a file with problem lines is
    /// NEVER published, and one report names the file and every offending line.
    #[test]
    fn a_file_with_problem_lines_is_never_published_and_every_line_is_named() {
        let dir = crate::testutil::temp_dir("scan-invalid-yaml");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("broken.yaml"),
            "engine: mock\nvram_gb: 2\njust text\nbad key: 1\ncontext: 1\ncontext: 2\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(
            catalog.models.is_empty(),
            "nothing is published blindly: a problem line drops the whole file"
        );
        let errors: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::InvalidYaml)
            .collect();
        assert_eq!(errors.len(), 1, "exactly ONE report: {:?}", catalog.errors);
        assert!(errors[0].file.contains("broken.yaml"), "{:?}", errors[0]);
        assert_eq!(
            errors[0].reason,
            "yaml not loaded \u{2014} problem lines: line 3: not a `key: value` line: `just \
             text`; line 4: invalid key `bad key`; line 6: duplicate key `context` (first \
             value kept)"
        );
    }

    /// The UnknownKey/InvalidYaml boundary (Fork C's first-class test): dead keys
    /// are not broken files. Valid syntax with a dead key publishes with a warn;
    /// one junk line never publishes; both never publishes; a lone duplicate never
    /// publishes.
    #[test]
    fn a_dead_key_publishes_but_a_problem_line_never_does() {
        let dir = crate::testutil::temp_dir("scan-dead-key-boundary");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("deadkey.yaml"),
            "engine: mock\nvram_gb: 2\nfrobnicate: yes\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("junk.yaml"),
            "engine: mock\nvram_gb: 2\njust text\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("both.yaml"),
            "engine: mock\nvram_gb: 2\nfrobnicate: yes\njust text\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("dup.yaml"),
            "engine: mock\nvram_gb: 2\nvram_gb: 3\n",
        )
        .expect("write");

        let catalog = scan_dir(&dir);
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["deadkey"],
            "only the dead-key file publishes: {names:?}"
        );

        let dead = catalog
            .errors
            .iter()
            .filter(|e| e.file.contains("deadkey.yaml"))
            .collect::<Vec<_>>();
        assert!(
            dead.iter().all(|e| e.kind == ScanKind::UnknownKey)
                && dead.iter().any(|e| e.reason.contains("frobnicate")),
            "the dead key is warn-only: {dead:?}"
        );
        for (file, needle) in [
            ("junk.yaml", "not a `key: value` line"),
            ("both.yaml", "not a `key: value` line"),
            ("dup.yaml", "duplicate key"),
        ] {
            let report = catalog
                .errors
                .iter()
                .find(|e| e.kind == ScanKind::InvalidYaml && e.file.contains(file))
                .unwrap_or_else(|| {
                    panic!(
                        "{file} must be refused as invalid yaml: {:?}",
                        catalog.errors
                    )
                });
            assert!(report.reason.contains(needle), "{}", report.reason);
        }
        assert!(
            !catalog
                .errors
                .iter()
                .any(|e| e.kind == ScanKind::Other && e.file.contains("junk.yaml")),
            "problem lines travel as InvalidYaml, never scattered Other reports"
        );
    }

    #[test]
    fn duplicate_ninfer_container_names_are_refused() {
        // Unload reaps by name: one container may back exactly one model
        // (review, 2026-10-26).
        let dir = crate::testutil::temp_dir("scan-dup");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("a.yaml"),
            "engine: ninfer\nninfer_autostart: /s.sh\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("b.yaml"),
            "engine: ninfer\nninfer_autostart: /t.sh\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("c.yaml"),
            "engine: ninfer\nninfer_autostart: /u.sh\nninfer_container: other\n",
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
            catalog.errors.iter().any(|e| e.file.contains("b.yaml")
                && e.kind == ScanKind::Other
                && e.reason.contains("duplicate ninfer_container")),
            "{:?}",
            catalog.errors
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_zero_vram_gb_file_is_named_not_silently_dropped() {
        // FINDINGS-12 [2]: `vram_gb: 0` books the model free at admission, so the
        // scan must refuse it — and name the file, as it does for every sibling
        // reject, so /har/status says which yaml the operator has to fix.
        let dir = crate::testutil::temp_dir("scan-vram0");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("free.yaml"), "engine: mock\nvram_gb: 0\n").expect("write");

        let catalog = scan_dir(&dir);
        assert!(
            catalog.models.is_empty(),
            "a free booking must never enter the catalog"
        );
        assert!(
            catalog
                .errors
                .iter()
                .any(|e| e.file.contains("free.yaml") && e.reason.contains("vram_gb")),
            "the file must be named in catalog_errors: {:?}",
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

    /// 13[5] Fork B acceptance matrix: `.yaml` (any case) is the only config
    /// shape; `.yml`, `README.md`, `.yaml.bak`, `.YAML.bak` and even a directory
    /// named `dir.yaml` are INERT with ZERO catalog errors (the anti-noise pin —
    /// a stray junk file must not start talking).
    #[test]
    fn only_regular_dot_yaml_files_are_configs_and_everything_else_is_silent() {
        let dir = crate::testutil::temp_dir("scan-gate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("good.yaml"), "engine: mock\nvram_gb: 2\n").expect("write");
        std::fs::write(dir.join("CAPS.YAML"), "engine: mock\nvram_gb: 2\n").expect("write");
        // A perfectly valid model document that is NOT accepted: inert, not broken.
        std::fs::write(dir.join("nope.yml"), "engine: mock\nvram_gb: 2\n").expect("write");
        std::fs::write(dir.join("README.md"), "# ModelConf notes\n").expect("write");
        // Junk inside a non-candidate: if the filter ever parsed it, an
        // InvalidYaml report would appear and the errors-empty assert would bite.
        std::fs::write(dir.join("x.yaml.bak"), "oops no colon\n").expect("write");
        std::fs::write(dir.join("UPPER.YAML.bak"), "oops no colon\n").expect("write");
        std::fs::create_dir_all(dir.join("dir.yaml")).expect("dir named yaml");

        let catalog = scan_dir(&dir);
        let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(
            names,
            ["CAPS", "good"],
            "exactly the *.yaml regular files: {names:?}"
        );
        assert!(
            catalog.errors.is_empty(),
            "non-candidates are silent-inert, never noise: {:?}",
            catalog.errors
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The predicate pinned as a unit, both directions, using real `FileType`s
    /// (a regular file's and a directory's) so the `is_file` half is not
    /// decorative either.
    #[test]
    fn the_extension_gate_accepts_only_dot_yaml_case_insensitively() {
        let dir = crate::testutil::temp_dir("scan-gate-predicate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("probe.txt"), "x").expect("write probe");
        std::fs::create_dir_all(dir.join("probe.d")).expect("make dir");
        let (mut file_type, mut dir_type) = (None, None);
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let entry = entry.expect("entry");
            if entry.file_name() == "probe.txt" {
                file_type = Some(entry.file_type().expect("file type"));
            } else if entry.file_name() == "probe.d" {
                dir_type = Some(entry.file_type().expect("dir type"));
            }
        }
        let file_type = file_type.expect("probe file");
        let dir_type = dir_type.expect("probe dir");
        for name in ["m.yaml", "m.YAML", "m.YaMl", "model.yaml"] {
            assert!(is_accepted_config(&file_type, Path::new(name)), "{name}");
        }
        for name in [
            "m.yml",
            "m.YML",
            "x.yaml.bak",
            "UPPER.YAML.bak",
            "README.md",
            "noext",
            ".yaml",
            "m.yamlx",
            // The write-back temp shape (FINDINGS-BATCH-2026-10-28 [5]): a killed
            // write can never leave a scannable file behind.
            "qwen3.yaml.4242.7.sh.tmp",
        ] {
            assert!(!is_accepted_config(&file_type, Path::new(name)), "{name}");
        }
        // Right name, wrong shape: a directory named like a config is inert.
        assert!(!is_accepted_config(&dir_type, Path::new("d.yaml")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The pipe/linked-tree hang shape the reviewer feared dies here: a symlink
    /// is judged by `file_type()` (never followed), so a `*.yaml` link is inert.
    /// Creating symlinks on Windows needs privilege (SeCreateSymbolicLinkPrivilege
    /// or Developer Mode) — when the OS refuses, the non-`is_file` rejection is
    /// still pinned by the directory case above and the std contract that
    /// `DirEntry::file_type` reports the link itself.
    #[test]
    #[cfg(windows)]
    fn a_symlink_is_inert_even_when_it_targets_a_valid_yaml() {
        let dir = crate::testutil::temp_dir("scan-symlink");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let real = dir.join("real.yaml");
        std::fs::write(&real, "engine: mock\nvram_gb: 2\n").expect("write");
        let link = dir.join("linked.yaml");
        match std::os::windows::fs::symlink_file(&real, &link) {
            Ok(()) => {
                let catalog = scan_dir(&dir);
                let names: Vec<&str> = catalog.models.iter().map(|m| m.name.as_str()).collect();
                assert_eq!(names, ["real"], "the symlink must not publish: {names:?}");
                assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
            }
            Err(error) => {
                // Falsifier role is unchanged either way; the privilege fact is
                // stated, not papered over.
                eprintln!(
                    "symlink_file refused on this host ({error}) — unprivileged Windows \
                     without Developer Mode; the non-regular-file rejection stays pinned \
                     by the dir.yaml directory case and the `is_accepted_config` unit test."
                );
                assert!(!link.exists() || link.is_symlink());
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn categories_survive_the_round_trip() {
        let dir = crate::testutil::temp_dir("scan-category");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("a.yaml"), "is_embedding_model: true\n").expect("write");
        std::fs::write(dir.join("b.yaml"), "category: AUX\n").expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models[0].category, CategoryId::Emb);
        assert_eq!(catalog.models[1].category, CategoryId::Aux);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file whose `name:` equals its stem is the frozen shape the t-suite and
    /// the gate-honesty probes write; it must keep working untouched.
    #[test]
    fn a_name_matching_the_stem_publishes_with_stem_identity() {
        let dir = crate::testutil::temp_dir("scan-name-equal");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("pair.yaml"),
            "name: pair\nengine: mock\nvram_gb: 2\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(catalog.models[0].name, "pair");
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    /// A file with no `name:` key at all is the post-stem-identity shape: it scans
    /// clean and carries the stem as its identity.
    #[test]
    fn an_absent_name_key_publishes_with_stem_identity() {
        let dir = crate::testutil::temp_dir("scan-name-absent");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("bare.yaml"), "engine: mock\nvram_gb: 2\n").expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(catalog.models[0].name, "bare");
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    /// FINDINGS-13 [3]: the twin class dies here — a `name:` that contradicts the
    /// file is refused with the exact operator-actionable sentence, not resolved by
    /// pick-one.
    #[test]
    fn a_name_contradicting_the_stem_is_dropped_with_the_exact_reason() {
        let dir = crate::testutil::temp_dir("scan-name-mismatch");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("real.yaml"),
            "name: impostor\nengine: mock\nvram_gb: 2\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "a contradicting name drops");
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::InvalidName)
            .expect("the mismatch must be an InvalidName report");
        assert_eq!(
            error.reason,
            "name: \"impostor\" does not match file real.yaml \u{2014} identity is the file \
             stem; rename the file or delete the key"
        );
        assert!(error.file.contains("real.yaml"));
    }

    #[test]
    fn an_empty_stem_is_refused() {
        // Belt-and-braces: `read_dir` cannot actually yield an empty stem, but the
        // guard must not be decorative — exercise it through the helper path.
        assert!("   ".trim().is_empty());
        let dir = crate::testutil::temp_dir("scan-empty-stem");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("x.yaml"), "engine: mock\n").expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1);
        assert_eq!(catalog.models[0].name, "x");
    }

    /// FINDINGS-13 [2]: a llama entry launched at a GGUF that is not on disk is
    /// refused AND named — the eviction-before-doomed-spawn hole.
    #[test]
    fn a_llama_entry_with_a_missing_model_file_is_dropped_and_named() {
        let dir = crate::testutil::temp_dir("scan-llama-missing");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("gone.yaml"),
            "engine: llama\nmodel: Z:/definitely-not-here/gone.gguf\nvram_gb: 8\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "a doomed spawn must not publish");
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("the missing file must be a ModelPathMissing report");
        assert!(
            error.reason.starts_with("model file not found: ")
                && error.reason.contains("gone.gguf"),
            "{}",
            error.reason
        );
        assert!(error.file.contains("gone.yaml"));
    }

    #[test]
    fn a_llama_entry_with_an_existing_model_file_publishes_its_size() {
        let dir = crate::testutil::temp_dir("scan-llama-present");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("weights.gguf"), b"0123456789").expect("write gguf");
        std::fs::write(
            dir.join("ok.yaml"),
            format!(
                "engine: llama\nmodel: {}\n",
                dir.join("weights.gguf").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
        assert!(
            catalog.models[0].gguf_size_gb.is_some_and(|s| s > 0.0),
            "the same stat still books the size"
        );
    }

    /// `scheme://` sentinels (hf:, strata://, ...) name no local file: the stat
    /// that would "prove" absence would drop every remote-backed model.
    #[test]
    fn a_scheme_sentinel_model_ref_is_exempt_from_the_existence_check() {
        let dir = crate::testutil::temp_dir("scan-sentinel");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("remote.yaml"),
            "engine: llama\nmodel: hf://org/repo/x.gguf\nvram_gb: 4\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(
            catalog.models[0].gguf_size_gb.is_none(),
            "sentinels have no size"
        );
    }

    /// Other engines keep their own existence rules (chunk 10 [2]'s adapter
    /// checks); the scan only polices llama here.
    #[test]
    fn a_non_llama_engine_is_exempt_from_the_model_existence_check() {
        let dir = crate::testutil::temp_dir("scan-exempt-engine");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("mock.yaml"),
            "engine: mock\nmodel: Z:/definitely-not-here/mock.gguf\nvram_gb: 2\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
    }

    #[test]
    fn an_unreadable_or_invalid_strata_profile_drops_the_entry() {
        let dir = crate::testutil::temp_dir("scan-strata-bad");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("nofile.yaml"),
            "engine: strata\nstrata_config: Z:/nope/missing.json\nvram_gb: 8\n",
        )
        .expect("write");
        std::fs::write(dir.join("broken.json"), "{ not json").expect("write profile");
        std::fs::write(
            dir.join("invalid.yaml"),
            format!(
                "engine: strata\nstrata_config: {}\nvram_gb: 8\n",
                dir.join("broken.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "neither entry may publish");
        for file in ["nofile.yaml", "invalid.yaml"] {
            assert!(
                catalog
                    .errors
                    .iter()
                    .any(|e| e.kind == ScanKind::ProfileUnreadable
                        && e.file.contains(file)
                        && e.reason.starts_with("strata profile not readable: ")),
                "{file} must be refused as an unreadable profile: {:?}",
                catalog.errors
            );
        }
    }

    #[test]
    fn a_strata_profile_naming_a_missing_path_drops_the_entry_naming_key_and_path() {
        let dir = crate::testutil::temp_dir("scan-strata-missing-path");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("profile.json"),
            r#"{"exe": "C:\\Strata\\engine\\strata.exe", "args": ["--native", "Z:\\nope\\doomed.gguf"]}"#,
        )
        .expect("write profile");
        std::fs::write(
            dir.join("ref.yaml"),
            format!(
                "engine: strata\nstrata_config: {}\nvram_gb: 8\n",
                dir.join("profile.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty());
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("a referenced path that is gone must drop the entry");
        assert!(
            error
                .reason
                .starts_with("strata profile references missing path: ")
                && error.reason.contains("doomed.gguf"),
            "{}",
            error.reason
        );
        // The array element reports under its owning key so the operator can find
        // the line in the profile.
        assert!(error.reason.contains("args"), "{}", error.reason);
    }

    /// The `log` key is exempt entirely: a profile that writes its log beside the
    /// model publishes even when the log target has never existed.
    #[test]
    fn a_strata_profile_log_key_is_exempt_from_existence() {
        let dir = crate::testutil::temp_dir("scan-strata-log");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("profile.json"),
            r#"{"exe": "Z:\\nope\\strata.exe.log", "log": "Z:\\nope\\never-existed.log", "cwd": "."}"#,
        )
        .expect("write profile");
        std::fs::write(
            dir.join("log.yaml"),
            format!(
                "engine: strata\nstrata_config: {}\nvram_gb: 8\n",
                dir.join("profile.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        // `exe` is checked (and missing) — but with the exe present and only the
        // log absent the entry must publish. Assert both halves.
        assert!(catalog.models.is_empty(), "a missing exe still drops");
        assert!(
            !catalog
                .errors
                .iter()
                .any(|e| e.reason.contains("never-existed")),
            "the log key must never be judged: {:?}",
            catalog.errors
        );
        std::fs::write(dir.join("strata.exe"), b"placeholder").expect("write exe");
        std::fs::write(
            dir.join("profile.json"),
            format!(
                "{{\"exe\": \"{}\", \"log\": \"Z:\\\\nope\\\\never-existed.log\"}}",
                dir.join("strata.exe")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            ),
        )
        .expect("rewrite profile");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    /// The `--control-vector-scaled <path>:1.0` shape: the scale suffix is stripped
    /// before the stat, so a real file named `x.gguf` referenced as `x.gguf:1.0`
    /// publishes instead of being falsely dropped.
    #[test]
    fn a_strata_control_vector_scale_suffix_is_stripped_before_the_stat() {
        let dir = crate::testutil::temp_dir("scan-strata-scaled");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("vec.gguf"), b"v").expect("write vector");
        std::fs::write(
            dir.join("profile.json"),
            format!(
                "{{\"args\": [\"--control-vector-scaled\", \"{}:1.0\"]}}",
                dir.join("vec.gguf")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            ),
        )
        .expect("write profile");
        std::fs::write(
            dir.join("scaled.yaml"),
            format!(
                "engine: strata\nstrata_config: {}\nvram_gb: 8\n",
                dir.join("profile.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    #[test]
    fn a_bom_prefixed_strata_profile_is_read_not_refused() {
        // The live `strata-qwen-gptq-nvfp4.json` is UTF-8 with BOM; the engine
        // reads it, so the scan must not call it unreadable.
        let dir = crate::testutil::temp_dir("scan-strata-bom");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("x.gguf"), b"m").expect("write model");
        std::fs::write(
            dir.join("profile.json"),
            format!(
                "\u{feff}{{\"exe\": \"{}\"}}",
                dir.join("x.gguf")
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
            ),
        )
        .expect("write profile");
        std::fs::write(
            dir.join("bom.yaml"),
            format!(
                "engine: strata\nstrata_config: {}\nvram_gb: 8\n",
                dir.join("profile.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    #[test]
    fn a_strata_entry_without_the_config_key_defers_to_the_adapter() {
        // The chunk 10 [2] preflight pin (`required field strata_config` refuses
        // before the victim dies) needs this entry published, so an absent key is
        // the adapter's refusal to name, not the scan's.
        let dir = crate::testutil::temp_dir("scan-strata-nokey");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("nocfg.yaml"), "engine: strata\nvram_gb: 8\n").expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
    }

    /// FINDINGS-13 [8]: the unknown key is *said*, never swallowed — and never
    /// fatal: the entry still publishes (README: preserved for reference).
    #[test]
    fn an_unknown_key_warns_and_the_entry_still_publishes() {
        let dir = crate::testutil::temp_dir("scan-unknown-key");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("typo.yaml"),
            "engine: mock\nvram_gb: 2\nfrobnicate: yes\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            1,
            "warn-only: the model stays loadable"
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::UnknownKey)
            .expect("the unknown key must be reported");
        assert!(error.file.contains("typo.yaml"));
        assert!(error.reason.contains("frobnicate"), "{}", error.reason);
    }

    #[test]
    fn the_host_and_port_policy_keys_get_their_exact_hints() {
        let dir = crate::testutil::temp_dir("scan-policy-keys");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("inert.yaml"),
            "engine: mock\nvram_gb: 2\nhost: 0.0.0.0\nport: 8080\n",
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "policy hints are warn-only too");
        assert!(
            catalog.errors.iter().any(|e| e.kind == ScanKind::UnknownKey
                && e.reason
                    == "engine host is harness-forced (loopback, \u{a7}5) \u{2014} this key \
                        does nothing"),
            "{:?}",
            catalog.errors
        );
        assert!(
            catalog.errors.iter().any(|e| e.kind == ScanKind::UnknownKey
                && e.reason
                    == "engine port is harness-assigned from category config \u{2014} this \
                        key does nothing"),
            "{:?}",
            catalog.errors
        );
    }

    #[test]
    fn a_one_edit_typo_gets_a_unique_suggestion() {
        assert_eq!(did_you_mean("contex"), " (did you mean 'context'?)");
        // `por` is distance 2 from every consumed key (and `port` is policy-hinted,
        // not consumed), so no suggestion is owed — a guess would be false advice.
        assert_eq!(did_you_mean("por"), "");
        // A key one edit from a single consumed key still gets the advice.
        assert_eq!(did_you_mean("ngl_"), " (did you mean 'ngl'?)");
        assert!(!within_one_edit("context", "context"));
    }

    /// The golden fixture pins the const from the front: every consumed key appears
    /// in a real file, and the whole file scans with ZERO unknown-key errors.
    #[test]
    fn the_golden_yaml_covers_every_consumed_key_and_scans_unknown_key_free() {
        let dir = crate::testutil::temp_dir("scan-golden");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("golden.yaml"),
            "engine: mock\n\
             name: golden\n\
             model: mock://golden\n\
             category: WORK\n\
             is_embedding_model: false\n\
             vram_gb: 12\n\
             cost: greedy\n\
             launch_only_with_at_least_vram_free: 6\n\
             load_timeout: 600\n\
             serve_model: golden-id\n\
             cpu_offload: false\n\
             jinja: true\n\
             extra_args: --warmup\n\
             mmproj: F:/m/mmproj.gguf\n\
             context: 4096\n\
             ngl: 99\n\
             batch: 512\n\
             ubatch: 256\n\
             flash_attn: 1\n\
             cache_type_k: q8_0\n\
             cache_type_v: q8_0\n\
             parallel: 2\n\
             fit: true\n\
             fit_target: 0.9\n\
             fit_ctx: 2048\n\
             measured_gb: 20.0\n\
             measured_gb_min: 10.0\n\
             measured_offload: false\n\
             strata_config: C:/Strata/profile.json\n\
             strata_python: C:/Strata/.venv/Scripts/python.exe\n\
             strata_cwd: C:/Strata\n\
             ninfer_autostart: /opt/a.sh\n\
             ninfer_container: golden-box\n\
             ninfer_profile: b\n\
             ninfer_key: true\n\
             external: false\n",
        )
        .expect("write golden");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(
            catalog
                .errors
                .iter()
                .all(|e| e.kind != ScanKind::UnknownKey),
            "every const member must be a real, read key: {:?}",
            catalog.errors
        );
        for key in CONSUMED_KEYS {
            if *key == "external" {
                continue; // retired: consumed (never flagged) but stripped from raw
            }
            assert!(
                catalog.models[0].raw.contains_key(*key),
                "the golden fixture must actually exercise `{key}`"
            );
        }
    }

    /// The const pinned from the other side: the code's key literals are members.
    /// `FLAG_TABLE` is the llama flag map read straight into argv; the manual list
    /// mirrors the remaining `.get("<key>")` reads across the adapters and the
    /// typed entry (adding a read without listing it here fails this test; adding
    /// it to the const without reading it fails the golden test).
    #[test]
    fn consumed_keys_cover_every_key_the_code_reads() {
        for (key, _flag) in crate::engine::llama::FLAG_TABLE {
            assert!(
                CONSUMED_KEYS.contains(key),
                "FLAG_TABLE key `{key}` must be a consumed key"
            );
        }
        for key in [
            "name",
            "engine",
            "model",
            "category",
            "is_embedding_model",
            "vram_gb",
            "cost",
            "launch_only_with_at_least_vram_free",
            "load_timeout",
            "serve_model",
            "cpu_offload",
            "jinja",
            "extra_args",
            "measured_gb",
            "measured_gb_min",
            "measured_offload",
            "strata_config",
            "strata_python",
            "strata_cwd",
            "ninfer_autostart",
            "ninfer_container",
            "ninfer_profile",
            "ninfer_key",
            "external",
        ] {
            assert!(
                CONSUMED_KEYS.contains(&key),
                "`{key}` must be a consumed key"
            );
        }
    }

    /// The machine check the sweep left behind: the operator's live ModelConf must
    /// carry no key the harness does not read. Skipped when the dir is absent (the
    /// repo copy is gitignored and machine-dependent).
    #[test]
    fn the_live_modelconf_directory_has_no_unknown_keys() {
        let dir = Path::new("ModelConf");
        if !dir.is_dir() {
            eprintln!("no ModelConf directory beside the test cwd; skipping");
            return;
        }
        let catalog = scan_dir(dir);
        let offenders: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::UnknownKey)
            .collect();
        assert!(
            offenders.is_empty(),
            "live ModelConf must scan with zero unknown-key errors: {offenders:?}"
        );
    }

    /// Fork C gate: the strict acceptance must not blind-side the live catalog —
    /// every accepted file the operator keeps publishes, and the dir talks to
    /// nobody: zero problem lines, zero reports of any kind. The expected count
    /// is the dir's own accepted enumeration (FINDINGS-BATCH-2026-10-28 [6]):
    /// adding or removing a live yaml is catalog state, not a test edit. Skipped
    /// when the dir is absent.
    #[test]
    fn the_live_modelconf_directory_scans_fully_clean() {
        let dir = Path::new("ModelConf");
        if !dir.is_dir() {
            eprintln!(
                "no ModelConf directory beside the test cwd (looked for {}); skipping",
                dir.display()
            );
            return;
        }
        let accepted = std::fs::read_dir(dir)
            .expect("read live ModelConf dir")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_type()
                    .ok()
                    .is_some_and(|file_type| is_accepted_config(&file_type, &entry.path()))
            })
            .count();
        assert!(accepted > 0, "an accepted-file count of zero pins nothing");
        let catalog = scan_dir(dir);
        assert_eq!(
            catalog.models.len(),
            accepted,
            "every accepted live file is problem-line-free and must publish: {:?}",
            catalog.errors
        );
        let offenders: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::InvalidYaml)
            .collect();
        assert!(
            offenders.is_empty(),
            "the live ModelConf must scan with zero invalid-yaml files: {offenders:?}"
        );
        assert!(
            catalog.errors.is_empty(),
            "the live ModelConf must scan fully clean, not just publishable: {:?}",
            catalog.errors
        );
    }

    /// `kind` is part of the wire contract (`/har/status` and `/har/rescan` both
    /// serialize `catalog_errors`): the JSON field names are pinned here.
    #[test]
    fn scan_error_kinds_serialize_as_snake_case() {
        let error = ScanError {
            kind: ScanKind::ModelPathMissing,
            file: "m.yaml".to_string(),
            reason: "model file not found: F:/x.gguf".to_string(),
        };
        let json = serde_json::to_value(&error).expect("serializable");
        assert_eq!(json["kind"], "model_path_missing");
        assert_eq!(json["file"], "m.yaml");
        assert_eq!(json["reason"], "model file not found: F:/x.gguf");
        for (kind, name) in [
            (ScanKind::ProfileUnreadable, "profile_unreadable"),
            (ScanKind::InvalidName, "invalid_name"),
            (ScanKind::EngineBinaryMissing, "engine_binary_missing"),
            (ScanKind::EngineUnavailable, "engine_unavailable"),
            (ScanKind::UnknownKey, "unknown_key"),
            (ScanKind::InvalidYaml, "invalid_yaml"),
            (ScanKind::Other, "other"),
        ] {
            let value = serde_json::to_value(ScanError {
                kind,
                file: "config".to_string(),
                reason: "x".to_string(),
            })
            .expect("serializable");
            assert_eq!(value["kind"], name, "{kind:?}");
        }
    }
}
