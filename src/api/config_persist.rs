//! The config read/persist path: the `POST /har/config/default` and
//! `POST /har/config/model` handlers, the runtime logging switch of
//! `POST /har/config/logging`, and the atomic boot-config `default_model`
//! write-back they share. The `ApiState` accessors, the catalog resolution
//! shared with the load and proxy paths, and the router wiring stay in
//! `mod.rs`.

use crate::config::{AppConfig, CategoryId, ConfigError};
use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

use super::{resolve_unique, ApiError, ApiState};

#[cfg(test)]
use std::sync::Mutex;

impl ApiState {
    /// Sets (or clears, with `None`) the category default and persists it to
    /// `simpleharn.json` atomically (temp file + rename). Memory applies only
    /// after the bytes landed: a write that never landed leaves the effective
    /// default at its previous value, so the config file stays king between the
    /// failed call and the next boot (R2-07-03). When `persisted: false` names a
    /// write that never landed, the answer carries the still-effective value.
    ///
    /// A write that lands re-validates the published bytes against the boot schema
    /// (07\[11]): the landed tree is exactly what the next boot will read, so its
    /// per-field verdict is recorded for `/har/status` `config_errors` — the field's
    /// only producer, replacing the last verdict on every write whose bytes reached
    /// the live file. A write that never landed leaves the field alone (the file
    /// still holds the bytes the previous verdict described). A rename that landed
    /// but whose read-back did not confirm the published tree records the verdict of
    /// the bytes that ARE on disk (R2-07-02): the harness just replaced the boot
    /// file, so keeping the old verdict would re-assert a claim about bytes the file
    /// no longer holds.
    ///
    /// The write-back runs on a blocking thread (FINDINGS-07 \[8]): the handler's
    /// async worker must not park on the gated read-patch-copy-rename, while the
    /// `persisted` answer still means what it always meant — the handler waits for
    /// the leg's verdict before replying, so `/har/config/default` keeps its
    /// response honesty (06\[5]). A blocking task that dies (panic/cancel) can only
    /// ever be answered `persisted: false`.
    pub async fn set_default_model(
        &self,
        category: CategoryId,
        model: Option<&str>,
    ) -> (Option<String>, bool) {
        let owned_model = model.map(str::to_string);
        let config = self.config.clone();
        let persist_model = owned_model.clone();
        match tokio::task::spawn_blocking(move || {
            persist_default_model(&config, category, persist_model.as_deref())
        })
        .await
        {
            Ok(outcome) => match outcome {
                PersistOutcome::Landed { verdict } => {
                    self.record_config_errors(verdict);
                    self.default_overrides
                        .lock()
                        .expect("default override lock poisoned by a writer panic")
                        .insert(category, owned_model.clone());
                    (owned_model, true)
                }
                // The rename landed: the live boot file now holds bytes the
                // read-back refused, so the verdict must describe them even
                // though the answer stays `persisted: false` (R2-07-02). The
                // file is king and it holds the NEW bytes, so memory follows
                // them (R2-07-03).
                PersistOutcome::LandedUnverified { verdict } => {
                    self.record_config_errors(verdict);
                    self.default_overrides
                        .lock()
                        .expect("default override lock poisoned by a writer panic")
                        .insert(category, owned_model.clone());
                    (owned_model, false)
                }
                // The bytes never landed: the file still holds the previous
                // default, so memory must not move (R2-07-03). The answer
                // carries the still-effective value, not the refused edit.
                PersistOutcome::NotLanded => (self.default_model(category), false),
            },
            Err(error) => {
                tracing::error!(
                    error = %error,
                    "config persist task died; answering persisted:false"
                );
                (self.default_model(category), false)
            }
        }
    }
}

/// Where a config write-back belongs: the file boot actually loaded (`SIMPLEHARN_CONFIG`
/// included), else the default beside the executable — taken from
/// [`crate::config::default_config_path`] so the file the harness reads and the file
/// it writes can never drift apart.
///
/// The final `PathBuf::from(CONFIG_FILE)` is a last resort for a process that cannot
/// resolve its own executable path: `boot_config` always stamps `source_path`, so this
/// is reached in practice only by a hand-built `AppConfig`. The harness never writes a
/// config file other than the one it loaded or [`crate::config::CONFIG_FILE`], unless
/// the operator explicitly selected another path as `SIMPLEHARN_CONFIG` — an explicit
/// path is an operator instruction and config file is king (ruling 2026-11-03,
/// `TASK-DEBT-LEFTOVERS`). The fallback must therefore name [`crate::config::CONFIG_FILE`]:
/// naming any other file would write a file boot never loads (README review, 2026-10-27).
pub(super) fn config_write_path(config: &AppConfig) -> std::path::PathBuf {
    config
        .source_path
        .clone()
        .or_else(crate::config::default_config_path)
        .unwrap_or_else(|| std::path::PathBuf::from(crate::config::CONFIG_FILE))
}

/// The verdict of one config publish, told in three legs so the caller can keep
/// two separate claims apart: "the bytes landed and read back" (`persisted`) and
/// "this is what the live boot file now says" (`config_errors`). A rename-success/
/// verify-failure separates them — the file changed while the answer must stay
/// `persisted: false` — and conflating the two let `/har/status` keep asserting
/// the previous verdict over a file the harness itself had just replaced
/// (R2-07-02).
#[derive(Debug)]
pub(super) enum PersistOutcome {
    /// The rename landed and the read-back confirmed the published tree;
    /// `verdict` is the boot-schema validation of the landed bytes (07[11]).
    Landed { verdict: Vec<ConfigError> },
    /// The rename landed but the read-back did not confirm the published tree:
    /// the live file now holds bytes nobody validated. `verdict` is what those
    /// landed bytes say the next boot would refuse — the same authority as a
    /// successful write-back's verdict, recorded even though the answer is
    /// `persisted: false` (R2-07-02).
    LandedUnverified { verdict: Vec<ConfigError> },
    /// Nothing reached the live file (pre-rename failure): the previous verdict
    /// still honestly describes it, so the caller must not record anything.
    NotLanded,
}

#[cfg(test)]
impl PersistOutcome {
    pub(super) fn is_landed(&self) -> bool {
        matches!(self, Self::Landed { .. })
    }
}

/// Writes `category.default_model` into the config file atomically: parse the
/// on-disk file (or the boot config when absent), patch the one field, temp+rename.
///
/// The whole read-patch-write-rename runs inside the model write-back's gate and onto
/// a per-process unique temp: two handlers reach this function (`/har/config/default`
/// and `/har/config/model`'s dangling-default clear) on separate workers, and an
/// ungated read-modify-write onto one fixed temp lost one of their fields while both
/// answered `persisted: true` (FINDINGS-07 [3]).
///
/// It is the synchronous core by design — [`ApiState::set_default_model`] runs it
/// inside `tokio::task::spawn_blocking`, so the gate is held on a blocking thread
/// where parking is allowed and the reactor never is (FINDINGS-07 [8]).
///
/// Returns a [`PersistOutcome`]: only the `Landed` leg may answer
/// `persisted: true`. `LandedUnverified` means the rename landed and the
/// read-back refused the tree — its verdict describes the bytes that ARE on
/// disk (R2-07-02). `NotLanded` (the pre-rename failures) left the live file
/// untouched, so the previous verdict still describes it and none is returned.
pub(super) fn persist_default_model(
    config: &AppConfig,
    category: CategoryId,
    model: Option<&str>,
) -> PersistOutcome {
    // One critical section with every yaml rewrite (FINDINGS-07 [3]): the second
    // writer can no longer read the ORIGINAL while the first one is between its
    // patch and its rename, which is how a persisted field used to vanish from disk.
    let _gate = crate::model::writeback::acquire_write_gate();
    let path = config_write_path(config);
    let mut root: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|error| {
            tracing::warn!(error = %error, path = %path.display(), "config unreadable; rewriting from boot config");
            serde_json::to_value(&*config).expect("invariant: config serializes")
        }),
        Err(error) => {
            // Silent before: the only difference between this arm and the one above
            // is that the file could not be read at all, and both rewrite the whole
            // file from the boot snapshot, so both must say so (FINDINGS-07 [2]'s
            // unlogged arm).
            tracing::warn!(error = %error, path = %path.display(), "config file missing or unreadable; writing from boot config");
            serde_json::to_value(config).expect("invariant: config serializes")
        }
    };
    let id = category.to_string();
    // A partial config file (e.g. only `modelconf_dir` set) has no categories to
    // patch — rebuild the tree from the boot config, which carries all four.
    if root
        .get("categories")
        .and_then(serde_json::Value::as_array)
        .is_none()
    {
        root = serde_json::to_value(config).expect("invariant: config serializes");
    }
    let Some(categories) = root.get_mut("categories").and_then(|c| c.as_array_mut()) else {
        tracing::error!(path = %path.display(), "cannot persist default_model: no categories in the config file");
        return PersistOutcome::NotLanded;
    };
    let mut patched = false;
    for entry in categories.iter_mut() {
        if entry.get("id").and_then(|v| v.as_str()) == Some(id.as_str()) {
            let map = match entry.as_object_mut() {
                Some(map) => map,
                None => continue,
            };
            match model {
                Some(name) => {
                    map.insert("default_model".to_string(), Value::String(name.to_string()));
                }
                None => {
                    map.insert("default_model".to_string(), Value::Null);
                }
            }
            patched = true;
            break;
        }
    }
    if !patched {
        tracing::error!(category = %id, "cannot persist default_model: category not found");
        return PersistOutcome::NotLanded;
    }
    let text = serde_json::to_string_pretty(&root).expect("invariant: JSON serializes");
    // Unique per (process, sequence), mirroring the yaml temp in `model::writeback`:
    // on the fixed name a second writer truncated the first one's temp, so one
    // rename failed while the other's content landed (FINDINGS-07 [3]).
    let tmp = path.with_extension(format!(
        "json.{}.{}.sh.tmp",
        std::process::id(),
        crate::model::writeback::next_temp_seq()
    ));
    // The flushed door the yaml publisher and the first-run config writer use
    // (writeback.rs:605-612, firstrun.rs:158): a rename may never expose a temp
    // whose content the kernel merely buffered, or `persisted: true` can describe
    // bytes the volume was never told to commit and the file that boots can be
    // zero/partial after a crash in the write->rename window (R2-07-01).
    if let Err(error) = crate::model::writeback::write_synced(&tmp, text.as_bytes()) {
        tracing::error!(error = %error, "cannot persist default_model: write failed");
        return PersistOutcome::NotLanded;
    }
    // Keep the previous version beside the live file: schema additions are a
    // one-way door for `deny_unknown_fields` (an old binary refuses a file a new
    // one wrote), so rollback needs the pre-write copy (review, 2026-10-26).
    // The copy goes through the same flushed write as the temp — a rollback reads
    // exactly this file after a bad publish, and the forensic copy in
    // `model::writeback` is flushed for the same reason (R2-07-01).
    if path.exists() {
        let bak = path.with_extension("json.bak");
        let backup = std::fs::read_to_string(&path)
            .map(|previous| crate::model::writeback::write_synced(&bak, previous.as_bytes()))
            .and_then(|written| written);
        if let Err(error) = backup {
            tracing::warn!(error = %error, "could not write config backup; continuing");
        }
    }
    if let Err(error) = std::fs::rename(&tmp, &path) {
        // The temp is ours alone; leaving it behind turned a failed write into
        // permanent litter (writeback does the same on its rename failure).
        let _ = std::fs::remove_file(&tmp);
        tracing::error!(error = %error, "cannot persist default_model: rename failed");
        return PersistOutcome::NotLanded;
    }
    // The symmetric leg the yaml writer has had since D4 and this one lacked
    // (FINDINGS2-0507 [7]): `rename()` returning `Ok` is the OS accepting the move, not
    // proof the bytes landed. This is the one file the harness re-reads at boot under
    // `deny_unknown_fields`, so a silent-short write is a harness that will not start —
    // `persisted: true` may not be cheaper than the truth. Re-read, re-parse, and
    // require it to parse back to exactly the tree that was published.
    // The test arm corrupts the bytes this verification is about to read (valid JSON,
    // different tree), so the `persisted:false` leg of the pin is produced by the real
    // `read_back == root` comparison rather than a hard-set verdict — a rubber-stamped
    // compare has to be caught by the pin (FINDINGS-BATCH-2026-10-28M, item 6 note).
    #[cfg(test)]
    config_corrupt_if_armed(&path);
    let read_back = match std::fs::read_to_string(&path)
        .map_err(|error| format!("read-back failed: {error}"))
        .and_then(|text| {
            serde_json::from_str::<Value>(&text)
                .map_err(|error| format!("re-parse failed: {error}"))
        }) {
        Ok(read_back) if read_back == root => read_back,
        // The rename already landed: the live boot file now holds these bytes,
        // whether or not they are the published tree. Refusing the answer is not
        // enough — `/har/status` `config_errors` must describe what is actually on
        // disk, or it would keep asserting the previous verdict over a file the
        // harness itself just replaced (R2-07-02). The verdict is derived with the
        // same authority as a successful write-back's: the boot's own validator
        // over the landed tree.
        Ok(landed) => {
            tracing::error!(
                path = %path.display(),
                "the persisted config file parses to a different tree than the one published; answering persisted:false"
            );
            return PersistOutcome::LandedUnverified {
                verdict: validate_published_config(&landed),
            };
        }
        // Unreadable or unparseable: the next boot would refuse the file root, so
        // one root-naming error is what the field must hold — same shape as
        // `validate_published_config`'s parse leg.
        Err(error) => {
            tracing::error!(
                error = %error,
                path = %path.display(),
                "cannot verify the persisted config file; answering persisted:false"
            );
            return PersistOutcome::LandedUnverified {
                verdict: vec![ConfigError::Invalid {
                    field: crate::config::CONFIG_FILE.to_string(),
                    reason: error,
                }],
            };
        }
    };
    tracing::info!(category = %id, model = ?model, path = %path.display(), "default_model persisted to the config file");
    PersistOutcome::Landed {
        verdict: validate_published_config(&read_back),
    }
}

/// Re-validate the tree the boot config file now holds after a landing write-back
/// (07[11]: the `/har/status` `config_errors` producer). The harness cannot fix a file
/// the operator hand-edited while it ran — reload re-reads the catalog and nothing
/// else (Step 9, LOCKED) — but the moment a harness write-back publishes the file it
/// owns every byte the next boot will read, so the verdict the boot would render
/// ([`AppConfig::validate`]) is reported while the harness is still alive to be asked.
/// A tree that no longer parses as the schema (a hand-edited unknown key rides the
/// read-patch-write as `Value` passthrough) yields one error naming the file root —
/// the same shape [`AppConfig::parse`] gives at boot.
fn validate_published_config(root: &Value) -> Vec<ConfigError> {
    match serde_json::from_value::<AppConfig>(root.clone()) {
        Ok(landed) => landed.validate(),
        Err(error) => vec![ConfigError::Invalid {
            field: crate::config::CONFIG_FILE.to_string(),
            reason: error.to_string(),
        }],
    }
}

/// Paths whose post-rename verification read is preceded by a byte corruption
/// (FINDINGS2-0507 [7]); see [`config_corrupt_if_armed`], which is shaped after the
/// write-back module's `corrupt_if_armed`.
#[cfg(test)]
fn config_read_back_faults() -> &'static Mutex<Vec<std::path::PathBuf>> {
    static ARMED: std::sync::OnceLock<Mutex<Vec<std::path::PathBuf>>> = std::sync::OnceLock::new();
    ARMED.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
pub(crate) fn arm_config_read_back_fault(path: &std::path::Path) {
    config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .push(path.to_path_buf());
}

#[cfg(test)]
pub(crate) fn release_config_read_back_fault(path: &std::path::Path) {
    config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .retain(|armed| armed != path);
}

/// The arm's bite (FINDINGS-BATCH-2026-10-28M, item 6 coverage note): between the
/// rename and the verification read, rewrite the file with **different but valid**
/// bytes, so production's own `read_back == root` comparison is what refuses the
/// `persisted: true`. Hard-setting a verdict here instead would have left the
/// always-`true` regression unpinned: the arm proves the compare runs, only a corrupted
/// file proves the compare decides.
#[cfg(test)]
fn config_corrupt_if_armed(path: &std::path::Path) {
    let armed = config_read_back_faults()
        .lock()
        .expect("config read-back arm lock")
        .iter()
        .any(|tracked| tracked == path);
    if !armed {
        return;
    }
    let text = std::fs::read_to_string(path).expect("the file exists in the verify window");
    let mut value: Value = serde_json::from_str(&text).expect("the published file is valid JSON");
    value
        .as_object_mut()
        .expect("a JSON object was published")
        .insert("tampered".to_string(), Value::Bool(true));
    let bytes = serde_json::to_string(&value).expect("invariant: JSON serializes");
    std::fs::write(path, bytes.as_bytes()).expect("the fixture corruption lands");
}

/// `POST /har/config/default {"category": …, "model": name|null}` — set the
/// category's default model (Step 11 fallback); validated against the catalog and
/// persisted to `simpleharn.json` atomically.
///
/// `model` is required: a string sets the default, `null` clears it, and an absent
/// key is a caller mistake (400), never an implicit clear. The persisted value is the
/// catalog entry's canonical `name`, not the caller's spelling: `catalog::resolve`
/// accepts case-insensitive names and GGUF aliases, while the scheduler and the boot
/// check compare `m.name == model` exactly (FINDINGS-06 [5]).
pub(super) async fn har_config_default(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with `category` and `model`: {rejection}"),
    })?;
    let Some(raw_category) = body.get("category").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: format!(
                "missing required field `category` ({})",
                crate::config::CATEGORY_HINT
            ),
        });
    };
    let category: CategoryId = raw_category.parse().map_err(|_| ApiError::BadRequest {
        reason: format!("unknown category `{raw_category}`"),
    })?;
    let requested = match body.get("model") {
        None => {
            return Err(ApiError::BadRequest {
                reason: "missing required field `model` (send null to clear)".to_string(),
            })
        }
        Some(Value::Null) => None,
        Some(Value::String(model)) => {
            let catalog = state.catalog().await?;
            let entry = resolve_unique(model, &catalog, |query| ApiError::BadRequest {
                reason: format!("unknown model `{query}`"),
            })?;
            if entry.category != category {
                return Err(ApiError::BadRequest {
                    reason: format!(
                        "model `{}` belongs to {}, not {}",
                        entry.name, entry.category, category
                    ),
                });
            }
            Some(entry.name.clone())
        }
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`model` must be a string or null".to_string(),
            })
        }
    };
    let (effective, persisted) = state
        .set_default_model(category, requested.as_deref())
        .await;
    Ok(Json(json!({
        "status": "ok",
        "category": category,
        "default_model": effective,
        "persisted": persisted,
    })))
}

/// `POST /har/config/model {"model": name, "category"?: WORK|RP|AUX|EMB, "cpu"?: bool}`
/// — write per-model flags into the model's yaml (Step 9: the file stays the
/// source of truth). A `cpu` edit on a model whose engine row lacks the
/// `cpu_offload` knob is refused 400 naming the engine: that key changes no
/// launch there, and the harness never writes inert keys.
/// A category change on a *loaded* model frees the slot so the
/// next use reloads it into the right listener (v3.3 §-behavior, kept). The reply
/// keeps `"status":"edited"` for the yaml edit it did and additionally names each
/// retag follow-up's outcome — `slot_freed` and `default_cleared` (`null` when not
/// attempted), plus `warnings` when either failed (R2-07-05). A wrong-typed
/// `category` is refused 400, not treated as absent (R2-07-04).
pub(super) async fn har_config_model(
    State(state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with a `model` field: {rejection}"),
    })?;
    let Some(model_name) = body.get("model").and_then(Value::as_str) else {
        return Err(ApiError::BadRequest {
            reason: "missing required field `model`".to_string(),
        });
    };
    let catalog = state.catalog().await?;
    let entry = resolve_unique(model_name, &catalog, |query| ApiError::NotFound {
        what: format!("model `{query}`"),
    })?;
    let path = entry.file.clone().ok_or_else(|| ApiError::BadRequest {
        reason: format!(
            "model `{}` has no backing file (synthetic entry)",
            entry.name
        ),
    })?;
    let old_category = entry.category;

    let mut edits: Vec<(&str, Option<String>)> = Vec::new();
    let mut requested_category: Option<CategoryId> = None;
    // A present-but-non-string `category` read as absent through
    // `and_then(Value::as_str)`: the same defect FINDINGS2-0507 [5] fixed for `cpu`
    // below — a client that asked for two changes got one and an "edited" saying
    // both. The parity rule (07[10]) forbids one field's type bug living one arm
    // from its sibling's fix, so a wrong-typed `category` is a named 400, before
    // any write (R2-07-04).
    match body.get("category") {
        Some(Value::String(raw)) => {
            let category: CategoryId = raw
                .parse()
                .map_err(|reason| ApiError::BadRequest { reason })?;
            requested_category = Some(category);
            edits.push(("category", Some(category.to_string())));
            // FINDINGS-12 [1]: a surviving `is_embedding_model: true` beside the new
            // non-EMB `category:` is an `EmbeddingCategoryConflict` that makes the next
            // scan DROP the whole file — one dropdown click deleted the model while the
            // endpoint answered success. The two facts are one fact, so the removal
            // rides the same atomic rewrite as the category write. EMB keeps the key:
            // alias + EMB is the legal shape, and removing it would destroy an
            // operator-authored declaration for no gain.
            if category != CategoryId::Emb {
                edits.push(("is_embedding_model", None));
            }
        }
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`category` must be a string".to_string(),
            })
        }
        None => {}
    }
    // A wrong-typed `cpu` is a caller mistake, not an absent one: `and_then(as_bool)`
    // dropped `"true"`/`1`/`"yes"` silently and the request still answered
    // `{"status":"edited"}` — a client that asked for two changes got one, reported as
    // success. The in-file sibling `har_config_logging` 400s a non-bool `enabled` for
    // exactly this, and 07[10] established the in-file parity rule
    // (FINDINGS2-0507 [5]). The type is checked before the engine gate, so a malformed
    // body gets the same answer regardless of the row it named.
    let cpu = match body.get("cpu") {
        Some(Value::Bool(value)) => Some(*value),
        Some(_) => {
            return Err(ApiError::BadRequest {
                reason: "`cpu` must be a boolean".to_string(),
            })
        }
        None => None,
    };
    if let Some(cpu) = cpu {
        // The key is a knob the row's engine must actually read (same const as the
        // scan hint, the stamp and the webui gate): the page stopped offering the
        // control for non-offload rows, so the API must not let a script mint the
        // dead yaml line the scan would then report. Refused with the named
        // sentence, not silently accepted or dropped.
        if !crate::engine::llama::offload_live(&entry.engine) {
            return Err(ApiError::BadRequest {
                reason: crate::model::scan::inert_key_reason("cpu_offload", &entry.engine),
            });
        }
        edits.push(("cpu_offload", Some(cpu.to_string())));
    }
    if edits.is_empty() {
        return Err(ApiError::BadRequest {
            reason: "nothing to edit: send `category` and/or `cpu`".to_string(),
        });
    }
    // One rewrite for every edit: calling `set_flag_on_file` per key would leave
    // the file half-edited (category moved, alias still there) if a write failed
    // between the two — 1g's non-atomic defect, not replicated here.
    // FINDINGS-07 [8]: the gated rewrite and the fingerprint that feeds the
    // re-baseline note run together on a blocking thread — the request worker
    // never parks on the yaml's read-modify-rename. The order the interlock
    // depends on survives intact: the write completes, the stat reads the bytes
    // the write landed, and only then does this handler send the note (before
    // dispatching the reload), so the actor's mailbox still orders the
    // re-baseline ahead of the reload turn's verdict refresh.
    let write_path = path.clone();
    let write_edits = edits.clone();
    let (written, fingerprint) = tokio::task::spawn_blocking(move || {
        let refs: Vec<(&str, Option<&str>)> = write_edits
            .iter()
            .map(|(key, value)| (*key, value.as_deref()))
            .collect();
        let written = crate::model::writeback::set_flags_on_file(&write_path, &refs);
        let fingerprint = written
            .is_ok()
            .then(|| crate::model::fingerprint(&write_path))
            .flatten();
        (written, fingerprint)
    })
    .await
    .map_err(|error| ApiError::Internal {
        reason: format!("model-yaml write task failed: {error}"),
    })?;
    written.map_err(|error| ApiError::from_writeback(&path, error))?;
    // Axis-2 interlock 5: the harness's own flag edit is not a config change since
    // launch. Re-baseline any resident's spawn-side capture from this file BEFORE
    // the reload.
    if let Some(fingerprint) = fingerprint {
        state
            .scheduler
            .note_model_file_written(&entry.name, &path, fingerprint);
    }
    // The catalog must reflect the file the moment the operator looks again.
    state.scheduler.reload().await.map_err(ApiError::from)?;

    // A model whose category actually changed must move slots, not lie in place
    // (v3.3 §-behavior, kept). Only ITS OWN slot: `unload(old_category)` freed every
    // occupant of the old category, so retagging one AUX model killed the other
    // five's engines and whatever WORK held, in-flight generations included
    // (FINDINGS-07 [1]). A retag of a never-loaded model is the idempotent `Done`
    // the actor already guarantees for `unload_model` (pinned by scheduler_actor's
    // T12.2 test), which is why the old inner catalog-name test — meant to skip
    // that no-op — is gone: it never asked whether the moved model was loaded, only
    // whether SOME entry shared its name in a different category, so the duplicate
    // name of chunk 7 [9] was enough to trigger the collateral kill. If the moved
    // model was its old category's default, that default now dangles — clear it too
    // (operator finding #1, 2026-10-26).
    //
    // Both follow-ups are part of the change the operator asked for (README: a
    // category change on a loaded model frees the slot), so their outcome belongs
    // in the reply: the old `let _ =` and dropped tuple answered `"edited"` even
    // when the slot stayed occupied or the default-clear never reached the file
    // (R2-07-05). The 200 stays an acceptance of the yaml edit — it is honest about
    // what it did — and `null` marks a follow-up that was not attempted.
    let mut slot_freed: Option<bool> = None;
    let mut default_cleared: Option<bool> = None;
    let mut warnings: Vec<String> = Vec::new();
    if let Some(new_cat) = requested_category.filter(|c| *c != old_category) {
        slot_freed = Some(match state.scheduler.unload_model(&entry.name).await {
            Ok(_) => true,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    model = %entry.name,
                    "retagged model's slot was not freed"
                );
                warnings.push(format!(
                    "slot not freed for `{}`: it may keep serving its old listener \
                     until it is unloaded ({error})",
                    entry.name
                ));
                false
            }
        });
        let catalog = state.catalog().await?;
        if let Some(default) = state.default_model(old_category) {
            // Only an UNAMBIGUOUS resolution proves the category's default is the
            // model that just moved. An ambiguous one is refused here rather than
            // accepted on a guess: clearing the wrong default would rewrite the
            // operator's config file for a model that never moved.
            match crate::catalog::resolve(&default, &catalog) {
                crate::catalog::Resolution::Found(d) if d.name == entry.name => {
                    let (_effective, persisted) = state.set_default_model(old_category, None).await;
                    default_cleared = Some(persisted);
                    if persisted {
                        tracing::info!(
                            model = %entry.name,
                            from = %old_category,
                            to = %new_cat,
                            "cleared dangling category default"
                        );
                    } else {
                        tracing::warn!(
                            model = %entry.name,
                            from = %old_category,
                            to = %new_cat,
                            "dangling category default cleared in memory only; \
                             the config file still names the retagged model"
                        );
                        warnings.push(format!(
                            "the dangling {old_category} default was cleared in memory \
                             only; the config file still names the retagged `{}` as its \
                             default",
                            entry.name
                        ));
                    }
                }
                crate::catalog::Resolution::Ambiguous(clients) => tracing::warn!(
                    model = %entry.name,
                    category = %old_category,
                    entries = clients.len(),
                    "left the category default alone: its name is ambiguous in the catalog"
                ),
                crate::catalog::Resolution::Found(_) | crate::catalog::Resolution::None => {}
            }
        }
    }
    let catalog = state.catalog().await?;
    let fresh = catalog.iter().find(|m| m.name == entry.name);
    Ok(Json(json!({
        "status": "edited",
        "model": entry.name,
        "edits": edits.iter().map(|(key, value)| json!({
            "key": key,
            "value": value,
        })).collect::<Vec<_>>(),
        "category": fresh.map(|m| m.category),
        "cpu_offload": fresh.and_then(|m| m.raw.get("cpu_offload").cloned()),
        "slot_freed": slot_freed,
        "default_cleared": default_cleared,
        "warnings": warnings,
    })))
}

/// `POST /har/config/logging {"enabled": bool}` — the page's "Extended Logging"
/// switch: decision-level scheduler lines plus the full request ring. Runtime
/// only; every boot starts with it off.
///
/// The body is buffered through the same `Result<Json<Value>, JsonRejection>`
/// wall as every sibling POST (FINDINGS-05 [3]): a bare `Json<Value>` handed
/// axum's default `text/plain` rejection straight to the client, breaking the
/// documented `{code,message}` + OpenAI-envelope contract on this one route.
pub(super) async fn har_config_logging(
    State(_state): State<Arc<ApiState>>,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|rejection| ApiError::BadRequest {
        reason: format!("body must be JSON with `enabled`: {rejection}"),
    })?;
    let Some(enabled) = body.get("enabled").and_then(Value::as_bool) else {
        return Err(ApiError::BadRequest {
            reason: "enabled (boolean) is required".to_string(),
        });
    };
    crate::observability::set_extended_logging(enabled);
    Ok(Json(json!({ "status": "ok", "extended_logging": enabled })))
}
