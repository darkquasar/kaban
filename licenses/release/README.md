# Full-profile binary release notices

Each of the four target directories contains `manifest.json`,
`THIRD_PARTY_LICENSES.txt` and `RUST_STANDARD_LIBRARY_NOTICES.html` for both `kaban`
and `kaban-index` with defaults plus PDF and Markdown splitting. Archives include
the selected directory as `third-party/`.

Check a committed bundle without Cargo or network access:

```sh
python3 scripts/release/licenses.py check --target aarch64-apple-darwin
```

After changing dependencies, manifests, target configuration, collector or toolchain,
review upstream terms and regenerate all four exact closures using cached sources:

```sh
for target in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu \
              x86_64-unknown-linux-musl aarch64-apple-darwin; do
  cargo fetch --locked --target "$target"
  python3 scripts/release/licenses.py generate --target "$target"
done
```

The generator resolves `-p kaban -p kaban-index` and
`--features kaban-ingest/pdf,kaban-ingest/md-splitter` for each target, including
normal/build/proc-macro dependencies conservatively and excluding dev-only edges.
A missing upstream notice fails generation. `upstream/sources.json` records exact
revision provenance for omissions in published crates; version-specific supplements
must be reviewed after upgrades. Source-header attribution and native/font/ICU data
notices are retained unmodified, including their original whitespace. Runtime
notices must be reviewed when changing Rust.

Inspect the generated inventory and diff before committing. Hash checks establish
that reviewed inputs/bundles match; they do not review new upstream terms.
See [THIRD_PARTY.md](../../docs/developer/THIRD_PARTY.md) for collection decisions.
