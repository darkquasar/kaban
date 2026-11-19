//! Prose, fuzzy surface forms, whole names, and split code identifiers.

use kaban_core::ident::split_terms;
use tantivy::Index;
use tantivy::tokenizer::{
    AsciiFoldingFilter, Language, LowerCaser, RawTokenizer, RemoveLongFilter, SimpleTokenizer,
    Stemmer, TextAnalyzer, Token, TokenStream, Tokenizer,
};

pub(crate) const STEMMED: &str = "kaban_stemmed";
pub(crate) const FOLDED: &str = "kaban_folded";
pub(crate) const CODE_IDENT: &str = "kaban_code_ident";
pub(crate) const CODE_NAME: &str = "kaban_code_name";

/// Tokens longer than this are dropped (FTS5 has no limit; Tantivy's term dictionary does).
const MAX_TOKEN_BYTES: usize = 255;

/// Register the analyzers on `index`; runs for every handle this crate opens.
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
        CODE_NAME,
        TextAnalyzer::builder(RawTokenizer::default())
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .build(),
    );
    tokenizers.register(
        CODE_IDENT,
        TextAnalyzer::builder(IdentTokenizer)
            .filter(RemoveLongFilter::limit(MAX_TOKEN_BYTES))
            .filter(LowerCaser)
            .filter(AsciiFoldingFilter)
            .build(),
    );
}

#[derive(Clone, Default)]
struct IdentTokenizer;

impl Tokenizer for IdentTokenizer {
    type TokenStream<'a> = IdentStream<'a>;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> IdentStream<'a> {
        IdentStream {
            parts: split_terms(text).into_iter(),
            token: Token::default(),
        }
    }
}

struct IdentStream<'a> {
    parts: std::vec::IntoIter<&'a str>,
    token: Token,
}

impl TokenStream for IdentStream<'_> {
    fn advance(&mut self) -> bool {
        let Some(part) = self.parts.next() else {
            return false;
        };
        self.token.position = self.token.position.wrapping_add(1);
        self.token.text.clear();
        self.token.text.push_str(part);
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
    use super::{CODE_IDENT, CODE_NAME, STEMMED, register};
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

    #[test]
    fn identifiers_and_prose_use_their_declared_analyzers() {
        let index = Index::create_in_ram(Schema::builder().build());
        register(&index);
        for (input, expected) in [
            (
                "TransactionCoordinator retries",
                vec!["transaction", "coordinator", "retries"],
            ),
            ("retry_path", vec!["retry", "path"]),
            ("config.yaml", vec!["config", "yaml"]),
            ("src/store/open.rs", vec!["src", "store", "open", "rs"]),
            ("plain prose only", vec!["plain", "prose", "only"]),
        ] {
            assert_eq!(terms(&index, CODE_IDENT, input), expected);
        }
        assert_eq!(terms(&index, CODE_NAME, "HTTPServer"), ["httpserver"]);
        assert_eq!(
            terms(&index, STEMMED, "TransactionCoordinator"),
            ["transactioncoordin"]
        );
        assert_eq!(terms(&index, STEMMED, "Coordinator"), ["coordin"]);
    }
}
