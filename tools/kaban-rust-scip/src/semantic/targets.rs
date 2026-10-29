//! Bind semantic crates to Cargo targets before reducing definitions to symbols.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context as _, Result, ensure};
use kaban_rust_scip::identity::{TargetIdentity, TargetKind};
use ra_ap_hir::Crate;
use ra_ap_ide_db::RootDatabase;
use ra_ap_project_model::{CargoWorkspace, TargetKind as CargoTargetKind};
use ra_ap_vfs::Vfs;
use salsa::Setter;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(super) enum Mode {
    Production,
    Test,
}

impl Mode {
    pub(super) fn set_test(self) -> bool {
        self == Self::Test
    }
}

#[derive(Clone, Debug)]
struct Target {
    identity: TargetIdentity,
    name: String,
    kind: TargetKind,
    test: bool,
    harness: bool,
}

pub(super) struct Catalog {
    roots: BTreeMap<PathBuf, Target>,
    checkout: PathBuf,
    /// Member targets left out because their `required-features` are not active in the
    /// default profile (cargo would not build them either); reported, never refused (DF-2).
    skipped: Vec<String>,
}

/// Identity does not include mode: unchanged production declarations in both
/// databases must share a symbol. Cfg stays attached to semantic facts, not lost
/// in a file-only traversal. Later emission must prove equivalence before merge.
pub(super) struct Context {
    pub(super) krate: Crate,
    pub(super) identity: TargetIdentity,
    pub(super) cfg: Vec<String>,
    pub(super) mode: Mode,
    kind: TargetKind,
    test: bool,
    harness: bool,
}

impl Context {
    pub(super) fn modes(&self) -> &'static [Mode] {
        // Qualified against real Cargo test --all-targets --no-run: explicit
        // all-targets includes test=false library tests; harness=false removes
        // --test, but Cargo still passes --cfg test to that target. Neither
        // metadata flag is permission to erase its test semantic context.
        let _cargo_flags = (self.test, self.harness);
        match self.kind {
            TargetKind::BuildScript => &[Mode::Production],
            TargetKind::Test | TargetKind::Bench => &[Mode::Test],
            _ => &[Mode::Production, Mode::Test],
        }
    }

    pub(super) fn in_mode<T>(
        &mut self,
        db: &mut RootDatabase,
        mode: Mode,
        extract: impl FnOnce(&RootDatabase, &Self) -> Result<T>,
    ) -> Result<T> {
        let baseline = self.krate.cfg(db).clone();
        ensure!(
            !(&baseline)
                .into_iter()
                .any(|atom| atom.to_string() == "test"),
            "selected Cargo target is not in its production baseline"
        );
        if mode == Mode::Test {
            let mut selected = baseline.clone();
            selected.insert_atom(ra_ap_hir::Name::new_root("test").symbol().clone());
            self.krate.base().set_cfg_options(db).to(selected);
        }
        self.mode = mode;
        self.cfg = self
            .krate
            .cfg(db)
            .into_iter()
            .map(ToString::to_string)
            .collect();
        self.cfg.sort();
        // Only this root changes: every dependency retains its production cfg.
        // This is crucial for integration tests calling cfg(not(test)) library
        // APIs, and for proc-macro/build dependencies used by library tests.
        let result = (|| {
            for dependency in self.krate.dependencies(db) {
                ensure!(
                    !dependency
                        .krate
                        .cfg(db)
                        .into_iter()
                        .any(|atom| atom.to_string() == "test"),
                    "unsupported test cfg on a production dependency"
                );
            }
            extract(db, self)
        })();
        if mode == Mode::Test {
            self.krate.base().set_cfg_options(db).to(baseline);
        }
        result
    }
}

impl Catalog {
    /// The names of the targets skipped for inactive `required-features`, in metadata order.
    pub(super) fn skipped(&self) -> &[String] {
        &self.skipped
    }

    pub(super) fn relative_source(&self, vfs: &Vfs, file_id: ra_ap_vfs::FileId) -> Result<String> {
        let path = vfs.file_path(file_id);
        let path: &Path = path
            .as_path()
            .context("local symbol has no source path")?
            .as_ref();
        Ok(path
            .strip_prefix(&self.checkout)
            .context("local symbol is outside the committed checkout")?
            .to_str()
            .context("local source path is not UTF-8")?
            .to_owned())
    }
    pub(super) fn identity(
        &self,
        db: &RootDatabase,
        vfs: &Vfs,
        krate: Crate,
    ) -> Option<&TargetIdentity> {
        let path = vfs.file_path(krate.root_file(db));
        let path: &Path = path.as_path()?.as_ref();
        self.roots.get(path).map(|target| &target.identity)
    }
    pub(super) fn read(
        cargo: &CargoWorkspace,
        checkout: &Path,
        metadata: &serde_json::Value,
    ) -> Result<Self> {
        let mut result = Self {
            roots: BTreeMap::new(),
            checkout: checkout.to_path_buf(),
            skipped: Vec::new(),
        };
        for package in cargo.packages() {
            let package = &cargo[package];
            if !package.is_member {
                continue;
            }
            let manifest_path: &Path = package.manifest.as_ref();
            let manifest: toml::Value = toml::from_str(&std::fs::read_to_string(manifest_path)?)?;
            for target in &package.targets {
                let target = &cargo[*target];
                if !target
                    .required_features
                    .iter()
                    .all(|f| package.active_features.contains(f))
                {
                    result.skipped.push(target.name.clone());
                    continue;
                }
                let root: &Path = target.root.as_ref();
                let relative = root
                    .strip_prefix(checkout)
                    .context("unsupported Cargo target root outside the committed checkout")?
                    .to_str()
                    .context("Cargo target root is not UTF-8")?;
                let kind = match target.kind {
                    CargoTargetKind::Lib {
                        is_proc_macro: false,
                    } => TargetKind::Library,
                    CargoTargetKind::Lib {
                        is_proc_macro: true,
                    } => TargetKind::ProcMacro,
                    CargoTargetKind::Bin => TargetKind::Binary,
                    CargoTargetKind::Test => TargetKind::Test,
                    CargoTargetKind::Example => TargetKind::Example,
                    CargoTargetKind::Bench => TargetKind::Bench,
                    CargoTargetKind::BuildScript => TargetKind::BuildScript,
                    CargoTargetKind::Other => {
                        anyhow::bail!("unsupported Cargo target kind for {}", target.name)
                    }
                };
                let identity = TargetIdentity::new(
                    &package.name,
                    &package.version.to_string(),
                    kind,
                    &target.name,
                    relative,
                )?;
                let metadata_target = metadata["packages"]
                    .as_array()
                    .context("Cargo metadata packages missing")?
                    .iter()
                    .find(|value| value["manifest_path"].as_str() == manifest_path.to_str())
                    .and_then(|value| value["targets"].as_array())
                    .and_then(|values| {
                        values.iter().find(|value| {
                            value["src_path"].as_str() == root.to_str()
                                && value["name"].as_str() == Some(&target.name)
                        })
                    })
                    .context("Cargo target test metadata unavailable")?;
                let test = metadata_target["test"]
                    .as_bool()
                    .context("Cargo target test flag unavailable")?;
                let table = match kind {
                    TargetKind::Library | TargetKind::ProcMacro => manifest.get("lib"),
                    TargetKind::BuildScript => None,
                    _ => {
                        let key = match kind {
                            TargetKind::Binary => "bin",
                            TargetKind::Test => "test",
                            TargetKind::Bench => "bench",
                            _ => "example",
                        };
                        manifest
                            .get(key)
                            .and_then(toml::Value::as_array)
                            .and_then(|values| {
                                values.iter().find(|value| {
                                    value.get("name").and_then(toml::Value::as_str)
                                        == Some(&target.name)
                                })
                            })
                    }
                };
                let harness = table
                    .and_then(|value| value.get("harness"))
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(true);
                result.insert(
                    root.to_path_buf(),
                    Target {
                        identity,
                        name: target.name.clone(),
                        kind,
                        test,
                        harness,
                    },
                )?;
            }
        }
        ensure!(
            !result.roots.is_empty(),
            "Cargo workspace contains no supported member targets"
        );
        Ok(result)
    }

    fn insert(&mut self, root: PathBuf, target: Target) -> Result<()> {
        // RA does not retain Cargo target kind at HIR crate identity, and can
        // collapse equal root/name/cfg crate builders. Do not guess a target or
        // silently drop one when that mapping is not injective.
        ensure!(
            !self.roots.contains_key(&root),
            "unsupported shared-source Cargo target root {}: distinct target contexts cannot be identified unambiguously",
            root.display()
        );
        self.roots.insert(root, target);
        Ok(())
    }

    pub(super) fn bind(&self, db: &RootDatabase, vfs: &Vfs, mode: Mode) -> Result<Vec<Context>> {
        let mut found = BTreeMap::new();
        for krate in Crate::all(db) {
            let root = vfs.file_path(krate.root_file(db));
            let Some(path) = root.as_path() else { continue };
            let path: &Path = path.as_ref();
            let Some(target) = self.roots.get(path) else {
                continue;
            };
            let name = krate
                .display_name(db)
                .context("semantic Cargo target has no name")?;
            ensure!(
                name.canonical_name().as_str() == target.name,
                "semantic Cargo target name does not match metadata"
            );
            let mut cfg: Vec<_> = krate.cfg(db).into_iter().map(ToString::to_string).collect();
            cfg.sort();
            ensure!(
                cfg.iter().any(|atom| atom == "test") == mode.set_test(),
                "semantic Cargo target cfg does not match requested context"
            );
            ensure!(
                found
                    .insert(
                        path.to_path_buf(),
                        Context {
                            krate,
                            identity: target.identity.clone(),
                            cfg,
                            mode,
                            kind: target.kind,
                            test: target.test,
                            harness: target.harness,
                        }
                    )
                    .is_none(),
                "unsupported multiple semantic crates for a Cargo target root"
            );
        }
        ensure!(
            found.len() == self.roots.len(),
            "semantic loading omitted one or more Cargo member target contexts"
        );
        Ok(found.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(kind: TargetKind) -> Target {
        Target {
            identity: TargetIdentity::new("demo", "1.0.0", kind, "demo", "src/shared.rs")
                .expect("fixture identity"),
            name: "demo".into(),
            kind,
            test: true,
            harness: true,
        }
    }

    #[test]
    fn same_named_library_and_binary_with_distinct_roots_are_retained() {
        let mut catalog = Catalog {
            roots: BTreeMap::new(),
            checkout: "/work".into(),
            skipped: Vec::new(),
        };
        catalog
            .insert("/work/src/lib.rs".into(), target(TargetKind::Library))
            .expect("lib");
        catalog
            .insert("/work/src/main.rs".into(), target(TargetKind::Binary))
            .expect("bin");
        assert_eq!(catalog.roots.len(), 2);
    }

    #[test]
    fn shared_source_target_contexts_are_refused_not_overwritten() {
        let mut catalog = Catalog {
            roots: BTreeMap::new(),
            checkout: "/work".into(),
            skipped: Vec::new(),
        };
        catalog
            .insert("/work/src/shared.rs".into(), target(TargetKind::Library))
            .expect("first");
        let error = catalog
            .insert("/work/src/shared.rs".into(), target(TargetKind::Binary))
            .expect_err("ambiguous");
        assert!(error.to_string().contains("unsupported shared-source"));
        assert_eq!(catalog.roots.len(), 1);
    }

    #[test]
    fn selected_integration_test_resolves_the_production_library_and_restores_cfg() {
        use ra_ap_hir::{CfgOptions, ChangeWithProcMacros, Name};
        use ra_ap_ide_db::base_db::{
            CrateDisplayName, CrateGraphBuilder, CrateName, CrateOrigin, CrateWorkspaceData,
            DependencyBuilder, Env, SourceRoot,
        };
        use ra_ap_vfs::{FileId, VfsPath, file_set::FileSet};

        let lib_file = FileId::from_raw(0);
        let test_file = FileId::from_raw(1);
        let mut files = FileSet::default();
        files.insert(
            lib_file,
            VfsPath::new_virtual_path("/work/src/lib.rs".into()),
        );
        files.insert(
            test_file,
            VfsPath::new_virtual_path("/work/tests/integration.rs".into()),
        );
        let mut graph = CrateGraphBuilder::default();
        let mut root = |file, name| {
            graph.add_crate_root(
                file,
                ra_ap_ide::Edition::Edition2024,
                Some(CrateDisplayName::from_canonical_name(name)),
                None,
                CfgOptions::default(),
                None,
                Env::default(),
                CrateOrigin::Local {
                    repo: None,
                    name: None,
                },
                vec![],
                false,
                ra_ap_paths::AbsPathBuf::assert_utf8("/work".into()).into(),
                CrateWorkspaceData {
                    target: Err("invented unit fixture".into()),
                    toolchain: None,
                }
                .into(),
            )
        };
        let lib = root(lib_file, "collision");
        let integration = root(test_file, "integration");
        graph
            .add_dep(
                integration,
                DependencyBuilder::new(
                    CrateName::new("collision").expect("fixture crate name"),
                    lib,
                ),
            )
            .expect("acyclic fixture");
        let mut change = ChangeWithProcMacros::default();
        change.set_roots(vec![SourceRoot::new_local(files)]);
        change.change_file(lib_file, Some("#[cfg(not(test))] pub fn production_only() {}\n#[cfg(test)] pub fn library_test_only() {}\ntrait Marker {}\nstruct Positive; struct Negative; struct Conditional; struct Generic<T>(T);\nimpl Marker for Positive {}\nimpl !Marker for Negative {}\nimpl Marker for Conditional where Conditional: Copy {}\nimpl<T> Marker for Generic<T> {}\nimpl Marker for &Positive {}\nimpl Marker for (Positive,) {}".into()));
        change.change_file(
            test_file,
            Some("pub fn integration() { collision::production_only(); }".into()),
        );
        change.set_crate_graph(graph);
        let mut db = RootDatabase::new(None);
        db.apply_change(change);
        let crates = Crate::all(&db);
        let library = *crates
            .iter()
            .find(|krate| krate.root_file(&db) == lib_file)
            .expect("library");
        let integration = *crates
            .iter()
            .find(|krate| krate.root_file(&db) == test_file)
            .expect("integration");
        ra_ap_hir::attach_db(&db, || {
            let implementations: Vec<_> = library
                .modules(&db)
                .into_iter()
                .flat_map(|module| module.impl_defs(&db))
                .collect();
            assert_eq!(implementations.len(), 6);
            let eligible: Vec<_> = implementations
                .into_iter()
                .filter_map(|implementation| {
                    super::super::declarations::implementation_endpoints(&db, implementation)
                })
                .collect();
            assert_eq!(
                eligible.len(),
                1,
                "negative, conditional, generic, reference and tuple impls cannot become unconditional edges"
            );
            assert_eq!(eligible[0].0.name(&db), Name::new_root("Positive"));
        });
        let mut context = Context {
            krate: integration,
            identity: TargetIdentity::new(
                "demo",
                "1.0.0",
                TargetKind::Test,
                "integration",
                "tests/integration.rs",
            )
            .expect("identity"),
            mode: Mode::Production,
            cfg: vec![],
            kind: TargetKind::Test,
            test: true,
            harness: true,
        };
        let resolve = |db: &RootDatabase| {
            integration
                .root_module(db)
                .resolve_mod_path(
                    db,
                    [
                        Name::new_root("collision"),
                        Name::new_root("production_only"),
                    ],
                )
                .expect("resolver")
                .collect::<Vec<_>>()
        };
        context
            .in_mode(&mut db, Mode::Test, |db, selected| {
                assert!(selected.cfg.iter().any(|atom| atom == "test"));
                let resolved = resolve(db);
                assert_eq!(resolved.len(), 1, "integration reference must resolve");
                assert_eq!(
                    resolved[0].krate(db),
                    Some(library),
                    "reference must target the production library crate"
                );
                Ok(())
            })
            .expect("selected-root test context");
        assert!(
            !integration
                .cfg(&db)
                .get_cfg_keys()
                .any(|key| key.as_str() == "test"),
            "selected root must be restored"
        );

        let production_cfg = library.cfg(&db).clone();
        let mut global_test_cfg = production_cfg.clone();
        global_test_cfg.insert_atom(Name::new_root("test").symbol().clone());
        library.base().set_cfg_options(&mut db).to(global_test_cfg);
        assert!(
            resolve(&db).is_empty(),
            "global test overlay must demonstrate the lost production reference"
        );
        let refused = context
            .in_mode(&mut db, Mode::Test, |_, _| Ok(()))
            .expect_err("test dependency cannot be passed off as production");
        assert!(
            refused
                .to_string()
                .contains("test cfg on a production dependency")
        );
        assert!(
            !integration
                .cfg(&db)
                .get_cfg_keys()
                .any(|key| key.as_str() == "test"),
            "error must restore selected root"
        );
        library.base().set_cfg_options(&mut db).to(production_cfg);
        context
            .in_mode(&mut db, Mode::Test, |db, _| {
                assert_eq!(
                    resolve(db).len(),
                    1,
                    "restoration must invalidate stale semantic caches"
                );
                Ok(())
            })
            .expect("restored production resolution");
    }
}
