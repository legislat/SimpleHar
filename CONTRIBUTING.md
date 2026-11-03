---
title: Contributor pointer
owner: operator
status: active
audience: [contributor]
source_of_truth_for: []
last_reviewed: 2026-11-02
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

## First task path

1. Read `README.md` and `docs/process/TASKS.md`.
2. Create a task file under `handoff/tasks/`.
3. Implement inside the task scope and run the gate
   (`docs/process/VERIFICATION.md`).
4. Request a review (`docs/process/REVIEW.md`).
5. Close the task file and the queue row.
6. Archive the task after the release that contains it.
