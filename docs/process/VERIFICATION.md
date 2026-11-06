---
title: Verification and evidence
owner: operator
status: active
audience: [agent, reviewer, release-operator]
source_of_truth_for: [gate, evidence]
last_reviewed: 2026-11-06
review_trigger: behavior-change, release
---

# VERIFICATION.md

Up: `docs/DOCS_README.md`. This file is the single home of the verification gate and the
evidence rules. No other document lists gate commands. Every other document links here.

## The cardless gate

The cardless gate is exactly these commands, in this order:

```text
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
node tests/webui/webui_dom_test.mjs
node scripts/check-docs.mjs
node scripts/check-instructions.mjs
```

- `cargo test` includes the doc-test.
- A gate line that omits one of these commands is stale.
- The gate must not start an engine and must not load a model.
- The gate runs from any session, including a served session.

`scripts/gate.ps1` is the executable form of this list. The script is authoritative and
this list describes it. A gate change needs an operator-approved change to this file
first, then the same change in the script.

## The card gate

- Card tests and model loads run only in an operator-freed card window.
- A served session must never run them. A harness may serve the session while it uses the
  card. The card is then not free.
- The operator runs the card package. Agents prepare the package and self-test its
  cardless parts only.
- Agents must not execute `-Card` paths.
- A card verdict needs evidence from the freed window. A cardless session records a card
  leg as BLOCKED or NOT RUN, never as PASS.

Manual test builds go to the one test-build directory named in
`docs/process/GIT_WORKFLOW.md` ("Build and release directories"), and you remove the
build when its test concludes, so no test build outlives its evidence. The sweep of the
other build and version directories runs after each build or release. That file owns the
full rule.

## Scheduled checks

These checks are not part of the cardless gate. Run them on the named schedule:

- Release and weekly: `scripts/check-onboarding.ps1` clones the repository to a temp
  directory and verifies the durable onboarding path and its links.
- Cycle start: `scripts/health-report.ps1` prints the read-only process health report.

Record each run under `handoff/evidence/<YYYY-MM-DD>-<slug>/`.

## Evidence requirements

Every gate claim needs dated evidence. Each evidence record names:

- the log path under `handoff/evidence/<YYYY-MM-DD>-<slug>/`,
- the exact command that ran,
- the exit code,
- the counts the command printed.

Rules:

- No claim without a run. Never claim tested, formatted, or lint-free unless the check
  actually ran.
- Report every check that did not run, and say why.
- Name the gate evidence file `gate*.txt` inside its dated evidence directory.
  `scripts/health-report.ps1` reads that name.
- Record a flakiness re-run in the same file or in a separate `gate-rerun*.txt`.
  The health report reads only the final verdict line, `GATE PASS (cardless)` or
  `GATE FAIL (cardless)`. An earlier FAIL line with a final PASS reports PASS.
- Never quote a count that the current run did not print. Counts live in evidence, not in
  durable documents. Durable documents must not embed test counts or DOM leg counts.

## Result vocabulary

| Word | Meaning |
|---|---|
| PASS | The command ran, exited 0, and printed its counts. |
| FAIL | The command ran and exited non-zero, or a check inside it failed. |
| BLOCKED | The environment cannot run the command. Name the blocker. |
| SKIP | The command does not apply to this change. Name the reason. |

- BLOCKED is not PASS. SKIP is not PASS.
- One result per command. Do not merge results into one line.

## Flakiness policy

1. A FAIL stops the line. Fix the cause or record a finding.
2. On suspected flakiness, re-run the failed command in isolation once.
3. Then classify the outcome: PASS with a named transient cause, FAIL with a real cause,
   or BLOCKED with a named environmental cause.
4. Record both runs in the evidence.
5. Do not re-run a suite repeatedly until it turns green.

## Test tiers

| Tier | Home | Runs where |
|---|---|---|
| Unit and integration | `cargo test` | cardless gate |
| Doc-test | inside `cargo test` | cardless gate |
| Web UI DOM stub | the node command above | cardless gate |
| Card suite | operator-run package | operator-freed window only |

The commands print their own counts.

## Definition of verified

A change is verified when all of these hold:

- every cardless gate command has PASS with dated evidence at the tested commit;
- the card legs the change needs have PASS in an operator-freed window, or are named as
  BLOCKED with the window they wait for.

Without that record you may not claim:

- "verified", "green", "tested", "fixed", or "safe";
- a product behavior that no check exercised;
- a card verdict drawn from cardless evidence;
- a pass based on a static code review. A static review is a review, not a run.
