# Documentation and Process Backlog

| Field | Value |
|---|---|
| Status | Ready for issue-tracker import after owner approval |
| Date | 2026-11-02 |
| Source | `docs/audit/AUDIT_REPORT.md` |
| Ordering | Priority first, then dependency. Do not start an item whose `Blocked by` is unresolved. |
| Labels | `docs`, `agent`, `process`, `git`, `automation`, `security`, `onboarding` |

Priorities: **P0** = false or dangerous instructions / release blocker; **P1** = current cycle; **P2** = next; **P3** = later.

---

## Slice 1 - Stop the false statements

### BL-01 - Rewrite the README strata documentation for template mode

- **Priority:** P0
- **Category:** docs
- **Related audit:** DOC-01, DOC-02
- **Description:** Replace the `strata_config`-required model, the `--config <strata_config>` launch description, the "per-model profile JSON" stack requirement, and the "bind comes from profile host" paragraph with the shipped template-mode model: `strata_exe`/`strata_args`/`strata_python`/`strata_cwd`, `strata_*` knobs, rendered profile root, import bridge, forced loopback, assigned port, and the migration sentence.
- **Definition of Done:** A new user can configure and launch a strata row from the README alone. No sentence claims `strata_config` is required. No sentence says strata controls its own bind. A reviewer checks every strata claim against `src/model/scan.rs`, `src/engine/strata.rs`, and `DECISIONS.md` rows 19-19d. A docs test/lint prevents the phrase "Required for strata" next to `strata_config`.

### BL-02 - Fix stale code doc comments

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-03, PRO-11
- **Description:** Update `src/lib.rs:4-6` (modules are present) and remove the "there is no auth" claim from `src/config/mod.rs:390` and `:1034`; align `DESIGN.md:150` if it remains current for the cycle.
- **Definition of Done:** No production doc comment claims a shipped module is absent. No durable doc says authentication does not exist. `cargo doc` is clean. The description matches the token behavior documented in README.

### BL-03 - Reclassify `DESIGN.md` as a historical record

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-02
- **Description:** Add a top banner stating that `DESIGN.md` is the historical design record; current architecture is `docs/architecture/OVERVIEW.md`; binding decisions are `docs/architecture/DECISIONS.md`. Mark the strata copy-in addendum as superseded by ruling 19. Change "DRAFT for operator review" to "Historical".
- **Definition of Done:** Banner is the first content after the title. The addendum names row 19 and states it is superseded. `HANDOFF.md` no longer calls `DESIGN.md` the unqualified "spec of record". No session-start chain requires reading the full file.

### BL-04 - Make `AGENTS.md` the single agent entry point

- **Priority:** P0
- **Category:** agent
- **Related audit:** GOV-02, GOV-03
- **Description:** Add a 5-line project orientation, "start here" chain, numbered precedence, conflict rule, required-reading matrix, safety fences, definition of done, and links to every process document. Keep the file <= 200 lines by moving generic standards later.
- **Definition of Done:** An agent given only `AGENTS.md` can name the queue, doctrine, gotcha ledger, gate, review protocol, and card prohibition. Precedence is explicit. A link check passes. The file is under budget.

### BL-05 - Add the autonomy and approval matrix

- **Priority:** P0
- **Category:** agent
- **Related audit:** GOV-05
- **Description:** Add one table covering read/edit/test, gate, dependency changes, commits, pushes, tags, releases, deletions, card tests, model loads, live-harness actions, doctrine changes, and subagents. State allowed, requires approval, and forbidden.
- **Definition of Done:** Every dangerous action in the project has an explicit row. The card and live-harness prohibitions are in the table, not only in `HANDOFF.md`. A reviewer can resolve an action without reading five files.

### BL-06 - Define the gate exactly once

- **Priority:** P0
- **Category:** process
- **Related audit:** GOV-04, PRO-11
- **Description:** Create `docs/process/VERIFICATION.md` with the cardless commands, card rules, evidence requirements, and pass/fail/blocked vocabulary. Replace the gate blocks in `AGENTS.md`, `AUX-RULES.md`, `PENDING_CHECKLIST.md`, and `DESIGN.md` with pointers. Remove embedded counts.
- **Definition of Done:** Only one durable document lists the commands. All other mentions are links. The commands match the current gate. No test count appears in a rule. A reviewer confirms all four old locations were handled.

### BL-07 - Remove stale worktrees and enforce a preflight

- **Priority:** P1
- **Category:** git
- **Related audit:** GOV-06, GOV-10
- **Description:** Inspect `brassy-mosquito` and the two detached pool entries, back up any uncommitted work, remove stale worktrees, and define a preflight that requires durable docs and a current base commit.
- **Definition of Done:** `git worktree list` shows no stale entries. A worktree without `AGENTS.md` is declared invalid. The removal is recorded with the branch commit. No uncommitted work is lost.

### BL-08 - Align the commit rule with the actual house style

- **Priority:** P1
- **Category:** git
- **Related audit:** PRO-04
- **Description:** Replace the "concise imperative subject" rule with the narrative-outcome style used by the repository, including a length limit and required body fields (finding IDs, falsifier, rationale). Explain the long review-round exception or forbid it and change future practice.
- **Definition of Done:** The rule and the last 10 commits are mutually consistent. A new contributor can write a compliant subject and body from the rule alone. No document still requires imperative subjects.

### BL-09 - Separate latest release from repository HEAD and add a changelog

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-05, PRO-09
- **Description:** Change the README release claim to "Latest release: v1.0.1; HEAD may be ahead." Add `CHANGELOG.md` with v1.0.0 and v1.0.1 entries and the version/tag/changelog rule.
- **Definition of Done:** README is unambiguous. Changelog has dated entries with links. `Cargo.toml` version matches the latest tag. The release rule is stated once.

---

## Slice 2 - Establish the durable architecture

### BL-10 - Create the docs index, front matter, and layer model

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-04, GOV-01
- **Description:** Create `docs/README.md` with the routing table and durable/live/evidence/archive model. Add required front matter (`title`, `owner`, `status`, `audience`, `source_of_truth_for`, `last_reviewed`, `review_trigger`) to every durable doc. Add `CONTRIBUTING.md`.
- **Definition of Done:** Every durable doc is listed and has front matter. No orphan docs. A fresh reader can find any information type in one hop. `CONTRIBUTING.md` is a short pointer, not a duplicate.

### BL-11 - Extract engineering standards from `AGENTS.md`

- **Priority:** P1
- **Category:** docs
- **Related audit:** GOV-07, GOV-08
- **Description:** Move generic coding/testing/doc/security/platform rules to `docs/process/ENGINEERING.md`. Remove unused stack mandates from the mandatory path and resolve the comment/`unwrap`/optimization tensions.
- **Definition of Done:** `AGENTS.md` <= 200 lines. No rule is lost without a replacement. Unused stacks are not project mandates. The tensions have explicit resolutions. A rule-mapping table was reviewed.

### BL-12 - Move the decisions ledger and add a doctrine card

- **Priority:** P1
- **Category:** agent
- **Related audit:** GOV-01, GOV-03, PRO-05
- **Description:** Move `handoff/DECISIONS.md` to `docs/architecture/DECISIONS.md` (if D2 approved) and create `docs/architecture/DOCTRINE.md` with the 12-20 binding invariants and one-line consequences. Point `HANDOFF.md` and subagent briefs at the card.
- **Definition of Done:** Every LOCKED invariant appears in the card with a pointer to the ledger row. No ledger row is lost. The card is <= 120 lines. A subagent brief can rely on the card plus specific row links instead of the full 30 KB ledger.

### BL-13 - Write the current architecture overview

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-02, DOC-04
- **Description:** Create `docs/architecture/OVERVIEW.md`: system context, scheduler/actor, admission/witness, engine adapters, L1/L2/L3 config, surfaces, module map, data flow, invariants, code links.
- **Definition of Done:** A new agent can understand the current system without `DESIGN.md`. No superseded strata path. Every claim is verified against code. <= 300 lines. Review trigger set to release.

### BL-14 - Consolidate `GIT_RULES.md` and `AUX-RULES.md`

- **Priority:** P1
- **Category:** process
- **Related audit:** GOV-03, PRO-04, PRO-10
- **Description:** Create `docs/process/GIT_WORKFLOW.md`, `TASKS.md`, `REVIEW.md`, and `PUBLICATION.md` from the durable parts of both files. Leave root redirects for one cycle or retire them.
- **Definition of Done:** No rule exists authoritatively in two places. Root files, if kept, are <= 15 lines and point to the new home. A rule-by-rule mapping is reviewed.

### BL-15 - Split README reference sections

- **Priority:** P2
- **Category:** docs
- **Related audit:** DOC-08
- **Description:** Move the full HTTP API and ModelConf schema to `docs/reference/API.md` and `docs/reference/MODELS.md`, and user troubleshooting to `docs/reference/TROUBLESHOOTING.md`, leaving summaries and links in README.
- **Definition of Done:** No content is lost. Links resolve. README still allows a quick start without opening reference docs. Reference content is verified against code. Only one home per topic.

### BL-16 - Fix archive paths and define retention

- **Priority:** P2
- **Category:** docs
- **Related audit:** DOC-07, PRO-05
- **Description:** Fix the `PENDING_CHECKLIST.md:14` path. Define `handoff/archive/<date>/`, `archive/`, and `base-archive/` roles. Add frozen-tree banners. State compression rules.
- **Definition of Done:** Every archive pointer resolves. No current doc treats an archive as authoritative. Retention rules are explicit. History is preserved.

### BL-17 - Establish a single public ninfer template

- **Priority:** P2
- **Category:** docs
- **Related audit:** DOC-06
- **Description:** Keep `ninfer-autostart-template.sh` as the only public copy. Replace the README inline block with a link. If an inline example is required, generate it from the file and fail on mismatch.
- **Definition of Done:** Only one public script body exists. README links to it. A check detects divergence between the compiled canonical script and the public copy, or the manual sync duty is removed.

---

## Slice 3 - Make tasks and handoff operational

### BL-18 - Adopt the task schema and migrate the live queue

- **Priority:** P1
- **Category:** process
- **Related audit:** PRO-01, PRO-02, PRO-08
- **Description:** Create `docs/process/TASKS.md` with lifecycle, classes, priorities, readiness, template, definition of ready/done, and archive rules. Convert live queue rows to one-line index rows with task-file links. Create task files for active multi-session work.
- **Definition of Done:** 100% of active tasks have complete task files. Queue rows are one line. Definition of Ready/Done are explicit. Completed narratives are archived, not left in the queue. No task detail lives only in prose.

### BL-19 - Implement the handoff protocol and checkpoints

- **Priority:** P1
- **Category:** process
- **Related audit:** PRO-03, GOV-10
- **Description:** Create `docs/process/HANDOFF.md` with triggers, required fields, dirty-tree protocol, resume checklist, and templates. Rewrite root `HANDOFF.md` to the snapshot template. Add the checkpoint to `AGENTS.md`.
- **Definition of Done:** A simulated dirty-tree interruption can be resumed by another agent using only `HANDOFF.md`, the task file, and `git status`. `HANDOFF.md` <= 120 lines. Checkpoint triggers are explicit. Parallel-session ownership is stated (or serial is declared).

### BL-20 - Formalize review and definition of done

- **Priority:** P1
- **Category:** process
- **Related audit:** PRO-10, PRO-02
- **Description:** Create `docs/process/REVIEW.md`: when review is required, lenses, read-only rules, finding format, severity, falsifier requirement, closure, accepted risk, evidence retention. Extend closure to all tasks.
- **Definition of Done:** A reviewer can run a chunk from the doc alone. A task with open findings cannot be accepted. Trivial changes are explicitly exempt. No finding can be closed without a commit or ruling.

### BL-21 - Write the real Git workflow

- **Priority:** P1
- **Category:** git
- **Related audit:** PRO-04, PRO-09
- **Description:** Create `docs/process/GIT_WORKFLOW.md`: trunk model, branch/worktree rules, staging, commit style, review/merge, protected operations, release, conflict resolution, stale worktree cleanup.
- **Definition of Done:** The documented model matches the repository. A new contributor can produce a compliant commit and release. Parallel work has a path if enabled. No PR language if no PRs are used.

### BL-22 - Decide and enforce the parallel-agent model

- **Priority:** P1
- **Category:** agent
- **Related audit:** GOV-06, GOV-10
- **Description:** Choose serial writers with parallel read-only research (recommended) or parallel with per-worktree task ownership and merge rules. Update `AGENTS.md`, `TASKS.md`, and `GIT_WORKFLOW.md`; add a worktree preflight.
- **Definition of Done:** One model is stated. No worktree can start without durable docs. `git worktree list` is clean at cycle start. Ledger write ownership is explicit. No shared-ledger overwrite path remains.

### BL-23 - Consolidate technical debt

- **Priority:** P2
- **Category:** process
- **Related audit:** PRO-06
- **Description:** Extend `FINDINGS-OPEN.md` with a debt class and fields (severity, impact, owner, target, age, linked task), or create a single index. Define intake, triage, aging, and accepted-risk rules.
- **Definition of Done:** One view lists all debt. No debt item exists only in prose. Accepted risks have a ruling and a permanent row. Aging is reviewed at cycle start.

### BL-24 - Add onboarding and subtask brief templates

- **Priority:** P2
- **Category:** onboarding
- **Related audit:** GOV-01, GOV-02, DOC-04
- **Description:** Add `CONTRIBUTING.md` (already in BL-10), a first-task walkthrough in `TASKS.md`, and a subagent brief template with scope, reading matrix, evidence format, and conflict rule.
- **Definition of Done:** The walkthrough can be followed end to end. The brief template fits on one screen plus links. It does not paste the full doctrine ledger. A new agent can start with it.

---

## Slice 4 - Automate the invariants

### BL-25 - Implement the single gate script

- **Priority:** P1
- **Category:** automation
- **Related audit:** GOV-04, PRO-07
- **Description:** Add `scripts/gate.ps1` (or `cargo xtask gate`) that runs the cardless gate and prints counts. `VERIFICATION.md` describes it; all other docs point to it.
- **Definition of Done:** The script exits non-zero on failure, never starts an engine, and is used by CI. No durable doc lists a competing command set.

### BL-26 - Implement the documentation checker

- **Priority:** P1
- **Category:** automation
- **Related audit:** DOC-04, PRO-14
- **Description:** Add `scripts/check-docs.mjs` (no external deps): required front matter, relative links, referenced paths, duplicate H1, `DRAFT` on released docs, absolute machine paths, index coverage, duplicate gate blocks.
- **Definition of Done:** It passes on the migrated tree and fails on fixtures with a missing link, missing front matter, absolute path, and orphan doc. It runs in CI.

### BL-27 - Implement task and instruction checkers

- **Priority:** P2
- **Category:** automation
- **Related audit:** PRO-01, GOV-03
- **Description:** Add `scripts/check-tasks.mjs` (schema, required fields, valid states) and `scripts/check-instructions.mjs` (AGENTS links, precedence present, no retired paths, no duplicated gate).
- **Definition of Done:** Both pass on the migrated state and fail on deliberately invalid fixtures. They run in CI. They validate structure, not prose style.

### BL-28 - Add cardless CI

- **Priority:** P1
- **Category:** automation
- **Related audit:** PRO-07
- **Description:** Add `.github/workflows/ci.yml` on `windows-latest`: checkout, toolchain, cache, gate script, docs checker, task checker. Exclude card tests and name them operator-only.
- **Definition of Done:** A green run on `main`; a broken test fails the workflow; no engine spawn; runtime is reasonable with a warm cache.

### BL-29 - Add the release check

- **Priority:** P2
- **Category:** automation
- **Related audit:** PRO-09, DOC-05
- **Description:** Add `scripts/release-check.ps1` verifying version, tag, changelog entry, README release claim, and artifact hash.
- **Definition of Done:** It fails on a version/tag mismatch or missing changelog; it passes on a correctly prepared release. The release checklist references it.

### BL-30 - Add the fresh-clone onboarding check

- **Priority:** P2
- **Category:** onboarding
- **Related audit:** GOV-01, DOC-04
- **Description:** Clone to a temp directory and verify that the durable onboarding path exists and links resolve.
- **Definition of Done:** It passes on a clone containing only tracked files. It fails if `AGENTS.md`, `docs/README.md`, or process docs are missing. It runs on release and weekly, not every push.

### BL-31 - Add a process health report

- **Priority:** P3
- **Category:** automation
- **Related audit:** PRO-06, PRO-14
- **Description:** Report docs older than a quarter, debt age, queue age, gate failures, and worktree age. Five metrics maximum; no auto-created issues.
- **Definition of Done:** The report runs read-only and produces the agreed fields. The operator reviews it at cycle start. Metrics that do not change a decision are removed.

### BL-32 - Define backup and recovery for local state

- **Priority:** P1
- **Category:** process
- **Related audit:** GOV-01, PRO-12
- **Description:** Define what is backed up (`handoff/`, `ModelConf/`, local configs), where, how often, retention, and restore verification. Record it in `docs/process/HANDOFF.md` or a short operations note.
- **Definition of Done:** A backup exists on a separate medium. A restore is tested. The policy names what is excluded and why. The single-copy risk is closed or explicitly accepted.

### BL-33 - Add the publication and scrub checklist

- **Priority:** P2
- **Category:** security
- **Related audit:** PRO-12, DOC-03
- **Description:** Create `docs/process/PUBLICATION.md`: absolute paths, usernames, hosts, ports, token/key names, container names, model paths, logs, PII, screenshots, repo visibility, second-review rule.
- **Definition of Done:** The checklist is required before any local artifact is committed or shared. Durable docs contain no machine paths (checked by BL-26). Security claims are verified against code at release.

### BL-34 - Remove unused stack mandates from the agent path

- **Priority:** P2
- **Category:** agent
- **Related audit:** GOV-07
- **Description:** Move `polars`, `ratatui`, `dioxus`, `maturin`, `secrecy`, and `indicatif` rules into an appendix gated on stack presence. Keep only `axum`, `tokio`, `serde`, `tracing`, `thiserror`/`anyhow`, and Windows-specific rules in the mandatory path.
- **Definition of Done:** No unused stack is a project mandate. The appendix is clearly conditional. An agent cannot introduce a dependency to satisfy a rule for a stack the project does not use.

### BL-35 - Add the missing doc-comment and security verification checks

- **Priority:** P3
- **Category:** automation
- **Related audit:** DOC-03, PRO-14
- **Description:** Add a release check for known stale phrases ("there is no auth", "arrive with their own turns") and a `cargo doc` warning check.
- **Definition of Done:** The check fails on a reintroduced stale phrase. `cargo doc` is warning-free. The check is part of the release process, not a separate ritual.

### BL-36 - Adopt ASD-STE100 for durable documentation

- **Priority:** P1
- **Category:** docs
- **Related audit:** DOC-08, PRO-14; operator requirement 2026-11-02
- **Description:** Add ASD-STE100 Simplified Technical English to `docs/process/ENGINEERING.md`. Apply it to new or changed durable docs: README, CONTRIBUTING, CHANGELOG, reference, process, and new architecture text. Keep exact LOCKED wording, code identifiers, paths, and log text unchanged. Define a glossary rule and a section-by-section review method.
- **Definition of Done:** The standard is stated once. New or changed sections are checked against the STE main rules and dictionary, and the review evidence names the check. A glossary exists where a document introduces more than three project terms. README and reference rewrites are reviewed section by section. No binding ruling is reworded. Full compliance is not claimed from a vocabulary-only check.

---

## Ready-to-file summary

| Priority | Items | Purpose |
|---|---|---|
| P0 | BL-01, BL-04, BL-05, BL-06 | Remove false user instructions and establish the agent entry point and autonomy |
| P1 | BL-02, BL-03, BL-07, BL-08, BL-09, BL-10, BL-11, BL-12, BL-13, BL-14, BL-18, BL-19, BL-20, BL-21, BL-22, BL-25, BL-26, BL-28, BL-32, BL-36 | Durable architecture and operational process |
| P2 | BL-15, BL-16, BL-17, BL-23, BL-24, BL-27, BL-29, BL-30, BL-33, BL-34 | Reference cleanup, debt, onboarding, automation, security |
| P3 | BL-31, BL-35 | Metrics and long-term freshness |

Owner decisions D1-D11 in `IMPLEMENTATION_PLAN.md` Section 1 were approved on 2026-11-02.
Items that require a repository-visibility confirmation (BL-10, BL-12, BL-28, BL-32) must
not move local artifacts into Git before that confirmation.
