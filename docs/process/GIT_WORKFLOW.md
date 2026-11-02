---
title: Git workflow and release
owner: operator
status: active
audience: [agent, reviewer, release-operator]
source_of_truth_for: [git, release]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release
---

# GIT_WORKFLOW.md

Up: `docs/README.md`. This file is the single home of Git and release practice. The gate
lives in `docs/process/VERIFICATION.md`; this file points to it and never repeats it.
Decision D6 approves the narrative commit style recorded here.

## Repository model

- The model is trunk-based. `main` is the only long-lived branch. `main` must stay
  releasable.
- The authorized main or orchestrator session commits directly to `main`. The project does
  not use pull requests.
- Subagents never commit. The main session commits reviewed work.
- A non-trivial change needs a subagent review before it lands on `main`. The review
  artifact lives in evidence, not in Git. The method lives in `docs/process/REVIEW.md`.

## Branches and worktrees

- Work directly on `main` for serial work.
- A branch is allowed only for parallel work or a risky experiment.
- Name a branch `<type>/<task-id>-<slug>`, for example `feat/TASK-042-engine-basalt`.
- At most two subagents work at the same time. Each owns a disjoint set of files.
- A worktree is valid only when all of these hold:
  - it has a task file (schema: `docs/process/TASKS.md`);
  - the task file names an owner;
  - the durable docs exist inside the tree;
  - the base commit is current with `main`.
- Run that preflight before the first edit in a worktree.
- No worktree may go stale. No worktree may live untouched for more than 7 days.

## Staging

- Run `git status --short` and read the diff before you stage or commit.
- Stage explicit paths. Never use `git add -A` or `git add .`.
- Inspect `git diff --cached` before every commit.
- Never stage secrets, `.env` files, machine-local config, build output, or unrelated
  changes.

## Commit style (D6)

The house style is a narrative subject that states the outcome. It is not an imperative
command.

- The subject states the result of the change, in plain narrative form.
- Prefer 72 characters or fewer. The hard maximum is 100 characters for a normal commit.
- A review-round subject may exceed 100 characters only when the commit carries a body
  and the body names the review round and the reason.
- The body names: the finding IDs the commit closes, what changed, the falsifier test, and
  the gate result.
- One commit is one logical change. One review-scoped batch may cover one review round.
- Never create empty commits or noisy splits.

Subject shape, with a body that lists finding IDs, falsifiers, and the gate line:

```text
Read the strata context figure the engine actually serves

<finding id>: <what changed>. <finding id>: <what changed>.
Falsifiers proven red on the pre-fix code; <gate result> (review <n>).
```

## Protected operations

Each of these requires explicit operator approval before you run it:

- `git reset --hard` and other commands that discard working-tree changes;
- `git commit --amend` and interactive rebase;
- deletion of branches or tags;
- pushes, tags, and release actions.

Force-push and history rewrite are forbidden, always (`AGENTS.md` Section 4). Do not run
them even with approval. There is no exception.

Before an approved destructive step, state the expected impact and verify the target.
Never run these operations to make a tree look clean.

## Review and merge

- Review method and closure live in `docs/process/REVIEW.md`.
- The author cannot review their own work.
- A change with open findings cannot land on `main`.

## Release

A release is a task with acceptance criteria. Run the steps in this order:

1. The gate passes on the release commit (`docs/process/VERIFICATION.md`).
2. Bump the `Cargo.toml` version. The version must match the tag.
3. Write the dated `CHANGELOG.md` entry. Use the Keep a Changelog structure and record
   user-visible changes only.
4. Update the README release claim. It names the tag, never HEAD.
5. Create the annotated tag.
6. Push the commits and the tag. Each push needs operator approval.
7. Build the artifact, record its hash, deploy it, and verify the boot.
8. Keep and name the rollback artifact.

Rules:

- No tag without a changelog entry and a matching version.
- Commits after a release are unreleased. The README release claim moves only with the
  next tag.

## Conflict resolution

- Rebase a local branch onto `main`. Do not merge `main` into a local branch to hide a
  conflict.
- Never force-push shared work.
- If a conflict touches doctrine (`docs/architecture/DOCTRINE.md`), a decision ledger, or
  another ledger, stop and ask the operator.

## Stale worktree cleanup

- Check worktree age at each cycle start and at each release.
- Before you remove a worktree: run `git status` inside it, save any needed diff, and
  record its branch and commit.
- Remove it with `git worktree remove`, then run `git worktree prune`.
- Delete the branch only after you confirm the work is merged or explicitly dropped.
- Never delete a worktree without operator confirmation.
