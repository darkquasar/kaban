//! `KabanApi::schema_describe` and `KabanApi::schema_extend`: the graph schema surface.

use super::KabanApi;
use crate::error::ApiError;
use crate::types::{Naming, SchemaExtendRequest, SchemaResponse};
use kaban_core::TypeDef;
use kaban_core::ontology::resolve::normalise_stored;
use std::collections::BTreeSet;

/// The namespace of discovered kinds (engine-spec §3.1: reserved, never a parseable `Ns`);
/// their `schema_type` rows are shared by every workspace and scoped through `kind_stats`.
const DISCOVERED_NS: &str = "x";

impl KabanApi {
    /// Describe the current graph schema: the declared kinds and rels database-wide, the
    /// discovered (`x:`) kinds of the bound workspace only (EN-F5, EN-F6, EN-S3), each with
    /// its namespace, plus the bound workspace's `kind_stats`.
    ///
    /// # Errors
    /// Store errors.
    pub fn schema_describe(&self) -> Result<SchemaResponse, ApiError> {
        let defs = self.store.schema()?;
        // EN-F6, EN-S3: the counters of the bound workspace only; an `x:` definition is shown
        // where the workspace counts it (E1.9a RE3-6: discovered vocabulary never crosses the
        // workspace boundary, declared vocabulary is database-wide)
        let kind_stats = self.store.kind_stats(&self.config.workspace)?;
        let discovered_here: BTreeSet<&str> = kind_stats
            .iter()
            .filter(|s| s.ns == DISCOVERED_NS)
            .map(|s| s.kind.as_str())
            .collect();
        // every definition passes the read gate (review R1-4): masked or hidden; `ns` is the
        // persisted column (0004) and is derived from the stored name for a legacy row only
        let (labels, relationships): (Vec<TypeDef>, Vec<TypeDef>) = defs
            .into_iter()
            .filter(|d| {
                d.ns.as_deref() != Some(DISCOVERED_NS)
                    || d.name
                        .strip_prefix("x:")
                        .is_some_and(|local| discovered_here.contains(local))
            })
            .map(|mut d| {
                if d.ns.is_none() {
                    d.ns = crate::ontology::namespace_of(&d.kind, &d.name, d.builtin);
                }
                d
            })
            .filter_map(|d| self.project_typedef(d))
            .partition(|d| d.kind == "label");
        Ok(SchemaResponse {
            labels,
            relationships,
            naming: Naming {
                label: "PascalCase".into(),
                rel: "UPPER_SNAKE_CASE".into(),
            },
            kind_stats,
        })
    }

    /// Register a new node kind or relationship type. Schema entries cannot be redacted: any
    /// denylist hit refuses the call.
    ///
    /// # Errors
    /// `Rejected`, `Invalid` (denylist hit that would have been redacted) or store errors.
    #[allow(clippy::needless_pass_by_value)] // core-plan §6 interface: requests are taken by value
    pub fn schema_extend(&self, p: SchemaExtendRequest) -> Result<TypeDef, ApiError> {
        let mut red = Vec::new();
        let t = "schema_extend";
        let mut ctx = self.write_ctx(None);
        let kind = self.gate(&mut ctx, t, "kind", &p.kind, &mut red)?;
        let name = self.gate(&mut ctx, t, "name", &p.name, &mut red)?;
        let description = self.gate(&mut ctx, t, "description", &p.description, &mut red)?;
        let mut from_kinds = Vec::new();
        for s in &p.from_kinds {
            from_kinds.push(self.gate(&mut ctx, t, "from_kinds", s, &mut red)?);
        }
        let mut to_kinds = Vec::new();
        for s in &p.to_kinds {
            to_kinds.push(self.gate(&mut ctx, t, "to_kinds", s, &mut red)?);
        }
        let props_schema = match &p.props_schema {
            Some(v) => Some(self.gate_json(&mut ctx, t, "props_schema", v, &mut red)?),
            None => None,
        };
        if !red.is_empty() {
            // schema entries cannot be redacted; record the redaction verdicts, then refuse
            self.flush_audit(&mut ctx)?;
            return Err(ApiError::Invalid(
                "denylist hit in schema definition; schema entries cannot be redacted, remove the value".into(),
            ));
        }
        let def = TypeDef {
            kind,
            // the canonical stored spelling (E1.9a RE3-2), as the store's gate applies it
            name: normalise_stored(&name),
            ns: None,
            description,
            from_kinds,
            to_kinds,
            directed: p.directed.unwrap_or(true),
            props_schema,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        };
        Ok(self.store.schema_extend(def)?)
    }
}
