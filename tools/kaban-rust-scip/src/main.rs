//! Repository-free probes for the standalone producer qualification candidate.

use std::io::{self, Write};

mod admission;
#[cfg(target_os = "macos")]
mod apple;
mod cargo_config;
mod semantic;

fn main() {
    if let Err(error) = run() {
        // Bound all diagnostics, including untrusted JSON keys and serde errors.
        let diagnostic = format!("{error:#}");
        let mut end = diagnostic.len().min(4000);
        while !diagnostic.is_char_boundary(end) {
            end -= 1;
        }
        eprintln!("Error: {}", &diagnostic[..end]);
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut stdout = io::stdout().lock();
    if args.len() == 1 && args[0] == "--version" {
        writeln!(stdout, "kaban-rust-scip {}", env!("CARGO_PKG_VERSION"))?;
    } else if args.len() == 2 && args[0] == "capabilities" && args[1] == "--json" {
        serde_json::to_writer(&mut stdout, &kaban_rust_scip::capabilities())?;
        writeln!(stdout)?;
    } else {
        if args
            .first()
            .is_some_and(|arg| arg == "index" || arg == "--load-prepared")
        {
            let prepared = admission::check(&args)?;
            return semantic::run(&args, &prepared);
        }
        anyhow::bail!("semantic exporter qualification is incomplete; indexing is unavailable");
    }
    Ok(())
}
