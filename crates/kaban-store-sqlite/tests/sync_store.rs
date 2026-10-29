//! The `SyncStore` role trait on SQLite (core-spec F13, F5, S5, S6; core-plan C5.1 and C5.3 as
//! amended by `reviews/m5-prep.md` M5-7, M5-8, M5-15, M5-18 and owner decisions OWN-18, OWN-23,
//! OWN-25): the dirty set, the serialized-rev stamp, the import of records and tombstones, the
//! conflict rows and the `post-commit` stamp, through the public trait only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use common::{Scratch, ctx, nid};
use kaban_core::{
    Actor, ForgetMode, GovernanceStore, MemoryStore, NewNode, Node, RememberOutcome, StoreScope,
    SyncStore, UnstampedCounts, UnstampedReason, WorkspaceId, WriteCtx,
};
use kaban_store_sqlite::SqliteStore;

/// The serialized kinds of the shipped `adr.yaml` (what `sync` passes from the registry, E1.6).
const KINDS: &[&str] = &["Decision", "Convention", "Failure"];

/// Remember a `kind` node in `ctx`'s workspace (fresh text each call, so always `Created`).
fn remember(store: &SqliteStore, kind: &str, text: &str, ctx: &WriteCtx) -> Node {
    match store
        .remember(
            NewNode {
                kind: kind.into(),
                text: text.into(),
                workspace_id: Some(ctx.workspace_id.as_str().to_owned()),
                props: (kind == "Decision").then(|| serde_json::json!({"rationale": "r"})),
                ..Default::default()
            },
            &[],
            ctx,
        )
        .unwrap()
    {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => panic!("fresh text: {node:?}"),
    }
}

fn ids(nodes: &[Node]) -> Vec<&str> {
    nodes.iter().map(|n| n.id.as_str()).collect()
}

/// M5-18 dirty set: a governed record never serialized, one advanced past its serialized rev
/// and a purged one whose tombstone is not yet written are dirty; a clean record, a purged
/// record that never had a file, a non-governed kind and another workspace's record are not.
/// `mark_serialized` cleans a record without touching `rev` or `updated_at`.
#[test]
fn the_dirty_set_is_the_stale_governed_records_of_the_workspace_and_mark_serialized_cleans_them() {
    let s = Scratch::new("kaban-sync", "dirty");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);

    let never = remember(&store, "Decision", "never serialized", &ws);
    let advanced = remember(&store, "Convention", "serialized then edited", &ws);
    let clean = remember(&store, "Failure", "\"make\" failed: clean", &ws);
    let purged_filed = remember(&store, "Decision", "purged after a file existed", &ws);
    let purged_unfiled = remember(&store, "Decision", "purged, never a file", &ws);
    let fact = remember(&store, "Fact", "facts have no file", &ws);
    let elsewhere = remember(&store, "Decision", "another tenant", &other);

    // serialize three of them at their current rev
    for n in [&advanced, &clean, &purged_filed] {
        store.mark_serialized(&nid(&n.id), n.rev).unwrap();
    }
    // the stamp changes serialized_rev only
    let stamped = store.get(&nid(&clean.id), false).unwrap().unwrap();
    assert_eq!(stamped.serialized_rev, Some(clean.rev));
    assert_eq!(stamped.rev, clean.rev, "no rev bump");
    assert_eq!(stamped.updated_at, clean.updated_at, "no updated_at change");

    // advance one (acceptance bumps rev), purge two
    let advanced_again = store
        .transition(
            &nid(&advanced.id),
            kaban_core::Governance::Accepted,
            kaban_core::NewEvent {
                reason: "ok".into(),
                ..Default::default()
            },
            &ws,
        )
        .unwrap()
        .0;
    assert!(advanced_again.rev > advanced.rev);
    store
        .forget(&nid(&purged_filed.id), ForgetMode::Purge, &ws)
        .unwrap();
    store
        .forget(&nid(&purged_unfiled.id), ForgetMode::Purge, &ws)
        .unwrap();

    let dirty = store
        .dirty_serializable(ws.workspace_id.as_str(), KINDS, 100)
        .unwrap();
    let mut expected = vec![
        never.id.as_str(),
        advanced.id.as_str(),
        purged_filed.id.as_str(),
    ];
    expected.sort_unstable();
    assert_eq!(ids(&dirty), expected, "ordered by id");
    for n in &dirty {
        assert_eq!(n.store_scope, StoreScope::Project);
        assert_ne!(n.id, fact.id);
        assert_ne!(n.id, elsewhere.id, "S6");
        assert_ne!(
            n.id, purged_unfiled.id,
            "M5-7: never serialized, no tombstone"
        );
    }
    // the purged one comes back as its ghost row (the tombstone needs only id and rev)
    let ghost = dirty.iter().find(|n| n.id == purged_filed.id).unwrap();
    assert_eq!(ghost.lifecycle, kaban_core::Lifecycle::Purged);
    assert_eq!(ghost.text, "");

    // the limit binds, in id order
    let two = store
        .dirty_serializable(ws.workspace_id.as_str(), KINDS, 2)
        .unwrap();
    assert_eq!(ids(&two), expected[..2].to_vec());

    // the other tenant sees only its own
    let theirs = store.dirty_serializable("ws-other", KINDS, 100).unwrap();
    assert_eq!(ids(&theirs), vec![elsewhere.id.as_str()]);

    // writing the files cleans the set; a second pass finds nothing (M5-19 idempotency)
    for n in &dirty {
        store.mark_serialized(&nid(&n.id), n.rev).unwrap();
    }
    assert!(
        store
            .dirty_serializable(ws.workspace_id.as_str(), KINDS, 100)
            .unwrap()
            .is_empty()
    );
    // an unknown id is NotFound
    assert!(matches!(
        store.mark_serialized(&nid("01a17a03-5cbc-7000-8000-00000000dead"), 1),
        Err(kaban_core::StoreError::NotFound(_))
    ));
}

/// An accepted Decision as a file would deliver it (invented data), at `rev`.
fn imported(id: &str, kind: &str, text: &str, rev: i64) -> kaban_core::ImportedRecord {
    kaban_core::ImportedRecord {
        id: nid(id),
        kind: kind.into(),
        text: text.into(),
        summary: Some("from a file".into()),
        tags: vec!["t1".into()],
        confidence: Some(0.8),
        importance: Some(0.6),
        lifecycle: kaban_core::Lifecycle::Active,
        governance: kaban_core::Governance::Accepted,
        path_scope: vec!["src/**".into()],
        props: serde_json::json!({"rationale": "written on another clone"}),
        rev,
        created_at: "2026-10-26T12:36:36Z".into(),
        updated_at: "2026-10-26T12:38:22Z".into(),
        valid_from: None,
        valid_to: None,
        accepted_commit: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
        agent_id: Some("other-agent".into()),
        session_id: None,
        repo: Some("nightjar-service".into()),
        commit_sha: None,
        dup_key: Some("from a file\u{1}src/**".into()),
        event: Some(kaban_core::NewEvent {
            reason: "reviewed in PR 7".into(),
            signer_kind: kaban_core::SignerKind::HumanPr,
            commit_sha: Some("89abcdef0123456789abcdef0123456789abcdef".into()),
            ..Default::default()
        }),
    }
}

/// M5-18 `import_record`: a file for an unknown id inserts the row with the file's content,
/// `rev` and `serialized_rev` equal to the file's rev and the acceptance event; a file for a
/// stored record updates every content column at the file's rev; a file that would move
/// governance backwards (accepted -> proposed) is `InvalidTransition` and writes nothing; a
/// purged id and another tenant's id are `NotFound`; a kind change is `KindMismatch`.
#[test]
fn import_record_inserts_or_updates_at_the_file_rev_and_refuses_invalid_transitions() {
    let s = Scratch::new("kaban-sync", "import");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);
    let id = "01a17a03-5cbc-7000-8000-000000000101";

    // insert path
    let record = imported(id, "Decision", "Use SQLite.", 3);
    let node = store.import_record(&record, &ws).unwrap();
    assert_eq!(node.id, id);
    assert_eq!((node.rev, node.serialized_rev), (3, Some(3)));
    assert_eq!(
        node.workspace_id, "default",
        "bound to the process workspace, OWN-25"
    );
    assert_eq!(node.governance, kaban_core::Governance::Accepted);
    assert_eq!(node.lifecycle, kaban_core::Lifecycle::Active);
    assert_eq!(node.text, "Use SQLite.");
    assert_eq!(node.summary.as_deref(), Some("from a file"));
    assert_eq!(node.path_scope, vec!["src/**"]);
    assert_eq!(node.props["rationale"], "written on another clone");
    assert_eq!(node.created_at, "2026-10-26T12:36:36Z");
    assert_eq!(node.updated_at, "2026-10-26T12:38:22Z");
    assert_eq!(
        node.accepted_commit.as_deref(),
        Some("89abcdef0123456789abcdef0123456789abcdef")
    );
    assert_eq!(node.agent_id.as_deref(), Some("other-agent"));
    assert_eq!(node.repo.as_deref(), Some("nightjar-service"));
    assert_eq!(node.dup_key.as_deref(), Some("from a file\u{1}src/**"));
    assert_eq!(node.policy_version, "pv");
    assert_eq!(node.store_scope, StoreScope::Project);
    assert!(!node.content_hash.is_empty(), "recomputed dedup hash");
    let events = store.events(&nid(id)).unwrap();
    assert_eq!(
        events.len(),
        1,
        "the acceptance event rides with the insert"
    );
    assert_eq!(events[0].kind, "accepted");
    assert_eq!(events[0].reason, "reviewed in PR 7");
    assert_eq!(events[0].signer_kind, kaban_core::SignerKind::HumanPr);
    // the imported record is visible and clean
    assert!(store.get(&nid(id), false).unwrap().is_some());
    assert_eq!(
        store
            .dirty_serializable("default", KINDS, 10)
            .unwrap()
            .len(),
        0
    );

    // update path: the file advanced (rev 5), a props edit and a supersession
    let mut newer = imported(id, "Decision", "Use SQLite, bundled.", 5);
    newer.props = serde_json::json!({"rationale": "bundled builds everywhere"});
    newer.lifecycle = kaban_core::Lifecycle::Archived;
    newer.governance = kaban_core::Governance::Superseded;
    newer.valid_to = Some("2026-10-26T07:27:27Z".into());
    newer.event = Some(kaban_core::NewEvent {
        reason: "superseded by a newer record".into(),
        ..Default::default()
    });
    let node = store.import_record(&newer, &ws).unwrap();
    assert_eq!((node.rev, node.serialized_rev), (5, Some(5)));
    assert_eq!(node.text, "Use SQLite, bundled.");
    assert_eq!(node.props["rationale"], "bundled builds everywhere");
    assert_eq!(node.lifecycle, kaban_core::Lifecycle::Archived);
    assert_eq!(node.governance, kaban_core::Governance::Superseded);
    assert_eq!(node.valid_to.as_deref(), Some("2026-10-26T07:27:27Z"));
    let events = store.events(&nid(id)).unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].kind, "superseded");
    assert_eq!(events[1].reason, "superseded by a newer record");

    // a content-only update without a governance change writes no event
    let mut same_state = newer.clone();
    same_state.rev = 6;
    same_state.text = "Use SQLite, bundled, always.".into();
    store.import_record(&same_state, &ws).unwrap();
    assert_eq!(
        store.events(&nid(id)).unwrap().len(),
        2,
        "no state change, no event"
    );

    // an invalid governance move (superseded -> proposed) writes nothing at all
    let mut backwards = same_state.clone();
    backwards.rev = 9;
    backwards.text = "a regression".into();
    backwards.governance = kaban_core::Governance::Proposed;
    assert!(matches!(
        store.import_record(&backwards, &ws),
        Err(kaban_core::StoreError::InvalidTransition(_))
    ));
    let after = store.get(&nid(id), true).unwrap().unwrap();
    assert_eq!(
        (after.rev, after.text.as_str()),
        (6, "Use SQLite, bundled, always.")
    );
    assert_eq!(store.events(&nid(id)).unwrap().len(), 2);
    // an invalid lifecycle move (archived -> active) too
    let mut revived = same_state.clone();
    revived.rev = 9;
    revived.lifecycle = kaban_core::Lifecycle::Active;
    assert!(matches!(
        store.import_record(&revived, &ws),
        Err(kaban_core::StoreError::InvalidTransition(_))
    ));
    assert_eq!(store.get(&nid(id), true).unwrap().unwrap().rev, 6);

    // a governance move needs an active stored record: a second Decision, archived first
    let archived_id = "01a17a03-5cbc-7000-8000-000000000102";
    let mut archived = imported(archived_id, "Decision", "archived", 1);
    archived.governance = kaban_core::Governance::Proposed;
    store.import_record(&archived, &ws).unwrap();
    store
        .forget(&nid(archived_id), ForgetMode::Archive, &ws)
        .unwrap();
    let mut accept_archived = archived.clone();
    accept_archived.rev = 7;
    accept_archived.lifecycle = kaban_core::Lifecycle::Archived;
    accept_archived.governance = kaban_core::Governance::Accepted;
    assert!(matches!(
        store.import_record(&accept_archived, &ws),
        Err(kaban_core::StoreError::InvalidTransition(_))
    ));

    // another tenant's row is invisible (S6), a kind change is refused, a purged id is gone
    assert!(matches!(
        store.import_record(&same_state, &other),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    let mut as_convention = same_state.clone();
    as_convention.kind = "Convention".into();
    assert!(matches!(
        store.import_record(&as_convention, &ws),
        Err(kaban_core::StoreError::KindMismatch { .. })
    ));
    store.forget(&nid(id), ForgetMode::Purge, &ws).unwrap();
    let mut resurrect = same_state.clone();
    resurrect.rev = 50;
    assert!(matches!(
        store.import_record(&resurrect, &ws),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    // an unknown kind is refused before anything is written
    let unknown = imported("01a17a03-5cbc-7000-8000-000000000103", "Rumour", "x", 1);
    assert!(matches!(
        store.import_record(&unknown, &ws),
        Err(kaban_core::StoreError::UnknownKind(_))
    ));
}

/// M5-7, M5-8, OWN-18 `import_purged`: a tombstone for an unknown id leaves a purged ghost
/// row at the file's rev, so the id is invisible to reads, never re-importable and not dirty;
/// a tombstone for a live record purges it in place (text, edges and dedup identity gone, one
/// `purge` changes row) at a rev above both sides; a tombstone for an already purged record only
/// moves the rev up; another tenant's row is `NotFound`.
#[test]
fn import_purged_leaves_a_ghost_row_that_never_comes_back_and_purges_live_records_in_place() {
    use kaban_core::ChangeLogStore;
    let s = Scratch::new("kaban-sync", "purged");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);

    // unknown id: a ghost
    let ghost_id = "01a17a03-5cbc-7000-8000-000000000201";
    assert_eq!(
        store
            .import_purged(&nid(ghost_id), "Decision", 4, &ws)
            .unwrap()
            .rev,
        4
    );
    assert!(
        store.get(&nid(ghost_id), false).unwrap().is_none(),
        "hidden"
    );
    let ghost = store.get(&nid(ghost_id), true).unwrap().unwrap();
    assert_eq!(ghost.lifecycle, kaban_core::Lifecycle::Purged);
    assert_eq!((ghost.rev, ghost.serialized_rev), (4, Some(4)));
    assert_eq!((ghost.kind.as_str(), ghost.text.as_str()), ("Decision", ""));
    assert_eq!(ghost.workspace_id, "default");
    // the id never comes back through a file (F5, WD-1) and the tombstone is not dirty
    assert!(matches!(
        store.import_record(
            &imported(ghost_id, "Decision", "back from the dead", 9),
            &ws
        ),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    assert_eq!(
        store
            .dirty_serializable("default", KINDS, 10)
            .unwrap()
            .len(),
        0
    );
    // a later tombstone at a higher rev moves the ghost up, a lower one is a no-op
    assert_eq!(
        store
            .import_purged(&nid(ghost_id), "Decision", 6, &ws)
            .unwrap()
            .rev,
        6
    );
    assert_eq!(
        store
            .import_purged(&nid(ghost_id), "Decision", 2, &ws)
            .unwrap()
            .rev,
        6
    );
    assert_eq!(store.get(&nid(ghost_id), true).unwrap().unwrap().rev, 6);

    // a live record: purged in place like forget(Purge)
    let live = remember(&store, "Decision", "secret-ish text to erase", &ws);
    let evidence = remember(&store, "Fact", "linked evidence", &ws);
    store
        .link(
            &nid(&evidence.id),
            &nid(&live.id),
            "RELATES_TO",
            None,
            None,
            &ws,
        )
        .unwrap();
    let before_seq = store
        .changes_read(0, 1000)
        .unwrap()
        .last()
        .map_or(0, |c| c.seq);
    let imported = store
        .import_purged(&nid(&live.id), "Decision", 1, &ws)
        .unwrap();
    // the purge's post-commit cleanup is reported with the rev (SS-03), never raised
    assert!(
        imported
            .cleanup
            .as_deref()
            .is_some_and(|c| c.starts_with("database/WAL: free-page residue until VACUUM; index:")),
        "{:?}",
        imported.cleanup
    );
    let rev = imported.rev;
    assert!(
        rev > live.rev,
        "above the database's rev: {rev} > {}",
        live.rev
    );
    let purged = store.get(&nid(&live.id), true).unwrap().unwrap();
    assert_eq!(purged.lifecycle, kaban_core::Lifecycle::Purged);
    assert_eq!((purged.rev, purged.serialized_rev), (rev, Some(rev)));
    assert_eq!(purged.text, "");
    assert_eq!(purged.content_hash, "");
    assert!(purged.dup_key.is_none());
    assert_eq!(purged.props, serde_json::json!({}));
    assert!(store.get(&nid(&live.id), false).unwrap().is_none());
    // the edge is gone and the `ChangeLog` carries the explicit purge row
    let tr = store
        .traverse(&kaban_core::TraverseQuery {
            start_ids: vec![nid(&evidence.id)],
            rels: vec![],
            direction: kaban_core::TraverseDirection::Both,
            max_depth: 1,
            node_kinds: vec![],
            governance: vec![],
            limit: 10,
            workspace_id: None,
            ..Default::default()
        })
        .unwrap();
    assert!(tr.edges.is_empty(), "edges erased: {:?}", tr.edges);
    let changes = store.changes_read(before_seq, 1000).unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c.op == kaban_core::ChangeOp::Purge && c.id == live.id),
        "{changes:?}"
    );
    // the same text can be remembered again: the dedup identity was erased
    let again = remember(&store, "Decision", "secret-ish text to erase", &ws);
    assert_ne!(again.id, live.id);
    // a higher file rev wins over the computed one
    let far = remember(&store, "Convention", "far ahead", &ws);
    assert_eq!(
        store
            .import_purged(&nid(&far.id), "Convention", 40, &ws)
            .unwrap()
            .rev,
        40
    );

    // another tenant's record is invisible; a kind change is refused
    let theirs = remember(&store, "Decision", "theirs", &other);
    assert!(matches!(
        store.import_purged(&nid(&theirs.id), "Decision", 3, &ws),
        Err(kaban_core::StoreError::NotFound(_))
    ));
    assert!(store.get(&nid(&theirs.id), false).unwrap().is_some());
    assert!(matches!(
        store.import_purged(&nid(&again.id), "Convention", 3, &ws),
        Err(kaban_core::StoreError::KindMismatch { .. })
    ));
}

/// M5-5, M5-6 conflict rows: upsert replaces the row of an id, `conflicts(ws)` lists the rows
/// whose node lives in the workspace by node id (S6), `conflicts_for` returns the rows among a
/// card list in list order, `clear` removes one row and says whether it existed.
#[test]
fn conflict_rows_are_upserted_listed_per_workspace_and_cleared() {
    let s = Scratch::new("kaban-sync", "conflicts");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);
    let a = remember(&store, "Decision", "a", &ws);
    let b = remember(&store, "Convention", "b", &ws);
    let theirs = remember(&store, "Decision", "theirs", &other);
    let row = |node: &Node, file_rev: i64, detected_at: i64| kaban_core::ConflictRow {
        node_id: node.id.clone(),
        file_rev,
        db_rev: node.rev,
        file_hash: "f".repeat(64),
        db_hash: "d".repeat(64),
        detected_at,
    };

    store.conflict_upsert(&row(&a, 2, 100)).unwrap();
    store.conflict_upsert(&row(&b, 3, 101)).unwrap();
    store.conflict_upsert(&row(&theirs, 4, 102)).unwrap();
    // a second detection of the same id replaces the row
    store.conflict_upsert(&row(&a, 5, 200)).unwrap();

    let mine = store.conflicts("default").unwrap();
    let mut expected = vec![(a.id.as_str(), 5i64, 200i64), (b.id.as_str(), 3, 101)];
    expected.sort_unstable();
    assert_eq!(
        mine.iter()
            .map(|c| (c.node_id.as_str(), c.file_rev, c.detected_at))
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(mine[0].db_hash, "d".repeat(64));
    let theirs_rows = store.conflicts("ws-other").unwrap();
    assert_eq!(theirs_rows.len(), 1);
    assert_eq!(theirs_rows[0].node_id, theirs.id);

    // the bounded card lookup, in card order, unknown ids skipped
    let cards = [
        nid(&b.id),
        nid("01a17a03-5cbc-7000-8000-00000000dead"),
        nid(&a.id),
    ];
    let found = store.conflicts_for(&cards).unwrap();
    assert_eq!(
        found.iter().map(|c| c.node_id.as_str()).collect::<Vec<_>>(),
        vec![b.id.as_str(), a.id.as_str()]
    );
    assert_eq!(store.conflicts_for(&[]).unwrap().len(), 0);

    // clear: once true, then false; the others stay
    assert!(store.conflict_clear(&nid(&a.id)).unwrap());
    assert!(!store.conflict_clear(&nid(&a.id)).unwrap());
    assert_eq!(store.conflicts("default").unwrap().len(), 1);
    // stats keeps counting the table (doctor's `conflicts` check)
    let stats = store.stats(None).unwrap();
    assert_eq!(stats["conflicts"], 2);
}

/// M5-15, OWN-23 `stamp_commit`: every unstamped, non-purged node of the workspace whose
/// `repo` is unset or equals the process repo gets `commit_sha`, `rev + 1` and a new
/// `updated_at`, once; an already stamped node, another repository's node, another tenant's
/// node and a purged ghost are untouched; a second stamp stamps nothing.
#[test]
fn stamp_commit_stamps_unstamped_nodes_of_the_workspace_once() {
    let s = Scratch::new("kaban-sync", "stamp");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);
    let sha = "0123456789abcdef0123456789abcdef01234567";

    let fresh = remember(&store, "Decision", "fresh", &ws);
    let fact = remember(&store, "Fact", "any kind is stamped", &ws);
    let mine_repo = store
        .remember(
            NewNode {
                kind: "Convention".into(),
                text: "made in this repo".into(),
                repo: Some("nightjar-service".into()),
                ..Default::default()
            },
            &[],
            &ws,
        )
        .unwrap();
    let mine_repo = match mine_repo {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => node,
    };
    let foreign_repo = store
        .remember(
            NewNode {
                kind: "Convention".into(),
                text: "made elsewhere".into(),
                repo: Some("other-repo".into()),
                ..Default::default()
            },
            &[],
            &ws,
        )
        .unwrap();
    let foreign_repo = match foreign_repo {
        RememberOutcome::Created(n) => n,
        RememberOutcome::Existing { node, .. } => node,
    };
    let theirs = remember(&store, "Decision", "theirs", &other);
    let purged = remember(&store, "Decision", "purged", &ws);
    store
        .forget(&nid(&purged.id), ForgetMode::Purge, &ws)
        .unwrap();
    assert!(fresh.commit_sha.is_none(), "insert never stamps");
    assert_eq!(
        store
            .unstamped_current_count(
                "default",
                Some("nightjar-service"),
                &["Decision", "Fact", "Convention"]
            )
            .unwrap(),
        3
    );
    assert_eq!(
        store
            .unstamped_current_count("default", None, &["Decision", "Fact", "Convention"])
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .unstamped_current_count("default", Some("nightjar-service"), &[])
            .unwrap(),
        0
    );

    let stamped = store
        .stamp_commit(
            "default",
            sha,
            Some("nightjar-service"),
            &["Decision", "Fact", "Convention"],
        )
        .unwrap();
    assert_eq!(stamped, 3, "fresh, fact, mine_repo");
    assert_eq!(
        store
            .unstamped_current_count(
                "default",
                Some("nightjar-service"),
                &["Decision", "Fact", "Convention"]
            )
            .unwrap(),
        0
    );
    for before in [&fresh, &fact, &mine_repo] {
        let after = store.get(&nid(&before.id), false).unwrap().unwrap();
        assert_eq!(after.commit_sha.as_deref(), Some(sha), "{}", before.kind);
        assert_eq!(after.rev, before.rev + 1, "every write bumps rev (F13)");
        assert_ne!(after.updated_at, before.updated_at);
    }
    for untouched in [&foreign_repo, &theirs] {
        let after = store.get(&nid(&untouched.id), false).unwrap().unwrap();
        assert!(after.commit_sha.is_none(), "{}", untouched.text);
        assert_eq!(after.rev, untouched.rev);
    }
    let ghost = store.get(&nid(&purged.id), true).unwrap().unwrap();
    assert!(ghost.commit_sha.is_none(), "a ghost is never touched");

    // once: a second commit stamps nothing, even with another sha
    assert_eq!(
        store
            .stamp_commit(
                "default",
                &"f".repeat(40),
                Some("nightjar-service"),
                &["Decision", "Fact", "Convention"],
            )
            .unwrap(),
        0
    );
    // outside a repository identity only unset repos qualify
    let later = remember(&store, "Decision", "later", &ws);
    assert_eq!(
        store
            .stamp_commit("default", sha, None, &["Decision", "Fact", "Convention"])
            .unwrap(),
        1
    );
    assert_eq!(
        store
            .get(&nid(&later.id), false)
            .unwrap()
            .unwrap()
            .commit_sha
            .as_deref(),
        Some(sha)
    );
    // the stamped records are dirty again for the next sync (serialized with provenance)
    let dirty = store.dirty_serializable("default", KINDS, 10).unwrap();
    assert!(dirty.iter().any(|n| n.id == fresh.id));
}

/// M5-11 (C5.1b): the records the absence pass may miss a file for are the serialized
/// project-scoped governed records of the workspace that are `active` or `archived`; a record
/// never serialized, a tombstoned or purged one, a non-governed kind and another workspace's
/// record are not listed. Ordered by id; `limit` binds.
#[test]
fn serialized_records_are_the_filed_live_governed_records_of_the_workspace() {
    let s = Scratch::new("kaban-sync", "serialized");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let other = WriteCtx::new("pv", WorkspaceId::new("ws-other"), Actor::Agent);
    let active = remember(&store, "Decision", "filed and active", &ws);
    let archived = remember(&store, "Convention", "filed then archived", &ws);
    let tombstoned = remember(
        &store,
        "Failure",
        "\"make\" failed: filed then tombstoned",
        &ws,
    );
    let purged = remember(&store, "Decision", "filed then purged", &ws);
    let never = remember(&store, "Decision", "never filed", &ws);
    let fact = remember(&store, "Fact", "facts have no file", &ws);
    let elsewhere = remember(&store, "Decision", "another tenant, filed", &other);
    for n in [&active, &archived, &tombstoned, &purged, &fact] {
        store.mark_serialized(&nid(&n.id), n.rev).unwrap();
    }
    store
        .mark_serialized(&nid(&elsewhere.id), elsewhere.rev)
        .unwrap();
    store
        .forget(&nid(&archived.id), ForgetMode::Archive, &ws)
        .unwrap();
    store
        .forget(&nid(&tombstoned.id), ForgetMode::Tombstone, &ws)
        .unwrap();
    store
        .forget(&nid(&purged.id), ForgetMode::Purge, &ws)
        .unwrap();
    let _ = never;

    let listed = store
        .serialized_records(ws.workspace_id.as_str(), KINDS, 100)
        .unwrap();
    let mut expected = vec![active.id.as_str(), archived.id.as_str()];
    expected.sort_unstable();
    assert_eq!(ids(&listed), expected);
    assert_eq!(
        store
            .serialized_records(ws.workspace_id.as_str(), KINDS, 1)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        ids(&store.serialized_records("ws-other", KINDS, 100).unwrap()),
        vec![elsewhere.id.as_str()]
    );
    assert!(
        store
            .serialized_records("ws-none", KINDS, 100)
            .unwrap()
            .is_empty()
    );
}

/// OWN-65 (engine-spec §3.1 `index`; engine-plan E2.0 item 5b): `stamp_commit` updates only the
/// kinds the caller names (the registry's stamping set): a `Document` beside a committed
/// `Decision` stays unstamped, an empty set stamps nothing, and a later call naming the kind
/// stamps the row (the policy is the caller's, the store filters).
#[test]
fn stamp_commit_updates_only_the_kinds_in_the_stamping_set() {
    // Arrange
    let s = Scratch::new("kaban-sync", "stamp-kinds");
    let store = SqliteStore::open(&s.db(), &s.keys()).unwrap();
    let ws = ctx();
    let sha = "0123456789abcdef0123456789abcdef01234567";
    let decision = remember(&store, "Decision", "stamped per commit", &ws);
    let document = remember(&store, "Document", "ingested, never stamped", &ws);

    // Act
    let none = store.stamp_commit("default", sha, None, &[]).unwrap();
    let decisions = store
        .stamp_commit("default", sha, None, &["Decision", "Convention", "Failure"])
        .unwrap();

    // Assert
    assert_eq!(none, 0, "an empty set stamps nothing");
    assert_eq!(decisions, 1, "the Decision only");
    let decision_after = store.get(&nid(&decision.id), false).unwrap().unwrap();
    assert_eq!(decision_after.commit_sha.as_deref(), Some(sha));
    assert_eq!(decision_after.rev, decision.rev + 1);
    let document_after = store.get(&nid(&document.id), false).unwrap().unwrap();
    assert!(document_after.commit_sha.is_none(), "{document_after:?}");
    assert_eq!(document_after.rev, document.rev, "untouched");
    // the filter is the caller's policy, not a fixed list: naming the kind stamps it
    assert_eq!(
        store
            .stamp_commit("default", sha, None, &["Document"])
            .unwrap(),
        1
    );
}

/// OWN-65 5c, the store half: `record_unstamped` writes one `events` row (`kind = 'unstamped'`,
/// `reason` the fixed text) for a record of a stamping kind written where no stamp can come,
/// and `unstamped_counts` groups those rows by reason for `doctor`.
#[test]
fn record_unstamped_writes_one_event_row_per_reason_and_doctor_counts_them() {
    // Arrange
    let store = SqliteStore::open_memory().unwrap();
    let ws = ctx();
    let a = remember(&store, "Decision", "outside any repository", &ws);
    let b = remember(&store, "Decision", "repository without history", &ws);
    let c = remember(&store, "Decision", "also without history", &ws);

    // Act
    store
        .record_unstamped(&nid(&a.id), "default", UnstampedReason::NoRepository)
        .unwrap();
    store
        .record_unstamped(&nid(&b.id), "default", UnstampedReason::NoHistory)
        .unwrap();
    store
        .record_unstamped(&nid(&c.id), "default", UnstampedReason::NoHistory)
        .unwrap();
    let counts = store.unstamped_counts("default").unwrap();

    // Assert
    let events = store.events(&nid(&a.id)).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].kind, "unstamped");
    assert_eq!(events[0].reason, "no_repository");
    assert_eq!(counts.no_repository, 1);
    assert_eq!(counts.no_history, 2);
    assert_eq!(
        store.unstamped_counts("ws-other").unwrap(),
        UnstampedCounts::default()
    );
}
