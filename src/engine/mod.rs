//! Engine adapters and launch specs (DESIGN Step 7 — data-first, code as escape hatch).
//!
//! The primary artifact is an engine *template* resolved from data; the trait stays
//! because the mock engine in tests is exactly such an adapter. `LaunchSpec` must be
//! constructible from data alone — no adapter may hide a decision a template could
//! express. Readiness is typed, never a string heuristic (defect D7/E1 class).

pub mod llama;
pub mod mock;
pub mod ninfer;
pub mod strata;

use crate::model::ModelEntry;
use crate::ownership::SpawnTicket;
use std::collections::BTreeMap;
use std::path::PathBuf;
use thiserror::Error;

/// Why an adapter could not produce a [`LaunchSpec`].
#[derive(Debug, Clone, PartialEq, Error)]
pub enum EngineError {
    /// The model entry lacks a key the engine requires to launch.
    #[error("model `{model}` is missing required field `{field}`")]
    MissingField {
        /// Harness name of the model.
        model: String,
        /// Missing key.
        field: String,
    },
    /// The adapter rejected the model for an engine-specific reason.
    #[error("engine `{engine}` rejected model `{model}`: {reason}")]
    Rejected {
        /// Engine id.
        engine: String,
        /// Harness name of the model.
        model: String,
        /// Engine-specific explanation (G6).
        reason: String,
    },
}

/// Readiness as typed data: one HTTP expectation, or two in sequence (Step 7 sketch).
///
/// Placeholders in `path` are confined to argv-style substitution — there is no string
/// mini-language for liveness or kill here (typed `kill`/`liveness` arrive with the
/// template schema, item 9).
#[derive(Debug, Clone, PartialEq)]
pub enum ReadinessSpec {
    /// `GET path` must answer `expect_status`.
    Http { path: String, expect_status: u16 },
    /// `GET path` must answer `expect_status` **and** the JSON body's `field` must
    /// not be an explicit `false`. A missing field, `null`, a non-bool, or a body
    /// that is not JSON at all counts as ready — the flag is honored only when the
    /// server actually reports it (strata `/health` answers 200 before the weights
    /// finish loading; v3.3 `engine.rs` parity, fail-open on unparseable bodies).
    HttpFlag {
        /// Probe path.
        path: String,
        /// Required status code.
        expect_status: u16,
        /// Top-level JSON boolean that must not be `false`.
        field: String,
    },
    /// Both specs in order (e.g. `/health` then `/v1/models` for ninfer).
    Then(Box<ReadinessSpec>, Box<ReadinessSpec>),
}

/// Everything the engine-blind spawn/kill/reap machinery needs (Step 7).
#[derive(Debug, Clone, PartialEq)]
pub struct LaunchSpec {
    /// Argv, fully substituted.
    pub argv: Vec<String>,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// Harness-declared environment for the child, applied last over the minimal
    /// OS baseline the runtime builds (`process::OS_ENV_BASELINE`, §5/E3): a
    /// declared var overrides its baseline twin, and nothing else from the
    /// harness environment is inherited.
    pub env: BTreeMap<String, String>,
    /// Log file base name; the process module appends the epoch (Step 8).
    pub log_name: String,
    /// Readiness contract.
    pub readiness: ReadinessSpec,
    /// Detached WSL container backing this engine (ninfer): the job object kills
    /// only the `wsl.exe` client, so the runtime must additionally `docker rm -f`
    /// this name (Step 8 extension, 2026-10-26). `None` for process engines.
    pub container: Option<String>,
}

/// The shared WSL client prefix: `wsl.exe -d <distro> -u <user> --`. Launch
/// (ninfer autostart) and teardown (`docker rm -f`) must address the same
/// distro/user or unload reaps the wrong place (review, 2026-10-26).
#[must_use]
pub fn wsl_prefix(distro: &str, user: &str) -> Vec<String> {
    vec![
        "wsl.exe".to_string(),
        "-d".to_string(),
        distro.to_string(),
        "-u".to_string(),
        user.to_string(),
        "--".to_string(),
    ]
}

/// Tri-state probe outcome (Step 7): the scheduler decides what a missing field means
/// per engine, in one place — with the reason surfaced in the job record (G6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Probe {
    /// Ready to serve.
    Ready,
    /// Not yet; keep waiting inside the deadline.
    NotYet { reason: String },
    /// Terminal failure; the reason goes to the job record.
    Failed { reason: String },
}

/// Produces launch specs for one engine id (Step 7). The harness assigns the engine
/// port from the slot plan (config `engine_ports`); the adapter embeds it in argv.
pub trait EngineAdapter: Send + Sync {
    /// The `engine:` name this adapter serves.
    fn engine_id(&self) -> &str;

    /// Builds the launch spec for `model`, serving loopback on `port`.
    ///
    /// # Errors
    ///
    /// [`EngineError::MissingField`] / [`EngineError::Rejected`] as documented; these
    /// become `Refused` job records, never panics.
    fn spec(&self, model: &ModelEntry, port: u16) -> Result<LaunchSpec, EngineError>;
}

/// Shared HTTP readiness probe (the semantics both runtimes use, Step 7).
///
/// `Then` chains probe steps in order: the first non-`Ready` step short-circuits.
pub async fn http_probe(client: &reqwest::Client, port: u16, readiness: &ReadinessSpec) -> Probe {
    match readiness {
        ReadinessSpec::Http { .. } | ReadinessSpec::HttpFlag { .. } => {
            probe_one(client, port, readiness).await
        }
        ReadinessSpec::Then(first, second) => {
            let first = probe_one(client, port, first).await;
            if first != Probe::Ready {
                return first;
            }
            probe_one(client, port, second).await
        }
    }
}

/// One leaf expectation (`Http` or `HttpFlag`); a nested `Then` is a contract bug.
async fn probe_one(client: &reqwest::Client, port: u16, spec: &ReadinessSpec) -> Probe {
    let (path, expect) = match spec {
        ReadinessSpec::Http {
            path,
            expect_status,
        }
        | ReadinessSpec::HttpFlag {
            path,
            expect_status,
            ..
        } => (path.clone(), *expect_status),
        ReadinessSpec::Then(_, _) => {
            return Probe::Failed {
                reason: "cannot probe a nested Then as a leaf".to_string(),
            };
        }
    };
    let url = format!("http://127.0.0.1:{port}{path}");
    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) => {
            return Probe::NotYet {
                reason: format!("no answer yet: {error}"),
            };
        }
    };
    if response.status().as_u16() != expect {
        return Probe::NotYet {
            reason: format!("health answered {}", response.status()),
        };
    }
    let ReadinessSpec::HttpFlag { field, .. } = spec else {
        return Probe::Ready;
    };
    // Status matched and a flag is required: honor `field` only when the body is
    // JSON with `field == false` (v3.3 fail-open on anything else).
    match response.json::<serde_json::Value>().await {
        Ok(body) if matches!(body.get(field), Some(serde_json::Value::Bool(false))) => {
            Probe::NotYet {
                reason: format!("engine reports `{field}: false`"),
            }
        }
        _ => Probe::Ready,
    }
}

/// Why a spawn/kill/release step failed at the process layer (Step 8).
#[derive(Debug, Clone, PartialEq, Error)]
pub enum SpawnError {
    /// The engine could not be started.
    #[error("spawn failed: {reason}")]
    SpawnFailed {
        /// What went wrong.
        reason: String,
    },
    /// The kill path failed for a resource the harness owns.
    #[error("kill failed for {holder}: {reason}")]
    KillFailed {
        /// Human name of the resource (model or container).
        holder: String,
        /// What went wrong.
        reason: String,
    },
    /// The ticket names a port whose live entry is a newer spawn, so this ticket is
    /// stale and the child it identified is already gone. Reported rather
    /// silently-ignored: killing or confirming the successor would be an unowned kill
    /// wearing a harness ticket.
    #[error(
        "stale ticket for port {port}: carries generation {expected}, live generation {found}"
    )]
    StaleTicket {
        /// Port the ticket names.
        port: u16,
        /// Generation this ticket carries.
        expected: u64,
        /// Generation the table holds for that port.
        found: u64,
    },
}

/// A started engine instance (Step 8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spawned {
    /// Port the engine now serves on.
    pub port: u16,
    /// OS process id, for witness attribution (0 when the runtime has no process).
    pub pid: u32,
}

/// Process-layer operations, driven by the scheduler actor but never executed inline
/// in the actor loop (Step 2: no I/O in the actor; Step 8: one implementation).
///
/// The production implementation (hidden spawn, Job Objects, log files, container
/// labels) is the future `process.rs`; the mock implementation used by tests lives in
/// `tests/mock_runtime.rs` (Step 19).
#[async_trait::async_trait]
pub trait EngineRuntime: Send + Sync {
    /// Starts an engine for `spec`; the ticket stamps provenance on the result.
    ///
    /// # Errors
    ///
    /// [`SpawnError::SpawnFailed`] when the engine cannot be started.
    async fn spawn(&self, spec: &LaunchSpec, ticket: SpawnTicket) -> Result<Spawned, SpawnError>;

    /// Stops an owned resource. Only ever called with an owned ticket (Step 1).
    ///
    /// # Errors
    ///
    /// [`SpawnError::KillFailed`] when the stop command itself fails.
    async fn kill(&self, ticket: &SpawnTicket) -> Result<(), SpawnError>;

    /// G12 release evidence: `true` only when death is *proven* (handle signaled /
    /// job tree empty / container gone + port closed).
    ///
    /// # Errors
    ///
    /// [`SpawnError::KillFailed`] when the evidence check cannot be performed.
    async fn release_confirmed(&self, ticket: &SpawnTicket) -> Result<bool, SpawnError>;

    /// One readiness attempt against `readiness` (Step 7 tri-state).
    async fn probe(&self, port: u16, readiness: &ReadinessSpec) -> Probe;

    /// The engine's self-reported OpenAI model id (`GET /v1/models`, first entry),
    /// discovered at readiness — the proxy rewrite target (Step 10, F-4 fix).
    /// `None` when the engine does not answer or reports nothing: callers fall
    /// back to the harness name.
    async fn discover_model_id(&self, port: u16) -> Option<String>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;
    use std::sync::atomic::{AtomicU16, Ordering};
    use std::sync::Arc;

    /// One-shot HTTP server on an ephemeral port answering `/health` with a fixed
    /// status and body; returns the bound port (kept alive by the spawned task).
    async fn serve_health(status: u16, body: &'static str) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral");
        let port = listener.local_addr().expect("addr").port();
        let code = Arc::new(AtomicU16::new(status));
        let app = {
            let code = Arc::clone(&code);
            axum::Router::new().route(
                "/health",
                axum::routing::get(move || async move {
                    (
                        axum::http::StatusCode::from_u16(code.load(Ordering::Relaxed))
                            .unwrap_or(axum::http::StatusCode::OK),
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        body,
                    )
                        .into_response()
                }),
            )
        };
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        port
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("client")
    }

    #[tokio::test]
    async fn http_flag_false_is_not_ready() {
        let port = serve_health(200, r#"{"loaded":false}"#).await;
        let spec = ReadinessSpec::HttpFlag {
            path: "/health".to_string(),
            expect_status: 200,
            field: "loaded".to_string(),
        };
        assert!(matches!(
            http_probe(&client(), port, &spec).await,
            Probe::NotYet { .. }
        ));
    }

    #[tokio::test]
    async fn http_flag_true_is_ready() {
        let port = serve_health(200, r#"{"loaded":true}"#).await;
        let spec = ReadinessSpec::HttpFlag {
            path: "/health".to_string(),
            expect_status: 200,
            field: "loaded".to_string(),
        };
        assert_eq!(http_probe(&client(), port, &spec).await, Probe::Ready);
    }

    #[tokio::test]
    async fn http_flag_missing_or_null_or_nonbool_is_ready_v33_parity() {
        let spec = ReadinessSpec::HttpFlag {
            path: "/health".to_string(),
            expect_status: 200,
            field: "loaded".to_string(),
        };
        for body in [
            r#"{"status":"ok"}"#,
            r#"{"loaded":null}"#,
            r#"{"loaded":"true"}"#,
            "not json at all",
        ] {
            let port = serve_health(200, body).await;
            assert_eq!(
                http_probe(&client(), port, &spec).await,
                Probe::Ready,
                "body `{body}` must count as ready"
            );
        }
    }

    #[tokio::test]
    async fn http_flag_wrong_status_is_not_ready() {
        let port = serve_health(503, r#"{"loaded":true}"#).await;
        let spec = ReadinessSpec::HttpFlag {
            path: "/health".to_string(),
            expect_status: 200,
            field: "loaded".to_string(),
        };
        assert!(matches!(
            http_probe(&client(), port, &spec).await,
            Probe::NotYet { .. }
        ));
    }

    #[tokio::test]
    async fn then_chains_in_order() {
        let port = serve_health(200, r#"{"loaded":false}"#).await;
        let spec = ReadinessSpec::Then(
            Box::new(ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200,
            }),
            Box::new(ReadinessSpec::HttpFlag {
                path: "/health".to_string(),
                expect_status: 200,
                field: "loaded".to_string(),
            }),
        );
        assert!(matches!(
            http_probe(&client(), port, &spec).await,
            Probe::NotYet { .. }
        ));
    }

    #[tokio::test]
    async fn nested_then_as_leaf_is_a_failed_contract_bug() {
        let spec = ReadinessSpec::Then(
            Box::new(ReadinessSpec::Then(
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
                Box::new(ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                }),
            )),
            Box::new(ReadinessSpec::Http {
                path: "/health".to_string(),
                expect_status: 200,
            }),
        );
        assert!(matches!(
            http_probe(&client(), 1, &spec).await,
            Probe::Failed { .. }
        ));
    }
}
