extern crate proc_macro;
#[proc_macro_attribute]
pub fn with_inner(_: proc_macro::TokenStream, _: proc_macro::TokenStream) -> proc_macro::TokenStream {
    "pub fn outer() { fn unused_generated() {} }".parse().expect("fixed invented fixture")
}
#[proc_macro_derive(WithInner)]
pub fn derive_inner(_: proc_macro::TokenStream) -> proc_macro::TokenStream {
    "impl Nominal { pub fn outer() { fn unused_generated() {} } }".parse().expect("fixed invented fixture")
}
