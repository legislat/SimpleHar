---
title: OpenAI compatibility matrix
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [OpenAI-compatible surface scope, known-good configurations]
last_reviewed: 2026-11-05
review_trigger: behavior-change, release
---

# OpenAI compatibility matrix

Up: `docs/DOCS_README.md`. This file owns the scope of the OpenAI-compatible
surface and the known-good configurations table. `docs/reference/API.md` owns
the control API contract, the full error code list, and the memory provenance
fields.

Every claim here names its evidence: a test name, a source file, or the word
"untested". A test name runs under `cargo test`. This page makes no claim of
full OpenAI API coverage. It states the tested behavior and the gaps.
Dated: 2026-11-05. This page describes the current `main` build, and that
build is unreleased. The renamed wire values below ship in no released binary
yet: v1.3.0, the current release, still answers the pre-rename values.
`CHANGELOG.md` (Unreleased) owns the rename.

## 1. Listeners and routes

The harness runs four category listeners, one per category. `README.md`
(Categories and ports) owns the port matrix. Each category listener answers:

| Route | Behavior | Evidence |
|---|---|---|
| `GET /health` | Liveness. | `category_router`, `src/api/mod.rs` |
| `GET /v1/models` | Category catalog. Never admits. | `v1_models`; `polling_an_empty_slot_never_loads_a_model` |
| `POST /v1/chat/completions` | Proxy plus admission. | `proxy`; `the_wake_set_is_exactly_the_proxy_routes` |
| `POST /v1/completions` | Proxy plus admission. | same |
| `POST /v1/embeddings` | Proxy plus admission. One shared handler. | same |

- The waking set is exactly those three POST paths. The pins
  `only_the_three_inference_posts_wake_the_slot` and
  `the_wake_set_is_exactly_the_proxy_routes` fail if the predicate and the
  route table diverge.
- The control API is separate. `/har/*` and the control page exist only on the
  control listener: `control_router` (`src/api/mod.rs`) merged per listener in
  `supervise_listener` (`src/main.rs`). A category listener names no `/har/*`
  route (`category_router`). A request to any path the listener does not know
  answers the 404 envelope. `unknown_routes_answer_the_envelope_on_every_listener`
  pins that shape on both listener kinds.
- Any other OpenAI path is unrouted. See "What is not covered".

## 2. GET /v1/models

Each listener lists only its own category. No cross-category disclosure
happens. Evidence: `v1_models` (`src/api/mod.rs`), tests
`models_are_filtered_per_listener`, and `wrong_category_is_404_before_any_load`.

The reply is `{"object": "list", "data": [...]}`. Each row carries `id`,
`object: "model"`, `owned_by`, and `engine`. Evidence: `v1_models` and
`models_are_filtered_per_listener`.

`owned_by` is the string `simpleharn`. The harness never sends the pre-rename
string `simplehar`. There is no backward compatibility for the old value, so a
client that branches on `owned_by` must match the new one. Evidence: the pins
inside `models_are_filtered_per_listener` and the naming pins in
`tests/naming_cleanup.rs`.

This answer carries no loaded stamp. The loaded facts `launched_at` and
`stale_config` ride only the control-side `/har/models` and the SSE `har_state`
document. Evidence: `model_json` (`src/api/mod.rs`), test
`model_json_marks_loaded_rows_and_nobody_else`. The listing is deliberately
minimal, so local file paths stay off the published surface.

This GET never admits and never spawns an engine. Evidence: `wakes_slot`
(`src/api/mod.rs`) and `polling_an_empty_slot_never_loads_a_model`.

## 3. Model selection on the inference routes

The three inference POSTs may admit. An empty slot loads the named model first,
then forwards. A gate refusal answers 503 (section 6). Evidence: the
positive-control leg of `polling_an_empty_slot_never_loads_a_model` and
`admission_refusal_via_proxy_is_503_with_numbers`.

How the `model` field resolves:

- A catalog name or alias resolves case-insensitively. Tests
  `proxy_forwards_with_model_rewrite_and_streams_the_response` (alias `QWEN3`)
  and `load_unknown_model_is_typed_not_found`.
- An unknown name answers the 404 envelope. The message points at the
  listener's `/v1/models`. Test `load_unknown_model_is_typed_not_found`.
- A name that belongs to another category answers 404 before any load or
  spawn. Test `wrong_category_is_404_before_any_load`.
- Two catalog rows that claim one GGUF alias answer 409 and name both files.
  Test `named_proxy_on_an_ambiguous_gguf_alias_is_the_same_409_not_a_misdirected_404`.

A request with no `model` field:

- With a serving occupant, the request goes to it. The most recently proxied
  occupant wins. Tests `no_model_fallback_serves_the_loaded_occupant` and
  `no_model_fallback_prefers_the_most_recently_proxied_occupant`.
- With nothing loaded, the harness loads the category `default_model`. Test
  `no_model_fallback_loads_the_category_default`.
- With neither, the answer is 400 and names the listener. Test
  `no_model_without_default_is_400_naming_the_listener`.

## 4. Pass-through

### Response direction: byte-for-byte

- Upstream status codes cross unchanged. A real upstream status is never
  retried. Test `genuine_upstream_error_is_surfaced_never_retried`.
- The response body streams through byte-for-byte. SSE is included. Test
  `proxy_forwards_with_model_rewrite_and_streams_the_response` demands the
  engine's `text/event-stream` literal back byte-for-byte and keeps the
  upstream `content-type`.
- Response headers pass through, except the RFC 9110 hop-by-hop names and the
  names an engine `connection` value enumerates. `transfer-encoding` and
  `content-length` are re-framed by the harness. Test
  `proxy_strips_the_engines_hop_by_hop_response_headers`.
- The proxy never decodes a body, so `content-encoding` crosses with the
  opaque bytes. Test `proxy_passes_the_engines_content_encoding_through_verbatim`.
- On the three proxy routes an engine 404 is the engine's answer. Its content
  type and bytes cross untouched. Test
  `an_upstream_plain_404_on_a_proxy_path_is_never_rewritten`.

### Request direction: field-for-field, not byte-for-byte

- A body must be JSON. A non-JSON content type answers 415 before parsing:
  `posts_without_json_content_type_are_415`. A malformed JSON body answers 400
  in the project envelope. Evidence: the proxy door in `proxy`
  (`src/api/mod.rs`). The same envelope shape is tested on a control route by
  `malformed_logging_body_answers_the_project_envelope`. A wire test on a
  proxy route: untested.
- The proxy parses the JSON and re-serializes it for the engine, so every
  field survives except `model`. Whitespace and key order may change. The
  harness claims no byte-for-byte request pass-through. Evidence: `proxy`
  (`src/api/mod.rs`) and the pass-through note in
  `proxy_passes_the_engines_content_encoding_through_verbatim`.
- The `model` field is rewritten to the engine's self-reported id. When
  discovery reports nothing, the harness catalog name crosses instead. Tests
  `proxy_forwards_with_model_rewrite_and_streams_the_response`,
  `proxy_rewrites_each_occupant_to_its_own_engine_id`, and
  `a_silent_engine_falls_back_to_the_harness_name_on_the_wire`. A request with
  no `model` field receives the resolved name on the wire (section 3).
- The query string crosses exactly as the client sent it, a bare `?`
  included. Test `proxy_forwards_the_query_string_the_client_sent`.
- The proxy copies no client request header toward the engine. Evidence: the
  forward call in `proxy` (`src/api/mod.rs`). A wire test that asserts the
  absence: untested.

### Engine failure mid-request

- An engine that dies before any response head gets exactly one reap, reload,
  and retry. Only a connect failure classifies as death. Tests
  `dead_engine_connect_failure_restarts_exactly_once`,
  `crash_retry_re_resolves_by_name_after_the_id_rewrite`,
  `crash_retry_re_sends_the_successor_discovered_id`, and
  `crash_retry_falls_back_to_the_harness_name_when_the_successor_is_silent`.
- A timeout on a live engine answers 502 and never reaps. Test
  `timeout_class_is_502_and_never_restarts`.
- A death mid-stream surfaces inside the body stream, past the retry loop.
  Evidence: the T10.1 note in `proxy` (`src/api/mod.rs`). A wire test for a
  mid-stream death: untested.

## 5. tools, stop, seed, and other OpenAI parameters

The proxy validates no OpenAI request parameter and rewrites no parameter.
Every field except `model` crosses unchanged. Evidence: `proxy`
(`src/api/mod.rs`). The `stream` field crosses and drives the engine's SSE
answer, proven by `proxy_forwards_with_model_rewrite_and_streams_the_response`.
No per-field test exists for `tools`, `stop`, `seed`, or the sampling knobs:
untested. What those fields do depends on the engine behind the slot. This
page claims no full OpenAI parameter coverage.

## 6. Errors

- Every refusal carries both envelopes: flat `code` and `message`, plus the
  OpenAI `error{message,type,code}`. Both hold the same values. Tests
  `error_body_carries_flat_and_openai_envelope`,
  `unknown_routes_answer_the_envelope_on_every_listener`, and
  `wrong_methods_answer_the_envelope_and_keep_the_allow_header`. The full code
  list lives in `docs/reference/API.md`.
- The `error.type` value is `simpleharn_error`. The tests above pin it. The
  harness never sends the pre-rename `simplehar_error` value; there is no
  backward compatibility for it.
- An admission refusal adds two wait fields at the flat top level.
  `queue_position` is a one-based integer or an explicit null.
  `expected_release_s` is the rolling median of the harness's own measured
  releases, to one decimal. The harness omits it with no measured release.
  Neither field appears inside the `error` sub-object. Tests
  `error_body_renders_the_wait_hint_as_flat_fields_only`,
  `a_first_refusal_names_no_estimate_and_no_place`, and
  `a_confirmed_release_lands_the_estimate_and_a_place_on_the_next_refusal`.
- The admission 503 classes:
  - `unavailable` with insufficient room: the message names the need, the
    free figure, the reserve, and the blockers. Home: `src/budget.rs`. Test
    `admission_refusal_via_proxy_is_503_with_numbers`.
  - `below_launch_floor`: refused before spawn, with its own code in both
    envelopes. Test `below_launch_floor_gets_its_own_error_code`.
  - The whole-machine gate: the code stays `unavailable`, and the message
    reads "whole machine requested" and names the holders. Test
    `a_whole_machine_refusal_travels_as_unavailable`.
- One class covers request-shape refusals: 400 (operator ruling 2026-11-04).
  Examples: `unload_body_with_both_model_and_category_is_refused`,
  `default_model_requires_the_model_key_and_rejects_wrong_types`,
  `no_model_without_default_is_400_naming_the_listener`. A body over the limit
  answers 400, not 413: `control_bodies_are_capped_below_the_proxy_ceiling`
  and `proxy_bodies_are_capped_at_64_mib`.
- An unknown route and a wrong method answer the envelope on every listener,
  and the `Allow` header survives a 405. The two envelope tests pin this. On
  a proxy route the harness rewrite never touches the engine's own 404
  (section 4).
- 401 answers a missing or wrong token, and 403 answers a foreign `Origin` on
  a POST. The proxy routes are token-gated like every route. Tests
  `control_token_gates_api_routes_but_never_the_page_itself` and
  `guards_refuse_rebinding_hosts_and_foreign_origins`.
- A 500 keeps its class and hides the OS detail:
  `harness_failure_bodies_hide_the_os_detail_but_keep_the_class`.
- Every answer carries `x-request-id`, echoed from the client or generated:
  `request_ids_are_echoed_or_generated`.

## 7. What is not covered

These capabilities do not exist on the harness listeners. Any path outside
section 1 answers the 404 envelope
(`unknown_routes_answer_the_envelope_on_every_listener`). The route table in
`category_router` (`src/api/mod.rs`) is the evidence for absence.

- No `/v1/images`.
- No audio endpoints.
- No `/v1/files`.
- No Responses API (`/v1/responses`). An engine may serve its own dialect
  inside its slot. The harness exposes none of it.
- No TLS. Listeners bind plain HTTP on the loopback default: `supervise_listener`
  (`src/main.rs`) and `internal_host` in `docs/reference/CONFIG.md`.
- One shared token, not per-client keys. `control_token` /
  `SIMPLEHARN_TOKEN` gates every route except the page assets:
  `docs/reference/CONFIG.md` and
  `control_token_gates_api_routes_but_never_the_page_itself`.

## 8. Known-good configurations

This table records verified configurations. It does not state a minimum
requirement. A configuration that is absent here is unproven, not broken.
An evidence-run name points at the local evidence directory; it links no file
in this repository. Each row names the build the run used: a run of the
working tree says `main (unreleased)`, and a run of a release names its tag.
The 2026-11-05 working-tree runs predate the rename, so no row yet records the
renamed wire values on a built binary.

| Configuration | OS | GPU | Engines | Harness | Date | Evidence |
|---|---|---|---|---|---|---|
| Full rig | Windows 11 | one NVIDIA GPU, 71.7 GiB total, 2.0 GiB reserve | llama.cpp, ninfer, basalt, strata rows | main (unreleased) | 2026-11-05 | Evidence run `2026-11-05-evidence-matrix` (RESULTS.md) |
| Minimal | Windows | one NVIDIA GPU that fits the model plus the reserve | llama.cpp only | main (unreleased) | 2026-11-05 | The `README.md` quick start; cardless llama adapter tests in `src/engine/llama.rs`. No isolated live leg on record |
| CPU-only llama | Windows | one NVIDIA GPU, 72 GiB; loads booked host RAM, not the card | llama.cpp only, CPU rows | v1.3.0 (run on pre-tag `9e7ddb7`) | 2026-11-05 | `docs/evidence/v1.3.0-card-run.md` |

## 9. Glossary

- Admission: the gate's decision to admit or refuse one load against live
  memory. Home: `docs/architecture/DOCTRINE.md`.
- Category: the four model classes WORK, RP, AUX, and EMB. One listener each.
  Home: `README.md`.
- Envelope: the dual JSON error shape of section 6. Home:
  `docs/reference/API.md`.
- Serve id: the model id an engine self-reports. The proxy rewrites `model` to
  it (section 4).
- Witness: the live measured memory that is the budget truth. Home:
  `docs/architecture/OVERVIEW.md`.
