---
title: Gotchas
owner: operator
status: active
audience: [contributor, agent, reviewer]
source_of_truth_for: [contributor traps, verification pitfalls]
last_reviewed: 2026-11-05
review_trigger: release
---

# GOTCHAS - traps that cost real time

Read this before you debug a failure blind. One line per trap; the tag names
the area (`ps`, `git`, `test`, `proc`, `ui`, `hw`, `doc`). The local-only
layer carries the machine-specific traps; this page carries the ones a
contributor can hit on any machine.

- [test] **libtest hides the stdout of a PASSING test** - a test that prints
  a harness verdict and returns `Ok` passes; the classifier needs
  `--nocapture`, as `scripts/gate.ps1` and the sweep scripts do.
- [test] **A test that asserts its own copy of a production list cannot catch
  drift** - iterate the production constant, never a duplicated literal.
- [test] **Check that a branch is reachable before you call it a live bug** -
  a latent fallback or a probe that reads the wrong state produces a credible
  but wrong claim.
- [test][proc] **Any feature that persists config: verify
  persisted -> parse -> validate across a restart** - a serializer detail can
  drop a field that a fresh boot reads back as a default.
- [test] **The engine truth set for `cpu_offload` and `jinja` is
  `true|1|on`** - the lenient YAML reader accepts more than the engine does;
  pins and UI use the engine set.
- [test] **Scripted witnesses queue their answers per read** - adding or
  removing a witness read shifts every scripted answer behind it; drive
  phases with a state override.
- [test] **Captured stdout fixtures must be byte-pinned from a real capture** -
  trailing spaces and column padding are part of the format.
- [ps] **`-match` is case-insensitive** - use `-cmatch` with anchors, never
  bare substrings, when text case is meaningful.
- [ps] **A predicate that throws inside `Where-Object` silently drops every
  item** - a comma turns two calls into one invalid call; verify count, size
  and content after a bulk rewrite.
- [ps] **PowerShell re-encodes non-trivial text** - `Set-Content -Encoding
  UTF8` writes a BOM in 5.1; write machine-read files with
  `[IO.File]::WriteAllText` and a BOM-less `UTF8Encoding`.
- [ps] **`($false).Count` is 1 in Windows PowerShell 5.1** - print the raw
  value or compare with `-eq $true`.
- [git] **Do not `git checkout -- <file>` to undo a test mutation** - if the
  file also holds your intended edits, the checkout erases them; reverse the
  mutation in place.
- [proc] **`cargo build` cannot replace a running binary; `include_str!`
  embeds at compile time** - rebuild and hash-check the served asset before
  you blame the source.
- [proc] **`/har/unload` 200 is an acceptance, not a release** - the slot
  keeps its occupant and `state` reads `releasing` until the engine is gone;
  wait for the jobs ledger to reach a terminal outcome before you sample
  memory or kill anything.
- [proc] **Container engines outlive their spawn client** - release evidence
  is the closed port after the container is removed, not the client exit.
- [doc] **`/har/models` is an OpenAI envelope (`{"data":[...]}`) keyed on
  `id`; `/har/status` carries memory as `mem`; `slots` is fixed length** - a
  scan-refused row is absent from `/har/models` and its sentence lives in
  `catalog_errors`.
- [doc] **A hand-written minimal config silently inherits every unset
  default** - start from the release config as a base.
- [doc] **A persisted measured pair in a model row can satisfy any booking
  predicate** - poll the live surface, not only the row, before you judge.
- [hw] **Per-process VRAM attribution is often `[N/A]` under WDDM** -
  measured figures are free-memory deltas, not footprints; trust them only on
  a freed card.
