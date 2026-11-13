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
| `session-start` | nothing from the envelope but `session_id` | prunes `session_edits` older than 7 days (bounded) | the briefing (at most 2 of the workspace's pending Failures) as `additionalContext`; `{}` when none is pending |
| `pre-tool-use` | the edited path(s) | a `session_edits` row per path (the attempted edit) | `{}` |
| `post-tool-use-failure` | `tool_input.command`, the error field | a `Failure { pending_fix: true, command, error_text, stack_head }` with the session id | `{}` |
| `post-tool-use` | `tool_input.command`, the response | a success of a command whose keyed fingerprint matches a pending Failure of the session appends `props.evidence_runs[] = {ts, files_edited_since}` (the paths of the session's `edit` rows since the failure; evidence, not a conclusion); on Codex and pi a reported failure is captured like `post-tool-use-failure` | `{}` |
| `subagent-stop` | `last_assistant_message` (the field observed live, M4-31; `summary` accepted as a fallback) | an `Episode` | `{}` |
| `stop` | `session_id`, Claude's `stop_hook_active` | one `prompted` marker per session | with pending Failures in the session and no marker yet: the one-time prompt, `{"decision":"block","reason":"kaban: how was \"<command>\" fixed? Failure <id>. Reply … `kaban hook fix` … or link FIXED_BY … or say 'unresolved'."}` on every host (live capture: Codex delivers `decision: block` to the model and not `systemMessage`, M4-16 switch applied); otherwise `{}`. Exit 0 either way: the host reads the decision from the JSON. |
| `fix` | `{"failure_id","fix"}` (the agent's reply, from stdin) | an `Improvement` with the fix text linked `FIXED_BY` from the Failure, then `props.fix` and `pending_fix: false` on the Failure | `{"status":"fixed","failure_id","improvement_id","pending_fix":false}`; an unknown id or a refused text prints `{}` with a stderr line |

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
