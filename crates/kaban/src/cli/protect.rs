//! `kaban protect` uses the same typed pruning-protection seam as MCP.
use crate::cli::{
    ToolArgs,
    output::{EXIT_INVALID, Format, print_error},
    seam::run_tool_with,
};
use crate::scrub::OnRefusal;
use kaban_api::{KabanApi, PruningProtectionRequest};
use kaban_core::Actor;
use serde_json::{Value, json};
use std::process::ExitCode;

/// Select exactly one target; --stdin-json uses the canonical request instead.
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Closed get/set/clear operation.
    #[arg(value_parser=["get","set","clear"])]
    operation: Option<String>,
    /// Node id.
    #[arg(long)]
    node: Option<String>,
    /// Stable assertion key.
    #[arg(long)]
    claim: Option<String>,
    /// Relationship source node.
    #[arg(long)]
    src: Option<String>,
    /// Relationship destination node.
    #[arg(long)]
    dst: Option<String>,
    /// Relationship name.
    #[arg(long)]
    rel: Option<String>,
}
impl ToolArgs for Flags {
    const NAME: &'static str = "pruning_protection";
    fn any_given(&self) -> bool {
        self.operation.is_some()
            || self.node.is_some()
            || self.claim.is_some()
            || self.src.is_some()
            || self.dst.is_some()
            || self.rel.is_some()
    }
    fn into_raw(self) -> Result<Value, ExitCode> {
        let Some(operation) = self.operation else {
            print_error("error: protect needs get, set or clear (or --stdin-json)");
            return Err(ExitCode::from(EXIT_INVALID));
        };
        let kind = if self.node.is_some() {
            "node"
        } else if self.claim.is_some() {
            "claim"
        } else {
            "relationship"
        };
        let mut raw = json!({"operation":operation,"target_kind":kind});
        for (field, value) in [
            ("id", self.node),
            ("key", self.claim),
            ("src", self.src),
            ("dst", self.dst),
            ("rel", self.rel),
        ] {
            if let Some(v) = value {
                raw[field] = Value::String(v);
            }
        }
        Ok(raw)
    }
}
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    // `get` reads; `set` and `clear` write, so their refusal carries the rejected receipt
    let on_refusal = if raw["operation"] == "get" {
        OnRefusal::ErrorOnly
    } else {
        OnRefusal::RejectedReceipt
    };
    run_tool_with(
        api,
        "pruning_protection",
        raw,
        &[],
        format,
        on_refusal,
        |api, p: PruningProtectionRequest| api.pruning_protection(p, Actor::HumanCli),
    )
}
