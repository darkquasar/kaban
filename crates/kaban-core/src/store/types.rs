//! Store-level types owned by no single role trait.

/// Permission bits (`mode & 0o777`) of the database file and its sidecars as a diagnostic
/// open found them (M4-19): read with `stat` before the connection existed and never
/// tightened, so they report what an operator would see. `None` is an absent sidecar; the
/// main file is present whenever an open succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DatabaseFileModes {
    /// The database file.
    pub db: u32,
    /// The `-wal` sidecar, when present.
    pub wal: Option<u32>,
    /// The `-shm` sidecar, when present.
    pub shm: Option<u32>,
}

impl DatabaseFileModes {
    /// The one mode every database file must have (core-spec S5).
    pub const OWNER_ONLY: u32 = 0o600;

    /// Whether every present file is owner-only.
    #[must_use]
    pub fn all_owner_only(&self) -> bool {
        [Some(self.db), self.wal, self.shm]
            .into_iter()
            .flatten()
            .all(|mode| mode == Self::OWNER_ONLY)
    }
}
