//! CG4.2 (stage1-spec G1-F20, G1-F7; stage1-plan CG4.2): language detection by root markers and
//! indexer resolution (`--indexer` absolute, else one `PATH` lookup, `fstat`-checked).

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kaban_index::detect::{
    DetectError, DetectErrorClass, IndexerOverride, Lang, languages, nested_workspaces,
    resolve_indexer,
};

struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "kaban-cg42-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&dir).expect("scratch");
        Self(dir)
    }
    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fake_tool(dir: &Path, name: &str, mode: u32) -> PathBuf {
    fs::create_dir_all(dir).unwrap();
    let p = dir.join(name);
    fs::write(&p, "#!/bin/sh\necho fake\n").unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(mode)).unwrap();
    p
}

/// Dangling `rust-toolchain[.toml]` links in the repository or an ancestor refuse; a
/// `.cargo/config[.toml]` is not the profile's concern (DF-2: cargo's environment beats it).
#[test]
fn rust_config_profile_refuses_dangling_toolchain_selectors_not_cargo_config() {
    let s = Scratch::new("rust-config");
    let repo = s.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    assert_eq!(kaban_index::doctor::rust_repository_profile(&repo), Ok(()));
    for config in [".cargo/config", ".cargo/config.toml"] {
        let path = repo.join(config);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[build]\nrustc-wrapper = \"./anything\"\n").unwrap();
        assert_eq!(
            kaban_index::doctor::rust_repository_profile(&repo),
            Ok(()),
            "{config}"
        );
        fs::remove_file(path).unwrap();
    }
    for selector in ["rust-toolchain", "rust-toolchain.toml"] {
        let path = s.path().join(selector);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("/nonexistent-config", &path).unwrap();
        assert!(
            kaban_index::doctor::rust_repository_profile(&repo).is_err(),
            "{selector}"
        );
        fs::remove_file(path).unwrap();
    }
}

#[test]
fn rust_selector_accepts_only_installed_explicit_requirements() {
    let s = Scratch::new("rust-selector");
    let repo = s.path().join("repo");
    let root = s.path().join("toolchain");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(root.join("lib/rustlib/src/rust/library/core/src")).unwrap();
    fs::write(
        root.join("lib/rustlib/src/rust/library/core/src/lib.rs"),
        "",
    )
    .unwrap();
    fs::write(root.join("lib/rustlib/components"), "rust-src\n").unwrap();
    let selector = repo.join("rust-toolchain.toml");
    for positive in [
        "[toolchain]\ncomponents = ['rust-src']\n",
        "[toolchain]\nchannel = '1.99.0'\ncomponents = ['rust-src']\n",
        // rustup's install-set hint, not a requirement: `minimal` adds none (RI-3 Task 0)
        "[toolchain]\nchannel = '1.99.0'\nprofile = 'minimal'\ncomponents = ['rust-src']\n",
    ] {
        fs::write(&selector, positive).unwrap();
        assert_eq!(
            kaban_index::doctor::rust_repository_toolchain(&repo, &root),
            Ok(())
        );
    }
    let host = kaban_index::doctor::rust_host();
    fs::create_dir_all(root.join("lib/rustlib").join(host).join("lib")).unwrap();
    fs::write(
        root.join("lib/rustlib/components"),
        format!("rust-src\nrust-std-{host}\n"),
    )
    .unwrap();
    fs::write(&selector, format!("[toolchain]\ntargets = ['{host}']\n")).unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Ok(())
    );
    for negative in [
        "channel = '1.95.0'",
        "channel = 'stable'",
        "profile = 'default'",
        "profile = 'complete'",
        "path = '/tmp'",
        "components = ['missing']",
        "targets = ['fixture-guaranteed-absent-target']",
    ] {
        fs::write(&selector, format!("[toolchain]\n{negative}\n")).unwrap();
        assert!(
            kaban_index::doctor::rust_repository_toolchain(&repo, &root).is_err(),
            "{negative}"
        );
    }
    // `default` asks for the installed rustfmt, clippy and rust-docs of the host through the
    // same requirement path as `components`: the `lib/rustlib/components` entry AND the
    // installed resource (ri3-3); `complete` stays outside the subset.
    fs::write(&selector, "[toolchain]\nprofile = 'default'\n").unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Err("rust_toolchain_component_missing")
    );
    fs::write(
        root.join("lib/rustlib/components"),
        format!("rust-src\nrust-std-{host}\nrustfmt-preview-{host}\nclippy-preview-{host}\nrust-docs-{host}\n"),
    )
    .unwrap();
    let profile_resources = [
        "bin/rustfmt",
        "bin/clippy-driver",
        "share/doc/rust/html/index.html",
    ];
    for resource in profile_resources {
        fs::create_dir_all(root.join(resource).parent().unwrap()).unwrap();
        fs::write(root.join(resource), "").unwrap();
    }
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Ok(())
    );
    for resource in profile_resources {
        fs::remove_file(root.join(resource)).unwrap();
        assert_eq!(
            kaban_index::doctor::rust_repository_toolchain(&repo, &root),
            Err("rust_toolchain_component_missing"),
            "manifest entry kept, {resource} removed"
        );
        fs::write(root.join(resource), "").unwrap();
    }
    fs::write(&selector, "[toolchain]\ncomponents = ['rust-docs']\n").unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Ok(())
    );
    fs::write(&selector, "[toolchain]\nprofile = 'complete'\n").unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Err("rust_toolchain_selector_unsupported")
    );
    fs::write(
        root.join("lib/rustlib/components"),
        format!("rust-src\nrust-std-{host}\n"),
    )
    .unwrap();
    fs::write(&selector, "[toolchain]\ncomponents = ['rust-src']\n").unwrap();
    fs::remove_file(root.join("lib/rustlib/src/rust/library/core/src/lib.rs")).unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Err("rust_toolchain_component_missing")
    );
    fs::write(repo.join("rust-toolchain"), "1.99.0").unwrap();
    assert_eq!(
        kaban_index::doctor::rust_repository_toolchain(&repo, &root),
        Err("rust_toolchain_selector_ambiguous")
    );
}

/// hd1-1: a repository `.cargo/config[.toml]` stays accepted (DF-2) unless, for the active
/// target, it redirects the compiler backend kaban pins: a forced `[env]` entry on a variable
/// of the pin, rustflags carrying `-B`/`-C linker=`/`-C link-self-contained`, or a `linker`.
/// Tables of other targets (kaban's own musl linker) stay inert and accepted.
#[test]
fn rust_config_profile_refuses_cargo_config_that_redirects_the_backend_for_the_active_target() {
    let s = Scratch::new("rust-config-backend");
    let repo = s.path().join("repo");
    let config = repo.join(".cargo/config.toml");
    fs::create_dir_all(config.parent().unwrap()).unwrap();
    let host = kaban_index::doctor::rust_host();
    for (text, expected) in [
        (
            "[env]\nCOMPILER_PATH = { value = \"x\", force = true }\n".to_owned(),
            Err("rust_cargo_config_redirects_backend"),
        ),
        (
            format!("[target.{host}]\nrustflags = [\"-C\", \"link-arg=-B/tmp/x/\"]\n"),
            Err("rust_cargo_config_redirects_backend"),
        ),
        (
            format!("[target.{host}]\nlinker = \"cc\"\n"),
            Err("rust_cargo_config_redirects_backend"),
        ),
        (
            "[target.x86_64-unknown-linux-musl]\nlinker = \"musl-gcc\"\nrustflags = [\"-C\", \"target-feature=+crt-static\"]\n".to_owned(),
            Ok(()),
        ),
        (
            "[env]\nCOMPILER_PATH = \"x\"\n[build]\nrustc-wrapper = \"./anything\"\n".to_owned(),
            Ok(()),
        ),
    ] {
        fs::write(&config, &text).unwrap();
        assert_eq!(
            kaban_index::doctor::rust_repository_profile(&repo),
            expected,
            "{text}"
        );
    }
}

#[test]
fn detect_markers() {
    let s = Scratch::new("markers");
    let no_path: Option<&OsString> = None;

    let go = s.path().join("go");
    fs::create_dir_all(&go).unwrap();
    fs::write(go.join("go.mod"), "module x\n").unwrap();
    let d = languages(&go, &[], no_path, &[]).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].lang, Lang::Go);
    assert_eq!(d[0].markers, vec!["go.mod"]);
    assert_eq!(d[0].indexer, Err(DetectErrorClass::IndexerNotFound));

    let ts = s.path().join("ts");
    fs::create_dir_all(&ts).unwrap();
    fs::write(ts.join("tsconfig.json"), "{}").unwrap();
    let d = languages(&ts, &[], no_path, &[]).unwrap();
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].lang, Lang::TypeScript);
    assert_eq!(d[0].markers, vec!["tsconfig.json"]);

    let both = s.path().join("both");
    fs::create_dir_all(&both).unwrap();
    fs::write(both.join("go.mod"), "module x\n").unwrap();
    fs::write(both.join("package.json"), "{}").unwrap();
    fs::write(both.join("tsconfig.json"), "{}").unwrap();
    let d = languages(&both, &[], no_path, &[]).unwrap();
    assert_eq!(
        d.iter().map(|x| x.lang).collect::<Vec<_>>(),
        vec![Lang::Go, Lang::TypeScript]
    );
    assert_eq!(d[1].markers, vec!["package.json", "tsconfig.json"]);

    // Cargo package and workspace roots are recognized without running Cargo.
    let rust = s.path().join("rust");
    fs::create_dir_all(&rust).unwrap();
    fs::write(rust.join("Cargo.toml"), "[package]\n").unwrap();
    assert_eq!(
        languages(&rust, &[], no_path, &[]).unwrap()[0].lang,
        Lang::Rust
    );
    assert_eq!(
        DetectError::UnsupportedInStage1.to_string(),
        "unsupported_in_stage_1"
    );

    // a marker that is a directory does not count
    let dirmarker = s.path().join("dirmarker");
    fs::create_dir_all(dirmarker.join("go.mod")).unwrap();
    assert_eq!(
        languages(&dirmarker, &[], no_path, &[]).unwrap_err(),
        DetectError::UnsupportedInStage1
    );
}

#[test]
fn indexer_on_path_but_group_writable_is_untrusted_and_not_used() {
    let s = Scratch::new("mode");
    let bin = s.path().join("bin");
    fake_tool(&bin, "scip-go", 0o775);
    let path_env = OsString::from(bin.as_os_str());
    assert_eq!(
        resolve_indexer(Lang::Go, None, Some(&path_env), &[]).unwrap_err(),
        DetectError::UntrustedMode
    );
    assert_eq!(
        DetectError::UntrustedMode.to_string(),
        "indexer_untrusted_mode"
    );
    // the detection report carries the class and no path
    let go = s.path().join("go");
    fs::create_dir_all(&go).unwrap();
    fs::write(go.join("go.mod"), "module x\n").unwrap();
    let d = languages(&go, &[], Some(&path_env), &[]).unwrap();
    assert_eq!(d[0].indexer, Err(DetectErrorClass::IndexerUntrustedMode));
    // the class is a fixed string (the resolved type itself is never serialised: U2 RU2-8)
    assert_eq!(
        d[0].indexer.as_ref().unwrap_err().as_str(),
        "indexer_untrusted_mode"
    );

    // the same tool at 0755 resolves to the absolute path on the first PATH hit
    fs::set_permissions(bin.join("scip-go"), fs::Permissions::from_mode(0o755)).unwrap();
    let r = resolve_indexer(Lang::Go, None, Some(&path_env), &[]).unwrap();
    assert_eq!(r.path, bin.join("scip-go"));
    assert!(!r.from_override);
    assert!(r.path.is_absolute());

    // a relative PATH entry is skipped, an other-writable later entry is not consulted when
    // the first absolute hit is good
    let other = s.path().join("other");
    fake_tool(&other, "scip-go", 0o777);
    let mixed =
        std::env::join_paths([PathBuf::from("relative/bin"), bin.clone(), other.clone()]).unwrap();
    assert_eq!(
        resolve_indexer(Lang::Go, None, Some(&mixed), &[])
            .unwrap()
            .path,
        bin.join("scip-go")
    );
    // and the first hit in untrusted mode is reported, not skipped over
    let mixed = std::env::join_paths([other.clone(), bin.clone()]).unwrap();
    assert_eq!(
        resolve_indexer(Lang::Go, None, Some(&mixed), &[]).unwrap_err(),
        DetectError::UntrustedMode
    );
    // no PATH at all: not found
    assert_eq!(
        resolve_indexer(Lang::Go, None, None, &[]).unwrap_err(),
        DetectError::NotFound
    );
}

#[test]
fn indexer_override_must_be_absolute_and_trusted() {
    let s = Scratch::new("override");
    let tool = fake_tool(&s.path().join("bin"), "my-scip", 0o700);
    let arg = format!("go={}", tool.display());
    let o = IndexerOverride::parse(&arg).unwrap();
    assert_eq!(o.lang, Lang::Go);
    let r = resolve_indexer(Lang::Go, Some(&o.path), None, &[]).unwrap();
    assert!(r.from_override);
    assert_eq!(r.path, tool);

    assert_eq!(
        IndexerOverride::parse("go=bin/scip-go").unwrap_err(),
        DetectError::NotAbsolute
    );
    assert_eq!(
        IndexerOverride::parse("python=/x").unwrap_err(),
        DetectError::OverrideMalformed
    );
    assert_eq!(
        IndexerOverride::parse("/x").unwrap_err(),
        DetectError::OverrideMalformed
    );
    assert_eq!(
        resolve_indexer(Lang::Go, Some(Path::new("/nonexistent/scip-go")), None, &[]).unwrap_err(),
        DetectError::NotFound
    );
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o722)).unwrap();
    assert_eq!(
        resolve_indexer(Lang::Go, Some(&tool), None, &[]).unwrap_err(),
        DetectError::UntrustedMode
    );
    // the override wins over PATH for its language only
    fs::set_permissions(&tool, fs::Permissions::from_mode(0o700)).unwrap();
    let repo = s.path().join("repo");
    fs::create_dir_all(&repo).unwrap();
    fs::write(repo.join("go.mod"), "module x\n").unwrap();
    fs::write(repo.join("package.json"), "{}").unwrap();
    let d = languages(&repo, &[o], None, &[]).unwrap();
    assert_eq!(d[0].indexer.as_ref().unwrap().path, tool);
    assert_eq!(d[1].indexer, Err(DetectErrorClass::IndexerNotFound));
}

/// Nested Cargo workspaces (backlog 17): a `Cargo.toml` below the root that declares a
/// `[workspace]` table (or a `[workspace.*]` sub-table) is a root of its own that the root
/// workspace's run never indexes, so it is named by its relative path. Members of the root
/// workspace are not nested roots; nothing under a nested root already found, under `target/`
/// or a hidden directory, or deeper than six directories is walked; under a root that is a
/// plain package every nested manifest is its own root; at most 32 roots are named.
#[test]
fn nested_cargo_workspaces_are_named_by_path_and_bounded() {
    let s = Scratch::new("nested");
    let repo = s.path().join("repo");
    let write = |rel: &str, body: &str| {
        let p = repo.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, body).unwrap();
    };
    write("Cargo.toml", "[workspace]\nmembers = [\"crates/a\"]\n");
    write("crates/a/Cargo.toml", "[package]\nname = \"a\"\n");
    write(
        "tools/producer/Cargo.toml",
        "[workspace]\nmembers = [\".\"]\n\n[package]\nname = \"producer\"\n",
    );
    write(
        "tools/producer/tests/fixtures/inner/Cargo.toml",
        "[workspace]\n",
    );
    write(
        "fixtures/deep/Cargo.toml",
        "[workspace.package]\nedition = \"2024\"\n",
    );
    write("target/debug/build/x/Cargo.toml", "[workspace]\n");
    write(".cargo/x/Cargo.toml", "[workspace]\n");
    write("a/b/c/d/e/f/Cargo.toml", "[workspace]\n");
    write("p/q/r/s/t/u/v/Cargo.toml", "[workspace]\n");
    assert_eq!(
        nested_workspaces(&repo),
        vec![
            PathBuf::from("a/b/c/d/e/f"),
            PathBuf::from("fixtures/deep"),
            PathBuf::from("tools/producer"),
        ]
    );

    // a plain package at the root: a nested package is not its member, so it is a root
    let plain = s.path().join("plain");
    fs::create_dir_all(plain.join("sub")).unwrap();
    fs::write(plain.join("Cargo.toml"), "[package]\nname = \"p\"\n").unwrap();
    fs::write(plain.join("sub/Cargo.toml"), "[package]\nname = \"s\"\n").unwrap();
    assert_eq!(nested_workspaces(&plain), vec![PathBuf::from("sub")]);

    // the cap: 40 nested roots, the first 32 in path order
    let capped = s.path().join("capped");
    fs::create_dir_all(&capped).unwrap();
    fs::write(capped.join("Cargo.toml"), "[workspace]\n").unwrap();
    for i in 0..40 {
        let dir = capped.join(format!("w{i:02}"));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
    }
    let found = nested_workspaces(&capped);
    assert_eq!(found.len(), 32);
    assert_eq!(found[31], PathBuf::from("w31"));

    // no manifest below the root: nothing
    let none = s.path().join("none");
    fs::create_dir_all(none.join("src")).unwrap();
    fs::write(none.join("Cargo.toml"), "[workspace]\n").unwrap();
    assert_eq!(nested_workspaces(&none), Vec::<PathBuf>::new());
}
