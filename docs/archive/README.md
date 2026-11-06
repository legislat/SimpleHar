---
title: Documentation archive
owner: operator
status: active
audience: [user, contributor, agent, architect]
source_of_truth_for: [archive conventions, archived durable document index]
last_reviewed: 2026-11-06
review_trigger: release, quarterly
---

# docs/archive/README.md

Up: [docs/DOCS_README.md](../DOCS_README.md). This directory holds superseded
durable project documents. The archive rule in the docs map requires this
home: a superseded durable document moves here with a status banner. It is
never silently deleted. Maintenance and audit records stay in the local-only
layer, never here.

## Banner convention

Every archived page keeps its front matter and sets:

- `status: archived`.
- An archive date line beside the banner.
- A one-line reason for the archive.
- A replacement link to the document or section that carries the fact now.

A minimal archived front matter block:

```
---
title: <original title>
owner: operator
status: archived
last_reviewed: <archive date>
review_trigger: none
---

> **ARCHIVED 2026-11-06.** <One-line reason.> Replaced by
> [<replacement>](<relative link>). This page is frozen.
```

Link the archived page back to this index. Every archived page name plus its
replacement belongs in the index table below.

## Freeze rule

Archived text is never read for current behavior. It is kept only for
rationale and history. A conflict between an archived page and any live
document is resolved in favor of the live document, and the archive rule
points the reader to the replacement. Current architecture lives in
`docs/architecture/OVERVIEW.md`. Binding decisions live in
`docs/architecture/DECISIONS.md`.

## Index of archived documents

| Archived document | Archived | Replaced by |
|---|---|---|
| (none yet) | - | - |
