//! Process-runtime integration tests: a real hidden child process under a Job
//! Object, real HTTP readiness, real G12 release evidence. The child is a tiny
//! Python HTTP server (`python` is present on this box); the test skips with a note
//! when it is not.

use simplehar_se::engine::{EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError, Spawned};
use simplehar_se::ownership::{ResourceKind, SpawnTicket};
use simplehar_se::process::ProcessRuntime;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

/// Test port, outside every SE plan and protected range (HANDOFF §3). Each test in
/// this binary takes its own offset: `#[tokio::test]` runs them concurrently, so two
/// children may never be given one port. `18_195` stays reserved for the lib binary's
/// own engine-child test.
const TEST_PORT: u16 = 18_190;

/// Log namespace owned by the capture test. It may delete any file starting with this
/// prefix because nothing else writes it (FINDINGS-20 [1]: a name left over from an
/// earlier run is not evidence that this run's child produced bytes).
const CAPTURE_PREFIX: &str = "process-runtime-logcapture";

/// The line only the child itself can put in its log file: the content a capture
/// assertion must demand. `log_message` stays a `pass`, so nothing else is printed.
const LOG_MARKER: &str = "SE-CHILD-OUTPUT-MARKER";

const PY_SERVER: &str = r#"
import http.server, sys
print('MARKER', file=sys.stderr, flush=True)
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

fn spec(port: u16, log_name: &str) -> LaunchSpec {
    let code = PY_SERVER
        .replace("PORT", &port.to_string())
        .replace("MARKER", LOG_MARKER);
    LaunchSpec {
        argv: vec!["python".to_string(), "-c".to_string(), code],
        cwd: PathBuf::from("."),
        env: BTreeMap::new(),
        log_name: log_name.to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
    }
}

/// The directory `ProcessRuntime::new` sends child output to (next to the exe, never
/// the cwd) — resolved by the same rule so the test reads what the product writes.
fn logs_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("logs")))
        .unwrap_or_else(|| PathBuf::from("logs"))
}

/// Names and sizes of every file in this run's own log namespace, before the spawn.
/// `std::fs::metadata` rather than `DirEntry::metadata`: the directory enumeration
/// cache carries the size the file had when the entry was read, which is 0 for a log
/// the child is still appending to.
fn snapshot_logs(prefix: &str) -> BTreeMap<String, u64> {
    let mut seen = BTreeMap::new();
    if let Ok(entries) = std::fs::read_dir(logs_dir()) {
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) {
                let len = std::fs::metadata(entry.path())
                    .map(|m| m.len())
                    .unwrap_or_default();
                seen.insert(name, len);
            }
        }
    }
    seen
}

/// Deletes this test's own stale log files. Legitimate because the prefix is owned by
/// this test alone; other suites' namespaces are never touched.
fn clear_own_logs(prefix: &str) {
    if let Ok(entries) = std::fs::read_dir(logs_dir()) {
        for entry in entries.filter_map(Result::ok) {
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
}

/// The first file that carries this prefix, was absent from `before`, and holds the
/// child's marker line — i.e. capture proven by creation *and* content, never by name.
fn new_log_with_output(before: &BTreeMap<String, u64>, prefix: &str) -> Option<(String, u64)> {
    let entries = std::fs::read_dir(logs_dir()).ok()?;
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(prefix) || before.contains_key(&name) {
            continue;
        }
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            if text.contains(LOG_MARKER) {
                let len = std::fs::metadata(entry.path())
                    .map(|m| m.len())
                    .unwrap_or_default();
                return Some((name, len));
            }
        }
    }
    None
}

/// Polls for a new log file carrying the child's own bytes, failing at the deadline.
async fn await_new_log_with_output(
    before: &BTreeMap<String, u64>,
    prefix: &str,
    seconds: u64,
) -> Option<(String, u64)> {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Some(found) = new_log_with_output(before, prefix) {
            return Some(found);
        }
        if std::time::Instant::now() > deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Polls `probe` until the runtime reports the child exited, which is the only
/// observation of "the process this runtime started is gone" that does not also
/// consume the release evidence under test.
async fn await_child_exited(runtime: &ProcessRuntime, port: u16, seconds: u64) -> String {
    let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
    loop {
        if let Probe::Failed { reason } = runtime
            .probe(
                port,
                &ReadinessSpec::Http {
                    path: "/health".to_string(),
                    expect_status: 200,
                },
            )
            .await
        {
            if reason.contains("exited early") {
                return reason;
            }
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the killed child must exit (probe fast-fail) within {seconds}s"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
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

/// True only when a spawn error is the OS saying the program itself is absent —
/// the single legitimate reason to skip these tests (FINDINGS-FIXROUND2 [5]).
/// `ProcessRuntime::spawn` wraps `std::process::Command::spawn`'s error in
/// `SpawnFailed { reason: "cannot spawn {program}: {e}" }` (src/process.rs:638),
/// and on Windows a missing program yields `NotFound`, `raw_os_error() == None`,
/// Display "program not found" (verified by executing that exact spawn shape on
/// this box). Every other `Err` — a log-file collision included — must panic,
/// never skip: a green run must never hide the flagship kill-path tests.
fn python_is_missing(error: &SpawnError) -> bool {
    matches!(
        error,
        SpawnError::SpawnFailed { reason }
            if reason.contains("cannot spawn python") && reason.contains("program not found")
    )
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

/// Binds a listener this runtime does not own on `port`, once the OS has released the
/// killed child's hold on it. Bounded, and dropped by the caller when the test wants
/// the port to refuse again.
async fn bind_foreign_listener(port: u16) -> std::net::TcpListener {
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Ok(listener) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            return listener;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "port {port} must come free of the killed child so a foreign listener can take it"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn spawns_a_real_child_probes_ready_and_confirms_release() {
    let runtime = ProcessRuntime::new();
    let spawned: Spawned = match runtime
        .spawn(
            &spec(TEST_PORT, "process-runtime-release"),
            ticket(TEST_PORT),
        )
        .await
    {
        Ok(spawned) => spawned,
        Err(error) if python_is_missing(&error) => {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        Err(error) => panic!("spawn failed for a reason other than a missing python: {error}"),
    };
    assert_eq!(
        spawned.port, TEST_PORT,
        "the runtime serves the assigned port"
    );
    assert_eq!(runtime.engine_count(), 1);

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
        log_name: "process-runtime-exited-early".to_string(),
        readiness: ReadinessSpec::Http {
            path: "/health".to_string(),
            expect_status: 200,
        },
        container: None,
    };
    let ticket = ticket(TEST_PORT + 1);
    if let Err(error) = runtime.spawn(&spec, ticket.clone()).await {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
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

#[tokio::test]
async fn child_output_lands_in_a_new_non_empty_log_file() {
    // FINDINGS-20 [1]: the Step-8 capture check used to pass on any leftover name in
    // `logs/`, and every leftover was 0 bytes because the child printed nothing. This
    // pins capture by creation AND content: the test deletes its own namespace, plants
    // a 0-byte decoy that a name-matching assertion would have accepted, and then
    // demands a file that did not exist before the spawn and carries the child's line.
    let runtime = ProcessRuntime::new();
    std::fs::create_dir_all(logs_dir()).expect("logs dir");
    clear_own_logs(CAPTURE_PREFIX);
    let decoy = format!("{CAPTURE_PREFIX}-decoy.log");
    std::fs::write(logs_dir().join(&decoy), b"").expect("plant the decoy log");
    let before = snapshot_logs(CAPTURE_PREFIX);
    assert!(
        before.contains_key(&decoy),
        "the decoy must be visible to the pre-spawn snapshot: {before:?}"
    );
    assert_eq!(
        before.get(&decoy).copied(),
        Some(0),
        "the planted decoy is a 0-byte name: what the pre-fix assertion passed on"
    );

    let port = TEST_PORT + 2;
    if let Err(error) = runtime
        .spawn(&spec(port, CAPTURE_PREFIX), ticket(port))
        .await
    {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
    }
    let found = await_new_log_with_output(&before, CAPTURE_PREFIX, 15).await;
    runtime.kill(&ticket(port)).await.expect("cleanup kill");

    let (name, bytes) = found.expect(
        "a NEW file with this test's prefix must appear in logs/ and carry the child's \
         stderr line; a pre-existing name (the planted decoy) never satisfies this",
    );
    assert_ne!(name, decoy, "the decoy is not the child's log");
    assert!(!before.contains_key(&name), "{name} predates the spawn");
    assert!(bytes > 0, "{name} exists but holds no bytes");
}

#[tokio::test]
async fn release_is_not_confirmed_while_the_port_still_answers() {
    // FINDINGS-20 [2] / FINDINGS-19 [1]: `release_confirmed` was only ever seen saying
    // `true`, so an implementation that answered true the moment `kill` was called —
    // the exact optimism 0fa70eb removed from the mock — passed every assertion here.
    // A foreign listener keeps the port answering after this runtime's own child has
    // exited, so the G12 pair (child exited AND port refused) is falsified on its port
    // leg, and the same call must flip to `true` once nothing else holds the port.
    let runtime = ProcessRuntime::new();
    let port = TEST_PORT + 3;
    let t = ticket(port);
    if let Err(error) = runtime
        .spawn(&spec(port, "process-runtime-foreign-port"), t.clone())
        .await
    {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
    }
    assert_eq!(wait_ready(&runtime, port, 15).await, Probe::Ready);
    assert!(
        !runtime
            .release_confirmed(&t)
            .await
            .expect("evidence check runs"),
        "a child that is alive and answering has not been released (FINDINGS-20 [2])"
    );

    runtime.kill(&t).await.expect("kill runs");
    let reason = await_child_exited(&runtime, port, 15).await;
    assert!(reason.contains("exited early"), "was: {reason}");

    // The kernel is done with the child's socket, so the listener below is the only
    // thing that can answer this port.
    let foreign = bind_foreign_listener(port).await;
    assert!(
        tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .is_ok(),
        "the foreign listener must be answering the port"
    );

    // FINDINGS-FIXROUND2 [4]: this window hammers the foreign listener's 128-entry
    // accept backlog (one unaccepted connect per poll, no sleep). Bounded per
    // attempt by the 800 ms wall; the 50-run `polls`-counting experiment that would
    // settle the backlog risk is deferred to the card window.
    let held = std::time::Instant::now() + Duration::from_millis(800);
    let mut polls = 0;
    while std::time::Instant::now() < held {
        assert!(
            !runtime
                .release_confirmed(&t)
                .await
                .expect("evidence check runs"),
            "poll {polls}: an exited child is not a released port (G12 needs both legs)"
        );
        polls += 1;
    }
    assert!(polls > 0, "the bounded window must actually have polled");
    assert_eq!(
        runtime.engine_count(),
        1,
        "an unconfirmed release must not drop the table entry"
    );

    drop(foreign);
    assert!(
        confirm_release(&runtime, &t, 15).await,
        "with nothing else answering, the same ticket must confirm"
    );
    assert_eq!(
        runtime.engine_count(),
        0,
        "a confirmed release cleans the table"
    );
}

#[tokio::test]
async fn release_confirmed_refuses_a_stale_ticket_as_a_typed_error() {
    // FINDINGS-20 [2]: every call in this file reused `generation: 1`, so the
    // `Err(StaleTicket)` arms of `kill` and `release_confirmed` were unreachable from
    // an integration test. A ticket that does not name the live spawn of that port is
    // reported, never silently answered.
    let runtime = ProcessRuntime::new();
    let port = TEST_PORT + 4;
    if let Err(error) = runtime
        .spawn(&spec(port, "process-runtime-stale-ticket"), ticket(port))
        .await
    {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
    }
    let stale = SpawnTicket {
        generation: 2,
        ..ticket(port)
    };
    assert_eq!(
        runtime
            .release_confirmed(&stale)
            .await
            .expect_err("a generation mismatch is an error, not an answer"),
        SpawnError::StaleTicket {
            port,
            expected: 2,
            found: 1
        },
        "the typed error must name the port and both generations"
    );
    assert!(
        matches!(
            runtime.kill(&stale).await,
            Err(SpawnError::StaleTicket { found: 1, .. })
        ),
        "a stale ticket may not kill the live spawn either"
    );
    assert_eq!(
        runtime.engine_count(),
        1,
        "a refused stale ticket leaves the engine owned and tracked"
    );

    runtime.kill(&ticket(port)).await.expect("cleanup kill");
    assert!(confirm_release(&runtime, &ticket(port), 15).await);
}

#[tokio::test]
async fn a_stale_ticket_cannot_confirm_away_the_successor_on_its_port() {
    // The safety property behind `(port, generation)`: a ticket whose child is gone
    // must neither kill nor "confirm the release" of the spawn that inherited the port
    // — an optimistic `true` here would let the scheduler spawn onto a busy card.
    let runtime = ProcessRuntime::new();
    let port = TEST_PORT + 6;
    if let Err(error) = runtime
        .spawn(&spec(port, "process-runtime-successor"), ticket(port))
        .await
    {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
    }
    runtime.kill(&ticket(port)).await.expect("first kill runs");
    assert!(
        confirm_release(&runtime, &ticket(port), 15).await,
        "the first child's release is provable"
    );

    let second = SpawnTicket {
        generation: 2,
        ..ticket(port)
    };
    if let Err(error) = runtime
        .spawn(&spec(port, "process-runtime-successor2"), second.clone())
        .await
    {
        if python_is_missing(&error) {
            eprintln!("python unavailable ({error}); skipping");
            return;
        }
        panic!("spawn failed for a reason other than a missing python: {error}");
    }
    assert_eq!(wait_ready(&runtime, port, 15).await, Probe::Ready);
    assert_eq!(
        runtime.release_confirmed(&ticket(port)).await.expect_err(
            "the retired ticket names a gone child; the port is not its release to prove"
        ),
        SpawnError::StaleTicket {
            port,
            expected: 1,
            found: 2
        }
    );
    assert_eq!(
        runtime.engine_count(),
        1,
        "the successor stays owned and tracked while a stale ticket is refused"
    );

    runtime.kill(&second).await.expect("cleanup kill");
    assert!(confirm_release(&runtime, &second, 15).await);
}
