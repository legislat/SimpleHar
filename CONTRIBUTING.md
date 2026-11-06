---
title: Contributor pointer
owner: operator
status: active
audience: [contributor]
source_of_truth_for: []
last_reviewed: 2026-11-06
review_trigger: release, quarterly
---

# CONTRIBUTING.md

This file is a pointer. It owns no rules. Read `README.md` for the product.

## Where the rules live

| Topic | File |
|---|---|
| Documentation map | `docs/DOCS_README.md` |
| Task lifecycle and templates | `docs/process/TASKS.md` |
| Coding standards | `docs/process/ENGINEERING.md` |
| Gate and evidence | `docs/process/VERIFICATION.md` |
| Git, commits, and release | `docs/process/GIT_WORKFLOW.md` |

## Local-only state

The live queue, the handoff snapshot, open findings, traps, and evidence are
machine-local and gitignored. They live in `HANDOFF.md` and `handoff/`. Ask the
operator if you need them.

## External contributors

You do not need `HANDOFF.md` or `handoff/`. They are gitignored and absent
from a fresh clone.

1. Read `README.md`, `docs/process/ENGINEERING.md`,
   `docs/process/VERIFICATION.md`, and `docs/process/GIT_WORKFLOW.md`.
2. Open an issue to propose the change, or open a pull request directly.
3. CI runs on every pull request.
4. The maintainer creates the local task file and queue row when the work
   needs one.

## First task path (operator and agent sessions)

This path needs the local-only layer in Local-only state above.

1. Read `README.md` and `docs/process/TASKS.md`.
2. Create a task file under `handoff/tasks/`.
3. Implement inside the task scope and run the gate
   (`docs/process/VERIFICATION.md`).
4. Request a review (`docs/process/REVIEW.md`).
5. Close the task file and the queue row.
6. Archive the task after the release that contains it.
