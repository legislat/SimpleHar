//! The strata engine adapter (DESIGN Step 7; port of the working v3.3 path).
//!
//! Strata is a Python server (`serve.server`) driven entirely by a profile JSON:
//! the harness passes only the interpreter, the module, the `--config` path and
//! the harness-assigned `--port` — weights, quant, context and cache behavior all
//! live in the profile. Readiness needs the JSON `loaded` flag because strata's
//! `/health` answers 200 while the weights are still streaming onto the card
//! (v3.3 `engine.rs:208-226` parity).
//!
//! There is no preload in this harness (engine work is on-demand only), and the
//! yamls carry no autostart keys: a strata engine spawns only when an operator
//! loads the model by name.
//!
//! What the adapter validates of the three yaml strings it passes on (FINDINGS2-0910
//! [5] floor, 2026-10-29): `strata_config` must be an existing file, `strata_python`
//! an existing `.exe` **file** (it is the launched program), and `strata_cwd` an
//! existing directory. Since slice 10c the profile's *contents* are checked too —
//! [`validate_profile`] runs at scan and again at spawn, so a profile that cannot
//! be parsed, references a path the disk lacks, or claims a harness-forced policy
//! field (`host`, `port`, key — FINDINGS-22 [1]) is refused before the child
//! starts, never obeyed. The roots that decide which paths are admissible at
//! all remain the strata `custody_*` cells of the
//! [`crate::engine::policy`] `EnginePolicy` table (§20-9 Fork C), still open in
//! 10a with the root values moving to the config's `engines:` section in a later
//! custody slice.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Fallback interpreter when a yaml omits `strata_python:` (v3.3 default). The
/// single home of the harness-side defaults is the config `engines:` section
/// (slice 10d), resolved through [`crate::config::AppConfig::strata_python`]
/// into the adapter at `default_adapters` boot time.
const DEFAULT_PYTHON: &str = crate::config::STRATA_PYTHON_DEFAULT;
/// Fallback working directory when a yaml omits `strata_cwd:` (v3.3 default).
const DEFAULT_CWD: &str = crate::config::STRATA_CWD_DEFAULT;

/// The top-level profile fields that claim a harness-owned decision —
/// FINDINGS-22 [1] / `DECISIONS` LOCKED "external engines obey only
/// harness-owned configuration": the bind, the port and the key are forced
/// from the harness side and are **never obeyed from engine-side files**
/// (slice 10c enforcement; the 22[2] docs fix recorded the ruling, this is the
/// refusal). Each entry names the mechanism that owns the decision so the
/// refusal can teach, not just deny.
const PROFILE_POLICY_FIELDS: &[(&str, &str)] = &[
    (
        "host",
        "the engine bind is harness-forced (loopback, \u{a7}5)",
    ),
    (
        "port",
        "the engine port is harness-assigned from category config",
    ),
    ("key", "request authentication is harness-owned"),
    ("api_key", "request authentication is harness-owned"),
];

/// The refusal sentence for a profile that claims one or more harness-forced
/// fields (slice 10c): every claim is named at once, with the mechanism that
/// owns it, so the operator's cleanup pass is one edit round, not one refusal
/// per field. Shared by the scan report and the spawn refusal — the same file
/// is refused the same way at both doors, and the engine is never silently
/// launched on a claimed mix (FINDINGS-22 [1]).
pub(crate) fn policy_field_claims(profile: &str, claims: &[(&str, &str)]) -> String {
    let listed = claims
        .iter()
        .map(|(field, owner)| format!("`{field}` \u{2014} {owner}"))
        .collect::<Vec<_>>()
        .join("; ");
    format!(
        "strata profile {profile} declares {listed}; policy fields are harness-forced and \
         never obeyed from engine-side files: delete them from the profile"
    )
}

/// Why a strata profile file is not loadable. The scan maps each leg to its
/// `ScanKind` (and its established sentence), the adapter refuses every leg
/// the same `EngineError::Rejected` way — one check, both doors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileProblem {
    /// The file cannot be read, or is not valid JSON (BOM permitted).
    Unreadable,
    /// The JSON references an absolute path the disk lacks (the doomed spawn
    /// one layer deeper; relative paths and the `log` key are exempt).
    MissingPath {
        /// Profile key carrying the path.
        key: String,
        /// The missing absolute path.
        path: String,
    },
    /// The JSON declares top-level fields the harness forces (22[1]); every
    /// claim the file carries is listed at once, each with its owning mechanism.
    PolicyField {
        /// `(field, owner)` for each claimed field.
        claims: Vec<(&'static str, &'static str)>,
    },
}

impl ProfileProblem {
    /// The operator-facing sentence, with the profile path as the caller
    /// names it (byte-identical to the established scan text for the two
    /// pre-existing legs).
    #[must_use]
    pub(crate) fn describe(&self, profile: &str) -> String {
        match self {
            Self::Unreadable => format!("strata profile not readable: {profile}"),
            Self::MissingPath { key, path } => {
                format!("strata profile references missing path: {key} = {path}")
            }
            Self::PolicyField { claims } => policy_field_claims(profile, claims),
        }
    }
}

/// The strata row's profile check, shared by the scan (which reports it) and
/// the adapter (which refuses the spawn with it — the profile is read by the
/// child minutes after admission evicted any victim, so an unreadable file, a
/// missing referenced path, or a policy-field claim all refuse at spec time,
/// never launch on it). Order: readability, referenced paths, then policy
/// fields — a profile that cannot even be parsed is reported as unreadable,
/// exactly as the scan did before this shared helper existed.
///
/// # Errors
///
/// The first [`ProfileProblem`] the file shows.
pub(crate) fn validate_profile(path: &Path) -> Result<(), ProfileProblem> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Err(ProfileProblem::Unreadable);
    };
    // A UTF-8 BOM is ignored, not refused (same doctrine as
    // `parse_flat_yaml`; RFC 8259 permits it, and the strata engine reads
    // BOM'd profiles today — the live `strata-qwen-gptq-nvfp4.json` carries
    // one).
    let json = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let Ok(root) = serde_json::from_str::<serde_json::Value>(json) else {
        return Err(ProfileProblem::Unreadable);
    };
    if let Some((key, missing)) = missing_profile_path(&root) {
        return Err(ProfileProblem::MissingPath { key, path: missing });
    }
    let claims: Vec<(&'static str, &'static str)> = root.as_object().map_or(Vec::new(), |map| {
        PROFILE_POLICY_FIELDS
            .iter()
            .filter(|(name, _)| map.contains_key(*name))
            .copied()
            .collect()
    });
    if !claims.is_empty() {
        return Err(ProfileProblem::PolicyField { claims });
    }
    Ok(())
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

/// Strata adapter: argv from data, `/health` 200 **and** `loaded != false`.
#[derive(Debug, Clone)]
pub struct StrataAdapter {
    /// Default interpreter (yaml `strata_python:` overrides per model).
    pub python: PathBuf,
    /// Default working directory (yaml `strata_cwd:` overrides per model).
    pub default_cwd: PathBuf,
}

impl Default for StrataAdapter {
    fn default() -> Self {
        StrataAdapter {
            python: PathBuf::from(DEFAULT_PYTHON),
            default_cwd: PathBuf::from(DEFAULT_CWD),
        }
    }
}

impl StrataAdapter {
    /// Creates the adapter with explicit defaults (tests, alternate installs).
    pub fn new(python: PathBuf, default_cwd: PathBuf) -> Self {
        StrataAdapter {
            python,
            default_cwd,
        }
    }

    /// The argv exactly as v3.3 built it: interpreter, then the forced tokens
    /// rendered from the `strata` row of the [`crate::engine::policy`] table
    /// (module, engine tag, profile position, harness-assigned port). Nothing
    /// else reaches the child.
    fn argv(
        &self,
        row: &super::policy::EnginePolicy,
        python: &str,
        config: &str,
        port: u16,
    ) -> Vec<String> {
        let mut argv = vec![python.to_string()];
        argv.extend(super::policy::render_forced(row, port, Some(config)));
        argv
    }
}

impl EngineAdapter for StrataAdapter {
    fn engine_id(&self) -> &str {
        "strata"
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let rejected = |reason: String| EngineError::Rejected {
            engine: "strata".to_string(),
            model: model.name.clone(),
            reason,
        };
        let row = super::policy::engine_policy("strata")
            .expect("the compiled-in table ships a strata row");
        // Required profile JSON, existence-checked before spawn (v3.3 parity:
        // the child would otherwise die minutes into a model load). It must also be
        // a FILE: a directory named `x.json` is the same doomed spawn one step
        // later, and the python child's own error would arrive after admission
        // already evicted a resident engine (FINDINGS2-0910 [5] floor).
        let Some(config) = model
            .raw
            .get("strata_config")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
        else {
            return Err(EngineError::MissingField {
                model: model.name.clone(),
                field: "strata_config".to_string(),
            });
        };
        // Custody admission (slice 10d): the profile is a data-custody input.
        // The strata data cells stay open until the copy-in slice populates them
        // (ruling 5), so admission passes today — the mechanism stands in front
        // of the disk work so the later slice enforces without touching this
        // call site.
        super::policy::admit_custody_path(
            row,
            super::policy::CustodyClass::Data,
            &model.name,
            "strata_config",
            config,
        )?;
        let config_path = Path::new(config);
        if !config_path.is_file() {
            return Err(rejected(if config_path.exists() {
                format!("strata_config is not a file: {config}")
            } else {
                format!("strata_config not found: {config}")
            }));
        }
        // The file is only the floor (10c): the profile's contents are the
        // launch the child will actually obey, so the same check the scan ran
        // is re-run here — a profile edited or swapped after the rescan cannot
        // silently launch on a mix, and the refusal (unreadable, missing path,
        // or claimed policy field — 22[1]) names the reason the scan would
        // have named.
        if let Err(problem) = validate_profile(config_path) {
            return Err(rejected(problem.describe(config)));
        }
        // The interpreter is `argv[0]`, so it is the one yaml string that chooses
        // which program CreateProcessW starts. Refuse garbage early rather than
        // start whatever path happens to exist: an existing FILE whose name ends in
        // `.exe`, matching how the llama adapter refuses a missing binary.
        // Location custody (the row's executed-path roots, §20-9 Fork C) rides on
        // top of this shape floor in the `admit_custody_path` call below: while
        // the cells are open it admits everything (ruling 5), and it will enforce
        // unchanged once a later copy-in slice populates them. The shape rule
        // stays load-bearing — a batch shim renamed to `.exe` is refused here,
        // which is why this is called a floor and not a control.
        let python = model
            .raw
            .get("strata_python")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(
                || self.python.to_string_lossy().into_owned(),
                |v| v.trim().to_string(),
            );
        super::policy::admit_custody_path(
            row,
            super::policy::CustodyClass::Executed,
            &model.name,
            "strata_python",
            &python,
        )?;
        let python_path = Path::new(&python);
        if !python_path.exists() {
            return Err(rejected(format!("strata_python not found: {python}")));
        }
        if !python_path.is_file() {
            return Err(rejected(format!(
                "strata_python is not a file (a directory cannot be the launched program): \
                 {python}"
            )));
        }
        if !python_path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
        {
            return Err(rejected(format!(
                "strata_python must be an existing .exe file: {python}"
            )));
        }
        // The cwd is handed to `Command::current_dir`, and a missing one makes the
        // spawn fail as an opaque `cannot spawn` (FINDINGS2-0910 [5]): refuse it
        // here, with the key the operator has to edit named.
        let cwd = model
            .raw
            .get("strata_cwd")
            .map(String::as_str)
            .filter(|v| !v.trim().is_empty())
            .map_or_else(|| self.default_cwd.clone(), |v| PathBuf::from(v.trim()));
        super::policy::admit_custody_path(
            row,
            super::policy::CustodyClass::Executed,
            &model.name,
            "strata_cwd",
            &cwd.to_string_lossy(),
        )?;
        if !cwd.is_dir() {
            return Err(rejected(format!(
                "strata_cwd is not an existing directory: {}",
                cwd.display()
            )));
        }
        Ok(LaunchSpec {
            argv: self.argv(row, &python, config, port),
            cwd,
            env: BTreeMap::new(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
            ),
            container: None,
            script: None,
            // The strata router outlives nothing but its own job object unless the
            // harness records it: every strata launch rides the custody sidecar
            // (chunk 10 slice 10b) so a crashed run's orphan is provably ours to
            // reap at the next boot, purely from the harness's own record.
            custody: Some("strata".to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(pairs: &[(&str, &str)]) -> ModelEntry {
        ModelEntry {
            name: "strata-test".to_string(),
            engine: "strata".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(55.0),
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            fit_gb: None,
            model_ref: Some("strata://test".to_string()),
            serve_model: Some("qwen3.8-flash-next".to_string()),
            file: None,
            raw: raw(pairs),
        }
    }

    /// A real file on disk stands in for the profile JSON and the interpreter:
    /// the adapter existence-checks both before handing a spec to the runtime.
    fn real_file(tag: &str) -> PathBuf {
        real_file_with_extension(tag, "json")
    }

    /// As [`real_file`], with the extension under test: the interpreter rule is an
    /// extension rule, so a "file that exists" fixture must be able to be a `.cmd`.
    fn real_file_with_extension(tag: &str, extension: &str) -> PathBuf {
        let path = crate::testutil::temp_dir("strata").join(format!("{tag}.{extension}"));
        std::fs::write(&path, b"{}").expect("temp file");
        path
    }

    #[test]
    fn argv_matches_the_v33_contract() {
        let config = real_file("argv");
        let spec = StrataAdapter::default()
            .spec(
                &entry(&[("strata_config", config.to_str().expect("utf-8 path"))]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv,
            vec![
                DEFAULT_PYTHON,
                "-m",
                "serve.server",
                "--engine",
                "strata",
                "--config",
                config.to_str().expect("utf-8 path"),
                "--port",
                "18109",
            ]
        );
        assert_eq!(spec.cwd, PathBuf::from(DEFAULT_CWD));
        assert_eq!(spec.log_name, "strata-test");
        assert_eq!(
            spec.custody.as_deref(),
            Some("strata"),
            "every strata launch rides the custody sidecar (slice 10b)"
        );
        assert!(spec.env.is_empty());
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::HttpFlag {
                    path: "/health".to_string(),
                    expect_status: 200,
                    field: "loaded".to_string(),
                }),
            )
        );
        let _ = std::fs::remove_file(config);
    }

    #[test]
    fn yaml_overrides_interpreter_and_cwd() {
        let config = real_file("override");
        let python = std::env::current_exe().expect("a real interpreter stand-in");
        let cwd = std::env::temp_dir();
        let spec = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", python.to_str().expect("utf-8 path")),
                    ("strata_cwd", cwd.to_str().expect("utf-8 path")),
                ]),
                18110,
            )
            .expect("valid spec");
        assert_eq!(spec.argv[0], python.to_str().expect("utf-8 path"));
        assert_eq!(spec.argv[8], "18110");
        assert_eq!(spec.cwd, cwd);
        let _ = std::fs::remove_file(config);
    }

    /// Pin of the `cpu_offload` engine scoping: the strata adapter reads exactly
    /// `strata_config`/`strata_python`/`strata_cwd` — the key is inert for this
    /// engine, so absent, `true` and `false` must produce a byte-identical
    /// [`LaunchSpec`] (argv, env, readiness, cwd, container alike). Falsifier: if
    /// this adapter starts reading the key, the equality fails here.
    #[test]
    fn cpu_offload_changes_nothing_for_a_strata_launch() {
        let config = real_file("inert-offload");
        let path = config.to_str().expect("utf-8 path");
        let base = StrataAdapter::default()
            .spec(&entry(&[("strata_config", path)]), 18109)
            .expect("absent key");
        for value in ["true", "false"] {
            let spec = StrataAdapter::default()
                .spec(
                    &entry(&[("strata_config", path), ("cpu_offload", value)]),
                    18109,
                )
                .expect("an inert key never refuses a launch");
            assert_eq!(
                spec, base,
                "cpu_offload: {value} must change nothing — if the strata adapter \
                 starts reading the key, this fails"
            );
        }
        let _ = std::fs::remove_file(config);
    }

    #[test]
    fn missing_profile_is_a_missing_field_refusal() {
        let err = StrataAdapter::default()
            .spec(&entry(&[]), 18109)
            .expect_err("must refuse");
        assert!(matches!(
            err,
            EngineError::MissingField { ref field, .. } if field == "strata_config"
        ));
    }

    #[test]
    fn absent_profile_or_interpreter_is_rejected_before_spawn() {
        let err = StrataAdapter::default()
            .spec(
                &entry(&[("strata_config", "Z:\\nope\\missing.json")]),
                18109,
            )
            .expect_err("must refuse");
        assert!(matches!(&err, EngineError::Rejected { reason, .. }
            if reason.starts_with("strata_config not found:")));

        let config = real_file("badpy");
        let err = StrataAdapter::new(
            PathBuf::from("Z:\\nope\\python.exe"),
            PathBuf::from(DEFAULT_CWD),
        )
        .spec(
            &entry(&[("strata_config", config.to_str().expect("utf-8 path"))]),
            18109,
        )
        .expect_err("must refuse");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
            if reason.starts_with("strata_python not found:")),
            "{err}"
        );
        let _ = std::fs::remove_file(config);
    }

    /// FINDINGS2-0910 [5]'s probe, turned into a pin: `argv[0]` is the program
    /// CreateProcessW starts, and an existing batch file is not an interpreter. The
    /// reviewer's `python.cmd` must be refused naming the yaml key.
    #[test]
    fn a_batch_file_is_refused_as_the_interpreter() {
        let config = real_file("cmd-python-config");
        let shim = real_file_with_extension("cmd-python", "cmd");
        let err = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", shim.to_str().expect("utf-8 path")),
                ]),
                18109,
            )
            .expect_err("a .cmd cannot be the launched program");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("strata_python") && reason.contains(".exe")),
            "{err}"
        );
        let _ = std::fs::remove_file(config);
        let _ = std::fs::remove_file(shim);
    }

    /// A directory that exists is not a program either: `Path::exists()` alone would
    /// have accepted it as `strata_python`.
    #[test]
    fn a_directory_is_refused_as_the_interpreter() {
        let config = real_file("dir-python-config");
        let dir = crate::testutil::temp_dir("strata-python-dir");
        std::fs::create_dir_all(&dir).expect("temp dir as interpreter");
        let err = StrataAdapter::default()
            .spec(
                &entry(&[
                    ("strata_config", config.to_str().expect("utf-8 path")),
                    ("strata_python", dir.to_str().expect("utf-8 path")),
                ]),
                18109,
            )
            .expect_err("a directory cannot be the launched program");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("strata_python") && reason.contains("not a file")),
            "{err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(config);
    }

    /// The cwd reaches `Command::current_dir`: today a missing one surfaces only as
    /// `cannot spawn` from the runtime (10[6]'s opaque failure, [5]'s companion).
    #[test]
    fn a_missing_or_non_directory_cwd_is_refused_at_spec_time() {
        let config = real_file("cwd-config");
        let path = config.to_str().expect("utf-8 path");
        let as_file = real_file_with_extension("cwd-not-a-dir", "json");
        for (value, expect) in [
            ("Z:\\no-such-dir", "not an existing directory"),
            (file_str(&as_file), "not an existing directory"),
        ] {
            let err = StrataAdapter::default()
                .spec(
                    &entry(&[("strata_config", path), ("strata_cwd", value)]),
                    18109,
                )
                .expect_err("a cwd the child cannot enter must not be launched");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("strata_cwd") && reason.contains(expect)),
                "{value}: {err}"
            );
        }
        let _ = std::fs::remove_file(config);
        let _ = std::fs::remove_file(as_file);
    }

    fn file_str(path: &std::path::Path) -> &str {
        path.to_str().expect("utf-8 path")
    }

    /// The floor refuses garbage, it does not narrow legitimate config: the real
    /// interpreter + profile + cwd trio still produces the exact v3.3 spec.
    #[test]
    fn a_valid_interpreter_profile_and_cwd_still_launch() {
        let config = real_file("valid-trio");
        let python = std::env::current_exe().expect("a real .exe stand-in");
        let spec = StrataAdapter::new(python.clone(), std::env::temp_dir())
            .spec(&entry(&[("strata_config", file_str(&config))]), 18109)
            .expect("a real trio must launch");
        assert_eq!(spec.argv[0], python.to_str().expect("utf-8 path"));
        assert_eq!(spec.cwd, std::env::temp_dir());
        let _ = std::fs::remove_file(config);
    }

    /// A profile written after the scan is re-validated at spawn (slice 10c):
    /// claimed policy fields (22[1]), a broken reference, and unparseable JSON
    /// all refuse the launch with the same sentence the scan would have
    /// reported — the engine is never silently launched on a mix.
    #[test]
    fn a_profile_the_scan_would_refuse_is_refused_at_spawn_too() {
        let dir = crate::testutil::temp_dir("strata-validate");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let python = std::env::current_exe().expect("a real .exe stand-in");
        let adapter = StrataAdapter::new(python, std::env::temp_dir());
        let cases = [
            (
                "claims-port",
                r#"{"port": 8080}"#,
                "`port`",
                "never obeyed from engine-side files",
            ),
            (
                "claims-host",
                r#"{"host": "0.0.0.0"}"#,
                "`host`",
                "harness-forced",
            ),
            ("broken-json", "not json at all", "not readable", ""),
            (
                "missing-ref",
                r#"{"model_file": "Z:\\nope\\weights.gguf"}"#,
                "references missing path",
                "Z:\\nope\\weights.gguf",
            ),
        ];
        for (name, body, must_name, must_say) in cases {
            let profile = dir.join(format!("{name}.json"));
            std::fs::write(&profile, body).expect("write profile");
            let err = adapter
                .spec(&entry(&[("strata_config", file_str(&profile))]), 18109)
                .expect_err("a refused profile must never launch");
            let EngineError::Rejected { reason, .. } = &err else {
                panic!("{name}: expected Rejected, got {err:?}");
            };
            assert!(
                reason.contains(must_name) && (must_say.is_empty() || reason.contains(must_say)),
                "{name}: {reason}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The check that refuses a claimed field accepts a profile that decides
    /// nothing the harness owns: nested objects and harness-blind names are
    /// config, not claims.
    #[test]
    fn a_profile_deciding_nothing_harness_owned_launches() {
        let dir = crate::testutil::temp_dir("strata-clean");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let profile = dir.join("clean.json");
        std::fs::write(
            &profile,
            r#"{"vision": {"port_hint": 1}, "nested": {"key_note": "x"}, "log": "Z:\\logs"}"#,
        )
        .expect("write profile");
        StrataAdapter::default()
            .spec(&entry(&[("strata_config", file_str(&profile))]), 18109)
            .expect("nested names are not top-level claims; `log` is exempt");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
