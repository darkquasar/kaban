//! The one-of selector rule (stage1-spec G1-F12, RS-12; G1-S4: the grammar checks are typed
//! steps after the scan, discriminated by identity kind) resolved against the reader
//! (cg-join-v1 §6, §9).
//!
//! ```text
//!   symbol        ==> symbol::parse (else symbol_invalid) ==> symbol_unique(key) ==> One
//!   ref           ==> parse_ref (else ref_invalid)         ==> node(ref)           ==> One
//!   {label, name} ==> label in SCIP_KIND_NAMES (else selector_invalid) ==> by_name(name)
//!     [, path]        ==> the rows of that label (in `path` when given): one ==> One,
//!                         several ==> ambiguous_symbol WITH the candidates (Q-2, backlog 16, 27:
//!                         never a merged list); none ==> the external placeholders of that name
//!                         whose descriptor fits the label (kind-agnostic, R-3 request 2): one
//!                         ==> One marked `external`, several ==> ambiguous_symbol; none ==> the
//!                         non-external rows of OTHER labels, `UnspecifiedKind` included (q2-2):
//!                         one ==> One with `label_corrected` (backlog 21), several ==>
//!                         unknown_symbol WITH `did_you_mean` (up to 5)
//!   {path, line}  ==> repo-relative path rule (else selector_invalid) ==> by_path_line  ==> Many
//!   path          ==> repo-relative path rule ==> node("file:<path>")  ==> One
//!   anything else ==> selector_invalid;  an empty result ==> unknown_symbol
//! ```

use kaban_codegraph::{
    CodeGraphReader, Descriptor, NodeRow, SCIP_KIND_NAMES, check_repo_relative_path,
};

use super::{CodeError, Hint, SymbolCandidate, SymbolSelector, TargetNote};

/// How many matched rows an `ambiguous_symbol` answer lists (`candidates_total` says how many
/// matched in all).
pub(crate) const CANDIDATES_SHOWN: usize = 20;

/// How many same-named rows of other labels an `unknown_symbol` answer suggests.
pub(crate) const DID_YOU_MEAN_SHOWN: usize = 5;

/// What `resolve` found: the selection and, for a name form answered other than by an exact
/// `(label, name)` match, the note the response carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Found {
    pub(crate) selection: Selection,
    pub(crate) note: Option<TargetNote>,
}

impl Found {
    /// A selection without a note.
    fn plain(selection: Selection) -> Self {
        Self {
            selection,
            note: None,
        }
    }
}

/// What a selector matched: one row, or every row of a `(path, line)` lookup (G1-F12: a
/// positional selector matching several nodes returns all; a list tool takes the union of
/// their results, `code_definition` needs exactly one). A name selector never yields `Many`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Selection {
    /// One node (boxed: a row is far larger than the `Vec` header of the other variant).
    One(Box<NodeRow>),
    /// Several nodes (never empty).
    Many(Vec<NodeRow>),
}

impl Selection {
    /// One row.
    fn one(row: NodeRow) -> Self {
        Self::One(Box::new(row))
    }

    /// Every matched row.
    pub(crate) fn into_rows(self) -> Vec<NodeRow> {
        match self {
            Self::One(row) => vec![*row],
            Self::Many(rows) => rows,
        }
    }

    /// Exactly one row, else `ambiguous_symbol` with the candidates (`code_definition`).
    pub(crate) fn into_one(self) -> Result<NodeRow, CodeError> {
        match self {
            Self::One(row) => Ok(*row),
            Self::Many(mut rows) if rows.len() == 1 => rows.pop().ok_or_else(unknown),
            Self::Many(rows) => Err(ambiguous(&rows)),
        }
    }
}

/// The selector form exactly one of which must be set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form<'a> {
    Symbol(&'a str),
    Ref(&'a str),
    /// `{label, name}` with the optional `path` that keeps only the rows defined there.
    LabelName(&'a str, &'a str, Option<&'a str>),
    PathLine(&'a str, u32),
    Path(&'a str),
}

/// The one form `sel` sets, or `selector_invalid`.
fn form(sel: &SymbolSelector) -> Result<Form<'_>, CodeError> {
    match (
        sel.symbol.as_deref(),
        sel.r#ref.as_deref(),
        sel.label.as_deref(),
        sel.name.as_deref(),
        sel.path.as_deref(),
        sel.line,
    ) {
        (Some(s), None, None, None, None, None) => Ok(Form::Symbol(s)),
        (None, Some(r), None, None, None, None) => Ok(Form::Ref(r)),
        (None, None, Some(l), Some(n), path, None) => Ok(Form::LabelName(l, n, path)),
        (None, None, None, None, Some(p), Some(line)) => Ok(Form::PathLine(p, line)),
        (None, None, None, None, Some(p), None) => Ok(Form::Path(p)),
        _ => Err(CodeError::SelectorInvalid),
    }
}

/// Whether `sel` is the bare `path` form (the one form a tool may refuse: `code_implementers`
/// has no file target).
pub(crate) fn is_bare_path(sel: &SymbolSelector) -> bool {
    matches!(form(sel), Ok(Form::Path(_)))
}

/// Resolve `sel` against `reader` (the module doc's table).
///
/// # Errors
/// [`CodeError`]: `SelectorInvalid`, `SymbolInvalid`, `RefInvalid`, `UnknownSymbol`,
/// `AmbiguousSymbol`, or a read failure by class.
pub(crate) fn resolve(
    sel: &SymbolSelector,
    reader: &dyn CodeGraphReader,
) -> Result<Found, CodeError> {
    match form(sel)? {
        Form::Symbol(key) => {
            kaban_codegraph::parse(key).map_err(|_| CodeError::SymbolInvalid)?;
            // the string as given: a versioned raw symbol is not a key and matches nothing
            reader
                .symbol_unique(key)?
                .map(|row| Found::plain(Selection::one(row)))
                .ok_or_else(unknown)
        }
        Form::Ref(r#ref) => {
            kaban_codegraph::parse_ref(r#ref).map_err(|_| CodeError::RefInvalid)?;
            reader
                .node(r#ref)?
                .map(|row| Found::plain(Selection::one(row)))
                .ok_or_else(unknown)
        }
        Form::LabelName(label, name, path) => {
            if !SCIP_KIND_NAMES.contains(&label) {
                return Err(CodeError::SelectorInvalid);
            }
            if let Some(path) = path {
                check_repo_relative_path(path).map_err(|_| CodeError::SelectorInvalid)?;
            }
            by_label(reader.by_name(name)?, label, path)
        }
        Form::PathLine(path, line) => {
            check_repo_relative_path(path).map_err(|_| CodeError::SelectorInvalid)?;
            many(reader.by_path_line(path, line)?).map(Found::plain)
        }
        Form::Path(path) => {
            check_repo_relative_path(path).map_err(|_| CodeError::SelectorInvalid)?;
            reader
                .node(&format!("file:{path}"))?
                .map(|row| Found::plain(Selection::one(row)))
                .ok_or_else(unknown)
        }
    }
}

/// The name rule over every row named as asked (`named`, the reader's `(path, ref)` order,
/// narrowed to `path` when given). The rows carrying `label` are the exact matches: one
/// answers, several are `ambiguous_symbol` with the candidates. Without one, the external
/// placeholders whose descriptor fits the label answer by name, then every non-external row
/// of another label (whatever its label) is a wrong-label match: one answers with
/// `label_corrected`, several are `unknown_symbol` with `did_you_mean`, none is the bare
/// `unknown_symbol`. Only exact and placeholder matching ever drop a row: correction counts
/// every local symbol of the name, so it can never make one unique by filtering (q2-2).
fn by_label(named: Vec<NodeRow>, label: &str, path: Option<&str>) -> Result<Found, CodeError> {
    let (mut exact, rest): (Vec<NodeRow>, Vec<NodeRow>) = named
        .into_iter()
        .filter(|row| path.is_none_or(|p| row.path.as_deref() == Some(p)))
        .partition(|row| row.label == label);
    match exact.len() {
        0 => {}
        1 => {
            return exact
                .pop()
                .map(|row| Found::plain(Selection::one(row)))
                .ok_or_else(unknown);
        }
        _ => return Err(ambiguous(&exact)),
    }
    let (placeholders, rest): (Vec<NodeRow>, Vec<NodeRow>) = rest
        .into_iter()
        .partition(|row| row.external && row.label == UNSPECIFIED);
    // a placeholder stands in for the asked label only where its descriptor fits; a local row
    // of unknown kind is a local symbol like any other and counts below (q2-2)
    let mut placeholders: Vec<NodeRow> = placeholders
        .into_iter()
        .filter(|row| descriptor_admits(label, row.scip.as_deref()))
        .collect();
    match placeholders.len() {
        0 => {}
        1 => {
            return placeholders
                .pop()
                .map(|row| Found {
                    note: Some(TargetNote {
                        r#ref: row.r#ref.clone(),
                        label_corrected: None,
                        external: true,
                    }),
                    selection: Selection::one(row),
                })
                .ok_or_else(unknown);
        }
        _ => return Err(ambiguous(&placeholders)),
    }
    let mut other: Vec<NodeRow> = rest.into_iter().filter(|row| !row.external).collect();
    match other.len() {
        0 => Err(unknown()),
        1 => other
            .pop()
            .map(|row| Found {
                note: Some(TargetNote {
                    r#ref: row.r#ref.clone(),
                    label_corrected: Some(row.label.clone()),
                    external: false,
                }),
                selection: Selection::one(row),
            })
            .ok_or_else(unknown),
        _ => Err(CodeError::UnknownSymbol {
            did_you_mean: Hint::Rows(
                other
                    .iter()
                    .take(DID_YOU_MEAN_SHOWN)
                    .map(SymbolCandidate::from)
                    .collect(),
            ),
        }),
    }
}

/// The label of a row whose producer named no kind: every external placeholder, and every
/// row of a producer that labels nothing (scip-typescript, SCR-2).
const UNSPECIFIED: &str = "UnspecifiedKind";

/// Whether a row of unknown kind can be what `label` asks for, read off the suffix of the last
/// descriptor of its raw symbol: a type (`#`) for the type-shaped labels, a method or term for
/// the callable ones, a term for values, a namespace for modules, a macro or meta descriptor
/// for `Macro`; a label outside these groups, or a symbol that does not parse, admits any row.
/// This keeps the `[Serialize]` type parameter of an impl and the `Serialize:` derive macro
/// from standing in for the trait when `{Trait, Serialize}` is asked (R-3 consumer request 2).
fn descriptor_admits(label: &str, scip: Option<&str>) -> bool {
    const TYPES: &[&str] = &[
        "Class",
        "Enum",
        "Interface",
        "Struct",
        "Trait",
        "TypeAlias",
        "Type",
        "Union",
        "Object",
        "Protocol",
        "Delegate",
        "AssociatedType",
    ];
    const CALLABLES: &[&str] = &[
        "Function",
        "Method",
        "Constructor",
        "AbstractMethod",
        "MethodSpecification",
        "MethodAlias",
        "StaticMethod",
        "Accessor",
        "Getter",
        "Setter",
    ];
    const VALUES: &[&str] = &[
        "Field",
        "Variable",
        "Constant",
        "Property",
        "EnumMember",
        "Key",
        "Value",
        "Event",
    ];
    const NAMESPACES: &[&str] = &["Module", "Namespace", "Package", "Library"];
    let Some(last) = scip
        .and_then(|raw| kaban_codegraph::parse(raw).ok())
        .and_then(|sym| sym.descriptors.last().cloned())
    else {
        return true;
    };
    if TYPES.contains(&label) {
        matches!(last, Descriptor::Type(_))
    } else if CALLABLES.contains(&label) {
        matches!(last, Descriptor::Method { .. } | Descriptor::Term(_))
    } else if VALUES.contains(&label) {
        matches!(last, Descriptor::Term(_))
    } else if NAMESPACES.contains(&label) {
        matches!(last, Descriptor::Namespace(_))
    } else {
        match label {
            "Macro" => matches!(last, Descriptor::Macro(_) | Descriptor::Meta(_)),
            "TypeParameter" => matches!(last, Descriptor::TypeParameter(_)),
            "Parameter" => matches!(last, Descriptor::Parameter(_)),
            _ => true,
        }
    }
}

/// The bare `unknown_symbol`: nothing of that identity, no hint.
fn unknown() -> CodeError {
    CodeError::UnknownSymbol {
        did_you_mean: Hint::Rows(Vec::new()),
    }
}

/// `ambiguous_symbol` naming the first [`CANDIDATES_SHOWN`] of `rows` (the reader's `(path,
/// ref)` order) and their total.
fn ambiguous(rows: &[NodeRow]) -> CodeError {
    CodeError::AmbiguousSymbol {
        candidates: Hint::Rows(
            rows.iter()
                .take(CANDIDATES_SHOWN)
                .map(SymbolCandidate::from)
                .collect(),
        ),
        total: rows.len(),
    }
}

/// A non-empty lookup result as `Many`, an empty one as `unknown_symbol`.
fn many(rows: Vec<NodeRow>) -> Result<Selection, CodeError> {
    if rows.is_empty() {
        Err(unknown())
    } else {
        Ok(Selection::Many(rows))
    }
}
