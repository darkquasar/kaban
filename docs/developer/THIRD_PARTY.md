# Third-party sources and release notices

Kaban is licensed under [Apache-2.0](../../LICENSE). Dependencies retain their
own licences. [NOTICE](../../NOTICE) records project attribution and vendored
documentation sources; the binary archives also carry the resolved dependency
and runtime notices described here.

## Linux release profile

The committed [release bundles](../../licenses/release/README.md) cover these
configurations, each building **both `kaban` and `kaban-index`** with default
features:

| Target | Registry packages in inventory |
|---|---:|
| `x86_64-unknown-linux-gnu` | 239 |
| `x86_64-unknown-linux-musl` | 239 |
| `aarch64-unknown-linux-gnu` | 237 |

The inventory comes from `cargo tree --locked --target TARGET -p kaban -p
kaban-index --edges normal,build`. Build dependencies and proc macros are
included conservatively; dev-only dependencies are excluded. Each target's
`manifest.json` records resolved versions, features, declared licence expressions,
source repositories, individual notice hashes, and the complete bundle hash.
The generator checks that both selected packages and a nonempty registry closure
were resolved; malformed Cargo output is an error.

The manifest binds the notices to `Cargo.lock`, the workspace/member manifests,
the toolchain, Cargo target configuration, the collector, and supplementary
upstream texts. The release check refuses changed inputs or damaged bundles.
Archive members under `third-party/` contain the target's manifest and full
`THIRD_PARTY_LICENSES.txt`, plus the original runtime inventory in
`RUST_STANDARD_LIBRARY_NOTICES.html`; Kaban's own `LICENSE` and `NOTICE` are separate members.

```text
+--------------------+  resolve  +----------------------+  collect  +------------------+
| Lock + release     | ========> | Target dependency    | ========> | Licence / notice |
| package selection  |           | versions + features  |           | bundle + manifest|
+--------------------+           +----------------------+           +------------------+
```

## Collection decisions

All supplied licence alternatives and notices are retained. Where alternatives
are offered, the distribution chooses MIT, otherwise Apache-2.0 where available;
additional `AND` obligations and embedded native-code terms remain applicable.
Retaining another alternative's text does not select that alternative.

The collection includes these less obvious sources:

- `encoding_rs`: MIT plus the BSD-3-Clause WHATWG text and copyright notices.
- `unicode-ident`: MIT plus Unicode-3.0; nested Unicode table notices from other
  crates are also retained.
- `rust-stemmers`: the MIT Rust wrapper and BSD-3-Clause Snowball algorithms.
- `libsqlite3-sys`: the MIT binding and the exact bundled SQLite amalgamation's
  copyright disclaimer. SQLCipher sources are not used by this profile.
- `zstd-sys`: the binding's terms and bundled libzstd `LICENSE` and `COPYING`.
- Apache DataSketches: upstream `LICENSE` and `NOTICE`, omitted from its crate
  archive, retrieved at the crate's recorded upstream revision.
- RMCP, Tantivy subcrates, `ownedbytes`, and `whatlang`: omitted licence texts
  retrieved at their crate archives' `.cargo_vcs_info.json` revisions.
- `htmlescape`: its manifest offers Apache-2.0, MIT, or MPL-2.0 but ships no
  licence text. Apache-2.0 is selected and its standard text is retained.
- Rust 1.99.0: the standard-library copyright/dependency inventory and licence
  texts from its official documentation component, plus the compiler-builtins
  licence at the pinned Rust commit. This runtime inventory is conservative.
- Static musl: musl 1.2.5's complete copyright terms from its official archive.
  Rust's pinned build recipe selects that version and applies upstream security
  patches. GNU host shared libraries are supplied by the operating system and
  their binaries are not included in the archives.

Supplement provenance is recorded in
[`upstream/sources.json`](../../licenses/release/upstream/sources.json). The
collector refuses reuse of version-specific supplements after an upgrade, and
requires a runtime-notice review when the pinned Rust version changes. Collected
source and licence texts are unmodified; deduplication only shares identical
complete text blocks while preserving every package/path association.

## Other configurations

These bundles cover the named release profiles. Optional PDF, `md-splitter`,
benchmark configurations, external producer executables, and other targets need
their own resolved inventories and notices before distribution. PDF remains
opt-in and is absent from the default archives. `cargo deny check` enforces the
repository dependency policy; it does not replace collecting distribution texts.

## Vendored documentation

`docs/developer/rust-style/` includes MIT-licensed material from Leonardo
Maldonado's `leonardomso/rust-skills` v1.5.1 and Microsoft Pragmatic Rust
Guidelines. Preserve
`docs/developer/rust-style/references/LICENSE-rust-skills` and
`LICENSE-microsoft-rust-guidelines` when copying that documentation. Those docs
are not included in the binary archives. Prior-art references in NOTICE do not
claim copied implementation.
