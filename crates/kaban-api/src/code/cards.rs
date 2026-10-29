//! `NodeRow` to `CodeNode` (stage1-spec §6; stage1-plan CG7.1 default 5, CG8.2). `symbol` is
//! set only for a non-external `kind = symbol` row (G1-F9: the cross-commit key an agent may
//! hand to `remember`); `snippet` is the stored G1-F17 text when the caller asked for it
//! (`code_definition` always, the list tools with `include_snippets: true`), `None` otherwise;
//! `distance` is filled by the CG8.1 ranking (`code/ranking.rs`); the card carries 0 before it runs.

use kaban_codegraph::{NodeKind, NodeRow};

use super::{CodeNode, CodeRange};

/// The card of one row. No string is scanned here (the adapter's response gate is the one
/// read gate for these tools; the snippet was gated as a free field at index time, G1-S1).
pub(crate) fn project(row: &NodeRow, include_snippets: bool) -> CodeNode {
    let symbol = if row.kind == NodeKind::Symbol && !row.external {
        row.key.clone()
    } else {
        None
    };
    let snippet = if include_snippets {
        row.snippet.clone()
    } else {
        None
    };
    CodeNode {
        kind: row.kind.as_str().to_owned(),
        r#ref: row.r#ref.clone(),
        symbol,
        raw: row.scip.clone(),
        label: row.label.clone(),
        name: row.name.clone(),
        path: row.path.clone(),
        language: row.language.clone(),
        range: row.def.map(CodeRange::from),
        enclosing: row.enc.map(CodeRange::from),
        signature: row.signature.clone(),
        doc: row.doc.clone(),
        snippet,
        external: row.external,
        is_test: row.is_test,
        distance: 0,
    }
}
