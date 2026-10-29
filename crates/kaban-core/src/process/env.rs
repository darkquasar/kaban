//! The clean environment allowlist (G1-S5.3, RV-1, SO-8).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

use super::ProcessError;

/// A constant list of `(name, value)` pairs a runner sets itself, trusted by construction
/// (`'static` literals, never caller data). The only way a name the refusal patterns cover
/// reaches a child: `GitRunner`'s two Git keys ([`GIT_INTERNAL`], RV-1) and, from CG4.3, the
/// indexer runner's offline toolchain constants (`GOPROXY=off` and friends, SO-8). The lint
/// `no_trusted_internal_outside_runners` keeps `TrustedInternal(` and `with_trusted` inside the
/// runners.
#[derive(Debug, Clone, Copy)]
pub struct TrustedInternal(pub &'static [(&'static str, &'static str)]);

/// The one trusted-internal exception of `GitRunner` (RV-1): exactly `GIT_OPTIONAL_LOCKS=0` and
/// `GIT_TERMINAL_PROMPT=0`, constant keys and values, not reachable through [`EnvAllowlist::insert`].
pub const GIT_INTERNAL: TrustedInternal =
    TrustedInternal(&[("GIT_OPTIONAL_LOCKS", "0"), ("GIT_TERMINAL_PROMPT", "0")]);

/// The child's whole environment: `env_clear()` then exactly these variables (G1-S5.3).
/// [`EnvAllowlist::insert`] and [`EnvAllowlist::carry`] refuse names matching
/// `*_TOKEN`, `*_KEY`, `*_SECRET`, `SSH_*`, `GIT_*`, `AWS_*` or `*PROXY*` (ASCII case-insensitive,
/// so `https_proxy` is refused too) before any spawn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvAllowlist(BTreeMap<String, OsString>);

impl EnvAllowlist {
    /// An empty allowlist: the child sees no variable at all.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// An allowlist pre-filled with a runner's constant trusted-internal pairs (SO-8). Caller
    /// variables still go through [`EnvAllowlist::insert`] or [`EnvAllowlist::carry`].
    #[must_use]
    pub fn with_trusted(trusted: TrustedInternal) -> Self {
        Self(
            trusted
                .0
                .iter()
                .map(|(k, v)| ((*k).to_owned(), OsString::from(v)))
                .collect(),
        )
    }

    /// `GitRunner`'s allowlist seed (RV-1): [`GIT_INTERNAL`] and nothing else. `kaban-index`
    /// never calls it.
    #[must_use]
    pub fn internal_git() -> Self {
        Self::with_trusted(GIT_INTERNAL)
    }

    /// Set `name` to `value` for the child, replacing an earlier value.
    ///
    /// # Errors
    /// [`ProcessError::Env`]`("env_key_refused")` when `name` matches a refused pattern, is empty
    /// or contains `=` or NUL.
    pub fn insert(&mut self, name: &str, value: impl AsRef<OsStr>) -> Result<(), ProcessError> {
        if refused(name) {
            return Err(ProcessError::Env("env_key_refused"));
        }
        self.0.insert(name.to_owned(), value.as_ref().to_owned());
        Ok(())
    }

    /// Copy the caller's own variable `name` into the allowlist (git needs `PATH` and `HOME`).
    /// A variable the caller does not have is skipped, not an error.
    ///
    /// # Errors
    /// [`ProcessError::Env`]`("env_key_refused")` under the same refusals as
    /// [`EnvAllowlist::insert`], checked before the lookup.
    pub fn carry(&mut self, name: &str) -> Result<(), ProcessError> {
        if refused(name) {
            return Err(ProcessError::Env("env_key_refused"));
        }
        if let Some(value) = std::env::var_os(name) {
            self.0.insert(name.to_owned(), value);
        }
        Ok(())
    }

    /// The value the child will see for `name`, if any.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&OsStr> {
        self.0.get(name).map(OsString::as_os_str)
    }

    /// Every `(name, value)` pair in name order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &OsStr)> {
        self.0.iter().map(|(k, v)| (k.as_str(), v.as_os_str()))
    }

    /// Number of variables the child will see.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the child sees no variable at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Whether `name` may never be set from caller data (G1-S5.3): the credential, agent, Git, AWS
/// and proxy families, plus names that cannot be an environment key at all.
fn refused(name: &str) -> bool {
    if name.is_empty() || name.contains('=') || name.contains('\0') {
        return true;
    }
    let upper = name.to_ascii_uppercase();
    upper.ends_with("_TOKEN")
        || upper.ends_with("_KEY")
        || upper.ends_with("_SECRET")
        || upper.starts_with("SSH_")
        || upper.starts_with("GIT_")
        || upper.starts_with("AWS_")
        || upper.contains("PROXY")
}
