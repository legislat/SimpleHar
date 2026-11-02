# SimpleHar SE - Documentation and Development Process Audit

| Field | Value |
|---|---|
| Audit date | 2026-11-02 |
| Repository | `C:\Inference\SimpleHar-SE` |
| Branch / HEAD at audit | `main` / `a454dbb` |
| Working tree | Clean (`git status --porcelain` empty) |
| Upstream | `origin/main` = `1596573`; 1 commit unpushed; tags `v1.0.0`, `v1.0.1` |
| Audit scope | All tracked and local-only instructions, documentation, handoff ledgers, Git practice, task process, context transfer |
| Method | Full file-content review; structural mapping; Git history and state inspection; documentation-to-code verification |
| Files modified by this audit | None. Only the five reports in `docs/audit/` were created. |
| Code executed | Read-only inspection only. The project-reported gate was **not** re-run during this audit. |

---

## 0. Evidence basis and limitations

### 0.1 Confirmed facts

- The working tree was clean at audit start and end.
- 57 files are tracked by Git. `AGENTS.md`, `AUX-RULES.md`, `DESIGN.md`, `GIT_RULES.md`, `HANDOFF.md`, `handoff/`, `archive/`, `base-archive/`, `ModelConf/`, and `.kilo/` are deliberately ignored (`.gitignore:2-18`).
- The project has a mature local-only governance system: a live handoff, an executable queue, a findings ledger, a decisions ledger, a gotcha ledger, review protocols, and retained evidence.
- `README.md` is the only tracked product documentation file other than `Cargo.toml`, `LICENSE`, and `ninfer-autostart-template.sh`.
- No CI configuration, `CONTRIBUTING.md`, `CHANGELOG.md`, or `docs/` tree existed before this audit. `git worktree list` shows one named stale branch worktree and two detached worktree-pool entries.

### 0.2 Facts reported by project documents but not independently re-verified

- At audit time, gate results (`703` tests, `0` failures, `24` ignored; DOM `102` legs) were reported in the then-current handoff and queue. The pre-migration queue is archived at `handoff/archive/PENDING_CHECKLIST-2026-11-02-pre-migration.md`; current evidence is in `handoff/evidence/2026-11-02-governance/gate.txt`. They were not re-run during the audit because it was constrained to read-only analysis and the machine hosts live inference services.
- The GitHub repository visibility is unconfirmed. Only the operator can check the remote setting; `gh` was unavailable on this machine.
- Runtime behavior around GPU/card windows was not exercised. Card evidence is explicitly gated to an operator-run, freed-card window (`docs/architecture/DECISIONS.md`).

### 0.3 Assumptions

- The project is currently maintained by one operator with AI agents, and the machine-specific chronicle is intentionally local-only (operator decision recorded at `.gitignore:9` and `handoff/HANDOFF.md:50-69`).
- The goal of this audit includes making the project reproducible for a new agent or contributor on another machine. If the project is intended to remain a single-machine, single-operator artifact forever, the severity of the portability findings should be reduced accordingly.
- The house commit-message style is intentional. This is supported by 30 consecutive narrative commit subjects and the absence of any counter-example in recent history.

### 0.4 Unknown / needs clarification

- Whether the remote repository is private.
- Whether the operator wants a team-oriented workflow or only safer multi-agent work on one machine.
- Whether the local `handoff/` directory has an independent backup today. `handoff/` is approximately 4.9 MiB and is a single copy.
- Whether `.kilo` global configuration contributes additional rules that are not visible in the repository.

---

## 1. Executive summary

SimpleHar SE is **process-rich and portability-poor**.

The project already does many things that most repositories do not: it records binding operator rulings, distinguishes settled from pending decisions, enforces a named commit gate, requires falsifier tests for review closures, keeps review evidence, separates cardless work from card-consuming work, and uses a deliberately small live handoff. These are genuine strengths and should be preserved.

The central problem is that nearly all of this system is **local-only and unversioned**. A fresh clone contains source code, tests, a README, and a license. It does not contain the rules that govern changes, the design rationale, the decision ledger, the task queue, the review protocol, or the handoff protocol. Every reference in code and commit messages to `DESIGN.md`, `AGENTS.md`, `handoff/DECISIONS.md`, or `GOTCHAS.md` dangles on a clean clone. The repository is therefore not self-describing: it cannot reproduce its own development system.

The second problem is **documentation drift at the user-facing boundary**. `README.md` still describes the superseded `strata_config` mode as required and still says strata takes its bind address from the profile, while the shipped code refuses `strata_config` at scan and forces loopback through rendered-profile argv. This is not cosmetic: a user following the README cannot configure a supported strata engine.

The third problem is **instruction overload and ambiguity**. `AGENTS.md` is a generic Rust/Python/Web/TUI cookbook, most of which does not apply to this crate. The actual project workflow lives in `HANDOFF.md`, `AUX-RULES.md`, `GIT_RULES.md`, and `handoff/*.md`, but the agent entry point does not route to them. Precedence between these files is asserted only as "supplement" and never defined. The commit gate appears in four places with three different command sets. Agent autonomy is scattered across five files with no single approval matrix.

The fourth problem is **process fragmentation**. The project has strong artifacts but no unified task schema: queue rows are prose, findings have review IDs, decisions have row numbers, and work items have labels such as `R9`, `B3`, `N5`, and `12-P`. There is no standard place for acceptance criteria, dependencies, affected files, owner, or the next command. Handoff works well at session end but has no mid-session checkpoint and no dirty-tree transfer protocol.

The fifth problem is **no automation of the process itself**. The gate is manual, documentation links are unchecked, templates are unvalidated, and no CI exists. The rules are only as reliable as the agent's memory and discipline.

### Overall assessment

| Dimension | Score (0-5) | Note |
|---|---:|---|
| Agent instruction clarity | 3 | Strong content, weak routing and precedence |
| Instruction consistency | 2 | Gate and commit-style contradictions confirmed |
| Git workflow definition | 2 | Generic rules; actual trunk-only model undocumented |
| Task management | 3 | Strong queue and review closure; no task schema |
| Context/handoff on this machine | 4 | Disciplined, budgeted, single-home intent |
| Context/handoff portability | 1 | Entire system untracked; single copy |
| Architecture documentation | 3 | Rich rationale; stale status and superseded addendum |
| Technical documentation accuracy | 2 | README contradicts shipped strata behavior |
| Planning and debt management | 3 | Three ledgers; no unified debt register |
| Quality assurance / automation | 1 | Manual gate only; no CI or doc checks |
| New-contributor onboarding | 1 | A clone has no instructions and no workflow |
| **Overall** | **2.4 / 5** | Defined locally, not repeatable elsewhere |

Maturity label: **Repeatable only on the current machine; not reproducible across machines or contributors.**

---

## 2. Repository structure and information map

### 2.1 Current tree (semantic)

```text
C:\Inference\SimpleHar-SE
|-- README.md                      tracked   user guide, product reference, operations
|-- Cargo.toml / Cargo.lock        tracked   build + dependency comments
|-- LICENSE                        tracked   Apache-2.0
|-- ninfer-autostart-template.sh   tracked   public copy of canonical script
|-- src/                           tracked   ~1.5 MiB Rust source, module doc comments
|-- tests/                         tracked   Rust integration tests + Node DOM test
|
|-- AGENTS.md                      ignored   agent instruction cookbook
|-- AUX-RULES.md                   ignored   review/bookkeeping/gate supplements
|-- GIT_RULES.md                   ignored   Git safety and commit rules
|-- DESIGN.md                      ignored   85 KiB design decomposition + defect register
|-- HANDOFF.md                     ignored   live snapshot, safety, session protocol
|-- handoff/                       ignored   ~4.9 MiB live ledgers + evidence archives
|   |-- PENDING_CHECKLIST.md       live      queue and work narratives
|   |-- FINDINGS-OPEN.md           live      open review findings
|   |-- DECISIONS.md               live      binding doctrine, deferred, archived rulings
|   |-- DECISIONS-PENDING.md       live      unresolved operator questions
|   |-- GOTCHAS.md                 live      tagged operational traps
|   |-- code-review/               evidence  review plan, prompt, 40+ findings files
|   |-- research/                  evidence  four research reports
|   |-- t-suite/, wave-verify/, gate-honesty/, r1r9-verify/
|   |-- strata-adoption/           evidence  adopted row evidence
|   `-- archive/                   archive   full pre-slim checklist
|-- archive/                       ignored   old handoffs + scratch logs
|-- base-archive/                  ignored   complete v3.3 baseline tree
|-- ModelConf/                     ignored   25 flat YAML model rows
`-- .kilo/                         ignored   skills, worktree state, agent-manager.json
```

### 2.2 Where each information type currently lives

| Information type | Current location(s) | Assessment |
|---|---|---|
| Product purpose and user instructions | `README.md` | Good content; stale strata sections |
| High-level architecture | `DESIGN.md` Sections 0-19, `src/lib.rs` | Mixed with historical design narration |
| Binding technical doctrine | `handoff/DECISIONS.md` LOCKED | Strong, but local-only |
| Settled rulings and rationale | `handoff/DECISIONS.md` ARCHIVED + commit bodies | Strong, but local-only |
| Open questions for the operator | `handoff/DECISIONS-PENDING.md` | Good, but duplicates N2/N3 detail |
| Agent behavior | `AGENTS.md`, `AUX-RULES.md`, `GIT_RULES.md` | Wrong entry-point routing; generic content |
| Review protocol | `handoff/code-review/PLAN.md` + `PROMPT.md`, `AUX-RULES.md` | Strong but split and undiscoverable |
| Task definition and order | `handoff/PENDING_CHECKLIST.md` | Prose rows; no unified schema |
| Task status | `HANDOFF.md` Section 2, `PENDING_CHECKLIST.md`, findings, commit bodies | Duplicated across homes |
| Open defects / technical debt | `handoff/FINDINGS-OPEN.md`, `PENDING_CHECKLIST.md` leftovers, `DECISIONS.md` DEFERRED | Three ledgers |
| Operational traps | `handoff/GOTCHAS.md` | Excellent content, local-only |
| Execution evidence | `handoff/*/logs/`, `RESULTS.md`, `forensics/` | Strong, directory convention implicit |
| Change history | Git tags and commit bodies | No changelog for users |
| Historical source/reference | `base-archive/` | Clearly labeled, safe if left ignored |
| Machine-specific paths/ports | `HANDOFF.md` Section 3, `GOTCHAS.md`, `ModelConf/` | Correctly local-only |

### 2.3 Logical overlaps

1. **Gate definition**: `AGENTS.md:225-235`, `AUX-RULES.md:33-36`, `handoff/PENDING_CHECKLIST.md:32-39`, `DESIGN.md:869`.
2. **Commit safety and autonomy**: `GIT_RULES.md:29-67`, `HANDOFF.md:108-109`, `AUX-RULES.md:6-13`.
3. **Review closure**: `AUX-RULES.md:15-29`, `handoff/code-review/PLAN.md`, `handoff/FINDINGS-OPEN.md`.
4. **Doctrine**: `handoff/DECISIONS.md` LOCKED, `HANDOFF.md` Section 5 digest, `DESIGN.md` Addendum and Section 20.
5. **Status**: `HANDOFF.md` Section 2, `PENDING_CHECKLIST.md` baseline, `FINDINGS-OPEN.md`, `DECISIONS-PENDING.md`.
6. **Strata behavior**: `README.md`, `DESIGN.md` Addendum, `handoff/DECISIONS.md` rows 19-19d, code comments in `src/engine/strata.rs`.
7. **Nifer template**: `README.md:272-337` and `ninfer-autostart-template.sh`.

---

## 3. Documentation and instruction inventory

### 3.1 Agent instructions and rules

| File | Lines / size | Tracked | Role as written | State |
|---|---|---:|---|---|
| `AGENTS.md` | 239 / 13.0 KB | No | Generic Rust + Python + Web + TUI standards; pre-commit list | Mostly accurate but generic; not project-routing |
| `GIT_RULES.md` | 77 / 3.3 KB | No | Safe staging, commit quality, protected operations | Generic; commit style contradicts history |
| `AUX-RULES.md` | 41 / 2.4 KB | No | Subagent policy, single-home bookkeeping, review closure, gate, release | High-value, but gate duplicates and is stale-prone |
| `HANDOFF.md` | 118 / 8.0 KB | No | Bootstrap, snapshot, safety, doctrine digest, closing protocol | Disciplined; contains detailed state despite "pointers only" claim |
| `DESIGN.md` | 1033 / 85.0 KB | No | Goals, 21 design steps, defect register, decision addendum | Rich but stale; marked DRAFT |
| `handoff/PENDING_CHECKLIST.md` | 395 / 31.0 KB | No | Live queue plus long attempt narratives | No longer only live queue |
| `handoff/FINDINGS-OPEN.md` | 58 / 7.0 KB | No | Open findings | Single-home intent, some duplication with pending decisions |
| `handoff/DECISIONS.md` | 370 / 30.6 KB | No | Locked doctrine, deferred, archived | Strong; grows monotonically by design |
| `handoff/DECISIONS-PENDING.md` | 45 / 2.9 KB | No | Owed operator rulings and GO requests | Repeats detail of N2/N3 |
| `handoff/GOTCHAS.md` | 162 / 15.6 KB | No | Tagged operational traps | Excellent; mandatory whole-file read for subagents |
| `handoff/code-review/PLAN.md` + `PROMPT.md` | Review protocol | No | Chunk plan, lenses, finding format, reviewer rules | Strong, but hidden from entry point |
| `.kilo/skills/*/SKILL.md` | 3 skills | No | Installation guide, skill creator, web investigator | Generic; not referenced by project rules |

### 3.2 Agent-specific instruction files

No `CLAUDE.md`, `GEMINI.md`, `.cursorrules`, `.windsurfrules`, `.github/copilot-instructions.md`, `kilo.json`, or project-level `.kilo/command/*.md` / `.kilo/agent/*.md` exist. The only agent-specific content is `AGENTS.md`, the Kilo skills, and `.kilo/agent-manager.json`.

### 3.3 Technical documentation

| Artifact | Tracked | State |
|---|---:|---|
| `README.md` | Yes | Detailed and mostly current; strata sections stale |
| API reference | Inside README only | Current at table level; no OpenAPI/schema |
| Model YAML schema | Inside README only | Large; one stale strata key |
| Setup/install | README "Quick start" | Current |
| Operations/troubleshooting | README + `GOTCHAS.md` | Split between tracked and local |
| Architecture | `DESIGN.md`, code doc comments | Stale in places |
| ADRs | None as files; `DESIGN.md` Section 20 + `DECISIONS.md` | Decision trail exists, format is not ADR |
| Changelog / release notes | None | Only Git tags and commit bodies |
| Roadmap / backlog | `PENDING_CHECKLIST.md` + `DECISIONS-PENDING.md` | Local only |
| Technical debt | `FINDINGS-OPEN.md` + leftovers + DEFERRED | Fragmented |
| Component docs | Rust doc comments | Uneven; `lib.rs` stale |

---

## 4. Findings

Severity definitions: **Critical** = blocks reproducibility or produces a wrong supported configuration; **High** = causes wrong work, lost context, or an unenforced critical rule; **Medium** = increases cost or drift risk; **Low** = hygiene.

### 4.1 Agent instructions and behavioral rules

#### GOV-01 - Critical - The repository is not self-contained; all governing documents are untracked

- **Description.** Every rule, design record, decision ledger, queue, review protocol, and handoff protocol is excluded from version control. A clone contains 57 files, none of which describe how to work on the project.
- **Evidence.** `.gitignore:9-18` ignores `AGENTS.md`, `AUX-RULES.md`, `DESIGN.md`, `GIT_RULES.md`, `HANDOFF.md`, `handoff/`, and `.kilo/`; `git ls-files` returns 57 entries and no governance file; `handoff/GOTCHAS.md` line "[doc] `HANDOFF.md`/`DESIGN.md`/`AGENTS.md` and `handoff/*.md` are gitignored" acknowledges the condition.
- **Consequences.** New machine or contributor gets no instructions. Code comments and commit messages cite documents that do not exist in the clone. Doctrine exists on one unbacked disk. Repository backup does not preserve the development system. AI agents on a fresh checkout will improvise against generic or global rules.
- **Nuance.** This is an explicit operator decision (2026-10-25), and the project already records it as finding `N2` (`handoff/FINDINGS-OPEN.md:14`) with option C proposed in `handoff/DECISIONS-PENDING.md:9-17`. The audit confirms the finding and rates it Critical against the stated handoff/reproducibility objective, while noting the owner rated it Medium under a single-machine assumption.
- **Remediation.** Adopt a durable/tracked vs live/local split (see `TARGET_STRUCTURE.md`). At minimum, commit the rules, process, doctrine, architecture overview, and decisions ledger after scrubbing machine paths; keep live state, evidence, machine-specific configs, and raw chronicles ignored. Confirm remote visibility before any commit. Add an independent backup for `handoff/`.

#### GOV-02 - Critical - The agent entry point does not route to the actual workflow

- **Description.** `AGENTS.md` is loaded automatically, but its "Map" lists only itself, `GIT_RULES.md`, and `AUX-RULES.md`. It never names `HANDOFF.md`, `DESIGN.md`, `handoff/PENDING_CHECKLIST.md`, `GOTCHAS.md`, or the review protocol. The session-start chain exists only inside `HANDOFF.md`.
- **Evidence.** `AGENTS.md:5-9`; `HANDOFF.md:6` ("Session start: this file -> PENDING_CHECKLIST.md -> DESIGN.md"); `AUX-RULES.md:8-10` requires subagent briefs to read `GOTCHAS.md` and carry `DECISIONS.md`, but `AGENTS.md` does not mention either.
- **Consequences.** An agent that reads only the automatically loaded instructions can edit code, skip the queue, skip the doctrine, skip the traps, and skip review. The most important rules are one hop away but undiscoverable from the entry point.
- **Remediation.** Make `AGENTS.md` a routing document: project one-paragraph orientation, precedence, autonomy matrix, required-reading matrix by task type, and links to the process documents. Put "start here" at the top, not in `HANDOFF.md` alone.

#### GOV-03 - High - Instruction hierarchy and conflict resolution are undefined

- **Description.** Three files call themselves supplements to `AGENTS.md`, but there is no ordered hierarchy, no rule for contradiction, and no statement of which document wins between `HANDOFF.md`, `DECISIONS.md`, `DESIGN.md`, task briefs, code, and the operator.
- **Evidence.** `AGENTS.md:3` ("rules MUST be followed"); `GIT_RULES.md:3` ("supplement AGENTS.md"); `AUX-RULES.md:3` ("supplement AGENTS.md"); `HANDOFF.md:5` ("Locked doctrine"); no precedence section in any file.
- **Consequences.** When `GIT_RULES.md` says "concise imperative commit subject" and actual history uses narrative subjects, an agent must guess. When `DESIGN.md` says copy-in remains and `DECISIONS.md` says it is superseded, an agent must guess. Guessing produces inconsistent work and re-litigated decisions.
- **Remediation.** Define a numbered precedence (operator instruction -> `AGENTS.md` -> doctrine -> scoped process -> task brief -> reference docs -> code/tests) and a mandatory stop-and-record rule for same-level conflicts. Add a one-line conflict-resolution statement to every process document.

#### GOV-04 - High - The commit gate is defined four times, with different commands and volatile counts

- **Description.** There is no single authoritative gate. `AGENTS.md` lists `cargo build`, `cargo test`, `clippy -- -D warnings`, `fmt --check`, and doc checks. `AUX-RULES.md` and `PENDING_CHECKLIST.md` list `cargo check --all-targets`, `cargo test`, `clippy --all-targets`, `fmt --check`, and a Node DOM test. `DESIGN.md` lists only test, clippy, and fmt. Test and leg counts are embedded and already drift.
- **Evidence.** `AGENTS.md:225-235`; `AUX-RULES.md:33-36`; `handoff/PENDING_CHECKLIST.md:32-39`; `DESIGN.md:869`; DOM leg count appears as `102` in `AUX-RULES.md:36` and `PENDING_CHECKLIST.md:36`, while `handoff/PENDING_CHECKLIST.md:82` records a `DOM 101` run and `handoff/DECISIONS.md:339` records `DOM 101`.
- **Consequences.** An agent can satisfy one documented gate and fail another. Counts become false statements over time. A commit can be declared green against the weakest definition.
- **Remediation.** Create one `docs/process/VERIFICATION.md` and one executable `scripts/gate.ps1` (or `xtask`). Every other document points to it. Never embed counts or line numbers in rules; report them only in dated evidence.

#### GOV-05 - High - Autonomy and approval requirements are scattered and incomplete

- **Description.** Rules about what an agent may do without asking exist, but they are spread across `GIT_RULES.md`, `HANDOFF.md`, `AUX-RULES.md`, and `DECISIONS.md`. There is no matrix covering code edits, dependency changes, commits, pushes, tags, releases, deletions, model loads, card tests, live-harness actions, doctrine changes, or subagent use.
- **Evidence.** `GIT_RULES.md:29-31` (commit only when asked), `GIT_RULES.md:54-67` (protected operations), `HANDOFF.md:108-109` ("Never commit or push unasked"), `AUX-RULES.md:6-7` (review delegation, one subagent), `handoff/DECISIONS.md:109-116` (card window belongs to handoff), `HANDOFF.md:50-69` (live machine safety).
- **Consequences.** An agent can reasonably interpret "commit only when asked" differently from "operator approval required for release", or run a cardless build in a served session. The most dangerous actions are guarded by prose in a local file the agent may not have read.
- **Remediation.** Add an explicit autonomy table to `AGENTS.md`: allowed, requires approval, forbidden. Keep the detailed rationale in process docs. Include card/GPU, live harness, protected ports, destructive Git, dependency changes, and file deletion.

#### GOV-06 - High - Stale worktrees and agent-manager state create competing sources of truth

- **Description.** A named worktree `brassy-mosquito` exists on an old branch at `27395ac`, plus two detached worktree-pool entries at `446a3d3` and `ae344cb`. The named worktree contains a full older source tree but **no** `AGENTS.md`, `HANDOFF.md`, or `DESIGN.md`, because those are ignored.
- **Evidence.** `git worktree list` output; `.kilo/agent-manager.json` registers `wt-1793458698815-1` / branch `brassy-mosquito`; direct checks show `AGENTS.md`, `HANDOFF.md`, and `DESIGN.md` absent in that worktree; `handoff/PENDING_CHECKLIST.md:265-267` already notes "stale `git worktree`s `brassy-mosquito` + two `worktree-pool` entries (operator cleanup)".
- **Consequences.** An Agent Manager session started in that worktree sees an older codebase and none of the project rules. It can produce changes against superseded behavior, or duplicate work already merged. Disk usage and branch confusion grow.
- **Remediation.** Remove stale worktrees after confirming no uncommitted work, and make every Agent Manager worktree inherit the durable rules by tracking them. Until then, prohibit worktree sessions or add a preflight that copies/links the rules and verifies the base commit.

#### GOV-07 - Medium - `AGENTS.md` is overloaded with unused technology rules

- **Description.** `AGENTS.md` prescribes `indicatif`, `ratatui`/`crossterm`, `axum`, `polars`, `dioxus`/WASM, PyO3/`maturin`, `uv`, and `secrecy`. The crate uses `axum` and `tokio`, but not the TUI, data-frame, WASM, or Python stacks. The file is 13 KB and is loaded for every task.
- **Evidence.** `AGENTS.md:25-64`; `Cargo.toml:8-54` shows no `indicatif`, `ratatui`, `crossterm`, `polars`, `dioxus`, `pyo3`, `maturin`, or `secrecy`; no `web/` or Python package exists.
- **Consequences.** Agents spend context on irrelevant rules, may introduce an unused dependency to satisfy a "preferred tool" rule, and have less attention for the rules that matter. It violates progressive disclosure.
- **Remediation.** Move generic standards to `docs/process/ENGINEERING.md`, keep only project-applicable rules in `AGENTS.md`, and gate stack-specific sections behind "only if this stack is present". Delete unused stack rules from the mandatory path.

#### GOV-08 - Medium - Internal tensions inside `AGENTS.md`

- **Description.** The file contains several rules that pull in different directions: add extra Rust comments for a Python expert vs avoid redundant comments; never `.unwrap()` in library code vs never in production; "fully optimized" with a monetary penalty vs unverifiable quality criteria; mandatory unit tests for every function vs the project's integration-heavy test style.
- **Evidence.** `AGENTS.md:23`, `AGENTS.md:74-76`, `AGENTS.md:117`, `AGENTS.md:124`, `AGENTS.md:149-154`.
- **Consequences.** Agents resolve the tension arbitrarily. Reviewers cannot cite a single standard. The "$100 fine" line is not enforceable and weakens the document's authority.
- **Remediation.** Resolve each tension explicitly: comments explain non-obvious Rust semantics only; `.unwrap()` forbidden on production paths and allowed only in tests with a reason; "optimized" replaced by measurable criteria (complexity, allocation, benchmark evidence); test requirement scoped to behavior and risk, not every function.

#### GOV-09 - Medium - No project-level Kilo configuration, commands, or specialized agents

- **Description.** The project relies on one large `AGENTS.md` and three generic skills. There is no `kilo.json`, no `.kilo/command/*.md`, and no `.kilo/agent/*.md` despite Kilo supporting them.
- **Evidence.** Absence checks for `kilo.json`, `.kilo/command`, `.kilo/agent` all return false; `.kilo` contains only skills, worktrees, and agent-manager state.
- **Consequences.** Recurring workflows (gate, handoff close, review chunk, release) are retyped or remembered. Permissions and agent defaults are implicit and machine-specific. A new machine cannot reproduce the agent setup.
- **Remediation.** Add a minimal `kilo.json` for permissions/defaults, and 3-4 commands: `gate`, `handoff-close`, `review-chunk`, `release-check`. Do not create agent personas without a demonstrated need.

#### GOV-10 - Medium - Parallel-agent policy contradicts the available tooling

- **Description.** `AUX-RULES.md` forbids more than one active subagent at a time, yet Agent Manager creates worktrees and the project has stale parallel worktree state. There is no merge, conflict, or ownership protocol for parallel sessions.
- **Evidence.** `AUX-RULES.md:7` ("only 1 active per session at the given time"); `.kilo/agent-manager.json`; `git worktree list`; `handoff/PENDING_CHECKLIST.md:265-267`.
- **Consequences.** The rule is either ignored (parallel sessions exist) or unnecessarily serializes work. If two sessions do run, they share one `HANDOFF.md` and one `handoff/` with no lock, so last-writer-wins.
- **Remediation.** Decide the model: (A) strictly serial, delete worktrees and state it; or (B) parallel with per-worktree task files, explicit file ownership, and a merge protocol. Do not leave both models half-enabled.

### 4.2 Documentation and technical accuracy

#### DOC-01 - Critical - `README.md` documents the superseded strata configuration as required

- **Description.** The README says strata requires `strata_config`, that the harness launches `--config <strata_config>`, that the stack requires a per-model profile JSON, and that strata takes its bind address from the profile `host`. The shipped code refuses `strata_config` at scan and launch, renders a profile under `.simplehar/strata-profiles`, and forces `--host 127.0.0.1`.
- **Evidence.**
 - `README.md:31` - "per-model profile JSON" in the required stack.
 - `README.md:80-82` - strata bind "from its own external config (the strata profile `host`)".
 - `README.md:176-178` - "`strata_config` | **Required for strata.**"
 - `README.md:212-219` - launch via `--config <strata_config>`.
 - `src/model/scan.rs:1781-1784` - a row declaring `strata_config` is the superseded mode and is refused.
 - `src/engine/strata.rs:786-796` - the adapter returns the migration sentence instead of launching.
 - `handoff/DECISIONS.md:321-338` - row 19: template mode ships; external `strata_config` superseded; bind forced to loopback; rendered profile carries no `host`/`port`/`key`.
- **Consequences.** A user following the README cannot start a strata model: the scan refuses the documented required key. A user may also believe a profile's `host` controls exposure when the harness forces loopback. This is the most damaging documentation defect in the repository.
- **Remediation.** Rewrite the strata sections for template mode: `strata_exe`, `strata_args`, `strata_python`, `strata_cwd`, `strata_*` profile knobs, rendered-profile root, import bridge, forced bind/port, and the migration message. Remove "required `strata_config`". Add a test or doc check that fails if `strata_config` appears as a required key in README.

#### DOC-02 - High - `DESIGN.md` status and addendum contradict the shipped decision

- **Description.** `DESIGN.md` is marked "DRAFT for operator review" after v1.0.1 shipped. Its additive addendum still describes the strata copy-in migration as the remaining chunk-10 piece, while `DECISIONS.md` row 19 says the copy-in slice is superseded by the import bridge and template mode.
- **Evidence.** `DESIGN.md:3` (DRAFT); `DESIGN.md:1015-1033` (copy-in migration, "remaining chunk-10 piece"); `handoff/DECISIONS.md:329-338`; `handoff/PENDING_CHECKLIST.md:244-248` ("The held copy-in migration slice is SUPERSEDED by ruling 19's import bridge").
- **Consequences.** `DESIGN.md` is called the "spec of record" (`HANDOFF.md:93`) but contains superseded architecture. Agents reading it will implement or argue for a retired path. Its 85 KB size makes reading it expensive and unlikely, so the stale part may be discovered only when it causes a wrong decision.
- **Remediation.** Reclassify `DESIGN.md` as a historical design record with a banner and a link to current architecture/decisions. Move the still-binding invariants to a short `docs/architecture/OVERVIEW.md` and `docs/architecture/DOCTRINE.md`. Never let a historical document be the default session reading.

#### DOC-03 - High - Code doc comments contain stale claims

- **Description.** `src/lib.rs` says tray, process supervision internals, the llama adapter, and the control page "arrive with their own turns"; all are present. `src/config/mod.rs` says "there is no auth", while `control_token` and `SIMPLEHAR_TOKEN` exist. `DESIGN.md` repeats the no-auth claim.
- **Evidence.** `src/lib.rs:4-6`; `src/config/mod.rs:390`; `src/config/mod.rs:1034`; `DESIGN.md:150`; contradiction with `README.md:126-127` and `README.md:475-486`; the project already records this as `N5` in `handoff/FINDINGS-OPEN.md:17`.
- **Consequences.** Agents and users reading code trust false statements. A security-relevant "no auth" claim is especially harmful because it can discourage token use or mislead a LAN deployment review.
- **Remediation.** Fix the comments in the same change that fixes README/DESIGN. Add a doc-comment review step to the release checklist. Prefer comments that state invariants, not milestone status.

#### DOC-04 - High - No documentation navigation, ownership, or freshness metadata

- **Description.** There is no docs index, no ownership field, no status field, no "last reviewed" date, and no statement of what each document is the source of truth for. Information type to location must be inferred.
- **Evidence.** Root tree; `AGENTS.md:5-9` map is incomplete; `HANDOFF.md:7` is the only routing statement and only covers session start; no `docs/` directory before this audit; no front matter in any document.
- **Consequences.** Contributors and agents duplicate information or put it in the wrong ledger. Stale documents are indistinguishable from current ones. The single-home rule cannot be enforced because there is no declared home map.
- **Remediation.** Create `docs/README.md` with a routing table and add front matter (`owner`, `status`, `audience`, `source_of_truth_for`, `last_reviewed`). Add a documentation check that validates required fields and links.

#### DOC-05 - Medium - Release and "current build" claims are ambiguous; no changelog

- **Description.** `README.md` says "Current build: v1.0.1" while HEAD is 11 commits after the `v1.0.1` tag and one commit is unpushed. `Cargo.toml` remains `1.0.1`. There is no changelog or release-note file; users must read commit bodies.
- **Evidence.** `README.md:10-11`; `git log --oneline c91e179..HEAD` returns 11 commits; `git rev-list --count origin/main..HEAD` returns 1; `Cargo.toml:3` is `1.0.1`; no `CHANGELOG.md`.
- **Consequences.** "Current build" can mean the tag, the deployed package, or HEAD; all three differ. Users cannot see what changed between releases. A future release can ship without a version bump.
- **Remediation.** Separate "Latest release" from "Repository HEAD". Add `CHANGELOG.md` with dated sections. Define a release task that bumps `Cargo.toml`, updates README, writes changelog, tags, and records artifact hash.

#### DOC-06 - Medium - The authoritative ninfer script is duplicated in the README

- **Description.** `README.md` embeds a full `autostart.sh` template and says it must be byte-synced manually with `ninfer-autostart-template.sh`, which is itself a readable copy of the compiled `CANONICAL_NINFER_SCRIPT`.
- **Evidence.** `README.md:257-266` (three copies described), `README.md:272-337` (embedded script), `ninfer-autostart-template.sh` (separate file), and `handoff/PENDING_CHECKLIST.md:352-363` (README sync remains a manual duty and has drifted once).
- **Consequences.** Two public copies drift; users copy an outdated script; a security-relevant flag such as `--restart no` or loopback publish can diverge.
- **Remediation.** Keep one public copy (the file) and link to it. If an inline example is necessary, generate it from the file in a docs build and fail on mismatch. Never describe manual byte-sync as the control.

#### DOC-07 - Medium - Historical and archival copies overlap without a retention policy

- **Description.** Old handoffs exist in `archive/`, a full pre-slim checklist exists in `handoff/archive/`, `handoff.7z` claims to hold pre-slim handoffs, and `base-archive/` holds an entire prior product. There is no rule for what is retained, where, and for how long. The queue also contains a broken archive path.
- **Evidence.** `archive/HANDOFF-2026-10-26.md`, `archive/HANDOFF-2026-10-26T1913Z.md`, `handoff/archive/PENDING_CHECKLIST-2026-11-02-full.md`, `handoff/handoff.7z`, `base-archive/`; `handoff/PENDING_CHECKLIST.md:14` points to `archive/PENDING_CHECKLIST-2026-11-02-full.md`, but the file is at `handoff/archive/`.
- **Consequences.** Search results return superseded instructions; an agent may read an old rule as current; disk and backup cost grows; broken pointers erode trust.
- **Remediation.** Define one archive root, one naming convention, and a retention rule (for example: close-of-cycle state moves to `handoff/archive/<date>/`; evidence older than N months may be compressed; `base-archive/` is explicitly "reference only, never read for current behavior"). Fix the pointer.

#### DOC-08 - Low - `README.md` mixes four document types

- **Description.** The README is simultaneously a product overview, installation guide, configuration reference, model-schema reference, API reference, operations guide, security note, and contributor entry point. It is 494 lines.
- **Evidence.** `README.md` headings 1-25.
- **Consequences.** Changes to one concern require editing a large file; readers cannot find the right level; the file will keep drifting as the product grows.
- **Remediation.** Keep the README as overview + quick start + links. Move the full API and model schema to `docs/reference/` when they exceed a threshold (for example, 150 lines each), not preemptively.

### 4.3 Task management, Git, and process

#### PRO-01 - High - No unified task schema or lifecycle artifacts

- **Description.** The queue is prose. A row may contain a class, a status box, dependencies in prose, commit hashes, test counts, and historical narrative. It has no required fields for goal, scope, acceptance criteria, affected files, owner, or next command.
- **Evidence.** `handoff/PENDING_CHECKLIST.md:1-30` defines homes and classes but no schema; rows at `:41-56`, `:192-238`, `:293-348` vary widely in structure; IDs are heterogeneous (`R9`, `B3`, `N5`, `12-P`, `19d`).
- **Consequences.** A task cannot be handed to a new agent without re-reading the project. "Done" depends on interpretation. Dependencies are hidden in sentences. The queue grows because there is no compact row shape.
- **Remediation.** Adopt the task template in `TARGET_STRUCTURE.md` / `PROCESS_IMPROVEMENTS.md`: ID, title, class, status, goal, scope, non-goals, acceptance criteria, dependencies, evidence, decisions, next step, owner, last updated. Migrate only live rows; archive the rest.

#### PRO-02 - High - Verification and acceptance are global, not per task

- **Description.** The project has a strong global gate and a strong review-closure rule, but no per-task acceptance criteria. A task is closed by prose, a commit, or a review disposition. There is no standard "this task is accepted because X" record.
- **Evidence.** `AUX-RULES.md:15-29` defines review closure; `handoff/PENDING_CHECKLIST.md:32-39` defines a global gate; no acceptance field in any queue row; `handoff/FINDINGS-OPEN.md` is the only ledger with a strict closure rule.
- **Consequences.** Work can be marked done because code exists, not because the stated behavior is verified. Card-dependent items can be closed on cardless evidence. Review findings are held to a higher standard than product tasks.
- **Remediation.** Add acceptance criteria and an evidence link to every task. Define done as: acceptance criteria met, gate passed, evidence recorded, docs updated, and review findings closed or accepted. Use the same rigor as `FINDINGS-OPEN.md`.

#### PRO-03 - High - Handoff covers session end but not mid-session interruption or dirty trees

- **Description.** The closing protocol routes facts to homes at session end. There is no checkpoint before context compaction, no protocol for an interrupted task, no record of attempted approaches that failed, and no explicit dirty-tree handoff format.
- **Evidence.** `HANDOFF.md:111-118` (closing protocol); `GIT_RULES.md:10-15` (preserve unknown changes, ask); `HANDOFF.md:17-31` assumes a clean snapshot; no WIP/task file exists.
- **Consequences.** An interrupted session can leave uncommitted edits with no narrative. The next agent sees `git status` but not why the work exists, what was tried, what failed, or what command to run next. Failed approaches get repeated.
- **Remediation.** Add a checkpoint trigger (before compaction, before handing off, before a long run) and a task file with status/done/remaining/decisions/blockers/files/next command. Record dirty state explicitly in `HANDOFF.md` if the task is not committed. Never rely on `git status` as the handoff.

#### PRO-04 - High - The documented Git workflow does not describe the actual repository

- **Description.** The repository is trunk-only on `main`, with local-only docs, direct commits, tags for releases, and narrative commit subjects. `GIT_RULES.md` gives generic advice (small imperative commits, PR-oriented review) that does not match this model. There is no branch naming, merge, conflict, release-branch, or hotfix policy.
- **Evidence.** `git branch -a` shows only `main` plus a stale `brassy-mosquito`; `GIT_RULES.md:28-41` prescribes concise imperative subjects and small logical commits; `git log` shows subjects of 100-257 characters and multi-finding commits (for example `1596573`, `4cdefca`); no PR workflow or CI exists.
- **Consequences.** An agent following `GIT_RULES.md` produces commits that violate house style. An agent following history produces commits that violate the written rule. There is no defined way to review or merge parallel work. Release steps live in prose and are easy to miss.
- **Remediation.** Document the actual model: trunk-based, direct commits by the operator/authorized agent, review by subagent, narrative subject allowed up to a stated limit, body carries finding IDs and rationale, release by tag after gate and changelog. If PRs are desired, define them; otherwise remove PR-flavored language.

#### PRO-05 - Medium - The single-home bookkeeping rule is violated by the ledgers themselves

- **Description.** `AUX-RULES.md` says every fact has exactly one file and a duplicate is a bug. In practice `N2`, `N3`, and `N5` appear in `FINDINGS-OPEN.md` and again in `DECISIONS-PENDING.md` or `PENDING_CHECKLIST.md`. Status, gate counts, and release state are repeated across `HANDOFF.md`, `PENDING_CHECKLIST.md`, and the decisions ledger. The queue also retains long historical narratives that its own header says belong in the archive.
- **Evidence.** `AUX-RULES.md:11-13`; `handoff/FINDINGS-OPEN.md:14-19`; `handoff/DECISIONS-PENDING.md:9-24`; `handoff/PENDING_CHECKLIST.md:252-256` (N2/N5/N7 restated), `:58-190` (attempt narratives), `:380-395` (done narrative); `HANDOFF.md:19-48` (snapshot detail).
- **Consequences.** A reader cannot tell which copy is authoritative. Updating one copy leaves the others stale. The queue grows without bound and becomes expensive to read, which defeats the archive.
- **Remediation.** Enforce pointers: one row owns the fact; every other mention is a link. Move attempt narratives to `handoff/evidence/` and keep one-line outcomes. Add a check that flags duplicate IDs across ledgers. Give the queue a line budget or row-count budget.

#### PRO-06 - Medium - Technical debt is split across three ledgers with no unified priority or owner

- **Description.** Open findings live in `FINDINGS-OPEN.md`; debt-like work sits in `PENDING_CHECKLIST.md` "leftovers"; deferred decisions live in `DECISIONS.md` DEFERRED. There is no single debt register, no severity/priority scale, and no owner or review date.
- **Evidence.** `handoff/FINDINGS-OPEN.md:10-19`; `handoff/PENDING_CHECKLIST.md:250-278`; `handoff/DECISIONS.md:118-125`.
- **Consequences.** Debt is invisible in aggregate, cannot be prioritized against features, and can be closed by being forgotten. The same item can exist in two forms.
- **Remediation.** Create one debt index (which may link to detailed homes) with ID, severity, impact, owner, target release, and status. Or make `FINDINGS-OPEN.md` the sole debt register and move non-finding debt into it with a `debt` class.

#### PRO-07 - Medium - No process automation or CI

- **Description.** There is no CI workflow, no pre-commit hook, no documentation validation, no link checking, and no template validation. The only automation is the Node DOM test and the local gate commands.
- **Evidence.** No `.github/`, `.gitlab-ci.yml`, or hooks; gate is described as manually run in `AUX-RULES.md` and `PENDING_CHECKLIST.md`.
- **Consequences.** Gate compliance depends on memory. A commit can be pushed without tests. Documentation links and paths rot silently. The same failures are rediscovered by agents.
- **Remediation.** Add a Windows CI workflow for cardless checks, a single `scripts/gate.ps1`, and a dependency-free Node documentation checker. Add release-time checks for version/changelog/tag consistency.

#### PRO-08 - Medium - No definition of ready or prioritization model

- **Description.** Work items have execution classes (`[S]`, `[R]`, `[C]`, `[O]`) but no priority, no cost/benefit, and no definition of ready. The queue mixes features, verification, cleanup, research, and operator chores.
- **Evidence.** `handoff/PENDING_CHECKLIST.md:17-18` defines classes; sections mix `R9`, `B1-B10`, README compliance, and a new engine; no priority field.
- **Consequences.** The next item is chosen by narrative order or operator memory. Items that need a card block the queue; cheap high-value items wait behind expensive ones. An agent cannot choose safely.
- **Remediation.** Add priority (`P0-P3`) and readiness (`ready`, `blocked`, `needs-design`, `needs-operator`). Separate queues by lane: now, next, later, blocked. Keep classes as execution constraints, not priority.

#### PRO-09 - Medium - The release process is a prose checklist without version/changelog coupling

- **Description.** Release close-out is a queue row and a paragraph in `AUX-RULES.md`. It says tag, push, update README, record hash. It does not require a version bump, changelog entry, or consistency check between `Cargo.toml`, tag, README, and deployed artifact.
- **Evidence.** `AUX-RULES.md:37-39`; `handoff/PENDING_CHECKLIST.md:280-291`; `Cargo.toml:3` still `1.0.1` at HEAD after 11 post-tag commits.
- **Consequences.** Releases can be tagged with mismatched versions or undocumented changes. "Current build" claims drift. Rollback artifact identity is recorded manually.
- **Remediation.** Define a release script/checklist that fails unless version, tag, changelog, README, and artifact hash agree. Record the deployed hash in the changelog or release note.

#### PRO-10 - Low - The review process is strong but not discoverable

- **Description.** The most rigorous process in the repository (chunk plan, reviewer prompt, falsifier requirement, closure rules) lives in `AUX-RULES.md` and `handoff/code-review/`. `AGENTS.md` does not mention it.
- **Evidence.** `AUX-RULES.md:6-29`; `handoff/code-review/PLAN.md`; `handoff/code-review/PROMPT.md`; `AGENTS.md` has no review section.
- **Consequences.** Agents skip review or invent a weaker one. The falsifier culture is not transmitted to new contributors.
- **Remediation.** Add a short review section to `AGENTS.md` and a full `docs/process/REVIEW.md`, linked from the task template.

#### PRO-11 - Low - Hardcoded volatile facts in durable rules

- **Description.** Rules embed test counts, DOM leg counts, line ranges, and file sizes. These change every cycle. Even the rule that says a stale gate line is stale is itself stale when the count changes.
- **Evidence.** `AUX-RULES.md:33-36`; `handoff/PENDING_CHECKLIST.md:32-39`; `handoff/FINDINGS-OPEN.md:12-19` (`:3584`, `:187-197`, `:6`, `:387`, `:150`).
- **Consequences.** Readers cannot distinguish a rule from a dated observation. Updates create noise and merge conflicts.
- **Remediation.** Durable docs state commands and invariants, not counts. Evidence files carry dated numbers. Code references use symbols or anchors where possible; `file:line` belongs in findings, not rules.

#### PRO-12 - Low - No documented secret/PII scrub policy for local artifacts

- **Description.** `handoff/`, `GOTCHAS.md`, and `ModelConf/` contain machine paths, ports, API-key file names, and incident logs. The project discusses committing some of them but has no scrub checklist.
- **Evidence.** `.gitignore:7-16`; `handoff/GOTCHAS.md` entries about keys, tokens, container names; `handoff/DECISIONS-PENDING.md:9-17` notes live paths in history as a risk of option B.
- **Consequences.** A future commit or share can leak local paths, key file names, or incident data. Reviewers cannot verify a scrub without a policy.
- **Remediation.** Add `docs/process/PUBLICATION.md` with a scrub checklist: absolute paths, hostnames, ports, token names, container names, logs, model paths, screenshots. Require it for any commit that moves local artifacts into Git.

#### PRO-13 - Low - Tooling and encoding hazards are documented as traps but not standardized

- **Description.** `GOTCHAS.md` records PowerShell 5.1 hazards (no `&&`, BOM behavior, UTC/encoding, `Select-String` parsing) and mojibake in archives. There is no standard for document encoding or script portability.
- **Evidence.** `handoff/GOTCHAS.md` `[ps]` entries; `handoff/PENDING_CHECKLIST.md:184-185` (em-dash/section mojibake); old handoffs show mojibake.
- **Consequences.** Generated docs and scripts can be corrupted or silently misinterpreted. Agents waste time on environment quirks.
- **Remediation.** State UTF-8 without BOM for Markdown, ASCII for scripts where practical, and PowerShell 5.1 compatibility rules in `docs/process/ENGINEERING.md`. Keep the detailed traps local.

#### PRO-14 - Low - No documentation review lifecycle or freshness triggers

- **Description.** No document states when it must be reviewed, who reviews it, or what event invalidates it. Updates happen ad hoc, usually after a defect.
- **Evidence.** No front matter; `README.md` strata sections stale since ruling 19 (2026-11-02); `DESIGN.md` DRAFT since 2026-10-24; `src/lib.rs` milestone text stale.
- **Consequences.** Drift is discovered by users or auditors, not by the process. The same class of defect recurs.
- **Remediation.** Add `last_reviewed` and `review_trigger` front matter; require a docs diff in every behavior-changing commit; review all docs at each release; add a scheduled quarterly review.

---

## 5. Strengths

1. **Explicit binding doctrine.** `handoff/DECISIONS.md` LOCKED is unusually clear for a personal project: witness is budget truth, config file is king, external engines obey harness-owned configuration, admission never overcommits.
2. **Settled vs pending vs archived separation.** Most projects conflate these. This one does not, at least in intent.
3. **Falsifier culture.** `AUX-RULES.md:25-26` requires a test that fails on pre-fix code; `handoff/code-review/PROMPT.md` teaches four real anti-patterns. This is stronger than typical review guidance.
4. **Safety rules for a live machine.** The card, live harness, protected ports, and foreign containers are explicitly fenced (`HANDOFF.md:50-69`, `DECISIONS.md:109-116`).
5. **Dated evidence.** Review findings, wave runs, and gate logs are retained with timestamps and hashes.
6. **Budgeted live handoff.** `HANDOFF.md` is 118 lines against its own 120-line budget, with a displacement rule. This discipline should be copied to the queue.
7. **Honest language.** The documents distinguish confirmed from assumed, and the review prompt explicitly forbids invented findings.
8. **Self-identified weaknesses.** `N2` (repo not self-contained) and `N5` (stale docs) already exist in the project's own ledger. The audit confirms and expands them rather than inventing a parallel critique.
9. **Single-writer scheduler doctrine and ownership-scoped reaping.** The architecture invariants are crisp and testable.
10. **No dirty tree at audit.** The project practices the cleanliness it preaches.

---

## 6. Documentation-to-implementation verification

| # | Claim | Document location | Actual implementation | Evidence | Verdict |
|---|---|---|---|---|---|
| 1 | `strata_config` is required for strata | `README.md:176-178` | Rows declaring it are refused at scan/launch; template mode renders and launches its own profile | `src/model/scan.rs:1781-1784`, `src/engine/strata.rs:786-796`, `DECISIONS.md:321-338` | **False** |
| 2 | Strata launches `--config <strata_config>` | `README.md:212-219` | Launches harness-rendered `.simplehar/strata-profiles/<slug>-<hash>.json` | `DECISIONS.md:323-328` | **Stale** |
| 3 | Strata bind comes from profile `host` | `README.md:80-82` | Bind forced to `--host 127.0.0.1` by argv | `DECISIONS.md:331-334` | **False** |
| 4 | The stack requires a per-model profile JSON | `README.md:31` | The row YAML owns profile fields; JSON is generated | `DECISIONS.md:323-334` | **Stale** |
| 5 | There is no authentication | `src/config/mod.rs:390`, `src/config/mod.rs:1034`, `DESIGN.md:150` | `control_token` / `SIMPLEHAR_TOKEN` gate every non-asset route | `README.md:126-127`, `README.md:475-486`, `src/config/mod.rs` token fields | **Stale** |
| 6 | Tray, process internals, llama adapter, control page "arrive with their own turns" | `src/lib.rs:4-6` | All modules are present and shipped | `src/tray.rs`, `src/process.rs`, `src/engine/llama.rs`, `src/webui/` | **Stale** |
| 7 | Strata copy-in migration remains the chunk-10 piece | `DESIGN.md:1015-1033` | Copy-in superseded by import bridge and template mode | `DECISIONS.md:329-338`, `PENDING_CHECKLIST.md:244-248` | **False** |
| 8 | Design is a draft for operator review | `DESIGN.md:3` | v1.0.1 released; 11 commits after tag | tags `v1.0.0`, `v1.0.1`; `git log` | **Stale** |
| 9 | Current build is v1.0.1 | `README.md:10-11` | HEAD is 11 commits after the tag; 1 unpushed; LIVE reportedly runs `a454dbb` | `git log c91e179..HEAD`, `HANDOFF.md:19-31` | **Ambiguous** |
| 10 | Commit subject should be concise imperative | `GIT_RULES.md:34-35` | 30 recent subjects are narrative; 100-257 chars in several | `git log --pretty=%s` | **Contradicted** |
| 11 | Gate is exactly four/five commands | `AUX-RULES.md:33-36` | Four documents list three different gate sets | `AGENTS.md:225-235`, `DESIGN.md:869`, `PENDING_CHECKLIST.md:32-39` | **Inconsistent** |
| 12 | DOM test has 102 legs | `AUX-RULES.md:36` | Logs record 101 in several runs | `PENDING_CHECKLIST.md:82`, `DECISIONS.md:339` | **Drift** |
| 13 | There is one authoritative ninfer template | `README.md:257-266` | Three copies exist; README sync is manual and has drifted | `ninfer-autostart-template.sh`, `README.md:272-337`, `PENDING_CHECKLIST.md:352-363` | **Duplicated** |
| 14 | One active subagent at a time | `AUX-RULES.md:7` | Worktrees and parallel session state exist | `git worktree list`, `.kilo/agent-manager.json` | **Contradicted in practice** |
| 15 | Archive copy is at `archive/PENDING...` | `PENDING_CHECKLIST.md:14` | File is at `handoff/archive/PENDING...` | filesystem | **Broken pointer** |

---

## 7. Task management analysis

### 7.1 Lifecycle as currently practiced

```text
Idea / operator observation
 -> brainstorm (one item per turn; options with +/-)
 -> decision (LOCKED in DECISIONS.md, or PENDING row)
 -> work item added to PENDING_CHECKLIST.md with class
 -> implementation (often direct on main)
 -> review by subagent (chunk plan + falsifier)
 -> findings recorded (FINDINGS-OPEN.md), fixed in commits
 -> gate run
 -> commit (narrative subject + finding IDs)
 -> push/tag if operator approves
 -> narrative moved to archive/done
```

### 7.2 What works

- Decisions are separated from tasks and from findings.
- Review closure has a real definition.
- The class system (`S`, `R`, `C`, `O`) correctly encodes execution prerequisites, especially the card constraint.
- The gate is concrete.
- Evidence is dated and retained.

### 7.3 What is missing

| Lifecycle stage | Current artifact | Gap |
|---|---|---|
| Idea | Operator message / brainstorm | No idea backlog; ideas are lost if not written immediately |
| Analysis | Research docs under `handoff/research/` | No standard template; only 4 reports exist, no index |
| Planning | Queue row prose | No scope, non-goals, dependencies, effort, acceptance |
| Implementation | Commit + code | No task file; no in-progress status beyond `[ ]`/`[x]` |
| Verification | Global gate + review findings | No per-task acceptance evidence; cardless/card evidence not structured per task |
| Acceptance | Operator implicit | No named acceptance record; `[x]` is the only signal |
| Documentation | Ad hoc; often missed | No same-commit docs rule; no changelog |
| Closure | Archive narrative | No archive index, no retention rule; queue retains narratives |

### 7.4 Handoff sufficiency test

A new agent is given the repository at `a454dbb` with no prior session. Can it continue the top queue item without rediscovery?

- **On this machine, probably yes** if it reads `HANDOFF.md` -> `PENDING_CHECKLIST.md` -> `GOTCHAS.md` -> `DECISIONS.md` -> relevant code. The queue row for `R9 L5/L6` names the design document, the evidence path, the gate, and the constraint.
- **On a fresh clone, no.** The queue and all rules are absent. The README describes a different strata model. The clone has no `handoff/`, no `ModelConf/`, and no decision ledger.
- **After an interrupted session, partially.** Git status shows files, but no record of intent, failed attempts, or next command. If the agent did not reach session end, the closing protocol did not run.

This is the core handoff finding: the protocol is good but **not durable, not checkpointed, and not portable**.

---

## 8. Context preservation and handoff analysis

### 8.1 What is recorded today

| Required handoff fact | Where it is recorded | Quality |
|---|---|---|
| Current task status | `PENDING_CHECKLIST.md` checkboxes + prose | Medium: status is mixed with narrative |
| Completed work | Commit bodies + `Done` narrative + archive | Medium: accurate, hard to query |
| Remaining work | Queue rows | Medium: prose, no required fields |
| Technical decisions and rationale | `DECISIONS.md` + commit bodies | High |
| Known issues | `FINDINGS-OPEN.md` + queue leftovers | Medium: split |
| Failed or incomplete tests | Queue notes + evidence logs | Medium: only if the agent writes it |
| Blockers | Prose in queue; classes encode prerequisites | Low: no explicit blocked-by field |
| Modified files | Git status / commit diff | Medium: not captured at interruption |
| Next steps | Occasional prose; no standard field | Low |
| Constraints and context | `HANDOFF.md` Section 3, `GOTCHAS.md`, `DECISIONS.md` | High, but local-only |

### 8.2 Failure modes

1. **Context window exhaustion mid-task.** The closing protocol is not triggered, so the next session sees code but not intent.
2. **Parallel session overwrite.** Two sessions share `HANDOFF.md` and `PENDING_CHECKLIST.md`; last writer wins.
3. **Wrong worktree.** A session starts in `brassy-mosquito`, which has no rules and older code.
4. **Fresh machine.** The entire system is absent.
5. **Stale doctrine.** The agent reads `DESIGN.md` or `README.md` and implements a retired path.
6. **Silent skip.** The agent reads only `AGENTS.md`, which does not route to the queue or doctrine.

### 8.3 Required handoff protocol properties

A usable protocol must be:

- **Triggered**, not optional: at session start, before compaction, before a long run, at session end, and before switching agents.
- **Task-scoped**: one file per active task, not one global narrative.
- **Field-complete**: status, done, remaining, decisions, blockers, files, evidence, next command, constraints.
- **Dirty-tree aware**: explicitly records uncommitted work and why it exists.
- **Pointer-based**: links to decisions, findings, and evidence rather than copying them.
- **Portable where needed**: the durable parts are tracked; the machine-specific parts are local and backed up.

The concrete template is in `TARGET_STRUCTURE.md` Section 6 and `PROCESS_IMPROVEMENTS.md` Section 7.

---

## 9. Technical documentation audit

### 9.1 Completeness

- User setup, configuration, model schema, API, security, and troubleshooting are covered in README.
- Architecture rationale is extensive in `DESIGN.md`.
- Component-level documentation is uneven: `scheduler/mod.rs` is 8,613 lines with one `impl ActorState` spanning roughly 3,550 lines; `api/mod.rs` is 4,469 lines; `process.rs` is 3,798 lines (recorded in `FINDINGS-OPEN.md:15`). Doc comments cannot compensate for low cohesion.
- No OpenAPI/schema artifact, no module map, no data-flow diagram, no operational runbook beyond README.
- No changelog, roadmap, or release notes for users.

### 9.2 Accuracy and freshness

Confirmed stale or contradictory: README strata (4 claims), `DESIGN.md` status and addendum, `src/lib.rs`, `src/config/mod.rs`, and the "no auth" claim. Full table in Section 6.

### 9.3 Consistency and duplication

- Gate definitions differ.
- Commit style differs.
- Ninfer template duplicated.
- Decision rationale duplicated between `DESIGN.md` and `DECISIONS.md`.
- Status duplicated across three ledgers.
- Old handoffs and archives overlap.

### 9.4 Navigation and discoverability

- No docs index.
- No front matter.
- No ownership.
- No "source of truth for" declarations.
- `AGENTS.md` map is incomplete.
- A new contributor would find README, then nothing.

### 9.5 Suitability for AI consumption

Positive: explicit MUST/NEVER rules, tagged gotchas, decision ledger, task classes, and evidence conventions are agent-friendly.

Negative: total mandatory context is large. A session that follows the documented start chain reads `HANDOFF.md` (8 KB) -> `PENDING_CHECKLIST.md` (31 KB) -> `DESIGN.md` (85 KB), plus `AGENTS.md` (13 KB), `AUX-RULES.md` (2.4 KB), `GIT_RULES.md` (3.3 KB), and for subagents `GOTCHAS.md` (15.6 KB) + `DECISIONS.md` LOCKED (~20 KB). That is roughly 180 KB, or about 45,000 tokens, before any code. This is not progressive disclosure; it is a context tax on every non-trivial task.

### 9.6 Relationship to actual code

- The code is the behavioral truth, but documents rarely say so.
- `DESIGN.md` is treated as a spec even where code has moved on.
- Stale `file:line` references in findings are explicitly acknowledged as drift-prone (`FINDINGS-OPEN.md:5-6`).
- The README's model schema is detailed enough to be useful, but one key is wrong for strata.

---

## 10. Git and repository analysis

### 10.1 Observed model

- Trunk-based: `main` only, direct commits.
- One stale feature branch worktree: `brassy-mosquito` at `27395ac`.
- Two detached worktree-pool entries.
- Release by annotated tag on a commit; `v1.0.1` at `c91e179`.
- 11 commits after the tag, 1 unpushed.
- Commit subjects are narrative, often long, with bodies carrying finding IDs and falsifier evidence.
- No merge commits in the recent history (linear).
- No CI, no PRs, no branch protection observed.

### 10.2 Documented vs actual

| Aspect | Documented | Actual |
|---|---|---|
| Branching | Not specified | Trunk-only |
| Commit subject | Concise imperative | Narrative, 35-257 chars |
| Commit size | Small, one logical change | Often multiple findings in one review-scoped commit |
| Review | Subagent, one at a time | Same, but no branch/PR artifact |
| Merge | Not specified | No merges |
| Release | Tag, push, README, hash | Done, but version/changelog not coupled |
| Protected operations | Listed in `GIT_RULES.md` | Consistent with `HANDOFF.md` |
| Validation | Gate | Manual; four definitions |
| Changelog | None | None |

### 10.3 Dangerous or missing practices

- Direct commits to the only branch with no CI and no second human.
- Local-only rules mean a clone cannot reproduce the gate.
- Stale worktrees can silently diverge.
- No secret scrub before potentially committing local artifacts.
- No signed tags/commits (not necessarily required, but undocumented).
- No backup policy for `handoff/`.
- Release version not bumped after post-release commits.

### 10.4 Unnecessary restrictions

- "One subagent at a time" is stricter than the tooling and not justified by the actual failure mode (which is shared mutable ledgers, not parallel compute). Better to serialize writes to `handoff/` and allow read-only parallel research.
- Generic "never use `unwrap()` in library code" is broader than the project's tested convention; it forces awkward code in tests and invariant paths.
- Blocking commits unless explicitly asked is correct for an agent, but the rule should distinguish "agent may create a local checkpoint commit on a task branch" if parallel work is ever enabled.

---

## 11. Risk register

| Risk | Likelihood | Impact | Evidence | Mitigation |
|---|---|---|---|---|
| New machine/clone cannot reproduce development | High | Critical | `GOV-01` | Commit durable doctrine/process; backup local state |
| Agent follows README and cannot start strata | High | High | `DOC-01` | Fix README strata sections |
| Agent works in stale worktree with old code | Medium | High | `GOV-06` | Remove stale worktrees; track rules |
| Agent reads only `AGENTS.md` and skips queue/doctrine | High | High | `GOV-02` | Routing + required-reading matrix |
| Commit passes one gate definition but fails another | Medium | High | `GOV-04` | Single gate script |
| Card test run from served session | Low | Critical | `DECISIONS.md:109-116`, `HANDOFF.md:50-69` | Keep explicit fence in `AGENTS.md` autonomy table |
| Handoff lost on disk failure | Medium | Critical | `handoff/` single copy | Independent backup |
| Parallel sessions overwrite ledgers | Medium | High | `GOV-10`, `PRO-03` | Per-task files + write ownership |
| Documentation drift recurs | High | Medium | `DOC-02`, `DOC-03`, `PRO-14` | Same-commit rule + doc checks |
| Release ships with wrong version/notes | Medium | Medium | `PRO-09` | Release check |
| Secret/local path leak on commit | Low | High | `PRO-12` | Scrub checklist |
| Debt forgotten across three ledgers | High | Medium | `PRO-06` | Unified debt index |
| Bus factor: one operator, one machine | High | Critical | Entire structure | Tracked durable docs + backup + reproducible setup |

---

## 12. Maturity assessment

### 12.1 By governance area

| Area | State | Rationale |
|---|---|---|
| Agent behavior rules | Defined but fragmented | Rich rules, no hierarchy, no autonomy matrix |
| Development standards | Defined, partly irrelevant | Strong Rust rules, unused stack rules |
| Git workflow | Ad hoc but disciplined | Trunk-only; written rules mismatch history |
| Task definition | Partially defined | Classes and queue exist; no schema |
| Planning | Partially defined | Queue and backlog; no priority/readiness |
| Context handoff | Strong locally, absent portably | Best-in-class local discipline; untracked |
| Architecture docs | Rich but stale | DESIGN is a design log, not current architecture |
| Technical docs | Good coverage, some false claims | README is strong except strata |
| QA | Strong manual, weak automated | Gate + review + falsifiers; no CI |
| Onboarding | Weak | No tracked entry path |

### 12.2 Comparison to common maturity stages

- **Initial**: not applicable; the project is far beyond ad hoc.
- **Managed**: yes locally; the operator and agents follow a repeatable process on this machine.
- **Defined**: partially; the process is written but not universally routed, and several definitions conflict.
- **Quantitatively managed**: no; no metrics for lead time, gate pass rate, debt age, or doc freshness.
- **Optimizing**: no; improvements are ad hoc, though the review culture is strong.

Overall: **Managed locally, not Defined organizationally, not reproducible.**

---

## 13. Missing processes

The following processes are absent or only implicit:

1. **Documentation ownership and review lifecycle.** No owner, status, or review date.
2. **Definition of Ready.** A task can enter the queue without scope, acceptance, or dependencies.
3. **Definition of Done.** No per-task acceptance record; global gate is not a task acceptance.
4. **Mid-task checkpoint and resume.** No protocol before context compaction or interruption.
5. **Dirty-tree handoff.** No format for uncommitted work.
6. **Parallel work ownership and merge.** No file ownership, no conflict protocol, no merge policy.
7. **Release/version/changelog coupling.** Prose checklist only.
8. **Documentation validation.** No link check, path check, front-matter check, or template check.
9. **CI.** No automated cardless verification.
10. **Debt prioritization.** No aggregate register or aging.
11. **Secret/PII scrub.** No checklist for publishing local artifacts.
12. **Backup/retention.** No policy for `handoff/`, evidence, or archives.
13. **Onboarding path.** No `CONTRIBUTING.md`, no docs index, no "start here".
14. **Decision-record format.** Decisions are ledger rows; no ADR convention for significant architecture choices.
15. **Metrics.** No measurement of gate compliance, task cycle time, or documentation freshness.

---

## 14. Summary of recommendations

The full design is in `TARGET_STRUCTURE.md`, process changes in `PROCESS_IMPROVEMENTS.md`, sequencing in `IMPLEMENTATION_PLAN.md`, and issue-tracker-ready work in `DOCUMENTATION_BACKLOG.md`.

Priority order:

1. **Make the system reproducible.** Commit a minimal durable core (agent entry, process, doctrine, architecture overview, decisions, changelog). Keep live state and machine-specific evidence local, with a backup.
2. **Fix false documentation.** Rewrite README strata sections; fix stale code comments; reclassify `DESIGN.md` as historical.
3. **Define the instruction hierarchy and autonomy matrix.** One entry point, explicit precedence, one gate.
4. **Separate durable knowledge from live state.** `docs/` for durable; `handoff/` for live; pointers instead of copies.
5. **Adopt a task schema and handoff template.** Acceptance criteria, dependencies, evidence, next command.
6. **Resolve worktree and parallel-agent policy.** Delete stale worktrees; choose serial or per-task ownership.
7. **Automate the invariants.** Gate script, docs checker, template validator, CI.
8. **Add release discipline.** Version, changelog, tag, artifact hash.

Implementation started after the owner approved the target structure and decisions D1-D11 on 2026-11-02. The approval record is in `IMPLEMENTATION_PLAN.md`.

---

## Appendix A - Full instruction and documentation inventory

| Path | Type | Tracked | Live/durable | Primary audience | Audit issue(s) |
|---|---|---:|---|---|---|
| `README.md` | Product/user guide | Yes | Durable | Users, contributors | DOC-01, DOC-05, DOC-06, DOC-08 |
| `AGENTS.md` | Agent entry | No | Durable (should be tracked) | Agents | GOV-02, GOV-03, GOV-04, GOV-05, GOV-07, GOV-08 |
| `GIT_RULES.md` | Git rules | No | Durable (should be tracked) | Agents | GOV-03, PRO-04 |
| `AUX-RULES.md` | Process supplements | No | Durable (should be tracked) | Agents | GOV-03, GOV-04, GOV-10, PRO-02, PRO-05, PRO-10 |
| `HANDOFF.md` | Live state + bootstrap | No | Live (local-only) | Agents, operator | GOV-02, PRO-03, PRO-05 |
| `DESIGN.md` | Design history | No | Durable (should be tracked/historical) | Architects, agents | DOC-02, DOC-03 |
| `handoff/PENDING_CHECKLIST.md` | Live queue | No | Live | Operator, agents | PRO-01, PRO-02, PRO-05, PRO-06, PRO-08, PRO-11 |
| `handoff/FINDINGS-OPEN.md` | Findings | No | Live | Operator, agents | PRO-02, PRO-05, PRO-06 |
| `handoff/DECISIONS.md` | Doctrine/rulings | No | Durable (should be tracked) | All | GOV-03, PRO-05, PRO-06 |
| `handoff/DECISIONS-PENDING.md` | Open rulings | No | Live | Operator | PRO-05 |
| `handoff/GOTCHAS.md` | Traps | No | Live (machine-specific) | Agents | GOV-10, PRO-13 |
| `handoff/code-review/*` | Review protocol/evidence | No | Mixed | Agents | PRO-10 |
| `handoff/research/*` | Research | No | Durable-ish | Architects | PRO-01 |
| `handoff/*/logs`, `RESULTS.md`, `forensics/` | Evidence | No | Live/archive | Operator | DOC-07 |
| `handoff/archive/*` | Archive | No | Archive | Operator | DOC-07, PRO-05 |
| `archive/*` | Old handoffs/scratch | No | Archive | Operator | DOC-07 |
| `base-archive/*` | v3.3 baseline | No | Reference only | Architect | DOC-07 |
| `ModelConf/*.yaml` | Machine config | No | Local | Operator | PRO-06, PRO-12 |
| `.kilo/skills/*` | Generic skills | No | Durable (should be tracked) | Agents | GOV-09 |
| `.kilo/agent-manager.json` | Session state | No | Local | Tooling | GOV-06 |
| `Cargo.toml` comments | Embedded rationale | Yes | Durable | Maintainers | - |
| Rust doc comments | Component docs | Yes | Durable | Maintainers | DOC-03 |
| Git commit messages | Change rationale | Yes | Durable | Maintainers | PRO-04 |

## Appendix B - Commands and evidence used

Read-only commands executed:

- `git status --porcelain`, `git branch --show-current`, `git log --oneline -25`, `git log --pretty=format:"%h|%s" -30`, `git log -5 --pretty=format:"%h%n%B%n---"`.
- `git ls-files`, `git worktree list`, `git branch -a`, `git remote -v`, `git tag`, `git rev-list --count c91e179..HEAD`, `git rev-list --count origin/main..HEAD`.
- Directory and file-size enumeration of root, `src/`, `tests/`, `.kilo/`, `handoff/`, `archive/`, `base-archive/`, `ModelConf/`.
- Content search for `strata_config`, `no auth`, stale module claims, gate commands, commit messages, and documentation references.
- Direct reads of all governance documents, the five handoff ledgers, README, DESIGN, Cargo.toml, `src/lib.rs`, `src/config/mod.rs`, `src/model/scan.rs`, `src/engine/strata.rs`, and representative review prompts and archived handoffs.

Not executed: `cargo build`, `cargo test`, `cargo clippy`, `cargo fmt`, DOM tests, any runtime harness action, any model load, any Git mutation.

## Appendix C - Unknown / needs clarification

| Item | Why it matters | Current status |
|---|---|---|
| GitHub repository visibility | Determines whether committing doctrine leaks internal data | Unknown; `DECISIONS-PENDING.md:16` says cannot check |
| Desired team model | Determines whether to optimize for one operator or multiple contributors | Unknown |
| Backup of `handoff/` | The live development system is a single copy | Unknown |
| Global Kilo rules | May add or contradict project rules | Not inspected |
| Card/gate results | Project reports green, not re-run here | Reported only |
| Operator preference on file moves | Target structure proposes moves/renames | Needs approval |
| Whether `GOTCHAS.md` can be scrubbed and tracked | It contains valuable reusable knowledge and machine-specific paths | Needs owner decision |
| Whether PR workflow is desired | Current practice is direct-to-main | Needs owner decision |

---

## Addendum - operator requirement 2026-11-02: ASD-STE100

The owner added a requirement after the audit: durable documentation must follow
ASD-STE100 Simplified Technical English. This requirement was not scored as a finding in
the original audit. It is now part of the target design and the implementation plan:

- Standard and exceptions: `TARGET_STRUCTURE.md` Section 5.6.
- Process change and measures: `PROCESS_IMPROVEMENTS.md` Section 8.3.
- Implementation task: `IMPLEMENTATION_PLAN.md` P2-09.
- Backlog item: `DOCUMENTATION_BACKLOG.md` BL-36.

The requirement applies to new or changed durable text. Exact LOCKED wording, code
identifiers, paths, log text, and the historical `DESIGN.md` are exempt from wholesale
rewriting. A vocabulary-only check is not proof of compliance.
