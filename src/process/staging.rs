//! The staged scripts/profiles seam of process supervision: content-addressed
//! staging of harness-rendered autostart scripts and rendered profiles, the
//! keep-N prunes that bound each pool, and the pure argv-slot rewrites that
//! point a launch at the staged bytes. Nearly stateless: every path is
//! injected by the caller.

use std::path::{Path, PathBuf};

use crate::engine::wsl_prefix;

use super::control::{run_control_command_with_stdin, ControlOutcome};
use super::excerpt;

/// The in-distro directory a harness-rendered autostart script is staged into.
/// Deliberately outside every model directory and outside the operator's script
/// tree: a staged file can never shadow or be confused with a hand-written one
/// (template adoption, operator ruling 2026-10-31). The value is READ from the
/// ninfer row's `custody_exec_roots` cell of the [`crate::engine::policy`]
/// table (slice 10b: the table cell is the single home, not a re-aliased
/// constant), so the compiled-in custody root and the staging path cannot
/// drift apart (§20-9 Fork C).
///
/// # Panics
///
/// Never: the compiled-in ninfer row always carries its staged-scripts
/// custody root (a row without it is a build-time bug).
#[must_use]
pub(super) fn staged_scripts_root() -> &'static str {
    crate::engine::policy::engine_policy(crate::ownership::NINFER_ENGINE)
        .and_then(|row| row.custody_exec_roots.first())
        .copied()
        .expect("invariant: the compiled-in ninfer row ships its staged-scripts custody root")
}

/// How many staged renders of one model's slug survive in the staging directory
/// (the staging driver prunes older `<slug>-<hash>.sh` files past this many). A
/// model under active editing re-renders on every yaml change; the cap keeps the
/// directory bounded while retaining enough history to re-launch what the ledger
/// still names.
const STAGED_SCRIPT_KEEP: usize = 10;

/// The `tail -n +N` bound that realizes [`STAGED_SCRIPT_KEEP`]: the first
/// `KEEP`-newest files are kept, everything from `KEEP+1` on is pruned.
const STAGED_SCRIPT_KEEP_PLUS_ONE: usize = STAGED_SCRIPT_KEEP + 1;

/// FNV-1a over the script bytes: a content address, not a security primitive. Two
/// renders of one model collide only when their bytes are equal, which is exactly
/// the dedup the staging path wants; a hostile yaml cannot engineer a collision
/// against another model because every render names its own container, and the
/// script's *content* is validated at render time regardless.
pub(super) fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    hash
}

/// The content address a staged script carries in its name. Public so the adapter's
/// argv placeholder names exactly the file `stage_script` will have verified —
/// one hash, one naming rule, no drift between what the ledger shows and what runs.
#[must_use]
pub fn script_content_hash(script: &str) -> u64 {
    fnv1a64(script.as_bytes())
}

/// How many staged profile renders of one model survive in the rendered-profile
/// root (the keep-N twin of [`STAGED_SCRIPT_KEEP`], strata template mode,
/// operator ruling 2026-11-02 row 19).
pub(super) const STAGED_PROFILE_KEEP: usize = 10;

/// The slug alphabet every staged artifact shares: the model or container name
/// with anything outside `[A-Za-z0-9._-]` replaced by `_`, so a staged file
/// name is always filesystem-bare and the keep-N prune matches one model's
/// pool literally.
#[must_use]
pub(crate) fn sanitize_slug(slug: &str) -> String {
    slug.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// The staged file name a rendered profile carries: the sanitized model slug
/// plus the same content address the runtime verifies the bytes against before
/// it rewrites `--config` (one hash, one naming rule — the ledger, the argv
/// placeholder and the file on disk name the same bytes). Engine-blind: every
/// engine that stages a rendered profile names it the same way
/// ([`crate::engine::strata`] delegates here; [`crate::engine::basalt`] calls
/// through it), so the prune and the verify can never disagree with the writer.
#[must_use]
pub(crate) fn staged_profile_name(slug: &str, text: &str) -> String {
    format!(
        "{}-{:016x}.json",
        sanitize_slug(slug),
        script_content_hash(text)
    )
}

/// The absolute directory harness-rendered profiles of one engine stage into:
/// the first `custody_data_roots` cell of that engine's compiled-in row,
/// resolved under the executable's own directory — the exe-adjacent home
/// [`strata_custody_path`] keeps (operator ruling 2026-11-02 row 19 for strata;
/// `TASK-BASALT` stage 1 generalized the door, one root per engine).
///
/// # Panics
///
/// When the compiled-in table carries no row for `engine`, or the row ships no
/// data root — an adapter handing the runtime a rendered profile for an engine
/// with no custody root is a builder bug, not operator input.
#[must_use]
pub(crate) fn rendered_profiles_dir(engine: &str) -> PathBuf {
    let root = crate::engine::policy::engine_policy(engine)
        .and_then(|row| row.custody_data_roots.first())
        .copied()
        .unwrap_or_else(|| {
            panic!("invariant: the compiled-in `{engine}` row ships its rendered-profiles custody root")
        });
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(root)))
        .unwrap_or_else(|| PathBuf::from(root))
}

/// The strata spelling of [`rendered_profiles_dir`], kept for the strata call
/// sites and their pins (byte-identical tail: `.simplehar/strata-profiles`).
#[must_use]
pub(crate) fn strata_profiles_dir() -> PathBuf {
    rendered_profiles_dir("strata")
}

pub(super) fn staged_script_path(slug: &str, script: &str) -> String {
    let slug = sanitize_slug(slug);
    let root = staged_scripts_root();
    format!("~/{root}/{slug}-{:016x}.sh", fnv1a64(script.as_bytes()))
}

/// Escape every POSIX ERE metacharacter so `find -regextype posix-extended -regex`
/// matches `literal` byte-for-byte. `staged_script_path` confines the slug it feeds
/// the prune regex to `[A-Za-z0-9._-]`, where only `.` is ERE-significant — but the
/// escape is total, so widening that charset upstream cannot silently hand a `.`- or
/// `[`-bearing slug the prune wildcard (review 2026-10-31: raw `har.a` pruned
/// `har-a`'s and `harxa`'s pools, measured on the distro's findutils).
///
/// # Arguments
///
/// * `literal` - text to be matched literally inside a POSIX ERE.
///
/// # Returns
///
/// The text with every metacharacter backslash-prefixed.
pub(super) fn escape_posix_regex(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len() + 8);
    for c in literal.chars() {
        if matches!(
            c,
            '.' | '\\' | '[' | ']' | '{' | '}' | '(' | ')' | '*' | '+' | '?' | '|' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Point the script slot of a staged-script argv at `path`, or refuse. The slot is
/// the word after `bash`, and `bash` must be the word after the *last* `--` that
/// closes `wsl.exe`'s own options — anchoring on the first `bash` anywhere would
/// let a config whose distro or user is literally `bash` move the rewrite onto the
/// wrong slot, leaving the unwritten placeholder for `bash` to open relative to
/// cwd (review 2026-10-31, LOW; fail-closed today, pinned so it stays so). Pure,
/// so the anchor is testable without a distro.
///
/// # Arguments
///
/// * `argv` - the adapter's launch argv, placeholder in the script slot.
/// * `path` - the staged path (from [`staged_script_path`]) to install.
///
/// # Returns
///
/// The rewritten argv, or `None` when argv is not `… -- bash <slot> …`.
pub(super) fn rewrite_script_slot(mut argv: Vec<String>, path: String) -> Option<Vec<String>> {
    let dashes = argv.iter().rposition(|arg| arg == "--")?;
    argv.get(dashes + 1).filter(|word| *word == "bash")?;
    let slot = argv.get_mut(dashes + 2)?;
    *slot = path;
    Some(argv)
}

/// Point the `--config` slot of a rendered-profile argv at `path`, or refuse.
/// The marker is the strata row's `path_markers` cell; a spec that carries a
/// rendered profile but no config slot is a builder bug, and the launch fails
/// closed rather than spawning a child pointed at the placeholder.
///
/// # Arguments
///
/// * `argv` - the adapter's launch argv, harness-minted placeholder in the slot.
/// * `path` - the staged absolute path to install as the flag's value.
///
/// # Returns
///
/// The rewritten argv, or `None` when no `--config` token carries a value.
pub(super) fn rewrite_config_slot(mut argv: Vec<String>, path: &str) -> Option<Vec<String>> {
    let slot = argv
        .iter()
        .position(|arg| arg == "--config")?
        .checked_add(1)?;
    *argv.get_mut(slot)? = path.to_string();
    Some(argv)
}

/// Stage a harness-rendered profile into the exe-adjacent root named by its
/// engine's row (operator ruling 2026-11-02 row 19, engine-tagged in
/// `TASK-BASALT` stage 1). Three guarantees, one door:
///
/// * **name binds bytes** — the argv placeholder must equal
///   `<slug>-<fnv>.json` recomputed from `text`, so a hand-built spec cannot
///   stage one profile under another model's name;
/// * **atomic write** — hash-named temp plus `rename` (same-directory,
///   replace-on-publish), then the published bytes are read back and re-hashed:
///   what the child reads is what the adapter validated;
/// * **bounded** — keep-N prune of this slug's
///   `<sanitized-slug>-<16hex>.json` pool ([`prune_staged_profiles`]),
///   newest first (the in-distro script driver's [`STAGED_SCRIPT_KEEP`] rule
///   ported to the Windows filesystem).
///
/// # Returns
///
/// The absolute staged path on success, or the refusal sentence.
pub(super) fn stage_rendered_profile(
    engine: &str,
    root: &Path,
    slug: &str,
    text: &str,
    placeholder: &str,
) -> Result<PathBuf, String> {
    let expected = staged_profile_name(slug, text);
    if placeholder != expected {
        return Err(format!(
            "the {engine} argv names `{placeholder}` but these bytes render `{expected}`: a \
             staged profile must carry its own content address"
        ));
    }
    std::fs::create_dir_all(root).map_err(|e| format!("cannot create {}: {e}", root.display()))?;
    let target = root.join(&expected);
    let temp = root.join(format!(".staging.{expected}.tmp"));
    if let Err(e) = std::fs::write(&temp, text.as_bytes()) {
        // A partial temp the prune never matches (it only claims `.json`)
        // must not survive its own failure (review 26 [3]).
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot write {}: {e}", temp.display()));
    }
    if let Err(error) = std::fs::rename(&temp, &target) {
        let _ = std::fs::remove_file(&temp);
        return Err(format!("cannot publish {}: {error}", target.display()));
    }
    let landed = std::fs::read(&target)
        .map_err(|e| format!("cannot read back {}: {e}", target.display()))?;
    if script_content_hash(&String::from_utf8_lossy(&landed)) != script_content_hash(text) {
        return Err(format!(
            "the staged profile at {} is not the bytes the harness rendered",
            target.display()
        ));
    }
    prune_staged_profiles(root, slug);
    Ok(target)
}

/// Keep-N prune of one slug's staged-profile pool, newest by mtime. Entries are
/// matched exactly — `<sanitized-slug>-<16 hex>.json`, the SAME name
/// [`crate::engine::strata::staged_profile_name`] writes (review 25 [3]: raw
/// vs sanitized would prune zero of its own files and grow the pool without
/// bound) — so a dot-bearing slug (`har.a`) can never prune another model's
/// pool (the lesson the ninfer staging driver learned on the distro's
/// findutils, review 2026-10-31). Two names that sanitize identically
/// (`a b` / `a_b`) share one pool by construction: the name is the content
/// address's owner and a launch re-stages its own bytes anyway.
fn prune_staged_profiles(root: &Path, slug: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let prefix = format!("{}-", sanitize_slug(slug));
    let mut pool: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let address = name.strip_suffix(".json")?.strip_prefix(prefix.as_str())?;
            // Exactly the 16-hex content address, nothing else, marks the pool.
            let is_address = address.len() == 16
                && address
                    .chars()
                    .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c));
            if !is_address {
                return None;
            }
            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            Some((mtime, entry.path()))
        })
        .collect();
    if pool.len() <= STAGED_PROFILE_KEEP {
        return;
    }
    pool.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, stale) in &pool[STAGED_PROFILE_KEEP..] {
        let _ = std::fs::remove_file(stale);
    }
}

/// The POSIX `cksum` value of `bytes` — CRC-32 with polynomial `0x04C11DB7`
/// (MSB-first, unreflected), zero init, then the message length folded in the same
/// way, then the complement — exactly what every POSIX `cksum` prints as its first
/// two fields. The staging driver re-computes this *in the distro* (`cksum < file`,
/// the two-field stdin form) and compares, so the staging evidence is the bytes,
/// not "some non-empty file exists". Chosen over awk arithmetic (a review measured
/// gawk and mawk disagreeing with each other and with any 64-bit hash) and over
/// zlib-style reflected CRC-32 (which is NOT what `cksum` computes — a review
/// caught the first cut confusing them): these are the algorithm's defining
/// constants, and the pin below is the operator's own coreutils 9.4 binary, whose
/// vectors this function reproduces byte-for-byte.
pub(super) fn cksum_expectation(bytes: &[u8]) -> (u32, usize) {
    fn fold(mut crc: u32, byte: u8) -> u32 {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
        crc
    }
    let mut crc = 0u32;
    for byte in bytes {
        crc = fold(crc, *byte);
    }
    let mut len = bytes.len();
    loop {
        crc = fold(crc, (len & 0xFF) as u8);
        len /= 256;
        if len == 0 {
            break;
        }
    }
    (!crc, bytes.len())
}

/// The stdin staging program: base64 heredoc in, hash-named temp, double `cksum`
/// gate (before and after the atomic `mv -f`), and a bounded per-slug keep-N prune.
/// Pure — every interpolation is harness-internal (the hash, the blob, the cksum
/// expectation, the path-safe slug escaped for the prune regex), so the driver's
/// shape is pinned without a distro. `file_name` is the argv word `bash -s` binds
/// to `$1`; its `<slug>-<16hex>.sh` shape names the same content address the temp
/// and the prune carry.
pub(super) fn staging_driver(script: &str, file_name: &str, blob: &str) -> String {
    let hash = fnv1a64(script.as_bytes());
    let slug = file_name
        .strip_suffix(&format!("-{:016x}.sh", hash))
        .unwrap_or(file_name);
    let (expected_crc, expected_len) = cksum_expectation(script.as_bytes());
    // `valid_container_name` admits `.`, which is the ERE any-character operator:
    // interpolated raw, a dot-bearing slug (`har.a`) would prune `har-a`'s and
    // `harxa`'s keep-N pool (review 2026-10-31, measured on the distro's findutils).
    // Escaping every metacharacter makes the regex match the slug LITERALLY.
    let slug_re = escape_posix_regex(slug);
    let root = staged_scripts_root();
    // The temp names the slug AND the hash, and both temp paths are `rm -f`-ed
    // before the redirects: shell `>` PRESERVES a pre-existing file's mode (umask
    // is creation-only), so a planted loose-mode temp must not propagate to the
    // published target, and two concurrent stagings of identical bytes must not
    // share one temp (review 2026-10-31, LOW; fail-closed either way — content is
    // cksum-gated — but a flake and a mode leak are both what staging must not do).
    format!(
        "#!/usr/bin/env bash\n\
         set -euo pipefail\n\
         umask 077\n\
         dir=\"$HOME/{root}\"\n\
         mkdir -p \"$dir\"\n\
         target=\"$dir/$1\"\n\
         tmp=\"$dir/.staging.{slug}-{hash:x}.tmp\"\n\
         trap 'rm -f -- \"$tmp\" \"$tmp.b64\"' EXIT\n\
         rm -f -- \"$tmp\" \"$tmp.b64\"\n\
         cat > \"$tmp.b64\" <<'SIMPLEHARN_B64'\n{blob}\nSIMPLEHARN_B64\n\
         base64 -d \"$tmp.b64\" > \"$tmp\"\n\
         rm -f -- \"$tmp.b64\"\n\
         test \"$(cksum < \"$tmp\")\" = \"{expected_crc} {expected_len}\" \\\n\
             || {{ echo \"staging: content mismatch before publish\" >&2; exit 3; }}\n\
         mv -f -- \"$tmp\" \"$target\"\n\
         test \"$(cksum < \"$target\")\" = \"{expected_crc} {expected_len}\" \\\n\
             || {{ echo \"staging: content mismatch after publish\" >&2; exit 4; }}\n\
         trap - EXIT\n\
         find \"$dir\" -maxdepth 1 -regextype posix-extended \\\n\
             -regex '.*/{slug_re}-[0-9a-f]{{16}}\\.sh' -printf '%T@\\t%p\\n' 2>/dev/null \\\n\
             | sort -rn | tail -n +{STAGED_SCRIPT_KEEP_PLUS_ONE} \\\n\
             | while IFS=$'\\t' read -r _ stale; do case \"$stale\" in \"$dir/\"*) rm -f -- \"$stale\";; esac; done\n",
    )
}

/// Stage `script`'s bytes at `path` (a `~/.simplehar/autostart/` path the wsl_user
/// owns) and prove the *content* landed. The distro rules (GOTCHAS `[ps]`) are "never
/// `wsl bash -c <string>` — the login shell re-tokenizes and expands the payload
/// before the inner shell sees it" and (same reason) never hand the inner shell a
/// Windows path: so the staging program arrives on **stdin** (`bash -s`), which the
/// login shell never sees at all, with only plain harness-minted words after it. The
/// script body never enters any command line — it rides a base64 blob inside the
/// driver's quoted heredoc, the one alphabet with no shell metacharacters — and
/// `$HOME` is resolved by the driver, inside the distro. Write is atomic
/// (slug-and-hash-named temp + `mv -f`; the temp is `rm -f`-ed up front because a
/// shell redirect preserves a pre-existing file's mode, while the *target* is
/// never pre-deleted, so a concurrent staging's published file is only ever
/// replaced by `mv` itself), a `trap` removes the temp on every exit path, and the
/// driver checks `cksum` of the staged bytes twice — before publish and after —
/// against the expectation computed here: a staging the distro did not confirm
/// byte-for-byte is reported, never spawned against.
///
/// # Returns
///
/// `Ok(())` only when the distro confirmed the staged file checksums to `script`'s
/// exact bytes.
pub(super) async fn stage_script(
    distro: &str,
    user: &str,
    path: &str,
    script: &str,
) -> Result<(), String> {
    use base64::Engine as _;
    if distro.trim().is_empty() || user.trim().is_empty() {
        return Err(
            "WSL coordinates missing from the config: cannot stage an autostart script".to_string(),
        );
    }
    let blob = base64::engine::general_purpose::STANDARD.encode(script.as_bytes());
    let file_name = path.rsplit('/').next().unwrap_or(path);
    let driver = staging_driver(script, file_name, &blob);
    // The driver rides stdin; the argv words after `bash -s` are plain words
    // (`<slug>-<16hex>.sh` re-sanitized by `staged_script_path`; the prune regex
    // carries the slug ERE-escaped — literal, never a wildcard — AND demands the
    // 16-hex content address, so no container name, prefix-related or dot-bearing,
    // can aim the keep-N pool at another model's renders). The
    // cksum expectation is NOT an argument: the login shell would re-split a
    // space-bearing value, so it rides as a literal inside the stdin script, and
    // `cksum < file` takes the two-field stdin form (the file-argument form prints
    // a third path field a `test =` can never match).
    let mut argv = wsl_prefix(distro, user);
    argv.extend(["bash".to_string(), "-s".to_string(), file_name.to_string()]);
    let (program, rest) = argv
        .split_first()
        .unwrap_or_else(|| unreachable!("invariant: wsl_prefix is never empty"));
    match run_control_command_with_stdin(program, rest, driver.as_bytes()).await {
        ControlOutcome::Ran(output) if output.status.success() => Ok(()),
        ControlOutcome::Ran(output) => Err(format!(
            "distro rejected the staging driver: {}",
            excerpt(&output.stderr)
        )),
        ControlOutcome::Failed(error) => Err(format!("cannot run the staging driver: {error}")),
        ControlOutcome::TimedOut => Err("the staging driver timed out".to_string()),
    }
}
