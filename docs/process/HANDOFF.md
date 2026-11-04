---
title: Handoff protocol
owner: operator
status: active
audience: [agent, operator]
source_of_truth_for: [handoff protocol]
last_reviewed: 2026-11-03
review_trigger: behavior-change, release, quarterly
---

# HANDOFF.md

Up: `docs/DOCS_README.md`. This file is the single home of the handoff protocol. It
contains the protocol only. It holds no live state.

The root `HANDOFF.md` is the live snapshot. It stays local-only and gitignored.
`docs/process/TASKS.md` owns the task lifecycle and the task file template. The
gate lives in `docs/process/VERIFICATION.md`. This file points to them and does
not repeat them.

## Triggers

Run the handoff checkpoint at each of these points:

1. At session start. Run the read protocol below.
2. Before context compaction.
3. Before a long-running operation.
4. Before switching agents or models.
5. Before ending a session, clean or dirty.
6. Before a card window, and before any operation the next session cannot
   repeat.

## Artifact

- One task file per active task: `handoff/tasks/<TASK-ID>.md`. The schema and
  template live in `docs/process/TASKS.md`. The queue row links to the task
  file.
- The root `HANDOFF.md` holds the snapshot, machine safety facts, and pointers.
  It carries no item detail. Its budget is 120 lines.

## Required fields

The task file progress state must carry these fields. `docs/process/TASKS.md` owns their definitions:

- status
- done
- remaining
- failed approaches
- decisions
- blockers
- files changed
- evidence
- constraints
- next step (the exact command)

Write "none" in an empty field. Do not delete the field.

## Dirty-tree protocol

When work is uncommitted:

- Record the exact files and why each one is uncommitted.
- Record what compiled and what ran last.
- Record the next action.
- Do not commit just to make the tree clean. Commit only when the task
  authorizes a checkpoint commit.
- Never let another session edit the same files without an explicit handoff.

## Read protocol

The next session reads in this order:

1. `HANDOFF.md`
2. its queue row in `handoff/PENDING_CHECKLIST.md`
3. the task file
4. only task-specific documents from the reading matrix in `AGENTS.md`

Reconcile `git status` against the task file before the first edit.

### Resume checklist

- [ ] `git status --short` matches the "Files changed" record in the task file.
- [ ] The task file names this session as owner.
- [ ] The dirty-tree record matches the tree.
- [ ] The failed approaches list was read. Those approaches will not repeat.
- [ ] The next command is known.

## Conflict rule

- If the task file and `git status` disagree, stop. Reconcile before you write.
- If two sessions are active, the task file names the owner. The non-owner must
  not edit.
- If the disagreement cannot be reconciled, record a blocker in the task file
  and ask the operator.

## Ownership

Ownership and subagent limits follow `AGENTS.md` Section 4. One active task per session.
The owning session writes the queue row and the snapshot. Other sessions write only their
own task file.

## Backup and restore

The live layer is local-only. Back it up to a separate physical drive or network share. Do
not use the same physical device as the working tree.

- Back up `HANDOFF.md`, `handoff/`, `ModelConf/`, and the active config file
  (`simpleharn.json`, or the legacy name it loaded).
- Exclude `target/`, `logs/`, build artifacts, and the frozen `archive/` and
  `base-archive/` trees. The frozen trees are backed up once or preserved in Git history.
- Cadence: at each handoff checkpoint: before ending a session, before a card window, and
  before a release.
- Retention: keep the 14 newest dated snapshots. At each checkpoint, prune older snapshots
  only after the new backup succeeds.
- Restore test: quarterly. Restore the newest retained snapshot to a temp directory. Run
  `scripts/check-docs.mjs`, `scripts/check-instructions.mjs`, and `scripts/check-tasks.mjs`
  on the restored tree. Verify `HANDOFF.md` and every task file have the required fields.
  The test verifies the retained set. Record the evidence under
  `handoff/evidence/<YYYY-MM-DD>-restore-test/`. Do not start an engine during the test.

The live `HANDOFF.md` snapshot names the current backup medium and the latest backup path.
The policy is the rule; the snapshot is the current fact.

## Snapshot template

The root `HANDOFF.md` uses this shape. Budget: 120 lines.

```markdown
# Handoff

## 1. What this project is
Three lines plus a link to README.md.

## 2. Current snapshot
Branch, HEAD, unpushed list, tree state, active task ID, gate status link,
deployed artifact.

## 3. Machine safety
Live services, protected ports, card rules, shell and toolchain facts.

## 4. Next action
One task ID plus a link. No detail.

## 5. Where things live
Short routing table.

## 6. Session-start chain
AGENTS.md, HANDOFF.md, queue, task file, DOCTRINE.md, GOTCHAS.md as needed.
```

## Handoff types

One artifact serves session to session, agent to agent, human to agent, and
agent to human. For a human reader, add a short "start here" line at the top.

## Never copy

- Do not copy this protocol into a subagent brief. The brief links to this file.
- Do not put HEAD, branch state, the active task, or blockers in this protocol document.
  That is live state and belongs in the snapshot.
- Do not copy gate commands here. `docs/process/VERIFICATION.md` owns them.
