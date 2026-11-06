---
title: Documentation map
owner: operator
status: active
audience: [user, contributor, agent, architect]
source_of_truth_for: [documentation routing]
last_reviewed: 2026-11-06
review_trigger: release, quarterly, new-decision
---

# docs/DOCS_README.md

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
| Know what the OpenAI surface does and does not do | [docs/reference/COMPATIBILITY.md](reference/COMPATIBILITY.md) |
| Set up an engine to run with the harness | [docs/reference/ENGINE-RECIPES.md](reference/ENGINE-RECIPES.md) |
| Know the security policy or report a security problem | [SECURITY.md](../SECURITY.md) |
| Operate the harness: autostart, upgrade, backup, logs | [docs/reference/OPERATIONS.md](reference/OPERATIONS.md) |
| Recover from a down listener, stuck release, or refusal | [docs/reference/RUNBOOK.md](reference/RUNBOOK.md) |
| Diagnose a failure case | [docs/reference/TROUBLESHOOTING.md](reference/TROUBLESHOOTING.md) |
| Use the command line or the import verb | [docs/reference/CLI.md](reference/CLI.md) |
| Start from a ready model row | [examples/README.md](../examples/README.md) |
| Know why a decision was made | [docs/architecture/DECISIONS.md](architecture/DECISIONS.md) (and [DESIGN.md](../DESIGN.md) for history) |
| Add or change code | [docs/process/ENGINEERING.md](process/ENGINEERING.md) |
| Run the gate | [docs/process/VERIFICATION.md](process/VERIFICATION.md) |
| Commit, branch, or release | [docs/process/GIT_WORKFLOW.md](process/GIT_WORKFLOW.md) |
| Pick up a task | `HANDOFF.md` -> `handoff/PENDING_CHECKLIST.md` -> task file (local-only) |
| Hand off work | [docs/process/HANDOFF.md](process/HANDOFF.md) |
| Review code | [docs/process/REVIEW.md](process/REVIEW.md) |
| Find a trap | [docs/process/GOTCHAS.md](process/GOTCHAS.md) (machine-specific traps stay local-only) |
| See what changed | [CHANGELOG.md](../CHANGELOG.md) |

Paths in plain text do not exist yet (they are planned) or are local-only.
Markdown links point only at existing files.

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
| [README.md](../README.md) | Product overview, quick start, safety, navigation | active |
| [AGENTS.md](../AGENTS.md) | Agent entry point: precedence, autonomy, routing | active |
| [CONTRIBUTING.md](../CONTRIBUTING.md) | Human contributor pointer | active |
| [SECURITY.md](../SECURITY.md) | Vulnerability reporting and supported versions | active |
| [CHANGELOG.md](../CHANGELOG.md) | Dated release history | active |
| [DESIGN.md](../DESIGN.md) | Historical design record; never a current spec | historical |

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
| [GOTCHAS.md](process/GOTCHAS.md) | Contributor traps that cost real time; the local-only layer holds machine-specific ones | active |

### docs/architecture/

| File | Owns | Status |
|---|---|---|
| [DOCTRINE.md](architecture/DOCTRINE.md) | One-page binding invariants; every agent must carry it | active |
| [DECISIONS.md](architecture/DECISIONS.md) | Full ruling ledger: LOCKED / DEFERRED / DELIVERED / DATED RULINGS (single home) | active |
| [OVERVIEW.md](architecture/OVERVIEW.md) | Current architecture, modules, data flow | active |
| [DEFERRED-IDEAS.md](architecture/DEFERRED-IDEAS.md) | Design notes for Berth items: B8 warm-during-release spike (measurement open), B9 priority classes, B10 restart reconciliation; no rulings | active |

### docs/reference/

| File | Owns | Status |
|---|---|---|
| [CONFIG.md](reference/CONFIG.md) | Harness config keys and eviction rules | active |
| [MODELS.md](reference/MODELS.md) | ModelConf schema and engine-specific model keys | active |
| [ENGINE-RECIPES.md](reference/ENGINE-RECIPES.md) | Minimal per-engine setup for users and their agents | active |
| [API.md](reference/API.md) | HTTP control API contract, error codes, memory provenance | active |
| [COMPATIBILITY.md](reference/COMPATIBILITY.md) | OpenAI compatibility matrix, known-good configurations | active |
| [OPERATIONS.md](reference/OPERATIONS.md) | Autostart, file layout, upgrade, uninstall, backup, log retention | active |
| [TROUBLESHOOTING.md](reference/TROUBLESHOOTING.md) | Failure diagnosis | active |
| [RUNBOOK.md](reference/RUNBOOK.md) | Sanitized operator recovery procedures | active |
| [CLI.md](reference/CLI.md) | Command-line verbs and exit behavior | active |

### docs/evidence/ (dated card and measurement records)

| Document | Owns | Status |
|---|---|---|
| [v1.3.0-card-run.md](evidence/v1.3.0-card-run.md) | v1.3.0 CPU-llama card proof | historical |
| [v1.3.2-card-run.md](evidence/v1.3.2-card-run.md) | v1.3.2 Project Maya engine card proof | historical |
| [2026-11-04-engine-measurement.md](evidence/2026-11-04-engine-measurement.md) | Engine VRAM surface measurements (cited as current by MODELS.md) | active |

### examples/

| File | Owns | Status |
|---|---|---|
| [README.md](../examples/README.md) | Model-row templates: one row per engine, one row per category | active |

### docs/archive/

| File | Owns | Status |
|---|---|---|
| [README.md](archive/README.md) | Index and banner convention for superseded durable documents | active |

## 4. Documentation language

Durable documentation must use ASD-STE100 Simplified Technical English: active
voice, one idea per sentence, short sentences, no idioms or metaphors. New or
changed text must comply immediately; existing text migrates in phases. The
rules, exceptions, and verification method live in
[docs/process/ENGINEERING.md](process/ENGINEERING.md). Exact operator
doctrine and LOCKED ruling wording stays verbatim and is never reworded for
style.

## 5. Maintenance rules

- Front matter: every durable file starts with `title`, `owner`, `status`,
  `audience`, `source_of_truth_for`, `last_reviewed`, `review_trigger`. A
  document without an owner is a defect. The operator owns every durable
  document. `README.md` is the one exempt file: it is the storefront page and
  GitHub renders front matter there as a metadata table (operator ruling
  2026-11-05, `docs/architecture/DECISIONS.md`).
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
- Archive rule: a superseded durable project document moves to
  `docs/archive/` with a status banner, never silently deleted. A superseded
  page is listed in [docs/archive/README.md](archive/README.md).
  Maintenance and audit records live in the local-only layer, never in the
  public tree. `archive/` and `base-archive/` are frozen and are never read
  for current behavior.
- No duplication: one fact, one home; every other mention is a pointer. A
  labeled summary that links to the source is allowed; copying a command
  list, a doctrine paragraph, or a schema table is a defect.

## 6. Link and writing conventions

- Relative links only; no author-specific machine paths in durable docs.
  Documented product defaults in README/reference and exact quoted decisions are
  allowed; paths under a user profile are not.
- Link only existing files; planned paths stay plain text until created.
- A repo-relative path written as inline code must resolve (`check-docs` rule
  7). The local-only layer (`handoff/`, `ModelConf/`, `archive/`,
  `base-archive/`, `.kilo/`, `HANDOFF.md`) is exempt; a trailing `:line` range
  keeps its base file. `src/`, `tests/` and `scripts/` are also tried as roots.
- Durable docs state commands and invariants, not volatile counts or file
  line numbers; dated numbers live in evidence.
