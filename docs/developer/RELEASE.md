# Linux releases

The first release is **3.0.0-alpha.1**, matching `Cargo.toml`. PR4 prepares the
release workflow; merging it does not create a tag or publish a release.

Each archive contains `kaban` and `kaban-index`, built together with the default
features, plus Kaban's LICENSE and NOTICE and the target's dependency licence
bundle. PDF, optional Markdown splitting, language producer tools and container
images are not included.

| Archive target | Runtime | Qualification |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | Linux x86-64; glibc floor recorded from the binaries | Main x86-64 CI plus checks of the packaged binaries |
| `x86_64-unknown-linux-musl` | Linux x86-64; static musl executables | Main musl build plus checks of the packaged binaries |
| `aarch64-unknown-linux-gnu` | Linux ARM64; glibc floor recorded from the binaries | Native ARM64 build and checks of the packaged binaries; full functional CI remains x86-64 |

GNU builds use Ubuntu 24.04. Read the measured minimum glibc version in each
package's metadata and release notes before choosing a download; an Ubuntu 24.04
build does not establish support for older distributions. The musl archive is the
portable x86-64 option. macOS and Windows packages require portability work and
are not part of this release.

## Build, validate and publish

```text
+-------------+  PR checks  +-------------------+
| PR4 / fixes | ==========> | Linux packages    |
+-------------+             | build + run checks|
                            +-------------------+

+-------------+  push v*    +-------------------+  archive  +------------------+
| Green main  | ==========> | Authorize + build | ========> | GitHub Release   |
+-------------+             +-------------------+          +------------------+
```

The release workflow validates packages on release-related pull requests. It
builds each selected configuration once with `--locked -p kaban -p kaban-index`.
GNU builds are native; musl uses `--target x86_64-unknown-linux-musl`. Compatible
Cargo dependency caches may shorten compilation; a cache miss never bypasses a
check. Archives move between jobs within the same workflow run.

Validation runs the existing MCP/CLI smoke script against the packaged `kaban`,
and ordinary CLI commands against both executables, including `kaban-index`'s
dependency-free scaffold report. This checks startup and basic behaviour of the
actual downloads without installing language producers or rerunning the complete
functional suite. It does not qualify a complete indexing toolchain on every
target. Packaging also checks binary size, licence bundle freshness, executable
permissions and runtime linkage, and records SHA-256 checksums.

To publish after the PR is merged:

1. Select a main tip whose **CI** push run has completed successfully. A successful
   PR run alone is insufficient. An intermediate commit that was never a main
   push tip has no eligible run.
2. Confirm `Cargo.toml` contains the intended version and that the committed
   licence bundles match `Cargo.lock` and the selected manifests.
3. As repository owner `darkquasar`, create and push the matching version tag:

   ```sh
   git fetch origin
   git tag -a v3.0.0-alpha.1 <green-main-commit> -m 'Kaban 3.0.0-alpha.1'
   git push origin v3.0.0-alpha.1
   ```

4. Inspect the release workflow and its generated metadata and checksums. Versions
   with a prerelease suffix publish as GitHub prereleases.

The tag gate checks repository, owner identity and triggering actor, version
agreement, main ancestry and the exact commit's successful main push CI. It fails
immediately if CI is still running; rerun after CI succeeds. Only the final
publication job receives `contents: write`, and it rechecks authorization before
publishing. Pull request validation cannot publish.

There is one release build path. Main CI does not upload a rolling set of release
assets, and tag jobs do not rerun the entire functional suite. Homebrew and
installer automation can be added separately.

## Dependency licence bundles

The committed bundles are under `licenses/release/<target>/`. Each records its
locked dependency graph, build configuration and required upstream texts,
including bundled native code. Check a target without network access:

```sh
python3 scripts/release/licenses.py check --target x86_64-unknown-linux-gnu
```

Dependency, target or feature changes require regenerating and reviewing the
corresponding bundle before publication. See [THIRD_PARTY.md](THIRD_PARTY.md).
`cargo deny check` remains a dependency policy check; it does not replace the
redistribution texts.

## User-visible limits

- Rust semantic indexing's external producer currently advertises
  `aarch64-unknown-linux-gnu` only. An x86-64 download does not provide Rust
  semantic indexing. Producer executables are not bundled on ARM64 either; see
  [code-graph.md](code-graph.md) for setup and trust requirements.
- PDF is optional and absent from these binaries. Build with
  `--features kaban-ingest/pdf` if needed; a PDF download would be a separate
  feature, size and licence configuration.
- MCP uses stdio. HTTP/REST/OpenAPI, embeddings, `ask`/workflow orchestration,
  automatic indexing and two-store/personal-memory routing are unavailable.
- Code queries need an explicitly produced snapshot. Indexing can execute
  repository tools and has separate isolation and trust requirements.

The earlier M7 handoff was local and unpublished. Its retained ARM64 measurements
and omitted checks describe those historical artifacts, not these release bytes.
Current CI enforces default GNU `kaban` below 25 MiB; the optional no-default size
table uses 17 MiB. Release metadata supplies the actual measured sizes.
