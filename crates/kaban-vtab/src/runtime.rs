//! The embedding initializer and the [`Runtime`] token that proves the allocator adapter is
//! installed (spec 11 QRY-04, QRY-07, PC-17).

use std::ffi::c_int;
use std::fmt;
use std::ptr;
use std::sync::OnceLock;

use rusqlite::ffi;

use crate::allocator;

/// The outcome of the one initialization this process runs; kaban's callers only.
static INSTALLED: OnceLock<Result<Runtime, InitError>> = OnceLock::new();

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
        INSTALLED.get().and_then(|outcome| outcome.ok())
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
            Self::AlreadyInitialized => f.write_str(
                "kaban already initialized its sqlite allocator adapter in this process",
            ),
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
    let mut ran = false;
    let outcome = INSTALLED.get_or_init(|| {
        ran = true;
        // SAFETY: the caller upholds this function's contract, which is `install`'s; the
        // `OnceLock` runs it at most once per process.
        unsafe { install() }
    });
    if ran {
        *outcome
    } else {
        Err(InitError::AlreadyInitialized)
    }
}

/// Record SQLite's current allocator and configure the wrapper in its place.
///
/// # Safety
/// As for [`initialize_for_embedding`]; runs at most once per process.
unsafe fn install() -> Result<Runtime, InitError> {
    let mut original = ffi::sqlite3_mem_methods {
        xMalloc: None,
        xFree: None,
        xRealloc: None,
        xSize: None,
        xRoundup: None,
        xInit: None,
        xShutdown: None,
        pAppData: ptr::null_mut(),
    };
    // SAFETY: GETMALLOC writes one `sqlite3_mem_methods` through the pointer, valid and
    // exclusively borrowed for the call; the caller excludes concurrent SQLite configuration or
    // initialization. Before initialization it only reads SQLite's configuration; after it, it
    // changes nothing and returns SQLITE_MISUSE.
    let rc = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_GETMALLOC, &raw mut original) };
    accepted(rc, "SQLITE_CONFIG_GETMALLOC")?;
    let wrapper = allocator::wrap(&original)?;
    // SAFETY: MALLOC copies the struct before returning, so the pointer need only be valid for
    // the call. SQLite is not initialized (GETMALLOC just succeeded and the caller excludes
    // concurrent use), so no block from the replaced allocator exists that the wrapper could be
    // handed; the wrapper's hooks forward to the allocator `wrap` recorded.
    let rc = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &raw const wrapper) };
    accepted(rc, "SQLITE_CONFIG_MALLOC")?;
    Ok(Runtime { _installed: () })
}

/// Map a `sqlite3_config` result: `SQLITE_MISUSE` means SQLite is already initialized.
fn accepted(rc: c_int, option: &'static str) -> Result<(), InitError> {
    match rc {
        ffi::SQLITE_OK => Ok(()),
        ffi::SQLITE_MISUSE => Err(InitError::HostInitializedSqlite),
        code => Err(InitError::ConfigRefused { option, code }),
    }
}
