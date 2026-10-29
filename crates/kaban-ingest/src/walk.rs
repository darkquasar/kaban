//! The walker (docs-ingest-plan.md DI0.2; docs-ingest-spec D5 sorted walks, D6 path rules;
//! dispositions D-7 to D-10, D-26, R-9; review RD-7): a gitignore-aware walk of the configured
//! roots that yields metadata-only candidates in byte-sorted `relative_path` order, or a bare
//! skip reason. Nothing here hashes, logs or stores a path: a skipped file is never read,
//! stored or identified, so no identity, keyed or not, is derived from its name (RD-7).

use crate::hash::{GatedPath, gate_path, keyed_path_id};
use crate::model::{Format, IngestError};
use globset::{GlobSet, GlobSetBuilder};
use kaban_core::WorkspaceId;
use kaban_policy::{AuditSigner, Policy};
use std::path::{Path, PathBuf};

/// Default file-size cap: 2 MiB (docs-ingest-plan.md §4).
pub const DEFAULT_MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;

/// Default path denylist (docs-ingest-spec D6). A pattern without `/` matches the file name at
/// any depth; a pattern with `/` matches the workspace-relative path.
pub const DEFAULT_DENY_GLOBS: [&str; 5] = [".env*", "*.pem", "*.key", "id_rsa*", "secrets/**"];

/// What to walk and which files to refuse before extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkConfig {
    /// Directories (or files) to walk; each must lie inside `workspace_root`.
    pub roots: Vec<PathBuf>,
    /// The root that relative paths and the symlink containment check refer to.
    pub workspace_root: PathBuf,
    /// Follow symlinks that resolve inside `workspace_root` (`--follow-symlinks`); links that
    /// escape the root are `OutsideRoot` regardless (D-10).
    pub follow_symlinks: bool,
    /// Files larger than this are `TooLarge` (`--max-file-bytes`, default 2 MiB).
    pub max_file_bytes: u64,
    /// Yield source-code files (`--include-code`, spec §7); otherwise they are `Code`.
    pub include_code: bool,
    /// Path denylist globs; the index of the matching glob is the `Denied` id (D-11).
    pub deny_globs: Vec<String>,
    /// Reactivate documents `forget --document` marked forgotten even when they are reached by
    /// a directory walk (`--include-forgotten`, DI1.8); a root naming the file itself always
    /// does.
    pub include_forgotten: bool,
}

impl WalkConfig {
    /// Walk `workspace_root` itself with the default cap, denylist and flags.
    pub fn new(workspace_root: PathBuf) -> Self {
        Self {
            roots: vec![workspace_root.clone()],
            workspace_root,
            follow_symlinks: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            include_code: false,
            deny_globs: DEFAULT_DENY_GLOBS.iter().map(|g| (*g).to_owned()).collect(),
            include_forgotten: false,
        }
    }
}

/// A file the walker accepted on metadata alone. The relative path is private: it has not
/// passed the denylist gate yet, so it must not be hashed, logged or stored; gate it with
/// [`Candidate::gate`] (DI0.2 `gate_path`) before deriving any identity from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// `/`-separated path relative to `workspace_root`; sorted ascending by bytes.
    relative_path: String,
    /// Absolute path to read the bytes from.
    pub abs: PathBuf,
    /// File size in bytes.
    pub size: u64,
    /// Modification time, Unix seconds UTC (D-26). Not part of any identity.
    pub mtime: i64,
    /// Format chosen from the extension.
    pub format: Format,
    /// Walk-time device and inode of the file (`open_checked` re-checks them on the opened fd).
    dev: u64,
    ino: u64,
    /// Whether the open may follow a final-component symlink (`WalkConfig::follow_symlinks`).
    follow_symlinks: bool,
    /// Read bound (`WalkConfig::max_file_bytes`).
    max_file_bytes: u64,
    /// Canonical workspace and target captured at walk time; never caller-provided authority.
    canonical_root: PathBuf,
    checked_relative: PathBuf,
    root_identity: (u64, u64),
}

/// A candidate's bytes, opened by [`Candidate::open_checked`] and bounded by `max_file_bytes`.
#[derive(Debug)]
pub struct CheckedFile {
    inner: std::io::Take<std::fs::File>,
}

impl std::io::Read for CheckedFile {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Candidate {
    /// Open this candidate's bytes for reading (RD-2): the only way the sniff and, from DI1, every
    /// content read may touch a candidate. While `follow_symlinks` is off the open does not follow
    /// a final-component symlink (`O_NOFOLLOW`), so a file swapped for a link after the walk is
    /// refused instead of read. On the opened descriptor the file must still be a regular file
    /// with the walk-time `(dev, ino)` and a size within `max_file_bytes`; reads are bounded by
    /// `max_file_bytes`. Every component is opened relative to a held directory descriptor with
    /// `O_NOFOLLOW`; even permitted symlinks are resolved only at walk time, beneath the root.
    ///
    /// # Errors
    /// The open error (`FilesystemLoop` for a refused link), or `InvalidData` when the opened
    /// file is not the regular file the walk saw. Messages carry no path.
    pub fn open_checked(&self) -> std::io::Result<CheckedFile> {
        use rustix::fs::{Mode, OFlags};
        use std::io::{Error, ErrorKind, Read};
        use std::os::unix::fs::MetadataExt;
        let directory_flags =
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::DIRECTORY;
        let mut directory = rustix::fs::open("/", directory_flags, Mode::empty())?;
        for component in self.canonical_root.components() {
            match component {
                std::path::Component::RootDir => {}
                std::path::Component::Normal(name) => {
                    directory =
                        rustix::fs::openat(&directory, name, directory_flags, Mode::empty())?;
                }
                _ => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "invalid workspace components",
                    ));
                }
            }
        }
        let root = std::fs::File::from(directory);
        let metadata = root.metadata()?;
        if (metadata.dev(), metadata.ino()) != self.root_identity {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "workspace root was replaced",
            ));
        }
        let mut directory = root;
        let mut components = self.checked_relative.components().peekable();
        let file = loop {
            let Some(std::path::Component::Normal(name)) = components.next() else {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    "invalid candidate components",
                ));
            };
            let final_component = components.peek().is_none();
            let flags = if final_component {
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK
            } else {
                directory_flags
            };
            let opened =
                std::fs::File::from(rustix::fs::openat(&directory, name, flags, Mode::empty())?);
            if final_component {
                break opened;
            }
            directory = opened;
        };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "candidate is no longer a regular file",
            ));
        }
        if (meta.dev(), meta.ino()) != (self.dev, self.ino) {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "candidate was replaced since the walk",
            ));
        }
        if meta.len() > self.max_file_bytes {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "candidate grew beyond max_file_bytes since the walk",
            ));
        }
        Ok(CheckedFile {
            inner: file.take(self.max_file_bytes),
        })
    }

    /// Step 1 of this document: scan the relative path before anything is derived from it
    /// ([`gate_path`]). `Ok` yields the only input `SourceId::new` accepts.
    ///
    /// # Errors
    /// `Skip::PathRejected { rule_ids }` on any denylist hit.
    pub fn gate(&self, policy: &Policy) -> Result<GatedPath, Skip> {
        gate_path(policy, &self.relative_path)
    }

    /// The keyed path id of this candidate ([`keyed_path_id`]): the identity of a
    /// `PathRejected` document and the only name a log line may carry. Derived only after
    /// [`Candidate::gate`] rejected the path (walker skips carry no id at all, RD-7).
    pub fn keyed_path_id(&self, signer: &dyn AuditSigner, ws: &WorkspaceId) -> String {
        keyed_path_id(signer, ws, &self.relative_path)
    }
}

/// Why the walker did not yield a file. Carries glob ids and rule ids only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// A deny glob matched; the string is the glob's index in `WalkConfig::deny_globs`.
    Denied(String),
    /// Larger than `max_file_bytes`.
    TooLarge,
    /// A NUL byte in the first 8 KiB of a text-format file (D-9).
    Binary,
    /// A source-code file and `include_code` is off (spec §7).
    Code,
    /// No extractor claims the extension.
    Unsupported,
    /// A symlink whose target resolves outside `workspace_root` (D-10).
    OutsideRoot,
    /// A symlink inside the root while `follow_symlinks` is off (D-10).
    Symlink,
    /// The denylist hit the path itself (`gate_path`); rule ids only.
    PathRejected {
        /// Rules that fired.
        rule_ids: Vec<String>,
    },
}

/// Walk `cfg.roots` honouring `.gitignore` and `.kabanignore`, apply the walker rules on
/// metadata (and an 8 KiB sniff for text formats, D-9) and yield candidates sorted by the
/// bytes of their relative path (D-8). A skip is reported as its bare reason (RD-7): the
/// skipped file is never read, stored or identified, so nothing, not even a keyed id, is
/// derived from its path. Only a candidate's path reaches `gate_path`, and only a gated path
/// becomes an identity.
///
/// Files excluded by an ignore file are not candidates and are not reported; a deny glob is the
/// first rule applied to every file the walk does see, and hidden files are seen (R-9). A
/// `.git` directory, at any depth, is pruned before it is descended into (DI2.0 (b)): nothing
/// under it is read, `stat`ed or reported, so a repository's object store costs the walk one
/// name comparison. Deny globs stay file-level rules (their `Denied` ids and counts are
/// unchanged).
///
/// # Errors
/// `IngestError::Config` when a deny glob does not compile, when the workspace root cannot be
/// resolved, or when a configured root is a symlink while `follow_symlinks` is off or resolves
/// outside the canonical workspace root (RD-2: validated before any traversal, because
/// `walkdir` follows a root symlink regardless of `follow_links`). Messages name no path.
pub fn walk(
    cfg: &WalkConfig,
) -> Result<impl Iterator<Item = Result<Candidate, Skip>>, IngestError> {
    let deny = compile_deny_globs(&cfg.deny_globs)?;
    let canonical_root = std::fs::canonicalize(&cfg.workspace_root).map_err(|e| {
        IngestError::Config(format!("workspace root cannot be resolved: {:?}", e.kind()))
    })?;
    let roots = cfg
        .roots
        .iter()
        .map(|root| validate_root(cfg, &canonical_root, root))
        .collect::<Result<Vec<_>, _>>()?;
    let mut entries: Vec<(String, Result<Candidate, Skip>)> = Vec::new();
    for root in &roots {
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .hidden(false) // R-9: `.env` must reach the deny globs
            .follow_links(cfg.follow_symlinks)
            .git_global(false) // the user's global excludes would make runs machine-dependent (N5)
            .require_git(false) // honour `.gitignore` in trees that are not git repositories
            .add_custom_ignore_filename(".kabanignore");
        // directories pruned before descent: `.git` always; with `follow_symlinks`, also a
        // symlinked directory that resolves outside the root
        let contained_root = cfg.follow_symlinks.then(|| canonical_root.clone());
        builder.filter_entry(move |e| {
            if is_git_dir(e) {
                return false;
            }
            match &contained_root {
                Some(root) if e.path_is_symlink() && e.file_type().is_some_and(|t| t.is_dir()) => {
                    std::fs::canonicalize(e.path()).is_ok_and(|target| target.starts_with(root))
                }
                _ => true,
            }
        });
        for entry in builder.build() {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    tracing::warn!(kind = ?e.io_error().map(std::io::Error::kind), "walk entry error");
                    continue;
                }
            };
            if entry.file_type().is_some_and(|t| t.is_dir()) {
                continue;
            }
            // entries hang off a canonical root, so the prefix is the canonical workspace root
            let Some(relative) = relative_path(&canonical_root, entry.path()) else {
                continue;
            };
            let verdict = judge(cfg, &deny, &canonical_root, &relative, &entry);
            entries.push((relative, verdict));
        }
    }
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries.dedup_by(|a, b| a.0 == b.0); // overlapping roots
    tracing::debug!(
        candidates = entries.iter().filter(|e| e.1.is_ok()).count(),
        skipped = entries.iter().filter(|e| e.1.is_err()).count(),
        "walk complete"
    );
    let items: Vec<_> = entries
        .into_iter()
        .map(|(_relative, verdict)| {
            if let Err(skip) = &verdict {
                // the reason only: glob ids and rule ids, never the path (RD-7)
                tracing::trace!(skip = ?skip, "walker skipped a file");
            }
            verdict
        })
        .collect();
    Ok(items.into_iter())
}

/// A directory named `.git` (the repository metadata of git itself; a worktree's `.git` is a
/// file and stays an ordinary, unsupported entry). Decided on the entry's own name and type
/// only, before anything under it is opened.
fn is_git_dir(entry: &ignore::DirEntry) -> bool {
    entry.file_name() == ".git" && entry.file_type().is_some_and(|t| t.is_dir())
}

/// Resolve one configured root before it is traversed (RD-2): a root that is itself a symlink
/// is refused while `follow_symlinks` is off (`walkdir` would follow it regardless), and the
/// canonical root must lie inside the canonical workspace root (a lexical `../outside` or a
/// link to another tree is refused instead of being traversed). Returns the canonical root,
/// which is what the walk descends into.
fn validate_root(
    cfg: &WalkConfig,
    canonical_root: &Path,
    root: &Path,
) -> Result<PathBuf, IngestError> {
    let is_symlink = std::fs::symlink_metadata(root)
        .map_err(|e| IngestError::Config(format!("root cannot be read: {:?}", e.kind())))?
        .file_type()
        .is_symlink();
    if is_symlink && !cfg.follow_symlinks {
        return Err(IngestError::Config(
            "root is a symlink and follow_symlinks is off".to_owned(),
        ));
    }
    let canonical = std::fs::canonicalize(root)
        .map_err(|e| IngestError::Config(format!("root cannot be resolved: {:?}", e.kind())))?;
    if !canonical.starts_with(canonical_root) {
        return Err(IngestError::Config(
            "root resolves outside the workspace root".to_owned(),
        ));
    }
    Ok(canonical)
}

/// Workspace-relative, `/`-separated path; `None` when the entry lies outside the root or has a
/// non-UTF-8 name.
fn relative_path(root: &Path, abs: &Path) -> Option<String> {
    let rel = abs.strip_prefix(root).ok()?;
    let mut out = String::new();
    for comp in rel.components() {
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(comp.as_os_str().to_str()?);
    }
    Some(out)
}

/// Apply the walker rules to one file, in order: deny globs (R-9), then everything the
/// candidate's metadata decides.
fn judge(
    cfg: &WalkConfig,
    deny: &GlobSet,
    canonical_root: &Path,
    relative: &str,
    entry: &ignore::DirEntry,
) -> Result<Candidate, Skip> {
    if let Some(first) = deny.matches(relative).into_iter().min() {
        return Err(Skip::Denied(first.to_string()));
    }
    // containment before any metadata or read of the target (D-10): a link that escapes the
    // root is never stat'ed, sniffed or read, whatever `follow_symlinks` says
    let is_symlink = entry.path_is_symlink() || entry.file_type().is_some_and(|t| t.is_symlink());
    if is_symlink {
        let contained = std::fs::canonicalize(entry.path())
            .is_ok_and(|target| target.starts_with(canonical_root));
        if !contained {
            return Err(Skip::OutsideRoot);
        }
        if !cfg.follow_symlinks {
            return Err(Skip::Symlink);
        }
    }
    let meta = entry.metadata().map_err(|_| Skip::Unsupported)?;
    let checked_abs = std::fs::canonicalize(entry.path()).map_err(|_| Skip::Unsupported)?;
    let checked_relative = checked_abs
        .strip_prefix(canonical_root)
        .map_err(|_| Skip::OutsideRoot)?
        .to_path_buf();
    let root_meta = std::fs::metadata(canonical_root).map_err(|_| Skip::Unsupported)?;
    if meta.len() > cfg.max_file_bytes {
        return Err(Skip::TooLarge);
    }
    let format = format_of(relative).ok_or(Skip::Unsupported)?;
    if format == Format::Code && !cfg.include_code {
        return Err(Skip::Code);
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or_default();
    let candidate = Candidate {
        relative_path: relative.to_owned(),
        abs: entry.path().to_path_buf(),
        size: meta.len(),
        mtime,
        format,
        dev: std::os::unix::fs::MetadataExt::dev(&meta),
        ino: std::os::unix::fs::MetadataExt::ino(&meta),
        follow_symlinks: cfg.follow_symlinks,
        max_file_bytes: cfg.max_file_bytes,
        canonical_root: canonical_root.to_path_buf(),
        checked_relative,
        root_identity: (
            std::os::unix::fs::MetadataExt::dev(&root_meta),
            std::os::unix::fs::MetadataExt::ino(&root_meta),
        ),
    };
    if format.is_text_like() && sniff_is_binary(&candidate) {
        return Err(Skip::Binary);
    }
    Ok(candidate)
}

/// Bytes sniffed for NUL (D-9).
const SNIFF_BYTES: usize = 8 * 1024;

/// Read at most 8 KiB into a transient buffer and look for a NUL byte. The buffer is dropped
/// here: nothing derived from the bytes is hashed, logged or stored (D-9). The file is opened
/// through [`Candidate::open_checked`] (RD-2). An unreadable or changed file is not binary; the
/// error surfaces later, on the gated path, when the worker opens it the same way.
fn sniff_is_binary(candidate: &Candidate) -> bool {
    use std::io::Read;
    let Ok(file) = candidate.open_checked() else {
        return false;
    };
    let mut buf = vec![0u8; SNIFF_BYTES];
    let mut filled = 0;
    let mut reader = file.take(SNIFF_BYTES as u64);
    loop {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return false,
        }
        if filled == SNIFF_BYTES {
            break;
        }
    }
    buf[..filled].contains(&0)
}

/// Source-code extensions excluded by default (docs-ingest-spec §7; docs-ingest-plan.md DI0.2).
const CODE_EXTENSIONS: [&str; 24] = [
    "rs", "py", "ts", "tsx", "js", "jsx", "mjs", "go", "java", "kt", "c", "cc", "cpp", "h", "hpp",
    "cs", "rb", "php", "swift", "scala", "sh", "bash", "zsh", "sql",
];

/// The format an extension maps to; `None` when no extractor claims it.
fn format_of(relative: &str) -> Option<Format> {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    let (_, ext) = name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    Some(match ext.as_str() {
        "md" | "markdown" => Format::Markdown,
        "txt" => Format::Text,
        "pdf" => Format::Pdf,
        "html" | "htm" => Format::Html,
        "csv" | "tsv" => Format::Csv,
        "xlsx" | "xls" | "xlsb" | "ods" => Format::Sheet,
        "docx" => Format::Docx,
        e if CODE_EXTENSIONS.contains(&e) => Format::Code,
        _ => return None,
    })
}

impl Format {
    /// Formats whose bytes are text, so a NUL byte means a binary file (D-9). PDF, spreadsheet
    /// and DOCX containers are binary by nature and are never sniffed.
    fn is_text_like(self) -> bool {
        matches!(
            self,
            Self::Markdown | Self::Text | Self::Html | Self::Csv | Self::Code
        )
    }
}

/// One `GlobSet` whose indices are the `deny_globs` indices (D-11). A pattern without `/` is
/// matched at any depth (`**/<pattern>`); `*` never crosses a `/`.
fn compile_deny_globs(globs: &[String]) -> Result<GlobSet, IngestError> {
    let mut b = GlobSetBuilder::new();
    for (i, g) in globs.iter().enumerate() {
        let pattern = if g.contains('/') {
            g.clone()
        } else {
            format!("**/{g}")
        };
        let glob = globset::GlobBuilder::new(&pattern)
            .literal_separator(true)
            .build()
            .map_err(|e| IngestError::Config(format!("deny glob {i} does not compile: {e}")))?;
        b.add(glob);
    }
    b.build()
        .map_err(|e| IngestError::Config(format!("deny globs do not compile: {e}")))
}
