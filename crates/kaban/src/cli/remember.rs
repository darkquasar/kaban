//! `kaban remember <KIND> <TEXT>`: store a memory from the command line (core-spec F1, F7;
//! core-plan C4.1b). The flags mirror the `remember` tool's scalar and list fields; the
//! structured fields (`props`, `links`, `refs`) are given as JSON text, one object per flag, and
//! `--stdin-json` takes the whole `RememberRequest` wire object instead of any flag. The actor
//! is the agent, as for every CLI write except `accept` (M4-5).
//!
//! The command is an adapter like the MCP server and goes through the same seam (core-spec S1,
//! S4; OWN-10; M4-3): the flags only build the raw request, [`crate::cli::seam::run_tool`] scans
//! it before the typed parse, gates the result and every error text, and prints the one JSON
//! document. A JSON-valued flag that does not parse is reported with a fixed text: its content
//! is caller data and is never echoed.

use crate::cli::ToolArgs;
use crate::cli::output::{EXIT_INVALID, Format, print_error};
use crate::cli::seam::run_write_tool;
use kaban_api::{KabanApi, RememberRequest};
use serde_json::{Map, Value};
use std::process::ExitCode;

/// The command's arguments as parsed by clap (caller data, not judged yet).
#[derive(clap::Args)]
pub(crate) struct Flags {
    /// Node kind (label), for example `Fact`, `Decision`, `Convention`, `Failure` (omit with
    /// --stdin-json)
    kind: Option<String>,
    /// The memory text (omit with --stdin-json)
    text: Option<String>,
    /// One-line summary for compact recall cards
    #[arg(long)]
    summary: Option<String>,
    /// A free-form tag; repeat for several
    #[arg(long = "tag", value_name = "TAG")]
    tags: Vec<String>,
    /// Path or glob a governing record (Decision, Convention) covers; repeat for several
    #[arg(long = "path-scope", value_name = "PATH_SCOPE")]
    path_scope: Vec<String>,
    /// 0..1 how sure you are
    #[arg(long)]
    confidence: Option<f64>,
    /// 0..1 how important this is to recall later
    #[arg(long)]
    importance: Option<f64>,
    /// Workspace the memory belongs to (default: the bound workspace)
    #[arg(long)]
    workspace_id: Option<String>,
    /// Session that produced the memory
    #[arg(long)]
    session_id: Option<String>,
    /// Agent that produced the memory
    #[arg(long)]
    agent_id: Option<String>,
    /// Kind-specific properties as one JSON object (a Decision needs `{"rationale": ...}`)
    #[arg(long)]
    props: Option<String>,
    /// A link created with the node, as one JSON object (`{"rel":"FIXED_BY","to":"<id>"}` or
    /// `{"rel":..,"from":"<id>"}`); repeat for several
    #[arg(long = "link", value_name = "LINK")]
    links: Vec<String>,
    /// A code reference the memory is about, as one JSON object (the `CodeRef` wire shape);
    /// repeat for several
    #[arg(long = "ref", value_name = "REF")]
    refs: Vec<String>,
}

impl ToolArgs for Flags {
    const NAME: &'static str = "remember";

    fn any_given(&self) -> bool {
        self.kind.is_some()
            || self.text.is_some()
            || self.summary.is_some()
            || !self.tags.is_empty()
            || !self.path_scope.is_empty()
            || self.confidence.is_some()
            || self.importance.is_some()
            || self.workspace_id.is_some()
            || self.session_id.is_some()
            || self.agent_id.is_some()
            || self.props.is_some()
            || !self.links.is_empty()
            || !self.refs.is_empty()
    }

    fn into_raw(self) -> Result<Value, ExitCode> {
        let (Some(kind), Some(text)) = (self.kind, self.text) else {
            print_error("error: remember needs the kind and the text (or --stdin-json)");
            return Err(ExitCode::from(EXIT_INVALID));
        };
        let mut args = Map::new();
        args.insert("kind".into(), Value::String(kind));
        args.insert("text".into(), Value::String(text));
        if let Some(summary) = self.summary {
            args.insert("summary".into(), Value::String(summary));
        }
        args.insert("tags".into(), strings(self.tags));
        args.insert("path_scope".into(), strings(self.path_scope));
        if let Some(confidence) = self.confidence {
            args.insert("confidence".into(), number("--confidence", confidence)?);
        }
        if let Some(importance) = self.importance {
            args.insert("importance".into(), number("--importance", importance)?);
        }
        for (key, value) in [
            ("workspace_id", self.workspace_id),
            ("session_id", self.session_id),
            ("agent_id", self.agent_id),
        ] {
            if let Some(value) = value {
                args.insert(key.into(), Value::String(value));
            }
        }
        if let Some(props) = self.props {
            args.insert("props".into(), json_flag("--props", &props)?);
        }
        args.insert("links".into(), json_flags("--link", &self.links)?);
        args.insert("refs".into(), json_flags("--ref", &self.refs)?);
        Ok(Value::Object(args))
    }
}

/// A list flag as a JSON array of strings.
fn strings(values: Vec<String>) -> Value {
    Value::Array(values.into_iter().map(Value::String).collect())
}

/// A numeric flag as a JSON number; clap accepts `NaN` and the infinities as `f64`, which JSON
/// has no number for, so they are refused rather than dropped.
fn number(flag: &str, value: f64) -> Result<Value, ExitCode> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or_else(|| {
            print_error(&format!("error: {flag} must be a finite number"));
            ExitCode::from(EXIT_INVALID)
        })
}

/// The JSON object a structured flag carries, parsed into the raw tree (still unjudged: the seam
/// scans it with the rest of the request). A parse failure is reported with a fixed text: the
/// flag's content is caller data.
fn json_flag(flag: &str, text: &str) -> Result<Value, ExitCode> {
    if let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(text) {
        Ok(value)
    } else {
        print_error(&format!("error: {flag} must be one JSON object"));
        Err(ExitCode::from(EXIT_INVALID))
    }
}

/// [`json_flag`] over a repeated flag, as a JSON array.
fn json_flags(flag: &str, texts: &[String]) -> Result<Value, ExitCode> {
    texts
        .iter()
        .map(|text| json_flag(flag, text))
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

/// Store the memory described by the raw request (flags or `--stdin-json`) through the seam and
/// print the result as JSON.
pub(crate) fn run(api: &KabanApi, raw: Value, format: Format) -> ExitCode {
    run_write_tool(
        api,
        "remember",
        raw,
        &[],
        format,
        |api, request: RememberRequest| api.remember(request),
    )
}
