//! The analyzers (spec 05 SS-08), registered on every `Index` handle. [`STEMMED`] is the FTS5
//! `porter unicode61` twin for the content fields and for every query operand; [`FOLDED`] is
//! the same without the stemmer, for the fuzzy field (a misspelling is matched against the
//! surface form); [`IDENT`] is the identifier-aware analyzer of the `ident_terms` field: it
//! expands its input with [`kaban_core::ident::ident_terms`] (whole identifiers, their
//! separator and camel parts, lowercase copies) and then analyzes the result like
//! [`STEMMED`], so the field's term set equals the FTS5 `ident_terms` column's when fed the
//! same five inputs, and a query analyzed with [`STEMMED`] matches it exactly as FTS5 does.

use kaban_core::ident::ident_terms;
use tantivy::Index;
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RemoveLongFilter, SimpleTokenizer, Stemmer,
    TextAnalyzer, Token, TokenStream, Tokenizer,
};

/// Bumped when an analyzer changes (the identifier rule is versioned separately through
/// `IDENT_TERMS_VERSION`, which the payload carries as well).
pub(crate) const ANALYZER_VERSION: u32 = 1;

pub(crate) const STEMMED: &str = "kaban_stemmed";
pub(crate) const FOLDED: &str = "kaban_folded";
pub(crate) const IDENT: &str = "kaban_ident";

/// Tokens longer than this are dropped (FTS5 has no limit; Tantivy's term dictionary does).
const MAX_TOKEN_BYTES: usize = 255;

/// Register the three analyzers on `index`; runs for every handle this crate opens.
pub(crate) fn register(index: &Index) {
    let tokenizers = index.tokenizers();
    tokenizers.register(
        STEMMED,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .filter(Stemmer::new(Language::English))
            .build(),
    );
    tokenizers.register(
        FOLDED,
        TextAnalyzer::builder(SimpleTokenizer::default())
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build(),
    );
    tokenizers.register(
        IDENT,
        TextAnalyzer::builder(IdentTokenizer)
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .filter(Stemmer::new(Language::English))
            .build(),
    );
}

/// The identifier expansion as a tokenizer: the derived terms of the input, each split on
/// non-alphanumerics as the simple tokenizer would (`foo_bar` is `foo bar` to FTS5 as well).
/// Derived terms have no source span, so the offsets are zero; positions are consecutive.
#[derive(Clone, Default)]
struct IdentTokenizer;

impl Tokenizer for IdentTokenizer {
    type TokenStream<'a> = IdentStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> IdentStream {
        let tokens = ident_terms(text, None, &[], &[], &[])
            .split(' ')
            .flat_map(|term| {
                term.split(|c: char| !c.is_alphanumeric())
                    .filter(|part| !part.is_empty())
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        IdentStream {
            tokens,
            next: 0,
            token: Token::default(),
        }
    }
}

struct IdentStream {
    tokens: Vec<String>,
    next: usize,
    token: Token,
}

impl TokenStream for IdentStream {
    fn advance(&mut self) -> bool {
        let Some(text) = self.tokens.get(self.next) else {
            return false;
        };
        self.token.position = self.token.position.wrapping_add(1);
        self.token.text.clear();
        self.token.text.push_str(text);
        self.next += 1;
        true
    }

    fn token(&self) -> &Token {
        &self.token
    }

    fn token_mut(&mut self) -> &mut Token {
        &mut self.token
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test code: failures are the assertion
mod tests {
    use super::{IDENT, STEMMED, register};
    use tantivy::Index;
    use tantivy::schema::Schema;

    fn terms(index: &Index, analyzer: &str, text: &str) -> Vec<String> {
        let mut analyzer = index.tokenizers().get(analyzer).unwrap();
        let mut stream = analyzer.token_stream(text);
        let mut out = Vec::new();
        while let Some(token) = stream.next() {
            out.push(token.text.clone());
        }
        out
    }

    /// The analyzer goldens: camel, snake, dotted and anchor-shaped identifiers expand into the
    /// whole identifier and its parts, stemmed and lowercased like the FTS5 column; prose stays
    /// prose; the query side (`STEMMED`) produces the term a camel identifier is indexed under.
    #[test]
    fn identifier_expansion_matches_the_fts5_column_and_the_query_side() {
        let index = Index::create_in_ram(Schema::builder().build());
        register(&index);
        let ident = |text| terms(&index, IDENT, text);
        assert_eq!(
            ident("TransactionCoordinator retries"),
            [
                "transactioncoordin",
                "transact",
                "coordin",
                "transactioncoordin",
                "transact",
                "coordin"
            ]
        );
        // an all-lowercase identifier has no lowercase copy to add: the whole, then its parts
        assert_eq!(ident("retry_path"), ["retri", "path", "retri", "path"]);
        assert_eq!(ident("config.yaml"), ["config", "yaml", "config", "yaml"]);
        assert_eq!(
            ident("src/store/open.rs"),
            ["src", "store", "open", "rs", "src", "store", "open", "rs"]
        );
        assert_eq!(ident("plain prose only"), Vec::<String>::new());
        // the query side: one term per camel identifier, as FTS5 tokenizes a query
        assert_eq!(
            terms(&index, STEMMED, "TransactionCoordinator"),
            ["transactioncoordin"]
        );
        assert_eq!(terms(&index, STEMMED, "Coordinator"), ["coordin"]);
    }
}
