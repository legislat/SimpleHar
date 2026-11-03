---
title: Documentation cleanup plan
owner: operator
status: maintenance
audience: [operator, architect, agent]
source_of_truth_for: []
last_reviewed: 2026-11-03
review_trigger: none
---

# CLEANUP_PLAN.md

| Field | Value |
|---|---|
| Initiative | 2026-11 documentation cleanup |
| Date | 2026-11-03 |
| Repository | `C:\Inference\SimpleHar-SE` |
| Branch / HEAD | `main` / `e931de5` |
| Status | **Approved 2026-11-03; executed 2026-11-03. The final state is in `CLEANUP_REPORT.md`. The reports are archived under `docs/maintenance/archive/2026-11-documentation-cleanup/`.** |
| Inputs | `DOCUMENTATION_INVENTORY.md`, `LEGACY_REFERENCE_AUDIT.md` |
| Constraint | Documentation only. No application source, runtime configuration, branch, commit, push, tag, engine, model, card, or live-harness change. |

## 1. Decision requested

Approved 2026-11-03. The execution record is `CLEANUP_REPORT.md`.

The original request: approve or reject the batches below. Approval could be given per batch. The recommended first approval was Batches A, B, C-01, F, and G; Batch D needed a separate doctrine ruling; Batch E needed a separate local-evidence decision.

The plan preserves every current ruling and every historical audit. It removes duplication, retires pure redirects, separates history from current guidance, and leaves a tracked backlog for work that is still open.

## 2. Target structure

```text
docs/
  DOCS_README.md                         current index and maintenance rules
  process/                               7 current process documents (unchanged)
  architecture/                          3 current architecture documents
  maintenance/
    README.md                            NEW: maintenance and audit archive policy
    2026-11-documentation-cleanup/
      DOCUMENTATION_INVENTORY.md         created
      LEGACY_REFERENCE_AUDIT.md          created
      CLEANUP_PLAN.md                    created
      CLEANUP_REPORT.md                  created after execution
    archive/
      README.md                          NEW: archive notice, non-authoritative
      2026-11-documentation-governance/
        AUDIT_REPORT.md                  moved from docs/audit/
        TARGET_STRUCTURE.md              moved from docs/audit/
        IMPLEMENTATION_PLAN.md           moved from docs/audit/
        PROCESS_IMPROVEMENTS.md          moved from docs/audit/
        DOCUMENTATION_BACKLOG.md         moved from docs/audit/
```

This is the pre-execution layout. At completion, this initiative moved to `docs/maintenance/archive/2026-11-documentation-cleanup/` per the adopted policy. See `CLEANUP_REPORT.md` for the final state.

Local archives keep their own roles:

| Location | Role | Authority |
|---|---|---|
| `handoff/archive/` | Closed tasks, old queues, superseded live state | None; frozen historical record |
| `archive/` | Verbatim revisions of superseded governing documents | None; frozen historical record |
| `base-archive/` | v3.3.0 baseline product and source | None; reference only, never built |
| `docs/maintenance/archive/` | Completed documentation and process initiatives | None; immutable historical record |

`docs/archive/` is reserved for a superseded **durable** project document. It is not created now because no such document exists after this plan. The rule is recorded in `docs/DOCS_README.md` so the next supersession has one home.

### 2.1 Adoption of the maintenance and audit archive rules

The policy input is `handoff/research/archive-policy-example.md`. This plan adopts it with one structural adaptation: an initiative with more than one deliverable gets its own `YYYY-MM-<slug>/` directory, so the four reports of this cleanup stay together. A single small initiative may sit directly under `docs/maintenance/`.

| Input rule | Adoption in this repository |
|---|---|
| Archive completed audits and maintenance initiatives as immutable records | A completed initiative moves to `docs/maintenance/archive/<YYYY-MM>-<slug>/` and is never edited |
| Preserve original findings, recommendations, and approved plans | The five 2026-11-02 audit files move byte-identical |
| Add a final execution report | `CLEANUP_REPORT.md` is part of this initiative |
| Do not keep obsolete working drafts unless they have value | The plan keeps only the three reports and the final report; no scratch drafts are added |
| Do not reference archived reports as active instructions | `docs/DOCS_README.md` will point to the archive as history only |
| Do not load archived documents into default agent context | The agent reading matrix and `AGENTS.md` do not name the archive |
| Do not treat historical recommendations as requirements | The residual backlog is extracted into a live task; the archived recommendations are not requirements |
| Keep links only for historical traceability | The archive is reachable from the maintenance README, not from current process documents |
| Do not modify archived reports to match the current architecture | The audit content is frozen; this cleanup writes a new report instead |
| Confirm the implementation or review is complete before archiving | The 2026-11-02 governance implementation is complete; its 8 residual rows are extracted first |
| Record unresolved issues and deferred recommendations | `CLEANUP_REPORT.md` and `TASK-DOCS-MAINTENANCE.md` carry them |
| Verify that no active document depends on temporary audit artifacts | Batch A updates the index and the checker comment; Batch G verifies the result |
| Archived maintenance documents do not override current instructions, architecture, or guidelines | Stated in `docs/maintenance/README.md` and the archive notice |

This adaptation prevents a fourth overlapping archive system. The roles stay separate: `docs/maintenance/archive/` for documentation initiatives, `docs/archive/` for superseded durable project documents, `handoff/archive/` for closed live state, and `archive/` plus `base-archive/` for frozen legacy trees.

## 3. Priority summary

| Priority | Actions | Purpose |
|---|---|---|
| P1 | A-01..A-05, B-01..B-04, C-01, D-01..D-06 (D-06 only if required), F-01..F-02, G-01..G-04 | Remove the historical audit as a current layer, retire pure redirects, fix broken current references, make decision statuses honest, preserve residual work |
| P2 | C-02..C-04, C-06, E-01..E-04, E-06, E-07 | Improve current-doc accuracy, add archive notices, reduce local evidence noise |
| P3 | C-05, E-05, E-08 | Optional context reduction and trap pruning |

## 4. Batch A - Adopt the maintenance archive and retire the audit layer

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| A-01 | `docs/maintenance/README.md` (new) | New durable policy | Add | Adopt the maintenance and audit archive rules. Define active versus archived initiatives, immutability, non-authority, and the completion requirements. | None | Low | Operator approval to add a durable doc | None; new text only |
| A-02 | `docs/audit/` -> `docs/maintenance/archive/2026-11-documentation-governance/` | C | Move with `git mv` | The 2026-11-02 audit is a completed initiative. It is 45.6 % of durable lines and 45.4 % of durable bytes. Its status rows are stale. | A-01 | Low | Operator approval for the move | None; content moves byte-identical and Git history follows |
| A-03 | `docs/DOCS_README.md` | A | Update | Replace the `docs/audit/` section with `docs/maintenance/`; remove `ADR/` and `docs/research/` empty rows; keep `docs/reference/` as threshold-gated; fix the planned-path sentence; add the archive rule; update `last_reviewed`. | A-01, A-02 | Medium | Operator approval | Routing error if incomplete; mitigated by `check-docs` |
| A-04 | `scripts/check-docs.mjs` | A (tooling) | Update comment only | Its header cites `docs/audit/TARGET_STRUCTURE.md` and exempts `docs/audit/*`. Both become false. | A-02 | Very low | Operator approval for a script comment edit | None; no behavior change |
| A-05 | `docs/maintenance/archive/README.md` (new) | C | Add | State that the directory is immutable, non-authoritative, and not default agent context. | A-01 | Low | Operator approval | None; notice only |

Acceptance criteria:

- `docs/audit/` no longer exists as a current layer.
- The five audit files are byte-identical in the archive.
- `docs/DOCS_README.md` lists every durable document and no empty planned layer except the threshold-gated `docs/reference/`.
- `node scripts/check-docs.mjs` and `node scripts/check-instructions.mjs` pass.
- No current document cites `docs/audit/TARGET_STRUCTURE.md` as authority.

## 5. Batch B - Retire superseded redirects and stubs

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| B-01 | `GIT_RULES.md` | D | Delete | 14-line redirect; the target is current and complete; no inbound link except the index. | A-03 | Low | Operator approval to delete | None; Git history keeps the file |
| B-02 | `AUX-RULES.md` | D | Delete | 21-line redirect; all rules live in the process documents. | A-03 | Low | Operator approval to delete | None; Git history keeps the file |
| B-03 | `handoff/DECISIONS.md` | D | Delete | 4-line pointer stub; the tracked ledger is the single home; it cites an audit that moves. | A-02 | Low | Operator approval to delete | None; no rulings are in the stub |
| B-04 | `docs/DOCS_README.md` root table | A | Update | Remove the two deleted rows. | B-01, B-02 | Low | Covered by A-03 approval | Index defect if missed |

Acceptance criteria:

- `Select-String` over the repository finds no reference to `GIT_RULES.md`, `AUX-RULES.md`, or `handoff/DECISIONS.md` except in historical archives.
- `check-docs` and `check-instructions` pass.
- `docs/architecture/DECISIONS.md` still states that `handoff/DECISIONS.md` is safe to delete.

## 6. Batch C - Current document accuracy

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| C-01 | `docs/process/REVIEW.md:131` | A | Update | `handoff/evidence/reviews/` does not exist. Use `handoff/evidence/<YYYY-MM-DD>-<slug>/`, the convention in `VERIFICATION.md` and the live tree. | None | Low | Operator approval | None; the path becomes true |
| C-02 | `docs/process/REVIEW.md:57` | A | Update | Keep the method examples but point to `PLAN.md` and `PROMPT.md` only if the findings files are archived. | E-02 | Low | Covered by E-02 approval | None; evidence stays |
| C-03 | `AGENTS.md:45-69` | A | Update | Clarify that code and tests are behavioral truth and that a code-doc contradiction is a defect to fix, not a license to follow the document. Removes the apparent conflict with precedence item 9. | Operator decision | Low | Operator approval; this is the constitution | Wording only; no rule change intended |
| C-04 | `DESIGN.md:13-33` | B | Update | Change "will live" to "lives"; mark the design-time `DRAFT` line as design-time; optionally fix the mojibake title. | Operator decision on keeping `DESIGN.md` at the root | Low | Operator approval | None; historical content is not rewritten |
| C-05 | `docs/process/ENGINEERING.md:172-222` | A | Update (optional) | Trim conditional blocks for stacks the crate does not use (`polars`, `ratatui`, `dioxus`, PyO3). Keeps the "only if present" rule for future stacks. | Operator decision | Low | Operator approval | Conditional guidance only; archive the removed block if zero loss is required |
| C-06 | `README.md` config table | A | Update (optional) | Document `allow_mock_engine` (default false; test-only grant). This closes open finding R2-11-07. | Operator approval; verify the exact semantics in `src/config/mod.rs` and `src/main.rs` | Low | Operator approval | None; additive text |

Acceptance criteria:

- `REVIEW.md` names a path that exists.
- `AGENTS.md` precedence and conflict rules cannot be read as contradictory.
- `DESIGN.md` reads as finished history, not as an unfinished plan.
- No removed stack is presented as a current mandate.
- If C-06 is approved, the README documents the key and its default, and the finding owner can close R2-11-07.

## 7. Batch D - Decision ledger status pass

This batch edits the binding ledger. It requires a separate operator ruling and a second-agent review.

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| D-01 | `docs/architecture/DECISIONS.md:19` | A | Amend the append-only rule | A one-time status annotation pass must be allowed. The amendment must state that only labels, section titles, and reference qualifiers may change; ruling text may not. | Operator doctrine ruling | High | Explicit operator ruling | A too-broad amendment could allow silent history edits. Constrain it in the ruling. |
| D-02 | `docs/architecture/DECISIONS.md:140` | A | Rename section and add a status legend | "ARCHIVED (tombstones)" holds 51 bullets, many of them active. A reader can mistake binding rules for dead history. | D-01 | Medium | Operator approval | Historical wording of the heading changes; record the old heading in the legend |
| D-03 | `docs/architecture/DECISIONS.md`, third section | A | Add one status word per entry: Accepted, Implemented, Superseded, Rejected, Closed | Makes current applicability explicit without rewriting the ruling. | D-01, D-02 | Medium | Operator approval plus second-agent review | A wrong label can mislead. Every label needs a cited reason (later ruling, commit, or removal). |
| D-04 | `docs/architecture/DECISIONS.md:133-134` | A | Move `13[10]` out of DEFERRED or mark it delivered | A delivered item currently reads as open. | D-01 | Low | Operator approval | None; the delivery line stays in the ledger |
| D-05 | `docs/architecture/DECISIONS.md:55,101,177,331` | A | Fix references | Point `HANDOFF queue` to `handoff/PENDING_CHECKLIST.md`; qualify `research/` as `handoff/research/` local evidence; label the external `D:\WORKSPACE-VS` path as operator-machine only. | D-01 | Low | Operator approval | None; meaning is unchanged |
| D-06 | `docs/architecture/DOCTRINE.md` | A | Update only if a LOCKED rule is reclassified | The card must match the ledger. | D-03 | High if touched | Explicit operator approval | A wrong card change alters binding doctrine; do not touch unless required |

Acceptance criteria:

- Every entry in the third section has one explicit lifecycle status.
- No ruling text changes meaning. A diff review names every changed line.
- `13[10]` is not presented as open.
- `DOCTRINE.md` still matches the LOCKED rules.
- A second agent reviews the diff against this plan before the change lands.

Alternative considered: split the third section into two physical sections, "Active dated rulings" and "Closed tombstones", by moving blocks. This is rejected as the default because it breaks the append-only layout and makes the history harder to audit. The operator may choose it instead; the acceptance criteria are the same. The recommended approach keeps every entry in place and changes only its status label.

## 8. Batch E - Local archive and evidence hygiene

These actions are local-only and do not change tracked durable docs, except E-02's pointer update.

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| E-01 | `archive/README.md`, `base-archive/ARCHIVE-NOTICE.md`, `handoff/archive/README.md` (new) | C | Add notices | Frozen trees must state that they are non-authoritative. Old handoffs carry obsolete start chains. | None | Low | Operator approval | None; notices only |
| E-02 | `handoff/code-review/` | C | Move findings files to `handoff/archive/2026-10-27-code-review/`; keep `PLAN.md` and `PROMPT.md` | 55 historical findings files dominate the local review directory. The method examples remain. | C-02 | Low | Operator approval | Evidence is preserved; only the location changes. No deletion. |
| E-03 | `handoff/gate-honesty/`, `handoff/r1r9-verify/`, older `handoff/t-suite/logs/` | C | Move to `handoff/archive/2026-11-evidence/` | Superseded evidence. Keep `handoff/wave-verify/`, `handoff/evidence/2026-11-03-r2/`, and the current suite definition. | Operator approval | Medium | Operator approval | Possible loss of traceability if a current task cites an old log. Check references first. |
| E-04 | `handoff/research/` | B/C | Archive unreferenced reports; keep the two cited by decisions | The policy example is adopted; `llamastash` and `reload-slot-drift` are not cited. | D-05 | Low | Operator approval | Keep the two cited reports; archive, do not delete, the others |
| E-05 | `handoff/GOTCHAS.md` | A/F | Prune stale traps after operator review | Some traps name removed paths and the retired 9b runner. Traps are hard-won, so deletion needs a human. | Operator review | Medium | Operator approval per trap | A removed trap can cost a future session. Archive a copy before pruning. |
| E-06 | `archive/t-suite-scratch-2026-10-27/` | E | Delete | Scratch logs with no continuing value. | Operator approval | Low | Operator approval | None; scratch output only |
| E-07 | `DESIGN.md` | B | Fix mojibake | The only durable doc with non-ASCII corruption. | C-04 | Low | Operator approval | Text-only encoding repair; keep a diff review |
| E-08 | `handoff/research/*` | C | Fix mojibake only if the file is promoted or edited | Local research is not durable; cosmetic repair is optional. | E-04 | Low | Operator approval | None |

Acceptance criteria:

- Every archive directory has a README that states its role and non-authority.
- No current task or document references a moved evidence path.
- The current release evidence and current suite definition remain in place.
- No evidence is deleted without a named reason and a backup.

## 9. Batch F - Carry-forward work

The 2026-11-02 backlog has 36 rows. 28 are delivered, 3 are partly delivered, 1 is deferred by its own threshold, and 4 remain open. The 8 non-delivered rows are carried below. Archiving the backlog without extraction would lose the open work.

| ID | Path | Class | Action | Reason | Dependencies | Risk | Approval | Information loss if wrong |
|---|---|---|---|---|---|---|---|---|
| F-01 | `handoff/tasks/TASK-DOCS-MAINTENANCE.md` (new) and one queue row | A (live) | Add | Carry the residual rows into the existing task system instead of a new backlog layer. | A-02 | Low | Operator approval | None; it preserves work |
| F-02 | `HANDOFF.md`, `handoff/PENDING_CHECKLIST.md` | A (live) | Update | Record this cleanup task, its next action, and the residual task. | F-01 | Low | Operator approval | None; live state only |

Residual rows to carry:

| Source row | Subject | Proposed status |
|---|---|---|
| BL-15 | Split README reference sections | Deferred by the agreed ~150-line threshold; no split now |
| BL-16 | Archive paths and retention | Partly done; E-01 completes the notices |
| BL-23 | Consolidate technical debt | Open; needs a single index decision |
| BL-24 | Onboarding and subagent brief template | Partly done; `TARGET_STRUCTURE.md` section 7.5 is archived, so a live template is missing |
| BL-30 | Fresh-clone onboarding check | Open |
| BL-31 | Process health report | Open; P3 |
| BL-32 | Backup and recovery policy | Open; the backup exists, the policy and restore test do not |
| BL-35 | Stale-phrase and `cargo doc` checks | Open; P3 |

## 10. Batch G - Validation and final report

| ID | Action | Acceptance |
|---|---|---|
| G-01 | Run `node scripts/check-docs.mjs`, `node scripts/check-instructions.mjs`, `node scripts/check-tasks.mjs`, `scripts/release-check.ps1` | All pass. Record the output under `handoff/evidence/2026-11-03-docs-cleanup/` |
| G-02 | Re-run the link scan and the retired-term grep | No broken project link; no current reference to `docs/audit`, `GIT_RULES.md`, `AUX-RULES.md`, `handoff/DECISIONS.md`, `research/` (bare), `HANDOFF queue`, or `handoff/evidence/reviews/` |
| G-03 | Spot-check current docs against code | README strata/ninfer sections, OVERVIEW module map, DOCTRINE items, and the stack table still match `src/` and `Cargo.toml` |
| G-04 | Write `CLEANUP_REPORT.md` | It lists files updated, consolidated, archived, deleted; references updated; decision statuses changed; validation results; remaining issues; manual-review items |
| G-05 | Commit | **Not performed.** The main/orchestrator session owns commits and needs explicit operator approval |

## 11. Execution order

1. Approve Batch A. Execute A-01, A-02, A-05, A-03, A-04. Run the two documentation checkers.
2. Approve Batch B. Execute B-01..B-04. Run the checkers and a reference grep.
3. Approve C-01 and F. Execute C-01, F-01, F-02. Run all three checkers.
4. Approve Batch D separately. Execute D-01..D-05 with a second-agent diff review. Do not touch `DOCTRINE.md` unless the review names a required change.
5. Approve Batch E per item. Execute and verify that no current path breaks.
6. Execute G-01..G-04. Write the final report. Stop before any commit.

Each batch ends with a checkpoint: run the checkers, inspect `git status`, and record the result. Do not combine batches in one commit.

## 12. Whole-plan acceptance criteria

- Every current document has one clear purpose and one authoritative home.
- No current document cites a historical audit as its authority.
- `docs/audit/` is no longer a current layer; its content is preserved under `docs/maintenance/archive/`.
- No current instruction depends on `GIT_RULES.md`, `AUX-RULES.md`, or `handoff/DECISIONS.md`.
- `DECISIONS.md` distinguishes Accepted, Implemented, Superseded, Rejected, and Closed.
- `DOCTRINE.md` remains the one-page binding card and does not change meaning.
- `DESIGN.md` is unmistakably historical and is not default reading.
- `base-archive/`, `archive/`, and `handoff/archive/` each state that they are non-authoritative.
- Every archive path in the documentation resolves, or is explicitly labelled local or external.
- The three documentation checkers and the release check pass.
- Residual open work is tracked in `handoff/tasks/TASK-DOCS-MAINTENANCE.md`.
- `CLEANUP_REPORT.md` records the final state and any remaining manual review.
- No application source, runtime configuration, branch, commit, push, tag, card, engine, or model was touched.

## 13. Explicitly out of scope

- Adding or changing application code, tests, dependencies, or runtime configuration.
- Changing product behavior or binding product doctrine. The only doctrine edit is the approved ledger-status rule in Batch D.
- Creating a per-decision ADR tree. The ledger is the decision system.
- Splitting the README into `docs/reference/` before the agreed threshold.
- Promoting local research into durable documentation. It contains machine paths and needs a scrub first.
- Deleting the live layer, evidence, or archives without a named approval.
- Running the card gate.
- Committing, pushing, tagging, or releasing.

## 14. Approval checklist

| Decision | Recommendation | Blocks |
|---|---|---|
| Adopt `docs/maintenance/` and archive `docs/audit/` | Yes | A-01..A-05, B, F |
| Delete `GIT_RULES.md`, `AUX-RULES.md`, `handoff/DECISIONS.md` | Yes | B |
| Fix the current-doc accuracy items | Yes | C-01, C-02, C-03, C-04 |
| **Amend the decision-ledger append-only rule for a status pass** | **Separate ruling required** | D-01..D-06 |
| Archive local evidence and prune traps | Per item; start with E-01 and E-02 | E |
| Carry residual backlog into a task | Yes | F |
| Commit the result | No, unless the operator asks | G-05 |

Approval wording is intentionally narrow. A general "proceed" is enough for Batches A, B, C, E-01, E-02, F, and G. Batch D needs an explicit sentence that authorizes a status-only amendment of `DECISIONS.md`. Batch E-03 and E-05 need per-item approval because they affect evidence and traps.
