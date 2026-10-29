//! The SCIP symbol grammar as a typed check (G1-S4, stage1-plan CG2.1).
//!
//! `scheme manager package version descriptor+` with space-escaped package segments (a doubled
//! space is a literal space) and backtick-escaped descriptor identifiers (a doubled backtick is a
//! literal backtick), or the document-local form `local <id>`. The grammar is the one of
//! `scip-0.10.0/src/symbol.rs` (read as the reference; nothing copied) with two additions the
//! spec asks for: a size and control-character bound, and canonicity, so that
//! `format(parse(s)) == s` holds for every accepted `s` (an identifier is backtick-escaped exactly
//! when it contains a character outside `[A-Za-z0-9_+$-]`). Internal joins never use a parsed
//! form: they compare the `key`/`ref` strings byte for byte (G1-S4).

use crate::error::SymbolError;

/// The grammar's size bound in bytes (G1-S4).
pub const MAX_SYMBOL_BYTES: usize = 1024;

/// The scheme of a document-local symbol and the prefix of its string form.
const LOCAL_PREFIX: &str = "local ";

/// A parsed SCIP symbol. The fields are public so that `kaban-index` can derive rows from them;
/// a value built by hand is formatted as given and is not re-validated.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Symbol {
    /// The indexer scheme (`scip-go`, `scip-typescript`) or `local`.
    pub scheme: String,
    /// The package manager (`gomod`, `npm`); empty for a local symbol.
    pub manager: String,
    /// The package name; empty for a local symbol.
    pub package: String,
    /// The package version (`.` in a normalised key); empty for a local symbol.
    pub version: String,
    /// One or more descriptors, outermost first.
    pub descriptors: Vec<Descriptor>,
}

/// One descriptor of a SCIP symbol, by its suffix in the grammar.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Descriptor {
    /// `name/`
    Namespace(String),
    /// `name#`
    Type(String),
    /// `name.`
    Term(String),
    /// `name(disambiguator).` (the disambiguator may be empty: `name().`)
    Method {
        /// The method name.
        name: String,
        /// The overload disambiguator, empty when absent.
        disambiguator: String,
    },
    /// `[name]`
    TypeParameter(String),
    /// `(name)`
    Parameter(String),
    /// `name:`
    Meta(String),
    /// The `<id>` of `local <id>`.
    Local(String),
    /// `name!`
    Macro(String),
}

impl Descriptor {
    /// The identifier the descriptor names (the method name for a method).
    #[must_use]
    pub fn identifier(&self) -> &str {
        match self {
            Self::Namespace(n)
            | Self::Type(n)
            | Self::Term(n)
            | Self::TypeParameter(n)
            | Self::Parameter(n)
            | Self::Meta(n)
            | Self::Local(n)
            | Self::Macro(n) => n,
            Self::Method { name, .. } => name,
        }
    }
}

/// Parse `s` under the grammar; the error is a class, never the input.
///
/// # Errors
/// [`SymbolError`]: oversize, control character, malformed, not canonical.
pub fn parse(s: &str) -> Result<Symbol, SymbolError> {
    #[cfg(any(test, feature = "instrumented"))]
    testing::record(s);
    if s.len() > MAX_SYMBOL_BYTES {
        return Err(SymbolError::Oversize);
    }
    if s.chars().any(char::is_control) {
        return Err(SymbolError::ControlCharacter);
    }
    if let Some(id) = s.strip_prefix(LOCAL_PREFIX) {
        return parse_local(id);
    }
    let mut p = Parser {
        chars: s.chars().collect(),
        at: 0,
    };
    let scheme = p.space_segment()?;
    let manager = p.space_segment()?;
    let package = p.space_segment()?;
    let version = p.space_segment()?;
    let mut descriptors = Vec::new();
    while !p.done() {
        descriptors.push(p.descriptor()?);
    }
    if descriptors.is_empty() {
        return Err(SymbolError::Malformed);
    }
    Ok(Symbol {
        scheme,
        manager,
        package,
        version,
        descriptors,
    })
}

/// `local <id>`: a non-empty simple identifier after the prefix.
fn parse_local(id: &str) -> Result<Symbol, SymbolError> {
    if id.is_empty() || !id.chars().all(is_simple_char) {
        return Err(SymbolError::Malformed);
    }
    Ok(Symbol {
        scheme: "local".to_owned(),
        manager: String::new(),
        package: String::new(),
        version: String::new(),
        descriptors: vec![Descriptor::Local(id.to_owned())],
    })
}

/// Format `sym` back into its string form; `format(parse(s)) == s` for every accepted `s`.
#[must_use]
pub fn format(sym: &Symbol) -> String {
    if sym.scheme == "local"
        && let [Descriptor::Local(id)] = sym.descriptors.as_slice()
    {
        return std::format!("{LOCAL_PREFIX}{id}");
    }
    let mut out = String::new();
    for segment in [&sym.scheme, &sym.manager, &sym.package, &sym.version] {
        out.push_str(&segment.replace(' ', "  "));
        out.push(' ');
    }
    for d in &sym.descriptors {
        push_descriptor(&mut out, d);
    }
    out
}

fn push_descriptor(out: &mut String, d: &Descriptor) {
    match d {
        Descriptor::Namespace(n) => {
            push_identifier(out, n);
            out.push('/');
        }
        Descriptor::Type(n) => {
            push_identifier(out, n);
            out.push('#');
        }
        Descriptor::Term(n) => {
            push_identifier(out, n);
            out.push('.');
        }
        Descriptor::Meta(n) => {
            push_identifier(out, n);
            out.push(':');
        }
        Descriptor::Macro(n) => {
            push_identifier(out, n);
            out.push('!');
        }
        Descriptor::Local(n) => push_identifier(out, n),
        Descriptor::Method {
            name,
            disambiguator,
        } => {
            push_identifier(out, name);
            out.push('(');
            if !disambiguator.is_empty() {
                push_identifier(out, disambiguator);
            }
            out.push_str(").");
        }
        Descriptor::TypeParameter(n) => {
            out.push('[');
            push_identifier(out, n);
            out.push(']');
        }
        Descriptor::Parameter(n) => {
            out.push('(');
            push_identifier(out, n);
            out.push(')');
        }
    }
}

/// An identifier is written bare when every character is simple, else in backticks with each
/// backtick doubled (the escaping rule of the SCIP reference formatter).
fn push_identifier(out: &mut String, name: &str) {
    if name.chars().all(is_simple_char) {
        out.push_str(name);
    } else {
        out.push('`');
        out.push_str(&name.replace('`', "``"));
        out.push('`');
    }
}

/// The characters an identifier may contain without backticks.
fn is_simple_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '$')
}

/// The normalised key of stage1-spec §4.2: `s` with its package version segment replaced by `.`
/// (a local symbol has no version and is returned as formatted).
///
/// # Errors
/// The grammar errors of [`parse`].
pub fn normalise_version(s: &str) -> Result<String, SymbolError> {
    let mut sym = parse(s)?;
    if sym.scheme != "local" {
        ".".clone_into(&mut sym.version);
    }
    Ok(format(&sym))
}

/// The last descriptor's identifier (G1-F1 `name`); empty for a symbol without descriptors.
#[must_use]
pub fn name(sym: &Symbol) -> &str {
    sym.descriptors.last().map_or("", Descriptor::identifier)
}

/// The symbol minus its last descriptor (the `CONTAINS` parent of stage1-spec §4.3), or `None`
/// when there is no enclosing descriptor.
#[must_use]
pub fn parent(sym: &Symbol) -> Option<Symbol> {
    if sym.descriptors.len() < 2 {
        return None;
    }
    let mut p = sym.clone();
    p.descriptors.pop();
    Some(p)
}

/// A cursor over the symbol's characters.
struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn done(&self) -> bool {
        self.at >= self.chars.len()
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<char> {
        self.chars.get(self.at + offset).copied()
    }

    fn expect(&mut self, c: char) -> Result<(), SymbolError> {
        if self.peek() == Some(c) {
            self.at += 1;
            Ok(())
        } else {
            Err(SymbolError::Malformed)
        }
    }

    /// A space-terminated segment where a doubled space is a literal space; non-empty, and the
    /// terminating space must exist (descriptors follow the version).
    fn space_segment(&mut self) -> Result<String, SymbolError> {
        let mut out = String::new();
        loop {
            match self.peek() {
                None => return Err(SymbolError::Malformed),
                Some(' ') if self.peek_at(1) == Some(' ') => {
                    out.push(' ');
                    self.at += 2;
                }
                Some(' ') => {
                    self.at += 1;
                    break;
                }
                Some(c) => {
                    out.push(c);
                    self.at += 1;
                }
            }
        }
        if out.is_empty() {
            return Err(SymbolError::Malformed);
        }
        Ok(out)
    }

    /// A descriptor identifier: backtick-escaped (canonical only when it needs escaping) or bare
    /// (simple characters only); never empty.
    fn identifier(&mut self) -> Result<String, SymbolError> {
        if self.peek() == Some('`') {
            self.at += 1;
            let mut out = String::new();
            loop {
                match self.peek() {
                    None => return Err(SymbolError::Malformed),
                    Some('`') if self.peek_at(1) == Some('`') => {
                        out.push('`');
                        self.at += 2;
                    }
                    Some('`') => {
                        self.at += 1;
                        break;
                    }
                    Some(c) => {
                        out.push(c);
                        self.at += 1;
                    }
                }
            }
            if out.is_empty() {
                return Err(SymbolError::Malformed);
            }
            if out.chars().all(is_simple_char) {
                return Err(SymbolError::NonCanonical);
            }
            return Ok(out);
        }
        let start = self.at;
        while self.peek().is_some_and(is_simple_char) {
            self.at += 1;
        }
        if start == self.at {
            // a character that is neither simple nor a backtick: unescaped where escaping is due
            return Err(match self.peek() {
                Some(c) if !is_suffix_char(c) => SymbolError::NonCanonical,
                _ => SymbolError::Malformed,
            });
        }
        Ok(self.chars[start..self.at].iter().collect())
    }

    fn descriptor(&mut self) -> Result<Descriptor, SymbolError> {
        match self.peek() {
            Some('(') => {
                self.at += 1;
                let name = self.identifier()?;
                self.expect(')')?;
                Ok(Descriptor::Parameter(name))
            }
            Some('[') => {
                self.at += 1;
                let name = self.identifier()?;
                self.expect(']')?;
                Ok(Descriptor::TypeParameter(name))
            }
            _ => {
                let name = self.identifier()?;
                let suffix = self.peek().ok_or(SymbolError::Malformed)?;
                self.at += 1;
                match suffix {
                    '/' => Ok(Descriptor::Namespace(name)),
                    '.' => Ok(Descriptor::Term(name)),
                    '#' => Ok(Descriptor::Type(name)),
                    ':' => Ok(Descriptor::Meta(name)),
                    '!' => Ok(Descriptor::Macro(name)),
                    '(' => {
                        let disambiguator = if self.peek() == Some(')') {
                            String::new()
                        } else {
                            self.identifier()?
                        };
                        self.expect(')')?;
                        self.expect('.')?;
                        Ok(Descriptor::Method {
                            name,
                            disambiguator,
                        })
                    }
                    _ => Err(SymbolError::Malformed),
                }
            }
        }
    }
}

/// The characters that may legitimately follow an identifier.
fn is_suffix_char(c: char) -> bool {
    matches!(c, '/' | '.' | '#' | ':' | '!' | '(' | ')' | ']')
}

/// Test-only observation of the parser's inputs (G1-S1 instrumented parser): recording is per
/// thread and lasts for the lifetime of the returned [`testing::Recorder`].
#[cfg(any(test, feature = "instrumented"))]
pub mod testing {
    use std::cell::RefCell;

    thread_local! {
        static RECORDED: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
    }

    /// Records every string `parse` sees on this thread until dropped.
    #[derive(Debug)]
    pub struct Recorder(());

    /// Start recording the parser's inputs on the current thread.
    #[must_use]
    pub fn record_inputs() -> Recorder {
        RECORDED.with(|r| *r.borrow_mut() = Some(Vec::new()));
        Recorder(())
    }

    impl Recorder {
        /// The inputs recorded so far, in call order.
        #[must_use]
        pub fn inputs(&self) -> Vec<String> {
            RECORDED.with(|r| r.borrow().clone().unwrap_or_default())
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            RECORDED.with(|r| *r.borrow_mut() = None);
        }
    }

    pub(super) fn record(s: &str) {
        RECORDED.with(|r| {
            if let Some(v) = r.borrow_mut().as_mut() {
                v.push(s.to_owned());
            }
        });
    }
}
