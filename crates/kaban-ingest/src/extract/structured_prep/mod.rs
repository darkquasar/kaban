//! Pure structured-ingestion preparation, not a job or extraction runtime.
//!
//! Callers supply already-admitted headings and sampled text. These helpers neither gate
//! that content nor open a path, execute instructions, fill records, or confer authority.

pub mod dialect;
pub mod schema;
pub mod signal;
