//! Command-line definitions of the `kaban-index` binary (stage1-spec G1-F7; stage1-plan CG4.4,
//! CG4.6): the user-facing verbs `index`, `gc`, `status`, `purge-derived`, `doctor`, `trust`,
//! `untrust`, and the hidden `ns-helper` subcommand that kaban builds itself and parses back
//! (its arguments are the [`View`] the parent built and the parent's namespace inodes; the
//! parent spells them with [`View::helper_args`] and the helper reads them back here, so
//! nothing user-controlled reaches the helper's argv unscanned). There is no `--keep-run-dir`:
//! raw child files are always deleted (G1-S5.6, RS2-7), and clap rejects the flag as unknown.

use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use kaban_core::config::Isolation;

use crate::detect::Lang;
use crate::isolate::namespaces::{
    CacheMount, Expect, HelperMode, HelperOpts, NsInodes, Overlay, View,
};

/// Command line of `kaban-index` (G1-F7).
#[derive(Debug, Parser)]
#[command(
    name = "kaban-index",
    version,
    about = "kaban-index: explicit, gated code indexing into per-commit snapshots (v4 stage 1)"
)]
pub struct Cli {
    /// Policy file (builtin rules only when absent).
    #[arg(long, global = true)]
    pub policy: Option<PathBuf>,
    /// The verb; none prints the scaffold report.
    #[command(subcommand)]
    pub verb: Option<Verb>,
}

/// The verbs of G1-F7.
#[derive(Debug, Subcommand)]
pub enum Verb {
    /// Index the checked-out commit into `<commit40>.cg.db` and print the JSON report.
    Index(IndexArgs),
    /// Keep the snapshot of HEAD plus the N most recent; collect the rest.
    Gc(GcArgs),
    /// Trust state, live repository path, identity validation, provenance of every snapshot.
    Status {
        /// Repository path (default: the working directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Delete every derived file of the repository (snapshots, logs, markers, scratch).
    #[command(name = "purge-derived")]
    PurgeDerived {
        /// Repository path (default: the working directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Record the trust mark for a repository (G1-S6); prints the fixed trust text.
    Trust {
        /// Repository path (default: the working directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Remove the trust mark of a repository.
    Untrust {
        /// Repository path (default: the working directory).
        #[arg(long)]
        repo: Option<PathBuf>,
    },
    /// Configuration in effect, overrides, languages, indexers, tiers, trust and platform.
    Doctor {
        /// Repository to inspect (optional).
        #[arg(long)]
        repo: Option<PathBuf>,
        /// `<lang>=<absolute path>`: use this indexer for the language.
        #[arg(long = "indexer")]
        indexer: Vec<String>,
    },
    /// The namespaces-tier helper (hidden): started by `unshare` inside the new namespaces,
    /// builds the view, verifies it and `exec`s the indexer (CG4.4, G1-S5.2).
    #[command(hide = true, name = "ns-helper")]
    NsHelper(NsHelperArgs),
    /// Internal descriptor-bound cwd handoff.
    #[command(hide = true, name = "cwd-helper")]
    CwdHelper(CwdHelperArgs),
}

/// The hidden native child helper. Identities are passed by the admitted parent.
#[derive(Debug, Args)]
pub struct CwdHelperArgs {
    /// Expected directory dev,ino.
    #[arg(long)]
    pub directory: String,
    /// Expected executable `dev,ino,size,ctime_sec,ctime_nsec`.
    #[arg(long)]
    pub identity: String,
    /// Canonical executable and its arguments.
    #[arg(last = true, required = true)]
    pub exec: Vec<OsString>,
}

/// `--isolation preferred|required|off` (OWN-29): overrides `index.isolation` for one run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum IsolationArg {
    /// The strongest available tier, else `none` with the warning.
    Preferred,
    /// Refuse (exit 8) without an isolating tier.
    Required,
    /// `none` without probing.
    Off,
}

impl From<IsolationArg> for Isolation {
    fn from(a: IsolationArg) -> Self {
        match a {
            IsolationArg::Preferred => Self::Preferred,
            IsolationArg::Required => Self::Required,
            IsolationArg::Off => Self::Off,
        }
    }
}

/// `kaban-index index [--repo P] [--commit SHA|HEAD] [--languages go,ts] [--timeout DUR]
/// [--indexer LANG=PATH]* [--partial] [--isolation preferred|required|off]`.
#[derive(Debug, Args)]
pub struct IndexArgs {
    /// Repository path (default: the working directory).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// The commit to index; must equal the checked-out HEAD (default `HEAD`).
    #[arg(long, default_value = "HEAD")]
    pub commit: String,
    /// The languages to index (`go`, `ts`/`typescript`, `rust`); default: detected.
    #[arg(long, alias = "lang", value_delimiter = ',', value_parser = parse_lang)]
    pub languages: Vec<Lang>,
    /// Hard deadline per indexer run (`600`, `600s`, `10m`, `1h`); overrides
    /// `index.deadline_secs`.
    #[arg(long, value_parser = parse_duration)]
    pub timeout: Option<Duration>,
    /// `<lang>=<absolute path>`: use this indexer for the language.
    #[arg(long = "indexer")]
    pub indexer: Vec<String>,
    /// Write the snapshot from the languages that succeeded when another failed.
    #[arg(long)]
    pub partial: bool,
    /// Override `index.isolation` for this run.
    #[arg(long, value_enum)]
    pub isolation: Option<IsolationArg>,
}

/// The `gc --keep` default: snapshots kept besides HEAD's. `index` applies the same retention
/// after a successful write.
pub const GC_KEEP_DEFAULT: usize = 5;

/// `kaban-index gc [--repo P] [--keep N] [--dry-run] [--prune-corrupt] [--stale-policy]`.
#[derive(Debug, Args)]
pub struct GcArgs {
    /// Repository path (default: the working directory).
    #[arg(long)]
    pub repo: Option<PathBuf>,
    /// Snapshots kept besides HEAD's, most recent by commit date first (default 5).
    #[arg(long, default_value_t = GC_KEEP_DEFAULT)]
    pub keep: usize,
    /// Print the plan, delete nothing.
    #[arg(long)]
    pub dry_run: bool,
    /// Also delete snapshots that fail `PRAGMA quick_check` or have no readable `meta`.
    #[arg(long)]
    pub prune_corrupt: bool,
    /// Also delete snapshots whose `policy_version` is not the current policy's.
    #[arg(long)]
    pub stale_policy: bool,
}

/// `go`, `typescript` or its short form `ts`.
fn parse_lang(s: &str) -> Result<Lang, String> {
    match s {
        "ts" => Ok(Lang::TypeScript),
        other => Lang::parse(other).ok_or_else(|| "expected go, ts or rust".to_owned()),
    }
}

/// Seconds, or a number with the suffix `s`, `m` or `h`; never zero.
///
/// # Errors
/// A fixed text (the value is not echoed).
pub fn parse_duration(s: &str) -> Result<Duration, String> {
    let (digits, unit) = match s.char_indices().find(|(_, c)| !c.is_ascii_digit()) {
        Some((i, _)) => s.split_at(i),
        None => (s, "s"),
    };
    let n: u64 = digits
        .parse()
        .map_err(|_| "expected <n>[s|m|h]".to_owned())?;
    let secs = match unit {
        "s" => n,
        "m" => n.checked_mul(60).ok_or("timeout too large")?,
        "h" => n.checked_mul(3600).ok_or("timeout too large")?,
        _ => return Err("expected <n>[s|m|h]".to_owned()),
    };
    if secs == 0 {
        return Err("timeout must be positive".to_owned());
    }
    Ok(Duration::from_secs(secs))
}

/// `kaban-index ns-helper [--probe-only] --parent-ns u,n,m --checkout C --scratch S
/// --overlay overlayfs|none [--ro P]* [--cache name=lower]* [-- <indexer> <args>...]`
/// (hidden; started by `unshare` inside the new namespaces, never by a user).
#[derive(Debug, Args)]
pub struct NsHelperArgs {
    /// Availability probe: build the view, self-check, print `overlay=<x>`, exit 0.
    #[arg(long)]
    pub probe_only: bool,
    /// The parent's `/proc/self/ns/{user,net,mnt}` inodes, comma separated.
    #[arg(long, value_name = "USER,NET,MNT")]
    pub parent_ns: String,
    /// The checkout (read-only; the indexer's working directory).
    #[arg(long)]
    pub checkout: PathBuf,
    /// The host scratch directory (`/scratch` inside).
    #[arg(long)]
    pub scratch: PathBuf,
    /// `overlayfs` or `none` (the probe's finding; ignored under `--probe-only`).
    #[arg(long, default_value = "none")]
    pub overlay: String,
    /// A read-only bind (host path == view path); repeatable.
    #[arg(long = "ro", value_name = "PATH")]
    pub ro: Vec<PathBuf>,
    /// A shared read-only cache `name=<lower>`; repeatable.
    #[arg(long = "cache", value_name = "NAME=LOWER")]
    pub cache: Vec<String>,
    /// The `(dev, ino)` the checkout must have before it is bound (U2 RU2-5).
    #[arg(long, value_name = "DEV,INO")]
    pub expect_checkout: Option<String>,
    /// The `(dev, ino)` the indexer must have immediately before the exec (U2 RU2-3).
    #[arg(long, value_name = "DEV,INO")]
    pub expect_indexer: Option<String>,
    /// The indexer and its arguments (after `--`).
    #[arg(last = true)]
    pub exec: Vec<OsString>,
}

/// Why the helper arguments did not parse (kaban built them: a mismatch is a bug, not input).
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum HelperArgsError {
    /// `--parent-ns` is not three integers.
    #[error("parent-ns malformed")]
    ParentNs,
    /// `--overlay` is not `overlayfs` or `none`.
    #[error("overlay malformed")]
    Overlay,
    /// A `--cache` value has no `=`.
    #[error("cache malformed")]
    Cache,
    /// No indexer after `--` in run mode.
    #[error("indexer missing")]
    Indexer,
    /// An `--expect-*` value is not `dev,ino`.
    #[error("expect malformed")]
    Expect,
}

/// `dev,ino`.
fn parse_dev_ino(s: &str) -> Option<(u64, u64)> {
    let (dev, ino) = s.split_once(',')?;
    Some((dev.parse().ok()?, ino.parse().ok()?))
}

impl NsHelperArgs {
    /// Turn the parsed arguments into the helper's options.
    ///
    /// # Errors
    /// [`HelperArgsError`].
    pub fn into_opts(self) -> Result<HelperOpts, HelperArgsError> {
        let parent = NsInodes::parse(&self.parent_ns).ok_or(HelperArgsError::ParentNs)?;
        let caches = self
            .cache
            .iter()
            .map(|c| {
                c.split_once('=')
                    .map(|(name, lower)| CacheMount {
                        name: name.to_owned(),
                        lower: PathBuf::from(lower),
                    })
                    .ok_or(HelperArgsError::Cache)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let view = View {
            checkout: self.checkout,
            scratch: self.scratch,
            ro: self.ro,
            caches,
        };
        let mode = if self.probe_only {
            HelperMode::Probe
        } else {
            let overlay = Overlay::parse(&self.overlay).ok_or(HelperArgsError::Overlay)?;
            let mut exec = self.exec.into_iter();
            let indexer = exec.next().ok_or(HelperArgsError::Indexer)?;
            HelperMode::Run {
                overlay,
                indexer: PathBuf::from(indexer),
                args: exec.collect(),
            }
        };
        let expect = Expect {
            checkout: self
                .expect_checkout
                .as_deref()
                .map(|v| parse_dev_ino(v).ok_or(HelperArgsError::Expect))
                .transpose()?,
            indexer: self
                .expect_indexer
                .as_deref()
                .map(|v| parse_dev_ino(v).ok_or(HelperArgsError::Expect))
                .transpose()?,
        };
        Ok(HelperOpts {
            parent,
            view,
            mode,
            expect,
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)] // test code: failures are the assertion

    use super::*;

    #[test]
    fn durations_parse_with_units_and_refuse_zero() {
        assert_eq!(parse_duration("600").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2s").unwrap(), Duration::from_secs(2));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("0").is_err());
        assert!(parse_duration("5d").is_err());
        assert!(parse_duration("abc").is_err());
    }

    #[test]
    fn languages_accept_the_short_typescript_form() {
        assert_eq!(parse_lang("go").unwrap(), Lang::Go);
        assert_eq!(parse_lang("ts").unwrap(), Lang::TypeScript);
        assert_eq!(parse_lang("typescript").unwrap(), Lang::TypeScript);
        assert_eq!(parse_lang("rust").unwrap(), Lang::Rust);
        assert!(parse_lang("cobol").is_err());
    }
}
