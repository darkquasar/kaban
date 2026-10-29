//! Reliability defaults and strict input loading, outside the decision path.
use super::{ClaimClass, Fixed, Layer, Method};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Concrete reliability triples. Wildcard defaults are expanded at construction.
#[derive(Debug, Clone)]
pub struct Priors(BTreeMap<(Layer, Method, ClaimClass), Fixed>);

/// Stable failures without echoing user input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PriorsError {
    /// Malformed YAML or an unknown structural field.
    #[error("priors_invalid")]
    Invalid,
    /// An unrecognized layer, method or claim class.
    #[error("priors_unknown_key")]
    UnknownKey,
    /// A value outside [0, 1].
    #[error("priors_out_of_range")]
    OutOfRange,
}

impl Priors {
    /// EN-F12 table: gazetteer and v4 method defaults across all classes;
    /// embed/gliner/llm/human layer defaults across all methods and classes.
    /// Script and reconcile have no implicit reliability. Missing triples reject.
    pub fn defaults() -> Self {
        let mut table = BTreeMap::new();
        for &class in ClaimClass::ALL {
            for (method, value) in [
                (Method::Id, 10000),
                (Method::Path, 9000),
                (Method::Exact, 9000),
                (Method::Alias, 8000),
                (Method::Symbol, 6000),
                (Method::Tag, 9000),
            ] {
                table.insert((Layer::Gazetteer, method, class), Fixed(value));
            }
            for (method, value) in [
                (Method::Measured, 10000),
                (Method::Rule, 8000),
                (Method::Heuristic, 5000),
            ] {
                table.insert((Layer::V4, method, class), Fixed(value));
            }
            // E2.10 (EN-F51): the ER identity claim's weight is already its `raw_score`
            // (`clamp(W / match_threshold)`), so its reliability is one and the score carries
            // the evidence; the only reconcile-layer triple with an implicit prior.
            if class == ClaimClass::Identity {
                table.insert((Layer::Reconcile, Method::Er, class), Fixed::ONE);
            }
            for (layer, value) in [
                (Layer::Embed, 5000),
                (Layer::Gliner, 6000),
                (Layer::Llm, 7500),
                (Layer::Human, 10000),
            ] {
                for &method in Method::ALL {
                    table.insert((layer, method, class), Fixed(value));
                }
            }
        }
        Self(table)
    }

    /// Load a replacement `priors: {layer.method.claim_class: confidence}` table.
    /// Keys are concrete triples; defaults implement the spec's wildcard behavior
    /// by expansion, so no wildcard syntax or implicit fallback is needed here.
    /// # Errors
    /// Unknown structural fields, unknown vocabulary and invalid confidences fail closed.
    pub fn load(yaml: &str) -> Result<Self, PriorsError> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct File {
            priors: BTreeMap<String, f64>,
        }
        let file: File = yaml_serde::from_str(yaml).map_err(|_| PriorsError::Invalid)?;
        Self::from_values(&file.priors)
    }

    /// Convert the merged ontology `priors` block once at the loading seam.
    /// # Errors
    /// Unknown triples and out-of-range values are rejected.
    pub fn from_values(values: &BTreeMap<String, f64>) -> Result<Self, PriorsError> {
        let mut table = BTreeMap::new();
        for (key, value) in values {
            let mut parts = key.split('.');
            let layer = parts
                .next()
                .and_then(Layer::parse)
                .ok_or(PriorsError::UnknownKey)?;
            let method = parts
                .next()
                .and_then(Method::parse)
                .ok_or(PriorsError::UnknownKey)?;
            let class = parts
                .next()
                .and_then(ClaimClass::parse)
                .ok_or(PriorsError::UnknownKey)?;
            if parts.next().is_some() {
                return Err(PriorsError::UnknownKey);
            }
            table.insert(
                (layer, method, class),
                Fixed::from_f64(*value).map_err(|_| PriorsError::OutOfRange)?,
            );
        }
        Ok(Self(table))
    }

    /// The reliability of a concrete supporter, or no prior.
    pub fn lookup(&self, layer: Layer, method: Method, class: ClaimClass) -> Option<Fixed> {
        self.0.get(&(layer, method, class)).copied()
    }
}
