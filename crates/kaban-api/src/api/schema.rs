//! `KabanApi::schema_describe` and `KabanApi::schema_extend`: the graph schema surface.

use super::KabanApi;
use crate::error::ApiError;
use crate::types::{Naming, SchemaExtendRequest, SchemaResponse};
use kaban_core::TypeDef;
use kaban_core::ontology::resolve::normalise_stored;

impl KabanApi {
    /// Describe the current graph schema: the declared kinds and rels database-wide, each
    /// with its namespace.
    ///
    /// # Errors
    /// Store errors.
    pub fn schema_describe(&self) -> Result<SchemaResponse, ApiError> {
        let defs = self.store.schema()?;
        // every definition passes the read gate (review R1-4): masked or hidden; `ns` is the
        // persisted column (0004) and is derived from the stored name for a legacy row only
        let (labels, relationships): (Vec<TypeDef>, Vec<TypeDef>) = defs
            .into_iter()
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
        let kind = self.gate(t, "kind", &p.kind, &mut red)?;
        let name = self.gate(t, "name", &p.name, &mut red)?;
        let description = self.gate(t, "description", &p.description, &mut red)?;
        let mut from_kinds = Vec::new();
        for s in &p.from_kinds {
            from_kinds.push(self.gate(t, "from_kinds", s, &mut red)?);
        }
        let mut to_kinds = Vec::new();
        for s in &p.to_kinds {
            to_kinds.push(self.gate(t, "to_kinds", s, &mut red)?);
        }
        let props_schema = match &p.props_schema {
            Some(v) => Some(self.gate_json(t, "props_schema", v, &mut red)?),
            None => None,
        };
        if !red.is_empty() {
            // schema entries cannot be redacted; record the redaction verdicts, then refuse
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
