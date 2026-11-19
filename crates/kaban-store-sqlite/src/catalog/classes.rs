//! The production index classes (spec 11 T6 seam): text, graph and code, one module each so
//! T7, T11 and T16 each fill their own file. Every writer catalog registers them at attach.

use std::sync::Arc;

use super::Catalog;
use super::code_class::CodeClass;
use super::graph_class::GraphClass;
use super::text_class::TextClass;

/// Register the text, graph and code classes on `catalog`, in that order.
pub(crate) fn register_all(catalog: &Catalog) {
    catalog.register(Arc::new(TextClass::new()));
    catalog.register(Arc::new(GraphClass::new()));
    catalog.register(Arc::new(CodeClass::new()));
}
