# Safety notes

Every workspace member forbids `unsafe` code (`unsafe_code = "forbid"` in
`[workspace.lints.rust]`), with one listed exception.

## The exception: `kaban-vtab`

`crates/kaban-vtab` is the only crate that may contain `unsafe` code (spec 11 QRY-07, OD-7).

**Why.** SQLite virtual tables and SQLite's allocator hooks are C interfaces. rusqlite's `vtab`
module exposes the first as `unsafe` traits and raw pointers, and installing request-scoped
allocation hooks goes through SQLite's raw configuration interface, so neither can be written in
safe Rust.

**What it may contain.** Three things and nothing else (PC-8, PC-17, QRY-07):

- The virtual-table glue: the module implementation, cursor and row-production code.
- The allocator adapter: hooks that charge request-scoped SQLite allocations to a `QueryBudget`,
  the unsafe embedding initializer that installs them, the request scope and its dedicated query
  connection, and the process-level bootstrap behind the `process-bootstrap` feature, which
  installs them for the standalone CLI.
- The byte-owner adapter behind `retain_owner`: it attaches an owner (a pin's lock) to an
  `OwnedBytes` span, so the owner lives as long as any clone or slice of the span.

It parses no SQL and holds no policy. Its only I/O is the request's database connection, which it
opens through SQLite (`RequestScope::open_query_connection`), and, behind `process-bootstrap`, one
stderr line from the startup constructor (the opt-in report, or the refusal before an abort). Every
`unsafe` block carries a `SAFETY:` comment naming the invariant it relies on, and the crate carries
`#![deny(unsafe_op_in_unsafe_fn)]` so an `unsafe fn` body still needs explicit blocks.

**Allocator safety contract.** The adapter is implemented against these obligations; each `unsafe`
site is listed below with the one it meets:

- Exclusive initialization. The hooks are installed once, before SQLite is initialized and while
  no other thread can call into SQLite, with no allocator replacement or shutdown afterwards. A
  `OnceLock` coordinates kaban only; it is not proof that the host is excluded. The standalone CLI
  meets this through the adapter-owned pre-main bootstrap on the audited Linux build.
- Early-host refusal. If SQLite was initialized before the adapter, initialization returns a named
  error and never forces a shutdown and retry. Safe APIs need an installed runtime (`register`
  checks it, `RequestScope::enter` takes the token) and never install it lazily.
- Allocation ownership. The wrapper forwards to the original hooks and honours `xMalloc`,
  `xRealloc`, `xFree`, `xSize`, `xRoundup` (including its refusals), alignment and overhead. Each
  allocation keeps the budget it was charged to through `realloc` and `free`, including a free on
  another thread or after the query ends; a failed or refused grow keeps the old block and its
  charge; a successful grow is charged for every byte the original handed out.
- No unwinding across FFI. The adapter's callbacks never unwind into C and never call back into
  SQLite; table-function code (metadata, plans, rows, destructors, panic payloads) runs contained.

**What exists (T12).** The crate holds the virtual-table glue (`src/adapter.rs`, `src/plan.rs`,
`src/value.rs`), the allocator adapter (`src/allocator.rs`), the embedding initializer
(`src/runtime.rs`), the request scope and its dedicated query connection (`src/budget_scope.rs`)
and, behind `process-bootstrap`, the startup constructor (`src/bootstrap.rs`). `QueryBudget` itself
is safe code in `kaban-core`.

```text
  +-------------------+  pre-main  +------------------------+  SQLITE_CONFIG_MALLOC
  | bootstrap [CLI]   | =========> | initialize_for_        | ====================+
  | .init_array.00101 |            | embedding (runtime.rs) |                     |
  +-------------------+            +------------------------+                     v
                                              | Runtime token          +-----------------+
                                              v                        | allocator.rs    |
  +-------------------+   enter    +------------------------+  slot    | header: owner + |
  | request (store)   | =========> | RequestScope (one per  | ~~~~~~~> | lease per block |
  +-------------------+            | thread) + QueryConn.   |          +-----------------+
                                   +------------------------+                 ^
                                              | xBestIndex / xFilter           | charges
                                              v read the slot                  |
                                   +------------------------+  copies, SQLite  |
                                   | TableFunction::plan,   | -----------------+
                                   | rows(&Scan, &Budget)   |
                                   +------------------------+
```

### Virtual-table glue

The glue has no `unsafe` block and no `unsafe fn`; it has exactly two `unsafe impl` blocks, both in
`crates/kaban-vtab/src/adapter.rs`:

| `unsafe impl` | rusqlite contract it meets | Invariants the adapter keeps |
|---|---|---|
| `VTab<'vtab> for FunctionTable` | The implementing struct is `#[repr(C)]` with `ffi::sqlite3_vtab` as its first field, so rusqlite can cast the `sqlite3_vtab` pointer SQLite passes back. | `FunctionTable` is `#[repr(C)]` and `base` is its first field. No reference into a `FunctionTable` outlives the callback that made it (cursor ownership below). The module is eponymous-only (no `xCreate`), so `CREATE VIRTUAL TABLE` cannot instantiate it. |
| `VTabCursor for FunctionCursor<'_>` | The implementing struct is `#[repr(C)]` with `ffi::sqlite3_vtab_cursor` as its first field, so rusqlite can cast the cursor pointer. | `FunctionCursor` is `#[repr(C)]` and `base` is its first field. It holds no reference into the `sqlite3_vtab` allocation. |

**Cursor ownership (CRIT-01).** The objects and who may write them:

```text
  +---------------------------+         +------------------------------+
  | FunctionTable (rusqlite   |  Arc    | Registered (Arc block, made  |
  | Box, one per connection)  | ------> | once by `register`):         |
  | base: sqlite3_vtab        |         | function, name, columns,     |
  |   SQLite writes nRef,     |         | schema, slots, arguments     |
  |   zErrMsg                 |         | never mutated after creation |
  +---------------------------+         +------------------------------+
                                           ^ Arc (_keep)   ^ &'vtab Registered
  +---------------------------+            |               |
  | FunctionCursor (Box, one  | -----------+---------------+
  | per xOpen) base, args,    |
  | rows, current             |
  +---------------------------+
```

- The function and everything registration read from it live in `Registered`, a separate heap
  allocation behind an `Arc`, created once and never mutated. The module's client data, every
  connection's `FunctionTable` and every cursor hold an `Arc` handle on it.
- A cursor stores `&'vtab Registered` and its own `Arc` clone. The reference points into the `Arc`'s
  heap block, never into the `FunctionTable` (whose first field is the `sqlite3_vtab`), and the
  cursor's own `Arc` keeps that block alive for the cursor's whole life, whatever order SQLite
  disconnects the table and closes cursors in. The row iterator borrows the function through that
  reference.
- `OP_VOpen` writes `pVtab->nRef` right after `xOpen` returns, and an error path writes `zErrMsg`
  (rusqlite's `set_err_msg`, after the method returned). No Rust reference into the
  `FunctionTable` is live at either point: `best_index`'s `&self` and `open`'s `&'vtab mut self`
  are used only during their call (the `'vtab` borrow of `open` yields the reference to
  `Registered`, whose referent is the other allocation).
- rusqlite's `rust_open` makes a fresh `&mut FunctionTable` for every cursor while earlier cursors
  are open (a self-join, interleaved statements). Earlier cursors hold no reference into that
  `FunctionTable`, so the new `&mut` aliases nothing they read; what they read (`Registered`) is
  only ever read, by anyone.
- Tests (`tests/cursors.rs`) run a self-join, interleave two statements and fail one cursor while
  another is open, then check the connection still works and closes with nothing charged. They
  are regression coverage: passing them does not prove the absence of aliasing, which rests on
  the argument above. Miri cannot run these paths (they go through SQLite's C code over FFI), so
  no Miri evidence exists.

**Panic containment (MAJ-01).** rusqlite's virtual-table callbacks do not catch panics, so every
call into table-function code is contained:

- `name()` and `columns()` run once, at `register`, under `catch_unwind`; a panic there is a
  registration error (`table function: name panicked`, `<name>: columns panicked`). Callbacks
  only read the validated snapshot, so a stateful `columns()` cannot change the declaration (or
  bypass the 31-argument cap) after validation.
- `plan`, `rows` and each step of the row iterator run under `catch_unwind`; a panic becomes a
  SQLite error `"<name>: <call> panicked"`.
- The row iterator's `Drop` and the function's own final `Drop` (in `xDisconnect` or SQLite's
  module destructor, through the `ContainedFunction` owner) run under `catch_unwind` and a panic
  there is discarded.
- A caught panic payload is leaked (`mem::forget`), never dropped: its destructor is
  table-function code too and could panic again outside any containment.
- Fresh-process tests (`tests/panics.rs`) cover each case, including a payload whose `Drop`
  panics and a function whose `Drop` panics at connection close; the child process must survive.
  The adapter's own code has no `unwrap`, `expect` or `panic!` (lint table).

**How a budget reaches the function.** `register` takes no budget (the plan's interface) but
refuses by name when the process has no installed runtime. Planning (`xBestIndex`) and each scan
(`xFilter`) read the budget of the request scope active on the thread (the allocator's scope
slot); a plan or scan with no active scope is refused with
`"<name>: table function planned|scanned outside a request budget scope"`. The scan hands the
budget to `rows` by reference. At most one request scope is active on a thread
(`RequestScope::enter` refuses a second, `ScopeActive`), and a `QueryConnection` cannot leave the
thread of its scope (it borrows the `!Send` `RequestScope`), so the slot names the request whose
connection runs the statement: a connection's work is never charged to another request.

**Request memory the adapter owns (MAJ-03).**

- Charged to the request: every TEXT or BLOB value the adapter copies into Rust (a plan's
  right-hand values, a scan's arguments and consumed constraint values), reserved before the copy
  at `QueryBudget::allocation_charge(len, 8)` (invalid UTF-8 at three bytes per input byte, the
  lossy decoding's bound). Plan-time copies are held for the plan call; scan copies stay charged on
  the cursor until the next scan or the close. A refused reservation fails the plan or scan with
  `"<name>: copying argument <arg> refused: query budget exceeded: Memory (...)"` before any copy.
- Owned by the function: the rows it produces and its working memory; it reserves through the
  `&QueryBudget` it receives.
- Not charged, bounded by the declaration or by what SQLite already charged: the registration
  snapshot (made once per `register`, outside any request), the `FunctionTable` and cursor boxes
  rusqlite allocates (fixed size), and the planner's bookkeeping vectors (one entry per declared
  argument or offered constraint, at most 31 and 32).

**Result-column constraints (MAJ-05).** `PlanInput::constraints` lists the usable `=`, `!=`, `<`,
`<=`, `>`, `>=` constraints on result columns (at most 32), with the plan-time value when SQLite
knows it. `Plan::consumes` selects those the scan receives (`Scan::bound`, after the arguments in
argv order) and `Plan::omits` those the function guarantees, the only ones SQLite then skips
checking; `Plan::strategy` reaches the scan as `Scan::strategy`. The strategy and the consumed
columns travel to `xFilter` in `idxStr` (`"<strategy> <column>:<op> ..."`), which SQLite frees.

### Allocator adapter

The adapter wraps the allocator SQLite had before it and forwards every hook to it. The embedding
initializer reads that allocator with `SQLITE_CONFIG_GETMALLOC`, records it once, and installs the
wrapper with `SQLITE_CONFIG_MALLOC`. It changes no process-wide heap limit (no
`sqlite3_hard_heap_limit64` or `sqlite3_soft_heap_limit64` call) and never calls
`sqlite3_shutdown`.

**Block layout.** Every block the wrapper hands to SQLite, scoped or not, has a 24-byte header in
front of it:

```text
  base (original xMalloc, 8-byte aligned)    base + 24 (what SQLite sees, 8-byte aligned)
  |                                           |
  v                                           v
  +-------------------------------------------+---------------------------------------+
  | Header { owner: Option<Owner> }           | usable bytes = original xSize - 24    |
  | Owner { budget: QueryBudget,              |                                       |
  |         lease: BudgetLease }  24 bytes    |                                       |
  +-------------------------------------------+---------------------------------------+
```

- The header holds the owner (the budget, kept so a grow can reserve a replacement against it, and
  the block's `BudgetLease`), or `None` for a block allocated with no scope on the thread.
  Compile-time assertions check that the header fits 24 bytes, needs at most 8-byte alignment and
  holds at least `kaban_core::query_budget::ALLOCATION_METADATA_BYTES` (16).
- A scoped block is charged `QueryBudget::allocation_charge(usable + 8, 8)`: the 16 metadata bytes
  that function adds plus the header's other 8, so the charge is exactly what the original holds
  for it. The charge is reserved before the original `xMalloc` runs; if the original reports a
  larger block, the lease grows to match, and a refused growth frees the block and fails the
  allocation.
- `xSize` reports the original size minus 24. `xRoundup(n)` is the original rounding of `n + 24`,
  minus 24, so `xSize(xMalloc(xRoundup(n))) == xRoundup(n)` whenever the original keeps that rule.
  A refusal is preserved (MED-01): `xRoundup` returns 0 when the original's rounding refuses the
  size (returns 0 or less than asked), when the request is not positive or overflows with the
  header, or on a panic; `xMalloc` and `xRealloc` refuse such a size (they check the original's
  rounding themselves too) without forwarding anything, and a refused `xRealloc` keeps the block.
- Ownership travels with the block. `xFree` drops the owner after the original free, on whatever
  thread frees, so the charge returns to its budget even after the query ended. An unscoped block
  stays unscoped and uncharged; its realloc and free are forwarded unchanged apart from the header.
- A scoped grow is a replacement, never a destructive resize (MAJ-04): the new block is reserved
  against the owner's budget while the old block keeps its charge (the copy's peak is charged),
  allocated with the original `xMalloc`, charged for every byte the original handed out, filled
  with the old bytes, and only then is the old block freed and its charge released. Any refusal
  (the reservation, the slack, a failed allocation) frees the replacement and leaves the old block,
  its bytes and its charge exactly as they were. Scoped growth therefore never uses the original
  `xRealloc`, and an original that over-allocates (as Apple's zone allocator does under SQLite's
  8-byte `xRoundup`) never leaves a block undercharged.
- A charged shrink keeps the block in place and returns the same pointer, so the block's reported
  size and its charge stay equal. Re-reserving a smaller charge could be refused under an exhausted
  or cancelled budget and would record a false memory breach, so the adapter never does that.

**Scope seam.** A thread-local slot holds the budget allocations on that thread are charged to.
`allocator::try_enter_scope(&QueryBudget) -> Option<ScopeGuard>` (crate-internal) sets it only
when the slot is empty, and the guard clears it on drop. `RequestScope::enter` is its only caller;
no public API sets the slot.

**Callback rules.** Every callback body runs under `catch_unwind` and fails closed if it panics:
null from `xMalloc` and `xRealloc`, 0 from `xSize` and `xRoundup`, nothing from `xFree` and
`xShutdown`, `SQLITE_ERROR` from `xInit`. The panic payload is leaked rather than dropped. The code
itself has no panicking path; the only panic is the fault injected by the `test-hooks` feature.
The callbacks never call into SQLite and never allocate through it. Locks and blocking, exactly:

- The wrapper's own state takes no lock: the scope slot is a thread-local read with
  non-panicking `try_with`/`try_borrow` (a slot already destroyed during thread exit reads as
  unscoped), and the recorded original hooks are read with `OnceLock::get`, a lock-free load.
- A charge goes through `QueryBudget::reserve`, an atomic compare-exchange loop. When it refuses,
  `QueryBudget::record` stores the first breach with `OnceLock::get_or_init`, which can wait
  briefly for another thread recording a breach of the same budget at the same moment.
- Releasing a charge is an atomic subtraction; dropping the last handle of a budget frees it
  through Rust's global allocator, never through SQLite. A scoped grow allocates nothing in Rust.
- The forwarded original hooks may lock or block as that allocator does; SQLite already holds its
  own memory mutex around them when memory statistics are on.

**Unsafe sites in the adapter and initializer.** Each carries a `SAFETY:` comment at the site.

| Site | Kind | Invariant relied on |
|---|---|---|
| `allocator.rs` `unsafe impl Send for AppData`, `unsafe impl Sync for AppData` | `unsafe impl` | The original allocator's `pAppData` is opaque to kaban and only handed back to that allocator's own `xInit`/`xShutdown`, which SQLite may call from any thread. |
| `allocator.rs` `x_malloc`, `x_free`, `x_realloc`, `x_size`, `x_roundup`, `x_init`, `x_shutdown` | `unsafe extern "C" fn` | Called only by SQLite as its allocator hooks, after `wrap` recorded the original allocator; pointers passed in are null or blocks the wrapper handed out, and sizes passed to `xRealloc` came from `xRoundup`. |
| `allocator.rs` `malloc`, `realloc`, `usable` | `unsafe fn` (callback bodies) | As for the hooks; `usable` needs a live original block. |
| calls to the original `xMalloc`, `xFree`, `xRealloc`, `xSize`, `xRoundup` | `unsafe` block | Each receives what SQLite's contract requires: a positive size the original's rounding accepts, or a live original block (the header address of a wrapper block). `xRoundup` is a pure size computation. |
| calls to the original `xInit`, `xShutdown` | `unsafe` block | Made exactly where SQLite would have called the original, with the original's own `pAppData`. |
| `Header` write after `xMalloc` and on the replacement block | `unsafe` block | The fresh block is live, at least 24 bytes, 8-byte aligned (the original's contract; the header's alignment is at most 8); the slot is uninitialized, so nothing is dropped. |
| `Header` read in `xFree` | `unsafe` block | A wrapper block's header is initialized; the read takes ownership and the block is freed next, so the header is never read again. |
| shared borrow of the `Header` in `xRealloc` | `unsafe` block | The block is live and its header initialized; SQLite does not use a block concurrently with its realloc, so nothing writes the header while the borrow lives; the borrow ends before the header is moved out. |
| `copy_nonoverlapping` from the old block to the replacement in `xRealloc` | `unsafe` block | Both blocks are live and distinct; the copied length is at most the usable size of each. |
| `Header` read of the old block after the replacement in `xRealloc` | `unsafe` block | The old header is still initialized; the read takes ownership, the old block is freed next and its header never read again; its charge is released after the free. |
| `runtime.rs` `initialize_for_embedding`, `install` | `unsafe fn` | The caller's contract below. |
| `sqlite3_config(SQLITE_CONFIG_GETMALLOC, ...)` | `unsafe` block | The out-pointer is valid and exclusive for the call; with SQLite initialized it changes nothing and returns `SQLITE_MISUSE`. |
| `sqlite3_config(SQLITE_CONFIG_MALLOC, ...)` | `unsafe` block | SQLite copies the struct during the call. GETMALLOC succeeded just before, so SQLite is not initialized and no block from the replaced allocator exists for the wrapper to be handed. |

### Embedding initializer

`pub unsafe fn initialize_for_embedding() -> Result<Runtime, InitError>` installs the adapter and
returns the `Runtime` token; `Runtime::get()` hands out the installed token again on any thread. No
safe API configures SQLite, and nothing installs the adapter lazily.

**Safety contract (the caller's obligations), sufficient preconditions.**

- SQLite is not initialized yet in this process, or it was initialized by the host (then the call
  is refused by name and changes nothing). Before the call the host may only have configured
  SQLite with `sqlite3_config` on this thread, for example `SQLITE_CONFIG_MALLOC` to supply the
  original allocator the wrapper records; that original must keep the allocator contract below.
- No other thread calls into SQLite (any API, including `sqlite3_config` and the implicit
  initialization of a first open) during the call. The internal `OnceLock` serializes kaban's own
  callers only; it does not exclude a host thread.
- After a successful call, for as long as the `Runtime` lives (until process exit): no allocator
  replacement (`SQLITE_CONFIG_MALLOC`), no `SQLITE_CONFIG_HEAP` or other memory reconfiguration,
  and no `sqlite3_shutdown`.

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

- Use: a table-function plan or scan outside a request scope (refused by name); registration
  without the installed runtime (refused by name); a second request scope on a thread (refused,
  `ScopeActive`); a shared-cache query connection (refused, `QueryError::SharedCache`); SQLite
  work on a thread that has a scope but does not belong to that request (for example another
  local task interleaved on the same thread): it is charged to the scope's request.
- Initialization: concurrent embedding initialization (calling the initializer while another
  thread may use SQLite, or from two threads kaban does not coordinate) breaks the contract and is
  undefined behaviour, the `OnceLock` does not make it safe; initializing after the host used
  SQLite (refused, never repaired); replacing the allocator or shutting SQLite down after
  installation; per-query heap limits through SQLite's process-wide hard or soft heap limit.
- Allocators: an original allocator that breaks SQLite's `sqlite3_mem_methods` contract (8-byte
  aligned blocks, `xSize` of a live block, a pure `xRoundup`, thread safety in a
  `SQLITE_THREADSAFE=1` build) or calls back into SQLite; SQLite's own fixed-heap allocators
  (`SQLITE_CONFIG_HEAP`, memsys3/memsys5), which this build does not compile in.
- Linkage: one statically linked SQLite (rusqlite's bundled build) per process. A dynamically
  linked SQLite, or a second SQLite copy in the process, has its own allocator configuration that
  the wrapper never sees.
- Platforms: the startup bootstrap is audited on the Linux build only; Mach-O ordering is untested;
  `process-bootstrap` does not compile on targets without an ELF or Mach-O constructor section.
  The embedding initializer has no platform-specific code.

**Tests.** Every case that needs the adapter installed runs in a fresh child process of its test
binary (it re-executes itself with `--exact <case>` and an environment marker), because the
allocator can be configured only once per process and before SQLite initializes; a child that runs
past 60 s is killed and fails its case. `tests/runtime.rs` covers installation, repeated token
access, refusal of a host that used SQLite first, a second initialization and registration
without a runtime; `tests/allocator.rs` alignment, `xSize`/`xRoundup` consistency, exact charges,
failed grow, grow, shrink, cross-thread and post-scope frees, a closed connection releasing
everything, simultaneous scoped queries beside an unscoped writer, unscoped forwarding and (with
`test-hooks`) a panicking charge; `tests/original.rs` a deterministic over-allocating original
(moving and in-place grows, refused slack, exact charges, release) and one whose `xRoundup`
refuses sizes (no allocation attempt reaches it). Miri cannot run these paths (FFI into SQLite's C
code); Valgrind memcheck runs them (see "Memory checker" under residual risks).

With `process-bootstrap` on, the startup constructor has already installed the adapter before each
child's `main`, so `tests/runtime.rs`, `tests/original.rs` and the allocator case that observes the
original allocator before installation are compiled out; their coverage comes from the
default-feature run (`cargo nextest run -p kaban-vtab`), and `tests/bootstrap.rs` covers the
feature build. The default CI shape, `cargo nextest run --workspace`, unifies features so that
`kaban-vtab` builds with `process-bootstrap` on (the CLI enables it): there these embedding-only
cases do not run, so CI keeps them only if it also runs `cargo nextest run -p kaban-vtab`. The
fault-injection case does run there: the crate's own dev-dependency on itself turns `test-hooks`
on in every build of its tests, and in no non-test build.

### Request scope and the dedicated query connection

`RequestScope::enter(Runtime, &QueryBudget) -> Result<RequestScope, ScopeActive>` is the public
scope API over the allocator's scope seam. It takes the `Runtime` token, so it proves the adapter
is installed and never configures SQLite or installs anything. While the scope lives, SQLite
allocations on its thread and every table-function plan and scan on it are charged to the budget.
The scope is `!Send`, and at most one is active on a thread: a second `enter` on the same thread
returns `ScopeActive` and changes nothing (MAJ-02).

- **One request per thread.** Because scopes do not nest, the request whose `QueryConnection` runs
  a statement is the request whose scope is active: opening, preparing, stepping, planning,
  scanning and closing are all charged to that connection's own budget. Out-of-order destruction
  of scopes cannot be constructed. Tests show the nested-request example refused, the outer
  connection still charging its own budget, and a new scope after an unwound one.
- **Guards survive cancellation.** The scope's guard clears the thread's slot in `Drop`, so an
  unwinding request (a panic, or a dropped future that owned the scope) leaves the thread unscoped
  again. Leases live in the block headers, not in the scope: a block allocated inside the request
  and freed after it, on any thread, returns its charge to the request's budget then.
- **Open to close inside the scope.** `RequestScope::open_query_connection(path, flags)` opens the
  request's connection inside the scope and returns a `QueryConnection<'scope>` that borrows the
  scope. The borrow checker therefore keeps preparing, stepping, finalizing and closing inside the
  scope; the connection cannot outlive the request.
- **Private cache (MED-03).** The connection always opens with `SQLITE_OPEN_PRIVATECACHE`, and a
  request for sharing is refused with `QueryError::SharedCache` before anything is opened:
  `SQLITE_OPEN_SHARED_CACHE` in the flags, or a `file:` URI (this build interprets them whatever the
  flags say) whose percent-decoded `cache` parameter is anything but `private` (a URI parameter
  overrides the flags). So no schema or page cache another connection allocated is shared with the
  request, and none survives it.
- **Connection settings.** Lookaside is disabled with `SQLITE_DBCONFIG_LOOKASIDE` (null buffer,
  zero size, zero slots) right after open, so every allocation goes through the charged allocator;
  `SQLITE_LIMIT_WORKER_THREADS` is set to 0, so no sorter thread allocates outside the scope's
  thread; a progress handler checks `QueryBudget::check()` every 1000 virtual-machine steps and
  interrupts the statement once the deadline passed, the request was cancelled or a limit was
  recorded. These are per-connection settings; connections not opened through a scope (writers)
  keep SQLite's defaults and original allocation behaviour.
- **Lock waits (MED-04).** rusqlite's default 5 s busy timeout is replaced by a busy handler that
  retries a lock every 5 ms only while `QueryBudget::check()` passes, for at most 1 s per wait. A
  deadline or cancellation during a lock wait therefore ends it within one pause and is reported
  as the limit; a lock still held after 1 s with a healthy budget is a genuine `SQLITE_BUSY`.
  Progress checks bound VM instruction intervals, not the time spent inside one callback or
  opaque operation; table functions check their budget in their own loops.
- **Error translation.** `RequestScope::translate(rusqlite::Error) -> QueryError` maps
  `SQLITE_NOMEM`, `SQLITE_INTERRUPT` and `SQLITE_BUSY` to `QueryError::Limit(BudgetExceeded)` only
  when this budget's `check()` reports an exceeded limit (memory, time or cancellation, the first
  one recorded); the message names it. Any other error, and an out-of-memory, interrupt or busy the
  budget did not cause, stays `QueryError::Sqlite` unchanged.

The one `unsafe` block is the `sqlite3_db_config(SQLITE_DBCONFIG_LOOKASIDE, ...)` call: the handle
is the live connection's, used on its own thread for the call, and the variadic arguments are
exactly the `(void *, int, int)` the option takes; SQLite refuses with `SQLITE_BUSY` rather than
free slots in use (just after open none are).

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
  link order, so a dependency's constructor may run first. An earlier SQLite initialization is
  detected below, but an earlier constructor that starts a thread using SQLite is not. Mach-O
  ordering is untested: no Apple build has been run or audited.
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

### Byte-owner adapter (`retain_owner`)

`retain_owner(bytes, owner)` wraps the span and its owner in a private holder and hands it to
`OwnedBytes::new`, which requires `StableDeref`. Every clone and slice of the result shares that
holder, so the owner is dropped after the last of them, after the span. The interface is safe;
`Send` and `Sync` follow from the fields.

| Site | Kind | Invariant relied on |
|---|---|---|
| `bytes.rs` `unsafe impl<O> StableDeref for Retained<O>` | `unsafe impl` | `deref` returns the span of the held `OwnedBytes`, which points into that span's own heap buffer or file mapping, never into the holder, so moving the holder does not move it; the span is private, set once and never replaced, and `deref` reads no other state, so it returns the same span until the holder is dropped. |

### Residual risks

- Scope of the startup audit: the Linux build is audited (the `.init_array` table above); Mach-O
  ordering is untested. A constructor added later that starts a thread which calls SQLite
  concurrently with the bootstrap would not be detected; a constructor that initializes SQLite
  first is refused (abort).
- Pre-`main` use of the standard library is best-effort, not guaranteed by Rust: the constructor
  reads one environment variable, may write one line to stderr and may call `abort`, before
  Rust's runtime setup. On glibc this works in practice. SIGPIPE is not yet ignored then, so a
  closed stderr pipe under the report variable or on refusal ends the process by SIGPIPE instead
  of the report or the named refusal.
- Memory checker: Valgrind memcheck runs this crate's tests, in the separate Linux CI job
  `vtab-memcheck` (not part of the gates or the release build). It runs the debug test binaries
  twice, with default features (the embedding-only cases) and with all features (the bootstrap
  cases), and follows the fresh-process children (`--trace-children=yes`), so the allocator
  adapter and SQLite's C code are checked together. Any invalid access, use of uninitialised
  memory or definite leak fails the job. `tests/valgrind.supp` suppresses one known definite
  leak, the payload of a contained panic (leaked on purpose, see `contain`); it matches on symbol
  names, so the job keeps the symbol table (`CARGO_PROFILE_DEV_STRIP=none`). Possible leaks, such
  as the standard library's main-thread handle, are reported but do not fail the run. On x86-64
  Linux, with Valgrind installed:

  ```sh
  CARGO_PROFILE_DEV_STRIP=none \
  CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUNNER="valgrind --error-exitcode=1 --leak-check=full \
    --errors-for-leak-kinds=definite --track-origins=yes --trace-children=yes \
    --suppressions=$PWD/crates/kaban-vtab/tests/valgrind.supp" \
    cargo nextest run -p kaban-vtab --locked --no-fail-fast --retries 2 -E "$MEMCHECK_FILTER"
  ```

  Run it again with `--all-features`; `MEMCHECK_FILTER` is the filter expression set in the
  `vtab-memcheck` job of `.github/workflows/ci.yml` (on aarch64 the runner variable is
  `CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_RUNNER`). The retries absorb timing failures under
  Valgrind's slowdown; a memcheck error repeats on every attempt and still fails the run. What it
  does not cover: only paths the tests reach; four tests whose assertions depend on wall-clock
  deadlines or timers, which the slowdown breaks, are excluded from this job and run only in the
  normal suite (`budget_scope`: `a_query_past_the_deadline_is_interrupted_with_the_named_limit`,
  `a_genuine_sqlite_failure_is_not_reported_as_a_limit`; `connection`:
  `a_deadline_during_a_lock_wait_ends_promptly_as_the_time_limit`,
  `a_cancellation_during_a_lock_wait_ends_promptly_as_cancelled`); no data races (memcheck is
  not a race detector; no ThreadSanitizer runs); Rust-level undefined behaviour that touches no
  invalid memory (aliasing, invalid values), since Miri cannot execute SQLite's C code over FFI;
  possible leaks and leaks still reachable at exit; and macOS.
  AddressSanitizer is not set up (it needs a nightly toolchain with `rust-src`, which the pinned
  toolchain does not use).
- Unscoped work on a scoped thread is charged to the scope's request (see "Not supported"). It
  over-charges that request; it never under-charges one.
- A table function's `FnError` that wraps a budget refusal, and the adapter's own copy refusal,
  stay SQLite errors whose text names the limit (`Memory`); `translate` does not turn them into
  `QueryError::Limit`, because SQLite reports them as module errors, not `SQLITE_NOMEM`. A
  consumer maps them by checking the shared budget.
- `TableFunction: Send + Sync + 'static` cannot borrow a transaction-bound snapshot; a function
  that must read the same snapshot as its statement needs an explicit ownership design in its
  consumer (T14).

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
