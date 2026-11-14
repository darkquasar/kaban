//! The one inventory of persisted node strings (M2 review R2-2), shared by the rescan job and
//! the read projection so that both judge exactly the same strings: a reject hit in any of
//! them hides the node (read path) or tombstones it (rescan); a redact hit is masked.
//!
//! ```text
//!   Node (every TEXT column except id, timestamps, hashes and closed vocabularies)
//!   +-------------------------------------------------------------------------------+
//!   | kind, workspace_id            -> scanned, never rewritten (projection masks)   |
//!   | text, summary, tags, props,   -> scanned and, on a redact hit, rewritten in    |
//!   | path_scope, session_id,          place by the rescan (RedactedStrings)         |
//!   | agent_id, repo, commit_sha,                                                    |
//!   | accepted_commit, dup_key                                                       |
//!   +-------------------------------------------------------------------------------+
//! ```
//!
//! The destructuring in [`KabanApi::scan_node_strings`] names every field of [`Node`] without
//! `..`, so a column added to the model fails to compile until this inventory decides on it.

use crate::api::KabanApi;
use crate::error::ApiError;
use kaban_core::{Node, RedactedStrings};

/// Every persisted string of a node after the policy's redact rules ran over it.
pub(crate) struct MaskedNode {
    /// Masked kind (never rewritten in place: it names a schema entry).
    pub(crate) kind: String,
    /// Masked workspace (never rewritten in place: it names a tenant).
    pub(crate) workspace_id: String,
    /// The rewritable strings.
    pub(crate) strings: RedactedStrings,
}

impl KabanApi {
    /// Scan every persisted string of `n` under `tool`; `Err` on the first reject hit,
    /// otherwise the masked strings.
    pub(crate) fn scan_node_strings(
        &self,
        tool: &str,
        n: &Node,
        red: &mut Vec<String>,
    ) -> Result<MaskedNode, ApiError> {
        let Node {
            id: _,
            kind,
            text,
            summary,
            tags,
            confidence: _,
            importance: _,
            lifecycle: _,
            store_scope: _,
            path_scope,
            rev: _,
            serialized_rev: _,
            dup_key,
            created_at: _,
            updated_at: _,
            valid_from: _,
            valid_to: _,
            session_id,
            agent_id,
            workspace_id,
            repo,
            commit_sha,
            accepted_commit,
            props,
            content_hash: _,
            policy_version: _,
        } = n;
        let kind = self.scan_str(tool, "kind", kind, red)?;
        let workspace_id = self.scan_str(tool, "workspace_id", workspace_id, red)?;
        let text = self.scan_str(tool, "text", text, red)?;
        let summary = self.scan_opt(tool, "summary", summary.as_deref(), red)?;
        let tags = tags
            .iter()
            .map(|t| self.scan_str(tool, "tag", t, red))
            .collect::<Result<Vec<_>, _>>()?;
        let props = self.scan_json(tool, "props", props, red)?;
        let path_scope = path_scope
            .iter()
            .map(|g| self.scan_str(tool, "path_scope", g, red))
            .collect::<Result<Vec<_>, _>>()?;
        let session_id = self.scan_opt(tool, "session_id", session_id.as_deref(), red)?;
        let agent_id = self.scan_opt(tool, "agent_id", agent_id.as_deref(), red)?;
        let repo = self.scan_opt(tool, "repo", repo.as_deref(), red)?;
        let commit_sha = self.scan_opt(tool, "commit_sha", commit_sha.as_deref(), red)?;
        let accepted_commit =
            self.scan_opt(tool, "accepted_commit", accepted_commit.as_deref(), red)?;
        let dup_key = self.scan_opt(tool, "dup_key", dup_key.as_deref(), red)?;
        Ok(MaskedNode {
            kind,
            workspace_id,
            strings: RedactedStrings {
                text,
                summary,
                tags,
                props,
                path_scope,
                session_id,
                agent_id,
                repo,
                commit_sha,
                accepted_commit,
                dup_key,
            },
        })
    }

    /// [`Self::scan_str`] over an optional string (`None` stays `None`, nothing to scan).
    fn scan_opt(
        &self,
        tool: &str,
        field: &str,
        s: Option<&str>,
        red: &mut Vec<String>,
    ) -> Result<Option<String>, ApiError> {
        match s {
            Some(s) => Ok(Some(self.scan_str(tool, field, s, red)?)),
            None => Ok(None),
        }
    }
}
