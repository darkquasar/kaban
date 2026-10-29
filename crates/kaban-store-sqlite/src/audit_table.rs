//! The `audit` table: an append-only HMAC chain inside the database (core-spec S3).
//!
//! `entry_hmac = HMAC(key, encode(prev_hmac, ts, op, tool, field, workspace_id, policy_version,
//! input_hmac, rules_json))` where `encode` is unambiguous (review R1-7): the domain tag
//! [`DOMAIN`] followed, for every field in that fixed order, by the field's byte length as a
//! little-endian `u64` and then its bytes. No byte value in a field can move a field boundary,
//! so content cannot be redistributed between adjacent fields without changing the HMAC. Rows
//! are appended inside the caller's `BEGIN IMMEDIATE` transaction, so the chain order is
//! SQLite's writer order and never forks across processes. The first row chains onto the
//! literal `genesis`. Chains written by pre-correction binaries (separator-joined encoding) do
//! not verify under this encoding; nothing was released with the old one.
//!
//! A mode-A fork (spec 05 SS-20) starts a child chain with a genesis row (`op = fork`) that
//! chains onto the parent's historical head and binds the parent pack; [`verify`] recomputes
//! this database's own chain from that genesis under its own key, and
//! [`verify_parent_history`] the inherited rows under the parent's key, two verdicts and
//! never one global chain.

use crate::sql_error::SqlResultExt;
use kaban_core::Result;
use kaban_core::{AuditEntry, AuditVerification};
use kaban_policy::AuditSigner;
use rusqlite::{Connection, OptionalExtension, params};
use subtle::ConstantTimeEq;

pub(crate) const GENESIS: &str = "genesis";
/// Version and domain tag at the start of every chain input.
const DOMAIN: &[u8] = b"kaban-audit/v2";

/// The fields of one row, in chain order (`prev_hmac` first, `rules` JSON last).
struct ChainRow<'a> {
    prev_hmac: &'a str,
    ts: i64,
    op: &'a str,
    tool: &'a str,
    field: &'a str,
    workspace_id: &'a str,
    policy_version: &'a str,
    input_hmac: &'a str,
    rules_json: &'a str,
}

impl ChainRow<'_> {
    /// Bytes under the HMAC: the domain tag, then every field as `len_u64_le ‖ bytes`.
    fn input(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(320);
        out.extend_from_slice(DOMAIN);
        for part in [
            self.prev_hmac,
            &self.ts.to_string(),
            self.op,
            self.tool,
            self.field,
            self.workspace_id,
            self.policy_version,
            self.input_hmac,
            self.rules_json,
        ] {
            let len = u64::try_from(part.len()).unwrap_or(u64::MAX);
            out.extend_from_slice(&len.to_le_bytes());
            out.extend_from_slice(part.as_bytes());
        }
        out
    }
}

fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

fn tail_hmac(conn: &Connection) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT entry_hmac FROM audit ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .sql()?
        .unwrap_or_else(|| GENESIS.to_string()))
}

/// Append `entries` to the chain inside the current transaction.
pub(crate) fn append(
    conn: &Connection,
    signer: &dyn AuditSigner,
    entries: &[AuditEntry],
) -> Result<()> {
    if entries.is_empty() {
        return Ok(());
    }
    let mut prev = tail_hmac(conn)?;
    let mut st = conn.prepare_cached(
        "INSERT INTO audit(ts, op, tool, field, workspace_id, policy_version, input_hmac, rules, prev_hmac, entry_hmac) VALUES (?,?,?,?,?,?,?,?,?,?)",
    ).sql()?;
    for e in entries {
        let ts = now_ms();
        let rules = serde_json::to_string(&e.rules)?;
        let op = e.op.as_str();
        let entry_hmac = signer.hmac_hex(
            ChainRow {
                prev_hmac: &prev,
                ts,
                op,
                tool: &e.tool,
                field: &e.field,
                workspace_id: e.workspace_id.as_str(),
                policy_version: &e.policy_version,
                input_hmac: &e.input_hmac,
                rules_json: &rules,
            }
            .input()
            .as_slice(),
        );
        st.execute(params![
            ts,
            op,
            e.tool,
            e.field,
            e.workspace_id.as_str(),
            e.policy_version,
            e.input_hmac,
            rules,
            prev,
            entry_hmac
        ])
        .sql()?;
        prev = entry_hmac;
    }
    Ok(())
}

/// Append read-path entries, skipping those whose `(node_id, policy_version)` was already
/// recorded (`INSERT OR IGNORE INTO read_audit_seen`). Entries without a node id are appended
/// unconditionally.
pub(crate) fn append_read(
    conn: &Connection,
    signer: &dyn AuditSigner,
    entries: &[AuditEntry],
) -> Result<()> {
    let mut fresh: Vec<AuditEntry> = Vec::new();
    for e in entries {
        if let Some(node_id) = &e.node_id {
            let inserted = conn
                .execute(
                    "INSERT OR IGNORE INTO read_audit_seen(node_id, policy_version) VALUES (?, ?)",
                    params![node_id, e.policy_version],
                )
                .sql()?;
            if inserted == 0 {
                continue;
            }
        }
        fresh.push(e.clone());
    }
    append(conn, signer, &fresh)
}

/// The `op` of a fork genesis row (spec 05 SS-20): the first row of a child chain, chained
/// onto the parent's historical head and binding the parent pack in its `rules` object.
pub(crate) const FORK_OP: &str = "fork";

/// What a fork genesis binds (SS-20): the parent pack's hash and identity, the parent
/// instance and its historical audit head. Stored as the row's `rules` JSON object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkBinding {
    pub(crate) parent_pack_id: String,
    pub(crate) parent_pack_sha256: String,
    pub(crate) parent_instance_id: String,
    pub(crate) parent_audit_head: String,
    pub(crate) parent_audit_entries: u64,
    pub(crate) base_commit: String,
    pub(crate) policy_version: String,
}

impl ForkBinding {
    fn to_json(&self) -> String {
        serde_json::json!({
            "parent_pack_id": self.parent_pack_id,
            "parent_pack_sha256": self.parent_pack_sha256,
            "parent_instance_id": self.parent_instance_id,
            "parent_audit_head": self.parent_audit_head,
            "parent_audit_entries": self.parent_audit_entries,
            "base_commit": self.base_commit,
        })
        .to_string()
    }

    fn from_json(rules: &str) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_str(rules).ok()?;
        let s = |key: &str| v.get(key)?.as_str().map(str::to_owned);
        Some(Self {
            parent_pack_id: s("parent_pack_id")?,
            parent_pack_sha256: s("parent_pack_sha256")?,
            parent_instance_id: s("parent_instance_id")?,
            parent_audit_head: s("parent_audit_head")?,
            parent_audit_entries: v.get("parent_audit_entries")?.as_u64()?,
            base_commit: s("base_commit")?,
            policy_version: String::new(),
        })
    }
}

/// The latest fork genesis of the chain: where the child chain starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ForkGenesis {
    /// The genesis row's `seq`.
    pub(crate) seq: u64,
    /// The row's `prev_hmac`: the parent's historical head it chains onto.
    pub(crate) prev_hmac: String,
    /// The binding, when the row's `rules` parse (an altered binding is `None` and fails
    /// verification anyway).
    pub(crate) binding: Option<ForkBinding>,
    /// The `policy_version` column of the genesis row.
    pub(crate) policy_version: String,
}

/// Append the fork genesis inside the caller's transaction: `op = fork`, chained onto the
/// current tail (the parent's historical head), signed by the child's key. Returns its `seq`.
pub(crate) fn append_fork_genesis(
    conn: &Connection,
    signer: &dyn AuditSigner,
    binding: &ForkBinding,
) -> Result<u64> {
    let prev = tail_hmac(conn)?;
    let ts = now_ms();
    let rules = binding.to_json();
    let row = ChainRow {
        prev_hmac: &prev,
        ts,
        op: FORK_OP,
        tool: "pack",
        field: "genesis",
        // the fork spans the whole database, every workspace
        workspace_id: kaban_core::WorkspaceId::ALL,
        policy_version: &binding.policy_version,
        input_hmac: &binding.parent_pack_sha256,
        rules_json: &rules,
    };
    let entry_hmac = signer.hmac_hex(row.input().as_slice());
    conn.execute(
        "INSERT INTO audit(ts, op, tool, field, workspace_id, policy_version, input_hmac, rules, prev_hmac, entry_hmac) VALUES (?,?,?,?,?,?,?,?,?,?)",
        params![
            ts,
            FORK_OP,
            "pack",
            "genesis",
            kaban_core::WorkspaceId::ALL,
            binding.policy_version,
            binding.parent_pack_sha256,
            rules,
            prev,
            entry_hmac
        ],
    )
    .sql()?;
    Ok(u64::try_from(conn.last_insert_rowid()).unwrap_or(0))
}

/// The latest fork genesis, when the chain has one.
pub(crate) fn fork_genesis(conn: &Connection) -> Result<Option<ForkGenesis>> {
    let row = conn
        .query_row(
            "SELECT seq, prev_hmac, rules, policy_version FROM audit WHERE op = ?1 ORDER BY seq DESC LIMIT 1",
            [FORK_OP],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .sql()?;
    Ok(
        row.map(|(seq, prev_hmac, rules, policy_version)| ForkGenesis {
            seq: u64::try_from(seq).unwrap_or(0),
            prev_hmac,
            binding: ForkBinding::from_json(&rules),
            policy_version,
        }),
    )
}

/// The `entry_hmac` of the row before `seq`, `genesis` when there is none.
fn head_before(conn: &Connection, seq: u64) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT entry_hmac FROM audit WHERE seq < ?1 ORDER BY seq DESC LIMIT 1",
            [i64::try_from(seq).unwrap_or(i64::MAX)],
            |r| r.get::<_, String>(0),
        )
        .optional()
        .sql()?
        .unwrap_or_else(|| GENESIS.to_string()))
}

/// Recompute this database's own chain: from the latest fork genesis when there is one
/// (the genesis must chain onto the parent's historical head it binds, SS-20), else from the
/// literal `genesis`. Rows before a genesis are the parent's history and are not this chain:
/// [`verify_parent_history`] checks them under the parent's key.
pub(crate) fn verify(conn: &Connection, signer: &dyn AuditSigner) -> Result<AuditVerification> {
    match fork_genesis(conn)? {
        Some(genesis) => {
            let parent_head = head_before(conn, genesis.seq)?;
            let bound = genesis
                .binding
                .as_ref()
                .is_some_and(|b| b.parent_audit_head == parent_head);
            if !bound {
                return Ok(AuditVerification {
                    entries: 0,
                    ok: false,
                    first_bad_seq: Some(genesis.seq),
                });
            }
            verify_range(conn, signer, genesis.seq, None, &parent_head)
        }
        None => verify_range(conn, signer, 1, None, GENESIS),
    }
}

/// Recompute the parent's history under `parent`: the rows before the latest fork genesis,
/// from the parent's own genesis (the previous fork row) or the literal `genesis`. `None`
/// when this database has no fork genesis (nothing is parent history).
pub(crate) fn verify_parent_history(
    conn: &Connection,
    parent: &dyn AuditSigner,
) -> Result<Option<AuditVerification>> {
    let Some(genesis) = fork_genesis(conn)? else {
        return Ok(None);
    };
    let previous: Option<i64> = conn
        .query_row(
            "SELECT seq FROM audit WHERE op = ?1 AND seq < ?2 ORDER BY seq DESC LIMIT 1",
            params![FORK_OP, i64::try_from(genesis.seq).unwrap_or(i64::MAX)],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    let first = previous.and_then(|s| u64::try_from(s).ok()).unwrap_or(1);
    let expected_prev = head_before(conn, first)?;
    Ok(Some(verify_range(
        conn,
        parent,
        first,
        Some(genesis.seq - 1),
        &expected_prev,
    )?))
}

/// Recompute the rows `first..=last` (to the end when `last` is `None`), the first of which
/// must chain onto `expected_prev`.
fn verify_range(
    conn: &Connection,
    signer: &dyn AuditSigner,
    first: u64,
    last: Option<u64>,
    expected_prev: &str,
) -> Result<AuditVerification> {
    let mut st = conn.prepare(
        "SELECT seq, ts, op, tool, field, workspace_id, policy_version, input_hmac, rules, prev_hmac, entry_hmac FROM audit WHERE seq >= ?1 AND seq <= ?2 ORDER BY seq",
    ).sql()?;
    let first = i64::try_from(first).unwrap_or(i64::MAX);
    let last = last.map_or(i64::MAX, |l| i64::try_from(l).unwrap_or(i64::MAX));
    let rows = st
        .query_map(params![first, last], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
                r.get::<_, String>(9)?,
                r.get::<_, String>(10)?,
            ))
        })
        .sql()?;
    let mut expected_prev = expected_prev.to_owned();
    let mut entries = 0u64;
    for row in rows {
        let (seq, ts, op, tool, field, ws, pv, ih, rules, prev, entry) = row.sql()?;
        entries += 1;
        let recomputed = signer.hmac_hex(
            ChainRow {
                prev_hmac: &prev,
                ts,
                op: &op,
                tool: &tool,
                field: &field,
                workspace_id: &ws,
                policy_version: &pv,
                input_hmac: &ih,
                rules_json: &rules,
            }
            .input()
            .as_slice(),
        );
        let links = prev.as_bytes().ct_eq(expected_prev.as_bytes());
        let intact = recomputed.as_bytes().ct_eq(entry.as_bytes());
        if !bool::from(links & intact) {
            return Ok(AuditVerification {
                entries,
                ok: false,
                first_bad_seq: u64::try_from(seq).ok(),
            });
        }
        expected_prev = entry;
    }
    Ok(AuditVerification {
        entries,
        ok: true,
        first_bad_seq: None,
    })
}
