//! tracing initialization, the harness log ring, and the harness log file
//! (DESIGN Step 16).
//!
//! Production logging is `tracing` only — the v3.3 baseline shipped 67 `println!` /
//! `eprintln!` calls and zero structured logging (Appendix A.6). The ring keeps the
//! last [`RING_CAPACITY`] formatted events in memory so `/har/log` can answer "why
//! did the last load fail" without opening files (Step 16).
//!
//! The ring alone could not answer that question after a process death: it is memory,
//! and the shipped harness runs hidden (`autostart.vbs`, window style 0), so the
//! console layer reaches nothing. A harness that exited — or was killed — left zero
//! on-disk record of its own admissions, refusals and timings, which is exactly the
//! evidence G6/G13 promise. `FileLayer` therefore appends every event it accepts to
//! `logs/harness-<epoch>.log` **as it happens**, one `write_all` per line with no
//! userland buffer, so the bytes reach the kernel and survive a process kill. Flushing
//! at exit was rejected: the failure mode that matters is the exit that never happens.

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Ring capacity: the last 500 events, enough to reconstruct an incident (G6)
/// without growing unbounded.
pub const RING_CAPACITY: usize = 500;

/// File-name prefix of the harness's own log, so it can never be confused with — or
/// budgeted alongside — the captured engine children. One source of truth:
/// [`crate::process`] skips this prefix when it prunes engine logs.
pub const HARNESS_LOG_PREFIX: &str = "harness-";

/// Bytes the live harness log may reach before it rolls to the next generation
/// (`.log.1`, `.log.2`, …). Unbounded growth on a shared box is not acceptable; the
/// ring's [`RING_CAPACITY`] is not a file bound.
const HARNESS_LOG_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Harness log files kept in `logs/`, the live one included — one boot holds several
/// once it rolls, so this counts files, not boots. Enforced at boot
/// (`attach_file_sink`) and again after every rotation (`rotate_sink`), so a long boot
/// cannot roll past the budget. Independent of `LOG_FILE_CAP` on purpose: a swap-heavy
/// session burns the engine budget, and the log that explains a refusal must not be
/// evicted by the next 50 spawns.
pub const HARNESS_LOG_CAP: usize = 10;

/// Runtime extended-logging switch (the page's "Extended Logging" checkbox,
/// off at boot): gates the decision-level lines in the scheduler and keeps
/// routine status polls in the request ring (operator, 2026-10-26). Process
/// static so the stateless middleware and the actor read one truth.
static EXTENDED_LOGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether extended logging is on.
#[must_use]
pub fn extended_logging() -> bool {
    EXTENDED_LOGGING.load(std::sync::atomic::Ordering::Relaxed)
}

/// Flips the switch (idempotent).
pub fn set_extended_logging(on: bool) {
    EXTENDED_LOGGING.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// One captured harness log event.
#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    /// Unix epoch milliseconds.
    pub ts_ms: u64,
    /// `INFO` / `WARN` / `ERROR` / ...
    pub level: String,
    /// The event's target (module path).
    pub target: String,
    /// The conventional `message` field.
    pub message: String,
    /// Every other structured field, stringified.
    pub fields: BTreeMap<String, String>,
}

impl LogEntry {
    /// Builds an entry stamped now (test helper and layer input).
    pub fn now(level: &str, target: &str, message: impl Into<String>) -> Self {
        LogEntry {
            ts_ms: now_ms(),
            level: level.to_string(),
            target: target.to_string(),
            message: message.into(),
            fields: BTreeMap::new(),
        }
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

struct Ring {
    entries: VecDeque<LogEntry>,
    dropped: u64,
}

static RING: std::sync::OnceLock<Mutex<Ring>> = std::sync::OnceLock::new();

fn ring() -> &'static Mutex<Ring> {
    RING.get_or_init(|| {
        Mutex::new(Ring {
            entries: VecDeque::with_capacity(RING_CAPACITY),
            dropped: 0,
        })
    })
}

/// Appends an event to the ring, dropping the oldest past capacity.
pub fn ring_push(entry: LogEntry) {
    let mut ring = ring().lock().expect("log ring lock poisoned by a panic");
    if ring.entries.len() >= RING_CAPACITY {
        ring.entries.pop_front();
        ring.dropped += 1;
    }
    ring.entries.push_back(entry);
}

/// Test-only serialization of the process-global ring (FINDINGS-BATCH-2026-10-28I
/// [2]). `RING` is one `static` shared by every test in this binary, and
/// [`ring_caps_at_capacity_and_reports_drops`] drains it and asserts the eviction
/// counter — so any second writer in the drain..fill window turns `dropped == 10`
/// into 11 (measured: 18/20 co-scheduled runs failed before this lock existed).
/// Ring-touching tests take this guard; the lock is poison-tolerant because a
/// failing test must not cascade into every later ring test.
///
/// Keep it as declared: it is timing-window **insurance**, not a live failure on every
/// host. After the cap pin's set-difference rewrite its single `ring_push` can land
/// outside `ring_caps`' drain..fill window, and one host measured 60/60 passes with the
/// pin-side lock removed (the locked pair still 10/10) — on a machine where the pin's
/// setup is faster than that window, `dropped == 10` becomes 11 (FINDINGS2-15 [5]).
#[cfg(test)]
static RING_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_ring_for_test() -> std::sync::MutexGuard<'static, ()> {
    RING_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The last `limit` events (oldest first) plus how many were dropped overall.
pub fn ring_tail(limit: usize) -> (Vec<LogEntry>, u64) {
    let ring = ring().lock().expect("log ring lock poisoned by a panic");
    let skip = ring.entries.len().saturating_sub(limit);
    (
        ring.entries.iter().skip(skip).cloned().collect(),
        ring.dropped,
    )
}

/// The tracing layer feeding the ring: captures every event that passes the
/// filter with its level, target, message, and structured fields.
struct RingLayer;

struct RingVisitor<'a> {
    message: &'a mut String,
    fields: &'a mut BTreeMap<String, String>,
}

impl<'a> tracing_subscriber::field::Visit for RingVisitor<'a> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        if field.name() == "message" {
            *self.message = format!("{value:?}");
        } else {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }
    }
}

/// Flattens one event into a [`LogEntry`]. Shared by the ring and the file sink so
/// the two sinks can never disagree about what an event said — a per-sink copy is how
/// a redaction added to one would silently miss the other.
fn capture(event: &Event<'_>) -> LogEntry {
    let mut message = String::new();
    let mut fields = BTreeMap::new();
    event.record(&mut RingVisitor {
        message: &mut message,
        fields: &mut fields,
    });
    LogEntry {
        ts_ms: now_ms(),
        level: event.metadata().level().to_string(),
        target: event.metadata().target().to_string(),
        message,
        fields,
    }
}

impl<S> Layer<S> for RingLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        ring_push(capture(event));
    }
}

/// One open harness log file and how many bytes this boot has written to it.
struct FileSink {
    file: std::fs::File,
    /// The generation-0 name, kept so later generations derive from it instead of
    /// stacking extensions on each other.
    base: PathBuf,
    /// The file the live handle points at.
    path: PathBuf,
    written: u64,
    generation: u32,
    /// Set once a roll fails: retrying it on every subsequent event would cost a create
    /// plus a write per line against a disk that is not answering.
    rotation_disabled: bool,
}

static FILE_SINK: OnceLock<Mutex<Option<FileSink>>> = OnceLock::new();

/// Events the file sink could not write. The post-mortem artifact failing must be
/// visible without becoming a log line of its own (see [`note_write_failure`]).
static LOG_WRITE_FAILURES: AtomicU64 = AtomicU64::new(0);

/// Guards the one-shot console report so a failing disk cannot turn every subsequent
/// event into a fresh warning — and cannot recurse through this very layer.
static WRITE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

fn file_sink() -> &'static Mutex<Option<FileSink>> {
    FILE_SINK.get_or_init(|| Mutex::new(None))
}

/// How many harness log lines could not be written (published at `/har/status`).
#[must_use]
pub fn log_write_failures() -> u64 {
    LOG_WRITE_FAILURES.load(Ordering::Relaxed)
}

/// Points the file sink at a fresh `logs/harness-<epoch>.log` under `dir`, pruning the
/// harness log budget first.
///
/// Called once from [`init`], which no lib test runs — so in tests the sink stays
/// `None` and `FileLayer` is a no-op that cannot touch the real `logs/`.
///
/// # Arguments
///
/// * `dir` - the harness `logs/` directory, beside the executable.
///
/// # Returns
///
/// The path of the created log file.
///
/// # Errors
///
/// `std::io::Error` when the directory or the file cannot be created — the caller
/// warns and the harness still boots, because a missing post-mortem artifact must not
/// cost the operator their models.
pub fn attach_file_sink(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    // Pruned before the create, so the budget reserves this boot's own slot: pruning
    // afterwards could reach the live file in an mtime tie, and the newest log is the
    // one an operator opens first.
    crate::process::prune_harness_log_files(dir, HARNESS_LOG_CAP.saturating_sub(1));
    let epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or_default();
    let base = dir.join(format!("{HARNESS_LOG_PREFIX}{epoch}.log"));
    let file = std::fs::File::create(&base)?;
    let path = base.clone();
    *file_sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(FileSink {
        file,
        base,
        path: path.clone(),
        written: 0,
        generation: 0,
        rotation_disabled: false,
    });
    Ok(path)
}

/// Rolls the live log forward to the next generation and reopens.
///
/// Rolling instead of renaming is deliberate: `std::fs::File::create` opens without
/// `FILE_SHARE_DELETE`, so Windows refuses to rename a log whose handle this process
/// still holds. The closed generation stays on disk under the boot's own budget, which
/// also means an incident sitting on the size boundary does not lose its earlier half.
///
/// A successful roll then re-prunes the budget (R2-15-01): [`HARNESS_LOG_CAP`] used to
/// be enforced only at boot, so a request-flooded or swap-heavy boot rolled generation
/// after generation with nothing capping `logs/` until the next boot. The prune keeps
/// the newest `HARNESS_LOG_CAP - 1` files and the live file is newest-by-mtime, so it
/// is never a prune target; if an mtime tie ever made it one, the delete fails on the
/// open handle (no `FILE_SHARE_DELETE`) and `prune_harness_log_files` tolerates that.
fn rotate_sink(sink: &mut FileSink) -> std::io::Result<()> {
    let next_generation = sink.generation.saturating_add(1);
    let next = sink.base.with_extension(format!("log.{next_generation}"));
    // `?` before any mutation, so a failed create leaves the live handle untouched and
    // not one event is dropped.
    let file = std::fs::File::create(&next)?;
    sink.file = file;
    sink.path = next;
    sink.written = 0;
    sink.generation = next_generation;
    // After the mutation, so the previous generation's handle is already dropped and
    // the prune can delete the closed file when it targets it.
    if let Some(dir) = sink.base.parent() {
        crate::process::prune_harness_log_files(dir, HARNESS_LOG_CAP.saturating_sub(1));
    }
    Ok(())
}

/// Reports a sink failure exactly once, and only after the sink lock is released:
/// `tracing::warn!` re-enters this layer, so reporting under the (non-reentrant)
/// mutex would self-deadlock, and the one-shot flag is what makes the re-entry
/// terminate.
fn note_write_failure() {
    LOG_WRITE_FAILURES.fetch_add(1, Ordering::Relaxed);
    if !WRITE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            failures = 1,
            "harness log file write failed; the ring and console still carry every event \
             (the post-mortem artifact is incomplete)"
        );
    }
}

/// Whether appending `line_len` bytes to a sink holding `written` would exceed the
/// per-boot budget. A pure predicate so the boundary is pinned without writing 8 MiB.
fn needs_rotation(written: u64, line_len: u64, max: u64) -> bool {
    written.saturating_add(line_len) > max
}

/// Test seam lowering the roll threshold, so the roll path and the
/// stop-after-a-failed-roll rule are reachable without writing 8 MiB. Production
/// always sees [`HARNESS_LOG_MAX_BYTES`]. (Precedent: `ProcessRuntime`'s
/// `fail_job_after_spawn`.)
#[cfg(test)]
static TEST_ROLL_BUDGET: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
pub(crate) fn set_roll_budget_for_test(bytes: u64) {
    TEST_ROLL_BUDGET.store(bytes, Ordering::Relaxed);
}

/// The threshold the live roll decision uses.
fn roll_budget() -> u64 {
    #[cfg(test)]
    {
        let seeded = TEST_ROLL_BUDGET.load(Ordering::Relaxed);
        if seeded > 0 {
            return seeded;
        }
    }
    HARNESS_LOG_MAX_BYTES
}

/// Appends one entry as a JSON line with a single unbuffered `write_all`, so the
/// bytes reach the kernel before this returns and survive a process kill. No
/// `BufWriter` (it would hold the tail the whole design exists to keep) and no
/// per-line `sync_all` (power loss is not the failure mode being defended).
///
/// The sink lock is taken with `try_lock` (R2-15-02): a `write_all` that stalls
/// instead of failing — a paused share, an AV filter, a hibernated disk — would
/// otherwise park every thread that emits, HTTP serving included, behind one wedged
/// handle. On contention the append is skipped (the ring already holds the event)
/// and counted with [`note_write_failure`]; every uncontended path still writes the
/// same bytes with the same single `write_all`.
fn write_entry(entry: &LogEntry) {
    let Some(cell) = FILE_SINK.get() else {
        return; // no sink attached: tests, and any boot that never called `init`
    };
    let line = match serde_json::to_string(entry) {
        Ok(mut line) => {
            line.push('\n');
            line
        }
        Err(_) => {
            note_write_failure();
            return;
        }
    };
    let outcome = {
        let mut guard = match cell.try_lock() {
            Ok(guard) => guard,
            Err(std::sync::TryLockError::WouldBlock) => {
                note_write_failure();
                return;
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        let Some(sink) = guard.as_mut() else {
            return;
        };
        if !sink.rotation_disabled
            && needs_rotation(sink.written, line.len() as u64, roll_budget())
            && rotate_sink(sink).is_err()
        {
            // A failed roll must not cost this boot its events: keep appending to the
            // live handle. Stop rolling for the rest of the boot so a dead disk costs
            // one syscall per event rather than a create and a write, and let the next
            // boot's `attach_file_sink` try the budget again.
            sink.rotation_disabled = true;
        }
        let outcome = sink
            .file
            .write_all(line.as_bytes())
            .map(|()| sink.written = sink.written.saturating_add(line.len() as u64));
        drop(guard);
        outcome
    };
    if outcome.is_err() {
        note_write_failure();
    }
}

/// The layer keeping the on-disk post-mortem artifact (G6/G13): same captured shape
/// as the ring, its own level floor so a `SIMPLEHAR_LOG` mistake cannot leave the
/// forensic file empty when it is needed most.
struct FileLayer;

impl<S> Layer<S> for FileLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        write_entry(&capture(event));
    }
}

/// The level floor of the file sink: never quieter than `INFO`, whatever the operator
/// asked the console for. A mistyped or deliberately narrowing `SIMPLEHAR_LOG` must
/// not empty the one artifact that explains a dead harness; a more verbose request is
/// honoured in the file too.
fn file_filter(parsed: &EnvFilter) -> EnvFilter {
    // `Level` orders ERROR(1) < WARN(2) < INFO(3) < DEBUG(4) < TRACE(5), so a hint
    // below INFO means the operator asked for something quieter than the floor.
    let quieter_than_info = match parsed.max_level_hint() {
        None => false, // per-target directives only: no global level to floor
        Some(hint) => match hint.into_level() {
            Some(level) => level < tracing::Level::INFO,
            None => true, // `LevelFilter::OFF` carries no level and is quieter than any
        },
    };
    if quieter_than_info {
        // The non-lossy path, as `log_filter` documents: `EnvFilter::new` is `parse_lossy`.
        log_filter(DEFAULT_LOG_LEVEL).0
    } else {
        parsed.clone()
    }
}

/// Serializes the process-global file sink across the pins that attach it, for the
/// same reason [`RING_TEST_LOCK`] exists: one `static` shared by every test in the
/// binary, so an attaching pin must not overlap a reading pin.
#[cfg(test)]
static FILE_SINK_TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
pub(crate) fn lock_file_sink_for_test() -> std::sync::MutexGuard<'static, ()> {
    FILE_SINK_TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
pub(crate) fn file_sink_for_test(dir: &Path) -> std::io::Result<PathBuf> {
    attach_file_sink(dir)
}

#[cfg(test)]
pub(crate) fn detach_file_sink_for_test() {
    *file_sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

/// Clears the failure counter and its one-shot report flag so a pin can assert on
/// the delta it caused rather than on whatever an earlier pin left behind.
#[cfg(test)]
pub(crate) fn reset_write_failures_for_test() {
    LOG_WRITE_FAILURES.store(0, Ordering::Relaxed);
    WRITE_FAILURE_REPORTED.store(false, Ordering::Relaxed);
}

/// Whether the one-shot console report has already fired (the recursion guard's
/// state: a second failure must not arm a second `tracing::warn!`).
#[cfg(test)]
pub(crate) fn write_failure_reported_for_test() -> bool {
    WRITE_FAILURE_REPORTED.load(Ordering::Relaxed)
}

/// Forces the rotation the live sink would perform on budget exhaustion.
#[cfg(test)]
pub(crate) fn rotate_sink_for_test() -> std::io::Result<()> {
    let mut guard = file_sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match guard.as_mut() {
        Some(sink) => rotate_sink(sink),
        None => Err(std::io::Error::other("no sink attached")),
    }
}

/// The live file's path, for pins that read the artifact back.
#[cfg(test)]
pub(crate) fn file_sink_path_for_test() -> Option<PathBuf> {
    file_sink()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .map(|sink| sink.path.clone())
}

/// Initializes the global tracing subscriber.
///
/// Filter level comes from the `SIMPLEHAR_LOG` environment variable; when unset it
/// defaults to `DEFAULT_LOG_LEVEL`. A value that does not parse is **not** silently
/// accepted: `EnvFilter::new` is `parse_lossy`, which drops unparseable directives and
/// defaults to `ERROR` (tracing-subscriber-0.3.23 src/filter/env/mod.rs:350-354) — so
/// `SIMPLEHAR_LOG=verbose`, or a mistyped directive typed while debugging a boot
/// failure, produced an errors-only ring with no diagnostic, and `parse_lossy`'s own
/// warning fired before a subscriber existed, going to the no-op dispatcher
/// (FINDINGS-15 \[8]). An invalid value now falls back to `info` and says so, into the
/// console and the ring. Installs the ring layer alongside the console formatter.
/// Called once from `main`; tests never call it (the default subscriber would
/// reject a second registration).
pub fn init() {
    let env = std::env::var("SIMPLEHAR_LOG").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
    let (filter, invalid) = log_filter(&env);
    let console_filter = filter.clone();
    // Taken before `filter` moves into the ring layer: the file keeps its own floor.
    let file_layer = FileLayer.with_filter(file_filter(&filter));
    let ring_layer = RingLayer.with_filter(filter);
    let console = tracing_subscriber::fmt::layer().with_target(true);
    tracing_subscriber::registry()
        .with(console.with_filter(console_filter))
        .with(ring_layer)
        .with(file_layer)
        .init();
    // After `init`, so a failure can be reported through the subscriber that now exists.
    if let Err(error) = attach_file_sink(&crate::process::harness_logs_dir()) {
        tracing::warn!(
            error = %error,
            "harness log file unavailable; the ring and console still carry every event \
             but this boot leaves no post-mortem artifact"
        );
    }
    if let Some(error) = invalid {
        // Emitted AFTER `init`: before it there is no subscriber to hear the warning,
        // which is exactly how the lossy parse managed to be silent.
        tracing::warn!(
            simplehar_log = %env,
            error = %error,
            "SIMPLEHAR_LOG is not a valid filter; using `{DEFAULT_LOG_LEVEL}` \
             (a mistyped level used to mean errors-only)"
        );
    }
    // `init` sets the global default; the ring backs `/har/log` and the file sink
    // backs the post-mortem artifact.
}

/// The level the harness logs at when `SIMPLEHAR_LOG` is unset or unusable.
const DEFAULT_LOG_LEVEL: &str = "info";

/// Builds the `SIMPLEHAR_LOG` filter strictly, so a bad value is visible instead of
/// quietly downgrading the operator to errors-only.
///
/// # Arguments
///
/// * `env` - the raw `SIMPLEHAR_LOG` value.
///
/// # Returns
///
/// The filter to install, plus the parse error when the value was unusable (the caller
/// reports it once the subscriber exists).
fn log_filter(env: &str) -> (EnvFilter, Option<String>) {
    // `FromStr` is the strict path; `EnvFilter::new` is the lossy one this replaces.
    match env.parse::<EnvFilter>() {
        Ok(filter) => (filter, None),
        Err(error) => (
            DEFAULT_LOG_LEVEL
                .parse::<EnvFilter>()
                .expect("invariant: the default directive parses"),
            Some(error.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_at_capacity_and_reports_drops() {
        // The drain-and-count below is only meaningful while nobody else can push;
        // the ring is process-global (see `lock_ring_for_test`).
        let _serial = lock_ring_for_test();
        // The ring is process-global; isolate by draining first.
        let mut ring = ring().lock().expect("lock");
        ring.entries.clear();
        ring.dropped = 0;
        drop(ring);

        for i in 0..(RING_CAPACITY + 10) {
            ring_push(LogEntry::now("INFO", "test", format!("event {i}")));
        }
        let (tail, dropped) = ring_tail(RING_CAPACITY);
        assert_eq!(tail.len(), RING_CAPACITY);
        assert_eq!(dropped, 10);
        assert_eq!(tail.last().expect("non-empty").message, "event 509");
        // limit smaller than the ring: the newest N.
        let (tail, _) = ring_tail(5);
        assert_eq!(tail.len(), 5);
        assert_eq!(tail.last().expect("non-empty").message, "event 509");
    }

    /// FINDINGS-15 [8]: `EnvFilter::new` is `parse_lossy` and defaults to `ERROR`, so a
    /// mistyped `SIMPLEHAR_LOG` — `verbose`, a slipped key — silently produced an
    /// errors-only ring, the opposite of what the operator asked for, with no
    /// diagnostic (the lossy parse's own warning fires before a subscriber exists).
    /// Falsifier: build the filter with `EnvFilter::new(env)` and the mistyped legs
    /// report `ERROR` with no warning.
    #[test]
    fn a_mistyped_log_directive_falls_back_to_info_and_says_so() {
        let (filter, warning) = log_filter("info");
        assert!(warning.is_none(), "the documented default is not an error");
        assert_eq!(filter.max_level_hint(), Some(tracing::Level::INFO.into()));

        let (filter, warning) = log_filter("debug");
        assert!(warning.is_none(), "a valid directive is accepted");
        assert_eq!(filter.max_level_hint(), Some(tracing::Level::DEBUG.into()));

        for mistyped in [
            "!!!not-a-directive!!!",
            "simplehar_se=nonsense",
            "level=banana",
        ] {
            let (filter, warning) = log_filter(mistyped);
            assert!(
                warning.is_some(),
                "`SIMPLEHAR_LOG={mistyped}` must be reported, not swallowed"
            );
            assert_eq!(
                filter.max_level_hint(),
                Some(tracing::Level::INFO.into()),
                "`SIMPLEHAR_LOG={mistyped}` must fall back to the documented default — \
                 never to errors-only, which is what the lossy parse handed back"
            );
        }

        // The finding's other example is not the same case, and the pin says so:
        // `verbose` is a *valid* EnvFilter directive (a target named "verbose" at
        // TRACE), so `parse_lossy` never dropped it and it was never the errors-only
        // path. A value that names a target no callsite matches leaves an empty ring;
        // that hazard is EnvFilter's own grammar, not the lossy parse, and is left as
        // documented operator trust rather than second-guessed here.
        let (filter, warning) = log_filter("verbose");
        assert!(
            warning.is_none(),
            "a grammatically valid directive is not a mistype"
        );
        assert_eq!(filter.max_level_hint(), Some(tracing::Level::TRACE.into()));
    }

    /// Attaches the process-global sink to a fresh temp dir for the length of one pin.
    /// `RING_TEST_LOCK`'s reasoning applies verbatim: the sink is one `static` shared
    /// by every test in the binary, so an attaching pin must not overlap a reading pin,
    /// and a panicking pin must still detach.
    struct SinkGuard {
        path: PathBuf,
        _serial: std::sync::MutexGuard<'static, ()>,
    }

    impl SinkGuard {
        fn attach(tag: &str) -> Self {
            let _serial = lock_file_sink_for_test();
            // Always start a pin at the production threshold, so a panicking pin that
            // had seeded a tiny roll budget cannot leak it into the next one's line
            // counts.
            set_roll_budget_for_test(0);
            let dir = crate::testutil::temp_dir(tag);
            let path = file_sink_for_test(&dir).expect("attach harness log sink");
            SinkGuard { path, _serial }
        }
    }

    impl Drop for SinkGuard {
        fn drop(&mut self) {
            detach_file_sink_for_test();
        }
    }

    /// The whole reason the sink exists: a killed process keeps what it logged.
    /// Falsifier: wrap `sink.file` in a `BufWriter` — the read-back below comes up
    /// empty until the handle drops, which is the tail a kill takes away.
    #[test]
    fn every_line_reaches_the_disk_before_the_write_returns() {
        let sink = SinkGuard::attach("logdur");
        for index in 0..3 {
            write_entry(&LogEntry::now("INFO", "test", format!("event {index}")));
            let on_disk = std::fs::read_to_string(&sink.path).expect("read back mid-run");
            assert!(
                on_disk.contains(&format!("event {index}")),
                "line {index} is still sitting in a userland buffer: {on_disk}"
            );
        }
        let lines = std::fs::read_to_string(&sink.path).expect("file");
        assert_eq!(
            lines.lines().count(),
            3,
            "one whole line per event, no partials"
        );
    }

    /// Redaction stays upstream's job (`config`'s `REDACTED(present)` marker, the
    /// 15[9] posture); this pin's teeth are **parity** — the file must add no surface
    /// the ring does not already have. R2-15-03: the pin now emits real `tracing`
    /// events through one local subscriber that carries BOTH sink layers, so every
    /// event runs each layer's real `on_event` — the old shape hand-fed `ring_push`
    /// and `write_entry`, so a redaction added inside one sink's `on_event` stayed
    /// green here. Falsifier: redact a field in `FileLayer::on_event` (or give the
    /// file its own field formatting) and the `api_key`/`min_free` compares below go
    /// red on the file side while the ring stays plain.
    #[test]
    fn the_file_carries_exactly_what_the_ring_carries() {
        let sink = SinkGuard::attach("logpar");
        let _serial = lock_ring_for_test();
        {
            let mut ring = ring().lock().expect("lock");
            ring.entries.clear();
            ring.dropped = 0;
        }
        // The same layer pair `init` composes at boot: the ring keeps the boot
        // filter, the file keeps its own INFO floor. A **local** subscriber, never
        // `init()` — one process may register a default subscriber once and no lib
        // test may touch it.
        let base = log_filter(DEFAULT_LOG_LEVEL).0;
        let subscriber = tracing_subscriber::registry()
            .with(RingLayer.with_filter(base.clone()))
            .with(FileLayer.with_filter(file_filter(&base)));
        tracing::subscriber::with_default(subscriber, || {
            for index in 0..3 {
                if index == 1 {
                    // One event carries a token-shaped field. No downstream sink
                    // may redact it: redaction is upstream's job (15[9]).
                    tracing::warn!(
                        target: "simplehar_se::scheduler",
                        min_free = %format!("{index}.0 GiB"),
                        api_key = %"sk-test-not-a-real-secret",
                        "launch floor not met {index}"
                    );
                } else {
                    tracing::warn!(
                        target: "simplehar_se::scheduler",
                        min_free = %format!("{index}.0 GiB"),
                        "launch floor not met {index}"
                    );
                }
            }
        });
        let (tail, _) = ring_tail(3);
        let lines = std::fs::read_to_string(&sink.path).expect("file");
        let file_lines: Vec<&str> = lines.lines().collect();
        assert_eq!(
            file_lines.len(),
            tail.len(),
            "same events, same order, no extras"
        );
        for (line, entry) in file_lines.iter().zip(&tail) {
            let parsed: serde_json::Value = serde_json::from_str(line).expect("JSONL line");
            assert_eq!(parsed["message"], entry.message);
            assert_eq!(parsed["level"], entry.level);
            assert_eq!(parsed["target"], entry.target);
            assert_eq!(parsed["fields"]["min_free"], entry.fields["min_free"]);
            let file_key = parsed["fields"]["api_key"].as_str();
            let ring_key = entry.fields.get("api_key").map(String::as_str);
            assert_eq!(
                file_key, ring_key,
                "a field one sink redacts (or drops) breaks parity: \
                 file={file_key:?} ring={ring_key:?}"
            );
        }
        // Name the token once, so the loop cannot pass by BOTH sinks dropping it:
        // the value must arrive in both, whole, with no downstream REDACTED applied.
        let file_key = serde_json::from_str::<serde_json::Value>(file_lines[1])
            .expect("JSONL line")["fields"]["api_key"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let ring_key = tail[1].fields.get("api_key").cloned().unwrap_or_default();
        assert!(
            file_key.contains("sk-test-not-a-real-secret")
                && ring_key.contains("sk-test-not-a-real-secret"),
            "both sinks must carry the field as the event said it: \
             file={file_key:?} ring={ring_key:?}"
        );
        assert!(
            !file_key.contains("REDACTED") && !ring_key.contains("REDACTED"),
            "redaction stays upstream's job: no sink may apply it downstream: \
             file={file_key:?} ring={ring_key:?}"
        );
    }

    /// Rolling forward, never renaming an open handle (no `FILE_SHARE_DELETE` on
    /// Windows), and never dropping events when the roll itself fails.
    #[test]
    fn rotation_rolls_forward_and_keeps_accepting_events() {
        let sink = SinkGuard::attach("logrot");
        let closed = sink.path.clone();
        write_entry(&LogEntry::now("INFO", "test", "the aged line"));
        rotate_sink_for_test().expect("roll forward");
        assert!(
            std::fs::read_to_string(&closed)
                .expect("closed generation")
                .contains("the aged line"),
            "the closed generation keeps the name it had and every event it held"
        );
        let live = file_sink_path_for_test().expect("live sink path");
        assert_ne!(
            live, closed,
            "the roll moves to a NEW file; renaming an open handle is what Windows denies"
        );
        assert_eq!(
            std::fs::read_to_string(&live)
                .expect("live")
                .lines()
                .count(),
            0,
            "the new generation opens empty"
        );
        write_entry(&LogEntry::now("INFO", "test", "the fresh line"));
        assert!(
            std::fs::read_to_string(&live)
                .expect("live")
                .contains("the fresh line"),
            "the sink keeps writing after a roll"
        );
        assert!(
            std::fs::read_to_string(&closed)
                .expect("closed generation")
                .contains("the aged line"),
            "and the roll never disturbs the half already on disk"
        );
    }

    /// Names of the harness log files currently in `dir`, for budget pins.
    fn harness_log_file_names(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .expect("read logs dir")
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(HARNESS_LOG_PREFIX))
            .collect()
    }

    /// R2-15-01: the budget is a bound, not a boot-time snapshot — every roll re-prunes
    /// `logs/`. Falsifier: drop the `prune_harness_log_files` call from `rotate_sink`
    /// and the 12 rolls below leave 13 harness logs in the dir, past the cap of 10.
    #[test]
    fn every_rotation_prunes_the_harness_log_budget() {
        let sink = SinkGuard::attach("logcap");
        let dir = sink.path.parent().expect("logs dir");
        for roll in 1..=12 {
            rotate_sink_for_test().expect("roll forward");
            let names = harness_log_file_names(dir);
            assert!(
                names.len() <= HARNESS_LOG_CAP,
                "after {roll} rolls logs/ holds {} harness logs, past the cap: {names:?}",
                names.len(),
            );
            let live = file_sink_path_for_test().expect("live sink path");
            assert!(
                live.exists(),
                "the prune spares the live generation it must never delete: {names:?}"
            );
        }
    }

    /// A roll that cannot happen must lose no events, must not be retried for the rest
    /// of the boot, and must not resurrect when the blocker later disappears — so a
    /// dead disk costs one syscall per event, not a create plus a write. Here the next
    /// generation's name is occupied by a directory, which `File::create` cannot
    /// replace. Falsifier: drop the `rotation_disabled` short-circuit and once the
    /// blocker is gone the next line rolls, creating the file this pin asserts absent.
    #[test]
    fn a_failed_roll_loses_no_events_and_is_not_retried_this_boot() {
        let sink = SinkGuard::attach("logrollfail");
        let failures_before = log_write_failures();
        set_roll_budget_for_test(48); // every line now wants to roll
        let next = sink.path.with_extension("log.1");
        std::fs::create_dir(&next).expect("block the roll target with a directory");
        for index in 0..4 {
            write_entry(&LogEntry::now("INFO", "test", format!("kept line {index}")));
        }
        let kept = std::fs::read_to_string(&sink.path).expect("live file");
        assert_eq!(
            kept.lines().count(),
            4,
            "a boot whose roll failed must still hold every event: {kept}"
        );
        std::fs::remove_dir(&next).expect("unblock the roll target");
        write_entry(&LogEntry::now("INFO", "test", "after the blocker"));
        assert!(
            !next.exists(),
            "this boot stopped rolling after a failed roll — a fresh attempt means the \
             retry guard is gone"
        );
        assert!(
            std::fs::read_to_string(&sink.path)
                .expect("live file")
                .contains("after the blocker"),
            "and it keeps accepting every event regardless"
        );
        assert_eq!(
            log_write_failures(),
            failures_before,
            "a lost roll loses no event, so it is not a write failure"
        );
    }

    #[test]
    fn the_rotation_boundary_is_the_configured_budget() {
        assert!(!needs_rotation(0, 10, 100));
        assert!(
            !needs_rotation(90, 10, 100),
            "exactly filling the budget is not an overflow"
        );
        assert!(needs_rotation(91, 10, 100));
        assert!(
            needs_rotation(u64::MAX, 1, 100),
            "a saturating byte counter must still roll, never silently stop"
        );
    }

    /// The recursion guard: `note_write_failure` reports through `tracing`, which
    /// re-enters this very layer. Two failures must count twice but arm the console
    /// line once, or a dead disk becomes an eviction engine.
    #[test]
    fn a_failing_sink_counts_every_loss_and_reports_it_once() {
        let _serial = lock_file_sink_for_test();
        reset_write_failures_for_test();
        assert_eq!(log_write_failures(), 0, "the pin owns its own baseline");
        note_write_failure();
        note_write_failure();
        assert_eq!(log_write_failures(), 2, "every lost event is counted");
        assert!(
            write_failure_reported_for_test(),
            "and the operator is told, once, that the artifact is incomplete"
        );
        reset_write_failures_for_test();
        assert_eq!(
            log_write_failures(),
            0,
            "the reset is real, not a tautology"
        );
        assert!(!write_failure_reported_for_test());
    }

    /// R2-15-02: a stalled `write_all` must not become a rendezvous for every emitter.
    /// While this thread holds the sink lock, another thread's `write_entry` returns
    /// promptly, skips the append (the ring already holds the event), and counts the
    /// loss. Falsifier: restore `lock()` and the recv below times out because the
    /// emitter queues behind this guard.
    #[test]
    fn a_contended_sink_lock_skips_the_append_and_counts_the_loss() {
        let sink = SinkGuard::attach("logbusy");
        reset_write_failures_for_test();
        write_entry(&LogEntry::now("INFO", "test", "line before the hold"));
        let guard = file_sink().lock().expect("sink lock held for the pin");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            write_entry(&LogEntry::now("INFO", "test", "line while held"));
            let _ = tx.send(());
        });
        rx.recv_timeout(std::time::Duration::from_secs(2))
            .expect("a contended sink lock must not block the emitting thread");
        assert_eq!(
            log_write_failures(),
            1,
            "the skipped append is one counted loss, not silence"
        );
        drop(guard);
        let lines = std::fs::read_to_string(&sink.path).expect("file");
        assert!(
            lines.contains("line before the hold"),
            "the uncontended path still writes the same bytes: {lines}"
        );
        assert!(
            !lines.contains("line while held"),
            "the contended append is skipped, not queued and written late: {lines}"
        );
        reset_write_failures_for_test();
    }

    /// With no sink attached — every lib test, and any boot whose `init` never ran —
    /// the layer is inert rather than writing beside the test binary.
    #[test]
    fn no_sink_attached_writes_nothing_and_never_panics() {
        let _serial = lock_file_sink_for_test();
        detach_file_sink_for_test();
        write_entry(&LogEntry::now("ERROR", "test", "must go nowhere"));
        assert!(file_sink_path_for_test().is_none(), "still detached");
    }

    /// The other pins drive `write_entry` directly; this one proves the layer's own
    /// wiring — that a dispatched event reaches `capture` and lands on disk. It uses a
    /// **local** subscriber, never `init()`, which one process may register once and
    /// which no lib test may touch. Falsifier: empty `FileLayer::on_event` (or stop
    /// calling `capture`) and the emitted line disappears, while the direct-write pins
    /// stay green. `init`'s own composition is not reachable from a lib test and is
    /// not claimed here.
    #[test]
    fn the_file_layer_lands_events_from_a_real_tracing_emit() {
        let sink = SinkGuard::attach("logemit");
        let subscriber = tracing_subscriber::registry()
            .with(FileLayer.with_filter(file_filter(&log_filter("info").0)));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(job_id = 7, "launch floor not met");
        });
        let lines = std::fs::read_to_string(&sink.path).expect("file");
        assert!(
            lines.contains("launch floor not met"),
            "an emitted event must reach the post-mortem file: {lines}"
        );
        assert!(
            lines.contains("\"job_id\""),
            "structured fields must survive the flatten to JSON: {lines}"
        );
        assert!(
            lines.contains("simplehar_se::observability"),
            "the target is recorded, so a refusal is attributable: {lines}"
        );
    }

    /// The file keeps an INFO floor whatever the console was asked for, so a narrowed
    /// `SIMPLEHAR_LOG` cannot leave the artifact empty in the one moment it is read.
    /// Falsifier: hand the file the console's filter and the `error` leg drops to
    /// `ERROR`.
    #[test]
    fn the_file_sink_never_goes_quieter_than_info() {
        let quiet = log_filter("error").0;
        assert_eq!(
            file_filter(&quiet).max_level_hint(),
            Some(tracing::Level::INFO.into()),
            "an errors-only console must not empty the post-mortem file"
        );
        let loud = log_filter("debug").0;
        assert_eq!(
            file_filter(&loud).max_level_hint(),
            Some(tracing::Level::DEBUG.into()),
            "a more verbose request is honoured in the file too"
        );
    }
}
