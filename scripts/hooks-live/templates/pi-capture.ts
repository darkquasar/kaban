// pi 0.87.1 capture extension for the live hook harness (M4 W2-D1, OWN-7, M4-21, M4-24).
// Instantiated inside the container from this template (placeholders __HARNESS__, __HOST__).
//
// - every listed event is serialised and piped on stdin to capture.sh (spawn with a pipe,
//   never argv: M4-21), which reduces it to field names and types before writing;
// - session_start sends the briefing sentinel (KBN_BRIEF) as a custom message for the next
//   turn (plan C4.5: pi.sendMessage nextTurn);
// - tool_call for edit|write sends KBN_PRE once as a steer message (PreToolUse-context analogue);
// - agent_before_settle appends KBN_STOP once and requests one continuation when
//   KBN_STOP_MODE is not "none" (the Stop-prompt analogue for pi);
// - KBN_PROBES=off (recipes mode, W2-D2) keeps the capture and switches every sentinel probe off,
//   so the real kaban extension loaded alongside is the only source of injected messages;
// - no process is started in the factory (E4 §3.3); nothing is printed to stdout.
import { spawn } from "node:child_process";
import { readFileSync } from "node:fs";

const HARNESS = "__HARNESS__";
const HOST = "__HOST__";
const CAPTURE = `${HARNESS}/capture.sh`;

const EVENTS = [
  "session_start",
  "before_agent_start",
  "agent_start",
  "turn_start",
  "tool_call",
  "tool_execution_start",
  "tool_execution_end",
  "tool_result",
  "turn_end",
  "agent_end",
  "agent_before_settle",
  "agent_settled",
  "session_shutdown",
];

function sentinel(name: string): string {
  try {
    const text = readFileSync("/capture/sentinel.env", "utf8");
    const m = text.match(new RegExp(`^${name}=(.*)$`, "m"));
    return m ? m[1].trim() : `${name}-00000000`;
  } catch {
    return `${name}-00000000`;
  }
}

function safeStringify(value: unknown): string {
  const seen = new WeakSet<object>();
  return JSON.stringify(value, (_k, v) => {
    if (typeof v === "bigint") return Number(v);
    if (typeof v === "function") return "<function>";
    if (v && typeof v === "object") {
      if (seen.has(v)) return "<cycle>";
      seen.add(v);
    }
    return v;
  });
}

function capture(eventType: string, event: unknown): Promise<void> {
  return new Promise((resolve) => {
    let body = "";
    try {
      body = safeStringify(event) ?? "";
    } catch {
      body = JSON.stringify({ type: eventType, "<unserializable>": true });
    }
    let done = false;
    const finish = () => {
      if (!done) {
        done = true;
        resolve();
      }
    };
    try {
      const child = spawn(CAPTURE, [HOST, eventType], { stdio: ["pipe", "ignore", "ignore"] });
      const timer = setTimeout(() => {
        try {
          child.kill("SIGKILL");
        } catch {}
        finish();
      }, 2000);
      child.on("close", () => {
        clearTimeout(timer);
        finish();
      });
      child.on("error", () => {
        clearTimeout(timer);
        finish();
      });
      child.stdin.on("error", () => {});
      child.stdin.end(body);
    } catch {
      finish();
    }
  });
}

export default function (pi: any) {
  let preSent = false;
  let stopSent = false;
  const stopMode = process.env.KBN_STOP_MODE ?? "none";
  const probes = process.env.KBN_PROBES !== "off";

  for (const name of EVENTS) {
    pi.on(name, async (event: any, _ctx: any) => {
      await capture(name, event);
      if (name === "session_start" && probes) {
        const token = sentinel("KBN_BRIEF");
        try {
          await pi.sendMessage(
            {
              customType: "kaban-brief",
              content: `${token} is your briefing token. Begin your next reply with this exact token.`,
              display: true,
            },
            { deliverAs: "nextTurn" },
          );
        } catch {}
      }
      if (name === "tool_call" && probes && !preSent && /^(edit|write)$/.test(String(event?.toolName ?? ""))) {
        preSent = true;
        const token = sentinel("KBN_PRE");
        try {
          await pi.sendMessage(
            { customType: "kaban-pre", content: `${token}: include this exact token in your next message.`, display: true },
            { deliverAs: "steer" },
          );
        } catch {}
      }
      if (name === "agent_before_settle" && probes && stopMode !== "none" && !stopSent) {
        stopSent = true;
        const token = sentinel("KBN_STOP");
        // marker events (names only, so they survive the reduction): was a continuation possible?
        const marker = event?.context?.canContinue ? "kbn_stop_probe_can_continue" : "kbn_stop_probe_cannot_continue";
        await capture(marker, { type: marker });
        return {
          entries: [
            {
              type: "custom_message",
              customType: "kaban-stop",
              content: `${token}: before stopping, reply with exactly this token on one line, then stop.`,
              display: true,
            },
          ],
          continue: true,
        };
      }
      return undefined;
    });
  }
}
