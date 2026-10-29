//! Row decoders: the [`NODE_COLS`] projection and the `rusqlite::Row` to [`Node`], [`TypeDef`] and
//! [`Edge`] conversions shared by the store's selects. The body (`text`) is decoded here
//! (`body_codec::read_column`, spec 05 SS-17): plain or encoded, the caller sees the original.

use kaban_core::{Edge, Governance, Lifecycle, Node, StoreScope, TypeDef};
use serde_json::Value;

/// Columns read into a [`Node`], in [`row_node`] order. `status` and `source_session` are frozen
/// legacy columns (MD-9) and are neither read nor written.
pub(crate) const NODE_COLS: &str = "id,kind,text,summary,tags,confidence,importance,lifecycle,governance,store_scope,path_scope,rev,serialized_rev,dup_key,created_at,updated_at,valid_from,valid_to,session_id,agent_id,workspace_id,repo,commit_sha,accepted_commit,props,content_hash,policy_version";

/// A persisted JSON column that no longer parses (RB-5): a read error with the
/// `serde_json::Error` as its source, never silently a default value, because the caller must
/// not act on data the database does not hold. The message names the column index and the
/// parser's position only; the stored bytes are never echoed (core-spec S1).
fn bad_json(idx: usize, e: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, Box::new(e))
}

/// Decode the JSON string list (`tags`, `path_scope`, endpoint kinds) persisted in column `idx`.
pub(crate) fn decode_json_strings(idx: usize, raw: &str) -> rusqlite::Result<Vec<String>> {
    serde_json::from_str(raw).map_err(|e| bad_json(idx, e))
}

/// Decode the JSON document (props, props schema) persisted in column `idx`.
pub(crate) fn decode_json_value(idx: usize, raw: &str) -> rusqlite::Result<Value> {
    serde_json::from_str(raw).map_err(|e| bad_json(idx, e))
}

/// A column value outside the enum's vocabulary (the CHECK constraint makes this unreachable).
pub(crate) fn bad_enum(idx: usize, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        idx,
        rusqlite::types::Type::Text,
        format!("unexpected state value '{value}'").into(),
    )
}

pub(crate) fn row_node(r: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    row_node_at(r, 0)
}

/// [`row_node`] over [`NODE_COLS`] starting at column `at` (for selects that prefix them).
pub(crate) fn row_node_at(r: &rusqlite::Row<'_>, at: usize) -> rusqlite::Result<Node> {
    let tags: String = r.get(at + 4)?;
    let lifecycle: String = r.get(at + 7)?;
    let governance: String = r.get(at + 8)?;
    let store_scope: String = r.get(at + 9)?;
    let path_scope: String = r.get(at + 10)?;
    let props: String = r.get(at + 24)?;
    Ok(Node {
        id: r.get(at)?,
        kind: r.get(at + 1)?,
        text: crate::body_codec::read_column(r, at + 2)?,
        summary: r.get(at + 3)?,
        tags: decode_json_strings(at + 4, &tags)?,
        confidence: r.get(at + 5)?,
        importance: r.get(at + 6)?,
        lifecycle: Lifecycle::parse(&lifecycle).ok_or_else(|| bad_enum(at + 7, &lifecycle))?,
        governance: Governance::parse(&governance).ok_or_else(|| bad_enum(at + 8, &governance))?,
        store_scope: StoreScope::parse(&store_scope)
            .ok_or_else(|| bad_enum(at + 9, &store_scope))?,
        path_scope: decode_json_strings(at + 10, &path_scope)?,
        rev: r.get(at + 11)?,
        serialized_rev: r.get(at + 12)?,
        dup_key: r.get(at + 13)?,
        created_at: r.get(at + 14)?,
        updated_at: r.get(at + 15)?,
        valid_from: r.get(at + 16)?,
        valid_to: r.get(at + 17)?,
        session_id: r.get(at + 18)?,
        agent_id: r.get(at + 19)?,
        workspace_id: r.get(at + 20)?,
        repo: r.get(at + 21)?,
        commit_sha: r.get(at + 22)?,
        accepted_commit: r.get(at + 23)?,
        props: decode_json_value(at + 24, &props)?,
        content_hash: r.get(at + 25)?,
        policy_version: r.get(at + 26)?,
    })
}
/// Decode a `schema_type` row in the [`crate::ontology::TypeTable::columns`] projection: on
/// the 0004 shape the ninth column is `ns` and `name` is bare, so the stored name is rebuilt
/// by the core rule; on the old shape the ninth column is `NULL` and `name` is already stored.
pub(crate) fn row_typedef(r: &rusqlite::Row<'_>) -> rusqlite::Result<TypeDef> {
    let f: String = r.get(3)?;
    let t: String = r.get(4)?;
    let ps: Option<String> = r.get(6)?;
    let ns: Option<String> = r.get(8)?;
    let name: String = r.get(1)?;
    let name = match &ns {
        // the discovered namespace is reserved (never a parseable `Ns`, engine-spec §3.1) and
        // always stores qualified (engine-plan E1.8)
        Some(ns) if ns == "x" => format!("x:{name}"),
        Some(ns) => {
            let ns = kaban_core::Ns::parse(ns).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    8,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            kaban_core::stored_name(&kaban_core::Qualified { ns, name })
        }
        None => name,
    };
    Ok(TypeDef {
        kind: r.get(0)?,
        name,
        ns,
        description: r.get(2)?,
        from_kinds: decode_json_strings(3, &f)?,
        to_kinds: decode_json_strings(4, &t)?,
        directed: r.get::<_, i64>(5)? != 0,
        props_schema: ps.map(|s| decode_json_value(6, &s)).transpose()?,
        builtin: r.get::<_, i64>(7)? != 0,
        declaration: kaban_core::TypeDeclaration {
            governed: json_col(r, 9)?,
            rank: json_col(r, 10)?,
            column_weights: json_col(r, 11)?,
            extract: json_col(r, 12)?,
            display: json_col(r, 13)?,
        },
    })
}
/// A nullable JSON text column (the 0004 declaration columns; `NULL` on the 0003 shape).
fn json_col(r: &rusqlite::Row<'_>, idx: usize) -> rusqlite::Result<Option<serde_json::Value>> {
    let text: Option<String> = r.get(idx)?;
    text.map(|s| decode_json_value(idx, &s)).transpose()
}
pub(crate) fn row_edge(r: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    let props: String = r.get(6)?;
    Ok(Edge {
        id: r.get(0)?,
        src: r.get(1)?,
        dst: r.get(2)?,
        rel: r.get(3)?,
        confidence: r.get(4)?,
        created_at: r.get(5)?,
        props: decode_json_value(6, &props)?,
    })
}
