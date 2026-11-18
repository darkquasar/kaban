//! The standalone CLI's pre-main installation of the allocator adapter (feature
//! `process-bootstrap`; spec 11 QRY-07, PC-17).
//!
//! A platform startup constructor in this crate calls [`initialize_for_embedding`] before
//! `main`, before any thread and before any SQLite open. On ELF targets the entry sits in
//! `.init_array.00101`: after the standard library's own `.init_array.00099` entry and before
//! every unprioritized constructor of the binary (shared-library constructors run earlier still,
//! and none of them can reach the statically linked SQLite). On Mach-O targets it sits in
//! `__DATA,__mod_init_func`, which has no priorities (untested: no Apple build has been run).
//! Any SQLite use before it, from any earlier constructor, is detected (`SQLITE_MISUSE`) and
//! refuses startup by name. Other targets do not
//! compile with the feature on: there is no constructor section to place the entry in, and a
//! silent no-install would leave every query unbudgeted.
//!
//! When the environment variable [`REPORT_ENV`] is set to `1`, the constructor writes one line
//! to stderr after it installed the adapter: the observable the CLI's bootstrap test reads.

use std::fmt;
use std::io::{self, Write};
use std::mem;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::process;

use crate::initialize_for_embedding;

/// Set to `1` to have the startup constructor report a successful installation on stderr.
pub const REPORT_ENV: &str = "KABAN_VTAB_BOOTSTRAP_REPORT";

/// The line the constructor writes to stderr under [`REPORT_ENV`].
pub const REPORT_LINE: &str = "kaban-vtab: sqlite allocator adapter installed before main";

/// Keep the startup constructor in a binary that otherwise names nothing from this crate.
///
/// rustc links a dependency only when the binary refers to it; one call from `main` makes the
/// constructor part of the binary. The call itself does nothing at run time.
#[inline(never)]
pub fn ensure_linked() {
    // Name the constructor function, not the `BOOTSTRAP` static: either keeps this object, and
    // with it the `#[used]` static, in the link. On Mach-O, Apple's linker rewrites the
    // `__mod_init_func` pointers into `__init_offsets` for recent deployment targets, so the
    // static has no address left and a code reference to it fails to link (ADRP fixup error).
    std::hint::black_box(bootstrap as extern "C" fn());
}

/// The startup constructor: install the adapter, or refuse startup by name.
///
/// It never unwinds out of the init array: the whole body runs under `catch_unwind`, and every
/// failure ends in [`refuse`], which aborts.
extern "C" fn bootstrap() {
    let outcome = catch_unwind(|| {
        // SAFETY: this runs from the init array, before `main`: the process has one thread and
        // no SQLite entry yet unless an earlier constructor made one, which `sqlite3_config`
        // detects (`HostInitializedSqlite`, refused below). Nothing in kaban replaces the
        // allocator or shuts SQLite down afterwards (docs/developer/SAFETY.md, startup hooks).
        let installed = unsafe { initialize_for_embedding() };
        let report = std::env::var_os(REPORT_ENV).is_some_and(|value| value == "1");
        (installed, report)
    });
    match outcome {
        Ok((Ok(_runtime), report)) => {
            if report {
                let _ = writeln!(io::stderr(), "{REPORT_LINE}");
            }
        }
        Ok((Err(error), _)) => refuse(&error),
        Err(payload) => {
            mem::forget(payload); // its destructor could panic again
            refuse(&"the allocator initializer panicked");
        }
    }
}

/// Report why startup is refused and abort the process (never unwind before `main`).
fn refuse(reason: &dyn fmt::Display) -> ! {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = writeln!(io::stderr(), "kaban-vtab: startup refused: {reason}");
    }));
    process::abort()
}

// SAFETY (`link_section`): the section holds an array of `void (*)(void)` startup functions that
// the loader calls in order before `main`; the static is exactly one such pointer, to a function
// that takes no arguments it reads and never unwinds. `.init_array.00101` is the first priority
// open to programs (0-100 are reserved for the implementation; the standard library uses 00099),
// and linkers place prioritized entries before unprioritized ones.
#[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
#[used]
#[unsafe(link_section = ".init_array.00101")]
static BOOTSTRAP: extern "C" fn() = bootstrap;

// SAFETY (`link_section`): `__mod_init_func` holds an array of startup function pointers dyld
// calls before `main`, in link order; the static is exactly one such pointer to a function that
// never unwinds.
#[cfg(target_vendor = "apple")]
#[used]
#[unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")]
static BOOTSTRAP: extern "C" fn() = bootstrap;

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_os = "freebsd",
    target_vendor = "apple"
)))]
compile_error!(
    "kaban-vtab's `process-bootstrap` feature supports ELF (.init_array) and Mach-O \
     (__mod_init_func) targets only; on this target the startup allocator adapter cannot be \
     installed before main, so build without the feature and call initialize_for_embedding"
);
