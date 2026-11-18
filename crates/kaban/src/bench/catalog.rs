//! `kaban bench catalog` (spec 11 T6 Step 6, KER-04, KER-06, OD-5): writers and a merge thread
//! for `--secs`, measuring the publish lock hold, the merge/seal switch hold and the WAL
//! high-water mark (informational), written as JSONL.
//!
//! Not delivered: a seal or fold needs an index class that writes extents, and until T7 and
//! T11 only the store's test class (behind its `failpoints` feature, not enabled by any
//! `kaban` build) does.

use std::path::PathBuf;
use std::process::ExitCode;

/// `bench catalog` arguments.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// How long to run, in seconds
    #[arg(long, default_value_t = 60)]
    secs: u64,
    /// Concurrent writer counts to run, as a range `MIN..MAX` (each count is one run)
    #[arg(long, default_value = "1..4")]
    writers: String,
    /// Run the merge thread beside the writers (`on` or `off`)
    #[arg(long, default_value = "on")]
    merge_thread: String,
    /// The JSONL output file
    #[arg(long, default_value = "catalog.jsonl")]
    out: PathBuf,
}

/// Not delivered yet: refused with exit 4.
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (args.secs, &args.writers, &args.merge_thread, &args.out);
    super::not_delivered("catalog")
}
