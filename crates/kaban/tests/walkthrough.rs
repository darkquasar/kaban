//! E3.11: the walkthrough (Steps 1..6) over the real stdio MCP server; CLI only for setup, plain
//! ingestion and doctor. Synthetic code rows never execute an indexer. EN-N4: two fresh
//! runs compare after relabelling generated ids (OWN-117); a same-database replay keeps every id.
#![cfg(all(feature = "ingest", feature = "code-graph"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "../../kaban-api/tests/common/cg_fixture.rs"]
mod cg_fixture;
mod common;
#[path = "common/extractor_script.rs"]
mod extractor_script;
#[path = "common/mcp_client.rs"]
mod mcp_client;
#[path = "../../../fixtures/walkthrough/shop/shop_rows.rs"]
mod shop_rows;

use common::Scratch;
use kaban_core::{
    Actor, AssertionStatus, BatchAssertion, BatchSubject, ChangeLogStore, ClaimClass, Fixed,
    IngestStore, Layer, MemoryStore, Method, NewNode, NodeId, Op, Rank, WorkspaceId, WriteBatch,
    WriteCtx,
};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use mcp_client::{McpClient, Spawn, snapshot_dir, write_codegraph_key};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const ADR: &str = "docs/adr/0007-discounts.md";
const ARCH: &str = "docs/ARCHITECTURE.md";
const INITIAL_DATE: &str = "2026-10-26T08:16:02Z";
const RENAME_DATE: &str = "2026-10-26T09:04:36Z";

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/walkthrough/shop")
}

fn git(root: &Path, date: &str, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_AUTHOR_NAME", "kaban fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@kaban.invalid")
        .env("GIT_COMMITTER_NAME", "kaban fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@kaban.invalid")
        .env("GIT_AUTHOR_DATE", date)
        .env("GIT_COMMITTER_DATE", date)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
}

fn cli(s: &Scratch, root: &Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_kaban"))
        .current_dir(root)
        .env("KABAN_HOME", s.home())
        .env("RUST_LOG", "warn")
        .args([
            "--db",
            s.db().to_str().unwrap(),
            "--workspace",
            "default",
            "--json",
        ])
        .args(args)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "CLI {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn start(s: &Scratch, root: &Path) -> McpClient {
    McpClient::start(&Spawn {
        home: &s.home(),
        db: &s.db(),
        cwd: root,
        policy: None,
        workspace: "default",
    })
}

fn store(s: &Scratch) -> SqliteStore {
    SqliteStore::open(&s.db(), &s.home().join("keys")).unwrap()
}

fn ctx() -> WriteCtx {
    WriteCtx::new(
        Policy::empty().effective_hash(),
        WorkspaceId::default(),
        Actor::Agent,
    )
}

fn fixed_id(i: usize) -> NodeId {
    NodeId::parse(&format!("01a17a03-5344-7000-8000-{i:012x}")).unwrap()
}

fn nodes(s: &Scratch, kind: &str) -> Vec<kaban_core::Node> {
    store(s)
        .list_nodes(kind, WorkspaceId::default().as_str(), 0, 500)
        .unwrap()
}

fn changes(s: &Scratch) -> u64 {
    let mut after = 0;
    loop {
        let page = store(s).changes_read(after, 500).unwrap();
        let Some(last) = page.last() else {
            return after;
        };
        after = last.seq;
    }
}

fn claim(
    subject: BatchSubject,
    class: ClaimClass,
    value: Value,
    layer: Layer,
    method: Method,
    score: u32,
    lineage: &str,
) -> BatchAssertion {
    BatchAssertion {
        subject,
        claim_class: class,
        predicate: None,
        object: None,
        value,
        layer,
        method,
        extractor_id: Some("walkthrough".into()),
        extractor_version: Some("1".into()),
        source_id: Some(ARCH.into()),
        lineage_id: lineage.into(),
        raw_score: Fixed::new(score).unwrap(),
        rank: Rank::Normal,
        rank_reason: None,
        calibration_version: 1,
        supersedes: None,
    }
}

fn mentions(mcp: &mut McpClient, chunk: &str) -> Value {
    mcp.call(
        "traverse",
        &json!({"start_ids":[chunk],"rels":["MENTIONS"],"direction":"out","max_depth":1}),
    )
}

fn page_with(mcp: &mut McpClient, brief: &Value, text: &str) -> Value {
    for n in 0..20 {
        let page = mcp.call("ingest_next", &json!({"job":brief["job"],"n":n}));
        if page["chunk"]["text"].as_str().unwrap().contains(text) {
            return page;
        }
        assert!(!page["next"].is_null(), "missing {text}: {page}");
    }
    panic!("bounded fixture chunk lookup exhausted")
}

fn step3(s: &Scratch, _root: &Path, mcp: &mut McpClient, arch: &str, adr: &str) -> Value {
    let brief = mcp.call("ingest_brief", &json!({"path":ADR,"with":"llm"}));
    let page = page_with(mcp, &brief, "never trusted from the client");
    let body = extractor_script::decision(
        &brief,
        &page,
        "Tampered client totals must not bypass promotion rules",
    );
    // Canonical dialect provenance and evidence; no --with script selector exists.
    let submitted = mcp.call("ingest_submit", &body);
    assert_eq!(submitted["created"], 1, "{submitted}");
    let decision = nodes(s, "Decision").into_iter().next().unwrap();
    // no surface accepts it, so the Decision stays proposed and the reads below include
    // candidates
    assert_eq!(decision.governance, kaban_core::Governance::Proposed);
    let before = changes(s);
    let repeat = mcp.call("ingest_submit", &body);
    assert_eq!(repeat["unchanged"], 1, "{repeat}");
    assert_eq!(
        changes(s),
        before,
        "identical return adds zero graph ChangeLog rows"
    );
    let code = nodes(s, "CodeRef")
        .into_iter()
        .find(|n| n.props["symbol"] == shop_rows::DISCOUNT)
        .unwrap();
    let mut forbidden = body.clone();
    forbidden["edges"] = json!([
        {"from_local_id":"discounts","rel":"GOVERNS","to_id":code.id},
        {"from_local_id":"discounts","rel":"SUPERSEDES","to_id":decision.id}
    ]);
    let refusal = mcp.call("ingest_submit", &forbidden);
    assert!(
        refusal.to_string().contains("edge_not_allowed"),
        "{refusal}"
    );
    let finish = mcp.call("ingest_finish", &json!({"job":brief["job"]}));
    assert_eq!(
        mcp.call("ingest_finish", &json!({"job":brief["job"]})),
        finish
    );
    let mut batch = WriteBatch::new(WorkspaceId::default(), vec![]);
    batch.assertions.push(claim(
        BatchSubject::Edge {
            src: NodeId::parse(&decision.id).unwrap(),
            rel: "GOVERNS".into(),
            dst: NodeId::parse(&code.id).unwrap(),
        },
        ClaimClass::Edge,
        json!({}),
        Layer::Llm,
        Method::Llm,
        10_000,
        "machine-governs",
    ));
    let report = store(s).apply_batch(batch, &ctx()).unwrap();
    assert_eq!(report.verdicts.get(&AssertionStatus::Candidate), Some(&1));
    let governs =
        |mcp: &mut McpClient| {
            mcp.call("traverse",&json!({
        "start_ids":[decision.id],"rels":["GOVERNS"],"max_depth":1,"include_candidates":true
    }))
        };
    assert_eq!(governs(mcp)["edges"], json!([]));
    // Explicitly human-authorized link request.
    mcp.call(
        "link",
        &json!({"from":decision.id,"rel":"GOVERNS","to":code.id}),
    );
    // Both original source chunks are citations; the architecture link is an explicit human act.
    mcp.call(
        "link",
        &json!({"from":arch,"rel":"EVIDENCE_FOR","to":decision.id}),
    );
    let human = governs(mcp);
    assert_eq!(human["edges"].as_array().unwrap().len(), 1, "{human}");
    let evidence = mcp.call(
        "traverse",
        &json!({"start_ids":[decision.id],"rels":["EVIDENCE_FOR"],"max_depth":1,"direction":"in","include_candidates":true}),
    );
    for id in [arch, adr] {
        assert!(
            evidence["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == id),
            "{evidence}"
        );
    }
    json!({"decision":decision.id,"brief":brief,"page":page,"submit":submitted,
        "forbidden":refusal,"finish":finish,"human_relation":human,"evidence":evidence,
        "replay":repeat})
}

fn step4(s: &Scratch, mcp: &mut McpClient, arch: &str) -> Value {
    let brief = mcp.call(
        "ingest_brief",
        &json!({"path":ARCH,"with":"gliner","link":"gazetteer"}),
    );
    let page = page_with(mcp, &brief, "Cart");
    let submitted = mcp.call("ingest_submit", &extractor_script::gliner(&brief, &page));
    assert_eq!(submitted["created"], 1, "{submitted}");
    assert!(
        submitted.to_string().contains("kind_not_allowed"),
        "{submitted}"
    );
    mcp.call("ingest_finish", &json!({"job":brief["job"]}));
    let org = fixed_id(100);
    let text = store(s)
        .get(&NodeId::parse(arch).unwrap(), false)
        .unwrap()
        .unwrap()
        .text;
    let offset = u32::try_from(text.find("Cart").unwrap()).unwrap();
    let mut batch = WriteBatch::new(
        WorkspaceId::default(),
        vec![Op::UpsertNode {
            id: org.clone(),
            node: NewNode {
                kind: "Org".into(),
                text: "Cart".into(),
                ..Default::default()
            },
        }],
    );
    let mut span = claim(
        BatchSubject::Span {
            chunk: NodeId::parse(arch).unwrap(),
            offset,
            len: 4,
        },
        ClaimClass::Type,
        json!("x:Org"),
        Layer::Gliner,
        Method::Gliner,
        6100,
        "gliner-cart",
    );
    span.object = Some(org.clone());
    batch.assertions.push(span);
    let report = store(s).apply_batch(batch, &ctx()).unwrap();
    assert!(
        report.verdicts.contains_key(&AssertionStatus::Candidate)
            || report.verdicts.contains_key(&AssertionStatus::Unresolved),
        "{report:?}"
    );
    let stats = store(s).kind_stats(&WorkspaceId::default()).unwrap();
    let stat = stats
        .iter()
        .find(|row| row.ns == "x" && row.kind == "Org")
        .unwrap();
    assert_eq!(stat.n_nodes, 1);
    let traversal = mentions(mcp, arch);
    assert!(
        !traversal["edges"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["to"] == org.as_str())
    );
    json!({"brief":brief,"page":page,"submit":submitted,"store":report,"kind_stats":stats,
        "mentions":traversal})
}

fn step5(mcp: &mut McpClient, decision: &str, arch: &str, adr: &str) -> Value {
    let evidence = mcp.call(
        "traverse",
        &json!({"start_ids":[decision],"rels":["EVIDENCE_FOR"],"direction":"in","max_depth":1,"include_candidates":true}),
    );
    for id in [arch, adr] {
        assert!(
            evidence["nodes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n["id"] == id),
            "{evidence}"
        );
    }
    let recall = mcp.call(
        "recall",
        &json!({"query":"apply_discount","full_text":true,"hops":0}),
    );
    // Identifier-routed; the architecture chunk is the first prose hit. The seeded CodeRef anchor
    // for the same symbol is a legitimate lexical hit the walkthrough text does not model.
    let results = recall["results"].as_array().unwrap();
    assert!(
        results
            .iter()
            .all(|r| r["why"].to_string().contains("query_class: identifier")),
        "{recall}"
    );
    let first_chunk = results.iter().find(|r| r["kind"] == "Chunk").unwrap();
    assert_eq!(first_chunk["id"], arch, "{recall}");
    let context = mcp.call(
        "code_context",
        &json!({"target":{"symbol":shop_rows::DISCOUNT}}),
    );
    json!({"evidence":evidence,"recall":recall,"code_context":context})
}

fn step6(s: &Scratch, root: &Path, mcp: &mut McpClient, dir: &Path, decision: &str) -> Value {
    let path = root.join("src/checkout/cart.rs");
    let text = std::fs::read_to_string(&path).unwrap();
    std::fs::write(path, text.replace("apply_discount", "apply_promotion")).unwrap();
    git(root, RENAME_DATE, &["add", "src/checkout/cart.rs"]);
    git(
        root,
        RENAME_DATE,
        &["commit", "-q", "-m", "C2: rename promotion"],
    );
    let commit = git(root, RENAME_DATE, &["rev-parse", "HEAD"]);
    let (files, rows) = shop_rows::rows(true);
    cg_fixture::build_rows(
        dir,
        dir.file_name().unwrap().to_str().unwrap(),
        &commit,
        Policy::empty().effective_hash(),
        "rust",
        kaban_codegraph::SCHEMA_VERSION,
        &files,
        &rows,
    );
    let code = nodes(s, "CodeRef")
        .into_iter()
        .find(|n| n.props["symbol"] == shop_rows::DISCOUNT)
        .unwrap();
    let doctor = cli(s, root, &["doctor"]);
    // Doctor names the renamed anchor in its stale_anchors check (not stale_artifacts).
    let stale = doctor["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "stale_anchors")
        .unwrap();
    let anchor = format!(
        "{} code: snapshot_mismatch symbol: {}",
        code.id,
        shop_rows::DISCOUNT
    );
    assert!(
        stale["detail"].as_str().unwrap().contains(&anchor),
        "{doctor}"
    );
    let new = mcp.call(
        "code_context",
        &json!({"target":{"symbol":shop_rows::PROMOTION}}),
    );
    // Path governance can still apply, but no old CodeRef is guessed onto the new symbol.
    assert!(!new.to_string().contains(&code.id), "{new}");
    let adr_path = root.join(ADR);
    let adr_text = std::fs::read_to_string(&adr_path).unwrap();
    std::fs::write(
        adr_path,
        format!("{adr_text}\nAdditional server audit validates promotion totals.\n"),
    )
    .unwrap();
    let brief = mcp.call("ingest_brief", &json!({"path":ADR,"with":"llm"}));
    let page = page_with(mcp, &brief, "never trusted from the client");
    let body = extractor_script::decision(
        &brief,
        &page,
        "Tampered totals require an additional server audit",
    );
    let submitted = mcp.call("ingest_submit", &body);
    let finished = mcp.call("ingest_finish", &json!({"job":brief["job"]}));
    let supersedes = mcp.call(
        "traverse",
        &json!({"start_ids":[decision],"rels":["SUPERSEDES"],"max_depth":1,"include_candidates":true}),
    );
    assert_eq!(supersedes["edges"], json!([]));
    json!({"commit":commit,"doctor":doctor,"new_symbol":new,
        "revision_submit":submitted,"revision_finish":finished})
}

fn step1(s: &Scratch, root: &Path) -> (McpClient, String, PathBuf) {
    // The checked-in copy is provenance only: startup refuses reloading builtin namespaces.
    // The runtime supplies these exact bytes; never install a duplicate adr.yaml in its repo.
    assert_eq!(
        std::fs::read_to_string(fixture().join(".kaban/ontology/adr.yaml")).unwrap(),
        kaban_core::shipped::ADR_YAML
    );
    for rel in [
        ARCH,
        ADR,
        "src/checkout/cart.rs",
        "src/checkout/pricing.rs",
        "index.scip",
    ] {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::copy(fixture().join(rel), path).unwrap();
    }
    git(root, INITIAL_DATE, &["init", "-q", "-b", "main"]);
    git(root, INITIAL_DATE, &["add", "-A"]);
    git(root, INITIAL_DATE, &["commit", "-q", "-m", "C1: checkout"]);
    let commit = git(root, INITIAL_DATE, &["rev-parse", "HEAD"]);
    let key = write_codegraph_key(&s.home());
    let dir = snapshot_dir(&s.home(), &key, root);
    let (files, rows) = shop_rows::rows(false);
    cg_fixture::build_rows(
        &dir,
        dir.file_name().unwrap().to_str().unwrap(),
        &commit,
        Policy::empty().effective_hash(),
        "rust",
        kaban_codegraph::SCHEMA_VERSION,
        &files,
        &rows,
    );
    // Fixed fixture identities, submitted through the public store; no copied codegraph rows.
    let ops = rows
        .iter()
        .enumerate()
        .map(|(i, row)| Op::UpsertNode {
            id: fixed_id(i + 1),
            node: NewNode {
                kind: "CodeRef".into(),
                text: format!("Symbol {}", row.name),
                props: Some(json!({"symbol":row.key,"repo":"shop","snapshot_scope":commit})),
                repo: Some("shop".into()),
                ..Default::default()
            },
        })
        .collect();
    store(s)
        .apply_batch(WriteBatch::new(WorkspaceId::default(), ops), &ctx())
        .unwrap();
    let ontology = cli(s, root, &["ontology", "list"]);
    assert!(
        ontology["ontologies"]
            .as_array()
            .unwrap()
            .iter()
            .any(|o| o["ns"] == "adr"),
        "{ontology}"
    );
    let mut mcp = start(s, root);
    let definition = mcp.call(
        "code_context",
        &json!({"target":{"symbol":shop_rows::DISCOUNT}}),
    );
    assert!(definition.to_string().contains(&commit), "{definition}");
    assert!(
        definition.to_string().contains("\"exact\":true"),
        "{definition}"
    );
    assert!(
        definition.to_string().contains("\"policy\":\"current\""),
        "{definition}"
    );
    (mcp, commit, dir)
}

fn step2(s: &Scratch, root: &Path, mcp: &mut McpClient) -> (String, String, Value) {
    cli(s, root, &["ingest", ADR]);
    // A user-owned alias, not inferred from prose; gazetteer consumes real stored metadata.
    let doc = nodes(s, "Document").into_iter().next().unwrap();
    let mut props = doc.props.clone();
    props["aliases"] = json!(["ADR 0007"]);
    props["title"] = json!("Server-side discounts");
    store(s)
        .apply_batch(
            WriteBatch::new(
                WorkspaceId::default(),
                vec![Op::UpsertNode {
                    id: NodeId::parse(&doc.id).unwrap(),
                    node: NewNode {
                        kind: "Document".into(),
                        text: "Server-side discounts".into(),
                        props: Some(props),
                        ..Default::default()
                    },
                }],
            ),
            &ctx(),
        )
        .unwrap();
    let report = cli(s, root, &["ingest", "docs/", "--link", "gazetteer"]);
    assert_eq!(report["failed"], 0, "{report}");
    let chunks = nodes(s, "Chunk");
    let arch = chunks
        .iter()
        .find(|n| n.text.contains("Checkout builds"))
        .unwrap();
    let adr = chunks
        .iter()
        .find(|n| n.text.contains("never trusted from the client") && n.id != arch.id)
        .unwrap();
    let traversal = mentions(mcp, &arch.id);
    let edges = traversal["edges"].as_array().unwrap();
    assert!(edges.len() >= 3, "{traversal}");
    // The walkthrough's L1 rows, read back from the store: the gazetteer method per MENTIONS
    // target (traverse does not project `method`); the identifier terms live in the store's
    // text index (spec 05 OWN-M), which the recall above answered from.
    let db =
        rusqlite::Connection::open_with_flags(s.db(), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let methods: std::collections::BTreeMap<String, String> = db
        .prepare(
            "SELECT t.id, e.method FROM edge e JOIN node c ON c.rid=e.src JOIN node t ON t.rid=e.dst \
             WHERE c.id=?1 AND e.rel='MENTIONS'",
        )
        .unwrap()
        .query_map([&arch.id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let anchor = |text: &str| {
        nodes(s, "CodeRef")
            .into_iter()
            .find(|n| n.text == text)
            .unwrap()
            .id
    };
    let expected: std::collections::BTreeMap<String, String> = [
        (anchor("Symbol CartService::apply_discount"), "symbol"),
        (anchor("Symbol price_lines"), "symbol"),
        (doc.id.clone(), "alias"),
    ]
    .into_iter()
    .map(|(id, m)| (id, m.to_owned()))
    .collect();
    assert_eq!(methods, expected);
    let hierarchy = mcp.call(
        "traverse",
        &json!({"start_ids":[arch.id],"rels":["PART_OF"],"max_depth":2}),
    );
    assert!(
        hierarchy["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "Section"),
        "{hierarchy}"
    );
    assert!(
        hierarchy["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["kind"] == "Document"),
        "{hierarchy}"
    );
    let brief = mcp.call(
        "ingest_brief",
        &json!({"path":ARCH,"with":"gliner","link":"gazetteer"}),
    );
    assert!(
        brief["candidates"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["ambiguous"] == true),
        "{brief}"
    );
    (
        arch.id.clone(),
        adr.id.clone(),
        json!({"ingest":report,"mentions":traversal,"hierarchy":hierarchy,"brief":brief}),
    )
}

/// One fresh walkthrough: its scratch store, repository root and complete raw output.
struct Run {
    s: Scratch,
    root: PathBuf,
}

impl Run {
    fn new(name: &str) -> Self {
        let s = Scratch::new("kaban-walkthrough", name);
        let root = s.0.join("shop");
        std::fs::create_dir_all(&root).unwrap();
        Self { s, root }
    }

    fn six_steps(&self) -> Value {
        let (s, root) = (&self.s, self.root.as_path());
        let (mut mcp, commit, dir) = step1(s, root);
        let (arch, adr, step2_output) = step2(s, root, &mut mcp);
        let step3_output = step3(s, root, &mut mcp, &arch, &adr);
        let decision = step3_output["decision"].as_str().unwrap();
        let step4_output = step4(s, &mut mcp, &arch);
        let step5_output = step5(&mut mcp, decision, &arch, &adr);
        let step6_output = step6(s, root, &mut mcp, &dir, decision);
        assert!(mcp.finish().status.success());
        json!({"step1":{"commit":commit},"step2":step2_output,"step3":step3_output,
            "step4":step4_output,"step5":step5_output,"step6":step6_output})
    }

    /// EN-N4 same-database replay: re-ingesting identical inputs keeps every id and adds zero
    /// graph `ChangeLog` rows (the first pass absorbs Step 6's edited ADR).
    fn replay_is_idempotent(&self) {
        let ingest = || {
            cli(
                &self.s,
                &self.root,
                &["ingest", "docs/", "--link", "gazetteer"],
            )
        };
        let ids = || {
            ["Document", "Section", "Chunk", "Decision", "CodeRef"]
                .iter()
                .flat_map(|kind| nodes(&self.s, kind))
                .map(|n| n.id)
                .collect::<Vec<_>>()
        };
        ingest();
        let (before, seq) = (ids(), changes(&self.s));
        ingest();
        assert_eq!(ids(), before, "replay keeps every generated id");
        assert_eq!(
            changes(&self.s),
            seq,
            "replay adds zero graph ChangeLog rows"
        );
    }
}

/// Observation-time fields, excluded by exact key: they record when, not what.
const OBSERVATION_KEYS: &[&str] = &[
    "created_at",
    "checked_at",
    "first_seen",
    "last_seen",
    "elapsed_ms",
];
/// Fixture ids chosen by this test ([`fixed_id`]); they are inputs and compare verbatim.
const FIXED_PREFIX: &str = "01a17a03-5344-7000-8000-";

/// OWN-117 fresh-database comparison: each distinct generated UUID becomes `@n` at its first
/// appearance, so two runs compare byte for byte after relabelling. The mapping is a bijection
/// by construction, an edge to a different endpoint changes its label, and every other byte
/// (text, props, status, scores, provenance, offsets, order) compares verbatim.
fn relabel(v: &Value, root: &str, labels: &mut HashMap<String, String>) -> Value {
    match v {
        Value::String(text) => Value::String(relabel_str(text, root, labels)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|x| relabel(x, root, labels)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, x)| {
                    let x = if OBSERVATION_KEYS.contains(&k.as_str()) {
                        json!("<observed>")
                    } else if k == "author_date" {
                        // Git versions spell UTC as either Z or +00:00. Preserve the
                        // instant while comparing the existing RFC 3339 fixture.
                        let date = x.as_str().unwrap();
                        json!(
                            date.strip_suffix("+00:00")
                                .map_or_else(|| date.to_owned(), |utc| format!("{utc}Z"))
                        )
                    } else {
                        relabel(x, root, labels)
                    };
                    (relabel_str(k, root, labels), x)
                })
                .collect(),
        ),
        other => other.clone(),
    }
}

fn relabel_str(text: &str, root: &str, labels: &mut HashMap<String, String>) -> String {
    let text = text.replace(root, "<root>");
    let is_uuid = |t: &[u8]| {
        t.iter().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => *b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(b),
        })
    };
    let (bytes, mut out, mut i) = (text.as_bytes(), String::new(), 0);
    while i < bytes.len() {
        if bytes.len() - i >= 36 && is_uuid(&bytes[i..i + 36]) {
            let token = &text[i..i + 36];
            if token.starts_with(FIXED_PREFIX) {
                out.push_str(token);
            } else {
                let next = format!("@{}", labels.len() + 1);
                out.push_str(labels.entry(token.to_owned()).or_insert(next));
            }
            i += 36;
        } else {
            let c = text[i..].chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
    }
    out
}

#[test]
fn six_steps_over_real_mcp() {
    let first = Run::new("first");
    let raw_first = first.six_steps();
    first.replay_is_idempotent();
    let second = Run::new("second");
    let raw_second = second.six_steps();
    let mut a = relabel(
        &raw_first,
        first.s.0.to_str().unwrap(),
        &mut HashMap::default(),
    );
    let mut b = relabel(
        &raw_second,
        second.s.0.to_str().unwrap(),
        &mut HashMap::default(),
    );
    // OWN-121: only these runtime-memory observations are excluded; same-named fields
    // elsewhere still compare verbatim. Keep both raw runs, including on success.
    for observed in [&mut a, &mut b] {
        for path in [
            "/step2/ingest/gazetteer/build_hwm_delta",
            "/step2/ingest/gazetteer/build_peak_delta",
        ] {
            *observed.pointer_mut(path).unwrap() = json!("<observed>");
        }
    }
    for (name, raw) in [("first", &raw_first), ("second", &raw_second)] {
        let path = std::env::temp_dir().join(format!("kaban-walkthrough-{name}.json"));
        std::fs::write(&path, serde_json::to_string_pretty(raw).unwrap()).unwrap();
    }
    assert_eq!(
        serde_json::to_string_pretty(&a).unwrap(),
        serde_json::to_string_pretty(&b).unwrap(),
        "two fresh runs differ beyond generated ids"
    );
    insta::assert_json_snapshot!("steps", a);
}
