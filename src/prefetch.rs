//! Release-window file warm (experimental, off by default): read the incoming
//! model's files into the OS cache while a committed release drains, so the
//! reload after the release reads from cache instead of from disk.
//!
//! The window this serves is the driver-confirmed release wait: the actor
//! commits a victim, then idles while the release is proven — seconds of
//! harness idle time the next load can use for nothing else. The warm is a
//! pure hint on the product path: fire-and-forget on the blocking pool,
//! never joined, never able to change an admission decision or a job
//! outcome. Neither the selection stats nor the read runs on the actor: the
//! commit turn only clones row keys in memory, and every path stat and
//! every byte read happens on the one detached blocking task. Every failure
//! ends as one log line.
//!
//! Which rows can warm — per-engine truth, pinned by selector tests: a
//! `llama` row warms its `model:` and `mmproj:` files, a `basalt` row warms
//! its `model:` pack file. Every other engine opens nothing its row's
//! top-level keys name, so its rows warm nothing. A strata row's weights
//! ride the `--native` token of `strata_args` and the tokens of
//! `strata_vision`; a legacy local `model:` left in a strata row names a
//! file the strata launch provably never opens, so that key is
//! deliberately not read for strata. A ninfer row's artifact rides
//! `ninfer_model_roots` with `ninfer_artifact`.
//!
//! Teardown bound (review B8R-03, accepted spike cost): the runtime joins
//! running blocking tasks when it drops at shutdown, so a tray Exit landing
//! during a warm waits out the remaining read before the process exits.
//! The wait is bounded per file by the `PREFETCH_MAX_FILE_BYTES` cap and
//! the volume's read rate — two files at most — and the Exit signal path
//! itself stays synchronous. The card run times the real delay
//! (`DEFERRED-IDEAS.md` section 3); no cancellation plumbing was added.

use crate::model::ModelEntry;
use std::collections::VecDeque;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// Row keys per warmable engine whose value the launch passes to the engine
/// as a file to open: llama resolves `model:` and `mmproj:` into its argv,
/// basalt passes its pack path verbatim. An engine not listed opens nothing
/// these keys name — the module header names each truth — so its rows warm
/// nothing, and a strata row's legacy `model:` is never read for it.
const WARM_ENGINES: [(&str, &[&str]); 2] =
    [("llama", &["model", "mmproj"]), ("basalt", &["model"])];

/// A file larger than this is skipped: warming it would push the standby cache
/// past what a box of this class can hold while serving, so the row reads
/// exactly as it did before the knob existed. The cap bounds one pathological
/// row; the real catalog rows of this box (tens of GiB each) pass.
const PREFETCH_MAX_FILE_BYTES: u64 = 48 * 1024 * 1024 * 1024;

/// Chunk size of the warm read: big enough to stream a large file in few
/// syscalls, small enough to keep the blocking thread's footprint trivial.
const PREFETCH_READ_CHUNK_BYTES: usize = 4 * 1024 * 1024;

/// Whether a stat'd row-named path may be warmed: a regular file at or under
/// the documented cap. Split out of [`RowWarmSpec::targets`] so the cap leg is
/// pinned without a `cap + 1` byte file on disk (a real one would fill the
/// volume; the stat is what the leg judges, and the stat is cheap to script).
fn file_worth_warming(is_file: bool, len: u64) -> bool {
    is_file && len <= PREFETCH_MAX_FILE_BYTES
}

/// Where each detached selection ran, oldest first: `(model, thread)`.
/// Bounded to the last [`SELECTION_LOG_CAP`] runs so a long-lived harness
/// cannot grow it. Pins read it through [`warm_selections_for_test`] to
/// prove the stats never ran on the actor thread (review B8R-01).
static WARM_SELECTIONS: Mutex<VecDeque<(String, std::thread::ThreadId)>> =
    Mutex::new(VecDeque::new());

/// Bound of [`WARM_SELECTIONS`]: a pin inspects one release window at a
/// time, and sixteen windows of `(String, ThreadId)` pairs are trivial.
const SELECTION_LOG_CAP: usize = 16;

/// The recorded selection runs, oldest first. Test seam for
/// [`WARM_SELECTIONS`]; production never reads it.
#[doc(hidden)]
pub fn warm_selections_for_test() -> Vec<(String, std::thread::ThreadId)> {
    WARM_SELECTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .iter()
        .cloned()
        .collect()
}

/// Note that a selection ran, on the current thread, for `model`.
fn record_selection(model: &str) {
    let mut runs = WARM_SELECTIONS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if runs.len() == SELECTION_LOG_CAP {
        runs.pop_front();
    }
    runs.push_back((model.to_string(), std::thread::current().id()));
}

/// The owned snapshot a detached warm selects from. Built cheaply on the
/// actor's commit turn — one in-memory clone of the keys this engine's
/// launch opens, no filesystem access — and consumed on the blocking pool,
/// where every stat of the selection runs (review B8R-01).
pub(crate) struct RowWarmSpec {
    /// The incoming model's name: what the log lines and the hook name.
    pub(crate) model: String,
    yaml_dir: Option<PathBuf>,
    /// The cloned `key: value` pairs this engine's launch opens. The commit
    /// turn checks `is_empty` in memory; the stats stay in `targets`.
    pub(crate) named: Vec<(String, String)>,
}

impl RowWarmSpec {
    /// Clone the warmable keys of one catalog row: cheap, in-memory, stats
    /// nothing. `None` names a row whose engine opens no harness-named
    /// weight file — that row has no work for a warm.
    pub(crate) fn from_entry(entry: &ModelEntry) -> Option<Self> {
        let (_, keys) = WARM_ENGINES
            .iter()
            .find(|(engine, _)| *engine == entry.engine)?;
        let named = keys
            .iter()
            .filter_map(|key| {
                entry
                    .raw
                    .get(*key)
                    .map(String::as_str)
                    .filter(|value| !value.is_empty())
                    .map(|value| ((*key).to_string(), value.to_string()))
            })
            .collect();
        Some(Self {
            model: entry.name.clone(),
            yaml_dir: entry
                .file
                .as_deref()
                .and_then(Path::parent)
                .map(Path::to_path_buf),
            named,
        })
    }

    /// The local weight files of the snapshot worth warming, in row-key
    /// order.
    ///
    /// Each value is resolved exactly like the launch resolves it (relative
    /// values join the yaml directory — the same directory the scan stat'd
    /// and booked), through [`crate::engine::llama::resolve_model_path`], so
    /// for an engine the table lists the warmed file is the opened file.
    /// Scheme sentinels (`hf://…`) name no local file and are skipped, as in
    /// the scan's existence trace. Missing files, directories, and files
    /// over [`PREFETCH_MAX_FILE_BYTES`] are skipped: the warm may only touch
    /// what a launch would actually read. A path named by two keys (e.g.
    /// `model` and `mmproj` spelling the same file) is returned once. Every
    /// path stat runs here, on whichever thread called — the product path
    /// calls this only inside the detached blocking task, never on the
    /// actor's commit turn.
    pub(crate) fn targets(&self) -> Vec<PathBuf> {
        record_selection(&self.model);
        let mut targets: Vec<PathBuf> = Vec::with_capacity(self.named.len());
        for (_key, value) in &self.named {
            if value.contains("://") {
                continue; // a scheme sentinel launches verbatim and names no local file
            }
            let path = PathBuf::from(crate::engine::llama::resolve_model_path(
                value,
                self.yaml_dir.as_deref(),
            ));
            if targets.iter().any(|seen| seen == &path) {
                continue; // dedup: one warm per file, however many keys name it
            }
            let Ok(metadata) = path.metadata() else {
                continue; // missing or unreadable stat: nothing to warm
            };
            if !file_worth_warming(metadata.is_file(), metadata.len()) {
                continue;
            }
            targets.push(path);
        }
        targets
    }
}

/// Select straight from a catalog row: the selector-rules shape of
/// `RowWarmSpec::from_entry(..).map(|spec| spec.targets())` for the unit
/// pins. The product path never calls this — it clones through
/// [`RowWarmSpec::from_entry`] on the actor and stats in the detached task.
#[cfg(test)]
pub(crate) fn prefetch_targets(entry: &ModelEntry) -> Vec<PathBuf> {
    RowWarmSpec::from_entry(entry)
        .map(|spec| spec.targets())
        .unwrap_or_default()
}

/// Warm `targets` into the OS cache by reading each file to its end and
/// discarding the bytes. The read is sequential and the buffer is reused, so
/// the cost is disk bandwidth and standby cache, not process memory.
///
/// # Errors
///
/// Returns the failure class (`"open-failed: …"` / `"read-failed: …"`) of the
/// first target that could not be fully read, with the OS reason. A partial
/// warm stays in the cache: a warm hint that dies half-way still helped, and
/// the class is what the log line names.
pub fn warm_files(targets: &[PathBuf]) -> Result<u64, String> {
    let mut buf = vec![0u8; PREFETCH_READ_CHUNK_BYTES];
    let mut total: u64 = 0;
    for path in targets {
        let mut file = std::fs::File::open(path)
            .map_err(|e| format!("open-failed: {}: {e}", path.display()))?;
        loop {
            let read = file
                .read(&mut buf)
                .map_err(|e| format!("read-failed: {}: {e}", path.display()))?;
            if read == 0 {
                break;
            }
            total += read as u64;
        }
    }
    Ok(total)
}

/// A release-window warm replacement for pins: called with the incoming model
/// name and the selector's targets; returns the byte count the warm line logs,
/// or the failure class the warn line logs. The hook runs on the blocking
/// pool, after selection has already run there.
pub type WarmHook = Arc<dyn Fn(&str, &[PathBuf]) -> Result<u64, String> + Send + Sync>;

/// Process-global warm hook. Always `None` in production: reading it is one
/// mutex load per (already knob-gated, rare) release window. Pins install a
/// recording hook to observe the invocation, park it to prove the actor stays
/// free, or fail it to prove the job outcome is untouched.
static WARM_HOOK: Mutex<Option<WarmHook>> = Mutex::new(None);

/// Whether a hook is installed — the fast check so production installs no
/// lock on the per-window path.
static WARM_HOOK_SET: AtomicBool = AtomicBool::new(false);

/// The hook the release-window warm must call, if a pin installed one.
pub(crate) fn warm_hook() -> Option<WarmHook> {
    if !WARM_HOOK_SET.load(Ordering::SeqCst) {
        return None;
    }
    WARM_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Replaces the process-global warm hook for the duration of a pin, clearing
/// it when the guard drops. The hook is one process-wide `static`, so pins
/// that install it must serialize against each other (the pin-side lock of
/// their test binary); an unserialized install would be hijacked by, or would
/// hijack, a concurrent pin.
#[doc(hidden)]
#[must_use = "the guard clears the hook; dropping it immediately uninstalls"]
pub fn install_warm_hook_for_test(hook: WarmHook) -> WarmHookGuard {
    *WARM_HOOK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
    WARM_HOOK_SET.store(true, Ordering::SeqCst);
    WarmHookGuard { _private: () }
}

/// Clears the test warm hook. See [`install_warm_hook_for_test`].
#[doc(hidden)]
pub struct WarmHookGuard {
    _private: (),
}

impl Drop for WarmHookGuard {
    fn drop(&mut self) {
        *WARM_HOOK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        WARM_HOOK_SET.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use std::collections::BTreeMap;

    /// A row whose yaml lives in `dir`, with the given raw keys. The engine
    /// is `llama` — the engine whose launch opens both keys, so selector
    /// rules run against a row that can actually warm.
    fn row(dir: &Path, raw: &[(&str, &str)]) -> ModelEntry {
        ModelEntry {
            name: "row".to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(1.0),
            ram_gb: None,
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            fit_gb: None,
            model_ref: None,
            serve_model: None,
            file: Some(dir.join("row.yaml")),
            raw: raw
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    fn seed(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("seed parent");
        }
        std::fs::write(&path, bytes).expect("seed file");
        path
    }

    #[test]
    fn the_selector_returns_the_rows_weight_files_in_key_order() {
        let dir = crate::testutil::temp_dir("b8-sel-order");
        let model = seed(&dir, "m.gguf", b"weights");
        let vision = seed(&dir, "v.gguf", b"vision");
        let entry = row(
            &dir,
            &[
                ("model", model.to_str().unwrap()),
                ("mmproj", vision.to_str().unwrap()),
            ],
        );
        assert_eq!(prefetch_targets(&entry), vec![model, vision]);
    }

    /// Falsifier: drop the dedup leg and the same file warms twice (the
    /// `files=`/`bytes=` figures double and the read runs twice).
    #[test]
    fn the_selector_names_a_file_twice_only_once() {
        let dir = crate::testutil::temp_dir("b8-sel-dedup");
        let shared = seed(&dir, "shared.gguf", b"weights");
        let entry = row(
            &dir,
            &[
                ("model", shared.to_str().unwrap()),
                ("mmproj", shared.to_str().unwrap()),
            ],
        );
        assert_eq!(prefetch_targets(&entry), vec![shared]);
    }

    /// Falsifier: remove any skip leg and the missing file, the directory, or
    /// the scheme sentinel enters the targets, and the warm either fails or
    /// reads bytes no launch will open.
    #[test]
    fn the_selector_skips_missing_directories_sentinels_and_blanks() {
        let dir = crate::testutil::temp_dir("b8-sel-skip");
        let real = seed(&dir, "real.gguf", b"weights");
        let a_dir = dir.join("adir.gguf");
        std::fs::create_dir_all(&a_dir).expect("seed dir");
        // `gone.gguf` is never created; the blank value is inert as in the
        // adapter; the scheme sentinel names no local file.
        let missing = dir.join("gone.gguf");
        let entry = row(
            &dir,
            &[
                ("model", missing.to_str().unwrap()),
                ("mmproj", real.to_str().unwrap()),
            ],
        );
        assert_eq!(prefetch_targets(&entry), vec![real.clone()]);
        let entry = row(&dir, &[("model", a_dir.to_str().unwrap()), ("mmproj", "")]);
        assert!(prefetch_targets(&entry).is_empty());
        let entry = row(&dir, &[("model", "hf://org/repo/x.gguf")]);
        assert!(prefetch_targets(&entry).is_empty());
        let entry = row(&dir, &[]);
        assert!(
            prefetch_targets(&entry).is_empty(),
            "an empty row warms nothing"
        );
    }

    #[test]
    fn the_selector_resolves_a_relative_key_against_the_yaml_dir() {
        let dir = crate::testutil::temp_dir("b8-sel-rel");
        let model = seed(&dir, "weights.gguf", b"weights");
        // A relative `model:` joins the yaml dir — the one resolver the scan,
        // the booking, and the launch share (R2-09-02).
        let entry = row(&dir, &[("model", "weights.gguf")]);
        assert_eq!(prefetch_targets(&entry), vec![model]);
    }

    /// The per-engine honesty leg (review B8R-02): an engine the launch-path
    /// evidence does not cover warms nothing, even a strata row carrying a
    /// legacy local `model:` — its launch opens that file never, so warming
    /// it would burn disk bandwidth no reload will reuse.
    /// Falsifier: drop the `WARM_ENGINES` lookup (read the keys for every
    /// engine) and the strata leg finds the legacy file, red here.
    #[test]
    fn engines_that_open_no_row_key_file_warm_nothing_even_with_a_legacy_model_key() {
        let dir = crate::testutil::temp_dir("b8-sel-engines");
        let legacy = seed(&dir, "legacy.gguf", b"weights");
        for engine in ["strata", "ninfer", "mock", "anything-else"] {
            let mut entry = row(&dir, &[("model", legacy.to_str().unwrap())]);
            entry.engine = engine.to_string();
            assert!(
                prefetch_targets(&entry).is_empty(),
                "`{engine}` opens no file the row keys name, so it warms nothing"
            );
        }
    }

    /// A basalt launch passes the pack path verbatim and never an mmproj,
    /// so the basalt row warms the pack only even when a stray `mmproj:` key
    /// rides along. Falsifier: share one key list between llama and basalt
    /// and the second file enters the targets, red here.
    #[test]
    fn a_basalt_row_warms_only_its_pack_file() {
        let dir = crate::testutil::temp_dir("b8-sel-basalt");
        let pack = seed(&dir, "m.basalt", b"pack");
        let vision = seed(&dir, "v.gguf", b"vision");
        let mut entry = row(
            &dir,
            &[
                ("model", pack.to_str().unwrap()),
                ("mmproj", vision.to_str().unwrap()),
            ],
        );
        entry.engine = "basalt".to_string();
        assert_eq!(
            prefetch_targets(&entry),
            vec![pack],
            "the basalt launch opens the pack file only"
        );
    }

    /// The cap leg pinned as the pure decision the selector applies to every
    /// stat: at the cap the file warms, one byte over it the row reads exactly
    /// as before the knob existed. Falsifier: drop the `len <=` compare (or
    /// flip it to `>=`) and the boundary legs swap.
    #[test]
    fn a_file_over_the_documented_cap_is_skipped_and_one_at_it_warms() {
        assert!(file_worth_warming(true, PREFETCH_MAX_FILE_BYTES));
        assert!(
            !file_worth_warming(true, PREFETCH_MAX_FILE_BYTES + 1),
            "one byte over the cap must skip, not warm tens of GiB past it"
        );
        assert!(!file_worth_warming(false, 1), "a directory never warms");
    }

    /// The selection seam itself pinned: every run lands in the record with
    /// its model and its thread. `any` not `last`: unit tests run on
    /// parallel threads and share the process-wide record.
    /// Falsifier: drop the `record_selection` call and no run appears, red.
    #[test]
    fn each_selection_run_is_recorded_with_its_model_and_thread() {
        let dir = crate::testutil::temp_dir("b8-sel-record");
        let model = seed(&dir, "m.gguf", b"weights");
        let mut entry = row(&dir, &[("model", model.to_str().unwrap())]);
        entry.name = "b8-recorded-row".to_string();
        assert_eq!(prefetch_targets(&entry), vec![model]);
        let thread = std::thread::current().id();
        assert!(
            warm_selections_for_test()
                .iter()
                .any(|(name, run)| name == "b8-recorded-row" && *run == thread),
            "the selection recorded its model and its thread"
        );
    }

    #[test]
    fn the_warm_reads_every_target_and_counts_the_bytes() {
        let dir = crate::testutil::temp_dir("b8-warm");
        let a = seed(&dir, "a.gguf", b"0123456789");
        let b = seed(&dir, "b.gguf", b"01234");
        let warmed = warm_files(&[a, b]).expect("warm reads both files");
        assert_eq!(warmed, 15);
    }

    #[test]
    fn the_warm_names_the_failure_class_and_never_panics() {
        let dir = crate::testutil::temp_dir("b8-warm-fail");
        let good = seed(&dir, "good.gguf", b"weights");
        let gone = dir.join("gone.gguf");
        let error = warm_files(&[good, gone]).expect_err("the missing file fails the warm");
        assert!(
            error.starts_with("open-failed:"),
            "the class is named in the message: {error}"
        );
    }

    #[test]
    fn the_warm_hook_installs_and_clears_with_its_guard() {
        assert!(warm_hook().is_none(), "no hook stands between pins");
        let hook: WarmHook = Arc::new(|_, _| Ok(0));
        let guard = install_warm_hook_for_test(Arc::clone(&hook));
        assert!(warm_hook().is_some(), "the pin's hook answers");
        drop(guard);
        assert!(warm_hook().is_none(), "the hook is gone with its guard");
    }
}
