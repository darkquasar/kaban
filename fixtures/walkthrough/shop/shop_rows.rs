//! Synthetic typed snapshot rows, never decoded from `index.scip` (OD-P-4).
use kaban_codegraph::write::{FileRow, SymbolRow};
use kaban_codegraph::{FileId, NodeId, NodeKind, Range};

pub(crate) const DISCOUNT: &str =
    "rust-analyzer cargo shop . checkout/cart/CartService#apply_discount().";
pub(crate) const PROMOTION: &str =
    "rust-analyzer cargo shop . checkout/cart/CartService#apply_promotion().";
pub(crate) const PRICING: &str = "rust-analyzer cargo shop . checkout/pricing/price_lines().";

pub(crate) fn rows(renamed: bool) -> (Vec<FileRow>, Vec<SymbolRow>) {
    let files = ["src/checkout/cart.rs", "src/checkout/pricing.rs"]
        .iter()
        .enumerate()
        .map(|(i, path)| FileRow {
            id: FileId(i64::try_from(i + 1).unwrap()),
            path: (*path).into(),
            language: "rust".into(),
            is_test: false,
            lines: 16,
        })
        .collect();
    let (key, name) = if renamed {
        (PROMOTION, "CartService::apply_promotion")
    } else {
        (DISCOUNT, "CartService::apply_discount")
    };
    let mut symbols = vec![
        symbol(key, name, "apply_discount", 1),
        symbol(PRICING, "price_lines", "price_lines", 2),
    ];
    for name in [
        "Cart",
        "CartLine",
        "CartItem",
        "CartTotal",
        "CartTax",
        "CartCoupon",
    ] {
        symbols.push(symbol(
            &format!("rust-analyzer cargo shop . checkout/cart/{name}#"),
            name,
            "Cart",
            1,
        ));
    }
    symbols.sort_by(|a, b| a.r#ref.as_bytes().cmp(b.r#ref.as_bytes()));
    for (i, row) in symbols.iter_mut().enumerate() {
        row.id = NodeId(i64::try_from(i + 1).unwrap());
    }
    (files, symbols)
}

fn symbol(key: &str, name: &str, label: &str, file: i64) -> SymbolRow {
    SymbolRow {
        id: NodeId(0),
        kind: NodeKind::Symbol,
        r#ref: format!("scip:{key}"),
        scip: Some(key.into()),
        key: Some(key.into()),
        label: label.into(),
        name: name.into(),
        file_id: Some(FileId(file)),
        def: Some(Range {
            sl: 3,
            sc: 4,
            el: 5,
            ec: 5,
        }),
        enc: None,
        signature: None,
        doc: None,
        external: false,
        is_test: false,
        ident_terms: name.replace("::", " ").replace('_', " "),
    }
}
