---
title: Changelog
owner: operator
status: active
audience: [user, maintainer]
source_of_truth_for: [release history]
last_reviewed: 2026-11-03
review_trigger: release
---

# Changelog

Up: `docs/DOCS_README.md`. Release history of SimpleHar SE. The format follows Keep
a Changelog. Versions follow Semantic Versioning.

Rule: no tag without a dated entry here and a matching `Cargo.toml` version.
The release steps live in `docs/process/GIT_WORKFLOW.md`.

## Unreleased

### Changed

- Split the scheduler's `ActorState` method clusters into child modules:
  `admission`, `catalog`, `commands`, `events`, `fit`, `measured`, `occupant`,
  `spawn`, and `status`. The actor loop, command channel, and state owner stay in
  `scheduler/mod.rs`. This completes Phase 1 and Phase 2 of the cohesion split;
  the API split remains deferred. No behavior changes.

## v1.1.0 - 2026-11-03

Commit: `f6c6381` (release prep). Artifact: `simplehar-se.exe`, SHA256
`031D94D827E407AB5A3B1B5E75B651E3FF108BE804B7C869A665EDD76D45A376`, built
2026-11-03 and staged at `C:\Inference\SimpleHar-SE_release_v1.1.0`. The live
harness remains on v1.0.1 until the operator switches it.

### Added

- `simplehar-se import strata <profile.json> --apply <row.yaml>` merges the
  emitted knob block into a live row in one atomic write. It preserves the
  scheduling keys and `strata_python`, deletes `strata_config`, and refuses
  conflicts without a partial write.

### Security

- The canonical ninfer launch now passes `--api-key` when `ninfer_key: true`.
  Before this fix the engine ran unauthenticated while the stamp said `key=on`.

### Fixed

- Strata rejects `--host=` and `--port=` as well as the space forms.
- Admission settles against `gate + reserve`, and a same-slot holder is
  credited once.
- A late unconfirmed release gets one bounded, observe-only watcher.
- The boot config is flushed before rename; a landed-but-unverified write
  records the real verdict.
- Model files must be regular files; `mmproj` is traced and resolved.
- Log rotation prunes the generation budget; a contended sink cannot wedge
  emitters.
- The control page is self-contained and a refused logging flip unflips.
- The Windows test gate uses an ephemeral foreign port.

### Changed

- Dependencies: the unused `reqwest` blocking feature is removed and `tokio`
  uses the explicit feature set.
- README documents the current logs, tray, model keys, and import contract.

### Removed

- The legacy `ninfer_autostart` launch path. A row that declares it is refused
  with a migration message; template mode is the only ninfer path.

## v1.0.1 - 2026-11-02

Commit: `c91e179`. The annotated tag `v1.0.1` was created, pushed, and deployed
on 2026-11-02. The release covers the 12 commits between `446a3d3` and
`c91e179`. The commit log holds the change detail.

## v1.0.0 - 2026-10-29

Commit: `11b60a1`. First release of SimpleHar SE.

### Added

- A Windows tray harness that supervises local LLM inference engines (llama,
  strata, ninfer, mock) on one GPU.
- A single-writer swap scheduler. It admits loads against live measured memory
  (the witness) and refuses with named blockers. It never overcommits.
- Four category listeners (WORK, RP, AUX, EMB) with an OpenAI-compatible API.
- A control page on the WORK listener.
- Declarative configuration: one JSON config file for the harness and one YAML
  file per model, with write-back.
