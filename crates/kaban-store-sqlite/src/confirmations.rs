//! The `confirmations` table (core-plan C3.2, PR-3; core-spec F4 handshake): one row per issued
//! confirmation token, keyed by the token's keyed hash (the token itself is never stored), with
//! the request strings it was issued for. Database-scoped (F17): no workspace column.

use crate::sql_error::SqlResultExt;
use kaban_core::{Confirmation, NewConfirmation, Result};
use rusqlite::{Connection, params};
use subtle::ConstantTimeEq;

/// Insert one row; `expires_at = now + ttl_s`.
pub(crate) fn create(conn: &Connection, c: &NewConfirmation) -> Result<()> {
    conn.execute(
        "INSERT INTO confirmations(token_hash,node_id,reason,path_scope,requested_by,host,session_id,created_at,expires_at)
         VALUES (?,?,?,?,?,?,?,unixepoch(),unixepoch()+?)",
        params![
            c.token_hash,
            c.node_id,
            c.reason,
            serde_json::to_string(&c.path_scope)?,
            c.requested_by,
            c.host,
            c.session_id,
            c.ttl_s,
        ],
    ).sql()?;
    Ok(())
}

/// Mark the live row of `node_id` whose hash equals `token_hash` as consumed and return it.
/// Candidates are selected by node (unconsumed, unexpired) and the hash is compared in constant
/// time in Rust, not by SQL. Runs inside the caller's `BEGIN IMMEDIATE` transaction.
pub(crate) fn consume(
    conn: &Connection,
    token_hash: &str,
    node_id: &str,
) -> Result<Option<Confirmation>> {
    let mut st = conn.prepare_cached(
        "SELECT token_hash,node_id,reason,path_scope,requested_by,host,session_id,created_at,expires_at
         FROM confirmations WHERE node_id = ? AND consumed_at IS NULL AND expires_at > unixepoch()
         ORDER BY created_at, token_hash",
    ).sql()?;
    let rows = st
        .query_map([node_id], |r| {
            let hash: String = r.get(0)?;
            let path_scope: String = r.get(3)?;
            Ok((
                hash,
                Confirmation {
                    node_id: r.get(1)?,
                    reason: r.get(2)?,
                    path_scope: serde_json::from_str(&path_scope).unwrap_or_default(),
                    requested_by: r.get(4)?,
                    host: r.get(5)?,
                    session_id: r.get(6)?,
                    created_at: r.get(7)?,
                    expires_at: r.get(8)?,
                },
            ))
        })
        .sql()?
        .collect::<rusqlite::Result<Vec<_>>>()
        .sql()?;
    // every candidate is compared, so the time taken does not reveal which (if any) matched
    let mut matched: Option<(String, Confirmation)> = None;
    for (hash, row) in rows {
        let equal = bool::from(hash.as_bytes().ct_eq(token_hash.as_bytes()));
        if equal && matched.is_none() {
            matched = Some((hash, row));
        }
    }
    let Some((hash, row)) = matched else {
        return Ok(None);
    };
    let changed = conn.execute(
        "UPDATE confirmations SET consumed_at = unixepoch() WHERE token_hash = ? AND consumed_at IS NULL",
        [hash],
    ).sql()?;
    Ok((changed == 1).then_some(row))
}

/// Delete rows that expired more than a day ago; returns how many.
pub(crate) fn prune(conn: &Connection) -> Result<u64> {
    let n = conn
        .execute(
            "DELETE FROM confirmations WHERE expires_at < unixepoch() - 86400",
            [],
        )
        .sql()?;
    Ok(u64::try_from(n).unwrap_or(0))
}
