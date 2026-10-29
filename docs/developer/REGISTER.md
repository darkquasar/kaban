# Registering kaban with your agent

Two things to register per host: the **MCP server** (the `remember`/`recall`/`why`/… tools) and
the **hooks** (briefing, pre-edit context, failure capture, the "how was it fixed" prompt). All
transports are stdio. Replace `/path/to/kaban` with the native binary. The
server never opens network connections. Hooks are covered in the second half of this page.
There is no working HTTP/REST transport: `serve --http` explicitly refuses the request.
The local M7 handoff does not refresh or distribute a Docker image; see [RELEASE.md](RELEASE.md).

## Part 1: the MCP server

The `code_*` tools read a code-graph snapshot from `$KABAN_HOME/codegraph/<repo-id>/` (default
`~/.kaban/codegraph/`) that an explicit `kaban-index index --repo <path>` wrote earlier; `serve`
never starts an indexer and never creates the snapshot directory or its key. The server finds
the repository through its own working directory, so register it with the project directory as
the working directory (the project scope below) or start it from inside the checkout; without a
snapshot the tools answer `no_snapshot` (see `docs/developer/code-graph.md`, "Reading the snapshot from
`kaban serve`").

## Claude Code
```bash
claude mcp add kaban --scope project -- /path/to/kaban \
  --db "$HOME/.kaban/kaban.db" --policy "$HOME/.kaban/policy.yaml" --audit "$HOME/.kaban/audit.jsonl"
```
`.mcp.json` equivalent:
```json
{ "mcpServers": { "kaban": { "type": "stdio", "command": "/path/to/kaban",
  "args": ["--db", "${HOME}/.kaban/kaban.db", "--policy", "${HOME}/.kaban/policy.yaml"] } } }
```

## Codex
```bash
codex mcp add kaban -- /path/to/kaban --db ~/.kaban/kaban.db --policy ~/.kaban/policy.yaml
```
`~/.codex/config.toml`:
```toml
[mcp_servers.kaban]
command = "/path/to/kaban"
args = ["--db", "/home/you/.kaban/kaban.db", "--policy", "/home/you/.kaban/policy.yaml"]
```

## pi
`~/.pi/agent/mcp.json` (user) or `.pi/mcp.json` (project):
```json
{ "mcpServers": { "kaban": { "command": "/path/to/kaban",
  "args": ["--db", "/home/you/.kaban/kaban.db", "--policy", "/home/you/.kaban/policy.yaml"] } } }
```

## Cursor
`.cursor/mcp.json` uses the same `mcpServers` shape as Claude Code.

## Docker (historical recipe, not an M7 artifact)

The following describes the existing Dockerfile convention only. It is not a
claim that a current image has been built, qualified or made available. Prefer
the native stdio registration above for the local handoff.
```json
{ "mcpServers": { "kaban": { "command": "docker",
  "args": ["run", "--rm", "-i", "-v", "kaban:/data", "kaban"] } } }
```
`-i` is required (stdio); do not allocate a TTY. Mount your own policy with
`-v /path/policy.yaml:/etc/kaban/policy.yaml:ro`. The image sets `KABAN_DB=/data/kaban.db`,
`KABAN_POLICY=/etc/kaban/policy.yaml` and `KABAN_AUDIT_EXPORT=/data/audit.jsonl`.
The image holds the `kaban` server only: no `kaban-index` and no SCIP indexer (OWN-68 SC-13,
SC-11; the image refresh follows 3.0.0). Index on the host with the native `kaban-index`, and
the container's `code_*` tools answer `no_snapshot` unless the host's `$KABAN_HOME/codegraph/`
and `keys/codegraph.key` are mounted at the container's `KABAN_HOME` and the container runs with
the repository as its working directory.

## Environment variables
`KABAN_DB`, `KABAN_POLICY`, `KABAN_AUDIT_EXPORT` mirror `--db`, `--policy`, `--audit-export` (alias
`--audit`). The spike's
`SB_DB`/`SB_POLICY`/`SB_AUDIT` are not read any more (clap binds one environment variable per
flag; see CHANGELOG 3.0.0-alpha.1). Logs go to stderr only; set `RUST_LOG=info` to see startup
timing. Avoid verbose logging when processing sensitive data; the application policy
is not a substitute for protecting client and host logs.

## Explicit CLI maintenance

`kaban import [--repo <root>]` runs the full record import without writing repository files.
Its absence pass tombstones records whose files git deleted, guarded by their serialized
revision. The old `rebuild` spelling is retired. `kaban reindex text` rebuilds the Tantivy text index
from the database; `kaban doctor --optimize` explicitly merges its segments once. Ordinary writes and ordinary doctor checks never request optimization.
CLI request/result content gates still apply; repository/document paths are not exempt.
`kaban reindex layer <name>` is a DB-only reset, not a completed rebuild: generated links
remain absent until the next explicit ingestion with that layer enabled. No automatic jobs
are scheduled. `forget --layer <name> [--document <path>]` permanently rejects existing
machine claims while keeping their history. Both internal gated verbs (`reindex_layer`,
`forget_layer`) are CLI-only and absent from the MCP catalog; human/other-layer state is kept.

The `reconcile` consumer identity and lease, with versioned cursor `reconcile:1`, are reserved.
No asynchronous reconcile loop is registered or scheduled; reconciliation stays synchronous.

## Part 2: the hooks (core-spec F8, F8b)

```
  +-----------+  stdin JSON   +----------------------+   =>   +----------+
  | host hook | ============> | kaban hook <event>   | -----> | kaban.db |
  | (Claude,  |               | --host <host>        |        | (opened, |
  |  Codex,pi)| <============ | --stdin-json         |        |  never   |
  +-----------+  stdout JSON  +----------------------+        |  created)|
                 exit 0 always                                +----------+
```

### Prerequisites (every host)

- The `kaban` binary on `PATH`, or `KABAN_BIN=/path/to/kaban` in the host's environment.
- The **same** `KABAN_DB`, `KABAN_HOME`, `KABAN_POLICY` and (if you set one) `KABAN_WORKSPACE`
  as the MCP server entry above, so hooks and tools read and write one database with one
  policy. The recipes default `KABAN_DB` to `<project dir>/kaban.db` (Claude Code:
  `$CLAUDE_PROJECT_DIR`; Codex and pi: the session's working directory). The workspace is bound
  like the server's: `KABAN_WORKSPACE`, else the enclosing git repository's root directory name
  (the hook process cwd; the event's own `cwd` only resolves relative paths).
- **The database must already exist.** Hooks never create, migrate or seed one (M4-6): run
  `kaban serve` once through your MCP client, or `kaban doctor --db …`, before the first session.
  Without a usable database every hook prints `{}` and one fixed line on stderr (the host's
  debug log) and exits 0: the session simply runs without kaban.

### The contract every recipe honours

| Rule | Detail |
|---|---|
| Exit 0 always | Decisions travel only in the stdout JSON. Exit 2, which hosts read as "block", is never produced; the wrappers end in `\|\| exit 0`. A usage error (`kaban hook bogus`) exits 4 before any hook logic, so a mistyped recipe fails loudly in the host log, never silently. |
| 2 s bound | Every host entry sets `timeout: 2`; the wrappers run `timeout 2 kaban hook …` where GNU `timeout` exists; kaban's own 1.8 s watchdog prints `{}` and exits 0 if a handler stalls. **macOS note (M4-9):** GNU `timeout` is absent, the wrappers run kaban bare, and the watchdog is the bound. Observed in the live sessions: at most 21 ms per hook on an idle database. |
| One JSON document | `{}` when there is nothing to inject or record; `hookSpecificOutput.hookEventName` echoes the host's event name. |
| Lock wait | At most 1 s for a database lock, then `{}`. |

### What is read (the allowlist, M4-7) and what is never read

Read: `session_id`, `hook_event_name`, `cwd`, the tool name, the edited path (Claude
`tool_input.file_path`, pi `input.path`, Codex the `*** Add File:` / `*** Update File:` headers of
the `apply_patch` text), the shell command (`tool_input.command`, pi `input.command`), the host's
error field (Claude `error`, Codex the string `tool_response`, pi `result.content[].text` with
`isError`), Claude's `stop_hook_active`, and `last_assistant_message` on `SubagentStop` only.

Never read, stored or scanned: file contents (`content`, `old_string`, `new_string`, pi
`edits[]`, the patch body), `transcript_path` and transcripts, the user prompt, `Stop`'s
`last_assistant_message`, anything on `SessionEnd`. Unknown fields are ignored. Every selected
string passes kaban's denylist before anything is stored; a credential anywhere in it refuses
the whole write (exit 0, fixed stderr text). kaban's own MCP tools (`mcp__kaban__*`) are never
captured as failures.

### Claude Code

Files: `hooks/claude-code/` (`settings.hooks.json`, `_run.sh`, one script per event).

1. Copy `hooks/claude-code/` into the project as `<project>/hooks/claude-code/` (the entries
   call `bash "$CLAUDE_PROJECT_DIR"/hooks/claude-code/<script>.sh`), or edit the paths.
2. Merge the `hooks` object of `settings.hooks.json` into `.claude/settings.json` (project) or
   `~/.claude/settings.json` (user).
3. Export `KABAN_DB`, `KABAN_HOME`, `KABAN_POLICY` in Claude Code's environment.

Events: `SessionStart` (`startup|resume|compact`) briefing; `PreToolUse` (`Edit|Write|MultiEdit`)
governing records for the file; `PostToolUseFailure` (`Bash`) Failure; `PostToolUse` (`Bash`)
success evidence; `SubagentStop` Episode; `Stop` the one-time prompt as `decision: block`.
`SessionEnd` is not used. Verified live (round 2, two sessions): all of the above except
`SubagentStop` and `Edit`/`MultiEdit` (the model chose `Write`).

### Codex CLI

Files: `hooks/codex/` (`hooks.json`, `config.hooks.toml`, `_run.sh`, one script per event).

1. `mkdir -p ~/.codex/hooks/kaban && cp hooks/codex/*.sh ~/.codex/hooks/kaban/`.
2. Register with **one** of: copy `hooks/codex/hooks.json` to `~/.codex/hooks.json`, or append
   `hooks/codex/config.hooks.toml` to `~/.codex/config.toml` (both verified live with Codex
   0.149.1). `<repo>/.codex/hooks.json` exists as a project scope but was not exercised.
3. Export `KABAN_DB`, `KABAN_HOME`, `KABAN_POLICY` in Codex's environment.
4. Trust the hooks in the TUI (`/hooks`): user-level hooks run only after this review, which is
   pinned to a hash of the configuration. `--dangerously-bypass-hook-trust` is a test flag for
   throwaway containers, never for a working checkout.

Events: `SessionStart` briefing; `PreToolUse` (`^apply_patch$|^Edit$|^Write$`) governing records
for the files in the patch headers; `PostToolUse` (`^Bash$`) success evidence; `SubagentStop`
Episode (unverified); `Stop` prompt as `decision: block`. See `hooks/codex/README.md` for the
Codex-specific details (the empty `Bash` response, trust, `config.toml`).

### pi

File: `hooks/pi/kaban-hooks.ts`.

1. `pi -e /path/to/hooks/pi/kaban-hooks.ts …` for one session, or copy the file to
   `~/.pi/agent/extensions/` for every session.
2. Export `KABAN_DB`, `KABAN_HOME`, `KABAN_POLICY` (and `KABAN_BIN` if `kaban` is not on `PATH`).

Events: `session_start` briefing (`sendMessage` next turn); `tool_call` for `edit`/`write`
governing records (steer message); `tool_execution_end` for `bash` Failure (`isError`) or success
evidence; `agent_before_settle` the one-time prompt as a single continuation. Verified live
(round 2, two sessions). In `pi --print` the continuation was honoured although pi reported
`canContinue: false`; see `hooks/pi/README.md`.

### git (`hooks/git/pre-commit`, `hooks/git/post-commit`; core-spec F9b, F13)

The two git hooks keep the committed serialization in step with the database: `pre-commit`
runs `kaban sync --stage` so `.kaban/<ontology>/<dir>/<id>.md` and `.kaban/<ontology>/INDEX.md`
(the shipped ontology: `.kaban/adr/decisions/<id>.md`, `.kaban/adr/INDEX.md`) land in the commit
being made; `post-commit` runs `kaban hook post-commit`, which stamps the commit's sha on the
records no commit stamped yet (re-serialized by the next sync, one commit later).

1. Inside the repository: `git config core.hooksPath hooks/git`, or copy both files into
   `.git/hooks/` (executable).
2. Make sure git's environment carries the same `KABAN_DB`, `KABAN_HOME`, `KABAN_POLICY` and (if
   set) `KABAN_WORKSPACE` as the MCP server entry, and `kaban` on `PATH` or `KABAN_BIN`. git
   runs hooks from the repository root with the caller's environment; it reads no `.env`.
3. Keep the database out of the commit (`kaban.db*` in `.gitignore`).

Contract: both hooks exit 0 on every path (a missing binary or database, a lock, a timeout or
any error never aborts a commit), print nothing, and stay inside `.kaban/`. `kaban hook
post-commit` has no envelope and never reads stdin. Partial commits (`git commit <paths>`)
get their serialization in the next commit (`hooks/README.md`, "git hooks", M5-29). Offline
proof: `crates/kaban/tests/hook_post_commit.rs` (a fixture repository with `core.hooksPath`).
The Stop hooks of the three agent hosts also flush the files with a detached `kaban sync`
(`hooks/README.md`); the git hooks are what commits them.

The instruction block an agent should see (`docs/developer/AGENTS-snippet.md`) tells it to read
`.kaban/adr/INDEX.md` at session start and call `why` before editing governed paths; the managed
block in `AGENTS.md`/`CLAUDE.md` is written by `kaban export --write` only.

### Manual fallbacks (core-spec §11: missing host events are documented manual flows)

| Gap | Fallback |
|---|---|
| **Codex shell failures.** Codex has no `PostToolUseFailure`; its `PostToolUse` string response for a failing command is empty (no exit code, observed). The hook records a Failure only when an `Exit code: <n>` marker is present (`apply_patch` style) and otherwise treats the event as success evidence. | Record it yourself: `kaban remember Failure "<command> failed: <last error line>" --session-id <id>` (or the MCP `remember` tool with `kind: "Failure"`). Pair the fix later with `kaban hook fix --stdin-json` and `{"failure_id":"…","fix":"…"}`, or an MCP `link FIXED_BY` to an Improvement. |
| **Codex `SubagentStop`.** Never fired in the captured sessions; the handler reads `last_assistant_message` by analogy with `Stop`. | `kaban remember Episode "<summary>"` after a subagent, or rely on `Stop`. |
| **Claude Code subagent failures.** Inside a subagent a failing `Bash` surfaced as `PostToolUse` (with `tool_response.{stdout,stderr}`) rather than `PostToolUseFailure`; the handler treats it as evidence, never as a failure. | The subagent (or you) records the Failure with `remember`; the top-level `Stop` prompt pairs the fix. |
| **No pending Failure at `Stop`.** The prompt only fires when the session recorded one. | `kaban remember Failure … --session-id <id>` first, or link the fix directly. |

### Uninstall

Remove the hook entries (Claude: the `hooks` object in `settings.json`; Codex: `~/.codex/hooks.json`
or the `[[hooks.*]]` tables in `config.toml`, then re-review `/hooks`; pi: the `-e` flag or the
file under `~/.pi/agent/extensions/`; git: `git config --unset core.hooksPath` or delete the two
files from `.git/hooks/`) and delete the copied scripts. The database, the audit
keys under `KABAN_HOME` and the MCP server entry are untouched; nothing else was written.
