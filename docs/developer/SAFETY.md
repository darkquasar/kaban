# Safety notes

Every workspace member forbids `unsafe` code (`unsafe_code = "forbid"` in
`[workspace.lints.rust]`), with one listed exception.

## The exception: `kaban-vtab`

`crates/kaban-vtab` is the only crate that may contain `unsafe` code (spec 11 QRY-07, OD-7).

**Why.** SQLite virtual tables and SQLite's allocator hooks are C interfaces. rusqlite's `vtab`
module exposes the first as `unsafe` traits and raw pointers, and installing request-scoped
allocation hooks goes through SQLite's raw configuration interface, so neither can be written in
safe Rust.

**What it may contain.** Two things and nothing else (PC-17, QRY-07):

- The virtual-table glue: the module implementation, cursor and row-production code.
- The allocator adapter: hooks that charge request-scoped SQLite allocations to a `QueryBudget`,
  the unsafe embedding initializer that installs them, and the process-level bootstrap behind the
  `process-bootstrap` feature, which installs them for the standalone CLI.

It does no I/O of its own, parses no SQL and holds no policy. Every `unsafe` block carries a
`SAFETY:` comment naming the invariant it relies on, and the crate carries
`#![deny(unsafe_op_in_unsafe_fn)]` so an `unsafe fn` body still needs explicit blocks.

**Allocator safety contract.** T12 implements the adapter against these obligations and lists
each `unsafe` block here with the one it meets:

- Exclusive initialization. The hooks are installed once, before any SQLite use and before other
  threads start, with no concurrent external SQLite initialization and no allocator replacement
  or shutdown while the runtime lives. A `OnceLock` coordinates kaban only; it is not proof that
  the host is excluded. The standalone CLI meets this through the adapter-owned pre-main
  bootstrap; unsupported constructor or linkage ordering refuses startup.
- Early-host refusal. If SQLite was initialized before the adapter, initialization returns a named
  error and never forces a shutdown and retry. Safe APIs consume an installed runtime and never
  install it lazily.
- Allocation ownership. The wrapper forwards to the original hooks and honours `xMalloc`,
  `xRealloc`, `xFree`, `xSize`, `xRoundup`, alignment and overhead. Each allocation keeps the
  budget it was charged to through `realloc` and `free`, including a free on another thread or
  after the query ends; a failed grow keeps the old block and its charge.
- No unwinding across FFI. Callbacks never unwind into C and never call back into SQLite.

**What exists (T12).** The crate holds the virtual-table glue (`src/adapter.rs`), the allocator
adapter (`src/allocator.rs`), the embedding initializer (`src/runtime.rs`), the request scope and
its dedicated query connection (`src/budget_scope.rs`) and, behind `process-bootstrap`, the startup
constructor (`src/bootstrap.rs`). `QueryBudget` itself is safe code in `kaban-core`.

```text
  +-------------------+  pre-main  +------------------------+  SQLITE_CONFIG_MALLOC
  | bootstrap [CLI]   | =========> | initialize_for_        | ====================+
  | .init_array.00101 |            | embedding (runtime.rs) |                     |
  +-------------------+            +------------------------+                     v
                                              | Runtime token          +-----------------+
                                              v                        | allocator.rs    |
  +-------------------+   enter    +------------------------+  slot    | header + lease  |
  | request (store)   | =========> | RequestScope           | ~~~~~~~> | per block       |
  +-------------------+            | + QueryConnection      |          +-----------------+
                                   +------------------------+                 ^
                                              | xFilter reads the slot         | charges
                                              v                                |
                                   +------------------------+  SQLite allocs   |
                                   | TableFunction::rows    | -----------------+
                                   | (&QueryBudget)         |
                                   +------------------------+
```

### Virtual-table glue

The glue has no `unsafe` block and no `unsafe fn`; it has exactly two `unsafe impl` blocks, both in
`crates/kaban-vtab/src/adapter.rs`:

| `unsafe impl` | rusqlite contract it meets | Invariants the adapter keeps |
|---|---|---|
| `VTab<'vtab> for FunctionTable` | The implementing struct is `#[repr(C)]` with `ffi::sqlite3_vtab` as its first field, so rusqlite can cast the `sqlite3_vtab` pointer SQLite passes back. | `FunctionTable` is `#[repr(C)]` and `base` is its first field. The module is eponymous-only (no `xCreate`), so `CREATE VIRTUAL TABLE` cannot instantiate it. |
| `VTabCursor for FunctionCursor<'_>` | The implementing struct is `#[repr(C)]` with `ffi::sqlite3_vtab_cursor` as its first field, so rusqlite can cast the cursor pointer. | `FunctionCursor` is `#[repr(C)]` and `base` is its first field. The cursor borrows its table for `'vtab`, which rusqlite's `open(&'vtab mut self)` ties to the table's lifetime. |

Both impls hold to the no-unwinding rule. rusqlite's virtual-table callbacks do not catch panics, so
every call into the safe `TableFunction` (`plan`, `rows`, each step of the row iterator and the
iterator's `Drop`) runs under `catch_unwind`; a panic in a call becomes a SQLite error naming the
function, and a panic while dropping the iterator is discarded. The adapter's own code has no
`unwrap`, `expect` or `panic!` (lint table). One gap remains: the `Drop` of the
`Arc<dyn TableFunction>` itself, when SQLite disconnects the table or frees the module's client
data, is not contained; a panic there aborts the process at the `extern "C"` boundary instead of
unwinding into C.

**How a budget reaches `rows`.** `register` takes no budget (the plan's interface). Each scan
(`xFilter`) reads the budget of the request scope active on the scanning thread (the allocator's
scope slot) and passes it to `rows` by reference; the adapter does not keep it, and the function
may clone it into its row iterator. SQLite calls `xFilter` on the thread that steps the statement,
and a `QueryConnection` cannot leave the thread of its scope (it borrows the `!Send`
`RequestScope`), so the slot names the request that owns the statement.

**No-scope rule.** A scan with no active scope on its thread is refused with
`"<function>: table function scanned outside a request budget scope"`. This is the narrowest
correct rule: a table function is part of the query surface, every query statement is bounded in
memory and time (QRY-04, QRY-06), and an invented unbounded budget would let a scan run unbounded
and unaccounted. Unscoped writers keep the original allocator behaviour for SQLite's own
allocations; they simply cannot scan a table function. (`EXPLAIN QUERY PLAN` and preparing a
statement do not call `xFilter`, so planning works outside a scope.)

### Allocator adapter

The adapter wraps the allocator SQLite had before it and forwards every hook to it. The embedding
initializer reads that allocator with `SQLITE_CONFIG_GETMALLOC`, records it once, and installs the
wrapper with `SQLITE_CONFIG_MALLOC`. It changes no process-wide heap limit (no
`sqlite3_hard_heap_limit64` or `sqlite3_soft_heap_limit64` call) and never calls
`sqlite3_shutdown`.

**Block layout.** Every block the wrapper hands to SQLite, scoped or not, has a 16-byte header in
front of it:

```text
  base (original xMalloc, 8-byte aligned)    base + 16 (what SQLite sees, 8-byte aligned)
  |                                           |
  v                                           v
  +-------------------------------------------+---------------------------------------+
  | Header { lease: Option<BudgetLease> }     | usable bytes = original xSize - 16    |
  | 16 bytes, align <= 8                      |                                       |
  +-------------------------------------------+---------------------------------------+
```

- The header holds the owner's `BudgetLease` (a `QueryBudget` handle and its charged bytes), or
  `None` for a block allocated with no budget scope on the thread. A compile-time assertion checks
  that the header type fits 16 bytes, needs at most 8-byte alignment, and that 16 equals
  `kaban_core::query_budget::ALLOCATION_METADATA_BYTES`.
- A scoped block is charged `QueryBudget::allocation_charge(usable, 8)`: the usable size rounded to
  8 plus the 16 header bytes, which is what the original allocator holds for it. The charge is
  reserved before the original `xMalloc` runs; if the original then reports a larger block, the
  lease grows to match, and a refused growth frees the block and fails the allocation.
- `xSize` reports the original size minus 16. `xRoundup(n)` is the original rounding of `n + 16`,
  minus 16, so `xSize(xMalloc(xRoundup(n))) == xRoundup(n)` whenever the original keeps that rule
  (SQLite's default allocator does: both are `ROUND8(n)`).
- Ownership travels with the block. `xRealloc` keeps the header's lease, charging growth to the
  owning budget, not to whatever scope is active at the time. `xFree` drops the lease after the
  original free, on whatever thread frees, so the charge returns to its owner even after the query
  ended. An unscoped block stays unscoped and uncharged; its realloc and free are forwarded
  unchanged apart from the header.
- A grow reserves the extra bytes first (`BudgetLease::grow_with`). A refused reservation or a
  failed original realloc returns null and leaves the old block, its bytes and its charge as they
  were.
- A charged shrink keeps the block in place and returns the same pointer, so the block's reported
  size and its charge stay equal. Re-reserving a smaller charge could be refused under an exhausted
  or cancelled budget and would record a false memory breach, so the adapter never does that.
- One inexact case is accepted: if an original allocator hands out more than the requested size on
  a grow and the budget refuses that slack, the block keeps the charge for the requested size.
  SQLite's built-in allocators never do this.

**Scope seam.** A thread-local slot holds the budget allocations on that thread are charged to.
`allocator::enter_scope(&QueryBudget) -> ScopeGuard` (crate-internal) sets it and the guard
restores the previous value on drop; guards on one thread drop in reverse order. The request-scoped
budget scope drives this seam. The tests drive it through the `#[doc(hidden)]` `test_hooks::scope`,
which is not a supported API.

**Callback rules.** Every callback body runs under `catch_unwind` and fails closed if it panics:
null from `xMalloc` and `xRealloc`, 0 from `xSize`, the request unchanged from `xRoundup`, nothing
from `xFree` and `xShutdown`, `SQLITE_ERROR` from `xInit`. The panic payload is leaked rather than
dropped, because its destructor could panic again. The code itself has no panicking path; the only
panic is the fault injected by the `test-hooks` feature, which tests use to show an allocation
fails instead of unwinding into SQLite. The callbacks never call into SQLite and never allocate
through it. They take no lock: the scope slot is a thread-local read with non-panicking
`try_with`/`try_borrow` (a slot already destroyed during thread exit reads as unscoped), and the
recorded original hooks are read with `OnceLock::get`, a lock-free load. Releasing a charge is an
atomic subtraction; dropping the last handle of a budget frees it through Rust's global allocator,
never through SQLite.

**Unsafe sites in the adapter and initializer.** Each carries a `SAFETY:` comment at the site.

| Site | Kind | Invariant relied on |
|---|---|---|
| `allocator.rs` `unsafe impl Send for AppData`, `unsafe impl Sync for AppData` | `unsafe impl` | The original allocator's `pAppData` is opaque to kaban and only handed back to that allocator's own `xInit`/`xShutdown`, which SQLite may call from any thread. |
| `allocator.rs` `x_malloc`, `x_free`, `x_realloc`, `x_size`, `x_roundup`, `x_init`, `x_shutdown` | `unsafe extern "C" fn` | Called only by SQLite as its allocator hooks, after `wrap` recorded the original allocator; pointers passed in are null or blocks the wrapper handed out. |
| `allocator.rs` `malloc`, `realloc`, `usable` | `unsafe fn` (callback bodies) | As for the hooks; `usable` needs a live original block. |
| calls to the original `xMalloc`, `xFree`, `xRealloc`, `xSize`, `xRoundup` | `unsafe` block | Each receives what SQLite's contract requires: a positive size, or a live original block (the header address of a wrapper block). |
| calls to the original `xInit`, `xShutdown` | `unsafe` block | Made exactly where SQLite would have called the original, with the original's own `pAppData`. |
| `Header` write after `xMalloc` | `unsafe` block | The fresh block is live, at least 16 bytes, 8-byte aligned (the original's contract; the header's alignment is at most 8); the slot is uninitialized, so nothing is dropped. |
| `Header` read in `xFree` | `unsafe` block | A wrapper block's header is initialized; the read takes ownership and the block is freed next, so the header is never read again. |
| `Header` read and write-back in `xRealloc` | `unsafe` block | The owner is moved out once (held in `ManuallyDrop`, so an unwind cannot drop it while header bytes still name it) and written to exactly one live block on every path: the moved block after a successful realloc (overwriting its stale bitwise copy without dropping it), or the untouched old block after a refused or failed grow or an in-place shrink. |
| `runtime.rs` `initialize_for_embedding`, `install` | `unsafe fn` | The caller's contract below. |
| `sqlite3_config(SQLITE_CONFIG_GETMALLOC, ...)` | `unsafe` block | The out-pointer is valid and exclusive for the call; with SQLite initialized it changes nothing and returns `SQLITE_MISUSE`. |
| `sqlite3_config(SQLITE_CONFIG_MALLOC, ...)` | `unsafe` block | SQLite copies the struct during the call. GETMALLOC succeeded just before, so SQLite is not initialized and no block from the replaced allocator exists for the wrapper to be handed. |

### Embedding initializer

`pub unsafe fn initialize_for_embedding() -> Result<Runtime, InitError>` installs the adapter and
returns the `Runtime` token; `Runtime::get()` hands out the installed token again on any thread. No
safe API configures SQLite, and nothing installs the adapter lazily.

**Safety contract (the caller's obligations).**

- Exclusive process startup: the call happens before any SQLite entry in the process, while no
  other thread can call into SQLite or `sqlite3_config`.
- No concurrent external SQLite use during the call. The internal `OnceLock` serializes kaban's own
  callers only; it does not exclude a host thread that calls SQLite at the same time.
- No allocator replacement (`SQLITE_CONFIG_MALLOC`) and no `sqlite3_shutdown` after a successful
  call, for as long as the `Runtime` lives, which is until process exit.

**Refusals, all by name (`InitError`).**

- `AlreadyInitialized`: any call after the first, successful or not. The initializer runs once per
  process and is never retried.
- `HostInitializedSqlite`: SQLite was initialized before the call (for example the host opened a
  connection first; rusqlite's first open initializes SQLite). `sqlite3_config` then returns
  `SQLITE_MISUSE`. SQLite is left running and untouched; the adapter never shuts SQLite down to
  retry.
- `OriginalIncomplete { hook }`: the current allocator lacks a hook the wrapper forwards to.
- `ConfigRefused { option, code }`: `sqlite3_config` refused an option with another code.

**Not supported.**

- A table-function scan outside a request scope (refused by name).
- `process-bootstrap` on targets without an ELF or Mach-O constructor section (compile error).
- Concurrent embedding initialization is unsupported. Calling the initializer while another thread
  may use SQLite, or from two threads kaban does not coordinate, breaks the contract above and is
  undefined behaviour; the `OnceLock` does not make it safe.
- Initializing after the host used SQLite: refused, never repaired.
- Replacing the allocator, or shutting SQLite down, after installation.
- Per-query heap limits through SQLite's process-wide hard or soft heap limit.

**Tests.** `crates/kaban-vtab/tests/runtime.rs` and `tests/allocator.rs` run every case in a fresh
child process of the test binary (it re-executes itself with `--exact <case>` and an environment
marker), because the allocator can be configured only once per process and before SQLite
initializes. They cover installation before any SQLite entry, repeated token access, refusal of a
host that used SQLite first (SQLite still works afterwards), refusal of a second initialization,
alignment, `xSize`/`xRoundup` consistency, exact charges, failed grow, grow, shrink, cross-thread
and post-scope frees, a closed connection releasing everything it charged, simultaneous scoped
queries beside an unscoped writer, unscoped forwarding, and a panicking charge that fails the
allocation without unwinding. Miri and sanitizers are not run on this crate yet.

With `process-bootstrap` on, the startup constructor has already installed the adapter before
each child's `main`, so `tests/runtime.rs` (the embedding initializer's own cases) and the
allocator case that observes the original allocator before installation are compiled out; their
coverage comes from the default-feature run, and `tests/bootstrap.rs` covers the feature build.

### Request scope and the dedicated query connection

`RequestScope::enter(Runtime, &QueryBudget)` is the public scope API over the allocator's scope
seam. It takes the `Runtime` token, so it proves the adapter is installed and never configures
SQLite or installs anything. While the scope lives, SQLite allocations on its thread and every
table-function scan on it are charged to the budget. The scope is `!Send`; scopes on one thread
nest and drop in reverse order.

- **Guards survive cancellation.** The scope's guard restores the thread's previous scope in
  `Drop`, so an unwinding request (a panic, or a dropped future that owned the scope) leaves the
  thread unscoped again. Leases live in the block headers, not in the scope: a block allocated
  inside the request and freed after it, on any thread, returns its charge to the request's
  budget then.
- **Open to close inside the scope.** `RequestScope::open_query_connection(path, flags)` opens the
  request's connection inside the scope and returns a `QueryConnection<'scope>` that borrows the
  scope. The borrow checker therefore keeps preparing, stepping, finalizing (statements borrow the
  connection) and closing (`QueryConnection::close` or its `Drop`) inside the scope; the
  connection cannot outlive the request, so no statement cache, page cache or schema it holds is
  handed to a later request. A test shows the budget back at zero after close and the next request
  paying for its own connection again.
- **Connection settings.** Lookaside is disabled with `SQLITE_DBCONFIG_LOOKASIDE` (null buffer,
  zero size, zero slots) right after open, so every allocation goes through the charged allocator;
  `SQLITE_LIMIT_WORKER_THREADS` is set to 0, so no sorter thread allocates outside the scope's
  thread; a progress handler checks `QueryBudget::check()` every 1000 virtual-machine steps and
  interrupts the statement once the deadline passed, the request was cancelled or a limit was
  recorded. These are per-connection settings: no process-global hard or soft heap limit, and no
  global lookaside or thread setting, changes per query. Connections that are not opened through a
  scope (writers) keep SQLite's defaults and original allocation behaviour.
- **Error translation.** `RequestScope::translate(rusqlite::Error) -> QueryError` maps
  `SQLITE_NOMEM` and `SQLITE_INTERRUPT` to `QueryError::Limit(BudgetExceeded)` only when this
  budget's `check()` reports an exceeded limit (memory, time or cancellation, the first one
  recorded); the message names it (`memory limit exceeded`, `time limit exceeded`, `cancelled`).
  Any other error, and an out-of-memory or interrupt the budget did not cause, stays
  `QueryError::Sqlite` unchanged.

The one `unsafe` block is the `sqlite3_db_config(SQLITE_DBCONFIG_LOOKASIDE, ...)` call: the
handle is the live connection's, used on its own thread for the call, and the variadic arguments
are exactly the `(void *, int, int)` the option takes; SQLite refuses with `SQLITE_BUSY` rather
than free slots in use (just after open none are).

### Process bootstrap (`process-bootstrap`)

The standalone CLI enables the feature (`crates/kaban/Cargo.toml`); library users call
`initialize_for_embedding` themselves. `src/bootstrap.rs` places one function pointer in the
platform's startup-constructor array; the loader calls it before `main`:

- **ELF (Linux, Android, FreeBSD): `.init_array.00101`.** Linkers sort `.init_array.NNNNN`
  entries by priority and place them before unprioritized `.init_array` entries; 0 to 100 are
  reserved for the implementation (the standard library's argument capture uses 00099), so 00101
  is the earliest priority a program may take. Shared-library constructors (libc, libm,
  libgcc_s, the loader) run before any executable constructor; SQLite is linked statically into
  the binary, so none of them can reach it.
- **Mach-O (Apple targets): `__DATA,__mod_init_func`.** No priorities exist; dyld runs entries in
  link order, so a dependency's constructor may run first. It is fail-closed: any earlier SQLite
  initialization is detected below. Not exercised on this project's CI machines.
- **Every other target: `compile_error!`.** There is no constructor section to place the entry in,
  and a silent no-install would leave every query unbudgeted. A compile error is explicit at build
  time rather than at first query; such a build turns the feature off and calls
  `initialize_for_embedding` from its own startup.

**What the constructor does.** It calls `initialize_for_embedding()` under `catch_unwind`. On
success it returns (and writes `kaban-vtab: sqlite allocator adapter installed before main` to
stderr only when `KABAN_VTAB_BOOTSTRAP_REPORT=1`). On `Err` (for example `HostInitializedSqlite`,
the named startup error when an earlier hook already initialized SQLite) or a panic it writes
`kaban-vtab: startup refused: <error>` to stderr and calls `std::process::abort()`; it never
unwinds out of an init-array entry and the process never reaches `main` unbudgeted. The panic
payload is leaked, not dropped.

**Linking.** rustc links a dependency only when the binary names it. The CLI named nothing from
`kaban-vtab` (`kaban-store-sqlite` depends on it but does not use it yet), so the constructor was
not in the binary: `nm` found 0 `kaban_vtab` symbols and the CLI test failed. `main` therefore
calls `kaban_vtab::bootstrap::ensure_linked()` first (a safe no-op that keeps the constructor's
static alive); with it the constructor is the fourth `.init_array` entry and the CLI test passes.

**Startup-hook review (the kaban binary's dependency set).** The `.init_array` of the dev build of
`kaban` (aarch64-unknown-linux-gnu, PIE, `R_AARCH64_RELATIVE` relocations resolved against the
symbol table) holds five entries, in this order; there is no `.preinit_array` and no `.ctors`:

| # | Entry | Origin | SQLite use |
|---|---|---|---|
| 1 | `init_have_lse_atomics` | compiler runtime (outline atomics CPU probe) | none |
| 2 | `__init_cpu_features_constructor` | compiler runtime (CPU feature probe) | none |
| 3 | `std::sys::args::unix::imp::ARGV_INIT_ARRAY::init_wrapper` | standard library, `.init_array.00099` (captures argv) | none |
| 4 | `kaban_vtab::bootstrap::bootstrap` | this crate, `.init_array.00101` | installs the adapter |
| 5 | `frame_dummy` | C runtime `crtbegin.o` (unwind-frame registration) | none |

No crate in the dependency set (tokio, tantivy, rmcp, tracing and the rest) registers a
constructor, no constructor starts a thread, and the binary imports no `sqlite3_*` symbol
dynamically. The review is of this build; a dependency added later that registers a constructor
is caught only if it initializes SQLite (refused by name, abort) and must otherwise be reviewed
here. The test `tests/bootstrap_refusal.rs` proves the refusal: a test-side constructor at
`.init_array.00100` calls `sqlite3_initialize` first, and the child aborts with SIGABRT before
`main`, its stderr naming the host-initialized error.

**Standalone CLI check and its cost.** `crates/kaban/tests/vtab_bootstrap.rs` runs the real
`kaban --version` with `KABAN_VTAB_BOOTSTRAP_REPORT=1` and requires the report line as the first
stderr line (only the constructor writes it, and it runs before `main`), and requires no line
without the variable. The cost is one environment-variable read in every CLI start and a
documented, kaban-vtab-owned variable; no CLI flag, doctor line or other CLI code is added beyond
the one `ensure_linked()` call. Writing to stderr before `main` happens only under the variable or
on refusal.

### Unsafe sites added by the scope and the bootstrap

| Site | Kind | Invariant relied on |
|---|---|---|
| `budget_scope.rs` `disable_lookaside`: `sqlite3_db_config(conn.handle(), SQLITE_DBCONFIG_LOOKASIDE, null, 0, 0)` | `unsafe` block | Live handle on its own thread; the variadic arguments match the option's `(void *, int, int)`; SQLite refuses with `SQLITE_BUSY` instead of freeing slots in use. |
| `bootstrap.rs` `bootstrap`: `initialize_for_embedding()` | `unsafe` block | Runs from the init array before `main`: one thread and no SQLite entry unless an earlier hook made one, which is detected and refused (abort); the startup-hook review above shows no earlier hook touches SQLite or starts a thread; kaban never replaces the allocator or shuts SQLite down. |
| `bootstrap.rs` `#[unsafe(link_section = ".init_array.00101")] static BOOTSTRAP` | unsafe attribute | The section is an array of startup function pointers; the static is exactly one, to an `extern "C" fn()` that reads no arguments and never unwinds. |
| `bootstrap.rs` `#[unsafe(link_section = "__DATA,__mod_init_func,mod_init_funcs")] static BOOTSTRAP` | unsafe attribute | As above, for dyld's initializer array. |

### Residual risks (for the end review)

- A panic in the `Drop` of the `Arc<dyn TableFunction>` when SQLite disconnects the table or frees
  the module's client data is not contained; it aborts at the `extern "C"` boundary.
- An original allocator that over-allocates on a grow, with the budget refusing the slack, leaves
  the block charged for the requested size only (SQLite's built-in allocators never over-allocate).
- No Miri and no sanitizers run on this crate (neither is installed on the bench machines); the
  evidence is the fresh-process tests and this review.
- The startup-hook review covers the current Linux build only; Mach-O ordering is link order and
  untested here. A future constructor that starts a thread which later calls SQLite concurrently
  with the bootstrap would not be detected.
- Pre-`main` use of the standard library (environment read, stderr write, `abort`) relies on
  glibc and std behaving before Rust's runtime setup, as they do in practice; SIGPIPE is not yet
  ignored then, so a closed stderr pipe under the report variable or on refusal ends the process
  by signal.
- A table function's `FnError` that wraps a budget refusal stays a SQLite error whose text names
  the limit (`Memory`); `translate` does not turn it into `QueryError::Limit`, because SQLite
  reports it as a module error, not `SQLITE_NOMEM`.

### Enforcement

**How it is held to this.**

- The manifest does not inherit the workspace lint table. It restates every workspace lint except
  `unsafe_code` (Cargo has no `workspace = false` for `[lints]`, so the key is omitted and the
  `[lints.*]` tables stand alone).
- `crates/kaban/tests/lints.rs` (`no_unsafe_in_new_crates`, `unsafe_scan_covers_every_member_except_vtab`)
  scans every other member's sources and fails if the scanned set is not exactly the workspace
  members minus `crates/kaban-vtab/`.
- `crates/kaban/tests/sql_boundary.rs` lists `crates/kaban-vtab/` among the crates that may carry
  SQL; no other new crate may.
- `scripts/ci/ci.sh` (`store_boundary_deps`) accepts `kaban-vtab` as a direct parent of `rusqlite`
  beside `kaban-store-sqlite` and `kaban-codegraph`, and no other.

## Dependencies

`unsafe` inside a mature dependency is allowed case by case, with a licence and audit note in the
manifest task that adds it. File mapping goes through tantivy's utility only; no second mmap
crate is added (OD-7).
