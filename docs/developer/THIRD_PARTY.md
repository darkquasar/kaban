# Third-party sources and redistribution limits

Kaban's own license is [Apache-2.0](../../LICENSE). [NOTICE](../../NOTICE) records
project attribution and vendored documentation sources. Dependencies retain their
own licenses: the workspace license does not relicense them.

This is a selected source guide, not an exhaustive transitive inventory or legal
clearance. Versions below match `Cargo.lock`; expressions and filenames were read
from locally cached crate manifests and source archives (no network lookup).

| Component | Version / declared license | Source texts to retain or inspect |
|---|---|---|
| rmcp (MCP adapter) | 3.5.1 / Apache-2.0 | crate license files and upstream notices |
| rusqlite | 0.40.2 / MIT | `LICENSE` |
| libsqlite3-sys (bundled SQLite enabled) | 0.38.2 / MIT for the binding | `LICENSE`; also inspect bundled `sqlite3/` source notices separately |
| encoding_rs (document decoding) | 0.8.42 / `(Apache-2.0 OR MIT) AND BSD-3-Clause` | `LICENSE-APACHE`, `LICENSE-MIT`, `LICENSE-WHATWG`, `COPYRIGHT`; an OR choice does not remove the AND obligation |
| blake3 | 1.8.7 / `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception` | crate license texts and applicable implementation notices |
| pdf_oxide (optional PDF) | 0.3.78 / MIT OR Apache-2.0 | `LICENSE-MIT`, `LICENSE-APACHE`, `NOTICE`, `TRADEMARKS.md`; its transitive dependencies need separate accounting |

For this machine, sources are under
`/home/developer/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/<name>-<version>/`.
That cache path is not a portable distribution asset. `Cargo.lock` supplies exact
registry versions/checksums, not all license texts or the feature-resolved linked
dependency list. Root and member `Cargo.toml` files describe declared dependencies;
target conditions, features, native code and build/dev-only dependencies matter.

Default `kaban` enables ingestion and code-tool adapters. `--no-default-features`
does not remove every code-graph dependency. `kaban-ingest/pdf` adds PDFOxide and
its dependency closure; `kaban-ingest/md-splitter` adds the optional splitter and
its data/dependencies. `bench` adds test-support code. Treat each delivered binary,
including a separately built `kaban-index`, as a distinct inventory scope.

## Vendored documentation

`docs/developer/rust-style/` includes MIT-licensed material from Leonardo Maldonado's
`leonardomso/rust-skills` v1.5.1 and Microsoft Pragmatic Rust Guidelines. Their
full notices are retained as `docs/developer/rust-style/references/LICENSE-rust-skills`
and `LICENSE-microsoft-rust-guidelines`. Preserve those files when copying that
documentation. Prior-art references in NOTICE do not claim copied implementation.

## Known distribution gap

No complete artifact-specific transitive license/NOTICE collection accompanies
this source guide. In particular, optional PDF/native code and combined-license
data notices cannot be discharged by Kaban's Apache license alone. Before any
external distribution, identify the actual linked components for the exact target
and feature profile, choose permitted alternatives where appropriate, collect
their required license/copyright/NOTICE texts, and include native/runtime terms
where applicable. Preserve source modification notices when required.

`deny.toml` and a successful `cargo deny check` establish only the checked policy
scope and inputs. They do not establish that this NOTICE or a package satisfies
every upstream redistribution obligation. No publication is authorized here.
