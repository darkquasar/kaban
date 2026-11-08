# Full-profile Linux and Apple Silicon packages

The workspace version is **3.0.0**. See the
[releases page](https://github.com/darkquasar/kaban/releases) for available downloads.
The historical [3.0.0-alpha.1 Linux prerelease](https://github.com/darkquasar/kaban/releases/tag/v3.0.0-alpha.1)
uses a different lean Linux profile. Publication requires the authorized release
workflow; preparing packages or merging a PR publishes nothing.

Each target uses one canonical build:

```sh
cargo build --release --locked -p kaban -p kaban-index \
  --features kaban-ingest/pdf,kaban-ingest/md-splitter
```

Archives contain exactly `kaban` and `kaban-index` as executables, LICENSE, NOTICE,
`third-party/` licences/runtime notices, BUILD.json and RELEASE.json. PDF extraction
uses bundled `pdf_oxide`; Markdown splitting includes the compiled ICU data. No
Cargo, external PDF renderer, producer executable, SDK or language runtime is bundled
or needed for memory, MCP and supported document ingestion. Source defaults/minimal
builds remain separate choices.

| Target | Runtime and qualification |
| --- | --- |
| `x86_64-unknown-linux-gnu` | Native Linux x86-64; glibc >= 2.39; generic functional CI and extracted package smoke |
| `aarch64-unknown-linux-gnu` | Native Linux ARM64; glibc >= 2.39; extracted package smoke; no complete ARM64 functional matrix |
| `x86_64-unknown-linux-musl` | Static Linux x86-64; no glibc dependency; its own extracted executables run on the Ubuntu x86-64 host |
| `aarch64-apple-darwin` | Apple Silicon; deployment/load-command minimum 15.0, tested local patch 15.6.1 and hosted 15.8.2; focused native CI and extracted package smoke |

The macOS 15.0 deployment setting alone does not prove launch/API compatibility on
15.0; the earliest exercised patch is **15.6.1**. No macOS 13/14, Intel or Windows
support is claimed. RELEASE.json records each package's actual tested macOS patch,
SDK, minimum load command and system dylibs. Non-system dylibs are refused.
GNU packages record their actual symbol requirements and refuse a floor above 2.39.

## Download and install both executables

Choose your target from the table. For example, on Apple Silicon, after stable
publication:

```sh
version=3.0.0
target=aarch64-apple-darwin
archive="kaban-v$version-$target.tar.gz"
base="https://github.com/darkquasar/kaban/releases/download/v$version"
curl -fLO "$base/$archive"
curl -fLO "$base/SHA256SUMS"
awk -v name="$archive" '$2 == name { print; found++ } END { if (found != 1) exit 1 }' \
  SHA256SUMS > "$archive.sha256"
# Use the checksum command supplied by the host.
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum -c "$archive.sha256"
else
  shasum -a 256 -c "$archive.sha256"
fi
tar -xzf "$archive"
package="kaban-v$version-$target"
mkdir -p "$HOME/.local/bin"
install -m 755 "$package/kaban" "$package/kaban-index" "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
kaban --version
kaban-index --version
```

Run the download/check/extract steps in a fresh directory; proceed to installation
only after checksum verification succeeds. Checksums establish consistency with the
trusted release download, not a separate publisher signature. Both binaries must
have the same version. Before publication, maintainers can download the four PR
artifacts with `gh run download RUN_ID --name package-TARGET`; verify the included
`.tar.gz.sha256`, then use the same extraction/install steps. Those artifacts are
validation candidates, not published releases.

Run from your project with private data paths (inspect existing directory modes;
`mkdir -m` sets modes only on creation):

```sh
mkdir -p -m 700 "$HOME/.kaban"
export KABAN_HOME="$HOME/.kaban"
export KABAN_DB="$KABAN_HOME/kaban.db"
export KABAN_WORKSPACE="my-project"
# Optional: export KABAN_POLICY="/absolute/path/to/reviewed-policy.yaml"
kaban remember Fact "The package installation check succeeded"
kaban recall "package installation"
kaban doctor
kaban serve
```

`serve` speaks MCP on stdin/stdout; register its absolute executable path using
[REGISTER.md](REGISTER.md). The database path must be explicit: its CLI default is
`kaban.db` in the working directory, independently of KABAN_HOME. Engine configuration
is `KABAN_HOME/config.yaml`; policy is a separate reviewed file. Audit/codegraph keys
live under `KABAN_HOME/keys/`; repository trust and snapshots live under
`KABAN_HOME/codegraph/<repo-id>/`. Keep these locations private and outside repositories.

Language producers are [separate installs](code-graph.md). Qualified versions include
Go 1.26.0/scip-go 0.2.7, Node 22.23.4/scip-typescript 0.4.0 and Rust 1.99.0 with
producer 0.1.0-dev. Rust producer qualification covers Linux ARM64 GNU and native
macOS ARM64 with root-owned CLT; runner-owned Xcode is correctly refused. Install
producers outside both indexed repositories and KABAN_HOME. Explicitly trust and
index a repository; serve/hooks/sync do not launch an indexer. On macOS, preferred
isolation warns and uses trusted `none`; required isolation refuses exit 8. This
is not Linux namespace isolation.

Hooks are source scripts, not archive executables. After the version tag exists,
download the matching source archive without building Rust:

```sh
curl -fL https://github.com/darkquasar/kaban/archive/refs/tags/v3.0.0.tar.gz \
  -o kaban-v3.0.0-source.tar.gz
tar -xzf kaban-v3.0.0-source.tar.gz
```

Follow that source's `hooks/README.md` and host instructions: copy Claude's
`hooks/claude-code/` and merge its hooks object; copy Codex shell payloads to
`${CODEX_HOME:-$HOME/.codex}/hooks/kaban/` and approve them normally; copy pi's
`hooks/pi/kaban-hooks.ts` to `~/.pi/agent/extensions/` or load it with `pi -e`.
Preserve existing host configuration and use the same PATH/data/workspace settings.

## Unsigned macOS approval and updates

Under OD-1, the first stable may be unsigned or linker ad-hoc signed. Ad-hoc
signatures provide no Developer ID or verified publisher identity. These packages
are **not notarized**. Existing ARM64 linker signatures are inspected and preserved;
there is no signing-account, certificate or notarization requirement.

A browser download can acquire quarantine and require normal macOS approval.
After verifying the archive and deliberately trusting this download, use the normal
macOS approval prompt/settings if available. If a bare CLI remains blocked, inspect
and remove quarantine only from its two exact verified files, then install them:

```sh
xattr -p com.apple.quarantine "$package/kaban"
xattr -p com.apple.quarantine "$package/kaban-index"
xattr -d com.apple.quarantine "$package/kaban"
xattr -d com.apple.quarantine "$package/kaban-index"
```

An absent attribute needs no removal. Never disable Gatekeeper/SIP or use recursive
quarantine removal. Automatic browser-quarantined launch is not promised. CLI
artifact download/install evidence and browser-quarantined behavior are distinct.

For updates, stop active MCP/CLI users and retain protected backups of databases
and their matching keys/config/policy, including any database outside KABAN_HOME.
Verify the replacement archive and replace **both binaries together**. Preserve data
and trust directories. Opening a newer binary can migrate a database; replacing
executables with older ones does not authorize a schema downgrade. Restore a
compatible protected backup if a rollback requires it.

## Build and publication authorization

```text
+----------------+  one build per target  +--------------------+
| Selected PR    | =====================> | Four native packages|
+----------------+                        +--------------------+

+----------------+  exact main CI + tag  +--------------------+  all four pass  +------------+
| Owner tag push | ====================> | Authorize + package| ==============> | Publisher  |
+----------------+                       +--------------------+                 +------------+
```

Release-related PR paths select independent GNU, musl and macOS jobs; helper-test-only
changes retain cheap helper validation. Each job builds the full profile once,
stages hashes/modes, packages/extracts it and runs CLI/MCP plus Markdown/PDF smoke on
extracted shipped bytes. It checks exact executables/notices, static musl, runtime
requirements, signatures, checksums and strict non-musl Kaban **<40 MiB**; existing
musl-index limits remain. It does not replay full debug suites. Compatible dependency
caches accelerate compilation; no verdict or executable-tool cache replaces checks.

Future publication requires separate owner authorization: the repository owner's
matching version tag must name the exact checked main commit with successful main
push CI. Only the one publisher receives contents-write permission; it waits for
all four package targets, verifies checksums/completeness and rechecks authorization
immediately before publication. No local asset upload, cross-run promotion, partial
stable success or renamed historical alpha asset is part of this path.

Licence bundles must match the exact target, lock, manifests and full features;
see [THIRD_PARTY.md](THIRD_PARTY.md). Producers, OCR, HTTP/REST, automatic indexing,
embeddings and two-store routing are not supplied by these downloads.
