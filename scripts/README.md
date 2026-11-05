# scripts

Reusable shell and Python tooling around the Rust workspace. The gate and fixture
scripts resolve the repository root themselves; the rest take explicit path arguments, so run
them from the repository root (for example `bash scripts/ci/ci.sh`).

| Folder | What it holds |
|---|---|
| `ci/` | `ci.sh` dispatches quality, helpers, functional, producer, GNU and musl groups after scoped prerequisites; no argument runs comprehensive checks (producer separately). `deps_allowlist.sh` and `clean-room.sh` are gates it or a release calls. |
| `mcp/` | `mcp_smoke.py` drives the MCP stdio server end to end; `mcp_catalog.py` checks the tool catalog. |
| `dev/` | Local helpers: `run-kaban.sh` (runs `bin/kaban` with a repo-local home), `cargo-slot.sh` (bounds parallel Cargo jobs), `lane-target.sh`/`lane-gate.sh` (isolated target dirs for parallel work), `regen_fixtures.sh`. |
| `hooks-live/` | Containerized harness that captures real host-hook events from Claude Code, Codex and pi. |

See [CI commands and setup](../docs/developer/CI.md) for focused Cargo checks,
`mise run core`, `mise run ci`, explicit group commands and scope limits. `bash ci/ci.sh --help`
(from this directory) needs no tool setup. Each group has a matching mise task. `core` composes quality + helpers + functional;
`ci` includes GNU/musl artifacts and helpers. Run `bash ci/ci.sh producer` explicitly
when applicable; it uses the producer workspace/toolchain and a private target.

Prerequisites: bash, pinned Python (3.11+ with `sqlite3`/FTS5), Node, jq, GNU
coreutils/sed, and for comprehensive gates native Linux, a C compiler, musl linker/
target and strace. Cargo/Rust versions come from `rust-toolchain.toml`; nextest,
deny, Python and Node come from `mise.toml`. Missing required nextest/deny or wrong pinned
versions exit 3 before compilation; there is no fallback test suite. The small
`ci/preflight.py` checks effective tools without replacing product trust checks.

Hosted CI installs Go 1.26.0 and scip-go 0.2.7 with SHA-256 checks using
`ci/install-go.sh` and `ci/install-scip-go.sh`. Both accept an absolute destination
outside the checkout; Go requires a new destination directory. CI installs Go at
`/usr/lib/kaban-ci-go-1.26.0` and exposes it through PATH and `/usr/bin/go`.
The namespace view already exposes `/usr/lib` read-only, letting version probes
read Go's root; some fixtures also use a minimal system PATH. Kaban's executable
trust checks remain in force. Use the same Go location in an sbx clone, with a
private producer directory under `/tmp`; export Go's `bin` on PATH and the
producer's absolute path as `SCIP_GO`.
CI also makes the image's resolved Node executable root-owned and removes
group/other write permissions before running TypeScript tests.

The CLI's fifty-write timing check allows 13 seconds; its single-write limit and
the MCP timing limits are unchanged. These prerequisites do not enable namespaces:
that remains a separate runner capability and the gate still reports unavailable
mandatory coverage.

Historical benchmark scripts, schemas and reports live in the sibling specs checkout at
`../specs/kaban/archive/benchmarks/`. The Rust `kaban bench` CLI and its test fixtures remain available.

These scripts came from a multi-agent development workflow. Lane and slot helpers in `dev/`
assume that workflow's directory conventions and may need adapting before reuse.
