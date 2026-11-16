//! The SQLite value a table function receives as an argument and returns in a row.

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
