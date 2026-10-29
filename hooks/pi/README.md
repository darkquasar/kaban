# kaban hooks for pi

Extension for pi 0.87.1 (core-spec F8, F8b; core-plan C4.5 as amended by M4-7, M4-21, M4-24 and
the live captures in `hooks/HOST-EVENTS.md` §3). pi has no hook-command layer: the extension
listens to pi's lifecycle events, spawns `kaban hook <event> --host pi --stdin-json` with a
small allowlisted envelope on stdin, and delivers kaban's answer through pi's own channels.

```
  pi event                 kaban hook (spawn, envelope on stdin)   what reaches the model
  session_start        =>  session-start --host pi            ->  pi.sendMessage(nextTurn)
  tool_call edit|write =>  pre-tool-use  --host pi            ->  pi.sendMessage(steer)
  tool_execution_end   =>  post-tool-use --host pi            ->  nothing (Failure or evidence)
    bash
  agent_before_settle  =>  stop          --host pi            ->  one continuation
                                                                 (custom_message, continue: true)
  session_shutdown     =>  nothing in M4 (`kaban sync` is M5)
```

## Files

| File | Role |
|---|---|
| `kaban-hooks.ts` | the extension (default export `(pi) => void`; no process is started in the factory) |

## Load

- For one session: `pi -e /path/to/hooks/pi/kaban-hooks.ts …`.
- For every session: copy it to `~/.pi/agent/extensions/kaban-hooks.ts` (pi loads every
  extension in that directory), or into a project's `.pi/extensions/`.

Environment (the same values as the kaban MCP server entry in `~/.pi/agent/mcp.json`):

| Variable | Meaning |
|---|---|
| `KABAN_BIN` | the kaban binary; default `kaban` on `PATH` |
| `KABAN_DB` | the database; default `<pi cwd>/kaban.db`. Hooks never create one |
| `KABAN_HOME` | the directory holding the audit keys (`~/.kaban` by default) |
| `KABAN_WORKSPACE` | optional explicit workspace; otherwise the enclosing git repository's root directory name (M4-32: the process cwd, never the event's cwd) |
| `KABAN_HOOK_DEBUG` | when set, kaban's stderr (fixed refusal texts only) is shown on pi's stderr; otherwise discarded |

## What the extension forwards (M4-7 allowlist)

Every envelope carries `session_id` (`ctx.sessionManager.getSessionId()`), `cwd` (`ctx.cwd`) and
`hook_event_name` (the pi event name, which kaban echoes back), plus:

| pi event | Forwarded | Never forwarded |
|---|---|---|
| `session_start` | the common fields only | `type`, `reason` |
| `tool_call` (`edit`, `write`) | `toolName`, `input.path` | `toolCallId`, `type`, `input.edits[]`, `input.content` |
| `tool_execution_end` (`bash`) | `toolName`, `isError`, the `text` items of `result.content[]` (as `{type: "text", text}`), and `input.command` attached from the matching `tool_call`/`tool_execution_start` by `toolCallId` (the end event carries no command, observed) | `toolCallId`, `type`, image items, `result.details` |
| `agent_before_settle` | the common fields only | `type`, `outcome`, `continue`, `context` |

`toolCallId` is read only to key the private `toolCallId -> command` map; it never enters an
envelope (R4-4). The kaban pi selector reads exactly these fields (`toolName`, `input.path`,
`input.command`, `isError`, `result.content[].text`), so nothing else is sent. Other tools
(`read`, `grep`, `find`, …) and other events are not forwarded at all.

## Behaviour

- **Briefing.** `session_start` -> the `additionalContext` of `kaban hook session-start` is sent
  with `pi.sendMessage({customType: "kaban-brief", content, display: true}, {deliverAs: "nextTurn"})`.
  Reached the model in every live session (round 1 sentinel 3/3; round 2 the seeded Decision
  text was echoed in the first reply).
- **Pre-edit context.** `tool_call` for `edit`/`write` -> `kaban hook pre-tool-use` -> the
  governing records for `input.path` as a `deliverAs: "steer"` message (reached the model 3/3 in
  round 1); the handler returns nothing, so the edit is never blocked or changed. `edit` was not
  used by the model live (**unverified**; `write` 5/5).
- **Failures and evidence.** `tool_execution_end` for `bash` -> `kaban hook post-tool-use`, which
  records a Failure (`pending_fix: true`, command, error text) when `isError` is true and
  success evidence (`evidence_runs[]`, with the files edited since) when the same command later
  succeeds. Both observed live in round 2.
- **Stop prompt.** `agent_before_settle` -> `kaban hook stop`; a `{"decision":"block","reason":…}`
  answer becomes exactly one `{entries: [{type: "custom_message", customType: "kaban-stop",
  content: reason, display: true}], continue: true}` continuation. Two guards: a once-flag in the
  extension and the handler's `prompted` row (the second `stop` of a session prints `{}`).
  Observed live: one extra agent cycle, the model answered "unresolved", no loop.
- **Print mode caveat.** In `pi --print` the `agent_before_settle` event reports
  `context.canContinue: false` at that moment, yet the continuation was still honoured
  (observed 3/3). The extension therefore does not gate on `canContinue`; if a future pi
  refuses the continuation in print mode, the prompt is simply lost for that session (never
  an error).
- **Bounds.** Every spawn has a 2 s kill timer (kaban's own 1.8 s watchdog answers first); stdout
  is capped at 64 KiB; the `toolCallId -> command` map holds at most 64 entries. Observed hook
  wall time in round 2: at most 6 ms per call.
- **Silence.** The extension writes nothing to stdout (pi's `--print --mode json` streams stayed
  intact in every live session) and never throws into pi: any failure (binary missing, timeout,
  non-JSON output) is ignored.
- `session_shutdown` does nothing in M4; `kaban sync` (M5) will be called there with
  argument-only `pi.exec`, which needs no stdin.

## Verification

Offline: `scripts/hooks-live/test-pi-extension.sh --kaban <bin>` runs, inside the harness image,
`node --experimental-strip-types --check` (syntax) and an invented-data run with a fake `pi`
object against the static binary with a recording shim: exact envelope keys per event (the
allowlist above), no `content` secret and no image item forwarded, briefing nextTurn,
pre-context steer, Failure recorded, one continuation, second settle silent, evidence appended
(`../specs/kaban/specs/00-project/evidence/m4-logs/m4corr2-pi-ext-fake-pi-green.log`). Live: the Docker lane
(`scripts/hooks-live/run.sh pi <s> --recipes …`, OWN-7), two round-2 sessions and one
re-verification session after R4-4 (`m4corr2-sessions/pi-c1`), all green.
