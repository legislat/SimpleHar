use super::*;
use crate::scheduler::SlotStatus;
use axum::http::Request;
use std::path::PathBuf;
use tower::ServiceExt;

/// FINDINGS-13 [10]: every writeback class has a decided HTTP class, and the
/// mapping re-words nothing. The two config-state classes additionally owe the
/// operator the *name of the file* that refused (the module's `Display`
/// deliberately carries none), so their reason is `"<file>: " + the module's
/// sentence` — a caller-side **prefix** with the sentence intact to the last
/// byte; every other class answers `reason == error.to_string()` exactly. The
/// split is asserted per row (`names_file`), so it cannot silently widen.
/// Falsifiers: send one variant back to `Internal` and its row fails; drop the
/// prefix and the two config rows fail; corrupt the sentence inside the
/// composition and the same two rows fail on the tail equality.
#[test]
fn writeback_errors_map_to_their_http_class_without_rewording() {
    let file = PathBuf::from("ModelConf/qwen3.yaml");
    let cases: Vec<(WritebackError, StatusCode, &'static str, bool)> = vec![
        (
            WritebackError::UnrepresentableValue {
                key: "name".to_string(),
                value: "\"a # b\"".to_string(),
            },
            StatusCode::BAD_REQUEST,
            "bad_request",
            false,
        ),
        (
            WritebackError::KeyNotInFile {
                key: "ghost".to_string(),
            },
            StatusCode::BAD_REQUEST,
            "bad_request",
            false,
        ),
        (
            WritebackError::WouldBreakFile {
                problems: vec!["line 2: not a `key: value` line: `nope`".to_string()],
            },
            StatusCode::CONFLICT,
            "conflict",
            true,
        ),
        (
            WritebackError::EmptyFile,
            StatusCode::CONFLICT,
            "conflict",
            true,
        ),
        (
            WritebackError::Unreadable {
                file: PathBuf::from("ModelConf/qwen3.yaml"),
                io: "os error 2".to_string(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            false,
        ),
        (
            WritebackError::WriteFailed {
                file: PathBuf::from("ModelConf/qwen3.yaml.1.2.sh.tmp"),
                io: "os error 28".to_string(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            false,
        ),
        (
            WritebackError::RenameFailed {
                file: PathBuf::from("ModelConf/qwen3.yaml"),
                io: "os error 5".to_string(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            false,
        ),
        (
            WritebackError::PostWriteValidation {
                detail: "RESTORE FAILED: io; original bytes are in qwen3.yaml.bak".to_string(),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            false,
        ),
    ];
    for (error, status, code, names_file) in cases {
        let text = error.to_string();
        let mapped = ApiError::from_writeback(&file, error);
        assert_eq!(mapped.status(), status, "{text}");
        assert_eq!(mapped.code(), code, "{text}");
        let reason = match &mapped {
            ApiError::BadRequest { reason }
            | ApiError::Conflict { reason }
            | ApiError::Internal { reason } => reason,
            other => panic!("writeback errors map to the three decided classes, got {other:?}"),
        };
        if names_file {
            assert_eq!(
                reason,
                &format!("{}: {text}", file.display()),
                "a config-state refusal must name the file that refused"
            );
            assert!(
                reason.ends_with(&text),
                "the path must be a prefix, never a re-word: {reason}"
            );
        } else {
            assert_eq!(reason, &text, "the mapping must not re-word the module");
        }
    }
}

#[test]
fn config_write_path_follows_boot_and_never_the_legacy_file_on_fallback() {
    // Write-back must land where boot reads. Comparing the WHOLE path is what makes
    // this bite: every fallback branch ends in CONFIG_FILE, so a filename-only
    // assertion would pass even if the default directory moved under it.
    let no_source = AppConfig {
        source_path: None,
        ..AppConfig::default()
    };
    let expected = crate::config::default_config_path().expect("test process has an exe path");
    assert_eq!(
        config_write_path(&no_source),
        expected,
        "with no stamped path, write to boot's own default location"
    );
    assert_ne!(
        config_write_path(&no_source).file_name(),
        Some(std::ffi::OsStr::new(crate::config::LEGACY_CONFIG_FILE)),
        "on the fallback, SE never writes the older app's config file (an explicit \
             SIMPLEHARN_CONFIG selection at that name is the operator-instruction exception)"
    );

    let loaded = std::path::PathBuf::from(r"C:\Configs\lan.json");
    let stamped = AppConfig {
        source_path: Some(loaded.clone()),
        ..AppConfig::default()
    };
    assert_eq!(
        config_write_path(&stamped),
        loaded,
        "the file boot actually loaded always wins"
    );

    // The ruling's exception leg (2026-11-03, `TASK-DEBT-LEFTOVERS`): when the
    // operator explicitly stamps a legacy-named path, it IS the write target —
    // config file is king. Boot warns about the strand; the API writes where boot
    // read. `main.rs::boot_config` pins the warn; this pins the follow-the-stamp.
    let legacy = std::path::PathBuf::from(r"C:\Configs\simplehar.json");
    let explicit_legacy = AppConfig {
        source_path: Some(legacy.clone()),
        ..AppConfig::default()
    };
    assert_eq!(
        config_write_path(&explicit_legacy),
        legacy,
        "an explicit SIMPLEHARN_CONFIG selection is honored as the write target, \
             even at the legacy name"
    );
}

#[test]
fn model_json_cpu_offload_agrees_with_the_engine() {
    // Review finding 6: one truth set — engine/llama::jinja_on — so the page
    // checkbox can never disagree with whether `-ngl 0` was actually forced.
    let entry = |cpu: &str| ModelEntry {
        name: "m".to_string(),
        engine: "llama".to_string(),
        category: CategoryId::Work,
        vram_gb: Some(4.0),
        ram_gb: None,
        gguf_size_gb: None,
        cost: crate::model::CostClass::Fixed,
        vram_gate: crate::model::VramGate::default(),
        load_timeout_s: None,
        min_turn_s: None,
        measured_gb: None,
        measured_min_gb: None,
        measured_offload: None,
        measured_attribution: None,
        measured_profile: None,
        fit_gb: None,
        model_ref: Some(r"F:\m.gguf".to_string()),
        serve_model: None,
        file: None,
        raw: BTreeMap::from([
            ("cpu_offload".to_string(), cpu.to_string()),
            ("context".to_string(), "262144".to_string()),
        ]),
    };
    for truthy in ["true", "1", "on"] {
        assert_eq!(
            model_json(&entry(truthy), None, false)["cpu_offload"],
            json!(true),
            "`{truthy}` is engine-truthy"
        );
    }
    assert_eq!(
        model_json(&entry("yes"), None, false)["cpu_offload"],
        json!(false)
    );
    assert_eq!(
        model_json(&entry("false"), None, false)["cpu_offload"],
        json!(false)
    );
    let body = model_json(&entry("true"), None, false);
    assert_eq!(body["context"], json!("262144"));
    assert_eq!(body["model_ref"], json!(r"F:\m.gguf"));
    assert_eq!(body["cost"], json!("fixed"));
    assert!(body["launch_floor_gb"].is_null());
    assert!(body["load_timeout_s"].is_null());
    assert!(body["measured_gb"].is_null());
    assert!(
        body.get("fit_gb").is_none(),
        "a never-probed row has no fit key at all: absent is not null, and the
             fixture parity rides it"
    );
}

/// R11: the page reads the gate's answer — `cost_basis: "fit"` only when
/// the projection actually books, and the `fit_gb` figure rides the payload
/// whenever it exists (even under a declared floor that keeps booking).
#[test]
fn the_fit_figure_rides_the_payload_only_as_the_gate_decides() {
    let row = |fit: Option<f64>| ModelEntry {
        fit_gb: fit,
        ..ModelEntry {
            name: "m".to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Work,
            vram_gb: Some(24.0),
            ram_gb: None,
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            fit_gb: None,
            model_ref: None,
            serve_model: None,
            file: None,
            raw: BTreeMap::new(),
        }
    };
    let books = model_json(&row(Some(31.0)), None, false);
    assert_eq!(books["cost_basis"], json!("fit"));
    assert_eq!(books["fit_gb"], json!(31.0));
    let floored = model_json(&row(Some(20.0)), None, false);
    assert_eq!(
        floored["cost_basis"],
        json!("declared"),
        "declared is a floor: a smaller projection displays but does not book"
    );
    assert_eq!(
        floored["fit_gb"],
        json!(20.0),
        "the figure itself still rides the payload for the page"
    );
}

/// R11 session cross-check, wire shape: `fit_validated` appears ONLY as a
/// `true` alongside a row the gate books from Fit — never null, never
/// `false`, and never beside a row the gate books some other way (the
/// verdict is about a Fit booking, so a row with no Fit booking cannot
/// carry it even if the set still names the model). Falsifier: emitting
/// the flag unconditionally fails the absent legs; dropping the cost-basis
/// gate fails the declared leg.
#[test]
fn the_fit_validated_key_rides_only_a_fit_booked_row_the_session_validated() {
    let fit_row = |fit: Option<f64>, declared: Option<f64>| ModelEntry {
        fit_gb: fit,
        vram_gb: declared,
        ram_gb: None,
        ..ModelEntry {
            name: "m".to_string(),
            engine: "llama".to_string(),
            category: CategoryId::Work,
            vram_gb: None,
            ram_gb: None,
            gguf_size_gb: None,
            cost: crate::model::CostClass::Fixed,
            vram_gate: crate::model::VramGate::default(),
            load_timeout_s: None,
            min_turn_s: None,
            measured_gb: None,
            measured_min_gb: None,
            measured_offload: None,
            measured_attribution: None,
            measured_profile: None,
            fit_gb: None,
            model_ref: None,
            serve_model: None,
            file: None,
            raw: BTreeMap::new(),
        }
    };
    let validated = model_json(&fit_row(Some(31.0), None), None, true);
    assert_eq!(validated["cost_basis"], json!("fit"));
    assert_eq!(
        validated["fit_validated"],
        json!(true),
        "a session-validated Fit row carries the flag"
    );
    let unvalidated = model_json(&fit_row(Some(31.0), None), None, false);
    assert!(
        unvalidated.get("fit_validated").is_none(),
        "an unvalidated row keeps the absent key, not null: {unvalidated}"
    );
    let declared = model_json(&fit_row(Some(20.0), Some(24.0)), None, true);
    assert!(
        declared.get("fit_validated").is_none(),
        "a row the gate books from its declared floor carries no verdict about a \
             Fit booking that is not happening: {declared}"
    );
    let unprobed = model_json(&fit_row(None, None), None, true);
    assert!(
        unprobed.get("fit_validated").is_none() && unprobed.get("fit_gb").is_none(),
        "a never-probed row is byte-identical to its pre-R11 shape: {unprobed}"
    );
}

#[test]
fn est_vram_gb_only_when_declared_and_measured_are_absent() {
    let mut entry = ModelEntry {
        name: "m".to_string(),
        engine: "llama".to_string(),
        category: CategoryId::Work,
        vram_gb: None,
        ram_gb: None,
        gguf_size_gb: Some(23.0),
        cost: crate::model::CostClass::Fixed,
        vram_gate: crate::model::VramGate::default(),
        load_timeout_s: None,
        min_turn_s: None,
        measured_gb: None,
        measured_min_gb: None,
        measured_offload: None,
        measured_attribution: None,
        measured_profile: None,
        fit_gb: None,
        model_ref: Some(r"F:\m.gguf".to_string()),
        serve_model: None,
        file: None,
        raw: BTreeMap::new(),
    };
    let body = model_json(&entry, None, false);
    assert_eq!(body["est_vram_gb"], json!(23.0 * 1.15 + 2.0));
    entry.measured_gb = Some(70.5);
    let body = model_json(&entry, None, false);
    assert_eq!(body["measured_gb"], json!(70.5));
    assert!(
        body["est_vram_gb"].is_null(),
        "admission books the measurement, not the estimate"
    );
}

#[test]
fn error_body_carries_flat_and_openai_envelope() {
    // A2: both shapes present and consistent.
    let err = ApiError::NotFound {
        what: "model `x`".to_string(),
    };
    let body = error_body(&err);
    assert_eq!(body["code"], "not_found");
    assert_eq!(body["message"], "not found: model `x`");
    assert_eq!(body["error"]["type"], "simplehar_error");
    assert_eq!(body["error"]["code"], "not_found");
    assert_eq!(body["error"]["message"], body["message"]);
    // An error without a wait hint grows neither field (falsifier: render
    // the hint for every class and the `not_found` body goes red here).
    assert!(body.get("queue_position").is_none());
    assert!(body.get("expected_release_s").is_none());
}

#[test]
fn error_body_renders_the_wait_hint_as_flat_fields_only() {
    // Both fields present: place + estimate at the flat top level, seconds
    // rounded to one decimal, the OpenAI sub-object untouched.
    let err = ApiError::Unavailable {
        reason: "insufficient VRAM".to_string(),
        wait_hint: Some(crate::budget::WaitHint {
            expected_release_s: Some(4.249),
            queue_position: Some(2),
        }),
    };
    let body = error_body(&err);
    assert_eq!(body["queue_position"], json!(2));
    assert_eq!(body["expected_release_s"], json!(4.2));
    assert_eq!(body["error"]["message"], body["message"]);
    assert_eq!(body["error"]["type"], "simplehar_error");
    assert!(body["error"].get("queue_position").is_none());
    assert!(body["error"].get("expected_release_s").is_none());

    // A refusal that got no queue place renders an explicit null, and a
    // hint without any release sample omits the estimate key entirely
    // (falsifier: invent `expected_release_s: 0.0` and this leg goes red —
    // the harness must stay silent when it has no basis).
    let no_basis = ApiError::BelowLaunchFloor {
        reason: "launch floor not met".to_string(),
        wait_hint: Some(crate::budget::WaitHint::default()),
    };
    let body = error_body(&no_basis);
    assert_eq!(body["code"], "below_launch_floor");
    assert_eq!(
        body.get("queue_position"),
        Some(&Value::Null),
        "no place is a stated null, not a missing key: {body}"
    );
    assert!(body.get("expected_release_s").is_none(), "{body}");
}

/// The `whole` refusal's wire shape (ruling 2026-11-04): it rides the
/// existing `unavailable` wait posture — no new code, no `min_free`
/// figure — and the B2 hint fields behave exactly as on `Insufficient`.
/// Falsifier: map it to `below_launch_floor` instead and the code pin
/// goes red.
#[test]
fn a_whole_machine_refusal_travels_as_unavailable() {
    let admission = crate::budget::AdmissionError::WholeMachineBusy {
        required: crate::budget::Gibibytes(68.0),
        free: crate::budget::Gibibytes(40.0),
        reserve: crate::budget::Gibibytes(2.0),
        facts: Box::new(crate::budget::BudgetFacts::default()),
        blockers: vec![crate::budget::BlockerInfo {
            model: "aux-resident".to_string(),
            category: "AUX".to_string(),
            reason: "whole machine requested".to_string(),
        }],
        wait_hint: crate::budget::WaitHint {
            expected_release_s: None,
            queue_position: Some(3),
        },
    };
    let msg = admission.to_string();
    assert!(
        msg.contains("whole machine requested") && msg.contains("aux-resident"),
        "{msg}"
    );
    assert!(
        msg.contains("at least 68.0 GiB free required"),
        "the refusal must print the target it needs (B5R-02): {msg}"
    );
    let err = ApiError::from(SchedulerError::Admission(admission));
    assert_eq!(err.code(), "unavailable");
    assert_eq!(err.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = error_body(&err);
    assert_eq!(body["code"], "unavailable");
    assert_eq!(body["queue_position"], json!(3));
    assert!(
        body.get("expected_release_s").is_none(),
        "no release basis stays silent, never zero: {body}"
    );
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|m| m.contains("whole machine requested")),
        "{body}"
    );
}

#[test]
fn status_codes_come_from_types_not_text() {
    // A model literally named "not found" must not change any mapping (E1).
    let err = ApiError::NotFound {
        what: "not found model".to_string(),
    };
    assert_eq!(err.status(), StatusCode::NOT_FOUND);
    assert_eq!(err.code(), "not_found");
    assert_eq!(
        ApiError::BadRequest {
            reason: "x".to_string()
        }
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        ApiError::Unavailable {
            reason: "x".to_string(),
            wait_hint: None,
        }
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        ApiError::Upstream {
            reason: "x".to_string()
        }
        .status(),
        StatusCode::BAD_GATEWAY
    );
    assert_eq!(
        ApiError::Conflict {
            reason: "x".to_string()
        }
        .status(),
        StatusCode::CONFLICT
    );
    assert_eq!(
        ApiError::NotImplemented {
            reason: "x".to_string()
        }
        .status(),
        StatusCode::NOT_IMPLEMENTED
    );
    assert_eq!(
        ApiError::Internal {
            reason: "x".to_string()
        }
        .status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
}

#[test]
fn scheduler_errors_map_to_typed_statuses() {
    assert_eq!(
        ApiError::from(SchedulerError::UnknownModel("m".to_string())).status(),
        StatusCode::NOT_FOUND
    );
    let admission = SchedulerError::Admission(crate::budget::AdmissionError::Insufficient {
        need: crate::budget::Gibibytes(50.0),
        free: crate::budget::Gibibytes(20.0),
        reserve: crate::budget::Gibibytes(2.0),
        facts: Box::new(crate::budget::BudgetFacts {
            budget: Some(crate::budget::Gibibytes(20.0)),
            observed_free: None,
            ..Default::default()
        }),
        victims_considered: Vec::new(),
        blockers: Vec::new(),
        wait_hint: crate::budget::WaitHint {
            expected_release_s: Some(4.0),
            queue_position: Some(2),
        },
    });
    let mapped = ApiError::from(admission);
    assert_eq!(mapped.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        mapped.to_string().contains("50.0 GiB"),
        "numbers must travel: {mapped}"
    );
    assert_eq!(
        mapped.wait_hint(),
        Some(crate::budget::WaitHint {
            expected_release_s: Some(4.0),
            queue_position: Some(2),
        }),
        "the decision's wait hint rides the typed error, not only the message"
    );
    assert_eq!(
        ApiError::from(SchedulerError::Spawn("boom".to_string())).status(),
        StatusCode::BAD_GATEWAY
    );
    // A dead witness is its own client-visible class: "the card is unreadable"
    // must not be mistaken for "the card is full".
    let witness_down =
        SchedulerError::Admission(crate::budget::AdmissionError::WitnessUnavailable {
            reason: "nvidia-smi timed out".to_string(),
        });
    let mapped = ApiError::from(witness_down);
    assert_eq!(mapped.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mapped.code(), "witness_unavailable");
    assert!(
        mapped.to_string().contains("nvidia-smi timed out"),
        "the witness's reason must travel: {mapped}"
    );
}

/// The RAM refusal is a plain over-commit wait on its own resource
/// (operator ruling 2026-11-05): `unavailable` (503), the numbers in
/// the message, the decision's wait hint on the typed error — the same
/// shape `Insufficient` has, judged on host RAM.
#[test]
fn the_ram_refusal_maps_to_unavailable_with_its_numbers() {
    let admission = SchedulerError::Admission(crate::budget::AdmissionError::InsufficientRam {
        need: crate::budget::Gibibytes(24.0),
        free: crate::budget::Gibibytes(6.0),
        reserve: crate::budget::Gibibytes(8.0),
        facts: Box::new(crate::budget::BudgetFacts::default()),
        victims_considered: Vec::new(),
        blockers: Vec::new(),
        wait_hint: crate::budget::WaitHint {
            expected_release_s: None,
            queue_position: Some(3),
        },
    });
    let mapped = ApiError::from(admission);
    assert_eq!(mapped.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(mapped.code(), "unavailable");
    let msg = mapped.to_string();
    assert!(
        msg.contains("need 24.0 GiB")
            && msg.contains("free 6.0 GiB")
            && msg.contains("reserve 8.0 GiB"),
        "the RAM numbers must travel: {msg}"
    );
    assert_eq!(
        mapped.wait_hint().and_then(|h| h.queue_position),
        Some(3),
        "the decision's queue place rides the typed error"
    );
}

fn unit_state() -> ApiState {
    // The selection and classification logic never issues a scheduler command,
    // so a dummy handle (no actor) is the honest seam here.
    ApiState::new(
        AppConfig::default(),
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    )
}

/// A state with a LIVE actor: `status_document` asks the scheduler for its
/// snapshot, so [`unit_state`]'s dummy handle cannot serve a `/har/status` test.
/// The empty catalog dir keeps the actor's scan trivial and engine-free.
fn live_state(boot: BootTimings) -> ApiState {
    let config = AppConfig::default();
    let mut adapters: BTreeMap<String, Arc<dyn crate::engine::EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(crate::engine::mock::MockAdapter) as Arc<dyn crate::engine::EngineAdapter>,
    );
    let scheduler = crate::scheduler::Scheduler::start(
        config.clone(),
        crate::testutil::temp_dir("api-boot"),
        adapters,
        Arc::new(crate::process::ProcessRuntime::new()) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(crate::budget::LedgerOnlyWitness),
    );
    ApiState::new(
        config,
        scheduler,
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        boot,
    )
}

/// FINDINGS-OPEN N1: the state pump builds the merged document once and every
/// SSE subscriber reads the same published value. Falsifier: restore the
/// per-client `merged_state` loop and the second subscriber's value is a
/// different build (and the pump flag is never set).
#[tokio::test]
async fn the_state_pump_builds_once_and_publishes_to_every_subscriber() {
    let state = Arc::new(live_state(BootTimings::default()));
    state.ensure_state_pump();
    let mut first = state.state_events.subscribe();
    let second = state.state_events.subscribe();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while matches!(*first.borrow(), StateEvent::Empty) {
        tokio::time::timeout_at(deadline, first.changed())
            .await
            .expect("the pump publishes within 5s")
            .expect("the pump's sender is alive");
    }
    let doc = match &*first.borrow() {
        StateEvent::Doc(doc) => Arc::clone(doc),
        _ => panic!("the pump published a document"),
    };
    assert!(
        doc.get("models").is_some(),
        "the merged document carries the model table: {doc}"
    );
    assert!(
        matches!(*second.borrow(), StateEvent::Doc(_)),
        "a late subscriber reads the same published value, not a second build"
    );
    assert!(
        state.state_pump_started.load(Ordering::Acquire),
        "the one-shot pump flag is set"
    );
}

/// Axis 2 (ruling 2026-10-29): the loaded row's two new facts are server-computed
/// and **absent when not loaded** (never null — not-loaded is a state, not an
/// empty value), and the same-name merge is honest: earliest launch (fixed-width
/// UTC ISO sorts as it compares), any-of stale. Falsifier: emitting nulls for
/// unloaded rows (the page would read them as falsy-but-present) or a max/latest
/// merge of `launched_at` fails the respective leg.
#[test]
fn model_json_marks_loaded_rows_and_nobody_else() {
    let entry = ModelEntry::from_flat(
        &BTreeMap::from([("name".to_string(), "m".to_string())]),
        "m",
    )
    .expect("entry");
    let plain = model_json(&entry, None, false);
    assert!(
        plain.get("launched_at").is_none() && plain.get("stale_config").is_none(),
        "an unloaded row carries neither key, not even null: {plain}"
    );
    let loaded = LoadedRow {
        launched_at: "2026-10-29T03:55:12Z".to_string(),
        stale_config: true,
    };
    let body = model_json(&entry, Some(&loaded), false);
    assert_eq!(body["launched_at"], json!("2026-10-29T03:55:12Z"));
    assert_eq!(body["stale_config"], json!(true));

    let slot = |model: &str, launched: &str, stale: bool| crate::scheduler::SlotStatus {
        category: CategoryId::Work,
        index: 0,
        model: Some(model.to_string()),
        port: Some(1),
        pid: Some(1),
        need_gib: Some(4.0),
        idle_s: Some(0),
        serve_id: None,
        loading_job: None,
        state: "serving".to_string(),
        launched_at: Some(launched.to_string()),
        stale_config: Some(stale),
        container_reported_gb: None,
        loaded_at: Instant::now(),
    };
    let rows = loaded_rows(&[
        slot("twin", "2026-05-05T00:00:00Z", false),
        slot("twin", "2026-01-01T00:00:00Z", true),
        slot("single", "2026-10-29T03:55:12Z", false),
    ]);
    assert_eq!(
        rows["twin"],
        LoadedRow {
            launched_at: "2026-01-01T00:00:00Z".to_string(),
            stale_config: true,
        },
        "same-name occupants merge: EARLIEST launch, ANY stale"
    );
    assert!(!rows["single"].stale_config, "clean slot, clean row");
    assert!(!rows.contains_key("ghost"));
}

/// The structural tier of the status-zero-stat rule (the repo's established
/// `include_str!` seam): the render/read bodies that publish `stale_config` and
/// `launched_at` may not reach for the filesystem or the clock — the verdict is
/// computed only at install, the reload turn, and harness write-backs.
///
/// The two-layer contract, stated honestly: the ban-list convicts SYNONYMS
/// (bare `fs::` catches `std::fs::` and `tokio::fs::`; `metadata(` catches
/// `std::fs::metadata`, `Path::metadata()` and `MetadataExt` spellings;
/// `SystemTime::now` is banned because the stamp is stored at the actor for
/// exactly that reason), inside these bodies only. What the net CANNOT see: a
/// stat whose result is discarded — inert by definition, it cannot move a
/// verdict — and a stat behind a helper not on the heads list. Anything whose
/// result REACHES the published verdict is convicted by the behavioural tier
/// too, regardless of spelling: the cache-probe in `scheduler`'s
/// `the_snapshot_reads_the_cached_verdict_and_never_stats` and the api_surface
/// hidden-file leg of `a_flag_edit_never_ambers_its_own_loaded_model_and_status_never_stats`.
/// Tail sentinels assert each slice covers its body's end.
///
/// Falsifier: a `fs::`/`metadata(`/`fingerprint(`/`SystemTime::now` token
/// appearing in any scanned body trips this tripwire before a hot-path stall
/// can ship — including FINDINGS-BATCH-2026-10-28P [P-1]'s M-E synonym form.
#[test]
fn the_status_and_models_render_paths_never_touch_the_filesystem() {
    let source = include_str!("mod.rs");
    for (head, tail) in [
        ("async fn render_status_document(", "\"config_errors\""),
        ("fn loaded_rows(", "or_insert"),
        ("fn model_json(", "stale_config"),
        ("async fn har_models(", "\"object\": \"list\""),
        ("async fn merged_state(", "Value::Array(models)"),
    ] {
        let start = source
            .find(head)
            .unwrap_or_else(|| panic!("{head} is still here"));
        let close = source[start..]
            .find("\n}\n")
            .unwrap_or_else(|| panic!("{head} closes at column 0"));
        let body = &source[start..start + close];
        assert!(
            body.contains(tail),
            "the scan slice must reach {head}'s tail (early-close fragility)"
        );
        for banned in [
            "fs::",
            "fingerprint(",
            "read_to_string(",
            "metadata(",
            "modified(",
            "try_exists",
            "File::",
            "FileTimes",
            "OpenOptions",
            "SystemTime::now",
            "read_dir",
        ] {
            assert!(
                !body.contains(banned),
                "`{head}` may only render cached scheduler/config state; found `{banned}`"
            );
        }
    }
}

/// FINDINGS-07 [8] structural net: the two control-handler write paths must
/// run their synchronous filesystem sequences on a blocking thread, never
/// inline on the async request worker. The order test is the whole pin: the
/// write call (and the re-baseline stat that follows it) must appear only
/// AFTER `tokio::task::spawn_blocking(` in the handler body, i.e. inside the
/// blocking leg, so a reactor thread can never park behind the yaml/config
/// read-patch-rename. Response honesty is a behavioural matter and stays
/// pinned where it lives (`config_persistence_is_answered_only_after_the_bytes_read_back`
/// and the api_surface `persisted` legs) — the handler still awaits the leg's
/// verdict before answering.
///
/// Falsifier: lift any of the three fs calls back out of its `spawn_blocking`
/// closure and the order assert fires first; delete the offload entirely and
/// the contains assert fires — both red on the pre-fix shape of this file.
/// Every occurrence of each token is checked, so a second inline call added
/// after the leg's `.await` trips the same assert.
#[test]
fn the_control_write_backs_run_on_a_blocking_thread() {
    let source = include_str!("mod.rs");
    for (head, tail, write_calls) in [
        (
            "async fn set_default_model(",
            "default_overrides",
            vec!["persist_default_model("],
        ),
        (
            "async fn har_config_model(",
            "\"status\": \"edited\"",
            vec!["set_flags_on_file(", "crate::model::fingerprint("],
        ),
    ] {
        let start = source
            .find(head)
            .unwrap_or_else(|| panic!("{head} is still here"));
        let close = source[start..]
            .find(if head.contains("set_default_model") {
                "\n    }\n"
            } else {
                "\n}\n"
            })
            .unwrap_or_else(|| panic!("{head} closes where expected"));
        let body = &source[start..start + close];
        assert!(
            body.contains(tail),
            "the scan slice must reach {head}'s tail (early-close fragility)"
        );
        let offload = body
            .find("tokio::task::spawn_blocking(")
            .unwrap_or_else(|| {
                panic!("`{head}` must run its sync fs on a blocking thread (FINDINGS-07 [8])")
            });
        let joined = offload
            + body[offload..]
                .find(".await")
                .unwrap_or_else(|| panic!("`{head}`'s blocking leg is never joined"));
        for call in write_calls {
            assert!(body.contains(call), "{head} must still perform `{call}`");
            for (at, _) in body.match_indices(call) {
                assert!(
                    offload < at && at < joined,
                    "`{head}` reaches `{call}` outside its `spawn_blocking` leg — \
                         the fs call is inline on the request thread again \
                         (FINDINGS-07 [8])"
                );
            }
        }
    }
}

#[test]
fn boot_stamp_is_first_write_wins() {
    // FINDINGS-14 [4]: the two late measures are written by whoever finishes the
    // work, so the cell must admit exactly one writer. A plain store would let
    // the last supervisor to bind overwrite the first one's timestamp.
    let stamp = BootMsStamp::default();
    assert_eq!(
        stamp.get(),
        None,
        "an unstamped measure is absent, never reported as 0"
    );
    assert!(stamp.stamp(3), "the first writer wins");
    assert!(
        !stamp.stamp(9_999),
        "a later writer must be ignored, not overwrite the measure"
    );
    assert_eq!(stamp.get(), Some(3));
    let zero = BootMsStamp::default();
    assert!(zero.stamp(0), "a sub-millisecond event is still an event");
    assert_eq!(
        zero.get(),
        Some(0),
        "0 ms is a measurement, not the 'never happened' sentinel"
    );
    assert!(
        !zero.stamp(5),
        "and it still closes the cell to later writers"
    );
}

#[test]
fn concurrent_boot_stamps_admit_exactly_one_writer() {
    // The same rule under a real race: three supervisors released together, one
    // value lands and it is one of the three that were offered.
    let stamp = BootMsStamp::default();
    let barrier = std::sync::Barrier::new(3);
    let winners = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for ms in [11_u64, 22, 33] {
            let stamp = stamp.clone();
            let barrier = &barrier;
            let winners = &winners;
            scope.spawn(move || {
                barrier.wait();
                if stamp.stamp(ms) {
                    winners.lock().expect("winners lock").push(ms);
                }
            });
        }
    });
    let won = winners.lock().expect("winners lock").clone();
    assert_eq!(won.len(), 1, "exactly one writer may land; got {won:?}");
    assert_eq!(
        stamp.get(),
        Some(*won.first().expect("one winner")),
        "the value `/har/status.boot` reports is the winner's"
    );
}

#[tokio::test]
async fn status_boot_reports_the_five_measures_as_they_complete() {
    // FINDINGS-14 [4]: `/har/status.boot` must carry the whole §0.2 measure set.
    // The three boot-thread fields are present from the start; the two that
    // complete after `ApiState` is shared are ABSENT (skipped, not null) until
    // stamped, then present with exactly the stamped value — the same shape
    // `bind_ms` has always had.
    let api = live_state(BootTimings {
        config_ms: 4,
        scheduler_spawn_ms: 6,
        ..BootTimings::default()
    });
    let before = status_document(&api).await.expect("status document");
    assert_eq!(before["boot"]["config_ms"], json!(4));
    assert_eq!(before["boot"]["scheduler_spawn_ms"], json!(6));
    assert!(
        before["boot"].get("bind_ms").is_none(),
        "bind_ms keeps its skip-when-unset shape: {before}"
    );
    assert!(
        before["boot"].get("first_bind_at").is_none(),
        "an unstamped first bind is absent from the document: {before}"
    );
    assert!(
        before["boot"].get("reap_ms").is_none(),
        "an unstamped reclaim is absent from the document: {before}"
    );

    api.first_bind_stamp().stamp(1);
    api.reap_stamp().stamp(4_321);
    api.boot.write().expect("boot timings lock").set_bound(20);

    let after = status_document(&api).await.expect("status document");
    assert_eq!(after["boot"]["first_bind_at"], json!(1), "ms-since-start");
    assert_eq!(after["boot"]["reap_ms"], json!(4_321));
    assert_eq!(after["boot"]["bind_ms"], json!(20));
    assert!(
        !api.first_bind_stamp().stamp(500),
        "the shared cell behind the lock is the one the route serializes"
    );
    let again = status_document(&api).await.expect("status document");
    assert_eq!(
        again["boot"]["first_bind_at"],
        json!(1),
        "and it is still first-write-wins through the API handle"
    );
}

/// The R5 rider on `/har/status` (ruling 9): the warned names appear when the
/// snapshot reports them and the key is ABSENT otherwise — absent, not null,
/// not `[]` — so a harness with nothing stranded sends the exact old document
/// (the fixture-parity pin keeps the DOM suite honest about that shape).
#[tokio::test]
async fn status_document_carries_stranded_containers_only_when_reported() {
    let api = live_state(BootTimings::default());
    let base = api.scheduler.status().await.expect("actor alive");
    let doc = render_status_document(&api, &base).await;
    assert!(
        doc.get("stranded_containers").is_none(),
        "no report: the key is absent, not null: {doc}"
    );
    let mut reported = base.clone();
    reported.stranded_containers = Some(["r5-api-a".to_string()].into_iter().collect());
    let doc = render_status_document(&api, &reported).await;
    assert_eq!(
        doc["stranded_containers"],
        json!(["r5-api-a"]),
        "the reconcile's warned names ride /har/status"
    );
    let mut emptied = base.clone();
    emptied.stranded_containers = Some(std::collections::BTreeSet::new());
    let doc = render_status_document(&api, &emptied).await;
    assert!(
        doc.get("stranded_containers").is_none(),
        "an empty set never reaches the page as []: {doc}"
    );
}

/// FINDINGS-14 [7] + the 2026-10-28 "A + amber" ruling: a category whose
/// supervisor never spoke must be PRESENT in the `listeners` object and LABELED
/// `Pending`, so the page can never read "no verdict" as "no such category" (the
/// old shape made the two indistinguishable). Falsifier: return the registry to an
/// unseeded `new()` and the four `Pending` assertions fail on a missing key.
#[tokio::test]
async fn a_category_with_no_verdict_is_present_and_labeled_pending() {
    let api = live_state(BootTimings::default());
    let fresh = status_document(&api).await.expect("status document");
    assert_eq!(
        fresh["listeners"].as_object().map(|m| m.len()),
        Some(4),
        "the passthrough serializes every configured category, not only the marked ones: {fresh}"
    );
    for name in ["WORK", "RP", "AUX", "EMB"] {
        assert_eq!(
            fresh["listeners"][name],
            json!("Pending"),
            "a never-marked category is present AND labeled: {fresh}"
        );
    }

    api.listeners.mark_serving(CategoryId::Work);
    api.listeners.mark_down(
        CategoryId::Emb,
        "cannot bind 127.0.0.1:8711: address in use".to_string(),
    );
    let after = status_document(&api).await.expect("status document");
    assert_eq!(after["listeners"]["WORK"], json!("Serving"));
    assert_eq!(
        after["listeners"]["RP"],
        json!("Pending"),
        "neighbours keep their no-verdict label while the boot is incomplete"
    );
    assert_eq!(
        after["listeners"]["EMB"]["Down"]["reason"],
        json!("cannot bind 127.0.0.1:8711: address in use"),
        "the four verdicts stay three distinct shapes"
    );
    assert!(
        after["listeners"].get("AUX").is_some(),
        "a category nobody marked is never the same as a removed one: {after}"
    );
    assert!(
        after["listeners"].get("NOSUCH").is_none(),
        "an absent category is a MISSING key — which seeding makes unrepresentable"
    );
}

/// The two `/har/status` booleans the header badge reads, pinned as exact JSON for
/// each of the three shapes that distinguish them (FINDINGS-16 [4]): four times
/// `Serving`, one `Down`, one `Pending`. Every field carries both a true and a
/// false leg, so neither verdict can be hardcoded in the other's place.
///
/// # Falsifier
///
/// Swap the two sources (`listeners_down` from `pending()`, or both from
/// `any_down()`) and two of the three fixtures fail the exact compare; drop either
/// field and it reads `null` against `false`/`true` and fails too — the page has no
/// fallback able to absorb that, because `renderHeader` reads these booleans and no
/// longer decodes `listeners` at all (pinned by
/// `served_app_js_reads_the_server_computed_listener_verdicts`).
#[tokio::test]
async fn status_document_emits_the_two_listener_verdicts_exactly() {
    fn listener_verdicts(doc: &Value) -> Value {
        json!({
            "listeners_down": doc["listeners_down"].clone(),
            "listeners_pending": doc["listeners_pending"].clone(),
        })
    }

    let all_serving = live_state(BootTimings::default());
    for category in CategoryId::all() {
        all_serving.listeners.mark_serving(category);
    }
    let doc = status_document(&all_serving)
        .await
        .expect("status document");
    assert_eq!(
        listener_verdicts(&doc),
        json!({ "listeners_down": false, "listeners_pending": false }),
        "four times Serving is the only all-clear: {doc}"
    );

    let one_down = live_state(BootTimings::default());
    for category in CategoryId::all() {
        one_down.listeners.mark_serving(category);
    }
    one_down
        .listeners
        .mark_down(CategoryId::Aux, "address in use".to_string());
    let doc = status_document(&one_down).await.expect("status document");
    assert_eq!(
        listener_verdicts(&doc),
        json!({ "listeners_down": true, "listeners_pending": false }),
        "one Down is red, and a Down is a verdict — never also a pending: {doc}"
    );

    let one_pending = live_state(BootTimings::default());
    for category in [CategoryId::Work, CategoryId::Rp, CategoryId::Emb] {
        one_pending.listeners.mark_serving(category);
    }
    let doc = status_document(&one_pending)
        .await
        .expect("status document");
    assert_eq!(
        listener_verdicts(&doc),
        json!({ "listeners_down": false, "listeners_pending": true }),
        "the category nobody has spoken for is pending, not down: {doc}"
    );
}

/// FINDINGS-15 [10]: reading the ring must not append to the ring, or the act of
/// asking "why did the last load fail" evicts the answer. Extended to every member
/// of the routine list (FINDINGS2-0507 [1]) — `/har/models` is polled on the same
/// 2 s timer and was silently missing from the exemption, so the models panel was
/// the one poll still evicting evidence. Falsifier: drop `/har/log` (or any member)
/// from `ROUTINE_POLL_PATHS` and the zero-count assertion fails with that path's
/// `request complete` lines present.
#[tokio::test]
async fn reading_the_log_never_appends_to_the_log() {
    // The switch is process-global and off at boot; state it explicitly so this
    // pin means "exempted", not "someone turned the logging off".
    crate::observability::set_extended_logging(false);
    // A live actor, not `unit_state`: a routine poll that 500s would make the
    // exemption pass for the wrong reason (the guard only mutes 2xx polls).
    let api = Arc::new(live_state(BootTimings::default()));
    // `request complete` is its own callsite (`:1465`, not the guard's) and the
    // assertion below reads its FIRST emission, so the same process-wide interest
    // hazard that made the guard pins flaky (D9) applies here: prove it delivers
    // into a capture — on a throwaway state — before the measured window opens.
    warm_capture_lines(&["request complete"], || async {
        let warm = warm_state(None);
        let response = control_router(Arc::clone(&warm))
            .oneshot(
                Request::get("/health")
                    .header("host", "127.0.0.1:8708")
                    .body(Body::empty())
                    .expect("warm-up request"),
            )
            .await
            .expect("in-process warm-up");
        assert_eq!(response.status(), StatusCode::OK);
    })
    .await;
    let session = crate::tray::testlog::Session::start();
    let polled = pages_own_polls();
    for path in &polled {
        for _ in 0..2 {
            let response = control_router(Arc::clone(&api))
                .oneshot(
                    Request::get(path)
                        .header("host", "127.0.0.1:8708")
                        .body(Body::empty())
                        .expect("routine poll request"),
                )
                .await
                .expect("in-process routine poll");
            assert_eq!(response.status(), StatusCode::OK, "{path} must answer");
        }
    }
    // And a GET that is not a routine poll still reports, so the exemption is
    // scoped rather than an accidental mute of the whole request line.
    let other = control_router(Arc::clone(&api))
        .oneshot(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("in-process health read");
    assert_eq!(other.status(), StatusCode::OK, "/health must answer");
    let lines = session.stop();
    for path in &polled {
        let poll_lines = lines
            .iter()
            .filter(|line| line.contains("request complete") && line.contains(path))
            .count();
        assert_eq!(
            poll_lines, 0,
            "a poll the page runs every 2 s may not evict the ring it is read from: \
                 {path} logged {poll_lines} lines: {lines:?}"
        );
    }
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("request complete") && line.contains("/health"))
            .count(),
        1,
        "non-routine GETs stay in the ring (the exemption is scoped): {lines:?}"
    );
}

/// The routes `webui/app.js` actually fetches with `getJson(...)`, query stripped.
/// The behavior pin polls THIS list and the parity pin compares it to the const, so
/// shrinking `ROUTINE_POLL_PATHS` fails both: the const cannot quietly stop covering
/// a poll the page still runs (FINDINGS2-0507 [1]).
fn pages_own_polls() -> Vec<String> {
    let page = include_str!("../webui/app.js");
    let mut polled: Vec<String> = Vec::new();
    for rest in page
        .match_indices("getJson(\"")
        .map(|(index, _)| &page[index + 9..])
    {
        if let Some(end) = rest.find('"') {
            let literal = &rest[..end];
            // The page's log panel asks for a window, not the bare route; the guard
            // matches on `uri().path()`, so the query is not part of the identity.
            let route = literal.split('?').next().unwrap_or(literal).to_string();
            if route.starts_with("/har/") && !polled.contains(&route) {
                polled.push(route);
            }
        }
    }
    polled.sort();
    polled
}

/// FINDINGS2-0507 [1]: the exemption list and the page's own polling code were two
/// files that nothing linked, so the list could shrink to "everything except
/// /har/log" and stay green. The third source of truth is `webui/app.js` itself:
/// every `getJson(...)` literal the page fetches on its timer must be in
/// `ROUTINE_POLL_PATHS`, and nothing extra may be (an exempt path is a path whose
/// requests leave no trace — a mute button needs proving absent, not assumed).
/// Falsifier: remove `"/har/models"` from the const and the set equality fails.
#[test]
fn the_routine_poll_list_is_the_pages_own_poll_list() {
    let polled = pages_own_polls();
    let mut declared: Vec<String> = ROUTINE_POLL_PATHS
        .iter()
        .map(|p| (*p).to_string())
        .collect();
    declared.sort();
    assert_eq!(
        declared, polled,
        "`ROUTINE_POLL_PATHS` drifted from the page's own `getJson(...)` literals \
             (a poll that is not exempted evicts the ring; an exempted route nobody \
             polls hides its traffic)"
    );
}

/// FINDINGS2-0507 [3]: the four guard refusals used to answer and vanish while
/// benign 200s filled the same ring the operator reads after a bad boot. A refusal
/// must leave a line, but a flood must not turn that line into an eviction engine —
/// these are precisely the responses an attacker produces in bulk. One line per
/// `(class, path)`, the repeats counted, and the count published by the next
/// successful command. Falsifier: report unconditionally (drop the dedup branch in
/// `note_guard_refusal`) and the first count assertion fails with 24 lines; never
/// re-arm and the last assertion fails with 0.
#[tokio::test]
async fn guard_refusals_leave_one_line_per_class_and_path_not_one_per_packet() {
    crate::observability::set_extended_logging(false);
    // Live actor: the recovery leg needs a poll that actually answers 2xx, and a
    // 5xx would let the window stay shut while looking like it re-armed.
    let api = Arc::new(live_state(BootTimings::default()));
    // The reporting callsites are warmed and *proven* to deliver before the
    // measured window opens (see [`warm_capture_lines`]): their process-wide
    // `Interest` used to be established by an un-asserted warm-up emission that
    // a co-scheduled, capture-less sibling guard pin could win, which is the
    // D9 flake — the flood's one line absent while its tower lines were present.
    warm_guard_reporting_callsites().await;
    let session = crate::tray::testlog::Session::start();

    async fn probe(
        api: Arc<ApiState>,
        path: &'static str,
        origin: Option<&'static str>,
        json: bool,
    ) -> StatusCode {
        let mut builder = Request::post(path).header("host", "127.0.0.1:8708");
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        if json {
            builder = builder.header("content-type", "application/json");
        }
        control_router(api)
            .oneshot(builder.body(Body::empty()).expect("probe request"))
            .await
            .expect("in-process probe")
            .status()
    }

    for _ in 0..20 {
        assert_eq!(
            probe(
                Arc::clone(&api),
                "/har/rescan",
                Some("http://evil.example"),
                true
            )
            .await,
            StatusCode::FORBIDDEN,
            "the cross-site POST is refused before anything else can see it"
        );
    }
    assert_eq!(
        probe(Arc::clone(&api), "/har/rescan", None, false).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "same path, different class: the dedup unit is (class, path), not path"
    );
    assert_eq!(
        probe(
            Arc::clone(&api),
            "/har/unload",
            Some("http://evil.example"),
            true
        )
        .await,
        StatusCode::FORBIDDEN,
        "same class, different path: a second path is its own line"
    );
    // The fourth class, from a state that demands the token: 401 is what a guess
    // produces, so it must be visible — and the secret must not be.
    const OFFERED: &str = "sekrit-never-in-the-ring";
    let guarded = Arc::new(ApiState::new(
        AppConfig {
            control_token: Some(OFFERED.to_string()),
            ..AppConfig::default()
        },
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ));
    for _ in 0..2 {
        let response = control_router(Arc::clone(&guarded))
            .oneshot(
                Request::post("/har/rescan")
                    .header("host", "127.0.0.1:8708")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer nope")
                    .body(Body::empty())
                    .expect("probe request"),
            )
            .await
            .expect("in-process probe");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
    let lines = session.stop();
    assert_eq!(
        refused_lines(&lines),
        4,
        "20 identical refusals are one line, the two distinct (class, path) pairs \
             are one line each, and the token guesses are one more: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("guard refused")
            && line.contains("refusal=\"token\"")
            && line.contains("status=401")),
        "the token class renders its own line: {lines:?}"
    );
    assert!(
        lines.iter().all(|line| !line.contains(OFFERED)),
        "the ring is readable by the page: the control token may not be in it: \
             {lines:?}"
    );

    // A routine poll answers 200 but is not a command: the page's 2 s timer must
    // never re-arm a flood into a line every tick.
    let poll = control_router(Arc::clone(&api))
        .oneshot(
            Request::get("/har/status")
                .header("host", "127.0.0.1:8708")
                .body(Body::empty())
                .expect("poll request"),
        )
        .await
        .expect("in-process poll");
    assert_eq!(poll.status(), StatusCode::OK);
    let quiet = crate::tray::testlog::Session::start();
    assert_eq!(
        control_router(Arc::clone(&api))
            .oneshot(
                Request::get("/health")
                    .header("host", "127.0.0.1:8708")
                    .body(Body::empty())
                    .expect("health request"),
            )
            .await
            .expect("in-process health read")
            .status(),
        StatusCode::OK,
        "a successful command closes the window"
    );
    let closed = quiet.stop();
    let cleared = closed
        .iter()
        .filter(|line| line.contains("guard refusals cleared"))
        .collect::<Vec<_>>();
    assert_eq!(cleared.len(), 1, "exactly one summary line: {closed:?}");
    let summary = &cleared[0];
    for expected in [
        "origin on /har/rescan (+19 suppressed)",
        "content-type on /har/rescan",
        "origin on /har/unload",
    ] {
        assert!(
            summary.contains(expected),
            "the summary must name what the flood hid, with its count — missing \
                 {expected:?} in {summary}"
        );
    }

    let rearmed = crate::tray::testlog::Session::start();
    for _ in 0..3 {
        assert_eq!(
            probe(
                Arc::clone(&api),
                "/har/rescan",
                Some("http://evil.example"),
                true
            )
            .await,
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        refused_lines(&rearmed.stop()),
        1,
        "the window re-armed after the command, so a fresh flood is still visible"
    );
}

fn refused_lines(lines: &[String]) -> usize {
    lines
        .iter()
        .filter(|line| line.contains("guard refused"))
        .count()
}

/// How many times a warm-up emission is retried before the pin fails loudly.
const CAPTURE_WARM_UP_ATTEMPTS: usize = 8;

/// Emit inside a throwaway capture until every fragment in `wanted` has landed
/// (FINDINGS-BATCH-2026-10-28N D9, the flake that read as "the flood's one line
/// is missing", 1 run in 5).
///
/// A `tracing` callsite's `Interest` is cached process-wide on its first touch,
/// and the guard's reporting lines come from callsites another test may touch
/// first with no subscriber installed. The sibling that does exactly that —
/// reading bookkeeping rather than a capture — is
/// `a_refused_token_is_counted_once_and_never_carries_the_token`, and
/// `set_default` (thread-local, which is what `Session::start` uses) never
/// re-registers an already-touched callsite. The symptom is what the D9 capture
/// showed: every `tower_http` line of the refused requests present and the
/// guard's own line absent, because `TraceLayer` is applied outside the guard
/// layers and reports independently of it.
///
/// So the precondition is enforced rather than hoped for: with the throwaway
/// capture as this thread's default, `rebuild_interest_cache` re-evaluates every
/// callsite against it (the step `set_default` alone never performs), the emit
/// runs, and the lines are *asserted* to have arrived — retried until they do, and
/// failing with the capture in hand rather than letting the pin measure against a
/// line that can never appear. No test lock is needed: the captures are
/// thread-local and every warm-up traffic goes through its own `ApiState`, so a
/// sibling cannot spend the `(class, path)` budget the caller asserts about; the
/// interest cache is the only shared state, and this function repairs it.
async fn warm_capture_lines<F, Fut>(wanted: &[&'static str], mut emit: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    for attempt in 0..CAPTURE_WARM_UP_ATTEMPTS {
        let session = crate::tray::testlog::Session::start();
        tracing::callsite::rebuild_interest_cache();
        emit().await;
        let lines = session.stop();
        if wanted
            .iter()
            .all(|want| lines.iter().any(|line| line.contains(want)))
        {
            return;
        }
        assert!(
            attempt + 1 < CAPTURE_WARM_UP_ATTEMPTS,
            "the reporting callsites never delivered a line to a capture session: \
                 wanted {wanted:?} after {CAPTURE_WARM_UP_ATTEMPTS} attempts, last \
                 capture {lines:?}"
        );
    }
}

/// A state for warm-up traffic only: never the state a pin measures. The interest
/// cache is keyed by callsite, so any state exercises it, and a throwaway keeps
/// the measured window's dedup budget and fold tallies untouched.
fn warm_state(token: Option<&str>) -> Arc<ApiState> {
    Arc::new(ApiState::new(
        AppConfig {
            control_token: token.map(str::to_string),
            ..AppConfig::default()
        },
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ))
}

/// The three guard reporting callsites: `note_guard_refusal`'s `Report` and
/// `Fold` arms and `note_guard_recovery`'s summary. Every pin that asserts on
/// their lines warms them through this first.
async fn warm_guard_reporting_callsites() {
    warm_capture_lines(
        &[
            "guard refused",
            "guard refusal window full",
            "guard refusals cleared",
        ],
        || async {
            let warm = warm_state(Some("warm-up-token"));
            // Past the per-class cap in one class: the cap's worth of `Report`
            // lines and the one `Fold` line, from a class no pin asserts about.
            for index in 0..=GUARD_TRACKED_PATHS_PER_CLASS {
                let response = control_router(Arc::clone(&warm))
                    .oneshot(
                        Request::post(format!("/warm/{index}"))
                            .header("host", "127.0.0.1:8708")
                            .header("origin", "http://warm.example")
                            .header("content-type", "application/json")
                            .body(Body::empty())
                            .expect("warm-up request"),
                    )
                    .await
                    .expect("in-process warm-up");
                assert_eq!(response.status(), StatusCode::FORBIDDEN);
            }
            // A successful command after refusals: the recovery summary line.
            let health = control_router(Arc::clone(&warm))
                .oneshot(
                    Request::get("/health")
                        .header("host", "127.0.0.1:8708")
                        .header("x-har-token", "warm-up-token")
                        .body(Body::empty())
                        .expect("warm-up health read"),
                )
                .await
                .expect("in-process warm-up");
            assert_eq!(health.status(), StatusCode::OK);
        },
    )
    .await;
}

/// FINDINGS2-0507 [3] + the 15[9] token posture: reporting a refusal must never
/// mean publishing the secret, and the flood must still count as one report. The
/// assertions read the report bookkeeping and the wire bodies directly rather than
/// a captured subscriber, because which thread's thread-local default is active
/// under the parallel harness is not this pin's business (the rendering of the line
/// itself is asserted by `guard_refusals_leave_one_line_per_class_and_path_not_one_per_packet`,
/// which owns the single capture session). Falsifier: build the reported sentence
/// from the presented credential and the absence assertions fail.
#[tokio::test]
async fn a_refused_token_is_counted_once_and_never_carries_the_token() {
    crate::observability::set_extended_logging(false);
    const OFFERED: &str = "sekrit-token-value-never-logged";
    let config = AppConfig {
        control_token: Some(OFFERED.to_string()),
        ..AppConfig::default()
    };
    let api = Arc::new(ApiState::new(
        config,
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ));
    let mut bodies: Vec<String> = Vec::new();
    for _ in 0..5 {
        let response = control_router(Arc::clone(&api))
            .oneshot(
                Request::post("/har/rescan")
                    .header("host", "127.0.0.1:8708")
                    .header("content-type", "application/json")
                    .header("authorization", "Bearer wrong-token")
                    .body(Body::empty())
                    .expect("probe request"),
            )
            .await
            .expect("in-process probe");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("refusal body");
        bodies.push(String::from_utf8_lossy(&bytes).into_owned());
    }
    assert_eq!(
        api.guard_report(),
        vec![("token".to_string(), "/har/rescan".to_string(), 4)],
        "five guesses at the token are one report and a count of four"
    );
    for body in &bodies {
        assert!(
            !body.contains(OFFERED),
            "the 401 body may not echo what the ring would have carried: {body}"
        );
    }
    assert!(
        !format!("{:?}", api.guard_report()).contains(OFFERED),
        "the evidence trail names the class and the route, never the credential"
    );

    let health = control_router(Arc::clone(&api))
        .oneshot(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .header("x-har-token", OFFERED)
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("in-process health read");
    assert_eq!(health.status(), StatusCode::OK);
    assert!(
        api.guard_report().is_empty(),
        "a successful command publishes and re-arms the window: {:?}",
        api.guard_report()
    );
}

/// FINDINGS-BATCH-2026-10-28M [2]: bounding *repeats* per `(class, path)` did not
/// bound the flood, because three of the four classes are refused **pre-auth on any
/// path** — `reject_reason` runs ahead of `auth_reason` and consults neither the
/// token nor route membership. So N bogus paths were N ring lines (ring turnover,
/// operator's post-mortem gone) and N map entries (unbounded state). The window is
/// now bounded on cardinality too: `GUARD_TRACKED_PATHS_PER_CLASS` named paths per
/// class, then one fold line, then silence, and the collapsed count rides the
/// recovery line.
///
/// Proves the stated bound, not a smaller one: 3× the threshold distinct paths from
/// an unauthenticated client cost the ring exactly `cap + 1` origin lines, the state
/// holds exactly `cap` keys, and the token class keeps its own budget (a flood on one
/// wall cannot mute the evidence for another).
///
/// Falsifier: revert `note_guard_refusal` to the unbounded map (drop the cap branch
/// and fold, report whenever the key is new) and the origin-count assertion fails with
/// 24 lines, the `guard_report().len()` assertion fails with 24 keys, and the fold
/// line disappears.
#[tokio::test]
async fn a_distinct_path_flood_costs_a_constant_number_of_lines() {
    crate::observability::set_extended_logging(false);
    const OFFERED: &str = "sekrit-never-in-the-ring";
    let api = Arc::new(ApiState::new(
        AppConfig {
            control_token: Some(OFFERED.to_string()),
            ..AppConfig::default()
        },
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ));

    async fn probe(api: &Arc<ApiState>, path: &str, origin: Option<&str>) -> StatusCode {
        let mut builder = Request::post(path)
            .header("host", "127.0.0.1:8708")
            .header("content-type", "application/json");
        if let Some(origin) = origin {
            builder = builder.header("origin", origin);
        }
        control_router(Arc::clone(api))
            .oneshot(builder.body(Body::empty()).expect("probe request"))
            .await
            .expect("in-process probe")
            .status()
    }

    // Both reporting callsites this pin asserts on (`Report`, `Fold`) and the
    // recovery summary are proven to deliver into a capture before the measured
    // window opens — on a throwaway state, so none of this spends the budget or
    // the fold tally the assertions below read (see [`warm_capture_lines`]).
    warm_guard_reporting_callsites().await;

    let session = crate::tray::testlog::Session::start();
    let flood = 3 * GUARD_TRACKED_PATHS_PER_CLASS;
    for index in 0..flood {
        assert_eq!(
            probe(
                &api,
                &format!("/flood/{index}"),
                Some("http://evil.example")
            )
            .await,
            StatusCode::FORBIDDEN,
            "a foreign Origin is refused before the token, on any path"
        );
    }
    // A different class keeps its own budget: a path flood on one wall may not mute
    // the evidence for token guessing on another.
    for _ in 0..2 {
        assert_eq!(
            probe(&api, "/har/rescan", None).await,
            StatusCode::UNAUTHORIZED
        );
    }
    let lines = session.stop();

    let origin_lines = lines
        .iter()
        .filter(|line| line.contains("guard refused") && line.contains("refusal=\"origin\""))
        .count();
    assert_eq!(
        origin_lines, GUARD_TRACKED_PATHS_PER_CLASS,
        "an attacker chose {flood} distinct paths; the ring may only pay for the \
             tracked cap: {lines:?}"
    );
    let fold_lines = lines
        .iter()
        .filter(|line| line.contains("guard refusal window full"))
        .count();
    assert_eq!(
        fold_lines, 1,
        "the overflow collapses to exactly one fold line for the class: {lines:?}"
    );
    let fold = lines
        .iter()
        .find(|line| line.contains("guard refusal window full"))
        .expect("the fold line exists");
    assert!(
        fold.contains("refusal=\"origin\"")
            && fold.contains(&format!("tracked={GUARD_TRACKED_PATHS_PER_CLASS}"))
            && fold.contains("folded=1"),
        "the fold line is spent on the first collapsed path and names the cap it \
             applied; the running total is the recovery line's job (asserted below): {fold}"
    );
    assert_eq!(
        refused_lines(&lines),
        GUARD_TRACKED_PATHS_PER_CLASS + 1,
        "cap + one token line, not one line per packet: {lines:?}"
    );
    assert!(
        refused_lines(&lines) <= 4 * (GUARD_TRACKED_PATHS_PER_CLASS + 1),
        "the documented worst-case window cost is 4 classes x (cap + fold): {:?}",
        refused_lines(&lines)
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("guard refused") && line.contains("status=401"))
            .count(),
        1,
        "the token leg is untouched by the Origin flood: {lines:?}"
    );
    // The untracked paths really are absent, not merely uncounted.
    let last = format!("/flood/{}", flood - 1);
    assert!(
        lines.iter().all(|line| !line.contains(&last)),
        "{last} was past the cap and must not appear: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.contains("guard refused") && line.contains("path=/flood/0 ")),
        "the first {GUARD_TRACKED_PATHS_PER_CLASS} paths keep their detail: {lines:?}"
    );

    // Bounded state, not merely bounded lines: this is what makes the bound provable
    // rather than lucky, and no attacker-chosen path can push it further.
    let tracked_origin: Vec<_> = api
        .guard_report()
        .into_iter()
        .filter(|(class, _, _)| class == "origin")
        .collect();
    assert_eq!(
        tracked_origin.len(),
        GUARD_TRACKED_PATHS_PER_CLASS,
        "the map is capped per class: {:?}",
        api.guard_report()
    );
    assert!(
        api.guard_report().len() <= 4 * GUARD_TRACKED_PATHS_PER_CLASS,
        "state is bounded by classes x cap, whatever the flood: {:?}",
        api.guard_report()
    );
    assert_eq!(
        api.guard_folds()
            .into_iter()
            .filter(|(class, _)| class == "origin")
            .collect::<Vec<_>>(),
        vec![(
            "origin".to_string(),
            (flood - GUARD_TRACKED_PATHS_PER_CLASS) as u64
        )],
        "the collapsed paths are counted, once each (the warm-up runs against its \
             own state, so nothing here is shared with it): {:?}",
        api.guard_folds()
    );

    // Recovery semantics survive the cap: one line, and it says what was hidden.
    let recovery = crate::tray::testlog::Session::start();
    let health = control_router(Arc::clone(&api))
        .oneshot(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .header("x-har-token", OFFERED)
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("in-process health read");
    assert_eq!(health.status(), StatusCode::OK);
    let closed = recovery.stop();
    let cleared = closed
        .iter()
        .filter(|line| line.contains("guard refusals cleared"))
        .collect::<Vec<_>>();
    assert_eq!(cleared.len(), 1, "exactly one summary line: {closed:?}");
    assert!(
        cleared[0].contains("origin: +16 further paths folded"),
        "the fold is published where the operator will read it: {}",
        cleared[0]
    );
    assert!(api.guard_report().is_empty() && api.guard_folds().is_empty());

    // Re-armed: a fresh distinct path reports again, and an attacker-chosen path of
    // any length costs only the tracked budget (the key and the line agree).
    let long = format!("/long/{}", "x".repeat(4 * GUARD_TRACKED_PATH_CHARS));
    let rearmed = crate::tray::testlog::Session::start();
    assert_eq!(
        probe(&api, &long, Some("http://evil.example")).await,
        StatusCode::FORBIDDEN
    );
    let rearmed_lines = rearmed.stop();
    assert_eq!(
        refused_lines(&rearmed_lines),
        1,
        "the window re-armed after the command: {rearmed_lines:?}"
    );
    let stored = api
        .guard_report()
        .first()
        .expect("the re-armed window holds the refusal")
        .1
        .clone();
    assert_eq!(
        stored.chars().count(),
        GUARD_TRACKED_PATH_CHARS,
        "an attacker-chosen path cannot become an allocation channel: {stored}"
    );
    let reported = rearmed_lines
        .iter()
        .find(|line| line.contains("guard refused"))
        .expect("the re-armed window prints its line");
    assert!(
        stored.ends_with("...") && reported.contains(&stored),
        "the line names the same elided path the map keys on: {reported}"
    );
}

/// R2-05-01: the window re-armed on ANY unauthenticated success, and the five
/// `PUBLIC_PATHS` answer 200 without a token in both postures — so an attacker
/// alternating refused POSTs with free `GET /` packets got one `guard refused`
/// line plus one `guard refusals cleared` line per two packets, turning the ring
/// over at request rate: the exact eviction flood the cardinality cap
/// (FINDINGS-BATCH-2026-10-28M [2]) was built to end. A page or asset is not a
/// command. Falsifier: drop the `PUBLIC_PATHS` leg of the guard's re-arm
/// condition and the origin-line count fails with one line per packet, the
/// fold tally never accumulates, and the no-cleared-lines leg fails with a
/// summary per interleave.
#[tokio::test]
async fn a_public_page_success_never_rearms_the_refusal_window() {
    crate::observability::set_extended_logging(false);
    // Live actor: the closing leg needs a successful command that really answers.
    let api = Arc::new(live_state(BootTimings::default()));
    warm_guard_reporting_callsites().await;
    let session = crate::tray::testlog::Session::start();

    let flood = 3 * GUARD_TRACKED_PATHS_PER_CLASS;
    for index in 0..flood {
        let refused = control_router(Arc::clone(&api))
            .oneshot(
                Request::post(format!("/bogus/{index}"))
                    .header("host", "127.0.0.1:8708")
                    .header("origin", "http://evil.example")
                    .header("content-type", "application/json")
                    .body(Body::empty())
                    .expect("probe request"),
            )
            .await
            .expect("in-process probe");
        assert_eq!(
            refused.status(),
            StatusCode::FORBIDDEN,
            "a foreign Origin is refused before the token, on any path"
        );
        // The attacker's free re-arm packet: the public page answers 200 with
        // no token in every posture.
        let page = control_router(Arc::clone(&api))
            .oneshot(
                Request::get("/")
                    .header("host", "127.0.0.1:8708")
                    .body(Body::empty())
                    .expect("page request"),
            )
            .await
            .expect("in-process page read");
        assert_eq!(page.status(), StatusCode::OK);
    }
    let lines = session.stop();
    let origin_lines = lines
        .iter()
        .filter(|line| line.contains("guard refused") && line.contains("refusal=\"origin\""))
        .count();
    assert_eq!(
        origin_lines, GUARD_TRACKED_PATHS_PER_CLASS,
        "the attacker chose {flood} distinct paths and {flood} free page 200s; the \
             ring may still only pay for the tracked cap: {lines:?}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains("guard refusal window full"))
            .count(),
        1,
        "the overflow collapses to the class's single fold line: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| !line.contains("guard refusals cleared")),
        "a public page is not a command: no window may close and re-arm mid-flood: \
             {lines:?}"
    );
    assert_eq!(
        refused_lines(&lines),
        GUARD_TRACKED_PATHS_PER_CLASS,
        "the whole interleaved flood costs the cap, not one line per packet: \
             {lines:?}"
    );
    assert_eq!(
        api.guard_folds()
            .into_iter()
            .filter(|(class, _)| class == "origin")
            .collect::<Vec<_>>(),
        vec![(
            "origin".to_string(),
            (flood - GUARD_TRACKED_PATHS_PER_CLASS) as u64
        )],
        "the collapsed paths keep counting across the page interleaves: {:?}",
        api.guard_folds()
    );

    // A real command still closes the window exactly once and re-arms it.
    let recovery = crate::tray::testlog::Session::start();
    let health = control_router(Arc::clone(&api))
        .oneshot(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("in-process health read");
    assert_eq!(health.status(), StatusCode::OK);
    let closed = recovery.stop();
    let cleared = closed
        .iter()
        .filter(|line| line.contains("guard refusals cleared"))
        .collect::<Vec<_>>();
    assert_eq!(cleared.len(), 1, "exactly one summary line: {closed:?}");
    assert!(
        cleared[0].contains(&format!(
            "origin: +{} further paths folded",
            flood - GUARD_TRACKED_PATHS_PER_CLASS
        )),
        "the real command publishes the whole collapse the pages hid: {}",
        cleared[0]
    );
    assert!(api.guard_report().is_empty() && api.guard_folds().is_empty());
}

/// The `reason` field value of a captured `guard refused` line. The test
/// capture renders `%value` fields raw (unquoted) in BTreeMap name order, so
/// the value runs from `reason=` to the next field, ` refusal=`.
fn captured_reason(line: &str) -> &str {
    let start = line
        .find("reason=")
        .expect("the report line carries its reason")
        + "reason=".len();
    let rest = &line[start..];
    let end = rest
        .find(" refusal=")
        .expect("the reason field is followed by refusal");
    &rest[..end]
}

/// R2-05-02: the logged refusal `reason` embedded the attacker's own path,
/// `Host` or `Origin` at full size — hyper's head bound alone lets one request
/// carry ~408 KiB of them into each of the 500 ring lines — while the
/// `GUARD_TRACKED_PATH_CHARS` doc says the line and the key are capped
/// together. Falsifier: pass `reason = %reason` uncapped and the length leg
/// fails with a several-hundred-char value on both classes. The client wire
/// body keeps the caller's own full path on purpose.
#[tokio::test]
async fn a_guard_refusal_line_carries_a_reason_capped_to_the_tracked_width() {
    crate::observability::set_extended_logging(false);
    const OFFERED: &str = "sekrit-never-in-the-ring";
    let api = Arc::new(ApiState::new(
        AppConfig {
            control_token: Some(OFFERED.to_string()),
            ..AppConfig::default()
        },
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ));
    warm_guard_reporting_callsites().await;
    let session = crate::tray::testlog::Session::start();

    // Token class: a kilobyte-long path refused 401 before route membership.
    let long_path = format!("/long/{}", "p".repeat(4 * GUARD_TRACKED_PATH_CHARS));
    let response = control_router(Arc::clone(&api))
        .oneshot(
            Request::post(long_path.as_str())
                .header("host", "127.0.0.1:8708")
                .header("content-type", "application/json")
                .body(Body::empty())
                .expect("probe request"),
        )
        .await
        .expect("in-process probe");
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("refusal body");
    let body = String::from_utf8_lossy(&body).into_owned();
    assert!(
        body.contains(&long_path),
        "the client wire body keeps the caller's own full path — only the \
             operator-facing line is capped: {body}"
    );

    // Host class: an oversized `Host` header on an ordinary path.
    let huge_host = format!(
        "{}.evil.test:8708",
        "h".repeat(4 * GUARD_TRACKED_PATH_CHARS)
    );
    let response = control_router(Arc::clone(&api))
        .oneshot(
            Request::post("/har/rescan")
                .header("host", huge_host.as_str())
                .header("content-type", "application/json")
                .body(Body::empty())
                .expect("probe request"),
        )
        .await
        .expect("in-process probe");
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    let lines = session.stop();
    let refused: Vec<&String> = lines
        .iter()
        .filter(|line| line.contains("guard refused"))
        .collect();
    assert_eq!(
        refused.len(),
        2,
        "one token line and one host line, both attacker-sized on the old code: \
             {lines:?}"
    );
    for line in &refused {
        let reason = captured_reason(line);
        assert!(
            reason.chars().count() <= GUARD_TRACKED_PATH_CHARS,
            "the ring/file `reason` field may not carry an attacker-sized value \
                 ({} chars): {reason}",
            reason.chars().count()
        );
        assert!(
            reason.ends_with("..."),
            "a hyper-sized input must show the elision, not a silent truncation: \
                 {reason}"
        );
    }
    let token_line = refused
        .iter()
        .find(|line| line.contains("refusal=\"token\""))
        .expect("the token class reports");
    assert!(
        captured_reason(token_line).starts_with("unauthorized:"),
        "the cap keeps the class prefix the operator reads: {token_line}"
    );
}

/// FINDINGS2-0507 [2]: structural refusals must be answered strictly before the
/// token check (README:265-268, DECISIONS LOCKED). A page that knows nothing about
/// the token must be told the security reason it triggered, not that a route wants
/// a secret — the 401 is an oracle for "this route exists and is protected".
/// Falsifier: move the `auth_reason` block above `reject_reason` in `guard` and all
/// three structural arms below fail with 401. The fourth arm is FINDINGS-05 [5]:
/// a request that carries no `Host` at all is itself a structural refusal and is
/// answered `400`, not allowed past the DNS-rebinding gate to the token.
#[tokio::test]
async fn structural_refusals_are_answered_before_the_token_is_consulted() {
    let config = AppConfig {
        control_token: Some("a-token-never-presented".to_string()),
        ..AppConfig::default()
    };
    let api = Arc::new(ApiState::new(
        config,
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    ));

    let structural = |builder: axum::http::request::Builder| async {
        control_router(Arc::clone(&api))
            .oneshot(builder.body(Body::empty()).expect("probe request"))
            .await
            .expect("in-process probe")
            .status()
    };
    assert_eq!(
        structural(Request::post("/har/rescan").header("host", "127.0.0.1:8708")).await,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a content-type probe with no token is a 415, never a 401"
    );
    assert_eq!(
        structural(
            Request::post("/har/rescan")
                .header("host", "127.0.0.1:8708")
                .header("content-type", "application/json")
                .header("origin", "http://evil.example")
        )
        .await,
        StatusCode::FORBIDDEN,
        "a cross-site POST with no token is a 403, never a 401"
    );
    assert_eq!(
        structural(
            Request::post("/har/rescan")
                .header("content-type", "application/json")
                .header("host", "rebind.example")
        )
        .await,
        StatusCode::BAD_REQUEST,
        "a DNS-rebinding Host with no token is a 400, never a 401"
    );
    assert_eq!(
        structural(
            Request::post("/har/rescan")
                .header("content-type", "application/json")
                .header("x-har-token", "a-token-never-presented")
        )
        .await,
        StatusCode::BAD_REQUEST,
        "the right token does not excuse a request the Host gate cannot inspect"
    );
    // And the token check is still there behind them: structurally clean + no
    // credential is the one case that may answer 401.
    assert_eq!(
        structural(
            Request::post("/har/rescan")
                .header("host", "127.0.0.1:8708")
                .header("content-type", "application/json")
        )
        .await,
        StatusCode::UNAUTHORIZED,
        "with nothing structural to refuse, the token is what gates the route"
    );
}

/// FINDINGS-05 [4] + FINDINGS-16 N[1]/N[2]: the Host gate must be exactly as
/// tight as its doc. The authority/port split is pinned directly (brackets
/// required for IPv6, unparseable ports refused), and the gate accepts IP
/// literals that are loopback — or any address on the only non-loopback bind
/// boot validation allows (`0.0.0.0`) — plus `localhost`, each only on one of
/// the harness's own configured ports. Pre-batch every IP literal passed
/// (`203.0.113.9`, `0.0.0.0`) and `rsplit_once(':')` threw the port away, so
/// the routable-address and wrong-port legs read red against it; the
/// publish-bind accept leg reads red against the dead equality-against-
/// `internal_host` leg the batch shipped (FINDINGS-16 N[1]).
#[test]
fn the_host_gate_accepts_loopback_published_binds_and_our_own_ports() {
    for (host, authority, port) in [
        ("127.0.0.1:8708", "127.0.0.1", Some(8708u16)),
        ("[::1]:8708", "::1", Some(8708)),
        ("[0:0:0:0:0:0:0:1]", "0:0:0:0:0:0:0:1", None),
        ("localhost", "localhost", None),
        ("localhost:8709", "localhost", Some(8709)),
    ] {
        assert_eq!(
            split_host(host),
            Some((authority, port)),
            "a Host a legitimate browser sends must parse: {host}"
        );
    }
    for host in [
        "::1",
        "127.0.0.1:",
        "127.0.0.1:99x",
        "127.0.0.1:8708:9",
        "[::1",
        "[::1]x",
    ] {
        assert_eq!(split_host(host), None, "malformed Host {host:?} is refused");
    }
    let default = ApiState::new(
        AppConfig::default(),
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    );
    for host in [
        "127.0.0.1",
        "127.0.0.1:8708",
        "127.0.0.42:8711",
        "localhost:8708",
        "LOCALHOST",
        "[::1]:8710",
    ] {
        assert!(
            loopback_host(&default, host),
            "{host} addresses this harness"
        );
    }
    for host in [
        "203.0.113.9:8708",
        "0.0.0.0:8708",
        "10.1.2.3:8708",
        "198.51.100.7:8708",
        "evil.example.com",
        "::1",
        "[fe80::1]:8708",
        "127.0.0.1:9999",
        "[::1]:18109",
    ] {
        assert!(
            !loopback_host(&default, host),
            "{host} is not this harness's loopback address on its own port"
        );
    }
    // The publish posture is the only non-loopback bind boot `validate`
    // allows, so the second acceptance rule is pinned on `0.0.0.0`, never on
    // a hand-built unicast bind the boot validator refuses (FINDINGS-16
    // N[2]): any address the bind covers answers on its own port, the port
    // rule still refuses a foreign port, and hostname clients stay refused
    // — the DNS-rebinding defense does not care how the harness is bound.
    let published = ApiState::new(
        AppConfig {
            internal_host: "0.0.0.0".parse().expect("publish bind"),
            ..AppConfig::default()
        },
        SchedulerHandle::test_dummy(),
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    );
    assert!(
        loopback_host(&published, "192.168.1.23:8708"),
        "a published harness answers LAN clients addressing its real address"
    );
    assert!(
        !loopback_host(&published, "192.168.1.23:9999"),
        "the publish widening does not excuse a foreign port"
    );
    assert!(
        !loopback_host(&published, "evil.example:8708"),
        "a hostname Host stays refused even on the publish bind"
    );
}

/// FINDINGS-05 [8]: the credential wall compares every byte of an
/// equal-length pair (`difference |= g ^ e`, never a short-circuiting `==`),
/// and the `Bearer` scheme is stripped in any ASCII case (RFC 7235) — the old
/// byte-exact `strip_prefix("Bearer ")` refused `bearer <token>` from
/// OpenAI-style clients with a message that read as a wrong token. The
/// structural legs read the shipped source, because a passing behavioral
/// matrix today does not stop `==` creeping back tomorrow.
#[test]
fn the_token_wall_is_constant_time_and_scheme_case_insensitive() {
    assert!(token_matches(b"secret", b"secret"));
    assert!(token_matches(b"", b""));
    for index in 0..6 {
        let mut differing = *b"secret";
        differing[index] ^= 0x20;
        assert!(
            !token_matches(&differing, b"secret"),
            "byte {index} must decide the verdict"
        );
    }
    assert!(!token_matches(b"secre", b"secret"), "shorter is refused");
    assert!(!token_matches(b"secre!", b"secret"), "longer is refused");
    for scheme in ["Bearer", "bearer", "BEARER", "BeArEr"] {
        assert_eq!(
            presented_token(&format!("{scheme} s3cret")),
            "s3cret",
            "RFC 7235: the auth-scheme is case-insensitive"
        );
    }
    assert_eq!(
        presented_token("s3cret"),
        "s3cret",
        "without a scheme the value IS the token"
    );
    assert_eq!(
        presented_token("bearer  s3cret "),
        "s3cret",
        "the trim matches `resolve_control_token`"
    );
    let source = include_str!("mod.rs");
    let body_of = |name: &str| {
        let start = source
            .find(name)
            .unwrap_or_else(|| panic!("{name} must exist"));
        source[start..start + source[start..].find("\n}\n").expect("function end") + 3].to_string()
    };
    let auth = body_of("fn auth_reason(");
    assert!(
        auth.contains("token_matches(") && !auth.contains("given == expected"),
        "auth_reason must compare through the constant-time wall: back to `==` is \
             the FINDINGS-05 [8] regression: {auth}"
    );
    let fold = body_of("fn token_matches(");
    assert!(
        fold.contains("|=") && fold.contains('^'),
        "the wall must examine every byte of an equal-length pair: {fold}"
    );
    let scheme = body_of("fn presented_token(");
    assert!(
        scheme.contains("eq_ignore_ascii_case(\"bearer\")")
            && !scheme.contains("strip_prefix(\"Bearer \")"),
        "scheme stripping must be ASCII-case-insensitive: {scheme}"
    );
}

/// TASK-RENAME-SIMPLEHARN pin (c, wiring half): the guard's only env read is
/// `crate::config::token_from_env` — the name pair (`SIMPLEHARN_TOKEN` first,
/// deprecated `SIMPLEHAR_TOKEN` second) and the once-per-process warning are
/// pinned in `config`; a direct `std::env::var` here would silently drop the
/// fallback or the warning while the guard stays green. Falsifier: revert this
/// line to `std::env::var("SIMPLEHARN_TOKEN")` and the assert fires.
#[test]
fn the_control_token_reads_env_only_through_the_renamed_reader() {
    let source = include_str!("mod.rs");
    let body = source
        .split("fn resolve_control_token(")
        .nth(1)
        .expect("the resolver must stay")
        .split("\n}")
        .next()
        .expect("the resolver must close");
    assert!(
        body.contains("crate::config::token_from_env()"),
        "the env read must go through the primary/fallback pair, not one raw name: {body}"
    );
    assert!(
        !body.contains("std::env::var"),
        "a second raw env read here bypasses the deprecated-name fallback and its \
             warning: {body}"
    );
}

/// FINDINGS-05 [6]: the two harness-failure classes carry OS-level failure
/// text — paths into model yamls, log files, the engine exe. The class stays
/// legible (`code` and message prefix unchanged, both envelope shapes equal),
/// but the client body must not carry the detail; `error_body` sends it to
/// `tracing::error!` instead (the live 500 with a capturing subscriber is
/// pinned in `tests/api_surface.rs`). The request-side classes still state
/// their own numbers verbatim: the withholding is scoped, not a mute.
#[test]
fn harness_failure_bodies_hide_the_os_detail_but_keep_the_class() {
    let leaked = "cannot read C:\\ModelConf\\qwen3.yaml: Access is denied. (os error 5)";
    for (error, prefix) in [
        (
            ApiError::Internal {
                reason: leaked.to_string(),
            },
            "internal error:",
        ),
        (
            ApiError::Upstream {
                reason: leaked.to_string(),
            },
            "upstream failure:",
        ),
    ] {
        let body = error_body(&error);
        assert_eq!(body["code"], error.code());
        assert_eq!(body["error"]["code"], error.code());
        assert_eq!(body["error"]["message"], body["message"]);
        let message = body["message"].as_str().expect("string message");
        assert!(
            message.starts_with(prefix),
            "{prefix} must survive: {message}"
        );
        assert!(
            message.contains("harness log"),
            "the body must say where the full reason lives: {message}"
        );
        for secret in ["cannot read", "ModelConf", "qwen3.yaml", "os error"] {
            assert!(
                !body.to_string().contains(secret),
                "the client body may not carry OS detail ({secret}): {body}"
            );
        }
    }
    let numbers = ApiError::Unavailable {
        reason: "the load needs 18.0 GiB free; 9.4 GiB is".to_string(),
        wait_hint: None,
    };
    let body = error_body(&numbers);
    assert!(
        body["message"]
            .as_str()
            .is_some_and(|message| message.contains("18.0 GiB free; 9.4 GiB")),
        "admission numbers are the client's business: {body}"
    );
}

/// FINDINGS-05 [7]: the ring is the operator's only "why did the last load
/// fail" channel and the guard its one request producer, so a client id must
/// not become an eviction channel or a megabyte echo. The useful half (a
/// short printable id echoes) is pinned by
/// `request_ids_are_echoed_or_generated`; this pins the wall — an oversized
/// id, or one with a space or a tab, regenerates a `req-` id in the response
/// header AND in the `request complete` line, and the rejected text never
/// reaches the ring. Falsifier: drop the `.filter(accepted_request_id)` in
/// `guard` and the three regenerated legs echo verbatim red.
#[tokio::test]
async fn client_request_ids_are_capped_and_sanitized_before_echo_and_log() {
    crate::observability::set_extended_logging(false);
    let api = Arc::new(unit_state());
    warm_capture_lines(&["request complete"], || async {
        let response = control_router(Arc::clone(&api))
            .oneshot(
                Request::get("/health")
                    .header("host", "127.0.0.1:8708")
                    .body(Body::empty())
                    .expect("warm-up request"),
            )
            .await
            .expect("in-process warm-up");
        assert_eq!(response.status(), StatusCode::OK);
    })
    .await;
    let session = crate::tray::testlog::Session::start();
    let echoed_id = |builder: axum::http::request::Builder| {
        let api = Arc::clone(&api);
        async move {
            let response = control_router(api)
                .oneshot(builder.body(Body::empty()).expect("probe request"))
                .await
                .expect("in-process probe");
            assert_eq!(response.status(), StatusCode::OK);
            response
                .headers()
                .get(&X_REQUEST_ID)
                .expect("request id header")
                .to_str()
                .expect("ascii id")
                .to_string()
        }
    };
    let printable_64 = "i".repeat(REQUEST_ID_MAX_CHARS);
    assert_eq!(
        echoed_id(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .header("x-request-id", &printable_64)
        )
        .await,
        printable_64,
        "a printable id at the cap still echoes verbatim"
    );
    let oversized = "a".repeat(REQUEST_ID_MAX_CHARS + 1);
    let rejected_ids = [&oversized[..], "has space", "tab\there"];
    for rejected in &rejected_ids {
        let echoed = echoed_id(
            Request::get("/health")
                .header("host", "127.0.0.1:8708")
                .header("x-request-id", *rejected),
        )
        .await;
        assert!(
            echoed.starts_with("req-") && echoed != *rejected,
            "a {rejected:?} id must regenerate, not echo or truncate into an \
                 id-shaped echo: {echoed:?}"
        );
    }
    let lines = session.stop();
    assert!(
        lines
            .iter()
            .any(|line| line.contains("request complete") && line.contains(&printable_64)),
        "the accepted id is the id in the ring line: {lines:?}"
    );
    for rejected in &rejected_ids {
        assert!(
            lines.iter().all(|line| !line.contains(*rejected)),
            "the rejected id {rejected:?} must never reach the ring: {lines:?}"
        );
    }
}

/// FINDINGS2-0507 [7]: `rename()` returning `Ok` is the OS accepting the move, not
/// proof the bytes landed, and this is the one file the harness re-reads at boot
/// under `deny_unknown_fields` — a silent-short config write is a harness that will
/// not start. `persisted: true` is therefore earned by reading the file back and
/// requiring it to parse to the tree that was published.
///
/// The false leg goes through the **real** comparison: the test arm corrupts the
/// on-disk bytes inside the verify window (valid JSON, one extra key), so it is
/// production's `read_back == root` that refuses the answer
/// (FINDINGS-BATCH-2026-10-28M, item 6 note). Falsifiers, both caught: delete the
/// verification block and the armed leg answers `persisted: true` for bytes nobody
/// checked; or replace the comparison with an always-true `Ok(_) => Ok(())` and the
/// same leg fails while every other pin stays green — which is exactly the
/// rubber-stamp the seam used to be unable to detect. The leg's outcome is
/// `LandedUnverified` with the verdict of the LANDED bytes (R2-07-02): the rename
/// succeeded, so the live file holds them and `/har/status` must describe them.
#[test]
fn config_persistence_is_answered_only_after_the_bytes_read_back() {
    let dir = crate::testutil::temp_dir("api-config-readback");
    let path = dir.join("simpleharn.json");
    let config = AppConfig {
        source_path: Some(path.clone()),
        ..AppConfig::default()
    };

    let (persisted, verdict) =
        match persist_default_model(&config, CategoryId::Work, Some("honest.gguf")) {
            PersistOutcome::Landed { verdict } => (true, verdict),
            other => {
                panic!("a write that reads back as published must report Landed, never {other:?}")
            }
        };
    assert!(
        persisted,
        "a write that reads back as published is a persistence the API may claim"
    );
    assert!(
        verdict.is_empty(),
        "clean published bytes earn an empty boot-config verdict: {verdict:?}"
    );
    let on_disk: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("the config file exists after a true answer"),
    )
    .expect("temp + rename never expose a partial file");
    assert_eq!(
        on_disk["categories"]
            .as_array()
            .expect("categories array")
            .iter()
            .find(|entry| entry["id"].as_str() == Some(CategoryId::Work.to_string().as_str()))
            .and_then(|entry| entry["default_model"].as_str()),
        Some("honest.gguf"),
        "and the answer means the field is on disk, not merely in memory: {on_disk}"
    );

    // The volume accepted the rename and the bytes are there — they are simply not
    // the tree that was published, and only the real compare can tell.
    arm_config_read_back_fault(&path);
    let outcome = persist_default_model(&config, CategoryId::Work, Some("phantom.gguf"));
    let (persisted, verdict) = match outcome {
        PersistOutcome::LandedUnverified { verdict } => (false, verdict),
        other => panic!(
            "a rename that landed without read-back confirmation must report \
                 LandedUnverified, never {other:?}"
        ),
    };
    assert!(
        !persisted,
        "an unverifiable write may only ever be answered `persisted: false`"
    );
    let named = verdict
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        named.contains(crate::config::CONFIG_FILE) && named.contains("tampered"),
        "the rename landed, so the live file holds these bytes: the outcome must \
             carry what THEY say the next boot would refuse (file root + landed key), \
             not an empty verdict that lets `/har/status` keep asserting the previous \
             one (R2-07-02): {named}"
    );
    release_config_read_back_fault(&path);
    let tampered: Value = serde_json::from_str(
        &std::fs::read_to_string(&path).expect("the file is present, merely unverified"),
    )
    .expect("the corruption is different-but-valid, not garbage");
    assert_eq!(
        tampered["tampered"],
        Value::Bool(true),
        "the leg has to be reached by genuinely-diverging bytes, or an always-true \
             compare would sail through it: {tampered}"
    );
    assert_eq!(
        tampered["categories"]
            .as_array()
            .expect("categories array")
            .iter()
            .find(|entry| entry["id"].as_str() == Some(CategoryId::Work.to_string().as_str()))
            .and_then(|entry| entry["default_model"].as_str()),
        Some("phantom.gguf"),
        "the refused write did land — `persisted: false` is a verdict about \
             verification, not about whether the rename worked: {tampered}"
    );
    let backup: Value = serde_json::from_str(
        &std::fs::read_to_string(path.with_extension("json.bak"))
            .expect("the pre-write copy is kept for exactly this case"),
    )
    .expect("the backup is a config file");
    assert_eq!(
        backup["categories"]
            .as_array()
            .expect("categories array")
            .iter()
            .filter(|entry| entry["default_model"].as_str() == Some("honest.gguf"))
            .count(),
        1,
        "the last value the harness can vouch for is still recoverable: {backup}"
    );
}

/// R2-07-01 flush-before-publish tripwire — **a pin by inspection, not a
/// power-loss falsifier** (the same honesty label `model::writeback` puts on
/// its own tripwire: a crash rig is card-window material, FINDINGS-07 [1]).
/// What it holds is the shape the durability line is made of:
/// [`persist_default_model`] — the only runtime writer of the file the harness
/// re-reads at boot under `deny_unknown_fields` — publishes its temp and its
/// `.json.bak` copy through the same flushed `write_synced` door as the yaml
/// publisher and the first-run config writer, and reaches no cache-only
/// create/copy primitive in the body. Routing either publish back through the
/// cache-only primitive turns this red; that is exactly the shape in which a
/// rename could expose bytes the volume was never told to commit while the
/// last API answer said `persisted: true`. Comments are stripped before the
/// match, so prose cannot satisfy or fake the asserts
/// (FINDINGS-BATCH-2026-10-28C [5] discipline).
#[test]
fn the_boot_config_publish_flushes_through_write_synced() {
    let source = include_str!("mod.rs");
    let start = source
        .find("fn persist_default_model(")
        .expect("persist_default_model is still here");
    let rest = &source[start..];
    let end = rest
        .find("\n}\n")
        .expect("persist_default_model closes at column 0");
    let body = rest[..end]
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        body.contains("write_synced("),
        "the boot-config temp and its backup must publish through the flushed \
             write_synced door (R2-07-01)"
    );
    for banned in ["std::fs::write(", "std::fs::copy("] {
        assert!(
            !body.contains(banned),
            "the publish body must not reach the cache-only `{banned}` — a \
                 rename may never expose a file whose content the kernel merely \
                 buffered (R2-07-01)"
        );
    }
}

/// FINDINGS2-0507 [6]: `snapshot()`, `any_down()` and `pending()` each took the
/// registry lock for themselves, so a supervisor flipping a verdict mid-document
/// could publish `"listeners": {"aux": "Serving"}` beside `"listeners_down": true` —
/// the red-over-green contradiction the badge work exists to prevent, re-opened
/// through read timing. The three fields are now three reads of ONE clone, so the
/// pair is checked against the map in the SAME document (never against the registry,
/// which would re-read the instant the pin is meant to audit) while a writer churns
/// the verdicts underneath. Falsifier: neuter `ListenerState::is_down` to
/// `matches!(self, Self::Serving)` and the agreement assertion fails on the map.
#[tokio::test]
async fn the_badge_booleans_are_always_the_published_maps_own_verdicts() {
    fn derived_from_the_published_map(doc: &Value) -> (bool, bool) {
        // The registry's serde shape, read exactly as the page reads it:
        // `"Serving"` / `"Pending"` as strings, `{"Down":{"reason":…}}` as the
        // externally-tagged struct variant.
        let listeners = doc["listeners"].as_object().expect("listeners map");
        (
            listeners.values().any(|state| state.get("Down").is_some()),
            listeners
                .values()
                .any(|state| state.as_str() == Some("Pending")),
        )
    }

    let api = live_state(BootTimings::default());
    let doc = status_document(&api).await.expect("status document");
    let (down, pending) = derived_from_the_published_map(&doc);
    assert!(
        pending && !down,
        "a freshly seeded registry is all-amber, never all-clear: {doc}"
    );
    assert_eq!(doc["listeners_down"], down, "unmixed boot: {doc}");

    api.listeners
        .mark_down(CategoryId::Aux, "port taken".to_string());
    api.listeners.mark_serving(CategoryId::Work);
    let doc = status_document(&api).await.expect("status document");
    let (down, pending) = derived_from_the_published_map(&doc);
    assert!(
        down && pending,
        "one Down and one still-unspoken category is red AND amber: {doc}"
    );
    assert_eq!(doc["listeners_down"], down, "mixed shape: {doc}");
    assert_eq!(doc["listeners_pending"], pending, "mixed shape: {doc}");

    // The timing leg: no document may ever contradict its own map, however the
    // verdicts move while it is being assembled.
    let churn_registry = api.listeners.clone();
    let churn = tokio::spawn(async move {
        for _ in 0..400 {
            churn_registry.mark_down(CategoryId::Aux, "flapping".to_string());
            tokio::task::yield_now().await;
            churn_registry.mark_serving(CategoryId::Aux);
            tokio::task::yield_now().await;
        }
    });
    for round in 0..120 {
        let doc = status_document(&api).await.expect("status document");
        let (down, pending) = derived_from_the_published_map(&doc);
        assert_eq!(
            doc["listeners_down"], down,
            "round {round} published a badge its own map denies: {doc}"
        );
        assert_eq!(
            doc["listeners_pending"], pending,
            "round {round} published a badge its own map denies: {doc}"
        );
    }
    churn.await.expect("the churner never panics");
}

/// FINDINGS2-0507 [10]: `Reply::Done => (0, None)` handed the proxy
/// `127.0.0.1:0`, a port that belongs to no listener, and the failure surfaced as a
/// connect error three layers from its cause. The arm now refuses, and this
/// inspection tripwire keeps it that way: the fabricated zero may not come back into
/// `swap_for_port`, and the arm must still name an error class. Falsifier: restore
/// `(0, None)` and the body assertion fails.
#[test]
fn a_swap_that_names_no_port_is_refused_rather_than_forwarded_to_zero() {
    let source = include_str!("mod.rs");
    let start = source
        .find("async fn swap_for_port(")
        .expect("swap_for_port is still in this file");
    let body = &source[start..source[start..].find("\n}\n").expect("the function closes") + start];
    assert!(
        !body.contains("(0, None)"),
        "`swap_for_port` may not answer a swap with a fabricated port 0: {body}"
    );
    assert!(
        body.contains("Reply::Done") && body.contains("ApiError::Internal"),
        "the no-port reply must still be refused as an invariant violation: {body}"
    );
}

/// "Reads never wake" as a table (operator ruling 2026-11-04, Variant A):
/// `wakes_slot` answers `true` for exactly the three inference POSTs and
/// `false` for every other method/path pair the listeners and a curious
/// client can produce — reads, admin routes, and non-POST variants of the
/// inference paths. Falsifier: widen `wakes_slot` to wake on a GET or on a
/// fourth path and the matching row fails.
#[test]
fn only_the_three_inference_posts_wake_the_slot() {
    let table: Vec<(&str, &str, bool)> = vec![
        ("POST", "/v1/chat/completions", true),
        ("POST", "/v1/completions", true),
        ("POST", "/v1/embeddings", true),
        ("GET", "/v1/chat/completions", false),
        ("GET", "/v1/completions", false),
        ("GET", "/v1/embeddings", false),
        ("HEAD", "/v1/chat/completions", false),
        ("HEAD", "/v1/completions", false),
        ("HEAD", "/v1/embeddings", false),
        ("OPTIONS", "/v1/chat/completions", false),
        ("OPTIONS", "/v1/completions", false),
        ("OPTIONS", "/v1/embeddings", false),
        ("PUT", "/v1/embeddings", false),
        ("DELETE", "/v1/completions", false),
        ("GET", "/v1/models", false),
        ("POST", "/v1/models", false),
        ("GET", "/health", false),
        ("POST", "/health", false),
        ("GET", "/har/status", false),
        ("GET", "/har/jobs", false),
        ("POST", "/har/load", false),
        ("POST", "/har/unload", false),
        ("POST", "/har/rescan", false),
        ("POST", "/har/config/model", false),
        // A fourth inference-style path that is not wired must never wake.
        ("POST", "/v1/rerank", false),
        // A trailing slash is a different path; the router 404s it, the
        // predicate must not wake on it either.
        ("POST", "/v1/chat/completions/", false),
        ("POST", "", false),
        ("GET", "", false),
    ];
    for (method, path, wakes) in table {
        let method = Method::from_bytes(method.as_bytes()).expect("valid method literal");
        assert_eq!(wakes_slot(&method, path), wakes, "{method} {path}");
    }
}

/// Route-table falsifier: the predicate's literal list and the category
/// listener route table must agree on the exact wake set, in both
/// directions — wiring any other route to `proxy` fails the per-route leg,
/// and adding any other waking path to `wakes_slot` fails the literal-list
/// and proxy-wiring legs. Falsifier: add a fourth path to `wakes_slot`
/// without touching the route table and the literal-list leg fails; re-wire
/// `/v1/models` to `proxy` and the per-route leg fails.
#[test]
fn the_wake_set_is_exactly_the_proxy_routes() {
    fn quoted_literals(text: &str) -> Vec<&str> {
        let mut out = Vec::new();
        let mut rest = text;
        while let Some(open) = rest.find('"') {
            rest = &rest[open + 1..];
            let Some(close) = rest.find('"') else { break };
            out.push(&rest[..close]);
            rest = &rest[close + 1..];
        }
        out
    }
    let source = include_str!("mod.rs");
    let start = source
        .find("fn wakes_slot(")
        .expect("wakes_slot is still in this file");
    let predicate =
        &source[start..start + source[start..].find("\n}\n").expect("wakes_slot closes")];
    let mut woken = quoted_literals(predicate);
    woken.sort_unstable();
    assert_eq!(
        woken,
        vec!["/v1/chat/completions", "/v1/completions", "/v1/embeddings"],
        "`wakes_slot` must list exactly the three inference paths and nothing else"
    );

    let router_at = source
        .find("pub fn category_router(")
        .expect("category_router is still in this file");
    let rest = &source[router_at..];
    let table = &rest[..rest
        .find(".layer((")
        .expect("the route entries end at the first layer")];
    // Absence leg (B4R-01): the enumeration below counts only `.route(`
    // entries, so a structural combinator inside the scanned region would
    // wake a slot without ever reaching the per-route checks or `total`.
    // A future `.nest(`/`.merge(`/`.fallback(`/`.route_method(` wire-up
    // must fail here, not escape the pin.
    assert!(
        !table.contains(".nest(")
            && !table.contains(".merge(")
            && !table.contains(".fallback(")
            && !table.contains(".route_method("),
        "route shape must stay enumerable by `.route(`"
    );
    let mut proxied: Vec<&str> = Vec::new();
    let mut total = 0;
    for entry in table.split(".route(").skip(1) {
        total += 1;
        let open = entry.find('"').expect("route path is quoted") + 1;
        let path = &entry[open..open + entry[open..].find('"').expect("path quote closes")];
        let (method, method_token) = match (entry.find("get("), entry.find("post(")) {
            (Some(g), Some(p)) if p < g => (Method::POST, "post("),
            (Some(_), Some(_)) => (Method::GET, "get("),
            (Some(_), None) => (Method::GET, "get("),
            (None, Some(_)) => (Method::POST, "post("),
            (None, None) => panic!("route {path} wires no known method combinator"),
        };
        let handler_at = entry.find(method_token).expect("method token") + method_token.len();
        let handler =
            &entry[handler_at..handler_at + entry[handler_at..].find(')').expect("handler closes")];
        assert_eq!(
            wakes_slot(&method, path),
            handler == "proxy",
            "route {method} {path} ({handler}) must agree with `wakes_slot`"
        );
        if handler == "proxy" {
            proxied.push(path);
        }
    }
    assert_eq!(
        total, 5,
        "the category-listener route table changed shape; re-read the wake pin: {table}"
    );
    proxied.sort_unstable();
    assert_eq!(
        proxied,
        vec!["/v1/chat/completions", "/v1/completions", "/v1/embeddings"],
        "`proxy` must be wired to exactly the three waking routes"
    );
}

fn occupied_slot(model: &str, port: u16, loaded_at: Instant) -> SlotStatus {
    SlotStatus {
        category: CategoryId::Aux,
        index: 0,
        model: Some(model.to_string()),
        port: Some(port),
        pid: Some(1),
        need_gib: Some(4.0),
        idle_s: Some(0),
        serve_id: Some("mock-model".to_string()),
        loading_job: None,
        state: "serving".to_string(),
        launched_at: None,
        stale_config: None,
        container_reported_gb: None,
        loaded_at,
    }
}

/// The R9 memory-bar sum (server side, where the honesty rule lives): it sums
/// the per-slot `engine_report` figures the scheduler anchored — one figure per
/// model even when the same container occupies two slots — and answers `None`
/// (never `0.0`) when nobody reported, so a silent container cannot render as
/// "reserved 0 GiB" next to a busy card. The engine/attribution gates themselves
/// are pinned at their source (`only_an_engine_report_anchor_reaches_the_snapshot_slot`,
/// scheduler); this pins the fold-over-slots above them.
#[test]
fn the_container_reported_sum_dedupes_models_and_never_fakes_zero() {
    let now = Instant::now();
    let reported = |model: &str, gib: Option<f64>| SlotStatus {
        container_reported_gb: gib,
        ..occupied_slot(model, 9_100, now)
    };
    let empty = SlotStatus {
        model: None,
        state: "empty".to_string(),
        ..occupied_slot("empty", 9_101, now)
    };
    let document = |slots: Vec<SlotStatus>| {
        container_reported_gb(&SchedulerStatus {
            slots,
            jobs: Vec::new(),
            catalog_errors: Vec::new(),
            stranded_containers: None,
            fit_validated: Default::default(),
        })
    };
    assert_eq!(document(vec![empty, reported("quiet", None)]), None);
    assert_eq!(
        document(vec![reported("a", Some(20.0)), reported("b", Some(11.25))]),
        Some(31.25)
    );
    assert_eq!(
        document(vec![
            reported("twin", Some(30.0)),
            reported("twin", Some(30.0))
        ]),
        Some(30.0),
        "one container on two slots is one reservation"
    );
}

#[test]
fn no_fabricated_instants_survive_short_uptime() {
    // FINDINGS-06 [1]: the removed fallback sorted untouched occupants by a
    // fabricated `Instant::now() - 3600s`, which panics outright on a host
    // younger than an hour. Fixture instants are FORWARD offsets of `now`
    // (`Instant + Duration` cannot underflow) so the test never panics on a
    // young host — the same panic the finding condemned. The pin is the
    // REAL picker's winner: pre-fix both slots collapse to the fabricated
    // hour and the last-evaluated slot (`older`, snapshot order reversed
    // against load order) wins the jitter tie; post-fix the actual
    // `loaded_at` ordering makes `newer` win, on any uptime.
    let api = unit_state();
    let now = Instant::now();
    // 20-minute separation is far above QueryPerformanceCounter jitter.
    let t_older = now + Duration::from_secs(600);
    let t_newer = now + Duration::from_secs(1_200);
    let status = SchedulerStatus {
        slots: vec![
            occupied_slot("newer", 9002, t_newer),
            occupied_slot("older", 9001, t_older),
        ],
        jobs: Vec::new(),
        catalog_errors: Vec::new(),
        stranded_containers: None,
        fit_validated: Default::default(),
    };
    let picked = pick_loaded_occupant(&api, &status, CategoryId::Aux)
        .expect("an occupied category must resolve");
    assert_eq!(
        picked.0, "newer",
        "untouched occupants sort by ACTUAL load time"
    );
    assert_eq!(picked.1, 9002);
    assert_eq!(picked.2.as_deref(), Some("mock-model"));
}

#[test]
fn mru_touch_wins_over_a_fresher_load() {
    let api = unit_state();
    // Load stamps sit NEAR now (not in the future: an in-process future stamp
    // would outrank a touch landing at ~now, inverting reality — real loads
    // always precede their touches). `hot` loaded last, yet the touch on
    // `cold` must outrank it. Backward offsets stay under a minute so the
    // fixtures are safe on any host uptime; the touch lands after them.
    let status = SchedulerStatus {
        slots: vec![
            occupied_slot("cold", 9001, Instant::now()),
            occupied_slot(
                "hot",
                9002,
                Instant::now()
                    .checked_sub(Duration::from_millis(150))
                    .expect("invariant: process start precedes first fixture stamp"),
            ),
        ],
        jobs: Vec::new(),
        catalog_errors: Vec::new(),
        stranded_containers: None,
        fit_validated: Default::default(),
    };
    api.touch("cold");
    let picked = pick_loaded_occupant(&api, &status, CategoryId::Aux)
        .expect("an occupied category must resolve");
    assert_eq!(picked.0, "cold", "a proxied model outranks a fresher load");
}

#[test]
fn other_category_and_dead_ports_resolve_to_nothing() {
    let api = unit_state();
    let now = Instant::now();
    let foreign = SlotStatus {
        category: CategoryId::Work,
        ..occupied_slot("elsewhere", 9003, now)
    };
    let portless = SlotStatus {
        port: None,
        ..occupied_slot("unbound", 9004, now)
    };
    let zero = SlotStatus {
        port: Some(0),
        ..occupied_slot("zombie", 0, now)
    };
    let empty = SlotStatus {
        category: CategoryId::Aux,
        index: 1,
        model: None,
        port: None,
        pid: None,
        need_gib: None,
        idle_s: None,
        serve_id: None,
        loading_job: None,
        state: "empty".to_string(),
        launched_at: None,
        stale_config: None,
        container_reported_gb: None,
        loaded_at: now,
    };
    let status = SchedulerStatus {
        slots: vec![foreign, portless, zero, empty],
        jobs: Vec::new(),
        catalog_errors: Vec::new(),
        stranded_containers: None,
        fit_validated: Default::default(),
    };
    assert!(
        pick_loaded_occupant(&api, &status, CategoryId::Aux).is_none(),
        "wrong-category, portless, and port-0 slots must not resolve"
    );
}

#[tokio::test]
async fn connect_failure_is_the_only_restart_class() {
    // FINDINGS-06 [2] evidence for the connect class: the T10.1 mock death is
    // a `handle.abort()` that drops the listener socket, so the port REFUSES
    // and `send()` yields `is_connect()` — exactly what this classifier maps
    // to the single sanctioned reap + reload + retry.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind free port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    let error = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect_err("nothing accepts on the released port");
    assert!(
        error.is_connect(),
        "dead port must be connect-class: {error}"
    );
    let action = classify_send_failure(&error, port).expect("connect-class retries once");
    assert_eq!(action, RetryAction::Restart);
}

/// FINDINGS-06 [6]: a total `ClientBuilder::timeout` covers the response body, so
/// it cut a long SSE generation mid-stream after the 200. The production client
/// must carry a per-read deadline, not a total one. Falsifier: put `.timeout(...)`
/// back into the builder and this tripwire fails.
#[test]
fn the_proxy_client_has_no_total_deadline() {
    let source = include_str!("mod.rs");
    let start = source
        .find("proxy: reqwest::Client::builder()")
        .expect("the proxy client builder is still in this file");
    let end = source[start..]
        .find(".build()")
        .expect("the builder still finishes with build()")
        + start;
    let builder = &source[start..end];
    assert!(
        builder.contains(".read_timeout("),
        "the proxy needs a per-read deadline: {builder}"
    );
    assert!(
        !builder.contains(".timeout("),
        "a total timeout covers the SSE body and truncates live generations: {builder}"
    );
}

#[tokio::test]
async fn timeout_of_a_live_engine_never_restarts() {
    // A server that accepts but never answers produces a real reqwest
    // timeout — the class that used to mean "engine died" and reaped a
    // healthy engine mid-generation. It must now be a typed 502 instead.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind free port");
    let port = listener.local_addr().expect("local addr").port();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .build()
        .expect("plain-HTTP client with a timeout");
    let error = client
        .post(format!("http://127.0.0.1:{port}/v1/chat/completions"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect_err("the socket is accepted but never answered");
    assert!(
        error.is_timeout(),
        "hanging server must be timeout-class: {error}"
    );
    let err = classify_send_failure(&error, port).expect_err("timeouts never restart");
    assert_eq!(err.status(), StatusCode::BAD_GATEWAY);
    let reason = match &err {
        ApiError::Upstream { reason } => reason.clone(),
        other => panic!("expected Upstream, got {other:?}"),
    };
    assert!(
        reason.contains(&format!("engine on :{port} timed out"))
            && reason.contains("not restarting a live engine"),
        "reason must name the class and the refusal to reap: {reason}"
    );
    drop(listener);
}

#[test]
fn timeout_precedes_connect_when_both_flags_are_true() {
    // R2-18-06 falsifier: the both-true row is RED under the old
    // connect-first order. The table pins the decision on the flags alone.
    assert_eq!(
        classify_send_failure_flags(false, false),
        SendFailureClass::Other,
        "neither flag must stay a plain forward failure"
    );
    assert_eq!(
        classify_send_failure_flags(true, false),
        SendFailureClass::Timeout,
        "timeout-only must be the 502 class"
    );
    assert_eq!(
        classify_send_failure_flags(false, true),
        SendFailureClass::Connect,
        "connect-only (refused/reset) must stay the restart class"
    );
    assert_eq!(
        classify_send_failure_flags(true, true),
        SendFailureClass::Timeout,
        "both flags must be the timeout class, never a reap"
    );
}

#[tokio::test]
async fn non_connect_errors_are_upstream_never_death() {
    // A redirect loop is the finding's named non-death trigger ("a redirect
    // loop or a request-body error"): the engine was alive and answering, so
    // the class must never be read as death and must never restart. Raw
    // sockets speak just enough HTTP/1.1 to bounce the client forever.
    let looping = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("redirect loop server binds");
    let port = looping.local_addr().expect("loop addr").port();
    let server = tokio::spawn(async move {
        let reply = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/loop\r\n\
                 Content-Length: 0\r\nConnection: keep-alive\r\n\r\n"
        );
        while let Ok((mut stream, _)) = looping.accept().await {
            let reply = reply.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 {
                        break;
                    }
                    if stream.write_all(reply.as_bytes()).await.is_err() {
                        break;
                    }
                }
            });
        }
    });
    let error = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::limited(4))
        .build()
        .expect("plain-HTTP client")
        .post(format!("http://127.0.0.1:{port}/loop"))
        .json(&json!({"messages": []}))
        .send()
        .await
        .expect_err("the redirect loop must exhaust the policy");
    assert!(
        !error.is_connect(),
        "a live answering server is not death: {error}"
    );
    let err = classify_send_failure(&error, port).expect_err("non-connect never restarts");
    match err {
        ApiError::Upstream { reason } => assert!(
            reason.starts_with("forward failed:"),
            "reason must name the class: {reason}"
        ),
        other => panic!("expected Upstream, got {other:?}"),
    }
    server.abort();
}

#[test]
fn concurrent_default_writes_never_lose_a_field_or_leave_a_temp() {
    // FINDINGS-07 [3]: the config write-back was an UNGATED read-modify-write onto
    // one FIXED temp name, and two handlers reach it (`/har/config/default` and
    // `/har/config/model`'s dangling-default clear) on separate workers. Interleaved,
    // the second writer renamed its copy of the ORIGINAL, so the first writer's
    // `default_model` vanished from disk while both responses said `persisted: true`;
    // the shared temp added a second failure mode — one writer truncating the other's
    // tmp made its rename fail although the other's content had landed. Six threads
    // over the four categories, a fresh value per round: under the gate plus the
    // unique temp every write answers true and no category can fall back to its seed.
    const ROUNDS: usize = 24;
    let dir = crate::testutil::temp_dir("api-persist-race");
    let path = dir.join("simpleharn.json");
    let mut config = AppConfig::default();
    for category in &mut config.categories {
        category.default_model = Some(format!("seed-{}", category.id));
    }
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&config).expect("seed serializes"),
    )
    .expect("seed config");
    config.source_path = Some(path.clone());

    let plan: [(CategoryId, &str); 6] = [
        (CategoryId::Work, "work"),
        (CategoryId::Work, "work"),
        (CategoryId::Rp, "rp"),
        (CategoryId::Rp, "rp"),
        (CategoryId::Aux, "aux"),
        (CategoryId::Emb, "emb"),
    ];
    let writers = plan
        .into_iter()
        .enumerate()
        .map(|(index, (category, prefix))| {
            let per_writer = config.clone();
            std::thread::spawn(move || {
                (0..ROUNDS)
                    .map(|round| {
                        let name = format!("{prefix}-t{index}-r{round}");
                        persist_default_model(&per_writer, category, Some(&name)).is_landed()
                    })
                    .collect::<Vec<bool>>()
            })
        })
        .collect::<Vec<_>>();
    let outcomes = writers
        .into_iter()
        .map(|writer| writer.join().expect("a writer thread never panics"))
        .collect::<Vec<_>>();
    assert!(
        outcomes.iter().flatten().all(|persisted| *persisted),
        "every writer must be able to answer honestly, not `persisted: true` \
             for bytes that a concurrent rename undid: {outcomes:?}"
    );

    let text = std::fs::read_to_string(&path).expect("the config file is always present");
    let on_disk: Value =
        serde_json::from_str(&text).expect("temp + rename never expose a partial file");
    let categories = on_disk["categories"].as_array().expect("categories array");
    let persisted_default = |category: CategoryId| -> String {
        let id = category.to_string();
        categories
            .iter()
            .find(|entry| entry["id"].as_str() == Some(id.as_str()))
            .and_then(|entry| entry["default_model"].as_str())
            .unwrap_or_else(|| panic!("category {id} lost its default_model: {on_disk}"))
            .to_string()
    };
    for &(category, prefix) in &plan {
        let value = persisted_default(category);
        assert!(
            value.starts_with(prefix),
            "{}'s default is a lost update — no writer ever named {value:?}: {on_disk}",
            category
        );
    }
    // A single-writer category is deterministic under the gate: every later commit
    // re-reads the committed file, so its last round can never be rolled back.
    assert_eq!(
        persisted_default(CategoryId::Aux),
        format!("aux-t4-r{}", ROUNDS - 1),
        "the last AUX write must still be on disk"
    );
    assert_eq!(
        persisted_default(CategoryId::Emb),
        format!("emb-t5-r{}", ROUNDS - 1),
        "the last EMB write must still be on disk"
    );

    let leftovers = std::fs::read_dir(&dir)
        .expect("read temp dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect::<Vec<_>>();
    assert!(
        leftovers.is_empty(),
        "a failed rename must not leave its temp behind: {leftovers:?}"
    );
}

/// FINDINGS-07 [11] producer falsifier: the write-back is the one moment the
/// harness becomes the last writer of the boot config, and its read-patch-write
/// carries every OTHER byte as `Value` passthrough — so an operator hand-edit made
/// while the harness ran (reload re-reads the catalog and nothing else, Step 9
/// LOCKED) survives into the published file. Pre-fix, `persist_default_model`
/// answered a bare bool and `/har/status` hardcoded `config_errors: []`, so a
/// hand-edited `load_timeout_s: 0` (a boot-fatal field per §0.2 step 1) rode the
/// harness's own `persisted: true` write unseen until the harness refused to
/// start. The verdict is the boot's own validator over the landed tree, so the
/// field can never drift from what boot would say.
#[test]
fn a_landed_write_back_validates_the_published_boot_file_per_field() {
    let dir = crate::testutil::temp_dir("api-config-verdict");
    let path = dir.join("simpleharn.json");
    let config = AppConfig {
        source_path: Some(path.clone()),
        ..AppConfig::default()
    };
    // Mid-run hand-edit: valid JSON, contents the boot must refuse.
    let mut root = serde_json::to_value(&config).expect("seed serializes");
    root["load_timeout_s"] = json!(0);
    root["vram_total_gb"] = json!(-1.0);
    std::fs::write(&path, root.to_string()).expect("seed the hand-edited file");

    let verdict = match persist_default_model(&config, CategoryId::Work, Some("pinned.gguf")) {
        PersistOutcome::Landed { verdict } => verdict,
        other => panic!(
            "the bytes landed and read back; persisted honesty is unchanged by \
                 the verdict, never {other:?}"
        ),
    };
    let named = verdict
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" | ");
    assert!(
        named.contains("load_timeout_s") && named.contains("vram_total_gb"),
        "every boot-refused field must be named while the harness is alive to say it: {named}"
    );

    // A hand-edited unknown key no longer parses as the schema at all: one error
    // naming the file root, the same shape `AppConfig::parse` gives at boot.
    let mut root = serde_json::to_value(&config).expect("seed serializes");
    root["wrok_port"] = json!(9);
    std::fs::write(&path, root.to_string()).expect("seed the unknown key");
    let verdict = match persist_default_model(&config, CategoryId::Work, Some("pinned2.gguf")) {
        PersistOutcome::Landed { verdict } => verdict,
        other => panic!(
            "the unknown key rode the passthrough and landed as Landed, \
                 never {other:?}"
        ),
    };
    assert_eq!(verdict.len(), 1, "one parse-level error: {verdict:?}");
    let text = verdict[0].to_string();
    assert!(
        text.contains(crate::config::CONFIG_FILE) && text.contains("wrok_port"),
        "the error must name the file and the key, as boot's parse does: {text}"
    );
}

/// FINDINGS-07 [11] render wiring: `config_errors` is the recorded verdict, in the
/// `kind`/`field`/`reason` vocabulary `catalog_errors` established, and an empty
/// array is earned (clean boot, clean later write) rather than asserted. Falsifier:
/// restoring the hardcoded `[]` leaves both non-empty legs red; flattening the
/// verdict into `last_error` (or into `catalog_errors`) is red on the shape leg.
#[tokio::test]
async fn config_errors_reports_the_recorded_verdict_and_starts_empty() {
    let api = live_state(BootTimings::default());
    let doc = status_document(&api).await.expect("status document");
    assert_eq!(
        doc["config_errors"],
        json!([]),
        "a boot that validated dirty never served this document: a fresh state is clean"
    );
    api.record_config_errors(vec![ConfigError::Invalid {
        field: "vram_total_gb".to_string(),
        reason: "must be a finite number greater than 0".to_string(),
    }]);
    let doc = status_document(&api).await.expect("status document");
    assert_eq!(
        doc["config_errors"],
        json!([{
            "kind": "invalid",
            "field": "vram_total_gb",
            "reason": "must be a finite number greater than 0"
        }]),
        "the wire shape is kind+field+reason, like catalog_errors"
    );
    api.record_config_errors(Vec::new());
    let doc = status_document(&api).await.expect("status document");
    assert_eq!(
        doc["config_errors"],
        json!([]),
        "a later landing write that validates clean replaces the verdict"
    );
}

/// FINDINGS-07 [11] end-to-end tripwire: the page-visible field through the real
/// `/har/config/default` path (`set_default_model` → gated blocking write-back →
/// recorded verdict → status document). The named test is the pin the `ApiState`
/// field's doc cites: a future boot-file mutation that forgets
/// `record_config_errors`, or a render path that regresses to `[]`, fails here.
/// `persisted` stays `true` — the bytes DID land; the field reports that the file
/// on disk will refuse the next boot, which is a different fact.
#[tokio::test]
async fn a_write_back_that_lands_a_boot_invalid_file_names_it_in_config_errors() {
    let dir = crate::testutil::temp_dir("api-config-e2e");
    let path = dir.join("simpleharn.json");
    let mut config = AppConfig::default();
    let mut root = serde_json::to_value(&config).expect("seed serializes");
    root["load_timeout_s"] = json!(0);
    std::fs::write(&path, root.to_string()).expect("seed the mid-run hand-edit");
    config.source_path = Some(path.clone());

    let mut adapters: BTreeMap<String, Arc<dyn crate::engine::EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(crate::engine::mock::MockAdapter) as Arc<dyn crate::engine::EngineAdapter>,
    );
    let scheduler = crate::scheduler::Scheduler::start(
        config.clone(),
        crate::testutil::temp_dir("api-config-e2e-boot"),
        adapters,
        Arc::new(crate::process::ProcessRuntime::new()) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(crate::budget::LedgerOnlyWitness),
    );
    let api = ApiState::new(
        config,
        scheduler,
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    );

    let doc = status_document(&api).await.expect("status document");
    assert_eq!(
        doc["config_errors"],
        json!([]),
        "nothing is recorded before any harness write-back"
    );
    let (effective, persisted) = api
        .set_default_model(CategoryId::Work, Some("pinned.gguf"))
        .await;
    assert_eq!(effective.as_deref(), Some("pinned.gguf"));
    assert!(persisted, "the patched file landed and read back");

    let doc = status_document(&api).await.expect("status document");
    assert!(
        doc["config_errors"]
            .as_array()
            .expect("config_errors stays an array")
            .iter()
            .any(|entry| entry["field"] == json!("load_timeout_s")),
        "the field the next boot would refuse is named now: {doc}"
    );
    assert_eq!(
        doc["categories"]
            .as_array()
            .expect("categories array")
            .iter()
            .find(|c| c["id"] == json!("WORK"))
            .and_then(|c| c["default_model"].as_str()),
        Some("pinned.gguf"),
        "the write-back itself is unchanged: only the verdict was added"
    );
}

/// R2-07-02 end-to-end: the test arm lands a rename whose read-back refuses the
/// tree, so the live boot file holds bytes nobody validated. `/har/status`
/// `config_errors` must then describe THOSE bytes — the same authority a
/// successful write-back's verdict has — because the previous (empty) verdict
/// would claim the next boot reads a file the harness itself just replaced.
/// Falsifier: the pre-fix `if persisted { record }` shape answers
/// `persisted: false` here while `config_errors` stays `[]` — this leg goes red
/// and only this leg; every other pin stays green on the stale-verdict shape.
#[tokio::test]
async fn a_landed_but_unverified_write_restates_the_recorded_config_verdict() {
    let dir = crate::testutil::temp_dir("api-config-unverified-e2e");
    let path = dir.join("simpleharn.json");
    let config = AppConfig {
        source_path: Some(path.clone()),
        ..AppConfig::default()
    };
    let mut adapters: BTreeMap<String, Arc<dyn crate::engine::EngineAdapter>> = BTreeMap::new();
    adapters.insert(
        "mock".to_string(),
        Arc::new(crate::engine::mock::MockAdapter) as Arc<dyn crate::engine::EngineAdapter>,
    );
    let scheduler = crate::scheduler::Scheduler::start(
        config.clone(),
        crate::testutil::temp_dir("api-config-unverified-boot"),
        adapters,
        Arc::new(crate::process::ProcessRuntime::new()) as Arc<dyn crate::engine::EngineRuntime>,
        Arc::new(crate::budget::LedgerOnlyWitness),
    );
    let api = ApiState::new(
        config,
        scheduler,
        crate::listeners::ListenerRegistry::new(),
        Arc::new(crate::budget::LedgerOnlyWitness),
        BootTimings::default(),
    );
    let doc = status_document(&api).await.expect("status document");
    assert_eq!(
        doc["config_errors"],
        json!([]),
        "nothing is recorded before any harness write-back"
    );

    arm_config_read_back_fault(&path);
    let (effective, persisted) = api
        .set_default_model(CategoryId::Work, Some("phantom.gguf"))
        .await;
    release_config_read_back_fault(&path);

    assert_eq!(
        effective.as_deref(),
        Some("phantom.gguf"),
        "the in-memory override still takes the edit"
    );
    assert!(
        !persisted,
        "an unverifiable write may only ever be answered `persisted: false`"
    );
    let doc = status_document(&api).await.expect("status document");
    let errors = doc["config_errors"]
        .as_array()
        .expect("config_errors stays an array");
    assert!(
        errors.iter().any(|entry| {
            entry["field"] == json!(crate::config::CONFIG_FILE)
                && entry["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.contains("tampered"))
        }),
        "the field must describe the bytes the harness just landed (file root + \
             the unknown key they carry), not keep the previous clean verdict: {doc}"
    );
}
