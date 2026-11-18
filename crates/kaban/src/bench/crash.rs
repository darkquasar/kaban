//! `kaban bench crash` (spec 11 T6 Step 6, KER-02): kill a child at a named crash point of the
//! catalog `--kills` times, recover and verify after each kill, one JSONL line per kill.
//!
//! Not delivered: the child needs the store's `failpoints` feature and its test index class,
//! neither of which a `kaban` build enables (a manifest change this step does not own).

use std::path::PathBuf;
use std::process::ExitCode;

/// The catalog phase a kill lands in.
#[derive(Clone, Copy, clap::ValueEnum)]
pub(crate) enum Phase {
    /// A publication (`publish.after_append`, `publish.before_commit`)
    Publish,
    /// A seal (`seal.mid`)
    Seal,
    /// A fold (`fold.mid`)
    Fold,
    /// A reclamation (`reclaim.after_floor`, `reclaim.mid_punch`)
    Reclaim,
}

/// `bench crash` arguments.
#[derive(clap::Args)]
pub(crate) struct Args {
    /// How many children to kill
    #[arg(long, default_value_t = 250)]
    kills: u32,
    /// Phases to kill in, comma-separated (default: every phase, in turn)
    #[arg(long, value_enum, value_delimiter = ',')]
    phase: Vec<Phase>,
    /// The JSONL file one line per kill is appended to
    #[arg(long, default_value = "crash.jsonl")]
    out: PathBuf,
}

/// Not delivered yet: refused with exit 4.
pub(crate) fn run(args: &Args) -> ExitCode {
    let _ = (args.kills, &args.phase, &args.out);
    super::not_delivered("crash")
}
