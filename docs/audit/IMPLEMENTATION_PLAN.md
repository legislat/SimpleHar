# Implementation Plan

| Field | Value |
|---|---|
| Status | Proposal - do not execute until the owner approves |
| Date | 2026-11-02 |
| Inputs | `docs/audit/AUDIT_REPORT.md`, `docs/audit/TARGET_STRUCTURE.md`, `docs/audit/PROCESS_IMPROVEMENTS.md` |
| Scope | Documentation, instructions, process, and automation. No product behavior change is proposed except where a document is corrected to match shipped behavior. |
| Constraint | Phase 1 may start only after the owner decisions in Section 1. No existing file is modified by this audit. |

---

## 1. Phase 0 - Owner decisions (blocking)

These decisions are not implementation tasks. They determine whether and how the plan proceeds.

| ID | Decision | Recommendation | Blocks | Audit refs |
|---|---|---|---|---|
| D1 | Track the durable core, or keep everything local? | Track rules, process, doctrine, architecture, decisions, changelog. Keep live state/evidence local. Confirm repository visibility first. | P1-04, P2-01..P2-05 | GOV-01 |
| D2 | Should `DECISIONS.md` move to `docs/architecture/` and be tracked? | Yes; add a short `DOCTRINE.md` for agent briefs. | P2-03 | GOV-01, PRO-05 |
| D3 | Is `DESIGN.md` the current spec or a historical record? | Historical. Create `OVERVIEW.md` for current architecture. | P1-03, P2-04 | DOC-02 |
| D4 | Approve file moves/renames and redirects? | Approve `GIT_RULES.md` -> `docs/process/GIT_WORKFLOW.md`, `AUX-RULES.md` -> process docs, `DECISIONS.md` move. Leave short root redirects for one cycle. | P2-05 | GOV-03, PRO-04 |
| D5 | Serial agents or parallel worktrees? | Serial writer, parallel read-only research. Delete stale worktrees. | P1-06, P3-05 | GOV-06, GOV-10 |
| D6 | Formalize the narrative commit style or enforce imperative? | Formalize narrative with a length limit; it carries rationale and matches history. | P1-07, P3-04 | PRO-04 |
| D7 | Allow GitHub Actions CI? | Yes, cardless checks only on `windows-latest`. | P4-04 | PRO-07 |
| D8 | Keep `GOTCHAS.md` and `FINDINGS-OPEN.md` local? | Yes; promote scrubbed, reusable entries only. | P2-01, P3-06 | PRO-12 |
| D9 | Approve an independent backup for `handoff/` and `ModelConf/`? | Yes; daily or per-cycle, separate disk. | P1-01 (risk reduction) | GOV-01 |
| D10 | Approve version/changelog/tag coupling? | Yes. No tag without a changelog and matching version. | P1-08, P4-05 | PRO-09, DOC-05 |
| D11 | Adopt ASD-STE100 for durable documentation? | Yes. Exact LOCKED wording and code identifiers stay unchanged. | P2-09 | Operator requirement 2026-11-02 |

**Gate:** D1-D11 are approved as of 2026-11-02. Phase 1 started on that approval. If D1 had been "keep everything local", Phase 2 and Phase 4 would shrink to documentation quality and local scripts.

---

## 2. Phase 1 - Critical fixes

Objective: remove false statements, establish a correct entry point, eliminate contradictory gates and commit rules, and remove competing worktrees. No structural migration yet.

### P1-01 - Rewrite README strata sections for template mode

| Field | Value |
|---|---|
| Priority | P0 |
| Description | Replace the superseded `strata_config` documentation with the shipped template-mode model: `strata_exe`, `strata_args`, `strata_python`, `strata_cwd`, `strata_*` profile knobs, rendered profile root, import bridge, forced loopback bind, assigned port, and the migration sentence. Correct the stack table and the bind paragraph. |
| Dependencies | D1 not required; can start immediately |
| Affected files | `README.md` (lines 26-33, 76-82, 146-219) |
| Expected outcome | A user following the README can configure and launch a strata model; no claim contradicts `src/model/scan.rs:1781-1784`, `src/engine/strata.rs:786-796`, or `DECISIONS.md:321-338`. |
| Acceptance criteria | `rg "strata_config" README.md` returns only the migration/deprecation mention, never "required". The strata launch description names the rendered profile. The bind description says harness-forced loopback. A reviewer verifies each statement against the code and the ruling. |
| Risks | Accidentally documenting internal rendering details that should stay private; overcorrecting and removing still-valid legacy notes. Mitigate with a focused review against `DECISIONS.md` 19/19b/19c/19d. |

### P1-02 - Fix stale code doc comments

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Update `src/lib.rs` milestone text; remove "there is no auth" from `src/config/mod.rs` and replace it with the actual token posture; correct `DESIGN.md:150` if DESIGN remains current for one cycle. |
| Dependencies | None |
| Affected files | `src/lib.rs:4-6`, `src/config/mod.rs:390`, `src/config/mod.rs:1034`, `DESIGN.md:150` |
| Expected outcome | Code comments no longer claim modules are absent or authentication does not exist. |
| Acceptance criteria | No "there is no auth" claim remains in durable docs or production comments. `cargo doc` has no new warnings. The reviewer confirms the token description matches `README.md:475-486`. |
| Risks | A broad comment rewrite creates noise; keep the change scoped to false claims. |

### P1-03 - Reclassify `DESIGN.md` and neutralize the superseded addendum

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Add a status banner: historical design record; current architecture in `docs/architecture/OVERVIEW.md`; binding decisions in `docs/architecture/DECISIONS.md`. Mark the strata copy-in addendum as superseded by row 19. Change "DRAFT for operator review" to "Historical". |
| Dependencies | D3 |
| Affected files | `DESIGN.md:1-10`, `DESIGN.md:1013-1033`, `HANDOFF.md:93` |
| Expected outcome | No agent treats `DESIGN.md` as the current spec; the copy-in contradiction is visibly retired. |
| Acceptance criteria | Banner present at the top; addendum has a superseded note naming row 19; `HANDOFF.md` no longer calls it the "spec of record" without qualification. |
| Risks | The 85 KB history is still read by habit; the banner must be unmissable and the session-start chain must stop requiring it. |

### P1-04 - Make `AGENTS.md` the real entry point

| Field | Value |
|---|---|
| Priority | P0 |
| Description | Add: 5-line project orientation; "start here" chain; numbered precedence; conflict rule; autonomy table; required-reading matrix; safety fences; definition of done; links to all process docs. In Phase 1, keep the generic sections in place but demote them to an appendix; full extraction happens in P2-02. |
| Dependencies | D1 (if links point to tracked files), otherwise use existing local paths |
| Affected files | `AGENTS.md`, `HANDOFF.md` (remove duplicate start chain), `GIT_RULES.md`, `AUX-RULES.md` (add "see AGENTS precedence" line) |
| Expected outcome | An agent that reads only `AGENTS.md` knows where to go next and what it may not do. |
| Acceptance criteria | A new agent, given only `AGENTS.md`, can name the queue, doctrine, gotcha ledger, gate, review protocol, and card prohibition. Precedence is explicit. The autonomy table covers commit, push, tag, delete, dependency, card, live harness, doctrine, subagent. |
| Risks | Adding a routing layer without slimming creates a longer file; enforce the 200-line budget by moving generic content to P2-02. |

### P1-05 - Define one gate, remove contradictory copies

| Field | Value |
|---|---|
| Priority | P0 |
| Description | Create `docs/process/VERIFICATION.md` as the single definition of the gate: cardless commands, card rules, evidence, pass/fail/blocked vocabulary, flakiness policy. Replace the gate blocks in `AGENTS.md`, `AUX-RULES.md`, `PENDING_CHECKLIST.md`, and `DESIGN.md` with pointers. Do not embed counts. |
| Dependencies | P1-04 (routing), D1 (if tracked) |
| Affected files | `docs/process/VERIFICATION.md` (new), `AGENTS.md:225-235`, `AUX-RULES.md:33-36`, `handoff/PENDING_CHECKLIST.md:32-39`, `DESIGN.md:869` |
| Expected outcome | One authoritative gate definition; no document can be satisfied by a weaker set of commands. |
| Acceptance criteria | Every gate mention either points to `VERIFICATION.md` or is removed. The commands match the current gate. No test counts appear in durable rules. A reviewer confirms all four old locations. |
| Risks | The doc may drift from the eventual script; cross-check in P4-01 and make the script authoritative. |

### P1-06 - Remove stale worktrees and add a worktree preflight

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Inspect `brassy-mosquito` and the two pool entries for uncommitted work; back up if needed; remove them with `git worktree remove`/`prune`. Add a preflight rule: a worktree is valid only if the durable docs exist and the base commit is current. |
| Dependencies | D5 |
| Affected files | Git worktree state, `.kilo/agent-manager.json` (tool-managed; do not hand-edit), `docs/process/GIT_WORKFLOW.md` (created in Phase 3), `AGENTS.md` |
| Expected outcome | No agent can start in a stale tree; disk and branch confusion removed. |
| Acceptance criteria | `git worktree list` shows only the main worktree (or intentional active tasks). `brassy-mosquito` branch is deleted or explicitly retained with a reason. A documented preflight exists. |
| Risks | Deleting uncommitted work. Mitigate: `git -C <worktree> status`, diff, and a backup before removal. Never delete without operator confirmation. |

### P1-07 - Align commit-message rules with actual history

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Rewrite the commit section of `GIT_RULES.md` (or its replacement) to document the narrative house style: subject states the outcome, preferred <= 72 chars, hard maximum 100 for normal commits, long review-round subjects allowed with a body; body names finding IDs, what changed, and the falsifier. Update the "small focused commit" rule to permit one review-scoped batch. |
| Dependencies | D6 |
| Affected files | `GIT_RULES.md:28-41`, `AGENTS.md:206-212` |
| Expected outcome | An agent following the rule produces a commit consistent with `git log`. |
| Acceptance criteria | The rule and 10 recent commits are mutually consistent. The 257-character subject is explained as an allowed review-round exception or the rule forbids it and future commits change. |
| Risks | Bikeshedding. Keep it simple: length limit, required body fields, one logical change. |

### P1-08 - Make release claims truthful and add a changelog

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Separate "Latest release" from "Repository HEAD" in README. Add `CHANGELOG.md` with v1.0.0 and v1.0.1 entries from existing records. State the version/tag/changelog coupling rule. |
| Dependencies | D10 |
| Affected files | `README.md:10-11`, `CHANGELOG.md` (new), `AUX-RULES.md:37-39`, `handoff/PENDING_CHECKLIST.md:280-291` |
| Expected outcome | A reader can tell what the latest release is, what HEAD is, and what changed. |
| Acceptance criteria | README names the latest tag and states that HEAD may be ahead. Changelog has dated entries with links. Version `1.0.1` matches the latest tag. The release rule is stated once. |
| Risks | Reconstructing history inaccurately; use git tags, commit bodies, and existing release notes; mark uncertain dates. |

### Phase 1 exit criteria

- No durable document claims `strata_config` is required.
- No durable document claims "no auth".
- `AGENTS.md` routes to the queue, doctrine, traps, gate, review, and handoff.
- One gate definition exists.
- One commit-style rule matches history.
- No stale worktrees.
- Release claim is unambiguous.
- The gate passes; a reviewer confirms the diff is documentation-only (except comment fixes).

---

## 3. Phase 2 - Documentation architecture

Objective: establish the durable tree, move durable knowledge out of the live layer, and make navigation explicit. This phase changes file locations; use redirects and a compatibility cycle.

### P2-01 - Create the docs skeleton and index

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/README.md`, `docs/process/`, `docs/architecture/`, `docs/reference/`, `docs/audit/` (exists). Add the routing table and the layer model. Add front matter to every new doc. Do not move content yet. |
| Dependencies | D1, P1-04 |
| Affected files | `docs/README.md`, directory structure, `README.md` contributor link, `AGENTS.md` map, `CONTRIBUTING.md` |
| Expected outcome | Every information type has a declared home; a new contributor can navigate. |
| Acceptance criteria | Routing table covers all durable topics. Every new doc has front matter. No path in `docs/README.md` is broken. |
| Risks | Creating empty scaffolding. Mitigate: create only directories that receive content in this phase. |

### P2-02 - Extract engineering standards from `AGENTS.md`

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Move generic coding/testing/documentation/security/platform rules into `docs/process/ENGINEERING.md`. Remove unused stack rules from the mandatory path; keep them in an appendix gated on stack presence. Resolve the internal tensions from GOV-08. |
| Dependencies | P2-01 |
| Affected files | `AGENTS.md`, `docs/process/ENGINEERING.md` |
| Expected outcome | `AGENTS.md` <= 200 lines and project-specific; engineering standards are complete and applicable. |
| Acceptance criteria | No rule is lost without a replacement. Unused tools no longer appear as project mandates. Comment/`unwrap`/optimization tensions are resolved explicitly. Reviewer confirms line budget. |
| Risks | Losing a subtle rule in the move. Mitigate: build a rule-by-rule mapping table and review it. |

### P2-03 - Move decisions and create the doctrine card

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Move `handoff/DECISIONS.md` to `docs/architecture/DECISIONS.md`, preserve IDs and history, add the front matter. Create `docs/architecture/DOCTRINE.md` with the 12-20 binding invariants and one-line consequences. Update `HANDOFF.md` Section 5 to point to the card, not the digest. |
| Dependencies | D2, P2-01 |
| Affected files | `handoff/DECISIONS.md`, `docs/architecture/DECISIONS.md` (new), `docs/architecture/DOCTRINE.md` (new), `HANDOFF.md:78-85`, `AUX-RULES.md:8-10` |
| Expected outcome | Binding doctrine is short and tracked; the full ledger remains available for rationale. Subagents stop carrying 30 KB. |
| Acceptance criteria | Every LOCKED invariant appears in `DOCTRINE.md` with a pointer to its ledger row. No ledger row is lost. `DOCTRINE.md` <= 120 lines. A subagent brief using only the card can answer the core safety questions. |
| Risks | Summarizing changes meaning. Mitigate: keep exact operator wording for binding clauses; the card points to the full text. |

### P2-04 - Write the current architecture overview

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/architecture/OVERVIEW.md`: system context, actor/scheduler model, admission/witness, engine adapters, config layers, surfaces, module map, data flow, invariants, links to code symbols. Do not duplicate 85 KB of design history. |
| Dependencies | D3, P2-01, P1-03 |
| Affected files | `docs/architecture/OVERVIEW.md`, `README.md` link, `AGENTS.md` reading matrix |
| Expected outcome | A new agent can understand the current system without reading `DESIGN.md`. |
| Acceptance criteria | A reviewer can trace each module to its responsibility. Every claim is verified against code. No superseded strata copy-in path. <= 300 lines. |
| Risks | Writing a second stale spec. Mitigate: keep it short, link to code, assign a review trigger, and verify at release. |

### P2-05 - Consolidate `GIT_RULES.md` and `AUX-RULES.md`

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/process/GIT_WORKFLOW.md`, `docs/process/TASKS.md`, `docs/process/REVIEW.md`, and `docs/process/PUBLICATION.md` from the durable parts of both files. Leave short root redirects or retire them after one cycle. Remove duplicated gate/release text. |
| Dependencies | D4, P1-05, P1-07, P2-01 |
| Affected files | `GIT_RULES.md`, `AUX-RULES.md`, four process docs, `AGENTS.md`, `HANDOFF.md` |
| Expected outcome | Two generic rule files become scoped, non-overlapping process docs with one source each. |
| Acceptance criteria | No rule exists in two authoritative documents. Root files, if kept, are <= 15 lines and point to the new home. A rule-by-rule mapping is reviewed. |
| Risks | Losing a safety rule; redirect confusion. Mitigate: mapping table; keep redirects for one release. |

### P2-06 - Split README reference sections when threshold is met

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Move the full HTTP API and ModelConf schema into `docs/reference/API.md` and `docs/reference/MODELS.md`, leaving summaries and links in README. Move troubleshooting to `docs/reference/TROUBLESHOOTING.md`. |
| Dependencies | P2-01, P1-01 |
| Affected files | `README.md`, `docs/reference/*` |
| Expected outcome | README remains an overview and quick start; reference detail has a stable home. |
| Acceptance criteria | No content is lost; links resolve; the README still lets a user start without opening reference docs. API/model docs are verified against code. |
| Risks | Link rot and two homes. Mitigate: move, do not copy; update all references; run the doc checker (P4-02). |

### P2-07 - Define ownership, front matter, and maintenance rules

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Add front matter to every durable doc; add the same-change rule, contradiction rule, release review, quarterly review, archive rule, and anti-duplication audit to `docs/README.md` or a dedicated `docs/process/DOCUMENTATION.md` (only if it does not duplicate `TARGET_STRUCTURE.md`). |
| Dependencies | P2-01..P2-05 |
| Affected files | All durable docs, `docs/README.md`, `CONTRIBUTING.md` |
| Expected outcome | Every document has an owner, status, audience, source-of-truth declaration, and review date. |
| Acceptance criteria | Front matter present on 100% of durable docs. No orphan docs. The maintenance rules are stated once. |
| Risks | Bureaucratic overhead. Mitigate: four required fields only; automation checks them. |

### P2-08 - Archive and retention cleanup

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Fix the broken `PENDING_CHECKLIST.md:14` path. Move closed narratives to `handoff/archive/<date>/`. Define what is retained in `archive/` vs `base-archive/`. Add a banner to frozen trees. Compress cold evidence only with operator approval. |
| Dependencies | P2-01, P1-06 |
| Affected files | `handoff/PENDING_CHECKLIST.md`, `handoff/archive/`, `archive/`, `base-archive/`, evidence directories |
| Expected outcome | Current state is easy to find; history is preserved but out of the way. |
| Acceptance criteria | Every archive pointer resolves. No current document points to a superseded archive as authoritative. Retention rules are written. |
| Risks | Deleting history. Mitigate: move, never delete; compress only after verification. |

### P2-09 - Adopt ASD-STE100 documentation language

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Add the ASD-STE100 standard to `docs/process/ENGINEERING.md` and apply it to new or changed durable documentation. Define scope, rules, exceptions (LOCKED wording, code identifiers, paths, logs), a glossary rule, and a section-by-section review method. Migrate README and reference docs first; do not rewrite `DESIGN.md` wholesale. |
| Dependencies | P2-01, P2-02, P2-07; operator requirement 2026-11-02 |
| Affected files | `docs/process/ENGINEERING.md`, `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `docs/reference/*`, `docs/process/*`, `docs/architecture/OVERVIEW.md` |
| Expected outcome | Durable documentation uses controlled, plain English. Users can read setup and failure text without interpreting metaphor. Reviewers have an objective style standard. |
| Acceptance criteria | The standard is stated once in `ENGINEERING.md`. New or changed sections are checked against the STE main rules and dictionary. A glossary exists where a document introduces more than three project terms. The README and reference rewrite is reviewed section by section. Exact LOCKED wording and code identifiers are unchanged. |
| Risks | Over-editing binding doctrine; losing useful house voice; claiming compliance from a vocabulary-only check. Mitigate: keep exact doctrine verbatim; allow a short plain-language preface plus the exact ruling; require review evidence. |

### Phase 2 exit criteria

- `docs/` is the durable home; `handoff/` is the live/evidence home.
- Decisions and doctrine are tracked (if D1/D2 approved).
- `AGENTS.md` is a routing document under budget.
- No durable rule is duplicated.
- Every durable doc has front matter and is listed in the index.
- README reference detail has a declared home.
- Archive pointers resolve.
- The ASD-STE100 standard is stated once and new documentation follows it.

---

## 4. Phase 3 - Development workflows

Objective: make tasks, handoff, review, Git, parallel work, and debt operationally consistent.

### P3-01 - Adopt the task schema and migrate the queue

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/process/TASKS.md`. Add the task template. Convert live queue rows to one-line index rows with IDs, class, priority, readiness, owner, and task-file link. Create task files for active multi-session tasks. Move completed narratives to archive. |
| Dependencies | P2-01, P2-05, D5 |
| Affected files | `docs/process/TASKS.md`, `handoff/PENDING_CHECKLIST.md`, `handoff/tasks/*` (new), `HANDOFF.md` |
| Expected outcome | Every active task has goal, scope, non-goals, acceptance, dependencies, evidence, and next step. |
| Acceptance criteria | 100% of active tasks have complete task files. Queue rows are one line. Definition of Ready and Done are explicit. No task detail remains only in prose. |
| Risks | Migration is laborious and may lose nuance. Mitigate: migrate only live tasks; archive the rest verbatim; do not rewrite history. |

### P3-02 - Implement the handoff protocol and checkpoints

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/process/HANDOFF.md` with triggers, required fields, dirty-tree protocol, resume checklist, and templates. Rewrite root `HANDOFF.md` to the snapshot template. Add a checkpoint instruction to `AGENTS.md`. |
| Dependencies | P3-01 |
| Affected files | `docs/process/HANDOFF.md`, `HANDOFF.md`, `AGENTS.md` |
| Expected outcome | Work can be interrupted at any point and resumed with bounded context. |
| Acceptance criteria | A simulated interruption (dirty tree, no commit) can be resumed by another agent using only `HANDOFF.md` + task file + `git status`. `HANDOFF.md` <= 120 lines. The dirty-tree protocol is explicit. |
| Risks | Agents skip checkpoints under time pressure. Mitigate: make checkpoint a required step in the task template and review it at closure. |

### P3-03 - Formalize review and definition of done

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/process/REVIEW.md` from the durable parts of the review plan and `AUX-RULES.md`. Extend closure to all tasks. Define when review is required and when it is not. |
| Dependencies | P2-05, P3-01 |
| Affected files | `docs/process/REVIEW.md`, `AGENTS.md`, `docs/process/TASKS.md`, `handoff/FINDINGS-OPEN.md` (format only) |
| Expected outcome | Review is discoverable, proportionate, and consistently closed. |
| Acceptance criteria | A reviewer can run a chunk using only the doc. Falsifier requirement is explicit. A task with open findings cannot be marked accepted. Trivial changes are exempt. |
| Risks | Over-review of small changes. Mitigate: explicit exemption list and risk-based triggers. |

### P3-04 - Write the real Git workflow

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Create `docs/process/GIT_WORKFLOW.md`: trunk model, branch/worktree rules, staging, commit style, review, merge, protected operations, release, conflict resolution, stale worktree cleanup. |
| Dependencies | P1-06, P1-07, P2-05, D6 |
| Affected files | `docs/process/GIT_WORKFLOW.md`, `AGENTS.md`, `CONTRIBUTING.md` |
| Expected outcome | The documented model matches the repository; parallel work has a path. |
| Acceptance criteria | A new contributor can follow the doc and produce a compliant commit/release. The 30 recent commits are consistent with the rule or the exceptions are explained. No PR language if no PRs are used. |
| Risks | Over-specifying a one-person workflow. Mitigate: keep it short; add branches/PRs only if D5 selects parallel work. |

### P3-05 - Decide and implement the parallel-agent model

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Based on D5, either (A) enforce serial writers and delete worktrees, or (B) define per-worktree task ownership, file ownership, merge order, and ledger write rules. Add a preflight that verifies durable docs and base commit. |
| Dependencies | D5, P3-01, P3-04 |
| Affected files | `AGENTS.md`, `docs/process/GIT_WORKFLOW.md`, `docs/process/TASKS.md`, `.kilo` (tool state only) |
| Expected outcome | No silent divergence or shared-ledger overwrite. |
| Acceptance criteria | The chosen model is stated once. A worktree without instructions cannot be considered valid. `git worktree list` is clean at cycle start. |
| Risks | Choosing parallel without real coordination discipline. Mitigate: default to serial until a concrete need appears. |

### P3-06 - Consolidate technical debt

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Add a debt class and required fields to `FINDINGS-OPEN.md`, or create a single debt index that links to findings/decisions/tasks. Define intake, triage, aging, and accepted-risk rules. |
| Dependencies | P3-01, P3-03 |
| Affected files | `handoff/FINDINGS-OPEN.md`, `handoff/PENDING_CHECKLIST.md`, `docs/process/TASKS.md`, `docs/process/REVIEW.md` |
| Expected outcome | One view of debt with severity, owner, target, and age. |
| Acceptance criteria | No debt item exists only in prose. Accepted risks have a ruling and a permanent row. A cycle-start triage is documented. |
| Risks | Creating a fourth ledger. Mitigate: extend the existing findings ledger; do not add a new file unless it is only an index. |

### P3-07 - Add onboarding and subagent brief templates

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Add `CONTRIBUTING.md`, the first-task walkthrough in `TASKS.md`, and a subagent brief template with the required reading matrix, scope, evidence format, and conflict rule. |
| Dependencies | P2-01, P3-01, P3-03 |
| Affected files | `CONTRIBUTING.md`, `docs/process/TASKS.md`, `docs/process/HANDOFF.md`, `AGENTS.md` |
| Expected outcome | A new human or agent can start without operator explanation. |
| Acceptance criteria | The walkthrough can be followed end to end. The brief template fits in one screen plus links. A fresh-clone check (P4-07) passes. |
| Risks | The walkthrough goes stale. Mitigate: use it for one real task and fix friction. |

### Phase 3 exit criteria

- Every active task has a complete task file.
- Handoff works for clean and dirty interruptions.
- Review is documented and enforced by the definition of done.
- Git workflow matches reality.
- The parallel model is explicit and worktrees are clean.
- Debt has one register and an aging rule.
- Onboarding exists.

---

## 5. Phase 4 - Automation and quality

Objective: make the invariants mechanical where practical. Automation supplements, never replaces, review and operator judgment.

### P4-01 - Single gate script

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Implement `scripts/gate.ps1` (or `cargo xtask gate`) that runs the cardless gate and prints counts. Make `VERIFICATION.md` describe it and all other docs point to it. |
| Dependencies | P1-05, D7 |
| Affected files | `scripts/gate.ps1`, `docs/process/VERIFICATION.md`, `AGENTS.md`, `TASKS.md`, `REVIEW.md` |
| Expected outcome | One executable gate; no document can misquote it. |
| Acceptance criteria | The script exits non-zero on any failure; output includes command, duration, and counts; it never starts an engine; it is used by CI. |
| Risks | Script drift from CI. Mitigate: CI calls the same script. |

### P4-02 - Documentation checker

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Implement `scripts/check-docs.mjs` with no external dependencies: front matter, relative links, referenced paths, duplicate H1, `DRAFT` on released docs, absolute machine paths, index coverage, duplicate gate blocks. |
| Dependencies | P2-01, P2-07, P4-01 |
| Affected files | `scripts/check-docs.mjs`, `docs/README.md`, CI workflow |
| Expected outcome | Documentation rot is caught mechanically. |
| Acceptance criteria | The checker passes on the migrated tree; it fails on a missing link, missing front matter, absolute path, and orphan doc (test with fixtures); it runs in CI. |
| Risks | False positives on intentional examples. Mitigate: allowlist code blocks and explicit `planned:` paths. |

### P4-03 - Template and instruction checkers

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Implement `scripts/check-tasks.mjs` (task/queue schema) and `scripts/check-instructions.mjs` (AGENTS links, precedence section, no duplicated gate, no retired paths). |
| Dependencies | P3-01, P3-02, P4-02 |
| Affected files | `scripts/check-tasks.mjs`, `scripts/check-instructions.mjs`, CI workflow |
| Expected outcome | Task files and instructions cannot silently drift from the schema. |
| Acceptance criteria | Both checkers pass on the migrated state; each fails on a deliberately invalid fixture; they run in CI. |
| Risks | Over-constraining prose. Mitigate: validate required fields and links, not writing style. |

### P4-04 - CI workflow

| Field | Value |
|---|---|
| Priority | P1 |
| Description | Add `.github/workflows/ci.yml` on `windows-latest`: checkout, Rust toolchain, cache, gate script, docs checker, task checker. Card tests excluded and named as operator-only. |
| Dependencies | D7, P4-01, P4-02, P4-03 |
| Affected files | `.github/workflows/ci.yml`, `README.md` badge (optional) |
| Expected outcome | Every push is checked without a card; the operator sees failures quickly. |
| Acceptance criteria | A green run on `main`; a deliberately broken test fails the workflow; no engine spawn; runtime under a reasonable bound (cache warm). |
| Risks | CI minutes/cost; Windows runner availability; private-repo limits. Mitigate: cache; run on push/PR only; keep the card gate out. |

### P4-05 - Release check

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Implement `scripts/release-check.ps1` verifying version, tag, changelog entry, README release claim, and artifact hash. |
| Dependencies | P1-08, D10, P3-04 |
| Affected files | `scripts/release-check.ps1`, `docs/process/GIT_WORKFLOW.md`, `CHANGELOG.md` |
| Expected outcome | No tag without matching version and changelog. |
| Acceptance criteria | The script fails on a version/tag mismatch and on a missing changelog entry; it passes on a correctly prepared release. |
| Risks | Over-automating a rare manual step. Mitigate: keep it a checklist script, not a deployer. |

### P4-06 - Fresh-clone onboarding check

| Field | Value |
|---|---|
| Priority | P2 |
| Description | Add a script/job that clones the repository to a temp directory and verifies the durable onboarding path exists and links resolve. |
| Dependencies | D1, P2-01, P4-02 |
| Affected files | `scripts/check-fresh-clone.ps1`, CI workflow |
| Expected outcome | The exact failure found by this audit cannot recur silently. |
| Acceptance criteria | The check passes on a clone with only tracked files; it fails if `AGENTS.md`, `docs/README.md`, or the process docs are missing. |
| Risks | Slow clone in CI. Mitigate: shallow clone; run on release and weekly, not every push. |

### P4-07 - Freshness and process metrics

| Field | Value |
|---|---|
| Priority | P3 |
| Description | Add a lightweight report: docs with `last_reviewed` older than a quarter, debt age, queue age, gate failures, worktree age. Optionally a scheduled CI job that opens no issues but writes an artifact. |
| Dependencies | P4-02, P3-06 |
| Affected files | `scripts/report-health.mjs`, CI schedule |
| Expected outcome | Drift is visible before it becomes a defect. |
| Acceptance criteria | The report runs and produces the agreed fields; it does not modify files; the operator reviews it at cycle start. |
| Risks | Metrics theater. Mitigate: keep five metrics, review them, delete any that do not change a decision. |

### Phase 4 exit criteria

- Gate script is the only executable definition.
- Docs, tasks, and instructions are checked.
- CI is green and cardless.
- Release check exists.
- Fresh-clone check proves the repository is self-describing.
- Metrics are used, not merely produced.

---

## 6. Dependency graph

```text
D1..D10 (owner decisions)
|
+-- Phase 1 critical fixes
|   |-- P1-01 README strata
|   |-- P1-02 code comments
|   |-- P1-03 DESIGN banner
|   |-- P1-04 AGENTS routing
|   |-- P1-05 gate doc
|   |-- P1-06 worktrees
|   |-- P1-07 commit style
|   `-- P1-08 release truth
|
+-- Phase 2 documentation architecture (needs P1-04)
|   |-- P2-01 docs skeleton
|   |-- P2-02 ENGINEERING
|   |-- P2-03 DECISIONS + DOCTRINE
|   |-- P2-04 OVERVIEW (needs P1-03)
|   |-- P2-05 process consolidation
|   |-- P2-06 reference split
|   |-- P2-07 front matter
|   |-- P2-08 archive cleanup
|   `-- P2-09 ASD-STE100 language
|
+-- Phase 3 workflows (needs P2-01, P2-05)
|   |-- P3-01 TASKS + queue
|   |-- P3-02 HANDOFF
|   |-- P3-03 REVIEW
|   |-- P3-04 GIT_WORKFLOW (needs P1-06, P1-07)
|   |-- P3-05 parallel model (needs P1-06)
|   |-- P3-06 debt
|   `-- P3-07 onboarding
|
`-- Phase 4 automation (needs Phase 2/3 docs)
    |-- P4-01 gate script (needs P1-05)
    |-- P4-02 docs check
    |-- P4-03 task/instruction check
    |-- P4-04 CI
    |-- P4-05 release check (needs P1-08)
    |-- P4-06 fresh clone
    `-- P4-07 metrics
```

Critical path: **D1/D2/D3 -> P1-04 -> P2-01 -> P2-03 -> P3-01 -> P3-02 -> P4-02 -> P4-04**.

---

## 7. Suggested effort and sequencing

Estimates assume one operator plus agents, cardless work only, and no product changes.

| Phase | Tasks | Estimate | Notes |
|---|---|---:|---|
| Phase 0 | Decisions | 0.5 day | Mostly operator thought; do not rush D1-D3 |
| Phase 1 | 8 | 2-3 days | README and AGENTS are the largest writing tasks |
| Phase 2 | 9 | 3-5 days | Migration, mapping tables, and ASD-STE100 review |
| Phase 3 | 7 | 3-4 days | Task migration is the largest; handoff is small |
| Phase 4 | 7 | 2-3 days | Scripts and CI; Node/Rust tooling exists |
| **Total** | **31** | **11-16 days** | Spread over cycles; no big-bang rewrite |

Recommended rollout:

1. Phase 0 decisions.
2. Phase 1 in one or two cycles; fix README first because it is user-facing.
3. Phase 2 in one cycle, starting with the docs skeleton and decisions move.
4. Use the new system for one real task before Phase 3 migration. This exposes friction.
5. Phase 3 in one or two cycles.
6. Phase 4 after the docs/task schema stabilize. Automation should encode an agreed process, not invent one.

---

## 8. Rollback strategy

- **Documentation moves:** keep redirects and archive the old file; never delete during the migration cycle. A move can be reversed by restoring the old path from the archive.
- **AGENTS.md rewrite:** keep the old file in `archive/AGENTS-<date>.md` until one cycle passes. If the new entry point fails, restore.
- **DECISIONS move:** copy first, verify IDs, then remove the old file; keep a tombstone pointer for one cycle.
- **Task migration:** archive the full old checklist verbatim before converting live rows. If the schema proves too heavy, keep the queue and drop task files for small items.
- **CI:** a failing or noisy workflow may be disabled; the local gate remains authoritative.
- **Worktree cleanup:** back up diffs/uncommitted files before removal; record the branch commit in the archive.

---

## 9. Plan-level acceptance criteria

The plan is complete when:

1. A fresh clone contains `README.md`, `AGENTS.md`, `CONTRIBUTING.md`, `docs/README.md`, `docs/process/*`, `docs/architecture/DOCTRINE.md`, `docs/architecture/DECISIONS.md`, `docs/architecture/OVERVIEW.md`, and `CHANGELOG.md`.
2. An agent given only the clone can follow `AGENTS.md` to the queue, understand doctrine, run the gate, and produce a compliant commit.
3. An active task can be handed to another agent using only `HANDOFF.md`, the queue row, and the task file.
4. No durable document contains a machine path, a gate command copy, a test count, or a claim contradicted by code.
5. `scripts/gate.ps1` is the only executable gate; CI calls it.
6. The docs checker, task checker, and instruction checker pass in CI.
7. The release check prevents version/tag/changelog mismatch.
8. Stale worktrees are gone and the worktree model is explicit.
9. There is one debt register with owners and aging.
10. The five audit reports are retained in `docs/audit/` and referenced by the docs index.
11. Changed durable documentation follows ASD-STE100, and the review evidence names the
    section-level check.

---

## 10. Risks of the plan

| Risk | Impact | Mitigation |
|---|---|---|
| Over-documentation | The system becomes heavier than the problem | Line budgets; create only the files listed; review at one cycle |
| Losing historical nuance during migration | Future decisions lose context | Move, do not rewrite; mapping tables; archive originals |
| Task schema too heavy for small work | Agents avoid it | Allow a one-line task for P3/trivial work; full template only for multi-session/card work |
| CI noise or cost | Operator disables it | Cardless only; cache; run on push/PR; keep local gate authoritative |
| Decision ledger summary changes meaning | Agents follow a weakened rule | Keep exact wording for binding clauses; card links to full text |
| Parallel model chosen without discipline | Ledger corruption | Default to serial; require task ownership before enabling parallel |
| Migration stalls halfway | Two homes, worse than before | Phase gates; don't start Phase 2 until Phase 1 exit criteria pass |
| Backups are not actually run | Single point of failure remains | Make D9 a precondition, not a task |

---

## 11. Approval checklist

Approved by the owner on 2026-11-02:

- [x] D1: Track the durable core? Repository private?
- [x] D2: Move `DECISIONS.md` and add `DOCTRINE.md`?
- [x] D3: Reclassify `DESIGN.md` as historical?
- [x] D4: Approve file moves and redirects?
- [x] D5: Maximum two working subagents; delete stale worktrees and old build artifacts?
- [x] D6: Narrative commit style?
- [x] D7: Allow cardless CI?
- [x] D8: Keep `GOTCHAS.md`/`FINDINGS-OPEN.md` local?
- [x] D9: Approve an independent backup?
- [x] D10: Approve version/changelog/tag coupling?
- [x] D11: Adopt ASD-STE100 for durable documentation?
- [x] Approve Phase 1 scope and start date.
- [x] Confirm no product behavior change is authorized by this plan.
