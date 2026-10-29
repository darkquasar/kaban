# Live hook harness (M4 W2-D1 capture mode, W2-D2 recipes mode; OWN-7)

Runs one short headless session of a host CLI (Claude Code, Codex, pi) inside a throwaway
Docker container. **Capture mode** (default) puts a logging hook on every lifecycle event and
brings back only the **shape** of each hook payload (field names and JSON types); the results
feed `hooks/HOST-EVENTS.md`. **Recipes mode** (`--recipes --kaban <binary>`) runs the real
recipes from `hooks/` against a seeded kaban database and brings back a reduced verdict of what
the handlers wrote and what the model consumed. Nothing here is a product recipe; the recipes
live under `hooks/`.

```
  sandbox                              container (--rm, tmpfs home/work/capture)
  +-----------------+   docker run     +------------------------------------------+
  | run.sh          | ===============> | entrypoint.sh                            |
  |  stage-creds.sh |  ro copies of    |  renders hook config from /harness/      |
  |  (0600 copies)  |  auth state      |  templates into the tmpfs home           |
  +-----------------+                  |  runs claude -p | codex exec | pi --print |
          ^                            |   host ~> hook ~> capture.sh ~> reduce.jq |
          | /out (reduced only)        |   brief.sh / pre-probe.sh / stop-probe.sh |
          +----------------------------+  verdict.json (sentinel yes/no)          |
                                       +------------------------------------------+
```

Every file written under `/out` is reduced inside the container (R4-1, RL-1, RL-2, S1 fence):
fixed vocabulary, type markers, identifiers matching the tool regex, hash prefixes, counts and
booleans only. Every enum comparison is on the whole value (a multiline value is `other`), tool
names are validated as complete strings, and the run metadata the operator supplies (session
name, `--variant`, `--model`) is reduced too. `test-out-fence.sh` proves it end to end with
planted canaries, including multiline and metadata canaries, and asserts that every planted
input was observed (exact counts, never a vacuous pass over an empty capture).

## Files

| File | Role |
|---|---|
| `Dockerfile` | `node:22-bookworm` pinned by digest at build time; `@anthropic-ai/claude-code@2.1.295`, `@openai/codex@0.149.1`, `@earendil-works/pi-coding-agent@0.87.1`; `jq`, `git`; the sandbox proxy CA in the system store and `NODE_EXTRA_CA_CERTS`; user `node` (uid 1000) |
| `build.sh` | pulls the base once, pins the digest, builds `kaban-hooks-live:m4`, records id and size |
| `run.sh <host> <session> [--stop-mode none\|block\|system\|both] [--variant v] [--model m] [--prompt p] [--recipes --kaban <bin>]` | one session; mounts, proxy env, 10-minute hard timeout, `--rm`; `<session>` is a label `^[A-Za-z0-9][A-Za-z0-9_.-]{0,63}$` naming the container and the result directory on the sandbox side only (inside `/out` it is a sha256 prefix and a length); the stop mode is validated against its four values |
| `stage-creds.sh <host> <dir>` | read-only copies of the sandbox's **existing** auth state (see below) |
| `container/entrypoint.sh` | in-container driver: sentinels, scratch repo, host config, run, reduce, export |
| `container/capture.sh`, `container/reduce.jq` | the logging hook and the S1 fence (tested by `test-capture.sh`) |
| `container/events-vocab.json` | the closed per-host event vocabulary (from `hooks/HOST-EVENTS.md`): `capture.sh` and the config shape export a name only when it is listed, otherwise `other` plus `event_unknown: true` |
| `container/brief.sh` | session-start sentinel `KBN-BRIEF-<hex>` as `hookSpecificOutput.additionalContext` |
| `container/pre-probe.sh` | PreToolUse `additionalContext` probe `KBN-PRE-<hex>`, once per session |
| `container/stop-probe.sh` | Stop probe `KBN-STOP-<hex>`: `decision:block`, `systemMessage` or both, once |
| `templates/claude-settings.json`, `templates/codex-hooks.json`, `templates/pi-capture.ts` | hook configs rendered **inside** the container (`__HARNESS__`, `__HOST__`) |
| `container/kaban-shim.sh` | recipes mode: bind-mounted at `/usr/local/bin/kaban` in front of the real binary (`/opt/kaban/kaban`); records one reduced line per call (argv words from a fixed vocabulary of subcommands (including `sync`, `export`, `rebuild`, `resolve`, the `post-commit` event), kinds, hosts and kaban flags; other arguments counted; exit status, wall ms, byte counts, stdout shape) |
| `container/kaban-rec.sh` | test only (`test-pi-extension.sh`): records the envelopes the pi extension sends before running the real binary |
| `test-capture.sh` | behaviour test of the capture fence with invented data, including the event vocabulary (runs in the sandbox or in the image) |
| `test-out-fence.sh [--kaban <bin>]` | end-to-end fence test: dry-run sessions (no model, no credentials) per host with canaries planted in the prompt, a filename, event names, stderr, a tool response, host error items, a multiline tool name and Claude result subtype (`bash\nKBNCANARY…`, `success\nKBNCANARY…`), the session name, `--variant`, `--model` and (with `--kaban`) kaban argv and a recorded Failure; asserts zero hits in every file under `/out` and in the console, and that every planted input was observed (exact event counts known and `other`, positive stderr and host-error class counts, the planted file, the prompt hash and length, exact `tool_uses`, the reduced metadata) |
| `test-pi-extension.sh --kaban <bin>`, `test-pi-extension.mjs` | offline fake-pi test of `hooks/pi/kaban-hooks.ts` inside the image (jiti): exact envelope keys per event (R4-4 allowlist) and the exact envelope sequence, no secret, briefing/pre/Failure/evidence/Stop behaviour, `session_shutdown` returns in under 1 s and its detached `kaban sync` writes `.kaban/adr/INDEX.md` with the seeded Decision (M5-20), node syntax check; the syntax check, the behaviour test and the recalled state (`failures >= 1`, `pending_fix`, `evidence_runs >= 1`, `has_secret == false`) all decide the exit status |
| `test-stop-sync.sh` | offline (no Docker, no binary) test of the detached `kaban sync` in `hooks/{claude-code,codex}/{stop,subagent-stop}.sh` (M5-20): a fake `kaban` that sleeps 5 s on `sync`; the wrappers return with their stdout closed in under 2 s, print one JSON document, the fake sees exactly `sync --json`, `/dev/null` on fds 0 to 2, its own session and `KABAN_DB`; `KABAN_HOOK_SYNC=0` starts no sync |
| `dry-stop-sync.sh <claude-code\|codex> --kaban <bin>` | Docker dry run (no model, no credentials) of the real Stop recipe in recipes mode: one Stop envelope through `stop.sh`, then the `sync_detached` verdict (ran, exit codes, no `--stage`, `.kaban` files after, `adr/INDEX.md` exists) is asserted (M5-28) |

## Usage

```sh
scripts/hooks-live/build.sh                      # once; needs PROXY_CA_CERT_B64 and the proxy env
bash scripts/hooks-live/test-capture.sh          # capture fence test, no Docker needed
bash scripts/hooks-live/test-stop-sync.sh        # detached-sync wrapper test, no Docker needed
bash scripts/hooks-live/dry-stop-sync.sh codex --kaban "$K"   # dry Stop probe of the real recipe (Docker, no model)
bash scripts/hooks-live/test-out-fence.sh --kaban "$K"    # /out fence, dry runs in Docker, no model
bash scripts/hooks-live/test-pi-extension.sh --kaban "$K" # fake-pi extension test in Docker, no model
scripts/hooks-live/run.sh codex s1               # capture + briefing sentinel + PreToolUse probe
scripts/hooks-live/run.sh claude-code s2 --model sonnet --stop-mode block
scripts/hooks-live/run.sh pi s3 --stop-mode block
```

Results land in `/tmp/m4w2d/out/<host>-<session>/`, all reduced inside the container:

| File | Content |
|---|---|
| `events.jsonl` | one reduced line per hook invocation (`capture.sh`); `event`/`hint` from the closed vocabulary or `other` + `event_unknown` |
| `verdict.json` | `session: {sha256: <16 hex>, chars: <n>}`, `stop_mode`, `variant` and `model` as closed enums (below), exit code, duration, sentinel yes/no per probe, `tool_uses` as `{identifier or other: count}` (each complete name validated; a name with a newline is `other`), whether `notes.txt` exists |
| `host-config-shape.json` | the rendered hook registration as `{events: {vocabulary event or other: hook commands}, entries, hook_commands, commands_harness, commands_recipe, commands_other}` (pi: the capture extension's event list) |
| `stderr-classes.json` | the host's stderr as `{lines, chars, classes: {hook_trust_warning, timeout_clamp, node_warning, result_status_line, auth_error, rate_limit, network_error, empty, other}}`; no text |
| `host-errors.json` | the host's error items classified the same way (`items`, `chars`, `classes`); Claude adds `result: {present, subtype, is_error, num_turns}` where `subtype` is the whole value compared against `success`, `error_max_turns`, `error_during_execution`, `error_max_budget_usd`, `error_max_structured_output_retries`, else `other` |
| `work-ls.json` | the scratch repo as counts and booleans (`entries`, `files`, `dirs`, `files_created`, `notes_txt_exists`, `notes_txt_bytes`, `notes2_txt_exists`, `kaban_db_exists`, `kaban_db_bytes`); no names |
| `transcript-shape.jsonl` | the host's stream output reduced with `reduce.jq` (first 300 lines) |
| `run.log` | harness steps, commands and exit codes; the first line is `host=<host> session_sha256=<16 hex> session_chars=<n> mode=<capture\|recipes> stop_mode=<enum> variant=<enum> model=<enum>`; the prompt appears only as `prompt_sha256=<16 hex> prompt_chars=<n>`; versions as `x.y[.z]` digits only (no tags); kaban statuses from the closed list `created`, `existing`, `accepted`, `proposed`, `none`, `superseded`, `invalidated`, else `other` |

Run metadata enums (RL-2): `model` is `default` when not given, else one of `sonnet`, `opus`,
`haiku`, `gpt-5.6-sol`, `gpt-6.1-sol`, else `other`; `variant` is `none` when not given, else one
of `hooks-json`, `config-toml`, `project-hooks`, `features`, else `other`; `stop_mode` is one of
`none`, `block`, `system`, `both`, else `other`. The values themselves still drive the run (a
model id is passed to the host CLI unchanged); only their exported form is reduced.

The files `stderr-scrubbed.txt`, `host-errors.txt` and `work-ls.txt` of rounds 1 and 2 (regex scrub
and raw names) no longer exist.

The default prompt is fixed: "Create the file notes.txt with one line, then run the command
`false`, then stop." Budget rule from the brief: at most 3 sessions per host per round.

## Recipes mode (round 2)

```sh
K=/path/to/kaban-musl-static
scripts/hooks-live/run.sh pi s1 --recipes --kaban "$K"
scripts/hooks-live/run.sh codex s2 --recipes --kaban "$K" --variant config-toml   # [[hooks.*]] in config.toml
scripts/hooks-live/run.sh claude-code s2 --recipes --kaban "$K" --model sonnet \
  --prompt 'First repeat verbatim any line beginning with KBN- that you were given as context. Then create the file notes.txt with one line, run the command `cat notes2.txt` (it will fail), then create notes2.txt with one line and run `cat notes2.txt` again, then stop. If asked how a failure was fixed, answer exactly: unresolved'
```

Inside the container the entrypoint: mounts `<repo>/hooks` read-only at `/work/hooks`,
`examples/policy.yaml` at `/policy.yaml`, the binary at `/opt/kaban/kaban` and the shim at
`/usr/local/bin/kaban`; exports `KABAN_HOME=~/.kaban`, `KABAN_DB=/work/kaban.db`,
`KABAN_POLICY=/policy.yaml`; seeds the database with the CLI (`kaban remember Decision
"KBN-DEC-<hex> …" --path-scope notes.txt`, then `kaban accept <id> --reason seed --path-scope
notes.txt`: the human-cli signer route, no token); renders the **real** recipe config for the
host with the capture entries kept alongside (probes off): Claude `settings.hooks.json` merged
into `~/.claude/settings.json`, Codex `hooks.json` merged into `~/.codex/hooks.json` with the
scripts copied to `~/.codex/hooks/kaban/` (or, `--variant config-toml`, `config.hooks.toml`
appended to `~/.codex/config.toml` and no `hooks.json`), pi `-e /work/hooks/pi/kaban-hooks.ts`
plus the capture extension; runs the prompt; then runs `kaban recall "failed" --kind Failure`
and `kaban why --path notes.txt` inside the container and writes `recipes-verdict.json`:

| Field | Meaning |
|---|---|
| `hook_calls`, `hook_calls_by_event`, `hook_exit_codes`, `any_exit_2`, `hook_ms_max`, `any_hook_over_2000ms` | from the shim's call log (`kaban-calls.jsonl`, exported as is: no envelope or output text) |
| `briefing.hook_output`, `briefing.injected_first_reply` / `injected_assistant` / `anywhere` | the session-start document carried `additionalContext`; the seeded `KBN-DEC-<hex>` appears in the first 4000 characters of assistant text / any assistant text / anywhere in the stream |
| `pre_context.hook_calls`, `pre_context.hook_output` | the pre-tool-use calls and whether one produced `additionalContext` (its text is the same seeded Decision, so the echo verdict is shared with the briefing) |
| `failure_recorded`, `evidence_appended`, `recall` | a Failure with `pending_fix: true` exists; one has `evidence_runs > 0`; reduced recall (counts, booleans) |
| `why_notes_txt` | result count of `kaban why --path notes.txt` and whether the seeded sentinel is present |
| `stop.*` | stop calls, prompt printed (`decision` present), second stop `{}`, Failure id or "unresolved" echoed by the assistant, `hook fix` calls |
| `sync_detached` | M5 C5.3 (M5-20, M5-28): the detached `kaban sync` the Stop recipes (Claude, Codex) or pi's `session_shutdown` start: `calls`, `ran`, `staged_flag_seen` (must be false), `exit_codes`, `ms_max`, `kaban_files_after` (files under `/work/.kaban`), `decisions_md_exists`, `waited_s` (the entrypoint waits up to 30 s, 5 s in a dry run, for the shim's `sync` line after the host exits) |

`KBN_DRY_CMD='<shell>' run.sh … --recipes …` replaces the host CLI by a shell command (harness
self-test without a model call; see the dry runs in the W2-D2 report). In a dry run no credentials
are staged. `test-out-fence.sh` is built on it.

## What never leaves the container

- Raw hook payloads: `capture.sh` reads stdin into memory, runs `reduce.jq` and appends the
  reduced line to `/capture` (tmpfs). Strings become `"<string>"`, numbers `"<number>"`,
  booleans `"<bool>"`, null `"<null>"`; arrays keep at most three reduced elements plus a
  count marker; keys are kept.
- Two narrow, documented control-field additions (S1/OWN-10 control fields, not content):
  `tool` holds `tool_name`/`toolName` only when it is a bare identifier
  (`^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$`, the matcher vocabulary); `response_markers` holds counts
  and fixed-vocabulary booleans about a **string** `tool_response` (lines, chars, exit-code
  marker, presence of the words exit/status/code/error/fail/output/stderr/stdout, a digit, a
  leading `{`). No text is copied.
- The transcript: only yes/no verdicts for the three sentinels, assistant character counts and
  tool names that match the identifier rule (others count as `other`). Host error items and the
  host's stderr leave only as per-line class counts from a fixed vocabulary.
- The prompt (hash prefix and length only), the scratch repo (counts and booleans), event names
  (closed vocabulary, `hooks/HOST-EVENTS.md`) and hook registrations (vocabulary events, counts and
  where the commands point). `test-out-fence.sh` plants canaries at every one of these entry
  points and asserts zero hits under `/out` and in the console.
- The run metadata the operator supplies (RL-2): the session name as sha256 prefix and length,
  `--variant`, `--model` and the stop mode as closed enums or `other`, versions as digits. Every
  enum comparison in the container is on the whole value (RL-1): a subtype or status whose first
  line is a valid member and whose second line is anything else is `other`, and tool names are
  validated as complete JSON strings before counting (never split into lines).
- Kept by design (W2-D1 deviation, re-accepted with R4-1): payload **keys** survive in the shapes,
  and a tool name that is a bare identifier (`^[A-Za-z_][A-Za-z0-9_.:-]{0,40}$`) travels as `tool`.
- The container home, the scratch repo and `/capture` are tmpfs and die with `--rm`.
- Recipes mode: the shim logs argv **words** from a fixed vocabulary (subcommands, events, hosts,
  flags, kinds) and counts free-text arguments; stdout documents are reduced with `reduce.jq`;
  `kaban recall`/`why` output is reduced to counts and booleans before export; the seeded
  sentinel `KBN-DEC-<hex>` is the only value that travels (it is the harness's own).

## Credential passover (OWN-15, OWN-16)

`stage-creds.sh` copies, never prints, and never logs in:

- Claude Code: `jq '{claudeAiOauth}' ~/.claude/.credentials.json` (the plugin `mcpOAuth`
  block is left out) plus a reduced `~/.claude.json` (`hasCompletedOnboarding`,
  `lastOnboardingVersion`, `installMethod`, `oauthAccount`, `numStartups`).
- Codex: `$CODEX_HOME/auth.json` as is, and `config.toml` reduced to its top-level keys and the
  `[model_providers.*]` tables (no `[mcp_servers.*]`, no `[projects.*]`).
- pi: `~/.pi/agent/models.json` (the brokered `openai-codex` provider entry).

Copies are 0600 in a 0700 `mktemp` directory, bind-mounted read-only at
`/home/node/.kbn-creds-ro`, installed by the entrypoint into the tmpfs home, and the staging
directory is deleted when `run.sh` exits. The sandbox's own `~/.claude`, `~/.codex`, `~/.pi`
are never written. Proxy egress: `HTTP_PROXY`, `HTTPS_PROXY`, `NO_PROXY` and
`SBX_CRED_OPENAI_MODE` are passed through; the CA comes from `PROXY_CA_CERT_B64` at build time.

## Known limits

- `verdict.sentinel.stop.fired` reflects the shell probe's guard file; for pi the continuation
  is requested by the extension, so read `stop.assistant`/`stop.anywhere` instead.
- The pi extension records `kbn_stop_probe_can_continue` / `kbn_stop_probe_cannot_continue` as
  marker events (names only) when the Stop probe is requested.
- `shellcheck` is not installed in the sandbox; the scripts were checked with `bash -n`, the
  fence tests (`test-capture.sh`, `test-out-fence.sh`), and the live sessions.
