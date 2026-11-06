---
title: Operator runbook
owner: operator
status: active
audience: [operator, user, agent]
source_of_truth_for: [operator recovery procedures]
last_reviewed: 2026-11-06
review_trigger: behavior-change, release
---

# Operator runbook

Up: `docs/DOCS_README.md`. This file owns sanitized operator recovery
procedures. Each procedure names a symptom, the evidence to collect, a safe
action, and a stop condition. Case-by-case diagnosis lives in
`docs/reference/TROUBLESHOOTING.md`. Two rules apply to every procedure: the
harness reaps only what it spawned, so never stop a process it does not own,
and never start a second harness instance.

## Engine listener down or slot unreachable

Symptom: the tray icon turns red, a category listener does not answer, or a
slot never reaches `serving`.

Evidence to collect: `GET /har/status` on the control listener (default port
8708) and its per-listener verdict; `GET /har/log` for the line that names
the port or the failed readiness; `GET /health` on the category port.

Safe action: read the named failure first. The harness keeps running and
retries the bind after a bind failure. If the log says the port lies in a
reserved range, apply the Hyper-V/WSL2 reservation fix in
`docs/reference/TROUBLESHOOTING.md`. If the load failed readiness, check the
engine install against `docs/reference/ENGINE-RECIPES.md`.

Stop condition: stop when the listener verdict reads `Serving` and one
request answers. If the port is held by a process the harness did not
spawn, go to the foreign-resident procedure. Never stop a listener the
harness did not spawn.

## Release or swap stuck past its timeout

Symptom: a slot stays `releasing` past `release_timeout_s`, or a job does not
reach a terminal outcome.

Evidence to collect: `GET /har/jobs` for the job row, its `state`, `path`,
and `outcome`; `slots[].state` in `GET /har/status`; the harness log lines
around that job.

Safe action: wait for the configured deadline. An unproven release turns
terminal at `release_timeout_s`, and the memory-settle wait then runs up to
`settle_timeout_s`. A `POST /har/unload` `200` is an acceptance, not a
release proof; read the ledger, not the reply code. `POST /har/cancel`
cancels an in-flight load cooperatively: the current step completes, cleanup
runs, then the job is reaped. When a release times out, the engine stays
installed and the slot reads `serving` again: the harness claims no death it
did not prove.

Stop condition: stop when the job row reaches a terminal outcome and the slot
state matches the ledger. If a port or a container is still held after the
harness exits, go to the foreign-resident procedure. Do not start a second
harness beside the running one.

## A row shows stale after a config edit

Symptom: a serving model row carries the amber stale marker or answers
`stale_config` in `GET /har/models` after you edited that model's YAML file.

Evidence to collect: the row entry in `GET /har/models`; compare the file
bytes on disk with the launch the slot installed. Judge what changed: a
cost-only edit (`vram_gb`, the cost class, or the measured pair) moves no
launch flag; any other edit changes the launch.

Safe action: rescan the catalog: press Rescan on the page or send
`POST /har/rescan`. A cost-only rescan re-books a loaded model in place; no
reload is needed and no marker appears. A launch-flag edit marks the row
`stale_config` and applies on the next load of that model: the marker is a
fact, not a fault, because a serving model keeps the launch it was spawned
with. To apply it, unload the model and load it again.

Stop condition: stop when the marker clears after that new load, which proves
the slot installed the current bytes. For a cost-only edit, stop when the
rescan confirms the booking. Do not expect a live server to change its
launch while it serves.

## A foreign process holds an engine slot or port

Symptom: a documented default port will not bind, the card shows VRAM held by
a process the harness did not spawn, or a ninfer container reads as stranded
in the amber page badge and in `stranded_containers`.

Evidence to collect: the bind refusal or boot log line that names the port;
`GET /har/status` for `stranded_containers` and the memory facts; a launch
floor refusal that names the unreleasable holders.

Safe action: harness reaping is ownership-scoped: it kills only its own job
objects, and it reaps only a ninfer container whose claim ledger proves this
harness started it. A foreign listener, process, or unclaimed container is
the operator's to manage outside the harness. Verify the owner first. For a
stranded container the harness cannot claim, the harness names it and never
reaps it; remove it by name yourself only after you prove it belongs to a
model you deleted, for example `docker rm -f <container>`.

Stop condition: stop when the next bind retry or the next load succeeds.
Never stop a process whose owner you cannot prove.

## Admission refuses and free memory looks wrong

Symptom: a load answers `503` with code `unavailable` or `below_launch_floor`,
names blockers, and the printed free figure disagrees with what you expect.

Evidence to collect: the refusal text and its numbers (`free`,
`unreleasable`); `GET /har/jobs` for the refused row; `GET /har/status` for
the `mem` provenance fields; the `GET /har/log` lines around the refusal.

Safe action: read the printed `free` as the gate figure: it is the card
reading minus the reserve, with the same-slot replacement credited. A card
reading that lags a kill settles inside `witness_tolerance_gb`. A launch
floor refusal needs real free VRAM: unload a holder the refusal names, or
lower `launch_only_with_at_least_vram_free` on the row (see
`docs/reference/MODELS.md`). Nothing over-admits, so a retry cannot succeed
until memory actually frees.

Stop condition: stop when a load admits, or when the refusal reads
`witness_unavailable`: then the card is unreadable, so stop loading and check
the driver before any retry. Never raise `vram_total_gb` to force a load
through: the witness, the card's own reading, is the budget truth
(`docs/architecture/DOCTRINE.md`).
