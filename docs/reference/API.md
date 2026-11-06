---
title: HTTP API reference
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [HTTP control API contract, error codes, memory provenance]
last_reviewed: 2026-11-05
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

Polling never loads a model. Only the three inference routes start a load:
`/v1/chat/completions`, `/v1/completions`, and `/v1/embeddings`. `GET
/v1/models` and `GET /health` answer from the catalog and static state. They
never admit. When a slot is not loaded, an inference request loads the model
first. Admission may refuse that load. The harness then answers `503` and
names the state or the blockers in `message`. The `wakes_slot` predicate and
its route-table falsifier pin this contract. A future route that would wake a
slot on a read fails the pin.

## Control listener endpoints (8708)

| Endpoint | Body / meaning |
|---|---|
| `GET /health` | Liveness check. Once `control_token` is set, it requires the token like every other route. A probe that omits the token reads 401 and can look like a dead listener. |
| `GET /har/status` | Full snapshot, including `mem`, `jobs`, `last_error`, and boot timings. |
| `GET /har/events` | SSE state pushes. |
| `GET /har/jobs`, `GET /har/jobs/{id}` | Job ledger. Each admitted load row names its admission rung in `path` (see "Job ledger rung field"). A row of a released resident leaves `Serving` at the confirmed release (see "Victim row terminalization"). The ledger keeps every live row, the load row of each installed occupant, and the newest 200 terminal rows; older terminal history ages out, so an aged id answers unknown on `GET /har/jobs/{id}` and on `POST /har/cancel`. |
| `GET /har/log?limit=n` | Ring buffer of harness events. An absent `limit` uses the default; a whole number clamps to the ring capacity; a `limit` that is present but not a whole number answers `400 bad_request` in the project envelope naming the value. |
| `GET /har/models` | Full catalog (the page view; includes paths). The `context` field is the **effective served context**, never a stale row key. For llama it is the row `context` knob. For strata it is the context figure in the row's `strata_args` argv (the `--max-context` token or the `--max-context=` spelling, digits only; anything else answers `null`). For ninfer it is the max-context of the profile its launcher resolves (the built-in table or the `ninfer_profile_<name>` line, the identical resolution the spawn uses). For maya it is the digits-only `--max-context` token in the L1 config's `args` (the `--max-context` or `--max-context=` spelling), captured at scan. Every other engine answers `null`, and so does any row whose effective context does not resolve. |
| `POST /har/load` | `{model}`. Loads the model into its own category, from its yaml. |
| `POST /har/unload` | `{model}` \| `{category}` \| `{}` (all). Never both keys at once (`400`). The reply status is honest: `unloading` (counts `unloading`/`jobs_cancelled`; 200 is an acceptance, and the release lands on the job ledger), `cancelled` (only load jobs were cancelled, and nothing installed was freed; cancelling an in-flight unload is not this case, because its re-armed release is reported under `unloading`), or `nothing_unloaded`. Unloading a *loading* slot cancels that swap, and cleanup still completes. |
| `POST /har/rescan` | `{}` catalog rescan. |
| `POST /har/config/default` | `{category, model \| null}`. `model` is required. A string sets the default, stored as its canonical name, and `null` clears it. The change persists to the config file. |
| `POST /har/config/logging` | `{enabled: bool}`. Toggles the verbose harness log. |
| `POST /har/config/model` | `{model, category?, cpu?}`. Writes back to the model yaml. |

## Category label field (`categories[].label`)

Every category row of `/har/status` and of the SSE `har_state` document
carries `label`. The value is the effective display name: the trimmed
`label` of the config entry, or the `id` when none is set. The field is
additive and always present; every pre-existing key stays. The `id` beside
it remains the identity: request payloads, the yaml `category:` values, the
`evictable_by` lists, and the engine-port plan keep the ids. The page
renders the label in its tabs and pills. The rules and the 24-character cap
live in `CONFIG.md` (the `categories` key).

## Slot state field (`slots[].state`)

Every slot in `/har/status` and in the SSE `har_state` document carries
`state`. The server derives it; the page renders the value verbatim.
The values are:

- `empty`: no occupant and no job.
- `loading`: a job is attached and no occupant is installed yet.
- `serving`: an occupant serves the slot with no release in flight.
- `releasing`: a committed release is in flight. The slot keeps the occupant
  installed and visible. The value stays `releasing` until the driver
  confirms the release. If the release times out, the job goes terminal,
  the occupant stays, and the slot reads `serving` again.

A `200` reply from `POST /har/unload` is an acceptance, not a release proof.
Read `slots[].state` and the job ledger for the release truth.

## Job ledger rung field (`jobs[].path`)

Every load the gate admitted carries `path`. The rows appear in
`GET /har/jobs`, in `GET /har/jobs/{id}` (under `job`), and in the `jobs`
array of `/har/status`. The value names the rung of the making-room ladder
that the gate decided at admission:

- `FitsNow`: the gate admitted the load as it stood. It decided to release
  nothing to make room. A same-slot replacement is also `FitsNow`: the
  request itself names that release, so it is not a rung.
- `ReleaseFirst`: the gate decided to release the slot's own occupant
  first, then to re-gate the load against the physical card.
- `Drain`: the eviction planner queued cross-category victims and the gate
  admitted the load to drain them.

The rung names the decision, not the outcome. A later failure does not
erase it: a release-timeout row still names the drain the gate decided.

A row with no rung omits the field. An unload row never made an admission
decision. A load the gate refused never reached a rung, so it carries the
refusal and no rung. That covers every gate refusal class, the launch floor
(`below_launch_floor`) included. A load the gate accepted keeps its rung
even when a later step refuses it, such as the launch preflight refusal:
the rung names the decision the gate took, which is exactly when that
refusal runs — after the gate said yes.

The gate implements these three rungs. The rungs `beside-residual`, `lend`,
and `park` are reserved for a future step. No code path produces a reserved
rung.

## Victim row terminalization

A confirmed release commits two ledger facts together: the victim's slot
clears, and the load row that produced that resident leaves `Serving` for
`Idle`. The row keeps `terminal: true`, reports `state: Idle` on the wire,
and its `outcome` takes the word that names the release:

- `evicted`: a cross-category eviction released the engine.
- `unloaded`: an operator unload, an idle-timeout sweep, a same-slot
  replacement, or a crash restart released the engine. No test pins the
  crash-restart cause; that one line is a static read.

The row carries no error: a release is not a fault, and the row never turns
the headline red. An unconfirmed release commits nothing: when a release
times out, the engine stays installed, the slot reads `serving` again (see
"Slot state field"), and the row keeps `Serving` and `loaded` — the harness
claims no death it did not prove.

Read the slot table and the ledger together: a slot is `empty` exactly when
no `Serving` row names a model for that slot. The scheduler terminalizes
the victim's own row at the commit point and never rewrites a row that a
newer spawn owns (the released engine's spawn ticket decides), so a
re-minted resident keeps its honest `Serving` row.

## Memory provenance fields

The status and catalog documents show how the harness obtained a memory figure.
Do not treat a displayed figure as a booked figure unless its provenance allows
booking.

`/har/status` carries these memory fields:

- `mem.attribution`: `per_process` or `unavailable`. It is `null` when the
  witness returned no sample.
- `mem.container_reported_gb`: the sum of the loaded containers' own reserved
  figures. It is `null` when no container reported a figure.
- `mem.booked_ram_gb`: the sum of the installed CPU-resident rows' RAM
  bookings. It equals the sum of the slot rows below and never counts a GPU
  row. An host that loads nothing CPU reads `0.0`.
- `mem.ram_reserve_gb`: the reserve the RAM gate subtracts before it
  compares. It rides beside the host reading.
- `slots[].container_reported_gb`: the occupant container's own reserved
  figure. It is present only for a ninfer container whose `engine_report`
  anchored the current launch.
- `slots[].booked_ram_gb`: the GiB a CPU-resident occupant books in host
  RAM.
- `slots[].ram_cost_basis`: the booked RAM rung: `declared`, `measured`,
  or `estimate`.
- `slots[].ram_source`: whose number the booking is: `per_process` when
  the measured rung books, `declared` when the floor or the estimate books.

The harness emits these three fields together, and only on a
CPU-resident row whose booked figure its catalog row still proves. The
harness omits all three when they cannot be proven against the ledger.
The row may have vanished after the load. A cost-only edit may have
moved the row's rung under the loaded resident. A same-model twin may
keep a booking that a fold did not re-book. Absent means "no claim":
never a re-label and never zero.

`/har/models` and the SSE `har_state` document carry these per-model fields:

- `cost_basis`: `declared`, `measured`, `fit`, `estimate`, or `undeclared`.
  On a `cpu_offload` row it names the booked **RAM** rung, never a VRAM
  rung; on a GPU row it keeps the VRAM ladder unchanged.
- `measured_attribution`: `engine_report`, `per_process`, `total_diff`, or
  `null`.
- `required_to_free_gb`: the figure the model needs free. It is present only
  when `cost_basis` is `measured`.

The RAM cost surface rides a `cpu_offload` row only. A GPU row carries none
of these keys:

- `ram_gb`: the declared RAM floor in GiB, or `null`.
- `measured_ram_gb`, `measured_ram_min_gb`: the trusted measured RAM pair,
  `null` until a load measured it. The harness writes the pair back.
- `est_ram_gb`: the GGUF RAM estimate. It is present only when the estimate
  is what books.
- `ram_source`: `per_process` or `declared`, the same rule as the slot rows.

A guarded `engine_report` above the declared `vram_gb` books instead of the
declaration (max truth). A declaration at or above the report stands.

## Error codes

Every error carries both a flat `{code, message}` and the OpenAI-style
`{error:{message,type,code}}` envelope, with the same values in both. The
`type` value is the wire identifier `simpleharn_error`. It follows the product
name. The harness never sends the pre-rename `simplehar_error` value; there is
no backward compatibility for it. The
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

An admission refusal that answers `unavailable` or `below_launch_floor` also
carries two wait fields at the flat envelope top level:

- `queue_position`: an integer, or an explicit `null`. It is the one-based
  place behind the loads that are already ahead in the refused category. A
  `null` means no other load is ahead in that category. The harness runs no
  queue and grants no queue place to any refusal.
- `expected_release_s`: a number of seconds, to one decimal. It is a rolling
  median of the harness's own measured releases. It is an estimate, never a
  promise. The harness omits the field when it has no measured release to
  quote; an absent field means the harness has no basis to quote it.

Other refusals that answer `unavailable` omit both fields, like every other
error class. Neither field appears inside the OpenAI `error` sub-object.
Both describe the decision moment only. The harness runs no wait queue and
keeps no promise about a later load.

An unknown route and a wrong method answer this same envelope on every
listener. A body over the limit answers `400 bad_request`, not `413`
(64 KiB on the `/har/*` control routes, 64 MiB on the proxy routes). The
operator ruled on 2026-11-04 that a request-shape refusal stays one class.
