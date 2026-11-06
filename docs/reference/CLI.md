---
title: CLI reference
owner: operator
status: active
audience: [user, operator, agent]
source_of_truth_for: [command-line verbs, exit behavior]
last_reviewed: 2026-11-06
review_trigger: behavior-change, release
---

# CLI reference

Up: `docs/DOCS_README.md`. This file owns the command-line surface of
`simpleharn.exe`. The model-row schema and the per-engine keys live in
`docs/reference/MODELS.md`.

## Run the harness

A command line that does not start with `import` boots the harness. Boot
runs the single-instance guard, reads the config, binds the listeners, and
starts no engine before a load. Two entry points run it:

- `SimpleHarn.vbs` beside the exe: starts the harness with no console
  window.
- `simpleharn.exe` directly: shows the live tracing console.

Autostart and the file layout live in `docs/reference/OPERATIONS.md`.

## `import strata`

The one command-line verb. It is the migration tool for a manually managed
Strata profile: it reads the profile and emits the model-row YAML knob block.
It is a command, not a serve: no single-instance guard, no config read, and
no listener. It is the only sanctioned reader of a manual Strata profile.

Print the block, write nothing:

```text
simpleharn import strata <profile.json>
```

The YAML knob block goes to stdout. stdout carries only that YAML, with no
log decoration, so you can pipe it. The command touches neither the profile
file nor any model row.

Merge the block into a live row:

```text
simpleharn import strata <profile.json> --apply <row.yaml>
```

`--apply` merges the block into the row file in one atomic write. Strata-owned
keys and `serve_model` are replaced. `strata_config` and stale `strata_*` keys
are deleted. `category`, `vram_gb`, `cost`, `load_timeout`, `min_turn`,
`launch_only_with_at_least_vram_free`, `context`, `name`, `model`,
`strata_python`, comments, and key order are preserved. The reply line counts
the row lines the merge changed. The merge key rules are in
`docs/reference/MODELS.md` (Strata engines).

## Exit behavior

- A successful print or apply exits with code `0`.
- A refused import names the reason on the tracing stream and exits with a
  non-zero code. A refused `--apply` writes nothing: a missing, unreadable,
  duplicate-key, empty, or non-strata target is refused with a named reason.
- A wrong verb, a missing profile path, or an extra argument prints the usage
  line and exits with a non-zero code.
