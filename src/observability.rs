//! tracing initialization and the harness log ring (DESIGN Step 16).
//!
//! Production logging is `tracing` only — the v3.3 baseline shipped 67 `println!` /
//! `eprintln!` calls and zero structured logging (Appendix A.6). The ring keeps the
//! last [`RING_CAPACITY`] formatted events in memory so `/har/log` can answer "why
//! did the last load fail" without opening files (Step 16).

use serde::Serialize;
use std::collections::{BTreeMap, VecDeque};
use std::fmt;
use std::sync::Mutex;
use tracing::Subscriber;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Ring capacity: the last 500 events, enough to reconstruct an incident (G6)
/// without growing unbounded.
pub const RING_CAPACITY: usize = 500;

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

impl<S> Layer<S> for RingLayer
where
    S: Subscriber,
{
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut message = String::new();
        let mut fields = BTreeMap::new();
        event.record(&mut RingVisitor {
            message: &mut message,
            fields: &mut fields,
        });
        ring_push(LogEntry {
            ts_ms: now_ms(),
            level: event.metadata().level().to_string(),
            target: event.metadata().target().to_string(),
            message,
            fields,
        });
    }
}

/// Initializes the global tracing subscriber.
///
/// Filter level comes from the `SIMPLEHAR_LOG` environment variable; when unset it
/// defaults to [`DEFAULT_LOG_LEVEL`]. A value that does not parse is **not** silently
/// accepted: `EnvFilter::new` is `parse_lossy`, which drops unparseable directives and
/// defaults to `ERROR` (tracing-subscriber-0.3.23 src/filter/env/mod.rs:350-354) — so
/// `SIMPLEHAR_LOG=verbose`, or a mistyped directive typed while debugging a boot
/// failure, produced an errors-only ring with no diagnostic, and `parse_lossy`'s own
/// warning fired before a subscriber existed, going to the no-op dispatcher
/// (FINDINGS-15 [8]). An invalid value now falls back to `info` and says so, into the
/// console and the ring. Installs the ring layer alongside the console formatter.
/// Called once from `main`; tests never call it (the default subscriber would
/// reject a second registration).
pub fn init() {
    let env = std::env::var("SIMPLEHAR_LOG").unwrap_or_else(|_| DEFAULT_LOG_LEVEL.to_string());
    let (filter, invalid) = log_filter(&env);
    let console_filter = filter.clone();
    let ring_layer = RingLayer.with_filter(filter);
    let console = tracing_subscriber::fmt::layer().with_target(true);
    tracing_subscriber::registry()
        .with(console.with_filter(console_filter))
        .with(ring_layer)
        .init();
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
    // `init` sets the global default; the ring is installed for /har/log.
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
}
