//! Walker rules (docs-ingest-plan.md DI0.2; docs-ingest-spec D5, D6 path rules; dispositions
//! D-7 to D-10, D-26, R-9; review RD-2, RD-7): every skip reason is reported bare, with no path
//! and no identity derived from the path (nothing is hashed before `gate_path`); the order is
//! byte-sorted and identical across runs; roots are validated before any traversal.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::Scratch;
use kaban_ingest::{Candidate, Skip, WalkConfig, walk};
use std::fs;
use std::path::Path;

/// A fresh scratch tree under the OS temp dir, removed on drop.
fn scratch(name: &str) -> Scratch {
    Scratch::new("kaban-ingest-walk", name)
}

fn write(root: &Path, rel: &str, bytes: &[u8]) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, bytes).unwrap();
}

type Item = Result<Candidate, Skip>;

fn run(cfg: &WalkConfig) -> Vec<Item> {
    walk(cfg).unwrap().collect()
}

fn skips(items: &[Item]) -> Vec<&Skip> {
    items.iter().filter_map(|i| i.as_ref().err()).collect()
}

fn ok_names(items: &[Item]) -> Vec<String> {
    items
        .iter()
        .filter_map(|i| i.as_ref().ok())
        .map(|c| c.abs.file_name().unwrap().to_string_lossy().into_owned())
        .collect()
}

/// Plan slice 1: `.env` and `secrets/x.txt` are `Denied` with the id (index) of the default
/// deny glob that matched; the hidden-file default of `ignore` does not hide `.env` (R-9). A
/// walker skip carries the reason only (RD-7): no keyed id exists before `gate_path`.
#[test]
fn env_and_secrets_files_are_denied_with_the_glob_id() {
    let root = scratch("denied");
    write(&root, ".env", b"TOKEN=abc\n");
    write(&root, "secrets/x.txt", b"plain\n");
    write(&root, "notes.md", b"# notes\n");
    let cfg = WalkConfig::new(root.to_path_buf());
    let items = run(&cfg);

    let denied: Vec<&str> = skips(&items)
        .into_iter()
        .filter_map(|s| match s {
            Skip::Denied(id) => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        denied,
        vec!["0", "4"],
        "`.env*` is glob 0, `secrets/**` is glob 4: {items:?}"
    );
    assert_eq!(ok_names(&items), vec!["notes.md"]);
    assert_eq!(
        skips(&items).len(),
        2,
        "two bare skips, nothing else attached to them: {items:?}"
    );
}

/// Plan slice 2: a 3 MiB file is `TooLarge` under the default 2 MiB cap; raising the cap admits it.
#[test]
fn a_three_mib_file_is_too_large_under_the_default_cap() {
    let root = scratch("too-large");
    write(&root, "big.md", &vec![b'a'; 3 * 1024 * 1024]);
    write(&root, "small.md", b"# small\n");
    let cfg = WalkConfig::new(root.to_path_buf());
    let items = run(&cfg);
    assert_eq!(skips(&items), vec![&Skip::TooLarge], "{items:?}");
    assert_eq!(ok_names(&items), vec!["small.md"]);

    let generous = WalkConfig {
        max_file_bytes: 4 * 1024 * 1024,
        ..cfg
    };
    let items = run(&generous);
    assert_eq!(skips(&items), Vec::<&Skip>::new(), "{items:?}");
    assert_eq!(ok_names(&items), vec!["big.md", "small.md"]);
}

/// Plan slice 3: a text-format file with a NUL byte in its first 8 KiB is `Binary` (D-9: the
/// sniff buffer is transient); a NUL beyond 8 KiB is not sniffed, and a PDF is never sniffed.
#[test]
fn a_file_with_nul_bytes_in_the_first_8_kib_is_binary() {
    let root = scratch("binary");
    let mut early = vec![b'x'; 100];
    early[50] = 0;
    write(&root, "early.md", &early);
    let mut late = vec![b'y'; 9000];
    late[8500] = 0;
    write(&root, "late.md", &late);
    write(&root, "doc.pdf", b"%PDF-1.4\n\0\0binary stream\n");
    let cfg = WalkConfig::new(root.to_path_buf());
    let items = run(&cfg);
    assert_eq!(skips(&items), vec![&Skip::Binary], "{items:?}");
    assert_eq!(ok_names(&items), vec!["doc.pdf", "late.md"]);
}

/// Plan slice 4: `main.rs` is `Code` by default (spec §7: code is excluded) and a `Format::Code`
/// candidate with `include_code`; an unknown extension is `Unsupported` either way.
#[test]
fn main_rs_is_code_unless_include_code() {
    let root = scratch("code");
    write(&root, "src/main.rs", b"fn main() {}\n");
    write(&root, "README.md", b"# readme\n");
    write(&root, "blob.xyz", b"???\n");
    let cfg = WalkConfig::new(root.to_path_buf());
    let items = run(&cfg);
    assert_eq!(
        skips(&items),
        vec![&Skip::Unsupported, &Skip::Code],
        "{items:?}"
    );
    assert_eq!(ok_names(&items), vec!["README.md"]);

    let with_code = WalkConfig {
        include_code: true,
        ..cfg
    };
    let items = run(&with_code);
    assert_eq!(skips(&items), vec![&Skip::Unsupported]);
    assert_eq!(ok_names(&items), vec!["README.md", "main.rs"]);
    let main = items
        .iter()
        .filter_map(|i| i.as_ref().ok())
        .find(|c| c.abs.ends_with("main.rs"))
        .unwrap();
    assert_eq!(main.format, kaban_ingest::Format::Code);
}

/// Plan slice 5 with D-10: a symlink to `/etc/passwd` is `OutsideRoot` whether or not symlinks
/// are followed; a symlink to a file inside the root is `Symlink` by default and a candidate
/// (resolved target metadata) with `follow_symlinks`.
#[test]
fn a_symlink_to_etc_passwd_is_outside_root() {
    let root = scratch("symlink");
    write(&root, "real.md", b"# real\n");
    std::os::unix::fs::symlink("/etc/passwd", root.join("passwd.txt")).unwrap();
    std::os::unix::fs::symlink(root.join("real.md"), root.join("alias.md")).unwrap();
    let cfg = WalkConfig::new(root.to_path_buf());
    let items = run(&cfg);
    assert_eq!(
        skips(&items),
        vec![&Skip::Symlink, &Skip::OutsideRoot],
        "{items:?}"
    );
    assert_eq!(ok_names(&items), vec!["real.md"]);

    let following = WalkConfig {
        follow_symlinks: true,
        ..cfg
    };
    let items = run(&following);
    assert_eq!(skips(&items), vec![&Skip::OutsideRoot], "{items:?}");
    assert_eq!(ok_names(&items), vec!["alias.md", "real.md"]);
    // a followed in-root link opens through the same checked path as a regular file (RD-2)
    let mut text = String::new();
    std::io::Read::read_to_string(
        &mut items[0].as_ref().unwrap().open_checked().unwrap(),
        &mut text,
    )
    .unwrap();
    assert_eq!(text, "# real\n");
}

/// A configuration error names configuration only: never a directory or file name (S1).
fn assert_config_error_without_path_content(
    result: Result<Vec<Item>, kaban_ingest::IngestError>,
    root: &Path,
) {
    match result {
        Err(kaban_ingest::IngestError::Config(msg)) => {
            let scratch_name = root.file_name().unwrap().to_str().unwrap();
            assert!(
                !msg.contains(scratch_name) && !msg.contains("secret") && !msg.contains("/tmp"),
                "message carries path content: {msg}"
            );
        }
        other => panic!("expected IngestError::Config, got {other:?}"),
    }
}

fn try_run(cfg: &WalkConfig) -> Result<Vec<Item>, kaban_ingest::IngestError> {
    walk(cfg).map(Iterator::collect)
}

/// Review RD-2: a configured root that is a symlink to a directory outside the workspace is
/// refused before any traversal, whether or not symlinks are followed (`walkdir` would follow a
/// root link regardless, and the files below it are not themselves symlinks). A root link that
/// resolves inside the workspace is refused only while `follow_symlinks` is off.
#[test]
fn a_root_that_is_a_symlink_is_validated_before_traversal() {
    let root = scratch("root-link");
    let outside = scratch("root-link-outside");
    write(&outside, "secret.md", b"# outside the workspace\n");
    write(&root, "inner/real.md", b"# inside\n");
    std::os::unix::fs::symlink(&outside, root.join("escape")).unwrap();
    std::os::unix::fs::symlink(root.join("inner"), root.join("alias")).unwrap();

    let escaping = WalkConfig {
        roots: vec![root.join("escape")],
        ..WalkConfig::new(root.to_path_buf())
    };
    assert_config_error_without_path_content(try_run(&escaping), &root);
    let escaping_following = WalkConfig {
        follow_symlinks: true,
        ..escaping.clone()
    };
    assert_config_error_without_path_content(try_run(&escaping_following), &root);

    let inner_link = WalkConfig {
        roots: vec![root.join("alias")],
        ..WalkConfig::new(root.to_path_buf())
    };
    assert_config_error_without_path_content(try_run(&inner_link), &root);
    let inner_link_following = WalkConfig {
        follow_symlinks: true,
        ..inner_link.clone()
    };
    let items = try_run(&inner_link_following).unwrap();
    assert_eq!(ok_names(&items), vec!["real.md"], "{items:?}");
}

/// Review RD-2: a root written as `../<sibling>` resolves outside the workspace and is refused
/// (instead of being traversed and silently dropped by the lexical prefix check); a `..` that
/// stays inside the workspace is fine.
#[test]
fn a_root_given_as_a_lexical_parent_path_is_refused() {
    let root = scratch("lexical");
    let outside = scratch("lexical-outside");
    write(&outside, "secret.md", b"# outside\n");
    write(&root, "docs/a.md", b"# a\n");

    let sibling = root.join("..").join(outside.file_name().unwrap());
    let escaping = WalkConfig {
        roots: vec![sibling],
        ..WalkConfig::new(root.to_path_buf())
    };
    assert_config_error_without_path_content(try_run(&escaping), &root);

    let inside = WalkConfig {
        roots: vec![root.join("docs").join("..").join("docs")],
        ..WalkConfig::new(root.to_path_buf())
    };
    let items = try_run(&inside).unwrap();
    assert_eq!(ok_names(&items), vec!["a.md"], "{items:?}");
    let a = items[0].as_ref().unwrap();
    assert_eq!(a.abs, root.canonicalize().unwrap().join("docs/a.md"));
}

/// Review RD-2: `Candidate::open_checked` is the only way to read a candidate's bytes. A file
/// swapped for a symlink to `/etc/passwd` between the walk and the open is refused (the open
/// does not follow a final-component link while `follow_symlinks` is off; with it on, the
/// opened file's `(dev, ino)` no longer matches the walk), and not one byte of the target is
/// returned. An unchanged file reads normally, bounded by `max_file_bytes`.
#[test]
fn a_file_replaced_by_an_outside_symlink_between_walk_and_open_is_refused() {
    use std::io::Read;
    for follow_symlinks in [false, true] {
        let root = scratch(&format!("swap-{follow_symlinks}"));
        write(&root, "doc.md", b"# doc\n");
        let cfg = WalkConfig {
            follow_symlinks,
            ..WalkConfig::new(root.to_path_buf())
        };
        let items = run(&cfg);
        assert_eq!(ok_names(&items), vec!["doc.md"]);
        let doc = items[0].as_ref().unwrap();

        let mut text = String::new();
        doc.open_checked()
            .unwrap()
            .read_to_string(&mut text)
            .unwrap();
        assert_eq!(text, "# doc\n", "an unchanged file reads normally");

        fs::remove_file(root.join("doc.md")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("doc.md")).unwrap();
        let mut leaked = String::new();
        let outcome = doc
            .open_checked()
            .and_then(|mut f| f.read_to_string(&mut leaked));
        assert!(
            outcome.is_err(),
            "follow_symlinks={follow_symlinks}: the swapped file must be refused, read {} bytes: {leaked:?}",
            leaked.len()
        );
        assert!(
            !leaked.contains("root:"),
            "follow_symlinks={follow_symlinks}: bytes of the link target were returned"
        );
    }
}

/// Plan slice 6 with D-8: the order is the byte order of the relative paths, not the directory
/// traversal order (`a-c.md` 0x2D sorts before `a/b.md` 0x2F, `B.md` before `a`), skips
/// included, and two runs yield the identical sequence.
#[test]
fn iteration_order_is_byte_sorted_and_identical_across_two_runs() {
    let root = scratch("order");
    for rel in [
        "a/b.md",
        "a-c.md",
        "B.md",
        "z.md",
        "a/a.md",
        "a/.env",
        "m/n/o.txt",
    ] {
        write(&root, rel, b"text\n");
    }
    let cfg = WalkConfig::new(root.to_path_buf());
    let describe = |items: Vec<Item>| -> Vec<String> {
        items
            .into_iter()
            .map(|i| match i {
                Ok(c) => format!("ok:{}", c.abs.strip_prefix(&root).unwrap().display()),
                Err(skip) => format!("skip:{skip:?}"),
            })
            .collect()
    };
    let first = describe(walk(&cfg).unwrap().collect());
    let second = describe(walk(&cfg).unwrap().collect());
    assert_eq!(first, second, "two runs must agree");
    let ok: Vec<&str> = first.iter().filter_map(|s| s.strip_prefix("ok:")).collect();
    assert_eq!(
        ok,
        vec!["B.md", "a-c.md", "a/a.md", "a/b.md", "m/n/o.txt", "z.md"]
    );
    // the skip (`a/.env`, glob 0) sits between `a-c.md` and `a/a.md` in byte order
    assert!(
        first[2].starts_with("skip:") && first[2].ends_with("Denied(\"0\")"),
        "{first:?}"
    );
}

/// DI2.0 (b): `.git` is pruned at the directory level, at any depth. A repository fixture whose
/// `.git` holds 2,000 small objects, a Markdown file (which a file-level rule would have yielded
/// as a candidate), and an object directory that cannot be opened: the walk yields no entry
/// under `.git`, opens no directory under it (an opened unreadable directory would surface as
/// the walker's "walk entry error" warning), and the non-`.git` part is judged exactly as
/// before (`notes.md` a candidate, `.env` glob 0, `secrets/x.txt` glob 4).
#[test]
fn git_directories_are_pruned_before_anything_under_them_is_visited() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("git-pruned");
    write(&root, "notes.md", b"# notes\n");
    write(&root, ".env", b"TOKEN=abc\n");
    write(&root, "secrets/x.txt", b"plain\n");
    write(&root, ".git/HEAD", b"ref: refs/heads/main\n");
    write(&root, ".git/x.md", b"# not a document\n");
    write(&root, "sub/.git/y.md", b"# not a document either\n");
    for i in 0..2000u32 {
        write(
            &root,
            &format!(".git/objects/{:02x}/{i:040x}", i % 16),
            b"x",
        );
    }
    let locked = root.join(".git/objects/zz");
    fs::create_dir_all(&locked).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let logs = common::pool::LogBuf::default();
    let sink = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("warn"))
        .with_writer(move || sink.clone())
        .with_ansi(false)
        .finish();
    let cfg = WalkConfig::new(root.to_path_buf());

    let items = tracing::subscriber::with_default(subscriber, || run(&cfg));
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o755)).unwrap();

    let warnings = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        !warnings.contains("walk entry error"),
        "a directory under .git was opened: {warnings}"
    );
    assert_eq!(ok_names(&items), vec!["notes.md"], "{items:?}");
    let reasons: Vec<&Skip> = skips(&items);
    assert_eq!(
        reasons,
        vec![&Skip::Denied("0".to_owned()), &Skip::Denied("4".to_owned())],
        "only the file rules of the non-.git part may fire: {items:?}"
    );
    assert_eq!(
        items.len(),
        3,
        "nothing under .git is yielded: {} items",
        items.len()
    );
}
