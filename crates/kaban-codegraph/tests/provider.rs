//! Provider slices (stage1-plan CG5.2; cg-join-v1 §4, §11; stage1-spec G1-F8, G1-S7): the
//! `SnapshotProvider` trait over `DiscoverProvider` (exact, nearest ancestor, skipped
//! candidates, the four `NoSnapshot` reasons, the LRU of two), `HeadCache` with injected
//! inputs, `FixedSnapshot`, and the two isolation slices: one repository per provider and the
//! symlinked directory refusal. The RU3-3 directory admission (symlink above `<codegraph>`,
//! a loose `<repo-id>`, a replaced `<repo-id>`) and its OWN-81 (4) refinement: a directory
//! found absent resets the retained identity, a swap in place never does.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/builder.rs"]
mod builder;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use builder::{Snapshot, SymbolSpec, commit, final_path, scratch};
use kaban_codegraph::sqlite::testing::quick_checks_run;
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::{
    CodeGraphReader, DiscoverProvider, HeadCache, HeadInput, IsolationTier, NoSnapshot, Resolved,
    SCHEMA_VERSION, SnapshotCache, SnapshotProvider,
};

fn head(commit: &str, ancestors: &[&str]) -> HeadInput {
    HeadInput {
        commit: commit.to_owned(),
        ancestors: ancestors.iter().map(|a| (*a).to_owned()).collect(),
    }
}

/// A repository directory under a data directory (`<codegraph>/<repo-id>`).
fn repo_dir(data: &Path, repo_id: &str) -> PathBuf {
    let dir = data.join(repo_id);
    std::fs::create_dir(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

/// A damaged file under `commit`'s final name.
fn corrupt_file(dir: &Path, commit: &str) -> PathBuf {
    let path = final_path(dir, commit);
    std::fs::write(&path, b"SQLite format 3\0 damaged").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

fn too_new_file(dir: &Path, commit: &str) -> PathBuf {
    let mut s = Snapshot::new(commit);
    s.set_meta("schema_version", &(SCHEMA_VERSION + 1).to_string());
    s.write_final(dir, commit)
}

/// How many descriptors of this process are open on `path` (one per read-only connection).
/// nextest runs each test in its own process, so no other test's connections are counted.
fn open_fds_to(path: &Path) -> usize {
    let target = std::fs::canonicalize(path).unwrap();
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| std::fs::read_link(e.path()).is_ok_and(|l| l == target))
        .count()
}

/// Resolve through the trait object, as the engine's adapter does.
fn resolve(provider: &dyn SnapshotProvider, head: &HeadInput) -> Result<Resolved, NoSnapshot> {
    provider.resolve("repo", head)
}

/// A `tracing` subscriber that keeps the `snapshot directory refused` events as
/// `class=<class> reset=<bool>` lines (the fields `resolve` traces; `reset` is absent at the
/// CG5.2b base), so a test can tell the two refusals apart the way the serve log does. The
/// default is thread-scoped (`tracing::subscriber::with_default`): each resolve runs under it.
#[derive(Clone, Default)]
struct Refusals(Arc<std::sync::Mutex<Vec<String>>>);

impl Refusals {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    /// Run `f` with this subscriber as the thread's default.
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        tracing::subscriber::with_default(self.clone(), f)
    }
}

impl tracing::Subscriber for Refusals {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}

    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        #[derive(Default)]
        struct Fields {
            message: String,
            class: Option<String>,
            reset: Option<bool>,
        }
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.message = format!("{value:?}");
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "class" {
                    self.class = Some(value.to_owned());
                }
            }
            fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
                if field.name() == "reset" {
                    self.reset = Some(value);
                }
            }
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        if fields.message == "snapshot directory refused" {
            let class = fields.class.unwrap_or_default();
            let reset = fields
                .reset
                .map_or_else(|| "absent".to_owned(), |b| b.to_string());
            self.0
                .lock()
                .unwrap()
                .push(format!("class={class} reset={reset}"));
        }
    }

    fn enter(&self, _: &tracing::span::Id) {}

    fn exit(&self, _: &tracing::span::Id) {}
}

#[test]
fn provider_resolves_exact_then_ancestor() {
    let data = scratch();
    let repo = repo_dir(data.path(), "r1");
    let head_commit = commit(0x10);
    let ancestor = commit(0x11);
    let older = commit(0x12);
    let mut exact = Snapshot::new(&head_commit);
    exact.set_meta("isolation", "container");
    exact.set_meta("policy_version", "pv-3");
    let exact_path = exact.write_final(&repo, &head_commit);
    let ancestor_path = Snapshot::new(&ancestor).write_final(&repo, &ancestor);
    Snapshot::new(&older).write_final(&repo, &older);
    let provider = DiscoverProvider::new(repo.clone());
    assert_eq!(provider.dir(), repo);

    // exact when present: every SD-4 field is filled from `meta`
    let r = resolve(&provider, &head(&head_commit, &[&ancestor, &older])).unwrap();
    assert!(r.r#ref.exact);
    assert_eq!(r.r#ref.commit, head_commit);
    assert_eq!(r.r#ref.isolation_tier, IsolationTier::Container);
    assert_eq!(r.r#ref.schema_version, SCHEMA_VERSION);
    assert_eq!(r.r#ref.policy_version, "pv-3");
    assert_eq!(r.snapshot.path(), exact_path);
    assert_eq!(r.attach.path(), exact_path);
    assert_eq!(r.attach.schema_version(), SCHEMA_VERSION);
    assert!(r.attach.as_sql_param().starts_with("file:"));
    assert!(format!("{r:?}").contains("Resolved"), "Debug on the result");
    drop(r);

    // no exact file: the first ancestor with a snapshot, `exact: false`
    let no_exact = commit(0x13);
    let r = resolve(
        &provider,
        &head(&no_exact, &[&commit(0x14), &ancestor, &older]),
    )
    .unwrap();
    assert!(!r.r#ref.exact);
    assert_eq!(r.r#ref.commit, ancestor);
    assert_eq!(r.snapshot.path(), ancestor_path);
    assert_eq!(r.r#ref.policy_version, "pv1");
    drop(r);

    // a corrupt exact file is skipped, the ancestor chosen
    let corrupt = commit(0x15);
    corrupt_file(&repo, &corrupt);
    let r = resolve(&provider, &head(&corrupt, &[&ancestor])).unwrap();
    assert!(!r.r#ref.exact);
    assert_eq!(r.r#ref.commit, ancestor);
    drop(r);

    // a refused mode (an admission failure) is skipped the same way
    let loose = commit(0x16);
    let loose_path = Snapshot::new(&loose).write_final(&repo, &loose);
    std::fs::set_permissions(&loose_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let r = resolve(&provider, &head(&loose, &[&older])).unwrap();
    assert_eq!(r.r#ref.commit, older);
    assert!(!r.r#ref.exact);
    drop(r);

    // SD-5: the repository identity is the caller's join key, the directory encodes the
    // repository; the string changes nothing here
    let r = provider
        .resolve("another/identity", &head(&head_commit, &[]))
        .unwrap();
    assert_eq!(r.snapshot.path(), exact_path);
    assert!(format!("{provider:?}").contains("DiscoverProvider"));
}

#[test]
fn provider_no_snapshot_reasons() {
    let data = scratch();
    let c = commit(0x20);

    // NoDirectory: no `<repo-id>` directory, or a file in its place
    let missing = DiscoverProvider::new(data.path().join("missing"));
    assert_eq!(
        resolve(&missing, &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    let as_file = data.path().join("as-file");
    std::fs::write(&as_file, b"x").unwrap();
    assert_eq!(
        resolve(&DiscoverProvider::new(as_file), &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );

    // NoFile: the directory exists, no candidate for head or any ancestor
    let repo = repo_dir(data.path(), "r2");
    let provider = DiscoverProvider::new(repo.clone());
    let other = commit(0x21);
    Snapshot::new(&other).write_final(&repo, &other);
    assert_eq!(
        resolve(&provider, &head(&c, &[&commit(0x22)])).map(|_| ()),
        Err(NoSnapshot::NoFile)
    );

    // TooNewOnly: the one candidate is too new; beside a corrupt one it still wins
    let too_new = commit(0x23);
    too_new_file(&repo, &too_new);
    assert_eq!(
        resolve(&provider, &head(&too_new, &[])).map(|_| ()),
        Err(NoSnapshot::TooNewOnly)
    );
    let corrupt = commit(0x24);
    corrupt_file(&repo, &corrupt);
    assert_eq!(
        resolve(&provider, &head(&too_new, &[&corrupt])).map(|_| ()),
        Err(NoSnapshot::TooNewOnly)
    );
    assert_eq!(
        resolve(&provider, &head(&corrupt, &[&too_new])).map(|_| ()),
        Err(NoSnapshot::TooNewOnly)
    );

    // CorruptOnly: every existing candidate was corrupt or refused
    assert_eq!(
        resolve(&provider, &head(&corrupt, &[&commit(0x25)])).map(|_| ()),
        Err(NoSnapshot::CorruptOnly)
    );
    let loose = commit(0x26);
    let loose_path = Snapshot::new(&loose).write_final(&repo, &loose);
    std::fs::set_permissions(&loose_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        resolve(&provider, &head(&loose, &[&corrupt])).map(|_| ()),
        Err(NoSnapshot::CorruptOnly)
    );
}

#[test]
fn provider_head_cache_keyed_by_head_sha() {
    let cache = HeadCache::new();
    assert!(format!("{cache:?}").contains("HeadCache"));
    let calls = AtomicUsize::new(0);
    let calls = &calls;
    let ancestors = |parent: u8| {
        move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, String>(vec![builder::commit(parent)])
        }
    };
    let (c0, c1) = (commit(0x30), commit(0x32));

    // the same sha: one fetch, the same `Arc`, the head is the key plus the fetched ancestors
    let first = cache.get_or_refresh(&c0, ancestors(0x20)).unwrap();
    let again = cache.get_or_refresh(&c0, ancestors(0x21)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(Arc::ptr_eq(&first, &again));
    assert_eq!(*first, head(&c0, &[&commit(0x20)]));

    // a new sha (a same-branch commit, a checkout, anything): a second fetch, the new value
    let refreshed = cache.get_or_refresh(&c1, ancestors(0x30)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    assert_eq!(*refreshed, head(&c1, &[&c0]));
    assert!(!Arc::ptr_eq(&first, &refreshed));

    // a failed fetch leaves the cache unchanged: the previous sha still hits
    let failed = cache.get_or_refresh(&commit(0x33), || {
        Err::<Vec<String>, _>("git failed".to_owned())
    });
    assert_eq!(failed.unwrap_err(), "git failed");
    let still = cache.get_or_refresh(&c1, ancestors(0x34)).unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "no fetch: the last sha is still cached"
    );
    assert!(Arc::ptr_eq(&refreshed, &still));

    // any other sha refetches, an earlier one too (the cache holds one head)
    let back = cache.get_or_refresh(&c0, ancestors(0x20)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    assert_eq!(*back, *first);
    assert!(!Arc::ptr_eq(&first, &back));
}

#[test]
fn provider_lru_two() {
    let data = scratch();
    let repo = repo_dir(data.path(), "r3");
    let commits: Vec<String> = (0x40..0x43u8).map(commit).collect();
    let paths: Vec<PathBuf> = commits
        .iter()
        .map(|c| Snapshot::new(c).write_final(&repo, c))
        .collect();
    let provider = DiscoverProvider::new(repo);
    let before = quick_checks_run();

    // each first open checks once; the lease is request-scoped, the cache keeps the connection
    let r0 = resolve(&provider, &head(&commits[0], &[])).unwrap();
    assert_eq!(quick_checks_run(), before + 1);
    assert_eq!(
        Arc::strong_count(&r0.snapshot),
        2,
        "the cache and the lease"
    );
    drop(r0);
    assert_eq!(open_fds_to(&paths[0]), 1, "the cached connection stays");
    drop(resolve(&provider, &head(&commits[1], &[])).unwrap());
    assert_eq!(quick_checks_run(), before + 2);
    assert_eq!(open_fds_to(&paths[0]), 1);
    assert_eq!(open_fds_to(&paths[1]), 1);

    // the third evicts the least recently used; no lease is retained, so it closes
    drop(resolve(&provider, &head(&commits[2], &[])).unwrap());
    assert_eq!(quick_checks_run(), before + 3);
    assert_eq!(open_fds_to(&paths[0]), 0, "evicted and closed");
    assert_eq!(open_fds_to(&paths[1]), 1);
    assert_eq!(open_fds_to(&paths[2]), 1);
    assert_eq!(SnapshotCache::CAPACITY, 2);

    // the reopen of the first runs no second quick_check (once per process per generation)
    // but is a new connection: the connection counter shows the eviction of the second
    let r0 = resolve(&provider, &head(&commits[0], &[])).unwrap();
    assert_eq!(
        quick_checks_run(),
        before + 3,
        "no second check on the reopen"
    );
    assert_eq!(open_fds_to(&paths[0]), 1);
    assert_eq!(
        open_fds_to(&paths[1]),
        0,
        "the least recently used was evicted"
    );
    assert_eq!(open_fds_to(&paths[2]), 1);
    drop(r0);
    assert_eq!(open_fds_to(&paths[0]), 1);
}

/// RU3-2 (G1-S7; cg-join-v1 §2, §4): a snapshot republished under the same commit name is a
/// new generation. After `remove` and a `write_final` with other rows and another policy
/// version, the next resolve's `snapshot`, `ref` and `attach` all describe the second file;
/// the first lease, still held, keeps reading the first rows.
#[test]
fn provider_same_name_republish_is_new_generation() {
    let data = scratch();
    let repo = repo_dir(data.path(), "r8");
    let c = commit(0x80);
    let mut first = Snapshot::new(&c);
    first.set_meta("policy_version", "pv-first");
    let path = first.write_final(&repo, &c);
    let provider = DiscoverProvider::new(repo.clone());
    let r1 = resolve(&provider, &head(&c, &[])).unwrap();
    assert_eq!(r1.r#ref.policy_version, "pv-first");
    assert_eq!(r1.snapshot.meta().unwrap()["policy_version"], "pv-first");

    // the same name, other rows, another policy version
    std::fs::remove_file(&path).unwrap();
    let mut second = Snapshot::new(&c);
    second.set_meta("policy_version", "pv-second");
    let f = second.file("src/second.go", false);
    second.symbol(SymbolSpec::file("file:src/second.go", "second.go", f));
    assert_eq!(second.write_final(&repo, &c), path);

    let r2 = resolve(&provider, &head(&c, &[])).unwrap();
    assert_eq!(r2.r#ref.policy_version, "pv-second");
    assert_eq!(
        r2.snapshot.meta().unwrap()["policy_version"],
        "pv-second",
        "the lease is the second generation, not the cached first"
    );
    assert!(r2.snapshot.node("file:src/second.go").unwrap().is_some());
    assert!(!Arc::ptr_eq(&r1.snapshot, &r2.snapshot));
    // the attach describes the second file too (opened through the admitted URI)
    let attached = rusqlite::Connection::open_with_flags(
        r2.attach.as_sql_param(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let pv: String = attached
        .query_row(
            "SELECT value FROM meta WHERE key = 'policy_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pv, "pv-second");
    drop(attached);

    // the first lease still reads the first rows
    assert_eq!(r1.snapshot.meta().unwrap()["policy_version"], "pv-first");
    assert!(r1.snapshot.node("file:src/second.go").unwrap().is_none());
    drop(r1);
    drop(r2);
    // the cache keeps one connection for the path: the second generation
    assert_eq!(open_fds_to(&path), 1, "the second file is the cached one");
}

#[test]
fn fixed_snapshot_resolves_any_path() {
    let data = scratch();
    // any directory: no `<repo-id>` layout, a name the attach URI must encode
    let odd = data.path().join("odd dir#1");
    std::fs::create_dir(&odd).unwrap();
    let fixed_commit = commit(0x50);
    let mut snapshot = Snapshot::new(&fixed_commit);
    snapshot.set_meta("isolation", "namespaces");
    snapshot.set_meta("policy_version", "pv-fixed");
    let path = snapshot.write_final(&odd, &fixed_commit);
    let fixed = FixedSnapshot::new(path.clone());
    assert_eq!(fixed.clone(), FixedSnapshot { path: path.clone() });
    assert!(format!("{fixed:?}").contains("FixedSnapshot"));

    // the head is not consulted: exact for any head
    let r = fixed
        .resolve("x", &head(&commit(0x51), &[&commit(0x52)]))
        .unwrap();
    assert!(r.r#ref.exact);
    assert_eq!(r.r#ref.commit, fixed_commit);
    assert_eq!(r.r#ref.isolation_tier, IsolationTier::Namespaces);
    assert_eq!(r.r#ref.policy_version, "pv-fixed");
    assert_eq!(r.r#ref.schema_version, SCHEMA_VERSION);
    assert_eq!(r.snapshot.path(), path);
    assert_eq!(r.attach.path(), path);
    assert!(
        r.attach.as_sql_param().contains("%23"),
        "the `#` is encoded"
    );
    drop(r);
    // no cache: a second resolve is a new connection
    let first = fixed.resolve("x", &head(&fixed_commit, &[])).unwrap();
    let second = fixed.resolve("x", &head(&fixed_commit, &[])).unwrap();
    assert!(!Arc::ptr_eq(&first.snapshot, &second.snapshot));
    drop((first, second));
    assert_eq!(open_fds_to(&path), 0, "nothing retained");

    // the reasons of a file that does not resolve
    assert_eq!(
        FixedSnapshot::new(odd.join(format!("{}.cg.db", commit(0x53))))
            .resolve("x", &head(&fixed_commit, &[]))
            .map(|_| ()),
        Err(NoSnapshot::NoFile)
    );
    let too_new = too_new_file(&odd, &commit(0x54));
    assert_eq!(
        FixedSnapshot::new(too_new)
            .resolve("x", &head(&fixed_commit, &[]))
            .map(|_| ()),
        Err(NoSnapshot::TooNewOnly)
    );
    let corrupt = corrupt_file(&odd, &commit(0x55));
    assert_eq!(
        FixedSnapshot::new(corrupt)
            .resolve("x", &head(&fixed_commit, &[]))
            .map(|_| ()),
        Err(NoSnapshot::CorruptOnly)
    );
    let temp_name = odd.join("not-a-final-name.cg.db");
    std::fs::write(&temp_name, b"x").unwrap();
    std::fs::set_permissions(&temp_name, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(
        FixedSnapshot::new(temp_name)
            .resolve("x", &head(&fixed_commit, &[]))
            .map(|_| ()),
        Err(NoSnapshot::CorruptOnly)
    );
}

/// G1-S7 `code_tools_single_repo`: two `<repo-id>` directories in the data directory; the
/// provider of one never opens a file of the other. The canary is a valid snapshot for the same
/// commit in the other directory: its open would be observable as a `quick_check`.
#[test]
fn code_tools_single_repo() {
    let data = scratch();
    let mine = repo_dir(data.path(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    let other = repo_dir(data.path(), "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    let c = commit(0x60);
    let canary = Snapshot::new(&c).write_final(&other, &c);
    let provider = DiscoverProvider::new(mine.clone());
    let before = quick_checks_run();

    // nothing for this commit in my directory: NoFile, the canary untouched
    assert_eq!(
        resolve(&provider, &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoFile)
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "the other repository's file was not opened"
    );
    assert_eq!(open_fds_to(&canary), 0);

    // my own snapshot for the commit resolves from my directory only, under any identity
    // string, the memory side's `workspace: "*"` included
    let mine_path = Snapshot::new(&c).write_final(&mine, &c);
    for identity in ["repo", "*", "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"] {
        let r = provider.resolve(identity, &head(&c, &[])).unwrap();
        assert_eq!(r.snapshot.path(), mine_path);
        assert!(r.snapshot.path().starts_with(&mine));
        assert_eq!(r.attach.path(), mine_path);
    }
    assert_eq!(quick_checks_run(), before + 1, "one check: my file, once");
    assert_eq!(open_fds_to(&canary), 0, "the canary was never opened");
}

/// G1-S7 (RS-5): symlinked components are refused on the read path. A `<repo-id>` directory
/// that is a symbolic link, or whose parent is one, is `NoDirectory` and nothing behind it is
/// opened (`files_modes_and_symlink_refusal` regression at the directory level).
#[test]
fn provider_refuses_symlinked_directory() {
    let data = scratch();
    let c = commit(0x70);
    let real = repo_dir(data.path(), "real");
    let real_file = Snapshot::new(&c).write_final(&real, &c);
    let before = quick_checks_run();

    // the directory itself is a symlink
    let link = data.path().join("link-repo");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        resolve(&DiscoverProvider::new(link), &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "nothing behind the link was opened"
    );
    assert_eq!(open_fds_to(&real_file), 0);

    // the parent (`codegraph`) is a symlink
    let codegraph = data.path().join("codegraph");
    std::fs::create_dir(&codegraph).unwrap();
    let rid = repo_dir(&codegraph, "rid");
    let rid_file = Snapshot::new(&c).write_final(&rid, &c);
    let link_parent = data.path().join("codegraph-link");
    std::os::unix::fs::symlink(&codegraph, &link_parent).unwrap();
    assert_eq!(
        resolve(
            &DiscoverProvider::new(link_parent.join("rid")),
            &head(&c, &[])
        )
        .map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(quick_checks_run(), before);
    assert_eq!(open_fds_to(&rid_file), 0);

    // the real directories still resolve
    let r = resolve(&DiscoverProvider::new(real), &head(&c, &[])).unwrap();
    assert_eq!(r.snapshot.path(), real_file);
    drop(r);
    let r = resolve(&DiscoverProvider::new(rid), &head(&c, &[])).unwrap();
    assert_eq!(r.snapshot.path(), rid_file);
    assert_eq!(quick_checks_run(), before + 2);
}

/// RU3-3 (G1-S7, RS-5): the whole directory chain is walked without following a link. A
/// symlink above `<codegraph>` (the grandparent of `<repo-id>`) is `NoDirectory`; nothing
/// behind it is opened.
#[test]
fn provider_refuses_symlink_above_codegraph() {
    let data = scratch();
    let c = commit(0x90);
    let real = data.path().join("real");
    let codegraph = real.join("codegraph");
    std::fs::create_dir_all(&codegraph).unwrap();
    let rid = repo_dir(&codegraph, "rid");
    let file = Snapshot::new(&c).write_final(&rid, &c);
    let link = data.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let before = quick_checks_run();

    let provider = DiscoverProvider::new(link.join("codegraph").join("rid"));
    assert_eq!(
        resolve(&provider, &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "nothing behind the link was opened"
    );
    assert_eq!(open_fds_to(&file), 0);
}

/// RU3-3 (G1-S7): `<repo-id>` must be private to the caller: a group-writable directory is
/// `NoDirectory`, nothing in it is opened.
#[test]
fn provider_refuses_group_writable_repo_dir() {
    let data = scratch();
    let c = commit(0x91);
    let repo = repo_dir(data.path(), "loose");
    let file = Snapshot::new(&c).write_final(&repo, &c);
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o770)).unwrap();
    let before = quick_checks_run();

    let provider = DiscoverProvider::new(repo.clone());
    assert_eq!(
        resolve(&provider, &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "nothing in the loose directory was opened"
    );
    assert_eq!(open_fds_to(&file), 0);

    // back to 0700: the same provider resolves
    std::fs::set_permissions(&repo, std::fs::Permissions::from_mode(0o700)).unwrap();
    let r = resolve(&provider, &head(&c, &[])).unwrap();
    assert_eq!(r.snapshot.path(), file);
    assert_eq!(quick_checks_run(), before + 1);
}

/// RU3-3 (G1-S7): the directory identities are retained and re-checked on every resolve. A
/// `<repo-id>` removed and recreated between two resolves, its absence never seen by the
/// provider, is another directory object: `NoDirectory`, nothing in the new one is opened
/// (the absence the provider does see is `provider_recreated_directory_after_absence_is_admitted`).
#[test]
fn provider_refuses_replaced_repo_dir() {
    let data = scratch();
    let c = commit(0x92);
    let repo = repo_dir(data.path(), "replaced");
    let first = Snapshot::new(&c).write_final(&repo, &c);
    let provider = DiscoverProvider::new(repo.clone());
    let r = resolve(&provider, &head(&c, &[])).unwrap();
    assert_eq!(r.snapshot.path(), first);
    drop(r);
    let before = quick_checks_run();

    std::fs::remove_dir_all(&repo).unwrap();
    let again = repo_dir(data.path(), "replaced");
    assert_eq!(again, repo);
    let mut other = Snapshot::new(&c);
    other.set_meta("policy_version", "pv-replaced");
    let second = other.write_final(&again, &c);
    assert_eq!(
        resolve(&provider, &head(&c, &[])).map(|_| ()),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "nothing in the recreated directory was opened"
    );
    assert_eq!(open_fds_to(&second), 0);
}

/// OWN-81 (4) (G1-S7; the RU3-3 residual): a `<repo-id>` found absent on a later resolve
/// (`purge-derived`, gc) is `NoDirectory` traced `not_found` and clears the retained identity,
/// so the directory `kaban-index` recreates at the same path is admitted on the next resolve
/// without a restart. An absence the provider itself observed is what tells a legitimate
/// re-index from a swap (`provider_replaced_directory_without_absence_refused`).
#[test]
fn provider_recreated_directory_after_absence_is_admitted() {
    let data = scratch();
    let c = commit(0x93);
    let repo = repo_dir(data.path(), "recreated");
    let first = Snapshot::new(&c).write_final(&repo, &c);
    let provider = DiscoverProvider::new(repo.clone());
    let traced = Refusals::default();
    let r = traced.run(|| resolve(&provider, &head(&c, &[]))).unwrap();
    assert_eq!(r.snapshot.path(), first);
    drop(r);

    // `purge-derived` ran: the directory is gone when the provider looks
    std::fs::remove_dir_all(&repo).unwrap();
    assert_eq!(
        traced.run(|| resolve(&provider, &head(&c, &[])).map(|_| ())),
        Err(NoSnapshot::NoDirectory)
    );
    assert_eq!(
        traced.lines(),
        ["class=not_found reset=true"],
        "the absence is traced as the directory's own, and as the reset it is"
    );

    // re-index: a fresh `0700` directory and a fresh snapshot file at the same path
    let again = repo_dir(data.path(), "recreated");
    assert_eq!(again, repo);
    let mut other = Snapshot::new(&c);
    other.set_meta("policy_version", "pv-reindexed");
    let second = other.write_final(&again, &c);
    let before = quick_checks_run();
    let r = traced
        .run(|| resolve(&provider, &head(&c, &[])))
        .unwrap_or_else(|e| {
            panic!(
                "the recreated directory is admitted, got {e:?}; traced {:?}",
                traced.lines()
            )
        });
    assert_eq!(r.snapshot.path(), second);
    assert_eq!(r.r#ref.policy_version, "pv-reindexed");
    assert_eq!(quick_checks_run(), before + 1, "the new file, opened once");
    drop(r);

    // the recreated directory is now the retained one: a further resolve is admitted too
    let r = traced.run(|| resolve(&provider, &head(&c, &[]))).unwrap();
    assert_eq!(r.r#ref.policy_version, "pv-reindexed");
    drop(r);
    assert_eq!(traced.lines().len(), 1, "no refusal after the re-index");
}

/// OWN-81 (4), the invariant that must not weaken (RU3-3, G1-S7): a `<repo-id>` swapped in
/// place between two successful resolves, never absent from the provider's point of view, is
/// another directory object: `NoDirectory` traced `directory_replaced`, and it stays so on
/// repeat (no reset); nothing in the swapped-in directory is opened.
#[test]
fn provider_replaced_directory_without_absence_refused() {
    let data = scratch();
    let c = commit(0x94);
    let repo = repo_dir(data.path(), "swapped");
    let first = Snapshot::new(&c).write_final(&repo, &c);
    let provider = DiscoverProvider::new(repo.clone());
    let traced = Refusals::default();
    let r = traced.run(|| resolve(&provider, &head(&c, &[]))).unwrap();
    assert_eq!(r.snapshot.path(), first);
    drop(r);

    // a sibling `0700` directory with its own snapshot is renamed onto the path: `rename(2)`
    // over a directory is atomic, so the path is never absent (the target must be empty for
    // the rename, hence the file removal first; the directory object stays until the swap)
    let sibling = repo_dir(data.path(), "sibling");
    let mut other = Snapshot::new(&c);
    other.set_meta("policy_version", "pv-swapped");
    other.write_final(&sibling, &c);
    std::fs::remove_file(&first).unwrap();
    std::fs::rename(&sibling, &repo).unwrap();
    let swapped_in = final_path(&repo, &c);
    assert!(
        swapped_in.is_file(),
        "the swapped-in snapshot sits at the path"
    );
    let before = quick_checks_run();

    for attempt in 1..=2 {
        assert_eq!(
            traced.run(|| resolve(&provider, &head(&c, &[])).map(|_| ())),
            Err(NoSnapshot::NoDirectory),
            "attempt {attempt}: traced {:?}",
            traced.lines()
        );
    }
    assert_eq!(
        traced.lines(),
        [
            "class=directory_replaced reset=false",
            "class=directory_replaced reset=false"
        ],
        "a swap without an observed absence is `directory_replaced` on both attempts, never a reset"
    );
    assert_eq!(
        quick_checks_run(),
        before,
        "nothing in the swapped-in directory was opened"
    );
    assert_eq!(open_fds_to(&swapped_in), 0);
}
