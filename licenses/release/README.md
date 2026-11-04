# Linux binary release notices

Each target directory contains `manifest.json` and
`THIRD_PARTY_LICENSES.txt` plus `RUST_STANDARD_LIBRARY_NOTICES.html` for the default-feature `kaban` + `kaban-index`
package selection. Release archives include the directory as `third-party/`.

To check a committed bundle without Cargo or network access:

```sh
python3 scripts/release/licenses.py check --target x86_64-unknown-linux-gnu
```

After a dependency, manifest, target configuration, collector, or toolchain change,
review the affected upstream licences, update version-specific supplements if
needed, and regenerate **all three** bundles:

```sh
cargo fetch --locked --target x86_64-unknown-linux-gnu
cargo fetch --locked --target x86_64-unknown-linux-musl
cargo fetch --locked --target aarch64-unknown-linux-gnu
python3 scripts/release/licenses.py generate --target x86_64-unknown-linux-gnu
python3 scripts/release/licenses.py generate --target x86_64-unknown-linux-musl
python3 scripts/release/licenses.py generate --target aarch64-unknown-linux-gnu
```

The collector reads cached crate sources and fails when a selected crate has no
licence/notice text and no reviewed supplement. `upstream/sources.json` records
supplement origins; these are required because some published crates omit their
repository licence/NOTICE files. Keep those exact upstream texts, including native
and runtime notices, when regenerating. Rust runtime notices must be refreshed
when changing the pinned toolchain.

Inspect the generated inventory and diff before committing. A successful checksum
check establishes that the reviewed inputs and bundle match; it does not review
new upstream licensing terms automatically. Details and collection decisions are
in [THIRD_PARTY.md](../../docs/developer/THIRD_PARTY.md).
