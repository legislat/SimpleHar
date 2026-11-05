//! The basalt engine adapter (`TASK-BASALT` stage 1, option A).
//!
//! Same doctrine as strata template mode (operator ruling 2026-11-02 row 19),
//! generalized: the row yaml names the interpreters, the weights and a few
//! scalars; the harness renders the whole profile itself, stages it
//! content-addressed into its own custody root (`.simpleharn/basalt-profiles`),
//! and rewrites the `--config` slot to exactly the staged file. `host`, `port`
//! and the key are absent from the profile **by construction** and harness-
//! forced in argv; the served id is verified at readiness by an exact
//! [`ReadinessSpec::HttpString`] match against `/v1/models`.
//!
//! Stage 1 is launch-only: no custody sidecar entry, no container, no script.
//! Ownership (custody, unload attribution) is the next slice.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec, RenderedProfile};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The inclusive range `basalt_parallel` admits (upstream `serve.server`
/// caps its worker pool at 8; the harness never admits more).
pub(crate) const PARALLEL_MIN: u32 = 1;
/// See [`PARALLEL_MIN`].
pub(crate) const PARALLEL_MAX: u32 = 8;

/// The typed refusal [`validate_row`] answers with, so the stage-2 owner
/// (custody/admission) can branch on the class without parsing prose:
/// [`BasaltRowError::Missing`] maps to the missing-key door,
/// [`BasaltRowError::MissingPath`] to the model-path door, and
/// [`BasaltRowError::Bad`] to the named-key refusal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BasaltRowError {
    /// The row carries no value for a key the launch requires.
    Missing {
        /// Harness name of the model.
        model: String,
        /// The missing key.
        field: &'static str,
    },
    /// The row's `.basalt` weights path is not a regular file on disk.
    MissingPath {
        /// Harness name of the model.
        model: String,
        /// The weights path that is absent.
        path: PathBuf,
    },
    /// The row carries a value the launch refuses.
    Bad {
        /// Harness name of the model.
        model: String,
        /// The offending yaml key.
        key: String,
        /// Why the value is refused (the tail of the engine voice).
        reason: String,
    },
}

impl BasaltRowError {
    /// The same refusal in the adapter voice [`EngineAdapter::spec`] returns:
    /// a missing key meets [`EngineError::MissingField`], a bad value meets
    /// [`EngineError::Rejected`] naming the key.
    #[must_use]
    pub(crate) fn into_engine_error(self) -> EngineError {
        match self {
            Self::Missing { model, field } => EngineError::MissingField {
                model,
                field: field.to_string(),
            },
            Self::MissingPath { model, path } => EngineError::Rejected {
                engine: "basalt".to_string(),
                model,
                reason: format!("`model`: is not an existing file: {}", path.display()),
            },
            Self::Bad { model, key, reason } => EngineError::Rejected {
                engine: "basalt".to_string(),
                model,
                reason: format!("`{key}`: {reason}"),
            },
        }
    }
}

/// One fully validated basalt row: every path exists, every scalar is in
/// range, the extra-args surface is clean. Produced only by [`validate_row`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BasaltRow {
    /// The `.basalt` weights file (absolute, existing).
    pub model: PathBuf,
    /// The served alias (`serve_model`), verified at readiness.
    pub alias: String,
    /// The interpreter that starts `serve.server` (`basalt_python`).
    pub python: PathBuf,
    /// The interpreter's working directory (`basalt_cwd`).
    pub cwd: PathBuf,
    /// The engine binary the profile names (`basalt_exe`).
    pub engine_bin: PathBuf,
    /// Worker-pool size (`basalt_parallel`, default [`PARALLEL_MIN`]).
    pub parallel: u32,
    /// Operator extra words, each comma entry one argv token, all value-checked.
    pub engine_args: Vec<String>,
}

fn knob<'a>(model: &'a ModelEntry, key: &str) -> Option<&'a str> {
    model
        .raw
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

fn bad(model: &ModelEntry, key: &str, reason: impl Into<String>) -> BasaltRowError {
    BasaltRowError::Bad {
        model: model.name.clone(),
        key: key.to_string(),
        reason: reason.into(),
    }
}

fn missing(model: &ModelEntry, field: &'static str) -> BasaltRowError {
    BasaltRowError::Missing {
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
) -> Result<PathBuf, BasaltRowError> {
    let path = Path::new(raw);
    if !path.is_absolute() {
        return Err(bad(model, key, format!("must be an absolute path: {raw}")));
    }
    if want_dir {
        if !path.is_dir() {
            return Err(bad(
                model,
                key,
                format!("is not an existing directory: {raw}"),
            ));
        }
    } else if !path.is_file() {
        return Err(bad(model, key, format!("is not an existing file: {raw}")));
    }
    Ok(path.to_path_buf())
}

/// Validates one row into a [`BasaltRow`]: the single door the adapter and
/// the stage-2 owner both pass through, so a launch can never be built from
/// a row the other consumer would refuse.
///
/// # Errors
///
/// [`BasaltRowError::Missing`] for an absent required key,
/// [`BasaltRowError::MissingPath`] for a weights path that is not a file,
/// [`BasaltRowError::Bad`] for a present value the launch refuses.
pub(crate) fn validate_row(model: &ModelEntry) -> Result<BasaltRow, BasaltRowError> {
    let weights = model
        .model_ref
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| missing(model, "model"))?;
    let weights_path = Path::new(weights);
    if !weights_path.is_absolute() {
        return Err(bad(
            model,
            "model",
            format!("must be an absolute path: {weights}"),
        ));
    }
    if !weights_path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("basalt"))
    {
        return Err(bad(
            model,
            "model",
            format!("must name a `.basalt` weights file: {weights}"),
        ));
    }
    if !weights_path.is_file() {
        return Err(BasaltRowError::MissingPath {
            model: model.name.clone(),
            path: weights_path.to_path_buf(),
        });
    }
    let alias = model
        .serve_model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| missing(model, "serve_model"))?
        .to_string();
    let python = {
        let raw = knob(model, "basalt_python").ok_or_else(|| missing(model, "basalt_python"))?;
        checked_path(model, "basalt_python", raw, false)?
    };
    let engine_bin = {
        let raw = knob(model, "basalt_exe").ok_or_else(|| missing(model, "basalt_exe"))?;
        checked_path(model, "basalt_exe", raw, false)?
    };
    let cwd = {
        let raw = knob(model, "basalt_cwd").ok_or_else(|| missing(model, "basalt_cwd"))?;
        checked_path(model, "basalt_cwd", raw, true)?
    };
    // Operator ruling 2026-11-04 (`TASK-BASALT-GREEDY`): Basalt is greedy with
    // dynamic RAM offload - it launches below its nominal footprint because
    // the experts spill to RAM, so the launch floor is the gate, not the
    // size. A `cost: greedy` row may omit `vram_gb`: an unknown nominal is
    // the legal greedy case (DOCTRINE 9/12 lineage — the greedy booking takes
    // the room at admit and never needs a nominal to book from), and a present
    // `vram_gb` on a greedy row stays the last measured display value. A
    // `cost: fixed` row still requires the declared figure: the engine reports
    // no projection to book from, so a fixed booking without it is a guess.
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
                "basalt rows book a fixed footprint (`cost: fixed`) or run greedy \
                 (`cost: greedy`: the engine offloads to RAM and the launch floor \
                 is the gate); this engine reports no computed requirement to \
                 book as `exact`",
            ));
        }
    }
    let parallel = match knob(model, "basalt_parallel") {
        None => PARALLEL_MIN,
        Some(raw) => {
            let n: u32 = raw.parse().map_err(|_| {
                bad(
                    model,
                    "basalt_parallel",
                    format!("must be a whole number {PARALLEL_MIN}..={PARALLEL_MAX}: {raw}"),
                )
            })?;
            if !(PARALLEL_MIN..=PARALLEL_MAX).contains(&n) {
                return Err(bad(
                    model,
                    "basalt_parallel",
                    format!("must be in {PARALLEL_MIN}..={PARALLEL_MAX}: {raw}"),
                ));
            }
            n
        }
    };
    let engine_args = match knob(model, "basalt_engine_args") {
        None => Vec::new(),
        Some(value) => {
            check_engine_args_value("basalt", &model.name, value).map_err(|error| match error {
                EngineError::Rejected { reason, .. } => bad(model, "basalt_engine_args", reason),
                other => bad(model, "basalt_engine_args", other.to_string()),
            })?;
            value
                .split(',')
                .map(|entry| entry.trim().to_string())
                .collect()
        }
    };
    Ok(BasaltRow {
        model: weights_path.to_path_buf(),
        alias,
        python,
        cwd,
        engine_bin,
        parallel,
        engine_args,
    })
}

/// The basalt row's [`ExtraWordsContract`] check over one raw
/// `basalt_engine_args` value: a comma-separated list, one word per entry,
/// each entry value-checked against the server-owned flag table
/// ([`crate::engine::policy::BASALT_FORBIDDEN_SERVER_FLAGS`]).
///
/// Exact-name matching only (no abbreviation door): the upstream
/// `serve.server` parser runs with `allow_abbrev=False`, so a unique
/// prefix like `--hos` is an *unknown* option to the server, never an
/// alias for `--host`. A `--flag` entry whose name is forbidden is refused
/// naming the mechanism that owns the decision (slice 10c voice); a
/// single-dash or whitespace-bearing entry is refused as shape. Bare
/// entries are values — data for an allowed flag, like the llama arity rule.
///
/// [`ExtraWordsContract`]: crate::engine::policy::ExtraWordsContract
///
/// # Errors
///
/// [`EngineError::Rejected`] naming the offending entry.
pub(crate) fn check_engine_args_value(
    engine: &str,
    model: &str,
    value: &str,
) -> Result<(), EngineError> {
    let rejected = |reason: String| EngineError::Rejected {
        engine: engine.to_string(),
        model: model.to_string(),
        reason,
    };
    for entry in value.split(',') {
        let token = entry.trim();
        if token.is_empty() {
            return Err(rejected(
                "`basalt_engine_args` carries an empty entry: the list is comma-separated \
                 with no gaps"
                    .to_string(),
            ));
        }
        if token.chars().any(char::is_whitespace) {
            return Err(rejected(format!(
                "`basalt_engine_args` entries carry exactly one word each (a flag and its \
                 value are separate comma entries or spelled `--flag=value`): {token}"
            )));
        }
        let name = token.split('=').next().unwrap_or(token);
        if let Some(flag) = super::policy::forbidden_basalt_server_flag(name) {
            return Err(rejected(format!(
                "`basalt_engine_args` refuses `{name}` — harness-owned flag: {}; \
                 remove the word from the yaml",
                super::policy::basalt_server_arg_owner(flag).unwrap_or("the harness writes it")
            )));
        }
        if name.starts_with('-') && !name.starts_with("--") {
            return Err(rejected(format!(
                "`basalt_engine_args` speaks long options only (`--flag` or `--flag=value`): \
                 {token}"
            )));
        }
    }
    Ok(())
}

/// The basalt profile: the three figures only — the alias the server serves,
/// the weights it loads, the pool it sizes. Every network/key placement the
/// strata profile also keeps out rides here the same way: absent by
/// construction, harness-forced in argv (row 19 posture, §5, §22[1]).
fn render_profile(row: &BasaltRow) -> String {
    let mut server = serde_json::Map::new();
    server.insert("alias".to_string(), serde_json::json!(row.alias));
    server.insert(
        "model".to_string(),
        serde_json::json!(row.model.to_string_lossy()),
    );
    server.insert("parallel".to_string(), serde_json::json!(row.parallel));
    let mut profile = serde_json::Map::new();
    profile.insert("server".to_string(), serde_json::Value::Object(server));
    // BTreeMap key order makes the render byte-stable across builds (the
    // staged name is a content address, strata's same rule).
    serde_json::to_string_pretty(&serde_json::Value::Object(profile))
        .expect("invariant: a string/number map serializes")
}

/// The Python child needs a home directory for Basalt's model cache. The
/// harness scrubs the parent environment, so pass the user home through the
/// launch spec; `Path.home()` cannot resolve without it (release v1.1.1
/// manual-test failure).
fn python_home_env() -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        let home = home.to_string_lossy().into_owned();
        env.insert("USERPROFILE".to_string(), home.clone());
        env.insert("HOME".to_string(), home);
    }
    env
}

/// Basalt adapter: argv from data, `/health` `loaded != false` **and** the
/// exact served id on `/v1/models`.
#[derive(Debug, Clone, Default)]
pub struct BasaltAdapter;

impl EngineAdapter for BasaltAdapter {
    fn engine_id(&self) -> &str {
        "basalt"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let row = super::policy::engine_policy("basalt")
            .expect("the compiled-in table ships a basalt row");
        let v = validate_row(model).map_err(BasaltRowError::into_engine_error)?;
        let text = render_profile(&v);
        let staged_name = crate::process::staged_profile_name(&model.name, &text);
        let mut argv = vec![v.python.to_string_lossy().into_owned()];
        argv.extend(super::policy::render_forced(row, port, Some(&staged_name)));
        argv.extend([
            "--engine-bin".to_string(),
            v.engine_bin.to_string_lossy().into_owned(),
            "--alias".to_string(),
            v.alias.clone(),
            "--gpu".to_string(),
            "0".to_string(),
            "--parallel".to_string(),
            v.parallel.to_string(),
        ]);
        argv.extend(v.engine_args.iter().cloned());
        Ok(LaunchSpec {
            argv,
            cwd: v.cwd,
            env: python_home_env(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Then(
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
                Box::new(ReadinessSpec::HttpString {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                    pointer: "/data/0/id".to_string(),
                    expect: v.alias.clone(),
                }),
            ),
            container: None,
            script: None,
            rendered_profile: Some(RenderedProfile {
                engine: "basalt",
                text,
            }),
            // Stage 1 rides the job object alone; the custody slice claims this
            // launch next (custody_record_writes_are_the_strata_launches_alone
            // pins that no other row may claim it before then).
            custody: None,
        })
    }
}

/// Reads the Basalt `GET /metrics` snapshot into a measured footprint
/// (`TASK-ENGINE-MEASUREMENT` phase 5, card capture 2026-11-04). The sampler
/// calls this once at readiness and on every elastic tick. Every refusal
/// answers `None` — absence, never a guess — and the sampler then takes the
/// witness-based process-tree fallback (doctrine 4).
///
/// The capture retired the phase-2 path: `GET /status` is liveness only and
/// `GET /v1/status` has no `engine.info.vram` (its `vram` object is
/// `{"elastic": false}` and `machine.gpu` is null on this serve). The only
/// engine-own figure is `expert_cache_mib` in the `engine` block of
/// `GET /metrics`, and this parse books that member alone — the captured
/// 61,363 MiB = 59.9 GiB.
///
/// `arena_mib` and `conversation_cache_mib` are excluded on purpose: the
/// captured components (59.9 expert + 19.8 arena + 4.0 conversation = 83.7
/// GiB) EXCEED the 72 GiB card, because this serve offloads to RAM
/// (`--ple-io ram`; RAM 72.4 of 95.1 GiB used) — for a spilling engine a
/// component sum is not a VRAM footprint, exactly as the engine's own word
/// "not a footprint" records in the capture. The measured row (67.4) stays
/// the booking floor: this figure only ever books growth above the declared
/// figure (attribution semantics unchanged). The old `reserve_mib` leg does
/// not exist on this engine's surface: `POST /v1/vram` answers 400 for a
/// non-elastic engine (`vram_elastic: 0`), so no reserve figure is bookable
/// here and none is required.
///
/// Refusal is always `None`: an error envelope (a top-level `error`), an
/// `engine` member that is not an object (the real `/v1/status` answers the
/// version STRING there), or `expert_cache_mib` missing, non-numeric,
/// negative, or non-finite. `EngineMeasurement::ram_gib` stays `None`: the
/// captured RAM figure rides `/v1/status.machine.ram`, not the metrics
/// block, and this parser never sees it.
///
/// Fall-through rule (task non-goal, ORCHESTRATION: "Basalt never use the
/// Python parent's per-process figure"): when this parse refuses, the
/// sampler's fallback sums the witness's per-process bytes over the engine
/// pid and ALL its descendants — the tree sum is allowed (the engine hangs
/// two levels below the harness child on the captured serves); the Python
/// parent's per-process figure **alone** is banned.
pub(crate) fn measurement_from_metrics(
    body: &serde_json::Value,
) -> Option<crate::engine::EngineMeasurement> {
    if body.get("error").is_some() {
        return None; // the engine's own refusal voice is absence, not a figure
    }
    let engine = body.pointer("/engine")?.as_object()?;
    let expert_cache_mib = engine
        .get("expert_cache_mib")
        .and_then(serde_json::Value::as_f64)?;
    if !expert_cache_mib.is_finite() || expert_cache_mib < 0.0 {
        return None;
    }
    Some(crate::engine::EngineMeasurement {
        gpu_gib: expert_cache_mib / 1024.0,
        ram_gib: None,
        source: crate::engine::MeasurementSource::EngineStatus,
        confidence: crate::engine::MeasurementConfidence::High,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::engine::policy;
    use crate::model::{CostClass, ModelEntry};

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// A temp install: `.basalt` weights, an interpreter file, an engine file,
    /// a cwd directory — the shape `validate_row` walks.
    fn fixture(overrides: &[(&str, &str)]) -> (std::path::PathBuf, ModelEntry) {
        let dir = crate::testutil::temp_dir("basalt-row");
        let weights = dir.join("m.basalt");
        std::fs::write(&weights, b"w").expect("weights");
        std::fs::write(dir.join("python.exe"), b"p").expect("python");
        std::fs::write(dir.join("basalt-engine.exe"), b"e").expect("engine");
        let python = dir.join("python.exe");
        let engine_bin = dir.join("basalt-engine.exe");
        let mut pairs: Vec<(&str, &str)> = vec![
            ("basalt_python", python.to_str().expect("utf-8")),
            ("basalt_exe", engine_bin.to_str().expect("utf-8")),
            ("basalt_cwd", dir.to_str().expect("utf-8")),
        ];
        pairs.extend(overrides);
        let mut model = ModelEntry {
            name: "basalt-test".to_string(),
            engine: "basalt".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(10.0),
            ram_gb: None,
            gguf_size_gb: None,
            cost: CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            measured_ram_gb: None,
            measured_ram_min_gb: None,
            fit_gb: None,
            model_ref: Some(weights.to_str().expect("utf-8").to_string()),
            serve_model: Some("basalt-test".to_string()),
            file: None,
            raw: raw(&pairs),
        };
        for (key, value) in overrides {
            if *key == "serve_model" {
                model.serve_model = Some((*value).to_string());
            }
        }
        (dir, model)
    }

    fn spec_of(overrides: &[(&str, &str)]) -> LaunchSpec {
        let (_dir, model) = fixture(overrides);
        BasaltAdapter.spec(&model, 18199).expect("fixture renders")
    }

    #[test]
    fn the_argv_is_the_exact_forced_prefix_then_the_row_figures() {
        let (dir, model) = fixture(&[
            ("basalt_parallel", "3"),
            ("basalt_engine_args", "--lazy-off"),
        ]);
        let spec = BasaltAdapter.spec(&model, 18199).expect("fixture renders");
        let placeholder = spec.argv[6].clone();
        let expected: Vec<String> = vec![
            dir.join("python.exe").to_string_lossy().into_owned(),
            "-m".to_string(),
            "serve.server".to_string(),
            "--host".to_string(),
            "127.0.0.1".to_string(),
            "--config".to_string(),
            placeholder,
            "--port".to_string(),
            "18199".to_string(),
            "--engine-bin".to_string(),
            dir.join("basalt-engine.exe").to_string_lossy().into_owned(),
            "--alias".to_string(),
            "basalt-test".to_string(),
            "--gpu".to_string(),
            "0".to_string(),
            "--parallel".to_string(),
            "3".to_string(),
            "--lazy-off".to_string(),
        ];
        assert_eq!(spec.argv, expected);
        assert_eq!(spec.cwd, dir);
    }

    #[test]
    fn the_config_placeholder_is_the_profiles_own_content_address() {
        let spec = spec_of(&[]);
        let text = spec
            .rendered_profile
            .as_ref()
            .expect("basalt renders")
            .text
            .clone();
        assert_eq!(
            spec.rendered_profile.as_ref().expect("tag").engine,
            "basalt"
        );
        let name = spec
            .argv
            .iter()
            .position(|a| a == "--config")
            .and_then(|at| spec.argv.get(at + 1))
            .expect("--config carries its placeholder")
            .clone();
        assert_eq!(
            name.as_str(),
            crate::process::staged_profile_name("basalt-test", &text),
            "the argv placeholder must be the render's own content address"
        );
    }

    #[test]
    fn the_profile_carries_the_three_figures_and_nothing_else() {
        let spec = spec_of(&[("basalt_parallel", "4")]);
        let text = spec.rendered_profile.as_ref().expect("render").text.clone();
        let profile: serde_json::Value = serde_json::from_str(&text).expect("rendered JSON");
        assert_eq!(profile["server"]["alias"], "basalt-test");
        assert_eq!(profile["server"]["parallel"], 4);
        assert!(profile["server"]["model"]
            .as_str()
            .expect("model")
            .ends_with(".basalt"));
        assert_eq!(profile.as_object().expect("top").len(), 1);
        assert_eq!(profile["server"].as_object().expect("server").len(), 3);
        for banned in [
            "host",
            "port",
            "api_key",
            "api-key",
            "gpu",
            "config",
            "engine_bin",
            "engine-bin",
            "python",
        ] {
            assert!(
                !text.contains(banned),
                "the rendered profile must never carry `{banned}`: {text}"
            );
        }
    }

    #[test]
    fn readiness_is_health_flag_then_exact_served_id() {
        let spec = spec_of(&[]);
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Then(
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
                Box::new(ReadinessSpec::HttpString {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                    pointer: "/data/0/id".to_string(),
                    expect: "basalt-test".to_string(),
                }),
            )
        );
        let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
        if let Some(home) = home {
            let home = home.to_string_lossy();
            assert_eq!(
                spec.env.get("USERPROFILE").map(String::as_str),
                Some(home.as_ref()),
                "the Python child needs USERPROFILE for the Basalt cache"
            );
            assert_eq!(
                spec.env.get("HOME").map(String::as_str),
                Some(home.as_ref()),
                "the Python child needs HOME for Path.home"
            );
        } else {
            assert!(spec.env.is_empty());
        }
        assert_eq!(spec.log_name, "basalt-test");
        assert_eq!(spec.container, None);
        assert_eq!(spec.script, None);
        assert_eq!(
            spec.custody, None,
            "stage 1 launches ride the job object alone"
        );
    }

    #[test]
    fn a_missing_required_key_refuses_as_the_missing_door() {
        let (_dir, mut model) = fixture(&[]);
        model.raw.remove("basalt_exe");
        let error = BasaltAdapter
            .spec(&model, 18199)
            .expect_err("no engine bin");
        assert_eq!(
            error,
            EngineError::MissingField {
                model: "basalt-test".to_string(),
                field: "basalt_exe".to_string(),
            }
        );
    }

    #[test]
    fn bad_shapes_refuse_naming_the_key() {
        let cases: Vec<(&str, &str)> = vec![
            ("basalt_python", "python.exe"),
            ("basalt_python", "C:\\nowhere\\python.exe"),
            ("basalt_exe", "C:\\Windows\\Temp"),
            ("basalt_cwd", "relative-dir"),
            ("basalt_parallel", "0"),
            ("basalt_parallel", "9"),
            ("basalt_parallel", "two"),
        ];
        for (key, value) in cases {
            let (_dir, model) = fixture(&[(key, value)]);
            let error = BasaltAdapter
                .spec(&model, 18199)
                .expect_err("`{key}` must refuse");
            let EngineError::Rejected { reason, .. } = error else {
                panic!("`{key}`=`{value}` must refuse as Rejected, got {error:?}");
            };
            assert!(reason.contains(&format!("`{key}`")), "{reason}");
        }
        let (_dir, mut model) = fixture(&[]);
        model.serve_model = None;
        assert!(matches!(
            BasaltAdapter.spec(&model, 18199),
            Err(EngineError::MissingField { field, .. }) if field == "serve_model"
        ));
        let (_dir, mut model) = fixture(&[]);
        model.model_ref = Some("C:/m.gguf".to_string());
        assert!(matches!(
            BasaltAdapter.spec(&model, 18199),
            Err(EngineError::Rejected { .. })
        ));
        let (_dir, mut model) = fixture(&[]);
        model.cost = CostClass::Exact;
        assert!(matches!(
            BasaltAdapter.spec(&model, 18199),
            Err(EngineError::Rejected { .. })
        ));
        let (_dir, mut model) = fixture(&[]);
        model.vram_gb = None;
        assert!(matches!(
            BasaltAdapter.spec(&model, 18199),
            Err(EngineError::MissingField { field, .. }) if field == "vram_gb"
        ));
    }

    /// Operator ruling 2026-11-04 (`TASK-BASALT-GREEDY`): Basalt is greedy
    /// with dynamic RAM offload, so a `cost: greedy` row builds a spec even
    /// when it keeps the last measured `vram_gb` (a present figure is
    /// display-only under greedy — the booking clamps to the room at admit).
    /// Falsifier: the pre-ruling guard refused the class outright.
    #[test]
    fn a_greedy_row_with_a_declared_vram_gb_builds_a_spec() {
        let (_dir, mut model) = fixture(&[]);
        model.cost = CostClass::Greedy;
        let row = validate_row(&model).expect("a greedy row with `vram_gb` validates");
        assert_eq!(row.alias, "basalt-test");
    }

    /// An unknown nominal is the legal greedy case (DOCTRINE 9/12 lineage:
    /// the greedy booking takes the room at admit and the launch floor is the
    /// gate), so a greedy row WITHOUT `vram_gb` builds a spec. Falsifier: the
    /// pre-ruling guard demanded `vram_gb` for every class and the row
    /// vanished from the catalog (the live 2026-11-04 failure).
    #[test]
    fn a_greedy_row_without_vram_gb_builds_a_spec() {
        let (_dir, mut model) = fixture(&[]);
        model.cost = CostClass::Greedy;
        model.vram_gb = None;
        let row = validate_row(&model).expect("a greedy row without `vram_gb` validates");
        assert_eq!(row.alias, "basalt-test");
    }

    /// The remaining refusal leg: `exact` is a precision claim this engine
    /// cannot answer (no computed requirement, no projection), and the refusal
    /// names the `cost` key and the two bookable classes.
    #[test]
    fn an_exact_row_refuses_naming_the_cost_key() {
        let (_dir, mut model) = fixture(&[]);
        model.cost = CostClass::Exact;
        let error = BasaltAdapter
            .spec(&model, 18199)
            .expect_err("`cost: exact` must refuse");
        let EngineError::Rejected { reason, .. } = error else {
            panic!("`cost: exact` must refuse as Rejected, got {error:?}");
        };
        assert!(reason.contains("`cost`"), "{reason}");
        assert!(
            reason.contains("greedy") && reason.contains("fixed"),
            "the refusal names the bookable classes: {reason}"
        );
    }

    #[test]
    fn server_owned_flags_refuse_from_engine_args_naming_their_owner() {
        for flag in policy::BASALT_FORBIDDEN_SERVER_FLAGS {
            let value = format!("{flag}, --temp=0.7");
            let error = check_engine_args_value("basalt", "t", &value)
                .expect_err("`{flag}` is server-owned and must refuse");
            let EngineError::Rejected { reason, .. } = error else {
                panic!("`{flag}` must refuse as Rejected");
            };
            assert!(
                reason.contains(flag) && reason.contains("harness-owned"),
                "the refusal names the flag and its owner: {reason}"
            );
        }
    }

    #[test]
    fn allowed_engine_args_pass_and_reach_the_argv_verbatim() {
        // None of these names is on the server-owned table (`--temperature`
        // and `--top-p` are; `--temp` is not an exact forbidden name, and the
        // upstream strict parser rejects whatever it does not speak itself).
        let spec = spec_of(&[("basalt_engine_args", "--temp=0.7, --warmup, 42")]);
        for token in ["--temp=0.7", "--warmup", "42"] {
            assert!(spec.argv.iter().any(|t| t == token), "{token} missing");
        }
    }

    #[test]
    fn malformed_engine_args_entries_refuse() {
        for value in ["--temp=1, ", "--temp 1", "-t 1", "-t"] {
            let error = check_engine_args_value("basalt", "t", value)
                .expect_err("must refuse the malformed list");
            assert!(
                matches!(error, EngineError::Rejected { .. }),
                "{value}: {error}"
            );
        }
        assert!(check_engine_args_value("basalt", "t", "--temp=0.7").is_ok());
    }

    #[test]
    fn an_abbreviation_is_not_the_forbidden_name() {
        // Upstream `allow_abbrev=False`: `--hos` is unknown to the server, not
        // an alias for `--host` — the harness refuses exact names only, and the
        // server rejects the unknown word itself.
        assert!(check_engine_args_value("basalt", "t", "--hos").is_ok());
        assert!(policy::forbidden_basalt_server_flag("--hos").is_none());
        assert!(policy::forbidden_basalt_server_flag("--host").is_some());
    }

    #[test]
    fn validate_row_is_the_single_door_with_the_typed_error() {
        let (_dir, model) = fixture(&[("basalt_parallel", "2")]);
        let row = validate_row(&model).expect("fixture row validates");
        assert_eq!(row.alias, "basalt-test");
        assert_eq!(row.parallel, 2);
        let (_dir, mut model) = fixture(&[]);
        model.raw.remove("basalt_cwd");
        assert_eq!(
            validate_row(&model),
            Err(BasaltRowError::Missing {
                model: "basalt-test".to_string(),
                field: "basalt_cwd",
            })
        );
        let (_dir, mut model) = fixture(&[]);
        model.serve_model = Some("".to_string());
        assert_eq!(
            validate_row(&model),
            Err(BasaltRowError::Missing {
                model: "basalt-test".to_string(),
                field: "serve_model",
            })
        );
    }

    #[test]
    fn parallel_defaults_to_one() {
        let (_dir, model) = fixture(&[]);
        let row = validate_row(&model).expect("validates");
        assert_eq!(row.parallel, PARALLEL_MIN);
    }

    /// The exact `GET /metrics` body captured 2026-11-04 from the manual
    /// 8003 serve, trimmed ONLY by dropping the irrelevant top-level `history` member;
    /// every field the parse reads plus the full surrounding shape is kept
    /// byte-for-byte from
    /// `handoff/evidence/2026-11-04-engine-measurement/captures/basalt-metrics.txt`.
    const CAPTURED_METRICS: &str = r#"{"engine": {"model": "Qwen3.8-Flash-Next UD-Q4_K_XL", "max_context": 262144, "version": "0.2.1", "logit_bias": 248320, "context": 262144, "kv": "int8", "kv_resident": 0, "expert_slots": 20544, "expert_cache_mib": 61363, "expert_slots_primary": 20544, "expert_cache_primary_mib": 61363, "spec": 8, "mtp_max": 8, "lookup": 3, "vram_free_mib": 2948, "cvec": 0, "arena_mib": 20288, "pool_workers": 7, "pcie_frac": "0.55", "conversation_cache_mib": 4096, "conversation_cache_slots": 4, "conversation_cache_min_free_mib": 2560, "conversation_cache_min_tokens": 0, "tail_role_token": -1, "vram_elastic": 0, "quant": "Q4_K_XL", "gpus": "0000:01:00.0", "engine": "0.2.1", "text_prompt": 1, "file_experts_bytes": 0}, "live": {"state": "idle", "load": null, "queued": 0, "slots": {"total": 1, "active": 0, "queue_depth": 0}, "queue": [], "prompt_tokens": null, "generated": null, "elapsed_s": null, "tok_s": null, "prefill_tok_s_mean": null}, "requests": [], "best": {}, "hardware": {"gpus": [], "cpu": 20.7, "ram_used": 77716008960, "ram_total": 102096601088, "disk_read_mb": 0.06732642228091411}, "hardware_static": {"gpus": [], "cpu_name": "AMD64 Family 25 Model 97 Stepping 2, AuthenticAMD", "cores": 8, "threads": 16, "driver": null, "cuda": null}}"#;

    /// The real `GET /status` body (liveness only).
    const CAPTURED_STATUS: &str = r#"{"busy": false, "queued": 0}"#;

    /// The real `GET /v1/status` body: `vram` is `{"elastic": false}`,
    /// `machine.gpu` is null — no footprint member anywhere.
    const CAPTURED_V1STATUS: &str = r#"{"service": "basalt", "model": "Qwen3.8-Flash-Next UD-Q4_K_XL", "loaded": true, "loading": false, "load": null, "asleep": false, "auto_load": true, "structured_output": {"formats": ["json_object", "json_schema"], "method": "grammar", "constrained_decoding": true, "stream_buffered": false}, "engine": "0.2.1", "started": 1793795596, "uptime_s": 61, "cache_max_tokens": 262144, "context": {"native": 262144, "max_positions": 262144}, "concurrency": {"serving": 1, "requested": 1}, "dialects": ["/v1/chat/completions", "/v1/messages", "/v1/responses"], "vision": {"enabled": true, "available": true, "error": null}, "activity": {"requests": 0, "in_flight": 0, "last_request_at": 1793795596}, "last_timings": null, "vram": {"elastic": false}, "machine": {"at": 1793795658, "gpu": null, "ram": {"used_gib": 72.4, "total_gib": 95.1}}}"#;

    fn captured_metrics() -> serde_json::Value {
        serde_json::from_str(CAPTURED_METRICS).expect("the captured body parses")
    }

    /// Phase-5 pin (capture): the real captured `/metrics` body books
    /// `expert_cache_mib` ONLY — 61,363 MiB = 59.9 GiB — not the component
    /// sum: expert + arena (20,288) + conversation (4,096) would sum to
    /// 83.7 GiB, over the 72 GiB card, because this serve offloads to RAM
    /// (`--ple-io ram`). Falsifier: sum the components and this equality
    /// answers 83.7 against a card that cannot hold it.
    #[test]
    fn the_captured_metrics_body_books_the_expert_cache_only() {
        let measurement =
            measurement_from_metrics(&captured_metrics()).expect("the captured body parses");
        assert_eq!(
            measurement.source,
            crate::engine::MeasurementSource::EngineStatus
        );
        assert_eq!(
            measurement.confidence,
            crate::engine::MeasurementConfidence::High,
            "the engine's own metrics word books with High confidence"
        );
        assert_eq!(measurement.ram_gib, None);
        assert!(
            (measurement.gpu_gib - (61_363.0 / 1024.0)).abs() < 1e-9,
            "expert_cache_mib alone books 59.9 GiB, got {}",
            measurement.gpu_gib
        );
        let components = (61_363.0 + 20_288.0 + 4_096.0) / 1024.0;
        assert!(
            components > 72.0,
            "the captured component sum ({components:.1} GiB) exceeds the 72 GiB card: \
             a spilling engine's sum is not a VRAM footprint"
        );
    }

    /// Phase-5 pin: the shipped phase-2 routes answer nothing. The real
    /// `/status` body is liveness only and the real `/v1/status` body has no
    /// footprint member (its `engine` member is the version STRING, which
    /// the pointer refuses as a non-object). Falsifier: read
    /// `engine.info.vram` and one of these answers `Some`.
    #[test]
    fn the_captured_status_and_v1status_bodies_answer_none() {
        for (route, text) in [
            ("/status", CAPTURED_STATUS),
            ("/v1/status", CAPTURED_V1STATUS),
        ] {
            let body: serde_json::Value =
                serde_json::from_str(text).expect("the captured body parses");
            assert_eq!(
                measurement_from_metrics(&body),
                None,
                "the real `{route}` body carries no /metrics engine block: {body}"
            );
        }
    }

    /// Phase-5 chain pin on the captured body: fake axum server serving the
    /// capture at `/metrics`, the runtime's generic GET, then this parse —
    /// the route the sampler's path map now reads for basalt.
    #[tokio::test]
    async fn the_metrics_chain_lands_the_expert_cache_on_the_captured_figure() {
        use crate::engine::EngineRuntime as _;
        let body = CAPTURED_METRICS;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("addr").port();
        let app =
            axum::Router::new().route("/metrics", axum::routing::get(move || async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let runtime = crate::process::ProcessRuntime::new();
        let json = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runtime.engine_status_json(port, "/metrics"),
        )
        .await
        .expect("metrics answer")
        .expect("the captured body parses as JSON");
        let measurement = measurement_from_metrics(&json).expect("the captured snapshot parses");
        assert_eq!(
            measurement.source,
            crate::engine::MeasurementSource::EngineStatus
        );
        assert_eq!(
            measurement.confidence,
            crate::engine::MeasurementConfidence::High
        );
        assert!(
            (measurement.gpu_gib - 61_363.0 / 1024.0).abs() < 1e-9,
            "61,363 MiB = 59.9 GiB, got {}",
            measurement.gpu_gib
        );
        assert_eq!(measurement.ram_gib, None, "no RAM figure in this body");
    }

    /// Phase-5 pin: the engine's error envelope is absence, even when it
    /// rides next to figures that would otherwise parse — a refusal voice
    /// never doubles as a footprint. The error text is the real 2026-11-04
    /// v1-pack refusal; the poisoned body is the REAL capture.
    #[test]
    fn an_error_envelope_is_absence() {
        assert_eq!(
            measurement_from_metrics(
                &serde_json::json!({"error": "RuntimeError: .basalt version 1, this build reads version 2"})
            ),
            None
        );
        let mut poisoned = captured_metrics();
        poisoned["error"] = serde_json::json!("engine refused");
        assert_eq!(
            measurement_from_metrics(&poisoned),
            None,
            "error wins over figures"
        );
    }

    /// Absence pins for the shapes around the metrics block: empty roots, a
    /// null or absent engine, the `/v1/status` engine-as-version-string, and
    /// the DEAD phase-2 path (the shipped `engine.info.vram` triple answers
    /// nothing now) — the sampler keeps its fallback legs, never a
    /// zero-booked footprint.
    #[test]
    fn a_body_without_the_captured_engine_block_is_absence() {
        for body in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"engine": {}}),
            serde_json::json!({"engine": null}),
            serde_json::json!({"engine": "0.2.1"}),
            serde_json::json!({"engine": {"info": {"vram": {"expert_cache_mib": 66048, "vram_free_mib": 2548, "reserve_mib": 3072}}}}),
            serde_json::json!({"vram": {"expert_cache_mib": 66048, "vram_free_mib": 2548, "reserve_mib": 3072}}),
        ] {
            assert_eq!(
                measurement_from_metrics(&body),
                None,
                "a body without the captured engine block is absence: {body}"
            );
        }
    }

    /// A malformed engine block is absence — `expert_cache_mib` must be a
    /// present, finite, non-negative number.
    #[test]
    fn the_expert_cache_member_must_be_a_finite_non_negative_number() {
        let good = serde_json::json!({"engine": {"expert_cache_mib": 61363}});
        assert!(
            measurement_from_metrics(&good).is_some(),
            "the captured member parses"
        );
        for bad in [
            serde_json::json!("61363"),
            serde_json::json!(null),
            serde_json::json!(-1),
        ] {
            let mut malformed = good.clone();
            malformed["engine"]["expert_cache_mib"] = bad.clone();
            assert_eq!(
                measurement_from_metrics(&malformed),
                None,
                "`expert_cache_mib`={bad} is not a figure"
            );
        }
        let mut missing = good.clone();
        missing["engine"]
            .as_object_mut()
            .expect("engine")
            .remove("expert_cache_mib");
        assert_eq!(
            measurement_from_metrics(&missing),
            None,
            "a block without the expert cache is absence"
        );
    }

    /// The `vram_elastic` decision (doc comment): no launch flag gates this
    /// parse. The captured engine block carries its own elasticity figure —
    /// the parse reads the expert cache alone, so an elastic and a fixed
    /// block answer the same figure. Falsifier: make the parse consult the
    /// elasticity member and the two answers diverge.
    #[test]
    fn no_launch_flag_gates_the_parse() {
        let mut elastic = serde_json::json!({"engine": {"expert_cache_mib": 61363}});
        let mut fixed = elastic.clone();
        elastic["engine"]["vram_elastic"] = serde_json::json!(1);
        fixed["engine"]["vram_elastic"] = serde_json::json!(0);
        assert_eq!(
            measurement_from_metrics(&fixed),
            measurement_from_metrics(&elastic),
            "the parse is blind to any elasticity claim"
        );
        assert!(measurement_from_metrics(&elastic).is_some());
    }

    /// The cross-file premise of the sampler's fallback pin
    /// (`scheduler::tests::an_unparsable_status_shape_falls_back_to_the_witness_loop`):
    /// the scalar `vram` body that fake answers with must stay refused by
    /// this parse (the metrics block requires `/engine/expert_cache_mib`),
    /// so that pin keeps testing the fallback leg honestly.
    #[test]
    fn the_scalar_vram_body_stays_refused() {
        assert_eq!(
            measurement_from_metrics(&serde_json::json!({"engine": {"info": {"vram": 67.4}}})),
            None
        );
    }
}
