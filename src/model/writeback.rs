//! Byte-preserving write-back of per-model yaml flags (DESIGN Step 9, choice B).
//!
//! The yaml file stays the single source of truth (the operator edits these files
//! by hand), so page edits are read-modify-write of the *raw lines*: comments,
//! ordering, CRLF endings, and unknown keys all survive. The file is replaced
//! atomically (temp + rename) and revalidated after write — the old bytes come
//! back on failure. Fixes v3.3 defects D3/D4 (non-atomic, CRLF-normalizing,
//! comment-clobbering write-back whose failure still answered 200).

use crate::model::scan::parse_flat_yaml;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Serializes every model-file rewrite process-wide. The harness is now a writer
/// too (install-time `measured_gb` persist races the operator-initiated flag
/// edits), and `with_extension` would also collapse `model.yaml` and `model.yml`
/// onto one temp name — a global lock plus a unique temp forecloses both the
/// lost-update race and the cross-file rename (review, 2026-10-26).
fn write_gate() -> &'static Mutex<()> {
    static GATE: OnceLock<Mutex<()>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(()))
}

/// Takes [`write_gate`] for the whole of a caller's read-modify-write, so another
/// file writer joins this module's critical section instead of racing it — the
/// config write-back in `api` is such a writer (FINDINGS-07 [3]). The gate is
/// exposed as a guard rather than as the `Mutex` so no caller can pick its own
/// poisoning policy: a poisoned gate is the residue of a failed write, never a
/// broken invariant, so it is taken as-is exactly as every internal caller does.
pub(crate) fn acquire_write_gate() -> MutexGuard<'static, ()> {
    write_gate()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Monotonic suffix so two temp files can never collide. Also used by the config
/// write-back, so every temp this process ever creates is unique process-wide.
pub(crate) fn next_temp_seq() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// One editable flag: the yaml key it maps to.
pub const EDITABLE_FLAGS: &[(&str, &str)] = &[("category", "category"), ("cpu", "cpu_offload")];

/// Sets `key: value` in a flat yaml document, preserving everything else.
///
/// - a matching key line (first one) is rewritten in place;
/// - a matching key inside a *commented block* is never touched (v3.3 clobbered
///   these) — only real, non-comment lines count as occurrences;
/// - a key that appears nowhere is appended at the end;
/// - `value: None` removes an existing key line rather than writing an empty one;
/// - the split is on `\n` only, so every `\r` of a CRLF file stays attached to its
///   line and re-emerges on join — endings never get normalized (the D4 fix).
///
/// BOM semantics: bytes the operator authored survive, except the edited line
/// itself. A file-leading UTF-8 BOM is kept when it rides a line the edit does
/// not target, and re-emitted when the rewrite lands on that first line (an
/// unrelated edit must not churn the operator's BOM); removing the first line
/// takes the BOM with it, which the scanner accepts either way.
///
/// # Returns
///
/// The new document text, or an error naming the problem.
pub fn set_key_in_text(text: &str, key: &str, value: Option<&str>) -> Result<String, String> {
    // Split on '\n' keeping every byte: each element keeps its own trailing '\r'.
    let body = text.strip_suffix('\n').unwrap_or(text);
    let mut out: Vec<String> = body.split('\n').map(str::to_string).collect();
    let bom = text.starts_with('\u{feff}');
    match (find_key_line(out.iter().map(String::as_str), key), value) {
        (Some(index), Some(value)) => {
            // CRLF files: the '\r' rides at the end of the line.
            let ending = if out[index].ends_with('\r') { "\r" } else { "" };
            // Line 1 is the only place a real file-leading BOM can ride; re-emit
            // it so overwriting that key does not strip the file's BOM.
            let lead = if index == 0 && bom { "\u{feff}" } else { "" };
            out[index] = format!("{lead}{key}: {value}{ending}");
        }
        (Some(index), None) => {
            // Drop exactly the key's own line — blanking-then-filtering would
            // also delete unrelated blank lines the operator authored (review,
            // 2026-10-26, byte-preservation promise).
            out.remove(index);
        }
        (None, Some(value)) => {
            // Match the file's dominant line ending so a CRLF file gets a CRLF line.
            let ending = if text.contains("\r\n") { "\r" } else { "" };
            out.push(format!("{key}: {value}{ending}"));
        }
        (None, None) => {
            return Err(format!("cannot remove `{key}`: the key is not in the file"));
        }
    }
    let mut joined = out.join("\n");
    if text.ends_with('\n') {
        joined.push('\n');
    }
    Ok(joined)
}

/// Index of the first real `key:` line — comments and blanks never count
/// (v3.3 clobbered commented-out blocks; the D3 fix).
///
/// The BOM strip mirrors the scan parser (`scan.rs`'s `parse_flat_yaml` trims
/// `\u{feff}` the same way): `str::trim` does not remove U+FEFF (it is category
/// Cf, not White_Space), so without this a BOM'd first line is invisible here
/// while scan happily reads its key. The asymmetry is what FINDINGS-13 [1]
/// reports: a batch retag reads the missing match as "absent key", silently
/// no-ops the `is_embedding_model` removal, and the next scan drops the whole
/// model with `EmbeddingCategoryConflict` — after the edit answered success.
fn find_key_line<'a>(mut lines: impl Iterator<Item = &'a str>, key: &str) -> Option<usize> {
    lines.position(|line| {
        let trimmed = line.trim_start_matches('\u{feff}').trim();
        !trimmed.starts_with('#')
            && !trimmed.is_empty()
            && trimmed
                .find(':')
                .is_some_and(|colon| trimmed[..colon].trim() == key)
    })
}

/// Applies [`set_key_in_text`] to the file on disk, atomically: write a temp file
/// next to the target, revalidate by parsing, then rename over the original.
///
/// # Errors
///
/// Read/parse/write failures as strings; the on-disk file is never left broken —
/// a failed re-parse restores the original bytes before returning.
pub fn set_flag_on_file(path: &Path, key: &str, value: Option<&str>) -> Result<(), String> {
    // One writer at a time, and a unique temp name (see `acquire_write_gate`).
    let _gate = acquire_write_gate();
    let original = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let updated = set_key_in_text(&original, key, value)?;
    commit_verified(path, &original, &updated)
}

/// Applies several edits in **one** atomic rewrite, for keys that must never be
/// observed half-applied: the measured range's high end, low end and offload stamp
/// are such a triple, because the scanner rejects an orphan or inverted pair and would
/// otherwise drop the whole model after a failure between two writes (review,
/// 2026-10-27). A category retag is the same shape: the non-EMB category write and
/// the `is_embedding_model` removal must land together, or the file between the two
/// writes is a scan-time conflict that drops the model (FINDINGS-12 [1]).
///
/// # Arguments
///
/// * `edits` - `(key, value)` pairs applied in order to the same buffer; a `None`
///   value removes that key's line. Removing an absent key is a no-op: a batch edit
///   states a desired end-shape, and a line that was never there is already in it.
///   (The single-key [`set_flag_on_file`] keeps removal strict — there the caller
///   named exactly one key and its absence is a mistake worth reporting.)
///
/// # Errors
///
/// As [`set_flag_on_file`]. Nothing reaches the disk unless every edit applied and the
/// result parsed, so the file keeps its previous complete state.
pub fn set_flags_on_file(path: &Path, edits: &[(&str, Option<&str>)]) -> Result<(), String> {
    let _gate = acquire_write_gate();
    let original = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut updated = original.clone();
    for (key, value) in edits {
        if value.is_none() && find_key_line(updated.split('\n'), key).is_none() {
            continue;
        }
        updated = set_key_in_text(&updated, key, *value)?;
    }
    commit_verified(path, &original, &updated)
}

/// Publishes `updated` over `path` only if it still parses; restores `original` if the
/// renamed file turns out unreadable. Called with [`write_gate`] already held.
fn commit_verified(path: &Path, original: &str, updated: &str) -> Result<(), String> {
    // Validate before touching the file: the result must still parse, and an edit
    // of an "identity" key must leave a parseable, non-empty map.
    let (map, problems) = parse_flat_yaml(updated);
    if !problems.is_empty() {
        return Err(format!(
            "edit would break the file: {}",
            problems.join("; ")
        ));
    }
    if map.is_empty() {
        return Err("edit would produce an empty model file".to_string());
    }
    let tmp = path.with_extension(format!(
        "yaml.{}.{}.sh.tmp",
        std::process::id(),
        next_temp_seq()
    ));
    std::fs::write(&tmp, updated.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", path.display()));
    }
    // Post-write revalidation: if the renamed file does not parse (fs hiccup),
    // restore the original bytes and fail loudly (D4's silent-200 fix).
    if std::fs::read_to_string(path)
        .map_err(|e| format!("post-write read failed: {e}"))
        .and_then(|text| {
            let (_, problems) = parse_flat_yaml(&text);
            if problems.is_empty() {
                Ok(())
            } else {
                Err(problems.join("; "))
            }
        })
        .is_err()
    {
        let _ = std::fs::write(path, original.as_bytes());
        return Err("post-write validation failed; original restored".to_string());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "# the operator's header comment\nname: qwen3\nengine: llama\ncategory: WORK\nmodel: F:/m.gguf\n";

    #[test]
    fn existing_key_is_rewritten_in_place() {
        let updated = set_key_in_text(SAMPLE, "category", Some("RP")).expect("edit");
        assert!(updated.contains("category: RP"));
        assert!(updated.starts_with("# the operator's header comment"));
        assert!(updated.contains("name: qwen3\nengine: llama\ncategory: RP\nmodel:"));
    }

    #[test]
    fn new_key_is_appended_and_crlf_survives() {
        let crlf = "name: qwen\r\ncategory: WORK\r\n";
        let updated = set_key_in_text(crlf, "cpu_offload", Some("true")).expect("edit");
        assert!(updated.contains("cpu_offload: true"));
        assert!(updated.ends_with('\n'));
        assert!(updated.contains("\r\n"), "CRLF endings must survive");
        assert!(updated.contains("name: qwen\r\n"));
    }

    #[test]
    fn commented_blocks_are_never_clobbered() {
        let text = "name: qwen\n# category: AUX\ncategory: WORK\n";
        let updated = set_key_in_text(text, "category", Some("RP")).expect("edit");
        assert!(updated.contains("# category: AUX"), "comment survives");
        assert!(updated.contains("category: RP"));
        assert_eq!(updated.matches("category:").count(), 2);
    }

    #[test]
    fn removal_deletes_the_line_and_fails_when_absent() {
        let updated = set_key_in_text(SAMPLE, "category", None).expect("edit");
        assert!(!updated.contains("category:"));
        assert!(set_key_in_text(SAMPLE, "cpu_offload", None).is_err());
    }

    #[test]
    fn removal_never_touches_unrelated_blank_lines() {
        // Byte-preservation promise (review, 2026-10-26): removing one key must
        // not delete the operator's paragraph breaks.
        let text = "name: qwen\n\ncategory: WORK\n\nengine: llama\n";
        let updated = set_key_in_text(text, "category", None).expect("edit");
        assert!(!updated.contains("category:"));
        assert_eq!(updated, "name: qwen\n\n\nengine: llama\n");
        assert!(
            updated.contains("name: qwen\n\n\nengine"),
            "blanks survive: {updated:?}"
        );
    }

    #[test]
    fn several_keys_land_in_one_rewrite_and_keep_the_operators_bytes() {
        // The measured triple must never be observable half-applied: an orphan or
        // inverted pair is hard-rejected by the scanner, which would drop the model.
        let dir = crate::testutil::temp_dir("writeback-batch");
        let path = dir.join("m.yaml");
        std::fs::write(
            &path,
            "# operator note\nname: m\nengine: llama\ncost: greedy\n",
        )
        .expect("seed");
        set_flags_on_file(
            &path,
            &[
                ("measured_gb", Some("20.0")),
                ("measured_gb_min", Some("10.0")),
                ("measured_offload", Some("false")),
            ],
        )
        .expect("batch edit");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.starts_with("# operator note\n"), "header lost: {text}");
        assert!(text.contains("cost: greedy"), "unrelated key lost: {text}");
        for key in [
            "measured_gb: 20.0",
            "measured_gb_min: 10.0",
            "measured_offload: false",
        ] {
            assert!(text.contains(key), "missing `{key}`: {text}");
        }
    }

    #[test]
    fn a_retag_rewrites_the_category_and_removes_the_embedding_alias_in_one_pass() {
        // FINDINGS-12 [1]: a non-EMB category left beside `is_embedding_model: true`
        // is a whole-file scan drop, so the two edits must share one rewrite and the
        // result must scan clean.
        let dir = crate::testutil::temp_dir("writeback-retag");
        let path = dir.join("emb.yaml");
        std::fs::write(
            &path,
            "# operator header\nname: emb\nengine: mock\nis_embedding_model: true\ncategory: EMB\nvram_gb: 2\n",
        )
        .expect("seed");
        set_flags_on_file(
            &path,
            &[("category", Some("WORK")), ("is_embedding_model", None)],
        )
        .expect("batch retag");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("category: WORK"), "{text}");
        assert_eq!(
            text.matches("category:").count(),
            1,
            "one category line: {text}"
        );
        assert!(
            !text.contains("is_embedding_model"),
            "the stale alias must not survive: {text}"
        );
        assert!(
            text.starts_with("# operator header\n"),
            "header lost: {text}"
        );
        assert!(text.contains("vram_gb: 2"), "unrelated key lost: {text}");
        let (map, problems) = parse_flat_yaml(&text);
        assert!(problems.is_empty(), "{problems:?}");
        let entry = crate::model::ModelEntry::from_flat(&map).expect("the retagged file must scan");
        assert_eq!(entry.category, crate::config::CategoryId::Work);
    }

    #[test]
    fn batch_removal_of_an_absent_key_is_a_no_op() {
        // The retag handler pushes the alias removal with every non-EMB category
        // write; a file that never carried the alias must still be edited, and
        // edited only where it was asked.
        let dir = crate::testutil::temp_dir("writeback-retag-absent");
        let path = dir.join("plain.yaml");
        std::fs::write(&path, "name: plain\ncategory: WORK\n").expect("seed");
        set_flags_on_file(
            &path,
            &[("category", Some("AUX")), ("is_embedding_model", None)],
        )
        .expect("no-op removal rides along");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "name: plain\ncategory: AUX\n"
        );
    }

    #[test]
    fn a_bom_prefixed_first_line_is_visible_to_batch_writeback() {
        // FINDINGS-13 [1]: scan.rs strips a UTF-8 BOM before matching a key, so
        // `find_key_line` must too. Without that symmetry the batch probe reads
        // the BOM'd `is_embedding_model` as "absent key", the removal no-ops, and
        // the retagged file keeps an alias+WORK pair the next scan hard-drops.
        let dir = crate::testutil::temp_dir("writeback-bom-retag");
        let path = dir.join("emb-bom.yaml");
        std::fs::write(
            &path,
            "\u{feff}is_embedding_model: true\nname: emb1\nengine: mock\ncategory: EMB\nvram_gb: 1\n",
        )
        .expect("seed");
        set_flags_on_file(
            &path,
            &[("category", Some("WORK")), ("is_embedding_model", None)],
        )
        .expect("batch retag of a BOM'd file");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !text.contains("is_embedding_model"),
            "the alias on the BOM'd first line must not survive: {text}"
        );
        assert!(text.contains("category: WORK"), "{text}");
        assert_eq!(
            text.matches("category:").count(),
            1,
            "the category was rewritten in place, never appended beside: {text}"
        );
        assert!(text.contains("name: emb1"), "unrelated key lost: {text}");
        // Removing line 1 takes the BOM with it — scan tolerates both shapes.
        assert!(
            !text.starts_with('\u{feff}'),
            "removal drops the BOM: {text:?}"
        );
        let (map, problems) = parse_flat_yaml(&text);
        assert!(problems.is_empty(), "{problems:?}");
        let entry = crate::model::ModelEntry::from_flat(&map).expect("must scan");
        assert_eq!(entry.category, crate::config::CategoryId::Work);
    }

    #[test]
    fn editing_a_later_key_keeps_a_bom_prefixed_name_line_in_place() {
        // Pre-fix, the BOM'd `name:` line matched no key and the set appended a
        // duplicate `engine:` — a file with no duplicate until the harness
        // created one, then refused by the commit gate. Post-fix, the BOM'd key
        // line is found, the later key is rewritten in place, and the first
        // line's bytes (BOM included) survive an edit that never targets them.
        let dir = crate::testutil::temp_dir("writeback-bom-later");
        let path = dir.join("name-bom.yaml");
        std::fs::write(&path, "\u{feff}name: emb1\nengine: mock\ncategory: EMB\n").expect("seed");
        set_flag_on_file(&path, "engine", Some("llama")).expect("edit a later key");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            text.starts_with("\u{feff}name: emb1\n"),
            "the file-leading BOM survives an unrelated edit: {text:?}"
        );
        assert!(text.contains("engine: llama"), "{text}");
        assert_eq!(
            text.matches("engine:").count(),
            1,
            "the BOM'd file's own engine line was rewritten, not appended beside: {text}"
        );
        assert_eq!(text.matches("name:").count(), 1, "{text}");
        let (map, problems) = parse_flat_yaml(&text);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(map.get("engine").map(String::as_str), Some("llama"));
    }

    #[test]
    fn rewriting_a_bom_prefixed_first_line_re_emits_the_bom() {
        // The overwrite case of the documented BOM semantics: the edit lands on
        // the BOM'd line itself, so the only line whose bytes change is the value
        // after the colon — the operator's BOM stays the file's first bytes.
        let updated = set_key_in_text(
            "\u{feff}category: EMB\nname: emb1\n",
            "category",
            Some("WORK"),
        )
        .expect("edit");
        assert!(
            updated.starts_with("\u{feff}category: WORK\n"),
            "BOM preserved through the overwrite: {updated:?}"
        );
        assert_eq!(updated, "\u{feff}category: WORK\nname: emb1\n");
    }

    #[test]
    fn set_flag_on_file_edits_and_is_atomic_in_effect() {
        let dir = crate::testutil::temp_dir("writeback");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("m.yaml");
        std::fs::write(&path, SAMPLE).expect("seed");

        set_flag_on_file(&path, "cpu_offload", Some("true")).expect("edit");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("cpu_offload: true"));
        assert!(text.starts_with("# the operator's header comment"));

        set_flag_on_file(&path, "cpu_offload", None).expect("remove");
        assert!(!std::fs::read_to_string(&path)
            .expect("read back")
            .contains("cpu_offload"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edit_that_would_break_the_file_is_refused_before_writing() {
        let dir = crate::testutil::temp_dir("writeback-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("m.yaml");
        std::fs::write(&path, "name: qwen\n").expect("seed");
        // Removing `name` leaves an empty map: refused, file untouched.
        assert!(set_flag_on_file(&path, "name", None).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).expect("intact"),
            "name: qwen\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
