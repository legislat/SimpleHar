---
title: Legacy reference and dependency audit
owner: operator
status: maintenance
audience: [operator, architect, agent]
source_of_truth_for: []
last_reviewed: 2026-11-03
review_trigger: none
---

# LEGACY_REFERENCE_AUDIT.md

| Field | Value |
|---|---|
| Initiative | 2026-11 documentation cleanup |
| Date | 2026-11-03 |
| Repository | `C:\Inference\SimpleHar-SE` |
| Branch / HEAD | `main` / `e931de5` |
| Scope | References between documents, references to removed technologies, decision-ledger dependencies, and references to local or external material |
| Method | Markdown link extraction; plain-text path and term search; content review of every durable document; comparison with `src/`, `Cargo.toml`, `scripts/`, CI, and live-layer files |
| Files modified by this audit | None |

## 1. Method and result summary

The project uses few Markdown links and many plain-text path references. A link-only check therefore under-reports the coupling. This audit checked both.

- **Markdown links:** no broken project links. The only two unresolved relative links are placeholders in `.kilo/skills/installation-guide/references/document-template.md` (`./installation-guide/<svc>/<name>-INSTALL.md`). They are template placeholders, not defects.
- **Plain-text path references:** the main risk. Current documents refer to local-only evidence, moved files, external research, and historical audits by bare path.
- **Retired terms:** the durable documents mention removed features only in retirement context, except `DESIGN.md`, which is historical, and the conditional appendix of `docs/process/ENGINEERING.md`, which names stacks the crate does not use.
- **Decision ledger:** the largest reference and status problem. 51 bullets sit under a heading that says "ARCHIVED (tombstones)", while many of them are active rulings. One delivered item remains in DEFERRED.

## 2. Current authority map

```text
AGENTS.md
  -> docs/process/*        how to work
  -> docs/architecture/DOCTRINE.md    binding invariants
  -> docs/architecture/DECISIONS.md   rulings and rationale
  -> docs/architecture/OVERVIEW.md    current architecture
  -> README.md             user-facing behavior
  -> docs/DOCS_README.md   documentation routing
  -> HANDOFF.md + handoff/ live state (local-only)
  -> DESIGN.md             historical rationale only
```

The intended direction is one-way: current documents may point to historical material for rationale. Historical material must never govern current work.

## 3. Legacy reference chains

| ID | Source | Referenced target | Type | Verdict | Recommended action |
|---|---|---|---|---|---|
| L-01 | `docs/DOCS_README.md:36-38` | `docs/audit/TARGET_STRUCTURE.md` as the authority for planned paths | Current index to historical audit | Outdated authority | Replace with the maintenance rules owned by `DOCS_README.md`; archive the audit |
| L-02 | `docs/DOCS_README.md:96-105` | `docs/audit/` as "this audit and successors" | Current index to historical audit | Outdated layer | Replace with a short `docs/maintenance/` section and an archive pointer |
| L-03 | `docs/audit/AUDIT_REPORT.md` | `handoff/HANDOFF.md`, `handoff/DECISIONS.md`, old line cites | Historical audit to moved or now-local files | Broken today; valid on its date | Do not edit the audit. Archive it and state that dated paths may not resolve |
| L-04 | `docs/audit/TARGET_STRUCTURE.md` | `docs/README.md` as the index; `docs/audit/` as a permanent layer | Historical target design | Outdated names | Archive. The live index is `docs/DOCS_README.md` |
| L-05 | `docs/audit/IMPLEMENTATION_PLAN.md:5` | Status "execution in progress" | Status reference | Stale | Archive. The cleanup report records actual completion |
| L-06 | `docs/audit/DOCUMENTATION_BACKLOG.md:5` | Status "ready for issue-tracker import" | Status reference | Stale | Archive after extracting the 8 residual rows |
| L-07 | `docs/architecture/DECISIONS.md:55` | "see HANDOFF queue" | Decision to the wrong home | Outdated; the queue is `handoff/PENDING_CHECKLIST.md` | Point to the queue index, or remove the clause |
| L-08 | `docs/architecture/DECISIONS.md:101` | `research/ninfer-telemetry-2026-10-30.md` | Durable decision to local evidence | Ambiguous; the file is `handoff/research/...`, and `docs/research/` does not exist | Qualify as `handoff/research/...` and mark it local evidence |
| L-09 | `docs/architecture/DECISIONS.md:177` | `research/cpu-offload-scoping-2026-10-28.md` | Durable decision to local evidence | Same as L-08 | Same as L-08 |
| L-10 | `docs/architecture/DECISIONS.md:331` | `D:\WORKSPACE-VS\research\Search#40-...md` | Durable decision to an external machine path | Unresolvable in this repository | Label it "external, operator machine only", or replace it with the ruling summary already in the ledger |
| L-11 | `docs/architecture/DECISIONS.md:126,399` | `handoff/wave-verify/` | Durable decision to local evidence | Required traceability, but not portable | Label it local evidence; keep the path |
| L-12 | `docs/architecture/DECISIONS.md:241` | `handoff/upstream-ninfer-post-ready-memory-summary.md` | Durable decision to a local file | Valid; the file exists | Keep; label it local |
| L-13 | `docs/architecture/DECISIONS.md:76,160,162` | `DESIGN.md` sections and line cites | Durable decision to the historical design record | Valid rationale, historical source | Keep the rationale; mark `DESIGN.md` as historical and avoid line cites in new text |
| L-14 | `docs/architecture/DOCTRINE.md:96,106` | `handoff/wave-verify/` | Binding invariant to local evidence | Valid but not portable | Keep; the card window is machine-local by doctrine |
| L-15 | `docs/process/REVIEW.md:57` | `handoff/code-review/` as method examples | Process document to local evidence | Valid today, but the directory mixes method files with 55 historical findings files | Keep `PLAN.md` and `PROMPT.md`; archive the findings; point to the kept files |
| L-16 | `docs/process/REVIEW.md:131` | `handoff/evidence/reviews/` | Process document to an evidence path | Broken convention; the directory does not exist | Replace with `handoff/evidence/<YYYY-MM-DD>-<slug>/` to match `VERIFICATION.md` |
| L-17 | `handoff/DECISIONS.md:6` | `docs/audit/IMPLEMENTATION_PLAN.md` | Live stub to an audit scheduled to move | Will break | Delete the stub |
| L-18 | `scripts/check-docs.mjs:2,11` | `docs/audit/TARGET_STRUCTURE.md`; `docs/audit/*` exemption | Tooling comment to the audit layer | Will break after the move | Update the comment to cite `DOCS_README.md` and the new maintenance archive |
| L-19 | `docs/DOCS_README.md:87,89,106` | `ADR/`, `docs/reference/`, `docs/research/` | Index to empty or planned layers | Noise; none exists | Remove `ADR/` and `docs/research/`; keep `docs/reference/` as a threshold-gated plan |
| L-20 | `AGENTS.md:55,107` | `docs/reference/*` and `MODELS.md` as planned | Agent entry point to planned docs | Acceptable, because it says planned and names README as the current owner | Keep; revisit only when the split happens |
| L-21 | `docs/architecture/OVERVIEW.md:283` | `DESIGN.md` as historical | Current architecture to historical rationale | Correct | Keep |
| L-22 | `README.md:223,232` | `strata_config` superseded; `ninfer_autostart` removed | Current user guide to retired modes | Correct; it is the migration message | Keep |
| L-23 | `base-archive/README.md` | v3.3 ports, `external: true`, `preload`, `sticky`, `protected_containers`, `adopt_externals` | Frozen archive to an older product | Misleading without a notice; the file reads like a current spec | Add `base-archive/ARCHIVE-NOTICE.md`; do not edit the baseline README |
| L-24 | `archive/HANDOFF-2026-10-26*.md` | Obsolete session-start chain (`HANDOFF -> DESIGN -> queue`) and one-subagent rule | Frozen archive to obsolete instructions | Misleading if read as current | Add `archive/README.md`; leave the archived files immutable |
| L-25 | `handoff/archive/PENDING_CHECKLIST-2026-11-02-*.md` | Old homes, old classes, and old status vocabulary | Frozen archive to obsolete process | Misleading if read as current | Add `handoff/archive/README.md`; leave the files immutable |
| L-26 | `DESIGN.md` | `base-archive/src/yamlcfg.rs` and old ports 8009/8030/18009/18203 | Historical design record to the v3.3 baseline | Valid history | Keep; the top banner must stay unmissable |
| L-27 | `docs/process/ENGINEERING.md:172-222` | `polars`, `ratatui`, `dioxus`, PyO3, `maturin`, `uv` | Active process document to absent stacks | Conditional, not misleading, but costly context | Optional trim to the stacks present; keep the "only if present" rule if retained |
| L-28 | `.kilo/skills/installation-guide/references/document-template.md:117-118` | Placeholder install-doc paths | Agent skill template | False positive in a link scan | No action |

## 4. Decision ledger review

There is no separate ADR directory. `docs/architecture/DECISIONS.md` is the decision system: 13 LOCKED bullets, 4 DEFERRED bullets, and 51 bullets in the third section. `DOCTRINE.md` is the one-page card. This structure is sound and should not be replaced with per-decision files.

### 4.1 Status defects

| Defect | Evidence | Consequence | Proposed correction |
|---|---|---|---|
| The third section is labelled `ARCHIVED (tombstones: ruling / date / commit)` | `DECISIONS.md:140` | An agent can read active rules as dead history | Rename the section to a dated-rulings register and state that a dated ruling stays binding until a later entry supersedes it |
| Active multi-paragraph rulings sit in that section | R3, R5, rows 17-19d, Fit/R11, 12-P, N3, N7, release, import | The strongest current rulings are filed as archive | Add an explicit status prefix per entry: Accepted, Implemented, Superseded, Rejected, Closed |
| A delivered item remains under DEFERRED | `DECISIONS.md:133-134` (`13[10]` "TRIPPED + DELIVERED") | A delivered decision looks open | Remove it from DEFERRED, or mark it "delivered; see archived line" |
| The append-only rule blocks correction | `DECISIONS.md:19` ("This file only ever *gains* lines") | Status errors cannot be fixed without violating the rule | Ask the operator to amend the rule for a one-time status annotation pass that changes labels only, never ruling text |
| Supersession is stated in prose but not in a uniform field | Entries 19, 19c, Question A, legacy ninfer | A reader must parse paragraphs to learn what is current | Use one status word at the start of every dated entry |
| Two research citations are ambiguous | `DECISIONS.md:101,177` | A fresh clone cannot resolve them | Qualify them as `handoff/research/` and local-only |
| One citation is an external machine path | `DECISIONS.md:331` | A fresh clone cannot resolve it | Label it external and keep the rationale in the ledger |

### 4.2 Decisions that remain valid but reference obsolete material

| Decision | Status | Obsolete reference | Keep or remove |
|---|---|---|---|
| "Config is not a database" | Active | `DESIGN.md` Section 0.4 | Keep the ruling; cite the historical record by name, not by line |
| CPU-offload engine scoping (B+C+D) | Active | `research/cpu-offload-scoping-2026-10-28.md` | Keep; qualify as local evidence |
| Measured-footprint provenance | Active | `research/ninfer-telemetry-2026-10-30.md` | Keep; qualify as local evidence |
| Fit booking (R11) | Active | `D:\WORKSPACE-VS\research\Search#40...` | Keep; label external |
| Strata template mode (row 19) | Active | `DESIGN.md` copy-in addendum | Keep the ruling; the addendum is already marked superseded |
| Legacy `ninfer_autostart` removal | Completed | The old operator-script path | Keep the tombstone; the migration sentence in README is current |
| Governance D1-D11 | Active | The 2026-11-02 audit set | Keep the one-line ruling; the audit becomes the archived rationale |

### 4.3 Decisions with no current applicability

| Decision | Disposition | Reason |
|---|---|---|
| 13[4], 13[5], 13[8], 13[9], 13[11], 13[12] | Archived tombstones | Implementation detail, closed in `e042b52` / `951023f` |
| 13[10] typed write-back errors | Delivered | The delivery line is in the same ledger |
| Question A same-slot credit | Superseded | The 2026-11-01 release-then-resample ruling replaced it |
| Strata copy-in migration | Superseded | Row 19 template mode and the import bridge replaced it |
| `protected_containers` | Rejected | Scrapped by the operator; ownership-scoped reaping replaced it |
| Bonsai class, engine adoption, preload at boot, parallel soak, state restore | Rejected | Scrapped by the operator; listed in DOCTRINE item 18 |

## 5. Obsolete technology references

| Term | Durable locations | Verdict | Action |
|---|---|---|---|
| `strata_config` | `README.md` (3, as superseded), `DESIGN.md` (3), `DECISIONS.md` (3), `OVERVIEW.md` (1) | Correctly framed as removed or superseded everywhere | Keep the retirement text; no active claim |
| `ninfer_autostart` | `README.md` (1, removed), `DESIGN.md` (1), `DECISIONS.md` (1), `OVERVIEW.md` (1) | Correctly framed as removed | Keep |
| `preload`, `sticky`, `protected_containers`, `external: true` | `DOCTRINE.md` (1 each), `DECISIONS.md` LOCKED, `DESIGN.md` history | Rejected or inert | Keep the tombstones |
| v3.3 ports 8009/8030/18009/18203 | `DESIGN.md` only | Historical | Keep with the historical banner |
| `adopt_externals`, `auto_register_externals` | `DESIGN.md` only | Historical | Keep |
| `bonsai` | `DESIGN.md`, `DECISIONS.md`, `DOCTRINE.md` | Rejected class | Keep the tombstone |
| `polars`, `ratatui`, `crossterm`, `dioxus`, `maturin`, `uv`, `indicatif`, `secrecy` | `docs/process/ENGINEERING.md` appendix | Not in the current stack; explicitly conditional | Optional trim; never present them as mandates |
| `simplehar.json` | `DECISIONS.md`, `DOCTRINE.md`, `README.md` | Deprecated but still read for discovery and migration | Keep; the current rule is explicit |
| v3.3 model aliases | `src/catalog.rs`, README | Deprecated but active compatibility seam | Documented; keep |

## 6. Broken, ambiguous, and unresolvable references

1. **`handoff/evidence/reviews/` does not exist.** `REVIEW.md:131` states that full review evidence lives there. Actual review evidence lives in `handoff/evidence/2026-11-03-r2/`, and historical review files live in `handoff/code-review/`.
2. **`handoff/HANDOFF.md` does not exist.** The live snapshot is the root `HANDOFF.md`. The 2026-11-02 audit cites the old path. The audit is historical; do not edit it.
3. **`docs/research/` does not exist.** `DOCS_README.md` lists it as an optional promoted-research layer. Two decisions cite `research/...` as if that layer existed.
4. **`D:\WORKSPACE-VS\research\...` is outside the repository.** Two decisions and one task cite it. The task file already says "operator machine"; the decision rows do not.
5. **`docs/audit/TARGET_STRUCTURE.md` is the stated authority for planned paths.** The current index should own its own routing rules.
6. **`handoff/DECISIONS.md` cites an audit file that will move.** The stub is itself planned for deletion.
7. **`base-archive/README.md` has no archive notice.** Its content is a v3.3 product specification with ports and features that no longer exist in SE.
8. **`archive/` and `handoff/archive/` have no README.** Old handoffs carry obsolete session-start chains. They are frozen, but the directory does not say so.

## 7. References that must be preserved

| Reference | Why it must stay |
|---|---|
| `README.md` to `ninfer-autostart-template.sh` | The template is the single public copy of the canonical script |
| `README.md` to the import command | It is the only migration path from `strata_config` |
| `docs/architecture/DECISIONS.md` to the retired-mode tombstones | They explain why the current paths are the only paths |
| `docs/architecture/DECISIONS.md` to `DESIGN.md` | The design rationale still explains current invariants |
| `docs/process/REVIEW.md` to the review method examples | The falsifier and trap culture must survive |
| `docs/architecture/DOCTRINE.md` to `DECISIONS.md` | The card is a summary; the ledger is the rationale |
| `handoff/GOTCHAS.md` traps about removed paths | They explain why the removal happened and prevent reintroduction |
| `base-archive/` and `archive/` | Traceability for the rewrite and the v3.3 defect register |

## 8. Dependency chains to remove

1. `docs/DOCS_README.md` -> `docs/audit/TARGET_STRUCTURE.md` -> historical target design. Replace with the current maintenance rules in `DOCS_README.md`.
2. `docs/DOCS_README.md` -> `docs/audit/*` as a current layer. Replace with `docs/maintenance/` plus one archive pointer.
3. `handoff/DECISIONS.md` -> `docs/audit/IMPLEMENTATION_PLAN.md`. Delete the stub.
4. `scripts/check-docs.mjs` -> `docs/audit/TARGET_STRUCTURE.md`. Update the comment.
5. `docs/process/REVIEW.md` -> `handoff/evidence/reviews/`. Replace with the real evidence convention.
6. `docs/architecture/DECISIONS.md` -> bare `research/...`. Qualify the local path.
7. `docs/architecture/DECISIONS.md` -> `HANDOFF queue`. Point to `handoff/PENDING_CHECKLIST.md`.

## 9. References to historical material that are valid

- `AGENTS.md` and `OVERVIEW.md` to `DESIGN.md`, explicitly labelled historical.
- `DECISIONS.md` to `DESIGN.md` for design rationale.
- `README.md` to removed modes, only as migration messages.
- `DOCS_README.md` to `archive/` and `base-archive/`, as frozen trees.
- `docs/process/TASKS.md` to `handoff/archive/<date>/`.

## 10. Needs review

| Topic | Why it needs a human decision |
|---|---|
| `DECISIONS.md` status pass | The ledger is append-only by doctrine. A status annotation pass needs an operator ruling. |
| `GOTCHAS.md` pruning | Traps are hard-won. Several name removed paths, but they may still prevent a regression. Only the operator can decide which are dead. |
| `handoff/code-review/` archive scope | 55 findings files are historical evidence. `PLAN.md` and `PROMPT.md` are still cited. The split needs approval. |
| `DESIGN.md` disposition | Keeping it at the root preserves rationale but costs 86 KB in the tree. Moving it to an archive is cleaner but breaks historical references in commit bodies. |
| Unused conditional stacks in `ENGINEERING.md` | Trimming reduces routine reading cost; keeping preserves approved conditional guidance. |
| `docs/reference/` split | The agreed threshold is not met. Do not split now; decide at the next release review. |
| Backup and restore policy | The live layer has a backup directory but no durable policy. This is a process gap, not a documentation defect. |
