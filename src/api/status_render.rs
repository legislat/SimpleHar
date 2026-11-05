//! Status and model rendering for the HTTP surface: the `GET /har/status`
//! handler, its fetch wrapper and the one snapshot renderer behind it; the
//! harness-facing model-row builder shared by `/har/models` and the SSE
//! document; the minimal client-facing `/v1/models` renderer (DOCTRINE 8); and
//! their helper formatters. The state pump and the SSE stream wiring stay in
//! `mod.rs` beside the `ApiState` watch channel they drive.

use crate::model::ModelEntry;
use crate::scheduler::SchedulerStatus;
use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Arc;

use super::{ApiError, ApiState, ListenerState};

/// `GET /har/status`: boot info + slot table + jobs, one call (G13).
pub(super) async fn har_status(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(status_document(&state).await?))
}

/// The full `/har/status` document: config categories, mem, boot, listener
/// health (raw verdicts plus the two derived booleans), slots, jobs, catalog
/// errors, and the boot config file's per-field write-back verdict — one call (G13).
/// Also the body of every SSE `har_state` event
/// (Step 14: one push replaces the page's state).
pub(super) async fn status_document(state: &ApiState) -> Result<Value, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    Ok(render_status_document(state, &scheduler).await)
}

/// The render half of `/har/status`: everything comes from ONE scheduler snapshot
/// already in hand plus the cached witness/config reads — no model-file stat, no
/// second actor round-trip. Keeping the fs-free shape explicit is what makes
/// `/har/status`, the SSE build, and `/har/models` stat-free reads by construction
/// (Axis-2 ruling 2026-10-29's one-stat-per-event rule: `stale_config`'s stats live
/// on install, the reload turn, and harness write-backs — pinned by
/// `the_status_and_models_render_paths_never_touch_the_filesystem` (structural, this
/// file), `the_snapshot_reads_the_cached_verdict_and_never_stats` (scheduler), and
/// the api_surface hidden-file leg of
/// `a_flag_edit_never_ambers_its_own_loaded_model_and_status_never_stats`).
pub(super) async fn render_status_document(state: &ApiState, scheduler: &SchedulerStatus) -> Value {
    let categories: Vec<Value> = state
        .config
        .categories
        .iter()
        .map(|cat| {
            let mut value = serde_json::to_value(cat).expect("invariant: category serializes");
            value["default_model"] = match state.default_model(cat.id) {
                Some(model) => Value::String(model),
                None => Value::Null,
            };
            // Additive display field (ruling 2026-11-05): the effective label is
            // ALWAYS present (the trimmed `label`, else the id), so the page reads
            // one field and never re-derives the fallback. The id next to it
            // stays the wire vocabulary; this value never replaces it anywhere.
            value["label"] = json!(cat.effective_label());
            value
        })
        .collect();
    let boot = state
        .boot
        .read()
        .expect("boot timings lock poisoned by a writer panic")
        .clone();
    // A3: the sticky headline fault, derived once by the scheduler crate
    // (`headline_fault`: newest failed LOAD, cancels excluded, cleared only by a
    // later successful load of the same model+category) — page and tray share it.
    let last_error = crate::scheduler::headline_fault(&scheduler.jobs).and_then(|job| {
        job.error
            .as_ref()
            .map(|reason| format!("{}@{}: {reason}", job.model, job.category))
    });
    // One registry acquisition for the whole badge story (FINDINGS2-0507 [6]):
    // `snapshot()`, `any_down()` and `pending()` each took the lock themselves, so a
    // supervisor flipping `Pending → Down` mid-document could publish `listeners`
    // saying `Serving` next to `listeners_down: true` — the red-over-green
    // contradiction 9c73a39 exists to prevent, re-opened through read timing. The
    // three fields are now three reads of ONE clone, through the registry's own
    // predicates (`is_down`/`is_pending`), never a re-derived match.
    let verdicts = state.listeners.snapshot();
    let mut document = json!({
        "version": env!("CARGO_PKG_VERSION"),
        "control_addr": state.control_addr,
        "extended_logging": crate::observability::extended_logging(),
        // Non-zero means the on-disk post-mortem artifact is incomplete. The sink
        // cannot report itself — the failed write is what would have carried the
        // report — so this counter is the only durable signal that it died.
        "log_write_failures": crate::observability::log_write_failures(),
        "boot": boot,
        "categories": categories,
        "listeners": verdicts,
        // The header badge's two truths, computed here from the registry's own
        // predicates (FINDINGS-16 [4]). `listeners` above is the raw verdict map; a
        // page that decoded "is any listener down" out of its serde shape would be a
        // second brain for the rule the registry owns, and a `Down`/`Pending` that
        // ever changed representation would hide behind a green badge. Total over the
        // four seeded categories: `down` is some supervisor that could not serve a
        // port, `pending` is a category nobody has spoken for (the tray's amber).
        "listeners_down": verdicts.values().any(crate::listeners::ListenerState::is_down),
        "listeners_pending": verdicts.values().any(crate::listeners::ListenerState::is_pending),
        "mem": mem_section(state, scheduler).await,
        "slots": &scheduler.slots,
        "jobs": &scheduler.jobs,
        "last_error": last_error,
        "catalog_errors": &scheduler.catalog_errors,
        // The boot config file's per-field verdict as of the last landing harness
        // write-back (07[11]): cached state only — the boot itself exits on any
        // invalid field (§0.2 step 1), so a served document starts empty and the
        // write-back validator is the only producer.
        "config_errors": state.config_errors(),
    });
    // R5 report-only reconcile (ruling 9): the stranded-container names the
    // reconcile warned about ride the SAME snapshot, so the page and the log can
    // never disagree. Absent — not empty, not null — whenever nothing is
    // reported: the snapshot already normalizes an empty set to `None`.
    if let Some(names) = scheduler
        .stranded_containers
        .as_ref()
        .filter(|names| !names.is_empty())
    {
        document["stranded_containers"] = json!(names);
    }
    document
}

/// The loaded-side facts for one model row, joined server-side from ONE scheduler
/// snapshot (Axis 2, ruling 2026-10-29): the page reads these fields and derives
/// nothing (`stale_config === true` and `launched_at` verbatim are pinned in
/// `webui`). Same-name occupants (one model serving in two categories) merge
/// honestly: `launched_at` is the EARLIEST install among them (fixed-width UTC ISO
/// sorts lexicographically = chronologically), `stale_config` is ANY-of — one
/// resident on stale bytes makes the row amber.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LoadedRow {
    pub(super) launched_at: String,
    pub(super) stale_config: bool,
}

pub(super) fn loaded_rows(slots: &[crate::scheduler::SlotStatus]) -> BTreeMap<String, LoadedRow> {
    let mut rows: BTreeMap<String, LoadedRow> = BTreeMap::new();
    for slot in slots {
        let (Some(model), Some(launched_at)) = (slot.model.as_ref(), slot.launched_at.as_ref())
        else {
            continue;
        };
        let stale = slot.stale_config.unwrap_or(false);
        rows.entry(model.clone())
            .and_modify(|row| {
                if launched_at < &row.launched_at {
                    row.launched_at = launched_at.clone();
                }
                row.stale_config |= stale;
            })
            .or_insert(LoadedRow {
                launched_at: launched_at.clone(),
                stale_config: stale,
            });
    }
    rows
}

/// The one harness-facing model-listing shape: OpenAI fields plus the extras the
/// page's table renders (A5: `context`, `serve_model`, `model_ref`) and the loaded
/// row's server-computed facts (`launched_at`, `stale_config` — absent when the
/// model is not loaded, never null: not-loaded is a state, not an empty value).
/// `fit_validated` is the third joined fact: the actor's session cross-check
/// verdict, present as `true` only while it stands (absent is not false — an
/// unvalidated row keeps its exact unvalidated shape).
/// Single builder for `merged_state` and `/har/models` so they cannot drift. The
/// client-facing listener `/v1/models` is deliberately minimal — local file
/// paths stay off the LAN-facing surface.
pub(super) fn model_json(
    entry: &ModelEntry,
    loaded: Option<&LoadedRow>,
    fit_validated: bool,
) -> Value {
    // A CPU-resident row books host RAM and no card bytes, so its cost basis
    // is the RAM ladder's rung (operator ruling 2026-11-05: RAM is its own
    // resource ladder — a card rung would describe a booking that never
    // happens). GPU rows keep their exact pre-RAM shape: no RAM keys, the
    // VRAM ladder's answer.
    let ram = (entry.resource() == crate::model::Resource::Ram)
        .then(|| entry.ram_booking())
        .flatten();
    let mut body = json!({
        "id": entry.name,
        "object": "model",
        "owned_by": "simpleharn",
        "engine": entry.engine,
        "category": entry.category,
        "vram_gb": entry.vram_gb,
        // Shown only when `cost_source` says admission really books the GGUF estimate
        // (x1.15 + 2) — the page can no longer display a cost the gate ignores
        // (duplication review, 2026-10-26).
        "est_vram_gb": (entry.cost_source() == crate::model::CostSource::Estimate).then(|| {
            crate::budget::need_gb(
                None,
                entry.gguf_size_gb.map(crate::budget::Gibibytes),
            )
            .ok()
            .map(|n| n.0)
        }).flatten(),
        "measured_gb": entry.measured_gb,
        "measured_min_gb": entry.measured_min_gb,
        "measured_offload": entry.measured_offload,
        // Which figure admission actually books, stated by the gate's own decision
        // (one derivation; the page renders this and never re-computes the
        // precedence — the duplication-review rule made visible for R9). On a
        // CPU-resident row the figure the gate books is RAM, so the answer
        // names the RAM rung (`declared` | `measured` | `estimate` — the same
        // vocabulary, the resource's own ladder).
        "cost_basis": match ram {
            Some((_, basis)) => basis.as_str(),
            None => match entry.cost_source() {
                crate::model::CostSource::Declared => "declared",
                crate::model::CostSource::Measured => "measured",
                crate::model::CostSource::Fit => "fit",
                crate::model::CostSource::Estimate => "estimate",
                crate::model::CostSource::Undeclared => "undeclared",
            },
        },
        // R9/R1: whose number the measured pair is. The page shows it and admits
        // nothing from it — only a `cost_basis: "measured"` row ever renders a
        // provenance tag, which a `total_diff` can never become (it never books),
        // so the token alone is display context, never a booking claim.
        "measured_attribution": entry
            .measured_attribution
            .map(crate::model::MeasuredAttribution::as_str),
        // The displayed number == what admission must have free, shown only when the
        // provenance actually books (the K-round page-must-not-derive rule: the gate's
        // answer is computed here, never reconstructed in the browser). The page's
        // Cost cell renders `measured_gb` under the same condition, so this field is
        // the cross-check the operator's forensic log reads against the gate.
        "required_to_free_gb": (entry.cost_source() == crate::model::CostSource::Measured)
            .then_some(entry.measured_gb)
            .flatten(),
        "cost": entry.cost,
        "launch_floor_gb": entry.vram_gate.floor(),
        "load_timeout_s": entry.load_timeout_s,
        "cpu_offload": entry
            .raw
            .get("cpu_offload")
            .is_some_and(|v| crate::engine::llama::jinja_on(v)),
        // The EFFECTIVE served context (operator ruling 2026-11-02): llama's own
        // row knob, strata's `--max-context` argv token, ninfer's resolved
        // profile max-context — never the inert row key; null when the engine
        // speaks none (the one home is `engine::effective_context`).
        "context": crate::engine::effective_context(entry),
        "serve_model": entry.serve_model,
        "model_ref": entry.model_ref,
    });
    // W3: the RAM cost surface rides CPU-resident rows ONLY — a GPU row's
    // document stays byte-identical to its pre-RAM shape. The displayed
    // figure follows the basis exactly as the VRAM rungs do: the declared
    // floor, the GGUF estimate only when the estimate is what books, and the
    // measured pair whenever the ladder may need it; `ram_source` says whose
    // number the booked figure is (`per_process` tree sum or `declared`
    // arithmetic) so the page never infers provenance from which key happens
    // to carry a value.
    if let Some((booked, basis)) = ram {
        body["ram_gb"] = json!(entry.ram_gb);
        body["measured_ram_gb"] = json!(entry.measured_ram_gb);
        body["measured_ram_min_gb"] = json!(entry.measured_ram_min_gb);
        if let Some(estimate) = (basis == crate::model::RamCostBasis::Estimate).then_some(booked) {
            body["est_ram_gb"] = json!(estimate);
        }
        body["ram_source"] = json!(basis.source());
    }
    // R11: the Fit projection rides the payload only when it exists — a key
    // that is absent is not a key that is null (not-probed is a state), and a
    // fixture row that has never been probed keeps its byte-identical shape.
    if let Some(fit) = entry.fit_gb {
        body["fit_gb"] = json!(fit);
    }
    // R11 session cross-check: `fit_validated` appears ONLY while the verdict
    // stands — absent, never null, never `false` (an unvalidated row's document
    // is byte-identical to a never-checkable one, the same absent-is-not-null
    // rule as `fit_gb`; the fixture-parity pin rides it). The gate answers the
    // flag: a row the gate no longer books from Fit cannot carry a verdict about
    // a booking that is not happening.
    if fit_validated && entry.cost_source() == crate::model::CostSource::Fit {
        body["fit_validated"] = json!(true);
    }
    if let Some(loaded) = loaded {
        body["launched_at"] = json!(loaded.launched_at);
        body["stale_config"] = json!(loaded.stale_config);
    }
    body
}

/// The merged document every `har_state` event carries: one push replaces the
/// page's per-panel polling (Step 14).
pub(super) async fn merged_state(state: &ApiState) -> Result<Value, ApiError> {
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let loaded = loaded_rows(&scheduler.slots);
    let catalog = state.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            model_json(
                entry,
                loaded.get(&entry.name),
                scheduler.fit_validated.contains(&entry.name),
            )
        })
        .collect();
    let mut document = render_status_document(state, &scheduler).await;
    document["models"] = Value::Array(models);
    Ok(document)
}

/// The R9 sum for the memory bar: what the running containers said they reserve,
/// summed over the loaded occupants' per-slot `engine_report` figures only (the
/// scheduler snapshot carries each container's own report, so `/har/status` and the
/// SSE push derive the identical number — review 2026-10-31: threading it only
/// through the SSE build made the annotation unreachable on the poll path, which is
/// the ONLY path a token-gated page has). `None` when no loaded container has
/// reported — a container that never spoke must not render as "reserved 0 GiB"
/// next to a card that disagrees. Same-name occupants on two slots are one
/// container: the sum takes the model's figure once.
pub(super) fn container_reported_gb(scheduler: &SchedulerStatus) -> Option<f64> {
    let mut per_model: BTreeMap<&str, f64> = BTreeMap::new();
    for slot in &scheduler.slots {
        if let (Some(model), Some(gib)) = (&slot.model, slot.container_reported_gb) {
            per_model
                .entry(model.as_str())
                .and_modify(|held| *held = held.max(gib))
                .or_insert(gib);
        }
    }
    let sum: f64 = per_model.values().sum();
    (sum > 0.0).then_some(sum)
}

/// `GET /har/models` — the full unfiltered catalog for the control page (G5: the
/// per-listener `/v1/models` stays category-filtered).
pub(super) async fn har_models(
    State(state): State<Arc<ApiState>>,
) -> Result<Json<Value>, ApiError> {
    let catalog = state.catalog().await?;
    let scheduler = state.scheduler.status().await.map_err(ApiError::from)?;
    let loaded = loaded_rows(&scheduler.slots);
    let models: Vec<Value> = catalog
        .iter()
        .map(|entry| {
            model_json(
                entry,
                loaded.get(&entry.name),
                scheduler.fit_validated.contains(&entry.name),
            )
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": models })))
}

/// The `/har/status` memory block: declared config numbers plus what the witness
/// actually sees, labeled with its source (F-7 lesson: never present a number
/// without saying whose number it is).
async fn mem_section(state: &ApiState, scheduler: &crate::scheduler::SchedulerStatus) -> Value {
    // Loads committed but not yet installed: the very set the admission gate debits
    // (`booking_gibs`), so the bar and a refusal can never quote two numbers
    // (review, 2026-10-26). The caller already holds this snapshot — asking the
    // actor for a second one per poll was a duplicated round-trip.
    let booked_gb = crate::scheduler::booking_gibs(&scheduler.jobs);
    let container_reported_gb = container_reported_gb(scheduler);
    let ram = crate::witness::ram_sample();
    let mut section = match state.witness.sample().await {
        Ok(sample) => {
            let attributed = sample.process_total_gib();
            json!({
                "total_gb": sample.total_gib,
                "reserve_gb": state.config.vram_reserve_gb,
                "booked_gb": booked_gb,
                "observed_used_gb": sample.used_gib,
                // What NVML accounted across all processes, or `null` when it
                // answered with no usable per-process figures at all (WDDM
                // degradation): absence, never a fake 0.0 GiB (review 2026-10-26).
                "attributed_gb": (attributed > 0.0).then_some(attributed),
                // R1: the witness's own answer to "could per-process bytes be
                // obtained" — the page's label must come from the witness, never
                // inferred from whether a number happened to be present (an idle
                // card legitimately has `attributed_gb: null` and `per_process`
                // capability at the same time).
                "attribution": sample.attribution.as_str(),
                // R9: what the loaded containers said they reserved, summed over
                // the currently loaded models' `engine_report` figures only —
                // `null` when no loaded model has a self-report. Display and
                // cross-check against `observed_used_gb`; never a gate input.
                "container_reported_gb": container_reported_gb,
                "source": state.witness.source(),
            })
        }
        Err(reason) => json!({
            "total_gb": state.config.vram_total_gb,
            "reserve_gb": state.config.vram_reserve_gb,
            "booked_gb": booked_gb,
            "observed_used_gb": null,
            "attributed_gb": null,
            // No sample, no claim: a witness that cannot answer cannot grade its
            // own attribution — absence, never a fake `unavailable`.
            "attribution": null,
            "container_reported_gb": container_reported_gb,
            "source": format!("unavailable ({reason})"),
        }),
    };
    // A1: host RAM is informational context next to the VRAM bars (§0.4 keeps it
    // out of admission). Since the RAM ladder landed (operator ruling 2026-11-05)
    // the block also states the RAM side of the gate: the reservation the loaded
    // CPU residents debit (the SAME figures the slot rows carry, so bar and rows
    // cannot quote two numbers), the RAM reserve the gate subtracts before
    // comparing, and the live host reading. `booked_ram_gb` counts installed RAM
    // residents whose booking their catalog row still proves (W3-01: a resident
    // the row cannot prove carries no figure on either side) —
    // the RAM twin of `container_reported_gb`'s loaded-sum rule; a GPU row's host
    // RAM rides `ram_used_gb` and never a ledger.
    if let Value::Object(fields) = &mut section {
        let booked_ram_gb: f64 = scheduler
            .slots
            .iter()
            .filter_map(|slot| slot.booked_ram_gb)
            .sum();
        fields.insert("ram_total_gb".to_string(), json!(ram.map(|r| r.total_gib)));
        fields.insert("ram_used_gb".to_string(), json!(ram.map(|r| r.used_gib)));
        fields.insert("booked_ram_gb".to_string(), json!(booked_ram_gb));
        fields.insert(
            "ram_reserve_gb".to_string(),
            json!(state.config.ram_reserve_gb),
        );
    }
    section
}

/// `GET /v1/models` — exactly this listener's category, never the full catalog
/// (no cross-category disclosure, G5).
pub(super) async fn v1_models(
    State(listener): State<Arc<ListenerState>>,
) -> Result<Json<Value>, ApiError> {
    let catalog = listener.api.catalog().await?;
    let models: Vec<Value> = catalog
        .iter()
        .filter(|entry| entry.category == listener.category)
        .map(|entry| {
            json!({
                "id": entry.name,
                "object": "model",
                "owned_by": "simpleharn",
                "engine": entry.engine,
            })
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": models })))
}
