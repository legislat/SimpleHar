//! The Project Maya engine adapter (`TASK-MAYA`).
//!
//! Maya 1.1.x is a Python HTTP server (`serve/server.py`) in front of a native
//! engine. The harness launches that server directly, never the `maya.py`
//! launcher: on Windows the launcher appends `--open` (it opens a browser when
//! the model is ready) and it may repair or calibrate a config before starting,
//! and both are decisions the harness must own. `serve.server` is the
//! documented service/Docker door and takes the same config.
//!
//! The row names the interpreter, the Maya root and the operator's JSON config.
//! That config is an L1 artifact: the adapter parses it, validates every path it
//! names, strips `host`, `port`, `api_key`, `idle_unload_s` and `cors_origins`,
//! forces `model_management` off, and stages the scrubbed bytes
//! content-addressed under `.simpleharn/maya-profiles`; the spawn
//! rewrites `--config` to exactly that file. The bind, the port and the key are
//! harness-forced argv (DOCTRINE 2); idle unload is forced off and model
//! management is forced off — the server enables it by default, and with it a
//! local request could unload the booked resident through `/models/unload` or
//! an Ollama-compat `keep_alive: 0` chat — so a resident
//! the harness booked cannot quietly unload itself.
//!
//! The config's `--max-context` figure is read exactly once, beside this
//! validation door: the scan captures the digits-only token and carries it on
//! the entry (the scan-derived `maya_max_context` raw key), so `/har/models`
//! shows the context the staged config really serves while the display door
//! never re-reads the config file.
//!
//! Readiness is the served id on the unauthenticated `/health` body. The server
//! binds its port only after the engine load returns, so a refused connection is
//! `NotYet` by construction, with `load_timeout` the harness deadline.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec, RenderedProfile};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// IPv4 loopback address the harness forces as the Maya server bind (`--host`),
/// the same literal every process-engine row forces: the server is never exposed
/// on the segment, so the row never obeys a config `host`.
pub(crate) const MAYA_ENGINE_HOST: &str = "127.0.0.1";

/// The shortest `load_timeout` a Maya row may declare, in seconds. A big pack
/// loads for minutes and the global 120 s default would kill a healthy load;
/// an absent key is a scan refusal, not a silent 120 s deadline.
pub(crate) const MAYA_MIN_LOAD_TIMEOUT_S: u64 = 300;

/// The typed refusal [`validate_row`] answers with, so the scan can map the
/// class without parsing prose: [`MayaRowError::Missing`] is the missing-key
/// door, [`MayaRowError::MissingPath`] is the model-path door, and
/// [`MayaRowError::Bad`] is the named-key refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MayaRowError {
    /// The row carries no value for a key the launch requires.
    Missing {
        /// Harness name of the model.
        model: String,
        /// The missing key.
        field: &'static str,
    },
    /// A path the row or its config names is not on disk.
    MissingPath {
        /// Harness name of the model.
        model: String,
        /// The key naming the absent path.
        key: String,
        /// The path that is absent.
        path: PathBuf,
    },
    /// The row or its config carries a value the launch refuses.
    Bad {
        /// Harness name of the model.
        model: String,
        /// The offending key (row key or config key).
        key: String,
        /// Why the value is refused (the tail of the engine voice).
        reason: String,
    },
}

impl MayaRowError {
    /// The same refusal in the adapter voice [`EngineAdapter::spec`] returns:
    /// a missing key meets [`EngineError::MissingField`], an absent path meets
    /// [`EngineError::Rejected`] naming the key, and a bad value names its key.
    #[must_use]
    pub(crate) fn into_engine_error(self) -> EngineError {
        match self {
            Self::Missing { model, field } => EngineError::MissingField {
                model,
                field: field.to_string(),
            },
            Self::MissingPath { model, key, path } => EngineError::Rejected {
                engine: "maya".to_string(),
                model,
                reason: format!("`{key}` is not an existing path: {}", path.display()),
            },
            Self::Bad { model, key, reason } => EngineError::Rejected {
                engine: "maya".to_string(),
                model,
                reason: format!("`{key}`: {reason}"),
            },
        }
    }
}

/// The scan-derived raw key carrying one maya row's effective served context:
/// the digits-only `--max-context` figure of the L1 config's `args`, captured
/// once beside [`validate_row`] and read by [`crate::engine::effective_context`].
/// It is not a yaml key: a hand-written line of this name is unknown to the
/// scan and replaced (or removed) by the derived figure, so the page can only
/// ever show the context the staged config actually launches with.
pub(crate) const MAYA_MAX_CONTEXT_KEY: &str = "maya_max_context";

/// The config's effective served context: the digits-only token after
/// `--max-context` in the config's own `args`, spelled with a space
/// (`--max-context 131072`) or with `=` (`--max-context=131072`). The last
/// occurrence wins, the way a later argv word overrides an earlier one; a
/// value slot that is not a non-empty run of ASCII digits is no figure — the
/// same honest reading `strata::effective_context` gives its argv.
fn max_context_of(args: &[&str]) -> Option<String> {
    fn digits(value: &str) -> bool {
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
    }
    let mut found: Option<String> = None;
    let mut words = args.iter();
    while let Some(word) = words.next() {
        if *word == "--max-context" {
            found = words
                .next()
                .filter(|value| digits(value))
                .map(|value| (*value).to_string());
        } else if let Some(value) = word.strip_prefix("--max-context=") {
            found = digits(value).then(|| value.to_string());
        }
    }
    found
}

/// One fully validated Maya row: every path exists, the config's `model_name`
/// is the row's `serve_model`, and the scrubbed config bytes are ready to stage.
/// Produced only by [`validate_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MayaRow {
    /// The interpreter the server runs under (`maya_python`).
    pub python: PathBuf,
    /// The launcher's working directory (`maya_cwd`), where `-m serve.server`
    /// resolves.
    pub cwd: PathBuf,
    /// The served id (`serve_model`), verified at readiness.
    pub alias: String,
    /// The scrubbed config bytes, staged content-addressed.
    pub text: String,
    /// The digits-only `--max-context` figure of the config's `args`, if it
    /// spells one — the effective served context the scan carries for display.
    pub max_context: Option<String>,
}

fn knob<'a>(model: &'a ModelEntry, key: &str) -> Option<&'a str> {
    model
        .raw
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn bad(model: &ModelEntry, key: &str, reason: impl Into<String>) -> MayaRowError {
    MayaRowError::Bad {
        model: model.name.clone(),
        key: key.to_string(),
        reason: reason.into(),
    }
}

fn missing(model: &ModelEntry, field: &'static str) -> MayaRowError {
    MayaRowError::Missing {
        model: model.name.clone(),
        field,
    }
}

/// One absolute path of the required shape: absolute (an argv-relative path
/// would resolve against the child's cwd, not the operator's expectation),
/// existing, and the right kind.
fn checked_path(
    model: &ModelEntry,
    key: &str,
    raw: &str,
    want_dir: bool,
) -> Result<PathBuf, MayaRowError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(bad(model, key, format!("must be an absolute path: {raw}")));
    }
    if want_dir {
        if !path.exists() {
            return Err(MayaRowError::MissingPath {
                model: model.name.clone(),
                key: key.to_string(),
                path: path.to_path_buf(),
            });
        }
        if !path.is_dir() {
            return Err(bad(
                model,
                key,
                format!("is not an existing directory: {raw}"),
            ));
        }
    } else if !path.is_file() {
        if !path.exists() {
            return Err(MayaRowError::MissingPath {
                model: model.name.clone(),
                key: key.to_string(),
                path: path.to_path_buf(),
            });
        }
        return Err(bad(model, key, format!("is not a file: {raw}")));
    }
    Ok(path.to_path_buf())
}

/// One JSON string value from the config, required and non-blank.
fn config_str(
    model: &ModelEntry,
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
) -> Result<String, MayaRowError> {
    config
        .get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| bad(model, key, "must be a non-empty string"))
}

/// One absolute path named by the config value `key`, checked on disk.
fn config_path(
    model: &ModelEntry,
    config: &serde_json::Map<String, serde_json::Value>,
    key: &str,
    want_dir: bool,
) -> Result<PathBuf, MayaRowError> {
    let raw = config_str(model, config, key)?;
    checked_path(model, key, &raw, want_dir)
}

/// The config's `gpu`: a whole number, or a list of whole numbers.
fn check_gpu(
    model: &ModelEntry,
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), MayaRowError> {
    let Some(value) = config.get("gpu") else {
        return Ok(());
    };
    let whole = |value: &serde_json::Value| {
        value
            .as_u64()
            .filter(|n| *n <= u64::from(u32::MAX))
            .map(|n| n as u32)
    };
    match value {
        serde_json::Value::Array(cards) => {
            if cards.is_empty() {
                return Err(bad(model, "gpu", "the list is empty"));
            }
            if cards.iter().any(|card| whole(card).is_none()) {
                return Err(bad(
                    model,
                    "gpu",
                    "every entry must be a non-negative whole GPU index",
                ));
            }
        }
        other => {
            if whole(other).is_none() {
                return Err(bad(
                    model,
                    "gpu",
                    "must be a non-negative whole GPU index or a list of them",
                ));
            }
        }
    }
    Ok(())
}

/// The config's `env`: a map of engine settings. Only the documented user-facing
/// names (`MAYA_*`) pass; a bare `PATH`, `CUDA_VISIBLE_DEVICES` or a legacy
/// `STRATA_*` name is refused by name, because the harness owns the child
/// environment and the settings surface. Values are scalars.
fn check_env(
    model: &ModelEntry,
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), MayaRowError> {
    let Some(value) = config.get("env") else {
        return Ok(());
    };
    let Some(entries) = value.as_object() else {
        return Err(bad(
            model,
            "env",
            "must be an object of NAME=value settings",
        ));
    };
    for (name, value) in entries {
        if !name.starts_with("MAYA_") {
            return Err(bad(
                model,
                &format!("env.{name}"),
                "only `MAYA_*` engine settings pass; the harness owns the rest of the child \
                 environment",
            ));
        }
        if !(value.is_string() || value.is_number() || value.is_boolean()) {
            return Err(bad(
                model,
                &format!("env.{name}"),
                "must be a string, number or boolean",
            ));
        }
    }
    Ok(())
}

/// The vision block, when present: every program and weights file it names must
/// exist before the load gesture. The harness does not manage the block; it
/// refuses a doomed launch instead of letting the server die after the manager
/// has evicted a resident.
fn check_vision(
    model: &ModelEntry,
    config: &serde_json::Map<String, serde_json::Value>,
) -> Result<(), MayaRowError> {
    let Some(value) = config.get("vision") else {
        return Ok(());
    };
    let Some(vision) = value.as_object() else {
        return Err(bad(model, "vision", "must be an object"));
    };
    for key in ["exe", "mmproj", "model"] {
        let raw = config_str(model, vision, key)?;
        checked_path(model, &format!("vision.{key}"), &raw, false)?;
    }
    Ok(())
}

/// Validates one row into a [`MayaRow`]: the single door the adapter and the
/// scan both pass through, so a launch can never be built from a row the other
/// consumer would refuse.
///
/// # Errors
///
/// [`MayaRowError::Missing`] for an absent required row key,
/// [`MayaRowError::MissingPath`] for a config or weights path that is not on
/// disk, [`MayaRowError::Bad`] for a present value the launch refuses.
pub(crate) fn validate_row(model: &ModelEntry) -> Result<MayaRow, MayaRowError> {
    let alias = model
        .serve_model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| missing(model, "serve_model"))?
        .to_string();
    let python = {
        let raw = knob(model, "maya_python").ok_or_else(|| missing(model, "maya_python"))?;
        checked_path(model, "maya_python", raw, false)?
    };
    let cwd = {
        let raw = knob(model, "maya_cwd").ok_or_else(|| missing(model, "maya_cwd"))?;
        checked_path(model, "maya_cwd", raw, true)?
    };
    let config_file = {
        let raw = knob(model, "maya_config").ok_or_else(|| missing(model, "maya_config"))?;
        checked_path(model, "maya_config", raw, false)?
    };
    // A big pack loads for minutes: an absent key would inherit the global
    // 120 s deadline and kill a healthy load. The row states its own number.
    match model.load_timeout_s {
        Some(seconds) if seconds >= MAYA_MIN_LOAD_TIMEOUT_S => {}
        Some(seconds) => {
            return Err(bad(
                model,
                "load_timeout",
                format!(
                    "must be {MAYA_MIN_LOAD_TIMEOUT_S} seconds or more for a Maya load: {seconds}"
                ),
            ));
        }
        None => return Err(missing(model, "load_timeout")),
    }
    match model.cost {
        crate::model::CostClass::Fixed => {
            if model.vram_gb.is_none() {
                return Err(missing(model, "vram_gb"));
            }
        }
        crate::model::CostClass::Greedy => {}
        crate::model::CostClass::Exact => {
            return Err(bad(
                model,
                "cost",
                "maya rows book a fixed footprint (`cost: fixed`) or run greedy \
                 (`cost: greedy`: the expert cache fills VRAM and spills to RAM/SSD; \
                 the launch floor is the gate); the engine reports no computed \
                 requirement to book as `exact`",
            ));
        }
    }
    let bytes = std::fs::read(&config_file).map_err(|_error| MayaRowError::MissingPath {
        model: model.name.clone(),
        key: "maya_config".to_string(),
        path: config_file.clone(),
    })?;
    let text = String::from_utf8(bytes).map_err(|_| MayaRowError::Bad {
        model: model.name.clone(),
        key: "maya_config".to_string(),
        reason: "is not UTF-8 JSON".to_string(),
    })?;
    let text = text.trim_start_matches('\u{feff}');
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|error| MayaRowError::Bad {
            model: model.name.clone(),
            key: "maya_config".to_string(),
            reason: format!("is not JSON: {error}"),
        })?;
    let mut config = value
        .as_object()
        .cloned()
        .ok_or_else(|| bad(model, "maya_config", "must hold a JSON object"))?;
    let model_name = config_str(model, &config, "model_name")?;
    if model_name != alias {
        return Err(bad(
            model,
            "serve_model",
            format!(
                "`{alias}` is not the config's `model_name` (`{model_name}`): the served id \
                 would not match the row"
            ),
        ));
    }
    config_path(model, &config, "exe", false)?;
    config_path(model, &config, "cwd", true)?;
    let tokenizer = config_path(model, &config, "tokenizer", true)?;
    if !tokenizer.join("vocab.json").is_file() {
        return Err(MayaRowError::MissingPath {
            model: model.name.clone(),
            key: "tokenizer".to_string(),
            path: tokenizer.join("vocab.json"),
        });
    }
    // The server treats a missing `args` as none; a present value must still
    // be a list of string tokens, or the argv the server builds from the
    // config is not what the harness validated.
    if let Some(args) = config.get("args") {
        if args
            .as_array()
            .is_none_or(|args| args.iter().any(|arg| !arg.is_string()))
        {
            return Err(bad(model, "args", "must be a list of string tokens"));
        }
    }
    // The effective context rides the config's own argv, read once here beside
    // the shape check that proves the tokens are strings: the display door
    // reads this figure, never the config file again.
    let max_context = config
        .get("args")
        .and_then(serde_json::Value::as_array)
        .and_then(|args| {
            let words: Vec<&str> = args.iter().filter_map(serde_json::Value::as_str).collect();
            max_context_of(&words)
        });
    if let Some(log) = config
        .get("log")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|log| !log.is_empty())
    {
        let path = Path::new(log);
        if !path.is_absolute() {
            return Err(bad(
                model,
                "log",
                format!("must be an absolute path: {log}"),
            ));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            if !parent.is_dir() {
                return Err(MayaRowError::MissingPath {
                    model: model.name.clone(),
                    key: "log".to_string(),
                    path: parent.to_path_buf(),
                });
            }
        }
    }
    check_gpu(model, &config)?;
    check_env(model, &config)?;
    check_vision(model, &config)?;
    // The harness owns these decisions: the bind and the port are forced
    // argv, the key is absent (loopback only), idle unload would break the
    // harness's residency, and CORS is harness-owned (loopback only). Remove
    // them from the staged copy so a fallback can never read them from the
    // engine-side file.
    for key in ["host", "port", "api_key", "idle_unload_s", "cors_origins"] {
        config.remove(key);
    }
    // Model management is the server's own default ON: with it on, a local
    // request could unload the booked resident (`/models/unload`, or the
    // Ollama-compat `keep_alive: 0` chat) without a scheduler release — the
    // harness is the single writer of the residency. Force it off in the
    // staged copy, after the scrub, so an L1 value can never turn it back on.
    config.insert(
        "model_management".to_string(),
        serde_json::Value::Bool(false),
    );
    // The engine binary and cwd stay in the staged copy: the server reads them
    // from the config, not from argv.
    let text = serde_json::to_string_pretty(&serde_json::Value::Object(config))
        .expect("invariant: a JSON object serializes");
    Ok(MayaRow {
        python,
        cwd,
        alias,
        text,
        max_context,
    })
}

/// The Python child needs a home directory (Maya writes caches and conversation
/// state below it) and UTF-8 pipes (the server prints model labels). The harness
/// scrubs the parent environment, so both ride the launch spec.
fn python_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = home.to_string_lossy().into_owned();
        env.insert("USERPROFILE".to_string(), home.clone());
        env.insert("HOME".to_string(), home);
    }
    env
}

/// Maya adapter: argv from the row and the scrubbed config, `/health` reporting
/// the exact served id.
#[derive(Debug, Clone, Default)]
pub struct MayaAdapter;

impl EngineAdapter for MayaAdapter {
    fn engine_id(&self) -> &str {
        "maya"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let row =
            super::policy::engine_policy("maya").expect("the compiled-in table ships a maya row");
        let v = validate_row(model).map_err(MayaRowError::into_engine_error)?;
        let staged_name = crate::process::staged_profile_name(&model.name, &v.text);
        let mut argv = vec![v.python.to_string_lossy().into_owned()];
        argv.extend(["-m".to_string(), "serve.server".to_string()]);
        argv.extend(super::policy::render_forced(row, port, Some(&staged_name)));
        Ok(LaunchSpec {
            argv,
            cwd: v.cwd,
            env: python_env(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::HttpString {
                path: "/health".to_string(),
                expect_status: 200,
                pointer: "/model".to_string(),
                expect: v.alias.clone(),
            },
            container: None,
            script: None,
            rendered_profile: Some(RenderedProfile {
                engine: "maya",
                text: v.text,
            }),
            custody: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::model::{CostClass, ModelEntry};

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// One temp install: interpreter, engine, tokenizer and a config shaped
    /// like the operator's installs.
    fn fixture(overrides: &[(&str, &str)]) -> (std::path::PathBuf, std::path::PathBuf, ModelEntry) {
        let dir = crate::testutil::temp_dir("maya-row");
        let tokenizer = dir.join("tokenizer");
        std::fs::create_dir_all(&tokenizer).expect("tokenizer dir");
        std::fs::write(tokenizer.join("vocab.json"), b"{}").expect("vocab");
        std::fs::write(dir.join("python.exe"), b"p").expect("python");
        std::fs::write(dir.join("maya.exe"), b"e").expect("engine");
        let config = dir.join("maya.json");
        std::fs::write(
            &config,
            serde_json::json!({
                "exe": dir.join("maya.exe"),
                "args": ["--glm-pack", "pack", "--max-context", "131072"],
                "cwd": dir,
                "tokenizer": tokenizer,
                "model_name": "maya-test",
                "gpu": [0],
                "log": dir.join("maya.log"),
            })
            .to_string(),
        )
        .expect("config");
        let python = dir.join("python.exe");
        let mut pairs: Vec<(&str, &str)> = vec![
            ("maya_python", python.to_str().expect("utf-8")),
            ("maya_cwd", dir.to_str().expect("utf-8")),
            ("maya_config", config.to_str().expect("utf-8")),
            ("load_timeout", "900"),
        ];
        pairs.extend(overrides);
        let mut model = ModelEntry {
            name: "maya-test".to_string(),
            engine: "maya".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(10.0),
            ram_gb: None,
            gguf_size_gb: None,
            cost: CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: Some(900),
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            measured_ram_gb: None,
            measured_ram_min_gb: None,
            fit_gb: None,
            model_ref: None,
            serve_model: Some("maya-test".to_string()),
            file: None,
            raw: raw(&pairs),
        };
        for (key, value) in overrides {
            match *key {
                "serve_model" => model.serve_model = Some((*value).to_string()),
                "load_timeout" => model.load_timeout_s = value.parse().ok(),
                "cost" if *value == "greedy" => model.cost = CostClass::Greedy,
                "cost" if *value == "exact" => model.cost = CostClass::Exact,
                "vram_gb" if value.is_empty() => model.vram_gb = None,
                _ => {}
            }
        }
        (dir, config, model)
    }

    fn config_of(model: &ModelEntry) -> serde_json::Value {
        let row = validate_row(model).expect("fixture validates");
        serde_json::from_str(&row.text).expect("rendered JSON")
    }

    #[test]
    fn the_argv_is_the_forced_server_prefix_and_the_staged_config() {
        let (dir, _config, model) = fixture(&[]);
        let spec = MayaAdapter.spec(&model, 18199).expect("fixture renders");
        let expected: Vec<String> = vec![
            dir.join("python.exe").to_string_lossy().into_owned(),
            "-m".to_string(),
            "serve.server".to_string(),
            "--engine".to_string(),
            "maya".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--config".to_string(),
            spec.argv[8].clone(),
            "--port".to_string(),
            "18199".to_string(),
            "--idle-unload".to_string(),
            "0".to_string(),
        ];
        assert_eq!(spec.argv, expected);
        assert!(
            spec.argv[8].ends_with(".json") && spec.argv[8].contains("maya-test"),
            "the staged config slot carries the content-addressed name"
        );
        assert_eq!(
            spec.argv[8],
            crate::process::staged_profile_name(
                &model.name,
                &validate_row(&model).expect("fixture validates").text
            ),
            "the `--config` slot carries exactly the name the staging door mints for these bytes"
        );
        assert_eq!(spec.cwd, dir);
        assert_eq!(spec.env.get("PYTHONUTF8").map(String::as_str), Some("1"));
        assert_eq!(spec.log_name, "maya-test");
        assert_eq!(
            spec.readiness,
            ReadinessSpec::HttpString {
                path: "/health".to_string(),
                expect_status: 200,
                pointer: "/model".to_string(),
                expect: "maya-test".to_string(),
            }
        );
        let profile = spec.rendered_profile.expect("maya stages a config");
        assert_eq!(profile.engine, "maya");
    }

    #[test]
    fn the_staged_config_strips_the_harness_owned_decisions() {
        let (dir, config, model) = fixture(&[]);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        let object = value.as_object_mut().expect("object");
        object.insert("host".to_string(), serde_json::json!("0.0.0.0"));
        object.insert("port".to_string(), serde_json::json!(8006));
        object.insert("api_key".to_string(), serde_json::json!("secret"));
        object.insert("idle_unload_s".to_string(), serde_json::json!(600));
        object.insert(
            "cors_origins".to_string(),
            serde_json::json!(["http://evil.example"]),
        );
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let rendered = config_of(&model);
        for key in ["host", "port", "api_key", "idle_unload_s", "cors_origins"] {
            assert!(
                rendered.get(key).is_none(),
                "the staged config must not carry `{key}`"
            );
        }
        assert_eq!(rendered["model_name"], serde_json::json!("maya-test"));
        assert_eq!(rendered["cwd"], serde_json::json!(dir.to_string_lossy()));
    }

    /// Model management is the server's own default ON, and with it a local
    /// request frees the booked resident outside the scheduler (`/models/unload`,
    /// `keep_alive: 0`). The staged bytes therefore carry `false` whatever L1
    /// says: both an absent key and an explicit `true` are forced off.
    /// Falsifier: dropping the post-scrub insert keeps the absent-key leg
    /// green only until this pin reads the staged bytes, and an `true` L1
    /// passes straight through.
    #[test]
    fn model_management_is_forced_off_whatever_the_config_says() {
        let (_dir, config, model) = fixture(&[]);
        let rendered = config_of(&model);
        assert_eq!(
            rendered["model_management"],
            serde_json::json!(false),
            "a config that omits the key still stages it off (the server's default is on)"
        );
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value["model_management"] = serde_json::json!(true);
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let rendered = config_of(&model);
        assert_eq!(
            rendered["model_management"],
            serde_json::json!(false),
            "an L1 `model_management: true` must not turn the lifecycle door back on"
        );
    }

    /// Maya treats a missing `args` as none; only a present value of the wrong
    /// shape is refused. Falsifier: requiring the key refuses a config Maya
    /// would serve happily, and dropping the shape check lets a JSON object
    /// reach the argv the server builds from the config.
    #[test]
    fn an_absent_args_key_is_accepted_and_a_foreign_shape_is_refused() {
        let (_dir, config, model) = fixture(&[]);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value.as_object_mut().expect("object").remove("args");
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        assert!(validate_row(&model).is_ok(), "absent `args` is legal Maya");
        value["args"] = serde_json::json!({"not": "a list"});
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let error = validate_row(&model)
            .expect_err("args shape")
            .into_engine_error()
            .to_string();
        assert!(error.contains("args"), "must name `args`: {error}");
        value["args"] = serde_json::json!(["ok", 7]);
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        assert!(
            validate_row(&model).is_err(),
            "a non-string entry is refused"
        );
    }

    #[test]
    fn the_render_is_byte_stable() {
        let (_dir, _config, model) = fixture(&[]);
        let first = validate_row(&model).expect("first").text;
        let second = validate_row(&model).expect("second").text;
        assert_eq!(first, second);
    }

    /// FR-08: the effective served context is the config's own `--max-context`
    /// token. Falsifier: accepting a non-digit value slot, losing the `=`
    /// spelling, letting the first occurrence win, or reading the row keys
    /// instead of the argv each fails one leg here.
    #[test]
    fn the_figure_reads_the_config_max_context_token() {
        let cases: &[(&[&str], Option<&str>)] = &[
            (
                &["--glm-pack", "pack", "--max-context", "131072"],
                Some("131072"),
            ),
            (&["--max-context=262144"], Some("262144")),
            (&["--glm-pack", "pack"], None),
            (&["--max-context", "auto"], None),
            (&["--max-context", "--glm-pack"], None),
            (&["--max-context"], None),
            (&["--max-context="], None),
            (&["--max-context=auto"], None),
            (&["--max-context", "-5"], None),
            (&["--max-contexts", "8192"], None),
            // argv truth: the last spelling wins, and a later malformed slot
            // does not resurrect an earlier figure.
            (
                &["--max-context", "8192", "--max-context=131072"],
                Some("131072"),
            ),
            (&["--max-context=131072", "--max-context"], None),
        ];
        for (args, want) in cases {
            assert_eq!(max_context_of(args).as_deref(), *want, "args {args:?}");
        }
    }

    /// FR-08 through the single door: the figure the config spells rides the
    /// validated row. Falsifier: dropping the capture from `validate_row`
    /// leaves the row silent whatever the config says.
    #[test]
    fn validate_row_carries_the_figure_the_config_spells() {
        let (_dir, config, model) = fixture(&[]);
        let row = validate_row(&model).expect("fixture validates");
        assert_eq!(
            row.max_context.as_deref(),
            Some("131072"),
            "the fixture config spells `--max-context 131072`"
        );
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value["args"] = serde_json::json!(["--max-context=262144"]);
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let row = validate_row(&model).expect("`=` form validates");
        assert_eq!(row.max_context.as_deref(), Some("262144"));
        value["args"] = serde_json::json!(["--glm-pack", "pack"]);
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let row = validate_row(&model).expect("args without a figure validate");
        assert_eq!(
            row.max_context, None,
            "a config without the token spells no figure"
        );
    }

    #[test]
    fn a_greedy_row_needs_no_nominal_size() {
        let (_dir, _config, model) = fixture(&[("cost", "greedy"), ("vram_gb", "")]);
        assert!(validate_row(&model).is_ok());
    }

    #[test]
    fn refusals_name_the_offending_key_or_field() {
        let cases: &[(&str, &str, &str)] = &[
            ("maya_python", "Z:/absent/python.exe", "maya_python"),
            ("load_timeout", "120", "load_timeout"),
            ("serve_model", "other", "serve_model"),
            ("cost", "exact", "cost"),
        ];
        for (key, value, named) in cases {
            let (_dir, _config, model) = fixture(&[(key, value)]);
            let error = validate_row(&model)
                .expect_err(key)
                .into_engine_error()
                .to_string();
            assert!(
                error.contains(named),
                "`{key}={value}` refusal must name `{named}`: {error}"
            );
        }
        let (_dir, _config, model) = fixture(&[("maya_config", "Z:/absent.json")]);
        assert!(validate_row(&model).is_err());
    }

    #[test]
    fn a_config_model_name_mismatch_is_refused() {
        let (_dir, config, model) = fixture(&[]);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value["model_name"] = serde_json::json!("other");
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let error = validate_row(&model)
            .expect_err("mismatch")
            .into_engine_error()
            .to_string();
        assert!(error.contains("serve_model") && error.contains("other"));
    }

    #[test]
    fn a_config_env_key_outside_maya_is_refused_by_name() {
        let (_dir, config, model) = fixture(&[]);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value["env"] = serde_json::json!({"PATH": "C:/evil"});
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let error = validate_row(&model)
            .expect_err("env")
            .into_engine_error()
            .to_string();
        assert!(error.contains("env.PATH"));
    }

    #[test]
    fn a_missing_vision_asset_is_a_missing_path() {
        let (dir, config, model) = fixture(&[]);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&config).expect("read")).expect("json");
        value["vision"] = serde_json::json!({
            "exe": dir.join("maya-vision.exe"),
            "mmproj": dir.join("mmproj.gguf"),
            "model": dir.join("vision.gguf"),
        });
        std::fs::write(&config, serde_json::to_string(&value).expect("serialize")).expect("write");
        let error = validate_row(&model).expect_err("vision path");
        assert!(
            matches!(error, MayaRowError::MissingPath { .. }),
            "unexpected refusal: {error:?}"
        );
    }

    #[test]
    fn a_fixed_row_without_vram_is_refused() {
        let (_dir, _config, model) = fixture(&[("vram_gb", "")]);
        let error = validate_row(&model)
            .expect_err("vram")
            .into_engine_error()
            .to_string();
        assert!(error.contains("vram_gb"));
    }

    #[test]
    fn a_bom_config_is_read() {
        let (_dir, config, model) = fixture(&[]);
        let text = std::fs::read_to_string(&config).expect("read");
        std::fs::write(&config, format!("\u{feff}{text}")).expect("write");
        assert!(validate_row(&model).is_ok());
    }
}
