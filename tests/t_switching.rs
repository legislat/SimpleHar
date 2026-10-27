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

fn mid_gguf() -> PathBuf {
    std::env::var("SE_T_MID")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(r"F:\MODELS_MID\Gemma-4-E4B-Uncensored-HauhauCS-Aggressive-Q8_K_P.gguf")
        })
}

/// Whether T8 cancels a mid-size load (default) or the 31B (`SE_T_MIDS=0`, or the mid
/// file is absent).
///
/// `tier.ps1` evaluates the same two conditions, so the memory gate can never disagree
/// with the test about what is about to occupy the card - which is the only property
/// that makes a gate like it worth having.
fn use_mid_t08() -> bool {
    std::env::var("SE_T_MIDS").map_or(true, |value| value != "0") && mid_gguf().exists()
}

/// The `vram_gb` to declare for `path`: the product's own estimate for an undeclared
/// model, so the ledger the admission gate reasons about here is the one it would
/// compute in production (`need_gb`, src/budget.rs:193) rather than a hand-written guess.
fn booked_gib(path: &std::path::Path) -> String {
    let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    let size = simplehar_se::budget::Gibibytes(bytes as f64 / 1024.0_f64.powi(3));
    simplehar_se::budget::need_gb(None, Some(size))
        .map(|need| format!("{:.2}", need.0))
        .unwrap_or_else(|_| "12".to_string())
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
    /// Async because the readiness probe must share the test's tokio runtime: a
    /// nested blocking runtime dropped from this context aborts the test (Run 2).
    async fn spawn(tag: &str) -> Harness {
        let work_dir = simplehar_se::testutil::temp_dir(&format!("t14-{tag}"));
        let modelconf = work_dir.join("ModelConf");
        std::fs::create_dir_all(&modelconf).expect("temp ModelConf");

        // Each entry declares the footprint the gate must book. The 31B Q6_K files
        // really do sit at ~29 GiB on the card, so saying `1` here would let the gate
        // approve three of them at once (87 GiB on a 72 GiB card) and only the driver
        // would notice, as a CUDA OOM: the whole point of T6/T7 is that admission
        // refuses, and it can only refuse against honest numbers.
        let yaml = |name: &str, category: &str, path: &PathBuf, vram: &str, extra: &str| {
            std::fs::write(
                modelconf.join(format!("{name}.yaml")),
                format!(
                    "name: {name}\nengine: llama\ncategory: {category}\nmodel: {}\ncontext: 4096\nvram_gb: {vram}{extra}\n",
                    path.display()
                ),
            )
            .expect("model yaml");
        };
        yaml("work06", "WORK", &small_gguf(), "1", "\ncpu_offload: true");
        yaml("work06b", "WORK", &small_gguf(), "1", "\ncpu_offload: true");
        // Listeners are per category and a wrong-category chat is a 404 by design
        // (T1 asserts it), so any test that drives the AUX listener needs a model
        // whose category IS AUX - `work06b` is WORK and never could serve there.
        yaml("aux06", "AUX", &small_gguf(), "1", "\ncpu_offload: true");
        yaml("emb", "EMB", &emb_gguf(), "1", "");
        // Identity sentinel (review, 2026-10-27). `/health` is anonymous - every
        // harness on the box answers it identically - so a server already listening
        // (a leaked one, or the operator's own instance) would let our child die on
        // the single-instance guard while the test drove the foreign server, where
        // `unload {}` takes down live models. Only OUR catalog carries this name.
        yaml(&format!("sentinel-{tag}"), "EMB", &emb_gguf(), "1", "");
        if big_work_gguf().exists() {
            yaml("work31", "WORK", &big_work_gguf(), "29", "");
        }
        if big_rp_gguf().exists() {
            yaml("rp31", "RP", &big_rp_gguf(), "29", "");
        }
        if big_aux_gguf().exists() {
            yaml("aux31", "AUX", &big_aux_gguf(), "29", "");
        }
        // T8's cancel target: a mid-size load is slow enough that the cancel arrives
        // while the job is genuinely still in flight, which is what the test claims to
        // prove and what a 0.6B model (ready in ~1 s) could never show.
        if use_mid_t08() {
            yaml("rp8", "RP", &mid_gguf(), &booked_gib(&mid_gguf()), "");
        }

        // The temp catalog must be named in the config: `default_modelconf_dir()` is
        // `<exe dir>/ModelConf`, i.e. target/debug/ModelConf, which on this box exists
        // and is empty - so omitting it gave every test "catalog scanned models=0"
        // and a `not found: model` assertion (T-suite Run 3).
        let config_path = work_dir.join("simplehar.json");
        std::fs::write(
            &config_path,
            json!({
                "internal_host": "127.0.0.1",
                "modelconf_dir": modelconf.display().to_string(),
            })
            .to_string(),
        )
        .expect("config");

        let exe = harness_exe();
        let mut child = Command::new(&exe)
            .env("SIMPLEHAR_CONFIG", &config_path)
            // An ambient token would authenticate every harness we spawn and then 401
            // the whole suite, because our client never sends one (review, 2026-10-27).
            .env_remove("SIMPLEHAR_TOKEN")
            .spawn()
            .expect("spawn simplehar-se.exe");

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .expect("client");
        // Readiness poll on the async client. A `reqwest::blocking` client builds its
        // own current-thread runtime and drops it on this thread, and every test here
        // runs inside `#[tokio::test]`: dropping a nested runtime from an async context
        // panics at once, so all 11 runnable tests failed before reaching a single
        // assertion while the harness itself served 200s (T-suite Run 2, 2026-10-27).
        let probe = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .expect("probe client");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if probe
                .get(format!("{CONTROL}/health"))
                .send()
                .await
                .is_ok_and(|response| response.status().as_u16() == 200)
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                // Never let a failed spawn leave a server behind: `Child::drop`
                // detaches, so an unwinding panic here used to leak a harness
                // listening on 8708-8711 and poison every later test.
                let _ = child.kill();
                let _ = child.wait();
                panic!("T-harness: the exe did not become ready in 20 s");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        // Liveness, then identity: our own child must still be running, and the server
        // that answered must be the one whose catalog only this run could have written
        // (review, 2026-10-27). Without this, a foreign harness silently impersonates a
        // fresh one and every verdict is fiction.
        if let Ok(Some(status)) = child.try_wait() {
            panic!(
                "T-harness: the exe we spawned exited ({status:?}) while something else \
                 answered {CONTROL}/health - another harness already holds 8708-8711; \
                 do not run the suite over it"
            );
        }
        let sentinel = format!("sentinel-{tag}");
        let mut identified = false;
        for _ in 0..25 {
            let value: Value = probe
                .get(format!("{CONTROL}/har/models"))
                .send()
                .await
                .expect("GET /har/models for identity")
                .json()
                .await
                .expect("models json");
            identified = value["data"]
                .as_array()
                .map(|rows| {
                    rows.iter().any(|row| {
                        row["id"] == sentinel.as_str() || row["name"] == sentinel.as_str()
                    })
                })
                .unwrap_or(false);
            if identified {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        assert!(
            identified,
            "T-harness: whatever is answering {CONTROL} has no `{sentinel}` in its catalog, \
             so it is not the harness this test spawned"
        );
        Harness {
            child,
            http,
            modelconf,
            // The product writes engine logs beside the exe (`logs/<name>-<epoch>.log`),
            // not in the temp dir; pointing here made t14's log bound assert on a
            // directory that never exists, so `unwrap_or(0)` made it unfailable.
            logs_dir: harness_exe().parent().expect("exe dir").join("logs"),
            run_tag: tag.to_string(),
        }
    }

    /// GET against an absolute URL (the four listeners are separate authorities, so a
    /// bare path cannot name one). `post` already takes absolute URLs; `get` must not
    /// be the odd one out, which is what made a per-listener check build
    /// `http://127.0.0.1:8708http://127.0.0.1:8711/v1/models` and die with
    /// `reqwest::Error { kind: Builder, source: InvalidPort }` (T-suite Run 3).
    async fn get_url(&self, url: &str) -> Value {
        self.http
            .get(url)
            .send()
            .await
            .expect("GET")
            .json()
            .await
            .expect("JSON")
    }

    /// GET against the control listener.
    /// POST on its own task. `/har/load` blocks until the engine is Serving, so a test
    /// that awaits it inline can never observe a load *in progress* — which is exactly
    /// how 8.1 became a normal unload in disguise and 9.3 never reached the in-flight
    /// attach path (review, 2026-10-27).
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

    /// The ledger row for `model`, newest first.
    async fn job_for(&self, model: &str) -> Option<Value> {
        let jobs = self.get("/har/jobs").await;
        jobs["jobs"]
            .as_array()?
            .iter()
            .rev()
            .find(|job| job["model"] == model)
            .cloned()
    }

    async fn get(&self, path: &str) -> Value {
        self.get_url(&format!("{CONTROL}{path}")).await
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

    /// The `(port, pid)` the slot table reports for the engine serving `model`.
    ///
    /// Both come from `/har/status` (`SlotStatus`), so the pair is the product's own
    /// view of the child process. `pid` is `0` when the runtime reports none, which
    /// is treated as missing rather than as a usable process identity.
    ///
    /// # Panics
    ///
    /// If no slot reports `model`, or the occupant reports no usable port or pid.
    async fn engine_of(&self, model: &str) -> (u64, u32) {
        let status = self.status().await;
        let slot = status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|s| s["model"] == model)
            .unwrap_or_else(|| panic!("no slot reports {model} as serving"));
        let port = slot["port"].as_u64().expect("engine port");
        let pid = slot["pid"]
            .as_u64()
            .and_then(|p| u32::try_from(p).ok())
            .filter(|p| *p != 0)
            .expect("engine pid");
        (port, pid)
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

    /// Polls until the newest job for `model` exists and is not terminal (or timeout).
    ///
    /// A load that already finished cannot be superseded, so any test claiming to
    /// replace or cancel an in-flight load must first see it in flight - or say it
    /// never did.
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
            tokio::time::sleep(Duration::from_millis(100)).await;
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

/// The harness exe every test drives. Integration test binaries run from
/// `target/<profile>/deps/`, so the exe sits one level up. `Harness::spawn` and the T1
/// guard must agree on that, so the derivation exists exactly once here: T-suite Run 3
/// found a second, divergent copy pointing at `deps/simplehar-se.exe`, which made t01
/// report "build the exe first" while the exe was in fact built and serving.
fn harness_exe() -> PathBuf {
    let deps_dir = std::env::current_exe()
        .expect("test exe path")
        .parent()
        .expect("deps dir")
        .to_path_buf();
    if deps_dir
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
    }
}

// ---------------------------------------------------------------- T1

#[tokio::test]
#[ignore = "operator-gated: real exe + real model files (SE_T_*)"]
async fn t01_listener_routing_guards() {
    serial_lock();
    let harness = Harness::spawn("t01").await;
    let work = harness_exe();
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

    // 1.8: per-listener /v1/models sets. Positive membership and a real count, not
    // `all()` over an array that is trivially true when empty - the very shape that hid
    // Run 3's `catalog scanned models=0` (review, 2026-10-27).
    let ids_of = |value: &Value| -> Vec<String> {
        value["data"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| row["id"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let work = harness.get_url(&format!("{CONTROL}/v1/models")).await;
    let work_ids = ids_of(&work);
    assert!(
        work_ids.contains(&"work06".to_string()) && work_ids.contains(&"work06b".to_string()),
        "the WORK listener must list both small models, got {work_ids:?}"
    );
    if big_work_gguf().exists() {
        assert!(
            work_ids.contains(&"work31".to_string()),
            "work31 is present on disk but not listed: {work_ids:?}"
        );
    }
    let emb = harness.get_url(&format!("{EMB_LISTENER}/v1/models")).await;
    let emb_ids = ids_of(&emb);
    assert!(
        emb_ids.contains(&"emb".to_string()),
        "the EMB listener must list `emb`, got {emb_ids:?}"
    );
    assert!(
        emb_ids
            .iter()
            .all(|id| id.starts_with("emb") || id.starts_with("sentinel-")),
        "the EMB listener must not leak other categories: {emb_ids:?}"
    );
    println!("T1 PASS");
}

// ---------------------------------------------------------------- T2

#[tokio::test]
#[ignore = "operator-gated"]
async fn t02_single_load_unload_llama() {
    serial_lock();
    let harness = Harness::spawn("t02").await;

    // 2.1-2.2: load the fast smoke model into AUX; the slot appears Serving.
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert_eq!(status, 200, "load: {body}");
    assert!(harness.wait_serving("work06", 120).await, "slot Serving");

    // 2.3: chat on the owning listener -> 200 with a completion. `work06` is a WORK
    // model, so its owning listener is the control port; on AUX the same request is
    // the 404 that T1 asserts (T-suite Run 3 drove it here and could never pass).
    let (status, body) = harness.chat(CONTROL, Some("work06")).await;
    assert_eq!(status, 200, "chat: {body}");
    assert!(body["choices"].as_array().is_some(), "completion: {body}");

    // 2.5: unload by model; 2.7: the engine port actually closes. Reading the slot's
    // own record proves only that the request was echoed — the same source writes it —
    // and it inspected the AUX row for a WORK engine. Release is proven the way the
    // product itself proves it: the port stops accepting (review, 2026-10-27).
    let engine_port = {
        let status = harness.status().await;
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .find(|slot| slot["category"] == "WORK" && slot["model"] == "work06")
            .and_then(|slot| slot["port"].as_u64())
            .expect("the loaded WORK slot must report an engine port while it serves")
    };
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "model": "work06" }),
        )
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_empty(30).await, "slot empty after unload");
    let still_open = std::net::TcpStream::connect_timeout(
        &std::net::SocketAddr::from(([127, 0, 0, 1], engine_port as u16)),
        Duration::from_secs(2),
    )
    .is_ok();
    assert!(
        !still_open,
        "engine port {engine_port} must stop accepting once the release is confirmed"
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
    let harness = Harness::spawn("t03").await;

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

    // 3.5 (adapted to SE): A loads, and B is posted only AFTER A is seen still loading;
    // final slot = B, and nothing in the ledger is a fault.
    //
    // Posting the pair blind makes the winner a coin-flip on arrival order: the 409
    // `cancelled: superseded by a newer load` lands on whichever request the scheduler
    // took first (Run 4 passed the coin-flip, Run 5 lost it twice). Requiring the
    // sighting is also what makes this a supersede test at all - two loads that never
    // overlap are just two loads - so an attempt that misses it is retried, and the
    // test reports the miss instead of banking a green it did not earn.
    let mut superseded = false;
    for attempt in 1..=4 {
        let (unload_status, unload_body) = harness
            .post(
                &format!("{CONTROL}/har/unload"),
                json!({ "category": "WORK" }),
            )
            .await;
        assert_eq!(unload_status, 200, "unload WORK: {unload_body}");
        assert!(
            harness.wait_empty(30).await,
            "WORK must be empty before the rapid pair"
        );

        let a = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }));
        if !harness.wait_job_in_flight("work06", 6).await {
            let _ = a.await.expect("load A task");
            println!("T3 attempt {attempt}: A finished before B could supersede it");
            continue;
        }
        let b = harness.post_task(
            &format!("{CONTROL}/har/load"),
            json!({ "model": "work06b" }),
        );
        let (ra, rb) = tokio::join!(a, b);
        let (status_a, body_a) = ra.expect("load A task");
        let (status_b, body_b) = rb.expect("load B task");
        assert!(
            status_a == 200 || status_a == 409,
            "load A must be accepted or superseded, never faulted: {body_a}"
        );
        // A 200 means the pair never actually overlapped: A finished between the
        // sighting and B reaching the actor, so the scheduler took its ordinary
        // same-slot replacement path and there was no supersede to observe. Retrying
        // is the honest answer; banking that attempt as a pass would be the same drift
        // that let the CSRF test assert 409 for years (review, 2026-10-27).
        if status_a != 409 {
            println!("T3 attempt {attempt}: A returned {status_a}, no supersede observed");
            continue;
        }
        assert_eq!(status_b, 200, "B displaced A, so B must win: {body_b}");
        assert!(harness.wait_serving("work06b", 120).await, "B ends serving");
        superseded = true;
        break;
    }
    assert!(
        superseded,
        "3.5 needs A observed loading while B arrives; every attempt found the window too narrow"
    );

    // Positive ledger facts replace the old negative substring: `"already used"`
    // appears nowhere in the product, so `!has_conflict` could never fail.
    let job_a = harness
        .job_for("work06")
        .await
        .expect("the superseded load stays in the ledger");
    assert_eq!(
        job_a["is_fault"], false,
        "a superseded load is a deliberate cancel, not a fault: {job_a}"
    );
    let job_b = harness
        .job_for("work06b")
        .await
        .expect("B is in the ledger");
    assert!(job_b["error"].is_null(), "B completed cleanly: {job_b}");
    let jobs = harness.get("/har/jobs").await;
    let faults: Vec<&Value> = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .filter(|job| job["is_fault"] == true)
        .collect();
    assert!(
        faults.is_empty(),
        "nothing in this run is a fault: {faults:?}"
    );
    println!("T3 PASS");
}

// ---------------------------------------------------------------- T4

#[tokio::test]
#[ignore = "operator-gated"]
async fn t04_cross_category_cycle() {
    serial_lock();
    let harness = Harness::spawn("t04").await;

    // T4 adapted: `max_models: 1` per category, so each category is a singleton slot
    // and a chat drives an implicit load on its OWN category's listener - a WORK model
    // on the AUX listener is the 404 T1 asserts, not a 200 (T-suite Run 3). The cycle
    // therefore drives AUX, then WORK twice, which covers both rules at once: the
    // same-category replacement (work06 -> work06b releases the previous occupant) and
    // cross-category independence.
    let occupant = |status: &Value, category: &str| -> Option<String> {
        status["slots"]
            .as_array()?
            .iter()
            .find(|slot| slot["category"] == category)
            .and_then(|slot| slot["model"].as_str())
            .map(str::to_string)
    };

    for (listener, model) in [
        (AUX_LISTENER, "aux06"),
        (CONTROL, "work06"),
        (CONTROL, "work06b"),
    ] {
        let own = if listener == CONTROL { "WORK" } else { "AUX" };
        let other = if own == "WORK" { "AUX" } else { "WORK" };
        let other_before = occupant(&harness.status().await, other);

        let (status, body) = harness.chat(listener, Some(model)).await;
        assert_eq!(status, 200, "cycle step {model}: {body}");
        assert!(
            harness.wait_serving(model, 120).await,
            "{model} never reached Serving"
        );

        let after = harness.status().await;
        assert_eq!(
            occupant(&after, own).as_deref(),
            Some(model),
            "the {own} singleton must hold the model just driven there"
        );
        // Nothing in the shipped contract releases a category the request never
        // touched; if cross-category exclusivity is intended instead, flip this
        // assertion and the PASS line below together (open operator ruling, 2026-10-27).
        assert_eq!(
            occupant(&after, other),
            other_before,
            "driving {own} must leave the {other} slot alone"
        );
    }
    println!(
        "T4 PASS (llama-only cycle; strata/ninfer SKIP in v1.0) \
         (FINDING: categories are independent, not exclusive - no cross-category release rule)"
    );
}

// ---------------------------------------------------------------- T5

#[tokio::test]
#[ignore = "operator-gated"]
async fn t05_implicit_load_and_no_model_fallback() {
    serial_lock();
    let harness = Harness::spawn("t05").await;

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
    let harness = Harness::spawn("t06").await;
    if !harness.baseline_check(&[&big_work_gguf(), &big_rp_gguf(), &big_aux_gguf()]) {
        println!("T6 BLOCKED: 31B model files missing (SE_T_* overrides)");
        return;
    }

    // SE ledger: rp31 (29) + aux31 (29) = 58 ledger, free 10; work31 needs 29
    // -> deficit 19 -> one 29 GiB victim is sufficient, and both candidates cost the
    // same. The tie must fall to disposability: AUX may be evicted by WORK and RP, RP
    // only by WORK, so AUX is the one released and RP survives. RP is NOT protected
    // from WORK (src/config/mod.rs:238 says `evictable_by: [Work]`) - it survives
    // because an equally cheap, more disposable victim existed (T6.2 keep + T6.3 evict).
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
    assert_eq!(
        of("RP").as_deref(),
        Some("rp31"),
        "RP survives: equally costly as AUX but evictable by fewer categories"
    );
    assert_eq!(of("AUX"), None, "AUX evicted (equal cost, less protected)");

    let jobs = harness.get("/har/jobs").await;
    let swap = jobs["jobs"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|job| job["model"] == "work31")
        .expect("swap job");
    assert_eq!(swap["victims_released"], json!(["aux31"]));
    println!("T6 PASS (llama matrix: evict-AUX-by-disposability, keep-RP, WORK loads)");
}

// ---------------------------------------------------------------- T7

#[tokio::test]
#[ignore = "operator-gated: loads three 31B models (~29 GiB each)"]
async fn t07_admission_failure() {
    serial_lock();
    let harness = Harness::spawn("t07").await;
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
#[ignore = "operator-gated: loads one mid-size model (~11 GiB booked); SE_T_MIDS=0 uses the 31B"]
async fn t08_cancel_during_load() {
    serial_lock();
    let harness = Harness::spawn("t08").await;
    let (model, file) = if use_mid_t08() {
        ("rp8", mid_gguf())
    } else {
        ("rp31", big_rp_gguf())
    };
    if !harness.baseline_check(&[&file]) {
        println!("T8 BLOCKED: {model} model file missing (SE_T_MID / SE_T_RP31)");
        return;
    }

    // 8.1: load it and cancel WHILE IT IS STILL LOADING. The POST is on its own task
    // because /har/load blocks until Serving: awaiting it inline means the engine
    // already finished, and the "cancel" would be an ordinary unload wearing a
    // cancel's clothes (review, 2026-10-27).
    let load = harness.post_task(&format!("{CONTROL}/har/load"), json!({ "model": model }));
    let mut in_flight = false;
    for _ in 0..40 {
        if let Some(job) = harness.job_for(model).await {
            if job["terminal"] == false {
                in_flight = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        in_flight,
        "8.1 tests a mid-load cancel; the {model} load job was never observed in flight"
    );
    let (status, _) = harness
        .post(
            &format!("{CONTROL}/har/unload"),
            json!({ "category": "RP" }),
        )
        .await;
    assert_eq!(status, 200, "unload mid-load = cancel");
    let _ = load.await.expect("load task"); // the waiter's reply may be 200/409/502
    assert!(harness.wait_empty(60).await, "slot empties after cancel");
    let cancelled = harness
        .job_for(model)
        .await
        .expect("the cancelled load stays in the ledger");
    assert_eq!(
        cancelled["is_fault"], false,
        "a deliberate cancel is policy, not a fault: {cancelled}"
    );

    // 8.2: immediately load it again - no stuck lock, no port in use.
    let (status, body) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": model }))
        .await;
    assert_eq!(status, 200, "reload after cancel: {body}");
    assert!(harness.wait_serving(model, 600).await);

    assert_eq!(harness.llama_server_count(), 1, "no orphan engines");
    println!("T8 PASS ({model} cancelled mid-load)");
}

// ---------------------------------------------------------------- T9

#[tokio::test]
#[ignore = "operator-gated"]
async fn t09_concurrency() {
    serial_lock();
    let harness = Harness::spawn("t09").await;

    // 9.3: duplicate /har/load x3 WHILE LOADING -> all accepted, one job. They must be
    // issued together and awaited together: awaiting them one by one lets the first
    // reach Serving, so #2 and #3 only ever see `Already` and the in-flight attach path
    // under test is never reached — while `load_jobs == 1` passes anyway.
    let url = format!("{CONTROL}/har/load");
    let body = json!({ "model": "work06" });
    let a = harness.post_task(&url, body.clone());
    let b = harness.post_task(&url, body.clone());
    let c = harness.post_task(&url, body.clone());
    let (ra, rb, rc) = tokio::join!(a, b, c);
    for result in [ra, rb, rc] {
        let (status, body) = result.expect("load task");
        assert_eq!(status, 200, "duplicate load: {body}");
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
    let harness = Harness::spawn("t10").await;

    // 10.1: kill the engine mid-serving; the next chat triggers one
    // restart-and-retry and answers 200 (built in SE).
    let (status, _) = harness
        .post(&format!("{CONTROL}/har/load"), json!({ "model": "work06" }))
        .await;
    assert_eq!(status, 200);
    assert!(harness.wait_serving("work06", 120).await);
    let (port_old, pid_old) = harness.engine_of("work06").await;

    // Only OUR engine is killed. The blanket `/FI "IMAGENAME eq llama-server.exe" /F`
    // also took down every unrelated llama.cpp session on the box - the same class of
    // harm the boot-reclaim rule exists to prevent, which only allows us to touch
    // processes this harness started.
    assert!(Command::new("taskkill")
        .args(["/PID", &pid_old.to_string(), "/F"])
        .output()
        .expect("taskkill")
        .status
        .success());
    tokio::time::sleep(Duration::from_secs(1)).await;

    let (status, body) = harness.chat(CONTROL, Some("work06")).await;
    assert_eq!(status, 200, "crash-retry must answer: {body}");
    let (port_new, pid_new) = harness.engine_of("work06").await;
    // Engine ports are assigned per slot and never move (`ENGINE_PORT_BASE` + slot
    // index, src/config/mod.rs:166), so a restart changes the PROCESS and keeps the
    // port. The old `assert_ne!(port_new, port_old)` was unsatisfiable by design.
    assert_eq!(
        port_new, port_old,
        "the slot must keep its assigned engine port across a crash-retry"
    );
    assert_ne!(pid_new, pid_old, "a fresh engine process must be serving");

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

    let harness = Harness::spawn("t10b").await;
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
    let harness = Harness::spawn("t12").await;

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
    //
    // This step exists to gate the SCOPE of `{}` - every occupied slot, not one
    // (src/api/mod.rs:539-542). 12.3 has just emptied WORK, so loading a single model
    // leaves the two statements this test must tell apart ("the one slot I happened to
    // free" and "every slot") indistinguishable to `wait_empty`, and a `{}` that
    // releases only the first occupied slot passes with a 31B still holding the card
    // (review, 2026-10-27). Occupy every category this harness registers
    // unconditionally - WORK, AUX, EMB - and prove they serve before the request.
    let occupants = |status: &Value, category: &str| -> Vec<String> {
        status["slots"]
            .as_array()
            .expect("slots")
            .iter()
            .filter(|slot| slot["category"] == category)
            .filter_map(|slot| slot["model"].as_str())
            .map(str::to_string)
            .collect()
    };
    for model in ["work06", "aux06", "emb"] {
        harness
            .post(&format!("{CONTROL}/har/load"), json!({ "model": model }))
            .await;
    }
    for (category, model) in [("WORK", "work06"), ("AUX", "aux06"), ("EMB", "emb")] {
        assert!(
            harness.wait_serving(model, 120).await,
            "{model} never reached Serving"
        );
        assert_eq!(
            occupants(&harness.status().await, category),
            vec![model.to_string()],
            "the {category} slot must be occupied before `{{}}` runs, or this step \
             proves nothing about the scope of unload-all"
        );
    }
    let (status, _) = harness
        .post(&format!("{CONTROL}/har/unload"), json!({}))
        .await;
    assert_eq!(status, 200);
    assert!(
        harness.wait_empty(30).await,
        "`{{}}` left the slot table non-empty"
    );
    let after = harness.status().await;
    for category in ["WORK", "RP", "AUX", "EMB"] {
        assert!(
            after["slots"]
                .as_array()
                .expect("slots")
                .iter()
                .any(|slot| slot["category"] == category),
            "the slot table must materialize a {category} slot: a missing record is \
             not an emptied one"
        );
        assert!(
            occupants(&after, category).is_empty(),
            "`{{}}` must release every category: {category} still serves {:?}",
            occupants(&after, category)
        );
    }
    assert_eq!(
        harness.llama_server_count(),
        0,
        "no engine may survive an explicit unload-all"
    );

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
    let harness = Harness::spawn("t13").await;
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
    let harness = Harness::spawn("t14").await;
    let iterations: usize = std::env::var("SE_SOAK_ITERATIONS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(3);

    let mut failures = 0;
    // The WORK legs alternate so every iteration forces a real same-category
    // replacement (the churn this soak exists to exercise); the AUX leg must use the
    // AUX-category model, since `work06b` is WORK and 404s there.
    for iteration in 1..=iterations {
        for (listener, model) in [
            (CONTROL, "work06"),
            (CONTROL, "work06b"),
            (AUX_LISTENER, "aux06"),
        ] {
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
    // Engine logs land beside the exe as `<name>-<epoch>.log`. Count only this run's
    // engines, and fail closed if the directory cannot be read: the previous version
    // counted a temp path that never exists, so `unwrap_or(0)` made this unfailable.
    let log_files = std::fs::read_dir(&harness.logs_dir)
        .map_err(|error| format!("cannot read {}: {error}", harness.logs_dir.display()))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("work06"))
                .count()
        })
        .expect("engine log directory readable");
    assert!(
        log_files <= 50,
        "this run's engine logs bounded: {log_files} files"
    );
    println!("T14 PASS ({iterations} iterations; full plan: set SE_SOAK_ITERATIONS=20)");
}
