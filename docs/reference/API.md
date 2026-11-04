---
title: HTTP API reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [HTTP control API contract, error codes, memory provenance]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release
---

# HTTP API reference

Up: `docs/DOCS_README.md`. This file owns the HTTP control API contract, the
error codes, and the memory provenance fields.

## Public listener endpoints (`/v1/*`)

Each public category listener answers the OpenAI-style endpoints
`/v1/models` (filtered by category), `/v1/chat/completions`,
`/v1/completions`, and `/v1/embeddings`. A request that names no model goes
to the loaded model of that category. At AUX this is the most recently used
model. When nothing is loaded, the harness loads that category's
`default_model` instead of returning an error.

## Control listener endpoints (8708)

| Endpoint | Body / meaning |
|---|---|
| `GET /health` | Liveness check. Once `control_token` is set, it requires the token like every other route. A probe that omits the token reads 401 and can look like a dead listener. |
| `GET /har/status` | Full snapshot, including `mem`, `jobs`, `last_error`, and boot timings. |
| `GET /har/events` | SSE state pushes. |
| `GET /har/jobs`, `GET /har/jobs/{id}` | Job ledger. |
| `GET /har/log?limit=n` | Ring buffer of harness events. |
| `GET /har/models` | Full catalog (the page view; includes paths). The `context` field is the **effective served context**, never a stale row key. For llama it is the row `context` knob. For strata it is the context figure in the row's `strata_args` argv (the `--max-context` token or the `--max-context=` spelling, digits only; anything else answers `null`). For ninfer it is the max-context of the profile its launcher resolves (the built-in table or the `ninfer_profile_<name>` line, the identical resolution the spawn uses). Every other engine answers `null`, and so does any row whose effective context does not resolve. |
| `POST /har/load` | `{model}`. Loads the model into its own category, from its yaml. |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all). Never both keys at once (`400`). The reply status is honest: `unloading` (counts `unloading`/`jobs_cancelled`; 200 is an acceptance, and the release lands on the job ledger), `cancelled` (only load jobs were cancelled, and nothing installed was freed; cancelling an in-flight unload is not this case, because its re-armed release is reported under `unloading`), or `nothing_unloaded`. Unloading a *loading* slot cancels that swap, and cleanup still completes. |
| `POST /har/rescan` | `{}` catalog rescan. |
| `POST /har/config/default` | `{category, model \| null}`. `model` is required. A string sets the default, stored as its canonical name, and `null` clears it. The change persists to the config file. |
| `POST /har/config/logging` | `{enabled: bool}`. Toggles the verbose harness log. |
| `POST /har/config/model` | `{model, category?, cpu?}`. Writes back to the model yaml. |

## Memory provenance fields

The status and catalog documents show how the harness obtained a memory figure.
Do not treat a displayed figure as a booked figure unless its provenance allows
booking.

`/har/status` carries these memory fields:

- `mem.attribution`: `per_process` or `unavailable`. It is `null` when the
  witness returned no sample.
- `mem.container_reported_gb`: the sum of the loaded containers' own reserved
  figures. It is `null` when no container reported a figure.
- `slots[].container_reported_gb`: the occupant container's own reserved
  figure. It is present only for a ninfer container whose `engine_report`
  anchored the current launch.

`/har/models` and the SSE `har_state` document carry these per-model fields:

- `cost_basis`: `declared`, `measured`, `fit`, `estimate`, or `undeclared`.
- `measured_attribution`: `engine_report`, `per_process`, `total_diff`, or
  `null`.
- `required_to_free_gb`: the figure the model needs free. It is present only
  when `cost_basis` is `measured`.

A guarded `engine_report` above the declared `vram_gb` books instead of the
declaration (max truth). A declaration at or above the report stands.

## Error codes

Every error carries both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope, with the same values in both. The
`code` is one of: `not_found` (404), `bad_request` (400), `unauthorized`
(401, control token), `forbidden` (403, cross-site `Origin` on a POST),
`conflict` (409, cancelled / superseded / port still held / an edit the
scanner would refuse to publish), `method_not_allowed` (405),
`unsupported_media_type` (415),
`unavailable` (503, admission refused with the numbers and blockers),
`below_launch_floor` (503, refused before spawn), `witness_unavailable`
(503, card unreadable), `upstream` (502), `not_implemented` (501),
`internal` (500). A refusal that carries numbers puts them in `message` and
in the job ledger, never in a bare status.

An unknown route and a wrong method answer this same envelope on every
listener. A body over the limit answers `400 bad_request`, not `413`
(64 KiB on the `/har/*` control routes, 64 MiB on the proxy routes). The
operator ruled on 2026-11-04 that a request-shape refusal stays one class.
