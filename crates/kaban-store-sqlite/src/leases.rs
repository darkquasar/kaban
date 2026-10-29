//! Leases with fencing tokens (core-spec F15; core-plan C2.5; MD-14): sync acquire and renew
//! on the `leases` table of migration 0002. Expiry is database time (`unixepoch()`), so the
//! processes sharing a file need no clock agreement. The caller wraps each call in a
//! `BEGIN IMMEDIATE` transaction (N4).

use crate::sql_error::SqlResultExt;
use kaban_core::{Lease, Result, StoreError};
use rusqlite::{Connection, OptionalExtension, params};

/// Insert the lease when the row is absent, or take it over when its `expires_at` has passed,
/// incrementing `fence`; then read the row back. `Some(lease)` only when the row now belongs
/// to `owner` with a fence that moved in this call.
pub(crate) fn acquire(
    conn: &Connection,
    name: &str,
    owner: &str,
    ttl_s: u32,
) -> Result<Option<Lease>> {
    let before: Option<i64> = conn
        .query_row("SELECT fence FROM leases WHERE name = ?1", [name], |r| {
            r.get(0)
        })
        .optional()
        .sql()?;
    // the plan's statement (core-plan C2.5): a free row is inserted with fence 1, an expired
    // row changes owner with fence + 1, an unexpired row (whoever holds it) is left alone
    conn.execute(
        "INSERT INTO leases(name, owner, fence, expires_at)
         VALUES (?1, ?2, 1, unixepoch() + ?3)
         ON CONFLICT(name) DO UPDATE
           SET owner = excluded.owner, fence = fence + 1, expires_at = unixepoch() + ?3
           WHERE expires_at < unixepoch()",
        params![name, owner, i64::from(ttl_s)],
    )
    .sql()?;
    let row = conn
        .query_row(
            "SELECT owner, fence, expires_at FROM leases WHERE name = ?1",
            [name],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
        .sql()?;
    let Some((row_owner, fence, expires_at)) = row else {
        return Ok(None);
    };
    if row_owner != owner || before == Some(fence) {
        return Ok(None);
    }
    Ok(Some(Lease {
        name: name.to_owned(),
        owner: owner.to_owned(),
        fence: u64::try_from(fence).unwrap_or(0),
        expires_at,
    }))
}

/// Extend `lease` while its fence is current; zero affected rows is `LeaseLost`.
pub(crate) fn renew(conn: &Connection, lease: &Lease, ttl_s: u32) -> Result<()> {
    let fence = i64::try_from(lease.fence).unwrap_or(i64::MAX);
    let changed = conn
        .execute(
            "UPDATE leases SET expires_at = unixepoch() + ?3 WHERE name = ?1 AND fence = ?2",
            params![lease.name, fence, i64::from(ttl_s)],
        )
        .sql()?;
    if changed == 0 {
        return Err(StoreError::LeaseLost {
            name: lease.name.clone(),
            fence: lease.fence,
        });
    }
    Ok(())
}

/// Give `lease` back while its fence is current (fence-checked like [`renew`], R5-11): the
/// expiry moves into the past, the row and its fence stay, so the next acquisition is
/// fence + 1 and a successor is never expired; zero affected rows is `LeaseLost`.
pub(crate) fn release(conn: &Connection, lease: &Lease) -> Result<()> {
    let fence = i64::try_from(lease.fence).unwrap_or(i64::MAX);
    let changed = conn
        .execute(
            "UPDATE leases SET expires_at = unixepoch() - 1 WHERE name = ?1 AND fence = ?2",
            params![lease.name, fence],
        )
        .sql()?;
    if changed == 0 {
        return Err(StoreError::LeaseLost {
            name: lease.name.clone(),
            fence: lease.fence,
        });
    }
    Ok(())
}
