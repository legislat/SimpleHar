//! Provenance-typed ownership (DESIGN Step 1, LOCKED: B + C).
//!
//! Every owned resource carries a [`SpawnTicket`]. "The harness never kills what it did
//! not start" is a *type*, not a check at the kill site: the kill path accepts a
//! [`Provenance::Owned`] ticket and nothing else — there is no API to kill "whatever is
//! on port X". Adoption is scrapped (operator 2026-10-24): a slot occupant is always
//! harness-spawned; externals are display-only and can never enter a slot.
//!
//! Containers outlive the process that started them, so their provenance must too:
//! [`ContainerClaims`] is the persisted claim ledger written at spawn and read at boot,
//! and [`boot_reap_decision`] is the single rule the boot reclaim evaluates against it
//! (chunk 10 [1], 2026-10-27). Both reaping paths — the spawn-time pre-reap and the boot
//! reclaim — address a name only if the ledger accepts it: the spawn-time pre-reap is
//! gated on [`ContainerClaims::claim`], the boot reclaim on [`boot_reap_decision`], and
//! both — with the adapter spec — validate the name through the one function
//! [`valid_container_name`], so the rules can never drift.

use std::collections::BTreeSet;
use std::path::PathBuf;

/// The engine id whose models may own docker containers: the **only** engine boot
/// reclaim may ever address (operator policy, chunk 10 [1]). The ninfer adapter's
/// `engine_id` returns this constant, so registry, scanner, and reclaim compare the
/// same token.
pub const NINFER_ENGINE: &str = "ninfer";

/// Shortest argument docker tries to resolve as a container **ID prefix** instead of a
/// name (chunk 10 [4]): `docker rm -f 1234abc` deletes whichever unrelated container's
/// id happens to start with those characters, in the operator's distro, with workloads
/// on it. A name of that shape is therefore never claimable and never reaped, whatever
/// the ledger says.
const MIN_ID_PREFIX_LEN: usize = 4;

/// Whether `name` is shaped like a docker container-ID prefix rather than a name.
fn id_shaped(name: &str) -> bool {
    name.len() >= MIN_ID_PREFIX_LEN && name.chars().all(|c| c.is_ascii_hexdigit())
}

/// Docker's own name rule plus the ID-prefix-shape exclusion, applied as a
/// defense-in-depth guard before the value ever reaches `docker rm -f` argv (review,
/// 2026-10-26; chunk 10 [1] and [4], 2026-10-27). Lives here — not in the adapter —
/// because the spawn path (`NinferAdapter::spec`), the spawn-time pre-reap through
/// [`ContainerClaims::claim`], and the boot reclaim decision must all validate through
/// this one function, so no reaping path can accept a name another one refuses.
#[must_use]
pub fn valid_container_name(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !id_shaped(name)
}

/// Why boot reclaim may (or may not) destroy a catalog-declared container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReapDecision {
    /// All three ownership gates passed: ninfer engine, docker-legal name, and a
    /// recorded harness spawn claim.
    Reap,
    /// A gate failed; `reason` names it in the boot warning. Absence of evidence is
    /// a skip — the harness never destroys on suspicion.
    Skip {
        /// Human-readable gate that failed, for the warn line.
        reason: &'static str,
    },
}

/// Skip reasons as constants: tests pin them, the warn line prints them, and the
/// wording cannot drift between the two.
pub const SKIP_NOT_NINFER: &str =
    "not a ninfer-engine model: the harness never starts containers for other engines";
pub const SKIP_INVALID_NAME: &str =
    "not a docker-legal container name, or it is shaped like a docker container-ID prefix";
pub const SKIP_UNCLAIMED: &str = "no harness spawn claim on record for this name";

/// The boot-reclaim rule, evaluated once per catalog-declared container name. Firing
/// requires all three gates (chunk 10 [1], operator policy 2026-10-27): (a) the
/// declaring model runs on the ninfer engine, (b) the name passes the very validation
/// the spawn path applies — docker-legal and not a container-ID shape, through
/// [`valid_container_name`] — and (c) [`ContainerClaims`] positively records that this
/// harness started the name and never released it.
///
/// # Arguments
///
/// * `engine` - the `engine:` value of the yaml that declares the name.
/// * `name` - the declared `ninfer_container` value (untrimmed shapes are refused).
/// * `claimed` - whether the persistent claim ledger holds `name`.
///
/// # Returns
///
/// [`ReapDecision::Reap`] only when every gate holds; otherwise
/// [`ReapDecision::Skip`] naming the failed gate.
#[must_use]
pub fn boot_reap_decision(engine: &str, name: &str, claimed: bool) -> ReapDecision {
    if engine != NINFER_ENGINE {
        return ReapDecision::Skip {
            reason: SKIP_NOT_NINFER,
        };
    }
    if !valid_container_name(name) {
        return ReapDecision::Skip {
            reason: SKIP_INVALID_NAME,
        };
    }
    if !claimed {
        return ReapDecision::Skip {
            reason: SKIP_UNCLAIMED,
        };
    }
    ReapDecision::Reap
}

/// The persisted record of container names this harness started: the ownership
/// evidence behind [`boot_reap_decision`]. Written (atomically, temp + rename) when
/// [`ProcessRuntime`](crate::process::ProcessRuntime) spawns a container, cleared when
/// a release is confirmed, and read at boot — so a container can only be reclaimed if
/// an earlier lifetime of *this* harness claimed it and died before releasing it. A
/// missing or corrupt ledger reads as no claims: absence of evidence is absence of
/// permission, never a guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerClaims {
    claims: BTreeSet<String>,
    path: PathBuf,
}

impl ContainerClaims {
    /// Loads the ledger at `path`; anything unreadable becomes the empty set (warned,
    /// not fatal — boot proceeds and reclaim simply stands down for every name).
    #[must_use]
    pub fn load(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let claims = match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<BTreeSet<String>>(&bytes) {
                Ok(claims) => claims,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        path = %path.display(),
                        "claim ledger unreadable; treating every container as unclaimed"
                    );
                    BTreeSet::new()
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    path = %path.display(),
                    "claim ledger unreadable; treating every container as unclaimed"
                );
                BTreeSet::new()
            }
        };
        ContainerClaims { claims, path }
    }

    /// Whether `name` carries a live harness claim.
    #[must_use]
    pub fn contains(&self, name: &str) -> bool {
        self.claims.contains(name)
    }

    /// Records that this harness started `name`, persisting the ledger. Only
    /// docker-legal names are claimable — the one validation rule shared with the
    /// spawn path and the boot decision, so an invalid name can never be claimed now
    /// nor reaped later.
    ///
    /// # Returns
    ///
    /// `true` when the claim was recorded, `false` when the name was refused as
    /// invalid (also warned).
    pub fn claim(&mut self, name: &str) -> bool {
        if !valid_container_name(name) {
            tracing::warn!(
                container = name,
                "refusing to claim an invalid docker container name"
            );
            return false;
        }
        if self.claims.insert(name.to_string()) {
            self.save();
        }
        true
    }

    /// Drops the claim for `name` (a confirmed release), persisting the ledger.
    pub fn unclaim(&mut self, name: &str) {
        if self.claims.remove(name) {
            self.save();
        }
    }

    /// Writes the ledger through a temp file + rename so a crash mid-write can never
    /// truncate the ownership evidence. Best-effort: a failed save warns and the
    /// in-memory set stays authoritative for this lifetime.
    fn save(&self) {
        let result = (|| -> std::io::Result<()> {
            let bytes = serde_json::to_vec(&self.claims).map_err(std::io::Error::other)?;
            let temp = self.path.with_extension("json.tmp");
            std::fs::write(&temp, &bytes)?;
            std::fs::rename(&temp, &self.path)?;
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(
                error = %error,
                path = %self.path.display(),
                "cannot persist the container claim ledger"
            );
        }
    }
}

/// What kind of resource backs a slot occupant (Step 8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceKind {
    /// A Windows child process tree supervised by a Job Object (Step 8 C).
    ChildProcess,
    /// A docker container started through the operator's WSL scripts. The label
    /// is the reap authority: the runtime removes exactly this name on release
    /// and reconciles it pre-spawn (Step 8 D, ninfer port 2026-10-26).
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
    /// A resource the harness must never touch, even on request.
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

    #[test]
    fn boot_reap_reclaims_only_a_claimed_ninfer_container() {
        assert_eq!(
            boot_reap_decision(NINFER_ENGINE, "SE-nin-bonsai", true),
            ReapDecision::Reap,
            "all three gates hold: the harness started this container and never released it"
        );
    }

    #[test]
    fn boot_reap_skips_a_container_it_never_claimed() {
        // The chunk-10 [1] defect: a hand-started container whose name happens to sit
        // in some yaml must NEVER be destroyed. Absence of a claim is absence of
        // permission.
        assert_eq!(
            boot_reap_decision(NINFER_ENGINE, "SE-nin-bonsai", false),
            ReapDecision::Skip {
                reason: SKIP_UNCLAIMED
            }
        );
    }

    #[test]
    fn boot_reap_skips_a_name_declared_by_a_non_ninfer_engine() {
        // A stray `ninfer_container:` key on a llama/strata yaml is reference data
        // (README:105), never a reap order.
        for engine in ["llama", "strata", "mock", "Ninfer", ""] {
            assert_eq!(
                boot_reap_decision(engine, "SE-nin-bonsai", true),
                ReapDecision::Skip {
                    reason: SKIP_NOT_NINFER
                },
                "engine `{engine}` may not own containers"
            );
        }
    }

    #[test]
    fn boot_reap_skips_a_name_the_spawn_path_would_refuse() {
        // The same validation the spawn path applies (chunk 10 [1] gate b): an
        // illegal shape cannot slip through boot even if a claim existed.
        for name in ["", "-v", "_x", "a b", "1234abc; rm -rf /"] {
            assert_eq!(
                boot_reap_decision(NINFER_ENGINE, name, true),
                ReapDecision::Skip {
                    reason: SKIP_INVALID_NAME
                },
                "name `{name}` is not docker-legal"
            );
        }
    }

    #[test]
    fn boot_reap_skips_a_name_docker_would_resolve_as_a_container_id() {
        // Chunk 10 [4]: `docker rm -f 1234abc` deletes whichever *unrelated* container's
        // id merely starts with those characters, so an ID-shaped name is refused by the
        // one rule the spawn path and the reclaim share. A short hex string stays legal:
        // docker only attempts id-prefix resolution from `MIN_ID_PREFIX_LEN` up, and the
        // adapter's own names ("c") depend on that.
        let dir = crate::testutil::temp_dir("claims-id-shaped");
        let mut claims = ContainerClaims::load(dir.join("ninfer-claims.json"));
        for name in ["1234abc", "beef", "deadbeef", "0aB9", "1234"] {
            assert!(
                !valid_container_name(name),
                "name `{name}` is a container-ID shape"
            );
            assert_eq!(
                boot_reap_decision(NINFER_ENGINE, name, true),
                ReapDecision::Skip {
                    reason: SKIP_INVALID_NAME
                },
                "the shape gate must fire before the claim gate: `{name}` is never reaped, \
                 even if a stale ledger still holds it"
            );
            assert!(
                !claims.claim(name) && !claims.contains(name),
                "an ID-shaped name can never even be claimed: `{name}`"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        for name in ["c", "abc", "1234abc-dead", "SE-nin-bonsai", "ninfer-0"] {
            assert!(valid_container_name(name), "`{name}` is a plain name");
        }
    }

    #[test]
    fn claim_ledger_round_trips_through_disk_and_refuses_invalid_names() {
        let dir = crate::testutil::temp_dir("claims");
        let path = dir.join("ninfer-claims.json");
        let mut claims = ContainerClaims::load(&path);
        assert!(
            !claims.contains("ninfer-a"),
            "a fresh ledger claims nothing"
        );
        assert!(claims.claim("ninfer-a"));
        assert!(!claims.claim("-bad"), "invalid names are never claimable");
        drop(claims);
        // Re-read: the boot path sees exactly what the spawn path wrote.
        let reloaded = ContainerClaims::load(&path);
        assert!(reloaded.contains("ninfer-a"));
        assert!(!reloaded.contains("-bad"));
        let mut reloaded = reloaded;
        reloaded.unclaim("ninfer-a");
        assert!(!ContainerClaims::load(&path).contains("ninfer-a"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
