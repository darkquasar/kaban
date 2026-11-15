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
