# Host hook events: observed payload shapes (M4 W2-D1)

Field tables captured live inside Docker (OWN-7) with `scripts/hooks-live/`, nine headless
sessions on 2026-10-26: Claude Code 2.1.295 (`claude -p`, model `sonnet`), Codex CLI 0.149.1
(`codex exec --json`, the sandbox's brokered default model) and pi 0.87.1
(`pi --print --mode json`, `openai-codex/gpt-5.6-sol`); image `kaban-hooks-live:m4` on
`node@sha256:0e5f9065…da2a8`. Every payload was reduced to field names and JSON types before
it left the container (S1 fence, `scripts/hooks-live/container/reduce.jq`); values shown here
are only type markers, tool identifiers and the harness's own sentinel outcomes. Reduced
captures: `../specs/kaban/specs/00-project/evidence/m4-logs/m4w2d-sessions/<host>-<session>/events.jsonl`.

Prompt for every session (fixed by the brief): "Create the file notes.txt with one line, then
run the command `false`, then stop." Claude s3 used a subagent variant of the prompt to reach
`SubagentStart`/`SubagentStop` (recorded deviation).

Legend: `str` string, `num` number, `bool` boolean, `null`, `[]` empty array, `{…}` object.
"M4-7 selects" lists the only fields the kaban handler may read (allowlist, M4-7). Items
marked **unverified** were not observed in these sessions.

## 1. Claude Code 2.1.295

Config used: `~/.claude/settings.json` (container home) with `hooks.<Event>[].hooks[]` entries
of `type: "command"` and a `timeout` in seconds; hooks ran with `claude -p … --output-format
stream-json --dangerously-skip-permissions` as user `node` (uid 1000).

Common fields on every event: `session_id: str`, `transcript_path: str`, `cwd: str`,
`hook_event_name: str`. Tool and turn events add `prompt_id: str`, `permission_mode: str`,
`effort: {level: str}`. Events fired inside a subagent add `agent_id: str`, `agent_type: str`.
Environment seen by the hook process (names only): `CLAUDE_CODE_CHILD_SESSION`,
`CLAUDE_CODE_ENTRYPOINT`, `CLAUDE_CODE_MESSAGING_SOCKET`, `CLAUDE_CODE_MESSAGING_TOKEN`,
`CLAUDE_CODE_SESSION_ATTENDED`, `CLAUDE_CODE_SESSION_ID`, `CLAUDE_EFFORT` (tool events),
`CLAUDE_ENV_FILE` (SessionStart only), `CLAUDE_PID`, `CLAUDE_PROJECT_DIR`. No argv beyond the
configured command; the payload is stdin JSON only.

| Event (as emitted) | Fires | Event-specific fields (reduced) | M4-7 selects | Observed |
|---|---|---|---|---|
| `SessionStart` | once at start (`source: str`, matcher `startup\|resume\|clear\|compact`) | `source: str` | `session_id` | 3/3 sessions |
| `UserPromptSubmit` | once per prompt | `prompt: str` | none (not used by kaban) | 3/3 |
| `PreToolUse` (`Write`) | before the file write | `tool_name: str`, `tool_input: {file_path: str, content: str}`, `tool_use_id: str` | `tool_input.file_path`, `session_id`, `hook_event_name` | 3/3 (`Edit`, `MultiEdit` **unverified**: the model chose `Write`) |
| `PreToolUse` (`Bash`) | before the command | `tool_name`, `tool_input: {command: str, description: str}`, `tool_use_id` | none at pre-time (`command` only at failure) | 3/3 |
| `PreToolUse` (`Agent`) | before a subagent spawn | `tool_input: {description: str, prompt: str, subagent_type: str, run_in_background: bool}` | none | s3 |
| `PostToolUse` (`Write`) | after a successful write | `tool_input` as above, `tool_response: {type: str, filePath: str, content: str, structuredPatch: [], originalFile: null, userModified: bool}`, `tool_use_id`, `duration_ms: num` | none | 3/3 |
| `PostToolUse` (`Bash`) | after a command the host counts as successful | `tool_response: {stdout: str, stderr: str, interrupted: bool, isImage: bool, noOutputExpected: bool}`, `duration_ms` | `tool_input.command`, `session_id` (success evidence, C4.4) | s3 only, inside the subagent (see note) |
| `PostToolUseFailure` (`Bash`) | after `false` (non-zero exit) at top level | `tool_input: {command: str, description: str}`, `tool_use_id`, `error: str`, `is_interrupt: bool`, `duration_ms: num` | `tool_input.command`, `error`, `session_id` | s1, s2 |
| `PostToolUse` (`Agent`) | after the subagent returns | `tool_response: {status: str, prompt: str, agentId: str, agentType: str, content: [{type: str, text: str}], resolvedModel: str, totalDurationMs: num, totalTokens: num, totalToolUseCount: num, usage: {…}, toolStats: {…}, canContinueAgent: bool, harness*: num/str}` | none | s3 |
| `SubagentStart` | when the subagent starts | `agent_id: str`, `agent_type: str` (no `permission_mode`, no `effort`) | none | s3 |
| `SubagentStop` | when the subagent ends | `agent_id`, `agent_type`, `stop_hook_active: bool`, `agent_transcript_path: str`, `last_assistant_message: str`, `background_tasks: []`, `session_crons: []` | **`last_assistant_message`** is the summary field (M4-31); `session_id`, `agent_type` | s3 |
| `Stop` | when the top-level turn ends; again after a `decision: block` continuation | `stop_hook_active: bool`, `last_assistant_message: str`, `background_tasks: []`, `session_crons: []` | `session_id`, `stop_hook_active` | 3/3; twice in s2 |
| `SessionEnd` | at exit (`reason: str`), no `permission_mode` | `reason: str` | not used (1.5 s budget) | 3/3 |

Notes:

- `PostToolUseFailure` carries no exit code and no stdout/stderr split: only `error: str`
  (plus `is_interrupt`, `duration_ms`). Failure capture (M4-10) therefore takes `error` as the
  error text and `tool_input.command` as the command.
- Inside the subagent (s3) the `false` command surfaced as `PostToolUse(Bash)` with
  `tool_response.{stdout,stderr}` rather than `PostToolUseFailure`; whether the subagent
  wrapped the command (for example `false; echo $?`) is unknown (content not captured).
  Treat subagent failures as **unverified**; the top-level path is verified twice.
- No `SubagentStop` field named `summary` exists; M4-31 resolves to `last_assistant_message`.
- Consumption of hook output (sentinels): `SessionStart` `hookSpecificOutput.additionalContext`
  reached the model in 3/3 sessions (the reply began with `KBN-BRIEF-…`); `PreToolUse`
  `additionalContext` reached the model in s1 and s2 (`KBN-PRE-…` echoed; in s3 it fired
  inside the subagent and only the subagent saw it); `Stop` `{"decision":"block","reason":…}`
  with exit 0 reached the model once (s2: the next assistant message echoed `KBN-STOP-…`,
  `Stop` fired a second time, the probe printed nothing, the session ended). `systemMessage`
  on `Stop` was not tested on Claude (**unverified**).
- Exit-code and stdout rules observed: a hook that prints nothing and exits 0 never changed
  behaviour (capture hook on every event); JSON on stdout with exit 0 is consumed as above;
  exit 2 and non-JSON stdout were not exercised (**unverified**, kaban never uses exit 2).
- Timing: all hook commands ran with `timeout: 5` (1 for `SessionEnd`) without a timeout.

### Round 2 (final recipes, W2-D2)

Two sessions with `hooks/claude-code/settings.hooks.json` as shipped (commands via
`$CLAUDE_PROJECT_DIR/hooks/claude-code/*.sh`), the static musl `kaban 3.0.0-alpha.1` on `PATH`
behind a logging shim, a seeded database (one accepted Decision `KBN-DEC-<hex>` with path scope
`notes.txt`) and the capture hook kept alongside (`scripts/hooks-live/run.sh claude-code s1|s2
--recipes`, model `sonnet`). Reduced results: `../specs/kaban/specs/00-project/evidence/m4-logs/m4w2d2-sessions/claude-code-s{1,2}/`.

| Entry | Fired | kaban call | Output consumed |
|---|---|---|---|
| `SessionStart` | 1/1 per session | `hook session-start` 6 to 13 ms | briefing `additionalContext`; s2 (prompt asked to repeat any `KBN-` line) echoed the seeded Decision in the first reply |
| `PreToolUse` (`Write`) | s1 1, s2 2 | `hook pre-tool-use` 5 to 12 ms | `additionalContext` with the `why` record for `notes.txt`; `{}` for the ungoverned `notes2.txt` |
| `PostToolUseFailure` (`Bash`) | 1/1 | `hook post-tool-use-failure` 8 to 10 ms | a Failure with `pending_fix: true`, command and error text (`recall` after the session) |
| `PostToolUse` (`Bash`) | s2 1 (the successful re-run) | `hook post-tool-use` 4 ms | `evidence_runs` 1 on the pending Failure (`files_edited_since` from the session's edit rows) |
| `Stop` | 2 per session | `hook stop` 7 to 12 ms, then 7 to 9 ms | first: `{"decision":"block","reason":…}` delivered, the model answered "unresolved"; second: `{}`, session ended |
| `SubagentStop` | 0 (no subagent) | | **unverified** in round 2 as in round 1 |

All hook exits 0; no exit 2; max 13 ms per call; host exit 0 (`result subtype=success`) in both
sessions, 11 s and 16 s. Deltas against round 1: none in the field shapes (the capture hook
re-confirmed the §1 tables); the recipes consumed exactly the channels round 1 proved.

## 2. Codex CLI 0.149.1

Config used: `$CODEX_HOME/hooks.json` (container home) with the same `hooks.<Event>[].hooks[]`
shape as Claude Code (`type: "command"`, `command`, `timeout`), run with
`codex exec --json --skip-git-repo-check --dangerously-bypass-approvals-and-sandbox
--dangerously-bypass-hook-trust` (M4-22d, container only). Hooks were active without any
`features` flag; Codex printed the warning "`--dangerously-bypass-hook-trust` is enabled.
Enabled hooks may run without review for this invocation." twice per session as `error`
items in the `--json` stream. The project location `<repo>/.codex/hooks.json` was not
exercised (**unverified**).

Common fields: `session_id: str`, `transcript_path: str`, `cwd: str`, `hook_event_name: str`,
`model: str`, `permission_mode: str`; turn-scoped events add `turn_id: str`. Environment
(names only): `CODEX_HOME`, `CODEX_MANAGED_BY_NPM`, `CODEX_MANAGED_PACKAGE_ROOT`
(`CLAUDE_PLUGIN_ROOT` from E4 was **not** present). Stdin JSON only.

| Event (as emitted) | Fires | Event-specific fields (reduced) | M4-7 selects | Observed |
|---|---|---|---|---|
| `SessionStart` | once (`source: str`), no `turn_id` | `source: str` | `session_id` | 3/3 |
| `UserPromptSubmit` | once per prompt | `prompt: str` | none | 3/3 |
| `PreToolUse` (`apply_patch`) | before the file change | `tool_name: str` (`apply_patch`), `tool_input: {command: str}`, `tool_use_id: str` | paths parsed from `tool_input.command` (`*** Add File:` / `*** Update File:` headers, M4-18, M4-30); `session_id` | 3/3 |
| `PreToolUse` (`Bash`) | before `false` | `tool_name` (`Bash`), `tool_input: {command: str}`, `tool_use_id` | none | 3/3 |
| `PostToolUse` (`apply_patch`) | after the change | `tool_input: {command: str}`, **`tool_response: str`** (6 lines, contains an `Exit code: N` marker), `tool_use_id` | none | 3/3 |
| `PostToolUse` (`Bash`) | after `false` (there is no `PostToolUseFailure`) | `tool_input: {command: str}`, **`tool_response: str`** (1 line, **no** `exit code` marker), `tool_use_id` | `tool_input.command`, `tool_response` (string, see note), `session_id` | 3/3 |
| `PostToolUse` (`Bash`), **R2 characterisation** (W2-D2, extended markers) | after `false` | `tool_response: str` is the **empty string**: 0 chars, `empty: true`, no `Exit code:` marker, does not start with `Exit code`, no "exited with code N", no digit, none of exit/status/code/error/fail/output/stderr/stdout, no leading `{`. (`apply_patch` in the same session: 6 lines, 98 chars, starts with `Exit code:`, `Wall time` present.) | the Codex `Bash` path stays **evidence-only**: no in-payload failure signal exists for a command without output | R2 1/1; re-confirmed in round 2 s1 and s2 (2/2) |
| `Stop` | at turn end; again after a `decision: block` continuation | `stop_hook_active: bool`, `last_assistant_message: str` | `session_id`, `stop_hook_active` | 3/3; twice in s3 |
| `SessionEnd` | at exit; only `session_id`, `transcript_path`, `cwd`, `hook_event_name`, `reason: str` | `reason: str` | not used | 3/3 |
| `SubagentStart`, `SubagentStop` | configured, never fired (no subagent in these sessions) | **unverified** | `last_assistant_message` by analogy with `Stop` (**unverified**) | 0/3 |
| `PreCompact`, `PostCompact`, `PermissionRequest`, `Interrupt` | not configured | **unverified** | | |

Notes (binding for the C4.5 Codex recipe):

- **`tool_response` is a plain string, not an object.** There is no `exit_code`/`exitCode`
  field in the `PostToolUse` payload (plan C4.5 assumed one). The handler's "stderr heuristic"
  is the only in-payload signal. For `apply_patch` the string carries an `Exit code: <n>`
  marker; for the failing `Bash` command the string was one line without that marker (chars
  and words were not captured in these sessions, see Decisions needed in the W2-D1 report).
  Until a further capture settles the `Bash` response format, the Codex failure signal
  stays **unverified** and the recipe must treat the `Bash` `PostToolUse` path as
  evidence-only (spec §11: missing host events fall back to documented manual flows).
- `apply_patch` input shape (M4-30): `tool_input.command: str` holding the patch text; the
  model also used `apply_patch` through a shell command (the `--json` stream showed a
  `file_change` item), and the hook still reported `tool_name: apply_patch`.
- Consumption: `SessionStart` `additionalContext` reached the model 3/3; `PreToolUse`
  `additionalContext` (M4-30) **reached the model** 3/3 (`KBN-PRE-…` echoed), so the recipe may
  keep the `PreToolUse` entry; `Stop` `{"systemMessage": …}` did **not** reach the model and did
  not appear anywhere in the `--json` stream (s2); `Stop` `{"decision":"block","reason":…}` with
  exit 0 **did** reach the model once (s3; second `Stop`, probe silent, session ended). M4-16's
  bounded switch applies: the Codex Stop prompt uses `decision: block`, not `systemMessage`.
- Exit-code and stdout rules observed: same as Claude (silent exit 0 is inert; stdout JSON with
  exit 0 consumed). Exit 2 **unverified**. `additionalContextLimit` not exercised (**unverified**).

### Round 2 (final recipes, W2-D2)

Two sessions with the shipped `hooks/codex/` recipe, scripts at `$CODEX_HOME/hooks/kaban/`, the
commands `bash "${CODEX_HOME:-$HOME/.codex}"/hooks/kaban/<script>.sh` (the shell expansion
works: Codex runs hook commands through a shell), the seeded database and the capture hook
alongside. s1 registered `hooks.json`; s2 registered the same entries as `[[hooks.<Event>]]`
tables appended to `$CODEX_HOME/config.toml` with **no** `hooks.json` (`--variant config-toml`).
Reduced results: `../specs/kaban/specs/00-project/evidence/m4-logs/m4w2d2-sessions/codex-s{1,2}/`.

| Entry | Fired (s1 hooks.json / s2 config.toml) | kaban call | Output consumed |
|---|---|---|---|
| `SessionStart` | 1 / 1 | `hook session-start` 4 ms / 21 ms | briefing `additionalContext`; the seeded Decision was echoed in the first reply in both sessions |
| `PreToolUse` (`apply_patch`) | 1 / 1 | `hook pre-tool-use` 6 ms / 9 ms | `additionalContext` with the `why` record for `notes.txt` (paths from the patch headers) |
| `PostToolUse` (`Bash`) | 1 / 1 | `hook post-tool-use` 4 ms / 4 ms | `{}`: the response string was empty (R2), so no Failure was recorded, as designed (evidence-only) |
| `Stop` | 1 / 1 | `hook stop` 7 ms / 7 ms | `{}` (no pending Failure in the session) |
| `SubagentStop` | 0 / 0 | | **unverified** |

**`config.toml` registration: verified.** Codex 0.149.1 fires hooks declared as `[[hooks.<Event>]]`
tables in `config.toml` (8 capture events and 4 kaban calls in s2, identical to s1). The host
printed one extra value-free diagnostic for that registration: "clamping SessionEnd hook timeout
to 3s in …/config.toml" (the capture entry asked for 5 s), which also settles the `SessionEnd`
budget at 3 s. All hook exits 0; no exit 2; max 21 ms; host exit 0, 15 s per session; the
bypass-trust warning twice per session as in round 1. Deltas against round 1: none in the
field shapes; the `Bash` response is now characterised (empty).

## 3. pi 0.87.1 (extension events)

Config used: an extension file loaded with `-e` (rendered from
`scripts/hooks-live/templates/pi-capture.ts`), `--no-extensions --no-skills --no-session`,
provider `openai-codex`, model `gpt-5.6-sol`. The extension pipes `JSON.stringify(event)` to
`capture.sh` on stdin via `child_process.spawn` (M4-21: `pi.exec` has no stdin). pi has no
`PostToolUseFailure`: `tool_execution_end.isError` is the failure signal.

| Event | Fires | Fields (reduced) | M4-7 selects | Observed |
|---|---|---|---|---|
| `session_start` | once | `type: str`, `reason: str` | none (briefing trigger) | 3/3 |
| `before_agent_start` | before the first model call | `prompt: str`, `systemPrompt: str`, `systemPromptOptions: {appendSystemPrompt, contextFiles, cwd, promptGuidelines, sections, selectedTools, skills, toolGuidelines, toolSnippets}` | none | 3/3 |
| `agent_start`, `agent_settled` | per agent run | `type` only | none | 3/3 |
| `turn_start` | per turn | `turnIndex: num`, `timestamp: num` | none | 3/3 |
| `tool_execution_start` (`write`) | before the write | `toolCallId: str`, `toolName: str`, `args: {path: str, content: str}` | `args.path` (why) | 3/3 |
| `tool_call` (`write`) | can block/mutate | `toolName`, `toolCallId`, `input: {path: str, content: str}` | `input.path` | 3/3 |
| `tool_execution_start` / `tool_call` (`bash`) | before the command | `args`/`input: {command: str, timeout: num}` | none | 3/3 |
| `tool_result` | after a tool | `toolName`, `toolCallId`, `input: {…}`, `content: [{type: str, text: str}]`, `details: {}` (bash), `isError: bool` | none | 3/3 |
| `tool_execution_end` (`bash`) | after the command | `toolCallId`, `toolName`, `result: {content: [{type: str, text: str}], details: {}}`, `isError: bool` | `toolName`, `isError`, `result.content[].text` (error text), `input.command` from the matching `tool_call` by `toolCallId` | 3/3 |
| `tool_execution_end` (`write`) | after the write | `result: {content: [{type, text}]}`, `isError` | none | 3/3 |
| `turn_end` | per turn | `turnIndex`, `message: {role, content: [thinking/text/toolCall items], api, provider, model, usage: {…cost…}, stopReason, timestamp, responseId, rawStopReason}`, `toolResults: [{role, toolCallId, toolName, content, details?, isError, timestamp}]`, `messageEntryId`, `toolResultEntryIds: [str]`, `outcome: str`, `entries: []`, `continue: bool` | none | 3/3 |
| `agent_end` | per agent run | `messages: [{content, role, sections, timestamp, toolsAdded}, …]` | none | 3/3 |
| `agent_before_settle` | final boundary | `outcome: str`, `entries: []`, `continue: bool`, `context: {canContinue: bool, contextEntries, contextMessages, llmMessages, pendingMessages}` | none (prompt trigger) | 3/3 |
| `session_shutdown` | at exit | `reason: str` | not used | 3/3 |

Notes:

- Tool names are lowercase (`write`, `bash`); the edit tool was not used by the model
  (`edit` **unverified** live; its input is `path` plus `edits[]` per the local types).
- `tool_execution_end` carries no exit code; `isError: bool` is the signal and the error text
  is `result.content[].text`. The command is not in the end event: the recipe keeps a
  `toolCallId -> input.command` map from `tool_call`/`tool_execution_start`.
- Consumption: `pi.sendMessage({…}, {deliverAs: "nextTurn"})` from `session_start` reached the
  model 3/3 (`KBN-BRIEF-…` echoed); a `deliverAs: "steer"` message sent from `tool_call` for
  the write reached the model 3/3 (`KBN-PRE-…`); an `agent_before_settle` result
  `{entries: [{type: "custom_message", …}], continue: true}` produced exactly one continuation
  (s3: extra `agent_start … agent_end agent_before_settle` cycle, `KBN-STOP-…` echoed, no loop
  thanks to the once-guard), even though `context.canContinue` was `false` in print mode at
  that moment (marker `kbn_stop_probe_cannot_continue`). s2 asked for no continuation because
  the extension gated on `canContinue` (fixed for s3).
- The `--print --mode json` stream printed every event as JSON lines (94 to 114 lines per
  session); the extension never wrote to stdout and did not disturb it.
- Exit codes: pi exited 0 in all sessions; the extension spawned `capture.sh` 21 to 27 times per
  session with no visible delay.

### Round 2 (final recipes, W2-D2)

Two sessions with `hooks/pi/kaban-hooks.ts` loaded with `-e` beside the capture extension
(sentinel probes off), the seeded database, `KABAN_HOOK_DEBUG=1`. Reduced results:
`../specs/kaban/specs/00-project/evidence/m4-logs/m4w2d2-sessions/pi-s{1,2}/`.

| Extension handler | Fired | kaban call | Output consumed |
|---|---|---|---|
| `session_start` | 1/1 | `hook session-start` 4 to 5 ms | `sendMessage(nextTurn)` with the briefing; s2 echoed the seeded Decision in the first reply |
| `tool_call` (`write`) | s1 1, s2 2 | `hook pre-tool-use` 4 to 6 ms | `sendMessage(steer)` with the `why` record for `notes.txt`; nothing sent for the ungoverned `notes2.txt` (`{}`) |
| `tool_execution_end` (`bash`, `isError: true`) | 1/1 | `hook post-tool-use` 5 ms | a Failure with `pending_fix: true`, command (attached from `tool_call`) and error text |
| `tool_execution_end` (`bash`, `isError: false`) | s2 1 | `hook post-tool-use` 4 ms | `evidence_runs` 1 on the pending Failure |
| `agent_before_settle` | 2 per session | `hook stop` once (4 ms; the once-flag skipped the second) | `{"decision":"block",…}` turned into one `custom_message` continuation; the model answered "unresolved"; one extra `agent_start … agent_end` cycle, no loop |

All hook exits 0; no exit 2; max 6 ms; pi exit 0, 24 s and 30 s; the extension wrote nothing
to stdout (81 and 194 JSON lines streamed intact). Deltas against round 1: the real extension
sends one steer message per governed edit instead of one sentinel per session; `canContinue`
was again `false` at `agent_before_settle` in print mode and the continuation was honoured
again (2/2).

Re-verified once after the R4-4 allowlist cut (C4.9, `m4corr2-sessions/pi-c1`, s2 prompt): the
same six calls and verdicts with the reduced envelopes (101 to 299 bytes against 143 to 439), all
exits 0, max 6 ms, pi exit 0 in 29 s.

## 4. Cross-host summary for the recipes

| Question | Claude Code | Codex | pi |
|---|---|---|---|
| Session briefing via `additionalContext` consumed | yes (3/3) | yes (3/3) | yes via `sendMessage nextTurn` (3/3) |
| PreToolUse context consumed (M4-30) | yes (2/2 top-level) | yes (3/3) | yes via `sendMessage steer` |
| Stop prompt channel that reaches the model (M4-16) | `decision: block` (1/1) | `decision: block` (1/1); `systemMessage` no (0/1) | `agent_before_settle` `continue: true` (1/1) |
| Failure signal | `PostToolUseFailure.error: str` | `PostToolUse` with `tool_response: str`; no exit field; marker format **unverified** for `Bash` | `tool_execution_end.isError` |
| Edit path field | `tool_input.file_path` | `tool_input.command` (patch headers) | `input.path` / `args.path` |
| SubagentStop summary field (M4-31) | `last_assistant_message` | **unverified** (never fired) | n/a |
| Second Stop after a block prints `{}`: loop-safe | yes | yes | yes (once-guard in the extension) |

## 5. Round-2 additions to the summary (W2-D2)

| Question | Claude Code | Codex | pi |
|---|---|---|---|
| Real recipe briefing echoed by the model | yes (s2, asked to repeat) | yes (2/2) | yes (s2) |
| Failure captured by the real recipe | yes (`PostToolUseFailure`) | **no**: the `Bash` response is empty (R2); manual fallback (spec §11, `docs/developer/REGISTER.md`) | yes (`isError`) |
| Success evidence on the re-run | yes | n/a (no pending Failure) | yes |
| Stop prompt delivered once, second Stop `{}` | yes | n/a (printed `{}`) | yes (once-flag; the handler's `prompted` row is the second guard) |
| Registration path verified | `settings.json` `hooks` object | `~/.codex/hooks.json` **and** `[[hooks.<Event>]]` in `config.toml` | `-e kaban-hooks.ts` |
| Max hook wall time (idle seeded database) | 13 ms | 21 ms | 6 ms |
| Any exit 2 (PR-9) | none | none | none |
