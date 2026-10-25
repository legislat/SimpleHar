# Git Rules for AI Agents

These rules supplement `AGENTS.md`. Follow repository-specific
instructions and existing project conventions first.

## Before Changing Git State

-   Run `git status --short` and inspect the relevant diff before
    editing, staging, or committing.
-   Preserve all pre-existing user changes. Do not assume uncommitted
    changes were created by you.
-   Do not discard, overwrite, or silently modify unrelated work.
-   If the working tree contains changes you cannot confidently
    distinguish from your own, leave them untouched and ask when
    necessary.

## Staging Changes

-   Stage only files and hunks required for the task. Prefer explicit
    paths or interactive staging over `git add .` or `git add -A`.
-   Review staged changes with `git diff --cached` before committing.
-   Never stage secrets, credentials, `.env` files, local configuration,
    build artifacts, temporary files, or unrelated changes.
-   Do not commit commented-out code, debug output (`println!`, `dbg!`),
    or generated files unless they are intentionally part of the project
    change.

## Commit Quality

-   Create a commit only when the user explicitly asks for one or
    repository instructions clearly authorize it.
-   Keep each commit small and focused on one logical change. Do not mix
    unrelated fixes, refactors, formatting, and dependency updates.
-   Write a concise, descriptive, imperative commit subject, such as
    `Fix token refresh handling`.
-   Add a commit body when needed to explain the reason for the change,
    important trade-offs, or context not evident from the diff.
-   Follow the repository's existing commit-message format and any
    configured hooks or signing requirements.
-   Do not create empty commits or split changes into noisy commits
    without a meaningful reason.

## Validation and Review

-   Before committing, run the relevant tests, formatters, and linters
    required by `AGENTS.md` and the project.
-   Review both `git diff` and `git diff --cached` as applicable.
    Confirm the final change contains only the intended work.
-   Report checks that were run and their outcomes accurately. Clearly
    disclose checks that were not run or that failed.
-   Do not claim a change is tested, formatted, or lint-free unless the
    corresponding checks support that claim.

## Protected Operations

Do not perform these operations without explicit user authorization:

-   `git reset --hard` or other commands that discard working-tree
    changes.
-   `git checkout -- <path>`, `git restore`, or equivalent commands that
    overwrite or discard user changes.
-   `git commit --amend`, interactive rebase, history rewriting, or
    changing existing commits.
-   `git push --force` or other force-push variants.
-   Deleting branches or tags, cleaning untracked files, or removing
    repository data.
-   Pushing commits or creating releases.

Before any authorized destructive or history-changing operation, explain
the expected impact and verify the target.

## Completion

-   Leave unrelated changes untouched.
-   Summarize the changes made and list the relevant validation results.
-   If Git state is ambiguous or an operation could lose work, stop and
    ask rather than guessing.
