//! Virtual-table scaffold (spec 11 T1). It returns empty results and contains no `unsafe` code
//! yet; the crate is the sole `unsafe` exception (see `docs/developer/SAFETY.md`).

#![deny(unsafe_op_in_unsafe_fn)]
