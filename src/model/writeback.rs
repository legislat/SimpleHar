//! Byte-preserving write-back of per-model yaml flags (DESIGN Step 9, choice B).
//!
//! The yaml file stays the single source of truth (the operator edits these files
//! by hand), so page edits are read-modify-write of the *raw lines*: ordering,
//! CRLF endings, and unknown keys survive, and comments survive — every untouched
//! line always; an edited line passes through byte-verbatim on an identical-value
//! write (identical **as the scanner reads the value**, both sides of that test
//! normalized by scan's own rule — `split_value_comment` for the line's value
//! text, `canonical_value` for the incoming word), and a real change
//! drops the line's own inline comment (a stale note beside a new value is a lie;
//! operator ruling 2026-10-28, FINDINGS-13 [8] Fork B; FINDINGS-BATCH-2026-10-28B
//! [1]/[4]). An edit whose result equals the input never reaches the disk at all —
//! no temp, no rename, no mtime churn on a file nobody changed ([6]). The file is
//! replaced atomically (temp + rename) and revalidated after write — on failure
//! the old bytes are restored **and** kept forensically: a single-slot
//! `<stem>.yaml.bak` incident copy is written first, every claimed byte location
//! is confirmed by its own read-back, and the error string tells the truth about
//! where the original bytes ended up (restored, backed up, believed to be at the
//! copy, or nowhere). Fixes v3.3 defects D3/D4 (non-atomic, CRLF-normalizing,
//! comment-clobbering write-back whose failure still answered 200) and
//! FINDINGS-13 [5] (the restore was the one non-atomic write and its failure was
//! discarded behind an unconditional "original restored" claim).

use crate::model::scan::{canonical_value, normalized_line, parse_flat_yaml, split_value_comment};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

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
/// - a matching key line (first one) is rewritten in place only when the value
///   actually changes. Identity is decided **as the scanner reads values**: the
///   line's value text goes through [`split_value_comment`] and the incoming word
///   through [`canonical_value`] — the same normalization, the value half of that
///   helper (BOM/`\r`/whitespace trim + one quote pair) with no comment cut, which
///   is what keeps a word like `a # b` a word and not a line. So an
///   identical-value write passes the line through
///   **byte-verbatim** — BOM, indentation, raw quoted value, inline comment and
///   trailing `\r` all stay exactly as authored, and the whole document comes back
///   byte-identical (operator ruling 2026-10-28, FINDINGS-13 [8] Fork B: nothing
///   changes when the value does not change; FINDINGS-BATCH-2026-10-28B [1]/[4]
///   for why both sides must be normalized by the same helper). A real change
///   harness-formats the line and drops the old inline comment (a stale note
///   beside a new value is a lie), and every later rewrite of the new value is
///   then stable — the fixed point is the same one step further on;
/// - `value` is trimmed once at entry, so all three arms see one canonical word
///   (the harness writes single words: an enum name, `true`/`false`, a `{:.1}`
///   float). Surrounding whitespace is never a fact about a value, and comparing
///   a trimmed word while emitting a padded one was the drift FINDINGS-BATCH-2026-10-28B
///   [4] reported. Values are trusted **caller-produced single words**: a value
///   containing `\n` would inject further lines into the document (and one
///   containing ` #` meets the open triage item below), so the trust boundary of
///   this function is its callers — production passes no free-form text;
/// - open triage item, deliberately NOT fixed by this ruling: the change-side
///   emit writes the incoming value verbatim, so a quoted value loses its
///   quotes — setting a value containing ` #` through a real change lands a
///   line that rescans to the pre-`#` prefix;
/// - a matching key inside a *commented block* is never touched (v3.3 clobbered
///   these) — only real, non-comment lines count as occurrences;
/// - a key that appears nowhere is appended at the end;
/// - `value: None` removes an existing key line rather than writing an empty one
///   (the key's own comment rides the line away — the ruling is about surviving
///   values, not preserved corpses);
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
    // One canonical incoming word for all three arms (FINDINGS-BATCH-2026-10-28B
    // [4]): trimming here, not per arm, is what keeps the identity test below and
    // the bytes the emit arms write in agreement.
    let value = value.map(str::trim);
    match (find_key_line(out.iter().map(String::as_str), key), value) {
        (Some(index), Some(value)) => {
            // Ruling B's strongest form: an identical value changes NOTHING about the
            // line — it is never reformatted, so BOM, indentation, raw quoting, the
            // comment and the '\r' all stay exactly as authored. "Identical" is the
            // scanner's own reading: both sides go through scan's rule, so the writer
            // cannot disagree with the reader about where the value ends and no shape
            // can churn on a write of the value the reader just published
            // (FINDINGS-BATCH-2026-10-28B [1]/[4]). A real change harness-formats the
            // line, which is what drops the stale comment.
            let line = normalized_line(&out[index]);
            let colon = line.find(':').expect(
                "invariant: find_key_line matched this line only because it has a key: value colon",
            );
            let (old_value, _comment) = split_value_comment(&line[colon + 1..]);
            if old_value != canonical_value(value) {
                // CRLF files: the '\r' rides at the end of the line.
                let ending = if out[index].ends_with('\r') { "\r" } else { "" };
                // Line 1 is the only place a real file-leading BOM can ride; re-emit
                // it so overwriting that key does not strip the file's BOM.
                let lead = if index == 0 && bom { "\u{feff}" } else { "" };
                out[index] = format!("{lead}{key}: {value}{ending}");
            }
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
/// The line is read through [`normalized_line`], the same normalizer the scan
/// parser uses: `str::trim` does not remove U+FEFF (it is category Cf, not
/// White_Space), so without that strip a BOM'd first line is invisible here
/// while scan happily reads its key. The asymmetry is what FINDINGS-13 [1]
/// reports: a batch retag reads the missing match as "absent key", silently
/// no-ops the `is_embedding_model` removal, and the next scan drops the whole
/// model with `EmbeddingCategoryConflict` — after the edit answered success.
fn find_key_line<'a>(mut lines: impl Iterator<Item = &'a str>, key: &str) -> Option<usize> {
    lines.position(|line| {
        let trimmed = normalized_line(line);
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

/// Publishes `updated` over `path` only if it still parses; on post-write
/// validation failure restores `original` atomically and writes the single-slot
/// `.yaml.bak` incident copy first (FINDINGS-13 [5] Fork B), the error naming
/// both files truthfully — and only claiming a location its own read-back
/// confirmed (FINDINGS-BATCH-2026-10-28 [1]/[3]). Called with [`write_gate`]
/// already held.
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
    // True no-op: the edit produced the bytes that are already on disk, so there
    // is nothing to commit (FINDINGS-BATCH-2026-10-28B [6]). Deliberately *after*
    // the two checks above, so a no-op against a file that already has problem
    // lines or would parse empty still refuses exactly as before (13[4] Fork C
    // kept `commit_verified` blanket) — the short-circuit removes the temp write,
    // the rename and the mtime churn on a wedged drive, never a refusal.
    if updated == original {
        return Ok(());
    }
    let tmp = temp_beside(path);
    std::fs::write(&tmp, updated.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    if let Err(error) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("cannot replace {}: {error}", path.display()));
    }
    // Post-write revalidation: if the renamed file does not parse (fs hiccup),
    // restore the original bytes and fail loudly (D4's silent-200 fix).
    let read_back = std::fs::read_to_string(path);
    #[cfg(test)]
    let read_back = corrupt_if_armed(path, read_back);
    if read_back
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
        return Err(restore_after_failed_validation(path, original));
    }
    Ok(())
}

/// Unique temp path beside `path`, carrying the file's own final extension:
/// `<name>.<ext>.<pid>.<seq>.sh.tmp`. The pid+seq pair makes collisions
/// impossible process-wide (shared with the config writer's counter), and the
/// `.sh.tmp` ending keeps the temp outside the scan's `.yaml` acceptance gate —
/// a killed write can never leave a scannable file behind.
fn temp_beside(path: &Path) -> PathBuf {
    let extension = format!(
        "{}.{}.{}.sh.tmp",
        path.extension().and_then(|e| e.to_str()).unwrap_or("yaml"),
        std::process::id(),
        next_temp_seq()
    );
    path.with_extension(extension)
}

/// The post-write validation failed (unreadable or unparseable re-read): the
/// bytes just renamed cannot be trusted on disk. Order matters (Fork B): the
/// forensic copy is taken **before** the restore is attempted, then the restore
/// runs through the same temp+rename machinery as the main write, and the error
/// string names the outcome of both paths — the anti-lie fix for the old
/// unconditional "original restored" (FINDINGS-13 [5]). The copy location is
/// claimed as fact only when the copy's own read-back confirmed the bytes; an
/// unconfirmed copy degrades the message to a believed-to-be-there form
/// (FINDINGS-BATCH-2026-10-28 [3]).
fn restore_after_failed_validation(path: &Path, original: &str) -> String {
    let bak = forensic_copy(path, original);
    match restore_original_bytes(path, original) {
        Ok(()) => format!(
            "post-write validation failed; original restored ({})",
            match &bak {
                Bak::Verified(bak_path) => format!("backup: {}", bak_path.display()),
                Bak::Unverified {
                    path: bak_path,
                    fault,
                } => {
                    format!("backup unverified: {} ({fault})", bak_path.display())
                }
                Bak::Absent(io) => format!("no backup written: {io}"),
            }
        ),
        Err(io) => match &bak {
            Bak::Verified(bak_path) => format!(
                "post-write validation failed; RESTORE FAILED: {io}; original bytes are \
                 in {} \u{2014} re-check {}",
                bak_path.display(),
                path.display()
            ),
            Bak::Unverified {
                path: bak_path,
                fault,
            } => format!(
                "post-write validation failed; RESTORE FAILED: {io}; original bytes \
                 believed to be in {} (copy unconfirmed: {fault}) \u{2014} re-check {}",
                bak_path.display(),
                path.display()
            ),
            Bak::Absent(_) => format!(
                "post-write validation failed; RESTORE FAILED: {io}; original bytes are \
                 in nowhere \u{2014} re-check {}",
                path.display()
            ),
        },
    }
}

/// Outcome of the incident-copy attempt, so the honest report can distinguish
/// "bytes provably on disk at this path" from "bytes believed to be there" from
/// "no trustworthy copy at all" (FINDINGS-BATCH-2026-10-28 [3]).
enum Bak {
    /// Written, then read back equal to the intended bytes.
    Verified(PathBuf),
    /// Created, but its confirming re-read did not match (io failure or byte
    /// mismatch) — the path may hold the bytes; never claimed as fact.
    Unverified { path: PathBuf, fault: String },
    /// Never successfully written; no trustworthy copy exists.
    Absent(String),
}

/// The single-slot incident-copy path beside `target`: `<file-stem>.yaml.bak`.
/// Shared by the writer and the order-probe so neither can drift from the other.
fn forensic_bak_path(target: &Path) -> PathBuf {
    let stem = target
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    target.with_file_name(format!("{stem}.yaml.bak"))
}

/// Confirms a read-back: `read` (the result of re-reading `path` after writing
/// `expected` bytes there) must match byte-for-byte, or the returned error
/// names the `claim` and the path truthfully. One helper for the three places
/// that must never trust an unverified re-read (FINDINGS-BATCH-2026-10-28 [3]).
fn compare_read_back(
    read: Result<String, std::io::Error>,
    expected: &str,
    path: &Path,
    claim: &str,
) -> Result<(), String> {
    read.map_err(|e| format!("{claim} {}: {e}", path.display()))
        .and_then(|text| {
            if text == expected {
                Ok(())
            } else {
                Err(format!(
                    "{claim} {} differs from the bytes written",
                    path.display()
                ))
            }
        })
}

/// Writes the verbatim `original` bytes to the single-slot incident copy
/// `<file-stem>.yaml.bak` beside the target (overwritten per incident — the
/// operator default, unchallenged), under the already-held [`write_gate`]. If
/// `original` carries a `bak:` node, that node is **rewritten** in the copy to a
/// one-line incident stamp naming the source file and the UTC time ("rewrite the
/// node in case of making .bak from .yaml with this node already present"); it
/// is never appended when absent — the `bak:` node is forensic-manual-only. Every
/// other byte of the copy is the verbatim original, and the copy's `.yaml.bak`
/// name is inert to the scan by the Fork B acceptance gate. The copy is trusted
/// only after its own read-back matches: a failed write is [`Bak::Absent`]
/// (warned, reflected as "no backup written"), a created-but-unconfirmed copy is
/// [`Bak::Unverified`] (warned, reflected as a believed-to-be-there form) — the
/// bytes' location must be truthful, never silent, never claimed on faith.
fn forensic_copy(target: &Path, original: &str) -> Bak {
    let bak = forensic_bak_path(target);
    let text = if find_key_line(original.split('\n'), "bak").is_some() {
        let source = target
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| target.display().to_string());
        let stamp = format!(
            "incident copy of {source} at {} \u{2014} post-write validation failed",
            utc_stamp()
        );
        set_key_in_text(original, "bak", Some(&stamp)).expect(
            "invariant: the node was just found by find_key_line, and set_key_in_text \
             only fails when a removal targets an absent key \u{2014} a set on a present \
             key cannot fail",
        )
    } else {
        original.to_string()
    };
    if let Err(io) = std::fs::write(&bak, text.as_bytes()) {
        tracing::warn!(
            error = %io,
            target = %target.display(),
            backup = %bak.display(),
            "cannot write the post-write incident copy"
        );
        return Bak::Absent(io.to_string());
    }
    let read_back = std::fs::read_to_string(&bak);
    #[cfg(test)]
    let read_back = corrupt_bak_if_armed(target, read_back);
    match compare_read_back(read_back, &text, &bak, "backup read-back") {
        Ok(()) => Bak::Verified(bak),
        Err(fault) => {
            tracing::warn!(
                fault = %fault,
                target = %target.display(),
                backup = %bak.display(),
                "the post-write incident copy could not be confirmed by read-back"
            );
            Bak::Unverified { path: bak, fault }
        }
    }
}

/// Puts `original` back over `path` atomically — unique temp, read-back verify
/// of the temp, one rename, then a post-rename re-read of the target: the
/// outcome is binary (old bytes or unrestored bytes, never half) and `Ok` is
/// returned only with the target's own bytes confirmed, symmetric with the main
/// write's post-rename revalidation (FINDINGS-BATCH-2026-10-28 [3]). Errors
/// carry the io detail for the honest message.
fn restore_original_bytes(path: &Path, original: &str) -> Result<(), String> {
    #[cfg(test)]
    restore_probe()
        .lock()
        .expect("restore probe lock")
        .push((path.to_path_buf(), forensic_bak_path(path).exists()));
    let tmp = temp_beside(path);
    std::fs::write(&tmp, original.as_bytes())
        .map_err(|e| format!("write {}: {e}", tmp.display()))?;
    let temp_read = std::fs::read_to_string(&tmp);
    if let Err(error) = compare_read_back(temp_read, original, &tmp, "temp read-back") {
        let _ = std::fs::remove_file(&tmp);
        return Err(error);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename -> {}: {e}", path.display())
    })?;
    let target_read = std::fs::read_to_string(path);
    #[cfg(test)]
    let target_read = corrupt_target_if_armed(path, target_read);
    compare_read_back(target_read, original, path, "restore read-back")
}

/// Seconds since the Unix epoch as ISO-8601 UTC (`2026-10-28T08:50:42Z`), no
/// date crate: `civil_from_days` below is Howard Hinnant's algorithm, which
/// shifts the epoch to a 0000-03-01-based 400-year era so the leap day lands at
/// the end of each year and month/day fall out by pure integer arithmetic.
fn utc_stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default();
    let days = (secs / 86_400) as i64;
    let rest = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3_600,
        (rest % 3_600) / 60,
        rest % 60
    )
}

/// (year, month, day) for a day number counted from 1970-01-01.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468; // shift epoch from 1970-01-01 to 0000-03-01
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // day of era [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // year of era [0, 399]
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // day of year [0, 365], March-based
    let mp = (5 * doy + 2) / 153; // month index [0, 11], March = 0
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 {
        (mp + 3) as u32
    } else {
        (mp - 9) as u32
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Test-only post-write corruption seam (FINDINGS-13 [5] Fork B), in the style of
/// scan's `arm_scan_pause`: armed for exactly one target path; the next
/// `commit_verified` post-rename re-read of **that** file has only its in-memory
/// read-back corrupted (a junk line `parse_flat_yaml` must report), so the
/// validation fails deterministically while the on-disk bytes stay honest and the
/// restore branch runs for real. With `lock_target` (Windows) the seam also opens
/// a delete-blocking handle (`share_mode(0x3)`) on the target at that moment —
/// after the main rename succeeded, before the restore's rename — reproducing a
/// wedged drive exactly where the restore can fail. One-shot: the arm disarms
/// itself on fire. Production never arms it: one uncontended mutex peek is the
/// entire cost.
#[cfg(test)]
struct CorruptArm {
    path: PathBuf,
    lock_target: bool,
}

#[cfg(test)]
fn post_write_corrupt() -> &'static Mutex<Vec<CorruptArm>> {
    static ARMS: OnceLock<Mutex<Vec<CorruptArm>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(all(test, windows))]
fn post_write_locks() -> &'static Mutex<Vec<std::fs::File>> {
    static LOCKS: OnceLock<Mutex<Vec<std::fs::File>>> = OnceLock::new();
    LOCKS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Arms the seam for `path`; the next post-write validation of exactly that file
/// fails. `lock_target` additionally refuses the restore's rename (Windows).
/// Arms are per-path and one-shot each, so concurrently running tests never
/// steal one another's seam.
#[cfg(test)]
pub(crate) fn arm_post_write_corrupt(path: &Path, lock_target: bool) {
    post_write_corrupt()
        .lock()
        .expect("corrupt seam lock")
        .push(CorruptArm {
            path: path.to_path_buf(),
            lock_target,
        });
}

/// Drops every `lock_target` handle the seam grabbed, so the temp dir frees.
#[cfg(all(test, windows))]
pub(crate) fn release_post_write_locks() {
    post_write_locks().lock().expect("lock vec").clear();
}

#[cfg(all(test, windows))]
fn lock_without_delete_share(path: &Path) {
    use std::os::windows::fs::OpenOptionsExt;
    match std::fs::OpenOptions::new()
        .read(true)
        .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE: DELETE refused
        .open(path)
    {
        Ok(handle) => post_write_locks().lock().expect("lock vec").push(handle),
        Err(error) => panic!("the seam could not lock {path:?}: {error}"),
    }
}

#[cfg(test)]
fn corrupt_if_armed(
    path: &Path,
    read_back: Result<String, std::io::Error>,
) -> Result<String, std::io::Error> {
    let armed = {
        let mut guard = post_write_corrupt().lock().expect("corrupt seam lock");
        guard
            .iter()
            .position(|arm| arm.path == path)
            .map(|index| guard.swap_remove(index))
    };
    match (armed, read_back) {
        (Some(arm), Ok(text)) => {
            #[cfg(windows)]
            if arm.lock_target {
                lock_without_delete_share(path);
            }
            Ok(format!("{text}\njust text corrupt seam\n"))
        }
        (_, other) => other,
    }
}

/// One-shot read-back mismatch arms, keyed by the **target** path whose incident
/// copy (`.bak` store) or restored body (`.target` store) must fail its
/// confirming re-read (FINDINGS-BATCH-2026-10-28 [3]). The on-disk bytes stay
/// honest — only the in-memory confirmation is disturbed, in the style of the
/// corrupt seam above.
#[cfg(test)]
fn bak_mismatches() -> &'static Mutex<Vec<PathBuf>> {
    static ARMS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
fn target_mismatches() -> &'static Mutex<Vec<PathBuf>> {
    static ARMS: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    ARMS.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(test)]
fn take_mismatch(store: &Mutex<Vec<PathBuf>>, path: &Path) -> bool {
    let mut guard = store.lock().expect("read-back mismatch arms");
    match guard.iter().position(|armed| armed == path) {
        Some(index) => {
            guard.swap_remove(index);
            true
        }
        None => false,
    }
}

/// Arms the incident-copy read-back to mismatch for the next `forensic_copy` of
/// exactly `path` (per-path one-shot): the write succeeds on disk, the
/// confirming re-read comes back altered, driving the [`Bak::Unverified`]
/// message shapes.
#[cfg(test)]
pub(crate) fn arm_bak_readback_mismatch(path: &Path) {
    bak_mismatches()
        .lock()
        .expect("read-back mismatch arms")
        .push(path.to_path_buf());
}

/// Arms the restore's post-rename target read-back to mismatch for the next
/// `restore_original_bytes` of exactly `path` (per-path one-shot): the rename
/// succeeds, the confirming re-read comes back altered, so "original restored"
/// must not be claimed.
#[cfg(test)]
pub(crate) fn arm_target_readback_mismatch(path: &Path) {
    target_mismatches()
        .lock()
        .expect("read-back mismatch arms")
        .push(path.to_path_buf());
}

#[cfg(test)]
fn corrupt_bak_if_armed(
    target: &Path,
    read_back: Result<String, std::io::Error>,
) -> Result<String, std::io::Error> {
    if take_mismatch(bak_mismatches(), target) {
        if let Ok(text) = read_back {
            return Ok(format!("{text}\njust text corrupt bak seam\n"));
        }
    }
    read_back
}

#[cfg(test)]
fn corrupt_target_if_armed(
    path: &Path,
    read_back: Result<String, std::io::Error>,
) -> Result<String, std::io::Error> {
    if take_mismatch(target_mismatches(), path) {
        if let Ok(text) = read_back {
            return Ok(format!("{text}\njust text corrupt restore seam\n"));
        }
    }
    read_back
}

/// Order probe for the restore (FINDINGS-BATCH-2026-10-28 [1]): `restore_original_bytes`
/// records, at entry, whether the incident copy already existed on disk. The
/// Fork B promise — copy **before** restore — is only real if the restore can
/// observe the copy; a reordering makes this read `Some(false)`.
#[cfg(test)]
fn restore_probe() -> &'static Mutex<Vec<(PathBuf, bool)>> {
    static PROBES: OnceLock<Mutex<Vec<(PathBuf, bool)>>> = OnceLock::new();
    PROBES.get_or_init(|| Mutex::new(Vec::new()))
}

/// What the probe saw at the last entry of `restore_original_bytes` for `path`:
/// `Some(true)` = the `.bak` was on disk before the restore began; `Some(false)`
/// = the order regressed; `None` = no restore ran for that path.
#[cfg(test)]
pub(crate) fn restore_entered_with_backup(path: &Path) -> Option<bool> {
    restore_probe()
        .lock()
        .expect("restore probe lock")
        .iter()
        .rev()
        .find(|(armed, _)| armed == path)
        .map(|(_, seen)| *seen)
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
        let entry =
            crate::model::ModelEntry::from_flat(&map, "emb").expect("the retagged file must scan");
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
        let entry = crate::model::ModelEntry::from_flat(&map, "emb1").expect("must scan");
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

    /// Operator ruling 2026-10-28 (FINDINGS-13 [8] Fork B), drop direction: a
    /// value-changing write must not leave the old note lying beside the new
    /// value. Passes at HEAD too — this is a policy pin, declared as such: it
    /// stops any future "improvement" from silently restoring the stale-comment
    /// side of the ruling.
    #[test]
    fn a_changed_value_drops_the_edited_keys_inline_comment() {
        let updated = set_key_in_text(
            "cpu_offload: false # tested -ngl 0, thrashes at 24k ctx\n",
            "cpu_offload",
            Some("true"),
        )
        .expect("edit");
        assert_eq!(
            updated, "cpu_offload: true\n",
            "the note described the old value and is gone: {updated}"
        );
    }

    /// The ruling's keep direction — the falsifier against pre-ruling code (the
    /// always-drop overwrite arm fails this): an identical-value write preserves
    /// the comment byte-verbatim, including its inner `#` and the spacing from
    /// the cut, and the no-op write is a fixed point.
    #[test]
    fn an_unchanged_value_keeps_the_edited_keys_inline_comment_verbatim() {
        let line = "cpu_offload: false # tested -ngl 0, thrashes at 24k ctx # twice\n";
        let updated = set_key_in_text(line, "cpu_offload", Some("false")).expect("no-op write");
        assert_eq!(
            updated, line,
            "a note beside the value it describes survives: {updated}"
        );
        assert_eq!(
            set_key_in_text(&updated, "cpu_offload", Some("false")).expect("again"),
            line,
            "the identical write is a fixed point"
        );
    }

    /// The boundary is scan's own rule, not a hand-copied parser: a `#` inside
    /// quotes is VALUE, not comment. An identical (post-unquote) write passes
    /// the line through byte-verbatim — and the line still RESCANS to the quoted
    /// value, not its pre-`#` prefix; a different value drops the comment. A
    /// writeback-local split at the first `#` would compute the old value as
    /// `"a` and fail the keep direction here. Falsifies any keep-side reformat:
    /// reconstructing `{key}: {value}` from the parts strips the protective
    /// quotes and corrupts the value on the next scan.
    #[test]
    fn a_hash_inside_quotes_is_value_so_only_an_identical_write_keeps_the_tail() {
        let line = "title: \"a # b\" # the tail\n";
        let kept = set_key_in_text(line, "title", Some("a # b")).expect("identical write");
        assert_eq!(
            kept, line,
            "a no-op write of a quoted value must be byte-verbatim, quotes intact"
        );
        let (map, problems) = parse_flat_yaml(&kept);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            map.get("title").map(String::as_str),
            Some("a # b"),
            "the rescan must yield the quoted value, not the pre-# prefix: {kept}"
        );
        assert_eq!(
            set_key_in_text(line, "title", Some("a # c")).expect("changed write"),
            "title: a # c\n",
            "a real change drops the tail"
        );
    }

    /// The keep branch's byte-verbatim promise is strictly stronger than
    /// reassembling the parts: indentation, raw quoting and value-to-comment
    /// spacing all survive untouched on a no-op write.
    #[test]
    fn an_indented_quoted_annotated_line_passes_through_a_no_op_write_byte_identical() {
        let doc = "  title:  \"a # b\"   # operator spacing\nname: q\n";
        assert_eq!(
            set_key_in_text(doc, "title", Some("a # b")).expect("no-op"),
            doc,
            "a no-op write may not touch a single byte of the line"
        );
    }

    /// The rule composes with CRLF: a no-op write keeps tail and `\r`; a
    /// value-changing write drops the tail and keeps the `\r` form intact.
    #[test]
    fn the_comment_rule_composes_with_crlf_endings() {
        let doc = "batch: 512 # tuned on 2026-10-01\r\nname: q\r\n";
        assert_eq!(
            set_key_in_text(doc, "batch", Some("512")).expect("no-op"),
            doc,
            "no-op write keeps the tail and the '\\r'"
        );
        assert_eq!(
            set_key_in_text(doc, "batch", Some("256")).expect("change"),
            "batch: 256\r\nname: q\r\n",
            "the change drops the tail and keeps the CRLF form"
        );
    }

    /// The rule composes with BOM re-emission: a no-op write to the BOM'd first
    /// line keeps the BOM and the comment together.
    #[test]
    fn an_unchanged_value_on_a_bom_prefixed_first_line_keeps_bom_and_comment() {
        assert_eq!(
            set_key_in_text(
                "\u{feff}category: WORK # the operator's note\nname: emb1\n",
                "category",
                Some("WORK"),
            )
            .expect("no-op"),
            "\u{feff}category: WORK # the operator's note\nname: emb1\n",
            "BOM re-emit and tail preservation compose"
        );
    }

    /// The removal arm is unchanged by the ruling: deleting the key's line still
    /// takes the key's own comment down with it.
    #[test]
    fn removal_still_takes_the_edited_keys_comment_with_the_line() {
        let updated = set_key_in_text("name: q\nfit: true # keep the reasoning\n", "fit", None)
            .expect("remove");
        assert_eq!(
            updated, "name: q\n",
            "the comment rode its line away: {updated}"
        );
    }

    /// The batch path inherits the rule through the same helper: re-persisting an
    /// unchanged measured value keeps the operator's note; a real change drops it.
    #[test]
    fn the_batch_path_inherits_the_comment_rule() {
        let dir = crate::testutil::temp_dir("writeback-comment-batch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("m.yaml");
        std::fs::write(
            &path,
            "name: m\nengine: mock\nmeasured_gb: 20.0 # peak of the session\n",
        )
        .expect("seed");
        set_flags_on_file(&path, &[("measured_gb", Some("20.0"))]).expect("no-op batch");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read back"),
            "name: m\nengine: mock\nmeasured_gb: 20.0 # peak of the session\n",
            "the batch path keeps the tail through the identical helper"
        );
        set_flags_on_file(&path, &[("measured_gb", Some("21.0"))]).expect("changed batch");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(
            !text.contains("peak of the session"),
            "the batch change drops the stale note: {text}"
        );
        assert!(text.contains("measured_gb: 21.0"), "{text}");
        // A mixed batch (one identical edit + one real edit) is still ONE rewrite:
        // the identical key's note survives next to the changed key's new value.
        std::fs::write(
            &path,
            "name: m\nengine: mock\nmeasured_gb: 21.0\nmeasured_min_gb: 19.0 # floor note\n",
        )
        .expect("reseed");
        set_flags_on_file(
            &path,
            &[
                ("measured_gb", Some("22.0")),
                ("measured_min_gb", Some("19.0")),
            ],
        )
        .expect("mixed batch");
        let text = std::fs::read_to_string(&path).expect("read back");
        assert!(text.contains("measured_gb: 22.0"), "{text}");
        assert!(
            text.contains("measured_min_gb: 19.0 # floor note"),
            "the no-op member of the batch kept its note: {text}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS-BATCH-2026-10-28B [2] — THE reader/writer agreement pin, through
    /// the product path rather than a helper auditing itself. For every torture
    /// shape, in both LF and CRLF form: (a-b) the scan reader publishes exactly the
    /// pinned value, (c) writing that published value back leaves the document
    /// byte-identical, (d) the document still rescans to the same value, and the
    /// negative direction: a real change harness-formats the line, drops the tail
    /// and rescans to the new value. A writer-local comment parser cannot survive
    /// this — that is the acceptance criterion the
    /// `strip_comment == split_value_comment` self-comparison it replaces could
    /// never meet.
    #[test]
    fn the_torture_shapes_round_trip_reader_to_writer_and_back() {
        for &(raw, expected) in crate::model::scan::TORTURE_SHAPES {
            for ending in ["\n", "\r\n"] {
                let doc = format!("k:{raw}{ending}name: q{ending}");
                let (map, problems) = parse_flat_yaml(&doc);
                assert!(problems.is_empty(), "reader reports {doc:?}: {problems:?}");
                assert_eq!(
                    map.get("k").map(String::as_str),
                    Some(expected),
                    "reader's published value for {raw:?}"
                );
                let back = set_key_in_text(&doc, "k", Some(expected)).expect("identical write");
                assert_eq!(back, doc, "an identical-value write churned {raw:?}");
                let (again, problems) = parse_flat_yaml(&back);
                assert!(problems.is_empty(), "rescan of {back:?}: {problems:?}");
                assert_eq!(
                    again.get("k").map(String::as_str),
                    Some(expected),
                    "the no-op must not change what the reader sees for {raw:?}"
                );
                let changed = set_key_in_text(&doc, "k", Some("zzz-other")).expect("change");
                let want = format!("k: zzz-other{ending}name: q{ending}");
                assert_eq!(
                    changed, want,
                    "a real change harness-formats and drops the tail for {raw:?}"
                );
                let (map2, problems2) = parse_flat_yaml(&changed);
                assert!(problems2.is_empty(), "{problems2:?}");
                assert_eq!(map2.get("k").map(String::as_str), Some("zzz-other"));
            }
        }
    }

    /// FINDINGS-BATCH-2026-10-28B [4]: one canonical incoming word, decided before
    /// any arm runs. Padded input is identity-equal to the clean word (so the line
    /// passes through byte-verbatim) and lands trimmed on the change and append
    /// arms — never raw, which is what let the compare and the emit disagree. A
    /// quoting-only difference is parsed-equal in BOTH directions, so the
    /// operator's note survives it: the ruling's "identical" is the scanner's.
    #[test]
    fn incoming_words_are_normalized_once_before_any_arm_decides() {
        let doc = "k: 4096 # the note\nname: q\n";
        assert_eq!(
            set_key_in_text(doc, "k", Some(" 4096 ")).expect("padded identical"),
            doc,
            "a padded identical word must not touch a byte"
        );
        assert_eq!(
            set_key_in_text(doc, "k", Some("\t4096\n")).expect("padded edges"),
            doc
        );
        assert_eq!(
            set_key_in_text(doc, "k", Some(" 8192 ")).expect("padded change"),
            "k: 8192\nname: q\n",
            "padded bytes never land"
        );
        assert_eq!(
            set_key_in_text("name: q\n", "k", Some(" 8192  ")).expect("padded append"),
            "name: q\nk: 8192\n",
            "no padded word in the file"
        );
        let quoted_line = "k: \"a b\" # the note\n";
        assert_eq!(
            set_key_in_text(quoted_line, "k", Some("a b")).expect("unquoted in"),
            quoted_line,
            "file quoted, caller plain: parsed-equal keeps the note"
        );
        let plain_line = "k: a b # the note\n";
        assert_eq!(
            set_key_in_text(plain_line, "k", Some("\"a b\"")).expect("quoted in"),
            plain_line,
            "file plain, caller quoted: parsed-equal keeps the note"
        );
    }

    /// FINDINGS-BATCH-2026-10-28B [6]: a true no-op never reaches the disk. The
    /// honest falsifier is a target the rename cannot touch — a delete-blocking
    /// handle, the same `share_mode(0x3)` trick the restore seam uses: the no-op
    /// must SUCCEED, leave the bytes and the directory alone (no temp, no `.bak`),
    /// while a real edit on that very same locked target must still be refused in
    /// the rename. Removing the short-circuit turns the first assert red.
    #[cfg(windows)]
    #[test]
    fn a_true_no_op_edit_never_reaches_the_disk() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = crate::testutil::temp_dir("writeback-noop-no-disk");
        let path = seed(&dir, CLEAN);
        let _lock = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0x3) // FILE_SHARE_READ | FILE_SHARE_WRITE: DELETE refused
            .open(&path)
            .expect("lock the target against rename");
        set_flag_on_file(&path, "category", Some("WORK")).expect("a no-op needs no rename");
        assert_eq!(
            std::fs::read_to_string(&path).expect("target"),
            CLEAN,
            "the no-op left the bytes alone"
        );
        let entries: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            vec!["qwen3.yaml".to_string()],
            "a no-op writes no temp and no incident copy: {entries:?}"
        );
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("a real edit on the locked target must still be refused");
        assert!(error.contains("cannot replace"), "{error}");
        drop(_lock);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A valid model document whose scan stays error-free (mock engine: the
    /// `model:` existence gate is exempt, no stray reports).
    const CLEAN: &str =
        "# operator header\nname: qwen3\nengine: mock\ncategory: WORK\nvram_gb: 2\n";

    fn seed(dir: &std::path::Path, text: &str) -> std::path::PathBuf {
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir).expect("temp dir");
        let path = dir.join("qwen3.yaml");
        std::fs::write(&path, text).expect("seed");
        path
    }

    /// Fork B restore branch, success path: the armed corrupt seam fails the
    /// post-write validation; the error is the honest success variant naming the
    /// `.bak`, the incident copy holds the original bytes, and the target reads
    /// back the original. The copy is inert to the scan by half A's own filter.
    #[test]
    fn a_failed_post_write_validation_restores_and_names_the_incident_backup() {
        let dir = crate::testutil::temp_dir("writeback-restore");
        let path = seed(&dir, CLEAN);
        arm_post_write_corrupt(&path, false);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the seam must fail post-write validation");
        assert!(
            error.starts_with("post-write validation failed; original restored (backup: "),
            "{error}"
        );
        assert!(error.contains("qwen3.yaml.bak"), "{error}");
        let bak_text = std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy");
        assert_eq!(bak_text, CLEAN, "the copy is the verbatim original");
        assert!(
            !bak_text.contains("bak:"),
            "no node appended when the source has none (forensic manual only): {bak_text}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("target"),
            CLEAN,
            "the original came back"
        );
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".sh.tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temp survived: {leftovers:?}");
        let catalog = crate::model::scan::scan_dir(&dir);
        assert_eq!(catalog.models.len(), 1, "{:?}", catalog.errors);
        assert_eq!(catalog.models[0].name, "qwen3");
        assert!(
            catalog.errors.is_empty(),
            "the incident copy is inert by the acceptance gate: {:?}",
            catalog.errors
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Operator ruling: "Rewrite the node in case of making .bak from .yaml with
    /// this node already present." The copy carries exactly ONE `bak:` line — the
    /// incident stamp naming the source file and UTC time — every other line is
    /// byte-verbatim, and the target's operator node is never altered by us.
    #[test]
    fn a_source_bak_node_is_rewritten_in_the_incident_copy_never_appended() {
        const WITH_NODE: &str = "# header\nname: qwen3\nengine: mock\n\
                                 bak: \"old snapshot # keep\" # manual\nvram_gb: 2\n";
        let dir = crate::testutil::temp_dir("writeback-restore-node");
        let path = seed(&dir, WITH_NODE);
        arm_post_write_corrupt(&path, false);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the seam must fail post-write validation");
        assert!(error.contains("qwen3.yaml.bak"), "{error}");
        let bak_text = std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy");
        let bak_lines: Vec<&str> = bak_text
            .lines()
            .filter(|line| line.trim_start().starts_with("bak:"))
            .collect();
        assert_eq!(
            bak_lines.len(),
            1,
            "single-slot, rewritten in place:\n{bak_text}"
        );
        assert!(
            bak_lines[0].starts_with("bak: incident copy of qwen3.yaml at "),
            "{}",
            bak_lines[0]
        );
        assert!(
            bak_lines[0].contains("post-write validation failed"),
            "{}",
            bak_lines[0]
        );
        let stamp = bak_lines[0]
            .split(" at ")
            .nth(1)
            .and_then(|tail| tail.split(" \u{2014} ").next())
            .expect("the stamp sits between ' at ' and the em-dash clause");
        assert_eq!(stamp.len(), "2026-10-28T08:50:42Z".len(), "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.as_bytes()[10], b'T', "{stamp}");
        let original_lines: Vec<&str> = WITH_NODE.split('\n').collect();
        let copied_lines: Vec<&str> = bak_text.split('\n').collect();
        assert_eq!(copied_lines.len(), original_lines.len(), "{bak_text}");
        for (index, (original, copied)) in original_lines.iter().zip(&copied_lines).enumerate() {
            if original.starts_with("bak:") {
                assert!(
                    copied.starts_with("bak: incident copy of"),
                    "line {index} must be the rewritten node: {copied}"
                );
            } else {
                assert_eq!(copied, original, "line {index} must stay verbatim");
            }
        }
        assert_eq!(
            std::fs::read_to_string(&path).expect("target"),
            WITH_NODE,
            "the restore puts back the operator's own bytes, node included"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fork B restore branch, failure path: the seam wedges the target (no
    /// DELETE share) exactly between the validation and the restore rename. The
    /// message is the RESTORE FAILED variant, names the bak path, and — the
    /// anti-lie pin — never claims "original restored" when nothing was restored.
    #[cfg(windows)]
    #[test]
    fn a_refused_restore_reports_restored_failed_and_never_claims_restored() {
        let dir = crate::testutil::temp_dir("writeback-restore-fail");
        let path = seed(&dir, CLEAN);
        arm_post_write_corrupt(&path, true);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the restore rename must be refused");
        assert!(
            error.starts_with("post-write validation failed; RESTORE FAILED: "),
            "{error}"
        );
        assert!(error.contains("original bytes are in"), "{error}");
        assert!(error.contains("qwen3.yaml.bak"), "{error}");
        assert!(error.contains("re-check"), "{error}");
        assert!(
            !error.contains("original restored"),
            "anti-lie: the unconditional claim is unreachable here: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy"),
            CLEAN,
            "the forensic copy is the only place the original bytes live"
        );
        assert_eq!(
            restore_entered_with_backup(&path),
            Some(true),
            "order pin (FINDINGS-BATCH-2026-10-28 [1]): the restore may begin only \
             once the incident copy is on disk"
        );
        let target = std::fs::read_to_string(&path).expect("target");
        assert!(
            target.contains("cpu_offload: true"),
            "restore failed => the rejected edit is what remains on disk: {target}"
        );
        release_post_write_locks();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS-BATCH-2026-10-28 [3]: a `.bak` whose confirming re-read does not
    /// match is reported as unverified — the message never uses the fact-claiming
    /// `(backup: …)` form — while the (verified) restore itself completes honestly.
    #[test]
    fn an_unverified_incident_copy_is_reported_unverified_and_never_as_a_backup() {
        let dir = crate::testutil::temp_dir("writeback-bak-unverified");
        let path = seed(&dir, CLEAN);
        arm_post_write_corrupt(&path, false);
        arm_bak_readback_mismatch(&path);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the seam must fail post-write validation");
        assert!(
            error.starts_with(
                "post-write validation failed; original restored (backup unverified: "
            ),
            "{error}"
        );
        assert!(error.contains("differs from the bytes written"), "{error}");
        assert!(
            !error.contains("(backup:"),
            "the fact-claim is unreachable for an unconfirmed copy: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy"),
            CLEAN,
            "the seam disturbs only the confirming re-read; disk stays honest"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("target"),
            CLEAN,
            "the original came back"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS-BATCH-2026-10-28 [3]: unconfirmed copy + refused restore — the
    /// message believes, never asserts: no `original bytes are in`, no
    /// `original restored`, and the wedged target keeps the rejected edit.
    #[cfg(windows)]
    #[test]
    fn an_unconfirmed_incident_copy_with_a_refused_restore_believes_and_never_asserts() {
        let dir = crate::testutil::temp_dir("writeback-bak-believed");
        let path = seed(&dir, CLEAN);
        arm_post_write_corrupt(&path, true);
        arm_bak_readback_mismatch(&path);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the restore rename must be refused");
        assert!(
            error.starts_with("post-write validation failed; RESTORE FAILED: "),
            "{error}"
        );
        assert!(error.contains("believed to be in"), "{error}");
        assert!(error.contains("qwen3.yaml.bak"), "{error}");
        assert!(error.contains("re-check"), "{error}");
        assert!(
            !error.contains("original bytes are in"),
            "no fact-claim for an unconfirmed copy: {error}"
        );
        assert!(!error.contains("original restored"), "anti-lie: {error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy"),
            CLEAN,
            "the copy exists on disk; only the confirming read was disturbed"
        );
        let target = std::fs::read_to_string(&path).expect("target");
        assert!(
            target.contains("cpu_offload: true"),
            "restore failed => the rejected edit is what remains on disk: {target}"
        );
        release_post_write_locks();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FINDINGS-BATCH-2026-10-28 [3]: the restore's rename lands but its
    /// post-rename re-read does not confirm — the restore reports failure and
    /// never claims `original restored`, while the (verified) `.bak` fact-claim
    /// stays, because its bytes are confirmed.
    #[test]
    fn a_restore_target_readback_mismatch_reports_restore_failed_and_never_claims_restored() {
        let dir = crate::testutil::temp_dir("writeback-target-unverified");
        let path = seed(&dir, CLEAN);
        arm_post_write_corrupt(&path, false);
        arm_target_readback_mismatch(&path);
        let error = set_flag_on_file(&path, "cpu_offload", Some("true"))
            .expect_err("the restore must report the unconfirmed re-read");
        assert!(
            error.starts_with("post-write validation failed; RESTORE FAILED: "),
            "{error}"
        );
        assert!(error.contains("restore read-back"), "{error}");
        assert!(
            error.contains("original bytes are in"),
            "the copy is verified, so the fact-claim is true: {error}"
        );
        assert!(error.contains("qwen3.yaml.bak"), "{error}");
        assert!(
            !error.contains("original restored"),
            "anti-lie: an unconfirmed target may never be called restored: {error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("target"),
            CLEAN,
            "the rename did land; only the re-read was disturbed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("qwen3.yaml.bak")).expect("incident copy"),
            CLEAN
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_incident_stamp_is_iso_utc() {
        let stamp = utc_stamp();
        assert_eq!(stamp.len(), "2026-10-28T08:50:42Z".len(), "{stamp}");
        assert!(stamp.ends_with('Z'), "{stamp}");
        assert_eq!(stamp.as_bytes()[4], b'-');
        assert_eq!(stamp.as_bytes()[7], b'-');
        assert_eq!(stamp.as_bytes()[10], b'T');
        // `civil_from_days` sanity, pinned to known days — the leap-day pair and
        // the century non-leap boundary come from an independent day-by-day
        // reference-calendar sweep (FINDINGS-BATCH-2026-10-28 [8]): 2100 is not a
        // leap year, so day 47_541 must be March 1, not February 29.
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_089), (2025, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(19_781), (2024, 2, 28));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(19_783), (2024, 3, 1));
        assert_eq!(civil_from_days(47_541), (2100, 3, 1));
    }
}
