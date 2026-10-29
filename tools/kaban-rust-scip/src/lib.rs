//! Contract and symbol primitives for the standalone Rust producer.
//!
//! A host is advertised only after the complete semantic fixture oracle (all 21 qualification
//! profiles) passed on it; the evidence lives outside the repository (R0 qualification folder).

pub mod identity;

/// Repository-free producer capabilities. Empty hosts mean indexing is unavailable.
#[derive(Debug, serde::Serialize)]
pub struct Capabilities {
    /// Prepared invocation protocol understood by this candidate.
    pub protocol_version: u32,
    /// Required semantic profile; this does not itself claim qualification.
    pub semantic_profile: &'static str,
    /// Distinct producer version, never the stock rust-analyzer version.
    pub producer_version: &'static str,
    /// Exact candidate rust-analyzer library version.
    pub ra_ap_version: &'static str,
    /// Native host triples that passed the complete producer oracle.
    pub supported_host_triples: &'static [&'static str],
}

/// Return bounded static capabilities without inspecting the repository or tools.
pub fn capabilities() -> Capabilities {
    Capabilities {
        protocol_version: 2,
        semantic_profile: "kaban-rust-v1",
        producer_version: env!("CARGO_PKG_VERSION"),
        ra_ap_version: "0.0.358",
        supported_host_triples: &["aarch64-unknown-linux-gnu"],
    }
}
