#![forbid(unsafe_code)]

fn main() -> std::io::Result<()> {
    let out =
        std::env::var_os("OUT_DIR").ok_or_else(|| std::io::Error::other("OUT_DIR missing"))?;
    std::fs::write(
        std::path::PathBuf::from(out).join("build-script-ran.txt"),
        "ran",
    )
}
