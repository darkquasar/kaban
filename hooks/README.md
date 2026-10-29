# kaban host hooks

Recipes that connect a coding-agent host to a running kaban database (core-spec F8, F8b;
core-plan C4.3 to C4.5). Every recipe calls `kaban hook <event> --host <host> --stdin-json`
with the host's event envelope on stdin and reads back one JSON document. Hooks are best
effort and never block the host.

```
  +-----------+  stdin JSON   +----------------------+   =>   +----------+
  | host hook | ============> | kaban hook <event>   | -----> | kaban.db |
  | (Claude,  |               | --host <host>        |        | (opened, |
  |  Codex,pi)| <============ | --stdin-json         |        |  never   |
  +-----------+  stdout JSON  +----------------------+        |  created)|
                 exit 0 always                                +----------+
```

## Contract of `kaban hook` (every host)

| Rule | Detail |
|---|---|
| Exit status | Always 0. Decisions are expressed only through the stdout JSON; exit 2 (which hosts read as "block") is never produced. A usage error (`kaban hook bogus`) is the only exception and exits 4 before any hook logic runs. |
| Output | Exactly one JSON document. `{}` when the hook has nothing to inject or record. `hookSpecificOutput.hookEventName` echoes the host's own event name from the envelope. |
| Deadline | kaban answers within 1.8 s on its own: a watchdog prints `{}` and exits 0 if a handler is slower. The host `timeout: 2` and the `timeout 2` wrapper are second and third lines of defence; on macOS, where GNU `timeout` is absent, the wrapper runs kaban bare and the watchdog still holds. |
| Database | Hooks never create, migrate or seed a database. Pass the same `KABAN_DB` (and `KABAN_HOME`) the kaban MCP server uses; the recipes default `KABAN_DB` to `<project dir>/kaban.db`. Without a usable database the hook prints `{}` and one line on stderr (the host's debug log). Hooks wait at most 1 s for a database lock. |
| Workspace | Bound like the MCP server: `--workspace`/`KABAN_WORKSPACE`, else the enclosing git repository's root directory name. The envelope `cwd` only resolves relative paths. |
| Intake | Only an allowlist of envelope fields is read: `session_id`, `hook_event_name`, `cwd`, `tool_name`, the edited path (`tool_input.file_path`, pi `input.path`, Codex `apply_patch` headers), the shell command (`tool_input.command`), the host's error field or exit code, and a subagent `summary`. File contents (`old_string`, `new_string`, `content`), transcripts and `transcript_path` are never read, stored or scanned. Unknown fields are ignored. |
| Secrets | The selected strings go through kaban's denylist before anything is stored; a credential anywhere in them refuses the whole write (stderr says `refused by the denylist policy; nothing stored`, exit 0). Every output string is gated on the way out. |
| Logging | Nothing variable is printed on stderr: fixed refusal texts only. |

Events and what they do:

| `kaban hook <event>` | Reads | Writes | Prints |
|---|---|---|---|
| `session-start` | nothing from the envelope but `session_id` | prunes `session_edits` older than 7 days (bounded) | the briefing (at most 5 items: accepted Decisions and Conventions by importance, plus the workspace's pending Failures) as `additionalContext` |
| `pre-tool-use` | the edited path(s) | a `session_edits` row per path (the attempted edit) | the records governing each path (`why`, 3 per path) as `additionalContext`; `{}` when none govern |
| `post-tool-use-failure` | `tool_input.command`, the error field | a `Failure { pending_fix: true, command, error_text, stack_head }` with the session id | `{}` |
| `post-tool-use` | `tool_input.command`, the response | a success of a command whose keyed fingerprint matches a pending Failure of the session appends `props.evidence_runs[] = {ts, files_edited_since}` (the paths of the session's `edit` rows since the failure; evidence, not a conclusion); on Codex and pi a reported failure is captured like `post-tool-use-failure` | `{}` |
| `subagent-stop` | `last_assistant_message` (the field observed live, M4-31; `summary` accepted as a fallback) | an `Episode` | `{}` |
| `stop` | `session_id`, Claude's `stop_hook_active` | one `prompted` marker per session | with pending Failures in the session and no marker yet: the one-time prompt, `{"decision":"block","reason":"kaban: how was \"<command>\" fixed? Failure <id>. Reply … `kaban hook fix` … or link FIXED_BY … or say 'unresolved'."}` on every host (live capture: Codex delivers `decision: block` to the model and not `systemMessage`, M4-16 switch applied); otherwise `{}`. Exit 0 either way: the host reads the decision from the JSON. |
| `fix` | `{"failure_id","fix"}` (the agent's reply, from stdin) | an `Improvement` with the fix text linked `FIXED_BY` from the Failure, then `props.fix` and `pending_fix: false` on the Failure | `{"status":"fixed","failure_id","improvement_id","pending_fix":false}`; an unknown id or a refused text prints `{}` with a stderr line |
| `post-commit` | nothing: this event has no envelope and never reads stdin (git sends no JSON and may leave stdin on a terminal, M5-14); `--stdin-json` is accepted and ignored, `--host` is irrelevant | `commit_sha = HEAD` on every node of the bound workspace that no commit stamped yet (`repo` unset or this repository's), with `rev + 1`, so the next `sync` re-serializes the record with its provenance; each node is stamped once (M5-15, OWN-23: no `changes(paths)` rows, no currency cache) | `{"stamped": n}`; outside a repository or without `HEAD`, `{}` with a fixed stderr line |

**Stop and SubagentStop also flush the files (M5-20).** After the hook call, the Claude Code
and Codex `stop.sh`/`subagent-stop.sh` wrappers start `kaban sync --json --deadline 60` **fully
detached** (`setsid`, every descriptor on `/dev/null`, backgrounded, never `--stage`), and the
pi extension does the same at `session_shutdown` (`spawn` with `detached: true`, `stdio:
"ignore"`, not awaited). The hook's stdout therefore stays exactly one JSON document and the
host never waits for the sync; the `.kaban/<ontology>/<dir>/<id>.md` records and the digests
`.kaban/<ontology>/INDEX.md` (the shipped ontology: `.kaban/adr/decisions/`, `.kaban/adr/INDEX.md`)
appear in the repository a moment later and are committed by the next commit (or by the
`pre-commit` hook below). `KABAN_HOOK_SYNC=0` turns the flush off.

**The detached sync has a deadline on every host (R5-9).** Two layers bound it, so a
pathological import cannot leave an orphaned sync behind and repeated Stop events cannot pile
processes up:

1. `kaban sync --deadline <s>` (env `KABAN_SYNC_DEADLINE`, the recipes pass 60) is a hard
   wall-clock bound inside the process: at the deadline it prints
   `kaban sync: the deadline passed; the run stopped, the next sync continues` on stderr and
   exits 4. Partial state is safe (files land by rename, the store is transactional, the
   repository lock is released with the process) and the next sync picks up the dirty set.
   This layer holds wherever the binary runs, including macOS and a pi that has already exited.
2. The shell kills the sync's whole process group a few seconds later, so a git descendant dies
   with it: on Linux `setsid timeout -k 5 <deadline + 5>`; on macOS (no `setsid`, no GNU
   `timeout`) a job-control subshell (`set -m`) gives the sync its own process group and a
   sleeper kills that group after the grace period (the supervisor cancels the sleeper when
   the sync ends first). Only fixed strings and a digits-only number reach argv.

The hook path still exits 0 and returns at once whatever the sync does; the sync's own exit
status is never the hook's. Offline proof: `scripts/hooks-live/test-stop-sync.sh` (a fake
`kaban` that forks a child and sleeps 30 s on `sync`: the wrappers exit 0 in about 0.1 s with
one JSON document, the fake sees `sync --json --deadline 60`, `/dev/null` on fds 0 to 2 and its
own session; with `KABAN_SYNC_DEADLINE=1` the fake and its child are gone within the grace
period on the Linux path, on the macOS fallback (exercised by hiding `setsid` and `timeout`,
where the fake runs in its own process group) and after three Stop events in a row), plus
`crates/kaban/tests/cli_sync.rs` for the in-process deadline (a sync stalled behind the
repository lock exits 4 with the fixed line in about 1 s, nothing written).

A Failure is *pending* while it is active, has no outgoing `FIXED_BY` edge and its
`pending_fix` is not `false`; a `FIXED_BY` edge made through the MCP `link` tool therefore
resolves it as well as `hook fix` does. The command fingerprint is
`HMAC(audit key, "kaban-cmd/v1" + NUL + normalized command)` (whitespace runs collapsed), never a
bare hash, so a stored fingerprint cannot confirm a guessed command.

Failure capture keeps `command` (at most 2 KiB), `error_text` (the last 40 lines, at most
4 KiB) and `stack_head` (the first 20 lines, at most 2 KiB), truncated at line boundaries
*after* the full strings were scanned, so truncation can never split a secret below a rule's
threshold. kaban's own MCP tools (`mcp__kaban__*`) are never captured.

## Claude Code

Files: `claude-code/settings.hooks.json` (the `hooks` block to merge into `.claude/settings.json`
of the project, or `~/.claude/settings.json` for every project), `claude-code/_run.sh` (the shared
wrapper) and one script per event.

1. Copy `hooks/claude-code/` into the project (the recipe refers to it as
   `$CLAUDE_PROJECT_DIR/hooks/claude-code/`), or point the `command` fields at another location.
2. Merge the `hooks` object of `settings.hooks.json` into `.claude/settings.json`.
3. Make sure Claude Code's environment carries the same `KABAN_DB` and `KABAN_HOME` as the
   kaban MCP server entry (the wrapper defaults `KABAN_DB` to `$CLAUDE_PROJECT_DIR/kaban.db`),
   and that `kaban` is on `PATH` (or set `KABAN_BIN`).

Matchers: `SessionStart` `startup|resume|compact`, `PreToolUse` `Edit|Write|MultiEdit`,
`PostToolUseFailure` `Bash`, `PostToolUse` `Bash`, `SubagentStop` and `Stop` unconditional.
`SessionEnd` is not used (its shared 1.5 s budget is below kaban's bound); the "how was it
fixed" prompt runs on `Stop`. Every entry sets `"timeout": 2`.

Offline verification shipped with this recipe: `jq` parses `settings.hooks.json`, `bash -n`
passes on every script, and the handler tests under `crates/kaban/tests/hook_*.rs` run the
binary on the invented envelopes in `crates/kaban/tests/fixtures/hooks/claude-code/`. The live
verification against a Claude Code session is the Docker lane's (owner decision M4-22).

## git hooks (`hooks/git/`)

```
  git commit
    | pre-commit  ==> kaban sync --stage --json   writes .kaban/<ontology>/<dir>/<id>.md,
    |                                             .kaban/<ontology>/INDEX.md, .kaban/.gitignore
    |                                             and stages them (this commit); the first run
    |                                             moves a pre-engine .kaban/<dir>/ tree (git mv)
    | (commit object written)
    | post-commit ==> kaban hook post-commit      commit_sha = HEAD on the unstamped records
    v                                             (re-serialized by the next sync, one commit later)
```

Files: `git/pre-commit`, `git/post-commit` (plain bash, executable, no arguments).

**Install** with one of:

- `git config core.hooksPath hooks/git` inside the repository (every hook of the directory;
  the two files are the only ones), or
- `cp hooks/git/pre-commit hooks/git/post-commit .git/hooks/` (keep them executable). If
  `.git/hooks/pre-commit` already exists, call this one from it (`bash hooks/git/pre-commit`)
  since git runs one script per hook name.

**Environment.** Both hooks run `kaban` from `PATH`, or `KABAN_BIN=/path/to/kaban`; the
database is `KABAN_DB` (default `kaban.db` in the repository root, where git runs hooks), the
audit keys `KABAN_HOME` (default `~/.kaban`), the workspace `KABAN_WORKSPACE` (default: the root
directory's name). Set them to the values the MCP server entry uses, for example through
`direnv` or the shell profile: git does not read `.env` files. Keep `kaban.db*` out of the
commit (`sync` writes `.kaban/.gitignore` for a database inside `.kaban/`; a root-level
`kaban.db` needs a root `.gitignore` line).

**Exit-0 contract.** Both hooks end with `exit 0` on every path: a missing binary (checked with
`command -v` first), a missing database (hooks never create one), a held lock, a slow sync
(`timeout 10` where GNU `timeout` exists) or any error never aborts or delays a commit beyond
the bound. stdin, stdout and stderr are detached (`</dev/null >/dev/null 2>&1`), so `git commit`
prints only git's own output. Proof: `crates/kaban/tests/hook_post_commit.rs` runs both hooks
through `core.hooksPath` over a temporary repository (the commit contains `.kaban/adr/INDEX.md`
with the accepted Decision, the session's note carries the commit's sha, a second commit
leaves the digest byte-identical, a commit with a missing `KABAN_BIN` succeeds).

**Partial commits (accepted residual, M5-29).** For `git commit <paths>` git builds the commit
from a temporary index; `sync --stage` stages into the real index (kaban's git runner drops
`GIT_INDEX_FILE` by design, R-C3.2-4), so the serialization written during a partial commit
lands in the **next** commit. Nothing is lost: the files are in the working tree and staged.
The same holds for `git commit --only` and `--include`. A plain `git commit` or
`git commit -a` includes them in the triggering commit.

**What is not covered.** `git commit --no-verify` skips both hooks (run `kaban sync --stage`
yourself). A merge commit made by `git merge` runs `pre-merge-commit`, not `pre-commit`
(`git merge --no-commit` followed by `git commit` runs it). The stamp is idempotent: however
many times `post-commit` fires, a record is stamped once. Bare repositories have no working
tree and no `.kaban/`.

## Codex CLI and pi: the handler variants

`--host codex` and `--host pi` select the host's envelope fields; the recipes (`hooks/codex/`,
`hooks/pi/`) and the registration guide `docs/developer/REGISTER.md` ship with task C4.5 (the live
verification is the Docker lane's, M4-22). What the variants read, offline-verified against
the fixtures in `crates/kaban/tests/fixtures/hooks/{codex,pi}/` (`tests/hook_hosts.rs`), which
follow the field tables captured live by the Docker lane (`hooks/HOST-EVENTS.md`):

| Host | Event (`hook_event_name`) | Fields read | Notes |
|---|---|---|---|
| codex | `SessionStart` | `session_id` | briefing as `hookSpecificOutput.additionalContext` |
| codex | `PreToolUse` (`apply_patch`, `Edit`, `Write`) | the `*** Update File:` / `*** Add File:` headers of the patch text in `tool_input.command` (observed live; `input`/`patch` accepted as fallbacks); `tool_input.file_path` | the patch body is never read; `additionalContext` is consumed by Codex (observed, M4-30) |
| codex | `PostToolUse` (`Bash`) | `tool_input.command`; `tool_response` is a **string** (observed live, no exit-code field): an `Exit code: <n>` marker in it is the failure signal, the whole string is the error text | marker `n != 0` = Failure; `0` = success evidence; **no marker** (the observed one-line `Bash` shape) = success evidence only, never a guessed failure (spec §11 fallback; the `Bash` response format stays unverified live). The object spellings `exit_code`/`exitCode` the plan assumed are still accepted. |
| codex | `Stop` | `session_id`, `stop_hook_active` | prompt as `{"decision":"block","reason": …}` (live: delivered to the model; `systemMessage` was not, M4-16 switch applied) |
| pi | `session_start` | `session_id` | the extension injects `additionalContext` with `pi.sendMessage(..., {deliverAs: "nextTurn"})` |
| pi | `tool_call` (`edit`, `write`) | `input.path` (`tool_execution_start`'s `args.path` accepted too) | `edits[]`/`content` never read |
| pi | `tool_execution_end` (`bash`) | `toolName`, `isError`, `result.content[0].text` (or a string, or `error`/`text`/`output`), and `input.command` which the extension attaches from the matching `tool_call` by `toolCallId` (the end event itself carries no command, observed live) | `isError: true` = Failure, `false` = success evidence; `kaban hook post-tool-use --host pi` serves both |
| pi | `agent_before_settle` | `session_id` | prompt as `{"decision":"block","reason":…}` for the extension to turn into one bounded continuation (`continue: true` with a custom message, observed live) |

Every variant keeps the common rules above: an envelope whose `hook_event_name` is not the
event the handler serves prints `{}` with a stderr line; `mcp__kaban__*` tools are never
captured; exit 0 always.
