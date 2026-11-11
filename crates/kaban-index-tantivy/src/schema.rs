//! The fixed field set (spec 05 SS-08). The ontology decides what goes into the fields (the
//! kinds' `fts_fields` projection reaches `summary` through the store, the `ident_fields` and
//! anchors feed `ident_terms`, the per-kind `column_weights` become query boosts); the fields
//! themselves are the FTS5 index's four content columns plus the retrieval keys and the filter
//! fields. Nothing is stored: a hit's `rid`, `revision` and `kind` come from fast columns, the
//! record UUID is the delete key, and the record body lives in SQLite only.

use crate::analyzers::{CODE_IDENT, CODE_NAME, FOLDED, IDENT, STEMMED};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions,
};

/// Bumped when the field set or an option changes; part of the payload's compatibility.
pub(crate) const SCHEMA_VERSION: u32 = 2;

/// The fast columns a hit is read from.
pub(crate) const RID: &str = "rid";
pub(crate) const REVISION: &str = "revision";
pub(crate) const KIND: &str = "kind";

/// The content fields in the FTS5 column order (`text`, `summary`, `tags`, `ident_terms`),
/// which is also the order of the column weights.
pub(crate) const CONTENT: [&str; 4] = ["text", "summary", "tags", "ident_terms"];

/// The lifecycle value every indexed row carries: the index holds active rows only.
pub(crate) const ACTIVE: &str = "active";

/// The fields by role.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fields {
    /// The record UUID, raw: the delete key of a projection.
    pub(crate) id: Field,
    /// The row id, indexed (the purge key) and fast (the tie-break and the hit).
    pub(crate) rid: Field,
    /// The revision, fast (the hit).
    pub(crate) revision: Field,
    /// The kind, raw (a filter) and fast (the hit).
    pub(crate) kind: Field,
    /// Filter fields, raw: never a text match.
    pub(crate) workspace: Field,
    pub(crate) lifecycle: Field,
    pub(crate) tag: Field,
    /// [`CONTENT`]: stemmed text, with positions for phrases.
    pub(crate) content: [Field; 4],
    /// The unstemmed, folded copy of text and summary the fuzzy terms run against.
    pub(crate) fuzzy: Field,
    /// The first identifier value whole and lowercased (an exact-name match) and the code
    /// parts of every identifier value ([`CODE_IDENT`], phrases possible), both fed from
    /// `ident_values`. A code row's path facet rides on `workspace`.
    pub(crate) name: Field,
    pub(crate) code: Field,
}

/// The schema and its fields.
pub(crate) fn schema() -> (Schema, Fields) {
    let mut builder = Schema::builder();
    let text = |analyzer: &str, record: IndexRecordOption| {
        TextOptions::default().set_indexing_options(
            TextFieldIndexing::default()
                .set_tokenizer(analyzer)
                .set_index_option(record),
        )
    };
    let positions = IndexRecordOption::WithFreqsAndPositions;
    let fields = Fields {
        id: builder.add_text_field("id", STRING),
        rid: builder.add_i64_field(RID, INDEXED | FAST),
        revision: builder.add_i64_field(REVISION, FAST),
        kind: builder.add_text_field(KIND, STRING | FAST),
        workspace: builder.add_text_field("workspace", STRING),
        lifecycle: builder.add_text_field("lifecycle", STRING),
        tag: builder.add_text_field("tag", STRING),
        content: [
            builder.add_text_field(CONTENT[0], text(STEMMED, positions)),
            builder.add_text_field(CONTENT[1], text(STEMMED, positions)),
            builder.add_text_field(CONTENT[2], text(STEMMED, positions)),
            builder.add_text_field(CONTENT[3], text(IDENT, positions)),
        ],
        fuzzy: builder.add_text_field("fuzzy", text(FOLDED, IndexRecordOption::WithFreqs)),
        name: builder.add_text_field("name", text(CODE_NAME, IndexRecordOption::WithFreqs)),
        code: builder.add_text_field("code", text(CODE_IDENT, positions)),
    };
    (builder.build(), fields)
}
