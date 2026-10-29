//! `kaban://code-index` (stage1-spec G1-F18; stage1-plan CG7.3): the resolved snapshot's
//! `meta` projected over the keys `kaban-index` writes (`crates/kaban-index/src/writer.rs`,
//! `unscanned_meta_rows`), so a caller can see which commit, languages, indexer versions,
//! policy and counts the `code_*` tools answer from. The handler is the resource's one body:
//! it takes no input (nothing to gate on the way in, so the canary sweep skips it), resolves
//! the head like every `code_*` tool and reads the one `meta` table. The adapter gates the
//! serialized value as `code_index` (`control.rs`: the snapshot rows, `languages[]` and
//! `resources[]` are control, every other string is content, so an indexer argv string is
//! masked in place).
//! A missing key is absent, never an error: a snapshot written by another publisher (the test
//! fixtures, the bench corpus) projects to the keys it has.

use std::collections::BTreeMap;

use kaban_codegraph::{CodeGraphReader, Meta};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{CodeError, CoverageIndex, SnapshotRefWire, coverage};
use crate::api::KabanApi;
use crate::error::ApiError;

/// The `kaban://code-index` body (stage1-spec G1-F18): the resolved snapshot and its `meta`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CodeIndexResponse {
    /// The resolved snapshot (commit, exactness, schema major, policy currency, isolation).
    pub snapshot: SnapshotRefWire,
    /// `meta.languages` split on `,` (control: the closed language set), empty when absent.
    pub languages: Vec<String>,
    /// `meta.indexer:<lang>`: the indexer's version text per language (content).
    pub indexers: BTreeMap<String, String>,
    /// `meta.kaban_index_version`.
    pub kaban_index_version: Option<String>,
    /// `meta.scip_version`.
    pub scip_version: Option<String>,
    /// `meta.schema_version` as written (numeric; the admitted major is `snapshot.schema_version`).
    pub schema_version: Option<u32>,
    /// `meta.policy_version`: the denylist policy version the snapshot was gated under
    /// (`snapshot.policy` says whether it is the live one).
    pub policy_version: Option<String>,
    /// `meta.isolation_detail`: the isolation tier's detail text (G1-S5).
    pub isolation_detail: Option<String>,
    /// `meta.resources` split on `,`: the read-only resource kinds the indexer's runs were
    /// given (`modcache`, `registry`, `toolchain`; control, FIX-E); empty without the row.
    pub resources: Vec<String>,
    /// `meta.counts:*` by name, numeric values only.
    pub counts: BTreeMap<String, u64>,
    /// The producer's coverage (`meta.coverage:*`, R-1): every counter and the per-crate
    /// table; `null` when the snapshot carries no coverage row (Go, TypeScript).
    pub coverage: Option<CoverageIndex>,
}

/// Project `meta` over the known keys; anything missing or non-numeric where a number is
/// expected is left out.
fn project(snapshot: SnapshotRefWire, meta: &Meta) -> CodeIndexResponse {
    let text = |k: &str| meta.get(k).cloned();
    let list = |k: &str| {
        meta.get(k).map_or_else(Vec::new, |v| {
            v.split(',')
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect()
        })
    };
    let languages = list("languages");
    let indexers = meta
        .iter()
        .filter_map(|(k, v)| {
            k.strip_prefix("indexer:")
                .map(|lang| (lang.to_owned(), v.clone()))
        })
        .collect();
    let counts = meta
        .iter()
        .filter_map(|(k, v)| Some((k.strip_prefix("counts:")?.to_owned(), v.parse().ok()?)))
        .collect();
    CodeIndexResponse {
        snapshot,
        languages,
        indexers,
        kaban_index_version: text("kaban_index_version"),
        scip_version: text("scip_version"),
        schema_version: meta.get("schema_version").and_then(|v| v.parse().ok()),
        policy_version: text("policy_version"),
        isolation_detail: text("isolation_detail"),
        resources: list("resources"),
        counts,
        coverage: coverage::index(meta),
    }
}

impl KabanApi {
    /// The `kaban://code-index` body: the current head's snapshot ([`Self::resolve_current`])
    /// and its `meta` projected over the known keys.
    ///
    /// # Errors
    /// `Invalid` with the fixed `no_snapshot` or `snapshot_too_new` class when nothing
    /// resolves (stage1-spec G1-S8), or a read failure named by class.
    pub fn code_index(&self) -> Result<CodeIndexResponse, ApiError> {
        let resolved = self.resolve_current()?;
        let reader: &dyn CodeGraphReader = resolved.snapshot.as_ref();
        let meta = reader.meta().map_err(CodeError::from)?;
        Ok(project(self.snapshot_wire(&resolved.r#ref), &meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire() -> SnapshotRefWire {
        SnapshotRefWire {
            commit: "ab".repeat(20),
            exact: true,
            schema_version: 1,
            policy: "current".to_owned(),
            isolation: "none".to_owned(),
        }
    }

    #[test]
    fn project_tolerates_missing_and_non_numeric_keys() {
        let meta: Meta = [
            ("commit", "x"),
            ("languages", "go,typescript"),
            ("indexer:go", "scip-go 0.1"),
            ("counts:symbols", "12"),
            ("counts:odd", "many"),
            ("schema_version", "not-a-number"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect();
        let r = project(wire(), &meta);
        assert_eq!(r.languages, ["go", "typescript"]);
        assert_eq!(
            r.indexers.get("go").map(String::as_str),
            Some("scip-go 0.1")
        );
        assert_eq!(r.counts.get("symbols"), Some(&12));
        assert!(!r.counts.contains_key("odd"));
        assert_eq!(r.schema_version, None);
        assert!(r.resources.is_empty(), "no resources row: no kinds");
        assert_eq!(r.coverage, None, "no coverage rows: no table");
        let empty = project(wire(), &Meta::new());
        assert!(empty.languages.is_empty() && empty.indexers.is_empty() && empty.counts.is_empty());
    }
}
