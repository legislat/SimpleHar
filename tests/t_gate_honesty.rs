//! Gate-honesty suite — executes the Falsifier of every FINDINGS-17 item against
//! the real exe on a free GPU card, answering one question per finding: can the
//! T-suite gate lie about this?
//!
//! Operator-gated like the T-suite: every test is `#[ignore]`d and driven by
//! `handoff/gate-honesty/` (PROMPT.md + RUN.md + preflight/run-group/tier scripts),
//! which an external agent runs on a machine with a free card. Run nothing from
//! this file directly unless RUN.md says so.
//!
//! Verdict vocabulary (each test prints one anchored line; `tier.ps1` classifies
//! it case-sensitively, exactly like the T-suite's `T<n>` markers):
//! - `G<n> REFUTED: ...`  the falsifier ran and the strengthened guard caught the
//!   defect class — the gate cannot lie about this item any more.
//! - `G<n> CONFIRMED: ...` the falsifier ran and the gate did lie (or the guard
//!   failed) — the finding is live and actionable. Should never appear on a fixed
//!   tree; it rides an assertion failure, so cargo reports FAIL and the marker
//!   text is the evidence line.
//! - `G<n> BLOCKED: ...`  a model/card prerequisite is missing (never a PASS).
//! - `G<n> SKIP: ...`     the item is not injectable from outside the product in
//!   this condition; the line names what would be needed.
//! - assertion failure    `FAIL` via cargo.
//!
//! Each test is labelled in its doc comment:
//! - ADVERSARIAL: it constructs the broken counterpart itself (a foreign harness,
//!   synthetic log litter, stranger PIDs) — real falsification evidence.
//! - STRENGTHENED: the broken product cannot be staged from a test (the defect
//!   lives in src/), so the test asserts the strengthened form of the check the
//!   old T-suite test weakened; passing proves the assertion is wired and the
//!   product honest, not that a hypothetical bad build would be caught.
//!
//! The `Harness` here is deliberately a copy, not a shared module with
//! `tests/t_switching.rs`: the T-suite is the frozen v1.0 release gate pending
//! its own operator-run (HANDOFF §4 item 3), and this suite must not be able to
//! destabilise it. Sharing the harness is a post-tag refactor.

use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

/// Serializes the whole suite: the harness spawns the real exe and the
/// single-instance guard refuses a second one.
static SERIAL: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
/// Per-spawn discriminator for the identity sentinel (FINDINGS-17 [3] fix): a
/// leaked or foreign harness can replicate a static `sentinel-{tag}` name, but
/// not a name this process minted seconds ago for one specific child.
static NONCE_SEQ: AtomicU64 = AtomicU64::new(0);

const CONTROL: &str = "http://127.0.0.1:8708";
const AUX_LISTENER: &str = "http://127.0.0.1:8710";

fn small_gguf() -> PathBuf {
    std::env::var("SE_T_SMALL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"F:\MODELS_MID\Qwen3-0.6B-GGUF\Qwen3-0.6B-Q8_0.gguf"))
}

fn emb_gguf() -> PathBuf {
    std::env::var("SE_T_EMB")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(r"F:\MODELS_MID\embeddinggemma-2-GGUF\embeddinggemma-2-Q8_0.gguf")
        })
}

fn mid_gguf() -> PathBuf {
    std::env::var("SE_T_MID")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(r"F:\MODELS_MID\Gemma-4-E4B-Uncensored-HauhauCS-Aggressive-Q8_K_P.gguf")
        })
}

/// The `vram_gb` to declare for `path`, using the product's own estimate so the
/// gate books what would really land on the card (same rule as the T-suite).
fn booked_gib(path: &std::path::Path) -> String {
    let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let size = simplehar_se::budget::Gibibytes(bytes as f64 / 1024.0_f64.powi(3));
    simplehar_se::budget::need_gb(None, Some(size))
        .map(|need| format!("{:.2}", need.0))
        .unwrap_or_else(|_| "12".to_string())
}

/// Takes the suite lock for the CALLER's lifetime: an acquired-then-dropped
/// lock serializes nothing (review FINDINGS-FIXTRACK [2]); each async test
/// binds the guard for its whole body. A tokio (not std) mutex because the
/// guard must survive `.await` points. `run-group.ps1 --test-threads=1`
/// remains the second line of defense, not the only one.
#[must_use = "the guard is the serialization; dropping it immediately guards nothing"]
async fn serial_lock() -> tokio::sync::MutexGuard<'static, ()> {
    SERIAL
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

/// Same derivation as the T-suite's: integration binaries live under `deps/`.
fn harness_exe() -> PathBuf {
    let deps_dir = std::env::current_exe()
        .expect("test exe path")
        .parent()
        .expect("deps dir")
        .to_path_buf();
    if deps_dir
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name == "deps")
    {
        deps_dir
            .parent()
            .expect("profile dir")
            .join("simplehar-se.exe")
    } else {
        deps_dir.join("simplehar-se.exe")
    }
}

struct Entry<'a> {
    name: &'a str,
    category: &'a str,
    path: PathBuf,
    vram: String,
    extra: &'a str,
}

impl<'a> Entry<'a> {
    fn aux(name: &'a str) -> Entry<'a> {
        Entry {
            name,
            category: "AUX",
            path: small_gguf(),
            vram: "1".to_string(),
            extra: "\ncpu_offload: true",
        }
    }

    fn mid(name: &'a str) -> Entry<'a> {
        Entry {
            name,
            category: "AUX",
            path: mid_gguf(),
            vram: booked_gib(&mid_gguf()),
            extra: "",
        }
    }
}

/// Writes the temp ModelConf + config and spawns the exe. `sentinel` is the
/// catalog entry that proves the answering server is this child's own;
/// `aux_slots` sizes the AUX category (2 for unload-all tests, 1 for the
/// same-slot supersede tests).
fn spawn_raw(entries: &[Entry], sentinel: &str, aux_slots: usize) -> (Child, PathBuf) {
    let work_dir = simplehar_se::testutil::temp_dir(&format!(
        "gate-honesty-{}",
        NONCE_SEQ.fetch_add(1, Ordering::SeqCst)
    ));
    let modelconf = work_dir.join("ModelConf");
    std::fs::create_dir_all(&modelconf).expect("temp ModelConf");
    for entry in entries {
        std::fs::write(
            modelconf.join(format!("{}.yaml", entry.name)),
            format!(
                "name: {}\nengine: llama\ncategory: {}\nmodel: {}\ncontext: 4096\nvram_gb: {}{}\n",
                entry.name,
                entry.category,
                entry.path.display(),
                entry.vram,
                entry.extra,
            ),
        )
        .expect("model yaml");
    }
    std::fs::write(
        modelconf.join(format!("{sentinel}.yaml")),
        format!(
            "name: {sentinel}\nengine: llama\ncategory: EMB\nmodel: {}\ncontext: 512\nvram_gb: 1\n",
            emb_gguf().display()
        ),
    )
    .expect("sentinel yaml");

    let mut config = serde_json::to_value(simplehar_se::config::AppConfig::default())
        .expect("invariant: config serializes");
    config["modelconf_dir"] = json!(modelconf.display().to_string());
    config["internal_host"] = json!("127.0.0.1");
    if let Some(categories) = config["categories"].as_array_mut() {
        for category in categories {
            if category["id"] == json!("AUX") {
                category["max_models"] = json!(aux_slots);
            }
        }
    }
    let config_path = work_dir.join("simplehar.json");
    std::fs::write(&config_path, config.to_string()).expect("config");

    let child = Command::new(harness_exe())
        .env("SIMPLEHAR_CONFIG", &config_path)
        .env_remove("SIMPLEHAR_TOKEN")
        .spawn()
        .expect("spawn simplehar-se.exe");
    (child, work_dir)
}

/// Polls `{CONTROL}/health` for a 200. `/health` is anonymous: it answers for ANY
/// harness, which is precisely the impersonation channel the sentinel closes.
async fn wait_health(client: &reqwest::Client, secs: u64) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
    loop {
        if client
            .get(format!("{CONTROL}/health"))
            .send()
            .await
            .is_ok_and(|response| response.status().as_u16() == 200)
        {
            return true;
        }
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

struct Harness {
    child: Child,
    http: reqwest::Client,
    _work_dir: PathBuf,
}

impl Harness {
    /// Spawns with a per-run nonce sentinel and re-samples `try_wait()` after
    /// the identity loop (FINDINGS-17 [3] minimal fix). Returns Err with the
    /// evidence rather than panicking, so adversarial tests assert on the refusal.
    async fn spawn_verified(
        tag: &str,
        entries: &[Entry<'_>],
        aux_slots: usize,
    ) -> Result<Harness, String> {
        let nonce = format!(
            "{tag}-{}-{}",
            std::process::id(),
            NONCE_SEQ.fetch_add(1, Ordering::SeqCst)
        );
        let sentinel = format!("sentinel-{nonce}");
        let (mut child, work_dir) = spawn_raw(entries, &sentinel, aux_slots);
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("client");
        let probe = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("probe client");
        if !wait_health(&probe, 20).await {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the spawned exe never answered {CONTROL}/health within 20 s"
            ));
        }
        // Identity, with liveness re-sampled inside and after: the pre-fix code
        // sampled `try_wait()` once ~one round trip after spawn, before the child
        // could even reach and lose the single-instance guard.
        let mut identified = false;
        let mut exited = None;
        for _ in 0..50 {
            let value: Value = probe
                .get(format!("{CONTROL}/har/models"))
                .send()
                .await
                .map_err(|error| format!("GET /har/models: {error}"))?
                .json()
                .await
                .map_err(|error| format!("/har/models json: {error}"))?;
            identified = value["data"].as_array().is_some_and(|rows| {
                rows.iter()
                    .any(|row| row["id"] == sentinel.as_str() || row["name"] == sentinel.as_str())
            });
            if identified {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                exited = Some(status);
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        if exited.is_none() {
            exited = match child.try_wait() {
                Ok(Some(status)) => Some(status),
                _ => None,
            };
        }
        if let Some(status) = exited {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the exe we spawned exited ({status:?}) while something answered {CONTROL} — \
                 identity sentinel `{sentinel}` {}",
                if identified {
                    "MATCHED a foreign server (static-name impersonation)"
                } else {
                    "was absent: the server is not ours"
                }
            ));
        }
        if !identified {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "the server answering {CONTROL} has no `{sentinel}` in its catalog in 10 s; \
                 it is not the harness we spawned"
            ));
        }
        Ok(Harness {
            child,
            http,
            _work_dir: work_dir,
        })
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    async fn get(&self, path: &str) -> Value {
        self.http
            .get(format!("{CONTROL}{path}"))
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("JSON")
    }

    async fn post(&self, url: &str, body: Value) -> (u16, Value) {
        let response = self.http.post(url).json(&body).send().await.expect("POST");
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }

    fn post_task(&self, url: &str, body: Value) -> tokio::task::JoinHandle<(u16, Value)> {
        let http = self.http.clone();
        let url = url.to_string();
        tokio::spawn(async move {
            let response = http.post(&url).json(&body).send().await.expect("POST");
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            (status, serde_json::from_str(&text).unwrap_or(Value::Null))
        })
    }

    async fn status(&self) -> Value {
        self.get("/har/status").await
    }

    async fn job_for(&self, model: &str) -> Option<Value> {
        let jobs = self.get("/har/jobs").await;
        jobs["jobs"]
            .as_array()?
            .iter()
            .rev()
            .find(|job| job["model"] == model)
            .cloned()
    }

    async fn wait_serving(&self, model: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let status = self.status().await;
            if status["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().any(|slot| slot["model"] == model))
            {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_job_in_flight(&self, model: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if let Some(job) = self.job_for(model).await {
                if job["terminal"] == false {
                    return true;
                }
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_empty(&self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let status = self.status().await;
            if status["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().all(|slot| slot["model"] == Value::Null))
            {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// The engine port the slot table currently reports for `model`.
    async fn engine_port(&self, model: &str) -> u16 {
        let status = self.status().await;
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|slot| slot["model"] == model)
            .unwrap_or_else(|| panic!("no slot reports {model}"))["port"]
            .as_u64()
            .expect("engine port") as u16
    }

    /// The engine pid the slot table reports for `model`.
    async fn engine_pid(&self, model: &str) -> u32 {
        let status = self.status().await;
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|slot| slot["model"] == model)
            .unwrap_or_else(|| panic!("no slot reports {model}"))["pid"]
            .as_u64()
            .expect("engine pid") as u32
    }

    async fn chat(&self, listener: &str, model: Option<&str>) -> (u16, Value) {
        let mut body =
            json!({ "messages": [{ "role": "user", "content": "say OK" }], "max_tokens": 4 });
        if let Some(model) = model {
            body["model"] = json!(model);
        }
        self.post(&format!("{listener}/v1/chat/completions"), body)
            .await
    }

    async fn load(&self, model: &str) -> (u16, Value) {
        self.post(&format!("{CONTROL}/har/load"), json!({ "model": model }))
            .await
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A harness spawned to impersonate: it owns 8708-8711 and carries the OLD
/// static `sentinel-{tag}` name, exactly the leaked/foreign server
/// FINDINGS-17 [3] describes.
struct Decoy {
    child: Child,
    _work_dir: PathBuf,
}

impl Decoy {
    async fn own_ports(tag: &str) -> Decoy {
        let (child, work_dir) = spawn_raw(&[Entry::aux("auxA")], &format!("sentinel-{tag}"), 1);
        let probe = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("probe client");
        assert!(
            wait_health(&probe, 20).await,
            "decoy harness never became ready"
        );
        Decoy {
            child,
            _work_dir: work_dir,
        }
    }
}

impl Drop for Decoy {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Polls until a TCP connect to `port` fails (refused): the engine that answered
/// there is gone. The T-suite's own proven release evidence (T2), replacing every
/// fixed sleep in this suite.
async fn port_closed(port: u16, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let attempt = tokio::time::timeout(
            Duration::from_millis(400),
            tokio::net::TcpStream::connect(("127.0.0.1", port)),
        )
        .await;
        let refused = match attempt {
            Err(_elapsed) => false,
            Ok(Err(_)) => true,
            Ok(Ok(_stream)) => false,
        };
        if refused {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

/// `(name, parent_pid)` from the live process table — the OS is the only
/// trustworthy source for "whose child is this" (FINDINGS-17 [6]/[9]).
fn process_info(pid: u32) -> Option<(String, u32)> {
    let script = format!(
        "(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}' -ErrorAction SilentlyContinue \
         | Select-Object -First 1 Name,ParentProcessId | ConvertTo-Json -Compress)"
    );
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell CIM query");
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text == "null" {
        return None;
    }
    let value: Value = serde_json::from_str(&text).ok()?;
    Some((
        value["Name"].as_str()?.to_ascii_lowercase(),
        value["ParentProcessId"].as_u64()? as u32,
    ))
}

/// The guard FINDINGS-17 [6] demands before any kill: the target must be
/// llama-server AND a direct child of the harness we spawned.
fn kill_target_authorized(pid: u32, harness_pid: u32) -> bool {
    process_info(pid)
        .is_some_and(|(name, parent)| name == "llama-server.exe" && parent == harness_pid)
}

/// Every llama-server.exe on the machine (the old, machine-global orphan check,
/// recorded in G9's evidence line so the divergence is visible).
fn machine_llama_count() -> usize {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq llama-server.exe", "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("llama-server.exe"))
        .count()
}

/// The [9] fix: only llama-server processes whose PARENT is our harness count.
fn child_llama_count(harness_pid: u32) -> usize {
    let script = "(Get-CimInstance Win32_Process -Filter \"Name='llama-server.exe'\" \
                  -ErrorAction SilentlyContinue | Select-Object ProcessId,ParentProcessId \
                  | ConvertTo-Json -Compress)"
        .to_string();
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .output()
        .expect("powershell CIM query");
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() || text == "null" {
        return 0;
    }
    let value: Value = serde_json::from_str(&text).expect("CIM json");
    let rows: Vec<&Value> = match &value {
        Value::Array(rows) => rows.iter().collect(),
        single => vec![single],
    };
    rows.iter()
        .filter(|row| row["ParentProcessId"].as_u64() == Some(u64::from(harness_pid)))
        .count()
}

/// The [8] fix, as pure logic so both rules run against the SAME synthetic
/// directory instead of being argued about. `run_local`: count only files that
/// appeared after the baseline AND name one of this run's models, bounded by the
/// run's own spawn count. `cap_bound`: the old product-constant check.
fn log_verdicts(
    baseline: &BTreeSet<String>,
    current: &BTreeSet<String>,
    run_models: &[&str],
    spawns: usize,
    cap: usize,
) -> (bool, bool) {
    let run_created = current
        .iter()
        .filter(|name| !baseline.contains(*name))
        .filter(|name| run_models.iter().any(|model| name.starts_with(model)))
        .count();
    let run_local_ok = run_created <= 2 * spawns + 4;
    let cap_ok = current
        .iter()
        .filter(|name| run_models.iter().any(|model| name.starts_with(model)))
        .count()
        <= cap;
    (run_local_ok, cap_ok)
}

// ---------------------------------------------------------------- G1 (FINDINGS-17 [1])

/// STRENGTHENED. T12.4 proved "`{}` unloads all" against ONE occupant, so a
/// first-occupant-only implementation passed green. Two residents now; both must
/// be gone, by slot table AND by their engine ports refusing TCP.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g01_unload_all_frees_every_occupant() {
    let _serial = serial_lock().await;
    let entries = [Entry::aux("auxA"), Entry::aux("auxB")];
    let harness = Harness::spawn_verified("g01", &entries, 2)
        .await
        .expect("verified harness");
    for model in ["auxA", "auxB"] {
        let (status, body) = harness.load(model).await;
        assert!(
            status == 200 || status == 202,
            "load {model}: {status} {body}"
        );
        assert!(
            harness.wait_serving(model, 120).await,
            "{model} never reached serving"
        );
    }
    let port_a = harness.engine_port("auxA").await;
    let port_b = harness.engine_port("auxB").await;
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({}))
        .await;
    assert_eq!(status, 200, "unload {{}} reply: {body}");
    assert!(
        harness.wait_empty(60).await,
        "CONFIRMED-g1: unload-all left an occupant installed"
    );
    assert!(
        port_closed(port_a, 30).await,
        "auxA port {port_a} still answering"
    );
    assert!(
        port_closed(port_b, 30).await,
        "auxB port {port_b} still answering"
    );
    println!("G1 REFUTED: unload-all verified against TWO residents (slots null, ports closed)");
}

// ---------------------------------------------------------------- G2 (FINDINGS-17 [2])

/// STRENGTHENED. The old T12.4 discarded the load waiter's reply (a 502 counted
/// as "loaded") and `wait_empty` accepted any all-null snapshot. Now every setup
/// load must be an accepted receipt, both engines must answer a real chat BEFORE
/// the unload, and after it both engine ports must refuse TCP — a null slot
/// table alone proves nothing, because a no-model chat would implicitly reload.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g02_evidence_before_and_after_the_unload() {
    let _serial = serial_lock().await;
    let entries = [Entry::aux("auxA"), Entry::aux("auxB")];
    let harness = Harness::spawn_verified("g02", &entries, 2)
        .await
        .expect("verified harness");
    for model in ["auxA", "auxB"] {
        let (status, body) = harness.load(model).await;
        assert!(
            status == 200 || status == 202,
            "load {model}: {status} {body}"
        );
        assert!(
            harness.wait_serving(model, 120).await,
            "{model} never reached serving"
        );
    }
    for model in ["auxA", "auxB"] {
        let (status, body) = harness.chat(AUX_LISTENER, Some(model)).await;
        assert_eq!(
            status, 200,
            "{model} must actually serve before unload: {body}"
        );
    }
    let port_a = harness.engine_port("auxA").await;
    let port_b = harness.engine_port("auxB").await;
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({}))
        .await;
    assert_eq!(status, 200, "unload {{}} reply: {body}");
    assert!(
        harness.wait_empty(60).await,
        "CONFIRMED-g2: unload-all left residents installed"
    );
    assert!(
        port_closed(port_a, 30).await,
        "CONFIRMED-g2: null slot but {port_a} answers — the snapshot lied"
    );
    assert!(
        port_closed(port_b, 30).await,
        "CONFIRMED-g2: null slot but {port_b} answers — the snapshot lied"
    );
    println!("G2 REFUTED: accepted receipts, live chats before, and port-closed after — not a discarded reply plus null slots");
}

// ---------------------------------------------------------------- G3 (FINDINGS-17 [3])

/// ADVERSARIAL. A foreign harness holding 8708-8711 with the OLD static
/// `sentinel-{tag}` name must NOT be able to impersonate our spawn: the nonce
/// sentinel is unguessable and `try_wait()` is re-sampled after the identity
/// loop. Then the positive control: with the decoy gone, the same verified
/// spawn succeeds — the refusal is the guard, not a broken spawn.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g03_foreign_harness_cannot_impersonate() {
    let _serial = serial_lock().await;
    let decoy = Decoy::own_ports("g03").await;
    let refusal = Harness::spawn_verified("g03", &[Entry::aux("auxA")], 1)
        .await
        .err()
        .expect(
            "CONFIRMED-g3: a foreign harness carrying the static sentinel impersonated our spawn",
        );
    println!("G3 foreign-server refusal: {refusal}");
    drop(decoy);
    let ports_free_deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(500))
            .build()
            .expect("probe");
        if client
            .get(format!("{CONTROL}/health"))
            .send()
            .await
            .is_err()
        {
            break;
        }
        assert!(
            Instant::now() < ports_free_deadline,
            "the decoy's ports never released after kill"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let ours = Harness::spawn_verified("g03", &[Entry::aux("auxA")], 1)
        .await
        .expect(
            "positive control: our own spawn must verify once no foreign server holds the ports",
        );
    println!(
        "G3 REFUTED: nonce sentinel + post-loop try_wait rejected the impostor; own spawn verified"
    );
    drop(ours);
}

// ---------------------------------------------------------------- G4 (FINDINGS-17 [4])

/// STRENGTHENED. A supersede 409 is indistinguishable from a port-holder 409 by
/// status code alone. One AUX slot; the slow mid load A is superseded by B
/// mid-flight; the supersede must name itself in the reply body or the ledger
/// row — a 409 that names neither is the misattribution the finding describes.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + mid model"]
async fn g04_supersede_409_names_the_reason() {
    let _serial = serial_lock().await;
    if !mid_gguf().exists() {
        println!("G4 BLOCKED: mid model missing (SE_T_MID) — the in-flight window cannot be opened honestly");
        return;
    }
    let entries = [Entry::mid("midA"), Entry::aux("auxA")];
    let harness = Harness::spawn_verified("g04", &entries, 1)
        .await
        .expect("verified harness");
    let a = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": "midA" }));
    let sighted = harness.wait_job_in_flight("midA", 20).await;
    if !sighted {
        let _ = a.await;
        println!(
            "G4 SKIP: the in-flight window never opened (A finished before B could supersede it)"
        );
        return;
    }
    let (status_b, body_b) = harness.load("auxA").await;
    let (status_a, body_a) = a.await.expect("load task");
    assert_eq!(status_b, 200, "B must load cleanly: {body_b}");
    if status_a == 409 {
        let job = harness
            .job_for("midA")
            .await
            .map(|job| job.to_string())
            .unwrap_or_default();
        let names_supersede = body_a.to_string().contains("supersed") || job.contains("supersed");
        assert!(
            names_supersede,
            "CONFIRMED-g4: a supersede-shaped 409 naming neither body nor ledger reason — \
             status alone cannot tell it from a port-holder clash. body={body_a} job={job}"
        );
        println!(
            "G4 REFUTED: supersede 409 carries its reason in body/ledger, not just the status"
        );
    } else {
        assert!(
            status_a == 200,
            "A must be accepted or superseded, never faulted: {status_a} {body_a}"
        );
        println!("G4 SKIP: A completed (200) despite sighting — no supersede to classify");
    }
}

// ---------------------------------------------------------------- G5 (FINDINGS-17 [5])

/// ADVERSARIAL-with-honest-SKIP. T9 claimed the in-flight attach path but never
/// sighted an in-flight job; issuance is not overlap. Two gates make REFUTED
/// mean the thing: the job must be SIGHTED non-terminal before the duplicates
/// go out, AND the duplicates must return `loaded` receipts carrying the SAME
/// job as the first post (review FINDINGS-FIXTRACK [3]) — the post-serving
/// `already` path answers 200 with no job and must never satisfy the claim.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g05_attach_path_is_sighted_not_assumed() {
    let _serial = serial_lock().await;
    let harness = Harness::spawn_verified("g05", &[Entry::aux("auxA")], 1)
        .await
        .expect("verified harness");
    let a = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": "auxA" }));
    let sighted = harness.wait_job_in_flight("auxA", 3).await;
    let b = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": "auxA" }));
    let c = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": "auxA" }));
    let (sa, sb, sc) = tokio::join!(a, b, c);
    let (sa, sb, sc) = (sa.expect("task"), sb.expect("task"), sc.expect("task"));
    for (status, body) in [(&sa.0, &sa.1), (&sb.0, &sb.1), (&sc.0, &sc.1)] {
        assert!(
            *status == 200 || *status == 202,
            "attach load: {status} {body}"
        );
    }
    let jobs = harness.get("/har/jobs").await;
    let load_rows = jobs["jobs"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .filter(|job| job["model"] == "auxA" && job["kind"] == "Load")
                .count()
        })
        .unwrap_or(usize::MAX);
    assert_eq!(
        load_rows, 1,
        "concurrent loads must collapse to one job row"
    );
    // The attach proof: duplicates that joined the in-flight job answer
    // `loaded` with that job's receipt; an `already` means the job served
    // before the post landed — the path FINDINGS-17 [5] says proves nothing.
    let job_of = |body: &Value| {
        (body["status"] == "loaded")
            .then(|| body["receipt"]["job"].clone())
            .filter(|job| job != &Value::Null)
    };
    let attached = [sb.1, sc.1]
        .iter()
        .filter_map(job_of)
        .filter(|job| *job == job_of(&sa.1).unwrap_or(Value::Null))
        .count();
    if !sighted || attached < 2 {
        println!(
            "G5 SKIP: sighted={sighted}, {attached}/2 duplicates proved the in-flight attach \
             (same loaded receipt); an Already-only window cannot REFUTE FINDINGS-17 [5]"
        );
        return;
    }
    println!("G5 REFUTED: duplicates attached to the SAME loaded job, sighted in flight — 3 posts, 1 job row");
}

// ---------------------------------------------------------------- G6 (FINDINGS-17 [6])

/// ADVERSARIAL. The kill guard must accept the product's real engine child and
/// reject the strangers the product (or a ticket-aliasing bug) could hand it:
/// our own test process and PID 4 must both fail closed.
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g06_kill_guard_accepts_child_rejects_strangers() {
    let _serial = serial_lock().await;
    let harness = Harness::spawn_verified("g06", &[Entry::aux("auxA")], 1)
        .await
        .expect("verified harness");
    let (status, body) = harness.load("auxA").await;
    assert!(status == 200 || status == 202, "load: {status} {body}");
    assert!(harness.wait_serving("auxA", 120).await, "auxA never served");
    let engine_pid = harness.engine_pid("auxA").await;
    assert!(
        kill_target_authorized(engine_pid, harness.pid()),
        "CONFIRMED-g6: the real engine child was NOT authorized — the guard cannot tell \
         our llama-server from a stranger (pid {engine_pid})"
    );
    assert!(
        !kill_target_authorized(std::process::id(), harness.pid()),
        "CONFIRMED-g6: the guard authorized the test process itself"
    );
    assert!(
        !kill_target_authorized(4, harness.pid()),
        "CONFIRMED-g6: the guard authorized PID 4 (System)"
    );
    println!("G6 REFUTED: guard authorized the real engine child and refused two stranger PIDs");
}

// ---------------------------------------------------------------- G7 (FINDINGS-17 [7])

/// STRENGTHENED. T7 7.2 ignored the unload reply and slept a fixed 2 s before
/// trusting "VRAM freed". Release is proven the way T2 proves it: asserted
/// unload receipt, then the engine port refusing TCP — never a sleep. (The
/// finding's full falsifier stubs the product's kill; that needs a product hook
/// and stays owed to the operator.)
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g07_release_is_proven_by_the_port_not_a_sleep() {
    let _serial = serial_lock().await;
    let harness = Harness::spawn_verified("g07", &[Entry::aux("auxA")], 1)
        .await
        .expect("verified harness");
    let (status, body) = harness.load("auxA").await;
    assert!(status == 200 || status == 202, "load: {status} {body}");
    assert!(harness.wait_serving("auxA", 120).await, "auxA never served");
    let port = harness.engine_port("auxA").await;
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({ "model": "auxA" }))
        .await;
    assert_eq!(
        status, 200,
        "CONFIRMED-g7: the unload reply was never asserted before: {body}"
    );
    assert!(
        port_closed(port, 30).await,
        "CONFIRMED-g7: port {port} still answering after unload — a fixed sleep would \
         have passed this while the card was held"
    );
    let (status, body) = harness.load("auxA").await;
    assert!(
        status == 200 || status == 202,
        "reload after proven release must succeed: {status} {body}"
    );
    println!("G7 REFUTED: unload receipt asserted, release proven by port-close, no fixed sleeps");
}

// ---------------------------------------------------------------- G8 (FINDINGS-17 [8])

/// ADVERSARIAL, no card needed: both log rules run against the SAME synthetic
/// directory. The old bound (count by prefix <= LOG_FILE_CAP) passes a real leak
/// and fails on harmless machine litter; the run-local bound (baseline delta,
/// this run's model names, its own spawn count) fails the leak and ignores the
/// litter. A bound that cannot fail is the finding.
#[test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: synthetic, kept with the suite"]
fn g08_log_bound_fails_a_leak_and_ignores_litter() {
    let baseline: BTreeSet<String> = (0..60)
        .map(|index| format!("work06-decoy-{index}.log"))
        .collect();
    let mut litter_only = baseline.clone();
    for index in 0..3 {
        litter_only.insert(format!("other{index}.log"));
    }
    let (local_ok, cap_ok) = log_verdicts(&baseline, &litter_only, &["work06"], 2, 50);
    assert!(
        local_ok,
        "run-local must not fail on litter it did not create"
    );
    assert!(
        !cap_ok,
        "CONFIRMED-g8: the old bound stopped failing on machine litter"
    );

    let clean_leak: BTreeSet<String> = (0..12)
        .map(|index| format!("work06-run{index}.log"))
        .collect();
    let (local_ok, cap_ok) = log_verdicts(&BTreeSet::new(), &clean_leak, &["work06"], 2, 50);
    assert!(!local_ok, "CONFIRMED-g8: run-local missed a 12-file leak");
    assert!(
        cap_ok,
        "CONFIRMED-g8: the old bound passed a 12-file leak on a clean machine"
    );
    println!(
        "G8 REFUTED: run-local bound fails leaks and shrugs off litter; the LOG_FILE_CAP \
         bound does neither (12 leaked files <= 50 = green)"
    );
}

// ---------------------------------------------------------------- G9 (FINDINGS-17 [9])

/// STRENGTHENED. Orphan accounting is parent-scoped through a real engine
/// lifecycle: 0 before load, exactly 1 (and it is the slot-reported pid) while
/// serving, back to 0 after unload — with the machine-global count recorded
/// beside the verdict so an operator-resident engine shows what the old check
/// would have charged to us. (A fully adversarial version needs a stable
/// foreign llama-server; staging one means loading a model outside the suite,
/// so the divergence is demonstrated by the recorded pair, not injected.)
#[tokio::test]
#[ignore = "operator-gated via handoff/gate-honesty/RUN.md: real exe + models"]
async fn g09_orphan_count_is_parent_scoped() {
    let _serial = serial_lock().await;
    let harness = Harness::spawn_verified("g09", &[Entry::aux("auxA")], 1)
        .await
        .expect("verified harness");
    let harness_pid = harness.pid();
    assert_eq!(
        child_llama_count(harness_pid),
        0,
        "CONFIRMED-g9: our harness owns engines before loading anything"
    );
    let (status, body) = harness.load("auxA").await;
    assert!(status == 200 || status == 202, "load: {status} {body}");
    assert!(harness.wait_serving("auxA", 120).await, "auxA never served");
    let engine_pid = harness.engine_pid("auxA").await;
    let port = harness.engine_port("auxA").await;
    assert_eq!(
        child_llama_count(harness_pid),
        1,
        "CONFIRMED-g9: parent-scoped count missed our one live engine ({engine_pid})"
    );
    let (status, _body) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({ "model": "auxA" }))
        .await;
    assert_eq!(status, 200);
    assert!(
        port_closed(port, 30).await,
        "CONFIRMED-g9: engine port {port} outlived its unload"
    );
    let deadline = Instant::now() + Duration::from_secs(30);
    while child_llama_count(harness_pid) != 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        child_llama_count(harness_pid),
        0,
        "CONFIRMED-g9: a child engine outlived its unload"
    );
    println!(
        "G9 REFUTED: parent-scoped count tracked 0->1->0 across the lifecycle (machine-global \
         reads {} now — what the old check would charge to us)",
        machine_llama_count()
    );
}
