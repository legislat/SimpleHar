//! Test-support helpers shared by the crate's unit tests and its integration
//! binaries.
//!
//! A single implementation, rather than a copy per test binary, because every
//! suite had the same defect: a temp path keyed by process id alone is shared by
//! all the tests in that binary, and `#[tokio::test]` runs them concurrently, so
//! one test's `remove_dir_all` deletes another's catalog YAMLs mid-swap (flaky
//! `unknown model` failure, review 2026-10-26).

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process discriminator for minted paths, so two runs never share a dir.
static NEXT: AtomicU64 = AtomicU64::new(0);

/// Mints a directory that is unique to this call.
///
/// # Arguments
///
/// * `tag` - Short suite label so leftovers stay attributable (e.g. `wit`).
///
/// # Returns
///
/// A freshly created directory under the system temp root, emptied first if an
/// earlier crashed run left one behind. It is not deleted afterwards, so a failing
/// test can be inspected; the temp root is reclaimed by the OS.
///
/// # Panics
///
/// If the directory cannot be created.
#[doc(hidden)]
#[must_use]
pub fn temp_dir(tag: &str) -> PathBuf {
    let seq = NEXT.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("se-{tag}-{}-{seq}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[cfg(test)]
mod tests {
    use super::temp_dir;

    #[test]
    fn consecutive_calls_never_share_a_directory() {
        // The whole point: two tests in one binary must not collide.
        let a = temp_dir("uniq");
        let b = temp_dir("uniq");
        assert_ne!(a, b);
        assert!(a.is_dir() && b.is_dir());
        std::fs::write(a.join("only-a.yaml"), "name: a\n").expect("write a");
        assert!(!b.join("only-a.yaml").exists(), "b must not see a's files");
    }
}
