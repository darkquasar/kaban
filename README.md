# kaban

Local, policy-gated memory and code context for coding agents. Kaban stores
records and their relationships in SQLite, retrieves them with lexical and graph
search, and exposes the same service through a CLI and an MCP stdio server.
It does not call a hosted model or run a network listener.

[Kaban 3.0.0-alpha.1 is available as a Linux prerelease](https://github.com/darkquasar/kaban/releases/tag/v3.0.0-alpha.1).
Packages target Linux x86-64 GNU, x86-64 musl and ARM64 GNU. Each archive includes
the separate `kaban` and `kaban-index` executables.
See [release workflow, runtime requirements and limitations](docs/developer/RELEASE.md).
For contributor orientation and preserved branch/evidence state, see [CODEBASE.md](CODEBASE.md).

## Build and first use

Use the Rust toolchain pinned in `rust-toolchain.toml` (1.99.0, also the MSRV)
and a native C compiler for bundled SQLite. From this checkout:

```sh
cargo build --release --locked -p kaban -p kaban-index

# Run from your project directory; supply a policy you have reviewed.
# The database is created on first open.
mkdir -p -m 700 "$HOME/.kaban"
export KABAN_DB="$HOME/.kaban/kaban.db"
export KABAN_POLICY="/absolute/path/to/reviewed-policy.yaml"
kaban remember Fact "The integration database uses SQLite" --tag storage
kaban recall "integration database"
kaban serve
```

Put `target/release/kaban` and `target/release/kaban-index` on your PATH first,
or use their absolute paths. Start a policy from [examples/policy.yaml](examples/policy.yaml);
if `--policy`/`KABAN_POLICY` is omitted, only built-in rules apply.
Keep the database and key directory in trusted, private parent directories.
`serve` (also the default when no subcommand is given) speaks MCP on stdin/stdout;
logs go to stderr. Do not type interactive commands into its protocol stream.

[Register your agent](docs/developer/REGISTER.md) for Claude Code, Codex, pi or Cursor,
and optionally install the [host/git hooks](hooks/README.md).
[Configuration](docs/developer/CONFIG.md) covers workspace binding, policies and engine budgets.

## What you can do

- Remember Facts, Decisions, Conventions, Failures and Episodes; recall matching
  cards and follow their graph relationships. Governance proposals need human
  acceptance; `why` explains which records govern a path.
- Review, accept, protect and explain records with the CLI. Serialize/import
  repository records and resolve conflicts with the [sync guide](docs/developer/SYNC.md);
  [review guidance](docs/developer/REVIEW.md) explains provenance and human decisions.
- Ingest Markdown and text documents with `kaban ingest docs/`; unchanged input
  is skipped. Structured ingestion provides bounded briefs for a host extractor,
  not an embedded model. See [ingestion](docs/developer/INGEST.md) for dialects, jobs and limits.
- Explicitly index a trusted repository with `kaban-index`, then read definitions,
  references, members, tests and context through the read-only `code_*` MCP tools.
  See [code graph](docs/developer/code-graph.md) before granting trust: indexing can execute
  repository tooling; isolation availability and snapshot provenance matter.
  `serve`, hooks and sync never launch the indexer. Without a matching snapshot,
  code tools report `no_snapshot`.

Default binary features are `ingest` (Markdown/plain text) and `code-graph`.
PDF extraction is opt-in:

```sh
cargo build --release -p kaban --features kaban-ingest/pdf
cargo build --release -p kaban --no-default-features
```

PDF uses `pdf_oxide`, with no fallback/OCR backend; an image-only PDF has no
extracted text. A build without PDF records PDF extraction as failed rather than
silently switching engines. The minimal build omits ingestion and code-tool
adapters, not every code-graph dependency. See [third-party licensing](docs/developer/THIRD_PARTY.md)
for feature-specific obligations.

## Boundaries and status

The denylist gates writes and rechecks reads under current policy; it is not a
guarantee that every possible secret will be detected. Existing database copies,
migration backups and filesystem snapshots remain separate sensitive copies.
A purge cannot erase those copies or guarantee physical erasure of SQLite free
pages; see the [storage/sync guide](docs/developer/SYNC.md) and `kaban doctor --vacuum`.

There is no working HTTP/REST transport or OpenAPI service. `serve --http` is a
rejected placeholder, not a listener. Embedding retrieval, `ask`/workflow
orchestration, `code_impact` and an automatic indexing loop are not delivered.
M6 two-store/personal-memory work is cancelled, not scheduled for a later version.
No refreshed Docker image is part of this release.

[Release notes](docs/developer/RELEASE.md) describe package validation and its limits.
[Offline Mintlify-structured user documentation](docs/index.mdx)
is available in the checkout; this is not a hosted documentation deployment.

## Development and licence

The workspace separates model/store, policy, service/API, CLI/MCP adapters,
document ingestion, code-graph reading and the explicit indexer. Start with
[the current delivery protocol](../specs/kaban/00-project/archive/PROTOCOL.md) for contribution gates,
and `docs/developer/rust-style/` for local Rust conventions.

For CI commands, see [developer CI](docs/developer/CI.md). The
[Tart runner image](runners/tart/README.md) contains the Packer recipe and setup
inputs for a local Linux ARM64 environment on Apple silicon.

Apache-2.0 for Kaban; see [LICENSE](LICENSE), [NOTICE](NOTICE) and
[third-party sources and redistribution gaps](docs/developer/THIRD_PARTY.md).
