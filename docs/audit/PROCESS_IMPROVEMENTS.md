# Process Improvement Recommendations

| Field | Value |
|---|---|
| Status | Proposal - requires owner approval |
| Date | 2026-11-02 |
| Inputs | `docs/audit/AUDIT_REPORT.md`, `docs/audit/TARGET_STRUCTURE.md` |
| Scope | Agent workflow, instructions, Git, review, task lifecycle, planning, handoff, documentation lifecycle, technical debt, QA/automation, parallel work, release, security, onboarding |
| Constraint | No existing file is changed by this document. Implementation is a separate, approved phase. |

Each section follows the same shape: **current problem -> proposed solution -> expected benefits -> measures -> audit references**.

---

## 1. Agent workflow and autonomy

### 1.1 Current problem

An agent starts with `AGENTS.md` (13 KB of mostly generic Rust/Python/Web/TUI rules). That file does not name `HANDOFF.md`, the queue, the doctrine ledger, the gotcha ledger, or the review protocol. The actual working rules are scattered across five files. There is no autonomy matrix. The most dangerous actions (card tests, live harness, pushes, tags, deletions) are guarded only by prose in documents the agent may never read.

### 1.2 Proposed solution

**A. One entry point.** Rewrite `AGENTS.md` as a 150-200 line routing document:

1. Project in five lines.
2. "Start here" chain.
3. Precedence (see `TARGET_STRUCTURE.md` Section 7).
4. Required reading by task type.
5. Autonomy table.
6. Safety fences.
7. Definition of done.
8. Conflict rule.
9. Links to all process docs.

**B. Autonomy table.** Use three columns:

| Action | Default | Condition |
|---|---|---|
| Read code, docs, evidence | Allowed | Always |
| Edit code/tests in task scope | Allowed | Task file exists or operator named the scope |
| Run cardless gate | Allowed | No engine spawn |
| Add/remove dependency | Requires approval | Show need, size, alternative |
| Commit locally | Requires approval | Unless the task explicitly authorizes it |
| Push, tag, release | Requires approval | Always |
| Delete/rename files | Requires approval | Always |
| Run card tests / load models | Requires approval | Only operator-freed card, never from a served session |
| Touch live harness / protected ports | Forbidden | Unless operator explicitly hands over |
| Change doctrine/decisions | Requires approval | Operator ruling recorded |
| Start a second subagent | Requires approval | See Section 11 |

**C. Session start.** Every session, including a resumed one, follows:

```text
AGENTS.md
 -> HANDOFF.md Section 2 (snapshot) and Section 4 (next task)
 -> handoff/PENDING_CHECKLIST.md row
 -> handoff/tasks/<ID>.md (if it exists)
 -> docs/architecture/DOCTRINE.md
 -> handoff/GOTCHAS.md (for technical tasks)
 -> task-specific documents from the reading matrix
```

**D. Session end / interruption.** Run the handoff protocol in Section 7 before the context is lost.

**E. State the non-goals.** The agent is not a co-owner of doctrine, does not choose scope, does not approve its own work, and does not decide that a test is unnecessary because it is expensive.

### 1.3 Expected benefits

- A new agent can find the rules in one hop.
- Dangerous actions are explicit, not inferred.
- Routine tasks read far less context.
- The operator spends less time correcting process drift.
- Reviewers can cite the autonomy rule instead of arguing.

### 1.4 Measures

- Time from session start to first correct action.
- Number of process corrections per session.
- Zero unauthorized pushes/tags/card runs.
- Required-reading set size per task type (target: <= 4 documents for routine code).

### 1.5 Audit references

`GOV-02`, `GOV-03`, `GOV-05`, `GOV-07`, `PRO-03`.

---

## 2. Instruction architecture and context budget

### 2.1 Current problem

`AGENTS.md` is a generic cookbook; the project-specific rules are elsewhere. The documented start chain reads `HANDOFF.md` -> `PENDING_CHECKLIST.md` -> `DESIGN.md`, which is about 124 KB before any code; subagents must additionally read `GOTCHAS.md` and carry the full `DECISIONS.md` LOCKED section, taking the mandatory context to roughly 180 KB (~45k tokens). This is a large fixed cost on every task and pushes agents toward skimming.

### 2.2 Proposed solution

**A. Three-tier reading model.**

| Tier | Content | When |
|---|---|---|
| Tier 1 - always | `AGENTS.md`, `DOCTRINE.md`, task file | Every session |
| Tier 2 - by task | `ENGINEERING.md`, `VERIFICATION.md`, `OVERVIEW.md`, `GOTCHAS.md`, `MODELS.md` | According to the reading matrix |
| Tier 3 - reference | `DECISIONS.md`, `DESIGN.md`, full evidence, archives | Only when resolving a specific question |

**B. Replace the "read the whole ledger" rule.** Subagent briefs carry the short `DOCTRINE.md` card and a link to `DECISIONS.md`; they do not paste 30 KB. The brief names the specific decision rows relevant to the task.

**C. Context budgets.** Enforce line budgets from `TARGET_STRUCTURE.md` Section 7.4. If a doc exceeds its budget, split it or move detail to a reference.

**D. Durable vs live.** Do not put task narratives or evidence in documents that are read on every session. The queue is an index, not a chronicle.

**E. Remove unused stack rules** from the mandatory path: `polars`, `ratatui`, `dioxus`, `maturin`, `secrecy`, `indicatif`. Keep them in an appendix labeled "only if this stack is present".

### 2.3 Expected benefits

- Routine tasks read roughly one quarter of the current context.
- Agents retain more of the rules that matter.
- Subagent briefs become cheaper and more precise.
- The doctrine remains authoritative without being a tax.

### 2.4 Measures

- Bytes/lines in the Tier 1 set.
- Percentage of tasks that need Tier 3.
- Context size of a typical subagent brief (target: < 8 KB including links).

### 2.5 Audit references

`GOV-06`, `GOV-07`, `GOV-08`, `PRO-11`.

---

## 3. Git workflow

### 3.1 Current problem

The written rules describe a generic small-commit, imperative-subject workflow. The actual repository is trunk-only on `main`, with direct commits, narrative subjects, multi-finding commits, tags for releases, and no PRs or CI. `GIT_RULES.md` does not mention branch naming, worktrees, merge, conflict, hotfix, or release versioning. A stale worktree branch exists.

### 3.2 Proposed solution

**A. Document the actual model.** Create `docs/process/GIT_WORKFLOW.md` with:

- **Branch model:** trunk-based. `main` is always releasable. Work happens on `main` for single-operator serial work.
- **When branches are allowed:** only for parallel work or risky experiments, named `<type>/<task-id>-<slug>` (for example `feat/TASK-042-engine-basalt`).
- **Worktrees:** allowed only when the branch has a task file and a named owner; no stale worktree older than 7 days; preflight checks that durable docs exist and the base commit is current.
- **Commits:** one logical change per commit; the house style is a narrative subject that states the outcome (up to 72 characters preferred, 100 hard maximum for normal commits; long review-round subjects are allowed only with a body); body carries finding IDs, falsifiers, and rationale.
- **Staging:** explicit paths; never `git add -A`; inspect `git diff --cached`.
- **Review:** subagent review before merge to `main` for non-trivial changes; the review artifact lives in evidence, not in Git.
- **Protected operations:** force-push, reset, amend, history rewrite, branch/tag deletion, and release actions require explicit operator approval.
- **Release:** tag only after the gate, version bump, changelog entry, README update, and artifact hash.
- **Conflicts:** rebase local branches on `main`; never force-push a shared branch; if a conflict touches doctrine or a ledger, stop and ask.
- **Stale worktrees:** quarterly cleanup; `git worktree prune`; delete only after confirming no uncommitted work.

**B. Align the rule and the history.** Either update `GIT_RULES.md` to describe the narrative style, or change practice to imperative subjects. Recommendation: keep the narrative style because it carries rationale and matches the review culture; document it explicitly with a length limit.

**C. Add a commit template** (`.gitmessage` or documented in `GIT_WORKFLOW.md`) with subject, finding IDs, falsifier note, and gate evidence.

### 3.3 Expected benefits

- Agents stop producing commits that violate house style.
- Parallel work has an explicit path.
- Release steps are reproducible.
- Stale worktrees stop competing with `main`.

### 3.4 Measures

- Commits rejected by review for style: target zero.
- Worktrees older than 7 days: target zero.
- Releases with version/changelog/tag mismatch: target zero.

### 3.5 Audit references

`PRO-04`, `GOV-06`, `PRO-09`, `PRO-11`.

---

## 4. Code review

### 4.1 Current problem

The review process is the strongest part of the project, but it is hidden in `AUX-RULES.md` and `handoff/code-review/`. `AGENTS.md` does not mention it. Closure rules are strict for findings but not for ordinary tasks. Reviewers have a clear prompt but no durable process document.

### 4.2 Proposed solution

**A. Create `docs/process/REVIEW.md`** containing:

- When review is required (all non-trivial code, all doctrine changes, all release candidates).
- Reviewer selection (writer-capable agent, not the author).
- Chunking and lenses.
- Read-only constraints.
- Finding format and severity.
- Falsifier requirement: a fix must include a test that fails on the pre-fix code.
- Finding classes (`[S]`, `[R]`, `[C]`, `[O]`).
- Closure: fixed by named commit or accepted by named ruling.
- Accepted-risk rule: an accepted finding gets a permanent row with rationale and owner.
- Evidence retention.
- Reviewer honesty rules: no invented results, distinguish static read from executed proof.

**B. Make review part of the task definition of done.** A task with open findings is not done. This is already true for chunks; extend it to all tasks.

**C. Keep the reviewer prompt** as an example in `REVIEW.md` or in `handoff/evidence/reviews/PROMPT.md`, but the durable rules live in the process doc.

**D. Do not require review for:** typo fixes, comment-only changes, and evidence-only updates. State this explicitly to avoid ritual review.

### 4.3 Expected benefits

- Review quality is preserved and discoverable.
- Authors know the closure bar before starting.
- Findings cannot be closed by assertion.
- Review cost is proportional to risk.

### 4.4 Measures

- Findings closed without a commit or ruling: target zero.
- Fixes without a falsifier: target zero for code findings.
- Review turnaround per task.

### 4.5 Audit references

`PRO-10`, `PRO-02`, `AUX-RULES.md:15-29`.

---

## 5. Task lifecycle

### 5.1 Current problem

There is no unified task schema. Queue rows are prose; some contain commit hashes, gates, dependencies, and multi-paragraph narratives. There is no required acceptance criterion, no owner, no next step, and no standard relation to findings or decisions. `[x]` is the only closure signal.

### 5.2 Proposed solution

**A. Adopt the task template** from `TARGET_STRUCTURE.md` Section 6.1. Required fields:

- ID, title, class, priority, readiness, owner, dates.
- Goal, scope, non-goals.
- Acceptance criteria (falsifiable checklist).
- Dependencies (blocks/blocked by, environment).
- Decisions (links only).
- Progress: done, remaining, failed approaches, blockers.
- Files changed.
- Evidence links.
- Next step (exact command).

**B. Lifecycle states.**

```text
proposed -> ready -> in-progress -> review -> verification -> accepted -> documented -> archived
                        |             |
                        +-> blocked   +-> changes-requested
```

- **proposed:** idea, not ready (missing scope or decision).
- **ready:** has scope, acceptance, dependencies resolved, owner.
- **in-progress:** one active task per session.
- **review:** code complete, awaiting review.
- **verification:** fixes applied, gate and acceptance evidence being collected.
- **accepted:** acceptance criteria met, gate passed, docs updated.
- **documented:** changelog/decision/README updated as needed.
- **archived:** moved to `handoff/archive/<date>/`.

**C. Definition of Ready.** A task may start only when: the goal is one paragraph, scope and non-goals are named, acceptance is falsifiable, dependencies are resolved, the environment class is known (`S/R/C/O`), and a decision exists for any open design fork.

**D. Definition of Done.** All acceptance criteria checked; gate passed with evidence; docs updated; review findings closed or accepted; task file updated; queue row updated; evidence linked; no uncommitted unrelated changes.

**E. Priority and readiness.** Add `P0` (release blocker/security), `P1` (current cycle), `P2` (next), `P3` (backlog). Readiness is separate: `ready`, `blocked`, `needs-design`, `needs-operator`. Classes remain execution constraints.

**F. Queue discipline.** Queue rows are one line. Narratives move to task files or evidence. Closed tasks move to archive after the release that contains them.

### 5.3 Expected benefits

- A task can be handed to another agent without rediscovery.
- "Done" is auditable.
- The queue stays readable.
- Priorities and blockers are visible.
- Card work cannot be closed on cardless evidence by accident.

### 5.4 Measures

- Percentage of active tasks with complete required fields (target 100%).
- Median queue row length (target one line).
- Tasks reopened after closure (target decline).
- Time to onboard an agent onto an active task.

### 5.5 Audit references

`PRO-01`, `PRO-02`, `PRO-05`, `PRO-08`.

---

## 6. Planning and prioritization

### 6.1 Current problem

The queue mixes features, verification, cleanup, research, and operator chores in narrative order. Classes describe execution prerequisites, not value. There is no capacity model, no cycle planning, and no explicit "not now" list. The backlog includes a full new engine next to a README compliance rewrite.

### 6.2 Proposed solution

**A. Lanes.** Maintain four views over the same task IDs:

| Lane | Meaning |
|---|---|
| Now | Current cycle, one active task at a time |
| Next | Ready, ordered, not started |
| Blocked | Waiting on a ruling, a card window, a dependency, or a release |
| Later | Accepted direction, not scheduled |

**B. Prioritization rules.** Order by: safety/release blocker -> unblocking others -> user-visible defect -> debt that blocks future work -> feature -> polish. Use `P0-P3`; do not use class as priority.

**C. Capacity.** The operator has one card window and limited attention. Card tasks are batched into one window, never five. Cardless work fills the gaps. State this in `TASKS.md`.

**D. Cycle planning.** At the start of a cycle, select a small set of tasks that fit the available card windows and operator time. At the end, archive completed work and record what slipped and why.

**E. Research spikes.** Time-box them; produce a decision-ready report with options, recommendation, and a date. Do not let a spike become an unfunded project.

### 6.3 Expected benefits

- The next task is obvious.
- Expensive card work is batched.
- Cheap high-value work is not starved by a large feature.
- Scope creep is visible.

### 6.4 Measures

- Cycle completion rate.
- Number of card windows used per cycle.
- Age of P0/P1 items.
- Percentage of "Later" items that are intentionally deferred vs forgotten.

### 6.5 Audit references

`PRO-08`, `PRO-06`, `GOV-06`.

---

## 7. Handoff protocol

### 7.1 Current problem

The closing protocol is good but only runs at session end. There is no checkpoint before context compaction, no task-scoped state, no dirty-tree format, and no guard against parallel sessions overwriting the same ledgers. The protocol is also entirely local and unversioned.

### 7.2 Proposed solution

**A. Triggers.** Run the handoff checkpoint:

1. At session start (read).
2. Before context compaction or a long-running operation.
3. Before switching agents or models.
4. Before ending a session, clean or dirty.
5. Before a card window or any operation the next session cannot repeat.

**B. Artifact.** One task file per active task (`handoff/tasks/<ID>.md`) with the template in `TARGET_STRUCTURE.md` Section 6.1. The queue row links to it.

**C. Required fields.** Status, done, remaining, failed approaches, decisions, blockers, files changed, evidence, constraints, next command. If a field is empty, write "none" rather than omitting it.

**D. Dirty-tree protocol.** If work is uncommitted:

- Record the exact files and why they are uncommitted.
- Record whether they compile and what was last run.
- Record the next action.
- Do not commit merely to make the tree clean unless the task authorizes a checkpoint commit.
- Never let another session edit the same files without an explicit handoff.

**E. Read protocol.** The next session reads `HANDOFF.md`, the queue row, the task file, and only the task-specific references. It confirms the tree state against the task file before editing.

**F. Conflict protocol.** If the task file and `git status` disagree, stop and reconcile before writing. If two sessions are active, the task file names the owner; the non-owner must not edit.

**G. Durability.** The protocol lives in `docs/process/HANDOFF.md` (tracked). The live state stays local and is backed up. The protocol is not copied into every brief; the brief links to it.

**H. Handoff types.** The same artifact serves session->session, agent->agent, human->agent, and agent->human. For a human, add a short "start here" line at the top.

### 7.3 Expected benefits

- Work can be interrupted at any point without losing intent.
- Failed approaches are not repeated.
- Parallel sessions have an ownership rule.
- A new agent can resume with a bounded reading list.
- The operator can see status without reading a narrative.

### 7.4 Measures

- Percentage of active tasks with a current task file (target 100%).
- Resume time to first correct action.
- Repeated failed approaches (target zero).
- Handoff-related questions per session.

### 7.5 Audit references

`PRO-03`, `GOV-10`, `PRO-05`.

---

## 8. Documentation lifecycle

### 8.1 Current problem

Documents have no owner, status, audience, or review trigger. Stale claims survive until a user or auditor finds them. Durable and live content are mixed. Duplicates are created because there is no declared home. There is no changelog and no release documentation step.

### 8.2 Proposed solution

**A. Front matter** for every durable doc: title, owner, status, audience, source-of-truth topics, last reviewed, review trigger.

**B. Routing table** in `docs/README.md`; every doc is listed; no orphans.

**C. Same-change rule.** A behavior change updates affected docs in the same commit or records why no doc is affected.

**D. Contradiction protocol.** Stop, record a finding, fix the authoritative source, replace copies with pointers.

**E. Release documentation gate.** Version, changelog, README release claim, reference docs, doctrine. No tag without it.

**F. Review cadence.** Release, quarterly, and event-driven (new engine, new decision, incident).

**G. Archive policy.** Superseded durable docs get a banner and move to `docs/archive/`; live state moves to `handoff/archive/<date>/`; evidence is retained with an index.

**H. Anti-duplication audit.** At each release, search for repeated command lists, doctrine paragraphs, schemas, and test counts. Replace copies with pointers.

**I. Line budgets.** Enforce the budgets in `TARGET_STRUCTURE.md` Section 7.4.

### 8.3 Expected benefits

- Stale documentation is detected at release, not by users.
- New contributors know where to write.
- The repository stays navigable as it grows.
- The documentation set stops expanding without control.

### 8.4 Measures

- Documents with missing front matter: target zero.
- Broken links/paths: target zero.
- Stale claims found at release: declining.
- Documents over budget: target zero.

### 8.5 Audit references

`DOC-02`, `DOC-03`, `DOC-04`, `DOC-05`, `DOC-07`, `DOC-08`, `PRO-14`.

---

## 9. Technical debt management

### 9.1 Current problem

Debt lives in three places: open findings, queue leftovers, and deferred decisions. There is no aggregate view, no aging, and no owner. A finding can be "accepted" and then disappear from attention. A deferred decision has a tripwire but no scheduled review.

### 9.2 Proposed solution

**A. One debt index.** Either:

- Extend `FINDINGS-OPEN.md` with a `debt` class and make it the single register, or
- Create `handoff/DEBT.md` as an index that links to findings/decisions/tasks.

Recommendation: extend `FINDINGS-OPEN.md`; avoid a fourth ledger.

**B. Required fields.** ID, severity, category, impact, evidence, owner, target release, status, age, linked task.

**C. Intake rule.** New debt is recorded at the moment it is found, with a severity. If it is not worth recording, it is not debt; it is an opinion.

**D. Triage.** At each cycle start, review debt older than one release. Decide: fix now, schedule, accept with rationale, or close as obsolete.

**E. Accepted risk.** Acceptance requires a named ruling and a permanent row. It does not delete the item.

**F. Coupling to features.** A feature that creates known debt must name the debt and the repayment trigger.

### 9.3 Expected benefits

- Debt is visible and can be prioritized against features.
- Accepted risk is explicit, not forgotten.
- Aging is measurable.
- Fewer duplicate records.

### 9.4 Measures

- Open debt count by severity and age.
- Percentage of debt with an owner and target.
- Debt older than two releases.
- Accepted risks reviewed at each release.

### 9.5 Audit references

`PRO-06`, `PRO-05`, `DOC-02`.

---

## 10. Quality assurance and automation

### 10.1 Current problem

The gate is manual and defined four times. There is no CI, no link checker, no template validator, and no check that durable docs match behavior. The project relies on agent discipline and the operator's memory. A commit can be declared green against the weakest definition.

### 10.2 Proposed solution

**A. One gate script.** `scripts/gate.ps1` (or `cargo xtask gate`) is the only executable definition. It runs:

```powershell
cargo check --all-targets
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
node tests/webui/webui_dom_test.mjs
```

It prints the counts and exits non-zero on failure. `VERIFICATION.md` describes it; no other document lists commands.

**B. Documentation checker.** `scripts/check-docs.mjs` (Node is already required for the DOM test) validates:

- Every durable Markdown file has required front matter.
- Every relative link resolves.
- Every path referenced in backticks exists (with an allowlist for planned files).
- No duplicate H1.
- No `DRAFT` status on a released document.
- No absolute machine paths in durable docs.
- Every doc is listed in `docs/README.md`.
- No duplicated gate command block.

**C. Template validator.** `scripts/check-tasks.mjs` validates active task files and queue rows against the schema: required fields, valid class/priority/readiness, acceptance criteria non-empty, evidence link present for closed tasks.

**D. Instruction consistency checker.** `scripts/check-instructions.mjs` verifies that `AGENTS.md` links resolve, that the precedence section exists, that the gate is referenced rather than copied, and that no retired file path is cited.

**E. CI.** `.github/workflows/ci.yml` on `windows-latest`:

- Trigger: push to `main`, pull request.
- Steps: checkout, Rust toolchain, cache, `cargo check`, `cargo test`, `cargo clippy`, `cargo fmt --check`, Node DOM test, docs checker, task checker.
- Card tests are excluded and explicitly named as operator-only.
- If the repository must remain private, CI is still valid; no secrets are needed for these checks.

**F. Pre-commit hook (optional).** A lightweight `cargo fmt --check` + docs checker. Do not make hooks mandatory; CI is the authority.

**G. Release check.** `scripts/release-check.ps1` verifies version, tag, changelog, README claim, and artifact hash.

### 10.3 Expected benefits

- The gate cannot be misquoted.
- Documentation rot is caught mechanically.
- Task and finding templates are enforced.
- Agents get fast, unambiguous feedback.
- The operator can trust a green CI badge.

### 10.4 Measures

- CI pass rate.
- Commits that fail CI after a local green claim (target zero).
- Broken links found in CI per month (target zero after cleanup).
- Gate definition copies in durable docs (target one).

### 10.5 Audit references

`GOV-04`, `PRO-07`, `PRO-11`, `PRO-14`.

---

## 11. Parallel agents and worktrees

### 11.1 Current problem

The written rule allows only one active subagent, but worktrees and parallel session state exist. A stale worktree has older code and no rules. There is no file ownership or merge protocol, and the live ledgers are shared mutable files.

### 11.2 Proposed solution

Choose one model explicitly.

**Model A - Serial (recommended for the current single-operator setup).**

- One active writer session at a time.
- Read-only research subagents may run in parallel.
- Delete all worktrees and pool entries after confirming no work.
- State the rule in `AGENTS.md` and `GIT_WORKFLOW.md`.
- Benefit: no merge protocol, no ledger races, simplest correct behavior.

**Model B - Parallel with ownership.**

- Each worktree gets a task file and an owner.
- File ownership is declared in the task file; no two tasks edit the same file.
- The live queue and `HANDOFF.md` are updated only by the owner at checkpoint; other sessions write only their task file.
- Merge order is defined; conflicts are resolved by the task owner with review.
- Durable docs are updated only after merge.
- A preflight verifies that the worktree has the durable docs and the correct base commit.
- Benefit: concurrency; cost: coordination overhead and merge risk.

**Regardless of model:**

- `git worktree list` must be clean of stale entries before a cycle starts.
- `.kilo/agent-manager.json` is tool state, not a source of truth.
- A worktree without `AGENTS.md` is not a valid workspace; either track the docs or copy them at creation.

### 11.3 Expected benefits

- No silent divergence.
- No shared-ledger corruption.
- Worktree disk and cognitive overhead disappear (Model A) or become controlled (Model B).

### 11.4 Measures

- Stale worktrees (target zero).
- Merge conflicts per cycle.
- Ledger overwrite incidents (target zero).

### 11.5 Audit references

`GOV-06`, `GOV-10`, `PRO-03`.

---

## 12. Release management

### 12.1 Current problem

Release close-out is prose. Version, changelog, README, tag, and deployed artifact are not coupled. The README claims v1.0.1 while HEAD is 11 commits later. There is no changelog.

### 12.2 Proposed solution

**A. Release task template.** A release is a task with acceptance criteria:

- Gate green on the release commit.
- `Cargo.toml` version bumped.
- `CHANGELOG.md` entry written.
- README release claim updated to the tag.
- Reference docs updated.
- Annotated tag created.
- Artifact built, hashed, deployed.
- Rollback artifact retained and named.
- Post-deploy verification recorded.

**B. Version policy.** Semantic versioning for the user-facing product. Post-release commits do not change the "latest release" claim; they are "unreleased" until the next version.

**C. Changelog policy.** Keep a Changelog format; user-visible changes only; link to commit/tag; include artifact hash when deployed.

**D. Release check script.** Fails if version/tag/changelog/README disagree.

**E. No tag without a changelog.** This is the single rule that prevents most release drift.

### 12.3 Expected benefits

- Users can see what changed.
- Releases are reproducible.
- Rollback is explicit.
- "Current build" is unambiguous.

### 12.4 Measures

- Releases with complete changelog (target 100%).
- Version/tag mismatches (target zero).
- Time to reconstruct a release.

### 12.5 Audit references

`PRO-09`, `DOC-05`.

---

## 13. Security and publication safety

### 13.1 Current problem

The project plans to commit some local artifacts. Those artifacts contain machine paths, ports, key-file names, container names, and incident logs. There is no scrub checklist. The "no auth" stale comment also weakens security understanding.

### 13.2 Proposed solution

**A. `docs/process/PUBLICATION.md`** with a checklist:

- Remove absolute paths, usernames, hostnames, IP addresses, ports.
- Remove token/key names and any secret material.
- Remove container names and model paths that identify the machine.
- Review logs for prompts, completions, PII.
- Confirm repository visibility.
- Require a second reviewer for any publication of incident evidence.
- Record what was scrubbed and why.

**B. Durable docs must contain no machine paths.** Enforce with the docs checker.

**C. Security claims must be verified against code.** The "no auth" comment is a defect; fix it and add a doc check for known stale phrases.

**D. Keep secrets out of code.** Existing rule in `AGENTS.md` is good; extend it to local evidence and screenshots.

### 13.3 Expected benefits

- No accidental leak when the repo becomes shareable.
- Security documentation matches reality.
- Incident evidence can be published safely.

### 13.4 Measures

- Absolute paths in durable docs (target zero).
- Secret-scan findings (target zero).
- Security claims verified at release.

### 13.5 Audit references

`PRO-12`, `DOC-03`, `GOV-01`.

---

## 14. Onboarding

### 14.1 Current problem

A new contributor clones 57 files and a README. There is no `CONTRIBUTING.md`, no docs index, no task guide, and no explanation of the local-only system. A new agent on a fresh machine has even less: no `AGENTS.md` at all, because it is ignored.

### 14.2 Proposed solution

**A. Tracked onboarding path:**

```text
README.md
 -> CONTRIBUTING.md
 -> docs/README.md
 -> docs/process/TASKS.md
 -> docs/architecture/OVERVIEW.md
 -> docs/process/VERIFICATION.md
```

**B. A "first task" walkthrough** in `TASKS.md`: pick a P3 doc fix, create a task file, run the gate, request review, close, archive. This teaches the system by using it.

**C. A "fresh clone" check** in CI or a script: clone to a temp directory and verify that the onboarding path files exist and links resolve. This catches the exact failure this audit found.

**D. Explain the local layer.** `CONTRIBUTING.md` states that live state and evidence are machine-local, where they live, and how to ask for them if needed.

### 14.3 Expected benefits

- A new contributor can become productive without the operator.
- A new agent on a new machine has rules.
- The repository becomes self-describing.

### 14.4 Measures

- Time to first merged change for a new contributor.
- Fresh-clone onboarding check passes.
- Onboarding questions per new contributor.

### 14.5 Audit references

`GOV-01`, `GOV-02`, `DOC-04`.

---

## 15. Recommended sequence

Do not implement all sections at once. The dependency order is:

1. **Decide the owner questions** (tracked vs local, DESIGN status, worktree model, commit style, CI).
2. **Fix false documentation** (README strata, code comments, DESIGN banner). This is independent and high value.
3. **Create the durable core** (`AGENTS.md` routing, `DOCTRINE.md`, `VERIFICATION.md`, `GIT_WORKFLOW.md`, `TASKS.md`, `HANDOFF.md`, `docs/README.md`).
4. **Migrate live ledgers** (task files, queue index, decisions move).
5. **Add automation** (gate script, docs checker, task checker, CI).
6. **Adopt the workflow** for one cycle and measure.
7. **Refine** based on actual friction; do not add more documents without evidence.

The detailed task sequence, acceptance criteria, and risks are in `IMPLEMENTATION_PLAN.md`.

---

## 16. What not to do

- Do not create a document for every module, decision, or meeting.
- Do not copy the full decision ledger into every subagent brief.
- Do not keep both `GIT_RULES.md` and `GIT_WORKFLOW.md` as authoritative.
- Do not track machine paths, evidence logs, or live state without a scrub.
- Do not add CI that runs card tests or starts engines.
- Do not enforce a commit style that the operator does not want to follow.
- Do not delete historical design rationale; reclassify it.
- Do not replace human judgment with a template. The template exists to make judgment visible, not to remove it.
