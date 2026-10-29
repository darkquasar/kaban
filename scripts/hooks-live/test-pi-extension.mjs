// Invented-data behaviour test of hooks/pi/kaban-hooks.ts with a fake pi API (no pi, no model).
// Runs inside the kaban-hooks-live image through test-pi-extension.sh (OWN-7); the extension is
// loaded with jiti, pi's own loader. KABAN_BIN points at a recording shim that appends every
// envelope the extension sends to $KBN_ENVELOPES (tmpfs) before passing it to the real binary.
//
// Asserts (R4-4 intake allowlist, M4-7): each envelope carries exactly the allowlisted keys for its
// event and nothing else; `type`, `reason`, `toolCallId`, `outcome`, `continue`, `context` and
// `details` never appear; the secret planted in `input.content` never appears; and the W2-D2
// behaviour holds (briefing nextTurn, pre context steer, Failure recorded, one continuation,
// second settle silent, evidence appended; C5.3/M5-20: session_shutdown returns in under 1 s and
// the detached `kaban sync` writes .kaban/adr/INDEX.md with the seeded Decision afterwards). The
// envelope sequence is asserted exactly (RL-3):
// session_start, the edit's tool_call, the failing tool_execution_end, one agent_before_settle
// and the success tool_execution_end carrying `isError: false` (the evidence the wrapper then
// recalls); a bash tool_call and the second settle send nothing. Prints one reduced JSON line
// (booleans, counts, the extension's own key names) and exits 1 on any failed assertion.
import { existsSync, readFileSync } from "node:fs";

const { createJiti } = await import(process.env.JITI);
const jiti = createJiti(import.meta.url);
const mod = await jiti.import(process.env.EXT);

const handlers = new Map();
const sent = [];
const pi = { on: (n, h) => handlers.set(n, h), sendMessage: async (m, o) => { sent.push({ m, o }); } };
mod.default(pi);
const ctx = { cwd: process.env.WORK, sessionManager: { getSessionId: () => "sess-ext-INVENTED" } };
const fire = (n, e) => handlers.get(n)(e, ctx);
const SECRET = "AKIAQWERTYUIOPASDFGH never forwarded";
const r = {};

await fire("session_start", { type: "session_start", reason: "startup" });
r.brief = sent.find((s) => s.m.customType === "kaban-brief");
await fire("tool_call", { type: "tool_call", toolCallId: "w1", toolName: "write", input: { path: "notes.txt", content: SECRET } });
r.pre = sent.find((s) => s.m.customType === "kaban-pre");
await fire("tool_call", { type: "tool_call", toolCallId: "b1", toolName: "bash", input: { command: "false" } });
await fire("tool_execution_end", {
  type: "tool_execution_end", toolCallId: "b1", toolName: "bash", isError: true,
  result: { content: [{ type: "text", text: "Command exited with code 1" }, { type: "image", data: "INVENTED-IMAGE" }], details: { exitCode: 1 } },
});
r.settle1 = await fire("agent_before_settle", { type: "agent_before_settle", outcome: "completed", continue: false, context: { canContinue: false, llmMessages: [SECRET] } });
r.settle2 = await fire("agent_before_settle", { type: "agent_before_settle", outcome: "completed", continue: false, context: { canContinue: false } });
await fire("tool_execution_start", { toolCallId: "b2", toolName: "bash", args: { command: "false" } });
await fire("tool_execution_end", { type: "tool_execution_end", toolCallId: "b2", toolName: "bash", isError: false, result: { content: [{ type: "text", text: "" }], details: {} } });
// session_shutdown (M5-20): the handler returns at once (the detached `kaban sync` is never
// awaited) and the flush shows up in the scratch repo afterwards (.kaban/adr/INDEX.md from the
// seeded Decision); it sends no envelope (no stdin), so the envelope sequence is unchanged
const shutdownStart = Date.now();
await fire("session_shutdown", { type: "session_shutdown", reason: "quit" });
const shutdownMs = Date.now() - shutdownStart;
const digestPath = `${process.env.WORK}/.kaban/adr/INDEX.md`;
let syncSeen = false;
for (let i = 0; i < 100 && !syncSeen; i++) {
  syncSeen = existsSync(digestPath);
  if (!syncSeen) await new Promise((r) => setTimeout(r, 100));
}

// the envelopes the recording shim saw, one JSON object per line
const envelopes = readFileSync(process.env.KBN_ENVELOPES, "utf8").split("\n").filter((l) => l.trim()).map((l) => JSON.parse(l));
const COMMON = ["session_id", "cwd", "hook_event_name"];
const ALLOWED = {
  session_start: COMMON,
  tool_call: [...COMMON, "toolName", "input"],
  tool_execution_end: [...COMMON, "toolName", "isError", "input", "result"],
  agent_before_settle: COMMON,
};
const NEVER = ["type", "reason", "toolCallId", "outcome", "continue", "context", "details"];
const keyReport = envelopes.map((e) => {
  const allowed = ALLOWED[e.hook_event_name] ?? [];
  const keys = Object.keys(e);
  const extra = keys.filter((k) => !allowed.includes(k));
  const missing = allowed.filter((k) => !keys.includes(k));
  const inputExtra = e.input ? Object.keys(e.input).filter((k) => !["path", "command"].includes(k)) : [];
  const resultExtra = e.result ? Object.keys(e.result).filter((k) => k !== "content") : [];
  const itemExtra = Array.isArray(e.result?.content) ? e.result.content.flatMap((i) => Object.keys(i).filter((k) => !["type", "text"].includes(k))) : [];
  return { event: e.hook_event_name, extra, missing, inputExtra, resultExtra, itemExtra };
});
const raw = readFileSync(process.env.KBN_ENVELOPES, "utf8");
const EXPECTED_EVENTS = ["session_start", "tool_call", "tool_execution_end", "agent_before_settle", "tool_execution_end"];
const last = envelopes[envelopes.length - 1];
const summary = {
  envelopes: envelopes.length,
  events: envelopes.map((e) => e.hook_event_name),
  events_exact: JSON.stringify(envelopes.map((e) => e.hook_event_name)) === JSON.stringify(EXPECTED_EVENTS),
  evidence_sent: last?.hook_event_name === "tool_execution_end" && last?.isError === false && last?.toolName === "bash",
  envelope_keys_exact: keyReport.every((k) => k.extra.length === 0 && k.missing.length === 0 && k.inputExtra.length === 0 && k.resultExtra.length === 0 && k.itemExtra.length === 0),
  extra_keys: [...new Set(keyReport.flatMap((k) => [...k.extra, ...k.inputExtra, ...k.resultExtra, ...k.itemExtra]))],
  missing_keys: [...new Set(keyReport.flatMap((k) => k.missing))],
  never_forwarded_absent: NEVER.every((k) => !envelopes.some((e) => k in e)),
  secret_absent: !raw.includes("AKIAQWERTYUIOPASDFGH"),
  image_item_absent: !raw.includes("INVENTED-IMAGE"),
  brief_deliverAs: r.brief?.o?.deliverAs,
  brief_has_sentinel: (r.brief?.m?.content ?? "").includes(process.env.SENT),
  pre_deliverAs: r.pre?.o?.deliverAs,
  pre_has_sentinel: (r.pre?.m?.content ?? "").includes(process.env.SENT),
  settle1_continue: r.settle1?.continue === true,
  settle1_entry: r.settle1?.entries?.[0]?.type,
  settle1_mentions_failure: /Failure [0-9a-f-]{36}/.test(r.settle1?.entries?.[0]?.content ?? ""),
  settle2_silent: r.settle2 === undefined,
  messages_sent: sent.length,
  shutdown_returned_ms: shutdownMs,
  shutdown_sync_ran: syncSeen,
  shutdown_digest_has_sentinel: syncSeen && readFileSync(digestPath, "utf8").includes(process.env.SENT),
};
const ok =
  summary.envelope_keys_exact && summary.never_forwarded_absent && summary.secret_absent && summary.image_item_absent &&
  summary.brief_deliverAs === "nextTurn" && summary.brief_has_sentinel && summary.pre_deliverAs === "steer" && summary.pre_has_sentinel &&
  summary.settle1_continue && summary.settle1_entry === "custom_message" && summary.settle1_mentions_failure && summary.settle2_silent &&
  summary.envelopes === 5 && summary.events_exact && summary.evidence_sent &&
  summary.shutdown_returned_ms < 1000 && summary.shutdown_sync_ran && summary.shutdown_digest_has_sentinel;
console.log(JSON.stringify({ ...summary, ok }));
process.exit(ok ? 0 : 1);
