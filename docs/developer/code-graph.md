# Code graph: `kaban-index`

`kaban-index` turns one checked-out commit of a repository into a per-commit code-graph
snapshot (`<commit40>.cg.db`, SQLite) that `kaban serve` reads for the `code_*` tools. Indexing
is explicit: nothing in `kaban serve`, the hooks or `sync` ever starts an indexer. The producer
is a SCIP indexer: `scip-go`, `scip-typescript` or `kaban-rust-scip` (this repository's own
Rust producer, `tools/kaban-rust-scip/`). Its stdout and stderr are gated before the log is
written; the content-policy scan over the SCIP output itself is opt-in (`index.policy_scan`,
off by default: the snapshot stays on this machine). Specification:
`../specs/kaban/specs/02-code-graph-v4/stage1-spec.md` (G1-F7, G1-S5, G1-S6, G1-S8, §7); Rust:
`../specs/kaban/specs/02-code-graph-v4/rust-indexing-plan.md`; user documentation:
`docs/guides/code-graph.mdx`, `docs/reference/kaban-index.mdx`,
`docs/reference/code-tools.mdx`.

```text
  kaban-index trust <repo>          one explicit mark per repository (0600, no paths inside)
         |
  kaban-index index [--repo P]      identity ==> trust ==> config ==> HEAD == commit, tree clean
         |                           ==> tier probe + select ==> per language: admit (toolchain,
         |                           module cache or registry, linker), run, gate log ==> decode
         |                           + gate ==> classify committed tests ==> derive ==> publish
         v
  ~/.kaban/codegraph/<repo-id>/<commit40>.cg.db        (0600 in a 0700 directory)
  ~/.kaban/codegraph/<repo-id>/<commit40>.<lang>.<a|b>.log   (the gated output of each run)
         |
  kaban-index status | gc | purge-derived | doctor      (read, collect, delete, inspect)
```

Measured on the integrated head (`../specs/kaban/specs/00-project/evidence/dogfood/shared-index-0e583ae.json`,
`../specs/kaban/specs/00-project/evidence/gofix1-evidence/t2-cli-*.json`, `../specs/kaban/specs/00-project/evidence/df2-evidence/df3-gate.md`):
Kaban indexes itself (Rust) in 188 s on `none` (producer 169 s; 576 files, 75,257 symbols,
425,601 edges, 209,350,656 B) and in 231 s on `namespaces` (at `2daaeb1`: 573 files, 76,504
symbols, 424,954 edges, 209,084,416 B); the GitHub CLI `v2.102.0` (Go) in 27 s on `none` and
26 s on `namespaces` (911 files, 66,907 symbols, 306,065 edges, 82,886,656 B, identical on
both tiers).

## Verbs

| Verb | What it does | Exit |
|---|---|---|
| `trust [--repo P]` | Records the trust mark after printing the trust text below; the mark binds the repository's common directory (device, inode, birth time) and its sorted root commits, never a path | 0, 4 |
| `untrust [--repo P]` | Removes the mark | 0, 4 |
| `index [--repo P] [--commit SHA\|HEAD] [--languages go\|ts\|typescript\|rust] [--timeout DUR] [--indexer LANG=PATH]... [--partial] [--isolation preferred\|required\|off]` | Writes the snapshot of the checked-out commit, applies `gc --keep 5` and prints the JSON report (`--lang` is an alias of `--languages`) | see below |
| `status [--repo P]` | Trust state, the live repository path (scanned, never persisted), identity validation, and for every snapshot its `isolation`, `isolation_detail`, `resources`, `languages`, `policy_version`, size and state | 0, 4 |
| `gc [--repo P] [--keep N] [--dry-run] [--prune-corrupt] [--stale-policy]` | Keeps HEAD's snapshot plus the `N` (default 5) most recent by commit date; unknown commits are collected first; `--prune-corrupt` adds the snapshots whose fresh open fails; `--stale-policy` adds those whose `policy_version` is not the current policy's hash; every `gc` also sweeps the left-overs of killed runs | 0, 4, 5 |
| `purge-derived [--repo P]` | Deletes every derived file of the repository (snapshots, logs, markers, scratch); the trust mark stays | 0, 4, 5 |
| `doctor [--repo P] [--indexer LANG=PATH]...` | Configuration in effect and its overrides, detected languages and indexers (versions through the runner, output gated; Rust: the linker row; Go: the toolchain row), every isolation tier's probe result, the tier `index` would select, the platform notes, the trust state and identity result, the trust text | 0, 4 |

There is no `--share-caches` and no `--keep-run-dir`: the Go module cache and the Cargo registry
are read-only inputs of every run (below), and the raw files an indexer writes are always
deleted (the scratch directory is removed on every exit path); both flags are rejected as unknown.

### Exit codes of `index`

| Exit | Meaning | Classes in the report |
|---|---|---|
| 0 | published, or `already_indexed` (a snapshot of this commit exists; nothing written) | |
| 2 | usage error | a fixed diagnostic naming the kind of argument error (`usage error (UnknownArgument); see kaban-index --help`); the offending argument is caller input and is never echoed |
| 3 | indexer not found | `indexer_not_found` (also a language without a stage-1 indexer) |
| 4 | refused | `trust_refused`, `trust_revoked`, `repo_path_refused`, `commit_not_checked_out`, `tree_dirty`, `config_invalid`, `unsupported_in_stage_1`, `language_not_detected`, `multi_language_not_in_this_release`, `indexer_untrusted_mode`, `indexer_untrusted_location` (an indexer under the checkout or the data directory), `indexer_override_invalid`, `indexer_changed` (the admitted indexer was replaced before a spawn), `toolchain_refused` (the `go`/`node`/`rustc` found on `PATH` is inside the checkout or the data directory, or is not a trusted executable), `checkout_replaced` (the checkout directory admitted at the start is not the one at the path before a spawn), `git_untrusted` (no trusted `git` at the fixed locations), `env` (environment, data directory or an inherited descriptor without `FD_CLOEXEC`; the line names the descriptor), `inherited_fd`; the Go admission classes `go_modcache_refused`, `go_toolchain_too_old`, `go_mod_unreadable`, `go_version_unreadable`, `go_toolchain_changed`; the Rust admission classes `rust_capabilities_refused`, `rust_toolchain_refused`, `rust_toolchain_selector_unsupported`, `rust_toolchain_selector_ambiguous`, `rust_toolchain_channel_mismatch`, `rust_toolchain_component_missing`, `rust_toolchain_component_unsupported`, `rust_toolchain_target_missing`, `rust_toolchain_target_unsupported`, `rust_cargo_config_redirects_backend`, `rust_config_unreadable`, `rust_linker_refused`, `rust_registry_refused`, `rust_prepared_file`, `rust_prepared_write`, `rust_scratch_failed` |
| 5 | snapshot write error | `published_unsynced` (the snapshot exists, marked for re-verification), `snapshot_corrupt`, `snapshot_exists`, `scratch`, `scratch_cleanup_failed` (the run's raw files could not be removed; nothing published), `log_write_failed`, `gc_failed`, the writer's classes (`temp_exists`, `temp_swapped`, `fs`, `write`, `dangling`, `data_dir_refused`) |
| 6 | indexer failed | `indexer_failed` (with `reason: project_root_mismatch`, `index_decode_failed`, `source_classification_refused`, or an output-file reason `output_missing`, `output_symlink`, `output_not_regular`, `output_too_large`, `output_dir_replaced`, `output_read_failed`), `output_cap`, `quota_exceeded`, `quota_unobservable` (the scratch could not be measured), `failed:monitor`, `failed:monitor_silent`, `descendants_survived` |
| 7 | indexer timed out | `indexer_timeout` |
| 8 | isolation refused | `isolation_unavailable` (`required` with no isolating tier), `isolation_selfcheck_failed` (also when another language succeeded: an isolation refusal is never skipped by `--partial`) |

`policy_refused` is gone (DF-3): with `index.policy_scan` on, a hit is masked and counted
(`counts:policy_*`, the rule ids per language in the report); off, nothing is scanned.

Every text on stderr is fixed: no repository path, commit argument, indexer output or
configuration value is echoed by an error. Everything the binary prints, the JSON report and the
stderr lines alike, passes one final gating pass under the policy in effect: a string that hits a
reject rule is replaced by `[removed: <rule id>]`, a redact hit is masked; this covers
configuration values, the override list, snapshot metadata read back by `status`, the indexer
version lines (scanned whole before they are shortened) and the live paths. `status` and `trust`
print the live repository path scanned through the policy; `doctor` prints `repo_id`, the trust
state and the identity result but never a path, and names an indexer only as `found` or by its
refusal class. The Rust admission classes share one stderr line (`Rust not admitted: ...; run
doctor for the row`); the producer's own diagnostic in the gated log names the `.cargo/config`
table and key when that is the cause (`[target.<triple>] linker redirects the compiler backend;
remove it to index`). `go_toolchain_too_old` prints the versions and the action
(`run 'GOTOOLCHAIN=auto go version' in the repository once ..., or install Go <v>`).

### The JSON report

`index` prints one JSON document on stdout:
`{verb, ok, class?, reason?, repo_id, commit, languages[], counts{files, symbols, edges,
occurrences, snippets, placeholder_rows, ...derivation counters}, snapshot_bytes,
wall_ms_per_indexer{lang}, peak_rss_bytes_per_indexer{lang}, skipped_targets_per_indexer{lang:
[names]}, policy_scan, policy_redacted_meta_per_indexer{lang: [rule ids]}?,
policy_skipped_source_per_indexer{lang: [rule ids]}?, source_classified_by_path_per_indexer{lang:
n}?, isolation, isolation_detail, resources[], cgo_enabled?, failed{lang: class}?,
overrides[[key, value]], warnings[], exit, gc{keep, collect, removed, swept_leftovers}}`.
`resources` names the read-only inputs the runs were given (`toolchain`, `modcache`,
`registry`; FIX-E); there is no `caches` field. `skipped_targets_per_indexer.rust` lists the
Cargo targets whose `required-features` are inactive (in Kaban: `gen_pdf_fixtures`,
`bench_d9`, `golden_pdf`). `source_classified_by_path_per_indexer` counts Rust files over the
committed-source test classifier's bound (16,384 raw open delimiters, 131,072 tokens),
classified by their path only. `overrides` lists every `index.*` key whose value differs from
the default, the per-run `--isolation` and `--timeout` included. `cgo_enabled` is the effective
cgo setting of a Go run as a fixed word: `0` or `1` (the caller's value, or the namespaces
default), `autodetect` (unset on the `none` tier), `caller` (any other caller value, never
echoed). `warnings` holds fixed classes only: `policy_skipped_row`, `policy_dropped`,
`grammar_skipped_row`, `isolation_none`, `partial`, `toolchain_root_refused`,
`coverage_invalid` (a malformed coverage sidecar publishes no `coverage:*` row rather than a
wrong figure), `gc_failed`.

### Rust

Cargo package and workspace roots (`Cargo.toml`) and `--languages rust` use `kaban-rust-scip`
(contract v2: `protocol_version: 2`, profile `kaban-rust-v1`, `ra_ap_version` 0.0.358, one
advertised host `aarch64-unknown-linux-gnu`; `rust_capabilities_refused` otherwise). Admission
(`language::rust`): the `rustc` on `PATH` must be the real Rust 1.99.0 for the native host (not a
rustup proxy: put the toolchain's `bin` first on `PATH`), its sysroot is the admitted toolchain
root; the system C compiler `/usr/bin/cc` is resolved canonically, admitted only under
`/usr/bin` and outside the checkout and the data directory, its helper directory
(`cc1`, `collect2`, `crt1.o`) is bound read-only into the view and its backend chain pinned
(`CARGO_TARGET_<TRIPLE>_LINKER`, `CC`, `COMPILER_PATH`), `rust_linker_refused` otherwise;
`/usr/include` is in the view so bundled C (SQLite) compiles; the caller's
`CARGO_HOME/registry` (else `~/.cargo/registry`) is a read-only input on both tiers
(bound at its host path under `namespaces`, linked from the scratch Cargo home), Cargo runs
offline, a missing dependency fails the run without downloading. A `rust-toolchain.toml` is
honoured when its `channel`, `components`, `targets` and `profile` (`minimal` or `default`) are
already installed in the admitted sysroot; `path`, `profile = "complete"` or any other shape
is `rust_toolchain_selector_unsupported`, and nothing is ever installed. A repository or
ancestor `.cargo/config[.toml]` is accepted except for the keys that redirect the pinned
compiler backend (`rust_cargo_config_redirects_backend`: `[target.*] linker`, `rustflags` with
`-C linker=`, `-C link-self-contained` or `-B`/`-specs` link arguments, forced `[env]` on `CC`,
`PATH`, `COMPILER_PATH` or the linker variable, any `LD_*` or `GCC_EXEC_PREFIX` entry).

What the producer skips and counts instead of refusing (DF-2, R-3, FIX-G): member targets
whose `required-features` are inactive, proc-macro crates the build did not produce, items
inside function bodies and nested macro expansions, anonymous items (except an impl inside an
anonymous `const` directly in a module, the shape serde's and thiserror's derives expand to:
published as a module-level impl, so derived impls are implementers; a generic or
predicate-bearing one keeps its declaration without an edge, `derived_impls_generic_skipped`),
files shared by several crates (indexed in their resolving crate), modules with an unresolved
import, bodies whose references differ between `cfg` contexts (published as the union), tokens
without a stable symbol. Each count is a `coverage:<key>` meta row of the snapshot
(`writer.rs`, `Coverage::KEYS`: `targets_skipped`, `proc_macros_not_built`, `contexts`,
`declarations`, `block_items_skipped`, `block_item_references`,
`shared_file_contexts_skipped`, `incomplete_modules`, `divergent_bodies`,
`cfg_divergent_declarations`, `tokens_without_symbol`, `positions_skipped`, `scip_bytes`,
`block_items_unique`, `block_item_lines`, `derived_impls_generic_skipped`, plus the
`per_crate` table) and reaches every `code_*` answer as the `coverage` object.

The consumer classifies Rust SCIP against committed source before publication: exact
`#[test]` and `#[cfg(test)]` items in module scopes, inline modules, whole-file inner
`#![cfg(test)]`, and `.rs` files below `tests/`. Source is read only through bounded pinned
`git show`, never from the working tree. A file over the per-file bound is classified by its
path only and counted (`counts:source_classified_by_path`), never refused. Inherent-impl members
are children of their type (`Postgres#new().`, R-2), so `code_members` of a struct lists its
fields and methods; members of trait impls keep rust-analyzer's `impl#[..]` identity.
References are never `CALLS`. The producer's own README (`tools/kaban-rust-scip/README.md`)
has the build and the contract.

## Trust

Indexers run build code. A repository is indexed only after an explicit `kaban-index trust
<repo>`, which prints this text and records the mark:

> indexers may run build code from this repository as your user.
> Under the namespaces or container tier they see only this checkout and the pinned toolchains
> read-only, one scratch directory, and no network.
> Under the none tier they see your files and network as you do.
> The tier is chosen at index time, recorded in every snapshot and shown by kaban-index doctor
> and status; set index.isolation = required to refuse indexing without isolation.
> This mark never waives environment hygiene, quotas, output gating or identity checks.

The mark holds identity material only (`{v, common_dir: {dev, ino, btime}, root_commits,
kaban_index_version}`): no path, no free string. The identity is recomputed and compared with
the mark immediately before every indexer spawn (both Go runs included), and on `status` and
`doctor`. Any mismatch deletes the mark and refuses the run with the fixed text
"repository identity changed; trust mark revoked; run kaban-index trust again" (exit 4). A
deleted and recreated directory, or a repository with different history, never inherits a mark.
Every worktree of one repository shares the common directory, so it shares the mark, the
`<repo-id>` and the snapshots: a snapshot of a commit serves every worktree checked out at it.
Trust never selects a tier and never waives any property of the runner: with and without the
mark the environment, the wrapper and the tier are the same; only whether the indexer starts
differs.

Git is a metadata oracle for `kaban-index`, never a tool the repository may configure: the
executable is pinned to `/usr/bin/git`, `/bin/git` or `/usr/local/bin/git` under the same handle
rule as the `unshare` wrapper (a regular ELF file owned by root or you, writable by neither group
nor other, executed through the handle; `git_untrusted` otherwise), the caller's `PATH` and
`HOME` are not consulted, the system and user configuration files are not read
(`GIT_CONFIG_NOSYSTEM`, `GIT_CONFIG_GLOBAL=/dev/null`), and the repository's own configuration
is read with `core.fsmonitor`, `core.hooksPath` and `log.showSignature` overridden on the command
line, so no repository-configured command runs during `trust`, `index`, `status` or `gc`. A
failed git query under `gc` aborts the collection (`gc_failed`); nothing is deleted on a
failure. `git` and the indexers are spawned with a clean descriptor table: a descriptor the
`kaban-index` process inherited without `FD_CLOEXEC` refuses the spawn (class `env`, the line
names the descriptor); a caller holding a `flock(1)` lock on fd 9 (`scripts/dev/cargo-slot.sh`)
runs `bash -c 'kaban-index ... 9>&-'`.

## Isolation tiers

Every tier is probed, never assumed; `doctor` shows every probe result and the tier `index`
would select under `index.isolation` (`preferred` by default, `required`, `off`; `--isolation`
overrides for one run). The tier that ran is recorded in every snapshot (`meta.isolation`,
`meta.isolation_detail`, `meta.resources`) and shown by `status` and the report.

| Tier | What the indexer sees | Read-only inputs (`meta.resources`) |
|---|---|---|
| `namespaces` | Linux user, network, mount and pid namespaces built by `unshare --user --map-root-user --net --mount --pid --fork` running `kaban-index ns-helper`: a tmpfs root with the checkout (read-only, the working directory), the indexer's directory and the toolchain's `bin` directory read-only at their host paths (these two plus `/usr/bin` and `/bin` are the pinned `PATH`), the admitted toolchain root and its symlinked entries read-only as resources (not on `PATH`), the linker's helper directory (Rust), `/usr/include`, `/lib`, `/lib64`, `/usr/lib`, `/etc/ld.so.cache`, one writable `/scratch`, `/dev/{null,zero,urandom}`, a fresh `/proc`, no network interface but `lo`. The helper verifies the view from the inside, then drops every privilege before `exec`ing the indexer: nested user namespaces are closed, the securebits are set and locked, the ambient and bounding sets are emptied, `no_new_privs` is set and the capability sets are cleared, all read back; the indexer runs as uid 0 of the user namespace with no capability and cannot remount a read-only bind, mount anything or regain privileges. Any mismatch is exit 8 under every policy | `toolchain`; `modcache` (Go: the caller's `GOMODCACHE`, else `$HOME/go/pkg/mod`, bound read-only at its host path; a private overlayfs on kernels that allow it, else a plain read-only bind: `overlay=none` in the detail); `registry` (Rust: `CARGO_HOME/registry`, bound read-only at its host path) |
| `container` | deferred past 3.0.0 (OWN-68 SC-11): `doctor` prints `tier container: deferred (not_in_this_release)`; the config keys `container_image` and `container_cli` are accepted and reserved | |
| `none` | your files and network as you do; the environment is still the clean allowlist, the scratch is still the only place kaban expects writes, quotas, deadline, output caps and log gating still apply | the same kinds: the caller's own module cache and registry directories, `GOFLAGS=-mod=readonly`, `GOPROXY=off`, Cargo offline |

`preferred` falls back to `none` with the warning `isolation_none` and one fixed stderr line;
`required` refuses (exit 8) when no isolating tier is available. The Go toolchain: when the
installed `go` is older than the `go.mod` `go` directive, the matching toolchain already in the
module cache (`golang.org/toolchain@v0.0.1-go<v>.<os>-<arch>`) is admitted first on the pinned
`PATH` and as the child's `GOROOT`; when nothing satisfies the directive the run is
`go_toolchain_too_old` with the action in the line, and `doctor` shows the same row.

### Platform notes (what `doctor` prints, and what was measured in the development sandbox)

`doctor` prints the kernel release, `/proc/sys/kernel/unprivileged_userns_clone` (absent on
most kernels: not restricted), `/proc/sys/user/max_user_namespaces`, and the `unshare`
candidate that resolved under the handle rule (`/usr/bin/unshare`, `/bin/unshare`; a regular
root-owned, non-writable ELF file; never `PATH`).

Measured on the development sandbox (kernel 7.0.12 aarch64, util-linux 2.41.3):

- unprivileged user namespaces available (`unprivileged_userns_clone = 1`); `doctor` selects
  `namespaces`.
- overlayfs inside a user namespace fails with `EINVAL`, so the module cache is a plain
  read-only bind: `isolation_detail` reads `unshare=/usr/bin/unshare kernel=7.0.12 overlay=none`
  and a Go indexer that needs to write the module cache fails there (`GOFLAGS=-mod=readonly`,
  `GOPROXY=off` hold anyway).
- a fresh `/proc` needs an owned pid namespace, hence `--pid --fork`: the helper is pid 1 of the
  new pid namespace and `exec`s the indexer in place.
- Debian's Go toolchain keeps `GOROOT/src` and `GOROOT/pkg/include` as symlinks into
  `/usr/share`; `index` binds the admitted toolchain root and the targets of its symlinked
  entries (one and two levels deep) as read-only resources so the standard library compiles
  inside the view. A root is admitted only when it is the canonical grandparent of the `go` (or
  `node`, or `rustc`) executable found on your `PATH`, has the toolchain layout and lies outside
  your home directory and its ancestors, the checkout and the kaban data directory; otherwise
  only the executable's own directory is admitted and the report carries the warning
  `toolchain_root_refused`.
- cgo: the view binds `gcc` (under `/usr/bin`) and `/usr/include`, but an unset `CGO_ENABLED`
  still becomes `0` under the namespaces tier and the report says `cgo_enabled: "0"`; an
  explicit caller value is carried unchanged. Disabling cgo changes build selection: files with
  a `cgo` build constraint are excluded and their `!cgo` alternatives included, so the indexed
  implementation set is the pure-Go one.

## Preconditions and refusals

`index` requires: the mark (`trust_refused` otherwise) and a matching identity
(`trust_revoked`); `<commit>` (default `HEAD`) resolving to the checked-out `HEAD`
(`commit_not_checked_out`); `git status --porcelain --untracked-files=no` empty (`tree_dirty`);
a loadable `~/.kaban/config.yaml` (`config_invalid`, field path only); at least one marker at the
root (`go.mod`; `package.json` or `tsconfig.json`; `Cargo.toml`); a resolved indexer per
language (`--indexer <lang>=<absolute path>`, else one `PATH` lookup; the indexer must be a
regular file owned by you or root and writable by neither group nor other,
`indexer_untrusted_mode` otherwise, and must not live under the checkout or the kaban data
directory, `indexer_untrusted_location`; its identity (device, inode, size, change time) is
re-checked immediately before every spawn, `indexer_changed`); a `go`, `node` or `rustc` found
on `PATH` under the checkout or the data directory is `toolchain_refused`; the language's own
admission (Go: module cache and toolchain against `go.mod`; Rust: producer capabilities,
toolchain, selector, registry, linker). The checkout directory admitted at the start must still
be the directory at the path before every spawn (`checkout_replaced`). The indexer's output
file is read through the run's own output-directory handle: it must be a regular file (no
symlink, no FIFO, no device) within `index.quota.file_bytes`, in the output directory kaban
created (`indexer_failed` with an `output_*` reason otherwise). A TypeScript project without
`tsconfig.json` is indexed with `--infer-tsconfig`. The SCIP metadata's `project_root` must
name the repository root (`project_root_mismatch`: exit 6, the snapshot is refused), so a
snapshot is never named for a commit whose tree it did not read. A snapshot name already
present is never overwritten: the run reports `already_indexed` (exit 0); a present snapshot
that carries the `published_unsynced` marker is re-verified first (a fresh open with
`quick_check`) and the marker cleared; one that fails re-verification is exit 5
(`snapshot_corrupt`) until `gc --prune-corrupt` removes it.

## Limitations

- Untracked and ignored files (for example `node_modules`, `vendor` without `modules.txt`) are
  visible to the indexer and influence what it emits; the snapshot is named for the commit, not
  for the working tree. Keep the tree clean of generated code before indexing.
- One snapshot holds one language. A repository that detects two needs `--languages`; when more
  than one language succeeds the run is refused with `multi_language_not_in_this_release` and no
  snapshot is written (the gated indexer logs of the runs may already exist in the data
  directory). With `--partial`, the snapshot holds the one language that succeeded and the
  report names the failed one; an isolation refusal is never skipped by `--partial`.
- Rust coverage is partial by construction and measured: on Kaban at `0e583ae` 65,449
  declarations are published beside 2,809 distinct block-local items (4,280 references into
  them), 9 incomplete modules, 352 shared-file contexts, 3 skipped targets, 1 proc macro not
  built, 406 tokens without a symbol and 44 generic derived impls without an edge
  (`../specs/kaban/specs/00-project/evidence/docs1-evidence/code-index-resource.json`). Every `code_*` answer says so;
  an empty answer does not prove absence.
- Every run keeps its own gated log: `<commit40>.<lang>.a.log` holds the first run,
  `<commit40>.go.b.log` Go's `--skip-tests` run; `gc` and `purge-derived` remove them with the
  snapshot.
- `gc --stale-policy` treats a `policy_version` different from the current policy's hash as
  stale (hashes have no order); a snapshot indexed with the scan off carries `off`.
- The `container` tier is deferred; `required` fails closed when namespaces are unavailable.
- On a host without unprivileged user namespaces, `preferred` runs the `none` tier with the
  warning; the indexer then sees your files and network. Prefer `required` where that is not
  acceptable.
- Under the namespaces tier cgo defaults to off (see the platform notes).
- A mode-000 directory an indexer leaves in the scratch is still measured by the quota walk and
  removed before success is reported; a scratch that cannot be removed is `scratch_cleanup_failed`
  (exit 5) and nothing is published.

## Files on disk

`~/.kaban/codegraph/<repo-id>/` (0700; `<repo-id>` is an HMAC of the canonical common directory
under `~/.kaban/keys/codegraph.key`, a storage key and never an authorisation):
`trust` (0600), `<commit40>.cg.db` (0600), `<commit40>.<lang>.<a|b>.log` (0600, gated; `a` the
first run, `b` Go's `--skip-tests` run),
`<commit40>.cg.db.unsynced` (the re-verification marker), `run-<pid>/` (a live run's scratch;
removed on exit and swept by the next `index`, `gc` or `purge-derived`). `KABAN_HOME` moves
the root. Configuration: the `index:` block of `~/.kaban/config.yaml` (see
`examples/config.yaml`): `isolation`, `deadline_secs`, `policy_scan`, `quota.{file_bytes,
run_dir_bytes, tmp_bytes}`, and the reserved `container_image`, `container_cli`.
`share_caches` is not a key (FIX-E); a file that names it fails to load (`config_invalid`).

## Reading the snapshot from `kaban serve`

`kaban serve` is the reader; it never writes under `codegraph/` and never starts an indexer.

- **Key.** At startup the server reads `$KABAN_HOME/keys/codegraph.key` (32 bytes, mode 0600,
  created by `kaban-index`, never by `serve`). No key means no code-graph provider: every
  `code_*` tool answers `no_snapshot`. A key readable by group or other, or a symbolic link at
  its path, is refused with a fixed text that names no path, as the audit key is.
- **Directory.** The repository is the server's working directory: `git rev-parse
  --git-common-dir` is resolved to its canonical common directory and `<repo-id>` is the HMAC of
  that path under the key (a storage name, never an authorisation), so the snapshot directory is
  `$KABAN_HOME/codegraph/<repo-id>/`, the one `kaban-index index` wrote for the same checkout,
  and every worktree of the repository reads the same directory.
- **HEAD, then its ancestors.** Every call runs `git rev-parse HEAD` (about 1 ms; no file mtime
  is a safe key for a branch ref) and, once per head sha, `git rev-list --max-count=200 <sha>`.
  The provider admits `<HEAD>.cg.db` when it exists, else the nearest ancestor's `<sha>.cg.db`
  in that window; the wire `snapshot` carries `commit` and `exact` (`false` for an ancestor),
  `policy` (`off` when the scan was disabled at index time, `current` when the snapshot's
  `policy_version` is the live policy's hash, else `stale`), `isolation` (the tier recorded at
  indexing) and `schema_version`. A candidate written by a newer `kaban-index` is skipped and,
  when nothing else resolves, the tools answer `snapshot_too_new: the code-graph snapshot was
  written by a newer kaban-index; upgrade kaban or re-index`.
- **Warm-up.** Once the transport is up the server resolves the current snapshot on the
  blocking pool (the `quick_check` and the head's `rev-list`), so the first `code_*` call does
  not pay for it; nothing is created or written.
- **Coverage.** Every answer carries the `coverage` object projected from the snapshot's
  `coverage:*` rows (declarations, distinct block items, incomplete modules, skipped targets,
  and one sentence naming every non-zero gap); `kaban://code-index` carries the whole table
  with `per_crate`, beside `languages`, `indexers`, `resources`, `policy_version` and `counts`.
- **`no_snapshot`.** When no candidate resolves (no key, not a repository, no directory, no
  file in the window), every code tool returns the one fixed text, and `kaban://code-index`
  reads it as content: `no_snapshot: no code-graph snapshot for this repository and commit; run
  \`kaban-index index\``. No path, sha or argument value is ever echoed in it. Both `git`
  calls run through the one process runner; `serve` spawns nothing else (G1-S3, checked during the
  MCP smoke by `serve_children_are_git_only`).
- **Selectors.** The name form is exact and case-sensitive; several matches are
  `ambiguous_symbol` with `candidates` (Q-2), a wrong label with a unique name answers with
  `target_note.label_corrected`, an unknown name offers `did_you_mean`; `code_neighbours` hides
  document-local symbols and counts them in `locals_hidden` unless `include_locals`. The fields
  are listed in `docs/reference/code-tools.mdx`.

## Build

The release profile (`lto = "thin"`, `codegen-units = 1`, `strip = true`) applies to both
binaries. The G1-N4 budget for `kaban-index` is a **musl** budget: the release build for the
host's musl triple, `cargo build --release --target <arch>-unknown-linux-musl -p kaban -p
kaban-index` (`.cargo/config.toml` names `musl-gcc` as the linker and `+crt-static`), must keep
`target/<triple>/release/kaban-index` strictly under 8,000,000 decimal B (owner decision
2026-10-26; budget alignment, not a size optimization). `scripts/ci/ci.sh` builds that triple only
(the box has its own architecture's `musl-gcc`; a foreign installed target has `std` but no
linker), applies the gate (`FAILED ... (G1-N4)`), prints the `kaban` musl size informationally
(N1 is the default gnu gate) and, when the host's musl target is not installed (`rustup target add
<arch>-unknown-linux-musl`), prints `not-run: musl target absent` and exits 3: the stage is not
complete without the musl figure. The Rust producer is its own workspace:
`cd tools/kaban-rust-scip && cargo build --release --locked --offline`, installed outside the
checkout and the data directory.
