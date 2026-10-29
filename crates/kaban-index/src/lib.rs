//! kaban v4 code indexer library (stage1-spec §3, G1-S1; stage1-plan GM3).
//!
//! CG3.1 ships the decode-time gate: `scip_read` decodes an `index.scip` and walks every
//! string-typed field of the pinned SCIP schema through the repository policy into a
//! [`GatedIndex`]; `gate` is the admission policy over gated values (structural, free and meta
//! fields with the fixed warning classes `policy_skipped_row`, `policy_dropped`,
//! `policy_redacted_meta`). Derivation and the snapshot writer (CG3.2, CG3.3) receive an `Admitted`
//! index only: the raw `scip::types::Index` never leaves `scip_read.rs`, so the type system
//! enforces RS2-1's order (gate first, grammar and derivation after).
//!
//! ```text
//!   index.scip bytes ==> scip_read::decode_and_gate(bytes, &Policy) ==> GatedIndex
//!                                   |  INVENTORY (every string field, with its FieldClass)
//!                                   v
//!   gate::admit(class, &Gated) -> Admission     gate::admit_index(&GatedIndex) -> Admitted
//!   (per value)                                 (rows that survive, relationships, counts)
//!                                   |
//!   union::implements(A, B?) ==> derive::nodes / structural_edges ==> writer::Rows
//!   (G1-F3)                        (rows, CONTAINS, DEFINED_IN, IMPLEMENTS*, TYPE_OF)
//!                                   |
//!   writer::publish(dir, &rows, &meta) ==> <commit40>.cg.db   (§4.1 file protocol, G1-N5)
//!   ranges: typed over legacy SCIP ranges, folded into meta.scip_range_form
//!
//!   identity_cli (git through the runner, key, gate, repo_id, admitted <repo-id> dir)
//!   trust (the 0600 O_EXCL record, revalidate, untrust)              (CG4.1, G1-S6)
//!   detect (markers, indexer resolution), doctor (config in effect, overrides, probes) (CG4.2)
//!   isolate (tiers, probes, select, wrapper handles, Scratch, env_for), quota (monitors),
//!   runner (run_indexer: the none tier end to end, gated log)                    (CG4.3)
//!   isolate::namespaces (the view, ns-helper, the probe), cli (the helper's argv)  (CG4.4)
//!   language (the fixed drivers: admit, caches, prepare; Rust-only logic in language::rust)
//!   cli (the user-facing verbs), gc (keep/collect plan, corrupt and stale checks, sweep),
//!   status (trust state, snapshot provenance, unsynced re-verification, purge); the
//!   `index` pipeline itself is the binary's (`main.rs`)                            (CG4.6)
//! ```
//!
//! The RS2-1 boundary is enforced by the type system (review unit U1, RU1-2): a `GatedIndex`
//! can only come out of `decode_and_gate` (private fields, read through accessors), a
//! `Gated::Pass` or `Gated::Redacted` can only be made by the gate (its payload `Scanned` has a
//! private field), an `Admitted` index and its rows are read-only outside the crate, and `Rows`
//! is built only by `Rows::derive` and consumed by `publish`.
//!
//! ```compile_fail,E0451
//! let g = kaban_index::GatedIndex {
//!     metadata: unimplemented!(),
//!     documents: Vec::new(),
//!     external_symbols: Vec::new(),
//! };
//! ```
//!
//! ```compile_fail,E0451
//! let passed = kaban_index::Gated::Pass(kaban_index::scip_read::Scanned("x".to_owned()));
//! ```
//!
//! ```compile_fail,E0616
//! fn documents(g: &mut kaban_index::GatedIndex) {
//!     g.documents.clear();
//! }
//! ```
//!
//! ```compile_fail,E0616
//! fn files<'a>(a: &kaban_index::Admitted<'a>) -> usize {
//!     a.files.len()
//! }
//! ```
//!
//! ```compile_fail,E0616
//! fn edges(rows: &mut kaban_index::writer::Rows) {
//!     rows.edges.clear();
//! }
//! ```

pub mod cli;
pub mod derive;
pub mod detect;
pub mod doctor;
pub mod error;
pub mod gate;
pub mod gc;
pub mod ident_terms;
pub mod identity_cli;
pub mod isolate;
pub mod language;
pub mod quota;
pub mod ranges;
pub mod runner;
pub mod scip_read;
pub mod snippets;
pub mod status;
pub mod tools;
pub mod trust;
pub mod union;
pub mod writer;

pub use error::IndexError;
pub use gate::{Admission, Admitted, FieldClass, GateCounts, WarningClass, admit, admit_index};
pub use scip_read::{Gated, GatedIndex, INVENTORY, RelationshipFlags, decode_and_gate};
