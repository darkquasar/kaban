//! Stable SCIP target namespaces, independent of checkout paths and database IDs.

use std::path::{Component, Path};

use scip::types::{Descriptor, Package, Symbol, descriptor::Suffix};

/// Cargo target category; same-named library and binary targets are distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetKind {
    /// Library target.
    Library,
    /// Executable target.
    Binary,
    /// Integration test target.
    Test,
    /// Example target.
    Example,
    /// Benchmark target.
    Bench,
    /// Cargo build script target.
    BuildScript,
    /// Procedural macro library target.
    ProcMacro,
}

impl TargetKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Library => "lib",
            Self::Binary => "bin",
            Self::Test => "test",
            Self::Example => "example",
            Self::Bench => "bench",
            Self::BuildScript => "custom-build",
            Self::ProcMacro => "proc-macro",
        }
    }
}

/// A Cargo target identity with a committed, checkout-relative source root.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetIdentity {
    package_name: String,
    package_version: String,
    namespace: String,
}

/// Invalid package fields or a root that is not a normalized relative Rust path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidIdentity;

impl std::fmt::Display for InvalidIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .write_str("target identity requires nonempty Cargo fields and a relative source root")
    }
}

impl std::error::Error for InvalidIdentity {}

impl TargetIdentity {
    /// Construct an identity from Cargo metadata and a checkout-relative root.
    ///
    /// # Errors
    /// Rejects empty or whitespace-bearing package fields, invalid Cargo target
    /// names, and absolute/non-normalized source roots. Never embeds a checkout.
    pub fn new(
        package_name: &str,
        package_version: &str,
        kind: TargetKind,
        target_name: &str,
        relative_root: &str,
    ) -> Result<Self, InvalidIdentity> {
        let cargo_name = |value: &str| {
            !value.is_empty()
                && value
                    .chars()
                    .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
        };
        if !cargo_name(package_name)
            || !cargo_name(target_name)
            || package_version.is_empty()
            || package_version.chars().any(char::is_whitespace)
            || relative_root.is_empty()
            || relative_root.contains('\\')
            || relative_root
                .split('/')
                .any(|part| matches!(part, "" | "." | ".."))
            || Path::new(relative_root)
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
        {
            return Err(InvalidIdentity);
        }
        Ok(Self {
            package_name: package_name.to_owned(),
            package_version: package_version.to_owned(),
            namespace: format!("{}:{target_name}:{relative_root}", kind.as_str()),
        })
    }

    /// The Cargo package name (the `per_crate` key of the coverage report).
    #[must_use]
    pub fn package_name(&self) -> &str {
        &self.package_name
    }

    /// Prefix semantic item descriptors with this target's escaped namespace.
    ///
    /// Item descriptors must come from semantic definitions, never parsed names.
    pub fn symbol(&self, descriptors: impl IntoIterator<Item = Descriptor>) -> String {
        let mut target_descriptors = vec![Descriptor {
            name: self.namespace.clone(),
            suffix: Suffix::Namespace.into(),
            ..Descriptor::default()
        }];
        target_descriptors.extend(descriptors);
        scip::symbol::format_symbol(Symbol {
            scheme: "kaban-rust-scip".to_owned(),
            package: Some(Package {
                manager: "cargo".to_owned(),
                name: self.package_name.clone(),
                version: self.package_version.clone(),
                ..Package::default()
            })
            .into(),
            descriptors: target_descriptors,
            ..Symbol::default()
        })
    }
}
