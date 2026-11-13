#!/usr/bin/env python3
"""MCP stdio black-box test: protocol flow, tool behaviour, the code-graph surface without a
snapshot, the denylist canary and redaction, purge (S5), a path-scoped Decision, the docs-ingest
tools and file modes, then the CLI and hook sections over the same database (`recall`, `doctor`,
a clap error, `kaban hook` failure capture, briefing and refusal; core-plan C4.6), the docs-ingest CLI
section, the engine E1 ontology verbs and `schema_describe`, the E2 human edge and the E3 strace
envelope, each in a temporary git repository next to the db, so the checkout's own `.kaban/` is
never written. Deterministic and offline: the subprocesses besides the server are `git` (the
server's currency checks, the smoke's own calls in the temporary repositories) and `strace`.
Exit code 0 only if every assertion holds. Usage: mcp_smoke.py <binary> <db> <policy.yaml>
The smoke binds one explicit workspace across its MCP servers and temporary-repository CLI
processes. KABAN_WORKSPACE overrides the default `kaban-smoke` workspace.
The server's KABAN_HOME is set to <db>.home so the key file never touches ~/.kaban.
When a server closes stdout without a reply, the smoke stops with the tail of its stderr.
Docker mode: set MCP_SMOKE_DOCKER_IMAGE=<image>; <binary> is then ignored and the image runs
via `docker run -i` with the db directory and the policy file bind-mounted at the same absolute
paths, so every MCP assertion (including the on-disk canary and mode checks) runs unchanged; the
key-file, CLI, hook and later sections are skipped with a printed note, because the CLI and the
hooks bind the workspace from the process's own directory, which inside the container is not this
repository."""
import json, subprocess, sys, time, os, glob
from mcp_catalog import load_tool_names, tool_names_match

expected_tool_names = load_tool_names()

bin_, db, policy = sys.argv[1:4]
for f in glob.glob(db + "*"):
    os.remove(f)
# the audit key lives under $KABAN_HOME/keys (core-spec S3); keep the smoke's keys next to its db
kaban_home = os.path.abspath(db) + ".home"
import shutil; shutil.rmtree(kaban_home, ignore_errors=True)
# RUST_LOG=trace: the server must never log argument values, whatever the filter (S1; m2 Me7)
env = dict(os.environ, KABAN_HOME=kaban_home, RUST_LOG="trace",
           KABAN_WORKSPACE=os.environ.get("KABAN_WORKSPACE", "kaban-smoke"))
docker_image = os.environ.get("MCP_SMOKE_DOCKER_IMAGE")
if docker_image:
    db = os.path.abspath(db); policy = os.path.abspath(policy)
    os.makedirs(os.path.dirname(db), exist_ok=True)
    cmd = ["docker", "run", "--rm", "-i", "--user", f"{os.getuid()}:{os.getgid()}", "-e", f"KABAN_HOME={kaban_home}",
           "-e", f"KABAN_WORKSPACE={env['KABAN_WORKSPACE']}",
           "-v", f"{os.path.dirname(db)}:{os.path.dirname(db)}", "-v", f"{policy}:{policy}:ro", docker_image]
else:
    cmd = [bin_]
CANARY = "Sup3rS3cretValue!"          # reject-rule value: must never appear anywhere
REDACT_CANARY = "db1.corp.acme.internal"  # redact-rule value: must not be persisted
PURGE_CANARY = "PurgeCanaryZq8vK3"        # clean value: stored, then purged; must leave no bytes behind (S5)
fails = []
def check(cond, msg):
    if not cond:
        fails.append(msg); print("FAIL:", msg)
    else:
        print("ok:", msg)

t0 = time.time()
p = subprocess.Popen(cmd + ["--db", db, "--policy", policy],
                     stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
raw_out = []
i = 0
def exchange(proc, msg, notify=False):
    """Write one JSON-RPC message and read its reply line (none for a notification). A broken
    pipe or an empty line means the server is gone: print the tail of its stderr and stop."""
    try:
        proc.stdin.write(json.dumps(msg) + "\n"); proc.stdin.flush()
        line = None if notify else proc.stdout.readline()
    except BrokenPipeError:
        line = ""
    if line is None or line.strip():
        return line
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill(); proc.wait()
    tail = "\n".join(proc.stderr.read().splitlines()[-20:])
    print(f"FAIL: the server closed stdout without a reply (exit {proc.returncode}); stderr tail:\n{tail}")
    sys.exit(1)
def rpc(method, params=None, notify=False):
    global i
    msg = {"jsonrpc": "2.0", "method": method}
    if params is not None: msg["params"] = params
    if not notify:
        i += 1; msg["id"] = i
    line = exchange(p, msg, notify)
    if notify: return None
    raw_out.append(line)
    return json.loads(line)
def call(name, args):
    r = rpc("tools/call", {"name": name, "arguments": args})
    res = r.get("result", r)
    txt = res["content"][0]["text"] if res.get("content") else json.dumps(res)
    children_ok()  # v4 CG10.3 serve_children_are_git_only, defined in the code-graph section
    return bool(res.get("isError")), txt

r = rpc("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "smoke", "version": "0"}})
init_ms = (time.time() - t0) * 1000
check(r["result"]["serverInfo"]["name"] == "kaban", f"initialize ok in {init_ms:.1f} ms, proto {r['result']['protocolVersion']}")
check(init_ms < 500, "initialize under 500 ms")
rpc("notifications/initialized", notify=True)
tools = rpc("tools/list")["result"]["tools"]
names = sorted(t["name"] for t in tools)
check(tool_names_match(names, expected_tool_names), f"tools: {names}")
ann = {t["name"]: t.get("annotations", {}) for t in tools}
check(ann["recall"].get("readOnlyHint") is True and ann["forget"].get("destructiveHint") is True and ann["remember"].get("openWorldHint") is False, f"annotations present: recall={ann['recall']}")
check(all(isinstance(t.get("outputSchema"), dict) for t in tools), "outputSchema declared on every tool (C3.7)")
# MCP 2025-06-18 and 2025-11-25 restrict outputSchema to a root `type: object` (review R3-8)
no_object_root = sorted(t["name"] for t in tools if not isinstance(t.get("outputSchema"), dict) or t["outputSchema"].get("type") != "object")
check(no_object_root == [], f"outputSchema.type == \"object\" on every tool (R3-8); tools without it: {no_object_root}")
res = rpc("resources/list")["result"]["resources"]
check(sorted(x["uri"] for x in res) == ["kaban://code-index", "kaban://documents", "kaban://schema", "kaban://stats"], "resources listed")
schema = json.loads(rpc("resources/read", {"uri": "kaban://schema"})["result"]["contents"][0]["text"])
check("Failure" in [l["name"] for l in schema["labels"]], "schema resource readable")

# --- code-graph section (v4 CG10.3) ---
# stage1-plan CG10.3 / spec §8.4, G1-S3: the code-graph surface of the server without a snapshot
# (the smoke's KABAN_HOME has no codegraph key, so every code tool answers the fixed
# `no_snapshot` sentence) and the operational `serve_children_are_git_only` check. Every message
# is prefixed `cg:`. The one edit outside this section is the `children_ok()` line in `call()`.
NO_SNAPSHOT = "no_snapshot: no code-graph snapshot for this repository and commit; run `kaban-index index`"
CODE_TOOLS = ["code_context", "code_definition", "code_implementers", "code_members", "code_neighbours", "code_references", "code_tests_for"]
# (a) a subset check beside the exact list above: the seven `code_*` tools are listed,
# `code_impact` is not in 3.0.0 (OWN-68 SC-09)
cg_missing = [n for n in CODE_TOOLS if n not in names]
check(cg_missing == [] and "code_impact" not in names, f"cg: tools/list has the seven code_* tools, no code_impact (missing: {cg_missing})")
# (d) serve_children_are_git_only (G1-S3): while the smoke drives the server, a daemon thread
# reads /proc/<pid>/task/*/children every 100 ms and `call()` reads them after every tool call;
# each child's /proc/<child>/comm must be `git`; the first other comm fails the smoke by name.
# Darwin samples the native process table; Docker observes the client, not the server.
import threading
cg_children = {"samples": 0, "comms": set(), "bad": [], "enabled": False}
def children_comms(pid):
    comms = []
    if sys.platform == "darwin":
        # ucomm is the kernel's short command name, matching Linux /proc/comm.
        # Darwin's executable-path field comm may print an exiting git as (git).
        result = subprocess.run(["/bin/ps", "-ww", "-axo", "ppid=,ucomm="],
                                capture_output=True, text=True, timeout=2, check=True)
        for line in result.stdout.splitlines():
            parent, comm = line.strip().split(None, 1)
            if int(parent) == pid:
                comms.append(comm.strip())
        return comms
    for f in glob.glob(f"/proc/{pid}/task/*/children"):
        try:
            kids = open(f).read().split()
        except OSError:
            continue
        for kid in kids:
            try:
                comms.append(open(f"/proc/{kid}/comm").read().strip())
            except OSError:
                pass  # the child exited between the two reads
    return comms
def children_ok():
    """Sample the server's children once; the first non-git comm fails the smoke by name (once)."""
    if not cg_children["enabled"]:
        return
    cg_children["samples"] += 1
    try:
        comms = children_comms(p.pid)
    except (OSError, ValueError, subprocess.SubprocessError) as error:
        if "observation failed" not in cg_children["bad"]:
            cg_children["bad"].append("observation failed")
            check(False, f"cg: child observation failed: {type(error).__name__}")
        return
    for comm in comms:
        cg_children["comms"].add(comm)
        if comm != "git" and comm not in cg_children["bad"]:
            cg_children["bad"].append(comm)
            check(False, f"cg: serve_children_are_git_only: child comm {comm!r} is not git (G1-S3)")
def children_sampler():
    while p.poll() is None:
        children_ok()
        time.sleep(0.1)
if docker_image:
    print("not-run: serve_children (docker: /proc sees the docker client)")
elif not (sys.platform.startswith("linux") or sys.platform == "darwin"):
    print("not-run: serve_children (unsupported host)")
else:
    cg_children["enabled"] = True
    cg_sampler = threading.Thread(target=children_sampler, daemon=True)
    cg_sampler.start()
    time.sleep(0.25); children_ok()
# (b) one `code_definition` call without a snapshot: `isError`, the fixed text, and the target is
# echoed nowhere (not in the reply, not in any stdout line so far)
cg_marker = "zq8v-cg-no-such-file"
e, t = call("code_definition", {"target": {"path": f"src/{cg_marker}.go"}})
check(e and t == NO_SNAPSHOT, f"cg: code_definition without a snapshot is the fixed no_snapshot text: {t!r}")
check(cg_marker not in t and not any(cg_marker in l for l in raw_out), "cg: the code_definition target is echoed nowhere")
# (c) the `kaban://code-index` resource reads as the same sentence (content, not an error)
rr = rpc("resources/read", {"uri": "kaban://code-index"})
cg_contents = rr.get("result", {}).get("contents", [{}])[0]
check(cg_contents.get("uri") == "kaban://code-index" and cg_contents.get("text") == NO_SNAPSHOT, f"cg: kaban://code-index reads as the no_snapshot sentence: {cg_contents.get('text')!r}")
if cg_children["enabled"]:
    check(not cg_children["bad"], f"cg: serve_children_are_git_only: sampler running, {cg_children['samples']} samples so far, children seen: {sorted(cg_children['comms'])} (the thread and call() keep sampling until the server exits)")
# --- end code-graph section ---

e, t = call("remember", {"kind": "Failure", "text": "cargo build failed: linker could not find libssl on arm64 musl", "tags": ["rust", "musl"], "confidence": 0.9})
check(not e and json.loads(t)["status"] == "created", "remember Failure"); fid = json.loads(t)["id"]
e, t = call("remember", {"kind": "Improvement", "text": "install pkg-config and libssl-dev, or use rustls to avoid the system OpenSSL dependency", "links": [{"rel": "FIXED_BY", "from": fid}]})
check(not e and json.loads(t)["links"] == 1, "remember Improvement with atomic link"); fixid = json.loads(t)["id"]
e, t = call("remember", {"kind": "Fact", "text": f"deploy password = {CANARY} for staging"})
check(e and "REJECTED" in t and CANARY not in t and "Su…" not in t, "secret in text rejected, not echoed")
e, t = call("remember", {"kind": "Fact", "text": "harmless", "props": {"db": {"password": CANARY}}})
check(e and CANARY not in t, "secret nested in props rejected")
e, t = call("remember", {"kind": 5, "text": f"deploy password = {CANARY}"})
check(e and "generic-credential-assignment" in t and CANARY not in t and "Sup3r" not in t, "malformed typed args with a secret rejected by the pre-parse scan, rule ids only (Me7)")
e, t = call("forget", {"id": "x", "mode": "n0t-a-m0de-Zq8v"})
check(e and "Zq8v" not in t, "typed-argument error does not echo the value")
e, t = call("remember", {"kind": "Fact", "text": "harmless", "tags": [f"pw={CANARY}"]})
check(e, "secret in tag rejected")
e, t = call("remember", {"kind": "Fact", "text": "see Project", "summary": "Nightjar launch"})
check(e, "phrase split across fields rejected")
e, t = call("link", {"from": fid, "to": fixid, "rel": "RELATES_TO", "props": {"note": "old pw hunter2"}})
check(not e and json.loads(t)["redactions"] == ["props:old-password"], "link props redacted (C1)")
e, t = call("schema_extend", {"kind": "rel", "name": "BLOCKED_BY", "description": f"blocked by {REDACT_CANARY}"})
check(e, "schema_extend with redactable value refused (C1)")
e, t = call("remember", {"kind": "Fact", "text": f"staging db host is {REDACT_CANARY} and old pw hunter2"})
check(not e and sorted(json.loads(t)["redactions"]) == ["text:internal-hostname", "text:old-password"], "redact rules applied on remember"); redid = json.loads(t)["id"]
# engine E1.8 (EN-F5, EN-F49): an undeclared label is placed as an `x:` candidate under the shipped `review`
# policy and stays hidden from recall until promoted; an unknown rel keeps today's rejection
e, t = call("remember", {"kind": "Rumour", "text": "an undeclared label about the zebrafish ledger"})
check(not e and json.loads(t)["kind"] == "x:Rumour", "undeclared label placed as the x:Rumour candidate (E1.8)")
e, t = call("recall", {"query": "zebrafish ledger"})
check(not e and json.loads(t)["count"] == 0, "the candidate is hidden from recall by default (EN-F49)")
e, t = call("link", {"from": fid, "to": fixid, "rel": "RUMOURED_BY"})
check(e and "unknown_rel" in t and "RUMOURED_BY" not in t, "unknown rel rejected as its fixed class, the name not echoed (RW3-6; the schema_extend hint went with the payload)")
e, t = call("schema_extend", {"kind": "rel", "name": "BLOCKED_BY", "description": "work blocked by", "from_kinds": ["Failure"]})
check(not e, "schema_extend BLOCKED_BY")
e, t = call("schema_extend", {"kind": "label", "name": "Fact", "description": "override"})
check(e and "builtin" in t, "builtin type cannot be changed (Me3)")
e, t = call("link", {"from": fixid, "to": fid, "rel": "BLOCKED_BY"})
check(e and "not allowed" in t, "endpoint constraint enforced")
t1 = time.time(); e, t = call("recall", {"query": "libssl linker", "hops": 1}); dt = (time.time() - t1) * 1000
r = json.loads(t)
check(r["count"] >= 2 and r["results"][0]["id"] == fid and any(x["id"] == fixid for x in r["results"]), f"recall finds failure + linked fix ({dt:.1f} ms round trip)")
sc = rpc("tools/call", {"name": "recall", "arguments": {"query": "libssl linker", "hops": 1}})["result"]
check(isinstance(sc.get("structuredContent"), dict) and sc["structuredContent"] == json.loads(sc["content"][0]["text"]), "recall carries structuredContent equal to its text content (C3.7)")
e, t = call("recall", {"query": "staging host", "full_text": True})
r = json.loads(t)
check(any(x["id"] == redid and "[REDACTED:internal-hostname]" in x.get("text", "") for x in r["results"]), "redacted text comes back masked")
e, t = call("traverse", {"start_ids": [fid], "max_depth": 1})
r = json.loads(t)
check(len(r["edges"]) == 2 and all(x["from"] in (fid, fixid) for x in r["edges"]), "traverse returns edges among returned nodes")
# redact-class values in identifier and query arguments never reach a handler or the reply raw (R1-3)
e, t = call("forget", {"id": "hunter2"})
check(e and "hunter2" not in t.lower() and "[REDACTED:old-password]" in t, "forget with a redact-class id reports the masked id only (R1-3)")
e, t = call("recall", {"query": "hunter2 staging"})
check(not e and "hunter2" not in t.lower(), "recall with a redact-class query term returns no raw term (R1-3)")
e, t = call("traverse", {"start_ids": ["hunter2"]})
check(e and "hunter2" not in t.lower(), "traverse with a redact-class start id does not echo it (R1-3)")
# dedup retry attaches links (m2 N5) and a self-link is refused (m2 N11)
e, t = call("remember", {"kind": "Improvement", "text": "install pkg-config and libssl-dev, or use rustls to avoid the system OpenSSL dependency", "summary": "retry summary", "links": [{"rel": "RELATES_TO", "to": fid}]})
r = json.loads(t)
check(not e and r["status"] == "existing" and r["id"] == fixid and r["links_created"] == 1 and r["links_ignored"] is False and "summary" in r["fields_ignored"], f"dedup retry attaches links and reports ignored fields: {t}")
e, t = call("link", {"from": fid, "to": fid, "rel": "SUPERSEDES"})
check(e and "itself" in t, "self-link refused")
# M3 (C3.8): a Decision with a path_scope is stored. Supersede via `link SUPERSEDES` is not
# exercised: the store moves only an accepted Decision to superseded, and no public surface
# accepts one since the governance removal
SCOPE = ["src/store/**"]
e, t = call("remember", {"kind": "Decision", "text": "store writes go through one connection per process", "summary": "one writer connection", "props": {"rationale": "SQLite allows a single writer"}, "path_scope": SCOPE})
check(not e and json.loads(t)["status"] == "created", "remember Decision with path_scope"); did = json.loads(t)["id"] if not e else ""
# a clean note is stored, purged, and must leave no bytes in db/-wal/-shm (S5)
e, t = call("remember", {"kind": "Fact", "text": f"rotate {PURGE_CANARY} before the release"})
check(not e and json.loads(t)["status"] == "created", "purge canary stored"); pcid = json.loads(t)["id"]
e, t = call("forget", {"id": pcid, "mode": "purge"})
check(not e, "purge canary purged")
e, t = call("forget", {"id": fixid, "mode": "purge"})
check(not e, "purge")
e, t = call("recall", {"query": "pkg-config rustls"})
check(json.loads(t)["count"] == 0, "purged node gone from FTS")
e, t = call("stats", {})
r = json.loads(t)
check(not e and r["policy"]["scan_on_read"] is True, "stats")
check(r["scope"]["workspace"] == env["KABAN_WORKSPACE"] and r["scope"]["db_bytes"] == "global", f"stats scoped to the bound workspace, file bytes labelled global (R1-5): {r.get('scope')}")
e, t = call("stats", {"workspace_id": "*"})
check(not e and json.loads(t)["scope"]["workspace"] == "*" and json.loads(t)["nodes"] >= r["nodes"], "stats with workspace_id \"*\" counts every workspace (R1-5)")

# docs-ingest DI1.6 over MCP: `index_docs` dry run on the committed Markdown fixtures of this
# repository (the server's workspace root; nothing is written), `index_status` and the
# `kaban://documents` resource on the empty index; the writing half of the section runs through
# the CLI in the temporary repository
FIXTURES = "crates/kaban-ingest/tests/fixtures/md"
e, t = call("index_docs", {"paths": [FIXTURES], "dry_run": True})
v = json.loads(t) if not e else {}
check(not e and v.get("dry_run") is True and v.get("documents") == 4 and v.get("outbox_rows") == 0, f"mcp index_docs dry_run counts the four fixtures without writing ({v.get('documents')} documents, {v.get('outbox_rows')} outbox rows)")
check(not e and v.get("skipped_forgotten") == 0 and v.get("sections_tombstoned") == 0, f"mcp index_docs reports skipped_forgotten and sections_tombstoned (DI1.8b) ({v.get('skipped_forgotten')}, {v.get('sections_tombstoned')})")
e, t = call("index_docs", {"paths": [FIXTURES], "dry_run": True, "include_forgotten": True})
v = json.loads(t) if not e else {}
check(not e and v.get("documents") == 4, f"mcp index_docs accepts include_forgotten ({v.get('documents')} documents)")
e, t = call("index_status", {})
st = json.loads(t) if not e else {}
check(not e and st.get("documents_by_status", {}) == {} and st.get("chunks") == 0 and st.get("pipeline_version") == 1 and len(st.get("extractors", [])) >= 1 and "last_run" not in st, f"mcp index_status on the empty index reports the pipeline version and the extractors ({st.get('extractors')})")
# docs-ingest DI2.2: the PDF capability comes from the binary, not from an env var. `pdf` is opt-in
# for 3.0.0 (OWN-70), so the default build `ci.sh` smokes takes the `unavailable` branch below and
# a `--features kaban-ingest/pdf` build takes the `pdf_oxide` branch; every PDF check names its branch.
HAS_PDF = "pdf_oxide@0.3.78" in st.get("extractors", [])
PDF_BRANCH = "pdf_oxide" if HAS_PDF else "unavailable"
NO_EXTRACTOR = "no extractor for this format"
print(f"pdf smoke branch: {PDF_BRANCH}")
rr = rpc("resources/read", {"uri": "kaban://documents?limit=2"})
body = json.loads(rr["result"]["contents"][0]["text"]) if "result" in rr else {}
check(body.get("documents", []) == [] and body.get("more") is False and body.get("limit") == 2 and body.get("status", {}).get("pipeline_version") == 1, f"mcp kaban://documents serves the status and an empty page with the requested limit ({body.get('limit')}, more={body.get('more')})")

# file modes while the server still holds the database (SQLite removes -wal/-shm on close)
live_modes = {os.path.basename(f): oct(os.stat(f).st_mode & 0o777) for f in glob.glob(db + "*") if os.path.isfile(f)}
p.stdin.close(); p.wait(timeout=5)
if cg_children["enabled"]:
    cg_sampler.join(timeout=3)
    check(not cg_sampler.is_alive() and not cg_children["bad"],
          f"cg: child observation completed ({cg_children['samples']} samples; {sorted(cg_children['comms'])})")
err = p.stderr.read()
check(all(json.loads(l).get("jsonrpc") == "2.0" for l in raw_out if l.strip()), "stdout purity: every line is JSON-RPC")
files = [f for f in glob.glob(db + "*") if os.path.isfile(f)]
blob = b"".join(open(f, "rb").read() for f in files)
check(CANARY.encode() not in blob, "canary absent from db/wal/shm bytes")
check(REDACT_CANARY.encode() not in blob, "redact canary absent from db bytes")
check(PURGE_CANARY.lower().encode() not in blob.lower(), "purged note absent from db/-wal/-shm bytes (S5)")
check(live_modes and all(m == "0o600" for m in live_modes.values()) and any(k.endswith("-wal") for k in live_modes) and any(k.endswith("-shm") for k in live_modes), f"db/-wal/-shm created 0600 (checked while the server was running): {live_modes}")
check(CANARY not in "".join(raw_out) and CANARY not in err, "canary absent from stdout/stderr (RUST_LOG=trace)")
check("harmless" not in err and REDACT_CANARY not in err, "no argument values in trace output")
check("hunter2" not in err.lower() and "hunter2" not in "".join(raw_out).lower(), "redact-class argument values absent from stdout/stderr (R1-3)")
check("call_tool{tool=remember}" in err, "call_tool span carries the tool name only")
# the key file lives under KABAN_HOME/keys (core-spec S3), never in ~/.kaban
if not docker_image:
    keys = glob.glob(os.path.join(kaban_home, "keys", "*.key"))
    check(len(keys) == 1 and os.stat(keys[0]).st_size == 32 and oct(os.stat(keys[0]).st_mode & 0o777) == "0o600", "one 32-byte key file, mode 0600, under KABAN_HOME/keys")

# CLI section (M4 C4.6): the same binary, db, policy and KABAN_HOME, after the server exited; the
# CLI goes through the same seam as the MCP adapter (M4-3) and binds the same workspace from this
# directory, so it reads what the MCP flow stored
if docker_image:
    print("note: key-file, CLI, hook and later sections skipped in Docker mode (workspace binding differs inside the container)")
else:
    FAILURE_CANARY = "SmokeFailureZq8vK3"  # a clean token in the captured error text, found by recall
    def cli(*args, stdin=None):
        return subprocess.run([bin_, "--db", db, "--policy", policy, "--json", *args], input=stdin, capture_output=True, text=True, env=env)
    def one_json(r):
        try:
            v = json.loads(r.stdout)
        except ValueError:
            return None
        return v if isinstance(v, dict) and r.stdout.strip().count("\n") == 0 else None
    r = cli("recall", "libssl linker", "--hops", "0")
    v = one_json(r)
    check(r.returncode == 0 and v is not None and any(x["id"] == fid for x in v["results"]), f"cli recall parses as one JSON object and finds the Failure the MCP flow stored (exit {r.returncode})")
    r = cli("doctor")
    v = one_json(r) or {}
    names = [c["name"] for c in v.get("checks", [])]
    # SS-21 replaces the retired trigram diagnostic with the store's text index,
    # payload watermark, index lag and registered-consumer lag (cli_doctor.rs).
    expected_checks = [
        "db_modes", "integrity", "fts_integrity", "claim_exhausted", "stale_artifacts",
        "retired_producers", "reconcile_cursor_lag", "schema_version", "changes_lag",
        "outbox_lag", "conflicts", "consumer_lag", "text_index", "text_index_watermark",
        "text_index_lag", "ident_terms_version", "layer_fts", "layer_gazetteer",
        "layer_embed", "layer_extractors", "key_file", "audit", "discovered", "unstamped",
        "unresolved", "stale_anchors",
    ]
    check(r.returncode == 0 and v.get("ok") is True and names == expected_checks and all(c["ok"] for c in v["checks"]), f"cli doctor ok with the twenty-six checks: {names}")
    r = cli("bogus-subcommand", CANARY)
    check(r.returncode == 4 and r.stdout == "" and CANARY not in r.stderr and "Sup3r" not in r.stderr and r.stderr.startswith("error: "), f"clap error exits 4 with a fixed text, the argument never echoed (exit {r.returncode})")
    # hook section: the Claude Code variant over minimal envelopes (hooks/README.md contract)
    cwd = os.getcwd()
    def hook(event, envelope):
        return cli("hook", event, "--host", "claude-code", "--stdin-json", stdin=json.dumps(envelope))
    failure_env = {"session_id": "sess-smoke-0001", "hook_event_name": "PostToolUseFailure", "cwd": cwd, "tool_name": "Bash", "tool_input": {"command": "false"}, "error": f"smoke: the command returned exit status 1 ({FAILURE_CANARY})", "is_interrupt": False}
    r = hook("post-tool-use-failure", failure_env)
    # A committed hook may report pending search visibility on stderr (SS-10);
    # the recall below verifies the captured record without replaying the write.
    check(r.returncode == 0 and one_json(r) == {}, f"hook post-tool-use-failure prints {{}} and exits 0 (exit {r.returncode}; stderr {r.stderr!r})")
    r = cli("recall", FAILURE_CANARY, "--hops", "0", "--full-text")
    v = one_json(r) or {"results": []}
    found = [x for x in v["results"] if x["kind"] == "Failure" and x.get("props", {}).get("pending_fix") is True and x.get("props", {}).get("command") == "false"]
    check(r.returncode == 0 and len(found) == 1 and found[0]["provenance"].get("session_id") == "sess-smoke-0001", f"recall finds the captured Failure with pending_fix and the session id ({len(found)} hit)")
    # the session briefing lists the pending Failures, never a Decision (governance is removed)
    r = hook("session-start", {"session_id": "sess-smoke-0003", "hook_event_name": "SessionStart", "cwd": cwd, "source": "startup"})
    v = one_json(r) or {}
    ctx = v.get("hookSpecificOutput", {}).get("additionalContext", "")
    check(r.returncode == 0 and v.get("hookSpecificOutput", {}).get("hookEventName") == "SessionStart" and found and found[0]["id"] in ctx and "pending fix" in ctx and did not in ctx, f"hook session-start briefs the captured pending Failure, no Decision, as hookSpecificOutput.additionalContext (exit {r.returncode})")
    secret_env = dict(failure_env, session_id="sess-smoke-0002", tool_input={"command": "deploy"}, error=f"auth failed: deploy password = {CANARY}")
    r = hook("post-tool-use-failure", secret_env)
    check(r.returncode == 0 and one_json(r) == {} and "refused by the denylist policy" in r.stderr and CANARY not in r.stdout + r.stderr and "Sup3r" not in r.stdout + r.stderr, f"a credential in the hook's error text is refused: {{}}, exit 0, stderr fixed text, no echo (exit {r.returncode})")
    r = cli("recall", "deploy auth failed", "--kind", "Failure", "--hops", "0")
    v = one_json(r) or {"results": []}
    check(r.returncode == 0 and not any(x.get("props", {}).get("command") == "deploy" or "deploy" in x.get("text", "") for x in v["results"]), "the refused failure stored nothing")
    r = cli("hook", "bogus", "--stdin-json", stdin="{}")
    check(r.returncode == 4 and r.stdout == "" and r.stderr.startswith("error: "), f"kaban hook bogus is a usage error: exit 4 (exit {r.returncode})")
    files = [f for f in glob.glob(db + "*") if os.path.isfile(f)]
    blob = b"".join(open(f, "rb").read() for f in files)
    check(CANARY.encode() not in blob and FAILURE_CANARY.encode() in blob, "after the hook section: the refused canary is absent from the db bytes, the captured failure present")
    # the temporary git repository next to the db (M5-27): the docs-ingest and engine sections
    # below write into it, so the checkout's own `.kaban/` is never written. Same binary, db,
    # policy and KABAN_HOME; `in_tmp` runs the CLI with the repository as its directory.
    ws = env["KABAN_WORKSPACE"]
    tmp = db + ".m5repo"
    shutil.rmtree(tmp, ignore_errors=True); os.makedirs(tmp)
    git_env = dict(env, GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_DATE="2026-10-26T08:16:02Z", GIT_COMMITTER_DATE="2026-10-26T08:16:02Z")
    def git(*args):
        r = subprocess.run(["git", "-C", tmp, *args], capture_output=True, text=True, env=git_env)
        if r.returncode != 0:
            raise RuntimeError(f"git {args}: {r.stderr}")
        return r.stdout.strip()
    git("init", "-q", "-b", "main"); git("config", "user.name", "smoke"); git("config", "user.email", "smoke@example.invalid")
    with open(os.path.join(tmp, "README.md"), "w") as fh: fh.write("# smoke\n")
    git("add", "-A"); git("commit", "-q", "-m", "initial")
    kdir = os.path.join(tmp, ".kaban")
    def files_under(root, skip_git=False):
        out = []
        for d, dirs, fs in os.walk(root):
            if skip_git and ".git" in dirs: dirs.remove(".git")
            out += [os.path.join(d, f) for f in fs]
        return sorted(out)
    def in_tmp(*args):
        return subprocess.run([os.path.abspath(bin_), "--db", os.path.abspath(db), "--policy", os.path.abspath(policy), "--json", *args], cwd=tmp, stdin=subprocess.DEVNULL, capture_output=True, text=True, env=dict(env, KABAN_WORKSPACE=ws))
    # docs-ingest DI1.6: the plain pipeline (`kaban ingest`, the MCP tools, the resource, the chunk
    # card, `forget --document`) in the temporary repository; the reject canary in a paragraph is
    # stubbed or blocks its file and never reaches a stream (D6, D7, D10)
    os.makedirs(os.path.join(tmp, "docs"), exist_ok=True)
    with open(os.path.join(tmp, "docs", "alpha.md"), "w") as f:
        f.write("# Alpha\n\n## Storage\n\nThe nightjar store uses WAL mode for concurrent readers.\n\nSee [[beta]] and #ops.\n")
    with open(os.path.join(tmp, "docs", "keys.md"), "w") as f:
        f.write(f"# Keys\n\nStaging login:\n\npassword = {CANARY}\n")
    r = in_tmp("ingest", "docs")
    v = one_json(r) or {}
    check(r.returncode == 0 and v.get("documents", 0) >= 1 and v.get("blocked_chunks", 0) + v.get("blocked", 0) >= 1 and CANARY not in r.stdout + r.stderr, f"docs: ingest commits the clean document, stubs or blocks the canary and echoes nothing ({v.get('documents')} documents, {v.get('blocked_chunks')} stubbed chunks, {v.get('blocked')} blocked, exit {r.returncode})")
    r2 = in_tmp("ingest", "docs")
    v2 = one_json(r2) or {}
    check(r2.returncode == 0 and v2.get("documents") == 0 and v2.get("outbox_rows") == 0 and v2.get("skipped") == 2, f"docs: a second ingest skips both documents and writes no outbox row ({v2})")
    r = in_tmp("recall", "WAL mode concurrent", "--kind", "Chunk", "--hops", "0")
    v = one_json(r) or {"results": []}
    card = (v["results"] or [{}])[0]
    check(r.returncode == 0 and card.get("kind") == "Chunk" and card.get("summary") == "docs/alpha.md > Alpha > Storage", f"docs: recall returns the chunk card `path > heading path` ({card.get('summary')})")
    r = in_tmp("forget", "--document", "docs/alpha.md")
    v = one_json(r) or {}
    rr = in_tmp("recall", "WAL mode concurrent", "--kind", "Chunk", "--hops", "0")
    check(r.returncode == 0 and v.get("tombstoned", 0) >= 3 and (one_json(rr) or {}).get("count") == 0, f"docs: forget --document tombstones the document with its sections and chunks and hides them from recall ({v.get('tombstoned')} tombstoned)")
    # DI1.8b: a directory ingest leaves the forgotten document alone and says so; `--include-forgotten` brings it back
    r3 = in_tmp("ingest", "docs")
    v3 = one_json(r3) or {}
    check(r3.returncode == 0 and v3.get("skipped_forgotten") == 1 and v3.get("documents") == 0, f"docs: a directory ingest after forget skips the forgotten document and counts it ({v3})")
    r4 = in_tmp("ingest", "docs", "--include-forgotten")
    v4 = one_json(r4) or {}
    check(r4.returncode == 0 and v4.get("skipped_forgotten") == 0 and v4.get("documents") == 1, f"docs: ingest --include-forgotten reactivates the forgotten document ({v4})")
    # docs-ingest DI2.2 (the DI2.1 residual): one born-digital PDF fixture through `kaban ingest`.
    # Both Markdown files are unchanged, so they are skipped (2); the PDF is the only new candidate.
    PDF_FIXTURES = "crates/kaban-ingest/tests/fixtures/pdf"
    shutil.copyfile(os.path.join(PDF_FIXTURES, "born1.pdf"), os.path.join(tmp, "docs", "born1.pdf"))
    r = in_tmp("ingest", "docs")
    v = one_json(r) or {}
    rr = in_tmp("recall", "second page heading", "--kind", "Chunk", "--hops", "0")
    vr = one_json(rr) or {"results": []}
    cards = [x.get("summary") for x in vr.get("results", [])]
    if HAS_PDF:
        check(r.returncode == 0 and v.get("documents") == 1 and v.get("failed") == 0 and v.get("partial") == 0 and v.get("skipped") == 2, f"pdf[{PDF_BRANCH}]: ingest commits born1.pdf beside the two unchanged Markdown files ({v.get('documents')} documents, {v.get('failed')} failed, {v.get('partial')} partial, {v.get('skipped')} skipped, exit {r.returncode})")
        check(rr.returncode == 0 and "docs/born1.pdf > p.2 > Second Page" in cards, f"pdf[{PDF_BRANCH}]: recall --kind Chunk returns the page card `docs/born1.pdf > p.2 > Second Page` ({cards})")
        shutil.copyfile(os.path.join(PDF_FIXTURES, "scanned.pdf"), os.path.join(tmp, "docs", "scanned.pdf"))
        r = in_tmp("ingest", "docs")
        v = one_json(r) or {}
        check(r.returncode == 0 and v.get("documents") == 1 and v.get("partial") == 1 and v.get("failed") == 0 and v.get("skipped") == 3, f"pdf[{PDF_BRANCH}]: a PDF without a text layer is committed partial (needs_ocr), nothing fails ({v.get('partial')} partial, {v.get('failed')} failed, {v.get('skipped')} skipped, exit {r.returncode})")
    else:
        check(r.returncode == 0 and v.get("failed") == 1 and v.get("documents") == 0 and v.get("partial") == 0 and v.get("skipped") == 2, f"pdf[{PDF_BRANCH}]: without the pdf feature born1.pdf is recorded failed and the batch continues past it (exit {r.returncode}; {v.get('failed')} failed, {v.get('skipped')} skipped, {v.get('documents')} documents)")
        check(rr.returncode == 0 and vr.get("count") == 0 and cards == [], f"pdf[{PDF_BRANCH}]: recall --kind Chunk finds no chunk of the PDF's text ({cards})")
    # the fixed reason is asserted through MCP `index_status.failed[].error` in the e1 section below
    # (the CLI report carries counts, not the failure list)
    shutil.rmtree(os.path.join(tmp, "docs"))  # the canary fixture file leaves the temporary repository before the byte sweep below
    files = [f for f in glob.glob(db + "*") if os.path.isfile(f)]
    blob = b"".join(open(f, "rb").read() for f in files)
    tmp_blob = b"".join(open(p, "rb").read() for p in files_under(tmp, skip_git=True))
    check(CANARY.encode() not in blob + tmp_blob, "docs: after the section the reject canary is absent from db/-wal/-shm bytes and from every file in the temporary repository outside .git")
    # engine E1 section (engine-plan E1.9, §12; EN-F2, EN-F5, EN-F7, EN-S8): the
    # ontology verbs through the CLI in the temporary repository (the S7 containment root), the
    # `schema_describe` body through a second server on the same database
    ONT_YAML = """ontology: ops
version: 1
kinds:
  Runbook:
    description: "An operational runbook."
    props_schema: {type: object, additionalProperties: false, properties: {owner: {type: string}}}
    governed: {acceptance: human, serialize: {dir: runbooks, format: md-frontmatter},
               export: {digest: true, marker: false, heading: Runbooks}, display: {label: Runbook, plural: Runbooks},
               governs_paths: true, dedup: none}
  Service:
    description: "A deployed service."
rels:
  OPERATES: {from: [ops:Service], to: [ops:Runbook], directed: true, description: "The service is operated by the runbook."}
discovered: {policy: review, cap: 500, prune_after_days: 90, promote_when: {min_count: 20, min_sources: 3, min_mean_score: 0.7}}
"""
    def ont_file(name, text):
        path = os.path.join(tmp, name)
        with open(path, "w") as fh: fh.write(text)
        return path
    def fixed_error(r, text):  # EN-F7: exit 4, stdout empty, stderr exactly the fixed text
        return r.returncode == 4 and r.stdout == "" and r.stderr.strip() == f"error: {text}"
    ont_file("ops.yaml", ONT_YAML)
    r = in_tmp("ontology", "load", "ops.yaml")
    v = one_json(r) or {}
    check(r.returncode == 0 and (v.get("ns"), v.get("version"), v.get("kinds"), v.get("rels"), v.get("changed")) == ("ops", 1, 2, 1, True) and len(v.get("hash", "")) == 64, f"e1: ontology load of a user file (one governed kind, one plain kind, one rel, discovered review) writes the namespace: {v}")
    # the same bytes under `.kaban/ontology/` so every later process in the repository starts with the namespace (EN-F2)
    os.makedirs(os.path.join(kdir, "ontology"), exist_ok=True)
    shutil.copyfile(os.path.join(tmp, "ops.yaml"), os.path.join(kdir, "ontology", "ops.yaml"))
    r = in_tmp("ontology", "list")
    v = one_json(r) or {}
    onts = {o["ns"]: o for o in v.get("ontologies", [])}
    check(r.returncode == 0 and list(onts) == ["core", "adr", "ops"] and onts.get("ops", {}).get("builtin") is False and onts["ops"].get("kinds") == 2 and onts["ops"].get("rels") == 1, f"e1: ontology list shows the shipped namespaces and the loaded one: {list(onts)}")
    # EN-S8: a credential-shaped value in a kind description fails the load with exit 2, rule ids only
    canary_ont = ont_file("canary.yaml", f"ontology: canary\nversion: 1\nkinds:\n  Secret:\n    description: \"deploy password = {CANARY} for staging\"\nrels: {{}}\n")
    r = in_tmp("ontology", "load", "canary.yaml")
    check(r.returncode == 2 and r.stdout == "" and "REJECTED" in r.stderr and "generic-credential-assignment" in r.stderr and CANARY not in r.stderr and "Sup3r" not in r.stderr, f"e1: a canary in an ontology file fails the load with exit 2 and rule ids only (exit {r.returncode})")
    os.remove(canary_ont)
    r = in_tmp("ontology", "list")
    check(r.returncode == 0 and [o["ns"] for o in (one_json(r) or {}).get("ontologies", [])] == ["core", "adr", "ops"], "e1: nothing of the refused file was loaded")
    # EN-F7: the three texts a user is most likely to hit, exit 4, fixed text, no stdout
    ont_file("ident.yaml", "ontology: shop\nversion: 1\nkinds:\n  vendor-thing: {description: \"lowercase\"}\nrels: {}\n")
    r = in_tmp("ontology", "load", "ident.yaml")
    check(fixed_error(r, "invalid_identifier"), f"e1: a lowercase kind name is invalid_identifier, exit 4 (exit {r.returncode}; stderr {r.stderr.strip()!r})")
    ont_file("conflict.yaml", "ontology: ops\nversion: 1\nkinds:\n  Runbook:\n    description: \"Another body.\"\nrels: {}\n")
    r = in_tmp("ontology", "load", "conflict.yaml")
    check(fixed_error(r, "ontology_conflict"), f"e1: a second file declaring ops:Runbook differently is ontology_conflict, exit 4 (exit {r.returncode}; stderr {r.stderr.strip()!r})")
    r = in_tmp("ontology", "promote", "x:Rumour", "ops:Gossip")
    check(fixed_error(r, "unknown_kind"), f"e1: promote to an undeclared kind is unknown_kind, exit 4 (exit {r.returncode}; stderr {r.stderr.strip()!r})")
    # EN-F5: prune --dry-run lists nothing yet (the candidate is younger than prune_after_days) and writes nothing
    r = in_tmp("ontology", "prune", "--dry-run")
    v = one_json(r) or {}
    check(r.returncode == 0 and v.get("dry_run") is True and v.get("kinds") == [] and v.get("nodes_tombstoned") == 0 and v.get("n_rejected") == 0, f"e1: ontology prune --dry-run reports the figures and tombstones nothing: {v}")
    # EN-F5: promote the MCP section's hidden candidate to the loaded plain kind; the record becomes visible
    r = in_tmp("ontology", "promote", "x:Rumour", "ops:Service")
    v = one_json(r) or {}
    rr = cli("recall", "zebrafish ledger", "--hops", "0")
    hits = (one_json(rr) or {"results": []})["results"]
    check(r.returncode == 0 and (v.get("from"), v.get("to"), v.get("nodes"), v.get("candidates_released")) == ("x:Rumour", "ops:Service", 1, 1) and len(hits) == 1 and hits[0]["kind"] == "ops:Service", f"e1: ontology promote x:Rumour ops:Service renames the record and releases it into recall: {v}")
    # the `schema_describe` body (the `kaban://schema` resource) through a second server on the same database
    def mcp_once(name, args):
        q = subprocess.Popen(cmd + ["--db", db, "--policy", policy], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
        n = 0
        def send(method, params=None, notify=False):
            nonlocal n
            msg = {"jsonrpc": "2.0", "method": method}
            if params is not None: msg["params"] = params
            if not notify:
                n += 1; msg["id"] = n
            line = exchange(q, msg, notify)
            return None if notify else json.loads(line)
        send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "smoke-e1", "version": "0"}})
        send("notifications/initialized", notify=True)
        res = send("tools/call", {"name": name, "arguments": args}).get("result", {})
        q.stdin.close(); q.wait(timeout=5)
        return bool(res.get("isError")), res["content"][0]["text"] if res.get("content") else json.dumps(res)
    e, t = mcp_once("schema_describe", {})
    sd = json.loads(t) if not e else {}
    labels = {l["name"]: l for l in sd.get("labels", [])}
    rels = {x["name"]: x for x in sd.get("relationships", [])}
    runbook = labels.get("ops:Runbook", {}); service = labels.get("ops:Service", {})
    check(not e and runbook.get("ns") == "ops" and runbook.get("builtin") is False and runbook.get("governed", {}).get("acceptance") == "human" and runbook.get("governed", {}).get("serialize") == {"dir": "runbooks", "format": "md-frontmatter"} and runbook.get("display", {}).get("label") == "Runbook" and service.get("ns") == "ops" and "governed" not in service, f"e1: schema_describe lists the loaded kinds with ns and the declaration columns (E1.8 R2): {runbook.get('ns')}, {sorted(runbook.get('governed', {}))}")
    check(rels.get("ops:OPERATES", {}).get("ns") == "ops" and rels.get("ops:OPERATES", {}).get("from_kinds") == ["ops:Service"] and rels.get("ops:OPERATES", {}).get("to_kinds") == ["ops:Runbook"], "e1: schema_describe lists the loaded rel with its endpoints")
    check(not any(n.startswith("canary:") for n in labels) and "Secret" not in labels, "e1: schema_type is unchanged by the refused canary file")
    ks = [k for k in sd.get("kind_stats", []) if k.get("ns") == "ops" and k.get("kind") == "Service"]
    check(len(ks) == 1 and ks[0].get("n_nodes") == 1 and ks[0].get("workspace_id") == ws and not any(k.get("ns") == "x" and k.get("kind") == "Rumour" for k in sd.get("kind_stats", [])), f"e1: schema_describe carries kind_stats for the bound workspace, the counter moved to the promoted kind: {sd.get('kind_stats')}")
    # docs-ingest DI2.2: the PDF ingested in the CLI docs section above, seen through `index_status`
    # on the same database (the one surface that lists failed documents with their fixed reason)
    e, t = mcp_once("index_status", {})
    ist = json.loads(t) if not e else {}
    failed_pdf = [f for f in ist.get("failed", []) if f.get("path") == "docs/born1.pdf"]
    by_status = ist.get("documents_by_status", {})
    if HAS_PDF:
        check(not e and failed_pdf == [] and by_status.get("partial") == 1 and by_status.get("failed") is None, f"pdf[{PDF_BRANCH}]: index_status lists no failed PDF and one partial document (scanned.pdf): {by_status}")
    else:
        check(not e and len(failed_pdf) == 1 and failed_pdf[0].get("error") == NO_EXTRACTOR and by_status.get("failed") == 1, f"pdf[{PDF_BRANCH}]: index_status.failed names docs/born1.pdf with the fixed reason `{NO_EXTRACTOR}` ({failed_pdf}, {by_status})")
    files = [f for f in glob.glob(db + "*") if os.path.isfile(f)]
    blob = b"".join(open(f, "rb").read() for f in files)
    e1_blob = b"".join(open(p, "rb").read() for p in files_under(tmp, skip_git=True))
    check(CANARY.encode() not in blob + e1_blob, "e1: after the section the ontology canary is absent from db/-wal/-shm bytes and from the temporary repository outside .git")
    # ---- E2 ----
    # engine E2 section (engine-plan E2.12 item 1; EN-F12c, EN-F14a, SC-02): the overlay through
    # the public surfaces that survive the governance removal, after the E1 lines, on one fresh
    # server over the same database (the main server exited before the CLI sections) and the same
    # CLI binding. Every message starts `e2:`. Not reachable black-box in 3.0.0 and therefore not
    # asserted here: a layer assertion on a human-linked edge (no CLI or MCP surface submits a
    # producer claim; `kaban-store-sqlite/tests/protection.rs` holds the property).
    class E2Server:
        """One stdio server for the section: `tools/list` and `tools/call` over the smoke's db."""
        def __init__(self):
            self.q = subprocess.Popen(cmd + ["--db", db, "--policy", policy], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True, env=env)
            self.n = 0
            self.send("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "smoke-e2", "version": "0"}})
            self.send("notifications/initialized", notify=True)
        def send(self, method, params=None, notify=False):
            msg = {"jsonrpc": "2.0", "method": method}
            if params is not None: msg["params"] = params
            if not notify:
                self.n += 1; msg["id"] = self.n
            line = exchange(self.q, msg, notify)
            return None if notify else json.loads(line)
        def call(self, name, args):
            res = self.send("tools/call", {"name": name, "arguments": args}).get("result", {})
            return bool(res.get("isError")), res["content"][0]["text"] if res.get("content") else json.dumps(res)
        def tools(self):
            return sorted(t["name"] for t in self.send("tools/list")["result"]["tools"])
        def close(self):
            self.q.stdin.close(); self.q.wait(timeout=5)
    e2 = E2Server()
    # (1) `link` between two plain records is the human edge; a second `link` on the same triple
    # is the same claim (no second edge, the first confidence kept); `traverse` shows one edge
    e, t = e2.call("remember", {"kind": "Fact", "text": "the e2 overlay smoke: a plain fact about the quokka tier"})
    e2_a = json.loads(t).get("id", "") if not e else ""
    e, t = e2.call("remember", {"kind": "Fact", "text": "the e2 overlay smoke: the quokka tier runbook note"})
    e2_b = json.loads(t).get("id", "") if not e else ""
    e, t = e2.call("link", {"from": e2_a, "to": e2_b, "rel": "RELATES_TO", "confidence": 0.9})
    lk = json.loads(t) if not e else {}
    check(not e and e2_a and e2_b and lk.get("from") == e2_a and lk.get("to") == e2_b and lk.get("confidence") == 0.9, f"e2: link of two plain records writes the human edge ({lk})")
    e, t = e2.call("link", {"from": e2_a, "to": e2_b, "rel": "RELATES_TO", "confidence": 0.9})
    e, t = e2.call("traverse", {"start_ids": [e2_a], "max_depth": 1})
    tv = json.loads(t) if not e else {"edges": [], "nodes": []}
    e2_edges = [x for x in tv.get("edges", []) if x.get("from") == e2_a and x.get("to") == e2_b]
    check(not e and len(e2_edges) == 1 and e2_edges[0].get("rel") == "RELATES_TO" and e2_edges[0].get("confidence") == 0.9 and len(tv.get("edges", [])) == 1, f"e2: a second link on the same edge leaves the one human edge in place at its confidence ({tv.get('edges')})")
    # (2) `ask` is absent from the catalog and from the CLI (OWN-68 SC-02)
    e2_tools = e2.tools()
    r = cli("--help")
    e2_verbs = [l.split()[0] for l in r.stdout.splitlines() if l.startswith("  ") and not l.startswith("   ") and l.split()]
    check("ask" not in e2_tools and "ask" not in e2_verbs and "recall" in e2_tools and "recall" in e2_verbs and "ontology" in e2_verbs, f"e2: ask is absent from tools/list and from kaban --help, recall and ontology are present ({len(e2_tools)} tools, {len(e2_verbs)} verbs)")
    e2.close()
    files = [f for f in glob.glob(db + "*") if os.path.isfile(f)]
    blob = b"".join(open(f, "rb").read() for f in files)
    check(CANARY.encode() not in blob, "e2: after the section the ontology canary is absent from db/-wal/-shm/audit bytes")
    # ---- end E2 ----
    # ---- E3 ----
    # engine E3 section (engine-plan E3.12; EN-S4, OWN-68 SC-04): the strace envelope over the stdio
    # surfaces, in a temporary git repository with its own database. Under `strace -f -e
    # trace=connect,execve`: plain `ingest`, `ingest --link gazetteer`, a structured `--with llm`
    # dry run, the deterministic script protocol (brief, submit, finish) and a fenced `ingest_claim`
    # submit, `recall` with every retained layer flag, `stats`, and `serve` over stdio with one tool
    # call. Asserted per run: no `execve` but the kaban binary itself and `git`, no `connect` but
    # AF_UNIX and loopback. The deferred selections (similarity recall, embed linking and its layer
    # reset; OWN-93/94) refuse with exit 4 instead of simulating success; `serve --http` is absent
    # in 3.0.0 (SC-04). Every message starts `e3:`. Linux with `strace` only; elsewhere a note.
    import re
    e3_strace = shutil.which("strace") if sys.platform.startswith("linux") else None
    if not e3_strace:
        print("note: e3 strace envelope not run (needs Linux and strace)")
    else:
        e3_repo = db + ".e3repo"
        e3_db = os.path.abspath(db + ".e3.db")
        e3_policy = os.path.abspath(policy)
        e3_log = db + ".e3.strace"
        shutil.rmtree(e3_repo, ignore_errors=True); os.makedirs(e3_repo)
        subprocess.run(["git", "init", "-q", e3_repo], check=True, env=git_env)
        for name, text in (("a.md", "# Alpha\n\nThe quokka tier gateway routes checkout.\n"), ("b.md", "# Beta\n\nThe quokka tier gateway is referenced here.\n")):
            with open(os.path.join(e3_repo, name), "w") as fh: fh.write(text)
        e3_cmd = [e3_strace, "-f", "-qq", "-e", "trace=connect,execve", "-o", e3_log]
        e3_bin = os.path.realpath(bin_)
        def e3_envelope():
            """Every traced execve/connect outside the envelope, and whether the binary's own exec was seen."""
            bad, seen = [], False
            for line in open(e3_log):
                m = re.search(r"\b(execve|connect)\((.*)", line)
                if not m:
                    continue  # signals, exits, `<... resumed>` halves
                if m.group(1) == "execve":
                    path = re.match(r'"([^"]*)"', m.group(2)).group(1)
                    if os.path.realpath(path) == e3_bin:
                        seen = True
                    elif os.path.basename(path) != "git" and "/git-core/" not in path:
                        bad.append(line.strip())
                elif not ("sa_family=AF_UNIX" in m.group(2) or re.search(r'inet_addr\("127\.|"::1"|"::ffff:127\.', m.group(2))):
                    bad.append(line.strip())
            return bad, seen
        def e3_cli(*args, stdin=None):
            """One traced CLI run in the E3 repository; returns (exit, stdout JSON or None, envelope violations)."""
            r = subprocess.run(e3_cmd + [e3_bin, "--db", e3_db, "--policy", e3_policy, "--json", *args], input=stdin, capture_output=True, text=True, env=env, cwd=e3_repo)
            bad, seen = e3_envelope()
            if not seen:
                bad.append("strace saw no execve of the kaban binary")
            try:
                out = json.loads(r.stdout)
            except ValueError:
                out = None
            return r.returncode, out, bad
        rc, v, bad = e3_cli("ingest", "a.md")
        check(rc == 0 and (v or {}).get("documents") == 1 and bad == [], f"e3: plain ingest under strace stays inside the envelope (exit {rc}; {bad})")
        rc0, _, bad0 = e3_cli("remember", "Fact", "quokka tier gateway")
        rc, v, bad = e3_cli("ingest", "b.md", "--link", "gazetteer")
        check(rc0 == 0 and rc == 0 and (v or {}).get("gazetteer", {}).get("outcome") == "ok" and bad0 + bad == [], f"e3: remember and ingest --link gazetteer under strace stay inside the envelope (exits {rc0}/{rc}; {bad0 + bad})")
        rc, v, bad = e3_cli("ingest", "--mode", "structured", "--with", "llm", "--dry-run", "a.md")
        check(rc == 0 and (v or {}).get("mode") == "structured" and bad == [], f"e3: a structured --with llm dry-run brief renders under strace without an extractor exec (exit {rc}; {bad})")
        rc, v, bad = e3_cli("ingest", "job", "brief", "a.md")
        job = (v or {}).get("job", "")
        body = {"job": job, "records": [{"local_id": "r", "kind": "Fact", "text": "The quokka tier gateway routes checkout"}]}
        rc2, v2, bad2 = e3_cli("ingest", "job", "submit", job, "--stdin-json", stdin=json.dumps(body))
        rc3, v3, bad3 = e3_cli("ingest", "job", "finish", job)
        check((rc, rc2, rc3) == (0, 0, 0) and (v2 or {}).get("created") == 1 and (v3 or {}).get("status") == "finished" and bad + bad2 + bad3 == [], f"e3: the script protocol (brief, submit, finish) under strace stays inside the envelope (exits {rc}/{rc2}/{rc3}; {bad + bad2 + bad3})")
        rc, v, bad = e3_cli("ingest", "job", "claim", "--with", "gliner", "--path-prefix", "b.md", "--worker", "smoke:1")
        job, fence = (v or {}).get("job") or "", str(((v or {}).get("lease") or {}).get("fence", ""))
        body = {"job": job, "fence": int(fence or 0), "records": [{"local_id": "g", "kind": "Fact", "text": "The quokka tier gateway is referenced"}]}
        rc2, v2, bad2 = e3_cli("ingest", "job", "submit", job, "--fence", fence, "--stdin-json", stdin=json.dumps(body))
        rc3, v3, bad3 = e3_cli("ingest", "job", "finish", job, "--fence", fence)
        check((rc, rc2, rc3) == (0, 0, 0) and job and (v2 or {}).get("created") == 1 and (v3 or {}).get("status") == "finished" and bad + bad2 + bad3 == [], f"e3: a fenced ingest_claim submit and finish under strace stay inside the envelope (exits {rc}/{rc2}/{rc3}; {bad + bad2 + bad3})")
        rc0, _, _ = e3_cli("reindex", "trigram")  # retired with FTS5 (spec 05 SS-16): refused, exit 4
        rc, v, bad = e3_cli("recall", "quokka", "--layers", "fts,fuzzy", "--fuzzy")
        check(rc0 == 4 and rc == 0 and (v or {}).get("count", 0) >= 1 and bad == [], f"e3: the retired trigram rebuild is refused and recall with every retained layer flag (fts, fuzzy) under strace stays inside the envelope (exits {rc0}/{rc}; {bad})")
        rc, v, bad = e3_cli("stats")
        check(rc == 0 and v is not None and bad == [], f"e3: stats under strace stays inside the envelope (exit {rc}; {bad})")
        q = subprocess.Popen(e3_cmd + [e3_bin, "--db", e3_db, "--policy", e3_policy, "serve"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=env, cwd=e3_repo)
        for n, (method, params) in enumerate((("initialize", {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "smoke-e3", "version": "0"}}), ("notifications/initialized", None), ("tools/call", {"name": "recall", "arguments": {"query": "quokka", "layers": ["fts", "fuzzy"], "fuzzy": True}}))):
            msg = {"jsonrpc": "2.0", "method": method, **({"params": params} if params is not None else {}), **({"id": n} if params is not None else {})}
            q.stdin.write(json.dumps(msg) + "\n"); q.stdin.flush()
            if params is not None:
                e3_reply = json.loads(q.stdout.readline())
        q.stdin.close(); q.wait(timeout=10)
        bad, seen = e3_envelope()
        e3_res = e3_reply.get("result", {})
        check(seen and q.returncode == 0 and not e3_res.get("isError") and (e3_res.get("structuredContent") or {}).get("count", 0) >= 1 and bad == [], f"e3: serve over stdio with one recall tool call under strace stays inside the envelope (exit {q.returncode}; {bad})")
        for args, what in ((("recall", "quokka", "--layers", "sim"), "similarity recall"), (("ingest", "a.md", "--link", "embed"), "embed linking"), (("reindex", "layer", "embed"), "the embed layer reset")):
            r = subprocess.run([e3_bin, "--db", e3_db, "--policy", e3_policy, "--json", *args], capture_output=True, text=True, env=env, cwd=e3_repo)
            check(r.returncode == 4 and r.stdout == "", f"e3: the deferred selection ({what}) refuses with exit 4 and no result (exit {r.returncode}; {r.stderr.strip()[:80]!r})")
        r = subprocess.run([e3_bin, "--db", e3_db, "serve", "--http"], stdin=subprocess.DEVNULL, capture_output=True, text=True, env=env, cwd=e3_repo)
        check(r.returncode == 4 and r.stdout == "", f"e3: serve --http is refused, absent in 3.0.0 (exit {r.returncode})")
    # ---- end E3 ----
print("\nRESULT:", "PASS" if not fails else f"FAIL ({len(fails)})")
sys.exit(1 if fails else 0)
