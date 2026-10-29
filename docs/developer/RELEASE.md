# Local M7 release handoff

This is an unpublished, local handoff under OWN-123, not a public release.
`Cargo.toml` still declares **3.0.0-alpha.1**. M7 and `3.0.0` milestone/changelog
labels do not identify the executable version. No version bump, tag, push or
publication is included.

## Artifacts and identity

The coordinator's local package location is
`../specs/kaban/specs/00-project/evidence/s31-m7-local-release/` outside this source worktree. Its manifest
and acceptance record must bind each supplied binary to its full SHA-256, byte
size, target, profile, feature set and source revision before it is called usable.
The retained native inputs are under
`../specs/kaban/specs/00-project/evidence/s30-l0-budget-logs/artifacts/`, with exact identities in
`../specs/kaban/specs/00-project/evidence/s30-l0-budget-acceptance.md`.

The package scope is `kaban` default and `--no-default-features` release binaries
for **aarch64-unknown-linux-gnu**. Retained metadata requires the aarch64 loader
`/lib/ld-linux-aarch64.so.1`, `libgcc_s.so.1`, `libm.so.6`, `libc.so.6` and
GLIBC_2.39 symbols. These require a compatible aarch64 GNU/Linux runtime;
they are not static musl executables and do not imply an x86_64 build. A native
source build is described in the [README](../../README.md). `kaban-index` is not
bundled: build it separately with `cargo build --release -p kaban-index` if needed.

The strict retained size limits are **<25 MiB default** and **<15 MiB no-default**
(26,214,400 and 15,728,640 bytes; OWN-B, spec 05 SS-13b: the default build now
links the Tantivy text index, measured at 20,213,088 bytes (19.28 MiB), the
no-default build at 13,067,904 bytes (12.46 MiB), see
`../specs/kaban/specs/00-project/evidence/s05-t5-sizes.md`). PDF and benchmark feature builds are not those
two feature profiles. The minimal build disables ingestion and code-tool adapters
("code-graph off"); the Tantivy text index is linked in every build since spec 05
OWN-M (the store owns it; a store without an index does not exist), so the no-default
figure above predates it and is remeasured as proposed OWN-N.

Static musl binaries have earlier stream evidence only. They are not packaged or
newly qualified by this handoff. Neither a configured Rust target nor an old size
row establishes availability of a current static executable. No refreshed Docker
image is delivered.

## Qualification scope

OWN-123 cancels further M7 tests, benchmarks, smoke, verification campaigns and
review rounds, retaining only `cargo deny check` and final-built binary sizes.
Exact applicable evidence may be reused instead of rerun. Consult the coordinator's
acceptance record for commands, exits, checked identities and logs; this document
does not itself attest a completed gate. Historical performance results belong
to their original binary, target, features and corpus, not automatically to the
final head. Omitted startup, latency, memory, static-build and protocol checks
remain omitted, not passed.

OWN-119 acceptance is separate: the earlier aggregate's inner CI3 retained a
known host-negative mandatory skip. That skip is neither a product pass nor a
demonstrated product failure. The independent OWN-119 review subsequently settled
with no findings; its acceptance record retains the CI3 limitation. That review
does not turn omitted M7 qualification into passes.

## User-visible limits

- MCP uses stdio; HTTP/REST/OpenAPI is unavailable. The parser's `serve --http`
  placeholder is explicitly rejected, not a working transport.
- M6 two-store/personal-memory functionality is cancelled. Existing single-store
  workspaces and resources remain; no global routing or merged recall is promised.
- No embeddings, `ask`/workflow orchestration, automatic code indexing or refreshed
  container distribution is delivered. PDF extraction is opt-in, without OCR.
- Code graph queries require an explicitly produced snapshot; indexing has separate
  trust, toolchain and isolation requirements in [code-graph.md](code-graph.md).

## Local usability is not external-distribution clearance

Include the Kaban LICENSE and NOTICE and read [THIRD_PARTY.md](THIRD_PARTY.md).
The selected inventory is not a complete transitive license bundle. `cargo deny`
is dependency-policy evidence, not proof that every upstream notice, copyright,
license text or native-library obligation has been collected. External distribution
needs an artifact-specific dependency/feature inventory and its required texts;
that legal/compliance gap does not prevent a clearly labelled local handoff and
does not authorize publication. [Offline user docs](../index.mdx) are available
in the checkout; no live documentation site is deployed.
