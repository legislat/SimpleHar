---
title: Deferred design notes
owner: operator
status: active
audience: [architect, contributor, agent]
source_of_truth_for: [design notes for deferred Berth items B9 and B10]
last_reviewed: 2026-11-05
review_trigger: new-decision, operator approval of either item
---

# DEFERRED-IDEAS.md

Up: `docs/DOCS_README.md`. This file holds design notes for deferred ideas. It
records design thinking and open risks. It makes no ruling.
`docs/architecture/DECISIONS.md` owns every ruling, and
`docs/architecture/OVERVIEW.md` owns current behavior.

Both notes come from the Berth borrow queue (Search#41 investigation). The
operator ruling of 2026-11-04 placed both items in the last tier: design notes
only, and no build without a new operator approval.

## 1. B9 - Priority classes (hook design only)

### End state

- Every admitted load carries one priority class. Four classes exist: hold,
  foreground, interactive, batch, in that order from highest.
- The scheduler uses the class in two places only: the choice among equal-cost
  victims, and the order of waiting loads.
- The class never enlarges a budget. Admission keeps the witness as budget
  truth and refuses with named blockers (DOCTRINE items 3 and 4).

### Why it waits

- Every request today arrives as one of the four category slots. The
  `evictable_by` matrix already ranks the categories. A per-request class
  would rank one single consumer class.
- One class changes no decision. It adds policy surface, refusal text, and a
  test matrix that nothing exercises.
- The operator ruling of 2026-11-04 keeps the item closed until two real
  consumer classes exist. A second real consumer re-opens it.

### Where a per-API-key class enters

- The request boundary decides the class. The API guard authenticates a
  request, and the three inference routes turn it into one admission command.
  The class rides that command, so a refusal, the queue place, and the job row
  can name it.
- The guard checks one shared control token today. It knows no caller
  identity. Per-key tagging therefore needs a caller registry first: a
  key-to-class map in the harness config, so config stays king.
- The guard order stays fixed. Structural refusals still precede the token
  (DOCTRINE item 7). The harness reads the class only after authentication,
  never instead of it.

### How the existing structures carry it

- A memory refusal already ships its decision data: `VictimInfo` (model,
  category, cost) and `BlockerInfo` (model, category, reason) in
  `src/budget.rs`. Each gains one class field.
- The planner reads the class from the registry at decision time, exactly as
  the `min_turn` guard reads the catalog. No queue row copies the class, so no
  second truth can drift.
- The class orders only what the `evictable_by` matrix already allows. It
  never frees a protected holder, so it grows no second eviction rule.

### Risks

- Starvation. A steady stream of high-class traffic can hold a batch load
  waiting forever. The design needs aging or a floor, and the wait estimate
  (B2) must keep telling the truth about that wait.
- Involuntary bumps. A high-class load that evicts a serving low-class holder
  makes service unpredictable. The `whole` gate (B5) and the `min_turn` guard
  (B1) deliberately never evict for their reasons. The class rule must state
  plainly whether a class alone may evict.
- Eviction fairness. A class order competes with the cheapest-first rule (B6)
  and with the honest queue place (B2). Every ordering change must keep the
  refusal text and the queue position honest, or the refusal lies to the
  client.

No implementation exists, and none is approved.

## 2. B10 - State survives the daemon (restart reconciliation)

Design note only. No implementation exists, and none is approved.

### Why it is gated behind B3

- A reconciliation decides what a live engine means to a new scheduler and a
  fresh slot table. That decision lives inside the slot lifecycle.
- B3 gave that lifecycle an observable truth: the `releasing` state and the
  evidence-based release path (committed 2026-11-04). A reconciliation design
  before B3 would have designed against a moving target. The ruling runs B3
  first, and B3 is done.

### The core tension

- Every owned engine tree runs inside one Win32 Job Object with
  kill-on-close. A harness death for any reason makes the kernel kill the
  tree. That is the crash-proof orphan guarantee. The spawn path seals the
  child into its job before it runs a first instruction, so no descendant
  escapes.
- A re-attach wants the opposite: the engine must outlive the harness. That
  means breaking it out of the job, which removes the kernel backstop. From
  then on, every guarantee that a crash leaves nothing running rests on the
  boot sweep and on claim identity: reaping stays ownership-scoped, identity
  re-verifies at the instant of the signal, and the harness never kills what
  it did not start (DOCTRINE items 5 and 6). These rules become the only
  wall, not a second wall.
- DOCTRINE item 18 scrapped "state restore across restarts". Options b and c
  restore live state, so each needs a new operator ruling before any build.

### Option a: no re-attach (today, plus B3 honesty)

- The harness does not re-attach. The job kills the children at death. The
  custody sidecar (`src/custody.rs`) and the container claims ledger record
  every owned launch with a pid and a start-time identity. The boot reconcile
  reaps proven strays before any engine work, and the scheduler reloads on
  demand.
- Pros: every kill-safety guarantee stays byte-identical. No new state and no
  new authority exist.
- Cons: a restart pays a full reload for every model. In-flight requests die.

### Option b: break-away launch with ownership re-verification

- The engine starts outside the harness kill job. The claim record stores its
  identity at launch. At boot the harness re-verifies each claim (pid plus
  start-time token), adopts a proven live engine into its slot, and reaps
  what it cannot prove.
- Pros: engines survive a daemon restart. A planned restart pays almost no
  reload cost.
- Cons: no kernel backstop remains for an escaped tree. A harness crash
  before the boot adopt leaves VRAM held with no timer. The
  suspended-create-then-assign order that keeps descendants sealed loses its
  meaning for these rows. Adoption must stay as strict as reaping: identity
  verified at the signal instant, port plus generation tickets, forged
  claims dropped.

### Option c: sidecar supervisor

- One small resident component owns the kill-on-close jobs. It survives
  harness restarts. The new harness re-attaches to the supervisor, and the
  supervisor hands the jobs back.
- Pros: one kernel-enforced kill owner stays in place, and the daemon
  restarts freely.
- Cons: a second always-on component brings its own install, update, and
  crash story. It needs its own orphan rule: what reaps the supervisor. The
  single-instance rule and the tray model must account for it. It is new
  product surface, not a borrowed rung.

### Kill-safety checklist (every option)

- Only a proven claim may be signaled: pid plus start-time identity,
  re-verified at the instant of the signal.
- A claim naming an engine the policy table does not know is forged. Drop it;
  never act on it.
- Kill and release stay evidence-based (handle signaled, port refused) before
  that port may serve a new spawn. A stale ticket is refused, never honored.
- The boot reconcile completes before the first admission decision. A live
  engine is adopted or reaped before the scheduler books it.
- Every crash moment must converge. VRAM held outside a job is bounded in
  time or surfaced to the operator, never silent.
- Reaping stays ownership-scoped. No option may add a path that kills
  whatever holds a port.
- The single-instance guard keeps exactly one harness as writer of slot
  state.
