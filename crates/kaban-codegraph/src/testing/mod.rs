//! Test support behind the `test-support` feature (cg-join-v1 §4; stage1-plan CG5.2, CG7.3):
//! the [`SnapshotProvider`](crate::provider::SnapshotProvider) over one file that the engine's
//! and the adapters' tests inject in place of `DiscoverProvider`, and the synthetic bench
//! [`corpus`] generator `kaban bench cg gen` writes through (feature `bench` of the `kaban`
//! binary). Production builds never enable the feature; nothing here is reachable from
//! `kaban serve`.

pub mod corpus;
mod fixed_snapshot;

pub use fixed_snapshot::FixedSnapshot;
