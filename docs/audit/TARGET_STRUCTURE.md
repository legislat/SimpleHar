# Target Documentation and Process Architecture

| Field | Value |
|---|---|
| Status | Proposal - requires owner approval |
| Date | 2026-11-02 |
| Based on | `docs/audit/AUDIT_REPORT.md` |
| Design goal | A minimal, navigable, reproducible system in which every fact has one home and every task can be resumed |
| Non-goal | Creating a document for every topic, module, or decision |

---

## 1. Design stance

The current repository already has good content. The problem is not a lack of documents; it is that the documents are **unversioned, overlapping, and not routed**. The target architecture therefore optimizes for four things:

1. **Reproducibility** - the durable part of the system travels with the repository.
2. **One home per fact** - every other mention is a pointer.
3. **Progressive disclosure** - an agent reads only what its task needs.
4. **Maintainability** - a small number of files with explicit owners and review triggers.

The target deliberately does **not** mirror the current file count one-to-one. It consolidates four instruction files into a routed set, moves durable knowledge out of `handoff/`, and keeps live state where it belongs.

---

## 2. The three layers

Every artifact belongs to exactly one layer.

| Layer | Purpose | Versioned | May contain machine paths | Lifetime |
|---|---|---:|---|---|
| **Durable** | Rules, process, doctrine, architecture, user docs, change history | Yes | No | Indefinite |
| **Live** | Current task state, open findings, pending questions, traps, handoff snapshot | No (backed up) | Yes | Days to weeks |
| **Evidence** | Logs, run results, review findings, forensic captures, research | No (backed up) | Yes | Until archived |
| **Archive** | Closed live state and superseded durable revisions | No | Yes | Indefinite, compressed |

The current failure is that durable content lives in the live layer (for example, `DECISIONS.md` and the review protocol), while live content lives in durable-looking prose (for example, queue narratives). The layers must be separated by directory, not by good intentions.

---

## 3. Target tree

```text
C:\Inference\SimpleHar-SE
|-- README.md                         DURABLE  product overview, quick start, links
|-- AGENTS.md                         DURABLE  agent entry point, precedence, autonomy, routing
|-- CONTRIBUTING.md                   DURABLE  short contributor pointer
|-- CHANGELOG.md                      DURABLE  dated release history
|-- DESIGN.md                         DURABLE  historical design record (banner; not default reading)
|-- LICENSE
|-- Cargo.toml
|-- Cargo.lock
|-- ninfer-autostart-template.sh      DURABLE  single public copy of the canonical script
|
|-- docs/
|   |-- README.md                     DURABLE  documentation index and routing table
|   |-- process/
|   |   |-- ENGINEERING.md            DURABLE  coding, testing, docs, security, platform standards
|   |   |-- VERIFICATION.md           DURABLE  the one gate, evidence rules, card/cardless tiers
|   |   |-- GIT_WORKFLOW.md           DURABLE  trunk, worktrees, commits, review, release
|   |   |-- TASKS.md                  DURABLE  lifecycle, template, DoR/DoD, priorities
|   |   |-- HANDOFF.md                DURABLE  handoff protocol and templates (not live state)
|   |   |-- REVIEW.md                 DURABLE  review lenses, finding format, closure, falsifiers
|   |   `-- PUBLICATION.md            DURABLE  scrub checklist before committing local artifacts
|   |-- architecture/
|   |   |-- OVERVIEW.md               DURABLE  current system, modules, data flow, invariants
|   |   |-- DOCTRINE.md               DURABLE  one-page binding invariants (agent must-read)
|   |   |-- DECISIONS.md              DURABLE  full ruling ledger: LOCKED / DEFERRED / ARCHIVED
|   |   `-- ADR/                      DURABLE  only for decisions too large for one ledger row
|   |       |-- README.md             index and template
|   |       `-- ADR-000X-<slug>.md    optional, created on demand
|   |-- reference/
|   |   |-- API.md                    DURABLE  HTTP API and error contract
|   |   |-- MODELS.md                 DURABLE  ModelConf schema and engine knobs
|   |   `-- TROUBLESHOOTING.md        DURABLE  user-facing operations and failures
|   |-- research/                     DURABLE  scrubbed research reports (optional)
|   `-- audit/                        DURABLE  this audit and future audits
|
|-- src/, tests/                      DURABLE  behavior is the final truth
|
|-- HANDOFF.md                        LIVE     current snapshot, safety, pointers (gitignored)
|-- handoff/                          LIVE     gitignored, backed up
|   |-- PENDING_CHECKLIST.md          LIVE     queue index (short, no narratives)
|   |-- FINDINGS-OPEN.md              LIVE     open findings
|   |-- DECISIONS-PENDING.md          LIVE     operator questions only
|   |-- GOTCHAS.md                    LIVE     machine-local traps
|   |-- tasks/
|   |   `-- <TASK-ID>.md              LIVE     one file per multi-session task
|   |-- evidence/
|   |   `-- <YYYY-MM-DD>-<slug>/      EVIDENCE logs, RESULTS.md, captures
|   `-- archive/
|       `-- <YYYY-MM-DD>/             ARCHIVE  closed tasks, old queue, superseded state
|
|-- archive/                          ARCHIVE  legacy; frozen, never read for current behavior
|-- base-archive/                     REFERENCE frozen v3.3 baseline; never read for current behavior
|-- ModelConf/                        LIVE     machine-specific model rows (gitignored)
`-- .kilo/                            LIVE     tool state (gitignored); skills may be tracked later
```

### 3.1 What is deliberately not created

- No per-module documentation tree. Rust doc comments and `docs/architecture/OVERVIEW.md` are enough until a module becomes a public contract.
- No ADR for every ruling. `DECISIONS.md` is the decision system; `ADR/` is only for a decision that needs more than roughly 60 lines of context.
- No separate `GIT_RULES.md` and `AUX-RULES.md` after migration. Their durable content is consolidated into `GIT_WORKFLOW.md`, `VERIFICATION.md`, `REVIEW.md`, and `TASKS.md`; root files become short redirects or are retired.
- No tracked `GOTCHAS.md`, `FINDINGS-OPEN.md`, or evidence. They contain machine paths and internal incident data; they stay local and backed up. If a trap is generally reusable, promote a scrubbed version into `docs/reference/TROUBLESHOOTING.md` or `ENGINEERING.md`.
- No API reference split until the README API section exceeds roughly 150 lines. Premature splitting creates two homes.
- No generated documentation site. Markdown plus a link checker is sufficient.

---

## 4. File-by-file specification

### 4.1 Root files

#### `README.md`

| Field | Value |
|---|---|
| Purpose | Product overview, who it is for, quick start, core concepts, links to reference and contributor docs |
| Owner | Operator (product), maintainer agent (accuracy) |
| Audience | Users, new contributors, agents needing product context |
| Expected content | What it is; supported engines and stack; quick start; categories/ports; minimal configuration example; links to `docs/reference/API.md`, `docs/reference/MODELS.md`, `docs/reference/TROUBLESHOOTING.md`; security summary; contributor link |
| Source of truth for | User-facing behavior and setup |
| Replaces | Current README, reduced by moving API/model schema/troubleshooting detail |
| Must not contain | Internal doctrine, design rationale, task state, agent rules |

#### `AGENTS.md`

| Field | Value |
|---|---|
| Purpose | The single agent entry point: project orientation, precedence, autonomy, required reading, routing |
| Owner | Operator |
| Audience | All AI agents and human contributors |
| Expected content | 5-line project summary; "start here" chain; instruction precedence; autonomy table; required-reading matrix; non-negotiable safety fences; links to every process document; definition of done; conflict rule |
| Source of truth for | Agent behavior and routing |
| Replaces | Current 239-line cookbook; generic standards move to `docs/process/ENGINEERING.md`; Git rules to `GIT_WORKFLOW.md`; review rules to `REVIEW.md`; gate to `VERIFICATION.md` |
| Budget | <= 200 lines |

#### `CONTRIBUTING.md`

| Field | Value |
|---|---|
| Purpose | Conventional GitHub entry point for humans |
| Owner | Operator |
| Audience | Human contributors |
| Expected content | Ten-line orientation; link to `docs/README.md`, `docs/process/TASKS.md`, `docs/process/GIT_WORKFLOW.md`; how to run the gate; how to request review |
| Source of truth for | Nothing; it is a pointer |
| Replaces | None (new) |

#### `CHANGELOG.md`

| Field | Value |
|---|---|
| Purpose | Dated user-visible change history per release |
| Owner | Operator |
| Audience | Users, maintainers |
| Expected content | Keep a Changelog structure: Added, Changed, Fixed, Removed, Security; version and date; link to tag/commit; artifact hash when deployed |
| Source of truth for | Release history |
| Replaces | Ad hoc release prose in `PENDING_CHECKLIST.md` and commit bodies (commit bodies remain implementation detail) |

#### `DESIGN.md`

| Field | Value |
|---|---|
| Purpose | Historical design record: goals, alternatives, rejected options, baseline defect register, dated rationale |
| Owner | Operator |
| Audience | Architects, agents investigating why a choice was made |
| Expected content | Banner: "Historical design record. Current architecture: `docs/architecture/OVERVIEW.md`. Binding decisions: `docs/architecture/DECISIONS.md`." Keep the 21 steps and Appendix A as history; mark superseded sections inline |
| Source of truth for | Historical rationale only |
| Replaces | Current `DESIGN.md` role as "spec of record" |
| Must not contain | Current behavior claims without a status banner; new decisions |

### 4.2 `docs/README.md`

| Field | Value |
|---|---|
| Purpose | The map of the documentation system |
| Owner | Operator |
| Audience | Everyone |
| Expected content | "I want to..." routing table; layer explanation; ownership/front-matter rules; document list with status and last review; link-check policy |
| Source of truth for | Where information belongs |

Routing table shape:

| I want to... | Read |
|---|---|
| Understand the product | `README.md` |
| Start contributing | `CONTRIBUTING.md` -> `docs/process/TASKS.md` |
| Know the rules an agent must follow | `AGENTS.md` |
| Know a binding product invariant | `docs/architecture/DOCTRINE.md` |
| Understand the system design | `docs/architecture/OVERVIEW.md` |
| Know why a decision was made | `docs/architecture/DECISIONS.md` (and `DESIGN.md` for history) |
| Add or change code | `docs/process/ENGINEERING.md` |
| Run the gate | `docs/process/VERIFICATION.md` |
| Commit, branch, or release | `docs/process/GIT_WORKFLOW.md` |
| Pick up a task | `HANDOFF.md` -> `handoff/PENDING_CHECKLIST.md` -> task file |
| Hand off work | `docs/process/HANDOFF.md` |
| Review code | `docs/process/REVIEW.md` |
| Find a trap | `handoff/GOTCHAS.md` |
| See what changed | `CHANGELOG.md` |

### 4.3 `docs/process/`

#### `ENGINEERING.md`

| Field | Value |
|---|---|
| Purpose | Durable coding, testing, documentation, security, and platform standards |
| Owner | Operator |
| Audience | Contributors and agents |
| Expected content | Rust style; error handling; concurrency; allocation/performance; test expectations; doc-comment rules; platform constraints (Windows, PowerShell 5.1, UTF-8); security rules; dependency policy; module ownership; explicit non-rules (do not add unused tooling) |
| Source of truth for | How code is written |
| Replaces | Generic sections of `AGENTS.md`; stack-specific rules moved to an "only if present" appendix |

#### `VERIFICATION.md`

| Field | Value |
|---|---|
| Purpose | The single definition of what "verified" means |
| Owner | Operator |
| Audience | Agents, reviewers, release operator |
| Expected content | Cardless gate commands; card gate and who may run it; evidence requirements; pass/fail/blocked vocabulary; flakiness policy; test tiers; what may be claimed without running a check; link to `scripts/gate.ps1` |
| Source of truth for | Gate and evidence |
| Replaces | `AGENTS.md:225-235`, `AUX-RULES.md:33-36`, `PENDING_CHECKLIST.md:32-39`, `DESIGN.md:869` |
| Rule | Never embed volatile counts; the script prints them |

#### `GIT_WORKFLOW.md`

| Field | Value |
|---|---|
| Purpose | The actual repository model |
| Owner | Operator |
| Audience | Agents and contributors |
| Expected content | Trunk-based model; when a branch/worktree is allowed; worktree hygiene; staging rules; commit subject/body style (including the narrative house style and length limit); finding-ID references; protected operations; review-before-merge; release/tag/version/changelog coupling; conflict resolution; stale worktree cleanup |
| Source of truth for | Git and release practice |
| Replaces | `GIT_RULES.md`; release paragraphs in `AUX-RULES.md` |

#### `TASKS.md`

| Field | Value |
|---|---|
| Purpose | Task lifecycle and schema |
| Owner | Operator |
| Audience | Agents and operator |
| Expected content | Lifecycle stages; task ID scheme; classes (`S/R/C/O`); priorities (`P0-P3`); readiness states; task template; acceptance criteria rules; dependency notation; definition of ready/done; queue rules; archive rules; how findings relate to tasks |
| Source of truth for | Task definition and status |
| Replaces | Prose conventions in `PENDING_CHECKLIST.md:1-30`; review closure stays in `REVIEW.md` |

#### `HANDOFF.md` (process document)

| Field | Value |
|---|---|
| Purpose | The protocol for transferring work, not the current state |
| Owner | Operator |
| Audience | Agents, operator |
| Expected content | Triggers (session end, before compaction, before long run, before agent switch); required fields; task-file template; dirty-tree protocol; resume checklist; what belongs in each ledger; what must never be copied; examples |
| Source of truth for | Handoff procedure |
| Replaces | `HANDOFF.md:111-118` closing protocol and the implicit session-start chain |
| Note | The live snapshot remains root `HANDOFF.md`; this document explains how to maintain it |

#### `REVIEW.md`

| Field | Value |
|---|---|
| Purpose | Review method and closure |
| Owner | Operator |
| Audience | Reviewers, authors |
| Expected content | Review chunking; lenses; reviewer permissions; finding format; severity; falsifier requirement; closure rule; accepted-risk rule; how findings map to tasks; review evidence retention |
| Source of truth for | Review |
| Replaces | `AUX-RULES.md:6-29` and the durable parts of `handoff/code-review/PLAN.md` and `PROMPT.md` |

#### `PUBLICATION.md`

| Field | Value |
|---|---|
| Purpose | Prevent leaks when durable content is extracted from local artifacts |
| Owner | Operator |
| Audience | Anyone committing or sharing |
| Expected content | Scrub checklist: absolute paths, usernames, hostnames, ports, token/key names, container names, model paths, incident logs, screenshots; review step; private-repo check; backup rule |
| Source of truth for | Publication safety |
| Replaces | Scattered warnings in `DECISIONS-PENDING.md:9-17` and `GOTCHAS.md` |

### 4.4 `docs/architecture/`

#### `OVERVIEW.md`

| Field | Value |
|---|---|
| Purpose | Current architecture as shipped |
| Owner | Operator, maintainer agent |
| Audience | New contributors, agents before modifying core |
| Expected content | System context; process/actor model; scheduler and job state machine; admission and witness; engine adapters; configuration layers L1/L2/L3; public/control surfaces; module map with responsibilities; data flow; key invariants; links to code symbols |
| Source of truth for | Current architecture |
| Replaces | `DESIGN.md` as the current-spec role; it summarizes and links rather than duplicating 85 KB |
| Budget | <= 300 lines |

#### `DOCTRINE.md`

| Field | Value |
|---|---|
| Purpose | The short, binding invariant card every agent must carry |
| Owner | Operator |
| Audience | All agents |
| Expected content | 12-20 numbered invariants with one-line consequence each; pointer to the full ledger for rationale |
| Source of truth for | Non-negotiable product behavior |
| Replaces | The `HANDOFF.md:78-85` digest and the mandatory whole-ledger read for subagents |
| Budget | <= 120 lines |

#### `DECISIONS.md`

| Field | Value |
|---|---|
| Purpose | Full decision ledger with rationale and status |
| Owner | Operator |
| Audience | Architects, agents resolving ambiguity |
| Expected content | LOCKED, DEFERRED, ARCHIVED sections; each entry: ID, date, status, decision, rationale, consequences, commit/ruling reference |
| Source of truth for | Settled and deferred decisions |
| Replaces | `handoff/DECISIONS.md` (moved and tracked) |
| Rule | Append-only; tombstones; no live status; no task detail |

#### `ADR/`

| Field | Value |
|---|---|
| Purpose | Long-form architecture decisions that exceed a ledger row |
| Owner | Operator |
| Audience | Architects |
| Expected content | One file per significant decision: context, options, decision, consequences, references; index in `ADR/README.md` |
| Source of truth for | Long-form rationale |
| Replaces | Nothing; created on demand only |
| Trigger | A decision needs more than roughly 60 lines, or affects multiple subsystems and needs a durable narrative |

### 4.5 `docs/reference/`

#### `API.md`

| Field | Value |
|---|---|
| Purpose | Complete HTTP contract |
| Owner | Maintainer agent |
| Audience | Users, client developers |
| Expected content | Every endpoint; request/response shapes; error codes and envelopes; auth; streaming; limits; status semantics (especially "200 = acceptance") |
| Source of truth for | HTTP behavior |
| Replaces | README API section when it exceeds roughly 150 lines |

#### `MODELS.md`

| Field | Value |
|---|---|
| Purpose | Complete ModelConf schema and engine-specific knobs |
| Owner | Maintainer agent |
| Audience | Users configuring models |
| Expected content | Common keys; llama keys; strata template-mode keys and import bridge; ninfer template/legacy keys; validation rules; inert/refused keys; examples |
| Source of truth for | Model configuration |
| Replaces | README model section when it exceeds roughly 150 lines |

#### `TROUBLESHOOTING.md`

| Field | Value |
|---|---|
| Purpose | User-facing failures and recovery |
| Owner | Maintainer agent |
| Audience | Users |
| Expected content | Port conflicts; missing models; admission refusals; stranded containers; log locations; recovery steps |
| Source of truth for | User operations |
| Replaces | README troubleshooting plus scrubbed, generalizable `GOTCHAS.md` entries |

### 4.6 Live layer (gitignored, backed up)

| File | Purpose | Content rules |
|---|---|---|
| `HANDOFF.md` | Bootstrap snapshot | <= 120 lines; HEAD, unpushed, tree state, gates, environment/safety, active task, pointers. No item detail. |
| `handoff/PENDING_CHECKLIST.md` | Queue index | One row per task: ID, class, priority, readiness, owner, one-line goal, link to task file/evidence. No narratives. |
| `handoff/tasks/<ID>.md` | Task record | Template in Section 6; created for any task spanning more than one session or requiring card work |
| `handoff/FINDINGS-OPEN.md` | Open findings | One row per finding; ID, severity, location, claim, class, status; closure evidence |
| `handoff/DECISIONS-PENDING.md` | Operator questions | One row per question; options; recommendation; owner decision; no task detail |
| `handoff/GOTCHAS.md` | Machine-local traps | One tagged line per trap; no paragraphs; promote reusable traps to durable docs |
| `handoff/evidence/<date>-<slug>/` | Run evidence | `RUN.md`, `PROMPT.md`, scripts, logs, `RESULTS.md`; immutable once closed |
| `handoff/research/` | Unscrubbed research | Dated reports; promote scrubbed versions to `docs/research/` |
| `handoff/archive/<date>/` | Closed state | Old queue rows, task files, evidence; compressed when cold |

---

## 5. Information separation principles

### 5.1 Durable vs live

- If a fact would still be true on another machine, it is durable.
- If a fact names this machine, this run, or this session, it is live.
- If a fact explains why code behaves as it does, it is durable architecture or a decision.
- If a fact explains what is happening now, it is live state.

### 5.2 Rules vs rationale vs state vs history

| Kind | Home | Example |
|---|---|---|
| Rule | `AGENTS.md`, `docs/process/*` | "Never run card tests from a served session" |
| Rationale | `docs/architecture/DECISIONS.md`, `DESIGN.md` | Why the card window belongs to the handoff |
| State | `HANDOFF.md`, queue, task files | Current HEAD, active task, blockers |
| Evidence | `handoff/evidence/` | A log showing the gate passed |
| History | `CHANGELOG.md`, git, `DESIGN.md` | v1.0.1 released on 2026-11-02 |

### 5.3 Product vs process vs implementation

- Product behavior belongs in `README.md`, `docs/reference/*`, and tests.
- Process belongs in `docs/process/*`.
- Implementation detail belongs in code comments, commit bodies, and `docs/architecture/OVERVIEW.md`.
- A document must not be the only home for a behavior that tests should pin.

### 5.4 Single source of truth

Each topic has exactly one authoritative home. Everything else is a link.

| Topic | Authoritative home | Pointers from |
|---|---|---|
| Product overview | `README.md` | `AGENTS.md`, `CONTRIBUTING.md`, `docs/README.md` |
| Agent behavior | `AGENTS.md` | `docs/README.md` |
| Coding standards | `docs/process/ENGINEERING.md` | `AGENTS.md` |
| Gate | `docs/process/VERIFICATION.md` + `scripts/gate.ps1` | `AGENTS.md`, `TASKS.md`, `REVIEW.md`, `CHANGELOG.md` process |
| Git/release | `docs/process/GIT_WORKFLOW.md` | `AGENTS.md`, `CONTRIBUTING.md` |
| Task schema | `docs/process/TASKS.md` | `AGENTS.md`, queue |
| Handoff protocol | `docs/process/HANDOFF.md` | `AGENTS.md`, `HANDOFF.md` |
| Review | `docs/process/REVIEW.md` | `AGENTS.md`, `TASKS.md` |
| Current architecture | `docs/architecture/OVERVIEW.md` | `README.md`, `AGENTS.md` |
| Binding invariants | `docs/architecture/DOCTRINE.md` | `AGENTS.md`, every technical doc |
| Decisions | `docs/architecture/DECISIONS.md` | `DESIGN.md`, task files, commit bodies |
| Historical design | `DESIGN.md` | `OVERVIEW.md` |
| API | `docs/reference/API.md` | `README.md` |
| Model schema | `docs/reference/MODELS.md` | `README.md` |
| Live state | `HANDOFF.md` + `handoff/` | `AGENTS.md` |
| Change history | `CHANGELOG.md` | `README.md` |

### 5.5 What must never be copied

- Gate command lists.
- Decision rationale.
- Test counts.
- File line numbers.
- Machine paths in durable docs.
- Current HEAD or branch state in durable docs.
- Full task narratives in the queue.

---

## 6. Task and handoff templates

### 6.1 Task file template

```markdown
# TASK-<ID> - <short title>

| Field | Value |
|---|---|
| Class | S / R / C / O |
| Priority | P0 / P1 / P2 / P3 |
| Readiness | ready / blocked / needs-design / needs-operator |
| Owner | operator / agent-session-id |
| Created | YYYY-MM-DD |
| Last updated | YYYY-MM-DD |
| Branch / commit | ... |

## Goal

One paragraph. What changes for the user or system.

## Scope

Files/modules in scope.

## Non-goals

What this task explicitly does not do.

## Acceptance criteria

- [ ] Falsifiable criterion 1
- [ ] Falsifiable criterion 2
- [ ] Documentation updated
- [ ] Gate passed (link to evidence)

## Dependencies

- Blocks / blocked by:
- Required environment (cardless, card, operator ruling):

## Decisions

Links to DOCTRINE / DECISIONS / pending ruling. Do not copy text.

## Progress

### Done
### Remaining
### Failed approaches
### Blockers

## Files changed

List or diff reference.

## Evidence

Links to `handoff/evidence/...` and gate output.

## Next step

The exact command or action the next session should take first.
```

### 6.2 Queue row template

```markdown
- [ ] TASK-<ID> - P<N> - [<class>] - <readiness> - <one-line goal> - owner: <who> - file: tasks/<ID>.md
```

No commit hashes, no test counts, no narratives. Those live in the task file or evidence.

### 6.3 Handoff snapshot template (root `HANDOFF.md`)

```markdown
# Handoff

## 1. What this project is
3 lines + link to README.

## 2. Current snapshot
Branch, HEAD, unpushed, tree state, active task ID, gate status (link), deployed artifact.

## 3. Machine safety
Live services, protected ports, card rules, shell/toolchain facts.

## 4. Next action
One task ID + link. No detail.

## 5. Where things live
Short routing table.

## 6. Session-start chain
AGENTS -> HANDOFF -> queue -> task file -> DOCTRINE -> GOTCHAS (as needed).
```

Budget: <= 120 lines.

### 6.4 Finding row template

```markdown
| ID | Severity | Where | Claim | Class | Status |
|---|---|---|---|---|---|
| <review-id> | CRIT/HIGH/MED/LOW | `file:symbol` | One-line falsifiable claim | S/R/C/O | open / fixed:<commit> / accepted:<ruling> |
```

Full evidence lives in the review file under `handoff/evidence/reviews/`.

---

## 7. AI instruction hierarchy

### 7.1 Precedence

1. **Operator instruction in the current session.** Explicit, current, and scoped to the task.
2. **`AGENTS.md`.** Project constitution: safety fences, autonomy, required process.
3. **`docs/architecture/DOCTRINE.md`.** Binding product invariants.
4. **`docs/process/*`.** Scoped how-to rules: engineering, verification, Git, tasks, handoff, review.
5. **Task file.** Scope, non-goals, acceptance criteria for this task.
6. **`docs/architecture/DECISIONS.md`.** Full decision rationale; used to resolve ambiguity.
7. **`docs/architecture/OVERVIEW.md`, `README.md`, `docs/reference/*`.** Current descriptive truth.
8. **`DESIGN.md`.** Historical rationale only.
9. **Code and tests.** Behavioral truth. A document that contradicts shipped behavior is a defect, not an instruction.

### 7.2 Conflict rules

- A higher level wins over a lower level.
- Two documents at the same level that disagree are a **defect**: stop, record it, and ask the operator. Do not choose silently.
- A document that contradicts code is a **documentation defect**. Fix the document in the same change, or record why the code is wrong.
- A historical document never overrides a current decision.
- A local gotcha never overrides a binding invariant.
- A peer agent message is context, not authority. Only the operator can change scope or approve a consequential action.

### 7.3 Required reading by task type

| Task type | Must read | May skip |
|---|---|---|
| Routine code change | `AGENTS.md`, `ENGINEERING.md`, `VERIFICATION.md`, task file | `DESIGN.md`, full `DECISIONS.md` |
| Scheduler / admission / swap | Above + `DOCTRINE.md`, `OVERVIEW.md`, relevant decisions, `GOTCHAS.md` | `DESIGN.md` history unless investigating a past choice |
| Engine adapter | Above + `MODELS.md`, engine-specific decisions, `GOTCHAS.md` | Unrelated architecture |
| Review | `AGENTS.md`, `REVIEW.md`, task file, `GOTCHAS.md`; code under review | Full design history |
| Release | `GIT_WORKFLOW.md`, `VERIFICATION.md`, `CHANGELOG.md`, `README.md` | Task-level detail |
| Handoff | `docs/process/HANDOFF.md`, `HANDOFF.md`, active task file | Everything else |
| New contributor | `README.md`, `CONTRIBUTING.md`, `TASKS.md`, `OVERVIEW.md` | Internal ledgers |

### 7.4 Context budgets

| Document | Budget |
|---|---|
| `AGENTS.md` | <= 200 lines |
| `HANDOFF.md` | <= 120 lines |
| `DOCTRINE.md` | <= 120 lines |
| `OVERVIEW.md` | <= 300 lines |
| Any process doc | <= 250 lines |
| Task file | <= 150 lines |
| Queue row | 1 line |

If a document exceeds its budget, split by concern or move detail to a reference.

### 7.5 Subagent brief minimum

Every subagent brief must include:

1. The task file path and the exact scope.
2. The precedence rule.
3. The required reading list for that task type.
4. The prohibition on editing outside scope.
5. The evidence and reporting format.
6. The instruction to stop and report a conflict rather than guess.

Do not paste the full doctrine ledger into every brief; paste the `DOCTRINE.md` card and link the ledger.

---

## 8. Navigation conventions

1. Every durable Markdown file starts with front matter:

```yaml
---
title: ...
owner: operator
status: active | draft | historical | deprecated
audience: user | contributor | agent | architect
source_of_truth_for: [ ... ]
last_reviewed: YYYY-MM-DD
review_trigger: behavior-change | release | quarterly | adr
---
```

2. Every file has a one-line "Up:" link to its parent index.
3. `docs/README.md` is the only table of contents. Other docs link to it, not to each other's sections, unless the link is semantic.
4. Stable filenames; no dates except in ADR IDs and evidence directories.
5. Relative links only; no absolute machine paths.
6. No orphan documents: every durable file is listed in `docs/README.md`.
7. Root files are entry points, not detail stores.
8. Use headings with unique, linkable names.
9. Prefer tables for routing, status, and comparisons.
10. Use code fences with a language tag.

---

## 9. Documentation maintenance rules

### 9.1 Ownership

- **Operator** owns all durable documents and is the final approver.
- **Maintainer agent** may update docs for accuracy in the same change as behavior.
- **Reviewer agent** verifies that docs match behavior and that no fact was copied.
- A document without an owner is a defect.

### 9.2 Same-change rule

If a change alters behavior, the same commit must update every affected durable document or explicitly state why no document is affected. The gate must not pass a behavior change with no documentation decision.

### 9.3 Contradiction rule

Finding a contradiction is a stop-the-line event for documentation work: record it as a finding, fix the authoritative source, and replace the copy with a pointer. Do not "fix" both sides independently.

### 9.4 Release rule

At each release:

1. `Cargo.toml` version = tag.
2. `CHANGELOG.md` has a dated entry.
3. `README.md` release claim names the tag, not HEAD.
4. `docs/reference/*` matches the shipped API/schema.
5. `DOCTRINE.md` and `DECISIONS.md` reflect any ruling since the last release.
6. Artifact hash is recorded.
7. Stale worktrees are checked.

### 9.5 Review cadence

- At every release: README, reference docs, changelog, doctrine.
- Quarterly: all durable docs; update `last_reviewed`.
- On contradiction: immediately.
- On a new decision: the decision entry and any affected overview.
- On a new engine: `MODELS.md`, `OVERVIEW.md`, troubleshooting.

### 9.6 Archive rule

- Closed task files move to `handoff/archive/<date>/` after the release that contains them.
- Evidence older than two releases may be compressed but not deleted without operator approval.
- `archive/` and `base-archive/` are frozen; they are never read for current behavior.
- A durable document that is superseded moves to `docs/archive/` with a status banner, not silently deleted.

### 9.7 Anti-duplication rule

- One fact, one home.
- A pointer is not duplication.
- A summary is allowed only if it is clearly labeled and links to the source.
- Copying a command list, doctrine paragraph, or schema table is a defect.

---

## 10. Migration mapping

| Current artifact | Target | Action | Notes |
|---|---|---|---|
| `AGENTS.md` | `AGENTS.md` (slim) + `docs/process/ENGINEERING.md` | Split | Keep the entry point; move generic standards; add precedence/autonomy/routing |
| `GIT_RULES.md` | `docs/process/GIT_WORKFLOW.md` | Merge and rewrite | Align commit style with actual history; add trunk/worktree/release |
| `AUX-RULES.md` | `docs/process/VERIFICATION.md`, `REVIEW.md`, `TASKS.md`, `GIT_WORKFLOW.md` | Split | Retire the file or leave a short redirect |
| `HANDOFF.md` (protocol parts) | `docs/process/HANDOFF.md` | Extract | Keep root `HANDOFF.md` as live snapshot only |
| `HANDOFF.md` (state) | root `HANDOFF.md` | Keep local | Trim to the template |
| `handoff/DECISIONS.md` | `docs/architecture/DECISIONS.md` | Move + track | Full ledger; add entry IDs |
| `handoff/DECISIONS.md` LOCKED digest | `docs/architecture/DOCTRINE.md` | Extract + track | Short card |
| `handoff/DECISIONS-PENDING.md` | `handoff/DECISIONS-PENDING.md` | Keep local | Pointer-only rows |
| `handoff/PENDING_CHECKLIST.md` | `handoff/PENDING_CHECKLIST.md` + `handoff/tasks/*` | Split | Queue index + task files; archive narratives |
| `handoff/FINDINGS-OPEN.md` | `handoff/FINDINGS-OPEN.md` | Keep local | Add unified severity/debt fields |
| `handoff/GOTCHAS.md` | `handoff/GOTCHAS.md` | Keep local | Promote reusable entries |
| `handoff/code-review/PLAN.md` + `PROMPT.md` | `docs/process/REVIEW.md` | Consolidate | Keep per-review evidence local |
| `handoff/research/*` | `docs/research/*` (scrubbed) or local | Selective | Only durable, non-sensitive reports |
| `DESIGN.md` | `DESIGN.md` (historical) + `docs/architecture/OVERVIEW.md` | Reclassify + summarize | Do not delete the history |
| `README.md` | `README.md` + `docs/reference/*` | Split when threshold | Fix strata first |
| `ninfer-autostart-template.sh` | unchanged | Keep one copy | README links to it |
| `archive/`, `base-archive/` | unchanged | Freeze | Add a README banner if not present |
| `.kilo/skills/*` | `.kilo/skills/*` | Keep | Optionally track; not part of core process |
| `.kilo/agent-manager.json` | local | Keep | Remove stale worktrees |

---

## 11. Approval decisions

The target structure requires explicit owner decisions before implementation:

1. **Repo self-containedness.** Approve committing the durable core (recommended) and confirm repository visibility. Keep live state local.
2. **`DECISIONS.md` home.** Move the full ledger to `docs/architecture/DECISIONS.md` and track it, with a short `DOCTRINE.md` card.
3. **`DESIGN.md` status.** Reclassify as historical and create a current `OVERVIEW.md`, or keep it as the maintained spec (not recommended).
4. **File moves.** Approve the move/rename of `GIT_RULES.md`, `AUX-RULES.md`, and `handoff/DECISIONS.md`, including the redirect strategy.
5. **Worktree policy.** Choose serial-only or parallel-with-task-ownership; approve deleting stale worktrees.
6. **Commit style.** Formalize the narrative house style or enforce the current imperative rule.
7. **CI.** Approve a GitHub Actions workflow for cardless checks.
8. **Tracked vs local gotchas/findings.** Confirm these stay local (recommended) or approve a scrub-and-track path.
9. **Backup.** Approve an independent backup location and cadence for `handoff/` and `ModelConf/`.
10. **Versioning.** Approve the version/changelog/tag coupling rule.

Until these are approved, no file in the existing repository should be moved, rewritten, or deleted.
