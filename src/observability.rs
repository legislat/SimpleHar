//! tracing initialization (DESIGN Step 16).
//!
//! Production logging is `tracing` only — the v3.3 baseline shipped 67 `println!` /
//! `eprintln!` calls and zero structured logging (Appendix A.6).

use tracing_subscriber::EnvFilter;

/// Initializes the global tracing subscriber.
///
/// Filter level comes from the `SIMPLEHAR_LOG` environment variable; when unset it
/// defaults to `info`. Called once from `main`; tests never call it (the default
/// subscriber would reject a second registration).
pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
