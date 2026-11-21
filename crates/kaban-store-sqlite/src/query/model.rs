//! The SQL names and fixed projections derived from the statement's ontology snapshot.
use super::declarations::{self, Declaration};
use super::functions::FUNCTIONS;
use crate::rollup::{identifier, key_column, literal};
use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::query_budget::{BudgetLease, QueryBudget};
use kaban_vtab::Value;
use rusqlite::Transaction;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;

type Reads = BTreeMap<String, BTreeSet<String>>;

struct View {
    public: String,
    columns: BTreeSet<String>,
    reads: Reads,
}

pub(super) struct Model {
    // Each private accessor belongs to exactly one installed projection.
    private: BTreeMap<String, View>,
    public: BTreeMap<String, String>,
    pub(super) rows: Vec<Vec<Value>>,
    pub(super) max_row_bytes: u64,
    storage: BudgetLease,
}

struct Object<'a> {
    kind: &'a str,
    ontology: &'a str,
    declared: &'a str,
    sql: Option<String>,
}

fn text(s: &str) -> Value {
    Value::Text(s.to_owned())
}

fn reserved(name: &str) -> bool {
    matches!(name, "nodes" | "edges" | "body")
        || FUNCTIONS.iter().any(|(fixed, _)| *fixed == name)
        || name.starts_with("sqlite_")
}

/// The leading @ cannot occur in validated ontology identifiers. The encoded identity
/// preserves ASCII case even under SQLite's case-insensitive name comparison.
fn distinct(kind: &str, ns: &str, name: &str) -> String {
    let mut out = format!("@{kind}.{ns}.");
    for b in name.bytes() {
        let _ = write!(out, "{b:02x}");
    }
    out
}

fn reads(entries: &[(&str, &[&str])]) -> Reads {
    entries
        .iter()
        .map(|(table, cols)| ((*table).into(), cols.iter().map(|c| (*c).into()).collect()))
        .collect()
}

type Fields = Vec<(String, String, String)>; // declared name, SQL name, fixed SQL expression

fn fixed(fields: &[(&str, &str)]) -> Fields {
    fields
        .iter()
        .map(|(c, e)| ((*c).into(), (*c).into(), (*e).into()))
        .collect()
}

fn extend_fields(fields: &mut Fields, items: Vec<(String, String)>) {
    let reserved: BTreeSet<_> = fields
        .iter()
        .map(|(_, name, _)| name.to_ascii_lowercase())
        .collect();
    let mut counts = BTreeMap::new();
    for (name, _) in &items {
        *counts.entry(name.to_ascii_lowercase()).or_insert(0_usize) += 1;
    }
    for (name, expr) in items {
        let key = name.to_ascii_lowercase();
        let sql = if reserved.contains(&key)
            || counts[&key] > 1
            || name.starts_with('@')
            || name.contains('\0')
        {
            distinct("column", "", &name)
        } else {
            name.clone()
        };
        fields.push((name, sql, expr));
    }
}

impl Model {
    pub(super) fn install(
        tx: &Transaction<'_>,
        prefix: &str,
        budget: &QueryBudget,
    ) -> Result<Self> {
        // Fixed functions and the two canonical views have a schema-independent footprint.
        let mut storage = budget.reserve(32768)?;
        let defs = declarations::read(tx, budget, &mut storage)?;
        let mut model = Self {
            private: BTreeMap::new(),
            public: BTreeMap::new(),
            rows: vec![],
            max_row_bytes: 0,
            storage,
        };
        let mut counts = BTreeMap::new();
        for d in defs.values() {
            if !d.per_commit {
                *counts.entry(d.name.to_ascii_lowercase()).or_insert(0_usize) += 1;
            }
            for r in &d.rollups {
                *counts
                    .entry(format!("rollup_{}", r.name.to_ascii_lowercase()))
                    .or_insert(0) += 1;
            }
        }
        let mapped = |kind: &str, ns: &str, name: &str| {
            let key = name.to_ascii_lowercase();
            if reserved(&key) || counts.get(&key).is_some_and(|n| *n > 1) {
                distinct(kind, ns, name)
            } else {
                name.to_owned()
            }
        };
        for &(name, columns) in FUNCTIONS {
            let object = Object {
                kind: "function",
                ontology: "",
                declared: name,
                sql: Some(name.into()),
            };
            for c in columns {
                // SQL needs distinct column names for expand's result depth and input depth.
                let declared = if name == "expand" && c.name == "max_depth" {
                    "depth"
                } else {
                    c.name
                };
                model.metadata(
                    &object,
                    Some((declared, c.name)),
                    if c.argument.is_some() {
                        "argument"
                    } else {
                        "column"
                    },
                    false,
                );
            }
        }
        let nodes = Object {
            kind: "view",
            ontology: "",
            declared: "nodes",
            sql: Some("nodes".into()),
        };
        model.view(
            tx,
            prefix,
            &nodes,
            fixed(&[
                ("id", "n.id"),
                ("label", "n.kind"),
                ("summary", "n.summary"),
                ("tags", "n.tags"),
                ("status", "n.status"),
                ("created_at", "n.created_at"),
                ("updated_at", "n.updated_at"),
                ("workspace", "n.workspace_id"),
            ]),
            "FROM main.node n WHERE n.lifecycle <> 'purged'",
            reads(&[(
                "node",
                &[
                    "id",
                    "kind",
                    "summary",
                    "tags",
                    "status",
                    "created_at",
                    "updated_at",
                    "workspace_id",
                    "lifecycle",
                ],
            )]),
        )?;
        let rels: Vec<_> = defs
            .values()
            .filter(|d| d.kind == "rel" && d.per_commit)
            .map(|d| literal(&d.name))
            .collect();
        let mut edge_filter = String::new();
        if !rels.is_empty() {
            let _ = write!(edge_filter, " AND e.rel NOT IN ({})", rels.join(","));
        }
        let edges = Object {
            kind: "view",
            ontology: "",
            declared: "edges",
            sql: Some("edges".into()),
        };
        let mut edge = Self::endpoints();
        edge.extend(fixed(&[
            ("rel", "e.rel"),
            ("confidence", "e.confidence"),
            ("created_at", "e.created_at"),
            ("producer", "e.producer"),
            ("method", "e.method"),
            ("score", "e.score"),
        ]));
        model.view(
            tx,
            prefix,
            &edges,
            edge,
            &format!("{}{}", Self::EDGE_FROM, edge_filter),
            reads(&[
                ("node", &["rid", "id", "lifecycle"]),
                (
                    "edge",
                    &[
                        "src",
                        "dst",
                        "rel",
                        "confidence",
                        "created_at",
                        "producer",
                        "method",
                        "score",
                    ],
                ),
            ]),
        )?;
        for d in defs.values() {
            budget.check()?;
            let ns = &d.ns;
            let object = Object {
                kind: &d.kind,
                ontology: ns,
                declared: &d.name,
                sql: (!d.per_commit).then(|| mapped(&d.kind, ns, &d.name)),
            };
            if d.per_commit {
                model.metadata(&object, None, "relationship", true);
            } else if d.kind == "label" {
                model.label(tx, prefix, &object, d)?;
            } else if d.kind == "rel" {
                let mut fields = Self::endpoints();
                let mut allow = reads(&[
                    ("node", &["id", "rid", "lifecycle"]),
                    ("edge", &["src", "dst", "rel"]),
                ]);
                if !d.props.is_empty() {
                    let mut declared = Vec::new();
                    for k in &d.props {
                        let path = literal(&format!("$.{}", serde_json::to_string(k)?));
                        declared.push((k.clone(), format!("json_extract(e.props,{path})")));
                    }
                    extend_fields(&mut fields, declared);
                    allow
                        .entry("edge".into())
                        .or_default()
                        .insert("props".into());
                }
                model.view(
                    tx,
                    prefix,
                    &object,
                    fields,
                    &format!("{} AND e.rel={}", Self::EDGE_FROM, literal(&d.name)),
                    allow,
                )?;
            }
            for r in &d.rollups {
                let name = format!("rollup_{}", r.name);
                let object = Object {
                    kind: "rollup",
                    ontology: ns,
                    declared: &r.name,
                    sql: Some(mapped("rollup", ns, &name)),
                };
                let mut fields = fixed(&[("value", "r.value")]);
                extend_fields(
                    &mut fields,
                    r.group_by
                        .iter()
                        .enumerate()
                        .map(|(i, k)| (k.clone(), key_column(i)))
                        .collect(),
                );
                fields.rotate_left(1);
                model.view(
                    tx,
                    prefix,
                    &object,
                    fields,
                    &format!("FROM main.rollup r WHERE r.name={}", literal(&r.name)),
                    reads(&[("rollup", &["name", "key", "value"])]),
                )?;
            }
        }
        Ok(model)
    }
    const EDGE_FROM: &'static str = "FROM main.edge e JOIN main.node s ON s.rid=e.src JOIN main.node d ON d.rid=e.dst WHERE s.lifecycle <> 'purged' AND d.lifecycle <> 'purged'";
    fn endpoints() -> Fields {
        fixed(&[("src", "s.id"), ("dst", "d.id")])
    }

    fn label(
        &mut self,
        tx: &Transaction<'_>,
        prefix: &str,
        obj: &Object<'_>,
        d: &Declaration,
    ) -> Result<()> {
        let mut fields = fixed(&[("id", "n.id")]);
        let mut from = "FROM main.node n".to_owned();
        let mut allow = reads(&[("node", &["id", "kind", "lifecycle"])]);
        if !d.columns.is_empty() {
            let table = format!("col_{}", d.name);
            let _ = write!(from, " JOIN main.{} c ON c.rid=n.rid", identifier(&table));
            allow.entry("node".into()).or_default().insert("rid".into());
            let mut cols = BTreeSet::from(["rid".to_owned()]);
            extend_fields(
                &mut fields,
                d.columns
                    .iter()
                    .map(|c| {
                        cols.insert(c.clone());
                        (c.clone(), format!("c.{}", identifier(c)))
                    })
                    .collect(),
            );
            allow.insert(table, cols);
        }
        let _ = write!(
            from,
            " WHERE n.lifecycle <> 'purged' AND n.kind={}",
            literal(&d.name)
        );
        self.view(tx, prefix, obj, fields, &from, allow)
    }

    fn metadata(
        &mut self,
        obj: &Object<'_>,
        column: Option<(&str, &str)>,
        role: &str,
        per_commit: bool,
    ) {
        let row = vec![
            text(obj.kind),
            text(obj.ontology),
            text(obj.declared),
            obj.sql.as_deref().map_or(Value::Null, text),
            column.map_or(Value::Null, |(c, _)| text(c)),
            column.map_or(Value::Null, |(_, s)| text(s)),
            text(role),
            Value::Integer(i64::from(per_commit)),
        ];
        let bytes = row
            .iter()
            .map(|v| {
                size_of::<Value>() as u64
                    + 48
                    + if let Value::Text(s) = v {
                        s.len() as u64
                    } else {
                        0
                    }
            })
            .sum();
        self.max_row_bytes = self.max_row_bytes.max(bytes);
        self.rows.push(row);
    }

    fn view(
        &mut self,
        tx: &Transaction<'_>,
        prefix: &str,
        obj: &Object<'_>,
        fields: Fields,
        from: &str,
        reads: Reads,
    ) -> Result<()> {
        let public = obj
            .sql
            .as_deref()
            .ok_or_else(|| super::refused("model", "missing SQL view name"))?;
        // Object names are repeated once for each discoverable column.
        super::charge(
            &mut self.storage,
            (fields.len() as u64).saturating_mul(
                512 + 2 * (public.len() + obj.declared.len() + obj.ontology.len()) as u64,
            ),
        )?;
        let private = format!("{prefix}_{}", self.private.len());
        let mut names = Vec::new();
        let mut projection = Vec::new();
        let mut cols = BTreeSet::new();
        for (declared, name, expr) in fields {
            let quoted = identifier(&name);
            projection.push(format!("{expr} AS {quoted}"));
            names.push(quoted);
            self.metadata(obj, Some((&declared, &name)), "column", false);
            cols.insert(name);
        }
        let projection = projection.join(",");
        let columns = names.join(",");
        let quoted = identifier(public);
        tx.execute_batch(&format!(
            "CREATE TEMP VIEW {private} AS SELECT {projection} {from};
             CREATE TEMP VIEW {quoted} AS SELECT {columns} FROM temp.{private}"
        ))
        .sql()?;
        self.public.insert(public.into(), private.clone());
        self.private.insert(
            private,
            View {
                public: public.into(),
                columns: cols,
                reads,
            },
        );
        Ok(())
    }
    pub(super) fn allows_read(
        &self,
        db: Option<&str>,
        table: &str,
        column: &str,
        source: Option<&str>,
    ) -> bool {
        match db {
            Some("temp") => {
                if let Some(v) = self.public.get(table).and_then(|key| self.private.get(key)) {
                    return column.is_empty() || v.columns.contains(column);
                }
                self.private.get(table).is_some_and(|v| {
                    source == Some(v.public.as_str())
                        && (column.is_empty() || v.columns.contains(column))
                })
            }
            Some("main") => {
                source
                    .and_then(|s| self.private.get(s))
                    .and_then(|v| v.reads.get(table))
                    .is_some_and(|cols| cols.contains(column))
                    || FUNCTIONS.iter().any(|(name, cols)| {
                        *name == table
                            && (column.is_empty() || cols.iter().any(|c| c.name == column))
                    })
            }
            _ => false,
        }
    }
    pub(super) fn allows_json(&self, source: Option<&str>) -> bool {
        source.is_some_and(|s| self.private.contains_key(s))
    }
}
