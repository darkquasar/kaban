//! The north-star queries through `KabanApi` (engine-spec EN-F19, EN-F21 code side; engine-plan
//! E2.7 item 4): "which Decisions govern this symbol" as the two-part answer of `why {symbol}`
//! (the `path_scope` leg over the defining file and the `symbol` leg over the anchored
//! `CodeRef`, a superseded predecessor absent), and `traverse(follow_anchors)` from that
//! `CodeRef` carrying the defining file and the symbol as cards, with the EN-F19 bounds (200 per
//! anchored node, 500 in total) over the `many` fixture. The snapshot is the §10 fixture builder's
//! file behind a `FixedSnapshot` with the head pinned (no git). Invented data.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

#[path = "common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use std::sync::{Arc, Mutex};

use cg_fixture::{AFERO_COMMIT, AFERO_KEY, AFERO_PATH, MANY_PATH, REPO_ID, Variant};
use common::Scratch;
use kaban_api::{
    AcceptRequest, AnchoredKind, ApiConfig, Caller, KabanApi, LinkRequest, MatchedBy,
    RememberRequest, TraverseRequest, TraverseResponse, WhyCard, WhyRequest, WhyTarget,
};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_codegraph::{HeadInput, NoSnapshot, Resolved, SnapshotProvider};
use kaban_core::Actor;
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use serde_json::{Value, json};

/// The process repository identity (`ApiConfig.repo` = `CodeRef.repo`), not the fixture's
/// keyed directory name (SD-5).
const REPO: &str = "kaban";

fn human() -> Caller {
    Caller {
        kind: Actor::HumanCli,
        ..Default::default()
    }
}

/// A provider a test flips between its [`FixedSnapshot`] and a refusal, so one service
/// instance sees a resolution and then the provider refusing (the directory gone, every
/// candidate too new, the directory refused at admission).
struct Switch {
    fixed: FixedSnapshot,
    refusal: Mutex<Option<NoSnapshot>>,
}

impl Switch {
    /// Refuse every resolve with `reason`; `None` resolves the fixture file again.
    fn refuse(&self, reason: Option<NoSnapshot>) {
        *self.refusal.lock().unwrap() = reason;
    }
}

impl SnapshotProvider for Switch {
    fn resolve(&self, repo_identity: &str, head: &HeadInput) -> Result<Resolved, NoSnapshot> {
        match *self.refusal.lock().unwrap() {
            Some(reason) => Err(reason),
            None => self.fixed.resolve(repo_identity, head),
        }
    }
}

struct Fixture {
    s: Scratch,
    api: KabanApi,
}

impl Fixture {
    /// A service bound to [`REPO`] over a fresh store, its head pinned to `commit`, resolving
    /// the `variant` fixture file through a `FixedSnapshot`.
    fn new(name: &str, variant: Variant, commit: &str) -> Self {
        Self::switchable(name, variant, commit).0
    }

    /// [`Self::new`] with the provider behind a [`Switch`] (resolving until told otherwise),
    /// returned beside the service.
    fn switchable(name: &str, variant: Variant, commit: &str) -> (Self, Arc<Switch>) {
        let s = Scratch::new("e27-north-star", name);
        let store = Arc::new(SqliteStore::open(&s.db(), &s.keys()).unwrap());
        let policy = Policy::from_file(Policy::parse(b"").unwrap(), "test".into()).unwrap();
        let built = cg_fixture::build(&s.0, variant, commit);
        let switch = Arc::new(Switch {
            fixed: FixedSnapshot::new(&built.path),
            refusal: Mutex::new(None),
        });
        let api = KabanApi::with_config(
            store.clone(),
            policy,
            store.signer(),
            None,
            ApiConfig {
                repo: Some(REPO.into()),
                // a root is required for a resolution; the pinned head never runs git in it,
                // and the currencies read `uncheckable` (no git history), which is not the point
                repo_root: Some(s.0.clone()),
                ..Default::default()
            },
        )
        .with_snapshot_provider(
            s.0.join("codegraph").join(REPO_ID),
            Arc::clone(&switch) as Arc<dyn SnapshotProvider>,
        )
        .with_pinned_head(commit);
        (Self { s, api }, switch)
    }

    /// A proposed Decision; its id.
    fn decision(&self, text: &str) -> String {
        self.api
            .remember(RememberRequest {
                kind: "Decision".into(),
                text: text.into(),
                props: Some(json!({"rationale": "because"})),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    /// Accept `id` with `scope` (a human action).
    fn accept(&self, id: &str, scope: &[&str]) {
        self.api
            .accept(
                AcceptRequest {
                    id: id.into(),
                    reason: Some("reviewed".into()),
                    path_scope: scope.iter().map(|s| (*s).to_owned()).collect(),
                    ..Default::default()
                },
                &human(),
            )
            .unwrap();
    }

    /// A `CodeRef` of this process's repository at `path` with `symbol`; its id.
    fn coderef(&self, path: &str, symbol: &str) -> String {
        self.api
            .remember(RememberRequest {
                kind: "CodeRef".into(),
                text: format!("{REPO} {path} {symbol}"),
                props: Some(json!({
                    "repo": REPO, "commit": AFERO_COMMIT, "tool": "fixture",
                    "relative_path": path, "name_path": "x", "kind": "Method", "line": 1,
                    "symbol": symbol,
                })),
                ..Default::default()
            })
            .unwrap()
            .id
    }

    fn link(&self, from: &str, rel: &str, to: &str) {
        self.api
            .link(LinkRequest {
                from: from.into(),
                rel: rel.into(),
                to: to.into(),
                confidence: None,
                props: None,
            })
            .unwrap();
    }

    fn why_symbol(&self, key: &str) -> Vec<WhyCard> {
        self.api
            .why(WhyRequest {
                targets: vec![WhyTarget::Symbol(key.into())],
                ..Default::default()
            })
            .unwrap()
            .results
    }

    fn traverse(&self, start: &str, follow_anchors: bool) -> TraverseResponse {
        self.api
            .traverse(TraverseRequest {
                start_ids: vec![start.to_owned()],
                follow_anchors,
                ..Default::default()
            })
            .unwrap()
    }
}

/// EN-F21 code side: `why {symbol}` over the `afero` fixture answers with the Decision whose
/// `path_scope` governs the defining file `memmap.go` (`matched_by: path_scope`) and the
/// Decision anchored `GOVERNS` on the `CodeRef` carrying the key (`matched_by: symbol`); the
/// predecessor the anchored Decision superseded is absent.
#[test]
fn why_symbol_answers_both_legs_without_the_superseded_predecessor() {
    let f = Fixture::new("why", Variant::Afero, AFERO_COMMIT);
    let coderef = f.coderef(AFERO_PATH, AFERO_KEY);
    let by_scope = f.decision("memmap keeps files in memory");
    f.accept(&by_scope, &["memmap.go"]);
    let old = f.decision("MemMapFs.Create used to lock the whole map");
    f.accept(&old, &["docs/**"]);
    f.link(&old, "GOVERNS", &coderef);
    let anchored = f.decision("MemMapFs.Create locks one entry");
    f.link(&anchored, "SUPERSEDES", &old);
    f.accept(&anchored, &["docs/**"]);
    f.link(&anchored, "GOVERNS", &coderef);
    let _elsewhere = {
        let id = f.decision("the cli prints json");
        f.accept(&id, &["src/cli/**"]);
        id
    };

    let cards = f.why_symbol(AFERO_KEY);
    let answer: Vec<(&str, MatchedBy)> = cards
        .iter()
        .map(|c| (c.node.id.as_str(), c.matched_by))
        .collect();
    assert_eq!(
        answer,
        vec![
            (anchored.as_str(), MatchedBy::Symbol),
            (by_scope.as_str(), MatchedBy::PathScope),
        ],
        "{cards:?}"
    );
    assert!(
        !cards.iter().any(|c| c.node.id == old),
        "the superseded predecessor is absent: {cards:?}"
    );
}

/// EN-F19 (the feature pair of E2.7 item 4): `traverse(follow_anchors: true)` from the
/// `CodeRef` carries the `memmap.go` file card and the `AFERO_KEY` symbol card, ordered
/// `(path, ref)`, each `from` the `CodeRef`; `false` carries none, and the nodes and edges
/// are the same either way.
#[test]
fn traverse_follow_anchors_carries_the_file_and_symbol_cards_and_off_carries_none() {
    let f = Fixture::new("traverse", Variant::Afero, AFERO_COMMIT);
    let coderef = f.coderef(AFERO_PATH, AFERO_KEY);
    let decision = f.decision("memmap keeps files in memory");
    f.accept(&decision, &["memmap.go"]);
    f.link(&decision, "GOVERNS", &coderef);

    let off = f.traverse(&coderef, false);
    assert_eq!(off.nodes.len(), 2, "{off:?}");
    assert!(off.anchored.is_empty(), "{off:?}");
    let wire = serde_json::to_value(&off).unwrap();
    assert_eq!(
        wire["anchored"],
        json!([]),
        "the field is on the wire, empty"
    );

    let on = f.traverse(&coderef, true);
    assert_eq!(
        (json!(on.nodes), json!(on.edges)),
        (json!(off.nodes), json!(off.edges)),
        "the memory side is unchanged"
    );
    let file = on
        .anchored
        .iter()
        .find(|a| a.kind == AnchoredKind::File && a.path == AFERO_PATH)
        .unwrap_or_else(|| panic!("the file card: {:?}", on.anchored));
    assert_eq!(file.from, coderef);
    assert_eq!(
        (
            file.r#ref.as_deref(),
            file.key.as_deref(),
            file.name.as_deref()
        ),
        (None, None, None)
    );
    let symbol = on
        .anchored
        .iter()
        .find(|a| a.kind == AnchoredKind::Symbol && a.key.as_deref() == Some(AFERO_KEY))
        .unwrap_or_else(|| panic!("the symbol card: {:?}", on.anchored));
    assert_eq!(symbol.from, coderef);
    assert_eq!(symbol.path, AFERO_PATH);
    assert_eq!(symbol.name.as_deref(), Some("Create"));
    assert!(symbol.r#ref.as_deref().unwrap().starts_with("scip:"));
    // the Decision's `memmap.go` anchor reaches the same rows from its own node, so every
    // card appears once per `from` and the whole list is ordered by (from's walk order, path, ref)
    let positions: Vec<(&str, &str, Option<&str>)> = on
        .anchored
        .iter()
        .map(|a| (a.from.as_str(), a.path.as_str(), a.r#ref.as_deref()))
        .collect();
    let mut sorted = positions.clone();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        positions.len(),
        "no duplicate card: {positions:?}"
    );
    let wire: Value = serde_json::to_value(&on).unwrap();
    let first = &wire["anchored"][0];
    assert_eq!(first["kind"], json!("file"), "{first}");
    assert!(
        first.get("ref").is_none(),
        "a file card has no ref: {first}"
    );
}

/// EN-F19 under S7 (the wave-2 review's RW2-2, the red kept): on one service instance a
/// resolved call carries cards; then the provider refuses (the directory gone, every candidate
/// too new, the directory refused at admission) and the next `follow_anchors: true` call
/// carries none. The expansion is bound to this request's admission, never to the attachment
/// an earlier request left on the store's read connection; the memory side is the same
/// throughout.
#[test]
fn traverse_follow_anchors_carries_no_card_once_the_provider_refuses() {
    let (f, switch) = Fixture::switchable("refusal", Variant::Afero, AFERO_COMMIT);
    let coderef = f.coderef(AFERO_PATH, AFERO_KEY);
    let decision = f.decision("memmap keeps files in memory");
    f.accept(&decision, &["memmap.go"]);
    f.link(&decision, "GOVERNS", &coderef);

    for refusal in [
        NoSnapshot::NoDirectory,
        NoSnapshot::TooNewOnly,
        NoSnapshot::CorruptOnly,
    ] {
        switch.refuse(None);
        let resolved = f.traverse(&coderef, true);
        assert!(
            !resolved.anchored.is_empty(),
            "resolved before {refusal:?}: {resolved:?}"
        );
        switch.refuse(Some(refusal));
        let refused = f.traverse(&coderef, true);
        assert_eq!(
            json!(refused.anchored),
            json!([]),
            "after {refusal:?} the earlier request's attachment must not answer"
        );
        assert_eq!(
            (json!(refused.nodes), json!(refused.edges)),
            (json!(resolved.nodes), json!(resolved.edges)),
            "the memory side is unchanged"
        );
    }
}

/// EN-F19 bounds over the `many` fixture: 540 non-external symbols in one file, three
/// `CodeRef`s anchored on that file; each node's complete set is cut to 200 after ordering and
/// the traversal stops at 500 (200 + 200 + 100).
#[test]
fn traverse_follow_anchors_is_bounded_per_node_and_in_total() {
    let f = Fixture::new(
        "many",
        Variant::Many {
            total: 600,
            external: 50,
            local: 10,
        },
        AFERO_COMMIT,
    );
    let hub = f.decision("many.go is the hub");
    f.accept(&hub, &["docs/**"]);
    let refs: Vec<String> = (0..3)
        .map(|i| {
            f.coderef(
                MANY_PATH,
                &format!("scip-go gomod example.com/many . p/S{i:05}#M()."),
            )
        })
        .collect();
    for r in &refs {
        f.link(&hub, "GOVERNS", r);
    }
    let on = f
        .api
        .traverse(TraverseRequest {
            start_ids: vec![hub.clone()],
            node_kinds: vec!["CodeRef".into()],
            follow_anchors: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(on.nodes.len(), 3, "{:?}", on.nodes);
    assert_eq!(on.anchored.len(), 500, "the total bound");
    let mut per_node: Vec<usize> = refs
        .iter()
        .map(|r| on.anchored.iter().filter(|a| a.from == *r).count())
        .collect();
    per_node.sort_unstable();
    assert_eq!(
        per_node,
        vec![100, 200, 200],
        "the per-node bound, then the stop"
    );
    // a node's cut keeps the ordered head: the file card first, then symbols by ref
    let first_node = &on.anchored[0].from;
    let head: Vec<&kaban_api::AnchoredCard> = on
        .anchored
        .iter()
        .filter(|a| a.from == *first_node)
        .collect();
    assert_eq!(head[0].kind, AnchoredKind::File);
    assert!(
        head[1..].windows(2).all(|w| w[0].r#ref <= w[1].r#ref),
        "symbols ordered by ref"
    );
    assert!(
        head.iter().all(|a| a.path == MANY_PATH),
        "every card is of the anchored file"
    );
    drop(f.s);
}
