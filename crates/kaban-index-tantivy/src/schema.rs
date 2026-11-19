//! Search fields over one catalog row. The projected summary holds the declared prose fields,
//! `name` holds whole identifiers, and `code` holds the parts of all declared text.
//! Search returns the fast row id and keeps the record body in the canonical store.

use crate::analyzers::{CODE_IDENT, CODE_NAME, FOLDED, STEMMED};
use tantivy::schema::{
    FAST, Field, INDEXED, IndexRecordOption, STRING, Schema, TextFieldIndexing, TextOptions,
};

/// The fast columns: a hit's `rid`, the revision and kind each document was indexed at.
pub(crate) const RID: &str = "rid";
pub(crate) const REVISION: &str = "revision";
pub(crate) const KIND: &str = "kind";

/// Existing configuration keys for the four individually weighted fields.
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
    /// Fields corresponding to the weights in [`CONTENT`].
    pub(crate) content: [Field; 4],
    /// The unstemmed, folded copy of text and summary the fuzzy terms run against.
    pub(crate) fuzzy: Field,
    /// Identifier parts and positions for code-aware operands on all declared text.
    pub(crate) code: Field,
    /// Complete, case-folded identifiers.
    pub(crate) name: Field,
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
            builder.add_text_field("identifier", text(CODE_IDENT, positions)),
        ],
        code: builder.add_text_field("code", text(CODE_IDENT, positions)),
        name: builder.add_text_field("name", text(CODE_NAME, IndexRecordOption::WithFreqs)),
        fuzzy: builder.add_text_field("fuzzy", text(FOLDED, IndexRecordOption::WithFreqs)),
    };
    (builder.build(), fields)
}
