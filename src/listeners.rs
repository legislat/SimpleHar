//! Listener health registry (DESIGN §0.2 failure contract, §15 red state).
//!
//! "Partial availability beats a dead process": a listener that cannot bind (something
//! else holds the port) or that died and cannot be restarted marks its category
//! `Down` here, the harness keeps running, the tray turns red, and `/har/status`
//! names the port and the reason. The supervisor in `main` retries dead listeners;
//! a successful rebind flips the entry back to `Serving`.

use crate::config::CategoryId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Health of one category listener.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ListenerState {
    /// Bound and serving.
    Serving,
    /// Not serving; the reason names the port and the bind failure.
    Down { reason: String },
}

/// Shared registry written by the listener supervisors and read by the API and tray.
#[derive(Debug, Clone, Default)]
pub struct ListenerRegistry {
    inner: Arc<Mutex<BTreeMap<CategoryId, ListenerState>>>,
}

impl ListenerRegistry {
    /// Creates an empty registry; the supervisor fills it at boot.
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks a listener as bound and serving.
    pub fn mark_serving(&self, category: CategoryId) {
        self.inner
            .lock()
            .expect("listener registry lock poisoned by a writer panic")
            .insert(category, ListenerState::Serving);
    }

    /// Marks a listener as down with the reason (port named, G13).
    pub fn mark_down(&self, category: CategoryId, reason: String) {
        self.inner
            .lock()
            .expect("listener registry lock poisoned by a writer panic")
            .insert(category, ListenerState::Down { reason });
    }

    /// Point-in-time snapshot for `/har/status`.
    pub fn snapshot(&self) -> BTreeMap<CategoryId, ListenerState> {
        self.inner
            .lock()
            .expect("listener registry lock poisoned by a writer panic")
            .clone()
    }

    /// Whether any listener is down — the tray's red condition (§15).
    pub fn any_down(&self) -> bool {
        self.snapshot()
            .values()
            .any(|state| !matches!(state, ListenerState::Serving))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn down_is_recorded_with_reason_and_visible_everywhere() {
        let registry = ListenerRegistry::new();
        registry.mark_serving(CategoryId::Work);
        registry.mark_down(
            CategoryId::Emb,
            "cannot bind 127.0.0.1:8112: address in use".to_string(),
        );
        assert!(registry.any_down());
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.get(&CategoryId::Work),
            Some(&ListenerState::Serving)
        );
        assert!(matches!(
            snapshot.get(&CategoryId::Emb),
            Some(ListenerState::Down { reason }) if reason.contains("8112")
        ));
    }

    #[test]
    fn all_serving_is_not_down() {
        let registry = ListenerRegistry::new();
        for category in CategoryId::all() {
            registry.mark_serving(category);
        }
        assert!(!registry.any_down());
    }

    #[test]
    fn a_successful_rebind_clears_down() {
        let registry = ListenerRegistry::new();
        registry.mark_down(CategoryId::Rp, "address in use".to_string());
        assert!(registry.any_down());
        registry.mark_serving(CategoryId::Rp);
        assert!(!registry.any_down());
    }
}
