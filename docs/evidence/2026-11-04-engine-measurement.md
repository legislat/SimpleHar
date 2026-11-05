---
title: Engine measurement card capture (2026-11-04)
owner: operator
status: active
audience: [maintainer, reviewer]
source_of_truth_for: [engine VRAM surface measurements, 2026-11-04]
last_reviewed: 2026-11-05
review_trigger: release
---

# Engine measurement card capture - 2026-11-04

Freed window against the live v1.1.2 harness on loopback, no token, all
slots empty at the start (3.3 of 71.7 GiB used). Loads and unloads went
through the live harness. Strata loaded in 65 s. The capture is the
empirical basis for the engine-status booking decisions in the ledger.

## Strata (v0.1.55, row `vram_gb: 67.8`)

- `GET /status` answers `{"busy": false, "queued": 0}` only. It carries no
  `info.vram`.
- `GET /v1/status` carries a `vram` object with `{"elastic": true}` only, plus
  `machine.gpu.used_mib` / `total_mib` (engine-read, card-wide).
- `GET /metrics` carries the full snapshot. The engine block reports
  `expert_cache_mib: 59832`, `conversation_cache_mib: 1536`, `arena_mib: 0`,
  `vram_free_mib: 1754`, `vram_elastic: 1`, `kv_resident: 32768`. It has no
  `reserve_mib`.
- `POST /v1/vram {"reserve_mib": 2048}` answered 200 in 3 ms with
  `expert_cache_mib: 59834`, `vram_free_mib: 1686`, `reserve_mib: 2048`,
  `prompt_chunk: 4096`. Those fields are top-level on the reply, not nested.
- The process tree is harness -> python -> python -> engine (+ a vision
  child). Direct children only miss the engine.
- Per-process VRAM answers `[N/A]` on this driver; the harness reports
  `attributed_gb: null` with source NVML. The process-tree fallback cannot
  book on this machine.
- Arithmetic: components plus reserve total 60.4 GiB, the actual hold is about
  69.75 GiB, and the row declares 67.8. The engine does not itemize weights or
  KV, so a component sum is a lower bound. The declared floor stays the
  booking.

## Basalt (v0.2.1, manual serve on loopback, row `vram_gb: 67.4`)

- Harness admission refused the load (67.4 plus the 2.0 reserve against 69.1
  free). The operator served it manually; captures are read-only GETs.
- `GET /status` answers liveness only. `GET /v1/status` carries
  `vram: {"elastic": false}` and `machine.ram.used_gib: 72.4`. There is no
  engine VRAM object.
- `GET /metrics` reports `expert_cache_mib: 61363`, `arena_mib: 20288`,
  `conversation_cache_mib: 4096`, `vram_free_mib: 2948`, `vram_elastic: 0`,
  `quant: Q4_K_XL`. The component sum (83.7 GiB) exceeds the card because part
  of the model lives in RAM.
- The card read 70,176 MiB (68.5 GiB) used while loaded; the row declares
  67.4, which is 1.1 GiB low against that hold.
- `POST /v1/vram` is inapplicable (`vram_elastic: 0`); the serve answers 400.
- The operator ruled that Basalt is greedy with dynamic RAM offload. Basalt
  rows stay fixed-cost until the greedy class lands.

## Consequences

1. No engine reports a footprint field. `/metrics` components are a lower
   bound, never a booking on their own.
2. Strata parses `/metrics` as an engine-status lower bound; the declared
   floor stays the booking and the parse only books growth above it.
3. Basalt bounds the parse and keeps the tree/declared path.
4. `descendant_pids` must walk the full tree.
5. Per-process NVML is unavailable on this driver, so the tree fallback
   cannot book here.
