extern crate proc_macro;
use proc_macro::{TokenStream, TokenTree};
// The serde shape: one anonymous const per derive, a private helper beside the impl.
#[proc_macro_derive(Marked)]
pub fn derive_marked(input: TokenStream) -> TokenStream {
    let mut tokens = input.into_iter().skip_while(|token| !matches!(token, TokenTree::Ident(ident) if ident.to_string() == "struct"));
    tokens.next();
    let name = tokens.next().map(|token| token.to_string()).expect("fixed invented fixture: a struct name");
    format!("const _: () = {{ fn helper() {{}} impl crate::Marker for {name} {{ fn m(&self) {{ helper() }} }} }};")
        .parse()
        .expect("fixed invented fixture")
}
// A second derive onto a distinct trait, so two derives of one item each expand to an anonymous const (fixg-2).
#[proc_macro_derive(Marked2)]
pub fn derive_marked2(input: TokenStream) -> TokenStream {
    let mut tokens = input.into_iter().skip_while(|token| !matches!(token, TokenTree::Ident(ident) if ident.to_string() == "struct"));
    tokens.next();
    let name = tokens.next().map(|token| token.to_string()).expect("fixed invented fixture: a struct name");
    format!("const _: () = {{ impl crate::MarkerB for {name} {{ fn b(&self) {{}} }} }};")
        .parse()
        .expect("fixed invented fixture")
}
// A pass-through attribute: the item it decorates lives in the expansion file (fixg-3).
#[proc_macro_attribute]
pub fn passthrough(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}
