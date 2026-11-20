//! A declared label's typed columns are written in the same transaction as its node.

use crate::catalog::capture::BatchChanges;
use crate::catalog::commit::Derived;
use crate::sql_error::SqlResultExt;
use kaban_core::ontology::{ColumnDecl, ColumnType, RollupDecl, RollupOp};
use kaban_core::{Result, SqlValue, StoreError, TypeDeclaration, TypeDef};
use rusqlite::functions::FunctionFlags;
use rusqlite::types::{Value as DbValue, ValueRef};
use rusqlite::{Connection, OptionalExtension, params};
use std::collections::{BTreeMap, BTreeSet};

/// Also installed transactionally when reopening an earlier database of the supported version.
pub(crate) const TABLES: &str = "CREATE TABLE IF NOT EXISTS rollup(
    name TEXT NOT NULL, key BLOB NOT NULL, members INTEGER NOT NULL, nonnull INTEGER NOT NULL,
    value ANY, PRIMARY KEY(name,key)) STRICT, WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS rollup_input(seq INTEGER PRIMARY KEY, name TEXT NOT NULL, key BLOB,
        delta INTEGER NOT NULL, amount ANY, real_sum INTEGER NOT NULL) STRICT;";

fn value(v: ValueRef<'_>) -> rusqlite::Result<SqlValue> {
    Ok(match v {
        ValueRef::Null => SqlValue::Null,
        ValueRef::Integer(n) => SqlValue::Integer(n),
        ValueRef::Real(n) => SqlValue::Real(if n == 0.0 { 0.0 } else { n }),
        ValueRef::Text(s) => SqlValue::Text(
            std::str::from_utf8(s)
                .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?
                .into(),
        ),
        ValueRef::Blob(b) => SqlValue::Blob(b.into()),
    })
}

fn encode_key(key: &[SqlValue]) -> Result<Vec<u8>> {
    // Distinct SQL storage classes have distinct encodings; -0.0 and 0.0 form one group.
    if key
        .iter()
        .any(|v| matches!(v,SqlValue::Real(n) if !n.is_finite()))
    {
        return Err(StoreError::InvalidName(
            "rollup".into(),
            "non-finite group key",
        ));
    }
    let normalized = key
        .iter()
        .map(|v| match v {
            SqlValue::Real(n) if *n == 0.0 => SqlValue::Real(0.0),
            _ => v.clone(),
        })
        .collect::<Vec<_>>();
    Ok(serde_json::to_vec(&normalized)?)
}

pub(crate) fn register(conn: &Connection) -> Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8
        | FunctionFlags::SQLITE_DETERMINISTIC
        | FunctionFlags::SQLITE_INNOCUOUS;
    conn.create_scalar_function("kaban_valid_timestamp", 1, flags, |ctx| {
        Ok(crate::rows::rfc3339_ms(ctx.get_raw(0).as_str()?).is_some())
    })
    .sql()?;
    conn.create_scalar_function("kaban_rollup_key", -1, flags, |ctx| {
        let key = (0..ctx.len())
            .map(|i| value(ctx.get_raw(i)))
            .collect::<rusqlite::Result<Vec<_>>>()?;
        encode_key(&key).map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))
    })
    .sql()
}

fn group_expr(r: &RollupDecl, prefix: &str) -> String {
    let keys = r
        .group_by
        .iter()
        .map(|c| format!("{prefix}{}", identifier(c)))
        .collect::<Vec<_>>()
        .join(",");
    format!("kaban_rollup_key({keys})")
}

fn is_real(r: &RollupDecl, model: &TypeDeclaration) -> bool {
    matches!(&r.op, RollupOp::Sum(name) if model.columns.iter().any(|c| c.name == *name && c.ty == ColumnType::Real))
}

fn capture_triggers(
    conn: &Connection,
    label: &str,
    table: &str,
    model: &TypeDeclaration,
) -> Result<()> {
    if model.rollups.is_empty() {
        return Ok(());
    }
    let mut indexed_groups = BTreeSet::new();
    for (i, r) in model.rollups.iter().enumerate() {
        if is_real(r, model) && indexed_groups.insert(&r.group_by) {
            let idx = identifier(&format!("rollup_group_{label}_{i}"));
            conn.execute_batch(&format!(
                "CREATE INDEX {idx} ON {table}({}) WHERE _active=1;",
                group_expr(r, "")
            ))
            .sql()?;
        }
    }
    for (op, sides) in [
        ("INSERT", &[("new", 1)][..]),
        ("UPDATE", &[("old", -1), ("new", 1)][..]),
        ("DELETE", &[("old", -1)][..]),
    ] {
        let mut statements = Vec::new();
        for (row, delta) in sides {
            for r in &model.rollups {
                let key = group_expr(r, &format!("{row}."));
                let amount = match &r.op {
                    RollupOp::Count => "1".to_owned(),
                    RollupOp::Sum(col) => format!("{row}.{}", identifier(col)),
                };
                statements.push(format!(
                    "INSERT INTO rollup_input(name,key,delta,amount,real_sum)
                    SELECT {},{key},{delta},{amount},{} WHERE {row}._active=1;",
                    literal(&r.name),
                    i32::from(is_real(r, model))
                ));
            }
        }
        let trigger = identifier(&format!("rollup_{op}_{label}"));
        conn.execute_batch(&format!(
            "CREATE TRIGGER {trigger} AFTER {op} ON {table} BEGIN {} END;",
            statements.join("\n")
        ))
        .sql()?;
    }
    Ok(())
}

fn identifier(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// One captured contribution, or a reset (no key) when its declaration changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Input {
    name: String,
    key: Option<Vec<u8>>,
    delta: i64,
    amount: Option<Amount>,
    real_sum: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Amount {
    Integer(i64),
    Real(u64),
}

pub(crate) fn installed(conn: &Connection) -> Result<bool> {
    // Diagnostic opens preserve earlier databases exactly; the first writable open
    // provisions the capture table atomically. Small catalog-only fixtures omit it too.
    conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE name='rollup_input')",
        [],
        |r| r.get(0),
    )
    .sql()
}

pub(crate) fn drain(conn: &Connection) -> Result<Vec<Input>> {
    if !installed(conn)? {
        return Ok(Vec::new());
    }
    let mut stmt = conn
        .prepare_cached("SELECT name,key,delta,amount,real_sum FROM main.rollup_input ORDER BY seq")
        .sql()?;
    let inputs = stmt
        .query_map([], |r| {
            let amount = match r.get_ref(3)? {
                ValueRef::Null => None,
                ValueRef::Integer(i) => Some(Amount::Integer(i)),
                ValueRef::Real(f) => Some(Amount::Real(f.to_bits())),
                v => {
                    return Err(rusqlite::Error::InvalidColumnType(
                        3,
                        "amount".into(),
                        v.data_type(),
                    ));
                }
            };
            Ok(Input {
                name: r.get(0)?,
                key: r.get(1)?,
                delta: r.get(2)?,
                amount,
                real_sum: r.get(4)?,
            })
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    if !inputs.is_empty() {
        conn.execute("DELETE FROM main.rollup_input", []).sql()?;
    }
    Ok(inputs)
}

#[derive(Default)]
struct Delta {
    members: i64,
    nonnull: i64,
    integer_sum: i128,
    real_sum: bool,
}

pub(crate) fn maintain(derived: &Derived<'_>, changes: &BatchChanges) -> Result<()> {
    if changes.rollups.is_empty() {
        return Ok(());
    }
    let mut groups = BTreeMap::<(&str, &[u8]), Delta>::new();
    let mut resets = BTreeSet::new();
    for input in &changes.rollups {
        let Some(key) = &input.key else {
            groups.retain(|(name, _), _| *name != input.name);
            resets.insert(input.name.as_str());
            continue;
        };
        let d = groups.entry((&input.name, key)).or_default();
        d.members += input.delta;
        d.real_sum = input.real_sum;
        if let Some(amount) = input.amount {
            d.nonnull += input.delta;
            if let Amount::Integer(v) = amount {
                d.integer_sum += i128::from(input.delta) * i128::from(v);
            }
        }
    }
    for name in resets {
        derived.execute("DELETE FROM main.rollup WHERE name=?", [name])?;
    }
    let reader = derived.reader();
    let mut previous =
        reader.prepare("SELECT members,nonnull,value FROM main.rollup WHERE name=? AND key=?")?;
    let mut recounts = BTreeMap::new();
    for ((name, key), d) in groups {
        let (members, nonnull, old) = previous
            .query_row(params![name, key], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, DbValue>(2)?,
                ))
            })?
            .unwrap_or((0, 0, DbValue::Null));
        let members = members + d.members;
        let nonnull = nonnull + d.nonnull;
        if members == 0 {
            derived.execute(
                "DELETE FROM main.rollup WHERE name=? AND key=?",
                params![name, key],
            )?;
            continue;
        }
        let value = if nonnull == 0 {
            DbValue::Null
        } else if d.real_sum {
            // SQLite's compensated SUM in canonical rid order gives stable results even
            // after cancellation. Recount each changed real group only once per commit.
            if !recounts.contains_key(name) {
                let rule: String = reader.prepare("SELECT j.value FROM schema_type s,json_each(s.rollups) j WHERE json_extract(j.value,'$.name')=?")?
                    .query_row([name],|r| r.get(0))?.ok_or_else(|| StoreError::InvalidName(name.into(),"missing rollup declaration"))?;
                let r: RollupDecl = serde_json::from_str(&rule)?;
                let RollupOp::Sum(col) = &r.op else {
                    return Err(StoreError::InvalidName(name.into(), "expected real SUM"));
                };
                let select = format!(
                    "SELECT sum(x) FROM (SELECT {} AS x FROM {} WHERE _active=1 AND {}=? ORDER BY rid)",
                    identifier(col),
                    identifier(&format!("col_{}", r.label)),
                    group_expr(&r, "")
                );
                recounts.insert(name, reader.prepare(&select)?);
            }
            let f: f64 = recounts
                .get_mut(name)
                .ok_or_else(|| StoreError::InvalidName(name.into(), "missing recount"))?
                .query_row([key], |r| r.get(0))?
                .ok_or_else(|| StoreError::InvalidName(name.into(), "missing sum"))?;
            if !f.is_finite() {
                return Err(StoreError::RollupOverflow);
            }
            DbValue::Real(f)
        } else {
            let prior = if let DbValue::Integer(i) = old { i } else { 0 };
            DbValue::Integer(
                i64::try_from(i128::from(prior) + d.integer_sum)
                    .map_err(|_| StoreError::RollupOverflow)?,
            )
        };
        derived.execute("INSERT INTO main.rollup(name,key,members,nonnull,value) VALUES(?,?,?,?,?)
            ON CONFLICT(name,key) DO UPDATE SET members=excluded.members,nonnull=excluded.nonnull,value=excluded.value",
            params![name,key,members,nonnull,value])?;
    }
    Ok(())
}

pub(crate) fn read(conn: &Connection, name: &str, key: &[SqlValue]) -> Result<SqlValue> {
    let key_bytes = encode_key(key)?;
    // One SQLite statement pins both the declared operation and the value to one commit.
    let result = conn
        .query_row(
            "SELECT j.value,s.columns,v.value,v.members FROM schema_type s,json_each(s.rollups) j
        LEFT JOIN main.rollup v ON v.name=?1 AND v.key=?2 WHERE json_extract(j.value,'$.name')=?1",
            params![name, key_bytes],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    value(r.get_ref(2)?)?,
                    r.get::<_, Option<i64>>(3)?,
                ))
            },
        )
        .optional()
        .sql();
    let result = match result {
        Err(_) if !crate::ontology::has_declared_columns(conn)? => None,
        other => other?,
    };
    let (rule, columns, stored, members) =
        result.ok_or_else(|| StoreError::InvalidName(name.into(), "unknown rollup"))?;
    let rule: RollupDecl = serde_json::from_str(&rule)?;
    let columns: Vec<ColumnDecl> = serde_json::from_str(&columns)?;
    if key.len() != rule.group_by.len()
        || !key.iter().zip(&rule.group_by).all(|(v, n)| {
            columns.iter().find(|c| &c.name == n).is_some_and(|c| {
                matches!(
                    (v, c.ty),
                    (SqlValue::Null, _)
                        | (SqlValue::Integer(_), ColumnType::Integer)
                        | (SqlValue::Real(_), ColumnType::Real)
                        | (SqlValue::Text(_), ColumnType::Text | ColumnType::Timestamp)
                )
            })
        })
    {
        return Err(StoreError::InvalidName(
            name.into(),
            "invalid typed group key",
        ));
    }
    Ok(if members.is_none() && rule.op == RollupOp::Count {
        SqlValue::Integer(0)
    } else {
        stored
    })
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

fn valid_name(s: &str) -> bool {
    s.len() <= 64
        && s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
}

pub(crate) fn validate(def: &TypeDef, existing: &[TypeDef]) -> Result<()> {
    let fields = &def.declaration;
    if fields.columns.is_empty() && fields.rollups.is_empty() && fields.fts_fields.is_empty() {
        return Ok(());
    }
    let invalid = |why| StoreError::InvalidName(def.name.clone(), why);
    if def.kind != "label" {
        return Err(invalid(
            "column, rollup and text declarations require a label",
        ));
    }
    if existing.iter().any(|other| {
        other.name != def.name
            && other.name.eq_ignore_ascii_case(&def.name)
            && (!other.declaration.columns.is_empty() || !other.declaration.rollups.is_empty())
    }) {
        return Err(invalid(
            "declared label table names must be distinct ignoring case",
        ));
    }
    let mut columns = BTreeSet::from([
        "rid".to_owned(),
        "rowid".into(),
        "_rowid_".into(),
        "oid".into(),
        "_active".into(),
    ]);
    for col in &fields.columns {
        if !valid_name(&col.name) || !columns.insert(col.name.to_ascii_lowercase()) {
            return Err(invalid(
                "invalid, repeated or reserved declared column name",
            ));
        }
    }
    let mut names: BTreeSet<_> = existing
        .iter()
        .filter(|d| d.name != def.name)
        .flat_map(|d| &d.declaration.rollups)
        .map(|r| r.name.as_str())
        .collect();
    for r in &fields.rollups {
        if !valid_name(&r.name) || !names.insert(&r.name) || r.label != def.name {
            return Err(invalid(
                "rollup names must be unique and refer to their declaring label",
            ));
        }
        let mut keys = BTreeSet::new();
        for name in &r.group_by {
            if !keys.insert(name) || !fields.columns.iter().any(|c| c.name == *name) {
                return Err(invalid(
                    "rollup group keys must name distinct declared columns",
                ));
            }
        }
        if let RollupOp::Sum(name) = &r.op
            && !fields
                .columns
                .iter()
                .any(|c| c.name == *name && matches!(c.ty, ColumnType::Integer | ColumnType::Real))
        {
            return Err(invalid("SUM requires a declared numeric column"));
        }
    }
    Ok(())
}

fn sql_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Integer => "INTEGER",
        ColumnType::Real => "REAL",
        ColumnType::Text | ColumnType::Timestamp => "TEXT",
    }
}

/// A mismatched JSON type becomes a blob, which the STRICT table refuses. This also
/// prevents SQLite's lossless coercion of a numeric string into an integer or real.
fn projected(column: &ColumnDecl, row: &str) -> String {
    let path = literal(&format!("$.{}", column.name));
    let allowed = match column.ty {
        ColumnType::Integer => "'integer'",
        ColumnType::Real => "'integer','real'",
        ColumnType::Text | ColumnType::Timestamp => "'text'",
    };
    format!(
        "CASE WHEN json_type({row}.props,{path}) NOT IN ('null',{allowed}) THEN x'00' ELSE json_extract({row}.props,{path}) END"
    )
}

pub(crate) fn declare(
    conn: &Connection,
    label: &str,
    old: &TypeDeclaration,
    fields: &TypeDeclaration,
) -> Result<()> {
    let table = identifier(&format!("col_{label}"));
    let ai = identifier(&format!("col_insert_{label}"));
    let au = identifier(&format!("col_update_{label}"));
    let had_table = !old.columns.is_empty() || !old.rollups.is_empty();
    let keep_table = !fields.columns.is_empty() || !fields.rollups.is_empty();
    if had_table {
        for r in &old.rollups {
            conn.execute(
                "INSERT INTO rollup_input(name,key,delta,real_sum) VALUES (?,NULL,0,0)",
                [&r.name],
            )
            .sql()?;
        }
        if keep_table {
            conn.execute_batch(&format!(
                "CREATE TEMP TABLE previous_label_columns AS SELECT * FROM {table};
                CREATE UNIQUE INDEX temp.previous_label_rid ON previous_label_columns(rid);"
            ))
            .sql()?;
        }
        conn.execute_batch(&format!(
            "DROP TRIGGER {ai}; DROP TRIGGER {au}; DROP TABLE {table};"
        ))
        .sql()?;
    }
    if fields.columns.is_empty() && fields.rollups.is_empty() {
        return Ok(());
    }
    let mut defs = vec![
        "rid INTEGER PRIMARY KEY REFERENCES node(rid) ON DELETE CASCADE".to_owned(),
        "_active INTEGER NOT NULL".into(),
    ];
    let mut names = vec!["rid".to_owned(), "_active".into()];
    for c in &fields.columns {
        let name = identifier(&c.name);
        let check = if c.ty == ColumnType::Timestamp {
            format!(" CHECK ({name} IS NULL OR kaban_valid_timestamp({name}))")
        } else {
            String::new()
        };
        defs.push(format!("{name} {}{check}", sql_type(c.ty)));
        names.push(name);
    }
    conn.execute_batch(&format!("CREATE TABLE {table}({}) STRICT;", defs.join(",")))
        .sql()?;
    capture_triggers(conn, label, &table, fields)?;
    for c in fields.columns.iter().filter(|c| c.indexed) {
        let index = identifier(&format!("col_idx_{}_{}_{}", label.len(), label, c.name));
        conn.execute_batch(&format!(
            "CREATE INDEX {index} ON {table}({});",
            identifier(&c.name)
        ))
        .sql()?;
    }
    let projection = |row: &str| {
        let mut values = vec![format!("{row}.rid"), format!("{row}.lifecycle='active'")];
        values.extend(fields.columns.iter().map(|c| projected(c, row)));
        values.join(",")
    };
    let label = literal(label);
    let columns = names.join(",");
    let updates = names
        .iter()
        .skip(1)
        .map(|c| format!("{c}=excluded.{c}"))
        .collect::<Vec<_>>()
        .join(",");
    let new = projection("new");
    let insert = format!(
        "INSERT INTO {table}({columns}) SELECT {new} WHERE new.kind={label} AND new.lifecycle<>'purged'"
    );
    let mut changed = vec!["_active=excluded._active".to_owned()];
    for column in &fields.columns {
        let name = identifier(&column.name);
        let path = literal(&format!("$.{}", column.name));
        // Property writes only replace the columns whose source properties changed.
        // A lifecycle change or an unrelated property edit preserves independent column writes.
        changed.push(format!(
            "{name}=CASE WHEN old.kind=new.kind AND
            json_type(old.props,{path}) IS json_type(new.props,{path}) AND
            json_extract(old.props,{path}) IS json_extract(new.props,{path})
            THEN {table}.{name} ELSE excluded.{name} END"
        ));
    }
    let changed = changed.join(",");
    conn.execute_batch(&format!(
        "CREATE TRIGGER {ai} AFTER INSERT ON node WHEN new.kind={label} BEGIN
           {insert} ON CONFLICT(rid) DO UPDATE SET {updates}; END;
         CREATE TRIGGER {au} AFTER UPDATE OF kind,props,lifecycle ON node WHEN new.kind={label} OR old.kind={label} BEGIN
           DELETE FROM {table} WHERE rid=old.rid AND (new.kind<>{label} OR new.lifecycle='purged');
           {insert} ON CONFLICT(rid) DO UPDATE SET {changed}; END;"
    )).sql()?;
    let mut values = vec!["n.rid".to_owned(), "n.lifecycle='active'".into()];
    for column in &fields.columns {
        let value = if had_table
            && old
                .columns
                .iter()
                .any(|previous| previous.name == column.name)
        {
            let name = identifier(&column.name);
            let allowed = match column.ty {
                ColumnType::Integer => "'integer'",
                ColumnType::Real => "'integer','real'",
                ColumnType::Text | ColumnType::Timestamp => "'text'",
            };
            format!("CASE WHEN typeof(p.{name}) IN ('null',{allowed}) THEN p.{name} ELSE x'00' END")
        } else {
            projected(column, "n")
        };
        values.push(value);
    }
    let values = values.join(",");
    let previous = if had_table {
        " JOIN temp.previous_label_columns p ON p.rid=n.rid"
    } else {
        ""
    };
    conn.execute_batch(&format!(
        "INSERT INTO {table}({columns}) SELECT {values}
        FROM node n{previous} WHERE n.kind={label} AND n.lifecycle<>'purged';"
    ))
    .sql()?;
    if had_table {
        conn.execute_batch("DROP TABLE temp.previous_label_columns")
            .sql()?;
    }
    Ok(())
}
