//! Listener health registry (DESIGN §0.2 failure contract, §15 red/amber states).
//!
//! "Partial availability beats a dead process": a listener that cannot bind (something
//! else holds the port) or that died and cannot be restarted marks its category
//! `Down` here, the harness keeps running, the tray turns red, and `/har/status`
//! names the port and the reason. The supervisor in `main` retries dead listeners;
//! a successful rebind flips the entry back to `Serving`.
//!
//! Every category carries a verdict from the moment the registry exists (FINDINGS-14
//! [7] + FINDINGS-15 [2], operator ruling "A + amber" 2026-10-28): a fresh registry is
//! `Pending` in all four, so "no supervisor has spoken for this category yet" is a
//! visible verdict rather than a missing key. Consumers therefore never ask whether a
//! category is *present* (the rejected Fork B) — they ask which of the verdicts it
//! holds. `Pending` is one word with the tray: a category with no verdict and a status
//! read that returned no verdict both read amber, never green and never red.

use crate::config::CategoryId;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Health of one category listener.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum ListenerState {
    /// No verdict yet: no supervisor has published a bind outcome for this category.
    /// Neither healthy nor down — the tray reads amber once the boot grace is over
    /// (§15, operator ruling 2026-10-28), and `/har/status` says `"Pending"`.
    Pending,
    /// Bound and serving.
    Serving,
    /// Not serving; the reason names the port and the bind failure.
    Down { reason: String },
}

impl ListenerState {
    /// THE no-verdict predicate — the single home of the `Pending` test, shared by
    /// [`ListenerRegistry::pending`] and the §0.2 boot gate (FINDINGS-BATCH-2026-10-28E
    /// [2]) so no consumer can re-implement the match and drift from it.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        matches!(self, Self::Pending)
    }
}

/// Shared registry written by the listener supervisors and read by the API and tray.
///
/// Clones share one map and one creation instant, so the tray, the API and every
/// supervisor read and write the same verdicts and agree on the boot-grace epoch.
#[derive(Debug, Clone)]
pub struct ListenerRegistry {
    inner: Arc<Mutex<BTreeMap<CategoryId, ListenerState>>>,
    created: Instant,
}

impl ListenerRegistry {
    /// Creates a registry with **every** configured category `Pending`.
    ///
    /// Seeding is the contract: the four categories are a closed set known before any
    /// bind, so an unseeded — and therefore silently "healthy" — registry is not
    /// representable. The creation instant is the epoch the tray's boot grace is
    /// measured from ([`ListenerRegistry::created_at`]).
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(
                CategoryId::all()
                    .into_iter()
                    .map(|category| (category, ListenerState::Pending))
                    .collect(),
            )),
            created: Instant::now(),
        }
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
    ///
    /// Total over the verdicts of every category, and deliberately *not* "any verdict
    /// that is not Serving": a `Pending` category has no verdict to be red about and
    /// belongs to amber. With the registry seeded at construction this boolean says
    /// exactly one thing — some supervisor looked at a port and could not serve it.
    pub fn any_down(&self) -> bool {
        self.snapshot()
            .values()
            .any(|state| matches!(state, ListenerState::Down { .. }))
    }

    /// Every category still without a verdict, in canonical (slot-priority) order.
    ///
    /// This is the list the §0.2 ready gate prints in `listeners UNRESOLVED: [...]`
    /// and the list the tray turns amber over once the boot grace is over.
    #[must_use]
    pub fn pending(&self) -> Vec<CategoryId> {
        self.snapshot()
            .iter()
            .filter(|(_, state)| state.is_pending())
            .map(|(category, _)| *category)
            .collect()
    }

    /// The instant this registry was constructed: the boot epoch the tray's amber
    /// grace is measured from, so "the harness is booting" and "the harness is
    /// lying" are distinguishable without either side keeping its own clock.
    #[must_use]
    pub fn created_at(&self) -> Instant {
        self.created
    }
}

impl Default for ListenerRegistry {
    /// Identical to [`ListenerRegistry::new`]: there is no unseeded registry, so the
    /// derived default cannot be a back door around the seeding contract.
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fresh_registry_is_pending_in_every_category() {
        // FINDINGS-14 [7] / FINDINGS-15 [2] pin: absence is unrepresentable, so no
        // consumer needs a presence check. Falsifier: un-seed `new()` and every
        // assertion below fails (empty map, no `Pending`, no names to report).
        let registry = ListenerRegistry::new();
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.len(),
            CategoryId::all().len(),
            "every configured category is in the registry before any supervisor runs"
        );
        for category in CategoryId::all() {
            assert_eq!(
                snapshot.get(&category),
                Some(&ListenerState::Pending),
                "{category} must start as a no-verdict-yet `Pending`, not absent and not Serving"
            );
        }
        assert_eq!(
            registry.pending(),
            CategoryId::all().to_vec(),
            "the ready gate can name every unresolved category, in canonical order"
        );
        assert!(
            !registry.any_down(),
            "no verdict is not a down verdict: an unbound-but-unresolved boot is amber, not red"
        );
        assert_eq!(
            ListenerRegistry::default().snapshot(),
            snapshot,
            "`default()` is not a way back to an unseeded registry"
        );
    }

    #[test]
    fn any_down_is_total_over_the_four_verdicts() {
        let all_up = ListenerRegistry::new();
        for category in CategoryId::all() {
            all_up.mark_serving(category);
        }
        assert!(
            !all_up.any_down(),
            "four times Serving is the only all-clear"
        );

        let half_booted = ListenerRegistry::new();
        half_booted.mark_serving(CategoryId::Work);
        half_booted.mark_serving(CategoryId::Rp);
        half_booted.mark_serving(CategoryId::Aux);
        assert!(
            !half_booted.any_down(),
            "the still-`Pending` EMB belongs to amber, never to red"
        );
        assert_eq!(
            half_booted.pending(),
            vec![CategoryId::Emb],
            "and it is exactly the category the ready gate must name"
        );

        let one_down = ListenerRegistry::new();
        for category in CategoryId::all() {
            one_down.mark_serving(category);
        }
        one_down.mark_down(CategoryId::Aux, "address in use".to_string());
        assert!(one_down.any_down(), "one explicit Down verdict is red");
        assert!(one_down.pending().is_empty(), "a Down verdict is a verdict");

        let all_down = ListenerRegistry::new();
        for category in CategoryId::all() {
            all_down.mark_down(category, "cannot bind".to_string());
        }
        assert!(all_down.any_down());
        assert!(all_down.pending().is_empty());
    }

    #[test]
    fn down_is_recorded_with_reason_and_visible_everywhere() {
        let registry = ListenerRegistry::new();
        registry.mark_serving(CategoryId::Work);
        registry.mark_down(
            CategoryId::Emb,
            "cannot bind 127.0.0.1:8711: address in use".to_string(),
        );
        assert!(registry.any_down());
        let snapshot = registry.snapshot();
        assert_eq!(
            snapshot.get(&CategoryId::Work),
            Some(&ListenerState::Serving)
        );
        assert!(matches!(
            snapshot.get(&CategoryId::Emb),
            Some(ListenerState::Down { reason }) if reason.contains("8711")
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

    #[test]
    fn clones_share_the_verdicts_and_the_boot_epoch() {
        // The tray and the supervisors hold clones of one registry; if a clone got its
        // own map (or its own grace epoch) the tray could read a stale all-`Pending`
        // world while every listener served.
        let registry = ListenerRegistry::new();
        let clone = registry.clone();
        let created = registry.created_at();
        assert_eq!(clone.created_at(), created, "clones share the grace epoch");
        clone.mark_serving(CategoryId::Work);
        assert_eq!(
            registry.snapshot().get(&CategoryId::Work),
            Some(&ListenerState::Serving),
            "a verdict published through a clone is visible through the original"
        );
    }
}
