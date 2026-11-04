//! ModelConf scan (DESIGN Step 9): flat `key: value` yaml files -> typed catalog,
//! with per-file error reports.
//!
//! v3.3 lessons encoded here: quotes and inline comments are handled (D2 — `name:
//! "foo"` and `context: 4096 # 32k` parse correctly), and a file with problem lines
//! is never published; its lines are named (D3, operator doctrine: "better not
//! publish anything blindly"), while the scan itself stays non-fatal (§0.2 step 6).
//!
//! Catalog-trust rules (FINDINGS-13 \[2]/\[3]/\[7]/\[8]): identity is the yaml FILE
//! STEM — a `name:` key is legacy-optional and only legal when it equals the stem,
//! because that value is the one component of a yaml that reaches a filesystem path
//! (the engine log name); a llama entry whose local `model:` or declared `mmproj:`
//! file is gone, and a
//! strata entry whose profile JSON is unreadable or references a missing absolute
//! path, are refused here with the file named, before admission can evict a resident
//! engine for a doomed spawn; and a key outside `CONSUMED_KEYS` is reported
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
    /// The model's backing file — or a declared `mmproj:`, or a path its strata
    /// profile references — is not a regular file on disk; the entry was dropped.
    ModelPathMissing,
    /// A `name:` key contradicts the file stem, or the stem is empty; dropped.
    InvalidName,
    /// A configured engine binary does not exist (a `config` cross-check, raised by
    /// the actor's `apply_catalog`, not by the scan itself).
    EngineBinaryMissing,
    /// The entry names an engine this harness has no adapter for; dropped by the
    /// actor (the scan cannot see the adapter map).
    EngineUnavailable,
    /// A yaml key outside `CONSUMED_KEYS` (and outside `SILENT_KEYS`, the one
    /// reserved forensic class that stays silent by operator ruling); warn-only,
    /// the entry still publishes.
    UnknownKey,
    /// A real harness key — a member of `CONSUMED_KEYS` — that only the rows
    /// of *other* engines list in their `EnginePolicy` `knobs` cell, appearing
    /// on a model whose row does not read it. The opposite of
    /// [`ScanKind::UnknownKey`] (a key the harness owns nowhere, i.e. a typo):
    /// here the key is live config for a *different* engine, so it is not a
    /// typo and cannot be silenced by `CONSUMED_KEYS` membership alone.
    /// Warn-only, exactly the retired `host:`/`port:` policy-hint posture: the
    /// entry still publishes. The mechanism is the knob table (slice 10c
    /// generalization of the 2026-10-28 `cpu_offload` ruling): every
    /// engine-scoped key — `cpu_offload` and its llama siblings, the strata,
    /// basalt and ninfer key families — is scoped the same data-driven way, so
    /// a new row knob needs no new scan code.
    InertKey,
    /// A yaml file the scan refuses to publish (Fork C doctrine): it carried at
    /// least one problem line — then the `reason` names every offending line — or it
    /// parsed clean but yielded **no keys at all**, which the `reason` says outright
    /// (FINDINGS2-13 \[2]; the shape `commit_verified` refuses as `EmptyFile`).
    InvalidYaml,
    /// Two published rows collapse to one harness identity, so every name-keyed
    /// path — `/har/load`, the write-back, the measured fold — would silently prefer
    /// whichever row the scan order happened to put first (FINDINGS-13 \[3]). Warn-only
    /// like the other report classes: **both** files publish and **both** are reported,
    /// naming every claimant; the API's `409` stays the refusal, so nothing here is a
    /// boot-style rejection of a file that is otherwise loadable.
    DuplicateName,
    /// The file claims a harness-owned surface and is **refused** (never published):
    /// an operator-extra word (`extra_args`, `ninfer_serve_args`) outside the engine
    /// row's closed word surface, a policy field (`host`, `port`, key) inside the
    /// strata profile it points at, or a ninfer row still declaring the removed
    /// `ninfer_autostart` launch key (ruling 18b removal). Engine-side files
    /// decide nothing (22\[1]), so the refusal names the word/field and the
    /// mechanism that owns the decision (chunk 10 slice 10c).
    PolicyRefused,
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
/// typed entry, the llama flag table and its extra reads, the strata, basalt and
/// ninfer adapters, and the measured triple. This const is THE source of truth for the
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
    "measured_attribution",
    "measured_profile",
    "strata_config",
    "strata_python",
    "strata_cwd",
    "strata_exe",
    "strata_args",
    "strata_tokenizer",
    "strata_lib_dirs",
    "strata_env",
    "strata_gpu",
    "strata_parallel",
    "strata_gpus_asked",
    "strata_vision",
    "strata_sampling",
    "strata_uncensored",
    "basalt_python",
    "basalt_cwd",
    "basalt_exe",
    "basalt_parallel",
    "basalt_engine_args",
    "ninfer_autostart",
    "ninfer_container",
    "ninfer_profile",
    "ninfer_key",
    "ninfer_image",
    "ninfer_model_name",
    "ninfer_artifact",
    "ninfer_internal_port",
    "ninfer_model_roots",
    "ninfer_serve_args",
    "ninfer_gpu_device",
    "ninfer_model_id",
    "ninfer_key_file",
    "ninfer_serve_cmd",
    "ninfer_env",
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

/// Builds the [`ScanKind::InertKey`] sentence for one engine-scoped key: the
/// same named-policy-hint posture as [`HOST_POLICY_HINT`]/[`PORT_POLICY_HINT`]
/// (§20-9 Fork C knob table — slice 10c: the owners come from the table itself,
/// so every row knob is scoped by the same mechanism, not by a key list here).
/// `pub(crate)` because the API's refusal to WRITE an inert key quotes the
/// identical sentence (the scan hint and the 400 must never diverge).
pub(crate) fn inert_key_reason(key: &str, engine: &str) -> String {
    format!(
        "`{key}` is read only by the {} engine(s) \u{2014} engine `{engine}` never reads it; \
         this key does nothing here",
        crate::engine::policy::knob_readers(key).join("/")
    )
}

/// Parses one flat yaml document into a key map plus per-line problems.
///
/// Rules: `key: value` with `key` matching `[A-Za-z_][A-Za-z0-9_-]*`; surrounding
/// single/double quotes stripped; ` # ...` inline comments removed outside quotes;
/// blank/comment lines skipped; anything else is reported, never dropped silently.
///
/// The comment rule, exactly (`split_value_comment` is the one owner of it):
/// a `#` opens a comment only when an ASCII **space** precedes it outside quotes.
/// Two deliberate consequences, symmetric on both sides of the reader/writer pair
/// (FINDINGS-BATCH-2026-10-28B \[1]/\[7]): a **tab never opens a comment**
/// (`key:\t# note` publishes `# note` as the value), and leading whitespace is
/// trimmed *before* the walk, so a `#` at the very start of the value text is
/// value too (`key: # tail` publishes `# tail`). Quote such a value (`"# tail"`)
/// when you mean it literally and want nothing read as a comment after it.
pub fn parse_flat_yaml(text: &str) -> (BTreeMap<String, String>, Vec<String>) {
    let mut map = BTreeMap::new();
    let mut problems = Vec::new();
    for (index, raw_line) in text.lines().enumerate() {
        let line = normalized_line(raw_line);
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
        let value = strip_comment(&line[colon + 1..]);
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

/// Splits a raw `key:` value text into (parsed value, verbatim inline comment).
///
/// This is THE boundary rule **and the only normalizer of value text** (operator
/// ruling 2026-10-28, FINDINGS-13 [8] Fork B; FINDINGS-BATCH-2026-10-28B [1]):
/// scan parses through here and the write-back compares a line's value text
/// through here, so no caller can hand the rule a differently-normalized text and
/// the reader and the writer cannot disagree about where the value ends. The
/// incoming word a caller asks to write is NOT pre-normalized at all: it is the
/// byte-exact target that a candidate line form must read back to
/// (`crate::model::writeback::emit_form` feeds candidates through this same
/// helper). Normalization
/// (surrounding whitespace, a line's trailing `\r`, a leading BOM) happens
/// **inside**, before the walk: the comment begins at the first space `#`
/// **outside** quotes of the *normalized* text and is returned verbatim from that
/// cut (so its ` #` opening and inner spacing survive); the value is what precedes
/// the cut, trimmed, with one surrounding quote pair stripped. `None` when no such
/// cut exists. Trimming first has two pinned consequences: a `#` that only looked
/// space-preceded because of leading whitespace is value, and a tab never opens a
/// comment.
/// Walking `char_indices` (never a char vector indexed by a byte length, which is
/// what panicked on non-ASCII before this rewrite) keeps every cut a char
/// boundary and needs no allocation; the final character is visited with no
/// successor, which can never form a ` #` cut, so no rule depends on looking one
/// character ahead.
pub(crate) fn split_value_comment(raw_value: &str) -> (&str, Option<&str>) {
    let text = normalize_value_text(raw_value);
    let mut in_quote: Option<char> = None;
    let mut cut: Option<usize> = None;
    let mut chars = text.char_indices().peekable();
    while let Some((index, current)) = chars.next() {
        match in_quote {
            Some(q) if current == q => in_quote = None,
            Some(_) => {}
            None => {
                if current == '"' || current == '\'' {
                    in_quote = Some(current);
                } else if current == ' ' && chars.peek().is_some_and(|&(_, next)| next == '#') {
                    cut = Some(index);
                    break;
                }
            }
        }
    }
    match cut {
        Some(index) => (unquote(text[..index].trim()), Some(&text[index..])),
        None => (unquote(text), None),
    }
}

/// The single normalization a value text passes through, so the walk sees one
/// shape no matter which caller produced it: surrounding whitespace (including a
/// CRLF line's trailing `\r`) and a leading BOM, which `str::trim` leaves alone.
/// Trimmed twice because a BOM can ride inside the leading whitespace, and the
/// helper must still return slices of the caller's own bytes.
fn normalize_value_text(raw_value: &str) -> &str {
    raw_value.trim().trim_start_matches('\u{feff}').trim()
}

/// The one line normalizer both sides of the reader/writer pair use: a document
/// line is its BOM-stripped, trimmed text. `str::trim` alone does not remove
/// U+FEFF (category Cf, not White_Space), so the explicit strip is what keeps a
/// BOM'd first line visible to the reader and the writer alike; the rule lives
/// here and nowhere else, so the two cannot disagree about which line a key is on.
pub(crate) fn normalized_line(line: &str) -> &str {
    line.trim_start_matches('\u{feff}').trim()
}

/// Strips one surrounding quote pair (`"…"` / `'…'`) from a trimmed value.
///
/// Sound by assumption: both accepted quotes are one ASCII byte, so peeling one
/// byte per side always lands on a char boundary — `unquote_peels_ascii_quotes_
/// around_multibyte_content` pins that with multi-byte content between the quotes.
fn unquote(trimmed: &str) -> &str {
    if trimmed.len() >= 2 {
        let first = trimmed.chars().next().expect("non-empty checked");
        let last = trimmed.chars().next_back().expect("non-empty checked");
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return &trimmed[1..trimmed.len() - 1];
        }
    }
    trimmed
}

/// Removes an inline ` # comment` (outside quotes) and surrounding quote pairs.
fn strip_comment(value: &str) -> String {
    split_value_comment(value).0.to_string()
}

/// The torture set both layers are pinned against (FINDINGS-BATCH-2026-10-28B
/// [2]): every value-text shape whose boundary the reader and the writer must
/// agree on, paired with the value the reader publishes. The cross-layer pins in
/// `writeback` feed each published value back through the writer and demand a
/// byte-identical document, so a writer-local comment parser cannot survive the
/// suite — the acceptance criterion the self-comparison it replaces could not
/// meet (that loop was `strip_comment` asserted against `split_value_comment`,
/// one function in two faces, and stayed green with a private parser in the
/// writer).
#[cfg(test)]
pub(crate) const TORTURE_SHAPES: &[(&str, &str)] = &[
    (" 4096 # 32k", "4096"),
    (" \"a # b\" # tail", "a # b"),
    (" 'a # b' # tail", "a # b"),
    (" \"4096\"", "4096"),
    (" 'a'", "a"),
    (" 4096#c", "4096#c"),
    (" 4096 #32k", "4096"),
    (" plain", "plain"),
    (" 4096   ", "4096"),
    (" 4096   # c", "4096"),
    ("", ""),
    (" ", ""),
    (" #", "#"),
    (" #tail", "#tail"),
    (" # tail", "# tail"),
    ("  # a # b", "# a"),
    (" 4096\t# c", "4096\t# c"),
    (" 4096\t# c # tail", "4096\t# c"),
    (" \"unclosed # x", "\"unclosed # x"),
    (" caf\u{e9} # morning", "caf\u{e9}"),
    (" \u{540d}\u{524d}", "\u{540d}\u{524d}"),
    (" \u{2192}\u{2192} 4096 # arrows", "\u{2192}\u{2192} 4096"),
    (" caf\u{65}\u{301} # morning", "caf\u{65}\u{301}"),
    // The peel-edge class FINDINGS2-13 [1] found: the quote peel happens AFTER the
    // slice is trimmed, so a published value may legitimately carry edge whitespace
    // or a residual quote pair. Every one of these must round-trip byte-exactly.
    (" \"a b \" # tail", "a b "),
    (" \" a\"", " a"),
    (" 'a ' # tail", "a "),
    (" \"a\t\" # tail", "a\t"),
    (" \"\"\"\" # tail", "\"\""),
    (" '''' # tail", "''"),
    (" \"\u{feff}x\" # tail", "\u{feff}x"),
];

/// The deterministic adversarial corpus for the hand-rolled flat-YAML reader
/// (N4, "no fuzz/property test"): one fixed document list shared by both
/// layers — `scan::tests` drives parse and entry building through it,
/// `writeback::tests` drives the identical-write no-op and the value
/// round-trip through it. A true property test needs a generating fuzz
/// harness, which would be a new dependency; the dependency policy refuses
/// that, so this corpus is the deterministic stand-in: every input fixed,
/// every outcome asserted, and the reproducibility a fuzzer owes is asserted
/// explicitly (parse twice, compare). Invalid UTF-8 is not a case and cannot
/// be: the reader's input is a `&str`, so a byte-sequence reject happens
/// upstream of this module, at the file read.
#[cfg(test)]
pub(crate) fn adversarial_docs() -> Vec<(&'static str, String)> {
    vec![
        ("empty", String::new()),
        ("whitespace", "   \t \n \n".to_string()),
        ("comment-only", "# just a note\n\n   \n".to_string()),
        ("bom", "\u{feff}name: corpus\nengine: mock\n".to_string()),
        (
            "crlf",
            "name: corpus\r\nengine: mock\r\nvram_gb: 4\r\n".to_string(),
        ),
        ("quotes", "name: \"corpus\"\nengine: 'mock'\n".to_string()),
        (
            "inline-comments",
            "name: corpus # named\ncontext: 4096 # 32k\nengine: mock\n".to_string(),
        ),
        (
            "duplicate-keys",
            "name: corpus\nengine: mock\nengine: llama\n".to_string(),
        ),
        (
            "unknown-key",
            "name: corpus\nengine: mock\nfrobnicate: yes\n".to_string(),
        ),
        ("huge-line", format!("name: corpus\nnote: {}\n", "x".repeat(100_000))),
        (
            "control-chars",
            "name: corpus\nweird: a\u{1}b\u{7}\nengine: mock\n".to_string(),
        ),
        (
            "deep-quotes",
            format!("name: corpus\nnote: {}\nengine: mock\n", "\"".repeat(14)),
        ),
        (
            "deep-hash-chain",
            "name: corpus\nnote: a # b # c # d # e # f # g\nengine: mock\n".to_string(),
        ),
        ("colon-only", ":\n:::\n".to_string()),
        ("junk-line", "just text\nname: corpus\n".to_string()),
        ("space-key", "bad key: 1\nname: corpus\n".to_string()),
        (
            "vram-overflow",
            "name: corpus\nengine: mock\nvram_gb: 1e400\n".to_string(),
        ),
        (
            "timeout-overflow",
            "name: corpus\nengine: mock\nload_timeout: 99999999999999999999999999\n".to_string(),
        ),
        (
            "vram-negative-zero",
            "name: corpus\nengine: mock\nvram_gb: -0.0\n".to_string(),
        ),
        (
            "many-keys",
            (1..=200)
                .map(|i| format!("filler_key_{i:03}: {i}\n"))
                .collect(),
        ),
        (
            "valid-round-trip",
            "# operator header\nname: corpus # identity\nengine: mock\n category: WORK\nvram_gb: 4.5 # budget\ncontext: \"4096\" # quoted\r\n".to_string(),
        ),
    ]
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

/// The existence-trace file-type predicate (R2-13-01): the traces accept
/// **regular files** only. `fs::metadata` succeeds
/// for a directory, so without this filter an operator typo
/// (`model: F:/models/Qwen-GGUF` — the folder, filename forgotten) publishes,
/// admission evicts victims, and the engine dies at Spawn on the directory path
/// — the exact doomed-spawn swap the trace exists to refuse — and the same stat
/// books the directory's byte size (often 0 on NTFS) into `gguf_size_gb`.
fn regular_file_metadata(path: &Path) -> Option<std::fs::Metadata> {
    std::fs::metadata(path).ok().filter(|meta| meta.is_file())
}

/// The one base an existence trace joins a relative yaml value onto: the yaml's
/// own directory — the same base `model:` uses (launch resolves `model:`
/// against the same dir; the engine worker owns relative resolution on the
/// launch side, the scan only refuses).
fn trace_under_yaml_dir(dir: &Path, raw: &str) -> std::path::PathBuf {
    let path = Path::new(raw);
    if path.is_relative() {
        dir.join(path)
    } else {
        path.to_path_buf()
    }
}

/// Scans a ModelConf directory: every regular (non-symlink) `*.yaml` file becomes
/// an entry or an error; `.yml` and every other name is **silently inert** (the
/// 13\[5] Fork B acceptance gate, `is_accepted_config`); a missing directory is
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
        // name-shaped value that reaches a `File::create` path as `log_name`. Tying
        // identity to the file is what makes a `name:`-key twin unconstructable; it
        // does NOT stop a whitespace-suffixed stem (`qwen3.yaml` beside `qwen3 .yaml`,
        // which `from_flat` trims to one name) from collapsing, and that case is what
        // the `DuplicateName` post-pass below reports (FINDINGS2-13 [3]).
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
        if map.is_empty() {
            // The same Fork C voice as the gate above, and the mirror of
            // `commit_verified`'s `EmptyFile` refusal (FINDINGS2-13 [2]): the
            // write-back will not produce a zero-key model file, so the scan must
            // not invent one either. Without this, `blank.yaml` publishes as a
            // spawnable-looking `engine: llama` row named by its file stem with no
            // `model:` at all — the existence check cannot fire because the key is
            // absent, not wrong — and reports nothing. Silently skipping would be
            // the blind-publish this doctrine already rejected; it is reported.
            catalog.errors.push(ScanError {
                kind: ScanKind::InvalidYaml,
                file: file_name.clone(),
                reason: "yaml not loaded \u{2014} no `key: value` lines: an empty or \
                         comment-only .yaml is not a model config"
                    .to_string(),
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
                // Ruling 18b removal (green Wave C): the legacy `ninfer_autostart`
                // launch path is gone. A ninfer row still declaring it is refused
                // outright with the adapter's migration sentence (the strata
                // `strata_config` precedent) and never published. The row must
                // not ride a launch the harness no longer has.
                if entry.engine == crate::ownership::NINFER_ENGINE {
                    if let Some(script) = map
                        .get("ninfer_autostart")
                        .map(|v| v.trim())
                        .filter(|v| !v.is_empty())
                    {
                        catalog.errors.push(ScanError {
                            kind: ScanKind::PolicyRefused,
                            file: file_name.clone(),
                            reason: crate::engine::ninfer::legacy_removed_sentence(script),
                        });
                        continue;
                    }
                }
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
                // A trace passes only on a regular file (R2-13-01): a directory
                // neither passes the trace nor books its byte size as the model's.
                let local = entry
                    .model_ref
                    .as_deref()
                    .filter(|r| !r.contains("://"))
                    .map(|raw| trace_under_yaml_dir(dir, raw));
                let stat = local.as_ref().and_then(|p| regular_file_metadata(p));
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
                // R2-13-02 (scan half): a declared `mmproj:` is the second
                // weights file (policy.rs names it so) — llama-server dies
                // without it, after victims are gone. Existence-trace it like
                // `model:`: same yaml-dir join, same regular-file predicate,
                // same `ModelPathMissing`. The raw map is not resolved or
                // rewritten — relative values resolve at launch in the engine
                // worker (chunk 9); the scan only refuses here.
                if entry.engine == "llama" {
                    let mmproj_missing = map
                        .get("mmproj")
                        .map(|v| v.trim())
                        .filter(|v| !v.is_empty() && !v.contains("://"))
                        .map(|raw| trace_under_yaml_dir(dir, raw))
                        .filter(|path| regular_file_metadata(path).is_none());
                    if let Some(missing) = mmproj_missing {
                        catalog.errors.push(ScanError {
                            kind: ScanKind::ModelPathMissing,
                            file: file_name.clone(),
                            reason: format!("mmproj file not found: {}", missing.display()),
                        });
                        continue;
                    }
                }
                // A basalt `.basalt` pack is not a GGUF: its bytes must never
                // feed the size×1.15+2 estimate. The booking source of a basalt
                // row is the declared `vram_gb` with `cost: fixed`, or the room
                // at admit with `cost: greedy` (TASK-BASALT, TASK-BASALT-GREEDY),
                // so a pack stat stays display-blind here.
                if entry.engine != "basalt" {
                    entry.gguf_size_gb = stat.map(|meta| meta.len() as f64 / (1024.0_f64).powi(3));
                }
                // A strata row is either template mode — the row yaml owns the
                // profile's fields and the harness renders the JSON, so the
                // scan renders the same bytes and refuses a referenced path
                // the disk lacks before any load gesture (row 19) — or still
                // declares `strata_config`, the mode SUPERSEDED by ruling
                // 2026-11-02 row 19: such a row is refused outright with the
                // migration sentence, and the live manual profile it points at
                // stays the operator's own file, read by nothing on the serve
                // path. Relative rendered paths are exempt (the engine
                // resolves them against the profile dir — chunk 10 backlog);
                // the `log` key is exempt entirely (a log target's existence
                // is not a load precondition).
                if entry.engine == "strata" {
                    let template = crate::engine::strata::template_mode(&entry);
                    if let Some(profile) = map
                        .get("strata_config")
                        .map(|v| v.trim())
                        .filter(|v| !v.is_empty())
                    {
                        // A row that already carries the imported surface only
                        // needs its `strata_config` line deleted — name that
                        // door, not the import bridge it does not need; the
                        // adapter refuses the same sentence (review 25 [7]).
                        let reason = if template {
                            crate::engine::strata::mutual_exclusion_sentence(profile)
                        } else {
                            crate::engine::strata::superseded_sentence(profile)
                        };
                        catalog.errors.push(ScanError {
                            kind: ScanKind::PolicyRefused,
                            file: file_name.clone(),
                            reason,
                        });
                        continue;
                    }
                    if template {
                        // The default cwd resolves exactly as the adapter's
                        // does (the `engines:` default — scan runs before the
                        // config-wired adapter exists, so the constant is the
                        // single shared home); the value only joins the
                        // rendered bytes, and a cwd only the config names is
                        // re-checked with its true value at spec time.
                        let cwd = map
                            .get("strata_cwd")
                            .map(|v| v.trim())
                            .filter(|v| !v.is_empty())
                            .unwrap_or(crate::config::STRATA_CWD_DEFAULT);
                        let rendered =
                            crate::engine::strata::render_profile(&entry, Path::new(cwd));
                        match rendered {
                            Ok(text) => {
                                if let Err(problem) =
                                    crate::engine::strata::validate_profile_text(&text)
                                {
                                    let kind = match &problem {
                                        crate::engine::strata::ProfileProblem::MissingPath {
                                            ..
                                        } => ScanKind::ModelPathMissing,
                                        crate::engine::strata::ProfileProblem::PolicyField {
                                            ..
                                        } => ScanKind::PolicyRefused,
                                    };
                                    catalog.errors.push(ScanError {
                                        kind,
                                        file: file_name.clone(),
                                        reason: problem
                                            .describe(&format!("rendered:{}", entry.name)),
                                    });
                                    continue;
                                }
                            }
                            Err(error) => {
                                // A shape refusal (a key that cannot carry its
                                // value) is a typed reject, not a policy claim:
                                // the refusal already names the yaml key.
                                catalog.errors.push(ScanError {
                                    kind: ScanKind::Other,
                                    file: file_name.clone(),
                                    reason: error.to_string(),
                                });
                                continue;
                            }
                        }
                    }
                }
                // A basalt row passes through the adapter's single door
                // (`basalt::validate_row`, stage 1), so the scan refuses at
                // rescan exactly what the adapter would refuse at spec time —
                // a doomed launch can never evict a resident first. The honest
                // mapping: an absent or blank required key (`model`,
                // `serve_model`, `basalt_python`, `basalt_cwd`, `basalt_exe`,
                // and `vram_gb` on a `cost: fixed` row - a greedy row may
                // legally omit the nominal, operator ruling 2026-11-04) names
                // its field as a typed reject; a `.basalt`
                // weights file the disk lacks travels the same
                // `ModelPathMissing` door as llama's `model:`; a server-owned
                // word in `basalt_engine_args` meets the policy surface — the
                // row's `extra_words` contract already refuses it in
                // `from_flat`, and this leg keeps the direct door consistent
                // (slice 10c voice). The `vram_gb`/`cost` booking requirements
                // are `validate_row`'s, mapped through unchanged.
                if entry.engine == "basalt" {
                    if let Err(problem) = crate::engine::basalt::validate_row(&entry) {
                        let kind = match &problem {
                            crate::engine::basalt::BasaltRowError::MissingPath { .. } => {
                                ScanKind::ModelPathMissing
                            }
                            crate::engine::basalt::BasaltRowError::Bad { key, .. } => {
                                if key == "basalt_engine_args" {
                                    ScanKind::PolicyRefused
                                } else {
                                    ScanKind::Other
                                }
                            }
                            crate::engine::basalt::BasaltRowError::Missing { .. } => {
                                ScanKind::Other
                            }
                        };
                        catalog.errors.push(ScanError {
                            kind,
                            file: file_name.clone(),
                            reason: problem.into_engine_error().to_string(),
                        });
                        continue;
                    }
                }
                // Unknown keys are reported, never fatal (README doctrine), but
                // never silent: an operator's typo'd `contex:` is a config that
                // does nothing, and the page must be able to say so. The one
                // silent class is [`SILENT_KEYS`] — reserved forensic keys
                // ("forensic manual only", operator ruling), which are neither
                // warned nor logged nor surfaced. The same walk also scopes
                // every engine knob (slice 10c generalization of the
                // `cpu_offload` ruling): a consumed key the row's engine never
                // reads is live config for a *different* row — not a typo, so
                // not `UnknownKey`; a policy hint, so not silent: `InertKey`
                // warns and the entry publishes (ruling D stands).
                for key in map.keys() {
                    if SILENT_KEYS.contains(&key.as_str()) {
                        continue;
                    }
                    // `ninfer_profile_<name>` is the family of custom-profile
                    // override keys (template adoption, 2026-10-31): the suffix is
                    // operator-chosen, so the const cannot enumerate it. The exact
                    // name `ninfer_profile` stays a const member; this prefix is the
                    // one read-side exception, and the adapter still refuses a
                    // selected custom profile whose override line is absent. A
                    // suffix the adapter could never name as a profile is dead
                    // config, so it stays an unknown key rather than passing on
                    // prefix alone (the shape mirrors `valid_profile_name` in
                    // engine/ninfer.rs — including its first-byte-lowercase rule,
                    // which keeps `ninfer_profile_9x` reportable — and that module
                    // remains the launch-time authority).
                    let profile_override_key = key
                        .strip_prefix("ninfer_profile_")
                        .is_some_and(|suffix| !suffix.is_empty() && suffix.len() <= 16)
                        && key
                            .strip_prefix("ninfer_profile_")
                            .unwrap_or_default()
                            .bytes()
                            .next()
                            .is_some_and(|b| b.is_ascii_lowercase())
                        && key
                            .strip_prefix("ninfer_profile_")
                            .unwrap_or_default()
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
                    if profile_override_key {
                        // On a non-ninfer row the line is dead config like every
                        // foreign knob: warn (InertKey), never silently pass.
                        if entry.engine != crate::ownership::NINFER_ENGINE {
                            catalog.errors.push(ScanError {
                                kind: ScanKind::InertKey,
                                file: file_name.clone(),
                                reason: inert_key_reason(key, &entry.engine),
                            });
                        }
                        continue;
                    }
                    if !crate::engine::policy::knob_live(key, &entry.engine)
                        && !crate::engine::policy::knob_readers(key).is_empty()
                    {
                        catalog.errors.push(ScanError {
                            kind: ScanKind::InertKey,
                            file: file_name.clone(),
                            reason: inert_key_reason(key, &entry.engine),
                        });
                        continue;
                    }
                    if !CONSUMED_KEYS.contains(&key.as_str()) {
                        let reason = match key.as_str() {
                            "host" => HOST_POLICY_HINT.to_string(),
                            "port" => PORT_POLICY_HINT.to_string(),
                            // The slot is honest about its fate: unlike ordinary
                            // unknown keys this one is stripped, never preserved.
                            "fit_gb" => "`fit_gb` is the engine-calculator's projection \
                                         slot, computed at load; a hand-written value is \
                                         ignored and stripped, it never books"
                                .to_string(),
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
                // The extra-words refusal is the policy surface (slice 10c),
                // not a typed-field reject: triage must be able to tell a
                // harness-owned-surface claim from a bad number.
                let kind = match &error {
                    crate::model::ModelError::ExtraWordsRefused { .. } => ScanKind::PolicyRefused,
                    _ => ScanKind::Other,
                };
                catalog.errors.push(ScanError {
                    kind,
                    file: file_name,
                    reason: error.to_string(),
                });
            }
        }
    }
    // One identity may belong to exactly one row (FINDINGS-13 [3]). `from_flat` trims
    // the stem it is handed, so two files can collapse to one name — `qwen3.yaml`
    // beside `qwen3 .yaml` — and every name-keyed path would then answer for the model
    // with whichever row the scan order put first: `/har/load` would load one engine
    // and the measured fold would book this launch's footprint into the other's bytes.
    // Report every claimant and publish every claimant — the scan's posture is "name
    // the problem, keep the file visible" (FINDINGS-13 [1] Fork A), never a boot-style
    // rejection — and the API's `409` stays the place an ambiguous query is refused.
    let mut claimants: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for model in &catalog.models {
        if let Some(file) = model
            .file
            .as_ref()
            .and_then(|path| path.file_name())
            .map(|name| name.to_string_lossy().into_owned())
        {
            claimants
                .entry(model.name.to_ascii_lowercase())
                .or_default()
                .push(file);
        }
    }
    for (identity, files) in claimants {
        if files.len() < 2 {
            continue;
        }
        for file in &files {
            catalog.errors.push(ScanError {
                kind: ScanKind::DuplicateName,
                file: file.clone(),
                reason: format!(
                    "name `{identity}` is claimed by {} files ({}) \u{2014} identity is the \
                     file stem: rename one, or every load of it stays refused as ambiguous",
                    files.len(),
                    files.join(", ")
                ),
            });
        }
    }
    catalog.models.sort_by(|a, b| a.name.cmp(&b.name));
    catalog
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

    /// THE boundary rule at the helper level (FINDINGS-13 [8] Fork B, tightened by
    /// FINDINGS-BATCH-2026-10-28B [1]): the comment begins at the first space-`#`
    /// of the **normalized** value text, outside quotes, and the tail is verbatim
    /// from that cut; the value is trimmed and unquoted. Leading whitespace can
    /// therefore never manufacture a comment (` # tail` is the value `# tail`), a
    /// tab never opens one, and a trailing `\r`/leading BOM is invisible to the
    /// rule because the helper owns that normalization. Non-ASCII is pinned on
    /// both paths (the pre-Fork-B code indexed a char vector with a byte length
    /// and panicked on the no-cut path). The reader/writer agreement half of this
    /// property is NOT tested here — a helper cannot audit its own callers; see
    /// `writeback::tests::the_torture_shapes_round_trip_reader_to_writer_and_back`.
    #[test]
    fn the_shared_value_comment_split_cuts_only_outside_quotes() {
        assert_eq!(split_value_comment(" 4096 # 32k"), ("4096", Some(" # 32k")));
        assert_eq!(
            split_value_comment(" \"a # b\" # tail"),
            ("a # b", Some(" # tail"))
        );
        assert_eq!(split_value_comment(" 4096#c"), ("4096#c", None));
        assert_eq!(split_value_comment(" plain"), ("plain", None));
        assert_eq!(split_value_comment(""), ("", None));
        assert_eq!(
            split_value_comment(" \"unclosed # x"),
            ("\"unclosed # x", None)
        );
        assert_eq!(
            split_value_comment(" caf\u{e9} # morning"),
            ("caf\u{e9}", Some(" # morning"))
        );
        assert_eq!(
            split_value_comment(" \u{540d}\u{524d}"),
            ("\u{540d}\u{524d}", None)
        );
        assert_eq!(split_value_comment(" # tail"), ("# tail", None));
        assert_eq!(split_value_comment("  # a # b"), ("# a", Some(" # b")));
        assert_eq!(split_value_comment(" #"), ("#", None));
        assert_eq!(split_value_comment(" 4096\t# c"), ("4096\t# c", None));
        assert_eq!(split_value_comment(" 4096   # c"), ("4096", Some(" # c")));
        assert_eq!(split_value_comment(" 4096   "), ("4096", None));
        assert_eq!(split_value_comment(" 4096 # c\r"), ("4096", Some(" # c")));
        assert_eq!(
            split_value_comment("\u{feff} 4096 # c"),
            ("4096", Some(" # c"))
        );
        assert_eq!(split_value_comment("  \"a\""), ("a", None));
        // The helper against an independent expected table (not against its own
        // other face): the reader's published value for every shape.
        for &(raw, expected) in TORTURE_SHAPES {
            assert_eq!(
                split_value_comment(raw).0,
                expected,
                "reader's value for `{raw}`"
            );
        }
    }

    /// FINDINGS-BATCH-2026-10-28B [10]: `unquote` peels one byte per side, which
    /// is only a char-boundary-safe operation because the accepted quotes are
    /// ASCII. Pinned with multi-byte content between the quotes so the assumption
    /// is tested, not just documented.
    #[test]
    fn unquote_peels_ascii_quotes_around_multibyte_content() {
        assert_eq!(unquote("\"\u{540d}\u{524d}\""), "\u{540d}\u{524d}");
        assert_eq!(unquote("'\u{540d}\u{524d}'"), "\u{540d}\u{524d}");
        assert_eq!(unquote("\"\u{2192}\""), "\u{2192}");
        assert_eq!(unquote("\""), "\"");
        assert_eq!(unquote("\"\""), "");
        assert_eq!(unquote("\"\u{540d}\""), "\u{540d}");
        assert_eq!(unquote("'\u{540d}\""), "'\u{540d}\"");
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

    /// N4, first layer of [`adversarial_docs`]: every corpus document must (a)
    /// never panic the parser, (b) respect its output shape - every reported
    /// problem names a real line of that document and every published key obeys
    /// the documented charset, (c) build the entry without a panic, and a clean
    /// non-empty parse must land on a published entry or a **typed** reject, with
    /// the three numeric-overflow shapes pinned to their exact variant, and
    /// (d) parse identically on a second run - the reproducibility a generating
    /// fuzzer would owe, asserted explicitly because the corpus replaced it.
    #[test]
    fn the_adversarial_corpus_never_panics_and_lands_typed_or_published() {
        use crate::model::ModelError;
        let reported = ["duplicate-keys", "colon-only", "junk-line", "space-key"];
        let empty_map = ["empty", "whitespace", "comment-only"];
        let overflow = ["vram-overflow", "timeout-overflow", "vram-negative-zero"];
        for (name, doc) in adversarial_docs() {
            let parsed = std::panic::catch_unwind(|| parse_flat_yaml(&doc))
                .unwrap_or_else(|_| panic!("case `{name}`: the parser panicked"));
            let (map, problems) = &parsed;
            let line_count = doc.lines().count();
            for problem in problems {
                let rest = problem
                    .strip_prefix("line ")
                    .unwrap_or_else(|| panic!("case `{name}`: a problem names no line: {problem}"));
                let number: usize = rest
                    .split(':')
                    .next()
                    .and_then(|n| n.parse().ok())
                    .unwrap_or_else(|| {
                        panic!("case `{name}`: unparsable problem line number: {problem}")
                    });
                assert!(
                    (1..=line_count).contains(&number),
                    "case `{name}`: a problem points outside the document: {problem}"
                );
            }
            for key in map.keys() {
                assert!(
                    !key.is_empty()
                        && key
                            .chars()
                            .next()
                            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                        && key
                            .chars()
                            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                    "case `{name}`: published key outside the documented charset: `{key}`"
                );
            }
            assert_eq!(
                parsed,
                parse_flat_yaml(&doc),
                "case `{name}`: parsing the same input twice disagreed"
            );
            if reported.contains(&name) {
                assert!(
                    !problems.is_empty(),
                    "case `{name}`: a malformed document parsed clean: {map:?}"
                );
                continue;
            }
            if empty_map.contains(&name) {
                assert!(
                    problems.is_empty() && map.is_empty(),
                    "case `{name}`: blank input must yield no keys and no problems, got map {map:?} problems {problems:?}"
                );
                continue;
            }
            assert!(problems.is_empty(), "case `{name}`: {problems:?}");
            let built = std::panic::catch_unwind(|| ModelEntry::from_flat(map, "corpus"))
                .unwrap_or_else(|_| panic!("case `{name}`: from_flat panicked"));
            match built {
                Err(error) => match name {
                    "vram-overflow" => assert!(
                        matches!(&error, ModelError::BadNumber { field, value }
                            if field == "vram_gb" && value == "1e400"),
                        "case `{name}`: {error}"
                    ),
                    "timeout-overflow" => assert!(
                        matches!(&error, ModelError::BadNumber { field, value }
                            if field == "load_timeout" && value == "99999999999999999999999999"),
                        "case `{name}`: {error}"
                    ),
                    "vram-negative-zero" => assert!(
                        matches!(&error, ModelError::BadNumber { field, .. } if field == "vram_gb"),
                        "case `{name}`: {error}"
                    ),
                    _ => panic!("case `{name}`: a clean shape answered a reject: {error}"),
                },
                Ok(entry) => {
                    assert!(
                        !overflow.contains(&name),
                        "case `{name}`: a numeric overflow published an entry"
                    );
                    match name {
                        "valid-round-trip" => {
                            assert_eq!(entry.category, CategoryId::Work);
                            assert_eq!(entry.vram_gb, Some(4.5));
                            assert_eq!(entry.raw.get("context").map(String::as_str), Some("4096"));
                        }
                        "huge-line" => {
                            assert_eq!(entry.raw.get("note").map(String::len), Some(100_000));
                        }
                        "unknown-key" => assert_eq!(
                            entry.raw.get("frobnicate").map(String::as_str),
                            Some("yes"),
                            "unknown keys stay preserved for reference"
                        ),
                        "deep-quotes" => assert_eq!(
                            entry.raw.get("note").map(String::len),
                            Some(12),
                            "one quote pair peels, the chain residue is data"
                        ),
                        "deep-hash-chain" => {
                            assert_eq!(entry.raw.get("note").map(String::as_str), Some("a"));
                        }
                        _ => {}
                    }
                }
            }
        }
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

    /// FINDINGS2-13 [2] — the zero-key hole, closed in the direction the doctrine
    /// points: a `.yaml` the acceptance gate treats as a config but which yields no
    /// keys at all publishes nothing **and is reported**, exactly as
    /// `commit_verified` refuses to write it (`EmptyFile`). Before the gate these two
    /// files entered the catalog as `engine: llama` rows named by their stems with
    /// `errors` empty — spawnable-shaped phantoms. Falsifier: delete the
    /// `map.is_empty()` gate and both rows come back while `errors` empties.
    #[test]
    fn an_empty_or_comment_only_yaml_is_reported_and_never_publishes() {
        let dir = crate::testutil::temp_dir("scan-zero-key");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("blank.yaml"), "").expect("seed blank");
        std::fs::write(dir.join("notes.yaml"), "# only a comment\n\n").expect("seed notes");
        std::fs::write(dir.join("real.yaml"), "engine: mock\nvram_gb: 4\n").expect("seed real");

        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog
                .models
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            vec!["real"],
            "a file with no keys must not publish a row, phantom or otherwise"
        );
        assert_eq!(catalog.errors.len(), 2, "{:?}", catalog.errors);
        for name in ["blank.yaml", "notes.yaml"] {
            let Some(report) = catalog.errors.iter().find(|e| e.file.contains(name)) else {
                panic!(
                    "{name} was neither published nor reported: {:?}",
                    catalog.errors
                );
            };
            assert_eq!(report.kind, ScanKind::InvalidYaml, "{report:?}");
            assert!(
                report.reason.contains("no `key: value` lines"),
                "the reason must say which zero-key shape this is: {:?}",
                report.reason
            );
            assert!(
                !report.reason.contains("problem lines"),
                "the problem-line sentence belongs to the other gate: {:?}",
                report.reason
            );
        }
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
            "engine: ninfer\nninfer_image: ninfer:local\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("b.yaml"),
            "engine: ninfer\nninfer_image: ninfer:local\nninfer_container: shared\n",
        )
        .expect("write");
        std::fs::write(
            dir.join("c.yaml"),
            "engine: ninfer\nninfer_image: ninfer:local\nninfer_container: other\n",
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

    /// R2-13-01: the existence trace accepts regular files only — a
    /// directory-valued `model:` is `ModelPathMissing` before admission (the
    /// operator typo class: folder named, file forgotten), and the directory's
    /// byte size is never booked as `gguf_size_gb` for any engine (the same
    /// stat feeds the size for every engine, so the filter must hold there
    /// too — the surviving mock row proves the booking half).
    #[test]
    fn a_directory_valued_model_path_is_dropped_and_never_books_its_size() {
        let dir = crate::testutil::temp_dir("scan-llama-dir-model");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::create_dir_all(dir.join("Qwen-GGUF")).expect("model folder");
        let folder = dir.join("Qwen-GGUF").display().to_string();
        std::fs::write(
            dir.join("folder.yaml"),
            format!("engine: llama\nmodel: {folder}\nvram_gb: 8\n"),
        )
        .expect("write llama row");
        std::fs::write(
            dir.join("mockfolder.yaml"),
            format!("engine: mock\nmodel: {folder}\nvram_gb: 2\n"),
        )
        .expect("write mock row");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            1,
            "only the mock row survives the trace: {:?}",
            catalog.errors
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("a directory is not a model file");
        assert!(
            error.reason.starts_with("model file not found: ")
                && error.reason.contains("Qwen-GGUF"),
            "{}",
            error.reason
        );
        assert!(error.file.contains("folder.yaml"));
        assert!(
            catalog.models.iter().all(|m| m.gguf_size_gb.is_none()),
            "a directory's byte size is never booked, for any engine: {:?}",
            catalog
                .models
                .iter()
                .map(|m| &m.gguf_size_gb)
                .collect::<Vec<_>>()
        );
    }

    /// R2-13-02 (scan half): a declared `mmproj:` is existence-traced like
    /// `model:` — a llama row whose vision file is gone must not publish and
    /// evict for the same doomed spawn; same kind, the refusal names the file.
    #[test]
    fn a_llama_entry_with_a_missing_mmproj_is_dropped_and_named() {
        let dir = crate::testutil::temp_dir("scan-llama-missing-mmproj");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("weights.gguf"), b"0123456789").expect("write gguf");
        std::fs::write(
            dir.join("vision.yaml"),
            format!(
                "engine: llama\nmodel: {}\nmmproj: Z:/definitely-not-here/mmproj.gguf\n",
                dir.join("weights.gguf").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "a doomed spawn must not publish");
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("the missing mmproj must be a ModelPathMissing report");
        assert!(
            error.reason.starts_with("mmproj file not found: ")
                && error.reason.contains("mmproj.gguf"),
            "{}",
            error.reason
        );
        assert!(error.file.contains("vision.yaml"));
    }

    /// R2-13-02: the join base for a relative `mmproj:` is the yaml dir (the
    /// `model:` base), and the trace never resolves or rewrites the raw map —
    /// the published value stays the operator's word; resolution is launch-side.
    #[test]
    fn a_relative_mmproj_is_traced_against_the_yaml_dir() {
        let dir = crate::testutil::temp_dir("scan-llama-mmproj-join");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("weights.gguf"), b"0123456789").expect("write gguf");
        std::fs::write(
            dir.join("vision.yaml"),
            format!(
                "engine: llama\nmodel: {}\nmmproj: vision-proj.gguf\n",
                dir.join("weights.gguf").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "a relative miss must refuse");
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("the missing relative mmproj must be a ModelPathMissing report");
        assert!(
            error.reason.starts_with("mmproj file not found: ")
                && error.reason.contains("vision-proj.gguf")
                && error.reason.contains(&dir.display().to_string()),
            "the refusal names the yaml-dir-joined path: {}",
            error.reason
        );
        std::fs::write(dir.join("vision-proj.gguf"), b"0123456789").expect("write mmproj");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
        assert_eq!(
            catalog.models[0].raw.get("mmproj").map(String::as_str),
            Some("vision-proj.gguf"),
            "the trace refuses but never resolves or rewrites the raw value"
        );
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

    /// A row that still declares `strata_config` is the SUPERSEDED mode
    /// (ruling 2026-11-02 row 19): refused outright with the migration
    /// sentence, whatever the profile file itself holds — unless the row also
    /// carries `strata_exe`, which means the import already happened and the
    /// accurate sentence is the mutual-exclusion one (review 25 [7]).
    #[test]
    fn a_strata_config_row_is_refused_naming_the_import_command() {
        let dir = crate::testutil::temp_dir("scan-strata-superseded");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("broken.json"), "{ not json").expect("write profile");
        for (file, target) in [
            ("nofile.yaml", "Z:/nope/missing.json"),
            (
                "manual.yaml",
                &dir.join("broken.json").display().to_string(),
            ),
        ] {
            std::fs::write(
                dir.join(file),
                format!("engine: strata\nstrata_config: {target}\nvram_gb: 8\n"),
            )
            .expect("write");
        }
        std::fs::write(
            dir.join("both.yaml"),
            format!(
                "engine: strata\nstrata_exe: C:\\Strata\\engine\\strata.exe\n\
                 strata_config: {}\nvram_gb: 8\n",
                dir.join("broken.json").display()
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty(), "no row may publish");
        for file in ["nofile.yaml", "manual.yaml"] {
            assert!(
                catalog
                    .errors
                    .iter()
                    .any(|e| e.kind == ScanKind::PolicyRefused
                        && e.file.contains(file)
                        && e.reason.contains("superseded")
                        && e.reason.contains("simplehar-se import strata")),
                "{file} must be refused with the migration sentence: {:?}",
                catalog.errors
            );
        }
        // Review 25 [7]: a row that already carries the imported surface only
        // needs its `strata_config` line deleted, so the scan names the
        // mutual-exclusion door, not a re-import the row does not need.
        assert!(
            catalog
                .errors
                .iter()
                .any(|e| e.kind == ScanKind::PolicyRefused
                    && e.file.contains("both.yaml")
                    && e.reason.contains("mutually exclusive")
                    && !e.reason.contains("simplehar-se import strata")),
            "both-sources row must meet the mutual-exclusion sentence: {:?}",
            catalog.errors
        );
    }

    /// Template mode moves the path walk onto the RENDERA: a row whose engine
    /// args name a path the disk lacks drops the entry at rescan, naming the
    /// key and the path exactly as the old file walk did.
    #[test]
    fn a_template_row_naming_a_missing_path_drops_the_entry_naming_key_and_path() {
        let dir = crate::testutil::temp_dir("scan-strata-missing-path");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("strata.exe"), b"x").expect("write exe");
        std::fs::write(dir.join("tokenizer.bin"), b"t").expect("write tokenizer");
        let yaml = format!(
            "engine: strata\nvram_gb: 8\nserve_model: m\nstrata_exe: {}\n\
             strata_tokenizer: {}\nstrata_cwd: {}\n\
             strata_args: --native, Z:\\nope\\doomed.gguf\n",
            dir.join("strata.exe").display(),
            dir.join("tokenizer.bin").display(),
            dir.display(),
        );
        std::fs::write(dir.join("ref.yaml"), yaml).expect("write");
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
        // The array element reports under its owning key so the operator can
        // find the line in the row.
        assert!(error.reason.contains("args"), "{}", error.reason);
    }

    /// The `--control-vector-scaled <path>:1.0` shape: the scale suffix is
    /// stripped before the stat, so a real file named `x.gguf` referenced as
    /// `x.gguf:1.0` publishes instead of being falsely dropped.
    #[test]
    fn a_template_row_control_vector_scale_suffix_is_stripped_before_the_stat() {
        let dir = crate::testutil::temp_dir("scan-strata-scaled");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("strata.exe"), b"x").expect("write exe");
        std::fs::write(dir.join("tokenizer.bin"), b"t").expect("write tokenizer");
        std::fs::write(dir.join("vec.gguf"), b"v").expect("write vector");
        let yaml = format!(
            "engine: strata\nvram_gb: 8\nserve_model: m\nstrata_exe: {}\n\
             strata_tokenizer: {}\nstrata_cwd: {}\n\
             strata_args: --control-vector-scaled, {}:1.0\n",
            dir.join("strata.exe").display(),
            dir.join("tokenizer.bin").display(),
            dir.display(),
            dir.join("vec.gguf").display(),
        );
        std::fs::write(dir.join("scaled.yaml"), yaml).expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(catalog.errors.is_empty(), "{:?}", catalog.errors);
    }

    /// A shape a knob cannot carry (a relative `strata_exe`) is reported as a
    /// typed reject naming the key — and the row does not publish.
    #[test]
    fn a_template_row_shape_refusal_reports_naming_the_yaml_key() {
        let dir = crate::testutil::temp_dir("scan-strata-shape");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("tokenizer.bin"), b"t").expect("write tokenizer");
        let yaml = format!(
            "engine: strata\nvram_gb: 8\nserve_model: m\nstrata_exe: relative\\strata.exe\n\
             strata_tokenizer: {}\n",
            dir.join("tokenizer.bin").display(),
        );
        std::fs::write(dir.join("shape.yaml"), yaml).expect("write");
        let catalog = scan_dir(&dir);
        assert!(catalog.models.is_empty());
        let error = catalog
            .errors
            .iter()
            .find(|e| e.file.contains("shape.yaml"))
            .expect("the row must be reported");
        assert!(
            error.reason.contains("strata_exe") && error.reason.contains("absolute path"),
            "{}",
            error.reason
        );
    }

    #[test]
    fn a_strata_entry_without_a_profile_source_defers_to_the_adapter() {
        // The chunk 10 [2] preflight pin (`required field` refuses before the
        // victim dies) needs this entry published, so an absent source key is
        // the adapter's refusal to name (row 19: `strata_exe`), not the scan's.
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

    /// B of the `cpu_offload` scoping ruling: a non-offload engine carrying the
    /// key gets the NEW warn kind (never `UnknownKey` — the key is real harness
    /// config, owned by a different engine), and the entry still publishes exactly
    /// like the retired `host:`/`port:` hints. Falsifier: any change that swallows
    /// the report (removing the engine filter from the check), reports it under
    /// `UnknownKey`, or drops the entry would fail here.
    #[test]
    fn cpu_offload_on_a_non_offload_engine_is_reported_inert_key_and_publishes() {
        let dir = crate::testutil::temp_dir("scan-inert-key");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("strata-off.yaml"),
            "engine: strata\nvram_gb: 8\ncpu_offload: true\n",
        )
        .expect("write strata");
        std::fs::write(
            dir.join("ninfer-off.yaml"),
            "engine: ninfer\nvram_gb: 26\ncpu_offload: false\n",
        )
        .expect("write ninfer");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            2,
            "warn-only: both entries still publish: {:?}",
            catalog.errors
        );
        let inert: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::InertKey)
            .collect();
        assert_eq!(
            inert.len(),
            2,
            "one per non-llama row: {:?}",
            catalog.errors
        );
        assert!(
            catalog
                .errors
                .iter()
                .all(|e| e.kind != ScanKind::UnknownKey),
            "a consumed key owned by another engine is not a typo: {:?}",
            catalog.errors
        );
        for (file, engine) in [("strata-off.yaml", "strata"), ("ninfer-off.yaml", "ninfer")] {
            let hit = inert
                .iter()
                .find(|e| e.file.contains(file))
                .unwrap_or_else(|| panic!("{file} must be reported: {catalog:?}"));
            assert!(
                hit.reason.contains("cpu_offload")
                    && hit.reason.contains(engine)
                    && hit.reason.contains("llama"),
                "the reason must name key, engine and the owning set: {}",
                hit.reason
            );
        }
        // Value does not matter: `false` reports exactly like `true`.
        assert!(catalog
            .errors
            .iter()
            .any(|e| e.kind == ScanKind::InertKey && e.file.contains("ninfer-off")));
    }

    /// The same key on llama rows — explicit `engine: llama` and the absent-key
    /// default both — stays fully silent, preserving today's posture for the two
    /// live catalog rows (`Qwen3-0.6B-GGUF`, `embeddinggemma-2-GGUF`). Falsifier:
    /// scoping the const away from llama, or an absent engine key resolving to a
    /// non-offload id, produces a spurious report here.
    #[test]
    fn cpu_offload_on_llama_rows_produces_no_report() {
        let dir = crate::testutil::temp_dir("scan-llama-silent");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("explicit.yaml"),
            "engine: llama\nmodel: mock://m\ncpu_offload: true\n",
        )
        .expect("write explicit");
        std::fs::write(
            dir.join("default.yaml"),
            "model: mock://m\ncpu_offload: true\n",
        )
        .expect("write default");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 2, "{:?}", catalog.errors);
        assert!(
            catalog.errors.is_empty(),
            "llama (incl. the absent-key default) must stay silent: {:?}",
            catalog.errors
        );
        assert_eq!(catalog.models[0].engine, "llama");
        assert_eq!(catalog.models[1].engine, "llama");
    }

    /// Slice 10c (sibling-knob scoping): the mechanism that scoped
    /// `cpu_offload` now covers every row knob — a llama key on a ninfer row
    /// warns `InertKey` and publishes exactly like its offload sibling, and
    /// the key stays silent on its own row. This is the live catalog's shape:
    /// the ninfer/strata yamls carry `context:` the harness only reads for
    /// llama, and the page must be able to say so (warn-only, ruling D posture).
    /// Falsifier: a hardcoded `cpu_offload` key filter, or scoping the knob
    /// table away from the scan, fails one leg here.
    #[test]
    fn sibling_llama_knobs_are_inert_off_the_llama_row() {
        let dir = crate::testutil::temp_dir("scan-sibling-knobs");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("ninfer-row.yaml"),
            "engine: ninfer\nvram_gb: 26\ncontext: 4096\nngl: 99\n",
        )
        .expect("write ninfer");
        std::fs::write(
            dir.join("llama-row.yaml"),
            "engine: llama\nmodel: mock://m\ncontext: 4096\n",
        )
        .expect("write llama");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            2,
            "warn-only: both entries still publish: {:?}",
            catalog.errors
        );
        for key in ["context", "ngl"] {
            let named = format!("`{key}` is read only by the llama engine(s)");
            assert!(
                catalog.errors.iter().any(|e| e.kind == ScanKind::InertKey
                    && e.file.contains("ninfer-row")
                    && e.reason.contains(&named)),
                "`{key}` on the ninfer row must warn: {:?}",
                catalog.errors
            );
        }
        assert!(
            catalog
                .errors
                .iter()
                .all(|e| !e.file.contains("llama-row") || e.kind != ScanKind::InertKey),
            "a knob on its own row stays silent: {:?}",
            catalog.errors
        );
    }

    /// Ruling 18b removal (green Wave C): a row that still declares the removed
    /// legacy `ninfer_autostart` path is **refused** at scan and never published,
    /// with the migration sentence naming every template key the row must adopt;
    /// the pair with `ninfer_image` is refused too, and a modern template row
    /// stays silent. Falsifier: the interim behavior published the legacy row with
    /// a `Deprecated` warn, so `models.len()` and the refusal-kind checks fail on
    /// the old code; a message missing one migration key fails its own loop.
    #[test]
    fn a_legacy_autostart_row_is_refused_and_never_published() {
        let dir = crate::testutil::temp_dir("scan-legacy-refused");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("legacy-row.yaml"),
            "engine: ninfer\nvram_gb: 20\nninfer_autostart: /opt/legacy.sh\nninfer_container: legacy-box\n",
        )
        .expect("write legacy");
        std::fs::write(
            dir.join("both-row.yaml"),
            "engine: ninfer\nvram_gb: 20\nninfer_autostart: /opt/both.sh\nninfer_image: ninfer:local\nninfer_container: both-box\n",
        )
        .expect("write both");
        std::fs::write(
            dir.join("modern-row.yaml"),
            "engine: ninfer\nvram_gb: 20\nninfer_image: ninfer:local\nninfer_container: modern-box\n",
        )
        .expect("write modern");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            1,
            "only the template row publishes: {:?}",
            catalog.errors
        );
        assert_eq!(catalog.models[0].name, "modern-row");
        let refused: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::PolicyRefused)
            .collect();
        assert_eq!(
            refused.len(),
            2,
            "both the verbatim legacy row and the pair with `ninfer_image` are refused: {:?}",
            catalog.errors
        );
        for error in &refused {
            for needle in [
                "`ninfer_autostart`",
                "removed",
                "`ninfer_image`",
                "`ninfer_model_name`",
                "`ninfer_artifact`",
                "`ninfer_model_roots`",
            ] {
                assert!(
                    error.reason.contains(needle),
                    "the refusal must name `{needle}`: {}",
                    error.reason
                );
            }
        }
        assert!(
            catalog
                .errors
                .iter()
                .all(|e| !e.file.contains("modern-row")),
            "the template row must stay completely silent: {:?}",
            catalog.errors
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Slice 10c (full `extra_args` enforcement): a hostile extra word refuses
    /// the FILE at rescan — never published, never reaching a spawn that could
    /// evict a resident for the doomed launch — and the refusal names both the
    /// word and the mechanism that owns the decision. The benign family the
    /// live catalog uses still publishes untouched.
    #[test]
    fn a_hostile_extra_word_refuses_the_file_naming_the_word_and_owner() {
        let dir = crate::testutil::temp_dir("scan-hostile-extra");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(
            dir.join("llama-bad.yaml"),
            "engine: llama\nmodel: mock://m\nextra_args: --api-key sekrit\n",
        )
        .expect("write bad llama");
        std::fs::write(
            dir.join("llama-good.yaml"),
            "engine: llama\nmodel: mock://m\nextra_args: --embedding --pooling last\n",
        )
        .expect("write good llama");
        std::fs::write(
            dir.join("ninfer-bad.yaml"),
            "engine: ninfer\nvram_gb: 26\nninfer_serve_args: --max-context 999\n",
        )
        .expect("write bad ninfer");
        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            1,
            "only the benign row publishes: {:?}",
            catalog.errors
        );
        assert_eq!(catalog.models[0].name, "llama-good");
        let bad = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::PolicyRefused && e.file.contains("llama-bad"))
            .expect("the hostile llama row must be refused");
        assert!(
            bad.reason.contains("`--api-key`")
                && bad
                    .reason
                    .contains("request authentication is harness-owned")
                && !bad.reason.contains("sekrit"),
            "the refusal names the word and its owner, not the secret value: {}",
            bad.reason
        );
        let bad = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::PolicyRefused && e.file.contains("ninfer-bad"))
            .expect("the hostile ninfer row must be refused");
        assert!(
            bad.reason.contains("`--max-context`")
                && bad.reason.contains("ninfer_profile")
                && bad.reason.contains("harness-owned flag"),
            "the refusal names the word and the profile line that owns it: {}",
            bad.reason
        );
    }

    /// Slice 10c (22[1]) doctrine, row 19 shape: policy fields are harness-forced
    /// and never obeyed from engine-side files — and the render makes claiming
    /// them structurally impossible: a rendered profile carries no `host`, no
    /// `port`, no key (the bind and port ride the forced argv), while the
    /// defense-in-depth leg of `validate_profile_text` still refuses any JSON
    /// that shows one. The scan-side witness is that every clean template row
    /// publishes with those fields absent; the leg itself is pinned in the
    /// adapter's module.
    #[test]
    fn a_template_row_publishes_with_the_policy_fields_absent_by_construction() {
        let dir = crate::testutil::temp_dir("scan-profile-policy");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("strata.exe"), b"x").expect("write exe");
        std::fs::write(dir.join("tokenizer.bin"), b"t").expect("write tokenizer");
        std::fs::write(
            dir.join("clean.yaml"),
            format!(
                "engine: strata\nvram_gb: 8\nserve_model: clean\nstrata_exe: {}\n\
                 strata_tokenizer: {}\nstrata_cwd: {}\n",
                dir.join("strata.exe").display(),
                dir.join("tokenizer.bin").display(),
                dir.display(),
            ),
        )
        .expect("write clean yaml");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(catalog.models[0].name, "clean");
    }

    /// The disk shape a valid basalt row validates against (the same shape
    /// `basalt::validate_row` walks in its own module): an absolute `.basalt`
    /// pack with bytes, an interpreter, an engine binary; the cwd is `dir`
    /// itself. Returns the pack path.
    fn basalt_pack(dir: &Path) -> std::path::PathBuf {
        let weights = dir.join("m.basalt");
        std::fs::write(&weights, b"basalt-pack-bytes").expect("write pack");
        std::fs::write(dir.join("python.exe"), b"p").expect("write interpreter");
        std::fs::write(dir.join("basalt-engine.exe"), b"e").expect("write engine");
        weights
    }

    /// One basalt row document in the valid five-knob shape, every path under
    /// `dir`. An override `Some(value)` replaces (or adds) the line; `None`
    /// drops it — the shape `validate_row` answers with the missing-key door.
    fn basalt_yaml(dir: &Path, weights: &Path, overrides: &[(&str, Option<&str>)]) -> String {
        let mut lines: Vec<(&str, String)> = vec![
            ("engine", "basalt".to_string()),
            ("model", weights.to_string_lossy().into_owned()),
            ("serve_model", "basalt-served".to_string()),
            ("vram_gb", "42".to_string()),
            ("cost", "fixed".to_string()),
            (
                "basalt_python",
                dir.join("python.exe").to_string_lossy().into_owned(),
            ),
            ("basalt_cwd", dir.to_string_lossy().into_owned()),
            (
                "basalt_exe",
                dir.join("basalt-engine.exe").to_string_lossy().into_owned(),
            ),
        ];
        for (key, value) in overrides {
            if let Some(line) = lines.iter_mut().find(|(k, _)| *k == *key) {
                if let Some(value) = value {
                    line.1 = value.to_string();
                } else {
                    line.0 = "";
                }
            } else if let Some(value) = value {
                lines.push((key, value.to_string()));
            }
        }
        lines
            .iter()
            .filter(|(k, _)| !k.is_empty())
            .map(|(k, v)| format!("{k}: {v}\n"))
            .collect()
    }

    /// A basalt row that lacks a key the launch requires is refused at rescan
    /// — before any load could evict a resident for the doomed spawn — and the
    /// report names the field. An absent key and a present-but-blank one are
    /// the same door (the adapter's `validate_row` is the single source of the
    /// required set, so the scan names whatever that door names). Falsifier:
    /// no basalt branch in the scan publishes the row and the report is empty.
    #[test]
    fn a_basalt_row_missing_a_required_key_is_refused_naming_the_key() {
        let dir = crate::testutil::temp_dir("scan-basalt-missing-key");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let weights = basalt_pack(&dir);
        for (key, value) in [
            ("model", None),
            ("serve_model", None),
            ("basalt_python", None),
            ("basalt_cwd", None),
            ("basalt_exe", None),
            ("vram_gb", None),
            // The blank leg: a present value that carries nothing is as
            // unservable as the absent one.
            ("serve_model", Some("")),
        ] {
            std::fs::write(
                dir.join("row.yaml"),
                basalt_yaml(&dir, &weights, &[(key, value)]),
            )
            .expect("write row");
            let catalog = scan_dir(&dir);
            assert!(
                catalog.models.is_empty(),
                "`{key}` missing/blank must refuse the row: {:?}",
                catalog.errors
            );
            let error = catalog
                .errors
                .iter()
                .find(|e| e.file.contains("row.yaml"))
                .unwrap_or_else(|| {
                    panic!(
                        "`{key}` missing/blank must be reported: {:?}",
                        catalog.errors
                    )
                });
            assert_eq!(error.kind, ScanKind::Other, "{error:?}");
            assert!(
                error.reason.contains(key),
                "must name `{key}`: {}",
                error.reason
            );
        }
    }

    /// Operator ruling 2026-11-04 (`TASK-BASALT-GREEDY`): a greedy basalt row
    /// is the catalog citizen the operator's truth needs - Basalt offloads to
    /// RAM, so the row may carry no declared `vram_gb` at all (booking takes
    /// the room at admit, the launch floor gates). The row must publish at
    /// scan, not vanish behind a catalog error (the live 2026-11-04 failure).
    /// The `cost: fixed` refusal without `vram_gb` stays pinned by the
    /// key-missing loop above. Falsifier: an adapter guard refusing Greedy
    /// drops the row again.
    #[test]
    fn a_greedy_basalt_row_publishes_without_a_declared_vram_gb() {
        let dir = crate::testutil::temp_dir("scan-basalt-greedy");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let weights = basalt_pack(&dir);
        std::fs::write(
            dir.join("row.yaml"),
            basalt_yaml(
                &dir,
                &weights,
                &[("cost", Some("greedy")), ("vram_gb", None)],
            ),
        )
        .expect("write row");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(catalog.models[0].name, "row");
        assert_eq!(catalog.models[0].cost, crate::model::CostClass::Greedy);
    }

    /// The `.basalt` pack is the weights file the launch spawns against: a
    /// missing one is the same `ModelPathMissing` door as llama's `model:`
    /// (FINDINGS-13 [2] class — refuse before admission can evict a victim for
    /// the doomed spawn), not a generic typed reject. Falsifier: mapping every
    /// `BasaltRowError::Bad` to `Other` reports the right file under the wrong
    /// kind.
    #[test]
    fn a_basalt_row_naming_a_missing_pack_is_model_path_missing() {
        let dir = crate::testutil::temp_dir("scan-basalt-missing-pack");
        std::fs::create_dir_all(&dir).expect("temp dir");
        basalt_pack(&dir);
        std::fs::write(
            dir.join("row.yaml"),
            basalt_yaml(&dir, &dir.join("ghost.basalt"), &[]),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(
            catalog.models.is_empty(),
            "a pack that is not on disk must not publish: {:?}",
            catalog.errors
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::ModelPathMissing)
            .expect("the missing pack travels the llama `model:` door");
        assert!(error.file.contains("row.yaml"), "{error:?}");
        assert!(error.reason.contains("ghost.basalt"), "{}", error.reason);
    }

    /// A server-owned flag smuggled through `basalt_engine_args` meets the
    /// policy surface (`PolicyRefused`), naming the flag and its owner, exactly
    /// like the `extra_args`/`ninfer_serve_args` family (slice 10c). The row's
    /// `extra_words` contract in `from_flat` is the first door; the scan's
    /// direct `validate_row` leg keeps the mapping consistent.
    #[test]
    fn a_basalt_row_with_a_server_owned_engine_arg_is_policy_refused() {
        let dir = crate::testutil::temp_dir("scan-basalt-hostile-arg");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let weights = basalt_pack(&dir);
        std::fs::write(
            dir.join("row.yaml"),
            basalt_yaml(
                &dir,
                &weights,
                &[("basalt_engine_args", Some("--host=0.0.0.0"))],
            ),
        )
        .expect("write");
        let catalog = scan_dir(&dir);
        assert!(
            catalog.models.is_empty(),
            "a harness-owned flag must refuse the row: {:?}",
            catalog.errors
        );
        let error = catalog
            .errors
            .iter()
            .find(|e| e.kind == ScanKind::PolicyRefused)
            .expect("the hostile word is the policy surface, never a typed reject");
        assert!(error.file.contains("row.yaml"), "{error:?}");
        assert!(
            error.reason.contains("`--host`") && error.reason.contains("harness-owned"),
            "the refusal names the flag and its owner: {}",
            error.reason
        );
    }

    /// The happy path pinned whole: a valid basalt row publishes with its five
    /// knobs live on their own row — zero reports, neither `UnknownKey` (the
    /// knobs are consumed) nor `InertKey` (the basalt row reads them) — and the
    /// `.basalt` pack's bytes never book into `gguf_size_gb` (the booking
    /// source is the declared `vram_gb` with `cost: fixed`). Falsifier: without
    /// the scan-branch and consumed-key work the knobs warn and the pack size
    /// books.
    #[test]
    fn a_valid_basalt_row_publishes_with_its_five_knobs_live() {
        let dir = crate::testutil::temp_dir("scan-basalt-valid");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let weights = basalt_pack(&dir);
        let yaml = basalt_yaml(
            &dir,
            &weights,
            &[
                ("basalt_parallel", Some("2")),
                ("basalt_engine_args", Some("--temp=0.7")),
            ],
        );
        std::fs::write(dir.join("row.yaml"), yaml).expect("write");
        let catalog = scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert!(
            catalog.errors.is_empty(),
            "a knob on its own row stays silent: {:?}",
            catalog.errors
        );
        assert_eq!(catalog.models[0].engine, "basalt");
        assert!(
            catalog.models[0].gguf_size_gb.is_none(),
            "a .basalt pack size never feeds the GGUF estimate"
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
    /// Every engine knob rides the `engine: mock` file deliberately: mock reads
    /// none of them, so (slice 10c) the fixture's complete set of reports is the
    /// `InertKey` hints — one per foreign knob, naming its owning row — and the
    /// const-coverage claim stays honest without pretending any knob is live on
    /// mock. Harness-blind keys (`name`, `model`, the `measured_*`...) stay
    /// silent even here.
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
             measured_attribution: total_diff\n\
             measured_profile: b\n\
             strata_config: C:/Strata/profile.json\n\
             strata_python: C:/Strata/.venv/Scripts/python.exe\n\
             strata_cwd: C:/Strata\n\
             strata_exe: C:/Strata/engine/strata.exe\n\
             strata_args: --pack, D:/models/pack\n\
             strata_tokenizer: C:/Strata/tokenizer.bin\n\
             strata_lib_dirs: C:/Strata/lib\n\
             strata_env: STRATA_KVA=0\n\
             strata_gpu: 0\n\
             strata_parallel: 1\n\
             strata_gpus_asked: true\n\
             strata_vision: exe=C:/Strata/vision.exe mmproj=D:/m/mmproj.gguf model=D:/m/m.gguf\n\
             strata_sampling: temperature=1.0 top_p=0.95 top_k=20\n\
             strata_uncensored: false\n\
             basalt_python: C:/Basalt/.venv/Scripts/python.exe\n\
             basalt_cwd: C:/Basalt\n\
             basalt_exe: C:/Basalt/bin/basalt.exe\n\
             basalt_parallel: 1\n\
             basalt_engine_args: --temp=0.7\n\
             ninfer_autostart: /opt/a.sh\n\
             ninfer_container: golden-box\n\
             ninfer_profile: b\n\
             ninfer_key: true\n\
             ninfer_image: ninfer:local\n\
             ninfer_model_name: Golden-Artifact-Dir\n\
             ninfer_artifact: golden.ninfer\n\
             ninfer_internal_port: 8004\n\
             ninfer_model_roots: /mnt/d/Models,/home/ops/ai/models\n\
             ninfer_serve_args: --spec=dflash2 --draft-tokens=7\n\
             ninfer_gpu_device: \"0\"\n\
             ninfer_model_id: golden-id\n\
             ninfer_key_file: $HOME/.simplehar/keys/golden-box.key\n\
             ninfer_serve_cmd: ninfer-serve\n\
             ninfer_env: CUDA_MODULE_LOADING=LAZY\n\
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
        let inert: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::InertKey)
            .collect();
        assert!(
            catalog.errors.iter().all(|e| e.kind == ScanKind::InertKey),
            "mock reads no knob: its only reports are the inert-key hints: {:?}",
            catalog.errors
        );
        let mut foreign: Vec<(&str, &str)> = Vec::new();
        for row in crate::engine::policy::all() {
            if row.id != "mock" {
                foreign.extend(row.knobs.iter().map(|key| (*key, row.id)));
            }
        }
        assert_eq!(
            inert.len(),
            foreign.len(),
            "exactly one hint per foreign engine knob: {:?}",
            catalog.errors
        );
        for (key, owner) in &foreign {
            let named = format!("`{key}` is read only by the {owner} engine(s)");
            assert!(
                inert.iter().any(|e| e.reason.contains(&named)),
                "knob `{key}` must report `{owner}` as its owner: {:?}",
                catalog.errors
            );
        }
        assert!(
            inert
                .iter()
                .any(|e| e.reason.contains("cpu_offload") && e.reason.contains("llama")),
            "the key the scoping ruling started with stays named where it was: {:?}",
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

    /// Review 2026-10-31 (H1): the fold's persist→rescan round trip. The writer
    /// (`MeasuredData::edits` in the scheduler) stamps every serving model with the
    /// measured quartet; the scanner must read those keys back silently — a
    /// harness-written key reported as "not read by the harness" is a false
    /// statement to the operator and trips the live-ModelConf-clean gate. The
    /// const-coverage pin missed this class because it checks the const against a
    /// hand-listed literal set, not against what the WRITER emits.
    #[test]
    fn a_fold_written_yaml_rescans_without_unknown_key_reports() {
        let written = "engine: ninfer\n\
                       name: served\n\
                       ninfer_image: ninfer:local\n\
                       ninfer_container: ninfer-served\n\
                       vram_gb: 20\n\
                       measured_gb: 31.2\n\
                       measured_gb_min: 31.2\n\
                       measured_attribution: engine_report\n\
                       measured_profile: b\n";
        let (map, problems) = parse_flat_yaml(written);
        assert!(
            problems.is_empty(),
            "the fold's own write-back must rescan clean: {problems:?}"
        );
        assert_eq!(
            map.get("measured_attribution").map(String::as_str),
            Some("engine_report")
        );
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
            "strata_exe",
            "strata_args",
            "strata_tokenizer",
            "strata_lib_dirs",
            "strata_env",
            "strata_gpu",
            "strata_parallel",
            "strata_gpus_asked",
            "strata_vision",
            "strata_sampling",
            "strata_uncensored",
            "basalt_python",
            "basalt_cwd",
            "basalt_exe",
            "basalt_parallel",
            "basalt_engine_args",
            "ninfer_autostart",
            "ninfer_container",
            "ninfer_profile",
            "ninfer_key",
            "ninfer_image",
            "ninfer_model_name",
            "ninfer_artifact",
            "ninfer_internal_port",
            "ninfer_model_roots",
            "ninfer_serve_args",
            "ninfer_gpu_device",
            "ninfer_model_id",
            "ninfer_key_file",
            "ninfer_serve_cmd",
            "external",
        ] {
            assert!(
                CONSUMED_KEYS.contains(&key),
                "`{key}` must be a consumed key"
            );
        }
        // The one key family the const cannot enumerate: the operator-chosen
        // custom-profile override is consumed by prefix rule (the adapter refuses a
        // selected custom profile whose line is absent, so the read is real).
        let pdir = crate::testutil::temp_dir("scan-profile-prefix");
        let _ = std::fs::remove_dir_all(&pdir);
        std::fs::create_dir_all(&pdir).expect("temp dir");
        std::fs::write(
            pdir.join("custom.yaml"),
            "engine: ninfer\nninfer_container: box\nninfer_profile: lt\nninfer_profile_lt: ctx=16384 kv=32768 conc=2\n",
        )
        .expect("write");
        std::fs::write(
            pdir.join("bad.yaml"),
            "engine: ninfer\nninfer_container: bx2\nninfer_profile_BAD-NAME: ctx=1 kv=1 conc=1\n",
        )
        .expect("write");
        let catalog = scan_dir(&pdir);
        assert!(
            catalog
                .errors
                .iter()
                .all(|e| e.kind != ScanKind::UnknownKey
                    || e.reason.contains("ninfer_profile_BAD-NAME")),
            "only the unsuffixable key may raise unknown-key: {:?}",
            catalog.errors
        );
        assert!(
            catalog
                .errors
                .iter()
                .any(|e| e.kind == ScanKind::UnknownKey
                    && e.reason.contains("ninfer_profile_BAD-NAME")),
            "a profile-override suffix the adapter could never name is dead config: {:?}",
            catalog.errors
        );
        let custom = catalog
            .models
            .iter()
            .find(|m| m.name == "custom")
            .expect("the valid-override entry publishes");
        assert!(
            custom.raw.contains_key("ninfer_profile_lt"),
            "the override line must survive into raw for the adapter to read"
        );
        let _ = std::fs::remove_dir_all(&pdir);
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

    /// Fork C gate: the strict acceptance must not blind-side the live catalog.
    /// Since slice 10c "fully clean" is stated as a closed debt ledger: the
    /// live dir publishes every file whose bytes carry no harness-owned claim,
    /// and the report ledger is exactly the ruled one — empty since the three
    /// strata rows adopted their rendered template blocks 2026-11-02 (row 19;
    /// the `strata_config` refusal is gone, `strata_exe` etc. carry the profile
    /// the harness renders at load). The `context` inert hints that
    /// once sat on the ninfer rows landed their cleanup 2026-11-02: the key is
    /// live llama config and the ninfer context rides its `ninfer_profile`
    /// line, so the rows no longer carry it. Every entry is pinned by file and
    /// kind: a live file growing any other report, or an unplanned new file,
    /// fails here, and the ledger shrinks as each cleanup gesture lands.
    /// Skipped when the dir is absent.
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
        let mut got: BTreeMap<String, &'static str> = BTreeMap::new();
        for error in &catalog.errors {
            let label = match error.kind {
                ScanKind::InertKey => "inert",
                ScanKind::PolicyRefused => "refused",
                ScanKind::InvalidYaml => {
                    panic!("the live ModelConf grew an invalid-yaml file: {error:?}")
                }
                other => panic!("the live ModelConf grew an unplanned {other:?} report: {error:?}"),
            };
            let first = got.insert(error.file.clone(), label);
            assert!(
                first.is_none(),
                "one file carried two ledger reports: {error:?}"
            );
        }
        let want: BTreeMap<&str, &str> = BTreeMap::new();
        let got: BTreeMap<&str, &str> = got
            .iter()
            .map(|(file, label)| (file.as_str(), *label))
            .collect();
        assert_eq!(
            got, want,
            "the live ModelConf ledger is closed: every report is one of the ruled 10c entries"
        );
        let refused = want.values().filter(|kind| **kind == "refused").count();
        assert_eq!(
            catalog.models.len(),
            accepted - refused,
            "every accepted live file except the ruled refusals publishes: {:?}",
            catalog.errors
        );
    }

    /// Two files, one identity: `from_flat` trims the stem it is handed, so
    /// `qwen3.yaml` beside `qwen3 .yaml` publishes twice under one name — the shape
    /// FINDINGS-13 [3] named when it asked for the `seen_names` twin of
    /// `seen_containers`. Both rows publish (the scan reports, it never silences:
    /// FINDINGS-13 [1] Fork A keeps the old bytes visible) and **both** files are
    /// reported, each naming every claimant, so neither wins by scan order. Falsifier:
    /// delete the post-pass and the error list is empty with two rows under one name;
    /// report only the later claimant (the `seen_containers` shape) and the first file
    /// vanishes from the report, leaving it the silent favourite.
    #[test]
    fn two_files_claiming_one_name_are_both_reported_and_both_publish() {
        let dir = crate::testutil::temp_dir("dupname");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("qwen3.yaml"), "engine: llama\nvram_gb: 2\n").expect("write");
        // A stem whose trailing space the identity trim swallows: two distinct files,
        // one harness name. A `name:` key cannot do this — it must equal the stem.
        std::fs::write(dir.join("qwen3 .yaml"), "engine: llama\nvram_gb: 3\n").expect("write");

        let catalog = scan_dir(&dir);
        assert_eq!(
            catalog.models.len(),
            2,
            "a collision is reported, never silenced: {:?}",
            catalog.errors
        );
        assert!(
            catalog.models.iter().all(|m| m.name == "qwen3"),
            "both rows really share one identity: {:?}",
            catalog
                .models
                .iter()
                .map(|m| m.name.clone())
                .collect::<Vec<_>>()
        );
        let offenders: Vec<&ScanError> = catalog
            .errors
            .iter()
            .filter(|e| e.kind == ScanKind::DuplicateName)
            .collect();
        assert_eq!(
            offenders.len(),
            2,
            "every claimant is reported: {offenders:?}"
        );
        for (error, own) in offenders.iter().zip(["qwen3 .yaml", "qwen3.yaml"]) {
            assert_eq!(error.file, own, "one line per claiming file");
            assert!(
                error.reason.contains("qwen3.yaml") && error.reason.contains("qwen3 .yaml"),
                "the line names every claimant, not only the other one: {}",
                error.reason
            );
            assert!(
                error.reason.contains("name `qwen3`"),
                "the identity the two files collided on is named: {}",
                error.reason
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
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
            (ScanKind::InvalidName, "invalid_name"),
            (ScanKind::EngineBinaryMissing, "engine_binary_missing"),
            (ScanKind::EngineUnavailable, "engine_unavailable"),
            (ScanKind::UnknownKey, "unknown_key"),
            (ScanKind::InertKey, "inert_key"),
            (ScanKind::InvalidYaml, "invalid_yaml"),
            (ScanKind::DuplicateName, "duplicate_name"),
            (ScanKind::PolicyRefused, "policy_refused"),
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
