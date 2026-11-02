---
title: Documentation map
owner: operator
status: active
audience: [user, contributor, agent, architect]
source_of_truth_for: [documentation routing]
last_reviewed: 2026-11-02
review_trigger: release, quarterly, new-decision
---

# docs/README.md

The map of the documentation system: what exists, what each file owns, and
where new text belongs. Every durable file is listed here (no orphans). Only
this file is the table of contents; other documents link to it.

## 1. I want to...

| I want to... | Read |
|---|---|
| Understand the product | [README.md](../README.md) |
| Start contributing | [CONTRIBUTING.md](../CONTRIBUTING.md) -> [docs/process/TASKS.md](process/TASKS.md) |
| Know the rules an agent must follow | [AGENTS.md](../AGENTS.md) |
| Know a binding product invariant | [docs/architecture/DOCTRINE.md](architecture/DOCTRINE.md) |
| Understand the system design | [docs/architecture/OVERVIEW.md](architecture/OVERVIEW.md) |
| Know why a decision was made | [docs/architecture/DECISIONS.md](architecture/DECISIONS.md) (and [DESIGN.md](../DESIGN.md) for history) |
| Add or change code | [docs/process/ENGINEERING.md](process/ENGINEERING.md) |
| Run the gate | [docs/process/VERIFICATION.md](process/VERIFICATION.md) |
| Commit, branch, or release | [docs/process/GIT_WORKFLOW.md](process/GIT_WORKFLOW.md) |
| Pick up a task | `HANDOFF.md` -> `handoff/PENDING_CHECKLIST.md` -> task file (local-only) |
| Hand off work | [docs/process/HANDOFF.md](process/HANDOFF.md) |
| Review code | [docs/process/REVIEW.md](process/REVIEW.md) |
| Find a trap | `handoff/GOTCHAS.md` (local-only) |
| See what changed | [CHANGELOG.md](../CHANGELOG.md) |

Paths in plain text do not exist yet (planned in the approved target
structure, `docs/audit/TARGET_STRUCTURE.md`) or are local-only. Markdown
links point only at existing files.

## 2. Layers: durable, live, evidence, archive

| Layer | Purpose | Versioned | Lifetime |
|---|---|---|---|
| Durable | Rules, process, doctrine, architecture, user docs, history | Yes | Indefinite |
| Live | Current task state, open findings, pending questions, traps, handoff snapshot | No (backed up) | Days to weeks |
| Evidence | Logs, run results, review findings, research | No (backed up) | Until archived |
| Archive | Closed live state and superseded durable revisions | No | Indefinite, compressed |

Local-only (never tracked, machine-specific, backed up separately):
`HANDOFF.md`, `handoff/`, `ModelConf/`, `.kilo/`, `archive/`,
`base-archive/`. If a fact would still be true on another machine, it is
durable and belongs in this tree.

## 3. Document list

### Root files

| File | Owns | Status |
|---|---|---|
| [README.md](../README.md) | Product overview, quick start, user-facing config and API tables | active |
| [AGENTS.md](../AGENTS.md) | Agent entry point: precedence, autonomy, routing | active |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | Human contributor pointer | active |
| [CHANGELOG.md](../CHANGELOG.md) | Dated release history | active |
| [DESIGN.md](../DESIGN.md) | Historical design record; never a current spec | historical |
| [GIT_RULES.md](../GIT_RULES.md) | Deprecated redirect to `docs/process/GIT_WORKFLOW.md` | deprecated |
| [AUX-RULES.md](../AUX-RULES.md) | Deprecated redirect to the process documents | deprecated |

### docs/process/

| File | Owns | Status |
|---|---|---|
| [ENGINEERING.md](process/ENGINEERING.md) | Coding, testing, docs, security, platform standards; ASD-STE100 rules | active |
| [VERIFICATION.md](process/VERIFICATION.md) | The one gate, evidence rules, card/cardless tiers | active |
| [GIT_WORKFLOW.md](process/GIT_WORKFLOW.md) | Trunk model, commits, review, release | active |
| [TASKS.md](process/TASKS.md) | Task lifecycle, schema, DoR/DoD | active |
| [HANDOFF.md](process/HANDOFF.md) | Handoff protocol and templates (not live state) | active |
| [REVIEW.md](process/REVIEW.md) | Review lenses, finding format, closure | active |
| [PUBLICATION.md](process/PUBLICATION.md) | Scrub checklist before committing local artifacts | active |

### docs/architecture/

| File | Owns | Status |
|---|---|---|
| [DOCTRINE.md](architecture/DOCTRINE.md) | One-page binding invariants; every agent must carry it | active |
| [DECISIONS.md](architecture/DECISIONS.md) | Full ruling ledger: LOCKED / DEFERRED / ARCHIVED (tracked copy; `handoff/DECISIONS.md` is the local live copy pending removal) | active |
| [OVERVIEW.md](architecture/OVERVIEW.md) | Current architecture, modules, data flow | active |
| `ADR/` | Long-form decisions only, on demand | planned |

### docs/reference/ (planned)

`API.md` (complete HTTP contract), `MODELS.md` (ModelConf schema and engine
knobs), `TROUBLESHOOTING.md` (user-facing failures). These split out of the
README when a section passes roughly 150 lines. Until then the README owns
their content.

### docs/audit/ (this audit and successors)

| File | Owns |
|---|---|
| [AUDIT_REPORT.md](audit/AUDIT_REPORT.md) | Findings of the 2026-11-02 documentation and process audit |
| [TARGET_STRUCTURE.md](audit/TARGET_STRUCTURE.md) | Approved target structure and maintenance rules |
| [IMPLEMENTATION_PLAN.md](audit/IMPLEMENTATION_PLAN.md) | Migration phases and scopes |
| [PROCESS_IMPROVEMENTS.md](audit/PROCESS_IMPROVEMENTS.md) | Process improvement detail |
| [DOCUMENTATION_BACKLOG.md](audit/DOCUMENTATION_BACKLOG.md) | Remaining doc work rows |

### docs/research/ (optional)

Scrubbed research reports, promoted from `handoff/research/` on selection.
Empty today.

## 4. Documentation language

Durable documentation uses ASD-STE100 Simplified Technical English: active
voice, one idea per sentence, short sentences, no idioms or metaphors. The
rules, exceptions, and verification method live in
[docs/process/ENGINEERING.md](process/ENGINEERING.md). Exact operator
doctrine and LOCKED ruling wording stays verbatim and is never reworded for
style.

## 5. Maintenance rules

- Front matter: every durable file starts with `title`, `owner`, `status`,
  `audience`, `source_of_truth_for`, `last_reviewed`, `review_trigger`. A
  document without an owner is a defect. The operator owns every durable
  document.
- Same-change rule: a change that alters behavior updates every affected
  durable document in the same change, or states why none is affected.
- Contradiction rule: a contradiction is a stop-the-line event. Record it as
  a finding, fix the authoritative source, replace the copy with a pointer.
  Never fix both sides independently.
- Release rule: at each release, doctrine and decisions reflect every ruling
  since the last release; README and reference docs match the shipped
  behavior; changed durable text passes the ASD-STE100 review.
- Quarterly review: every durable document is reviewed each quarter; update
  its `last_reviewed`. New decisions and new engines trigger immediate review
  of the affected overview.
- Archive rule: a superseded durable document moves to an archive with a
  status banner, never silently deleted. `archive/` and `base-archive/` are
  frozen and are never read for current behavior.
- No duplication: one fact, one home; every other mention is a pointer. A
  labeled summary that links to the source is allowed; copying a command
  list, a doctrine paragraph, or a schema table is a defect.

## 6. Link and writing conventions

- Relative links only; no author-specific machine paths in durable docs.
  Documented product defaults in README/reference and exact quoted decisions are
  allowed; paths under a user profile are not.
- Link only existing files; planned paths stay plain text until created.
- Durable docs state commands and invariants, not volatile counts or file
  line numbers; dated numbers live in evidence.
