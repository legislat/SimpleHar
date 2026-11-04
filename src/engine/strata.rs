//! The strata engine adapter (DESIGN Step 7; strata template mode, operator
//! ruling 2026-11-02 row 19).
//!
//! Strata is a Python server (`serve.server`) driven entirely by a profile JSON.
//! Template mode means the ROW YAML owns the profile's fields and the harness
//! renders the full JSON at every load: the child receives only the
//! existence-checked interpreter plus the forced tokens the `strata` row of the
//! [`crate::engine::policy`] table spells (module, engine tag, loopback bind,
//! harness-rendered profile position, harness-assigned port). Readiness needs
//! the JSON `loaded` flag because strata's `/health` answers 200 while the
//! weights are still streaming onto the card (v3.3 `engine.rs:208-226` parity).
//!
//! The rendered profile lives in the harness's own root beside the custody
//! sidecar (content-addressed name, atomic write, keep-N prune — the runtime
//! stages it and rewrites `--config` to exactly that path, chunk 10 custody
//! doctrine: engines obey only harness-owned locations). `host`, `port` and key
//! fields are absent by construction — the bind is the forced loopback
//! `--host` argv and the port the forced `--port` (server.py resolves both
//! flags ahead of any profile key) — so ruling 22[1] holds by rendering, not
//! by policing. The external `strata_config` mode is SUPERSEDED: a row that
//! still declares it is refused with the migration sentence, and
//! `simplehar-se import strata <profile.json>` turns a manual profile into the
//! row yaml. `C:\Strata` stays the manual-launch home the serve path never
//! reads.
//!
//! There is no preload in this harness (engine work is on-demand only), and the
//! yamls carry no autostart keys: a strata engine spawns only when an operator
//! loads the model by name.
//!
//! What the adapter validates (FINDINGS2-0910 [5] floor, 2026-10-29):
//! `strata_python` must be an existing `.exe` **file** (it is the launched
//! program) and `strata_cwd` an existing directory. The rendered profile's
//! contents are checked too — [`validate_profile_text`] runs at scan and again
//! at spec, so a value that references a path the disk lacks is refused before
//! the child starts, never launched on.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec, RenderedProfile};
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

/// Why a rendered strata profile is not loadable. The scan maps each leg to its
/// `ScanKind` (and its established sentence), the adapter refuses every leg
/// the same `EngineError::Rejected` way — one check, both doors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProfileProblem {
    /// The JSON references an absolute path the disk lacks (the doomed spawn
    /// one layer deeper; relative paths and the `log` key are exempt).
    MissingPath {
        /// Profile key carrying the path.
        key: String,
        /// The missing absolute path.
        path: String,
    },
    /// The JSON declares top-level fields the harness forces (22[1]). The
    /// renderer structurally cannot emit one, so reaching this leg means a
    /// renderer bug — it refuses anyway, because a profile that claims a
    /// harness-forced field must never launch regardless of who wrote it.
    PolicyField {
        /// `(field, owner)` for each claimed field.
        claims: Vec<(&'static str, &'static str)>,
    },
}

impl ProfileProblem {
    /// The operator-facing sentence, with the profile as the caller names it
    /// (the rendered copy is named `rendered:<model>`, never a path — pointing
    /// at an operator file would suggest one is a launch input).
    #[must_use]
    pub(crate) fn describe(&self, profile: &str) -> String {
        match self {
            Self::MissingPath { key, path } => {
                format!("strata profile references missing path: {key} = {path}")
            }
            Self::PolicyField { claims } => policy_field_claims(profile, claims),
        }
    }
}

/// The strata profile check, run over the RENDERED JSON text: the scan runs it
/// on every template row (a doomed spawn refuses at rescan, not at load, after
/// admission may already have evicted a resident), and the adapter re-runs it
/// at spec time — a model file deleted between scan and load meets the same
/// typed refusal. The bytes arrive from [`render_profile`], so an unparseable
/// shape is structurally impossible; the legs that stay load-bearing are the
/// referenced paths and (defense in depth) the harness-forced-field claim.
///
/// # Errors
///
/// The first [`ProfileProblem`] the JSON shows.
pub(crate) fn validate_profile_text(json: &str) -> Result<(), ProfileProblem> {
    let root = serde_json::from_str::<serde_json::Value>(json)
        .expect("the renderer's own JSON parses; a failure here is a renderer bug");
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

/// The migration sentence for a row that still declares `strata_config`
/// (operator ruling 2026-11-02 row 19: no compat path — the external profile
/// mode is superseded at once, and the import command is the bridge). One
/// sentence, two doors: the scan refuses the row with it and the adapter
/// refuses a hand-built entry the same way.
pub(crate) fn superseded_sentence(path: &str) -> String {
    format!(
        "`strata_config` ({path}) is superseded by the rendered-profile template: run \
         `simplehar-se import strata \"{path}\"`, merge its output into this row, and delete \
         the `strata_config` line"
    )
}

/// The sentence a row naming BOTH profile sources meets, at either door (the
/// scan for a row that already carries `strata_exe` and so only needs its old
/// `strata_config` line deleted — review 25 [7]; the adapter as
/// defense-in-depth): one launch, exactly one profile source.
pub(crate) fn mutual_exclusion_sentence(config_path: &str) -> String {
    format!(
        "`strata_exe` (harness-rendered profile) and `strata_config` (operator file) are \
         mutually exclusive: a launch has exactly one profile source, and the row still \
         names the file: {config_path}"
    )
}

/// The template-mode selector: a strata row renders its profile when the yaml
/// carries `strata_exe` — the engine binary the rendered profile will name.
pub(crate) fn template_mode(model: &ModelEntry) -> bool {
    knob(model, "strata_exe").is_some()
}

/// One optional yaml value, trimmed; an empty or whitespace-only value reads
/// as absent, so clearing a key clears the field.
fn knob<'a>(model: &'a ModelEntry, key: &str) -> Option<&'a str> {
    model
        .raw
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
}

/// The `strata_args` tokenization: comma-separated engine argv tokens, each
/// trimmed. The one well-formedness rule, shared by both consumers: an empty
/// entry (bare or whitespace-only between commas) makes the line malformed —
/// the render refuses it (one token per entry, FINDINGS-10 [3]) and
/// [`effective_context`] answers `None` for the same line, so a row that
/// never loads can never display a context (FINDINGS-17).
fn well_formed_tokens(line: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    for entry in line.split(',') {
        let token = entry.trim();
        if token.is_empty() {
            return None;
        }
        tokens.push(token.to_string());
    }
    Some(tokens)
}

/// The context a strata row actually serves: the figure riding the engine's
/// own argv in the comma-separated `strata_args` list — the token after
/// `--max-context` (`--pack, --max-context, 262144`) or the suffix of
/// `--max-context=262144`. Either spelling must carry an all-ASCII-digits
/// figure: a `--max-context` followed by a flag or other non-figure, or an
/// eq suffix that is not a figure, answers `None` — Ctx shows only what the
/// engine will actually serve, never a flag and never a guess. A line with
/// an empty comma entry never loads (the render refuses it,
/// [`well_formed_tokens`]), so the display answers `None` for it too. The
/// row `context:` key is the llama knob the strata engine never reads (the
/// scan reports it as `InertKey`), so it is never a fallback: no token, no
/// answer.
///
/// # Arguments
///
/// * `raw` - the row's raw yaml keys (its `strata_args` line, if any).
///
/// # Returns
///
/// The spelled max-context figure, `None` when the argv names none the
/// engine would serve.
#[must_use]
pub fn effective_context(raw: &BTreeMap<String, String>) -> Option<String> {
    let line = raw
        .get("strata_args")
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())?;
    let tokens = well_formed_tokens(line)?;
    let mut tokens = tokens.iter();
    while let Some(token) = tokens.next() {
        if let Some(value) = token.strip_prefix("--max-context=") {
            return context_figure(value).map(str::to_string);
        }
        if token == "--max-context" {
            return context_figure(tokens.next()?).map(str::to_string);
        }
    }
    None
}

/// The all-ASCII-digits shape of a context figure: what the engine would
/// actually serve. Anything else — a flag, `auto`, the empty suffix of a
/// bare `--max-context=` — is not a figure.
fn context_figure(text: &str) -> Option<&str> {
    (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())).then_some(text)
}

/// A required single-value key that must carry an absolute-path shape
/// (drive-letter or UNC): the rendered profile is walked for existence at both
/// doors, but shape refuses here so the error names the yaml key a hand-shape
/// check at the walk could only report as a path.
fn required_path(
    model: &ModelEntry,
    key: &str,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<String, EngineError> {
    let Some(value) = knob(model, key) else {
        return Err(EngineError::MissingField {
            model: model.name.clone(),
            field: key.to_string(),
        });
    };
    if profile_path_candidate(value).is_none() {
        return Err(rejected(format!(
            "`{key}` must be an absolute path (drive-letter or UNC): {value}"
        )));
    }
    Ok(value.to_string())
}

/// A comma-separated path list (`strata_lib_dirs`): the ninfer
/// `ninfer_model_roots` idiom — the comma is the only separator, so paths may
/// hold spaces but never commas (measured: no inference path on this box does).
fn path_list(
    model: &ModelEntry,
    key: &str,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<Vec<String>, EngineError> {
    let Some(line) = knob(model, key) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in line.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(rejected(format!(
                "`{key}` entries must be non-empty, comma-separated absolute paths: {line}"
            )));
        }
        if profile_path_candidate(entry).is_none() {
            return Err(rejected(format!(
                "`{key}` entries must be absolute paths (drive-letter or UNC): {entry}"
            )));
        }
        out.push(entry.to_string());
    }
    Ok(out)
}

/// An optional unsigned integer key (`strata_gpu`, `strata_parallel`).
fn unsigned(
    model: &ModelEntry,
    key: &str,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<Option<u64>, EngineError> {
    let Some(value) = knob(model, key) else {
        return Ok(None);
    };
    value
        .parse::<u64>()
        .map(Some)
        .map_err(|_| rejected(format!("`{key}` must be a non-negative integer: {value}")))
}

/// The `strata_env` surface: whitespace-separated `NAME=VALUE` tokens where the
/// name is a `STRATA_`-prefixed engine setting (`STRATA_KVA=0`) — the shape
/// ninfer enforces by enumerating its vars, enforced here by the engine's own
/// namespace: no foreign `LD_PRELOAD`-style name can enter the child's profile.
fn env_map(
    model: &ModelEntry,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<serde_json::Map<String, serde_json::Value>, EngineError> {
    let mut map = serde_json::Map::new();
    let Some(line) = knob(model, "strata_env") else {
        return Ok(map);
    };
    for token in line.split_whitespace() {
        let bad = |why: &str| {
            Err(rejected(format!(
                "`strata_env` tokens must be STRATA_<NAME>=<VALUE> ({why}): {token}"
            )))
        };
        let Some((name, value)) = token.split_once('=') else {
            return bad("one `=`");
        };
        let name_ok = name.strip_prefix("STRATA_").is_some_and(|rest| {
            !rest.is_empty()
                && rest
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
        });
        if !name_ok {
            return bad("uppercase `STRATA_`-prefixed name");
        }
        let value_ok = !value.is_empty()
            && value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
        if !value_ok {
            return bad("plain value (letters, digits, `._+-`)");
        }
        map.insert(
            name.to_string(),
            serde_json::Value::String(value.to_string()),
        );
    }
    Ok(map)
}

/// The `strata_vision` block: whitespace-separated `k=v` tokens over the fixed
/// key set — `exe`, `mmproj` and `model` are absolute paths (required together,
/// the vision server cannot start without them), `gpu` is truthy (default on,
/// as every manual profile has it), `max_tokens`, `min_tokens` and `threads`
/// unsigned counts (operator ruling 2026-11-02: the set mirrors the vision
/// spawn's full cfg contract in `serve.server` — server.py:1936-1946 — so no
/// hand-edited engine key can ride the knob unreported). Rendering
/// the block also appends the engine's `--vision` switch to `args` (server.py
/// derives the switch from the block's presence); the canonical row therefore
/// spells the vision ONCE, in the block.
fn vision_block(
    model: &ModelEntry,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, EngineError> {
    let Some(line) = knob(model, "strata_vision") else {
        return Ok(None);
    };
    let mut taken = serde_json::Map::new();
    let mut gpu = true;
    let mut max_tokens: Option<u64> = None;
    let mut min_tokens: Option<u64> = None;
    let mut threads: Option<u64> = None;
    for token in line.split_whitespace() {
        let Some((key, value)) = token.split_once('=') else {
            return Err(rejected(format!(
                "`strata_vision` tokens must be k=v over exe, mmproj, model, gpu, \
                 max_tokens, min_tokens, threads: {token}"
            )));
        };
        match key {
            "exe" | "mmproj" | "model" => {
                if profile_path_candidate(value).is_none() {
                    return Err(rejected(format!(
                        "`strata_vision` {key} must be an absolute path (drive-letter or UNC): {value}"
                    )));
                }
                taken.insert(
                    key.to_string(),
                    serde_json::Value::String(value.to_string()),
                );
            }
            "gpu" => gpu = crate::model::truthy(value),
            "max_tokens" => {
                max_tokens = Some(value.parse::<u64>().map_err(|_| {
                    rejected(format!(
                        "`strata_vision` max_tokens must be a number: {value}"
                    ))
                })?);
            }
            "min_tokens" => {
                min_tokens = Some(value.parse::<u64>().map_err(|_| {
                    rejected(format!(
                        "`strata_vision` min_tokens must be a number: {value}"
                    ))
                })?);
            }
            "threads" => {
                threads = Some(value.parse::<u64>().map_err(|_| {
                    rejected(format!("`strata_vision` threads must be a number: {value}"))
                })?);
            }
            other => {
                return Err(rejected(format!(
                    "`strata_vision` key `{other}` is not one of exe, mmproj, model, gpu, \
                 max_tokens, min_tokens, threads"
                )))
            }
        }
    }
    for required in ["exe", "mmproj", "model"] {
        if !taken.contains_key(required) {
            return Err(rejected(format!(
                "`strata_vision` requires `{required}` when a vision block is declared"
            )));
        }
    }
    taken.insert("gpu".to_string(), serde_json::Value::Bool(gpu));
    if let Some(tokens) = max_tokens {
        taken.insert("max_tokens".to_string(), serde_json::json!(tokens));
    }
    if let Some(tokens) = min_tokens {
        taken.insert("min_tokens".to_string(), serde_json::json!(tokens));
    }
    if let Some(n) = threads {
        taken.insert("threads".to_string(), serde_json::json!(n));
    }
    Ok(Some(taken))
}

/// The `strata_sampling` block: whitespace-separated `key=value` sampling
/// defaults (server.py's `sampling` profile dict) — every value a number or a
/// bool literal, the two shapes the server validates there. The block is
/// rendered as a JSON object; an integer-looking value keeps its integer form
/// so an import→render round-trip reproduces the profile bytes' types.
fn sampling_map(
    model: &ModelEntry,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<Option<serde_json::Map<String, serde_json::Value>>, EngineError> {
    let Some(line) = knob(model, "strata_sampling") else {
        return Ok(None);
    };
    let mut map = serde_json::Map::new();
    for token in line.split_whitespace() {
        let bad = || {
            Err(rejected(format!(
                "`strata_sampling` tokens must be key=value with a number or bool: {token}"
            )))
        };
        let Some((key, value)) = token.split_once('=') else {
            return bad();
        };
        if key.is_empty() {
            return bad();
        }
        let json = match value {
            "true" => serde_json::Value::Bool(true),
            "false" => serde_json::Value::Bool(false),
            other => match other.parse::<u64>() {
                Ok(n) => serde_json::json!(n),
                Err(_) => other
                    .parse::<f64>()
                    .map(serde_json::Value::from)
                    .map_err(|_| {
                        rejected(format!(
                            "`strata_sampling` value must be a number or bool: {token}"
                        ))
                    })?,
            },
        };
        map.insert(key.to_string(), json);
    }
    Ok(Some(map))
}

/// The `strata_uncensored` switch: this strata fork's honest name for the
/// speed-projection default (server.py maps the profile `uncensored` key); a
/// present knob must read as a strict bool word, and its value — `true` OR
/// `false` — is rendered, because the server treats an explicit `false` as
/// written intent, distinct from an absent key.
fn uncensored_switch(
    model: &ModelEntry,
    rejected: &dyn Fn(String) -> EngineError,
) -> Result<Option<bool>, EngineError> {
    let Some(value) = knob(model, "strata_uncensored") else {
        return Ok(None);
    };
    match value.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(Some(true)),
        "false" | "0" => Ok(Some(false)),
        _ => Err(rejected(format!(
            "`strata_uncensored` must be `true` or `false`: {value}"
        ))),
    }
}

/// Renders the complete strata profile JSON from the row's yaml (operator
/// ruling 2026-11-02 row 19 — the L3 render of the ninfer idiom: the harness
/// writes the artifact, so the launch is exactly what this function checked).
/// `host`, `port` and key fields are never emitted: the bind and the port ride
/// the forced argv (the `--host` loopback literal and the slot `--port`), and
/// server.py resolves both flags ahead of any profile key. `cwd` arrives from
/// the adapter's already-existence-checked resolution; the engine `log` lands
/// in the harness's own profile root. Every refusal names the yaml key the
/// operator has to fix (G6).
///
/// # Errors
///
/// [`EngineError::MissingField`] for a required key, [`EngineError::Rejected`]
/// for a shape the key cannot carry.
pub(crate) fn render_profile(model: &ModelEntry, cwd: &Path) -> Result<String, EngineError> {
    let rejected = &|reason: String| EngineError::Rejected {
        engine: "strata".to_string(),
        model: model.name.clone(),
        reason,
    };
    let exe = required_path(model, "strata_exe", rejected)?;
    let tokenizer = required_path(model, "strata_tokenizer", rejected)?;
    let Some(model_name) = model
        .serve_model
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    else {
        return Err(EngineError::MissingField {
            model: model.name.clone(),
            field: "serve_model".to_string(),
        });
    };
    let mut args: Vec<String> = match knob(model, "strata_args") {
        Some(line) => well_formed_tokens(line).ok_or_else(|| {
            rejected(format!(
                "`strata_args` must be comma-separated non-empty engine tokens: {line}"
            ))
        })?,
        None => Vec::new(),
    };
    if let Some(banned) = args.iter().find(|token| {
        // The ban reads the option token's *name*, not the whole token: the eq
        // form is spawn truth too (server.py splits `--flag=value`, ruling 10[1]),
        // so `--host=0.0.0.0` must not smuggle past a whole-token comparison
        // (R2-10-02, the ninfer serve-args denylist is the in-repo precedent).
        let name = token
            .split_once('=')
            .map_or(token.as_str(), |(name, _)| name);
        name == "--host" || name == "--port"
    }) {
        return Err(rejected(format!(
            "`{banned}` in `strata_args` is harness-forced argv and must not be spelled by the row"
        )));
    }
    let vision = vision_block(model, rejected)?;
    if vision.is_some() {
        if args.iter().any(|token| token == "--vision") {
            return Err(rejected(
                "the `strata_vision` block already switches vision on; drop the `--vision` \
                 token from `strata_args`"
                    .to_string(),
            ));
        }
        args.push("--vision".to_string());
    }
    let mut profile = serde_json::Map::new();
    profile.insert("exe".to_string(), serde_json::Value::String(exe));
    profile.insert(
        "args".to_string(),
        serde_json::Value::Array(
            args.into_iter()
                .map(serde_json::Value::String)
                .collect::<Vec<_>>(),
        ),
    );
    profile.insert(
        "cwd".to_string(),
        serde_json::Value::String(cwd.to_string_lossy().into_owned()),
    );
    profile.insert(
        "tokenizer".to_string(),
        serde_json::Value::String(tokenizer),
    );
    profile.insert(
        "model_name".to_string(),
        serde_json::Value::String(model_name.to_string()),
    );
    profile.insert(
        "log".to_string(),
        serde_json::Value::String(
            crate::process::strata_profiles_dir()
                .join(format!(
                    "{}.log",
                    crate::process::sanitize_slug(&model.name)
                ))
                .to_string_lossy()
                .into_owned(),
        ),
    );
    let dirs = path_list(model, "strata_lib_dirs", rejected)?;
    if !dirs.is_empty() {
        profile.insert(
            "lib_dirs".to_string(),
            serde_json::Value::Array(
                dirs.into_iter()
                    .map(serde_json::Value::String)
                    .collect::<Vec<_>>(),
            ),
        );
    }
    let env = env_map(model, rejected)?;
    if !env.is_empty() {
        profile.insert("env".to_string(), serde_json::Value::Object(env));
    }
    if let Some(gpu) = unsigned(model, "strata_gpu", rejected)? {
        profile.insert("gpu".to_string(), serde_json::json!(gpu));
    }
    if let Some(parallel) = unsigned(model, "strata_parallel", rejected)? {
        profile.insert("parallel".to_string(), serde_json::json!(parallel));
    }
    if knob(model, "strata_gpus_asked").is_some_and(crate::model::truthy) {
        profile.insert("gpus_asked".to_string(), serde_json::Value::Bool(true));
    }
    if let Some(vision) = vision {
        profile.insert("vision".to_string(), serde_json::Value::Object(vision));
    }
    if let Some(sampling) = sampling_map(model, rejected)? {
        profile.insert("sampling".to_string(), serde_json::Value::Object(sampling));
    }
    if let Some(on) = uncensored_switch(model, rejected)? {
        profile.insert("uncensored".to_string(), serde_json::Value::Bool(on));
    }
    // The content address is the version: one deterministic render per yaml,
    // so the pretty form only has to stay stable across harness builds, and
    // serde_json's BTreeMap order already guarantees that.
    serde_json::to_string_pretty(&serde_json::Value::Object(profile))
        .map_err(|e| rejected(format!("the rendered profile would not serialize: {e}")))
}

/// The staged file name a rendered profile carries: the sanitized model slug
/// plus the same content address the runtime verifies the bytes against before
/// it rewrites `--config`. The naming rule is engine-blind and lives with the
/// staging door it must agree with — [`crate::process::staged_profile_name`]
/// is the single home; this is the strata-side spelling the strata pins use.
pub(crate) fn staged_profile_name(slug: &str, text: &str) -> String {
    crate::process::staged_profile_name(slug, text)
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
        // One profile source, gated before anything else (operator ruling
        // 2026-11-02 row 19): `strata_exe` selects the harness render, and the
        // external `strata_config` mode is SUPERSEDED — a row still declaring
        // it meets the migration sentence, not a launch. The argv placeholder
        // below is a harness-minted file name, and the runtime rewrites it to
        // exactly the staged bytes, so no operator path ever reaches the child.
        let legacy = knob(model, "strata_config");
        if template_mode(model) {
            if let Some(path) = legacy {
                return Err(rejected(mutual_exclusion_sentence(path)));
            }
        } else if let Some(path) = legacy {
            return Err(rejected(superseded_sentence(path)));
        } else {
            return Err(EngineError::MissingField {
                model: model.name.clone(),
                field: "strata_exe".to_string(),
            });
        }
        // The interpreter is `argv[0]`, so it is the one yaml string that chooses
        // which program CreateProcessW starts. Refuse garbage early rather than
        // start whatever path happens to exist: an existing FILE whose name ends in
        // `.exe`, matching how the llama adapter refuses a missing binary.
        // Location custody (the row's executed-path roots, В§20-9 Fork C) rides on
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
        // The render is the launch (row 19): every shape the profile must keep
        // is refused naming its yaml key, and the referenced paths get the same
        // walk the scan ran — a model file deleted between scan and load meets
        // the typed refusal here, minutes before the child would die on it.
        let profile_text = render_profile(model, &cwd)?;
        if let Err(problem) = validate_profile_text(&profile_text) {
            return Err(rejected(
                problem.describe(&format!("rendered:{}", model.name)),
            ));
        }
        let staged_name = staged_profile_name(&model.name, &profile_text);
        Ok(LaunchSpec {
            argv: self.argv(row, &python, &staged_name, port),
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
            rendered_profile: Some(RenderedProfile {
                engine: "strata",
                text: profile_text,
            }),
            // The strata router outlives nothing but its own job object unless the
            // harness records it: every strata launch rides the custody sidecar
            // (chunk 10 slice 10b) so a crashed run's orphan is provably ours to
            // reap at the next boot, purely from the harness's own record.
            custody: Some("strata".to_string()),
        })
    }
}

/// Reads the Strata `GET /status` snapshot into a measured footprint
/// (`TASK-ENGINE-MEASUREMENT` phase 3, ORCHESTRATION §3). The sampler calls
/// this once at readiness. Every refusal answers `None` — absence, never a
/// guess — and the sampler then takes the witness-based process-tree sum over
/// the `strata.exe` engine and its `strata-vision` child (doctrine 4).
///
/// The figure: `info.vram` — the path the task and ORCHESTRATION record for
/// this snapshot — carries the three figures the 2026-11-04 evidence line
/// records for the sibling `serve.server` engine (`expert_cache_mib`,
/// `vram_free_mib`, `reserve_mib`; ORCHESTRATION §Endpoints reference, the
/// same triple `basalt.rs` parses from the same code base). The harness books
/// `expert_cache_mib + reserve_mib`: the bytes the engine took, plus the
/// headroom its fill policy keeps and reclaims on demand — the live row's
/// argv spells `--vram-reserve-mib 2048`. `vram_free_mib` is not in the sum
/// (free bytes belong to nobody), but a snapshot missing any of the three is
/// not a vram snapshot and the parse refuses it.
///
/// Grounding honesty: the repo holds **no captured** strata `/status` body.
/// The pointer is the task's recorded path; the member names and MiB units
/// are recorded only for the sibling engine's status snapshot, not for this
/// one. The derivation stays on recorded figures: the operator's measured row
/// is `vram_gb: 67.8` (`ModelConf/strata-fp4local.yaml`), the launch reserves
/// 2,048 MiB, and the footprint observed while loaded was 70.4 GiB
/// (`handoff/evidence/2026-11-04-basalt-v2-readmission/RESULTS.md`) — the
/// realistic test fixture lands on the row figure inside that observed band.
/// The card run (task acceptance, operator-freed window) must capture the
/// real body; if its field set differs, fix this parse from that capture,
/// never by widening a guess.
///
/// `--vram-elastic` gates nothing here, and a row without it needs no special
/// parse: this function sees only the response body, and the reported figure
/// states the bytes the engine holds whatever the launch argv spells. The
/// flag decides how the footprint **moves** after this reading (phase 4's
/// hysteresis), not whether the figure means "footprint"; without it the
/// status either still carries the triple or the process-tree fallback takes
/// the launch.
///
/// Refusal is always `None`: an error envelope (a top-level `error`), a body
/// that is not the status shape, an empty or absent `vram` object, or any
/// member of the three that is missing, non-numeric, negative, or
/// non-finite. `EngineMeasurement::ram_gib` stays `None`: no repo record
/// shows a host-RAM figure in this snapshot (the live evidence carries no
/// status body at all), so the `#[allow(dead_code)]` on `ram_gib` stays in
/// `engine/mod.rs` — nothing here fills it.
pub(crate) fn measurement_from_status(
    body: &serde_json::Value,
) -> Option<crate::engine::EngineMeasurement> {
    if body.get("error").is_some() {
        return None; // the engine's own refusal voice is absence, not a figure
    }
    let vram = body
        .pointer("/info/vram")
        .and_then(serde_json::Value::as_object)?;
    let mib = |key: &str| vram.get(key).and_then(serde_json::Value::as_f64);
    let (Some(expert_cache_mib), Some(vram_free_mib), Some(reserve_mib)) = (
        mib("expert_cache_mib"),
        mib("vram_free_mib"),
        mib("reserve_mib"),
    ) else {
        return None;
    };
    if !expert_cache_mib.is_finite()
        || !vram_free_mib.is_finite()
        || !reserve_mib.is_finite()
        || expert_cache_mib < 0.0
        || vram_free_mib < 0.0
        || reserve_mib < 0.0
    {
        return None;
    }
    let gpu_gib = (expert_cache_mib + reserve_mib) / 1024.0;
    if !gpu_gib.is_finite() {
        return None; // two finite giants can sum to infinity: absence, never +inf
    }
    Some(crate::engine::EngineMeasurement {
        gpu_gib,
        ram_gib: None,
        source: crate::engine::MeasurementSource::EngineStatus,
    })
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

    #[test]
    fn effective_context_reads_the_argv_max_context_token() {
        let raw = raw(&[("strata_args", "--pack, --native, --max-context, 262144")]);
        assert_eq!(effective_context(&raw).as_deref(), Some("262144"));
    }

    #[test]
    fn effective_context_names_no_value_without_the_token() {
        for line in ["--pack, --native", "--max-context", ""] {
            let raw = raw(&[("strata_args", line)]);
            assert_eq!(effective_context(&raw), None, "`{line}` names no value");
        }
        assert_eq!(effective_context(&raw(&[])), None, "no knob, no answer");
    }

    /// 10[1]: the eq form is spawn truth too (server.py splits `--flag=value`).
    /// Falsifier: drop the eq-form arm from the lookup and this line answers
    /// None while the engine would serve 262144.
    #[test]
    fn effective_context_reads_the_eq_form_figure() {
        let raw = raw(&[("strata_args", "--pack, --max-context=262144, --kv, int8")]);
        assert_eq!(effective_context(&raw).as_deref(), Some("262144"));
    }

    /// 10[2]: a flag in the value slot is not a figure. Falsifier: answer the
    /// value as `tokens.get(at + 1)` with no plausibility guard and `--kv`
    /// displays as the context.
    #[test]
    fn effective_context_refuses_a_flag_in_the_value_slot() {
        let raw = raw(&[("strata_args", "--max-context, --kv, int8")]);
        assert_eq!(effective_context(&raw), None, "a flag is not a figure");
    }

    /// Falsifier: drop the digits guard on the eq suffix and `auto` displays
    /// as the context.
    #[test]
    fn effective_context_refuses_a_non_numeric_eq_value() {
        let raw = raw(&[("strata_args", "--max-context=auto, --kv, int8")]);
        assert_eq!(effective_context(&raw), None, "`auto` is not a figure");
    }

    /// 10[3]: the two consumers share [`well_formed_tokens`] — a line with an
    /// empty entry refuses at render, so it shows no context either.
    /// Falsifier: let `effective_context` tokenize leniently (skip the shared
    /// well-formedness check) and the display shows `262144` for a row that
    /// never loads.
    #[test]
    fn an_empty_entry_shows_no_context_and_refuses_the_render() {
        let line = "--pack, , --max-context, 262144";
        let raw = raw(&[("strata_args", line)]);
        assert_eq!(effective_context(&raw), None, "`{line}` never loads");
        let base = fixture_pairs();
        let mut pairs: Vec<(&str, &str)> =
            base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pairs.push(("strata_args", line));
        let err = StrataAdapter::new(
            std::env::current_exe().expect("real .exe"),
            std::env::temp_dir(),
        )
        .spec(&entry(&pairs), 18109)
        .expect_err("an empty entry must refuse the render");
        let EngineError::Rejected { reason, .. } = &err else {
            panic!("expected Rejected, got {err:?}");
        };
        assert!(
            reason.contains("strata_args") && reason.contains("non-empty"),
            "{reason}"
        );
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

    /// A real file on disk stands in for every referenced path the walk checks
    /// (engine exe, tokenizer, weights) and, with the right extension, for the
    /// interpreter: the adapter shape-checks the interpreter and the render
    /// existence-checks every profile path before handing a spec over.
    fn real_file_with_extension(tag: &str, extension: &str) -> PathBuf {
        let path = crate::testutil::temp_dir("strata").join(format!("{tag}.{extension}"));
        std::fs::write(&path, b"{}").expect("temp file");
        path
    }

    fn path_text(path: &Path) -> &str {
        path.to_str().expect("utf-8 fixture path")
    }

    /// The minimal valid template row: existing engine exe + tokenizer, and a
    /// cwd that does not depend on the machine's engine homes.
    fn fixture_pairs() -> Vec<(String, String)> {
        let exe = real_file_with_extension("fixture-exe", "exe");
        let tokenizer = real_file_with_extension("fixture-tokenizer", "bin");
        let cwd = crate::testutil::temp_dir("strata-cwd");
        vec![
            ("strata_exe".to_string(), path_text(&exe).to_string()),
            (
                "strata_tokenizer".to_string(),
                path_text(&tokenizer).to_string(),
            ),
            ("strata_cwd".to_string(), path_text(&cwd).to_string()),
        ]
    }

    fn spec_of(base: &[(String, String)], extra: &[(&str, &str)]) -> LaunchSpec {
        let mut pairs: Vec<(&str, &str)> =
            base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pairs.extend_from_slice(extra);
        StrataAdapter::new(
            std::env::current_exe().expect("a real .exe stand-in"),
            std::env::temp_dir(),
        )
        .spec(&entry(&pairs), 18109)
        .expect("valid template spec")
    }

    /// The full-contract pin (row 19): the harness forces the bind and port as
    /// argv, the `--config` slot carries ONLY the content-addressed render
    /// name, and the rendered JSON owns every profile field with no policy
    /// keys of its own.
    #[test]
    fn the_render_names_its_bytes_and_the_argv_forces_bind_and_port() {
        let base = fixture_pairs();
        let weights = real_file_with_extension("weights", "gguf");
        let spec = spec_of(
            &base,
            &[(
                "strata_args",
                &format!("--pack, --native, {}", path_text(&weights)),
            )],
        );
        let text = spec.rendered_profile.expect("template render").text;
        let name = spec
            .argv
            .iter()
            .skip_while(|arg| *arg != "--config")
            .nth(1)
            .expect("the forced --config slot");
        assert_eq!(
            name.as_str(),
            staged_profile_name("strata-test", &text),
            "the argv placeholder must be the render's own content address"
        );
        let mut expected: Vec<String> = vec![std::env::current_exe()
            .expect("test exe")
            .to_string_lossy()
            .into_owned()];
        expected.extend(
            [
                "-m",
                "serve.server",
                "--engine",
                "strata",
                "--host",
                "127.0.0.1",
                "--config",
                name.as_str(),
                "--port",
                "18109",
            ]
            .map(str::to_string),
        );
        assert_eq!(spec.argv, expected);
        let profile: serde_json::Value = serde_json::from_str(&text).expect("rendered JSON");
        let top: Vec<&str> = profile
            .as_object()
            .expect("object")
            .keys()
            .map(String::as_str)
            .collect();
        for banned in ["host", "port", "key", "api_key"] {
            assert!(
                !top.contains(&banned),
                "the render owns no {banned}: {text}"
            );
        }
        assert_eq!(profile["model_name"], "qwen3.8-flash-next");
        assert_eq!(profile["args"][2], path_text(&weights));
        assert!(profile["log"]
            .as_str()
            .is_some_and(|log| log.ends_with("strata-test.log")));
        assert_eq!(spec.log_name, "strata-test");
        assert_eq!(
            spec.custody.as_deref(),
            Some("strata"),
            "every strata launch rides the custody sidecar (slice 10b)"
        );
        assert!(spec.env.is_empty());
    }

    /// Every optional knob lands in the rendered JSON with its engine shape.
    #[test]
    fn every_owned_field_lands_in_the_render() {
        let base = fixture_pairs();
        let pack = crate::testutil::temp_dir("strata-pack");
        let spec = spec_of(
            &base,
            &[
                ("strata_args", &format!("--pack, {}", path_text(&pack))),
                ("strata_lib_dirs", path_text(&pack)),
                ("strata_env", "STRATA_KVA=0 STRATA_HC_Q8=1"),
                ("strata_gpu", "0"),
                ("strata_parallel", "1"),
                ("strata_gpus_asked", "true"),
                (
                    "strata_sampling",
                    "temperature=1.0 top_k=20 uncensored=false",
                ),
                ("strata_uncensored", "true"),
                (
                    "strata_vision",
                    &format!(
                        "exe={} mmproj={} model={} gpu=true max_tokens=1024 min_tokens=4 threads=16",
                        path_text(&pack),
                        path_text(&pack),
                        path_text(&pack)
                    ),
                ),
            ],
        );
        let text = spec.rendered_profile.expect("template render").text;
        let profile: serde_json::Value = serde_json::from_str(&text).expect("rendered JSON");
        assert_eq!(profile["lib_dirs"][0], path_text(&pack));
        assert_eq!(profile["env"]["STRATA_KVA"], "0");
        assert_eq!(profile["gpu"], 0);
        assert_eq!(profile["parallel"], 1);
        assert_eq!(profile["gpus_asked"], true);
        assert_eq!(profile["vision"]["max_tokens"], 1024);
        assert_eq!(profile["vision"]["min_tokens"], 4);
        assert_eq!(profile["vision"]["threads"], 16);
        assert_eq!(profile["sampling"]["temperature"], serde_json::json!(1.0));
        assert_eq!(profile["sampling"]["top_k"], serde_json::json!(20));
        assert_eq!(profile["sampling"]["uncensored"], serde_json::json!(false));
        assert_eq!(profile["uncensored"], serde_json::json!(true));
        let last_arg = profile["args"].as_array().expect("args").last().cloned();
        assert_eq!(
            last_arg,
            Some(serde_json::Value::String("--vision".to_string())),
            "the vision block switches the engine on at the canonical tail: {text}"
        );
    }

    #[test]
    fn a_superseded_row_is_refused_naming_the_import_command() {
        let err = StrataAdapter::default()
            .spec(
                &entry(&[("strata_config", "C:\\Strata\\strata-iq3_s.json")]),
                18109,
            )
            .expect_err("external mode is superseded (row 19)");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("simplehar-se import strata") && reason.contains("superseded")),
            "{err}"
        );
    }

    #[test]
    fn declaring_both_profile_sources_is_a_refusal_not_a_priority() {
        let base = fixture_pairs();
        let mut pairs: Vec<(&str, &str)> =
            base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        pairs.push(("strata_config", "C:\\Strata\\manual.json"));
        let err = StrataAdapter::default()
            .spec(&entry(&pairs), 18109)
            .expect_err("one launch, one profile source");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("mutually exclusive")),
            "{err}"
        );
    }

    #[test]
    fn a_row_naming_no_profile_source_is_a_missing_field_naming_strata_exe() {
        let err = StrataAdapter::default()
            .spec(&entry(&[]), 18109)
            .expect_err("nothing to render");
        assert!(
            matches!(&err, EngineError::MissingField { ref field, .. } if field == "strata_exe"),
            "{err}"
        );
    }

    /// Pin of the `cpu_offload` engine scoping: the key stays inert for the
    /// strata adapter, so absent, `true` and `false` produce a byte-identical
    /// [`LaunchSpec`]. Falsifier: if this adapter starts reading the key, the
    /// equality fails here.
    #[test]
    fn cpu_offload_changes_nothing_for_a_strata_launch() {
        let base = fixture_pairs();
        let as_str: Vec<(&str, &str)> =
            base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        let spec_entry = |value: Option<&str>| {
            let mut pairs = as_str.clone();
            if let Some(value) = value {
                pairs.push(("cpu_offload", value));
            }
            StrataAdapter::new(
                std::env::current_exe().expect("real .exe"),
                std::env::temp_dir(),
            )
            .spec(&entry(&pairs), 18109)
            .expect("an inert key never refuses a launch")
        };
        let base_spec = spec_entry(None);
        for value in ["true", "false"] {
            assert_eq!(spec_entry(Some(value)), base_spec, "cpu_offload: {value}");
        }
    }

    /// The renderer refuses every shape a hostile-or-tired hand can type, and
    /// every refusal names the yaml key that must change (G6).
    #[test]
    fn shape_refusals_name_the_key_that_carries_the_value() {
        let base = fixture_pairs();
        let cases: &[(&str, &str, &str)] = &[
            ("strata_exe", "relative/exe.exe", "absolute path"),
            ("strata_gpu", "0;rm", "non-negative integer"),
            ("strata_env", "LD_PRELOAD=x", "STRATA_"),
            ("strata_args", "--pack,,--native", "non-empty"),
            ("strata_args", "--host, 0.0.0.0", "harness-forced argv"),
            ("strata_args", "--port, 8004", "harness-forced argv"),
            ("strata_args", "--host=0.0.0.0", "harness-forced argv"),
            ("strata_args", "--port=8004", "harness-forced argv"),
            ("strata_vision", "exe=nowhere", "absolute path"),
            ("strata_sampling", "temperature=hot", "number or bool"),
            ("strata_uncensored", "maybe", "`true` or `false`"),
        ];
        for (key, value, must_say) in cases {
            let mut pairs: Vec<(&str, &str)> =
                base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            pairs.push((key, value));
            let err = StrataAdapter::new(
                std::env::current_exe().expect("real .exe"),
                std::env::temp_dir(),
            )
            .spec(&entry(&pairs), 18109)
            .expect_err("{key} must refuse");
            let EngineError::Rejected { reason, .. } = &err else {
                panic!("{key}={value}: expected Rejected, got {err:?}");
            };
            assert!(
                reason.contains(key) && reason.contains(must_say),
                "{key}={value}: {reason}"
            );
        }
    }

    /// A model file deleted between scan and load meets the same typed walk
    /// refusal the scan gave (both doors run `validate_profile_text`).
    #[test]
    fn a_referenced_path_the_disk_lacks_refuses_the_launch() {
        let base = fixture_pairs();
        let spec_err = StrataAdapter::new(
            std::env::current_exe().expect("real .exe"),
            std::env::temp_dir(),
        )
        .spec(
            &entry(
                &(base
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .chain(std::iter::once((
                        "strata_args",
                        "--native, Z:\\nope\\weights.gguf",
                    )))
                    .collect::<Vec<_>>()),
            ),
            18109,
        )
        .expect_err("a doomed path must refuse");
        assert!(
            matches!(&spec_err, EngineError::Rejected { reason, .. }
                if reason.contains("references missing path")
                    && reason.contains("Z:\\nope\\weights.gguf")),
            "{spec_err}"
        );
    }

    /// Defense in depth for the 22[1] leg: the renderer structurally cannot
    /// emit a harness-forced field, but a JSON that shows one anyway — whatever
    /// wrote it — is refused naming every claim and its owner.
    #[test]
    fn a_profile_claiming_a_policy_field_refuses_even_from_the_harness_itself() {
        let exe = std::env::current_exe().expect("existing path for the walk");
        let problem = validate_profile_text(&format!(
            r#"{{"exe": "{}", "host": "0.0.0.0", "port": 8080}}"#,
            path_text(&exe).replace('\\', "\\\\")
        ))
        .expect_err("claims must refuse");
        assert_eq!(
            problem.describe("rendered:test"),
            policy_field_claims(
                "rendered:test",
                &[
                    (
                        "host",
                        "the engine bind is harness-forced (loopback, \u{a7}5)"
                    ),
                    (
                        "port",
                        "the engine port is harness-assigned from category config"
                    ),
                ]
            )
        );
    }

    /// FINDINGS2-0910 [5]'s interpreter floor survives the template: `argv[0]`
    /// is still the program CreateProcessW starts — a batch shim and a
    /// directory are both refused naming `strata_python`.
    #[test]
    fn the_interpreter_floor_still_refuses_a_shim_and_a_directory() {
        let base = fixture_pairs();
        let shim = real_file_with_extension("cmd-python", "cmd");
        let dir = crate::testutil::temp_dir("strata-python-dir");
        for (value, expect) in [
            (path_text(&shim), "must be an existing .exe file"),
            (path_text(&dir), "not a file"),
        ] {
            let mut pairs: Vec<(&str, &str)> =
                base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            pairs.push(("strata_python", value));
            let err = StrataAdapter::default()
                .spec(&entry(&pairs), 18109)
                .expect_err("a non-interpreter must not launch");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("strata_python") && reason.contains(expect)),
                "{value}: {err}"
            );
        }
    }

    #[test]
    fn a_missing_or_non_directory_cwd_is_refused_at_spec_time() {
        let base = fixture_pairs();
        let not_a_dir = real_file_with_extension("cwd-not-a-dir", "json");
        for value in ["Z:\\no-such-dir", path_text(&not_a_dir)] {
            let mut pairs: Vec<(&str, &str)> =
                base.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            pairs.push(("strata_cwd", value));
            let err = StrataAdapter::default()
                .spec(&entry(&pairs), 18109)
                .expect_err("a cwd the child cannot enter must not be launched");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("strata_cwd") && reason.contains("not an existing directory")),
                "{value}: {err}"
            );
        }
    }

    /// The golden round-trip (ruling 19c): a manual profile imports to a row,
    /// and the row renders a profile that carries the same engine contract —
    /// minus the harness-owned `host`/`port`/`log` home, and with vision
    /// spelled ONCE (the block), the switch re-appended canonically.
    #[test]
    fn import_then_render_reproduces_the_manual_profile() {
        let home = crate::testutil::temp_dir("roundtrip");
        let gguf = home.join("weights.gguf");
        let pack = home.join("pack");
        std::fs::write(&gguf, b"gguf").expect("weights");
        std::fs::create_dir_all(&pack).expect("pack");
        let profile = home.join("strata-iq3_s.json");
        std::fs::write(
            &profile,
            format!(
                r#"{{
                "exe": "{}",
                "args": ["--pack", "{}", "--native", "{}", "--vision"],
                "cwd": "{}",
                "tokenizer": "{}",
                "model_name": "qwen3.8-flash-next-iq3_s",
                "log": "C:\\Strata\\strata-iq3_s.log",
                "lib_dirs": ["{}"],
                "port": 8003,
                "gpu": 0,
                "gpus_asked": true,
                "host": "0.0.0.0",
                "env": {{"STRATA_KVA": "0"}},
                "sampling": {{"temperature": 1.0, "top_p": 0.95, "top_k": 20}},
                "uncensored": false,
                "vision": {{"exe": "{}", "mmproj": "{}", "model": "{}", "gpu": true,
                            "max_tokens": 1024, "min_tokens": 4, "threads": 16}}
            }}"#,
                path_text(&gguf).replace('\\', "\\\\"),
                path_text(&pack).replace('\\', "\\\\"),
                path_text(&gguf).replace('\\', "\\\\"),
                path_text(&home).replace('\\', "\\\\"),
                path_text(&pack).replace('\\', "\\\\"),
                path_text(&pack).replace('\\', "\\\\"),
                path_text(&gguf).replace('\\', "\\\\"),
                path_text(&gguf).replace('\\', "\\\\"),
                path_text(&gguf).replace('\\', "\\\\"),
            ),
        )
        .expect("write fixture profile");
        let yaml = crate::import::strata_yaml(&profile).expect("imports");
        let keys: BTreeMap<String, String> = yaml
            .lines()
            .filter(|line| !line.starts_with('#'))
            .filter_map(|line| line.split_once(": "))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let model = ModelEntry::from_flat(&keys, "strata-roundtrip").expect("row parses");
        let rendered = render_profile(&model, &home).expect("row renders");
        let got: serde_json::Value = serde_json::from_str(&rendered).expect("rendered JSON");
        let wanted: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&profile).expect("read back"))
                .expect("fixture JSON");
        assert_eq!(got["exe"], wanted["exe"]);
        assert_eq!(
            got["args"], wanted["args"],
            "the vision switch round-trips at the canonical tail"
        );
        assert_eq!(got["cwd"], wanted["cwd"]);
        assert_eq!(got["tokenizer"], wanted["tokenizer"]);
        assert_eq!(got["model_name"], wanted["model_name"]);
        assert_eq!(got["lib_dirs"], wanted["lib_dirs"]);
        assert_eq!(got["gpu"], wanted["gpu"]);
        assert_eq!(got["gpus_asked"], wanted["gpus_asked"]);
        assert_eq!(got["env"], wanted["env"]);
        assert_eq!(
            got["sampling"], wanted["sampling"],
            "sampling defaults round-trip with their source types"
        );
        assert_eq!(got["uncensored"], wanted["uncensored"]);
        // The vision pin below is only a TYPE pin while number equality is
        // variant-strict (review 26): 16 must never equal 16.0 here.
        assert_ne!(
            serde_json::json!(16),
            serde_json::json!(16.0),
            "number equality must stay variant-strict or the round-trip could \
             pass with a drifted type"
        );
        assert_eq!(got["vision"], wanted["vision"]);
        assert!(got.get("host").is_none() && got.get("port").is_none());
        assert!(got["log"]
            .as_str()
            .is_some_and(|log| !log.contains("C:\\Strata")));
        validate_profile_text(&rendered).expect("the round-tripped render validates");
    }

    fn snapshot(
        expert: serde_json::Value,
        free: serde_json::Value,
        reserve: serde_json::Value,
    ) -> serde_json::Value {
        serde_json::json!({
            "status": "ok",
            "loaded": true,
            "info": { "model": "qwen3.8-flash-next-fp4local", "vram": {
                "expert_cache_mib": expert,
                "vram_free_mib": free,
                "reserve_mib": reserve,
            } }
        })
    }

    /// Phase-3 pin (a): a realistic-shape `/status` body answers the full
    /// chain — fake axum server on an ephemeral port, the runtime's generic
    /// GET, then this parse. The figures are the recorded row facts, not a
    /// capture (no raw `/status` body is on file): the operator's measured
    /// 67.8 GiB row (`strata-fp4local.yaml`), the launch reserve of 2,048
    /// MiB from the row's `strata_args`, and a free figure inside the card
    /// state the 2026-11-04 run observed — cache+reserve lands on the row
    /// figure, below the 70.4 GiB footprint observed while loaded.
    #[tokio::test]
    async fn the_status_chain_lands_the_footprint_on_the_row_figure() {
        use crate::engine::EngineRuntime as _;
        let body: &'static str = r#"{"status":"ok","loaded":true,"info":{"model":"qwen3.8-flash-next-fp4local","loaded":true,"vram":{"expert_cache_mib":67380,"vram_free_mib":2512,"reserve_mib":2048}}}"#;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("addr").port();
        let app =
            axum::Router::new().route("/status", axum::routing::get(move || async move { body }));
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let runtime = crate::process::ProcessRuntime::new();
        let json = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            runtime.engine_status_json(port, "/status"),
        )
        .await
        .expect("status answers")
        .expect("the body parses as JSON");
        let measurement = measurement_from_status(&json).expect("the realistic snapshot parses");
        assert_eq!(
            measurement.source,
            crate::engine::MeasurementSource::EngineStatus
        );
        assert!(
            (measurement.gpu_gib - 67.8).abs() < 0.01,
            "cache+reserve = 69,428 MiB = the 67.8 GiB row figure, got {}",
            measurement.gpu_gib
        );
        assert_eq!(measurement.ram_gib, None, "no RAM figure in this snapshot");
    }

    /// Phase-3 pin (b): an empty or absent vram snapshot is absence — the
    /// sampler keeps its fallback legs, never a zero-booked footprint. The
    /// basalt wrapper (`engine.info.vram`) is a foreign shape here: strata's
    /// `/status` answers the vram object under `info`, not under `engine`.
    #[test]
    fn an_empty_or_absent_vram_snapshot_is_absence() {
        let triple = snapshot(
            serde_json::json!(67380),
            serde_json::json!(2512),
            serde_json::json!(2048),
        );
        let basalt_wrapped = serde_json::json!({"engine": {"info": triple["info"]}});
        for body in [
            serde_json::json!({}),
            serde_json::json!(null),
            serde_json::json!([]),
            serde_json::json!({"info": {}}),
            serde_json::json!({"info": {"vram": null}}),
            serde_json::json!({"info": {"vram": 67.8}}),
            serde_json::json!({"vram": {"expert_cache_mib": 67380, "vram_free_mib": 2512, "reserve_mib": 2048}}),
            basalt_wrapped,
        ] {
            assert_eq!(
                measurement_from_status(&body),
                None,
                "a body without a vram snapshot object under `info` is absence: {body}"
            );
        }
    }

    /// Phase-3 pin (c): the engine's error envelope is absence, even when it
    /// rides next to figures that would otherwise parse — a refusal voice
    /// never doubles as a footprint. (The envelope texts are shapes, not
    /// captured strata sentences.)
    #[test]
    fn an_error_envelope_is_absence() {
        assert_eq!(
            measurement_from_status(
                &serde_json::json!({"error": "RuntimeError: status unavailable"})
            ),
            None
        );
        let mut poisoned = snapshot(
            serde_json::json!(67380),
            serde_json::json!(2512),
            serde_json::json!(2048),
        );
        poisoned["error"] = serde_json::json!("engine refused");
        assert_eq!(
            measurement_from_status(&poisoned),
            None,
            "error wins over figures"
        );
    }

    /// Phase-3 pin: a partial or malformed vram snapshot is absence — every
    /// member of the three must be a present, finite, non-negative number.
    #[test]
    fn a_partial_or_malformed_snapshot_is_absence() {
        let good = snapshot(
            serde_json::json!(67380),
            serde_json::json!(2512),
            serde_json::json!(2048),
        );
        assert!(
            measurement_from_status(&good).is_some(),
            "the full triple parses"
        );
        for key in ["expert_cache_mib", "vram_free_mib", "reserve_mib"] {
            let mut missing = good.clone();
            missing["info"]["vram"]
                .as_object_mut()
                .expect("vram object")
                .remove(key);
            assert_eq!(
                measurement_from_status(&missing),
                None,
                "`{key}` missing is absence"
            );
            for bad in [
                serde_json::json!("2048"),
                serde_json::json!(null),
                serde_json::json!(-1),
            ] {
                let mut malformed = good.clone();
                malformed["info"]["vram"][key] = bad.clone();
                assert_eq!(
                    measurement_from_status(&malformed),
                    None,
                    "`{key}`={bad} is not a figure"
                );
            }
        }
    }

    /// Each member is finite, yet two astronomical ones can sum to infinity
    /// — the derived figure's own finiteness is the last gate, so a giant
    /// never reaches the actor as `+inf` (the phase-2 review gate, pinned
    /// for this parse too).
    #[test]
    fn two_finite_giant_fields_that_would_sum_to_infinity_answer_none() {
        let giant = serde_json::json!(f64::MAX);
        let body = snapshot(giant.clone(), serde_json::json!(2512), giant);
        assert_eq!(
            measurement_from_status(&body),
            None,
            "finite + finite must not book +inf"
        );
    }

    /// The `--vram-elastic` decision (doc comment): no launch flag gates this
    /// parse. The extra key stands for any body-level elasticity claim —
    /// this parse reads the triple alone, so a row launched without
    /// `--vram-elastic` (whose status carries no such claim, or the opposite
    /// one) parses to the same figure, and a status that lacks the figure
    /// falls to the process-tree fallback either way. Falsifier: make the
    /// parse consult an elasticity marker and the two answers diverge.
    #[test]
    fn no_launch_flag_gates_the_parse() {
        let without = snapshot(
            serde_json::json!(67380),
            serde_json::json!(2512),
            serde_json::json!(2048),
        );
        let mut with_claim = without.clone();
        with_claim["info"]["vram"]["vram_elastic"] = serde_json::json!(false);
        assert_eq!(
            measurement_from_status(&with_claim),
            measurement_from_status(&without),
            "the parse is blind to any elasticity claim"
        );
        assert!(measurement_from_status(&without).is_some());
    }
}
