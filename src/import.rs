//! The one-shot strata profile migration (`simplehar-se import strata
//! <profile.json>`, operator ruling 2026-11-02 row 19c; `--apply`, ruling
//! 2026-11-03): read a manual profile, emit the row yaml the harness renders
//! from, and — only under `--apply <row.yaml>` — merge that block into the live
//! row. It is the only sanctioned reader of a `C:\Strata` profile — the serve
//! path never opens one.
//!
//! The output is honest about what a strata profile cannot carry: the
//! scheduling keys (`category`, `vram_gb`, `cost`, `load_timeout`,
//! `launch_only_with_at_least_vram_free`) live only in the row, so the emit
//! says to merge them from the row being replaced and `--apply` preserves them
//! in place. Harness-forced fields found in the profile (`host`, `port`, key,
//! the `log` home) are reported as dropped, never obeyed; keys the renderer has
//! no knob for are listed as not mapped — nothing is ever silently discarded.
//!
//! The mapping itself has exactly one writer: `strata_import` reads the
//! profile into a structured field list; `render_yaml` turns that list into
//! the stdout document (with its guidance comments) and [`apply_strata_row`]
//! turns it into the row merge (without them). Neither command boots a
//! listener, reads the live config, or touches the legacy v3.3 config.

use crate::model::scan::parse_flat_yaml;
use crate::model::writeback;
use std::collections::BTreeSet;
use std::path::Path;

/// The manual-profile keys the harness forces or mints itself: present in a
/// profile, they are dropped into a comment naming the replacement mechanism.
const HARNESS_OWNED_KEYS: &[&str] = &["host", "port", "key", "api_key", "log"];

/// The manual-profile keys this import maps to row knobs (the mapping itself
/// lives in [`strata_yaml`], one writer of both facts).
const MAPPED_KEYS: &[&str] = &[
    "exe",
    "args",
    "cwd",
    "tokenizer",
    "lib_dirs",
    "env",
    "gpu",
    "parallel",
    "gpus_asked",
    "vision",
    "sampling",
    "uncensored",
    "model_name",
];

/// One profile read through the import mapping: the row fields the profile can
/// carry, in render order, plus the honesty notes. The emit path prints all of
/// it; the apply path merges only [`fields`](Self::fields) — the comments are
/// stdout guidance, never row bytes.
struct StrataImport {
    /// The `key: value` pairs the profile declares, in the order they render.
    fields: Vec<(String, String)>,
    /// Harness-owned profile keys found and dropped (named, never obeyed).
    dropped: Vec<&'static str>,
    /// Keys the renderer has no knob for, each one reported on stdout.
    unmapped: Vec<String>,
    /// True when the profile carries no `model_name` at all: stdout says the
    /// row must supply `serve_model`; apply then simply leaves the row's own.
    serve_model_absent: bool,
}

/// Reads one strata profile into the [`StrataImport`] field list — the one
/// writer of the profile→row mapping, shared by both command paths.
///
/// # Arguments
///
/// * `profile_path` - the manual profile JSON to read (a BOM is ignored, the
///   same shape the engine itself tolerates).
///
/// # Errors
///
/// The refusal sentence naming what made the file unusable as an import
/// source: unreadable file, JSON that is not a top-level object, or a required
/// key in an unusable shape.
fn strata_import(profile_path: &Path) -> Result<StrataImport, String> {
    let text = std::fs::read_to_string(profile_path)
        .map_err(|e| format!("cannot read {}: {e}", profile_path.display()))?;
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let root: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(json).map_err(|e| {
            format!(
                "{} is not a JSON object profile: {e}",
                profile_path.display()
            )
        })?;
    let str = |key: &str| -> Option<&str> { root.get(key).and_then(serde_json::Value::as_str) };
    // The honesty channel (review 25 [4]): a MAPPED key the renderer's shape
    // cannot carry is reported like an unmapped one — never dropped without a
    // line — and nested unmapped keys are audited one level down ([1]), since
    // a live profile turned out to carry `vision.threads` through the gap.
    let mut wrong_type: Vec<&str> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut serve_model_absent = false;
    fields.push(("engine".to_string(), "strata".to_string()));
    match str("model_name") {
        Some(name) if name.trim().is_empty() => {
            return Err(format!(
                "{} declares an empty `model_name`: fix the profile before importing",
                profile_path.display()
            ));
        }
        Some(name) => fields.push(("serve_model".to_string(), name.to_string())),
        None => {
            if root.contains_key("model_name") {
                wrong_type.push("model_name");
            } else {
                serve_model_absent = true;
            }
        }
    }
    // `exe` is the one required key, so each unusable shape is refused under
    // its own true name (review 26 [2]): "declares no exe" must not accuse a
    // value that is present but misshapen, and an empty exe must not import to
    // an empty knob line that only refuses at load.
    match str("exe") {
        Some(exe) if !exe.trim().is_empty() => {
            fields.push(("strata_exe".to_string(), exe.to_string()));
        }
        Some(_) => {
            return Err(format!(
                "{} declares an empty `exe`: nothing to render",
                profile_path.display()
            ));
        }
        None if root.contains_key("exe") => {
            return Err(format!(
                "{} declares an `exe` that is not a string: nothing to render",
                profile_path.display()
            ));
        }
        None => {
            return Err(format!(
                "{} declares no `exe`: nothing to render",
                profile_path.display()
            ));
        }
    }
    // The block ONCE rule needs the block itself, not the key: a `vision`
    // that is not an object can never render, so its args switch must survive
    // in the imported line rather than be silently turned off.
    let vision_block = root.get("vision").and_then(serde_json::Value::as_object);
    if root.contains_key("vision") && vision_block.is_none() {
        wrong_type.push("vision");
    }
    if let Some(value) = root.get("args") {
        match value.as_array() {
            None => wrong_type.push("args"),
            Some(args) => {
                let spelled: Vec<&str> =
                    args.iter().filter_map(serde_json::Value::as_str).collect();
                if spelled.len() != args.len() {
                    return Err(format!(
                        "{} `args` holds a token that is not a string: fix the profile \
                         before importing",
                        profile_path.display()
                    ));
                }
                if spelled.iter().any(|token| token.trim().is_empty()) {
                    return Err(format!(
                        "{} carries an empty `args` token: fix the profile before importing",
                        profile_path.display()
                    ));
                }
                // The canonical row spells vision ONCE, in the block: the
                // renderer re-appends the switch, so a block that WILL render
                // owns the token; a block that cannot leaves it in the data.
                let has_vision = vision_block.is_some();
                let tokens: Vec<&str> = spelled
                    .iter()
                    .copied()
                    .filter(|token| !(has_vision && *token == "--vision"))
                    .collect();
                fields.push(("strata_args".to_string(), tokens.join(", ")));
            }
        }
    }
    if let Some(value) = root.get("cwd") {
        match value.as_str() {
            Some(cwd) if cwd.trim().is_empty() => {
                return Err(format!(
                    "{} declares an empty `cwd`: fix the profile before importing",
                    profile_path.display()
                ));
            }
            Some(cwd) => fields.push(("strata_cwd".to_string(), cwd.to_string())),
            None => wrong_type.push("cwd"),
        }
    }
    if let Some(value) = root.get("tokenizer") {
        match value.as_str() {
            Some(tokenizer) if tokenizer.trim().is_empty() => {
                return Err(format!(
                    "{} declares an empty `tokenizer`: fix the profile before importing",
                    profile_path.display()
                ));
            }
            Some(tokenizer) => fields.push(("strata_tokenizer".to_string(), tokenizer.to_string())),
            None => wrong_type.push("tokenizer"),
        }
    }
    if let Some(value) = root.get("lib_dirs") {
        match value.as_array() {
            None => wrong_type.push("lib_dirs"),
            Some(dirs) => {
                let entries: Vec<&str> =
                    dirs.iter().filter_map(serde_json::Value::as_str).collect();
                if entries.len() != dirs.len() {
                    return Err(format!(
                        "{} `lib_dirs` holds a non-string entry",
                        profile_path.display()
                    ));
                }
                fields.push(("strata_lib_dirs".to_string(), entries.join(", ")));
            }
        }
    }
    if let Some(value) = root.get("env") {
        match value.as_object() {
            None => wrong_type.push("env"),
            Some(env) => {
                if env.values().any(|v| v.as_str().is_none()) {
                    return Err(format!(
                        "{} `env` holds a non-string value",
                        profile_path.display()
                    ));
                }
                let tokens: Vec<String> = env
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                    .collect();
                fields.push(("strata_env".to_string(), tokens.join(" ")));
            }
        }
    }
    for (key, yaml) in [("gpu", "strata_gpu"), ("parallel", "strata_parallel")] {
        if let Some(value) = root.get(key) {
            match value.as_u64() {
                Some(number) => fields.push((yaml.to_string(), number.to_string())),
                None => wrong_type.push(key),
            }
        }
    }
    if let Some(value) = root.get("gpus_asked") {
        match value.as_bool() {
            Some(true) => fields.push(("strata_gpus_asked".to_string(), "true".to_string())),
            // An explicit false is the renderer's absent-key default (the
            // profile key is written only when truthy): faithful as absent.
            Some(false) => {}
            None => wrong_type.push("gpus_asked"),
        }
    }
    if let Some(vision) = vision_block {
        let mut tokens: Vec<String> = Vec::new();
        for key in ["exe", "mmproj", "model"] {
            match vision.get(key).and_then(serde_json::Value::as_str) {
                Some(value) => tokens.push(format!("{key}={value}")),
                None => {
                    return Err(format!(
                        "{} vision block lacks `{key}`",
                        profile_path.display()
                    ))
                }
            }
        }
        match vision.get("gpu") {
            Some(serde_json::Value::Bool(on)) => tokens.push(format!("gpu={on}")),
            Some(_) => {
                return Err(format!(
                    "{} vision `gpu` must be a boolean",
                    profile_path.display()
                ))
            }
            // server.py:1939 reads an ABSENT gpu as falsy, while the renderer's
            // token-absent default is true — an omitted key imported silently
            // would flip the encoder CPU→GPU (review 26 [1]). The faithful
            // token for an absent key is the explicit false the engine ran.
            None => tokens.push("gpu=false".to_string()),
        }
        // The three numeric keys of the server's vision contract (ruling
        // 2026-11-02): a present value the token shape cannot carry is
        // reported, never dropped (the same rule as the root keys).
        for key in ["max_tokens", "min_tokens", "threads"] {
            match vision.get(key) {
                None => {}
                Some(value) => match value.as_u64() {
                    Some(n) => tokens.push(format!("{key}={n}")),
                    None => notes.push(format!("vision.{key} (present, wrong type)")),
                },
            }
        }
        for key in vision.keys() {
            if ![
                "exe",
                "mmproj",
                "model",
                "gpu",
                "max_tokens",
                "min_tokens",
                "threads",
            ]
            .contains(&key.as_str())
            {
                notes.push(format!("vision.{key}"));
            }
        }
        fields.push(("strata_vision".to_string(), tokens.join(" ")));
    }
    if let Some(value) = root.get("sampling") {
        match value.as_object() {
            None => wrong_type.push("sampling"),
            Some(sampling) => {
                let mut tokens: Vec<String> = Vec::new();
                for (key, value) in sampling {
                    // `Value`'s Display keeps the source's numeric form (int stays
                    // int, 1.0 stays 1.0), which is what the renderer re-parses.
                    if value.as_u64().is_none()
                        && value.as_f64().is_none()
                        && value.as_bool().is_none()
                    {
                        return Err(format!(
                            "{} `sampling` value for `{key}` is neither number nor bool",
                            profile_path.display()
                        ));
                    }
                    tokens.push(format!("{key}={value}"));
                }
                fields.push(("strata_sampling".to_string(), tokens.join(" ")));
            }
        }
    }
    if let Some(on) = root.get("uncensored") {
        match on.as_bool() {
            Some(on) => fields.push(("strata_uncensored".to_string(), on.to_string())),
            None => {
                return Err(format!(
                    "{} `uncensored` must be a boolean",
                    profile_path.display()
                ))
            }
        }
    }
    let owned: Vec<&'static str> = HARNESS_OWNED_KEYS
        .iter()
        .copied()
        .filter(|key| root.contains_key(*key))
        .collect();
    let mut unmapped: Vec<String> = wrong_type
        .iter()
        .map(|key| format!("{key} (present, wrong type)"))
        .chain(notes)
        .collect();
    unmapped.extend(
        root.keys()
            .filter(|key| !MAPPED_KEYS.contains(&key.as_str()) && !owned.contains(&key.as_str()))
            .cloned(),
    );
    Ok(StrataImport {
        fields,
        dropped: owned,
        unmapped,
        serve_model_absent,
    })
}

/// Renders the [`StrataImport`] as the stdout document: the guidance header,
/// the field block (with the `serve_model` guidance line right after `engine`
/// when the profile cannot carry one), then the honesty notes. The `--apply`
/// path never calls this — its row carries bytes, not advice.
fn render_yaml(profile_path: &Path, import: &StrataImport) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "# strata template row rendered by `simplehar-se import strata {}` (ruling 2026-11-02).\n\
         # The harness now RENDERS this profile from the row: the strata_* keys below are\n\
         # the whole engine config. Copy the scheduling keys (`category`, `vram_gb`,\n\
         # `cost`, `load_timeout`, `launch_only_with_at_least_vram_free`, `context`)\n\
         # and `name`/`model` from the row you are replacing; `serve_model` rides the\n\
         # `model_name` the profile already declares. If the old row carried\n\
         # `strata_python:`, copy that line too \u{2014} the interpreter is a row key, never a\n\
         # profile field.\n",
        profile_path.display()
    ));
    for (key, value) in &import.fields {
        out.push_str(&format!("{key}: {value}\n"));
        if key == "engine" && import.serve_model_absent {
            out.push_str(
                "# not mapped: model_name is absent — `serve_model` must come from the row\n",
            );
        }
    }
    if !import.dropped.is_empty() {
        out.push_str(&format!(
            "# harness-owned, dropped (bind, port, key and log home are harness-forced): {}\n",
            import.dropped.join(", ")
        ));
    }
    if !import.unmapped.is_empty() {
        out.push_str(&format!("# not mapped: {}\n", import.unmapped.join(", ")));
    }
    out
}

/// Renders the row yaml for one strata profile file (emit-only: stdout bytes,
/// nothing written anywhere).
///
/// # Arguments
///
/// * `profile_path` - the manual profile JSON to read (a BOM is ignored, the
///   same shape the engine itself tolerates).
///
/// # Returns
///
/// The yaml text (with its guidance comments), or the refusal sentence naming
/// what made the file unusable as an import source.
///
/// # Errors
///
/// Unreadable file, or JSON that is not a top-level object.
pub fn strata_yaml(profile_path: &Path) -> Result<String, String> {
    Ok(render_yaml(profile_path, &strata_import(profile_path)?))
}

/// What a successful [`apply_strata_row`] reports — counts, a truth flag and a
/// guidance note, never values: the row may hold paths the operator keeps
/// private, and the summary must be loggable beside the profile's dropped key
/// material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// How many key lines the merge actually changed (appended or rewritten).
    /// A field re-emitted with the value the line already publishes is NOT
    /// counted: the write-back keeps such a line byte-verbatim, and a partial
    /// re-apply must not be reported as a full rewrite (REV9-03).
    pub changed: usize,
    /// How many stale `strata_*` keys (including `strata_config`) the merge
    /// deleted.
    pub removed: usize,
    /// True when the profile declares no `model_name`, so the row's own
    /// `serve_model` was kept: surfaced as a summary note, never silently
    /// skipped (REV9-02).
    pub serve_model_absent: bool,
    /// True when the row already carried the block byte-for-byte: the commit
    /// short-circuited and nothing reached the disk.
    pub unchanged: bool,
}

impl std::fmt::Display for ApplyOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.unchanged {
            write!(f, "row already carries the imported block: nothing written")?;
        } else {
            write!(
                f,
                "merged {} {}, deleted {} stale {}",
                self.changed,
                if self.changed == 1 { "key" } else { "keys" },
                self.removed,
                if self.removed == 1 { "key" } else { "keys" }
            )?;
        }
        if self.serve_model_absent {
            write!(
                f,
                " \u{2014} note: profile declared no model_name; serve_model was not replaced"
            )?;
        }
        Ok(())
    }
}

/// Merges the emitted template block into the live row file (ruling 2026-11-03:
/// `simplehar-se import strata <profile.json> --apply <row.yaml>`).
///
/// The merge contract:
///
/// - preserved byte-verbatim or in place: `name`, `model`, `category`,
///   `vram_gb`, `cost`, `load_timeout`, `launch_only_with_at_least_vram_free`,
///   `context`, `strata_python`, and every other unrelated line — comments,
///   blank lines, inline notes and key order included;
/// - replaced or added: `engine`, `serve_model` and every emitted `strata_*`
///   key, each rewritten in place when the row already carries it, appended in
///   field order when it does not;
/// - deleted: `strata_config` and every `strata_*` key the import no longer
///   emits (any `strata_config` line is gone only when the whole merge
///   validated — a refused merge deletes nothing);
/// - refused, with a named reason and no partial write: missing or unreadable
///   target, a target the scan reader would not publish (junk or duplicate
///   key), an empty target, a target that is not a strata row, and a value no
///   single yaml line could round-trip. No guessing.
///
/// The write goes through the write-back module's machine under its gate: one
/// temp + rename publish, revalidated after write, the original bytes restored
/// and forensically copied if the revalidation fails.
///
/// # Errors
///
/// The refusal sentence naming what refused the merge. On every refusal the
/// target's bytes are exactly what they were — the edits are computed in memory
/// and only a fully validated document is ever committed.
pub fn apply_strata_row(profile_path: &Path, row_path: &Path) -> Result<ApplyOutcome, String> {
    let import = strata_import(profile_path)?;
    let target = row_path.display();
    // One writer at a time: the merge joins the write-back critical section so
    // it cannot race a harness flag edit against the same file.
    let _gate = writeback::acquire_write_gate();
    let original = match std::fs::read_to_string(row_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "target row {target} does not exist: nothing to merge into"
            ));
        }
        Err(e) => return Err(format!("cannot read target row {target}: {e}")),
    };
    let (map, problems) = parse_flat_yaml(&original);
    if !problems.is_empty() {
        return Err(format!(
            "target row {target} is not a clean single row: {}",
            problems.join("; ")
        ));
    }
    if map.is_empty() {
        return Err(format!(
            "target row {target} holds no keys: nothing to merge into"
        ));
    }
    match map.get("engine").map(String::as_str) {
        Some("strata") => {}
        Some(other) => {
            return Err(format!(
                "target row {target} is not a strata row: its engine is `{other}`"
            ));
        }
        None => {
            return Err(format!(
                "target row {target} carries no `engine:` key: not a strata row"
            ));
        }
    }
    let emitted: BTreeSet<&str> = import.fields.iter().map(|(key, _)| key.as_str()).collect();
    let mut text = original.clone();
    for (key, value) in &import.fields {
        text = writeback::set_key_in_text(&text, key, Some(value.as_str()))
            .map_err(|e| format!("cannot merge `{key}` into target row {target}: {e}"))?;
    }
    let mut removed = 0;
    for stale in map.keys() {
        let name = stale.as_str();
        if name.starts_with("strata_") && name != "strata_python" && !emitted.contains(&name) {
            text = writeback::set_key_in_text(&text, name, None).map_err(|e| {
                format!("cannot delete stale `{name}` from target row {target}: {e}")
            })?;
            removed += 1;
        }
    }
    writeback::commit_verified(row_path, &original, &text)
        .map_err(|e| format!("cannot apply to target row {target}: {e}"))?;
    // REV9-03: the honest count. A field whose emitted value is exactly the
    // value the row's line already publishes was kept byte-verbatim by the
    // write-back, so its line did not change and it is not counted.
    let changed = import
        .fields
        .iter()
        .filter(|(key, value)| map.get(key.as_str()).map(String::as_str) != Some(value.as_str()))
        .count();
    Ok(ApplyOutcome {
        changed,
        removed,
        serve_model_absent: import.serve_model_absent,
        unchanged: text == original,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_profile(tag: &str, body: &str) -> std::path::PathBuf {
        let path = crate::testutil::temp_dir("import").join(format!("{tag}.json"));
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    #[test]
    fn a_manual_profile_imports_to_the_knobs_it_can_carries() {
        let profile = temp_profile(
            "full",
            r#"{
                "exe": "C:\\Strata\\engine\\strata.exe",
                "args": ["--pack", "D:\\Models\\pack", "--max-context", "262144", "--vision"],
                "cwd": "C:\\Strata",
                "tokenizer": "D:\\Models\\pack\\tokenizer",
                "model_name": "qwen3.8-flash-next-iq3_s",
                "log": "C:\\Strata\\strata-iq3_s.log",
                "lib_dirs": ["C:\\Strata\\.venv\\Lib\\x"],
                "port": 8003,
                "gpu": 0,
                "parallel": 1,
                "gpus_asked": true,
                "host": "0.0.0.0",
                 "env": {"STRATA_KVA": "0"},
                 "sampling": {"temperature": 1.0, "top_p": 0.95, "top_k": 20},
                 "uncensored": false,
                 "vision": {"exe": "C:\\Strata\\engine\\strata-vision.exe",
                           "mmproj": "D:\\Models\\mmproj.gguf",
                           "model": "D:\\Models\\m.gguf",
                           "gpu": true, "max_tokens": 1024}
            }"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("serve_model: qwen3.8-flash-next-iq3_s"),
            "{yaml}"
        );
        assert!(
            yaml.contains("strata_exe: C:\\Strata\\engine\\strata.exe"),
            "{yaml}"
        );
        assert!(
            yaml.contains("strata_args: --pack, D:\\Models\\pack, --max-context, 262144"),
            "the canonical line spells vision ONCE, in the block: {yaml}"
        );
        assert!(
            yaml.contains("strata_lib_dirs: C:\\Strata\\.venv\\Lib\\x"),
            "{yaml}"
        );
        assert!(yaml.contains("strata_env: STRATA_KVA=0"), "{yaml}");
        assert!(
            yaml.contains("strata_sampling: temperature=1.0 top_k=20 top_p=0.95"),
            "the sampling defaults ride the row in source type: {yaml}"
        );
        assert!(yaml.contains("strata_uncensored: false"), "{yaml}");
        assert!(yaml.contains("strata_gpu: 0"), "{yaml}");
        assert!(yaml.contains("strata_parallel: 1"), "{yaml}");
        assert!(yaml.contains("strata_gpus_asked: true"), "{yaml}");
        assert!(yaml.contains("strata_vision: exe=C:\\Strata\\engine\\strata-vision.exe mmproj=D:\\Models\\mmproj.gguf model=D:\\Models\\m.gguf gpu=true max_tokens=1024"), "{yaml}");
        assert!(
            yaml.contains("# harness-owned, dropped")
                && yaml.contains("host, port")
                && yaml.contains("log"),
            "policy fields are reported as dropped: {yaml}"
        );
        let _ = std::fs::remove_file(profile);
    }

    #[test]
    fn an_empty_args_token_is_refused_not_dropped_silently() {
        let profile = temp_profile(
            "empty-token",
            r#"{"exe": "C:\\e.exe", "args": ["--pack", " "], "tokenizer": "C:\\t"}"#,
        );
        let error = strata_yaml(&profile).expect_err("an empty token is not importable");
        assert!(error.contains("empty `args` token"), "{error}");
        let _ = std::fs::remove_file(profile);
    }

    #[test]
    fn an_unmapped_key_is_reported_never_invented_into_a_knob() {
        let profile = temp_profile(
            "unmapped",
            r#"{"exe": "C:\\e.exe", "tokenizer": "C:\\t", "mystery": {"a": 1}}"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(yaml.contains("# not mapped: mystery"), "{yaml}");
        let _ = std::fs::remove_file(profile);
    }

    /// Review 25 [4]: every MAPPED key whose JSON shape the renderer cannot
    /// carry is reported on the not-mapped line — the module promise is that
    /// the operator can merge the output blind, so a present-but-wrong-type
    /// key may never simply vanish.
    #[test]
    fn a_mapped_key_of_the_wrong_shape_is_reported_not_dropped() {
        let profile = temp_profile(
            "wrong-type",
            r#"{
                "exe": "C:\\e.exe",
                "model_name": 7,
                "cwd": 5,
                "lib_dirs": "C:\\x",
                "env": "STRATA_KVA=0",
                "gpu": "0",
                "parallel": 1.5,
                "gpus_asked": "yes",
                "sampling": 5
            }"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        for key in [
            "model_name",
            "cwd",
            "lib_dirs",
            "env",
            "gpu",
            "parallel",
            "gpus_asked",
            "sampling",
        ] {
            assert!(
                yaml.contains(&format!("{key} (present, wrong type)")),
                "{key} must be reported, not dropped: {yaml}"
            );
        }
        assert!(
            !yaml.contains("strata_cwd")
                && !yaml.contains("strata_env")
                && !yaml.contains("strata_sampling"),
            "a key the renderer cannot carry never becomes a half-knob: {yaml}"
        );
        let _ = std::fs::remove_file(profile);
    }

    /// Review 25 [4], the worse-than-silent arm: a non-object `vision` that
    /// strips `--vision` from the args line AND renders no block switches
    /// vision off with nothing printed. The token must survive the import.
    #[test]
    fn a_vision_that_cannot_render_leaves_the_switch_in_the_args_line() {
        let profile = temp_profile(
            "vision-scalar",
            r#"{"exe": "C:\\e.exe", "args": ["--pack", "P", "--vision"], "vision": true}"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("strata_args: --pack, P, --vision"),
            "no block can carry the switch, so the data keeps it: {yaml}"
        );
        assert!(yaml.contains("vision (present, wrong type)"), "{yaml}");
        let _ = std::fs::remove_file(profile);
    }

    /// Review 26 [1]: server.py reads an absent `vision.gpu` as falsy, the
    /// renderer's token-absent default is true — the import must spell the
    /// false the engine ran, never leave the default to materialize a GPU.
    #[test]
    fn a_vision_block_without_gpu_imports_the_explicit_false_the_engine_ran() {
        let profile = temp_profile(
            "vision-nogpu",
            r#"{"exe": "C:\\e.exe", "vision": {"exe": "C:\\v.exe", "mmproj": "M", "model": "D"}}"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("strata_vision: exe=C:\\v.exe mmproj=M model=D gpu=false"),
            "an absent gpu must import as the explicit false: {yaml}"
        );
        assert!(
            !yaml.contains("vision."),
            "the contract-carrying block must leave nothing not mapped: {yaml}"
        );
        let _ = std::fs::remove_file(profile);
    }

    /// Review 26 [2]: `exe` is required, and each unusable shape meets its own
    /// true refusal — "declares no exe" must not misname a present value.
    #[test]
    fn an_unusable_exe_is_refused_under_its_own_true_name() {
        for (body, needle) in [
            (r#"{"exe": "C:\\e.exe"}"#, "nothing to render"), // control: not this door
            (r#"{"exe": 5}"#, "not a string"),
            (r#"{"exe": " "}"#, "empty"),
            (r#"{"tokenizer": "C:\\t"}"#, "declares no `exe`"),
        ] {
            let profile = temp_profile("exe-shape", body);
            let outcome = strata_yaml(&profile);
            if body.contains("C:\\\\e.exe") {
                assert!(outcome.is_ok(), "control row must import");
            } else {
                let error = outcome.expect_err("an unusable exe must refuse");
                assert!(error.contains(needle), "{body}: {error}");
            }
            let _ = std::fs::remove_file(profile);
        }
    }

    /// Ruling 2026-11-02 (FINDINGS-25 [1] settled): `threads` and `min_tokens`
    /// are tokens now — a live profile's `vision.threads` imports faithful —
    /// while a nested key the server contract does not carry, and a nested
    /// number of the wrong shape, are still reported one level down.
    #[test]
    fn a_nested_vision_key_without_a_knob_is_named_one_level_down() {
        let profile = temp_profile(
            "vision-nested",
            r#"{
                "exe": "C:\\e.exe",
                "vision": {"exe": "C:\\v.exe", "mmproj": "M", "model": "D",
                           "gpu": false, "max_tokens": 300, "min_tokens": 4,
                           "threads": "16", "quality": 2}
            }"#,
        );
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("strata_vision: exe=C:\\v.exe mmproj=M model=D gpu=false max_tokens=300 min_tokens=4"),
            "{yaml}"
        );
        assert!(
            yaml.contains("vision.threads (present, wrong type)"),
            "a nested number spelled as text is reported, not dropped: {yaml}"
        );
        assert!(
            yaml.contains("vision.quality"),
            "a nested key the server contract lacks must be named: {yaml}"
        );
        let _ = std::fs::remove_file(profile);
    }

    /// The full-shape profile every apply test imports: it carries every mapped
    /// key plus harness-owned `log`/`port`/`host`, so one merge exercises the
    /// whole field list.
    const FULL_PROFILE: &str = r#"{
        "exe": "C:\\Strata\\engine\\strata.exe",
        "args": ["--pack", "D:\\Models\\pack", "--max-context", "262144", "--vision"],
        "cwd": "C:\\Strata",
        "tokenizer": "D:\\Models\\pack\\tokenizer",
        "model_name": "qwen3.8-flash-next-iq3_s",
        "log": "C:\\Strata\\strata-iq3_s.log",
        "lib_dirs": ["C:\\Strata\\.venv\\Lib\\x"],
        "port": 8003,
        "gpu": 0,
        "parallel": 1,
        "gpus_asked": true,
        "host": "0.0.0.0",
        "env": {"STRATA_KVA": "0"},
        "sampling": {"temperature": 1.0, "top_p": 0.95, "top_k": 20},
        "uncensored": false,
        "vision": {"exe": "C:\\Strata\\engine\\strata-vision.exe",
                   "mmproj": "D:\\Models\\mmproj.gguf",
                   "model": "D:\\Models\\m.gguf",
                   "gpu": true, "max_tokens": 1024}
    }"#;

    fn write_fixture(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write fixture");
        path
    }

    fn dir_entries(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn publish_residues(dir: &Path) -> Vec<String> {
        dir_entries(dir)
            .into_iter()
            .filter(|name| name.ends_with(".sh.tmp") || name.ends_with(".bak"))
            .collect()
    }

    /// The non-goals pin: without `--apply` the command is still emit-only —
    /// stdout bytes only, the directory holds nothing but the profile.
    #[test]
    fn the_emit_only_command_writes_nothing() {
        let dir = crate::testutil::temp_dir("import-emit-only");
        let profile = write_fixture(&dir, "profile.json", FULL_PROFILE);
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("serve_model: qwen3.8-flash-next-iq3_s"),
            "{yaml}"
        );
        assert_eq!(
            dir_entries(&dir),
            vec!["profile.json".to_string()],
            "emit-only created a file next to the profile"
        );
    }

    /// The acceptance core: the merge replaces the strata-owned keys and
    /// `serve_model`, deletes `strata_config` and the stale knob, preserves the
    /// scheduling keys, `strata_python`, comments, the blank line and the key
    /// order, and a second apply is a byte fixed point that never reaches disk.
    #[test]
    fn apply_merges_the_block_into_the_live_row_and_keeps_the_operators_bytes() {
        let dir = crate::testutil::temp_dir("import-apply");
        let profile = write_fixture(&dir, "profile.json", FULL_PROFILE);
        let row = write_fixture(
            &dir,
            "row.yaml",
            "# operator header note\n\
             name: qwen3.8-flash-next-iq3_s\n\
             model: D:/Models/pack/qwen3.gguf\n\
             engine: strata # rendered from the row\n\
             category: WORK\n\
             vram_gb: 24.0\n\
             cost: greedy\n\
             load_timeout: 900\n\
             launch_only_with_at_least_vram_free: 26.0\n\
             context: 262144 # long ctx\n\
             serve_model: old-name\n\
             \n\
             strata_python: C:/Strata/.venv/Scripts/python.exe\n\
             strata_config: C:/Strata/manual.json\n\
             strata_exe: C:/Strata/old/strata.exe\n\
             strata_retired_knob: 7\n",
        );
        let outcome = apply_strata_row(&profile, &row).expect("applies");
        assert!(!outcome.unchanged);
        assert_eq!(outcome.changed, 13, "{outcome:?}");
        assert!(!outcome.serve_model_absent, "{outcome:?}");
        assert!(
            !outcome.to_string().contains("serve_model was not replaced"),
            "a profile that declares model_name gets no note: {outcome}"
        );
        assert_eq!(
            outcome.removed, 2,
            "strata_config and the retired knob are the only deletions: {outcome:?}"
        );
        let text = std::fs::read_to_string(&row).expect("read back");
        assert!(
            text.starts_with("# operator header note\n"),
            "header lost: {text}"
        );
        assert!(
            text.contains("engine: strata # rendered from the row"),
            "an identical-value key keeps its inline note byte-verbatim: {text}"
        );
        assert!(
            text.contains("serve_model: qwen3.8-flash-next-iq3_s"),
            "{text}"
        );
        assert_eq!(
            text.matches("serve_model:").count(),
            1,
            "replaced in place, never appended beside: {text}"
        );
        assert!(!text.contains("old-name"), "the old value survived: {text}");
        assert!(
            text.contains("strata_exe: C:\\Strata\\engine\\strata.exe"),
            "{text}"
        );
        assert_eq!(
            text.matches("strata_exe:").count(),
            1,
            "the old exe line was rewritten in place: {text}"
        );
        assert!(!text.contains("old/strata"), "the old exe survived: {text}");
        for kept in [
            "name: qwen3.8-flash-next-iq3_s",
            "model: D:/Models/pack/qwen3.gguf",
            "category: WORK",
            "vram_gb: 24.0",
            "cost: greedy",
            "load_timeout: 900",
            "launch_only_with_at_least_vram_free: 26.0",
            "context: 262144 # long ctx",
            "strata_python: C:/Strata/.venv/Scripts/python.exe",
        ] {
            assert!(text.contains(kept), "preserved key lost: `{kept}`: {text}");
        }
        for gone in ["strata_config", "strata_retired_knob"] {
            assert!(!text.contains(gone), "{gone} must be deleted: {text}");
        }
        assert!(
            text.contains("\n\nstrata_python"),
            "the operator's blank line survived the deletions: {text:?}"
        );
        for added in [
            "strata_args: --pack, D:\\Models\\pack, --max-context, 262144",
            "strata_cwd: C:\\Strata",
            "strata_tokenizer: D:\\Models\\pack\\tokenizer",
            "strata_lib_dirs: C:\\Strata\\.venv\\Lib\\x",
            "strata_env: STRATA_KVA=0",
            "strata_gpu: 0",
            "strata_parallel: 1",
            "strata_gpus_asked: true",
            "strata_uncensored: false",
            "strata_sampling: temperature=1.0 top_k=20 top_p=0.95",
            "strata_vision: exe=C:\\Strata\\engine\\strata-vision.exe",
        ] {
            assert!(
                text.contains(added),
                "emitted key not merged: `{added}`: {text}"
            );
        }
        let tail: Vec<usize> = [
            "strata_gpus_asked:",
            "strata_vision:",
            "strata_sampling:",
            "strata_uncensored:",
        ]
        .iter()
        .map(|key| text.find(key).expect("appended key present"))
        .collect();
        assert!(
            tail.windows(2).all(|pair| pair[0] < pair[1]),
            "appended keys ride the field order: {text}"
        );
        let (map, problems) = parse_flat_yaml(&text);
        assert!(
            problems.is_empty(),
            "the merged row must rescan clean: {problems:?}"
        );
        assert_eq!(map.get("category").map(String::as_str), Some("WORK"));
        assert_eq!(
            map.get("serve_model").map(String::as_str),
            Some("qwen3.8-flash-next-iq3_s")
        );
        // The merge is a fixed point: a second apply writes no byte, no temp,
        // and no incident copy.
        let again = apply_strata_row(&profile, &row).expect("re-apply");
        assert!(again.unchanged, "the second apply must report a no-op");
        assert_eq!(
            again.changed, 0,
            "a no-op re-apply must not claim any merged key"
        );
        assert_eq!(
            std::fs::read_to_string(&row).expect("read back"),
            text,
            "the re-apply churned the row"
        );
        assert!(
            publish_residues(&dir).is_empty(),
            "clean applies leave no temp and no .bak"
        );
    }

    /// Every refusal names its reason and its target, and the target's bytes
    /// never change — a refused merge deletes and writes nothing, not even a
    /// temp or an incident copy.
    #[test]
    fn a_refused_target_keeps_its_bytes_exactly() {
        let dir = crate::testutil::temp_dir("import-apply-refuse");
        let profile = write_fixture(&dir, "profile.json", FULL_PROFILE);
        for (name, body, needle) in [
            (
                "dup.yaml",
                "name: q\nengine: strata\ncontext: 1\ncontext: 2\n",
                "duplicate key `context`",
            ),
            (
                "junk.yaml",
                "name: q\nthis is not a row line\nengine: strata\n",
                "not a `key: value`",
            ),
            (
                "other-engine.yaml",
                "name: q\nengine: llama\n",
                "not a strata row: its engine is `llama`",
            ),
            (
                "no-engine.yaml",
                "name: q\ncategory: WORK\n",
                "carries no `engine:` key",
            ),
            ("empty.yaml", "# only a comment, no keys\n", "holds no keys"),
        ] {
            let row = write_fixture(&dir, name, body);
            let error = apply_strata_row(&profile, &row).expect_err("{name} must refuse");
            assert!(error.contains(needle), "{name}: {error}");
            assert!(
                error.contains(name),
                "the reason must name the target: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&row).expect("read back"),
                body,
                "{name} bytes changed"
            );
        }
        // A missing target is its own named door, not a bare io code.
        let missing = dir.join("never-written.yaml");
        let error = apply_strata_row(&profile, &missing).expect_err("a missing target must refuse");
        assert!(error.contains("does not exist"), "{error}");
        assert!(error.contains("never-written.yaml"), "{error}");
        // An unreadable target (a directory wearing the row's name) is named too.
        let wedged = dir.join("wedged.yaml");
        std::fs::create_dir(&wedged).expect("dir as row");
        let error =
            apply_strata_row(&profile, &wedged).expect_err("an unreadable target must refuse");
        assert!(error.contains("cannot read target row"), "{error}");
        assert!(
            publish_residues(&dir).is_empty(),
            "a refusal creates no temp and no incident copy"
        );
    }

    /// The atomic-publish failure door, driven through the write-back module's
    /// own seam: the rename commits, the post-write validation is poisoned, and
    /// the restore must put the original bytes back exactly while the refusal
    /// names the failure.
    #[test]
    fn a_failed_publish_leaves_the_original_bytes_unchanged() {
        let dir = crate::testutil::temp_dir("import-apply-fail");
        let profile = write_fixture(&dir, "profile.json", FULL_PROFILE);
        let body = "# header\nname: q\nengine: strata\n";
        let row = write_fixture(&dir, "row.yaml", body);
        writeback::arm_post_write_corrupt(&row, false);
        let error =
            apply_strata_row(&profile, &row).expect_err("the armed seam must fail the publish");
        assert!(error.contains("post-write validation failed"), "{error}");
        assert!(
            error.contains("row.yaml"),
            "the reason must name the target: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&row).expect("read back"),
            body,
            "the restore must leave the original bytes exactly"
        );
        let temps: Vec<String> = dir_entries(&dir)
            .into_iter()
            .filter(|name| name.ends_with(".sh.tmp"))
            .collect();
        assert!(temps.is_empty(), "no temp survives the failure: {temps:?}");
    }

    /// The 15[9] posture through the new door: a profile's `key` value is a
    /// harness-forced secret-shaped field — emit reports it as dropped by NAME,
    /// apply never lets its VALUE reach the row, the summary, or stdout.
    #[test]
    fn the_import_never_carries_key_material_into_the_row_or_the_summary() {
        let dir = crate::testutil::temp_dir("import-apply-secret");
        let profile = write_fixture(
            &dir,
            "profile.json",
            r#"{"exe": "C:\\e.exe", "model_name": "m", "key": "sk-secret-value", "port": 8003}"#,
        );
        let row = write_fixture(&dir, "row.yaml", "name: m\nengine: strata\n");
        let yaml = strata_yaml(&profile).expect("imports");
        assert!(
            yaml.contains("harness-owned, dropped") && yaml.contains("key"),
            "the forced key must be reported dropped by name: {yaml}"
        );
        let outcome = apply_strata_row(&profile, &row).expect("applies");
        let text = std::fs::read_to_string(&row).expect("read back");
        for (where_, haystack) in [
            ("stdout yaml", yaml.as_str()),
            ("merged row", text.as_str()),
            ("summary", outcome.to_string().as_str()),
        ] {
            assert!(
                !haystack.contains("sk-secret-value"),
                "key material leaked into {where_}"
            );
        }
        assert!(
            !text.contains("8003"),
            "the harness-assigned port is no row key: {text}"
        );
    }

    /// REV9-01: a mapped string key that is PRESENT but blank meets the same
    /// door as an empty `exe` — refused under its own name, never merged as an
    /// empty knob. An absent optional key is NOT this door (the fixed-point
    /// profile applies without `cwd`/`tokenizer` happily).
    #[test]
    fn a_present_but_blank_mapped_string_is_refused_not_merged() {
        let dir = crate::testutil::temp_dir("import-apply-blank");
        let profile_path = dir.join("profile.json");
        let row = write_fixture(
            &dir,
            "row.yaml",
            "# keep\nname: m\nengine: strata\nserve_model: keep-me\n",
        );
        let before = std::fs::read_to_string(&row).expect("row");
        for (key, json) in [
            ("model_name", r#"{"exe": "C:\\e.exe", "model_name": " "}"#),
            ("cwd", r#"{"exe": "C:\\e.exe", "cwd": ""}"#),
            ("tokenizer", r#"{"exe": "C:\\e.exe", "tokenizer": "   "}"#),
        ] {
            std::fs::write(&profile_path, json).expect("profile");
            let error = apply_strata_row(&profile_path, &row)
                .expect_err("a present-but-blank key must refuse");
            assert!(
                error.contains(&format!("declares an empty `{key}`")),
                "{key}: {error}"
            );
            assert_eq!(
                std::fs::read_to_string(&row).expect("row"),
                before,
                "the {key} refusal must leave the row bytes unchanged"
            );
            // The refusal is also true of emit-only: the door is the import,
            // not the apply.
            let error = strata_yaml(&profile_path).expect_err("emit meets the same door");
            assert!(
                error.contains(&format!("declares an empty `{key}`")),
                "{key}: {error}"
            );
        }
        assert!(
            publish_residues(&dir).is_empty(),
            "a blank-key refusal creates no residues"
        );
    }

    /// REV9-02: a profile that declares no `model_name` must say so on the
    /// apply path — the flag rides the outcome and the summary names it —
    /// because the row keeps its own `serve_model` and silence reads like a
    /// replacement.
    #[test]
    fn the_summary_notes_a_profile_without_model_name() {
        let dir = crate::testutil::temp_dir("import-apply-noname");
        let profile = write_fixture(&dir, "profile.json", r#"{"exe": "C:\\e.exe"}"#);
        let row = write_fixture(
            &dir,
            "row.yaml",
            "name: m\nengine: strata\nserve_model: the rows own\n",
        );
        let outcome = apply_strata_row(&profile, &row).expect("applies");
        assert!(outcome.serve_model_absent, "{outcome:?}");
        let summary = outcome.to_string();
        assert!(
            summary.contains("profile declared no model_name; serve_model was not replaced"),
            "the summary must state the kept key: {summary}"
        );
        let text = std::fs::read_to_string(&row).expect("read back");
        assert!(
            text.contains("serve_model: the rows own"),
            "the row keeps its own serve_model: {text}"
        );
    }

    /// REV9-03: a partial re-apply counts only the lines that actually moved.
    /// The old shape reported the emitted field count, so a one-knob re-apply
    /// claimed a full rewrite; and an `engine: strata` line beside a strata row
    /// is kept byte-verbatim, so it is not counted either.
    #[test]
    fn a_partial_reapply_counts_only_the_keys_that_actually_changed() {
        let dir = crate::testutil::temp_dir("import-apply-count");
        let profile = write_fixture(
            &dir,
            "profile.json",
            r#"{"exe": "C:\\e.exe", "model_name": "m", "gpu": 0}"#,
        );
        let row = write_fixture(&dir, "row.yaml", "name: m\nengine: strata\n");
        let first = apply_strata_row(&profile, &row).expect("first applies");
        assert_eq!(
            first.changed, 3,
            "serve_model, exe and gpu land; the row already reads engine: strata: {first:?}"
        );
        let moved = write_fixture(
            &dir,
            "profile-moved.json",
            r#"{"exe": "C:\\e.exe", "model_name": "m", "gpu": 1}"#,
        );
        let second = apply_strata_row(&moved, &row).expect("second applies");
        assert_eq!(
            second.changed, 1,
            "only strata_gpu moved on the re-apply: {second:?}"
        );
        assert!(!second.unchanged);
        let summary = second.to_string();
        assert!(
            summary.contains("merged 1 key"),
            "the singular count reads true: {summary}"
        );
    }
}
