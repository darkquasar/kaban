//! MCP stdio adapter: the eight memory tools (`remember`, `link`, `recall`, `traverse`,
//! `schema_describe`, `schema_extend`, `forget`, `stats`), the ingest tools with the `ingest`
//! feature and the `code_*` tools with `code-graph`, plus four resources (`kaban://schema`,
//! `kaban://stats`, `kaban://documents` with `ingest`, `kaban://code-index` with `code-graph`),
//! each a thin wrapper over [`KabanApi`] that adds no behaviour. Tool errors carry the service
//! error's message text (never an offending value).
//!
//! `call_tool` is written by hand (not `#[tool_handler]`) so the raw `arguments` value is
//! scanned by the denylist before typed deserialization (core-spec S1, `KabanApi::prepare_args`):
//! a serde error can never echo a secret, and the span records only a tool name the router
//! knows (never argument values, never an unrecognized name). The typed handlers gate every
//! string field they use (so redactions are reported and audited inside the write's
//! transaction), and every content, error and resource string leaving the adapter passes the
//! response gate (`KabanApi::gate_response` through [`gate_reply`], its plain-string form
//! `KabanApi::gate_response_text` through [`gate_text`]; review R1-3), and a gated result that
//! no longer has its declared shape is withheld (review R3-9). Errors for an unknown
//! tool or resource are fixed texts without caller data (review RC-3). The adapter calls only
//! those seam entries and never decides itself which field is control (OWN-10, C3.0).
//!
//! Every tool declares an `outputSchema` (the schema of its response type, with the root
//! `type: "object"` the 2025 protocol revisions require, [`output_schema`]) and a successful
//! reply carries the gated value twice, as `content[0].text` and as `structuredContent`
//! (core-spec §6; core-plan C3.7). Both come from the one `gate_response` call in
//! [`gate_reply`]: the handlers never return rmcp's typed JSON wrapper (`wrapper::Json`), which
//! would serialize the ungated value (M3-16). Errors stay text-only.
//!
//! Every [`KabanApi`] call (SQLite, audit chain, denylist) is synchronous and may block, for
//! example in SQLite's `busy_timeout`, so each one runs on `tokio::task::spawn_blocking` through
//! [`KabanServer::blocking`] (core-spec E2, N4; review R0-1). Nothing is held across an `.await`:
//! the closure owns its request and an `Arc` of the API.

use crate::scrub::{OnRefusal, rejected_receipt_document, scrub_error_text};
#[cfg(feature = "ingest")]
use kaban_api::ingest_jobs::{
    IngestBrief, IngestBriefRequest, IngestClaimRequest, IngestClaimResponse, IngestFinishReport,
    IngestFinishRequest, IngestNextRequest, IngestNextResponse, IngestSubmitReport,
    IngestSubmitRequest,
};
use kaban_api::{
    ForgetRequest, ForgetResponse, KabanApi, LinkRequest, LinkResponse, RecallRequest,
    RecallResponse, RememberRequest, RememberResponse, SchemaExtendRequest, SchemaResponse,
    StatsRequest, StatsResponse, TraverseRequest, TraverseResponse,
};
#[cfg(feature = "ingest")]
use kaban_api::{IndexDocsRequest, IndexDocsResponse, IndexStatus};
use kaban_core::TypeDef;
use rmcp::handler::server::common::schema_for_output;
use rmcp::handler::server::router::tool::ToolRouter;
#[cfg(feature = "ingest")]
pub(crate) mod resources;

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
    JsonObject, ListResourcesResult, ListToolsResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig, Tool,
};
use rmcp::service::RequestContext;
use rmcp::{
    ErrorData as McpError, RoleServer, ServerHandler, ServiceExt, schemars, tool, tool_router,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Tool parameters for tools without arguments.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub(crate) struct Empty {}

/// The rmcp server handler.
#[derive(Clone)]
pub(crate) struct KabanServer {
    api: Arc<KabanApi>,
    tool_router: ToolRouter<KabanServer>,
}

/// Text of the tool error returned when a blocking task is cancelled or panics. Fixed on
/// purpose: a panic payload could carry argument content, so none of it is echoed.
const BLOCKING_FAILED: &str = "internal error: the request could not be completed";

/// Text of the tool error returned when the result itself hits a reject rule of the policy
/// (a stored value that predates the rule, for example). Fixed on purpose.
const RESPONSE_WITHHELD: &str =
    "response withheld: the result contains a value the denylist policy forbids";

/// Text of the tool error returned when a redaction changed the result so that it no longer
/// has the shape its `outputSchema` declares (a masked property name, enum value or follow-up
/// tool name; review R3-9). Fixed on purpose: the result is withheld rather than sent as
/// malformed structured output.
const RESPONSE_RESHAPED: &str =
    "response withheld: a denylist redaction changed the declared response shape";

/// Text of the tool error returned when an error message hits a reject rule. Fixed on purpose.
const ERROR_WITHHELD: &str = "error details withheld (denylist hit)";

/// Text of the JSON-RPC error for a tool name the router does not know. Fixed on purpose
/// (the same text rmcp uses): the name is caller data and is never echoed or logged.
const UNKNOWN_TOOL: &str = "tool not found";

/// Text of the JSON-RPC error for a resource URI the server does not serve. Fixed on purpose.
const UNKNOWN_RESOURCE: &str = "resource not found";

/// Span label for a tool name the router does not know (the name itself is never recorded).
const UNRECOGNIZED_TOOL_LABEL: &str = "unknown";

/// The `outputSchema` of a response type: rmcp's generated schema with the root `type:
/// "object"` that MCP 2025-06-18 and 2025-11-25 require (review R3-8). schemars emits a tagged
/// enum (`AcceptResponse`) as a root `oneOf` without a root `type`; every variant is an object,
/// so the sibling is exact, and a strict 2025 client no longer rejects the catalog. A schema
/// that already carries the root type is returned as generated (the shared cached `Arc`).
fn output_schema<T: schemars::JsonSchema + 'static>() -> Arc<JsonObject> {
    let generated = schema_for_output::<T>();
    if generated.contains_key("type") {
        return generated;
    }
    let mut object = JsonObject::clone(&generated);
    object.insert(
        "type".to_owned(),
        serde_json::Value::String("object".to_owned()),
    );
    Arc::new(object)
}

/// The response gate for free text (error messages): masked for redact rules, replaced by a
/// fixed text for reject rules (core-spec S1 "before returning"; review R1-3, RC-6).
fn gate_text(api: &KabanApi, text: &str) -> String {
    api.gate_response_text(text)
        .unwrap_or_else(|_| ERROR_WITHHELD.to_string())
}

/// The response gate for a tool result: every content string of the serialized value (keys and
/// leaves, as plain text) is masked or, on a reject hit, the whole result is withheld; error
/// messages go through [`gate_text`]. The gated value is the reply's `structuredContent` and,
/// serialized, its text content (`CallToolResult::structured`), so the two never differ
/// (M3-16); a withheld or failed result is a text-only error. A redaction that leaves the value
/// outside the declared response type `T` withholds the result too (R3-9, [`gate_value`]).
/// Runs on the blocking pool (a verdict may write an audit row).
fn gate_reply<T: Serialize + DeserializeOwned>(
    api: &KabanApi,
    tool: &str,
    r: Result<T, kaban_api::ApiError>,
    on_refusal: OnRefusal,
) -> CallToolResult {
    match r {
        Ok(v) => match gate_value(api, tool, &v) {
            Ok(clean) => CallToolResult::structured(clean),
            Err(text) => CallToolResult::error(vec![ContentBlock::text(text)]),
        },
        Err(e) => {
            let mut content = vec![ContentBlock::text(gate_text(api, &e.to_string()))];
            // a refused write still answers with its typed outcome (spec 05 SS-10
            // `rejected`): the `{"receipt": ...}` document as a second text block, the same
            // object the CLI prints beside its error; `isError` and the text stay
            if on_refusal == OnRefusal::RejectedReceipt
                && let Some(document) = rejected_receipt_document(api, tool)
            {
                content.push(ContentBlock::text(document));
            }
            CallToolResult::error(content)
        }
    }
}

/// Serialize `tool`'s result, gate every content string in it and check that the gated value
/// still parses as the declared response type `T` (the type whose schema is the tool's
/// `outputSchema`): a mask that replaced a property name, an enum value or a follow-up tool
/// name would otherwise reach a schema-validating client as malformed `structuredContent`
/// (review R3-9). The check borrows the gated tree (no second copy of the value). `Err` carries
/// the fixed withheld text.
fn gate_value<T: Serialize + DeserializeOwned>(
    api: &KabanApi,
    tool: &str,
    v: &T,
) -> Result<serde_json::Value, &'static str> {
    let value = serde_json::to_value(v).map_err(|e| {
        tracing::error!(error = %e, "response serialization failed");
        BLOCKING_FAILED
    })?;
    let clean = api
        .gate_response(tool, &value)
        .map_err(|_| RESPONSE_WITHHELD)?;
    if T::deserialize(&clean).is_err() {
        // the mask is not logged: it names the rule, and the field it replaced may be sensitive
        tracing::warn!(
            tool,
            "a redaction changed the result's declared shape; result withheld"
        );
        return Err(RESPONSE_RESHAPED);
    }
    Ok(clean)
}

/// Resource body through the response gate (`tool` names the tool whose result shape the
/// resource mirrors, and `T` is that result type): the gated JSON text, or the gated error text.
fn gated_resource<T: Serialize + DeserializeOwned>(
    api: &KabanApi,
    tool: &str,
    r: Result<T, kaban_api::ApiError>,
) -> Result<String, String> {
    match r {
        Ok(v) => gate_value(api, tool, &v)
            .map(|clean| clean.to_string())
            .map_err(str::to_owned),
        Err(e) => Err(gate_text(api, &e.to_string())),
    }
}

/// The pre-parse scan of `tool`'s raw arguments (`KabanApi::prepare_args`, the request side of
/// the seam). The arguments are taken by value and handed back for the typed parse: the
/// adapter makes no copy of them (review R3-2, RC-5).
fn prescan(
    api: &KabanApi,
    tool: &str,
    args: JsonObject,
) -> (JsonObject, Result<(), kaban_api::ApiError>) {
    let raw = serde_json::Value::Object(args);
    let verdict = api.prepare_args(tool, &raw);
    // `raw` was built from an object one line above; the fallback cannot be reached and
    // would fail the typed parse (missing fields), never dispatch on other data
    let args = match raw {
        serde_json::Value::Object(args) => args,
        _ => JsonObject::new(),
    };
    (args, verdict)
}

/// Log a failed blocking task (kind only, never its payload) and build the fixed tool error.
fn join_failure(e: &tokio::task::JoinError) -> &'static str {
    tracing::error!(
        panicked = e.is_panic(),
        cancelled = e.is_cancelled(),
        "blocking api task failed"
    );
    BLOCKING_FAILED
}

impl KabanServer {
    /// Run one synchronous API call on the blocking pool. The closure owns everything it needs
    /// (the request by value, the API by `Arc`), so no lock or connection crosses an `.await`.
    async fn blocking<T, F>(&self, f: F) -> Result<T, &'static str>
    where
        T: Send + 'static,
        F: FnOnce(&KabanApi) -> T + Send + 'static,
    {
        let api = Arc::clone(&self.api);
        tokio::task::spawn_blocking(move || f(&api))
            .await
            .map_err(|e| join_failure(&e))
    }

    /// [`Self::blocking`] for tool handlers: the API result, gated as `tool`'s response, as a
    /// tool reply (or the fixed join-failure text).
    async fn tool_call<T, F>(&self, tool: &'static str, f: F) -> CallToolResult
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(&KabanApi) -> Result<T, kaban_api::ApiError> + Send + 'static,
    {
        self.tool_call_with(tool, OnRefusal::ErrorOnly, f).await
    }

    /// [`Self::tool_call`] for a public write (spec 05 SS-10): a refusal carries the typed
    /// `rejected` receipt beside its error text.
    async fn write_tool_call<T, F>(&self, tool: &'static str, f: F) -> CallToolResult
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(&KabanApi) -> Result<T, kaban_api::ApiError> + Send + 'static,
    {
        self.tool_call_with(tool, OnRefusal::RejectedReceipt, f)
            .await
    }

    async fn tool_call_with<T, F>(
        &self,
        tool: &'static str,
        on_refusal: OnRefusal,
        f: F,
    ) -> CallToolResult
    where
        T: Serialize + DeserializeOwned + Send + 'static,
        F: FnOnce(&KabanApi) -> Result<T, kaban_api::ApiError> + Send + 'static,
    {
        match self
            .blocking(move |api| gate_reply(api, tool, f(api), on_refusal))
            .await
        {
            Ok(r) => r,
            Err(msg) => CallToolResult::error(vec![ContentBlock::text(msg)]),
        }
    }
}

/// The typed `code_*` tools (stage1-plan CG7.1), merged into the router below.
#[cfg(feature = "code-graph")]
mod code;

#[tool_router]
impl KabanServer {
    pub(crate) fn new(api: KabanApi) -> Self {
        #[allow(unused_mut)] // mutated only with the `ingest` or `code-graph` feature
        let mut tool_router = Self::tool_router();
        #[cfg(feature = "ingest")]
        {
            tool_router += Self::ingest_router();
        }
        #[cfg(feature = "code-graph")]
        {
            tool_router += Self::code_router();
        }
        Self {
            api: Arc::new(api),
            tool_router,
        }
    }

    #[tool(
        description = "Store a memory node (Fact, Idea, Failure, Improvement, Decision, Insight, Entity, Episode or a registered custom kind). Every field is scanned by the denylist first; sensitive content is rejected (nothing stored) or redacted. Exact duplicates return the existing node. Links are created atomically with the node.",
        output_schema = output_schema::<RememberResponse>(),
        annotations(
            title = "Remember",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn remember(&self, Parameters(p): Parameters<RememberRequest>) -> CallToolResult {
        self.write_tool_call("remember", move |api| api.remember(p))
            .await
    }

    #[tool(
        description = "Create a typed relationship between two existing nodes. The relationship type must be registered (see schema_describe / schema_extend). Re-linking the same (from, rel, to) updates confidence/props. SUPERSEDES/INVALIDATES mark the target node accordingly.",
        output_schema = output_schema::<LinkResponse>(),
        annotations(
            title = "Link",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn link(&self, Parameters(p): Parameters<LinkRequest>) -> CallToolResult {
        self.write_tool_call("link", move |api| api.link(p)).await
    }

    #[tool(
        description = "Hybrid recall: BM25 full-text search over text/summary/tags plus 1-2 hop graph expansion along typed edges, fused with reciprocal rank fusion and weighted by confidence. Returns compact cards (id, kind, summary, why, score). Use full_text=true to get bodies and props.",
        output_schema = output_schema::<RecallResponse>(),
        annotations(
            title = "Recall",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn recall(&self, Parameters(p): Parameters<RecallRequest>) -> CallToolResult {
        self.tool_call("recall", move |api| api.recall(p)).await
    }

    #[tool(
        description = "Bounded graph traversal from one or more node ids along selected relationship types (direction out|in|both, depth 1-4). Returns active nodes and the edges among them.",
        output_schema = output_schema::<TraverseResponse>(),
        annotations(
            title = "Traverse",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn traverse(&self, Parameters(p): Parameters<TraverseRequest>) -> CallToolResult {
        self.tool_call("traverse", move |api| api.traverse(p)).await
    }

    #[tool(
        description = "Describe the current graph schema: node kinds (labels) and relationship types with descriptions and endpoint constraints. Also available as resource kaban://schema.",
        output_schema = output_schema::<SchemaResponse>(),
        annotations(
            title = "Describe schema",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn schema_describe(&self, Parameters(_p): Parameters<Empty>) -> CallToolResult {
        self.tool_call("schema_describe", KabanApi::schema_describe)
            .await
    }

    #[tool(
        description = "Register a new node kind (label, PascalCase) or relationship type (rel, UPPER_SNAKE_CASE) with a description and optional endpoint constraints, so the graph can grow new shapes at runtime. Builtin types cannot be changed.",
        output_schema = output_schema::<TypeDef>(),
        annotations(
            title = "Extend schema",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn schema_extend(
        &self,
        Parameters(p): Parameters<SchemaExtendRequest>,
    ) -> CallToolResult {
        self.tool_call("schema_extend", move |api| api.schema_extend(p))
            .await
    }

    #[tool(
        description = "Forget a node: tombstone (default; keeps history, hidden from every read, never re-proposed as a duplicate), invalidate (tombstone plus governance invalidated with an event), archive (reachable by id and graph expansion only), or purge (destructive; removes text, index entries and edges, the id never reappears).",
        output_schema = output_schema::<ForgetResponse>(),
        annotations(
            title = "Forget",
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn forget(&self, Parameters(p): Parameters<ForgetRequest>) -> CallToolResult {
        self.write_tool_call("forget", move |api| api.forget(p))
            .await
    }

    #[tool(
        description = "Counts of nodes/edges by kind and relationship in the bound workspace (workspace_id \"*\" for all), database size, denylist policy version.",
        output_schema = output_schema::<StatsResponse>(),
        annotations(
            title = "Stats",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn stats(&self, Parameters(p): Parameters<StatsRequest>) -> CallToolResult {
        self.tool_call("stats", move |api| api.stats(p)).await
    }
}

impl KabanServer {
    /// Gate every error string the router produced (typed-argument failures and any other
    /// router error, review RC-3): backticked values are scrubbed, then the text passes the
    /// response gate like every other outgoing string, and JSON-RPC error `data` is dropped.
    /// Handler replies were gated in [`Self::tool_call`]; gating their error text again is
    /// idempotent.
    async fn scrub_response(
        &self,
        r: Result<CallToolResponse, McpError>,
    ) -> Result<CallToolResponse, McpError> {
        match r {
            Ok(CallToolResponse::Complete(mut result)) if result.is_error == Some(true) => {
                for block in &mut result.content {
                    if let Some(raw) = block.as_text() {
                        let scrubbed = scrub_error_text(&raw.text);
                        *block = ContentBlock::text(self.gate_error_text(scrubbed).await);
                    }
                }
                Ok(CallToolResponse::Complete(result))
            }
            Err(e) => {
                let scrubbed = scrub_error_text(&e.message);
                Err(McpError::new(
                    e.code,
                    self.gate_error_text(scrubbed).await,
                    None,
                ))
            }
            other => other,
        }
    }

    /// [`gate_text`] on the blocking pool (the denylist check may write an audit row).
    async fn gate_error_text(&self, text: String) -> String {
        match self.blocking(move |api| gate_text(api, &text)).await {
            Ok(safe) => safe,
            Err(msg) => msg.to_string(),
        }
    }
}

impl KabanServer {
    /// The `kaban://documents` body when `uri` names it (with the `ingest` feature), else
    /// `None` (the caller answers "resource not found").
    #[cfg(feature = "ingest")]
    async fn documents_resource(
        &self,
        uri: &str,
    ) -> Option<Result<Result<String, String>, &'static str>> {
        if !resources::matches(uri) {
            return None;
        }
        let uri = uri.to_owned();
        Some(
            self.blocking(move |api| {
                gated_resource(api, "index_status", resources::documents(api, &uri))
            })
            .await,
        )
    }

    /// Without the `ingest` feature no extra resource exists: a ready `None`, awaited like the
    /// feature-on version (no `async` without an `.await`, DI2.0 (d)).
    #[cfg(not(feature = "ingest"))]
    #[allow(clippy::unused_self)] // signature parity with the feature-on method (one call site)
    fn documents_resource(
        &self,
        _uri: &str,
    ) -> std::future::Ready<Option<Result<Result<String, String>, &'static str>>> {
        std::future::ready(None)
    }
}

/// The document-ingestion tools (docs-ingest-spec D7, D10; DI1.6), compiled with the `ingest`
/// feature and merged into the router by [`KabanServer::new`]. `index_docs` shares the CLI
/// verb's service ([`crate::cli::ingest::index_docs`]).
#[cfg(feature = "ingest")]
#[tool_router(router = ingest_router)]
impl KabanServer {
    #[tool(
        description = "Claim the first available pending document in a bounded workspace-local path-ordered scan. Returns a gated extraction brief and fenced lease, or a null job when idle. External workers carry the fence on next, submit and finish; no extractor or scheduler is executed.",
        output_schema = output_schema::<IngestClaimResponse>(),
        annotations(title = "Claim ingestion job", read_only_hint = false,
            destructive_hint = false, idempotent_hint = false, open_world_hint = false)
    )]
    async fn ingest_claim(&self, Parameters(p): Parameters<IngestClaimRequest>) -> CallToolResult {
        self.tool_call("ingest_claim", move |api| {
            crate::cli::ingest::ingest_claim(api, &p)
        })
        .await
    }

    #[tool(
        description = "Finish a workspace-scoped structured ingestion job: reconcile the union of successful submits, preserve human and foreign edges, and retain a gated canonical report for bounded byte-identical retries. Reopens and revalidates the source and current policy; no extractor is executed.",
        output_schema = output_schema::<IngestFinishReport>(),
        annotations(title = "Finish ingestion job", read_only_hint = false,
            destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ingest_finish(
        &self,
        Parameters(p): Parameters<IngestFinishRequest>,
    ) -> CallToolResult {
        self.write_tool_call("ingest_finish", move |api| {
            crate::cli::ingest::ingest_finish(api, &p)
        })
        .await
    }

    #[tool(
        description = "Validate a workspace-scoped structured extractor return against its checked source and pinned ontology, then atomically publish proposed records, acceptance-safe revision candidates, L3 assertions, owned edges, successful audit and bounded job accumulation. No extractor is executed; governed relationships and foreign targets are refused.",
        output_schema = output_schema::<IngestSubmitReport>(),
        annotations(title = "Submit ingestion records", read_only_hint = false,
            destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ingest_submit(
        &self,
        Parameters(p): Parameters<IngestSubmitRequest>,
    ) -> CallToolResult {
        self.tool_call("ingest_submit", move |api| {
            crate::cli::ingest::ingest_submit(api, &p)
        })
        .await
    }

    #[tool(
        description = "Prepare and place one local document, then publish a workspace-scoped structured extraction brief with an ontology-derived schema and byte-bounded chunk plan. Repeating the same source preserves the original job contract. dry_run previews without persistence; no extractor is executed.",
        output_schema = output_schema::<IngestBrief>(),
        annotations(title = "Ingestion brief", read_only_hint = false,
            destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ingest_brief(&self, Parameters(p): Parameters<IngestBriefRequest>) -> CallToolResult {
        self.tool_call("ingest_brief", move |api| {
            crate::cli::ingest::ingest_brief(api, &p)
        })
        .await
    }

    #[tool(
        description = "Reopen and revalidate a structured ingestion job source, then deliver one chunk slice or a serialized-byte-bounded range of whole chunks. Workspace scoped; no cache or extractor execution. Content or ontology edits make the job stale.",
        output_schema = output_schema::<IngestNextResponse>(),
        annotations(title = "Next ingestion chunk", read_only_hint = false,
            destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn ingest_next(&self, Parameters(p): Parameters<IngestNextRequest>) -> CallToolResult {
        self.tool_call("ingest_next", move |api| {
            crate::cli::ingest::ingest_next(api, &p)
        })
        .await
    }

    #[tool(
        description = "Ingest local documents (Markdown, text) under the given paths of the workspace root into Document, Section and Chunk nodes: deterministic ids, incremental (unchanged files are skipped), every path, title and chunk denylist-gated. Returns the run report (counts only). dry_run counts without writing.",
        output_schema = output_schema::<IndexDocsResponse>(),
        annotations(
            title = "Index documents",
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn index_docs(&self, Parameters(p): Parameters<IndexDocsRequest>) -> CallToolResult {
        self.tool_call("index_docs", move |api| {
            crate::cli::ingest::index_docs(api, &p)
        })
        .await
    }

    #[tool(
        description = "Status of the document index of the bound workspace: documents by status, chunks, blocked chunks and denylist rule ids (counts only), failed documents with their error, last run, outbox lag, pipeline version and the compiled extractors. Also available as resource kaban://documents with the document list.",
        output_schema = output_schema::<IndexStatus>(),
        annotations(
            title = "Index status",
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn index_status(&self, Parameters(_p): Parameters<Empty>) -> CallToolResult {
        self.tool_call("index_status", move |api| {
            kaban_ingest::index_status(api.store().as_ref(), api.workspace())
                .map_err(kaban_api::ApiError::from)
        })
        .await
    }
}

#[allow(clippy::unused_async_trait_impl)] // rmcp's trait is async; resource handlers need no await
impl ServerHandler for KabanServer {
    /// Pre-parse scan of the raw arguments, then dispatch through the generated router. The
    /// span records the router's own copy of the tool name when it knows the tool, otherwise a
    /// fixed label: the requested name is caller data (review RC-3).
    #[tracing::instrument(skip_all, fields(tool = tracing::field::Empty))]
    async fn call_tool(
        &self,
        mut request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let Some(known) = self.tool_router.get(&request.name) else {
            tracing::Span::current()
                .record("tool", tracing::field::display(UNRECOGNIZED_TOOL_LABEL));
            tracing::debug!("unknown tool requested; fixed error returned");
            return Err(McpError::invalid_params(UNKNOWN_TOOL, None));
        };
        tracing::Span::current().record("tool", tracing::field::display(known.name.as_ref()));
        if let Some(args) = request.arguments.take() {
            let tool = request.name.to_string();
            // a rejection writes an audit row inside the store: blocking pool. The arguments
            // move through the scan and come back for the typed parse (no clone, review R3-2)
            let scanned = self.blocking(move |api| prescan(api, &tool, args)).await;
            match scanned {
                // no reject hit: the typed handler gates every field itself (and records the
                // redactions inside the write's transaction), so dispatch proceeds on the
                // request as received (review R1-3, coordinator decision: option A)
                Ok((args, Ok(()))) => request.arguments = Some(args),
                Ok((_, Err(e))) => {
                    tracing::debug!("arguments rejected by the denylist before parsing");
                    return Ok(
                        CallToolResult::error(vec![ContentBlock::text(e.to_string())]).into(),
                    );
                }
                Err(msg) => return Ok(CallToolResult::error(vec![ContentBlock::text(msg)]).into()),
            }
        }
        let tcc = ToolCallContext::new(self, request, context);
        let r = self.tool_router.call(tcc).await;
        self.scrub_response(r).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        // the router keeps its tools in a hash map: sort by name so the catalog is stable
        // across runs (core-spec N5) and the `tools/list` snapshot compares byte for byte
        let mut tools = self.tool_router.list_all();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().enable_resources().build())
            .with_server_info(Implementation::new("kaban", env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "Local second brain. Record facts, ideas, failures, improvements and decisions with `remember`, connect them with `link` (or inline `links`), find them with `recall` (keywords work best), walk them with `traverse`. Read kaban://schema or call schema_describe before inventing a kind or relationship; add new ones with schema_extend. Never include passwords, tokens or secrets: the denylist rejects them and nothing is stored."
                    .to_string(),
            )
    }

    async fn list_resources(
        &self,
        _r: Option<PaginatedRequestParams>,
        _c: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        #[allow(unused_mut)] // extended only with the `ingest` or `code-graph` feature
        let mut list = vec![
            Resource::new("kaban://schema", "schema"),
            Resource::new("kaban://stats", "stats"),
        ];
        #[cfg(feature = "ingest")]
        list.push(Resource::new(resources::URI, "documents"));
        // CG7.3 (G1-F18): the resolved snapshot's meta
        #[cfg(feature = "code-graph")]
        list.push(Resource::new(code::CODE_INDEX_URI, "code-index"));
        Ok(ListResourcesResult {
            resources: list,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _c: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri.clone();
        // both bodies and both error texts pass the response gate on the blocking pool
        let body = match uri.as_str() {
            "kaban://schema" => {
                self.blocking(|api| gated_resource(api, "schema_describe", api.schema_describe()))
                    .await
            }
            // the resource reads the bound workspace
            "kaban://stats" => {
                self.blocking(|api| {
                    gated_resource(api, "stats", api.stats(StatsRequest::default()))
                })
                .await
            }
            // CG7.3 (G1-F18): the resolved code-graph snapshot's meta, gated as `code_index`;
            // without a snapshot the body is the fixed `no_snapshot` sentence
            #[cfg(feature = "code-graph")]
            code::CODE_INDEX_URI => self.code_index_resource().await,
            // DI1.6: the document index listing, gated as the `index_status` result shape;
            // the URI is caller data: an unknown one is the fixed text, no echo (review RC-3)
            other => match self.documents_resource(other).await {
                Some(body) => body,
                None => return Err(McpError::resource_not_found(UNKNOWN_RESOURCE, None)),
            },
        };
        let text = body
            .map_err(|m| McpError::internal_error(m, None))?
            .map_err(|m| McpError::internal_error(m, None))?;
        Ok(ReadResourceResult::new(vec![ResourceContents::text(text, uri)]).into())
    }
}

/// Serve the API over stdio until the client disconnects. Once the transport is up, the
/// rescan job (core-spec S4; core-plan C2.7; MD-18) runs on the blocking pool with a
/// cancellation token that fires when the client leaves: the job stops at the next batch
/// boundary and the server waits for that batch, so earlier batches are committed and the rest
/// resumes at the next start (`KabanApi::rescan_if_policy_changed`). With `code-graph`, the
/// current snapshot is resolved once at the same moment so the first `code_*` call does not
/// pay the admission (G6, [`code::spawn_warmup`]).
pub(crate) async fn serve_stdio(api: KabanApi) -> anyhow::Result<()> {
    let job_api = api.clone();
    let service = KabanServer::new(api)
        .serve(rmcp::transport::stdio())
        .await?;
    let cancel = CancellationToken::new();
    #[cfg(feature = "code-graph")]
    let warmup = code::spawn_warmup(job_api.clone());
    let job = spawn_rescan(job_api, cancel.clone());
    let served = service.waiting().await;
    cancel.cancel();
    if let Err(e) = job.await {
        tracing::error!(error = %e, "rescan task failed");
    }
    #[cfg(feature = "code-graph")]
    if let Err(e) = warmup.await {
        tracing::error!(error = %e, "code warm-up task failed");
    }
    served?;
    Ok(())
}

/// Run the rescan job on the blocking pool; `cancel` is checked between batches. The closure
/// owns its API (`async-clone-before-await`); the outcome is logged, never returned to the
/// client (the server has no caller to report it to).
fn spawn_rescan(api: KabanApi, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || match api.rescan_if_policy_changed(&cancel) {
        Ok(outcome) => {
            if outcome.status == kaban_api::RescanStatus::Cancelled {
                tracing::info!(
                    scanned = outcome.scanned,
                    "rescan cancelled by shutdown; it resumes at the next start"
                );
            }
        }
        Err(e) => tracing::error!(error = %e, "rescan failed; it is retried at the next start"),
    })
}
