# Safety notes

Every workspace member forbids `unsafe` code (`unsafe_code = "forbid"` in
`[workspace.lints.rust]`), with one listed exception.

## The exception: `kaban-vtab`

`crates/kaban-vtab` is the only crate that may contain `unsafe` code (spec 11 QRY-07, OD-7).

**Why.** SQLite virtual tables and SQLite's allocator hooks are C interfaces. rusqlite's `vtab`
module exposes the first as `unsafe` traits and raw pointers, and installing request-scoped
allocation hooks goes through SQLite's raw configuration interface, so neither can be written in
safe Rust.

**What it may contain.** Two things and nothing else (PC-17, QRY-07):

- The virtual-table glue: the module implementation, cursor and row-production code.
- The allocator adapter: hooks that charge request-scoped SQLite allocations to a `QueryBudget`,
  the unsafe embedding initializer that installs them, and the process-level bootstrap behind the
  `process-bootstrap` feature, which installs them for the standalone CLI.

It does no I/O of its own, parses no SQL and holds no policy. Every `unsafe` block carries a
`SAFETY:` comment naming the invariant it relies on, and the crate carries
`#![deny(unsafe_op_in_unsafe_fn)]` so an `unsafe fn` body still needs explicit blocks.

**Allocator safety contract.** T12 implements the adapter against these obligations and lists
each `unsafe` block here with the one it meets:

- Exclusive initialization. The hooks are installed once, before any SQLite use and before other
  threads start, with no concurrent external SQLite initialization and no allocator replacement
  or shutdown while the runtime lives. A `OnceLock` coordinates kaban only; it is not proof that
  the host is excluded. The standalone CLI meets this through the adapter-owned pre-main
  bootstrap; unsupported constructor or linkage ordering refuses startup.
- Early-host refusal. If SQLite was initialized before the adapter, initialization returns a named
  error and never forces a shutdown and retry. Safe APIs consume an installed runtime and never
  install it lazily.
- Allocation ownership. The wrapper forwards to the original hooks and honours `xMalloc`,
  `xRealloc`, `xFree`, `xSize`, `xRoundup`, alignment and overhead. Each allocation keeps the
  budget it was charged to through `realloc` and `free`, including a free on another thread or
  after the query ends; a failed grow keeps the old block and its charge.
- No unwinding across FFI. Callbacks never unwind into C and never call back into SQLite.

**What exists today (T1).** The crate is a scaffold with no `unsafe` code: the `process-bootstrap`
feature is empty and nothing is installed. `QueryBudget` itself is safe code in `kaban-core`.

**How it is held to this.**

- The manifest does not inherit the workspace lint table. It restates every workspace lint except
  `unsafe_code` (Cargo has no `workspace = false` for `[lints]`, so the key is omitted and the
  `[lints.*]` tables stand alone).
- `crates/kaban/tests/lints.rs` (`no_unsafe_in_new_crates`, `unsafe_scan_covers_every_member_except_vtab`)
  scans every other member's sources and fails if the scanned set is not exactly the workspace
  members minus `crates/kaban-vtab/`.
- `crates/kaban/tests/sql_boundary.rs` lists `crates/kaban-vtab/` among the crates that may carry
  SQL; no other new crate may.
- `scripts/ci/ci.sh` (`store_boundary_deps`) accepts `kaban-vtab` as a direct parent of `rusqlite`
  beside `kaban-store-sqlite` and `kaban-codegraph`, and no other.

## Dependencies

`unsafe` inside a mature dependency is allowed case by case, with a licence and audit note in the
manifest task that adds it. File mapping goes through tantivy's utility only; no second mmap
crate is added (OD-7).
