//! Audit signing and key management (core-spec S3). The chain of record is the `audit` table
//! inside the SQLite database (`kaban-store-sqlite`); this module owns the HMAC key
//! (`$KABAN_HOME/keys/<db-id>.key`, 0600, created once with `O_EXCL`; concurrent creators retry
//! and reuse), the [`AuditSigner`] used by the store for the chain and by the service layer for
//! `input_hmac` stubs, and the optional JSONL export (`--audit-export`), which is a copy for
//! external shipping and not the chain of record.

use hmac::{Hmac, Mac};
use kaban_core::AuditEntry;
use serde_json::json;
use sha2::Sha256;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use thiserror::Error;
use zeroize::Zeroizing;

/// Length of the HMAC key in bytes.
pub const KEY_LEN: usize = 32;

/// Why the audit key or export could not be initialised or written.
#[derive(Debug, Error)]
pub enum AuditError {
    /// Key file or export file I/O failed.
    #[error("audit i/o failed: {0}")]
    Io(#[from] std::io::Error),
    /// The OS random source failed while creating the key.
    #[error("getrandom: {0}")]
    Random(String),
    /// The key file exists but does not hold exactly [`KEY_LEN`] bytes.
    #[error("audit key file has {len} bytes, expected {KEY_LEN}; refusing to start")]
    BadKeyLength {
        /// Observed length.
        len: usize,
    },
}

/// Computes keyed hashes for the audit chain and for input stubs.
pub trait AuditSigner: Send + Sync + std::fmt::Debug {
    /// Lowercase hex `HMAC-SHA256(key, data)`.
    fn hmac_hex(&self, data: &[u8]) -> String;
}

/// An [`AuditSigner`] over a 32-byte key held in zeroizing memory.
pub struct KeySigner {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl std::fmt::Debug for KeySigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeySigner([redacted])")
    }
}

impl KeySigner {
    /// Signer over an explicit key.
    pub fn new(key: Zeroizing<[u8; KEY_LEN]>) -> Self {
        Self { key }
    }

    /// Signer over a fresh random key that is never written anywhere (in-memory stores, tests).
    ///
    /// # Errors
    /// `Random` when the OS random source fails.
    pub fn ephemeral() -> Result<Self, AuditError> {
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        getrandom::fill(key.as_mut()).map_err(|e| AuditError::Random(e.to_string()))?;
        Ok(Self { key })
    }

    /// Open the key for `db_id` under `dir`, creating it once with `O_EXCL` and mode 0600.
    ///
    /// # Errors
    /// See [`open_or_create_key`].
    pub fn open_or_create(dir: &Path, db_id: &str) -> Result<Self, AuditError> {
        Ok(Self {
            key: open_or_create_key(dir, db_id)?,
        })
    }

    /// Open an existing key for `db_id` under `dir`; never creates one (verifiers, and servers
    /// whose database already carries a chain).
    ///
    /// # Errors
    /// `Io` (`NotFound` when absent) or `BadKeyLength`.
    pub fn open_existing(dir: &Path, db_id: &str) -> Result<Self, AuditError> {
        let bytes = Zeroizing::new(std::fs::read(key_path(dir, db_id))?);
        if bytes.len() != KEY_LEN {
            return Err(AuditError::BadKeyLength { len: bytes.len() });
        }
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        key.copy_from_slice(&bytes);
        Ok(Self { key })
    }
}

impl AuditSigner for KeySigner {
    fn hmac_hex(&self, data: &[u8]) -> String {
        // Invariant: HMAC accepts keys of any length, so new_from_slice cannot fail.
        #[allow(clippy::expect_used)]
        let mut m =
            <Hmac<Sha256>>::new_from_slice(self.key.as_ref()).expect("hmac accepts any key length");
        m.update(data);
        hex::encode(m.finalize().into_bytes())
    }
}

/// Path of the key file for `db_id` under `dir`.
pub fn key_path(dir: &Path, db_id: &str) -> PathBuf {
    dir.join(format!("{db_id}.key"))
}

/// Read the key at `dir/<db_id>.key`, or create it (mode 0600, `create_new`) when absent.
/// A concurrent creator loses the `O_EXCL` race, retries and reads the winner's key. The
/// directory (and any missing parent) is created with mode exactly 0700 when missing.
///
/// # Errors
/// `Io` on filesystem errors, `Random` if key generation fails, `BadKeyLength` if an existing
/// key file does not hold exactly [`KEY_LEN`] bytes (also after the retries a half-written file
/// gets).
pub fn open_or_create_key(dir: &Path, db_id: &str) -> Result<Zeroizing<[u8; KEY_LEN]>, AuditError> {
    use std::os::unix::fs::DirBuilderExt;
    // every directory this call creates (`keys` and any missing parent such as `$KABAN_HOME`)
    // is set to exactly 0700 as soon as it exists: the builder's mode is filtered by the umask,
    // and a parent left at 0500 could not even receive its child (C2.5b, P-6). Directories that
    // already existed keep their mode.
    let missing: Vec<&Path> = dir
        .ancestors()
        .take_while(|d| d.symlink_metadata().is_err())
        .collect();
    for d in missing.into_iter().rev() {
        match std::fs::DirBuilder::new().mode(0o700).create(d) {
            Ok(()) => std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700))?,
            // a concurrent opener made it first; it sets the mode
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(e.into()),
        }
    }
    let path = key_path(dir, db_id);
    for attempt in 0..5u32 {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(mut f) => {
                // exact 0600 whatever the umask, on the descriptor (C2.5b, P-6)
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                let mut key = Zeroizing::new([0u8; KEY_LEN]);
                getrandom::fill(key.as_mut()).map_err(|e| AuditError::Random(e.to_string()))?;
                f.write_all(key.as_ref())?;
                f.sync_all()?;
                return Ok(key);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let bytes = Zeroizing::new(std::fs::read(&path)?);
                if bytes.len() == KEY_LEN {
                    let mut key = Zeroizing::new([0u8; KEY_LEN]);
                    key.copy_from_slice(&bytes);
                    return Ok(key);
                }
                // the creator may still be writing; give it a moment, then refuse
                if attempt == 4 {
                    return Err(AuditError::BadKeyLength { len: bytes.len() });
                }
                std::thread::sleep(std::time::Duration::from_millis(20 << attempt));
            }
            Err(e) => return Err(e.into()),
        }
    }
    Err(AuditError::BadKeyLength { len: 0 })
}

/// Optional JSONL copy of audit entries for external shipping (`--audit-export`). Each line
/// carries the entry (rule ids and keyed hashes only), a timestamp and a per-process line chain
/// (`prev` = HMAC of the previous line). The chain of record is the database's `audit` table;
/// export failures are logged by the caller and never reject a write.
#[derive(Debug)]
pub struct AuditExport {
    path: PathBuf,
    signer: Arc<dyn AuditSigner>,
    prev: Mutex<String>,
}

impl AuditExport {
    /// Export to `path` (created 0600, appended), chaining lines with `signer`.
    pub fn new(path: PathBuf, signer: Arc<dyn AuditSigner>) -> Self {
        Self {
            path,
            signer,
            prev: Mutex::new("genesis".into()),
        }
    }

    /// Append one line per entry and fsync.
    ///
    /// # Errors
    /// `Io` when the file cannot be appended to or synced.
    pub fn append(&self, entries: &[AuditEntry]) -> Result<(), AuditError> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut prev = self
            .prev
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.path)?;
        if f.metadata()?.len() == 0 {
            // just created (or still empty): exact 0600 whatever the umask (C2.5b, P-6)
            f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        for e in entries {
            let line = json!({
                "ts": kaban_core::now_iso(),
                "op": e.op.as_str(), "tool": e.tool, "field": e.field,
                "workspace_id": e.workspace_id, "policy_version": e.policy_version,
                "input_hmac": e.input_hmac, "rules": e.rules, "node_id": e.node_id,
                "prev": *prev,
            })
            .to_string();
            writeln!(f, "{line}")?;
            *prev = self.signer.hmac_hex(line.as_bytes());
        }
        f.sync_data()?;
        Ok(())
    }
}
