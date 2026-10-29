//! `kaban verify-audit`: recompute the database's audit chain with the key from
//! `$KABAN_HOME/keys/<db-id>.key` (never created here) and print the result as JSON.
//! Exit 0 when the chain is intact, 5 when a row fails or when the database or key cannot be
//! opened (M4-2: open failures are store errors). Failures print the store error's own message
//! (a refused newer schema names its version; a driver failure is the fixed "storage backend
//! error"), never its source chain (review RM-1).
//!
//! A mode-A fork (spec 05 SS-20) has two chains in one table and the document says so:
//! `chain: child` with the fork genesis's binding under `fork`, this database's own chain
//! verified from that genesis under its own key (`ok`, `entries`, `first_bad_seq`), and the
//! parent's history under `parent_history`, verified under the parent instance's key when
//! that key is in the key directory (`key: present`) and reported unverified otherwise
//! (`key: absent`, `ok: null`); only a key file that does not exist is absent, a key that is
//! there but cannot be read or has the wrong length is a verification failure (review e-8).
//! A database that is not a fork is `chain: origin`. Nothing here claims one global chain; a
//! failing segment of either kind exits 5.
//!
//! `--pack <dir> verify-audit` ([`run_pack`], review e-3) verifies the pack's chain: the pack
//! is opened immutable, its chain recomputed under the pack instance's key, and the document
//! names the pack and the head its manifest declares under `pack`.

use crate::cli::output::{
    EXIT_FAILURE, EXIT_STORE_ERROR, Format, print_failure, print_json, store_exit_code,
};
use kaban_core::{ActorId, AuditStore, AuditVerification};
use kaban_policy::{AuditError, KeySigner};
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};
use std::path::Path;
use std::process::ExitCode;

/// Exit code for a chain that does not verify.
pub(crate) const EXIT_CHAIN_BROKEN: u8 = EXIT_STORE_ERROR;

/// Verify the chain of the database at `db` with the key under `keys_dir` (the key must
/// already exist) and print the result; `actor` is the process's writer, as for every open.
pub(crate) fn run(db: &Path, keys_dir: &Path, actor: &ActorId, format: Format) -> ExitCode {
    let store = match SqliteStore::open_existing_key(db, keys_dir, actor) {
        Ok(store) => store,
        Err(e) => {
            print_failure(&format_args!(
                "opening database {} for verification: {e}",
                db.display()
            ));
            return ExitCode::from(store_exit_code(&e));
        }
    };
    let v = match store.audit_verify() {
        Ok(v) => v,
        Err(e) => {
            print_failure(&format_args!("verifying audit chain: {e}"));
            return ExitCode::from(store_exit_code(&e));
        }
    };
    finish(&store, keys_dir, &v, None, format)
}

/// Verify the chain of the pack at `pack` under the key of the pack's instance
/// (`keys/<instance_id>.key`, never created) and print the result: the pack is opened
/// immutable, so nothing is written.
pub(crate) fn run_pack(pack: &Path, keys_dir: &Path, format: Format) -> ExitCode {
    let (store, manifest) = match SqliteStore::open_pack(pack, crate::cli::pack::ADMISSION) {
        Ok(opened) => opened,
        Err(e) => {
            print_failure(&format_args!("opening pack for verification: {e}"));
            return ExitCode::from(EXIT_STORE_ERROR);
        }
    };
    let signer = match KeySigner::open_existing(keys_dir, &manifest.instance_id) {
        Ok(signer) => signer,
        Err(e) => {
            print_failure(&format_args!("opening the pack instance's key: {e}"));
            return ExitCode::from(EXIT_STORE_ERROR);
        }
    };
    let v = match store.audit_verify_with(&signer) {
        Ok(v) => v,
        Err(e) => {
            print_failure(&format_args!("verifying audit chain: {e}"));
            return ExitCode::from(store_exit_code(&e));
        }
    };
    let pack = json!({
        "pack_id": manifest.pack_id,
        "head": manifest.audit.head,
        "entries": manifest.audit.entries,
    });
    finish(&store, keys_dir, &v, Some(pack), format)
}

/// The document for a verified chain, printed; `pack` is the pack mode's manifest summary.
fn finish(
    store: &SqliteStore,
    keys_dir: &Path,
    v: &AuditVerification,
    pack: Option<Value>,
    format: Format,
) -> ExitCode {
    let mut report = match report(store, keys_dir, v) {
        Ok(report) => report,
        Err(e) => {
            print_failure(&e);
            return ExitCode::from(match e {
                ReportError::Store(e) => store_exit_code(&e),
                ReportError::ParentKey(_) => EXIT_STORE_ERROR,
            });
        }
    };
    if let Some(pack) = pack {
        report.document["pack"] = pack;
    }
    if let Err(e) = print_json(&report.document, format) {
        print_failure(&e);
        return ExitCode::from(EXIT_FAILURE);
    }
    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(EXIT_CHAIN_BROKEN)
    }
}

/// The printed document and whether every verified segment held.
struct Report {
    document: Value,
    ok: bool,
}

/// Why the document could not be produced: a store failure (its own exit class), or a parent
/// key that is there but cannot be used (review e-8; the key error's own message, which names
/// the failure class and a byte count, never a path).
#[derive(Debug)]
enum ReportError {
    Store(kaban_core::StoreError),
    ParentKey(AuditError),
}

impl From<kaban_core::StoreError> for ReportError {
    fn from(e: kaban_core::StoreError) -> Self {
        Self::Store(e)
    }
}

impl From<serde_json::Error> for ReportError {
    fn from(e: serde_json::Error) -> Self {
        Self::Store(e.into())
    }
}

impl std::fmt::Display for ReportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(e) => write!(f, "verifying audit chain: {e}"),
            Self::ParentKey(e) => write!(f, "opening the parent instance's key: {e}"),
        }
    }
}

/// The verification document: the chain's own result, plus the fork provenance and the
/// parent-history result for a fork.
fn report(
    store: &SqliteStore,
    keys_dir: &Path,
    v: &AuditVerification,
) -> Result<Report, ReportError> {
    let mut document = serde_json::to_value(v)?;
    let Some(fork) = store.audit_fork()? else {
        document["chain"] = json!("origin");
        return Ok(Report { document, ok: v.ok });
    };
    document["chain"] = json!("child");
    document["fork"] = json!({
        "genesis_seq": fork.genesis_seq,
        "binding": fork.binding,
    });
    let parent_instance = fork
        .binding
        .as_ref()
        .and_then(|b| b.get("parent_instance_id"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let parent_key = match parent_instance
        .as_deref()
        .map(|id| KeySigner::open_existing(keys_dir, id))
    {
        Some(Ok(signer)) => Some(signer),
        // no parent key here: the parent pack is the evidence, not this file
        None => None,
        Some(Err(AuditError::Io(e))) if e.kind() == std::io::ErrorKind::NotFound => None,
        // a key that is there but unreadable, truncated or malformed is a failure, never
        // reported as ordinary absence (review e-8)
        Some(Err(e)) => return Err(ReportError::ParentKey(e)),
    };
    let (parent_history, parent_ok) = match parent_key {
        Some(signer) => match store.audit_verify_parent_history(&signer)? {
            Some(p) => (
                json!({
                    "key": "present",
                    "entries": p.entries,
                    "ok": p.ok,
                    "first_bad_seq": p.first_bad_seq,
                }),
                p.ok,
            ),
            None => (json!({ "key": "present", "entries": 0, "ok": true }), true),
        },
        None => (
            json!({
                "key": "absent",
                "ok": Value::Null,
                "note": "verify the parent pack with the parent instance's key",
            }),
            true,
        ),
    };
    document["parent_history"] = parent_history;
    Ok(Report {
        document,
        ok: v.ok && parent_ok,
    })
}
