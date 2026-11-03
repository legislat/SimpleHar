---
title: Engineering standards
owner: operator
status: active
audience: [contributor, agent]
source_of_truth_for: [how code is written]
last_reviewed: 2026-11-02
review_trigger: behavior-change, release, quarterly
---

# ENGINEERING.md

Up: `docs/DOCS_README.md`. The detailed coding, testing, documentation, security, and platform
standards for this crate. Gate commands live in `docs/process/VERIFICATION.md`; review
method lives in `docs/process/REVIEW.md`. This file points to them and does not repeat them.

## Rust style

- Four spaces, never tabs. Line limit 100 columns (rustfmt default).
- snake_case for functions/variables/modules, PascalCase for types/traits,
  SCREAMING_SNAKE_CASE for constants.
- Meaningful, descriptive names; follow the Rust API Guidelines and idiomatic style.
- `rustfmt` and `clippy` must pass with no warnings before any commit. Suppression
  (`#[allow]`) only with a reason; never `#![deny(warnings)]` in source (the gate uses
  `-D warnings`).

## Error handling

- Never `.unwrap()` on a production path; an `Err` there becomes a panic.
- `.expect()` only for an invariant violation that cannot happen, with a message that
  states the invariant.
- Typed errors with `thiserror` for domain and library code; `anyhow` at application
  boundaries, adding context with `.context()`.
- Propagate with `?`. Report console errors with `tracing::error!`, never `println!`.
- In tests `.unwrap()` is permitted; give each one a reason (asserts and setup).

## Type design

- Use the type system to prevent bugs at compile time; newtypes distinguish semantically
  different values that share a representation.
- Prefer `Option<T>` over sentinel values.
- Keep structs and enums on one responsibility; derive `Debug` and `Clone`/`PartialEq`/
  `Default` where sensible.
- Fields private by default with accessors as needed; builders for complex construction;
  composition over inheritance-like patterns.

## Function design

- One responsibility per function; at most 5 parameters, then a config struct.
- Prefer borrowing (`&T`, `&mut T`); return early to reduce nesting.
- Prefer iterators and combinators to manual loops; `enumerate()` over a manual counter;
  `if let`/`while let` for a single pattern and exhaustive `match` otherwise; `format!` for
  formatting.
- `unsafe` only when unavoidable, with the safety invariant documented.

## Testing

- Unit tests for new behavior and real risk, not for every function.
- Integration tests where behavior crosses module or process boundaries.
- Every bug fix ships a falsifier: a test that fails on the pre-fix code.
- Arrange-Act-Assert; `#[cfg(test)]` modules; mock external dependencies (APIs, files,
  engines).
- Never commit commented-out or disabled tests. Test tiers and the gate live in
  `docs/process/VERIFICATION.md`.

## Documentation

- Doc comments on every public function, struct, enum, and method; document arguments,
  returns, and errors; add an example for non-obvious functions.
- Comments explain non-obvious Rust for a reader strong in another language but new to
  Rust: lifetimes, ownership moves, `Cow`, `Send`/`Sync`, `async`/`await`, `Deref`,
  trait objects, `unsafe` invariants.
- No tautological comments that restate the code or a self-explanatory name; no comments
  that leak internal process notes or a task prompt.
- Update comments in the same change as the code. Stale claims are defects, not
  milestones: never encode "arrives in a later turn" or "no auth yet".

## Documentation language (ASD-STE100)

Durable user-facing and process documentation must follow ASD-STE100 Simplified
Technical English. This covers `README.md`, `CONTRIBUTING.md`, `CHANGELOG.md`,
`docs/reference/*`, `docs/process/*`, and new `docs/architecture/*` text.

Rules:

- Use the active voice. Use the imperative for instructions.
- Write one idea per sentence. Keep procedural sentences to 20 words or fewer and
  descriptive sentences to 25 words or fewer.
- Use approved, consistent terminology. Define a term once, then use the same word.
- Use simple verb tenses. Avoid passive voice, idioms, metaphors, humor, and wordplay.
- Use "must" for a requirement, "should" for a recommendation, and "can" for a
  possibility. Do not use "shall".
- Keep noun clusters to three words or fewer.
- Use a vertical list for a sequence of three or more steps or conditions.
- Do not use a semicolon to join two instructions. Write two sentences.
- Keep code blocks byte-synced with their source of truth. Do not paraphrase code.
- Add a short glossary when a document introduces more than three project terms.

Exceptions:

- Exact operator doctrine and LOCKED decision wording stays verbatim. Do not rewrite a
  binding sentence to satisfy a style rule.
- Code identifiers, command names, file paths, log text, and API field names are not
  translated or reworded.
- `DESIGN.md` is a historical record. Do not rewrite it wholesale; new annotations must
  follow this section.
- Existing documents are migrated in phases. New text must comply immediately.

Verification:

- Check each changed section against the STE main rules and the approved dictionary.
- Record the check in the review or release evidence. Do not claim full compliance from a
  vocabulary-only check.
- A documentation review must name at least one sentence-level rule that was checked.

## Concurrency

- `tokio` for async; keep the reactor unblocked (never `sleep`, blocking I/O, or CPU work
  on an async task; use `spawn_blocking`). `rayon` for CPU-bound parallelism.
- Respect `Send`/`Sync`. Prefer `RwLock` or channels (`mpsc`, `crossbeam`) over a
  contended `Mutex`.
- The scheduler is single-writer: do not add a second writer to scheduler state.

## Memory and performance

- Avoid needless allocation; prefer `&str` to `String` and `Cow<'_, str>` when ownership is
  conditional; `Vec::with_capacity` when the size is known; prefer stack to heap.
- `.clone()` explicit on non-`Copy` types; avoid hidden clones in closures and iterators;
  `Arc`/`Rc` only when sharing is genuinely required.
- "Fully optimized" is replaced by measurable criteria:
  - Algorithmic complexity no worse than the obvious correct approach, or justified in
    review.
  - No allocation inside a hot per-item or per-event loop without a note.
  - A performance claim carries benchmark evidence in the task, not a slogan.
  - Benchmarks run solo, never with `RUSTFLAGS`/`target-cpu=native`; comparisons are
    apples-to-apples; benchmark cases independent (disable caching when a case would reuse
    a prior case's work).

## Security

- No secrets, keys, or passwords in code or logs; never log tokens or PII.
- Secrets come from environment or `.env`; `.env` stays gitignored.
- Scrub author-specific absolute paths, hostnames, ports, container names, and model
  paths from any committed artifact; the checklist is `docs/process/PUBLICATION.md`.
  Documented product defaults in README/reference and exact quoted decisions are allowed.
- Never log prompts, completions, or key material into a shared artifact.

## Platform (Windows, PowerShell 5.1)

- Command chains use `; if ($?) { ... }`; PowerShell 5.1 has no `&&`.
- Write docs and source as UTF-8 without BOM; ASCII only in docs and scripts unless a test
  intentionally exercises multibyte input.
- Invoke `curl` as `curl.exe` (the alias is not curl); `netsh` emits UTF-16LE, so decode
  explicitly.
- Scripts must not assume `rg` is on PATH; use the editor search tools or `Select-String`.

## Dependency policy

- Do not add a crate without a concrete need; prefer the standard library and existing
  deps. A small crate that removes a large amount of correct code is worth adding, with a
  documented reason.
- Keep `Cargo.toml` comments: version constraints and a one-line why for every dependency.

## Pointers (do not duplicate)

- Gate and evidence: `docs/process/VERIFICATION.md`.
- Review method and finding closure: `docs/process/REVIEW.md`.
- Git, commit style, worktrees, release: `docs/process/GIT_WORKFLOW.md`.
- Binding invariants: `docs/architecture/DOCTRINE.md`. Full ledger:
  `docs/architecture/DECISIONS.md`.

## Appendix: Only if this stack is present

Conditional, not project mandates. Apply a block only when its stack actually exists in
this crate (`Cargo.toml`); otherwise skip it. Nothing here is a reason to introduce a stack.

### Web server (`axum` / `tower`)

- Handlers async, returning `Result<Response, AppError>` to centralize errors.
- Layered extractors and shared state structs; no global mutable state.
- `tower` middleware (timeouts, tracing, compression) for resilience and observability.
- Offload CPU-bound work to `tokio::task::spawn_blocking` so the reactor stays free.

### Web or WASM front end (for example `dioxus`)

- Deep computation stays in Rust (the WASM binary or the server process), never in
  JavaScript.
- Pico CSS plus vanilla JavaScript; no jQuery or component frameworks such as React.
- Speed and HID guidelines first; adaptive light/dark theme with a toggle.
- Modern typography with real heading and body fonts; never ship Pico defaults unchanged, a
  separate CSS file tailored to the app's semantics is preferred.
- Rebuild the WASM binary whenever underlying Rust changes.

### Terminal UI (`ratatui` / `crossterm`)

- Intuitive mouse controls; account for scroll offsets in click hit-testing.

### Data frames (for example `polars`)

- Use `polars` for tabular work; when printing a frame do not also print its count and
  schema; ingest small row subsets, not whole frames.

### Python / PyO3 (`maturin`, `uv`)

- Build with `maturin`, never `cargo build --features python`; manage the environment with
  `uv` and a gitignored `.venv`; add `ipykernel`/`ipywidgets` for notebooks outside package
  requirements.
- Python style: type hints on every signature, single responsibility, at most 5 params,
  early returns, no mutable default arguments, `T | None` for nullables, no `Any` unless
  unavoidable, `mypy` clean.

### Dropped or re-scoped from the previous AGENTS.md

- Standing mandates to use `ratatui`/`crossterm`, `polars`, `dioxus`/WASM, PyO3/`maturin`,
  and `uv`: dropped as mandates because nothing here uses those stacks; retained only as
  the conditional appendix blocks above.
- `indicatif`: dropped outright; this crate has no CLI progress UI, so no appendix block is
  warranted.
- `secrecy`: dropped as a named-crate mandate; its intent folded into the Security section
  as an env/`.env` rule.
- "Unit tests for all new functions": rescoped to behavior and risk to match the
  integration-heavy style, keeping the falsifier and no-disabled-tests rules.
