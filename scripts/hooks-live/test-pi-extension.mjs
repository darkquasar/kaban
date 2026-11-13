// Invented-data behaviour test of hooks/pi/kaban-hooks.ts with a fake pi API (no pi, no model).
// Runs inside the kaban-hooks-live image through test-pi-extension.sh (OWN-7); the extension is
// loaded with jiti, pi's own loader. KABAN_BIN points at a recording shim that appends every
// envelope the extension sends to $KBN_ENVELOPES (tmpfs) before passing it to the real binary.
//
// Asserts (R4-4 intake allowlist, M4-7): each envelope carries exactly the allowlisted keys for its
// event and nothing else; `type`, `reason`, `toolCallId`, `outcome`, `continue`, `context` and
// `details` never appear; the secret planted in `input.content` and the image item never appear;
// and the hook behaviour without governance holds: the extension registers exactly the five
// handlers below (no `session_shutdown`, so no sync is started at shutdown); the first
// session_start briefs nothing (the seeded Decision is never briefed); the edit's pre-tool-use is
// recorded and injects nothing; the failing bash records a Failure; one continuation names it and
// the second settle is silent; a session_start in a second session briefs the pending Failure
// (nextTurn) and still not the Decision; evidence is appended. The envelope sequence is asserted
// exactly (RL-3): session_start, the edit's tool_call, the failing tool_execution_end, one
// agent_before_settle, the second session's session_start and the success tool_execution_end
// carrying `isError: false` (the evidence the wrapper then recalls); a bash tool_call and the
// second settle send nothing. Only registered handlers are dispatched. Prints one reduced JSON
// line (booleans, counts, the extension's own key names) and exits 1 on any failed assertion.
import { readFileSync } from "node:fs";

const { createJiti } = await import(process.env.JITI);
const jiti = createJiti(import.meta.url);
const mod = await jiti.import(process.env.EXT);

const handlers = new Map();
const sent = [];
const pi = { on: (n, h) => handlers.set(n, h), sendMessage: async (m, o) => { sent.push({ m, o }); } };
mod.default(pi);
const ctx = { cwd: process.env.WORK, sessionManager: { getSessionId: () => "sess-ext-INVENTED" } };
const ctx2 = { cwd: process.env.WORK, sessionManager: { getSessionId: () => "sess-ext-INVENTED-2" } };
const REGISTERED = ["session_start", "tool_call", "tool_execution_start", "tool_execution_end", "agent_before_settle"];
const registered = [...handlers.keys()];
// only events the extension registered are dispatched; the registered set is asserted below
const fire = (n, e, c = ctx) => (handlers.has(n) ? handlers.get(n)(e, c) : undefined);
const SECRET = "AKIAQWERTYUIOPASDFGH never forwarded";
const r = {};

await fire("session_start", { type: "session_start", reason: "startup" });
r.brief1 = sent.find((s) => s.m.customType === "kaban-brief");
await fire("tool_call", { type: "tool_call", toolCallId: "w1", toolName: "write", input: { path: "notes.txt", content: SECRET } });
r.pre = sent.find((s) => s.m.customType === "kaban-pre");
await fire("tool_call", { type: "tool_call", toolCallId: "b1", toolName: "bash", input: { command: "false" } });
await fire("tool_execution_end", {
  type: "tool_execution_end", toolCallId: "b1", toolName: "bash", isError: true,
  result: { content: [{ type: "text", text: "Command exited with code 1" }, { type: "image", data: "INVENTED-IMAGE" }], details: { exitCode: 1 } },
});
r.settle1 = await fire("agent_before_settle", { type: "agent_before_settle", outcome: "completed", continue: false, context: { canContinue: false, llmMessages: [SECRET] } });
r.settle2 = await fire("agent_before_settle", { type: "agent_before_settle", outcome: "completed", continue: false, context: { canContinue: false } });
// a second session starts while the Failure is pending: the briefing lists it (nextTurn)
await fire("session_start", { type: "session_start", reason: "startup" }, ctx2);
r.brief2 = sent.find((s) => s.m.customType === "kaban-brief");
await fire("tool_execution_start", { toolCallId: "b2", toolName: "bash", args: { command: "false" } });
await fire("tool_execution_end", { type: "tool_execution_end", toolCallId: "b2", toolName: "bash", isError: false, result: { content: [{ type: "text", text: "" }], details: {} } });

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
const EXPECTED_EVENTS = ["session_start", "tool_call", "tool_execution_end", "agent_before_settle", "session_start", "tool_execution_end"];
const last = envelopes[envelopes.length - 1];
const failureId = (r.settle1?.entries?.[0]?.content ?? "").match(/Failure ([0-9a-f-]{36})/)?.[1];
const summary = {
  registered,
  registered_exact: JSON.stringify([...registered].sort()) === JSON.stringify([...REGISTERED].sort()),
  session_shutdown_absent: !handlers.has("session_shutdown"),
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
  brief1_silent: r.brief1 === undefined,
  pre_silent: r.pre === undefined,
  brief2_deliverAs: r.brief2?.o?.deliverAs,
  brief2_names_failure: failureId !== undefined && (r.brief2?.m?.content ?? "").includes(failureId),
  brief_no_decision: !sent.some((s) => (s.m?.content ?? "").includes(process.env.SENT)),
  settle1_continue: r.settle1?.continue === true,
  settle1_entry: r.settle1?.entries?.[0]?.type,
  settle1_mentions_failure: failureId !== undefined,
  settle2_silent: r.settle2 === undefined,
  messages_sent: sent.length,
};
const ok =
  summary.registered_exact && summary.session_shutdown_absent &&
  summary.envelope_keys_exact && summary.never_forwarded_absent && summary.secret_absent && summary.image_item_absent &&
  summary.brief1_silent && summary.pre_silent &&
  summary.brief2_deliverAs === "nextTurn" && summary.brief2_names_failure && summary.brief_no_decision &&
  summary.settle1_continue && summary.settle1_entry === "custom_message" && summary.settle1_mentions_failure && summary.settle2_silent &&
  summary.messages_sent === 1 && summary.envelopes === 6 && summary.events_exact && summary.evidence_sent;
console.log(JSON.stringify({ ...summary, ok }));
process.exit(ok ? 0 : 1);
