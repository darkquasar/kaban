//! Public-behaviour tests of the content/control field-class seam (core-spec S1, S4 as amended
//! by OWN-10; core-plan C3.0): `KabanApi::prepare_args` on the request side and
//! `KabanApi::gate_response` on the response side, plus the registry completeness check.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_api::control::{
    ControlEntry, Direction, LAYER_VALUES, METHOD_VALUES, REGISTRY, Shape, append_index,
    append_key, control_entry, tool_schemas,
};
use kaban_api::{
    ApiError, ForgetRequest, KabanApi, LinkRequest, LinkSpec, RememberRequest, TraverseRequest,
};
use kaban_core::{AssertionStatus, ClaimClass, Layer, Method, NodeId, Rank};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use schemars::{JsonSchema, schema_for};
use serde_json::{Map, Value, json};
use std::collections::BTreeSet;
use std::sync::Arc;

/// Hits the builtin credential rule `aws-access-key-id` (always reject) in every view.
const CANARY: &str = "AKIAQWERTYUIOPASDFGH";

/// A test policy whose only user rule rejects anything shaped like a UUID.
const UUID_REJECT: &str = "rules:\n  - id: uuid-reject\n    regex: '\\b[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\\b'\n    action: reject\n";

/// A policy whose one user rule masks the word `hunter2` (id `old-password`).
const REDACT_WORD: &str = "words:\n  - { term: hunter2, action: redact, id: old-password }\n";

#[test]
fn ingest_submit_control_ids_and_closed_report_values_are_registered() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new().to_string();
    api.prepare_args(
        "ingest_submit",
        &json!({"job":"a".repeat(64),"records":[{"evidence":[{"chunk_id":id}]}],
        "edges":[{"to_id":id}],"links":[{"to_id":id}],"about":[id],
        "pair_judgements":[{"pair":{"a":id,"b":id},"same":true}]}),
    )
    .unwrap();
    assert!(
        api.prepare_args("ingest_submit", &json!({"records":[{"local_id":id}]}))
            .is_err()
    );
    let malformed = format!("not-an-id-{id}");
    assert!(
        api.prepare_args("ingest_submit", &json!({"links":[{"to_id":malformed}]}))
            .is_err()
    );
    let report = json!({"job":"a".repeat(64),"status":"open","records":[{"id":id,"outcome":"revision_candidate"}],
        "revision_candidates":[{"id":id,"of":id}],"skipped":[{"reason":"unknown_target"}],
        "evidence_dropped":[{"reason":"quote_not_found"}]});
    assert_eq!(api.gate_response("ingest_submit", &report).unwrap(), report);
}

#[test]
fn ingest_finish_control_ids_and_closed_values_are_registered() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new().to_string();
    api.prepare_args("ingest_finish", &json!({"job":"a".repeat(64)}))
        .unwrap();
    let report = json!({"job":"a".repeat(64),"status":"finished",
        "reconcile":"complete","orphans":[id]});
    assert_eq!(api.gate_response("ingest_finish", &report).unwrap(), report);
    assert!(
        api.prepare_args("ingest_finish", &json!({"job":CANARY}))
            .is_err()
    );
    assert!(
        api.gate_response(
            "ingest_finish",
            &json!({"orphans":[format!("invalid-{id}")]})
        )
        .is_err()
    );
    assert!(
        api.gate_response("ingest_finish", &json!({"status":CANARY}))
            .is_err()
    );
    assert!(
        api.gate_response("ingest_finish", &json!({"reconcile":CANARY}))
            .is_err()
    );
}

fn api_with(policy_yaml: &str) -> KabanApi {
    api_and_store_with(policy_yaml).0
}

fn api_and_store_with(policy_yaml: &str) -> (KabanApi, Arc<SqliteStore>) {
    let file = Policy::parse(policy_yaml.as_bytes()).unwrap();
    let policy = Policy::from_file(file, "test".into()).unwrap();
    let store = Arc::new(SqliteStore::open_memory().unwrap());
    let signer = store.signer();
    (KabanApi::new(store.clone(), policy, signer, None), store)
}

fn reject_rules(r: Result<(), ApiError>) -> Vec<String> {
    match r {
        Err(ApiError::Rejected { rules, .. }) => rules,
        other => panic!("expected a denylist rejection, got {other:?}"),
    }
}

/// What a schema leaf admits at a path.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Leaf {
    /// A free string (`type: string`).
    Str,
    /// A closed vocabulary (`enum` or `const`), with its spellings.
    Enum(Vec<String>),
    /// An integer or number.
    Num,
    /// A free-form subtree (`true`, bare `object`, `additionalProperties: true`).
    Free,
}

/// Every leaf of `schema` (schemars 1 JSON Schema) with its path: object keys joined with
/// `.`, `[]` for array elements, `*` for free-form object members. A path appears once per
/// combinator branch that ends in a leaf there.
fn leaves(schema: &Value) -> Vec<(String, Leaf)> {
    let mut out = Vec::new();
    walk(schema, schema, "", &mut out);
    out
}

/// Every path at which a string may sit in `schema`: free strings, closed vocabularies and
/// free-form subtrees (recorded at their own path).
fn string_paths(schema: &Value) -> BTreeSet<String> {
    leaves(schema)
        .into_iter()
        .filter(|(_, leaf)| !matches!(leaf, Leaf::Num))
        .map(|(path, _)| path)
        .collect()
}

/// Every path at which a number sits in `schema`.
fn number_paths(schema: &Value) -> BTreeSet<String> {
    leaves(schema)
        .into_iter()
        .filter(|(_, leaf)| matches!(leaf, Leaf::Num))
        .map(|(path, _)| path)
        .collect()
}

/// Every closed-vocabulary path of `schema` with the union of its spellings over the
/// combinator branches (a mixed enum such as `PendingStatus` lists its unit variants in one
/// `enum` branch beside its object branches).
fn enum_paths(schema: &Value) -> Vec<(String, BTreeSet<String>)> {
    let mut by_path: std::collections::BTreeMap<String, BTreeSet<String>> =
        std::collections::BTreeMap::new();
    for (path, leaf) in leaves(schema) {
        if let Leaf::Enum(values) = leaf {
            by_path.entry(path).or_default().extend(values);
        }
    }
    by_path.into_iter().collect()
}

/// The schema walker builds paths with the seam's own segment rules (RC-2).
fn join(path: &str, seg: &str) -> String {
    let mut p = path.to_owned();
    append_key(&mut p, seg);
    p
}

fn join_index(path: &str) -> String {
    let mut p = path.to_owned();
    append_index(&mut p);
    p
}

fn walk(root: &Value, node: &Value, path: &str, out: &mut Vec<(String, Leaf)>) {
    let obj = match node {
        Value::Bool(true) => {
            out.push((path.to_owned(), Leaf::Free));
            return;
        }
        Value::Bool(false) => return,
        Value::Object(o) => o,
        other => panic!("unexpected schema node at {path}: {other}"),
    };
    if let Some(Value::String(r)) = obj.get("$ref") {
        let name = r
            .strip_prefix("#/$defs/")
            .or_else(|| r.strip_prefix("#/definitions/"))
            .unwrap_or_else(|| panic!("unsupported $ref {r}"));
        let defs = root
            .get("$defs")
            .or_else(|| root.get("definitions"))
            .unwrap();
        walk(root, &defs[name], path, out);
        return;
    }
    for combinator in ["anyOf", "oneOf", "allOf"] {
        if let Some(Value::Array(branches)) = obj.get(combinator) {
            for b in branches {
                walk(root, b, path, out);
            }
        }
    }
    // a schema that constrains nothing (`serde_json::Value`: description only) is free-form
    let constraining = [
        "type",
        "anyOf",
        "oneOf",
        "allOf",
        "properties",
        "items",
        "enum",
        "const",
        "additionalProperties",
    ];
    if !constraining.iter().any(|k| obj.contains_key(*k)) {
        out.push((path.to_owned(), Leaf::Free));
        return;
    }
    let types: Vec<&str> = match obj.get("type") {
        Some(Value::String(t)) => vec![t.as_str()],
        Some(Value::Array(ts)) => ts.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    // a closed vocabulary: the `enum` spellings, or the one `const` spelling
    let vocabulary: Option<Vec<String>> = match (obj.get("enum"), obj.get("const")) {
        (Some(Value::Array(values)), _) => Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        (None, Some(Value::String(c))) => Some(vec![c.clone()]),
        _ => None,
    };
    if let Some(values) = vocabulary {
        out.push((path.to_owned(), Leaf::Enum(values)));
    } else if types.contains(&"string") {
        out.push((path.to_owned(), Leaf::Str));
    }
    if types.contains(&"integer") || types.contains(&"number") {
        out.push((path.to_owned(), Leaf::Num));
    }
    if let Some(Value::Object(props)) = obj.get("properties") {
        for (k, v) in props {
            walk(root, v, &join(path, k), out);
        }
    }
    match obj.get("items") {
        Some(Value::Array(items)) => {
            for v in items {
                walk(root, v, &join_index(path), out);
            }
        }
        Some(v) => walk(root, v, &join_index(path), out),
        None => {}
    }
    match obj.get("additionalProperties") {
        Some(Value::Bool(false)) | None => {}
        Some(v) => walk(root, v, &join(path, "*"), out),
    }
    // a bare object with no declared members is free-form
    if types.contains(&"object")
        && !obj.contains_key("properties")
        && !obj.contains_key("additionalProperties")
    {
        out.push((join(path, "*"), Leaf::Free));
    }
}

/// A minimal document with `leaf` at `path` (`links[].to` -> `{"links": [{"to": leaf}]}`).
fn document_at(path: &str, leaf: Value) -> Value {
    fn build(segs: &[&str], leaf: Value) -> Value {
        let Some((first, rest)) = segs.split_first() else {
            return leaf;
        };
        let mut key = *first;
        let mut arrays = 0;
        while let Some(k) = key.strip_suffix("[]") {
            key = k;
            arrays += 1;
        }
        let mut inner = build(rest, leaf);
        for _ in 0..arrays {
            inner = Value::Array(vec![inner]);
        }
        let key = if key == "*" { "free" } else { key };
        let mut m = Map::new();
        m.insert(key.to_owned(), inner);
        Value::Object(m)
    }
    if path.is_empty() {
        return leaf;
    }
    build(&path.split('.').collect::<Vec<_>>(), leaf)
}

#[test]
fn canary_in_every_content_input_path_of_every_tool_is_rejected() {
    let api = api_with("");
    let mut checked = 0;
    for t in tool_schemas() {
        let input = serde_json::to_value(&t.input).unwrap();
        for path in string_paths(&input) {
            if control_entry(t.tool, Direction::Request, &path).is_some() {
                continue;
            }
            let doc = document_at(&path, Value::String(CANARY.into()));
            let rules = reject_rules(api.prepare_args(t.tool, &doc));
            assert!(
                rules.iter().any(|r| r == "aws-access-key-id"),
                "{}:{path}: {rules:?}",
                t.tool
            );
            checked += 1;
        }
    }
    // 8 tools, 34 string or free-form input paths at C3.0, 7 of them control
    assert!(checked >= 27, "only {checked} content paths were exercised");
}

#[test]
fn valid_node_id_passes_pre_parse_scan_unscanned_while_same_string_in_text_is_rejected() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new();
    let args = json!({"from": id.as_str(), "rel": "RELATES_TO", "to": id.as_str()});
    api.prepare_args("link", &args)
        .expect("node ids are control: validated, not scanned");
    let rules =
        reject_rules(api.prepare_args("remember", &json!({"kind": "Fact", "text": id.as_str()})));
    assert_eq!(rules, vec!["uuid-reject".to_string()]);
}

#[test]
fn control_shaped_value_in_a_content_field_is_still_scanned() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new();
    // a node id is content in `summary`, in `rel` and in a traverse `rels` entry
    for (tool, args) in [
        (
            "remember",
            json!({"kind": "Fact", "text": "note", "summary": id.as_str()}),
        ),
        (
            "link",
            json!({"from": id.as_str(), "rel": id.as_str(), "to": id.as_str()}),
        ),
        (
            "traverse",
            json!({"start_ids": [id.as_str()], "rels": [id.as_str()]}),
        ),
    ] {
        let rules = reject_rules(api.prepare_args(tool, &args));
        assert_eq!(rules, vec!["uuid-reject".to_string()], "{tool}");
    }
}

#[test]
fn malformed_control_value_carrying_a_canary_is_rejected_and_never_echoed() {
    let api = api_with("");
    for (tool, args) in [
        ("forget", json!({"id": CANARY})),
        (
            "forget",
            json!({"id": NodeId::new().as_str(), "mode": CANARY}),
        ),
        (
            "link",
            json!({"from": CANARY, "rel": "RELATES_TO", "to": NodeId::new().as_str()}),
        ),
        ("traverse", json!({"start_ids": [CANARY]})),
        (
            "remember",
            json!({"kind": "Fact", "text": "n", "links": [{"rel": "ABOUT", "to": CANARY}]}),
        ),
    ] {
        let err = api.prepare_args(tool, &args).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("aws-access-key-id"), "{tool}: {text}");
        assert!(!text.contains(CANARY), "{tool} echoed the canary: {text}");
    }
    // a UUID that is not a node id (version 4) fails the shape and is scanned as content
    let api = api_with(UUID_REJECT);
    let mut v4 = NodeId::new().as_str().to_owned();
    v4.replace_range(14..15, "4");
    let rules = reject_rules(api.prepare_args("forget", &json!({"id": v4})));
    assert_eq!(rules, vec!["uuid-reject".to_string()]);
}

#[test]
fn response_gate_validates_own_ids_but_still_withholds_a_card_whose_text_matches() {
    let api = api_with(UUID_REJECT);
    let created = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "a plain note".into(),
            ..Default::default()
        })
        .unwrap();
    let value = serde_json::to_value(&created).unwrap();
    let gated = api
        .gate_response("remember", &value)
        .expect("the response's own id is control, not content");
    assert_eq!(gated["id"], json!(created.id));

    // the same rule still withholds a recall card whose content carries a UUID
    let card = json!({
        "count": 1,
        "results": [{
            "id": created.id,
            "kind": "Fact",
            "lifecycle": "active",
            "governance": "none",
            "created_at": "2026-10-26T12:22:26Z",
            "text": format!("see {}", created.id),
            "workspace_id": "default",
        }]
    });
    let rules = reject_rules(api.gate_response("recall", &card).map(|_| ()));
    assert_eq!(rules, vec!["uuid-reject".to_string()]);
}

/// RC-2: an object key that spells a control path (`links[].to`, `start_ids[]`, a nested
/// `links[]`) never aliases the registered path: the value under it is content and scanned,
/// inside free-form maps too.
#[test]
fn crafted_keys_spelling_a_control_path_are_content() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new();
    let id = id.as_str();
    for (tool, args) in [
        // literal dots and brackets in one key
        (
            "remember",
            json!({"kind": "Fact", "text": "n", "links[].to": id}),
        ),
        // a literal `[]` key around the real last segment
        (
            "remember",
            json!({"kind": "Fact", "text": "n", "links[]": {"to": id}}),
        ),
        ("traverse", json!({"start_ids[]": id})),
        // the real key with the wrong container shape
        (
            "remember",
            json!({"kind": "Fact", "text": "n", "links": {"to": id}}),
        ),
        // crafted keys inside free-form maps
        (
            "remember",
            json!({"kind": "Fact", "text": "n", "props": {"links[].to": id}}),
        ),
        (
            "link",
            json!({"from": id, "rel": "R", "to": id, "props": {"from": id}}),
        ),
    ] {
        let rules = reject_rules(api.prepare_args(tool, &args));
        assert_eq!(rules, vec!["uuid-reject".to_string()], "{tool}: {args}");
    }
}

/// RC-1: the typed handlers and the read projection use the same control-aware gate as the
/// seam, so a policy whose rule matches UUIDs does not break calls that only name kaban's own
/// ids (`remember` inline links, `link`, `traverse` start ids and response edges, `forget`).
#[test]
fn typed_handlers_and_projections_validate_registered_ids_under_a_uuid_reject_policy() {
    let api = api_with(UUID_REJECT);
    let a = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "node a".into(),
            ..Default::default()
        })
        .unwrap()
        .id;
    let b = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: "node b".into(),
            links: vec![LinkSpec {
                rel: "RELATES_TO".into(),
                to: Some(a.clone()),
                from: None,
                confidence: None,
            }],
            ..Default::default()
        })
        .expect("inline link ids are control");
    assert_eq!(b.links_created, 1, "{b:?}");
    let b = b.id;
    let edge = api
        .link(LinkRequest {
            from: a.clone(),
            rel: "ABOUT".into(),
            to: b.clone(),
            ..Default::default()
        })
        .expect("link ids are control");
    assert_eq!(
        (edge.from.as_str(), edge.to.as_str()),
        (a.as_str(), b.as_str())
    );
    let t = api
        .traverse(TraverseRequest {
            start_ids: vec![a.clone()],
            ..Default::default()
        })
        .expect("traverse start ids are control");
    assert!(t.nodes.iter().any(|n| n.id == b), "{t:?}");
    assert!(
        t.edges.iter().any(|e| e.id == edge.id),
        "edge ids are control on the response side: {t:?}"
    );
    let gone = api
        .forget(ForgetRequest {
            id: a.clone(),
            mode: None,
        })
        .expect("forget id is control");
    assert_eq!(gone.id, a);
    // the same rule still rejects a UUID in content
    let err = api
        .remember(RememberRequest {
            kind: "Fact".into(),
            text: format!("see {b}"),
            ..Default::default()
        })
        .unwrap_err();
    assert!(matches!(err, ApiError::Rejected { .. }), "{err}");
}

/// Last path segments core-spec S1 lists as content; a registry entry ending in one of them is
/// a declaration error, whatever its shape.
const CONTENT_NAMES: &[&str] = &[
    "kind",
    "rel",
    "rels",
    "kinds",
    "node_kinds",
    "text",
    "summary",
    "tags",
    "query",
    "workspace_id",
    "path_scope",
    "name",
    "description",
    "reason",
    "repo",
    "commit",
    "tool",
    "relative_path",
    "name_path",
    "info",
    "agent_id",
    "session_id",
    "source_session",
    "signer",
    "host",
    "props",
    "*",
];

/// RU3-8: the exact code-side entries whose last segment core-spec S1 lists as content
/// (`kind`, `commit`) and that stage1-spec §6.1 / G1-S2 nevertheless classify as control: the
/// response `snapshot.commit` of the six `code_*` tools and of the `kaban://code-index` body,
/// and the card `kind` at each card position (`control.rs` `card_rows` / `list_rows`). A new
/// entry ending in one of those segments is added here by name or refused.
const CODE_CONTROL_ALLOWANCE: &[(&str, Direction, &str)] = &[
    ("code_implementers", Direction::Response, "snapshot.commit"),
    ("code_members", Direction::Response, "snapshot.commit"),
    ("code_definition", Direction::Response, "snapshot.commit"),
    ("code_references", Direction::Response, "snapshot.commit"),
    ("code_tests_for", Direction::Response, "snapshot.commit"),
    ("code_neighbours", Direction::Response, "snapshot.commit"),
    ("code_index", Direction::Response, "snapshot.commit"),
    ("code_implementers", Direction::Response, "results[].kind"),
    ("code_members", Direction::Response, "results[].kind"),
    ("code_definition", Direction::Response, "definition.kind"),
    (
        "code_references",
        Direction::Response,
        "by_enclosing[].enclosing.kind",
    ),
    ("code_tests_for", Direction::Response, "tests[].test.kind"),
    ("code_neighbours", Direction::Response, "nodes[].kind"),
    ("code_context", Direction::Response, "snapshot.commit"),
    ("code_context", Direction::Response, "definition.kind"),
    ("code_context", Direction::Response, "callers[].kind"),
    ("code_context", Direction::Response, "callees[].kind"),
    ("code_context", Direction::Response, "tests[].test.kind"),
    ("code_context", Direction::Response, "last_change.commit"),
    // EN-E6 names `embed_submit.model.commit` `Hex{40,40}`: the producer's revision in its
    // resolved form (SO-2), exactly 40 hex, never a caller's unresolved `CodeRef` commit
    ("embed_submit", Direction::Request, "model.commit"),
];

/// RE-15 (EN-F50, EN-E6; engine-plan E2.11 default 1): the closed-vocabulary paths (a schemars
/// node carrying `enum`) that are deliberately *content*, by exact name, each with the reason
/// it carries no `Shape::Enum` row. Every other enum path of every tool must have a row whose
/// spellings equal the schema's exactly (`every_closed_vocabulary_path_is_rowed_or_allowed`),
/// so a new enum-typed field, or a new variant of a rowed one, fails that test until the row
/// moves or the path is named here. Free strings stay content by default (OWN-10).
///
/// The response-side closed vocabularies kaban itself emits (card `lifecycle`, `governance`
/// and `provenance.store`, the `follow_ups[].tool` names, `remember` `status`, `stats.stores[]`,
/// `anchored[].kind`) are content on purpose: the response gate scans them
/// as text and the adapter withholds a result whose enum value a redaction reshaped
/// (`crates/kaban/src/mcp/mod.rs`, review R3-9), so a policy word rule is honoured on every
/// string that leaves. Rowing them would make them unmaskable: an owner decision, not a lane's
/// (E2.11 report, deviation).
const CONTENT_ENUM_ALLOWANCE: &[(&str, Direction, &str)] = &[
    (
        "remember",
        Direction::Response,
        "candidate_duplicates[].governance",
    ),
    (
        "remember",
        Direction::Response,
        "candidate_duplicates[].lifecycle",
    ),
    (
        "remember",
        Direction::Response,
        "candidate_duplicates[].provenance.store",
    ),
    ("remember", Direction::Response, "status"),
    ("recall", Direction::Response, "follow_ups[].tool"),
    ("recall", Direction::Response, "results[].governance"),
    ("recall", Direction::Response, "results[].lifecycle"),
    ("recall", Direction::Response, "results[].provenance.store"),
    ("traverse", Direction::Response, "anchored[].kind"),
    ("traverse", Direction::Response, "nodes[].governance"),
    ("traverse", Direction::Response, "nodes[].lifecycle"),
    ("traverse", Direction::Response, "nodes[].provenance.store"),
    ("forget", Direction::Response, "governance"),
    ("forget", Direction::Response, "lifecycle"),
    ("stats", Direction::Response, "stores[]"),
    ("stats", Direction::Response, "layers[].layer"),
    ("stats", Direction::Response, "layers[].producer_layers[]"),
    ("stats", Direction::Response, "layers[].last_outcome"),
    (
        "code_implementers",
        Direction::Response,
        "follow_ups[].tool",
    ),
    ("code_members", Direction::Response, "follow_ups[].tool"),
    ("code_definition", Direction::Response, "follow_ups[].tool"),
    ("code_references", Direction::Response, "follow_ups[].tool"),
    ("code_tests_for", Direction::Response, "follow_ups[].tool"),
    ("code_context", Direction::Response, "follow_ups[].tool"),
];

/// Whether `(tool, direction, path)` is named in [`CONTENT_ENUM_ALLOWANCE`].
fn content_enum(tool: &str, direction: Direction, path: &str) -> bool {
    CONTENT_ENUM_ALLOWANCE
        .iter()
        .any(|(t, d, p)| *t == tool && *d == direction && *p == path)
}

/// The RE-15 check over one tool: every closed-vocabulary path of its request and response
/// has a `Shape::Enum` row with exactly the schema's spellings, or is named content in
/// `allowance`. Returns one line per defect, `tool:direction:path (why)`, empty when the tool
/// passes.
fn unrowed_enum_paths(
    t: &kaban_api::control::ToolSchemas,
    allowance: &[(&str, Direction, &str)],
) -> Vec<String> {
    let mut out = Vec::new();
    for (direction, schema) in [
        (Direction::Request, &t.input),
        (Direction::Response, &t.output),
    ] {
        for (path, values) in enum_paths(&serde_json::to_value(schema).unwrap()) {
            let allowed = allowance
                .iter()
                .any(|(tool, d, p)| *tool == t.tool && *d == direction && *p == path);
            let row = control_entry(t.tool, direction, &path);
            let why = match (allowed, row) {
                (true, None) => continue,
                (true, Some(_)) => "named content in the allowance but carries a row",
                (false, None) => "a closed vocabulary with no Shape::Enum row",
                (false, Some(e)) => match e.shape {
                    Shape::Enum(spellings) => {
                        let rowed: BTreeSet<String> =
                            spellings.iter().map(|s| (*s).to_owned()).collect();
                        if rowed == values {
                            continue;
                        }
                        "the row's spellings differ from the schema's"
                    }
                    _ => "the row is not a Shape::Enum",
                },
            };
            out.push(format!(
                "{}:{direction:?}:{path} ({why}; schema enum {values:?})",
                t.tool
            ));
        }
    }
    out
}

/// RE-15 negative: a test-only tool whose request carries one enum field with no row fails the
/// check with that field's path, and only that path (the free string beside it is content by
/// default); the same tool passes once the path is named content.
#[test]
fn a_tool_schema_with_an_unregistered_enum_field_fails_the_completeness_check_by_path() {
    use schemars::{JsonSchema, schema_for};

    #[derive(serde::Deserialize, JsonSchema)]
    #[serde(rename_all = "lowercase")]
    #[allow(dead_code)]
    enum CanaryMode {
        Fast,
        Slow,
    }
    #[derive(serde::Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct CanaryIn {
        mode: CanaryMode,
        text: String,
    }
    #[derive(serde::Deserialize, JsonSchema)]
    #[allow(dead_code)]
    struct CanaryOut {
        id: String,
    }
    let canary = kaban_api::control::ToolSchemas {
        tool: "canary_tool",
        input: schema_for!(CanaryIn),
        output: schema_for!(CanaryOut),
    };
    let defects = unrowed_enum_paths(&canary, &[]);
    assert_eq!(defects.len(), 1, "{defects:?}");
    assert!(
        defects[0].starts_with("canary_tool:Request:mode ("),
        "{defects:?}"
    );
    assert!(defects[0].contains(r#"{"fast", "slow"}"#), "{defects:?}");
    // named content by exact path: passes; a different path does not cover it
    assert_eq!(
        unrowed_enum_paths(&canary, &[("canary_tool", Direction::Request, "mode")]),
        Vec::<String>::new()
    );
    assert_eq!(
        unrowed_enum_paths(&canary, &[("canary_tool", Direction::Response, "mode")]).len(),
        1
    );
}

/// RE-15, the production side: every closed-vocabulary path of every tool is rowed with the
/// schema's exact spellings or named content in [`CONTENT_ENUM_ALLOWANCE`]; the allowance
/// itself is live (every entry is an enum path of a declared tool).
#[test]
fn every_closed_vocabulary_path_is_rowed_or_allowed() {
    let tools = tool_schemas();
    let defects: Vec<String> = tools
        .iter()
        .flat_map(|t| unrowed_enum_paths(t, CONTENT_ENUM_ALLOWANCE))
        .collect();
    assert!(defects.is_empty(), "{}", defects.join("\n"));
    for (tool, direction, path) in CONTENT_ENUM_ALLOWANCE {
        let t = tools
            .iter()
            .find(|t| t.tool == *tool)
            .unwrap_or_else(|| panic!("allowance names unknown tool {tool}"));
        let schema = match direction {
            Direction::Request => &t.input,
            Direction::Response => &t.output,
        };
        assert!(
            enum_paths(&serde_json::to_value(schema).unwrap())
                .iter()
                .any(|(p, _)| p == path),
            "stale content allowance {tool}:{direction:?}:{path}"
        );
        assert!(content_enum(tool, *direction, path));
    }
}

/// Whether `e` is one of the [`CODE_CONTROL_ALLOWANCE`] entries (exact tool, direction, path).
fn admitted(e: &ControlEntry) -> bool {
    CODE_CONTROL_ALLOWANCE
        .iter()
        .any(|(tool, direction, path)| {
            *tool == e.tool && *direction == e.direction && *path == e.path
        })
}

/// RU3-8 negative: the allowance is exact. A request-side `commit` leaf of a code tool (a
/// caller-named commit under `target`) is not admitted, so such an entry would fail the
/// completeness test as content; the response row it would mirror is admitted, and no such
/// request entry exists in the registry.
#[test]
fn code_request_commit_leaf_is_refused() {
    let refused = ControlEntry {
        tool: "code_definition",
        direction: Direction::Request,
        path: "target.commit",
        shape: Shape::Hex { min: 7, max: 64 },
        secret: false,
        issuing: false,
    };
    assert!(!admitted(&refused));
    assert!(CONTENT_NAMES.contains(&"commit"));
    assert!(control_entry("code_definition", Direction::Request, "target.commit").is_none());
    let response = control_entry("code_definition", Direction::Response, "snapshot.commit")
        .expect("the response row exists");
    assert!(admitted(response));
    // every allowance entry is a live registry row (no stale allowance)
    for (tool, direction, path) in CODE_CONTROL_ALLOWANCE {
        assert!(
            control_entry(tool, *direction, path).is_some(),
            "stale allowance {tool}:{direction:?}:{path}"
        );
    }
}

#[test]
fn every_control_entry_points_at_a_string_path_of_its_tool_and_names_no_content_field() {
    let tools = tool_schemas();
    let mut seen = BTreeSet::new();
    for e in REGISTRY {
        let t = tools
            .iter()
            .find(|t| t.tool == e.tool)
            .unwrap_or_else(|| panic!("control entry for unknown tool {}", e.tool));
        let schema = match e.direction {
            Direction::Request => &t.input,
            Direction::Response => &t.output,
        };
        let schema = serde_json::to_value(schema).unwrap();
        // an `Int` row declares a number path (E2.11, `embed_submit.model.dims`); every other
        // shape validates a string
        let paths = if matches!(e.shape, Shape::Int { .. }) {
            number_paths(&schema)
        } else {
            string_paths(&schema)
        };
        assert!(
            !e.path.contains('\\'),
            "{}:{} contains the key escape; a registered path is made of literal keys",
            e.tool,
            e.path
        );
        assert!(
            paths.contains(e.path),
            "stale control entry {}:{:?}:{} (paths of its class: {paths:?})",
            e.tool,
            e.direction,
            e.path
        );
        let last = e.path.rsplit('.').next().unwrap().trim_end_matches("[]");
        // core-spec S1 lists `kind` (a node kind name) and `commit` (a `CodeRef` field) as
        // content; on the code side stage1-spec §6.1 and G1-S2 classify the card `kind` (the
        // closed `symbol | local | file`) and the response `snapshot.commit` (`Hex{7,64}`) as
        // control: exactly the entries of `CODE_CONTROL_ALLOWANCE` (RU3-8), nothing else
        assert!(
            admitted(e)
                || (e.tool == "ingest_claim"
                    && e.direction == Direction::Response
                    && matches!(
                        e.path,
                        "brief.chunk_plan.chunks[].kind" | "brief.next[].tool"
                    ))
                || matches!(
                    (e.tool, e.direction, e.path),
                    (
                        "ingest_next",
                        Direction::Response,
                        "chunk.kind" | "chunks[].kind"
                    )
                )
                || matches!(
                    (e.tool, e.direction, e.path),
                    (
                        "ingest_brief",
                        Direction::Response,
                        "chunk_plan.chunks[].kind" | "next[].tool"
                    )
                )
                || matches!(
                    (e.tool, e.direction, e.path),
                    (
                        "ingest_submit",
                        Direction::Response,
                        "skipped[].reason" | "evidence_dropped[].reason"
                    )
                )
                || !CONTENT_NAMES.contains(&last),
            "{}:{} is content per core-spec S1 and cannot be control",
            e.tool,
            e.path
        );
        assert!(
            seen.insert((e.tool, e.direction, e.path)),
            "duplicate control entry {}:{:?}:{}",
            e.tool,
            e.direction,
            e.path
        );
        // secrets (RC-5): kaban-issued tokens only; a request entry never issues; the issuing
        // response entry is the one place a secret leaves
        if e.secret {
            assert_eq!(e.shape, Shape::Token64, "{}:{}", e.tool, e.path);
        }
        if e.direction == Direction::Request {
            assert!(
                !e.issuing,
                "{}:{} cannot issue on the request side",
                e.tool, e.path
            );
        }
        assert!(
            e.secret || !e.issuing,
            "{}:{} issuing without secret",
            e.tool,
            e.path
        );
    }
    // the one kaban-issued secret (the `accept` confirmation token) left with governance
    let issuing: Vec<_> = REGISTRY.iter().filter(|e| e.issuing).collect();
    assert!(issuing.is_empty(), "no issuing response entry: {issuing:?}");
    // every string path of every tool is either listed above or content by default; the walk
    // itself must see every tool on both sides
    for t in &tools {
        let input = string_paths(&serde_json::to_value(&t.input).unwrap());
        let output = string_paths(&serde_json::to_value(&t.output).unwrap());
        assert!(!input.is_empty() && !output.is_empty(), "{}", t.tool);
        for path in input.iter().chain(output.iter()) {
            assert!(
                !path.is_empty(),
                "{}: a whole side cannot be one string",
                t.tool
            );
        }
    }
}

#[test]
fn ingestion_chunk_kind_controls_do_not_admit_record_kind_content() {
    assert!(control_entry("ingest_next", Direction::Response, "chunk.kind").is_some());
    assert!(control_entry("ingest_next", Direction::Response, "chunks[].kind").is_some());
    assert!(control_entry("ingest_next", Direction::Request, "records[].kind").is_none());
    assert!(
        api_with("{}")
            .prepare_args("ingest_next", &json!({"records": [{"kind": CANARY}]}))
            .is_err()
    );
}

#[test]
fn shapes_are_exact() {
    let id = NodeId::new();
    assert!(Shape::NodeId.accepts(id.as_str()));
    assert!(!Shape::NodeId.accepts(&id.as_str().to_uppercase().replace('-', "")));
    assert!(Shape::Hex { min: 7, max: 64 }.accepts("8d10f88"));
    assert!(!Shape::Hex { min: 7, max: 64 }.accepts("8D10F88"));
    assert!(!Shape::Hex { min: 7, max: 64 }.accepts("8d10f8"));
    let token = "a".repeat(64);
    assert!(Shape::Token64.accepts(&token));
    assert!(!Shape::Token64.accepts(&token[..63]));
    assert!(!Shape::Token64.accepts(&format!("{}g", &token[..63])));
    assert!(Shape::Enum(&["out", "in"]).accepts("in"));
    assert!(!Shape::Enum(&["out", "in"]).accepts("In"));
    let dims = Shape::Int { min: 1, max: 4096 };
    assert!(dims.accepts("1") && dims.accepts("4096") && dims.accepts("768"));
    assert!(!dims.accepts("0") && !dims.accepts("4097") && !dims.accepts("-1"));
    assert!(!dims.accepts("12a") && !dims.accepts("") && !dims.accepts(CANARY));
}

/// The schemars spellings of a closed vocabulary at the root of its schema (EN-E7 `Verify:`):
/// one `enum`, or one `const` per documented variant (schemars 1), read through the same walker
/// the completeness check uses.
fn vocabulary_schema<T: JsonSchema>() -> BTreeSet<String> {
    let v = serde_json::to_value(schema_for!(T)).unwrap();
    let paths = enum_paths(&v);
    let (_, values) = paths
        .into_iter()
        .find(|(p, _)| p.is_empty())
        .unwrap_or_else(|| panic!("no closed vocabulary at the root: {v}"));
    values
}

/// EN-E7: every `kaban-core` vocabulary has `schemars` output equal to its `ALL` spellings,
/// and the `traverse` `edge_filter.layers[]` / `methods[]` rows spell exactly those lists, so
/// adding a variant fails here until the row moves. The spec's `layer` list (`fts`, `fuzzy`
/// included) is the one asserted.
#[test]
fn vocabularies_have_schema_output_equal_to_their_spellings_and_the_traverse_rows_match() {
    fn spellings<T: Copy>(all: &[T], f: impl Fn(T) -> &'static str) -> Vec<String> {
        all.iter().map(|&v| f(v).to_owned()).collect()
    }
    fn set(v: &[String]) -> BTreeSet<String> {
        v.iter().cloned().collect()
    }
    let classes = spellings(ClaimClass::ALL, ClaimClass::as_str);
    assert_eq!(vocabulary_schema::<ClaimClass>(), set(&classes));
    assert_eq!(classes.len(), ClaimClass::ALL.len());
    let ranks = spellings(Rank::ALL, Rank::as_str);
    assert_eq!(vocabulary_schema::<Rank>(), set(&ranks));
    let layers = spellings(Layer::ALL, Layer::as_str);
    assert_eq!(vocabulary_schema::<Layer>(), set(&layers));
    let methods = spellings(Method::ALL, Method::as_str);
    assert_eq!(vocabulary_schema::<Method>(), set(&methods));
    let statuses = spellings(AssertionStatus::ALL, AssertionStatus::as_str);
    assert_eq!(vocabulary_schema::<AssertionStatus>(), set(&statuses));
    assert_eq!(LAYER_VALUES, layers);
    assert_eq!(METHOD_VALUES, methods);
    for s in [
        "fts",
        "fuzzy",
        "gazetteer",
        "embed",
        "gliner",
        "llm",
        "script",
        "human",
        "v4",
        "reconcile",
    ] {
        assert!(LAYER_VALUES.contains(&s), "EN-E7 layer {s} missing");
    }
    assert_eq!(LAYER_VALUES.len(), 10);
    for (path, values) in [
        ("edge_filter.layers[]", LAYER_VALUES),
        ("edge_filter.methods[]", METHOD_VALUES),
    ] {
        let row = control_entry("traverse", Direction::Request, path).expect(path);
        assert_eq!(row.shape, Shape::Enum(values), "{path}");
    }
}

/// EN-E6: a valid layer or method spelling under `edge_filter` is validated, not scanned
/// (a word rule naming it cannot reject the request), while an unknown spelling is scanned as
/// content and then refused by the handler; `node_kinds[]`, `rels[]` and `kind_set` stay
/// content (a canary under `node_kinds[0]` is `Rejected` before any ontology lookup).
#[test]
fn traverse_layers_and_methods_are_control_while_node_kinds_stay_content() {
    let api = api_with("words:\n  - { term: gazetteer, action: reject, id: no-gaz }\n");
    let id = NodeId::new();
    api.prepare_args(
        "traverse",
        &json!({"start_ids": [id.as_str()], "edge_filter": {"layers": ["gazetteer"], "methods": ["alias"]}}),
    )
    .expect("a vocabulary spelling is control");
    let rules = reject_rules(api.prepare_args(
        "traverse",
        &json!({"start_ids": [id.as_str()], "node_kinds": ["gazetteer"]}),
    ));
    assert_eq!(rules, vec!["no-gaz".to_owned()], "node_kinds[] is content");
    assert!(control_entry("traverse", Direction::Request, "node_kinds[]").is_none());
    assert!(control_entry("traverse", Direction::Request, "rels[]").is_none());
    assert!(control_entry("traverse", Direction::Request, "kind_set").is_none());
    // the canary under `node_kinds[0]` is a policy rejection, never `unknown_kind`
    let err = api
        .prepare_args(
            "traverse",
            &json!({"start_ids": [id.as_str()], "node_kinds": [CANARY]}),
        )
        .unwrap_err();
    assert!(matches!(err, ApiError::Rejected { .. }), "{err}");
    assert!(!err.to_string().contains("unknown_kind"));
    // an unknown layer spelling is scanned as content, then refused by the typed handler
    let api = api_with(UUID_REJECT);
    let rules = reject_rules(api.prepare_args(
        "traverse",
        &json!({"start_ids": [id.as_str()], "edge_filter": {"layers": [id.as_str()]}}),
    ));
    assert_eq!(rules, vec!["uuid-reject".to_owned()]);
    let err = api
        .traverse(TraverseRequest {
            start_ids: vec![id.as_str().to_owned()],
            edge_filter: Some(kaban_api::EdgeFilter {
                layers: vec!["nope".into()],
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap_err();
    assert!(err.to_string().contains("unknown_layer"), "{err}");
}

/// EN-E6 (E2.11 default 2): the reserved `embed_submit` shape. `model.name` is content (a
/// canary there is rejected), `rows[].node_rid` is a node id (validated under a UUID-rejecting
/// policy where the same id in `model.name` is rejected), `model.commit` is 40 hex, and the
/// tool is declared but not advertised (`RESERVED`); `rank_reason` and the `ontology load`
/// file text have no row (content by default) and `ask` has no entry anywhere.
#[test]
fn embed_submit_is_reserved_with_its_rows_and_nothing_names_ask_rank_reason_or_ontology_load() {
    let api = api_with(UUID_REJECT);
    let id = NodeId::new();
    let commit = "a".repeat(40);
    api.prepare_args(
        "embed_submit",
        &json!({"model": {"name": "minilm", "dims": 384, "dtype": "f32", "normalize": true, "commit": commit},
                "rows": [{"node_rid": id.as_str(), "vector": [0.1, 0.2]}]}),
    )
    .expect("node_rid is control");
    let rules =
        reject_rules(api.prepare_args("embed_submit", &json!({"model": {"name": id.as_str()}})));
    assert_eq!(
        rules,
        vec!["uuid-reject".to_owned()],
        "model.name is content"
    );
    let api = api_with("");
    let err = api
        .prepare_args("embed_submit", &json!({"model": {"name": CANARY}}))
        .unwrap_err();
    assert!(err.to_string().contains("aws-access-key-id"), "{err}");
    assert_eq!(
        control_entry("embed_submit", Direction::Request, "model.commit").map(|e| e.shape),
        Some(Shape::Hex { min: 40, max: 40 })
    );
    assert_eq!(
        control_entry("embed_submit", Direction::Request, "model.dims").map(|e| e.shape),
        Some(Shape::Int { min: 1, max: 4096 })
    );
    assert!(control_entry("embed_submit", Direction::Request, "model.name").is_none());
    assert!(kaban_api::control::RESERVED.contains(&"embed_submit"));
    assert!(tool_schemas().iter().any(|t| t.tool == "embed_submit"));
    // content by default, asserted by name
    assert!(!REGISTRY.iter().any(|e| e.path.ends_with("rank_reason")));
    assert!(!REGISTRY.iter().any(|e| e.tool.starts_with("ontology")));
    assert!(
        !tool_schemas()
            .iter()
            .any(|t| t.tool.starts_with("ontology"))
    );
    // `ask` is absent, not stubbed (OWN-68 SC-02)
    assert!(!tool_schemas().iter().any(|t| t.tool == "ask"));
    assert!(!REGISTRY.iter().any(|e| e.tool == "ask"));
}

/// EN-E5: the four gate entries are callable from outside the crate. The per-field gate
/// masks a redact hit and names the rule, rejects a canary without echoing it; the JSON props
/// gate masks inside a tree; the joined-view gate accepts clean text and rejects a canary;
/// the EN-F3 validator is reachable.
#[test]
fn the_public_gates_mask_redact_hits_and_reject_canaries_from_outside_the_crate() {
    let api = api_with(REDACT_WORD);
    let (masked, rules) = api.gate_field("t", "text", "pw is hunter2 here").unwrap();
    assert!(!masked.contains("hunter2"), "{masked}");
    assert!(masked.contains("[REDACTED:old-password]"), "{masked}");
    assert_eq!(rules, vec!["text:old-password".to_owned()]);
    let (clean, none) = api.gate_field("t", "text", "nothing to see").unwrap();
    assert_eq!((clean.as_str(), none.len()), ("nothing to see", 0));
    let err = api.gate_field("t", "text", CANARY).unwrap_err();
    assert!(
        matches!(err, ApiError::Rejected { ref field, .. } if field == "text"),
        "{err}"
    );
    assert!(err.to_string().contains("aws-access-key-id") && !err.to_string().contains(CANARY));

    let (props, rules) = api
        .gate_props_json(
            "t",
            "props",
            &json!({"note": "hunter2", "n": 1, "list": ["x"]}),
        )
        .unwrap();
    assert_eq!(props["n"], json!(1));
    assert!(
        !props["note"].as_str().unwrap().contains("hunter2"),
        "{props}"
    );
    assert_eq!(rules, vec!["props:old-password".to_owned()]);
    let err = api
        .gate_props_json("t", "props", &json!({"k": CANARY}))
        .unwrap_err();
    assert!(matches!(err, ApiError::Rejected { .. }), "{err}");

    api.gate_joined_view("t", "joined", "title plus body, nothing wrong")
        .unwrap();
    let err = api
        .gate_joined_view("t", "joined", &format!("split {CANARY} across"))
        .unwrap_err();
    assert!(
        matches!(err, ApiError::Rejected { ref field, .. } if field == "joined"),
        "{err}"
    );

    // the EN-F3 validator (`validate_kind_props`) is public beside them
    let _: fn(&kaban_api::OntologyRegistry, &str, &Value) -> Result<(), ApiError> =
        kaban_api::validate_kind_props;
}
