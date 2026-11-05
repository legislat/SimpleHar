---
title: Binding product invariants
owner: operator
status: active
audience: agent
source_of_truth_for: [binding product invariants]
last_reviewed: 2026-11-05
review_trigger: new-decision, release
---

# DOCTRINE.md

Up: `docs/DOCS_README.md`. This is the one-page binding invariant card every agent
must carry. Each item states one rule and its consequence. Do not soften a
rule. Do not invent a ruling here. Full rationale and tombstones live in
`docs/architecture/DECISIONS.md`. Quoted clauses keep the operator wording;
non-ASCII punctuation in quotes is replaced with ASCII equivalents only.

1. **Config file is king.** "Config file is king (read and write-back target
   `AppConfig.source_path`, `SIMPLEHARN_CONFIG`-aware; SimpleHarn's own
   `simpleharn.json`, and no other config name is ever read (ruling
   2026-11-05; an
   explicitly selected `SIMPLEHARN_CONFIG` path is an operator instruction and the
   write target, ruling 2026-11-03), one shared
   `config::default_config_path()`)." `SIMPLEHARN_TOKEN` beats the file value.
   Consequence: page edits persist to the file boot read; an invalid file
   fails boot and is never silently defaulted.
2. **External engines obey only harness-owned configuration.** "A child's
   whole config set - strata profile, ninfer autostart inputs, engine env -
   lives under harness control (like `ModelConf/`), validated at scan/spawn; policy
   fields (`host`, `port`, key) harness-forced, never obeyed from engine-side
   files." Consequence: no engine-side file can open a bind, move a port, or
   set a key.
3. **Admission is the product.** "Admission is the product: refuse with named
   blockers, never overcommit." Consequence: a refusal that names blockers
   and numbers is a correct product outcome, not a failure to hide.
4. **The witness is budget truth.** "Witness 2 s TTL, no stale guesses"; the
   ledger sum "left the gate and the UI, surviving only in `LedgerOnlyWitness`
   (no-GPU mode; dead witness on real HW fails closed)". Consequence: the gate
   computes `total - reserve - observed_used + same-slot victim credit -
   in-flight bookings` from the card's live reading; an unreadable card
   refuses instead of guessing.
5. **Kill and release are evidence-based.** "Evidence-based kill/release
   (handle signaled + TCP-refused proof)"; engine kill/release require
   `(port, generation)`, and "a stale ticket is refused, never honoured".
   Consequence: a spawn fires only on a proven release; a late ticket can
   never act on a reused port or a newer generation.
6. **Reaping is ownership-scoped.** "A job reaps every process it owns (victim
   + own spawn)"; "the harness never registers/monitors what it did not
   spawn". Consequence: there is no path to kill "whatever holds a port"; a
   foreign resident is report-only, never killed.
7. **Guard order is fixed; structural refusals precede the token.**
   "Structural refusals (Host, cross-site `Origin` -> 403, content-type ->
   415) precede the token (401); only `/`, `/index.html`, `/app.js`,
   `/style.css`, `/pico.min.css`, `/favicon.png`, `/favicon.ico` are
   unauthenticated, and `GET /health` is token-gated." Consequence: the token
   decides who, never whether a request shape is acceptable; a liveness probe
   must carry the token.
8. **Client-facing `/v1/models` stays minimal.** "Client-facing `/v1/models`
   stays minimal (no local paths); control surface owns detail." Consequence:
   a public listener never leaks machine paths; `/har/*` owns the detail view.
9. **Cost verdicts are data.** "Strata yamls declare `cost: greedy` with their
   launch floors (currently 60/65 in `ModelConf/`, not code); ninfer defaults
   to `fixed` at declared vram." Consequence: a cost policy change rides the
   model rows, not a code constant.
10. **Scan accepts only `*.yaml`.** "Scan accepts only `*.yaml` (regular,
    non-symlink, case-insensitive; `.yml` and everything else silently inert;
    a problem line never publishes)." Consequence: a broken row cannot publish
    into the catalog; unrecognized files stay inert and silent.
11. **Config layers are L1/L2/L3, and the harness launches only from L3.**
    Imported L1 artifacts are copies "then forgotten - after import the
    harness never reads or writes the original"; the L3 launch spec is
    "generated from L2 + `EnginePolicy` + scrubbed L1, and the harness
    launches only from L3"; "L3 must be byte-reproducible from L2 + policy +
    L1 - if anyone needs to hand-edit L3, the design has failed." Dangerous
    L1 parameters are scrubbed twice: at import for auditability, at render
    for authority. Consequence: a hand-edit of a rendered launch spec is a
    design defect, not a configuration.
12. **A measured figure books only if its provenance is trusted.** "A measured
    figure carries a provenance and books only if trusted"; a `total_diff`
    figure "never books"; "provenance is never upgraded post-hoc"; a
    `cpu_offload` flip or a `ninfer_profile`/`--kv-capacity` change voids the
    figure. Amended ruling: "Cost = max booking cost" - a guarded engine-reported
    figure above the declared value books instead of it (`engine_report` from a
    container, `engine_status` from a process engine's own status/metrics; valid
    without a profile stamp, the profile identity stays ninfer-only, and an
    `engine:` flip voids it). Consequence: admission books only trusted
    attributions, and the physical gate stopper stays the observed `used_gib`.
13. **Container-internal ports are a third namespace (R10).** The container
    inside is a third namespace; `ninfer_internal_port` defaults to the harness
    number 8808; "a row naming a `protected_ports`, public-listener or
    engine-plan number is refused at arm time (`AdmissionError::PortClaimed`)
    and named by the rescan"; `AppConfig::port_claim` is the single decision.
    The canonical template makes the publish argument required: "a missing
    publish argument fails the script, never borrows the host namespace".
    Consequence: the harness, manual, and container port sets stay disjoint.
14. **The card window belongs to the handoff, never to a served session.** No
    agent may "start harness instances, load models, or execute
    `verify.ps1 -Card`"; card evidence is produced only by the
    `handoff/wave-verify/` package run in an operator-spawned session.
    Cardless in-tree work (fmt, clippy, `cargo test`, DOM stub) stays allowed
    because it execs no engine. Consequence: an agent whose session is served
    by engines never touches the card.
15. **No third-party software names in code or config defaults.** The
    name-free rule: "no hardcoded third-party software names in code/config
    defaults"; documented defaults of supported engines are exempt; "never
    names of software outside the harness's engine scope". Consequence: a new
    default may name only a supported engine.
16. **The `bak:` yaml node is silent forensic, manual only.** "Never read,
    warned, logged, surfaced or written back (stripped from parsed `raw` like
    `external`); one line; a second is a problem line; the harness rewrites a
    present node only inside the incident `.yaml.bak` copy, never appends
    one." Consequence: forensic notes are for the operator; the harness never
    acts on them.
17. **Tray red means a listener is down.** "Tray red = listener down, not
    per-job errors." Consequence: a per-job error rides the tooltip; only a
    dead listener turns the icon red.
18. **Scrapped by the operator, never re-opened.** "Scrapped by operator:
    bonsai class, engine adoption, preload at boot, parallel soak, state
    restore across restarts"; "`protected_containers` scrapped - reaping
    strictly ownership-scoped"; "v3.3 `external: true` inert; `sticky:`->AUX
    alias deleted from the parser". Consequence: work that reintroduces any
    of these needs a new operator ruling first.
