//! `kaban ontology load <file> [--json]` and `kaban ontology list [--json]` (engine-spec
//! EN-F2, EN-S7, EN-S8; engine-plan E1.3; the discovered-kind verbs `promote` and `prune`
//! went with discovered kinds, PC-14). Both run through the tool seam ([`crate::cli::seam::run_tool`]):
//! the raw request (the file path, caller data) is scanned before anything is parsed, the
//! service's registry (the shipped files plus the repository's startup files, loaded by
//! `open_api`; engine-plan E1.5) loads the file under the S7 containment root (the git
//! repository enclosing the current directory, else the current directory), and the one JSON
//! document passes the response gate before it is printed.
//!
//! Exit codes (core-spec §6): 0 ok; 2 a reject-class denylist hit in the file (rule ids only)
//! or a withheld response; 4 an invalid file (`schema_invalid:*`, `ontology_conflict`,
//! `unknown_namespace`, `ontology_builtin_immutable`, ...), a containment or byte-budget
//! refusal, or a usage error; 5 a store error.

use crate::cli::output::Format;
use crate::cli::seam::run_tool;
use kaban_api::{KabanApi, LoadReport, OntologySummary};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;
use std::process::ExitCode;

/// The `ontology` subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub(crate) enum OntologyCmd {
    /// Load one ontology file into the database (stored names `ns:Kind`; the shipped `core`
    /// and `adr` namespaces are immutable). Exit 0 ok, 2 denylist hit, 4 invalid file or
    /// containment, 5 store error
    Load {
        /// The YAML ontology file; it must lie inside the repository (or the current directory
        /// outside one) and may not be reached through a symlink
        file: PathBuf,
    },
    /// List the loaded ontologies: the shipped ones and every `.kaban/ontology/*.yaml`
    List,
}

/// `load`'s typed request (the path is caller data the seam judged as content).
#[derive(Debug, Deserialize)]
struct LoadRequest {
    file: PathBuf,
}

/// `list`'s document.
#[derive(Debug, Serialize, Deserialize)]
struct ListResponse {
    ontologies: Vec<OntologySummary>,
}

/// Run one `ontology` subcommand over an open service.
pub(crate) fn run(api: &KabanApi, cmd: OntologyCmd, format: Format) -> ExitCode {
    match cmd {
        OntologyCmd::Load { file } => run_tool::<LoadRequest, LoadReport, _>(
            api,
            "ontology_load",
            json!({ "file": file }),
            format,
            |api, request| api.registry().load_file(api, &request.file),
        ),
        OntologyCmd::List => run_tool::<serde_json::Value, ListResponse, _>(
            api,
            "ontology_list",
            json!({}),
            format,
            |api, _| {
                Ok(ListResponse {
                    ontologies: api.registry().list(),
                })
            },
        ),
    }
}
