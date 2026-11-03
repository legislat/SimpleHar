---
title: Troubleshooting
owner: operator
status: active
audience: [user, contributor, agent]
source_of_truth_for: [troubleshooting and failure diagnosis]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release
---

# Troubleshooting

Up: `docs/DOCS_README.md`. This file owns troubleshooting and failure
diagnosis. Log retention counts live in the Log retention section of
`docs/reference/OPERATIONS.md`.

- **A port will not bind.** The status line and the log name the port. If a
  Hyper-V/WSL2 dynamic reservation covers the port, the harness already
  reports `port N lies in the reserved range`. The command
  `netsh interface ipv4 show excludedportrange protocol=tcp` shows all such
  ranges. Reserve your port permanently with
  `netsh int ipv4 add excludedportrange protocol=tcp startport=N numberofports=1`.
- **A model file is missing at boot.** The scan drops a `llama` row whose
  GGUF is gone and reports `! <file>.yaml: model file not found: <path>`. A
  row that publishes but cannot load leads its log line with `!`. Either
  way you see it before the first load tries it.
- **Refusals.** Every refusal names the blockers and the numbers, in the
  `/har/status` jobs and in `last_error`. Nothing over-admits, so a refusal
  means you must unload something. The witness, the card's own reading, is
  the budget truth.
- **`launch floor not met`** (code `below_launch_floor`). The card holds
  too little physically free VRAM for that model to launch sanely, foreign
  processes included. The harness refused the load **before spawn**, so
  nothing thrashed. Free VRAM, or lower the model's
  `launch_only_with_at_least_vram_free`. The printed `free` is the number
  the gate compared: reserve deducted, same-slot replacement credited.
- **A second instance.** A single-instance guard makes a duplicate exit
  immediately.
- **Logs.** Logs live in `logs/` next to the exe. The harness streaming log
  is `logs/harness-<boot-epoch>.log`, and engine children keep
  `logs/<model>-<epoch>.log`. `GET /har/log` reads the in-memory ring. A
  non-zero `log_write_failures` at `/har/status` means the file sink died,
and this boot leaves an incomplete artifact. Roll sizes and retained file
   counts are in the Log retention section of `docs/reference/OPERATIONS.md`.
