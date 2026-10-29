//! The typed code-graph tools (stage1-spec G1-F12, G1-F14, §6; stage1-plan CG7.1, CG7.2, CG9.1) and the
//! `kaban://code-index` resource (G1-F18; CG7.3), compiled with the `code-graph` feature and
//! merged into the router by [`KabanServer::new`]. Each tool is a thin wrapper over the
//! `KabanApi` handler through [`KabanServer::tool_call`]: the pre-parse scan, the typed gate and
//! the response gate are the adapter's shared seam, nothing is decided here. Every tool is
//! read-only, idempotent and closed-world (§6).

use kaban_api::{
    ApiError, CodeContextRequest, CodeContextResponse, CodeDefinitionRequest,
    CodeDefinitionResponse, CodeError, CodeImplementersRequest, CodeListResponse,
    CodeMembersRequest, CodeNeighboursRequest, CodeNeighboursResponse, CodeReferencesRequest,
    CodeReferencesResponse, CodeTestsForRequest, CodeTestsForResponse,
};
use std::time::Instant;

use kaban_api::KabanApi;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

use super::{KabanServer, gate_text, gated_resource, output_schema};

/// The resolved snapshot's meta (stage1-spec G1-F18).
pub(super) const CODE_INDEX_URI: &str = "kaban://code-index";

/// Warm the code graph once the transport is up (backlog G6): resolve the current snapshot on
/// the blocking pool so that admission (`PRAGMA quick_check` once per process and file
/// generation, half a second on an 83 MB snapshot) and the head's `rev-list` are paid before
/// the first `code_*` call, not by it. The lease is dropped at once (RG2-6); the provider's
/// cache keeps the snapshot open. Nothing is created, nothing is logged but the outcome: one
/// `info` line with the duration when a snapshot resolved, a `debug` line with the fixed class
/// text when none did (no provider, no repository, no snapshot: the tools say the same later).
pub(super) fn spawn_warmup(api: KabanApi) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        let started = Instant::now();
        match api.resolve_current() {
            Ok(resolved) => {
                drop(resolved);
                tracing::info!(
                    ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                    "code graph warmed"
                );
            }
            Err(e) => tracing::debug!(error = %e, "code graph not warmed"),
        }
    })
}

impl KabanServer {
    /// The `kaban://code-index` body: the `code_index` result gated as that shape
    /// ([`gated_resource`]). Without a resolvable snapshot the body is the fixed `no_snapshot`
    /// sentence as content (the resource exists, the snapshot does not; the same text the
    /// tools return as their error), through the text gate like every string that leaves the
    /// adapter. Any other failure is the gated error text.
    pub(super) async fn code_index_resource(&self) -> Result<Result<String, String>, &'static str> {
        self.blocking(|api| {
            let no_snapshot = CodeError::NoSnapshot.to_string();
            match api.code_index() {
                Err(ApiError::Invalid(text)) if text == no_snapshot => Ok(gate_text(api, &text)),
                r => gated_resource(api, "code_index", r),
            }
        })
        .await
    }
}

#[tool_router(router = code_router, vis = "pub(super)")]
impl KabanServer {
    #[tool(
        description = "Implementers of an interface or type in the code-graph snapshot of the current commit: the target is exactly one of symbol (the normalised SCIP key), ref (an exact row ref), {label, name} (exact, case-sensitive; add path to keep one file; several matches answer ambiguous_symbol with the candidates to pick a ref from; an external trait or interface answers by name too, target_note.external true) or {path, line}; results are CodeNode cards sorted by path then ref (default 50, max 200, truncated flag), external implementers only with include_external.",
        output_schema = output_schema::<CodeListResponse>(),
        annotations(
            title = "Code implementers",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_implementers(
        &self,
        Parameters(p): Parameters<CodeImplementersRequest>,
    ) -> CallToolResult {
        self.tool_call("code_implementers", move |api| api.code_implementers(p))
            .await
    }

    #[tool(
        description = "Direct members (CONTAINS children) of a symbol or file in the code-graph snapshot: the target is exactly one of symbol, ref, {label, name} (exact, case-sensitive; add path to keep one file; several matches answer ambiguous_symbol with the candidates, never a merged list), {path, line} or path (the file node); labels keeps only members with those SCIP kind names (for example [\"Method\"]); results are CodeNode cards sorted by path then ref (default 50, max 200, truncated flag).",
        output_schema = output_schema::<CodeListResponse>(),
        annotations(
            title = "Code members",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_members(&self, Parameters(p): Parameters<CodeMembersRequest>) -> CallToolResult {
        self.tool_call("code_members", move |api| api.code_members(p))
            .await
    }

    #[tool(
        description = "The definition card of one node in the code-graph snapshot (kind, label, name, path, definition and enclosing ranges, signature, doc): the target is exactly one of symbol, ref, {label, name} (exact, case-sensitive; add path to keep one file), {path, line} or path; a target matching several nodes is ambiguous_symbol with the candidates (label, name, path, line, ref), select by ref instead.",
        output_schema = output_schema::<CodeDefinitionResponse>(),
        annotations(
            title = "Code definition",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_definition(
        &self,
        Parameters(p): Parameters<CodeDefinitionRequest>,
    ) -> CallToolResult {
        self.tool_call("code_definition", move |api| api.code_definition(p))
            .await
    }

    #[tool(
        description = "Reference sites of a symbol or file in the code-graph snapshot, grouped by enclosing node (a symbol, or the file node when the site has no enclosing symbol): the target is exactly one of symbol, ref, {label, name} (exact, case-sensitive; add path to keep one file; several matches answer ambiguous_symbol with the candidates), {path, line} or path; sites in test files only with include_tests; groups come by path then ref with their sites in range order and SCIP SymbolRole names, and the limit counts sites (default 50, max 200, truncated flag).",
        output_schema = output_schema::<CodeReferencesResponse>(),
        annotations(
            title = "Code references",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_references(
        &self,
        Parameters(p): Parameters<CodeReferencesRequest>,
    ) -> CallToolResult {
        self.tool_call("code_references", move |api| api.code_references(p))
            .await
    }

    #[tool(
        description = "Tests linked to a symbol or file in the code-graph snapshot, in three labelled tiers (how): direct = the target's own TESTED_BY links (a symbol target takes the reference form, matched_by reference; a path target or file: ref the file's naming form, matched_by naming, heuristic true); via = the TESTED_BY links of the target's callers, one REFERENCES hop, the first 50 callers by path, each row naming its caller in via (tests reached only through a wrapper function); textual = only when direct and via are both empty, a heuristic (matched_by naming, heuristic true): test functions whose path or snippet spell every word of the target's name (black-box tests that spawn the binary for a CLI handler). The target is exactly one of symbol, ref, {label, name} (exact, case-sensitive; add path to keep one file; several matches answer ambiguous_symbol with the candidates), {path, line} or path; rows are ranked by path then ref within their tier, tiers in order, then cut (default 50, max 200, truncated flag).",
        output_schema = output_schema::<CodeTestsForResponse>(),
        annotations(
            title = "Code tests for",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_tests_for(
        &self,
        Parameters(p): Parameters<CodeTestsForRequest>,
    ) -> CallToolResult {
        self.tool_call("code_tests_for", move |api| api.code_tests_for(p))
            .await
    }

    #[tool(
        description = "The neighbourhood of exactly one node in the code-graph snapshot: the target is exactly one of symbol, ref, {label, name}, {path, line} or path (several matches are ambiguous_symbol with the candidates, select by ref; {label, name} takes an optional path); depth is 1 to 3 hops in both directions, edge_types keeps only those edge types (CONTAINS, DEFINED_IN, IMPLEMENTS, IMPLEMENTS_MEMBER, REFERENCES, TESTED_BY, TYPE_OF; empty keeps every type); document-local symbols (parameters and variables) are hidden and counted in locals_hidden unless include_locals; nodes come start first then by depth and ref, the limit cuts the nodes after the start (default 50, max 200), edges are kept when both ends survive, and truncated also reports the 200-per-hub and 500-total traversal bounds.",
        output_schema = output_schema::<CodeNeighboursResponse>(),
        annotations(
            title = "Code neighbours",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_neighbours(
        &self,
        Parameters(p): Parameters<CodeNeighboursRequest>,
    ) -> CallToolResult {
        self.tool_call("code_neighbours", move |api| api.code_neighbours(p))
            .await
    }

    #[tool(
        description = "Everything about one node of the code-graph snapshot in one call: the target is exactly one of symbol, ref, {label, name}, {path, line} or path (several matches are ambiguous_symbol with the candidates, select by ref; {label, name} takes an optional path); the answer carries the definition card (signature, doc, snippet), up to 3 callers and 3 callees over REFERENCES edges (approximate: true until a call graph is derived), up to 3 TESTED_BY test links, the governing records of why(symbol) with their currency and superseded_by, and last_change (the commit, author date and subject of the last commit touching the defining file; null when git is slow or the file is unknown, never an error).",
        output_schema = output_schema::<CodeContextResponse>(),
        annotations(
            title = "Code context",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn code_context(&self, Parameters(p): Parameters<CodeContextRequest>) -> CallToolResult {
        self.tool_call("code_context", move |api| api.code_context(p))
            .await
    }
}
