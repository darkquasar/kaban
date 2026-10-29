// kaban hooks for pi 0.87.1 (core-spec F8, F8b, F13; core-plan C4.5 as amended by M4-7, M4-21,
// M4-24, M4-32; C5.3 as amended by M5-20). Load with `pi -e /path/to/kaban-hooks.ts` or copy into
// `~/.pi/agent/extensions/`. See README.md in this directory.
//
//   pi event                 kaban hook (spawn, envelope on stdin)   what reaches the model
//   session_start        =>  kaban hook session-start --host pi  ->  sendMessage(nextTurn)
//   tool_call edit|write =>  kaban hook pre-tool-use  --host pi  ->  sendMessage(steer)
//   tool_execution_end   =>  kaban hook post-tool-use --host pi  ->  nothing (Failure or evidence)
//     bash
//   agent_before_settle  =>  kaban hook stop          --host pi  ->  one continuation
//                                                                    (custom_message, continue)
//   session_shutdown     =>  kaban sync --json --deadline 60 (detached, ->  nothing (the .kaban/*.md
//                            own session, no descriptor inherited,        flush; never --stage)
//                            never awaited; bounded by kaban itself)
//
// Rules: kaban is spawned with the envelope on stdin, never in argv (M4-21: `pi.exec` has no
// stdin and argv would show error text to `ps`); every spawn has a 2 s kill timer and kaban's
// own 1.8 s watchdog answers first; a hook failure is silent (best effort, never blocks pi);
// only allowlisted fields are forwarded (M4-7, R4-4): `session_id`, `cwd`, `hook_event_name`,
// `toolName`, the edited `path` or the shell `command`, `isError` and the shell result text;
// `type`, `reason`, `toolCallId`, `outcome`, `continue`, `context` and `details` are never
// forwarded (`toolCallId` lives only in the private command map); `edits[]` and `content` are
// never read; nothing is written
// to stdout, so `--print --mode json` streams stay intact; no process is started in the
// extension factory. `kaban hook` never creates a database: point KABAN_DB (and KABAN_HOME,
// KABAN_WORKSPACE) at the same values as the kaban MCP server entry.
//
// session_shutdown (M5-20, R5-9): `kaban sync --json --deadline <s>` is started with
// `spawn(..., {detached: true, stdio: "ignore"})` and `unref()` and is never awaited, so pi's
// shutdown is not delayed and pi's exit does not kill the flush (`pi.exec` and `runHook` would
// pipe the child's output and await it, killing it on their 2 s timeout, and a timer in this
// extension dies with pi; neither can bound a flush that outlives the host). The bound is the
// deadline kaban honours itself: at `KABAN_SYNC_DEADLINE` seconds (default 60; digits only,
// anything else falls back to 60) the sync prints a fixed line and exits 4, and the next sync
// continues. Set KABAN_HOOK_SYNC=0 to skip the sync.
import { spawn } from "node:child_process";

const KABAN_BIN = process.env.KABAN_BIN ?? "kaban";
const HOOK_TIMEOUT_MS = 2000;
/** Seconds the detached `kaban sync` may run (R5-9); a fixed, digits-only argv value. */
const SYNC_DEADLINE_S = /^[0-9]{1,5}$/.test(process.env.KABAN_SYNC_DEADLINE ?? "")
  ? (process.env.KABAN_SYNC_DEADLINE as string)
  : "60";
const MAX_OUTPUT_BYTES = 64 * 1024;
const MAX_TRACKED_COMMANDS = 64;

type HookEvent = "session-start" | "pre-tool-use" | "post-tool-use" | "stop";

/** Run `kaban hook <event> --host pi` with `envelope` on stdin; the parsed JSON document, or
 *  undefined on any failure (timeout, spawn error, non-JSON, empty). Never throws. */
function runHook(event: HookEvent, envelope: Record<string, unknown>, cwd: string): Promise<any> {
  return new Promise((resolve) => {
    let done = false;
    let out = "";
    const finish = (value: any) => {
      if (!done) {
        done = true;
        resolve(value);
      }
    };
    let body: string;
    try {
      body = JSON.stringify(envelope);
    } catch {
      finish(undefined);
      return;
    }
    try {
      const env: NodeJS.ProcessEnv = { ...process.env };
      if (!env.KABAN_DB) env.KABAN_DB = `${cwd}/kaban.db`;
      const child = spawn(KABAN_BIN, ["hook", event, "--host", "pi", "--stdin-json", "--json"], {
        cwd,
        env,
        // stderr carries only kaban's fixed refusal texts; shown when KABAN_HOOK_DEBUG is set
        stdio: ["pipe", "pipe", process.env.KABAN_HOOK_DEBUG ? "inherit" : "ignore"],
      });
      const timer = setTimeout(() => {
        try {
          child.kill("SIGKILL");
        } catch {}
        finish(undefined);
      }, HOOK_TIMEOUT_MS);
      child.stdout?.on("data", (chunk: Buffer) => {
        if (out.length < MAX_OUTPUT_BYTES) out += chunk.toString("utf8");
      });
      child.on("close", () => {
        clearTimeout(timer);
        try {
          finish(out.trim() ? JSON.parse(out) : undefined);
        } catch {
          finish(undefined);
        }
      });
      child.on("error", () => {
        clearTimeout(timer);
        finish(undefined);
      });
      child.stdin.on("error", () => {});
      child.stdin.end(body);
    } catch {
      finish(undefined);
    }
  });
}

function additionalContext(doc: any): string | undefined {
  const text = doc?.hookSpecificOutput?.additionalContext;
  return typeof text === "string" && text.length > 0 ? text : undefined;
}

/** The text items of a pi tool result, as the kaban pi variant reads them
 *  (`result.content[].text`); other item kinds (images) are dropped. */
function textItems(result: any): Array<{ type: "text"; text: string }> {
  const content = Array.isArray(result?.content) ? result.content : [];
  return content
    .filter((item: any) => item && item.type === "text" && typeof item.text === "string")
    .map((item: any) => ({ type: "text" as const, text: item.text }));
}

export default function (pi: any) {
  // toolCallId -> bash command, attached to tool_execution_end which carries no command
  const commands = new Map<string, string>();
  let stopPrompted = false; // first guard; the handler's `prompted` row is the second

  const remember = (id: unknown, command: unknown) => {
    if (typeof id !== "string" || typeof command !== "string") return;
    if (commands.size >= MAX_TRACKED_COMMANDS) {
      const oldest = commands.keys().next().value;
      if (oldest !== undefined) commands.delete(oldest);
    }
    commands.set(id, command);
  };

  const base = (ctx: any, hookEventName: string): Record<string, unknown> => ({
    session_id: ctx?.sessionManager?.getSessionId?.() ?? "",
    cwd: ctx?.cwd ?? process.cwd(),
    hook_event_name: hookEventName,
  });

  pi.on("session_start", async (_event: any, ctx: any) => {
    // common fields only: the handler reads nothing else at session start (R4-4)
    const doc = await runHook("session-start", base(ctx, "session_start"), ctx?.cwd ?? process.cwd());
    const text = additionalContext(doc);
    if (!text) return;
    try {
      await pi.sendMessage(
        { customType: "kaban-brief", content: text, display: true },
        { deliverAs: "nextTurn" },
      );
    } catch {}
  });

  pi.on("tool_call", async (event: any, ctx: any) => {
    const toolName = String(event?.toolName ?? "");
    if (toolName === "bash") {
      remember(event?.toolCallId, event?.input?.command);
      return undefined;
    }
    if (toolName !== "edit" && toolName !== "write") return undefined;
    const path = event?.input?.path;
    if (typeof path !== "string") return undefined;
    const doc = await runHook(
      "pre-tool-use",
      {
        ...base(ctx, "tool_call"),
        toolName,
        input: { path }, // never `edits[]` or `content` (M4-7); no `toolCallId` (R4-4)
      },
      ctx?.cwd ?? process.cwd(),
    );
    const text = additionalContext(doc);
    if (text) {
      try {
        await pi.sendMessage(
          { customType: "kaban-pre", content: text, display: true },
          { deliverAs: "steer" },
        );
      } catch {}
    }
    return undefined; // kaban never blocks an edit
  });

  // second source for the command map (the event the host fires right before running)
  pi.on("tool_execution_start", async (event: any) => {
    if (String(event?.toolName ?? "") === "bash") remember(event?.toolCallId, event?.args?.command);
  });

  pi.on("tool_execution_end", async (event: any, ctx: any) => {
    if (String(event?.toolName ?? "") !== "bash") return;
    const id = typeof event?.toolCallId === "string" ? event.toolCallId : undefined;
    const command = id ? commands.get(id) : undefined;
    if (id) commands.delete(id);
    if (command === undefined) return; // no command, nothing kaban could pair or record
    await runHook(
      "post-tool-use",
      {
        ...base(ctx, "tool_execution_end"),
        toolName: "bash",
        isError: event?.isError === true,
        input: { command },
        result: { content: textItems(event?.result) }, // text items only; no `details` (R4-4)
      },
      ctx?.cwd ?? process.cwd(),
    );
  });

  pi.on("agent_before_settle", async (_event: any, ctx: any) => {
    if (stopPrompted) return undefined;
    // common fields only: `outcome`, `continue` and `context` are never forwarded (R4-4)
    const doc = await runHook("stop", base(ctx, "agent_before_settle"), ctx?.cwd ?? process.cwd());
    if (doc?.decision !== "block" || typeof doc.reason !== "string" || !doc.reason) return undefined;
    stopPrompted = true;
    // one bounded continuation; observed live to reach the model even when
    // context.canContinue was false in print mode (HOST-EVENTS.md §3)
    return {
      entries: [{ type: "custom_message", customType: "kaban-stop", content: doc.reason, display: true }],
      continue: true,
    };
  });

  pi.on("session_shutdown", async (_event: any, ctx: any) => {
    if (process.env.KABAN_HOOK_SYNC === "0") return;
    const cwd = ctx?.cwd ?? process.cwd();
    try {
      const env: NodeJS.ProcessEnv = { ...process.env };
      if (!env.KABAN_DB) env.KABAN_DB = `${cwd}/kaban.db`;
      // fully detached: own session, no inherited descriptor, not awaited, not tracked; the
      // wall-clock bound is kaban's own `--deadline` (R5-9), since nothing here outlives pi
      const child = spawn(KABAN_BIN, ["sync", "--json", "--deadline", SYNC_DEADLINE_S], {
        cwd,
        env,
        detached: true,
        stdio: "ignore",
      });
      child.on("error", () => {});
      child.unref();
    } catch {}
  });
}
