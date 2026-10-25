//! Provenance-typed ownership (DESIGN Step 1, LOCKED: B + C).
//!
//! Every owned resource carries a [`SpawnTicket`]. "The harness never kills what it did
//! not start" is a *type*, not a check at the kill site: the kill path accepts a
//! [`Provenance::Owned`] ticket and nothing else — there is no API to kill "whatever is
//! on port X". Adoption is scrapped (operator 2026-10-24): a slot occupant is always
//! harness-spawned; externals are display-only and can never enter a slot.

/// What kind of resource backs a slot occupant (Step 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceKind {
    /// A Windows child process tree supervised by a Job Object (Step 8 C).
    ChildProcess,
    /// A docker container started through the operator's WSL scripts, labeled for
    /// reap-by-label at startup (Step 8 D).
    Container { name: String },
}

/// Mandatory metadata on every harness-owned resource (Step 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnTicket {
    /// Monotonic generation; distinguishes this spawn from any stale predecessor on the
    /// same port (Step 17 newtype rationale).
    pub generation: u64,
    /// How the resource is backed.
    pub kind: ResourceKind,
    /// OS process id (`wsl.exe` for containers).
    pub pid: u32,
    /// The port the resource serves.
    pub port: u16,
}

/// Who a resource belongs to. `External` and `Protected` are unkillable *types*;
/// the kill path cannot even name them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provenance {
    /// Started by this harness in this lifetime; only this variant is killable.
    Owned(SpawnTicket),
    /// A manual server shown in the display list (Step 13); never touched.
    External,
    /// A protected container (e.g. `mara-lab`); never touched, even on request.
    Protected,
}

impl Provenance {
    /// Whether the kill path may act on this resource.
    ///
    /// # Returns
    ///
    /// `true` only for [`Provenance::Owned`]; externals and protected entries are
    /// refused by construction (G4, Step 1).
    pub fn can_kill(&self) -> bool {
        matches!(self, Provenance::Owned(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket() -> SpawnTicket {
        SpawnTicket {
            generation: 3,
            kind: ResourceKind::ChildProcess,
            pid: 4242,
            port: 18109,
        }
    }

    #[test]
    fn only_owned_resources_are_killable() {
        assert!(Provenance::Owned(ticket()).can_kill());
        assert!(!Provenance::External.can_kill());
        assert!(!Provenance::Protected.can_kill());
    }

    #[test]
    fn tickets_carry_generation_and_backing_kind() {
        let t = ticket();
        assert_eq!(t.generation, 3);
        assert_eq!(t.kind, ResourceKind::ChildProcess);
        let container = SpawnTicket {
            generation: 4,
            kind: ResourceKind::Container {
                name: "ninfer-a".to_string(),
            },
            pid: 100,
            port: 18304,
        };
        assert_eq!(
            container.kind,
            ResourceKind::Container {
                name: "ninfer-a".to_string()
            }
        );
    }
}
