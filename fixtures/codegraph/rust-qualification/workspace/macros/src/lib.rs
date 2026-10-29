#![forbid(unsafe_code)]

extern crate proc_macro;

#[proc_macro]
pub fn make_value(_input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let scratch = std::env::var_os("TMPDIR").expect("qualification requires TMPDIR");
    std::fs::write(
        std::path::PathBuf::from(scratch).join("proc-macro-ran.txt"),
        "ran",
    )
    .expect("qualification marker must be writable");
    "pub fn generated_value() -> usize { 7 }"
        .parse()
        .expect("fixed valid Rust")
}
