//! The one-shot strata profile migration (`simplehar-se import strata
//! <profile.json>`, operator ruling 2026-11-02 row 19c): read a manual profile,
//! emit the row yaml the harness renders from, delete nothing. It is the only
//! sanctioned reader of a `C:\Strata` profile — the serve path never opens one.
//!
//! The output is honest about what a strata profile cannot carry: the
//! scheduling keys (`category`, `vram_gb`, `cost`, `load_timeout`,
//! `launch_only_with_at_least_vram_free`) live only in the row, so the output
//! says to merge them from the row being replaced. Harness-forced fields found
//! in the profile (`host`, `port`, key, the `log` home) are reported as dropped,
//! never obeyed; keys the renderer has no knob for are listed as not mapped —
//! nothing is ever silently discarded.

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

/// Renders the row yaml for one strata profile file.
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
    match str("model_name") {
        Some(name) => out.push_str(&format!("engine: strata\nserve_model: {name}\n")),
        None => {
            out.push_str("engine: strata\n");
            if root.contains_key("model_name") {
                wrong_type.push("model_name");
            } else {
                out.push_str(
                    "# not mapped: model_name is absent — `serve_model` must come from the row\n",
                );
            }
        }
    }
    if let Some(exe) = str("exe") {
        out.push_str(&format!("strata_exe: {exe}\n"));
    } else {
        return Err(format!(
            "{} declares no `exe`: nothing to render",
            profile_path.display()
        ));
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
                out.push_str(&format!("strata_args: {}\n", tokens.join(", ")));
            }
        }
    }
    if let Some(value) = root.get("cwd") {
        match value.as_str() {
            Some(cwd) => out.push_str(&format!("strata_cwd: {cwd}\n")),
            None => wrong_type.push("cwd"),
        }
    }
    if let Some(value) = root.get("tokenizer") {
        match value.as_str() {
            Some(tokenizer) => out.push_str(&format!("strata_tokenizer: {tokenizer}\n")),
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
                out.push_str(&format!("strata_lib_dirs: {}\n", entries.join(", ")));
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
                out.push_str(&format!("strata_env: {}\n", tokens.join(" ")));
            }
        }
    }
    for (key, yaml) in [("gpu", "strata_gpu"), ("parallel", "strata_parallel")] {
        if let Some(value) = root.get(key) {
            match value.as_u64() {
                Some(number) => out.push_str(&format!("{yaml}: {number}\n")),
                None => wrong_type.push(key),
            }
        }
    }
    if let Some(value) = root.get("gpus_asked") {
        match value.as_bool() {
            Some(true) => out.push_str("strata_gpus_asked: true\n"),
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
            None => {}
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
        out.push_str(&format!("strata_vision: {}\n", tokens.join(" ")));
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
                out.push_str(&format!("strata_sampling: {}\n", tokens.join(" ")));
            }
        }
    }
    if let Some(on) = root.get("uncensored") {
        match on.as_bool() {
            Some(on) => out.push_str(&format!("strata_uncensored: {on}\n")),
            None => {
                return Err(format!(
                    "{} `uncensored` must be a boolean",
                    profile_path.display()
                ))
            }
        }
    }
    let owned: Vec<&str> = HARNESS_OWNED_KEYS
        .iter()
        .copied()
        .filter(|key| root.contains_key(*key))
        .collect();
    if !owned.is_empty() {
        out.push_str(&format!(
            "# harness-owned, dropped (bind, port, key and log home are harness-forced): {}\n",
            owned.join(", ")
        ));
    }
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
    if !unmapped.is_empty() {
        out.push_str(&format!("# not mapped: {}\n", unmapped.join(", ")));
    }
    Ok(out)
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
}
