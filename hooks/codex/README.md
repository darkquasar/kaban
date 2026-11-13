# kaban hooks for Codex CLI

Recipe for Codex CLI 0.149.1 (core-spec F8, F8b; core-plan C4.5 as amended by the live captures
in `hooks/HOST-EVENTS.md` §2). Same handler contract as every host (`hooks/README.md`): each
entry runs `kaban hook <event> --host codex --stdin-json` through `_run.sh`, exits 0 always and
answers within 2 s.

```
  Codex CLI  ---stdin JSON--->  _run.sh <event>  ---> kaban hook <event> --host codex
  (hook entry, timeout 2)  <---stdout JSON----  (timeout 2 || exit 0)   (1.8 s watchdog)
```

## Files

| File | Role |
|---|---|
| `hooks.json` | the `hooks.<Event>[].hooks[]` entries (`type: "command"`, `command`, `timeout: 2`), the shape Codex 0.149.1 reads from `$CODEX_HOME/hooks.json` (observed live) |
| `config.hooks.toml` | the same entries as `[[hooks.<Event>]]` tables for `$CODEX_HOME/config.toml` (**verified live**: Codex 0.149.1 fires hooks registered this way, round-2 session s2); use one of the two files, never both |
| `_run.sh` | shared wrapper: `--host codex`, GNU `timeout 2` when present, `|| exit 0`, `KABAN_DB` default `$PWD/kaban.db` |
| `session-start.sh` | `SessionStart` -> briefing as `hookSpecificOutput.additionalContext` (consumed by the model: observed 3/3 in round 1, 2/2 in round 2) |
| `pre-tool-use.sh` | `PreToolUse` matcher `^apply_patch$\|^Edit$\|^Write$` -> records the files named in the patch headers (`*** Add File:` / `*** Update File:` in `tool_input.command`) as the session's attempted edits and injects nothing; kaban never blocks an edit |
| `post-tool-use.sh` | `PostToolUse` matcher `^Bash$` -> success evidence for a pending Failure of the session; see "The Bash failure path" below |
| `subagent-stop.sh` | `SubagentStop` -> an Episode from `last_assistant_message` (**unverified live**: the event never fired in the captured sessions; the field name is by analogy with `Stop`) |
| `stop.sh` | `Stop` -> the one-time "how was it fixed" prompt as `{"decision":"block","reason":…}` (the channel Codex delivers to the model; `systemMessage` does not reach it, observed) |

## Registration

1. Copy the scripts to `$CODEX_HOME/hooks/kaban/` (`~/.codex/hooks/kaban/` by default):
   `mkdir -p ~/.codex/hooks/kaban && cp hooks/codex/*.sh ~/.codex/hooks/kaban/`. The `command`
   fields use `bash "${CODEX_HOME:-$HOME/.codex}"/hooks/kaban/<script>.sh`; Codex runs hook
   commands through a shell, so the expansion works (observed live). Edit the paths if you keep
   the scripts elsewhere.
2. Register the entries with **one** of:
   - `hooks.json`: copy `hooks/codex/hooks.json` to `~/.codex/hooks.json` (user scope; observed
     live), or merge its `hooks` object into an existing file. Codex also looks for
     `<repo>/.codex/hooks.json` (project scope; **unverified live**, not exercised).
   - `config.toml`: append `hooks/codex/config.hooks.toml` to `~/.codex/config.toml`
     (verified live in round 2). TOML array-of-tables entries may be appended anywhere in the
     file as long as `hooks` is not already an inline table.
3. Make sure Codex's environment carries the same `KABAN_DB` and `KABAN_HOME` (and
   `KABAN_WORKSPACE` if you set one) as the kaban MCP server entry, and that `kaban` is on
   `PATH` (or set `KABAN_BIN`). Without `KABAN_DB` the wrapper uses `<session cwd>/kaban.db`;
   hooks never create a database (`docs/developer/REGISTER.md`).
4. **Trust the hooks.** Codex runs user-level hooks only after you review them: open `/hooks` in
   the Codex TUI and approve the kaban entries (the approval is pinned to a hash of the hook
   configuration, so any edit asks again). `codex exec --dangerously-bypass-hook-trust` skips
   that review and is a **test flag for throwaway containers only** (the Docker harness uses
   it, M4-22d); never use it in a working checkout. Codex prints "`--dangerously-bypass-hook-trust`
   is enabled…" on every such run.

## What Codex sends and what kaban reads (HOST-EVENTS.md §2)

| Event | Fields kaban reads | Output |
|---|---|---|
| `SessionStart` | `session_id` | `hookSpecificOutput.additionalContext` (briefing, at most 5 items) |
| `PreToolUse` (`apply_patch`) | `session_id`, `tool_name`, the `*** Add File:` / `*** Update File:` headers of `tool_input.command`; `tool_input.file_path` if an `Edit`/`Write` tool ever exposes one (**unverified**: the model used `apply_patch` in every session) | `hookSpecificOutput.additionalContext` (`why` for each path, 3 records per path) or `{}` |
| `PostToolUse` (`Bash`) | `session_id`, `tool_input.command`, `tool_response` (a plain **string**) | `{}` |
| `SubagentStop` | `session_id`, `last_assistant_message` (**unverified**) | `{}` |
| `Stop` | `session_id`, `stop_hook_active` | `{"decision":"block","reason":…}` once per session when the session has a pending Failure, else `{}` |

Never read: `transcript_path`, the patch body, `UserPromptSubmit.prompt`, `last_assistant_message`
on `Stop`, anything under `SessionEnd`. Unknown fields are ignored.

## The Bash failure path (evidence-only until further notice)

Codex has no `PostToolUseFailure`. Its `PostToolUse` payload for the shell tool carries
`tool_response` as a plain string and **no exit-code field**. Characterised live (R2, W2-D2) for
the failing command `false`: the string is **empty** (0 characters), with no `Exit code:`
marker, no "exited with" phrase, no digit. The `apply_patch` response, by contrast, is a 6-line
string beginning with `Exit code: 0`. So a Codex shell failure is not detectable from the
payload today. The handler therefore records a Failure only when the string carries an
`Exit code: <n>` marker with `n != 0`, and treats every other `Bash` `PostToolUse` as success
evidence for an already pending Failure (never a guessed failure). The manual fallback (spec
§11) is `kaban remember Failure "<what failed>" --session-id <id>` or the MCP `remember` tool;
the Stop prompt then pairs the fix as usual. Whether a failing command **with output** carries
an `Exit code:` line is still unverified (only `false` was characterised); a handler branch, if
one is ever warranted, is the coordinator's decision (W2-C residual R2).

Consequence for `stop.sh`: on Codex the prompt fires only after a Failure was recorded through
the marker or manually; in the round-2 sessions `Stop` printed `{}`.

## Other notes

- `additionalContextLimit` (plan C4.5: 1500) was never exercised and is not set here
  (**unverified**); the briefing is capped at 5 items by kaban itself.
- `SessionEnd` is not used: Codex clamps its hook timeout to 3 s (observed: "clamping SessionEnd
  hook timeout to 3s" on a `config.toml` registration with a larger value), and the plan keeps
  the fix prompt on `Stop`.
- Codex `PreCompact`, `PostCompact`, `PermissionRequest`, `Interrupt`, `SubagentStart` are not
  used by kaban.
- Every script exits 0; a `kaban hook` usage error exits 4 before any hook logic (a misconfigured
  recipe fails loudly in Codex's hook log, never blocks). Exit 2 is never produced (PR-9).
- Offline verification shipped with this recipe: `jq -e .` on `hooks.json`, `config.hooks.toml`
  parses to the same document (`tomllib`), `bash -n` on every script; live verification is the
  Docker lane's (`scripts/hooks-live/run.sh codex <s> --recipes …`, OWN-7).
