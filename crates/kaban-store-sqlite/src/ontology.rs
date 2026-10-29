//! The store side of ontologies as data (engine-spec §3.1, EN-F2, EN-F11; engine-plan E1.3,
//! E1.7): the builtin `schema_type` rows derived from the shipped ontology files
//! (`kaban_core::shipped`, one place of truth: a change to `kaban-core.yaml` or `adr.yaml` is
//! the only way a builtin row changes), the stored-name rule `schema_extend` applies to its
//! input, and every read and write of the `schema_type` table.
//!
//! The table has two shapes. Up to schema 0003 it is `PK(kind, name)` with `name` holding the
//! stored name (`incidents:Decision`); since 0004 it is `PK(ns, kind, name)` with the bare name
//! in `name` and the namespace in `ns` (RE-2). The functions here take the shape from the
//! connection's schema version (a production store is always at the latest, spec 05 OWN-P;
//! the 0003 arm serves a raw fixture only), so both share one code path;
//! callers always speak stored names, as `node.kind` and `edge.rel` do.

use crate::rows::row_typedef;
use crate::sql_error::SqlResultExt;
use kaban_core::ontology::resolve::RelEnd;
use kaban_core::{
    Qualified, Result, StoreError, TypeDeclaration, TypeDef, split_stored, stored_name,
};
use rusqlite::{Connection, OptionalExtension, params};
use std::sync::OnceLock;

/// The namespaces whose names are stored bare (`kaban_core::stored_name`), as a SQL list for
/// the lookups of a bare stored name; `stored_name_sql_list_matches_the_core_rule` keeps it
/// equal to the core rule.
const BARE_NS_SQL: &str = "('core', 'adr', 'user')";
/// The namespace a bare user name is stored under since 0004 (engine-spec §3.2).
const USER_NS: &str = "user";

/// The shape of `schema_type` behind a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TypeTable {
    /// Schema <= 0003: `PK(kind, name)`, `name` is the stored name.
    Stored,
    /// Schema >= 0004: `PK(ns, kind, name)`, `name` is the bare name.
    Qualified,
}

impl TypeTable {
    /// The shape implied by the schema version recorded in `index_meta` (a database without
    /// the key is at most 0001).
    pub(crate) fn of(conn: &Connection) -> Result<Self> {
        let version: Option<String> = conn
            .prepare_cached("SELECT value FROM index_meta WHERE key = 'schema_version'")
            .sql()?
            .query_row([], |r| r.get(0))
            .optional()
            .sql()?;
        let version = version.and_then(|v| v.parse::<u32>().ok()).unwrap_or(1);
        Ok(if version >= 4 {
            Self::Qualified
        } else {
            Self::Stored
        })
    }

    /// The projection [`row_typedef`] decodes: the stored name is rebuilt from `(ns, name)` by
    /// the decoder, and `ns` is `NULL` on the old shape (derived by the API layer there).
    const fn columns(self) -> &'static str {
        match self {
            Self::Stored => {
                "kind, name, description, from_kinds, to_kinds, directed, props_schema, builtin, NULL, NULL, NULL, NULL, NULL, NULL"
            }
            Self::Qualified => {
                "kind, name, description, from_kinds, to_kinds, directed, props_schema, builtin, ns, governed, rank, column_weights, extract, display"
            }
        }
    }
}

/// One builtin `schema_type` row resolved from the shipped files: identity, description, the
/// declared endpoints and direction, the resolved `props_schema` and the declaration columns
/// 0004 added (EN-F11: `schema_describe` shows the same kinds plus the new columns). On the
/// 0004 shape the rel rows carry their declared `from_kinds`/`to_kinds` (E1.7 R1, landed in
/// E1.8: `link_in` enforces a non-empty list as data, so `Fact SUPERSEDES Fact` is refused
/// while the `core` rels' `[]` stay unconstrained) and the kind rows their resolved
/// `props_schema`; the 0003 shape keeps the pre-E1.3 seed bytes (`[]`, `[]`, 1, `NULL`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TypeRow {
    pub(crate) ns: String,
    /// `label` or `rel`.
    pub(crate) kind: &'static str,
    /// The bare name.
    pub(crate) name: String,
    pub(crate) description: String,
    /// The declared endpoints as JSON arrays of stored names (`path` literal included for
    /// `GOVERNS`); written on the 0004 shape.
    pub(crate) declared_from: String,
    pub(crate) declared_to: String,
    /// The declared direction; written on the 0004 shape.
    pub(crate) declared_directed: bool,
    /// The resolved `props_schema` as JSON (kinds only); written on the 0004 shape.
    pub(crate) declared_props_schema: Option<String>,
    /// The parent's stored name.
    pub(crate) is_a: Option<String>,
    /// JSON array of the mixins' qualified names (`adr:Governed`).
    pub(crate) mixins: String,
    pub(crate) fts_fields: Option<String>,
    pub(crate) ident_fields: Option<String>,
    pub(crate) min_confidence: Option<f64>,
    /// The five JSON declaration columns as canonical JSON text (E1.7 R2, landed in E1.8).
    pub(crate) declaration: [Option<String>; 5],
}

impl TypeRow {
    /// The stored name (`node.kind` / `edge.rel` form).
    pub(crate) fn stored(&self) -> String {
        format_stored(&self.ns, &self.name)
    }
}

/// The stored name of `(ns, name)` by the core rule (a namespace that fails the regex cannot
/// come from the store: the column is written from parsed values only).
fn format_stored(ns: &str, name: &str) -> String {
    match kaban_core::Ns::parse(ns) {
        Ok(ns) => stored_name(&Qualified {
            ns,
            name: name.to_owned(),
        }),
        Err(_) => format!("{ns}:{name}"),
    }
}

fn json_ends(ends: &[RelEnd]) -> String {
    serde_json::Value::Array(
        ends.iter()
            .map(|e| {
                serde_json::Value::String(match e {
                    RelEnd::Kind(q) => stored_name(q),
                    RelEnd::Path => "path".to_owned(),
                })
            })
            .collect(),
    )
    .to_string()
}

fn json_strings(v: &[String]) -> Option<String> {
    (!v.is_empty()).then(|| serde_json::to_string(v).unwrap_or_else(|_| "[]".to_owned()))
}

/// The builtin rows, resolved once from the embedded files and sorted by `(kind, stored name)`.
///
/// # Errors
/// `StoreError::Migration { from: 0, to: 0 }` when the embedded files do not resolve (never
/// for the shipped bytes; kept as an error so the store does not panic on a build defect).
pub(crate) fn builtin_rows() -> Result<&'static [TypeRow]> {
    static ROWS: OnceLock<std::result::Result<Vec<TypeRow>, kaban_core::OntologyError>> =
        OnceLock::new();
    let rows = ROWS.get_or_init(|| {
        let ontology = kaban_core::shipped::resolved()?;
        let mut rows: Vec<TypeRow> = ontology
            .kinds
            .values()
            .map(|k| TypeRow {
                ns: k.id.ns.to_string(),
                kind: "label",
                name: k.id.name.clone(),
                description: k.description.clone(),
                declared_from: "[]".to_owned(),
                declared_to: "[]".to_owned(),
                declared_directed: true,
                declared_props_schema: Some(k.props_schema.as_value().to_string()),
                is_a: k.is_a.as_ref().map(stored_name),
                mixins: serde_json::Value::Array(
                    k.mixins
                        .iter()
                        .map(|m| serde_json::Value::String(m.to_string()))
                        .collect(),
                )
                .to_string(),
                fts_fields: json_strings(&k.fts_fields),
                ident_fields: json_strings(&k.ident_fields),
                min_confidence: k.min_confidence,
                declaration: TypeDeclaration::of(k).texts(),
            })
            .chain(ontology.rels.values().map(|r| TypeRow {
                ns: r.id.ns.to_string(),
                kind: "rel",
                name: r.id.name.clone(),
                description: r.description.clone().unwrap_or_default(),
                declared_from: json_ends(&r.from),
                declared_to: json_ends(&r.to),
                declared_directed: r.directed,
                declared_props_schema: None,
                is_a: None,
                mixins: "[]".to_owned(),
                fts_fields: None,
                ident_fields: None,
                min_confidence: None,
                declaration: Default::default(),
            }))
            .collect();
        rows.sort_by(|a, b| (a.kind, a.stored()).cmp(&(b.kind, b.stored())));
        Ok(rows)
    });
    match rows {
        Ok(rows) => Ok(rows.as_slice()),
        Err(e) => Err(StoreError::Migration {
            from: 0,
            to: 0,
            source: Box::new(e.clone()),
        }),
    }
}

/// Seed the builtin rows that are missing (`INSERT OR IGNORE`): on the old shape the three
/// columns of EN-F4 (byte-identical to the pre-E1.3 seeds), on the 0004 shape the full
/// declaration of [`TypeRow`].
pub(crate) fn seed(conn: &Connection, now: &str) -> Result<()> {
    match TypeTable::of(conn)? {
        TypeTable::Stored => {
            let mut st = conn.prepare_cached(
                "INSERT OR IGNORE INTO schema_type(kind, name, description, builtin, created_at) VALUES (?, ?, ?, 1, ?)",
            ).sql()?;
            for row in builtin_rows()? {
                st.execute(params![row.kind, row.stored(), row.description, now])
                    .sql()?;
            }
        }
        TypeTable::Qualified => {
            let mut st = conn.prepare_cached(&format!(
                "INSERT OR IGNORE INTO schema_type({BUILTIN_COLS}, builtin, created_at) VALUES ({BUILTIN_BINDS}, 1, ?)"
            )).sql()?;
            for row in builtin_rows()? {
                st.execute(rusqlite::params_from_iter(
                    builtin_values(row)
                        .into_iter()
                        .chain([rusqlite::types::Value::Text(now.to_owned())]),
                ))
                .sql()?;
            }
        }
    }
    Ok(())
}

/// The declaration columns of a builtin row on the 0004 shape, in [`builtin_values`] order.
const BUILTIN_COLS: &str = "ns, kind, name, description, is_a, mixins, fts_fields, ident_fields, min_confidence, from_kinds, to_kinds, directed, props_schema, governed, rank, column_weights, extract, display";
const BUILTIN_BINDS: &str = "?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?";

fn builtin_values(row: &TypeRow) -> Vec<rusqlite::types::Value> {
    use rusqlite::types::Value;
    let text = |s: &str| Value::Text(s.to_owned());
    let opt = |s: &Option<String>| s.as_deref().map_or(Value::Null, text);
    vec![
        text(&row.ns),
        text(row.kind),
        text(&row.name),
        text(&row.description),
        opt(&row.is_a),
        text(&row.mixins),
        opt(&row.fts_fields),
        opt(&row.ident_fields),
        row.min_confidence.map_or(Value::Null, Value::Real),
        text(&row.declared_from),
        text(&row.declared_to),
        Value::Integer(i64::from(row.declared_directed)),
        opt(&row.declared_props_schema),
        opt(&row.declaration[0]),
        opt(&row.declaration[1]),
        opt(&row.declaration[2]),
        opt(&row.declaration[3]),
        opt(&row.declaration[4]),
    ]
}

/// The `WHERE` fragment and parameters selecting the row of stored name `stored` under
/// `kind`: on the old shape by name; on the 0004 shape by `(ns, name)`, a bare name matching
/// any bare namespace.
fn locate(table: TypeTable, kind: &str, stored: &str) -> (String, Vec<String>) {
    match table {
        TypeTable::Stored => (
            "kind = ?1 AND name = ?2".to_owned(),
            vec![kind.to_owned(), stored.to_owned()],
        ),
        TypeTable::Qualified => match split_stored(stored) {
            (Some(ns), bare) => (
                "kind = ?1 AND name = ?2 AND ns = ?3".to_owned(),
                vec![kind.to_owned(), bare.to_owned(), ns.to_string()],
            ),
            (None, bare) => (
                format!("kind = ?1 AND name = ?2 AND ns IN {BARE_NS_SQL}"),
                vec![kind.to_owned(), bare.to_owned()],
            ),
        },
    }
}

/// Is `stored_kind` a declared label?
pub(crate) fn kind_exists(conn: &Connection, stored_kind: &str) -> Result<bool> {
    let (where_sql, binds) = locate(TypeTable::of(conn)?, "label", stored_kind);
    Ok(conn
        .prepare_cached(&format!("SELECT 1 FROM schema_type WHERE {where_sql}"))
        .sql()?
        .query_row(rusqlite::params_from_iter(binds), |_| Ok(()))
        .optional()
        .sql()?
        .is_some())
}

/// The definition of the relationship type `stored_rel`, when declared.
pub(crate) fn rel_def(conn: &Connection, stored_rel: &str) -> Result<Option<TypeDef>> {
    let table = TypeTable::of(conn)?;
    let (where_sql, binds) = locate(table, "rel", stored_rel);
    conn.prepare_cached(&format!(
        "SELECT {} FROM schema_type WHERE {where_sql}",
        table.columns()
    ))
    .sql()?
    .query_row(rusqlite::params_from_iter(binds), row_typedef)
    .optional()
    .sql()
}

/// Every definition, builtin first within a kind, then by stored name.
pub(crate) fn all_defs(conn: &Connection) -> Result<Vec<TypeDef>> {
    let table = TypeTable::of(conn)?;
    let mut defs = conn
        .prepare_cached(&format!(
            "SELECT {} FROM schema_type ORDER BY kind, builtin DESC, name",
            table.columns()
        ))
        .sql()?
        .query_map([], row_typedef)
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    // the 0004 shape orders by the bare name; the API order is by stored name (E1.3 goldens)
    defs.sort_by(|a, b| (&a.kind, !a.builtin, &a.name).cmp(&(&b.kind, !b.builtin, &b.name)));
    Ok(defs)
}

/// Insert or refresh the user definition `def` (validated by the caller): `ON CONFLICT` of the
/// table's primary key, never touching a builtin row. `Ok(false)` when nothing changed because
/// the row is builtin.
pub(crate) fn extend(conn: &Connection, def: &TypeDef, now: &str) -> Result<bool> {
    let from = serde_json::to_string(&def.from_kinds)?;
    let to = serde_json::to_string(&def.to_kinds)?;
    let schema = def.props_schema.as_ref().map(ToString::to_string);
    let changed = match TypeTable::of(conn)? {
        TypeTable::Stored => conn
            .execute(
                "INSERT INTO schema_type(kind, name, description, from_kinds, to_kinds, directed, props_schema, builtin, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 0, ?)
                 ON CONFLICT(kind, name) DO UPDATE SET description = excluded.description, from_kinds = excluded.from_kinds, to_kinds = excluded.to_kinds, directed = excluded.directed, props_schema = excluded.props_schema
                 WHERE schema_type.builtin = 0",
                params![def.kind, def.name, def.description, from, to, i64::from(def.directed), schema, now],
            )
            .sql()?,
        TypeTable::Qualified => {
            let (ns, bare) = match split_stored(&def.name) {
                (Some(ns), bare) => (ns.to_string(), bare),
                (None, bare) => (USER_NS.to_owned(), bare),
            };
            // a bare name may not shadow a builtin of another bare namespace (`Decision`)
            if ns == USER_NS {
                let builtin: Option<i64> = conn
                    .prepare_cached(&format!(
                        "SELECT 1 FROM schema_type WHERE kind = ?1 AND name = ?2 AND builtin = 1 AND ns IN {BARE_NS_SQL}"
                    ))
                    .sql()?
                    .query_row(params![def.kind, bare], |r| r.get(0))
                    .optional()
                    .sql()?;
                if builtin.is_some() {
                    return Ok(false);
                }
            }
            let [governed, rank, column_weights, extract, display] = def.declaration.texts();
            conn.execute(
                "INSERT INTO schema_type(ns, kind, name, description, from_kinds, to_kinds, directed, props_schema, builtin, created_at, governed, rank, column_weights, extract, display) VALUES (?, ?, ?, ?, ?, ?, ?, ?, 0, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(ns, kind, name) DO UPDATE SET description = excluded.description, from_kinds = excluded.from_kinds, to_kinds = excluded.to_kinds, directed = excluded.directed, props_schema = excluded.props_schema,
                   governed = excluded.governed, rank = excluded.rank, column_weights = excluded.column_weights, extract = excluded.extract, display = excluded.display
                 WHERE schema_type.builtin = 0",
                params![ns, def.kind, bare, def.description, from, to, i64::from(def.directed), schema, now, governed, rank, column_weights, extract, display],
            )
            .sql()?
        }
    };
    Ok(changed > 0)
}

/// The canonical stored spelling of `name` (E1.7a RE2-1; E1.9a RE3-2): the one contract of
/// `kaban_core::ontology::resolve::normalise_stored`, shared with the API's registry lookups.
/// Applied at the `schema_extend` gate and at the placement seam so the table never holds two
/// spellings of one `(ns, kind, name)` identity and a record is judged under one name.
pub(crate) fn normalise_stored(name: &str) -> String {
    kaban_core::ontology::resolve::normalise_stored(name)
}

/// Is `name` a valid stored label name: `^(?:[a-z][a-z0-9-]{0,31}:)?[A-Z][A-Za-z0-9]{0,63}$`
/// (engine-plan E1.3)? Bare names keep today's rule; `incidents:Decision` passes;
/// `INCIDENTS:Decision`, `vendor` and `Vendor-X` do not.
pub(crate) fn valid_stored_label(name: &str) -> bool {
    match name.split_once(':') {
        Some((ns, local)) => valid_namespace(ns) && valid_bare_label(local),
        None => valid_bare_label(name),
    }
}

/// Is `name` a valid stored rel name: `^(?:[a-z][a-z0-9-]{0,31}:)?[A-Z][A-Z0-9_]{0,63}$`? The
/// stored-name rule applies to rels exactly as to labels (engine-spec §3.1: `edge.rel` carries
/// the stored name), so `incidents:SUPERSEDES` passes and bare names keep today's rule.
pub(crate) fn valid_stored_rel(name: &str) -> bool {
    match name.split_once(':') {
        Some((ns, local)) => valid_namespace(ns) && valid_bare_rel(local),
        None => valid_bare_rel(name),
    }
}

/// Today's rel rule: `UPPER_SNAKE_CASE`, at most 64 bytes.
fn valid_bare_rel(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_uppercase())
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_')
}

/// Today's label rule: `PascalCase` alphanumerics, at most 64 bytes.
pub(crate) fn valid_bare_label(name: &str) -> bool {
    let mut c = name.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_uppercase())
        && name.len() <= 64
        && name.chars().all(|ch| ch.is_ascii_alphanumeric())
}

/// A namespace prefix: `^[a-z][a-z0-9-]{0,31}$`.
fn valid_namespace(ns: &str) -> bool {
    let mut c = ns.chars();
    matches!(c.next(), Some(ch) if ch.is_ascii_lowercase())
        && ns.len() <= 32
        && ns
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion
mod tests {
    use super::*;

    #[test]
    fn the_stored_label_rule_accepts_bare_and_namespaced_pascal_case_only() {
        for ok in ["Fact", "Decision", "incidents:Decision", "a-b2:X9"] {
            assert!(valid_stored_label(ok), "{ok}");
        }
        for bad in [
            "vendor",
            "Vendor-X",
            "INCIDENTS:Decision",
            "incidents:decision",
            ":Decision",
            "incidents:",
            "x:y:Z",
            "",
        ] {
            assert!(!valid_stored_label(bad), "{bad}");
        }
    }

    #[test]
    fn the_canonical_spelling_drops_a_bare_namespace_prefix_only() {
        assert_eq!(normalise_stored("user:Widget"), "Widget");
        assert_eq!(normalise_stored("adr:Decision"), "Decision");
        assert_eq!(normalise_stored("core:SUPERSEDES"), "SUPERSEDES");
        assert_eq!(normalise_stored("incidents:Widget"), "incidents:Widget");
        assert_eq!(normalise_stored("Widget"), "Widget");
    }

    #[test]
    fn the_stored_rel_rule_accepts_bare_and_namespaced_upper_snake_only() {
        for ok in [
            "SUPERSEDES",
            "CAUSED_BY",
            "incidents:SUPERSEDES",
            "a-b2:X_9",
        ] {
            assert!(valid_stored_rel(ok), "{ok}");
        }
        for bad in [
            "supersedes",
            "Supersedes",
            "INCIDENTS:SUPERSEDES",
            "incidents:Decision",
            ":X",
            "",
        ] {
            assert!(!valid_stored_rel(bad), "{bad}");
        }
    }

    #[test]
    fn the_builtin_rows_are_the_thirty_shipped_names_sorted_by_kind_and_stored_name() {
        let rows = builtin_rows().unwrap();
        assert_eq!(rows.len(), 30);
        assert!(
            rows.windows(2)
                .all(|w| (w[0].kind, w[0].stored()) < (w[1].kind, w[1].stored()))
        );
        assert!(rows.iter().all(|r| !r.description.is_empty()));
        assert!(rows.iter().all(|r| r.ns == "core" || r.ns == "adr"));
        let governs = rows
            .iter()
            .find(|r| r.kind == "rel" && r.name == "GOVERNS")
            .unwrap();
        assert_eq!(governs.declared_to, r#"["CodeRef","path"]"#);
    }

    #[test]
    fn stored_name_sql_list_matches_the_core_rule() {
        // every namespace in the SQL list stores bare, and a namespace outside it does not
        let listed: Vec<&str> = BARE_NS_SQL
            .trim_matches(|c| c == '(' || c == ')')
            .split(',')
            .map(|s| s.trim().trim_matches('\''))
            .collect();
        for ns in &listed {
            assert_eq!(format_stored(ns, "Name"), "Name", "{ns}");
        }
        assert_eq!(format_stored("incidents", "Name"), "incidents:Name");
        assert_eq!(listed.len(), 3);
    }
}
