---
title: Changelog
owner: operator
status: active
audience: [user, maintainer]
source_of_truth_for: [release history]
last_reviewed: 2026-11-02
review_trigger: release
---

# Changelog

Up: `docs/DOCS_README.md`. Release history of SimpleHar SE. The format follows Keep
a Changelog. Versions follow Semantic Versioning.

Rule: no tag without a dated entry here and a matching `Cargo.toml` version.
The release steps live in `docs/process/GIT_WORKFLOW.md`.

## Unreleased

Repository governance work: durable documentation, process documents, and
automation. This work changes no product behavior yet.

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
