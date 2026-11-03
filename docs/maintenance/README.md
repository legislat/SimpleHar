---
title: Maintenance and audit archive policy
owner: operator
status: active
audience: [operator, agent, architect]
source_of_truth_for: [maintenance and audit archive policy]
last_reviewed: 2026-11-03
review_trigger: release, quarterly
---

# docs/maintenance/README.md

This file is the maintenance and audit archive policy for this repository.
The operator adopted it on 2026-11-03 with the 2026-11 documentation cleanup.

## 1. Purpose

This directory stores documentation maintenance activities, repository
audits, cleanup reports, and historical improvement initiatives. It is not
part of the project's operational documentation. Agents do not read it for
current behavior, rules, or requirements.

## 2. Directory structure

- Active maintenance initiatives live in
  `docs/maintenance/<YYYY-MM>-<slug>/`.
- An initiative with more than one deliverable gets its own directory under
  `docs/maintenance/`. A single small initiative may sit directly under
  `docs/maintenance/`.
- A completed initiative moves to
  `docs/maintenance/archive/<YYYY-MM>-<slug>/`.

## 3. Archiving rules

1. Archive completed audits and maintenance initiatives as immutable
   historical records.
2. Preserve the original audit findings, recommendations, and approved
   implementation plans. Move them byte-identical.
3. Add the final execution report when an initiative is implemented.
4. Do not keep obsolete working drafts unless they carry meaningful
   historical value.
5. Do not reference archived reports as active instructions.
6. Do not load archived documents into the default agent context.
7. Do not treat historical recommendations as current requirements.
8. Keep links to archived reports only for historical traceability.
9. Do not edit an archived report to match the current architecture. Write a
   new report instead.

## 4. Completion requirements

Before archiving an initiative, confirm all of the following:

- Its implementation or review is complete.
- The final report exists.
- Unresolved issues and deferred recommendations are recorded.
- No active document depends on temporary audit artifacts.

## 5. Authority

Archived maintenance documents are historical records only. They do not
override current instructions, architecture documentation, approved
decisions, or development guidelines.

## 6. Role separation

Each archive location has one distinct role. Do not mix roles.

| Location | Holds | Authority |
|---|---|---|
| `docs/maintenance/archive/` | Completed documentation and process initiatives | None; immutable historical record |
| `docs/archive/` | Superseded durable project documents (reserved; not created because none exists) | None; non-authoritative |
| `handoff/archive/` | Closed live state: tasks, queues, snapshots (local-only) | None; frozen historical record |
| `archive/` | Frozen revisions of superseded governing documents (local-only) | None; never read for current behavior |
| `base-archive/` | Frozen v3.3.0 baseline product and source (local-only) | None; reference only, never built |
