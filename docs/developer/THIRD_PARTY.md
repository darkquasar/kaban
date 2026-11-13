# Third-party sources and release notices

Kaban is licensed under [Apache-2.0](../../LICENSE). Dependencies retain their
own licences. [NOTICE](../../NOTICE) records project attribution and vendored
documentation sources; the binary archives also carry the resolved dependency
and runtime notices described here.

## Four-target full release profile

The reviewed [licence inventory](../../licenses/release/README.md) covers these
configurations, each building **both `kaban` and `kaban-index`** with default
features plus `kaban-ingest/pdf,kaban-ingest/md-splitter`:

| Target | Registry packages in inventory |
|---|---:|
| `x86_64-unknown-linux-gnu` | 338 |
| `x86_64-unknown-linux-musl` | 338 |
| `aarch64-unknown-linux-gnu` | 337 |
| `aarch64-apple-darwin` | 340 |

The closure comes from `cargo tree --locked --target TARGET -p kaban -p
kaban-index --features kaban-ingest/pdf,kaban-ingest/md-splitter --edges normal,build`. Build dependencies and proc macros are
included conservatively; dev-only dependencies are excluded. The generator checks
that both selected packages and a nonempty registry closure were resolved;
malformed Cargo output is an error.

Generated notice bundles are not committed. `licenses/release/inventory.json`
records, for all four targets, each package's version, declared licence expression
and notice text hashes, each target's bundle hash, the Rust runtime notice hashes,
and the hashes of `Cargo.lock`, the workspace/member manifests, the toolchain,
Cargo target configuration, the collector and the supplements. Before building,
the release check refuses changed inputs. After building, the release regenerates
the notices from the locked Cargo sources and the pinned toolchain and refuses any
drift from the inventory. Archive members under `third-party/` contain the
target's `manifest.json` (versions, features, repositories, notice hashes) and full
`THIRD_PARTY_LICENSES.txt`, plus the original runtime inventory in
`RUST_STANDARD_LIBRARY_NOTICES.html`; Kaban's own `LICENSE` and `NOTICE` are separate members.

```text
+--------------------+  resolve  +----------------------+  collect  +-----------------------+
| Lock + release     | ========> | Target dependency    | ========> | Notices, compared     |
| package selection  |           | versions + features  |           | with inventory.json   |
+--------------------+           +----------------------+           +-----------------------+
```

## Collection decisions

All supplied licence alternatives and notices are retained. Where alternatives
are offered, the distribution chooses MIT, otherwise Apache-2.0 where available;
additional `AND` obligations and embedded native-code terms remain applicable.
Retaining another alternative's text does not select that alternative.

The collection includes these less obvious sources:

- `pdf_oxide`: MIT/Apache alternatives, its NOTICE and nested DejaVu/fallback-font
  notices. The selected features are `icc` and `legacy-crypto`; retaining fallback
  notices does not claim CJK/emoji font features are enabled.
- `qcms`: complete original MIT headers, including both Mozilla Foundation and
  Corporation copyright variants and Marti Maria attribution.
- `text-splitter` and ICU: MIT splitter terms and Unicode-3.0 licences covering
  embedded segmentation/normalization/property data and their IBM attribution.
- `alloc-stdlib` and `taffy`: omitted repository-root BSD/MIT texts recovered at
  their crate-recorded revisions. No dependency version was changed.
- Brotli: both BSD and MIT texts; the decompressor's exact Google 2013 MIT
  attribution is retained alongside the complete upstream Brotli MIT terms and
  its Dropbox BSD licence, even though its filename-matched LICENSE has only BSD.
- `encoding_rs`: MIT plus the BSD-3-Clause WHATWG text and copyright notices.
- `unicode-ident`: MIT plus Unicode-3.0; nested Unicode table notices from other
  crates are also retained.
- `rust-stemmers`: the MIT Rust wrapper and BSD-3-Clause Snowball algorithms.
- `libsqlite3-sys`: the MIT binding and the exact bundled SQLite amalgamation's
  copyright disclaimer. SQLCipher sources are not used by this profile.
- `zstd-sys`: the binding's terms and bundled libzstd `LICENSE` and `COPYING`.
- Apache DataSketches: upstream `LICENSE` and `NOTICE`, omitted from its crate
  archive, retrieved at the crate's recorded upstream revision.
- Tantivy subcrates, `ownedbytes`, and `whatlang`: omitted licence texts
  retrieved at their crate archives' `.cargo_vcs_info.json` revisions. The
  `ownedbytes` revision's LICENSE is byte-identical to the Tantivy one, so one
  committed copy serves both; `sources.json` records both revisions.
- RMCP: from 3.9.0 the `rmcp` and `rmcp-macros` archives ship the upstream
  `LICENSE` (Apache-2.0 with the MIT-to-Apache transition preamble), so it is
  collected from the archives and no supplement is kept.
- `htmlescape`: its manifest offers Apache-2.0, MIT, or MPL-2.0 but ships no
  licence text. Apache-2.0 is selected and its standard text is retained.
- Rust 1.99.0: the standard-library copyright/dependency inventory
  (`COPYRIGHT-library.html`) and licence texts, read at release time from the
  pinned toolchain's `rustc` component and checked against the inventory hashes,
  plus the committed compiler-builtins licence at the pinned Rust commit. The HTML
  is shipped unmodified: it is the Rust project's own notice for code linked from
  the standard library, carrying per-crate copyright lines that the plain licence
  texts lack. This runtime inventory is conservative.
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

## openCypher parser selection (spec 11 T1)

The `GRAPH_TABLE` pattern language (T15) borrows a parser instead of writing one. Candidates
were taken in the order `open-cypher`, `cyrs`, `decypher` and judged against four conditions:
it builds on the pinned 1.99.0 toolchain, its licence is on the `deny.toml` allow list, it adds at
most 15 packages to `Cargo.lock`, and it parses a full read-only query (`MATCH ... [WHERE] ...
RETURN ...`) for one sample of each T15 construct class (a) to (e).

| Candidate | Version | Licence | MSRV | Result |
|---|---|---|---|---|
| `open-cypher` | 0.5.0 | Apache-2.0 | 1.88 | Selected. Builds on 1.99.0; 5 new packages; every class parses (see below). |
| `cyrs` | 0.3.3 | MIT | unknown | Rejected without a build: it is a command-line tool ("C-c C-v"), not a Cypher parser. |
| `decypher` | 0.2.0-alpha.10 | EUPL-1.2 OR MIT OR Apache-2.0 | 1.94 | Not evaluated: an earlier candidate passed, and it is an alpha release. |

`open-cypher` is pinned exactly (`=0.5.0`) in the workspace manifest. It is a lexer and parser for
the openCypher 2024.3 grammar (Logos tokens, LALRPOP parser, spanned AST) and parses syntax only.
Its normal dependencies add these lock entries, all licensed `MIT OR Apache-2.0` except the
parser itself:

- `open-cypher` 0.5.0 (Apache-2.0)
- `lalrpop-util` 0.23.2 (Apache-2.0 OR MIT)
- `logos` 0.16.1, `logos-derive` 0.16.1, `logos-codegen` 0.16.1 (MIT OR Apache-2.0)

`logos-codegen` pulls `fnv`, `proc-macro2`, `quote`, `regex-automata`, `regex-syntax` and `syn`,
which were already in the lock. The optional `serde` feature is off.

Probe results, one sample per class, each parsed to a syntax tree without error:

- (a) paths: `MATCH (a:Function)-[:CALLS]->(b:Function) WHERE a.name = 'x' RETURN b.name AS n`
- (b) traversal: `MATCH (m:Commit {id: 'main'})-[*1..3]->(x) RETURN x.id AS id` and
  `MATCH p = shortestPath((a:N)-[:R*..5]->(b:N)) WHERE a.id = 1 RETURN b.id AS id`
- (c) shaping: `OPTIONAL MATCH`, `WITH ... count(b) AS c WHERE c > 1`, `DISTINCT`, `ORDER BY`,
  `SKIP`, `LIMIT`
- (d) composition: `UNION`, `CASE WHEN ... END` and `EXISTS { MATCH ... [WHERE ...] }`
- (e) functions: `RETURN pagerank(a) AS pr, toLower(a.name) AS n, count(*) AS c`

Known gap for T15: the bare-pattern subquery form `EXISTS { (a)-[:R]->(m:M) }` is a syntax error
in this parser, while `EXISTS { MATCH (a)-[:R]->(m:M) }` and `exists((a)-[:R]->(:M))` parse. The
parser also accepts write clauses (`MATCH (a) DELETE a`), so the refusal inventory stays the
pattern layer's job.

## Other configurations

These bundles cover the four full-profile downloads, including PDF and
`md-splitter`. Other source/benchmark configurations, external producer executables
and other targets need their own resolved inventories/notices before distribution.
Cargo defaults remain lean. `cargo deny check` enforces the
repository dependency policy; it does not replace collecting distribution texts.

## Vendored documentation

`docs/developer/rust-style/` includes MIT-licensed material from Leonardo
Maldonado's `leonardomso/rust-skills` v1.5.1 and Microsoft Pragmatic Rust
Guidelines. Preserve
`docs/developer/rust-style/references/LICENSE-rust-skills` and
`LICENSE-microsoft-rust-guidelines` when copying that documentation. Those docs
are not included in the binary archives. Prior-art references in NOTICE do not
claim copied implementation.
