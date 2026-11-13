# Safety notes

Every workspace member forbids `unsafe` code (`unsafe_code = "forbid"` in
`[workspace.lints.rust]`), with one listed exception.

## The exception: `kaban-vtab`

`crates/kaban-vtab` is the only crate that may contain `unsafe` code (spec 11 QRY-07, OD-7).

**Why.** SQLite virtual tables are defined through a C interface. rusqlite's `vtab` module
exposes that interface as `unsafe` traits and raw pointers, so the glue that registers kaban's
table functions cannot be written in safe Rust.

**What it may contain.** Only the virtual-table glue: the module implementation, cursor and
row-production code and the process-level bootstrap behind the `process-bootstrap` feature (T12).
It does no I/O of its own, parses no SQL and holds no policy. Every `unsafe` block carries a
`SAFETY:` comment naming the invariant it relies on, and the crate carries
`#![deny(unsafe_op_in_unsafe_fn)]` so an `unsafe fn` body still needs explicit blocks.

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
crate is added (OD-7). The `kaban-vtab` crate has no `unsafe` code yet.
