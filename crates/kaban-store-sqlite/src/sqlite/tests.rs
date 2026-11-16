//! Unit tests of [`SqliteStore`](super::SqliteStore) (moved from `sqlite.rs`, MT-2).

use super::*;
use kaban_core::Op;

/// Legacy tests passed the policy version string; the write context now carries it.
fn ctx() -> WriteCtx {
    WriteCtx::new(kaban_core::WorkspaceId::default(), kaban_core::Actor::Agent)
}

fn nid(id: &str) -> NodeId {
    NodeId::parse(id).unwrap()
}
fn mk(kind: &str, text: &str) -> NewNode {
    NewNode {
        kind: kind.into(),
        text: text.into(),
        ..Default::default()
    }
}

#[test]
fn remember_link_recall_roundtrip() {
    let s = SqliteStore::open_memory().unwrap();
    let f = match s
        .remember(
            mk(
                "Failure",
                "cargo build failed: linker could not find libssl",
            ),
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!(),
    };
    let fix = match s
        .remember(
            mk(
                "Improvement",
                "install pkg-config and libssl-dev before building",
            ),
            &[LinkReq {
                rel: "FIXED_BY".into(),
                from: Some(nid(&f.id)),
                to: None,
                confidence: Some(0.9),
            }],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!(),
    };
    // atomic links: a bad link must roll back the node
    assert!(
        s.remember(
            mk("Idea", "rolled back"),
            &[LinkReq {
                rel: "NOPE".into(),
                to: Some(nid(&f.id)),
                ..Default::default()
            }],
            &ctx()
        )
        .is_err()
    );
    assert!(
        s.recall(&RecallQuery {
            query: "rolled back".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap()
        .is_empty()
    );
    // dedup
    assert!(matches!(
        s.remember(
            mk(
                "Failure",
                "cargo build failed: linker could not find libssl"
            ),
            &[],
            &ctx()
        )
        .unwrap(),
        RememberOutcome::Existing { .. }
    ));
    let hits = s
        .recall(&RecallQuery {
            query: "libssl linker".into(),
            limit: 5,
            hops: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits[0].node.id, f.id);
    assert!(
        hits.iter().any(|h| h.node.id == fix.id),
        "graph expansion should pull in the fix: {hits:?}"
    );
    // an undeclared kind is refused (PC-14: no discovered kinds); an unknown rel is rejected
    // until registered
    assert!(matches!(
        s.remember(mk("Rumour", "x"), &[], &ctx()),
        Err(StoreError::UnknownKind(_))
    ));
    assert!(matches!(
        s.schema_extend(TypeDef {
            kind: "label".into(),
            name: "Fact".into(),
            ns: None,
            description: "x".into(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: true,
            props_schema: None,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        }),
        Err(StoreError::Builtin(_))
    ));
    assert!(matches!(
        s.link(&nid(&f.id), &nid(&fix.id), "BLOCKED_BY", None, None, &ctx()),
        Err(StoreError::UnknownRel(_))
    ));
    s.schema_extend(TypeDef {
        kind: "rel".into(),
        name: "BLOCKED_BY".into(),
        ns: None,
        description: "blocked".into(),
        from_kinds: vec!["Failure".into()],
        to_kinds: vec![],
        directed: true,
        props_schema: None,
        builtin: false,
        declaration: kaban_core::TypeDeclaration::default(),
    })
    .unwrap();
    s.link(&nid(&f.id), &nid(&fix.id), "BLOCKED_BY", None, None, &ctx())
        .unwrap();
    assert!(matches!(
        s.link(&nid(&fix.id), &nid(&f.id), "BLOCKED_BY", None, None, &ctx()),
        Err(StoreError::EndpointNotAllowed { .. })
    ));
    assert!(
        s.schema_extend(TypeDef {
            kind: "rel".into(),
            name: "bad-name".into(),
            ns: None,
            description: "x".into(),
            from_kinds: vec![],
            to_kinds: vec![],
            directed: true,
            props_schema: None,
            builtin: false,
            declaration: kaban_core::TypeDeclaration::default(),
        })
        .is_err()
    );
    let t = s
        .traverse(&TraverseQuery {
            start_ids: vec![nid(&f.id)],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            limit: 10,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(t.nodes.len(), 2);
    assert_eq!(t.edges.len(), 2);
    s.forget(&NodeId::parse(&fix.id).unwrap(), ForgetMode::Purge, &ctx())
        .unwrap();
    assert!(s.get(&nid(&fix.id), false).unwrap().is_none());
    let hits = s
        .recall(&RecallQuery {
            query: "pkg-config".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert!(hits.is_empty(), "purged node must leave FTS too");
}

#[test]
fn filters_apply_before_limit_and_supersedes_archives_the_target() {
    let s = SqliteStore::open_memory().unwrap();
    for i in 0..60 {
        s.remember(
            NewNode {
                workspace_id: Some("ws-a".into()),
                ..mk("Fact", &format!("deploy pipeline note {i}"))
            },
            &[],
            &ctx(),
        )
        .unwrap();
    }
    let t = match s
        .remember(
            NewNode {
                workspace_id: Some("ws-b".into()),
                ..mk("Decision", "deploy pipeline uses blue green")
            },
            &[],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!(),
    };
    let hits = s
        .recall(&RecallQuery {
            query: "deploy pipeline".into(),
            workspace_id: Some("ws-b".into()),
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].node.id, t.id);
    let hits = s
        .recall(&RecallQuery {
            query: "deploy pipeline".into(),
            kinds: vec!["Decision".into()],
            limit: 5,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(hits.len(), 1);
    let newer = match s
        .remember(
            NewNode {
                workspace_id: Some("ws-b".into()),
                ..mk("Decision", "deploy pipeline uses canary now")
            },
            &[LinkReq {
                rel: "SUPERSEDES".into(),
                to: Some(nid(&t.id)),
                ..Default::default()
            }],
            &ctx(),
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!(),
    };
    // the superseded Decision is archived (by lifecycle; governance states are gone)
    let old = s.get(&nid(&t.id), false).unwrap().unwrap();
    assert_eq!(old.lifecycle, Lifecycle::Archived);
    assert_eq!(old.rev, 2, "rev bumped by the archive");
    assert!(old.valid_to.is_some());
    assert!(
        s.recall(&RecallQuery {
            query: "blue green".into(),
            limit: 5,
            ..Default::default()
        })
        .unwrap()
        .is_empty(),
        "archived rows are not in the text index"
    );
    // and never seed a recall even on request: they are reached through expansion only
    assert!(
        s.recall(&RecallQuery {
            query: "blue green".into(),
            limit: 5,
            include_archived: true,
            ..Default::default()
        })
        .unwrap()
        .is_empty()
    );
    let via_graph = s
        .recall(&RecallQuery {
            query: "canary now".into(),
            limit: 5,
            hops: 1,
            include_archived: true,
            ..Default::default()
        })
        .unwrap();
    assert!(
        via_graph
            .iter()
            .any(|h| h.node.id == t.id && h.why.iter().any(|w| w == "hop1:SUPERSEDES")),
        "superseded record reachable over the SUPERSEDES edge on request: {via_graph:?}"
    );
    assert!(
        !s.recall(&RecallQuery {
            query: "canary now".into(),
            limit: 5,
            hops: 1,
            ..Default::default()
        })
        .unwrap()
        .iter()
        .any(|h| h.node.id == t.id),
        "and hidden by default"
    );
    s.forget(
        &NodeId::parse(&newer.id).unwrap(),
        ForgetMode::Invalidate,
        &ctx(),
    )
    .unwrap();
    assert!(matches!(
        s.remember(
            NewNode {
                workspace_id: Some("ws-b".into()),
                ..mk("Decision", "deploy pipeline uses canary now")
            },
            &[],
            &ctx()
        )
        .unwrap(),
        RememberOutcome::Created(_)
    ));
}

/// DI1.6 `list_nodes`: active rows of the kind and workspace only, ordered by id, the page
/// cap at 500 and `offset` honoured.
#[test]
fn list_nodes_pages_active_rows_of_one_workspace_in_id_order() {
    // Arrange: three Documents in `ws-a` (one tombstoned later), one in `ws-b`, one Section
    let s = SqliteStore::open_memory().unwrap();
    let ws_a = kaban_core::WorkspaceId::new("ws-a");
    let ws_b = kaban_core::WorkspaceId::new("ws-b");
    let ids: Vec<NodeId> = (0..3)
        .map(|_| NodeId::parse(&kaban_core::new_id()).unwrap())
        .collect();
    let other = NodeId::parse(&kaban_core::new_id()).unwrap();
    let section = NodeId::parse(&kaban_core::new_id()).unwrap();
    let batch = |ws: &kaban_core::WorkspaceId, ops: Vec<Op>| WriteBatch::new(ws.clone(), ops);
    let ctx_a = WriteCtx::new(ws_a.clone(), kaban_core::Actor::Agent);
    let ctx_b = WriteCtx::new(ws_b.clone(), kaban_core::Actor::Agent);
    let mut ops: Vec<Op> = ids
        .iter()
        .map(|id| Op::UpsertNode {
            id: id.clone(),
            node: mk("Document", "d"),
        })
        .collect();
    ops.push(Op::UpsertNode {
        id: section.clone(),
        node: mk("Section", "s"),
    });
    s.apply_batch(batch(&ws_a, ops), &ctx_a).unwrap();
    s.apply_batch(
        batch(
            &ws_b,
            vec![Op::UpsertNode {
                id: other.clone(),
                node: mk("Document", "d"),
            }],
        ),
        &ctx_b,
    )
    .unwrap();
    s.apply_batch(
        batch(
            &ws_a,
            vec![Op::Tombstone {
                id: ids[1].clone(),
                reason: "test".into(),
            }],
        ),
        &ctx_a,
    )
    .unwrap();

    // Act
    let all = s.list_nodes("Document", "ws-a", 0, 1000).unwrap();
    let page = s.list_nodes("Document", "ws-a", 1, 1).unwrap();
    let past = s.list_nodes("Document", "ws-a", 5, 10).unwrap();
    let b = s.list_nodes("Document", "ws-b", 0, 10).unwrap();

    // Assert
    let mut expected = vec![ids[0].to_string(), ids[2].to_string()];
    expected.sort();
    let got: Vec<String> = all.iter().map(|n| n.id.clone()).collect();
    assert_eq!(got, expected, "active Documents of ws-a in id order");
    assert_eq!(page.len(), 1);
    assert_eq!(page[0].id, expected[1]);
    assert!(past.is_empty());
    assert_eq!(b.len(), 1);
    assert_eq!(b[0].id, other.to_string());
    assert!(s.list_nodes("Section", "ws-b", 0, 10).unwrap().is_empty());
}

/// SS-23's first consumer: the hygiene's claim under a capability const that says no physical
/// cleanup is incomplete whatever the hygiene reported (never `physically_cleaned`, the reason
/// before the residue); under this store's const the hygiene's own word stands (SS-03).
#[test]
fn a_purge_report_without_physical_cleanup_is_incomplete_whatever_the_hygiene_says() {
    let unable = kaban_core::StoreCapabilities {
        physical_cleanup: false,
        ..SqliteStore::CAPABILITIES
    };
    let report = SqliteStore::hygiene_report(unable, MaintenanceOutcome::Ok);
    assert!(!report.physically_cleaned);
    assert_eq!(
        report.cleanup.as_deref(),
        Some("physical cleanup is not supported by this store; free-page residue until VACUUM")
    );

    let report = SqliteStore::hygiene_report(SqliteStore::CAPABILITIES, MaintenanceOutcome::Busy);
    assert!(!report.physically_cleaned);
    assert_eq!(
        report.cleanup.as_deref(),
        Some("busy; free-page residue until VACUUM")
    );
}

/// Private scratch directory (database and keys), removed on drop.
struct Scratch(std::path::PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("kaban-v14-open-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }
    fn db(&self) -> std::path::PathBuf {
        self.0.join("kaban.db")
    }
    fn keys(&self) -> std::path::PathBuf {
        self.0.join("keys")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Spec 11 CMP-01/KER-05: creating a project ends with its version in the main database file
/// (a reader of the main file alone, as the refusal probe is, sees v14 while the store is
/// still open), and the file was created in incremental auto-vacuum mode.
#[test]
fn a_created_project_holds_its_version_in_the_main_file() {
    let d = Scratch::new("main-file");
    let store = SqliteStore::open(&d.db(), &d.keys()).unwrap();
    let uri = format!("file:{}?mode=ro&immutable=1", d.db().display());
    let probe = Connection::open_with_flags(
        &uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .unwrap();
    let version: String = probe
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(version, crate::LATEST_SCHEMA_VERSION.to_string());
    let auto_vacuum: i64 = probe
        .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
        .unwrap();
    assert_eq!(auto_vacuum, 2, "incremental");
    drop(store);
}

/// The per-database key is created with a new project and required of an existing one: a
/// reopen signs with the same key, and a project whose key was lost is refused rather than
/// given a fresh key (that would silently change every keyed identity it stores).
#[test]
fn an_existing_project_requires_its_key_and_never_gets_a_fresh_one() {
    let d = Scratch::new("key");
    let (db_id, signed) = {
        let store = SqliteStore::open(&d.db(), &d.keys()).unwrap();
        (store.db_id().to_owned(), store.signer().hmac_hex(b"probe"))
    };
    let key = kaban_policy::key_path(&d.keys(), &db_id);
    assert!(key.is_file(), "a new project creates its key");
    {
        let store = SqliteStore::open(&d.db(), &d.keys()).unwrap();
        assert_eq!(
            store.signer().hmac_hex(b"probe"),
            signed,
            "same key on reopen"
        );
    }
    std::fs::remove_file(&key).unwrap();
    let err = SqliteStore::open(&d.db(), &d.keys()).unwrap_err();
    assert!(matches!(err, StoreError::Audit(_)), "{err:?}");
    assert!(!key.exists(), "no fresh key for an existing project");
}

/// Every entry of `dir` by name with its bytes (a directory as `None`): the probe's
/// "nothing created, nothing written" oracle.
fn dir_bytes(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = if entry.file_type().unwrap().is_dir() {
                None
            } else {
                Some(std::fs::read(entry.path()).unwrap())
            };
            (entry.file_name().into_string().unwrap(), bytes)
        })
        .collect()
}

/// Copy `kaban.db` and `kaban.db-wal` of `from` into `to` while `from`'s writer is still open:
/// the files a killed writer leaves (no `-shm`, the log not checkpointed).
fn copy_as_killed(from: &Scratch, to: &Scratch) {
    for name in ["kaban.db", "kaban.db-wal"] {
        std::fs::copy(from.0.join(name), to.0.join(name)).unwrap();
    }
}

/// Spec 11 CMP-01: a main file without a kaban schema beside a non-empty `-wal` (an unknown
/// writer's log) is refused as no kaban project, by the ordinary, diagnostic and hook opens,
/// and neither file is replayed, written or joined by a `-shm` or `.owner`. Both shapes: a
/// main file holding another program's table, and an empty main file beside the log.
#[test]
fn a_foreign_file_beside_a_non_empty_wal_is_refused_untouched() {
    for (case, foreign_main) in [("empty-main", false), ("foreign", true)] {
        let writer = Scratch::new(&format!("foreign-writer-{case}"));
        let raw = Connection::open(writer.db()).unwrap();
        if foreign_main {
            raw.execute_batch("CREATE TABLE other(x); INSERT INTO other VALUES (1);")
                .unwrap();
        }
        raw.execute_batch(
            "PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;
             CREATE TABLE logged(y); INSERT INTO logged VALUES (2);",
        )
        .unwrap();
        let d = Scratch::new(&format!("foreign-{case}"));
        copy_as_killed(&writer, &d);
        drop(raw);
        if !foreign_main {
            // the WAL conversion wrote the header page; the log alone is left
            std::fs::File::create(d.db()).unwrap();
        }
        let before = dir_bytes(&d.0);
        assert!(
            before["kaban.db-wal"]
                .as_ref()
                .is_some_and(|wal| !wal.is_empty()),
            "{case}: the log is not empty"
        );
        assert_eq!(
            before["kaban.db"].as_ref().is_some_and(Vec::is_empty),
            !foreign_main,
            "{case}: the main file's shape"
        );
        let actor = ActorId::default();
        let refusals = [
            SqliteStore::open(&d.db(), &d.keys()).map(drop),
            SqliteStore::open_diagnostic(&d.db(), &d.keys(), &actor).map(drop),
            SqliteStore::open_for_hook(&d.db(), &d.keys(), &actor).map(drop),
        ];
        for refused in refusals {
            let err = refused.unwrap_err();
            assert!(
                matches!(
                    err,
                    StoreError::NotAKabanProject {
                        supported: crate::LATEST_SCHEMA_VERSION
                    }
                ),
                "{case}: {err:?}"
            );
            assert!(
                err.to_string().contains("not a kaban v14 project"),
                "{case}: {err}"
            );
        }
        assert_eq!(
            dir_bytes(&d.0),
            before,
            "{case}: nothing created or written"
        );
    }
}

/// The probe's fresh branch (spec 11 CMP-01): an empty main file, alone or with an empty
/// `-wal`, is a fresh project; the open creates the v14 schema in it.
#[test]
fn an_empty_main_file_is_a_fresh_project() {
    for (case, empty_wal) in [("alone", false), ("empty-wal", true)] {
        let d = Scratch::new(&format!("empty-main-{case}"));
        std::fs::File::create(d.db()).unwrap();
        if empty_wal {
            std::fs::File::create(d.0.join("kaban.db-wal")).unwrap();
        }
        let store = SqliteStore::open(&d.db(), &d.keys()).unwrap();
        assert_eq!(
            (store.migration().from, store.migration().to),
            (0, crate::LATEST_SCHEMA_VERSION),
            "{case}: created at the latest version"
        );
    }
}

/// The probe's normal branch (spec 11 CMP-01): a v14 project left by a killed writer (its
/// last write only in the `-wal`) opens normally and recovers that write.
#[test]
fn a_v14_project_with_a_leftover_wal_opens_and_recovers() {
    let writer = Scratch::new("v14-killed-writer");
    let store = SqliteStore::open(&writer.db(), &writer.keys()).unwrap();
    let node = match store
        .remember(mk("Fact", "written just before the crash"), &[], &ctx())
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { .. } => panic!("a fresh project has no such node"),
    };
    let d = Scratch::new("v14-leftover-wal");
    copy_as_killed(&writer, &d);
    drop(store);
    assert!(
        std::fs::metadata(d.0.join("kaban.db-wal")).unwrap().len() > 0,
        "the write is in the log"
    );
    let reopened = SqliteStore::open(&d.db(), &writer.keys()).unwrap();
    assert_eq!(
        reopened.migration().from,
        crate::LATEST_SCHEMA_VERSION,
        "opened, not created"
    );
    let recovered = reopened.get(&nid(&node.id), false).unwrap();
    assert!(recovered.is_some(), "the logged write is recovered");
}

/// A created and closed v14 project: its main file holds the whole schema, no log beside it.
fn closed_v14_project(name: &str) -> Scratch {
    let d = Scratch::new(name);
    drop(SqliteStore::open(&d.db(), &d.keys()).unwrap());
    d
}

/// Count the probe's reads on this thread from now on; the first read also sends on
/// `first`, if given, once its outcome is known.
fn count_probe_reads(
    first: Option<std::sync::mpsc::Sender<()>>,
) -> std::rc::Rc<std::cell::Cell<usize>> {
    let reads = std::rc::Rc::new(std::cell::Cell::new(0));
    let counted = std::rc::Rc::clone(&reads);
    crate::connection::PROBE_ATTEMPT.with(|seam| {
        *seam.borrow_mut() = Some(Box::new(move || {
            counted.set(counted.get() + 1);
            if counted.get() == 1
                && let Some(first) = &first
            {
                let _ = first.send(());
            }
        }));
    });
    reads
}

/// Stop counting the probe's reads on this thread.
fn stop_counting_probe_reads() {
    crate::connection::PROBE_ATTEMPT.with(|seam| *seam.borrow_mut() = None);
}

/// Another process finishing its creation of `to` once the probe's first read has seen the
/// pending shape (`first_read`): the v14 main file of `from` is renamed into place, then the
/// log is truncated (the creator's TRUNCATE checkpoint).
fn resolve_into_v14_after(
    from: &Scratch,
    to: &Scratch,
    first_read: std::sync::mpsc::Receiver<()>,
) -> std::thread::JoinHandle<()> {
    let (main, staged, wal) = (from.db(), to.0.join("staged.db"), to.0.join("kaban.db-wal"));
    let target = to.db();
    std::thread::spawn(move || {
        first_read
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("the probe read the pending shape");
        std::fs::copy(main, &staged).unwrap();
        std::fs::rename(&staged, target).unwrap();
        if wal.exists() {
            std::fs::OpenOptions::new()
                .write(true)
                .open(wal)
                .unwrap()
                .set_len(0)
                .unwrap();
        }
    })
}

/// Spec 11 CMP-01, the pending creator: a main file without a kaban schema beside a non-empty
/// `-wal` may be another process's creation between its DDL and its TRUNCATE checkpoint. The
/// probe reads it again inside its retry window, so once the creator's v14 main file is in
/// place the open succeeds on that project (opened, not created).
#[test]
fn a_pending_creation_beside_its_log_opens_once_the_main_file_holds_v14() {
    let project = closed_v14_project("pending-source");
    let d = Scratch::new("pending-creation");
    std::fs::File::create(d.db()).unwrap();
    // the log's content is never read by the probe; these bytes are no valid WAL, so nothing
    // could be replayed from them
    std::fs::write(
        d.0.join("kaban.db-wal"),
        b"a creator's log, not yet checkpointed",
    )
    .unwrap();
    let (first, first_read) = std::sync::mpsc::channel();
    let creator = resolve_into_v14_after(&project, &d, first_read);
    let reads = count_probe_reads(Some(first));
    let store = SqliteStore::open(&d.db(), &project.keys()).unwrap();
    stop_counting_probe_reads();
    creator.join().unwrap();
    assert!(
        reads.get() > 1,
        "the first read saw the pending shape and was retried ({} reads)",
        reads.get()
    );
    assert_eq!(
        *store.migration(),
        MigrationReport::unchanged(crate::LATEST_SCHEMA_VERSION),
        "the creator's project, opened"
    );
}

/// Spec 11 CMP-01, a torn read: an immutable read of a main file under another process's
/// checkpoint may fail as "not a database" (or busy, I/O, corrupt). The probe reads it again
/// inside its retry window, so once the main file is whole the open succeeds.
#[test]
fn a_main_file_unreadable_for_a_moment_opens_once_it_reads_as_v14() {
    let project = closed_v14_project("torn-source");
    let d = Scratch::new("torn-read");
    std::fs::write(d.db(), vec![0xA5_u8; 4096]).unwrap();
    let (first, first_read) = std::sync::mpsc::channel();
    let creator = resolve_into_v14_after(&project, &d, first_read);
    let reads = count_probe_reads(Some(first));
    let store = SqliteStore::open(&d.db(), &project.keys()).unwrap();
    stop_counting_probe_reads();
    creator.join().unwrap();
    assert!(
        reads.get() > 1,
        "the first read failed and was retried ({} reads)",
        reads.get()
    );
    assert_eq!(
        *store.migration(),
        MigrationReport::unchanged(crate::LATEST_SCHEMA_VERSION),
        "the whole main file, opened"
    );
}

/// Spec 11 CMP-01, the static shapes: a foreign main file beside a non-empty `-wal` that no
/// creator ever resolves is still refused as no kaban project, and a main file that never
/// reads as a database still fails, each after more than one read and not before the retry
/// window has passed (a lower bound only), with nothing created or written.
#[test]
fn a_shape_that_never_resolves_is_refused_after_the_retry_window() {
    let writer = Scratch::new("static-foreign-writer");
    let raw = Connection::open(writer.db()).unwrap();
    raw.execute_batch(
        "CREATE TABLE other(x); INSERT INTO other VALUES (1);
         PRAGMA journal_mode = WAL; PRAGMA wal_autocheckpoint = 0;
         CREATE TABLE logged(y); INSERT INTO logged VALUES (2);",
    )
    .unwrap();
    let foreign = Scratch::new("static-foreign");
    copy_as_killed(&writer, &foreign);
    drop(raw);
    let garbage = Scratch::new("static-garbage");
    std::fs::write(garbage.db(), vec![0xA5_u8; 4096]).unwrap();
    for (case, d) in [
        ("foreign beside a log", &foreign),
        ("not a database", &garbage),
    ] {
        let before = dir_bytes(&d.0);
        let reads = count_probe_reads(None);
        let started = std::time::Instant::now();
        let err = SqliteStore::open(&d.db(), &d.keys()).map(drop).unwrap_err();
        let waited = started.elapsed();
        stop_counting_probe_reads();
        assert!(
            reads.get() > 1,
            "{case}: read again inside the window ({} reads)",
            reads.get()
        );
        match case {
            "foreign beside a log" => assert!(
                matches!(err, StoreError::NotAKabanProject { .. }),
                "{case}: {err:?}"
            ),
            _ => assert!(matches!(err, StoreError::Backend { .. }), "{case}: {err:?}"),
        }
        assert!(
            waited >= crate::connection::PROBE_RETRY_WINDOW,
            "{case}: refused after {waited:?}, inside the retry window"
        );
        assert_eq!(
            dir_bytes(&d.0),
            before,
            "{case}: nothing created or written"
        );
    }
}

/// Spec 11 CMP-01: a readable kaban schema at another version is a deterministic refusal,
/// returned on the probe's first read, never retried.
#[test]
fn a_readable_older_schema_is_refused_without_waiting() {
    let d = closed_v14_project("older-at-once");
    let raw = Connection::open(d.db()).unwrap();
    raw.execute(
        "UPDATE index_meta SET value = '13' WHERE key = 'schema_version'",
        [],
    )
    .unwrap();
    drop(raw);
    let reads = count_probe_reads(None);
    let err = SqliteStore::open(&d.db(), &d.keys()).map(drop).unwrap_err();
    stop_counting_probe_reads();
    assert!(
        matches!(err, StoreError::SchemaUnsupported { found: 13, .. }),
        "{err:?}"
    );
    assert_eq!(reads.get(), 1, "refused on the first read, never retried");
}

/// Every entry of `dir` by its raw name (non-UTF-8 names included) with its bytes.
fn raw_dir_bytes(dir: &std::path::Path) -> std::collections::BTreeMap<std::ffi::OsString, Vec<u8>> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let bytes = if entry.file_type().unwrap().is_dir() {
                Vec::new()
            } else {
                std::fs::read(entry.path()).unwrap()
            };
            (entry.file_name(), bytes)
        })
        .collect()
}

/// Spec 11 CMP-01: the checked file is the opened file whatever its name. For names holding a
/// space, `%`, `#`, `?`, `:` or a non-UTF-8 byte, a v3.0.0 file (the killed-writer fixture)
/// is refused with its bytes and directory unchanged, and a fresh project is created, read
/// back by the immutable probe form and reopened under exactly that name, with nothing
/// created under another one.
#[test]
fn a_database_name_with_uri_characters_is_one_file_to_the_probe_and_the_open() {
    use std::os::unix::ffi::OsStrExt;
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/v3.0.0-wal");
    let names: [&[u8]; 6] = [
        b"with space.db",
        b"pct%41.db",
        b"hash#x.db",
        b"query?mode=memory.db",
        b"file:colon.db",
        b"bad\xff.db",
    ];
    for (i, name) in names.into_iter().enumerate() {
        let name = std::ffi::OsStr::from_bytes(name);
        let older = Scratch::new(&format!("uri-older-{i}"));
        let db = older.0.join(name);
        let mut wal = db.as_os_str().to_owned();
        wal.push("-wal");
        std::fs::copy(fixture.join("kaban.db"), &db).unwrap();
        std::fs::copy(fixture.join("kaban.db-wal"), &wal).unwrap();
        let before = raw_dir_bytes(&older.0);
        let err = SqliteStore::open(&db, &older.keys()).map(drop).unwrap_err();
        assert!(
            matches!(err, StoreError::SchemaUnsupported { found: 13, .. }),
            "{name:?}: {err:?}"
        );
        assert_eq!(raw_dir_bytes(&older.0), before, "{name:?}: nothing written");

        let fresh = Scratch::new(&format!("uri-fresh-{i}"));
        let db = fresh.0.join(name);
        let created = SqliteStore::open(&db, &fresh.keys()).unwrap();
        assert_eq!(created.migration().from, 0, "{name:?}: created");
        drop(created);
        let probe = Connection::open_with_flags(
            format!("{}?mode=ro&immutable=1", crate::connection::sqlite_uri(&db)),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        let version: String = probe
            .query_row(
                "SELECT value FROM index_meta WHERE key = 'schema_version'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        drop(probe);
        assert_eq!(
            version,
            crate::LATEST_SCHEMA_VERSION.to_string(),
            "{name:?}"
        );
        let reopened = SqliteStore::open(&db, &fresh.keys()).unwrap();
        assert_eq!(
            reopened.migration().from,
            crate::LATEST_SCHEMA_VERSION,
            "{name:?}: reopened, not created again"
        );
        drop(reopened);
        let databases: Vec<_> = raw_dir_bytes(&fresh.0)
            .into_iter()
            .filter(|(_, bytes)| bytes.starts_with(b"SQLite format 3\0"))
            .map(|(entry, _)| entry)
            .collect();
        assert_eq!(
            databases,
            [name.to_owned()],
            "{name:?}: one database, under its name"
        );
    }
}

/// The schema version recorded in the main file of `db` alone (the refusal probe's read), or
/// `None` when the main file has none.
fn main_file_version(db: &std::path::Path) -> Option<String> {
    let probe = Connection::open_with_flags(
        format!("{}?mode=ro&immutable=1", crate::connection::sqlite_uri(db)),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .ok()?;
    probe
        .query_row(
            "SELECT value FROM index_meta WHERE key = 'schema_version'",
            [],
            |r| r.get(0),
        )
        .ok()
}

/// Whether `err` is the refused publication of a new project's version (spec 11 CMP-01).
fn is_unpublished(err: &StoreError) -> bool {
    matches!(
        err,
        StoreError::Backend { source }
            if source
                .driver()
                .downcast_ref::<crate::connection::VersionUnpublished>()
                .is_some()
    )
}

/// An empty WAL-mode database at `d` (a fresh project to the probe) and a reader holding a
/// read transaction on it: while that transaction lasts no checkpoint can copy a frame into
/// the main file (the reader reads the main file alone).
fn fresh_file_with_blocking_reader(d: &Scratch) -> Connection {
    let raw = Connection::open(d.db()).unwrap();
    let mode: String = raw
        .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    raw.execute_batch("BEGIN").unwrap();
    let tables: i64 = raw
        .query_row("SELECT count(*) FROM sqlite_master", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tables, 0, "nothing created yet");
    raw
}

/// Spec 11 CMP-01: a creation whose TRUNCATE checkpoint cannot complete (a reader blocks it
/// for the whole publication window) is not reported as success with its version only in
/// the `-wal`: the open fails. Once the reader is gone (its close folds the log into the main
/// file) the project reopens as the v14 project it is.
#[test]
fn a_creation_whose_checkpoint_stays_blocked_is_not_reported_as_success() {
    let d = Scratch::new("blocked-publication");
    let reader = fresh_file_with_blocking_reader(&d);
    match SqliteStore::open(&d.db(), &d.keys()) {
        Ok(store) => {
            let in_main_file = main_file_version(&d.db());
            drop(store);
            panic!(
                "expected error: the creation reported success, main-file version {in_main_file:?}"
            );
        }
        Err(err) => assert!(
            is_unpublished(&err),
            "the open failed on its publication: {err:?}"
        ),
    }
    reader.execute_batch("COMMIT").unwrap();
    drop(reader);
    let reopened = SqliteStore::open(&d.db(), &d.keys()).unwrap();
    assert_eq!(
        main_file_version(&d.db()),
        Some(crate::LATEST_SCHEMA_VERSION.to_string()),
        "the version is in the main file"
    );
    assert_eq!(
        reopened.migration().from,
        crate::LATEST_SCHEMA_VERSION,
        "reopened as the v14 project, not created again"
    );
}

/// Spec 11 CMP-01, two first openers: a second opener that arrives after the creator
/// committed its schema and seeds but before the creator published them into the main file
/// (held there by [`super::BEFORE_PUBLICATION`]) does not return with the version only in the
/// `-wal`: when it succeeds, the main file holds v14. The creator then finishes as well.
#[test]
fn a_second_first_opener_never_succeeds_before_the_version_is_in_the_main_file() {
    let d = Scratch::new("two-first-openers");
    let (at_publication, creator_paused) = std::sync::mpsc::channel::<()>();
    let (go_on, resume) = std::sync::mpsc::channel::<()>();
    let creator = {
        let (db, keys) = (d.db(), d.keys());
        std::thread::spawn(move || {
            super::BEFORE_PUBLICATION.with(|seam| {
                *seam.borrow_mut() = Some(Box::new(move || {
                    at_publication.send(()).unwrap();
                    // the second opener runs now; never wait for it forever
                    let _ = resume.recv_timeout(std::time::Duration::from_secs(20));
                }));
            });
            SqliteStore::open(&db, &keys).map(|store| store.migration().from)
        })
    };
    creator_paused
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the creator reached its publication");
    let second = SqliteStore::open(&d.db(), &d.keys());
    let in_main_file = main_file_version(&d.db());
    go_on.send(()).unwrap();
    let second_from = second
        .as_ref()
        .map(|store| store.migration().from)
        .map_err(ToString::to_string);
    drop(second);
    assert_eq!(
        second_from.as_ref().ok(),
        Some(&crate::LATEST_SCHEMA_VERSION),
        "the second opener opened the creator's project: {second_from:?}"
    );
    assert_eq!(
        in_main_file,
        Some(crate::LATEST_SCHEMA_VERSION.to_string()),
        "the second opener succeeded before the version was in the main file"
    );
    assert_eq!(
        creator.join().unwrap().unwrap(),
        0,
        "the creator created it"
    );
    assert_eq!(
        main_file_version(&d.db()),
        Some(crate::LATEST_SCHEMA_VERSION.to_string())
    );
}

/// Spec 11 CMP-01, an independent reader beside a first opener: while the creator has
/// committed its schema but not yet published it into the main file (held there by
/// [`super::BEFORE_PUBLICATION`]), `IndexRows::open` in the same process gives no usable
/// reader; once the creator has published, it opens.
#[test]
fn an_index_reader_is_not_admitted_before_the_creator_published_the_version() {
    let d = Scratch::new("index-reader-before-publication");
    let (at_publication, creator_paused) = std::sync::mpsc::channel::<()>();
    let (go_on, resume) = std::sync::mpsc::channel::<()>();
    let creator = {
        let (db, keys) = (d.db(), d.keys());
        std::thread::spawn(move || {
            super::BEFORE_PUBLICATION.with(|seam| {
                *seam.borrow_mut() = Some(Box::new(move || {
                    at_publication.send(()).unwrap();
                    let _ = resume.recv_timeout(std::time::Duration::from_secs(20));
                }));
            });
            SqliteStore::open(&db, &keys).map(|store| store.migration().from)
        })
    };
    creator_paused
        .recv_timeout(std::time::Duration::from_secs(20))
        .expect("the creator reached its publication");

    let early = crate::index_rows::IndexRows::open(&d.db());
    let in_main_file = main_file_version(&d.db());
    go_on.send(()).unwrap();
    match early {
        Ok(_) => panic!(
            "an index reader was admitted before publication, main-file version {in_main_file:?}"
        ),
        Err(err) => assert!(is_unpublished(&err), "{err:?}"),
    }

    assert_eq!(
        creator.join().unwrap().unwrap(),
        0,
        "the creator created it"
    );
    assert_eq!(
        main_file_version(&d.db()),
        Some(crate::LATEST_SCHEMA_VERSION.to_string())
    );
    crate::index_rows::IndexRows::open(&d.db())
        .expect("the published project opens for the index reader");
}
