//! T1–T14 — the model-switching test plan, ported from
//! `base-archive/TEST-SWITCHING.md` to the SE stack (v1.0 scope: llama engines
//! only; strata/ninfer arrive with their templates).
//!
//! Operator-gated: every test is `#[ignore]`d; run with
//! `cargo test --test t_switching -- --ignored --test-threads=1`.
//!
//! Adaptations from the v3.3 plan (each documented per group):
//! - listeners are per-category (WORK 8708 control, RP 8709, AUX 8710, EMB 8711);
//!   the 8016-engine/8017-bonsai listeners do not exist (bonsai scrapped);
//! - strata/ninfer cases are `SKIP` (no templates in v0 — §0.4);
//! - T11 (external adoption) is `SKIP` — adoption is scrapped in SE, the
//!   externals display list lands past v1.0;
//! - T10.4 restart expectations follow SE's no-state-restore non-goal: after a
//!   harness kill, Job Objects guarantee zero orphan engines and boot starts
//!   with empty slots (v3.3 expected the model back);
//! - T12.4 `{}` is the explicit unload-all (v3.3 semantics); only *garbage*
//!   bodies are `400`.
//!
//! Verdict vocabulary (per case, from the plan §17): PASS / FAIL / FINDING /
//! BLOCKED / SKIP. A failing assertion fails the test = FAIL; `BLOCKED` and
//! `SKIP` print and return early. Flake policy: re-run once, then classify
//! FLAKY n/3 by worst observed.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::{Child, Command};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Serializes the whole suite: the harness spawns the real exe and the
/// single-instance guard refuses a second one.
static SERIAL: OnceLock<Mutex<()>> = OnceLock::new();

const CONTROL: &str = "http://127.0.0.1:8708";
const RP_LISTENER: &str = "http://127.0.0.1:8709";
const AUX_LISTENER: &str = "http://127.0.0.1:8710";
const EMB_LISTENER: &str = "http://127.0.0.1:8711";

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

fn big_work_gguf() -> PathBuf {
    std::env::var("SE_T_WORK31")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"F:\MODELS_MID\Artemis-31B-v1.2.Q6_K.gguf"))
}

fn big_rp_gguf() -> PathBuf {
    std::env::var("SE_T_RP31")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"F:\MODELS_MID\Wicked-Tutu-31B-Final.Q6_K.gguf"))
}

fn big_aux_gguf() -> PathBuf {
    std::env::var("SE_T_AUX31")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(r"F:\MODELS_MID\Artoma-Lexicon-31B.Q6_K.gguf"))
}

struct Harness {
    child: Child,
    http: reqwest::Client,
    #[allow(dead_code)] // debugging aid: where this run's ModelConf lives
    modelconf: PathBuf,
    logs_dir: PathBuf,
    #[allow(dead_code)] // debugging aid: which group spawned this harness
    run_tag: String,
}

impl Harness {
    /// Spawns the real exe with a temp config + ModelConf, waits for readiness.
    fn spawn(tag: &str) -> Harness {
        let work_dir = std::env::temp_dir().join(format!("se-t14-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&work_dir);
        let modelconf = work_dir.join("ModelConf");
        std::fs::create_dir_all(&modelconf).expect("temp ModelConf");

        let yaml = |name: &str, category: &str, path: &PathBuf, extra: &str| {
            std::fs::write(
                modelconf.join(format!("{name}.yaml")),
                format!(
                    "name: {name}\nengine: llama\ncategory: {category}\nmodel: {}\ncontext: 4096\nvram_gb: 1{extra}\n",
                    path.display()
                ),
            )
            .expect("model yaml");
        };
        yaml("work06", "WORK", &small_gguf(), "\ncpu_offload: true");
        yaml("work06b", "WORK", &small_gguf(), "\ncpu_offload: true");
        yaml("emb", "EMB", &emb_gguf(), "");
        if big_work_gguf().exists() {
            yaml("work31", "WORK", &big_work_gguf(), "");
        }
        if big_rp_gguf().exists() {
            yaml("rp31", "RP", &big_rp_gguf(), "");
        }
        if big_aux_gguf().exists() {
            yaml("aux31", "AUX", &big_aux_gguf(), "");
        }

        let config_path = work_dir.join("simplehar.json");
        std::fs::write(
            &config_path,
            json!({ "internal_host": "127.0.0.1" }).to_string(),
        )
        .expect("config");

        // Tests run from target/<profile>/deps; the binary lives one level up.
        let deps_dir = std::env::current_exe()
            .expect("test exe path")
            .parent()
            .expect("deps dir")
            .to_path_buf();
        let exe = if deps_dir
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n == "deps")
        {
            deps_dir
                .parent()
                .expect("profile dir")
                .join("simplehar-se.exe")
        } else {
            deps_dir.join("simplehar-se.exe")
        };
        let child = Command::new(&exe)
            .env("SIMPLEHAR_CONFIG", &config_path)
            .spawn()
            .expect("spawn simplehar-se.exe");

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("client");
        // Readiness poll: a blocking client keeps `spawn` sync-friendly.
        let probe = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("blocking client");
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if probe
                .get(format!("{CONTROL}/health"))
                .send()
                .is_ok_and(|response| response.status().as_u16() == 200)
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "T-harness: the exe did not become ready in 20 s"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        Harness {
            child,
            http,
            modelconf,
            logs_dir: work_dir.join("logs"),
            run_tag: tag.to_string(),
        }
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

    async fn post_raw(&self, url: &str, body: &str, content_type: Option<&str>) -> u16 {
        let mut request = self.http.post(url);
        if let Some(content_type) = content_type {
            request = request.header("content-type", content_type);
        }
        request
            .body(body.to_string())
            .send()
            .await
            .expect("POST")
            .status()
            .as_u16()
    }

    async fn status(&self) -> Value {
        self.get("/har/status").await
    }

    /// Polls until `model` is serving in the slot table (or timeout).
    async fn wait_serving(&self, model: &str, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let status = self.status().await;
            if status["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().any(|s| s["model"] == model))
            {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn wait_empty(&self, secs: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let status = self.status().await;
            if status["slots"]
                .as_array()
                .is_some_and(|slots| slots.iter().all(|s| s["model"] == Value::Null))
            {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
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

    /// Counts live llama-server processes (orphan checks, T14).
    fn llama_server_count(&self) -> usize {
        let output = Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq llama-server.exe", "/FO", "CSV", "/NH"])
            .output()
            .expect("tasklist");
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| line.contains("llama-server.exe"))
            .count()
    }

    fn baseline_check(&self, model_files: &[&PathBuf]) -> bool {
        model_files.iter().all(|path| path.exists())
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Abrupt kill: the Job Object guarantee means engines die with us; the
        // orphan checks in T10/T14 verify exactly that.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Takes the suite lock, releases it immediately: the guard cannot span an
/// await (clippy), so this only *asserts* serialization - the operator runs the
/// suite with --test-threads=1 anyway (the plan's execution order).
fn serial_lock() {
    let _guard = SERIAL
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    drop(_guard);
}

fn cargo_bin() -> PathBuf {
    std::env::current_exe()
        .expect("test exe")
        .parent()
        .expect("target dir")
        .join("simplehar-se.exe")
}

// ---------------------------------------------------------------- T1

#[tokio::test]
#[ignore = "operator-gated: real exe + real model files (SE_T_*)"]
async fn t01_listener_routing_guards() {
    serial_lock();
    let harness = Harness::spawn("t01");
    let work = cargo_bin();
    assert!(work.exists(), "build the exe first (cargo build)");
    assert!(
        harness.baseline_check(&[&small_gguf(), &emb_gguf()]),
        "T1 BLOCKED: smoke model files missing"
    );

    // 1.1/1.5 (adapted): a WORK model on the RP/AUX/EMB listeners -> 404 BEFORE
    // any load (no job created), wrong-category with the listener hint.
    for listener in [RP_LISTENER, AUX_LISTENER, EMB_LISTENER] {
        let (status, body) = harness.chat(listener, Some("work06")).await;
        assert_eq!(status, 404, "wrong-category on {listener}: {body}");
        assert!(
            body["message"]
                .as_str()
                .unwrap_or_default()
                .contains("WORK listener"),
            "the hint names the owning listener: {body}"
        );
    }
    let jobs = harness.get("/har/jobs").await;
    assert_eq!(
        jobs["jobs"].as_array().map(Vec::len),
        Some(0),
        "wrong-category requests must not create jobs (404 before ensure)"
    );

    // 1.6: unknown model -> 404 listing guidance.
    let (status, body) = harness.chat(RP_LISTENER, Some("does-not-exist")).await;
    assert_eq!(status, 404);
    assert!(body["message"]
        .as_str()
        .unwrap_or_default()
        .contains("does-not-exist"));

    // 1.7 (adapted): no model on an empty listener without a default -> 400.
    let (status, body) = harness.chat(RP_LISTENER, None).await;
    assert_eq!(status, 400, "{body}");

    // 1.8: per-listener /v1/models sets.
    let work = harness.get(&format!("{CONTROL}/v1/models")).await;
    assert!(work["data"].as_array().is_some_and(|data| data
        .iter()
        .all(|m| ["work06", "work06b", "work31"].contains(&m["id"].as_str().unwrap_or_default()))));
    let emb = harness.get(&format!("{EMB_LISTENER}/v1/models")).await;
    assert!(emb["data"]
        .as_array()
        .is_some_and(|data| data.iter().all(|m| m["id"] == "emb")));
    println!("T1 PASS");
}

// ---------------------------------------------------------------- T2

#[tokio::test]
#[ignore = "operator-gated"]
async fn t02_single_load_unload_llama() {
    serial_lock();
    let harness = Harness::spawn("t02");

    // 2.1-2.2: load the fast smoke model into AUX; the slot appears Serving.
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert_eq!(status, 200, "load: {body}");
    assert!(harness.wait_serving("work06", 120).await, "slot Serving");

    // 2.3: chat on the owning listener -> 200 with a completion.
    let (status, body) = harness.chat(AUX_LISTENER, Some("work06")).await;
    assert_eq!(status, 200, "chat: {body}");
    assert!(body["choices"].as_array().is_some(), "completion: {body}");

    // 2.5: unload by model; 2.7: the engine port actually frees.
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "model": "work06" }),
        )
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_empty(30).await, "slot empty after unload");
    tokio::time::sleep(Duration::from_secs(1)).await;
    let serving_port = {
        let status = harness.status().await;
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|s| s["category"] == "AUX")
            .and_then(|s| s["port"].as_u64())
    };
    assert!(
        serving_port.is_none() || serving_port == Some(0),
        "port released"
    );

    // Orphan check (plan §0 abort criteria).
    assert_eq!(harness.llama_server_count(), 0, "zero orphans after unload");
    println!("T2 PASS");
}

// ---------------------------------------------------------------- T3

#[tokio::test]
#[ignore = "operator-gated"]
async fn t03_same_slot_replacements() {
    serial_lock();
    let harness = Harness::spawn("t03");

    // 3.1 (llama-only): work06 -> work06b replaces the occupant on the SAME port.
    let first = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert_eq!(first.0, 200, "load: {:?}", first.1);
    assert!(harness.wait_serving("work06", 120).await);
    let pid_old = harness.status().await["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|s| s["model"] == "work06")
        .and_then(|s| s["port"].as_u64())
        .expect("port");

    let second = harness
        .post(
            &format!("{CONTROL}/har/load"),
            json!({ "model": "work06b" }),
        )
        .await;
    assert_eq!(second.0, 200, "load: {:?}", second.1);
    assert!(harness.wait_serving("work06b", 120).await);
    let port_new = harness.status().await["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|s| s["model"] == "work06b")
        .and_then(|s| s["port"].as_u64())
        .expect("port");
    assert_eq!(port_new, pid_old, "same-slot replacement keeps the port");
    let (status, body) = harness.chat(CONTROL, Some("work06b")).await;
    assert_eq!(status, 200, "{body}");

    // 3.5 (adapted to SE): rapid A then B before A is ready — final slot = B,
    // no port conflict errors anywhere in the ledger.
    harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "category": "WORK" }),
        )
        .await;
    let _ = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    let rapid = harness
        .post(
            &format!("{CONTROL}/har/load"),
            json!({ "model": "work06b" }),
        )
        .await;
    assert_eq!(rapid.0, 200, "load: {:?}", rapid.1);
    assert!(harness.wait_serving("work06b", 120).await);
    let jobs = harness.get("/har/jobs").await;
    let has_conflict = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter(|job| job["error"].is_string())
        .any(|job| {
            job["error"]
                .as_str()
                .unwrap_or_default()
                .contains("already used")
        });
    assert!(!has_conflict, "no port-conflict errors: {jobs}");
    println!("T3 PASS");
}

// ---------------------------------------------------------------- T4

#[tokio::test]
#[ignore = "operator-gated"]
async fn t04_cross_category_cycle() {
    serial_lock();
    let harness = Harness::spawn("t04");

    // T4 adapted: the singleton slot is per category; the cycle drives implicit
    // loads through the category listeners (chat = auto-load).
    for (listener, model) in [
        (AUX_LISTENER, "work06"),
        (CONTROL, "work06b"),
        (AUX_LISTENER, "work06"),
    ] {
        let (status, body) = harness.chat(listener, Some(model)).await;
        assert_eq!(status, 200, "cycle step {model}: {body}");
        assert!(harness.wait_serving(model, 120).await);
        // The other category must be empty (singleton per slot, per category).
        let status = harness.status().await;
        let other = if listener == CONTROL { "AUX" } else { "WORK" };
        assert!(
            status["slots"]
                .as_array()
                .expect("slots")
                .iter()
                .filter(|s| s["category"] == other)
                .all(|s| s["model"] == Value::Null),
            "the previous occupant was released"
        );
    }
    println!("T4 PASS (llama-only cycle; strata/ninfer SKIP in v1.0)");
}

// ---------------------------------------------------------------- T5

#[tokio::test]
#[ignore = "operator-gated"]
async fn t05_implicit_load_and_no_model_fallback() {
    serial_lock();
    let harness = Harness::spawn("t05");

    // 5.4: empty + no default -> 400 naming the listener.
    let (status, body) = harness.chat(CONTROL, None).await;
    assert_eq!(status, 400, "{body}");

    // 5.1: chat with a model name auto-loads it (implicit load).
    let (status, body) = harness.chat(CONTROL, Some("work06")).await;
    assert_eq!(status, 200, "{body}");
    assert!(harness.wait_serving("work06", 120).await);

    // 5.2: no-model request now serves the loaded occupant (F-2 rule).
    let (status, body) = harness.chat(CONTROL, None).await;
    assert_eq!(status, 200, "{body}");
    println!("T5 PASS");
}

// ---------------------------------------------------------------- T6

#[tokio::test]
#[ignore = "operator-gated: loads three 31B models (~29 GiB each, minutes)"]
async fn t06_eviction_matrix() {
    serial_lock();
    let harness = Harness::spawn("t06");
    if !harness.baseline_check(&[&big_work_gguf(), &big_rp_gguf(), &big_aux_gguf()]) {
        println!("T6 BLOCKED: 31B model files missing (SE_T_* overrides)");
        return;
    }

    // SE ledger: rp31 (29) + aux31 (29) = 58 ledger, free 10; work31 needs 29
    // -> deficit 19 -> the cheapest evictable set is aux31 (AUX is evictable by
    // WORK; RP is not) -> AUX evicted, RP KEPT (T6.2 keep + T6.3 evict).
    for model in ["rp31", "aux31"] {
        let (status, body) = harness
            .post(&format!("{CONTROL}/har/load"), json!({ "model": model }))
            .await;
        assert_eq!(status, 200, "{model}: {body}");
        assert!(harness.wait_serving(model, 600).await, "{model} Serving");
    }
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work31" }))
        .await;
    assert_eq!(status, 200, "work31 must evict AUX and load: {body}");
    assert!(harness.wait_serving("work31", 600).await);

    let status = harness.status().await;
    let of = |category: &str| {
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|s| s["category"] == category)
            .and_then(|s| s["model"].as_str())
            .map(str::to_string)
    };
    assert_eq!(of("WORK").as_deref(), Some("work31"));
    assert_eq!(of("RP").as_deref(), Some("rp31"), "RP kept (not evictable)");
    assert_eq!(of("AUX"), None, "AUX evicted (cheapest sufficient set)");

    let jobs = harness.get("/har/jobs").await;
    let swap = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["model"] == "work31")
        .expect("swap job");
    assert_eq!(swap["victims_released"], json!(["aux31"]));
    println!("T6 PASS (llama matrix: keep-RP, evict-AUX, WORK loads)");
}

// ---------------------------------------------------------------- T7

#[tokio::test]
#[ignore = "operator-gated: loads three 31B models (~29 GiB each)"]
async fn t07_admission_failure() {
    serial_lock();
    let harness = Harness::spawn("t07");
    if !harness.baseline_check(&[&big_work_gguf(), &big_rp_gguf(), &big_aux_gguf()]) {
        println!("T7 BLOCKED: 31B model files missing (SE_T_* overrides)");
        return;
    }

    // Fill WORK + RP (58 of 68 budgeted GiB); an AUX load cannot evict WORK/RP
    // (neither is evictable by AUX): 503 with named blockers (T7.1).
    for model in ["work31", "rp31"] {
        let (status, body) = harness
            .post(&format!("{CONTROL}/har/load"), json!({ "model": model }))
            .await;
        assert_eq!(status, 200, "{model}: {body}");
        assert!(harness.wait_serving(model, 600).await);
    }
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "aux31" }))
        .await;
    assert_eq!(status, 503, "admission refusal: {body}");
    let message = body["message"].as_str().unwrap_or_default();
    assert!(message.contains("insufficient VRAM"), "{message}");
    assert!(message.contains("blocked by"), "{message}");

    // 7.2 (adapted): after freeing VRAM the same load succeeds; the fresh job
    // carries no error (SE's error history is the per-job ledger).
    harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "model": "work31" }),
        )
        .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "aux31" }))
        .await;
    assert_eq!(status, 200, "retry after freeing: {body}");
    println!("T7 PASS");
}

// ---------------------------------------------------------------- T8

#[tokio::test]
#[ignore = "operator-gated: loads a 31B model (~29 GiB, minutes)"]
async fn t08_cancel_during_load() {
    serial_lock();
    let harness = Harness::spawn("t08");
    if !harness.baseline_check(&[&big_rp_gguf()]) {
        println!("T8 BLOCKED: 31B model file missing (SE_T_RP31)");
        return;
    }

    // 8.1: load the big model; mid-load, unload the category -> the load job is
    // cancelled and cleanup (kill + release confirmation) completes.
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "rp31" }))
        .await;
    assert_eq!(status, 200, "{body}");
    tokio::time::sleep(Duration::from_secs(8)).await; // mid-load
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "category": "RP" }),
        )
        .await;
    assert_eq!(status, 200, "unload mid-load = cancel");
    assert!(harness.wait_empty(60).await, "slot empties after cancel");

    // 8.2: immediately load it again - no stuck lock, no port in use.
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "rp31" }))
        .await;
    assert_eq!(status, 200, "reload after cancel: {body}");
    assert!(harness.wait_serving("rp31", 600).await);

    assert_eq!(harness.llama_server_count(), 1, "no orphan engines");
    println!("T8 PASS");
}

// ---------------------------------------------------------------- T9

#[tokio::test]
#[ignore = "operator-gated"]
async fn t09_concurrency() {
    serial_lock();
    let harness = Harness::spawn("t09");

    // 9.3: duplicate /har/load x3 while loading -> all accepted, one job.
    let url = format!("{CONTROL}/har/load");
    let body = json!({ "model": "work06" });
    let mut results = Vec::new();
    for _ in 0..3 {
        results.push(harness.post(&url, body.clone()));
    }
    for result in results {
        let (status, _) = result.await;
        assert_eq!(status, 200);
    }
    assert!(harness.wait_serving("work06", 120).await);
    let jobs = harness.get("/har/jobs").await;
    let load_jobs = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter(|job| job["model"] == "work06")
        .count();
    assert_eq!(load_jobs, 1, "identical loads share one job: {jobs}");

    // 9.4: rescan during serving does not stall.
    let (status, _) = harness
        .post(&format!("{CONTROL}/har/rescan"), json!({}))
        .await;
    assert_eq!(status, 200);
    println!("T9 PASS");
}

// ---------------------------------------------------------------- T10

#[tokio::test]
#[ignore = "operator-gated"]
async fn t10_crash_resilience() {
    serial_lock();
    let harness = Harness::spawn("t10");

    // 10.1: kill every llama-server mid-serving; the next chat triggers one
    // restart-and-retry and answers 200 (built in SE).
    let (status, _) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_serving("work06", 120).await);
    let port_old = harness.status().await["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|s| s["model"] == "work06")
        .and_then(|s| s["port"].as_u64())
        .expect("port");

    assert!(Command::new("taskkill")
        .args(["/FI", "IMAGENAME eq llama-server.exe", "/F"])
        .output()
        .expect("taskkill")
        .status
        .success());
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (status, body) = harness.chat(CONTROL, Some("work06")).await;
    assert_eq!(status, 200, "crash-retry must answer: {body}");
    let port_new = harness.status().await["slots"]
        .as_array()
        .expect("slots")
        .iter()
        .find(|s| s["model"] == "work06")
        .and_then(|s| s["port"].as_u64());
    assert_ne!(port_new, Some(port_old), "a fresh engine is serving");

    // 10.4 (adapted, FINDING-by-design): kill the HARNESS abruptly; Job Objects
    // kill every engine; a fresh boot starts with empty slots (no state
    // restore, §0.4 non-goal).
    drop(harness);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        llama_server_count_global(),
        0,
        "Job Objects killed every engine with the harness"
    );

    let harness = Harness::spawn("t10b");
    let status = harness.status().await;
    assert!(
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .all(|s| s["model"] == Value::Null),
        "boot starts with empty slots (no state restore, by design)"
    );
    println!("T10 PASS (FINDING: restart loses slot state by design, §0.4)");
}

fn llama_server_count_global() -> usize {
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq llama-server.exe", "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("llama-server.exe"))
        .count()
}

// ---------------------------------------------------------------- T11

#[tokio::test]
#[ignore = "operator-gated"]
async fn t11_external_interplay() {
    println!("T11 SKIP: adoption is scrapped in SE (operator 2026-10-24); externals are a display list landing past v1.0 (Step 13).");
}

// ---------------------------------------------------------------- T12

#[tokio::test]
#[ignore = "operator-gated"]
async fn t12_unload_semantics() {
    serial_lock();
    let harness = Harness::spawn("t12");

    // 12.1: loaded -> unloaded.
    harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert!(harness.wait_serving("work06", 120).await);
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "model": "work06" }),
        )
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_empty(30).await);

    // 12.2: not loaded -> accepted, idempotent.
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "model": "work06" }),
        )
        .await;
    assert_eq!(status, 200);

    // 12.3: category unload of a loaded slot.
    harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert!(harness.wait_serving("work06", 120).await);
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "category": "WORK" }),
        )
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_empty(30).await);

    // 12.4: {} -> explicit unload-all.
    harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "emb" }))
        .await;
    assert!(harness.wait_serving("emb", 120).await);
    let (status, _) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_empty(30).await);

    // 12.5: garbage -> 400, never an implicit unload-all.
    let status = harness
        .post_raw(
            &format!("{CONTROL}/har/unload"),
            "garbage",
            Some("application/json"),
        )
        .await;
    assert_eq!(status, 400);
    println!("T12 PASS");
}

// ---------------------------------------------------------------- T13

#[tokio::test]
#[ignore = "operator-gated"]
async fn t13_security_guards() {
    serial_lock();
    let harness = Harness::spawn("t13");
    for listener in [CONTROL, RP_LISTENER, AUX_LISTENER, EMB_LISTENER] {
        // 13.1: POST without JSON content type -> 415.
        let status = harness
            .post_raw(
                &format!("{listener}/v1/chat/completions"),
                "{}",
                Some("text/plain"),
            )
            .await;
        assert_eq!(status, 415, "{listener} content-type guard");
        // 13.2 (SE): hostname Host -> 400 (DNS-rebinding guard).
        let response = harness
            .http
            .get(listener.replace("127.0.0.1", "localhost"))
            .header("host", "evil.example")
            .send()
            .await
            .expect("request");
        assert_eq!(response.status().as_u16(), 400, "{listener} host guard");
        // 13.3 (SE): foreign Origin on a mutating POST -> 403.
        let response = harness
            .http
            .post(format!("{listener}/v1/chat/completions"))
            .header("content-type", "application/json")
            .header("origin", "http://evil.example")
            .body("{}")
            .send()
            .await
            .expect("request");
        assert_eq!(response.status().as_u16(), 403, "{listener} origin guard");
    }
    println!("T13 PASS");
}

// ---------------------------------------------------------------- T14

#[tokio::test]
#[ignore = "operator-gated: soak loop; iterations via SE_SOAK_ITERATIONS (default 3)"]
async fn t14_soak() {
    serial_lock();
    let harness = Harness::spawn("t14");
    let iterations: usize = std::env::var("SE_SOAK_ITERATIONS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3);

    let mut failures = 0;
    for iteration in 1..=iterations {
        for (listener, model) in [(CONTROL, "work06"), (AUX_LISTENER, "work06b")] {
            let (status, body) = harness.chat(listener, Some(model)).await;
            if status != 200 {
                println!("T14 iteration {iteration} {model}: FAIL ({body})");
                failures += 1;
            }
            if !harness.wait_serving(model, 120).await {
                println!("T14 iteration {iteration} {model}: serving TIMEOUT");
                failures += 1;
            }
        }
    }
    harness
        .post(&format!("{CONTROL}/har/unload"), json!({}))
        .await;
    assert!(harness.wait_empty(60).await);

    // Pass criteria (adapted): zero failed chats, zero orphans, logs bounded.
    assert_eq!(
        failures, 0,
        "{failures} chat failures across {iterations} iterations"
    );
    assert_eq!(
        harness.llama_server_count(),
        0,
        "zero orphans after the soak"
    );
    let log_files = std::fs::read_dir(&harness.logs_dir)
        .map(|entries| entries.filter_map(Result::ok).count())
        .unwrap_or(0);
    assert!(log_files <= 50, "logs/ bounded: {log_files} files");
    println!("T14 PASS ({iterations} iterations; full plan: set SE_SOAK_ITERATIONS=20)");
}
