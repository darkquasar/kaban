//! Compare all decoded SCIP facts across analyzer-visible checkout roots.
use anyhow::{Result, ensure};
use protobuf::Message;
use scip::types::Index;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(args.len() == 3, "usage: compare-output FIRST SECOND");
    let mut first = Index::parse_from_bytes(&std::fs::read(&args[1])?)?;
    let mut second = Index::parse_from_bytes(&std::fs::read(&args[2])?)?;
    let first_root = first.metadata.project_root.clone();
    let second_root = second.metadata.project_root.clone();
    ensure!(
        first_root != second_root,
        "different analyzer-visible checkout roots required"
    );
    if let Some(metadata) = first.metadata.as_mut() {
        metadata.project_root.clear();
    }
    if let Some(metadata) = second.metadata.as_mut() {
        metadata.project_root.clear();
    }
    ensure!(
        first == second,
        "decoded keys, local identities, ranges or relationships changed across checkouts/commits"
    );
    println!(
        "all decoded facts equal after project_root-only normalization: {first_root} vs {second_root}"
    );
    Ok(())
}
