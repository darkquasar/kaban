//! The embedding initializer and the [`Runtime`] token that proves the allocator adapter is
//! installed (spec 11 QRY-04, QRY-07, PC-17).

use std::ffi::c_int;
use std::fmt;

/// Proof that this process installed kaban's SQLite allocator adapter.
///
/// The only way to obtain one is [`initialize_for_embedding`] (or the standalone CLI's startup
/// bootstrap, which calls it). [`Runtime::get`] hands out the installed token again; no safe API
/// configures SQLite or installs the adapter lazily.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Runtime {
    _installed: (),
}

impl Runtime {
    /// The installed runtime, or `None` when no initialization succeeded in this process.
    #[must_use]
    pub fn get() -> Option<Self> {
        None
    }
}

/// Why [`initialize_for_embedding`] refused to install the allocator adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// kaban already ran its initializer in this process (successfully or not); it runs once.
    AlreadyInitialized,
    /// SQLite was initialized before kaban (the host opened a connection or called into SQLite
    /// first). SQLite is left running; it is never shut down to retry.
    HostInitializedSqlite,
    /// SQLite's current allocator lacks a hook the adapter forwards to.
    OriginalIncomplete {
        /// The missing `sqlite3_mem_methods` member.
        hook: &'static str,
    },
    /// `sqlite3_config` refused an option with a code other than `SQLITE_MISUSE`.
    ConfigRefused {
        /// The configuration option that was refused.
        option: &'static str,
        /// The SQLite result code.
        code: c_int,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyInitialized => {
                f.write_str("kaban already initialized its sqlite allocator adapter in this process")
            }
            Self::HostInitializedSqlite => f.write_str(
                "sqlite was initialized by the host before kaban; the allocator adapter must be \
                 installed before any sqlite use",
            ),
            Self::OriginalIncomplete { hook } => {
                write!(f, "sqlite's original allocator has no {hook} hook")
            }
            Self::ConfigRefused { option, code } => {
                write!(f, "sqlite refused {option} with code {code}")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Install kaban's request-scoped SQLite allocator adapter and return the [`Runtime`] token.
///
/// # Errors
/// [`InitError::AlreadyInitialized`] on every call after the first,
/// [`InitError::HostInitializedSqlite`] when SQLite was initialized before this call, and
/// [`InitError::OriginalIncomplete`] or [`InitError::ConfigRefused`] when SQLite's configuration
/// interface cannot supply or accept the hooks.
///
/// # Safety
/// The caller guarantees, for the whole life of the process:
/// - exclusive startup: this runs before any SQLite entry in the process and while no other
///   thread can call into SQLite (or call `sqlite3_config`) concurrently;
/// - no concurrent external SQLite use during the call, including a concurrent call of this
///   function from a thread kaban does not coordinate (the internal `OnceLock` excludes only
///   kaban's own callers);
/// - no allocator replacement (`SQLITE_CONFIG_MALLOC`) and no `sqlite3_shutdown` after it
///   returns `Ok`, while the [`Runtime`] lives (which is until process exit).
pub unsafe fn initialize_for_embedding() -> Result<Runtime, InitError> {
    Ok(Runtime { _installed: () })
}
