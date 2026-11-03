---
title: Documentation inventory and classification
owner: operator
status: maintenance
audience: [operator, architect, agent]
source_of_truth_for: []
last_reviewed: 2026-11-03
review_trigger: none
---

# DOCUMENTATION_INVENTORY.md

| Field | Value |
|---|---|
| Initiative | 2026-11 documentation cleanup |
| Date | 2026-11-03 |
| Repository | `C:\Inference\SimpleHar-SE` |
| Branch / HEAD | `main` / `e931de5` (tag `v1.1.0`) |
| Working tree at start | Clean (`git status --porcelain` empty) |
| Scope | Every tracked and local-only instruction, architecture, process, audit, handoff, research, evidence, and archive document, plus documentation-bearing tooling |
| Method | Full-content review of every durable document and every local document that governs current work; directory and role review of dated evidence groups; link and reference scan; comparison with `src/`, `Cargo.toml`, `scripts/`, and `.github/workflows/ci.yml`; Git state and history inspection |
| Files modified by this inventory | None. This report and its two companions are new files under `docs/maintenance/`. |
| Code executed | Read-only checks only: `check-docs.mjs`, `check-instructions.mjs`, `check-tasks.mjs`, and `release-check.ps1`. No engine, model, card, or live harness was touched. |

## 1. Evidence basis and limitations

### 1.1 Confirmed facts

- The tracked tree was clean at the start of the inventory.
- 23 documentation files are tracked in Git: 7 root files, the documentation index, 7 process files, 3 architecture files, and 5 audit files.
- The durable documentation set before this initiative is 7,312 lines and 455,377 bytes. The five audit files are 3,331 lines and 206,864 bytes, or 45.6 % of lines and 45.4 % of bytes.
- `HANDOFF.md`, `handoff/`, `ModelConf/`, `archive/`, `base-archive/`, and `.kilo/` are local-only by `.gitignore`.
- The current stack was verified against `Cargo.toml`, `src/`, `scripts/`, and CI. It is Rust 2021, crate version 1.1.0.
- The three documentation checkers pass on the current tree: `check-docs` 15 durable docs, `check-instructions` PASS, `check-tasks` 4 task files. `release-check.ps1` passes: version 1.1.0, tag v1.1.0, changelog and README agree.
- A relative-link scan over the current Markdown set (excluding `target/`, `.git/`, frozen archive trees, and `ModelConf/`) found no broken project links. It found two placeholder links in a Kilo skill template only.
- `git worktree list` shows one worktree, the main tree. The stale worktrees named in the 2026-11-02 audit are gone.

### 1.2 Reported but not independently re-verified

- Card and GPU behavior. This inventory is cardless and read-only.
- The 57 files under `handoff/code-review/` (55 findings files plus `PLAN.md` and `PROMPT.md`) were reviewed by directory, role, and header, not line by line. They are dated evidence, not current guidance. The live register is `handoff/FINDINGS-OPEN.md`.
- The individual log files under `handoff/*/logs/` and `handoff/evidence/` were reviewed by directory, date, and role, not line by line.
- The live harness build and the deployed release in use. `HANDOFF.md` reports the live harness remains on v1.0.1 while v1.1.0 is staged; this was not checked against the running process.
- Repository visibility on the remote. Only the operator can confirm it.
- The exact contents of external research files under `D:\WORKSPACE-VS\research\`. They are outside this repository.

### 1.3 Assumptions

- The 2026-11-02 governance decisions D1-D11 remain the governing documentation architecture. This inventory does not redesign that architecture.
- Local-only files are still documentation for the agents that work on this machine, even when they are not shipped in Git.
- A dated audit is evidence of what was true on its date, not a current specification.

## 2. Current technology stack, verified from repository evidence

| Area | Current fact | Evidence |
|---|---|---|
| Language and edition | Rust 2021 | `Cargo.toml` |
| Crate and version | `simplehar-se` 1.1.0 | `Cargo.toml`; tag `v1.1.0` |
| Runtime | `tokio` 1 with explicit features: rt-multi-thread, macros, signal, fs, process, sync, time, net, io-util | `Cargo.toml` |
| HTTP server | `axum` 0.8 with `tower-http` 0.6 (trace, compression-gzip, timeout) | `Cargo.toml`; `src/api/mod.rs` |
| HTTP client | `reqwest` 0.12, default features off, `json` + `stream` | `Cargo.toml`; `src/api/mod.rs` |
| GPU witness | `nvml-wrapper` 0.10 with `nvidia-smi` fallback | `Cargo.toml`; `src/witness.rs` |
| Tray and Win32 | `tray-icon` 0.21; `windows` 0.62 (Job Objects, mutex, message pump, GDI) | `Cargo.toml`; `src/tray.rs`, `src/process.rs` |
| Logging | `tracing` + `tracing-subscriber` with env filter | `Cargo.toml`; `src/observability.rs` |
| Errors | `thiserror` 2 in domain code; `anyhow` 1 at `main` | `Cargo.toml`; source modules |
| Serialization | `serde`, `serde_json`; no YAML crate, the model parser is hand-written | `Cargo.toml`; `src/model/scan.rs`, `src/model/writeback.rs` |
| Script staging | `base64` 0.22 | `Cargo.toml`; `src/process.rs` |
| Streaming | `tokio-stream` 0.1 (sync) | `Cargo.toml`; `src/api/mod.rs` |
| Test tooling | `tower` 0.5 oneshot in dev; Node DOM stub in `tests/webui/` | `Cargo.toml`; `tests/webui/webui_dom_test.mjs` |
| CI | GitHub Actions, `windows-latest`, stable Rust, Node 20, cardless only | `.github/workflows/ci.yml` |
| Local shell | Windows PowerShell 5.1; `; if ($?)` chaining; `curl.exe`; no assumed `rg` | `docs/process/ENGINEERING.md`; `HANDOFF.md` |
| External engines | `llama` native exe, `strata` Python venv, `ninfer` container in WSL2; `mock` test-only | `src/engine/`; `README.md` |

## 3. Technology and component status

| Component or term | Status | Evidence |
|---|---|---|
| `llama`, `strata`, `ninfer` adapters | Active | `src/engine/llama.rs`, `strata.rs`, `ninfer.rs`; `src/engine/policy.rs` |
| `mock` engine | Active but gated | `src/engine/mock.rs`; `AppConfig.allow_mock_engine` defaults false |
| L1/L2/L3 configuration model | Active | `docs/architecture/DOCTRINE.md` item 11; `src/import.rs`; `src/model/scan.rs` |
| `simplehar.json` legacy name | Deprecated but present | Read-only for auto-discovery and migration; explicit `SIMPLEHAR_CONFIG` selection is a write target (`DECISIONS.md` 2026-11-03 ruling) |
| v3.3 model aliases | Deprecated but present | `src/catalog.rs`; README says the harness name is canonical |
| `strata_config` external profile mode | Removed / superseded | Refused at scan and adapter; `README.md` documents the refusal; `DECISIONS.md` row 19 |
| `ninfer_autostart` legacy path | Removed | Refused at scan and adapter; `README.md`; `DECISIONS.md` row 18 |
| Strata copy-in migration | Superseded | `DESIGN.md` addendum; `DECISIONS.md` rows 19/19c; no code path |
| `external: true`, `sticky:`, `preload`, `protected_containers`, `adopt_externals`, `auto_register_externals` | Removed or inert | `DECISIONS.md` LOCKED; `DOCTRINE.md` item 18; no current code path |
| v3.3 ports 8009, 8030, 18009-18011, 18203, 18204 | Historical | `base-archive/README.md`; `DESIGN.md` history; current README uses 8708-8711 and 18109+ |
| `polars`, `ratatui`, `crossterm`, `dioxus`, `maturin`, `uv`, `indicatif`, `secrecy` | Not in the current stack | `Cargo.toml`; only the conditional appendix of `docs/process/ENGINEERING.md` names them |
| Windows tray, Job Objects, single-instance mutex | Active | `src/tray.rs`, `src/process.rs` |
| Embedded Pico CSS + vanilla JavaScript page | Active | `src/webui/`; `Cargo.toml` has no WASM or JS framework |
| GitHub Actions CI | Active | `.github/workflows/ci.yml` |
| Node 20 | Active in CI and the DOM stub | `.github/workflows/ci.yml`; `tests/webui/` |

## 4. Documentation layers and size

| Layer | Location | Tracked | Files | Purpose |
|---|---|---:|---:|---|
| Durable | `README.md`, `AGENTS.md`, `CONTRIBUTING.md`, `CHANGELOG.md`, `DESIGN.md`, `docs/` | Yes | 23 pre-cleanup | Rules, process, architecture, user guide, audit history |
| Live | `HANDOFF.md`, `handoff/` | No | see section 6 | Current task state, findings, traps, task files |
| Evidence | `handoff/evidence/`, `handoff/*/logs/`, review and research files | No | 80+ | Dated proof of runs, reviews, and investigations |
| Archive | `handoff/archive/`, `archive/`, `base-archive/` | No | 20+ | Closed state and frozen prior generations |
| Tooling instructions | `.kilo/skills/`, `scripts/`, `.github/` | Mixed | 15+ | Agent skills and executable process checks |

## 5. Durable documentation inventory

### 5.1 Root files

| Path | Subject | Class | Current relevance | Main dependencies | Recommended action |
|---|---|---|---|---|---|
| `README.md` | Product overview, setup, configuration, model schema, API table, operations | A | Current; reflects template modes and v1.1.0 | `Cargo.toml`, `src/`, `CHANGELOG.md`, `ninfer-autostart-template.sh` | Keep. Optional: add the missing `allow_mock_engine` row (open finding R2-11-07) |
| `AGENTS.md` | Agent entry point, precedence, autonomy, reading matrix, safety fences | A | Current; 159 lines, under its 200-line budget | `docs/process/*`, `docs/architecture/*`, live layer | Keep. Optional: clarify the precedence of code versus descriptive docs |
| `CONTRIBUTING.md` | Human contributor pointer | A | Current; owns no rules | `README.md`, `docs/DOCS_README.md`, process docs | Keep |
| `CHANGELOG.md` | Dated release history | A | Current; v1.1.0 entry matches tag and artifact hash | `Cargo.toml`, `README.md`, release check | Keep |
| `DESIGN.md` | Historical design decomposition and dated addenda | B | Historical rationale; clearly marked, but the banner has stale future tense and a design-time DRAFT line | `docs/architecture/OVERVIEW.md`, `docs/architecture/DECISIONS.md`, `base-archive/` | Keep as historical. Fix the banner wording and the DRAFT line. Do not make it default reading |
| `GIT_RULES.md` | Redirect to `docs/process/GIT_WORKFLOW.md` | D | Superseded; 14 lines, no unique rule | `docs/process/GIT_WORKFLOW.md` | Delete after approval |
| `AUX-RULES.md` | Redirect to the process documents | D | Superseded; 21 lines, no unique rule | `docs/process/*` | Delete after approval |

### 5.2 Documentation index

| Path | Subject | Class | Current relevance | Main dependencies | Recommended action |
|---|---|---|---|---|---|
| `docs/DOCS_README.md` | Documentation map, layer model, maintenance rules, link conventions | A | Current, but section 3 still presents `docs/audit/` as the current audit layer and cites it as the authority for planned paths | All durable docs; `docs/audit/`; `docs/process/*` | Update after the audit move: replace the `docs/audit/` section, add `docs/maintenance/`, remove empty planned rows, fix the planned-path sentence |

### 5.3 Process documents

| Path | Subject | Class | Current relevance | Main dependencies | Recommended action |
|---|---|---|---|---|---|
| `docs/process/ENGINEERING.md` | Coding, testing, documentation, security, platform standards; ASD-STE100 | A | Current. The conditional appendix keeps unused stacks (`polars`, `ratatui`, `dioxus`, PyO3) as non-mandates | `Cargo.toml`; `PUBLICATION.md`; `VERIFICATION.md`; `REVIEW.md` | Keep. Optional: trim the unused conditional stacks to reduce routine reading cost |
| `docs/process/VERIFICATION.md` | The one cardless gate, card rules, evidence vocabulary | A | Current; matches `scripts/gate.ps1` command for command | `scripts/gate.ps1`; `handoff/evidence/` | Keep |
| `docs/process/GIT_WORKFLOW.md` | Trunk model, worktrees, commit style, release, protected operations | A | Current; matches repository practice and `release-check.ps1` | `AGENTS.md`; `VERIFICATION.md`; `REVIEW.md` | Keep |
| `docs/process/TASKS.md` | Task lifecycle, schema, queue row, DoR/DoD, archive | A | Current; matches `handoff/tasks/` and `check-tasks.mjs` | `VERIFICATION.md`; `REVIEW.md`; live layer | Keep |
| `docs/process/HANDOFF.md` | Handoff protocol, checkpoints, dirty-tree rule, snapshot template | A | Current; matches root `HANDOFF.md` | `TASKS.md`; live layer | Keep |
| `docs/process/REVIEW.md` | Review method, lenses, finding format, closure, evidence retention | A | Current method, but the retention path `handoff/evidence/reviews/` does not exist; actual evidence uses `handoff/evidence/<date>-<slug>/` | `FINDINGS-OPEN.md`; `handoff/code-review/`; `VERIFICATION.md` | Keep. Fix the evidence path. Update the code-review example pointer if that directory is archived |
| `docs/process/PUBLICATION.md` | Scrub checklist before local artifacts become durable | A | Current | `ENGINEERING.md`; `.gitignore` | Keep |

### 5.4 Architecture documents

| Path | Subject | Class | Current relevance | Main dependencies | Recommended action |
|---|---|---|---|---|---|
| `docs/architecture/DOCTRINE.md` | 18 binding product invariants | A | Current; matches the shipped rules and `DECISIONS.md` | `DECISIONS.md`; `OVERVIEW.md`; live evidence | Keep |
| `docs/architecture/DECISIONS.md` | Full ruling ledger: LOCKED, DEFERRED, dated rulings | A, with status defects | Current decisions, but section 3 is labelled "ARCHIVED (tombstones)" while holding active multi-paragraph rulings; DEFERRED still holds a delivered item; two `research/` references are ambiguous | `DOCTRINE.md`; `handoff/research/`; local evidence; external research | Keep every ruling. Add explicit lifecycle statuses, fix the section label, move the delivered DEFERRED item, qualify local and external references |
| `docs/architecture/OVERVIEW.md` | Current architecture, modules, data flow, invariants | A | Current; verified against `src/` and `Cargo.toml` on 2026-11-02 | `src/`; `Cargo.toml`; `DOCTRINE.md`; `README.md` | Keep |

### 5.5 Audit documents

| Path | Subject | Class | Current relevance | Main dependencies | Recommended action |
|---|---|---|---|---|---|
| `docs/audit/AUDIT_REPORT.md` | Findings of the 2026-11-02 documentation and process audit | C | Dated snapshot. Many "current state" statements are now false (57 tracked files, no CI, no `docs/`, no changelog) | `docs/audit/TARGET_STRUCTURE.md`, `IMPLEMENTATION_PLAN.md`, `PROCESS_IMPROVEMENTS.md`, `DOCUMENTATION_BACKLOG.md`; old `handoff/` paths | Archive as immutable history. Do not edit its findings |
| `docs/audit/TARGET_STRUCTURE.md` | Approved target documentation and process architecture | C | Historical design record for the current docs architecture. Some paths are stale (`docs/README.md`, `docs/audit/` as a permanent layer) | `AUDIT_REPORT.md`; `IMPLEMENTATION_PLAN.md`; `scripts/check-docs.mjs` comment | Archive. Its live rules now belong to `docs/DOCS_README.md` section 5 |
| `docs/audit/IMPLEMENTATION_PLAN.md` | Migration phases and owner decisions D1-D11 | C | Status says "execution in progress"; the phases are complete or superseded | `AUDIT_REPORT.md`; `TARGET_STRUCTURE.md`; `PROCESS_IMPROVEMENTS.md` | Archive. Record completion in the cleanup report |
| `docs/audit/PROCESS_IMPROVEMENTS.md` | Process improvement recommendations | C | Historical recommendations; most are implemented | `AUDIT_REPORT.md`; `TARGET_STRUCTURE.md` | Archive |
| `docs/audit/DOCUMENTATION_BACKLOG.md` | 36 documentation and process backlog items | C, with residual open items | Mostly delivered. 8 rows remain open, partly open, or deferred | `AUDIT_REPORT.md`; `IMPLEMENTATION_PLAN.md`; current process docs | Archive after extracting the residual rows into a live task |

## 6. Local live, evidence, and archive inventory

### 6.1 Live state

| Path | Subject | Class | Current relevance | Recommended action |
|---|---|---|---|---|
| `HANDOFF.md` | Live snapshot, machine safety, next action, routing | A | Current at 2026-11-03 | Update after the cleanup starts; keep under 120 lines |
| `handoff/PENDING_CHECKLIST.md` | Queue index, one row per task | A | Current | Keep; add or close rows as the cleanup proceeds |
| `handoff/FINDINGS-OPEN.md` | Sole home for open review findings | A | Current; 23 open rows, including grouped and INFO rows, plus a large accepted/closed table | Keep; do not duplicate into other ledgers |
| `handoff/DECISIONS-PENDING.md` | Unresolved operator questions only | A | Current; no open question | Keep |
| `handoff/DECISIONS.md` | Pointer stub to the tracked ledger | D | Superseded; 4 lines; cites the audit that will move | Delete after approval |
| `handoff/GOTCHAS.md` | Hard-won machine and process traps | A, with stale entries | Mostly current. Several traps name removed paths (`ninfer_autostart`, the 9b runner, old ports) | Keep; prune only with operator review |
| `handoff/tasks/*.md` | Four active task files | A | Current; schema-valid | Keep |
| `handoff/upstream-ninfer-post-ready-memory-summary.md` | External upstream ask | A | Current | Keep |

### 6.2 Evidence and research

| Path | Subject | Class | Current relevance | Recommended action |
|---|---|---|---|---|
| `handoff/code-review/` (57 files) | Review plans, prompts, and per-round findings from 2026-10-27 to 2026-10-29 | C | Historical evidence. `PLAN.md` and `PROMPT.md` are still cited as method examples | Keep `PLAN.md` and `PROMPT.md`; archive the findings files after approval |
| `handoff/research/ninfer-telemetry-2026-10-30.md` | Ninfer memory-report source investigation | B | Cited by `DECISIONS.md`; contains machine paths | Keep local; label the decision citation as local evidence |
| `handoff/research/cpu-offload-scoping-2026-10-28.md` | CPU-offload lifecycle audit | B | Cited by `DECISIONS.md` | Keep local; label the citation |
| `handoff/research/reload-slot-drift-2026-10-28.md` | Reload-drift investigation | C | Its ruling shipped; no current decision cites the file | Keep or archive with the other research |
| `handoff/research/llamastash-2026-10-28.md` | External product comparison | C | No current decision cites it; the engine-schema decision is already implemented | Archive or delete after approval |
| `handoff/research/archive-policy-example.md` | Maintenance archive policy input | B | Source for the proposed `docs/maintenance/` rules | Adopt into `docs/maintenance/README.md`, then archive the input copy |
| `handoff/evidence/2026-11-02-governance/` | Governance gate logs | C | Historical evidence | Retain until the archive window; then compress or archive |
| `handoff/evidence/2026-11-03-n7/` | Dependency-hygiene gate log | C | Historical evidence | Retain |
| `handoff/evidence/2026-11-03-r2/` | Review gate and findings evidence | C | Current release evidence | Retain |
| `handoff/wave-verify/` | Card-window package, logs, and results for the 1.1.0 release | B | Current release evidence; the package is the live card procedure | Keep |
| `handoff/t-suite/` | T-suite prompt, run book, results, and logs | B | Current suite definition | Keep |
| `handoff/gate-honesty/` | Gate-honesty incident, forensics, and problem log | C | Historical incident evidence; its lessons live in `GOTCHAS.md` | Archive after approval |
| `handoff/r1r9-verify/` | Gate logs from 2026-10-31 to 2026-11-01 | C | Superseded by later evidence | Archive after approval |
| `handoff/strata-adoption/` | Adopted strata rows | C | Evidence of the adoption round | Retain |

### 6.3 Archives and legacy trees

| Path | Subject | Class | Current relevance | Recommended action |
|---|---|---|---|---|
| `handoff/archive/2026-11-03/` | Ten closed task files | C | Closed work; useful traceability | Keep |
| `handoff/archive/PENDING_CHECKLIST-2026-11-02-*.md` | Pre-migration queue snapshots (364 and 795 lines) | C | Obsolete session-start chains and status vocabularies | Keep frozen; add an archive notice at `handoff/archive/README.md` |
| `handoff/archive/HANDOFF-2026-11-02-pre-migration.md` | Pre-migration snapshot | C | Obsolete start chain | Keep frozen |
| `handoff/archive/T-SUITE-RESULTS-2026-11-02.md` | Card-window results | C | Record of the 1.1.0 card run | Keep |
| `archive/HANDOFF-2026-10-26.md` | Early design-session handoff (706 lines) | C | Historical rationale only | Keep frozen; add `archive/README.md` notice |
| `archive/HANDOFF-2026-10-26T1913Z.md` | Early handoff (181 lines) | C | Historical rationale only | Keep frozen |
| `archive/t-suite-scratch-2026-10-27/` | Scratch logs | E | No continuing value | Delete after approval, or leave frozen |
| `base-archive/README.md` | v3.3 product README | C, but misleading | Reads like a current product spec; describes removed ports and removed features | Add `base-archive/ARCHIVE-NOTICE.md`; never edit the baseline content |
| `base-archive/PLAN.md` | v3.3 implementation plan | C | Historical baseline | Keep frozen |
| `base-archive/TEST-SWITCHING.md` | v3.2.1 test plan | C | Historical baseline | Keep frozen |
| `base-archive/TEST-RESULTS-20261023-1042.md` | v3.2.1 NO-GO test report | C | Source of the design goals and defect register | Keep frozen |
| `base-archive/ModelConf/`, `base-archive/src/` | v3.3 rows and source | C | Reference baseline; never built | Keep frozen |

## 7. Agent instructions and tooling

| Path | Subject | Class | Current relevance | Recommended action |
|---|---|---|---|---|
| `.kilo/skills/installation-guide/` | Installation guide skill and templates | A (local) | Generic tooling; referenced by the operator workflow, not by project rules | Keep |
| `.kilo/skills/skill-creator/` | Skill authoring guide | A (local) | Generic tooling | Keep |
| `.kilo/skills/web-investigator/` | Research workflow | A (local) | Generic tooling | Keep |
| `scripts/gate.ps1` | Executable cardless gate | A | Current; matches `VERIFICATION.md` | Keep |
| `scripts/check-docs.mjs` | Documentation checker | A | Current; its header cites `docs/audit/TARGET_STRUCTURE.md` | Update the comment after the audit move |
| `scripts/check-instructions.mjs` | Instruction and anti-duplication checker | A | Current | Keep |
| `scripts/check-tasks.mjs` | Task file checker | A | Current | Keep |
| `scripts/release-check.ps1` | Release consistency check | A | Current; passes on v1.1.0 | Keep |
| `.github/workflows/ci.yml` | Cardless CI | A | Current | Keep |
| `ninfer-autostart-template.sh` | Public copy of the canonical ninfer script | A | Current; README links to it | Keep; do not create a second copy |
| `.gitignore` | Tracked versus local policy comments | A | Current | Keep; update only if the archive layout changes |
| `Cargo.toml` dependency comments | Inline dependency rationale | A | Current; one comment contains mojibake, but it is code metadata | Leave in this documentation cleanup |

## 8. Classification summary

| Class | Meaning | Durable count | Main examples |
|---|---|---:|---|
| A | Active and current | 15 | README, AGENTS, CONTRIBUTING, CHANGELOG, DOCS_README, 7 process docs, DOCTRINE, DECISIONS, OVERVIEW |
| B | Active with historical references | 1 | DESIGN |
| C | Historical or archived | 5 | The five `docs/audit/` files, plus local evidence and frozen archives |
| D | Superseded | 3 | `GIT_RULES.md`, `AUX-RULES.md`, `handoff/DECISIONS.md` |
| E | Obsolete or irrelevant | 1 group | `archive/t-suite-scratch-2026-10-27/` |
| F | Needs review | 5 topics | DECISIONS status pass, GOTCHAS pruning, code-review archive scope, DESIGN disposition, unused conditional appendix |

## 9. Gaps and items that need review

1. **`docs/audit/` is the largest single documentation block and is 45 % of the durable set.** It is historical but still presented in the index as the current audit layer. This is the main context-efficiency defect.
2. **`docs/DOCS_README.md` cites `docs/audit/TARGET_STRUCTURE.md` as the authority for planned paths.** A current index must not delegate its routing rules to a historical audit.
3. **`DECISIONS.md` status labels are misleading.** The "ARCHIVED (tombstones)" section holds active rulings, and a delivered item remains under DEFERRED.
4. **`REVIEW.md` names a review-evidence path that does not exist.**
5. **`DESIGN.md` is correctly marked historical, but its banner says the current architecture "will live" in `OVERVIEW.md`, and a design-time `DRAFT` line remains.** Both read as unfinished work.
6. **`GIT_RULES.md` and `AUX-RULES.md` are pure redirects.** They add noise to the root and are not checked by `check-docs`.
7. **`handoff/DECISIONS.md` is a pointer stub that cites an audit file scheduled to move.**
8. **`base-archive/README.md` reads like a current product specification.** The directory is frozen, but the file has no archive notice.
9. **`archive/` has no README.** Old handoffs still carry obsolete session-start instructions.
10. **No `docs/reference/` split has happened.** The README is 548 lines, but each candidate section is below the agreed ~150-line threshold, so this remains deferred by policy, not forgotten.
11. **`docs/research/` is listed as an optional layer with no content.** It adds routing noise with no content.
12. **The `docs/audit/DOCUMENTATION_BACKLOG.md` has 8 residual rows** that will be lost if the backlog is archived without extraction.
13. **`README.md` omits `allow_mock_engine`.** This is already open as finding R2-11-07; it is not a cleanup invention.
14. **`AGENTS.md` precedence puts descriptive docs above code, while the conflict rule makes a code-doc contradiction a doc defect.** The two statements can be read as contradictory.
15. **`DESIGN.md` is the only durable document with non-ASCII mojibake.** The title and several body lines decode as a Cyrillic mojibake sequence for an em dash. The content is historical, but the defect is visible.
16. **No documented backup policy exists for the live layer.** `HANDOFF.md` names a backup directory, but no process document owns cadence, retention, or restore verification.
17. **No fresh-clone check and no process-health report exist.** Both remain open from the 2026-11-02 backlog.
18. **Technical debt is still split across `FINDINGS-OPEN.md`, `DECISIONS.md` DEFERRED, and task leftovers.** No single debt index exists.

## 10. Boundary of this inventory

Application source, runtime configuration, `ModelConf/` rows, and test fixtures were inspected only to verify documentation claims. They are not documentation and are not proposed for change. `Cargo.lock` and `target/` were not reviewed. The three reports of this initiative are maintenance artifacts; they are not part of the 23-file durable count and are not operational documentation.
