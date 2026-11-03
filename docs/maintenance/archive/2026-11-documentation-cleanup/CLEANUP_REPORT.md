---
title: Documentation cleanup report
owner: operator
status: maintenance
audience: [operator, architect, agent]
source_of_truth_for: []
last_reviewed: 2026-11-03
review_trigger: none
---

# CLEANUP_REPORT.md

| Field | Value |
|---|---|
| Initiative | 2026-11 documentation cleanup |
| Date | 2026-11-03 |
| Status | Complete; archived at `docs/maintenance/archive/2026-11-documentation-cleanup/` |
| Approval | Operator approved all batches on 2026-11-03. Implementation, review, and fixes were delegated to subagents under the two-worker limit. |
| Base | `main` / `e931de5` (tag `v1.1.0`) |
| Commit | The cleanup commit subject is "Consolidate the documentation and retire the audit layer". No push or tag. |

## 1. Outcome

- The 2026-11-02 audit is no longer a current documentation layer. Its five files moved byte-identically to `docs/maintenance/archive/2026-11-documentation-governance/`.
- The maintenance and audit archive policy is adopted in `docs/maintenance/README.md`.
- The pure redirects `GIT_RULES.md` and `AUX-RULES.md`, and the local pointer stub `handoff/DECISIONS.md`, are deleted.
- The current index, review guidance, agent precedence, design banner, engineering appendix, and README configuration table are updated.
- The decision ledger now states one lifecycle status per dated ruling. Eight labels were corrected to `Implemented` after the independent review.
- The residual 2026-11-02 backlog lives in `handoff/tasks/TASK-DOCS-MAINTENANCE.md` with 8 rows.
- No application source, test, dependency, runtime configuration, engine, model, card, or live-harness state changed.

## 2. Files updated

| File | Change |
|---|---|
| `AGENTS.md` | Added the code-versus-document clarification after the precedence list. The autonomy table and safety fences are unchanged. |
| `DESIGN.md` | Top banner is past tense and ASCII; the design-time `DRAFT` line now records confirmation. Body history is unchanged. |
| `README.md` | Added the `allow_mock_engine` configuration row. It is the only README change. |
| `docs/DOCS_README.md` | Removed the retired redirects and the audit section; added the maintenance policy and archive; removed the empty `ADR/` and `docs/research/` entries; updated the ledger section names; updated the archive rule and review date. |
| `docs/architecture/DECISIONS.md` | Renamed the third section, added the status legend, added a status to every dated ruling, moved `13[10]` to `DELIVERED`, amended the append-only rule for the 2026-11-03 status pass, and qualified the local and external references. |
| `docs/process/ENGINEERING.md` | Removed the conditional blocks for stacks the crate does not use; kept the axum/tower block, the Pico/vanilla rules, and the "only if this stack is present" rule. |
| `docs/process/REVIEW.md` | Replaced the non-existent `handoff/evidence/reviews/` path with the date-slug convention; named `PLAN.md` and `PROMPT.md` as the method examples and labelled the findings archive as frozen. |
| `scripts/check-docs.mjs` | Header comment only: cites `docs/DOCS_README.md` and says maintenance artifacts are not scanned. No behavior change. |
| `docs/maintenance/README.md` | New adopted policy: active and archived initiatives, immutability, non-authority, completion requirements, and role separation. |
| `docs/maintenance/archive/README.md` | New notice: the archive is immutable, non-authoritative, and not default agent context. |

## 3. Files consolidated

- The audit's live maintenance rules were already consolidated into `docs/DOCS_README.md` section 5 by the 2026-11-02 governance work. This cleanup did not create a second rule set; it archived the audit as rationale and kept one current home.
- The documentation index now has one maintenance section instead of a separate `docs/audit/` layer.
- The decision ledger remains the single decision home. No per-decision ADR tree was created.

## 4. Files archived

| Path | Contents | Note |
|---|---|---|
| `docs/maintenance/archive/2026-11-documentation-governance/` | `AUDIT_REPORT.md`, `TARGET_STRUCTURE.md`, `IMPLEMENTATION_PLAN.md`, `PROCESS_IMPROVEMENTS.md`, `DOCUMENTATION_BACKLOG.md` | Byte-identical to the Git HEAD versions. SHA-256-verified by the implementation subagent; blob hashes match by `git rev-parse` and `git hash-object`. |
| `docs/maintenance/archive/2026-11-documentation-cleanup/` | This report plus the inventory, reference audit, and plan | Archived at completion per the adopted policy. |
| `handoff/archive/2026-10-27-code-review/` | 55 historical review-findings files | `PLAN.md` and `PROMPT.md` remain in `handoff/code-review/` as method examples. |
| `handoff/archive/2026-11-evidence/r1r9-verify/` | Gate logs from 2026-10-31 to 2026-11-01 | Superseded by later evidence. |
| `handoff/archive/2026-11-evidence/t-suite-scratch-2026-10-27/` | Scratch logs | Archived, not deleted, to preserve a backup. |
| `handoff/archive/2026-10-28-research/llamastash-2026-10-28.md` | External product comparison | No current decision cites it. |
| `handoff/archive/2026-11-03-research/archive-policy-example.md` | Source policy input | Adopted into `docs/maintenance/README.md`. |

Retained in place because live references still resolve to them:

- `handoff/gate-honesty/` was moved during the first pass, then restored. The test module docs and ten `#[ignore]` strings in `tests/t_gate_honesty.rs` name `handoff/gate-honesty/RUN.md`. Tests are out of scope for a documentation change.
- `handoff/research/reload-slot-drift-2026-10-28.md` was restored. Comments in `src/scheduler/mod.rs` and `tests/scheduler_actor.rs` cite it.
- `handoff/t-suite/` and `handoff/wave-verify/` remain live. They are the current suite definition and the 1.1.0 card evidence.

## 5. Files deleted

| Path | Reason |
|---|---|
| `GIT_RULES.md` | 14-line redirect to `docs/process/GIT_WORKFLOW.md`; no unique rule. |
| `AUX-RULES.md` | 21-line redirect to the process documents; no unique rule. |
| `handoff/DECISIONS.md` | 4-line local pointer stub; the tracked ledger is the single home. |

## 6. References updated

- `docs/DOCS_README.md`: the planned-path sentence no longer cites `docs/audit/TARGET_STRUCTURE.md`; the root table no longer lists the deleted redirects; the ledger row names `LOCKED / DEFERRED / DELIVERED / DATED RULINGS`; the archive rule names `docs/archive/` and `docs/maintenance/archive/`.
- `docs/architecture/DECISIONS.md`: `HANDOFF queue` now points to `handoff/PENDING_CHECKLIST.md`; the two `research/...` citations are qualified as `handoff/research/...` local evidence; the external `D:\WORKSPACE-VS\...` path is labelled operator-machine only; the old `handoff/DECISIONS.md` statement is past tense.
- `docs/process/REVIEW.md`: the evidence path is now `handoff/evidence/<YYYY-MM-DD>-<slug>/`; the code-review pointer names the two kept method files and labels the findings archive as frozen.
- `handoff/code-review/PLAN.md`: the subagent rule now cites `AGENTS.md` section 4 instead of the deleted `AUX-RULES.md`.
- `scripts/check-docs.mjs`: the header comment cites the current index and names maintenance artifacts as not scanned.
- `README.md`: the configuration table now documents `allow_mock_engine`. This closes the documentation gap in open finding R2-11-07; the live finding register is updated after the commit.

## 7. Decision status changes

- The section formerly titled `ARCHIVED (tombstones: ruling / date / commit)` is now `DATED RULINGS (binding until superseded; status is explicit)`.
- The ledger added the statuses `Accepted`, `Implemented`, `Superseded`, `Rejected`, and `Closed`; the old heading is recorded in the section note so in-text `ARCHIVED` references still resolve.
- 52 entries carry a status: 32 `Implemented`, 10 `Accepted`, 8 `Closed`, 2 `Rejected`, and 0 `Superseded`.
- `13[10]` moved from `DEFERRED` to a new `DELIVERED` section. The pre-existing dated delivery line remains for detail; the section note explains the relationship.
- Eight labels were corrected to `Implemented` after the independent review: Section 20-9 engine schema, Ninfer published-port option (b), release-then-resample, R5, row 10 launch-floor guard, R2-02-02, release 1.1.0, and import `--apply`.
- `N7` remains `Accepted`. The dependency narrowing landed, but duplicate `base64` and `syn` versions remain and the dedupe clause is conditional. `TASK-N7-DEPS` owns the close.
- `DOCTRINE.md` is untouched. No binding card changed.

## 8. Validation results

| Check | Result |
|---|---|
| `node scripts/check-docs.mjs` | PASS (15 durable docs checked) |
| `node scripts/check-instructions.mjs` | PASS |
| `node scripts/check-tasks.mjs` | PASS (5 task files checked) |
| `scripts/release-check.ps1` | RELEASE CHECK PASS (version 1.1.0, tag v1.1.0, changelog and README agree) |
| Audit file identity | All five blob hashes match the Git HEAD versions |
| Relative-link scan | 0 broken links in the current Markdown set (151 files, 32 relative links) |
| Source/config safety | No `src/`, `tests/`, `Cargo.toml`, `Cargo.lock`, or runtime configuration change |
| Git state | Only the intended documentation changes and the untracked maintenance reports; no commit before the report |

Checkers were run cardless. No cargo command, engine start, model load, card test, or live-harness action was performed.

## 9. Remaining issues and manual review

1. `N7` status remains `Accepted` until the dependency dedupe closes. The review verified that the narrowing landed and the conditional duplicates remain.
2. `handoff/gate-honesty/` and `handoff/research/reload-slot-drift-2026-10-28.md` stay live because code and tests reference them. Do not archive them until those references are updated in a separate approved code change.
3. `handoff/t-suite/` stays live. It is the current suite definition and the 2026-11-02 run evidence.
4. `DESIGN.md` body still contains historical non-ASCII typography. The title and banner are repaired; the body is historical and is not scanned. A full encoding pass needs a separate review.
5. `DOCUMENTATION_INVENTORY.md`, `LEGACY_REFERENCE_AUDIT.md`, and `CLEANUP_PLAN.md` describe the pre-execution tree. This report is the final state. Read them as dated audit records, not as current status.
6. The live layer (`HANDOFF.md`, `handoff/PENDING_CHECKLIST.md`, `handoff/FINDINGS-OPEN.md`) is updated after the commit with the commit hash and the new task row. It is local-only and is not part of the commit.
7. `R2-11-07` (README omits `allow_mock_engine`) is closed by this commit. The live findings register records the closure after the commit.
8. The `docs/reference/` split remains deferred. The README is 549 lines; the candidate sections are below the agreed ~150-line trigger.
9. `docs/archive/` remains reserved and is not created. No superseded durable project document exists after this cleanup.
10. The notices under `archive/`, `base-archive/`, and `handoff/archive/` are local-only and are not committed. They are visible to agents on this machine.
11. `handoff/tasks/TASK-DOCS-MAINTENANCE.md` is local-only. It carries the 8 residual backlog rows.
12. `handoff/code-review/PLAN.md` was corrected locally. It is not tracked.

## 10. Deviations from the plan

| Plan item | Deviation | Reason |
|---|---|---|
| E-06 delete `archive/t-suite-scratch-2026-10-27/` | Moved to `handoff/archive/2026-11-evidence/` instead | The plan's own safety rule forbids deleting evidence without a backup. |
| E-03 move `handoff/gate-honesty/` | Moved, then restored | Live test module docs and `#[ignore]` strings still name the path. Tests are out of scope. |
| E-04 archive `reload-slot-drift-2026-10-28.md` | Restored to `handoff/research/` | Live source and test comments cite it. |
| E-03 move older `handoff/t-suite/logs/` | Kept in place | The live suite definition and the current run evidence are one unit. |
| C-05 trim unused stacks | Executed | The conditional appendix now names only stacks present in `Cargo.toml`. |
| C-06 document `allow_mock_engine` | Executed | The README row matches `src/config/mod.rs` and `src/engine/mod.rs`. |
| Cleanup reports stay active | Archived at completion | The adopted policy requires completed initiatives to move to `docs/maintenance/archive/`. |

## 11. Residual backlog

`handoff/tasks/TASK-DOCS-MAINTENANCE.md` carries these rows:

| Row | Subject | Status |
|---|---|---|
| BL-15 | Split README reference sections | Deferred by its own ~150-line threshold |
| BL-16 | Archive paths and retention | Completed by this cleanup; close after the notices are verified |
| BL-23 | Consolidate technical debt | Open; needs one debt-index home |
| BL-24 | Onboarding and subagent brief template | Partly done; template still owed |
| BL-30 | Fresh-clone onboarding check | Open |
| BL-31 | Process health report | Open; P3 |
| BL-32 | Backup and recovery policy | Open; needs operator decision and a restore test |
| BL-35 | Stale-phrase and `cargo doc` checks | Open; P3 |

## 12. Commit and close

- The cleanup commit contains the tracked documentation changes and the archived maintenance reports.
- It does not contain local evidence, local task files, archive notices, or the live ledger updates.
- No push, tag, or release was performed.
- The initiative is complete. Per the adopted policy, this directory is now archival and must not be edited to match later architecture.
