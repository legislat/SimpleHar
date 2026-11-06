---
title: Publication safety
owner: operator
status: active
audience: [agent, contributor, operator]
source_of_truth_for: [publication safety]
last_reviewed: 2026-11-06
review_trigger: release, quarterly
---

# PUBLICATION.md

Up: `docs/DOCS_README.md`. This file is the single home of the publication scrub
rules. It prevents leaks when durable content is extracted from local
artifacts. Local artifacts contain machine paths, ports, key-file names,
container names, and incident logs.

## When to scrub

Run the scrub checklist before you:

- commit a file that came from the live layer or the evidence layer;
- promote a research report, an incident log, or a review artifact into the
  durable tree;
- share a log, a screenshot, or an artifact outside this machine.

## Scrub checklist

Remove or replace every occurrence of:

- absolute paths;
- usernames;
- hostnames;
- IP addresses;
- port numbers that identify this machine;
- token and key names, and any secret material;
- container names;
- model paths that identify this machine;
- incident logs that name this machine or its work;
- prompts, completions, and any personal data found in logs;
- screenshots that show any of the above.

## Screenshots

- Generate a UI screenshot from generic rows. Show the current version.
- Check the image for model names, file paths, tokens, hostnames, and local
  inventory before you commit it.
- Replace `docs/assets/control-page.png` when the UI or the version changes.
  The release package ships the current image.

## Rules

- Confirm repository visibility before you commit local artifacts. Never
  assume it. Check the actual setting first.
- Any publication of incident evidence needs a second reviewer. The second
  reviewer runs the checklist independently, before the commit.
- Record what was scrubbed and why. Put the record in the promotion note or in
  the commit body.
- Durable docs contain no machine paths. A machine path in a durable doc is a
  defect.
- Local evidence stays local. `HANDOFF.md`, `handoff/`, and `ModelConf/` are
  gitignored and backed up separately. Promote content out of them only after a
  scrub.
- Secrets never enter durable files, logs, or screenshots. The coding-side rule
  lives in `docs/process/ENGINEERING.md`.

## Scope

- This file does not change what is public in the product. It governs only what
  is extracted from local artifacts into tracked or shared files.
- If a scrub decision is unclear, keep the artifact local and ask the operator.
