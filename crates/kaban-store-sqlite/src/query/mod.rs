//! Read-only SQL over one request budget, catalog pin and SQLite read transaction.
mod functions;
mod guard;

use crate::SqliteStore;
use functions::Payload;
use guard::Guard;
use kaban_core::query_budget::{BudgetExceeded, BudgetLease, GrowError, Limit, QueryBudget};
use kaban_core::{QueryLimits, Result, RowSet, SqlValue, Statement, StoreError, ViewName};
use kaban_vtab::{Execution, RequestScope, Runtime};
use rusqlite::{Batch, ErrorCode, OpenFlags, fallible_iterator::FallibleIterator, types::ValueRef};
use std::path::Path;

fn refused(construct: &str, reason: &str) -> StoreError {
    StoreError::QueryRefused {
        construct: construct.into(),
        reason: reason.into(),
    }
}
fn limit(limit: &'static str, value: u64) -> StoreError {
    StoreError::QueryLimit { limit, value }
}
fn exceeded(e: BudgetExceeded) -> StoreError {
    limit(
        match e.limit {
            Limit::Time => "time",
            Limit::Memory => "memory_bytes",
            Limit::Cancelled => "cancelled",
        },
        e.value,
    )
}
fn charge(lease: &mut BudgetLease, bytes: u64) -> Result<()> {
    if bytes == 0 {
        return Ok(());
    }
    let bytes = QueryBudget::allocation_charge(bytes, align_of::<usize>() as u64)?;
    lease
        .grow_with(bytes, || Ok::<_, std::convert::Infallible>(()))
        .map_err(|e| match e {
            GrowError::Budget(e) => e.into(),
            GrowError::Alloc(infallible) => match infallible {},
        })
}
pub(crate) fn run(
    store: &SqliteStore,
    path: Option<&Path>,
    statement: &Statement,
    limits: &QueryLimits,
) -> Result<RowSet> {
    if limits.rows > 1500 {
        return Err(limit("rows", u64::from(limits.rows)));
    }
    if statement.view != ViewName::Main {
        return Err(refused("view", "unsupported graph view"));
    }
    let budget = QueryBudget::new(limits.memory_bytes, Some(limits.time));
    let result = execute(store, path, statement, limits, &budget);
    budget.check().map_err(exceeded)?;
    result.map_err(|e| match e {
        StoreError::BudgetExceeded(e) => exceeded(e),
        other => other,
    })
}
fn execute(
    store: &SqliteStore,
    path: Option<&Path>,
    s: &Statement,
    limits: &QueryLimits,
    budget: &QueryBudget,
) -> Result<RowSet> {
    let runtime =
        Runtime::get().ok_or_else(|| refused("runtime", "query allocator unavailable"))?;
    let scope = RequestScope::enter(runtime, budget)
        .map_err(|_| refused("runtime", "request already active"))?;
    let path = path.ok_or_else(|| refused("database", "dedicated file connection required"))?;
    let mut conn = scope
        .open_query_connection(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|_| refused("database", "cannot open query connection"))?;
    let _inputs = budget.reserve(s.sql.len() as u64)?;
    if s.sql.as_bytes().contains(&0) {
        return Err(refused("statement", "embedded NUL"));
    }
    functions::register(&conn).map_err(|_| refused("model", "cannot register functions"))?;
    let guard = Guard::install(&mut conn, &s.sql, budget)?;
    let result = store.catalog().with_snapshot(&conn, budget, |tx, pin| {
        let same: bool = tx
            .query_row(
                "SELECT value=?1 FROM main.index_meta WHERE key='db_id'",
                [store.db_id()],
                |r| r.get(0),
            )
            .map_err(crate::sql_error::backend_error)?;
        if !same {
            return Err(refused("database", "database identity changed"));
        }
        let payload = Payload::new(tx, pin, &s.view, store, &guard)?;
        let plan = |name, input: &kaban_vtab::PlanInput<'_>, _: &QueryBudget| {
            Ok(payload.plan(name, input))
        };
        let execution = Execution {
            plan: &plan,
            payload: &payload,
        };
        kaban_vtab::with_execution(&conn, &execution, || {
            guard.user(|| {
                let error = |e: rusqlite::Error| {
                    if let Err(e) = budget.check() {
                        return exceeded(e);
                    }
                    if let Some(e) = payload.error.borrow_mut().take() {
                        return e;
                    }
                    if e.sqlite_error_code() == Some(ErrorCode::TooBig) {
                        return limit(
                            "value_bytes",
                            u64::from(limits.value_bytes).saturating_add(1),
                        );
                    }
                    guard.failure()
                };
                let length = rusqlite::limits::Limit::SQLITE_LIMIT_LENGTH;
                let public_limit = i32::try_from(limits.value_bytes).unwrap_or(i32::MAX);
                let catalog_limit = conn.set_limit(length, public_limit).map_err(error)?;
                let mut batch = Batch::new(tx, &s.sql);
                let mut stmt = batch
                    .next()
                    .map_err(error)?
                    .ok_or_else(|| refused("statement", "one statement required"))?;
                if batch.next().map_err(error)?.is_some() {
                    return Err(refused("statements", "one statement required"));
                }
                if !stmt.readonly() {
                    return Err(refused("statement", "read-only statement required"));
                }
                if stmt.parameter_count() != s.params.len() {
                    return Err(refused("parameters", "parameter count differs"));
                }
                if stmt.is_explain() == 0 {
                    // Catalog packs can hold many user values. Their reads are already charged
                    // to the shared memory budget and run before the first public VM step.
                    conn.set_limit(length, catalog_limit).map_err(error)?;
                    guard.catalog(|| payload.load(budget))?;
                    conn.set_limit(length, public_limit).map_err(error)?;
                }
                for (i, param) in s.params.iter().enumerate() {
                    let value = match param {
                        SqlValue::Null => ValueRef::Null,
                        SqlValue::Integer(n) => ValueRef::Integer(*n),
                        SqlValue::Real(n) => ValueRef::Real(*n),
                        SqlValue::Text(text) => ValueRef::Text(text.as_bytes()),
                        SqlValue::Blob(blob) => ValueRef::Blob(blob),
                    };
                    check_size(value, limits)?;
                    stmt.raw_bind_parameter(i + 1, rusqlite::types::ToSqlOutput::Borrowed(value))
                        .map_err(error)?;
                }
                let (mut result, storage) = materialize(&mut stmt, limits, budget, &error)?;
                result.truncated |= payload.truncated.get();
                Ok((result, storage))
            })
        })
        .map_err(|_| refused("execution", "cannot bind statement"))?
    });
    conn.close().map_err(crate::sql_error::backend_error)?;
    result.map(|(rows, _storage)| rows)
}

fn check_size(v: ValueRef<'_>, limits: &QueryLimits) -> Result<usize> {
    let size = match v {
        ValueRef::Text(s) | ValueRef::Blob(s) => s.len(),
        ValueRef::Null => 0,
        ValueRef::Integer(_) | ValueRef::Real(_) => 8,
    };
    if size as u64 > u64::from(limits.value_bytes) {
        return Err(limit("value_bytes", size as u64));
    }
    Ok(size)
}
fn materialize(
    stmt: &mut rusqlite::Statement<'_>,
    limits: &QueryLimits,
    budget: &QueryBudget,
    error: &impl Fn(rusqlite::Error) -> StoreError,
) -> Result<(RowSet, BudgetLease)> {
    let mut storage = budget.reserve(0)?;
    let width = stmt.column_count();
    charge(&mut storage, (width * size_of::<String>()) as u64)?;
    let mut columns = Vec::with_capacity(width);
    for i in 0..width {
        let name = stmt.column_name(i).map_err(error)?;
        charge(&mut storage, name.len() as u64)?;
        columns.push(name.to_owned());
    }
    let cap = limits.rows as usize;
    charge(&mut storage, (cap * size_of::<Vec<SqlValue>>()) as u64)?;
    let mut rows = Vec::with_capacity(cap);
    let mut stream = stmt.raw_query();
    let mut truncated = false;
    while let Some(row) = stream.next().map_err(error)? {
        budget.check()?;
        if rows.len() == cap {
            truncated = true;
            break;
        }
        charge(&mut storage, (width * size_of::<SqlValue>()) as u64)?;
        let mut out = Vec::with_capacity(width);
        for i in 0..width {
            let v = row.get_ref(i).map_err(error)?;
            let size = check_size(v, limits)?;
            let value = match v {
                ValueRef::Null => SqlValue::Null,
                ValueRef::Integer(n) => SqlValue::Integer(n),
                ValueRef::Real(n) => SqlValue::Real(n),
                ValueRef::Text(s) => {
                    charge(&mut storage, size as u64)?;
                    SqlValue::Text(
                        std::str::from_utf8(s)
                            .map_err(|_| refused("TEXT", "invalid UTF-8"))?
                            .to_owned(),
                    )
                }
                ValueRef::Blob(s) => {
                    charge(&mut storage, size as u64)?;
                    SqlValue::Blob(s.to_owned())
                }
            };
            out.push(value);
        }
        rows.push(out);
    }
    Ok((
        RowSet {
            columns,
            rows,
            truncated,
        },
        storage,
    ))
}
