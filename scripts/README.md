# scripts

Reusable shell and Python tooling around the Rust workspace. The gate and fixture
scripts resolve the repository root themselves; the rest take explicit path arguments, so run
them from the repository root (for example `bash scripts/ci/ci.sh`).

| Folder | What it holds |
|---|---|
| `ci/` | `ci.sh` runs every gate in order (fmt, clippy, nextest, deny, release build, MCP smoke, size checks). `deps_allowlist.sh` and `clean-room.sh` are gates it or a release calls. |
| `mcp/` | `mcp_smoke.py` drives the MCP stdio server end to end; `mcp_catalog.py` checks the tool catalog. |
| `dev/` | Local helpers: `run-kaban.sh` (runs `bin/kaban` with a repo-local home), `cargo-slot.sh` (bounds parallel Cargo jobs), `lane-target.sh`/`lane-gate.sh` (isolated target dirs for parallel work), `regen_fixtures.sh`. |
| `hooks-live/` | Containerized harness that captures real host-hook events from Claude Code, Codex and pi. |

Prerequisites: bash, Python 3 with `sqlite3`/FTS5, jq, coreutils/util-linux (`flock`), and for the
Cargo gates `cargo-nextest` and `cargo-deny`.

Historical benchmark scripts, schemas and reports live in the peer specs repository at
`../specs/kaban/archive/benchmarks/`. The Rust `kaban bench` CLI and its test fixtures remain available.

These scripts came from a multi-agent development workflow. Lane and slot helpers in `dev/`
assume that workflow's directory conventions and may need adapting before reuse.
