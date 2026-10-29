#![forbid(unsafe_code)]

//! The `workspace` fixture's build script plus one C compile through the admitted `CC` with
//! `-flto`, so that the C compiler proper, the assembler, `collect2`, `lto-wrapper` and the
//! linker all run inside the semantic child; `c-compiled.txt` records that the compile really
//! happened (the `shadow-backend` qualification profile, hd1-2).

use std::{path::PathBuf, process::Command};

fn main() -> std::io::Result<()> {
    let out = std::env::var_os("OUT_DIR").ok_or_else(|| std::io::Error::other("OUT_DIR missing"))?;
    let out = PathBuf::from(out);
    std::fs::write(out.join("build-script-ran.txt"), "ran")?;
    println!("cargo:rerun-if-changed=shadow.c");
    let cc = std::env::var_os("CC").ok_or_else(|| std::io::Error::other("CC missing"))?;
    let status = Command::new(cc)
        .args(["-flto", "-o"])
        .arg(out.join("shadow"))
        .arg("shadow.c")
        .status()?;
    if !status.success() {
        return Err(std::io::Error::other(format!("C compile failed: {status}")));
    }
    std::fs::write(out.join("c-compiled.txt"), "ran")
}
