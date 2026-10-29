//! Trusted wrapper resolution (stage1-spec G1-S5.2, RS2-5, RV-9): the wrapper of a tier comes
//! from a fixed absolute candidate list (or the absolute path of `index.container_cli`), never
//! from `PATH`; each candidate is opened once as an `O_PATH|O_NOFOLLOW|O_CLOEXEC` handle that
//! must be a regular ELF file owned by root or the user and writable by neither group nor
//! other (`kaban_core::process::Exec::open_handle`), and is executed through that handle.

use std::path::{Path, PathBuf};

use kaban_core::process::{Exec, ProcessError};
use thiserror::Error;

/// The fixed `unshare` candidates of the `namespaces` tier.
pub const UNSHARE_CANDIDATES: [&str; 2] = ["/usr/bin/unshare", "/bin/unshare"];

/// The fixed container CLI candidates (docker, then podman).
pub const CONTAINER_CLI_CANDIDATES: [&str; 6] = [
    "/usr/bin/docker",
    "/usr/local/bin/docker",
    "/opt/homebrew/bin/docker",
    "/usr/bin/podman",
    "/usr/local/bin/podman",
    "/opt/podman/bin/podman",
];

/// Why no wrapper resolved. The candidate path is not part of the text (the list is fixed and
/// the caller knows it); the handle class is.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum WrapperError {
    /// No candidate exists (every open failed with an io error).
    #[error("wrapper_not_found")]
    NotFound,
    /// A candidate exists but the handle rule refused it (`wrapper_not_elf`,
    /// `wrapper_writable`, `wrapper_owner`, `wrapper_not_regular`); a later candidate is not
    /// tried over a refused one (a refused file at a fixed path is a finding, not a miss).
    #[error("wrapper refused: {0}")]
    Refused(&'static str),
    /// A configured path is not absolute.
    #[error("configured wrapper path not absolute")]
    NotAbsolute,
    /// Handle execution is unsupported on this target (RV-9).
    #[error("handle_exec_unsupported")]
    HandleExecUnsupported,
}

impl WrapperError {
    /// The fixed class text.
    #[must_use]
    pub const fn class(&self) -> &'static str {
        match self {
            Self::NotFound => "wrapper_not_found",
            Self::Refused(class) => class,
            Self::NotAbsolute => "wrapper_path_not_absolute",
            Self::HandleExecUnsupported => "handle_exec_unsupported",
        }
    }
}

/// A resolved wrapper: the handle to execute through and the candidate path it came from (for
/// `meta.isolation_detail`, kaban-built).
#[derive(Debug)]
pub struct ResolvedWrapper {
    /// The verified handle (`Exec::Handle`).
    pub exec: Exec,
    /// The absolute candidate path.
    pub path: PathBuf,
}

/// Resolve the wrapper: `configured` first (must be absolute), then `candidates` in order.
/// The first candidate that opens decides: an acceptable handle resolves, a refused one is
/// the error. Nothing is executed and `PATH` is never consulted.
///
/// # Errors
/// [`WrapperError`].
pub fn resolve_wrapper(
    candidates: &[&Path],
    configured: Option<&Path>,
) -> Result<ResolvedWrapper, WrapperError> {
    if let Some(path) = configured {
        if !path.is_absolute() {
            return Err(WrapperError::NotAbsolute);
        }
        return match Exec::open_handle(path) {
            Ok(exec) => Ok(ResolvedWrapper {
                exec,
                path: path.to_path_buf(),
            }),
            Err(e) => Err(classify(&e)),
        };
    }
    for path in candidates {
        match Exec::open_handle(path) {
            Ok(exec) => {
                return Ok(ResolvedWrapper {
                    exec,
                    path: path.to_path_buf(),
                });
            }
            Err(ProcessError::Spawn(_)) => {}
            Err(e) => return Err(classify(&e)),
        }
    }
    Err(WrapperError::NotFound)
}

fn classify(e: &ProcessError) -> WrapperError {
    match e {
        ProcessError::Env("handle_exec_unsupported") => WrapperError::HandleExecUnsupported,
        ProcessError::Env(class) => WrapperError::Refused(class),
        ProcessError::InheritedFd { .. } => WrapperError::Refused("inherited_fd"),
        ProcessError::Spawn(_) => WrapperError::NotFound,
    }
}
