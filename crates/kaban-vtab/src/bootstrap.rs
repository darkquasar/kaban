//! The standalone CLI's pre-main installation of the allocator adapter (feature
//! `process-bootstrap`; spec 11 QRY-07, PC-17).
//!
//! A platform startup constructor in this crate calls [`initialize_for_embedding`] before
//! `main`, before any thread and before any SQLite open. On ELF targets the entry sits in
//! `.init_array.00101`: after the standard library's own `.init_array.00099` entry and before
//! every unprioritized constructor of the binary (shared-library constructors run earlier still,
//! and none of them can reach the statically linked SQLite). On Mach-O targets it sits in
//! `__DATA,__mod_init_func`, which has no priorities. Any SQLite use before it, from any earlier
//! constructor, is detected (`SQLITE_MISUSE`) and refuses startup by name. Other targets do not
//! compile with the feature on: there is no constructor section to place the entry in, and a
//! silent no-install would leave every query unbudgeted.
//!
//! When the environment variable [`REPORT_ENV`] is set to `1`, the constructor writes one line
//! to stderr after it installed the adapter: the observable the CLI's bootstrap test reads.

use crate::initialize_for_embedding;

/// Set to `1` to have the startup constructor report a successful installation on stderr.
pub const REPORT_ENV: &str = "KABAN_VTAB_BOOTSTRAP_REPORT";

/// The line the constructor writes to stderr under [`REPORT_ENV`].
pub const REPORT_LINE: &str = "kaban-vtab: sqlite allocator adapter installed before main";

/// Keep the startup constructor in a binary that otherwise names nothing from this crate.
#[inline(never)]
pub fn ensure_linked() {}

/// The startup constructor.
extern "C" fn bootstrap() {
    let _ = initialize_for_embedding;
}

#[used]
static BOOTSTRAP: extern "C" fn() = bootstrap;
