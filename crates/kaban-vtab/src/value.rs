//! The SQLite value a table function receives as an argument and returns in a row.

use kaban_core::query_budget::{BudgetExceeded, BudgetLease, GrowError, Limit, QueryBudget};

/// One SQLite value, owned.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// SQL `NULL`; also what an unbound optional argument reads as.
    Null,
    /// A 64-bit signed integer.
    Integer(i64),
    /// A 64-bit float.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Raw bytes.
    Blob(Vec<u8>),
}

/// Copy `value` into Rust, charging the copy to `lease` before it is made.
///
/// A TEXT or BLOB copy is charged `QueryBudget::allocation_charge(bytes, 8)` for its heap
/// buffer (text that is not valid UTF-8 is charged for its lossy decoding, at most three bytes per
/// input byte); other values copy nothing to the heap and are charged nothing. The charge stays
/// on `lease` for as long as the caller keeps the lease, which must outlive the copy.
///
/// # Errors
/// [`BudgetExceeded`] when the budget refuses the charge; nothing is copied then.
pub(crate) fn copy_charged(
    value: rusqlite::types::ValueRef<'_>,
    lease: &mut BudgetLease,
) -> Result<Value, BudgetExceeded> {
    use rusqlite::types::ValueRef;
    let overflow = BudgetExceeded {
        limit: Limit::Memory,
        value: u64::MAX,
    };
    let heap = match value {
        ValueRef::Text(bytes) if std::str::from_utf8(bytes).is_ok() => bytes.len(),
        ValueRef::Text(bytes) => bytes.len().checked_mul(3).ok_or(overflow)?,
        ValueRef::Blob(bytes) => bytes.len(),
        ValueRef::Null | ValueRef::Integer(_) | ValueRef::Real(_) => return Ok(value.into()),
    };
    let charge = QueryBudget::allocation_charge(u64::try_from(heap).map_err(|_| overflow)?, 8)?;
    lease
        .grow_with(charge, || Ok::<_, BudgetExceeded>(Value::from(value)))
        .map_err(|error| match error {
            GrowError::Budget(exceeded) | GrowError::Alloc(exceeded) => exceeded,
        })
}

impl From<rusqlite::types::ValueRef<'_>> for Value {
    /// Text that is not valid UTF-8 is decoded lossily; SQLite itself does not validate it.
    fn from(value: rusqlite::types::ValueRef<'_>) -> Self {
        use rusqlite::types::ValueRef;
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(v) => Self::Integer(v),
            ValueRef::Real(v) => Self::Real(v),
            ValueRef::Text(bytes) => Self::Text(String::from_utf8_lossy(bytes).into_owned()),
            ValueRef::Blob(bytes) => Self::Blob(bytes.to_vec()),
        }
    }
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        use rusqlite::types::ValueRef;
        let value = match self {
            Self::Null => ValueRef::Null,
            Self::Integer(v) => ValueRef::Integer(*v),
            Self::Real(v) => ValueRef::Real(*v),
            Self::Text(text) => ValueRef::Text(text.as_bytes()),
            Self::Blob(bytes) => ValueRef::Blob(bytes),
        };
        Ok(rusqlite::types::ToSqlOutput::Borrowed(value))
    }
}
