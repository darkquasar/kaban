# Full-profile binary release notices

The repository keeps only what the release cannot derive:

- `inventory.json`: the reviewed closure for all four targets. It records each
  registry package's version, declared licence expression and the SHA-256 of every
  collected notice text, the targets that include it (omitted when all four do), each
  target's notice bundle hash, the Rust runtime notice hashes and the hashes of the
  inputs (lock file, manifests, toolchain, Cargo configuration, collector, supplements).
- `upstream/`: licence texts for crates whose archives omit them, the musl and
  compiler-builtins terms, and their provenance in `upstream/sources.json`.

Release archives still carry `third-party/manifest.json`, `THIRD_PARTY_LICENSES.txt`
and `RUST_STANDARD_LIBRARY_NOTICES.html` for both `kaban` and `kaban-index` with
defaults plus PDF and Markdown splitting. `scripts/release/build.sh` regenerates them
after the build from the locked Cargo sources and the pinned toolchain:

```text
+----------------+  check (offline)  +-------------+  stage (cargo fetch, rustc)  +--------------+
| inventory.json | ================> | build.sh    | ===========================> | third-party/ |
| + upstream/    |  inputs unchanged | cargo build |  regenerate, compare hashes  | in archive   |
+----------------+                   +-------------+                              +--------------+
```

`check` refuses changed inputs without Cargo or network access. `stage` refuses any
package, licence expression, notice text, Rust runtime file or bundle byte that differs
from the inventory, and writes nothing in that case:

```sh
python3 scripts/release/licenses.py check --target aarch64-apple-darwin
python3 scripts/release/licenses.py stage --target aarch64-apple-darwin --output DIRECTORY
```

After changing dependencies, manifests, target configuration, collector, supplements
or toolchain, review upstream terms and regenerate the inventory for all four targets
(the collector runs `cargo fetch --locked --target` for each):

```sh
python3 scripts/release/licenses.py generate
```

The generator resolves `-p kaban -p kaban-index` and
`--features kaban-ingest/pdf,kaban-ingest/md-splitter` for each target, including
normal/build/proc-macro dependencies conservatively and excluding dev-only edges.
A missing upstream notice fails generation; version-specific supplements must be
reviewed after upgrades. Source-header attribution and native/font/ICU data notices
are retained unmodified, including their original whitespace. The Rust standard
library notices come from the pinned toolchain's `rustc` component
(`share/doc/rust/COPYRIGHT-library.html` and `share/doc/rust/licenses/*.txt`) and
must be reviewed when changing Rust.

Review the `inventory.json` diff before committing: a changed notice hash means
that package's licence text changed. Hash checks establish that the shipped notices
match the reviewed inventory; they do not review new upstream terms.
See [THIRD_PARTY.md](../../docs/developer/THIRD_PARTY.md) for collection decisions.
