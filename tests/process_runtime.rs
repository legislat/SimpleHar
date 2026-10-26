//! Process-runtime integration tests: a real hidden child process under a Job
//! Object, real HTTP readiness, real G12 release evidence. The child is a tiny
//! Python HTTP server (`python` is present on this box); the test skips with a note
//! when it is not.

use simplehar_se::engine::{EngineRuntime, LaunchSpec, Probe, ReadinessSpec, Spawned};
use simplehar_se::ownership::{ResourceKind, SpawnTicket};
use simplehar_se::process::ProcessRuntime;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Test port, outside every SE plan and protected range (HANDOFF §3).
const TEST_PORT: u16 = 18_190;

const PY_SERVER: &str = r#"
import http.server
class H(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = b'{"loaded": true}'
        self.send_response(200)
        self.send_header('Content-Type', 'application/json')
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
http.server.HTTPServer(('127.0.0.1', PORT), H).serve_forever()
"#;

fn spec() -> LaunchSpec {
    let code = PY_SERVER.replace("PORT", &TEST_PORT.to_string());
    LaunchSpec {
        argv: vec!["python".to_string(), "-c".to_string(), code],
        cwd: PathBuf::from("."),
        env: BTreeMap::new(),
        log_name: "process-runtime-test".to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
    }
}

fn ticket(port: u16) -> SpawnTicket {
    SpawnTicket {
        generation: 1,
        kind: ResourceKind::ChildProcess,
        pid: 0,
        port,
    }
}

async fn wait_ready(runtime: &ProcessRuntime, port: u16, seconds: u64) -> Probe {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        let probe = runtime
            .probe(
                port,
                &ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                },
            )
            .await;
        if matches!(probe, Probe::Ready) || std::time::Instant::now() > deadline {
            return probe;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn confirm_release(runtime: &ProcessRuntime, ticket: &SpawnTicket, seconds: u64) -> bool {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if runtime
            .release_confirmed(ticket)
            .await
            .expect("evidence check runs")
        {
            return true;
        }
        if std::time::Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn spawns_a_real_child_probes_ready_and_confirms_release() {
    let runtime = ProcessRuntime::new();
    let spawned: Spawned = match runtime.spawn(&spec(), ticket(TEST_PORT)).await {
        Ok(spawned) => spawned,
        Err(error) => {
            eprintln!("python unavailable or spawn failed ({error}); skipping");
            return;
        }
    };
    assert_eq!(
        spawned.port, TEST_PORT,
        "the runtime serves the assigned port"
    );
    assert_eq!(runtime.engine_count(), 1);

    // A log file must exist for the child (Step 8).
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut logged = false;
    while std::time::Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir("logs").or_else(|_| {
            std::fs::read_dir(
                std::env::current_exe()
                    .ok()
                    .and_then(|e| e.parent().map(|p| p.join("logs")))
                    .unwrap_or_else(|| PathBuf::from("logs")),
            )
        }) {
            if entries.filter_map(Result::ok).any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("process-runtime-test")
            }) {
                logged = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(logged, "child stdout/stderr must land in logs/");

    let probe = wait_ready(&runtime, TEST_PORT, 15).await;
    assert_eq!(
        probe,
        Probe::Ready,
        "the python child must answer /health with 200"
    );

    runtime.kill(&ticket(TEST_PORT)).await.expect("kill runs");
    assert!(
        confirm_release(&runtime, &ticket(TEST_PORT), 10).await,
        "G12 evidence: child exited AND port closed"
    );
    assert_eq!(
        runtime.engine_count(),
        0,
        "confirmed release cleans the table"
    );
}

#[tokio::test]
async fn killing_a_port_we_do_not_own_is_a_noop() {
    // Step 1: never kill what it did not start — an unknown ticket is not an error.
    let runtime = ProcessRuntime::new();
    runtime
        .kill(&ticket(59_999))
        .await
        .expect("unknown ticket is a no-op kill");
    assert!(runtime
        .release_confirmed(&ticket(59_999))
        .await
        .expect("evidence runs"));
}

#[test]
fn single_instance_guard_refuses_a_second_holder() {
    let first = simplehar_se::process::SingleInstanceGuard::acquire();
    if first.is_err() {
        eprintln!("a SimpleHar SE instance is running; skipping guard test");
        return;
    }
    let second = simplehar_se::process::SingleInstanceGuard::acquire();
    assert!(second.is_err(), "a second harness must be refused (A10)");
}

#[tokio::test]
async fn probe_fails_fast_when_the_child_already_exited() {
    // A child that dies immediately must fail readiness now, not after the deadline.
    let runtime = ProcessRuntime::new();
    let spec = LaunchSpec {
        argv: vec![
            "python".to_string(),
            "-c".to_string(),
            "import sys; sys.exit(3)".to_string(),
        ],
        cwd: PathBuf::from("."),
        env: BTreeMap::new(),
        log_name: "process-runtime-dies".to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
    };
    let ticket = ticket(TEST_PORT + 1);
    if runtime.spawn(&spec, ticket.clone()).await.is_err() {
        eprintln!("python unavailable; skipping");
        return;
    }
    let started = std::time::Instant::now();
    let probe = loop {
        let probe = runtime
            .probe(
                ticket.port,
                &ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                },
            )
            .await;
        if matches!(probe, Probe::Failed { .. }) {
            break probe;
        }
        if started.elapsed() > Duration::from_secs(10) {
            panic!("dead child must fail readiness fast; last probe: {probe:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let Probe::Failed { reason } = probe else {
        unreachable!("checked above")
    };
    assert!(reason.contains("exited early"), "was: {reason}");
    runtime.kill(&ticket).await.expect("cleanup kill");
}
