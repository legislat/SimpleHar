---
title: Task lifecycle and schema
owner: operator
status: active
audience: [agent, operator]
source_of_truth_for: [task lifecycle, task schema]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release
---

# TASKS.md

Up: `docs/README.md`. This file is the single home of the task lifecycle and schema. The
gate lives in `docs/process/VERIFICATION.md`. The review method lives in
`docs/process/REVIEW.md`. This file points to both and never repeats them.

## Lifecycle

```text
proposed -> ready -> in-progress -> review -> verification -> accepted -> documented -> archived
                      |                |
                      +-> blocked      +-> changes-requested
```

- proposed: the idea exists; scope or a needed decision is missing.
- ready: scope, acceptance criteria, dependencies, and an owner exist.
- in-progress: work runs. One active task per session.
- review: code is complete and waits for review.
- verification: fixes are applied; gate and acceptance evidence are collected.
- accepted: acceptance criteria are met, the gate passed, and docs are updated.
- documented: changelog, decision, and README entries are done where needed.
- archived: the task file moved to `handoff/archive/<date>/`.
- blocked: a named outside cause stops progress. Name the cause and its owner.
- changes-requested: review asked for fixes. Work returns to in-progress.

## Classes, priorities, readiness

The class is an execution constraint:

| Class | Meaning |
|---|---|
| S | cardless, decidable in-tree |
| R | needs a build plus a cardless run |
| C | needs an operator-freed card window |
| O | needs an operator ruling |

The priority is value:

- P0: release blocker or security problem.
- P1: current cycle.
- P2: next cycle.
- P3: backlog.

Do not use the class as a priority.

Readiness states:

| Readiness | Meaning |
|---|---|
| ready | the task may start |
| blocked | a named external cause stops it |
| needs-design | a design decision is missing |
| needs-operator | an operator ruling is missing |

## Task file

Create `handoff/tasks/<TASK-ID>.md` for any task that spans more than one session or needs
card work. Budget: 150 lines. Use this template:

```markdown
# TASK-<ID> - <short title>

| Field | Value |
|---|---|
| Class | S / R / C / O |
| Priority | P0 / P1 / P2 / P3 |
| Readiness | ready / blocked / needs-design / needs-operator |
| Owner | operator / agent-session-id |
| Created | YYYY-MM-DD |
| Last updated | YYYY-MM-DD |
| Branch / commit | ... |

## Goal

One paragraph. What changes for the user or system.

## Scope

Files/modules in scope.

## Non-goals

What this task explicitly does not do.

## Acceptance criteria

- [ ] Falsifiable criterion 1
- [ ] Falsifiable criterion 2
- [ ] Documentation updated
- [ ] Gate passed (link to evidence)

## Dependencies

- Blocks / blocked by:
- Required environment (cardless, card, operator ruling):

## Decisions

Links to DOCTRINE / DECISIONS / pending ruling. Do not copy text.

## Progress

### Done
### Remaining
### Failed approaches
### Blockers

## Files changed

List or diff reference.

## Evidence

Links to `handoff/evidence/...` and gate output.

## Next step

The exact command or action the next session should take first.
```

Template rules:

- Acceptance criteria must be falsifiable. A reviewer can disprove each one.
- Every code fix needs a falsifier test that fails on the pre-fix code.
- The Decisions section holds links only. Never copy ruling text.
- Write "none" in an empty field. Do not delete the field.
- The Next step names one exact first action for the next session.

## Queue row

The queue index lives in `handoff/PENDING_CHECKLIST.md`. One row per task, one line:

```markdown
- [ ] TASK-<ID> - P<N> - [<class>] - <readiness> - <one-line goal> - owner: <who> - file: tasks/<ID>.md
```

The queue carries no commit hashes, no test counts, and no narratives. Those live in the
task file or in evidence.

## Definition of Ready

A task may start only when all of these hold:

- the goal fits one paragraph;
- scope and non-goals are named;
- every acceptance criterion is falsifiable;
- dependencies are resolved or named;
- the execution class is known;
- a decision exists for every open design fork.

Otherwise set readiness to needs-design or needs-operator.

## Definition of Done

A task is done only when all of these hold:

- every acceptance criterion is checked;
- the cardless gate passed with dated evidence (`docs/process/VERIFICATION.md`);
- the card legs the task needs passed in a freed window, or are named as blocked;
- affected docs were updated in the same change, or a note says why none is affected;
- review findings are closed by named commits or accepted by named rulings
  (`docs/process/REVIEW.md`);
- the task file and the queue row are current;
- evidence is linked;
- no unrelated change remains uncommitted.

## Findings and tasks

- A review finding lives in `handoff/FINDINGS-OPEN.md`, one row per finding.
- A finding that needs multi-session work gets a task. The task and the finding cite each
  other by ID.
- A finding closes by a named commit or a named operator ruling. Nothing else closes it.
- An accepted risk needs a named ruling and a permanent row. Acceptance does not delete
  the item.
- A finding that one session fixes and reviews does not need its own task.

## Archive

- A closed task stays in place until the release that contains it ships.
- After that release, move the task file to `handoff/archive/<YYYY-MM-DD>/`.
- Evidence is retained beside its task. Compression needs operator approval.

## Working rules

- One active task per session.
- At most two working subagents at the same time, with disjoint file ownership.
- A subagent never edits a shared ledger and never commits.
- The owning session writes the queue row and the handoff snapshot. Other sessions write
  only their own task file.
