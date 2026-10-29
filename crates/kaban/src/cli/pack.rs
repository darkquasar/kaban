//! `kaban pack build | verify | fork` (spec 05 SS-18, SS-19, SS-20): the CLI side of
//! `kaban_store_sqlite::pack`. This module resolves the base commit (locally, from the
//! enclosing or `--repo` repository through [`GitRunner`], or an explicit full `--base`;
//! never from a remote), admits the base code-graph snapshot through the same provider wiring
//! `serve` uses ([`crate::codegraph::wire`] and `kaban_codegraph`'s admission) and prints the
//! manifest as the one JSON document. The store builds, stages, verifies and finalizes.
//!
//! Exit codes: 0 ok; 4 a refused argument (no base, a short base, a destination that exists);
//! 5 a busy source, a store failure, a pack that does not verify or a fork that could not be
//! initialized.

use crate::Cli;
use crate::cli::output::{EXIT_INVALID, EXIT_STORE_ERROR, Format, print_failure, print_json};
use kaban_api::GitRunner;
use kaban_codegraph::{CodeGraphReader, SqliteSnapshot};
use kaban_store_sqlite::pack::{
    self, Admission, BuildRequest, Manifest, PackError, SnapshotSource,
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// The `pack` subcommands.
#[derive(Debug, clap::Subcommand)]
pub(crate) enum PackCmd {
    /// Build an immutable pack of the database at --db into DIR (created; must not exist):
    /// a consistent snapshot taken under the maintenance window, the admitted code-graph
    /// snapshot of the base commit when present, and the manifest. Exit 5 when another
    /// writer holds the database (busy)
    Build {
        /// The pack directory to create
        dir: PathBuf,
        /// The full 40-hex base commit (default: HEAD of the enclosing repository, resolved
        /// locally; never guessed from a remote)
        #[arg(long)]
        base: Option<String>,
        /// Repository root (default: the git repository enclosing the current directory)
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Check the pack at DIR without writing to it: hashes, supported versions, SQLite
    /// integrity, declared snapshots and the manifest's consistency. Exit 5 when it fails
    Verify {
        /// The pack directory
        dir: PathBuf,
    },
    /// Fork the pack at PACK into the writable database DEST (must not exist; spec 05 SS-20):
    /// an ordinary copy (a reflink when the filesystem offers one, never a hard link) that
    /// gets a new instance id, fresh leases and signing key, the pack's lineage, record UUIDs
    /// and base commit, the pack as its parent, and an audit genesis binding the parent.
    /// Exit 4 when DEST exists, 5 when the pack does not verify or the copy fails
    Fork {
        /// The pack directory (its id differs from the global `--pack`, whose value clap
        /// would otherwise fill from this positional)
        #[arg(id = "source", value_name = "PACK")]
        source: PathBuf,
        /// The database file to create
        dest: PathBuf,
    },
}

/// Run a `pack` subcommand.
pub(crate) fn run(
    cli: &Cli,
    policy: &kaban_policy::Policy,
    keys_dir: &Path,
    cmd: PackCmd,
    format: Format,
) -> ExitCode {
    match cmd {
        PackCmd::Build { dir, base, repo } => {
            build(cli, policy, keys_dir, &dir, base, repo, format)
        }
        PackCmd::Verify { dir } => verify(&dir, format),
        PackCmd::Fork { source, dest } => fork(&source, &dest, keys_dir, format),
    }
}

/// The repository the base is resolved in: `--repo`, else the one enclosing the current
/// directory.
fn repository(repo: Option<PathBuf>) -> Option<PathBuf> {
    repo.or_else(|| {
        std::env::current_dir()
            .ok()
            .and_then(|cwd| crate::repo_root(&cwd))
    })
}

fn build(
    cli: &Cli,
    policy: &kaban_policy::Policy,
    keys_dir: &Path,
    dir: &Path,
    base: Option<String>,
    repo: Option<PathBuf>,
    format: Format,
) -> ExitCode {
    let git = GitRunner::default();
    let root = repository(repo);
    let head = root
        .as_deref()
        .and_then(|root| git.head(root))
        .filter(|head| pack::is_full_commit(head));
    let base = match base {
        Some(base) if pack::is_full_commit(&base) => base,
        Some(_) => {
            print_failure(&"pack build: --base must be a full 40-hex commit id");
            return ExitCode::from(EXIT_INVALID);
        }
        None => {
            let Some(head) = head.clone() else {
                print_failure(
                    &"pack build: no base commit (run inside a repository or pass --base <40-hex>)",
                );
                return ExitCode::from(EXIT_INVALID);
            };
            head
        }
    };
    // the base snapshot, when the provider wiring finds one and admits it (SS-18)
    let Some(home) = keys_dir.parent() else {
        print_failure(&"pack build: the key directory has no parent");
        return ExitCode::from(EXIT_INVALID);
    };
    let wiring = match crate::codegraph::wire(home, root.as_deref(), policy, &git) {
        Ok(wiring) => wiring,
        Err(e) => {
            print_failure(&e);
            return ExitCode::from(EXIT_INVALID);
        }
    };
    let mut snapshots = Vec::new();
    if let Some(wiring) = wiring {
        let candidate = wiring.dir.join(format!("{base}.cg.db"));
        if candidate.symlink_metadata().is_ok() {
            match admitted_commit(&candidate) {
                Ok(commit) if commit == base => snapshots.push(SnapshotSource {
                    commit,
                    path: candidate,
                }),
                Ok(_) => {
                    print_failure(&PackError::BaseMismatch);
                    return ExitCode::from(EXIT_STORE_ERROR);
                }
                // a present but unadmitted file is not part of the pack, as discovery skips it
                Err(class) => {
                    tracing::debug!(class = %class, "base snapshot not admitted; pack built without it");
                }
            }
        }
    }
    // the index component of an active Tantivy deployment (SS-18): the store over a
    // diagnostic open (nothing created or migrated) with its index attached as it exists,
    // whose projection the builder holds and drains; a store without one needs none
    let component = match text_index_component(cli, keys_dir) {
        Ok(component) => component,
        Err(code) => return code,
    };
    let index = component.text_index_component();
    let request = BuildRequest {
        db: &cli.db,
        dest: dir,
        base_commit: &base,
        source_commit: head,
        policy_version: policy.effective_hash().to_owned(),
        snapshots: &snapshots,
        admit: ADMISSION,
        index,
    };
    match pack::build(&request) {
        Ok(manifest) => print_manifest(&manifest, format),
        Err(e) => failure("pack build", &e),
    }
}

/// The store whose index the build captures: a diagnostic open (the index attached as it
/// exists, nothing created). A failure is printed and mapped (config 4, store 5).
fn text_index_component(
    cli: &Cli,
    keys_dir: &Path,
) -> Result<kaban_store_sqlite::SqliteStore, ExitCode> {
    // the engine configuration is validated like every command's (config errors are exit 4)
    crate::load_engine().map_err(|error| {
        error.report(&cli.db);
        ExitCode::from(EXIT_INVALID)
    })?;
    kaban_store_sqlite::SqliteStore::open_diagnostic(&cli.db, keys_dir, &cli.actor_id)
        .map(|(store, _modes)| store)
        .map_err(|e| {
            print_failure(&format_args!("pack build: {e}"));
            ExitCode::from(EXIT_STORE_ERROR)
        })
}

/// The store checks presence, hashes and the snapshot's consistency and runs
/// [`ADMISSION`] on each declared code-graph snapshot and on the declared index directory
/// (SS-19): one admission path for `pack verify`, `--pack` reads and `pack fork` (review e-5).
fn verify(dir: &Path, format: Format) -> ExitCode {
    match pack::verify(dir, ADMISSION) {
        Ok(manifest) => print_manifest(&manifest, format),
        Err(e) => failure("pack verify", &e),
    }
}

/// The binary's [`pack::Admission`]: [`admitted_commit`] for the code-graph snapshots and
/// [`admitted_index`] for the Tantivy component.
pub(crate) const ADMISSION: Admission = Admission {
    snapshot: admitted_commit,
    index: admitted_index,
};

/// The binary's [`pack::SnapshotAdmission`]: admit the snapshot at `path` through
/// `kaban-codegraph` and read its `meta.commit`; the failure is the admission class.
pub(crate) fn admitted_commit(path: &Path) -> Result<String, String> {
    let snapshot = SqliteSnapshot::open_ro(path).map_err(|e| e.to_string())?;
    let meta = snapshot.meta().map_err(|e| e.to_string())?;
    meta.get("commit")
        .cloned()
        .ok_or_else(|| "meta.commit absent".to_owned())
}

/// The binary's [`pack::IndexAdmission`]: the adapter's read-only verifier of a captured
/// directory (feature `text-index-tantivy`, T4 review t4-5); the failure is the admission
/// class.
fn admitted_index(path: &Path) -> Result<serde_json::Value, String> {
    kaban_index_tantivy::TantivyIndex::admit_captured(path).map_err(|e| e.to_string())
}

fn fork(pack_dir: &Path, dest: &Path, keys_dir: &Path, format: Format) -> ExitCode {
    match pack::fork(pack_dir, dest, keys_dir, ADMISSION) {
        Ok(report) => match print_json(&report.to_value(), format) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                print_failure(&e);
                ExitCode::from(crate::cli::output::EXIT_FAILURE)
            }
        },
        Err(e) => failure("pack fork", &e),
    }
}

fn print_manifest(manifest: &Manifest, format: Format) -> ExitCode {
    match print_json(&manifest.to_value(), format) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            print_failure(&e);
            ExitCode::from(crate::cli::output::EXIT_FAILURE)
        }
    }
}

/// Print `<what>: <error>` and map it: refused arguments are 4, everything else 5.
fn failure(what: &str, e: &PackError) -> ExitCode {
    print_failure(&format_args!("{what}: {e}"));
    ExitCode::from(match e {
        PackError::InvalidCommit | PackError::Destination => EXIT_INVALID,
        _ => EXIT_STORE_ERROR,
    })
}
