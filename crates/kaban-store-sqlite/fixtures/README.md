# Older-format fixtures (v3.0.0, schema version 13)

Real databases written by the kaban 3.0.0 binary at commit `c15cd88`, the last release
before the v14 format. `crates/kaban/tests/older_format.rs` copies each directory into a
temporary directory and checks that every entry point refuses it without changing a byte
(spec 11 CMP-01, OD-1). Never open these files in place: any SQLite write, including WAL
recovery, would change them.

| File | sha256 |
|---|---|
| `v3.0.0/kaban.db` | `74a204dd654b923059c32843e7ace21b656cdd50375ba13a2bb299d22bedc23c` |
| `v3.0.0-wal/kaban.db` | `74a204dd654b923059c32843e7ace21b656cdd50375ba13a2bb299d22bedc23c` |
| `v3.0.0-wal/kaban.db-wal` | `9c1c7343b524ca4ad09a8885f831a59539b165dae9110771eb7d0f042dc97b7a` |

The two `kaban.db` files are identical: the killed-writer fixture starts from the clean-close
file, and its new commit lives only in the WAL. Both main files hold `schema_version = 13` in
`index_meta`; the clean file has 3 nodes and 1 edge, and the WAL adds a fourth node.

## Recipe

Build the 3.0.0 binary from a detached worktree of `c15cd88` with its own
`rust-toolchain.toml` (Rust 1.99.0) and a separate target directory:

```sh
git worktree add --detach ../c15 c15cd88
cd ../c15
CARGO_TARGET_DIR=../target-c15 cargo build --release -p kaban -j4
KABAN=../target-c15/release/kaban
export KABAN_WORKSPACE=fixture KABAN_ACTOR_ID=fixture
```

### `v3.0.0/` (clean close)

In an empty directory, three `remember` calls, the third linked to the second, each a
separate process that exits normally:

```sh
$KABAN --db kaban.db --json remember Fact "The build cache lives in a shared target directory"
$KABAN --db kaban.db --json remember Failure "Release build ran out of memory with codegen-units 1"
$KABAN --db kaban.db --json remember Fact "Lowering codegen-units avoids the out of memory failure" \
  --link '{"rel":"FIXED_BY","from":"<id of the Failure>"}'
```

A normal close checkpoints the WAL into the main file and removes `kaban.db-wal`. Keep only
`kaban.db` (the run also leaves `kaban.db.owner` and `kaban.index/`, which are not part of the
fixture).

### `v3.0.0-wal/` (killed writer)

Copy the clean `kaban.db` into an empty directory and run `$KABAN --db kaban.db serve` there
as an MCP stdio server. Send `initialize`, then `notifications/initialized`, then one
`tools/call` of `remember` with `{"kind":"Fact","text":"A killed server leaves its committed
frames in the WAL"}`. After the committed response arrives, send SIGKILL to the server
process. The kill skips the close-time checkpoint, so the main file is byte-identical to the
clean one and the committed frames stay in a non-empty `kaban.db-wal`. Keep `kaban.db` and
`kaban.db-wal`; drop `kaban.db-shm` (SQLite rebuilds it), `kaban.db.owner` and `kaban.index/`.

Check a fixture without writing it by opening `file:kaban.db?mode=ro&immutable=1`, which
reads the main file only.
