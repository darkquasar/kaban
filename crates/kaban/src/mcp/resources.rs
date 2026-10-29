//! The `kaban://documents` resource (docs-ingest-spec D7; docs-ingest-plan.md DI1.6; behind
//! the `ingest` feature): the index status plus one page of the bound workspace's document
//! list (path, status, chunks, lang, title) in the store's id order. Pagination through the URI
//! query (`kaban://documents?offset=N&limit=M`; defaults 0 and 100, limit capped by the
//! service); the page is read as a page, the status by the store's aggregates (DI2.0 (c)).
//! The body passes the response gate as the `index_status` tool's result shape does.

use kaban_api::{ApiError, DocumentsResource, KabanApi};

/// Default page size of the listing.
const DEFAULT_LIMIT: usize = 100;

/// The resource URI without a query.
pub(crate) const URI: &str = "kaban://documents";

/// Whether `uri` is this resource (with or without a query).
pub(crate) fn matches(uri: &str) -> bool {
    uri == URI
        || uri
            .strip_prefix(URI)
            .is_some_and(|rest| rest.starts_with('?'))
}

/// `offset` and `limit` from the URI query (unknown keys ignored, unparsable values default;
/// the URI is caller data and is never echoed).
fn page(uri: &str) -> (usize, usize) {
    let mut offset = 0;
    let mut limit = DEFAULT_LIMIT;
    if let Some((_, query)) = uri.split_once('?') {
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("offset", v)) => offset = v.parse().unwrap_or(0),
                Some(("limit", v)) => limit = v.parse().unwrap_or(DEFAULT_LIMIT),
                _ => {}
            }
        }
    }
    (offset, limit)
}

/// The resource body for `uri` over the bound workspace.
///
/// # Errors
/// Store errors.
pub(crate) fn documents(api: &KabanApi, uri: &str) -> Result<DocumentsResource, ApiError> {
    let (offset, limit) = page(uri);
    kaban_ingest::documents_resource(api.store().as_ref(), api.workspace(), offset, limit)
        .map_err(ApiError::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_query_selects_the_page_and_defaults_otherwise() {
        assert!(matches(URI));
        assert!(matches("kaban://documents?offset=5"));
        assert!(!matches("kaban://documentsx"));
        assert_eq!(page(URI), (0, DEFAULT_LIMIT));
        assert_eq!(page("kaban://documents?offset=20&limit=5"), (20, 5));
        assert_eq!(page("kaban://documents?offset=x"), (0, DEFAULT_LIMIT));
    }
}
