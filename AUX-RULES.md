# Additional project repository-specific rules

These rules supplement `AGENTS.md`. Follow repository-specific
instructions and existing project conventions first.

- Code Review: never do yourself. Delegate to subagents.
- When using subagents use them consequently (only 1 active per session at the given time).
- Subagent briefs must command a whole-file read of `handoff/GOTCHAS.md` and carry
  `handoff/DECISIONS.md` LOCKED as standing context; briefs cite the ledger
  (`handoff/DECISIONS-PENDING.md`) as the only source of unresolved rulings.
- Bookkeeping single-home rule: every fact has exactly one file (routing table in
  `HANDOFF.md` §8); a fact found in two files is a bug — fix by replacing the copy with a
  pointer. `DECISIONS-PENDING.md` holds only questions the operator owes an answer to.

## Review closure

- Review completion is not finding closure. A chunk is `[x]` only when every finding is
  fixed or explicitly accepted; a reviewed chunk with open findings stays `[ ]` with a
  pointer to the finding.
- `handoff/FINDINGS-OPEN.md` is the only home for open findings. Every finding carries its
  review ID (`05[8]`, `06[5]`, …), severity, `file:line`, one-line claim, class
  (`[S]`/`[R]`/`[C]`/`[O]`), and status. A finding leaves the ledger only by a commit that
  fixes it or by an operator ruling that accepts it; the commit or ruling is named in the
  row.
- A code fix for a finding must add a falsifier test that fails on the pre-fix code. A
  fix without a falsifier is a claim, not a closure.
- The checklist's chunk-coverage line states R1 and R2 separately: `R1 done: …`,
  `R2 done: …`, `R2 candidates: …`, `R2 not scheduled: …`. A chunk that is neither done
  nor a candidate is a gap, not an omission.

## Gates and release

- The commit gate is exactly: `cargo check --all-targets` · `cargo test` (includes the
  doc-test) · `cargo clippy --all-targets -- -D warnings` · `cargo fmt --check` ·
  `node tests/webui/webui_dom_test.mjs` (102 legs). A gate line that omits one of these is
  stale.
- Release close-out is a queue row, not prose: tag the reviewed HEAD, push the commits,
  update the README's release claim, and record the deployed artifact hash. Until the tag
  exists, the README may not claim the release.
- HEAD, the unpushed list, and tree state live only in `HANDOFF.md` §2. The checklist
  points there; it does not restate them.
