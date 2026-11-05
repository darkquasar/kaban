---
name: rust-style-pi
description: Idiomatic Rust conventions for writing or reviewing Rust in this project (error handling, ownership, async/tokio, testing, lints, workspace/MSRV, unsafe policy, API naming, serde, observability), distilled from permissively licensed sources. Use when writing, reviewing or refactoring Rust code or Cargo manifests.
---

# Rust style (pi)

> In-repo copy (core-plan.md E1, owner decision OD-9) of the installed `rust-style-pi` skill,
> vendored unmodified apart from this note at M0 task C0.4 (source: `~/.pi/agent/skills/rust-style-pi/`,
> SKILL.md sha256 `996f129d9d91f0f7cb0233a59f74e2d3c3162c7de762085d769090e7697548e1`). Repository paths:
> `docs/developer/rust-style/SKILL.md`, `docs/developer/rust-style/references/RULES-INDEX.md`,
> `docs/developer/rust-style/references/rules/*.md`, `docs/developer/rust-style/references/microsoft-pragmatic-rust-guidelines.txt`.
> Every task brief's E1 read names rule files from `RULES-INDEX.md`; implementers list the files
> read with their hashes in the task report.
>
> **Recorded project conventions (OWN-73, 2026-10-26; historical context: `../specs/kaban/00-project/archive/PROTOCOL.md`):**
> Historical task grants are not fresh authority. Current repository/user instructions
> govern current work; see `docs/developer/CI.md` for current verification commands.
> reports name the rule files read in one line, no hashes; the test runner is `cargo nextest` (no `cargo test`
> fallback); the MSRV is the pinned toolchain in `rust-toolchain.toml` (1.99.0), not "stable minus 2"; the gate
> before claiming done is PROTOCOL.md §4 (crate-level in a lane, the workspace in the wave clean-room).

Thin index over vendored, MIT-licensed rule files. Read the rules you need from
`references/rules/` (one rule per file: why it matters, bad, good, related rules); see
`references/RULES-INDEX.md` for the one-line list, and
`references/microsoft-pragmatic-rust-guidelines.txt` for the longer Microsoft guidelines.
Resolve paths relative to this directory (`docs/developer/rust-style/`).

## Operating procedure

1. Model data with types first: make invalid states unrepresentable (enums, newtypes,
   builders); keep `pub` surfaces small and documented.
2. Choose ownership deliberately: borrow over clone, `&str`/`&[T]` in parameters, `Cow`
   when conditional, `Arc` only for shared ownership across threads/tasks (`own-*`).
3. Errors: `thiserror` enums in library crates, `anyhow` with `.context()` at the binary
   edge, `?` everywhere, no `unwrap`/`expect` in production paths except documented
   invariants (`err-*`).
4. Async: tokio; blocking work (SQLite, file parsing, embeddings) in `spawn_blocking` or a
   dedicated worker thread with bounded channels; never hold a lock or DB connection across
   `.await`; design for cancel safety (`async-*`, `conc-*`).
5. Observability: `tracing` with structured fields, writer = stderr for stdio servers,
   never log secrets or raw user content (`obs-*`).
6. Tests: unit + integration, `proptest` for invariants (e.g. redaction never leaks a hit),
   `insta` snapshots for serialized outputs, `criterion` for hot paths (`test-*`).
7. Gate before claiming done: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
   `cargo nextest run` (or `cargo test`), `cargo deny check` when a manifest changed.

## Non-negotiables for this project

- `unsafe_code = "forbid"` in every workspace crate; FFI stays inside upstream crates.
- Edition 2024, explicit `rust-version` (MSRV = stable minus 2), `[workspace.lints]` with a
  clippy pedantic subset; `cargo-deny` licence allowlist (deny GPL/AGPL) and advisories.
- Libraries expose `Result<T, CrateError>`; binaries convert to `anyhow`.
- Feature-gate optional engines and heavy deps (`sqlite` default, `ladybug`, `pdf`, `ocr`).
- stdout is the MCP protocol channel: nothing else may write to it.

## Routing

| Task | Read |
|---|---|
| error types, `?`, context | `references/rules/err-*.md` |
| borrowing, clones, lifetimes | `references/rules/own-*.md` |
| tokio, channels, cancellation, blocking | `references/rules/async-*.md`, `conc-*.md` |
| logging, tracing, redaction | `references/rules/obs-*.md` |
| tests, proptest, snapshots, benches | `references/rules/test-*.md` |
| clippy, rustfmt, CI lints | `references/rules/lint-*.md` |
| workspace, features, MSRV, layout | `references/rules/proj-*.md` |
| unsafe, FFI, Miri | `references/rules/unsafe-*.md` |
| public API design and naming | `references/rules/api-*.md`, `name-*.md` |
| serde, type design | `references/rules/serde-*.md`, `type-*.md` |
| broader guidance (Microsoft) | `references/microsoft-pragmatic-rust-guidelines.txt` |

## Attribution

Rules vendored from `leonardomso/rust-skills` v1.5.1 (MIT, commit
fd2a861ab0406a4ac536a55274d14ea6fd1ca9c9, retrieved 2026-10-26; see
`references/LICENSE-rust-skills`), curated to 13 of 26 categories. Microsoft Pragmatic
Rust Guidelines (MIT; `references/LICENSE-microsoft-rust-guidelines`). The operating
procedure and project rules above are original; structure inspired by common agent skill
layouts. See `SOURCE.md`.
