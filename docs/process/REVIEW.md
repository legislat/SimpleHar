---
title: Code review method
owner: operator
status: active
audience: [agent, reviewer]
source_of_truth_for: [review]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release, quarterly
---

# REVIEW.md

Up: `docs/DOCS_README.md`. This file is the single home of review method and finding
closure. The gate lives in `docs/process/VERIFICATION.md`; this file never
repeats it.

## When review is required

Review is required for:

- all non-trivial code changes;
- all doctrine and decision changes;
- all release candidates.

Review is not required for:

- typo fixes;
- comment-only changes;
- evidence-only updates.

Do not run ritual reviews on exempt changes.

## Reviewer rules

- The reviewer must be writer-capable and must not be the author.
- The author cannot self-review.
- The reviewer is read-only, except for the reviewer's own findings file.
- The reviewer must not run cargo commands. The gates are already green, and a
  review must not churn `target/`. The task may state otherwise.
- Read-only git is allowed: `status`, `diff`, `log`, `show`, `blame`.
- Run one reviewer per chunk. Two reviewers must not report over the
  same files at the same time.
- The reviewer reads outside its chunk for understanding. It reports only on
  files its chunk owns. A defect elsewhere gets one line under `out of scope`,
  never a finding.
- The reviewer must not read `Cargo.lock` or `target/`.

## Chunking and lenses

- Review a large scope in chunks of a few hundred lines.
- Give each chunk one risk lens and a short list of questions it must answer.
- Order chunks so the ones that can cause the most harm come first. A chunk
  that can cost a model, a process, or a false PASS comes first.
- Each chunk writes its own findings file. No run may overwrite another run's
  file.
- The method examples are `PLAN.md` and `PROMPT.md` in `handoff/code-review/`
  (local-only). The per-round findings files that used to sit beside them moved
  to `handoff/archive/2026-10-27-code-review/`; that archive is a frozen record,
  not current guidance.

## The four traps

This repo was caught in four traps during the review wave. Check for their
cousins in every chunk:

1. An assertion that cannot fail. A check searched product text for a string
   that exists nowhere, so it could never trip. A claim of absence needs the
   grep that shows the absence, and the pattern it used.
2. A test moved to match a bug. A test comment said 403 while its assertion
   asserted the buggy 409. When you read a test, ask what it would do under the
   broken implementation.
3. A verifier that trusts its own log text. A verdict classifier matched the
   product log and read a passing test as blocked. Anything that turns output
   into a verdict is production code and is reviewed as such.
4. A plausible read presented as a fact. A static read predicted an eviction
   tie-break; only a later experiment confirmed it. Label the kind of every
   claim.

## Finding format

Record one row per finding:

| ID | Severity | Where | Claim | Class | Status |
|---|---|---|---|---|---|
| <review-id> | CRITICAL/HIGH/MEDIUM/LOW/INFO | `file:symbol` | One-line falsifiable claim | S/R/C/O | open / fixed:<commit> / accepted:<ruling> |

Each finding also carries:

- Why it matters: the consequence for the operator, not for the style guide.
- Evidence: the code quoted, or the grep that proves an absence.
- Kind: verified, static read, or inference.
- Falsifier note: what would make this finding wrong.
- Minimal fix: the smallest correct change, not a redesign.

## Severity definitions

- CRITICAL: data, machine, or process harm. Also: a gate that can report green
  without the behavior it names.
- HIGH: a wrong product decision.
- MEDIUM: a wrong contract: a code, a status, a persistence target.
- LOW: hygiene.
- INFO: an observation, no action.

Do not pad. A chunk that finds nothing in a category says so.

## Falsifier rule

- A code fix for a finding must add a falsifier test that fails on the pre-fix
  code.
- A fix without a falsifier is a claim, not a closure.

## Closure

- A finding leaves the ledger only by a named commit that fixes it or by a
  named operator ruling that accepts it. Nothing else closes a finding.
- A fixed finding names the fixing commit in its row.
- An accepted finding keeps a permanent row with the ruling rationale and the
  owner. Acceptance does not delete the item.
- Review completion is not finding closure. A reviewed chunk with open findings
  stays open.

## Relationship to tasks

- A task with open findings is not accepted. The Definition of Done in
  `docs/process/TASKS.md` requires it.
- A finding that needs multi-session work gets a task. The task and the finding
  cite each other by ID.
- Open findings live in `handoff/FINDINGS-OPEN.md` (local-only). A fix or a
  ruling moves the finding out of it.

## Evidence retention

- Full review evidence lives under `handoff/evidence/<YYYY-MM-DD>-<slug>/`
  (local-only, backed up).
- Each review record names the scope, the reviewer, the date, the commit under
  review, and the findings file.
- Evidence is immutable once closed.

## Reviewer honesty

- Distinguish a static read from executed proof. Label each finding verified,
  static read, or inference.
- Never claim a check you did not run. Never invent a result.
- A finding must change something the operator cares about. A style opinion is
  not a finding. Report the consequence, not the rule.
- Apply this test to every candidate: what happens if I am right, and what
  happens if I am wrong? If neither answer matters, drop it.
- An empty findings file with an honest "verified clean" note is acceptable and
  better than an invented finding.
- End every findings file with two sections: what was checked and found clean,
  and what could not be checked plus the experiment that would settle each one.

## Fix flow

- A reviewer may propose a fix. The fix is not applied in the review pass.
- Fixes are separate commits, one logical change each, after the operator rules
  on the findings.
- The gates and their evidence rules live in `docs/process/VERIFICATION.md`.
  The reviewer does not re-run the gate.
