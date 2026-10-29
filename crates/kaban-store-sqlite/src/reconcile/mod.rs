//! Reconciliation inside the store transaction (engine-spec EN-E9, EN-F8, EN-F9, EN-F12;
//! engine-plan E2.2): the batch's claims become immutable `assertion` rows, every touched
//! conflict group is decided by the injected [`Reconciler`] in ascending `group_key` order
//! over all of its stored supporters, and the verdicts are projected (`project`): statuses,
//! the one edge per triple, the review rows. Nothing here opens a transaction; every function
//! runs on the caller's connection inside `apply_batch`, `link`, `forget` or the purge.
//!
//! ```text
//!   apply_batch                      link / remember(links)         forget --purge
//!   node ops                         human claim synthesised        purge_claims_of_node
//!   insert_batch_claims  ──┐         insert_claim         ──┐       (groups that lost a member)
//!   edge ops (structural)  │                                │                 │
//!   edge diff              ▼                                ▼                 ▼
//!                       decide_groups: load_group -> Reconciler::decide -> project::apply
//! ```

pub(crate) mod er;
pub(crate) mod materialise;
pub(crate) mod persist;
pub(crate) mod project;
pub(crate) mod v1;

use crate::ontology::TypeTable;
use crate::sql_error::SqlResultExt;
use crate::sqlite::SqliteStore;
use kaban_core::{
    Actor, Assertion, AssertionKey, AssertionStatus, BatchAssertion, BatchReport, BatchSubject,
    CanonicalJson, ClaimClass, Fixed, GovernedSet, Layer, LineageParts, Method, NodeId, Ontology,
    Priors, ProtectionSet, Rank, ReconcileCtx, Reconciler, Result, StoreError, Subject,
    SupersedesClass, Verdict, WorkspaceId, config, group_key, lineage_id, stored_name,
};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

/// The frozen reconciliation policy of one store: the decision entry (EN-E9: the trait is the
/// only one), the ontology its thresholds come from, the priors table and the `[reconcile]`
/// keys. Built once at open from the shipped ontology; `with_reconciler` swaps the entry.
pub(crate) struct Setup {
    pub(crate) reconciler: Box<dyn Reconciler>,
    ontology: Arc<Ontology>,
    pub(crate) priors: Priors,
    policy: config::Reconcile,
    /// The frozen `[er]` keys (E2.10; the store has no `EngineConfig` path, so the §15.1
    /// defaults, which freeze by construction).
    pub(crate) er: kaban_core::ErPriors,
    pub(crate) recall: config::Recall,
}

impl std::fmt::Debug for Setup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Setup")
            .field("tau_type", &self.policy.tau_type)
            .field("tau_id", &self.policy.tau_id)
            .finish_non_exhaustive()
    }
}

/// The shipped ontology, resolved once per process (the store crate holds no loaded user
/// ontology; the service passes one through `SqliteStore::with_ontology`).
fn shipped_ontology() -> Result<Arc<Ontology>> {
    static SHIPPED: OnceLock<std::result::Result<Arc<Ontology>, kaban_core::OntologyError>> =
        OnceLock::new();
    SHIPPED
        .get_or_init(|| kaban_core::shipped::resolved().map(Arc::new))
        .clone()
        .map_err(StoreError::Ontology)
}

impl Setup {
    pub(crate) fn ontology(&self) -> &Ontology {
        &self.ontology
    }
    /// `WeightedVoteV1` over the shipped ontology, its `priors` block when it declares one
    /// (else the EN-F12 defaults) and the default `[reconcile]` keys. The embedded files
    /// resolve by construction (the `GovernedSet::shipped` invariant, tested there).
    #[allow(clippy::expect_used)] // documented invariant: the shipped ontology resolves
    pub(crate) fn shipped() -> Self {
        shipped_ontology()
            .and_then(Self::for_ontology)
            .expect("the shipped ontology files resolve (kaban-core build invariant)")
    }

    /// The same policy over another resolved ontology.
    pub(crate) fn for_ontology(ontology: Arc<Ontology>) -> Result<Self> {
        let priors = if ontology.priors.is_empty() {
            Priors::defaults()
        } else {
            Priors::from_values(&ontology.priors)
                .map_err(|_| StoreError::Fixed(kaban_core::FixedError))?
        };
        Ok(Self {
            reconciler: Box::new(v1::WeightedVoteV1),
            ontology,
            priors,
            policy: config::Reconcile::default(),
            recall: config::Recall::default(),
            er: kaban_core::ErPriors::from_config(&config::Er::default())
                .map_err(|_| StoreError::Fixed(kaban_core::FixedError))?,
        })
    }

    /// The same policy over another resolved ontology, its thresholds and its priors (RW5-1):
    /// the injected reconciler, the `[reconcile]` keys and the `[er]` section are kept, so
    /// the order of the store's `with_*` seams does not matter.
    pub(crate) fn set_ontology(&mut self, ontology: Arc<Ontology>) -> Result<()> {
        let fresh = Self::for_ontology(ontology)?;
        self.ontology = fresh.ontology;
        self.priors = fresh.priors;
        Ok(())
    }

    pub(crate) fn set_reconciler(&mut self, reconciler: Box<dyn Reconciler>) {
        self.reconciler = reconciler;
    }

    /// The same policy scoring entity resolution with `er` instead of the `[er]` defaults
    /// (E2.10 boundary review BR-7): the validated form of the loaded `EngineConfig.er`.
    pub(crate) fn set_er(&mut self, er: kaban_core::ErPriors) {
        self.er = er;
    }

    /// The context of one transaction's decisions (thresholds frozen to fixed point).
    pub(crate) fn ctx<'a>(&'a self, protection: &'a ProtectionSet) -> Result<ReconcileCtx<'a>> {
        ReconcileCtx::new(&self.ontology, &self.priors, &self.policy, protection)
            .map_err(StoreError::Fixed)
    }
}

/// Whether the connection carries the 0004 engine tables (`assertion`, `assertion_status`,
/// `node_anchor`): a store opened at an older schema (fixtures, benches) has no claims.
pub(crate) fn engine_tables(conn: &Connection) -> Result<bool> {
    Ok(TypeTable::of(conn)? == TypeTable::Qualified)
}

/// A conflict group as stored: the row ids, the claims and their statuses before this
/// decision, all in ascending `key` order.
#[derive(Debug, Default)]
pub(crate) struct Group {
    pub(crate) ids: Vec<i64>,
    pub(crate) claims: Vec<Assertion>,
    pub(crate) statuses: Vec<AssertionStatus>,
}

impl Group {
    fn claim_of(&self, key: &AssertionKey) -> Option<&Assertion> {
        self.claims.iter().find(|a| a.key == *key)
    }

    /// Whether a human claim joined the group since its last decision (its status is still
    /// `pending`): the one event that re-projects a protected edge row (EN-F12c "a human
    /// claim on a protected group re-projects normally").
    fn fresh_human(&self) -> bool {
        self.claims
            .iter()
            .zip(&self.statuses)
            .any(|(a, s)| a.layer == Layer::Human && *s == AssertionStatus::Pending)
    }
}

/// The subject, span chunk and object of a batch claim, resolved inside `ws` (EN-S3, F17):
/// an unknown, purged or foreign id fails the whole batch before any claim row is written.
struct Resolver<'a> {
    conn: &'a Connection,
    ws: &'a str,
    cache: HashMap<String, (i64, String)>,
}

impl Resolver<'_> {
    fn rid(&mut self, id: &NodeId) -> Result<(i64, String)> {
        if let Some(hit) = self.cache.get(id.as_str()) {
            return Ok(hit.clone());
        }
        let hit = SqliteStore::rid_kind_in_workspace(self.conn, id.as_str(), self.ws)
            .map_err(|_| StoreError::AssertionSubjectNotFound(id.to_string()))?;
        self.cache.insert(id.to_string(), hit.clone());
        Ok(hit)
    }
}

/// What inserting a batch's claims leaves for the rest of the batch.
#[derive(Debug, Default)]
pub(crate) struct Claimed {
    /// Resolved claim keys, used by complete producer-set retirement.
    pub(crate) keys: BTreeSet<String>,
    /// The group keys the batch touched, for the end-of-batch pass.
    pub(crate) touched: BTreeSet<String>,
    /// The triples the batch's `edge` claims name (`src`, `dst`, `rel`): named by the batch,
    /// so the edge diff keeps them (MD-23) whatever the pass decides.
    pub(crate) edges: Vec<(i64, i64, String)>,
}

/// Resolve and insert the batch's claims (EN-F8: immutable rows; a re-submitted key writes
/// nothing); every `edge` claim is checked against the rel's declaration as a link would be.
/// Every claim is resolved and its `supersedes` validated ([`supersession_order`]) before the
/// first claim row is written; the rows go in that dependency order.
pub(crate) fn insert_batch_claims(
    conn: &Connection,
    setup: &Setup,
    ws: &str,
    governed: &GovernedSet,
    claims: Vec<BatchAssertion>,
    now: i64,
    report: &mut BatchReport,
) -> Result<Claimed> {
    let mut resolver = Resolver {
        conn,
        ws,
        cache: HashMap::new(),
    };
    let mut out = Claimed::default();
    let mut assertions = Vec::with_capacity(claims.len());
    for claim in claims {
        let subject = match claim.subject {
            BatchSubject::Span { chunk, offset, len } => Subject::Span {
                chunk_rid: resolver.rid(&chunk)?.0,
                offset,
                len,
            },
            BatchSubject::Node(id) => Subject::Node(resolver.rid(&id)?.0),
            BatchSubject::Record(id) => Subject::Record(resolver.rid(&id)?.0),
            BatchSubject::Edge { src, rel, dst } => {
                let (srid, skind) = resolver.rid(&src)?;
                let (drid, dkind) = resolver.rid(&dst)?;
                if srid == drid {
                    return Err(StoreError::SelfLink(src.to_string()));
                }
                SqliteStore::edge_rule(conn, governed, &rel, &skind, &dkind)?;
                out.edges.push((srid, drid, rel.clone()));
                Subject::Edge {
                    src: srid,
                    rel,
                    dst: drid,
                }
            }
            // `BatchSubject` is `non_exhaustive`: a shape this store does not know names
            // nothing it can resolve
            _ => return Err(StoreError::AssertionSubjectNotFound(String::new())),
        };
        let object_rid = match &claim.object {
            Some(id) => Some(resolver.rid(id)?.0),
            None => None,
        };
        // an identity group is one unordered pair (E2.10b BR-2): both endpoints are stored
        // in rid order whatever the producer's order, and a node is never paired with itself
        let (subject, object_rid) = match (subject, object_rid, claim.claim_class) {
            (Subject::Node(s), Some(o), ClaimClass::Identity) => {
                if s == o {
                    let id = claim.object.as_ref().map(ToString::to_string);
                    return Err(StoreError::SelfLink(id.unwrap_or_default()));
                }
                (Subject::Node(s.min(o)), Some(s.max(o)))
            }
            (subject, object_rid, _) => (subject, object_rid),
        };
        let assertion = Assertion {
            key: AssertionKey::default(),
            workspace_id: WorkspaceId::new(ws),
            subject,
            claim_class: claim.claim_class,
            predicate: claim.predicate,
            object_rid,
            value: CanonicalJson::new(claim.value),
            layer: claim.layer,
            method: claim.method,
            extractor_id: claim.extractor_id,
            extractor_version: claim.extractor_version,
            source_id: claim.source_id,
            lineage_id: claim.lineage_id,
            raw_score: claim.raw_score,
            rank: claim.rank,
            rank_reason: claim.rank_reason,
            calibration_version: claim.calibration_version,
            supersedes: claim.supersedes,
            ts: now,
        }
        .with_key()?;
        // the key omits `supersedes`, so a claim can name its own key: a claim never
        // supersedes itself (the human path's filter, `human_span`)
        let mut assertion = Assertion {
            supersedes: assertion.supersedes.filter(|prior| *prior != assertion.key),
            ..assertion
        };
        persist::carry_gazetteer_retirement(conn, &mut assertion)?;
        assertions.push(assertion);
    }
    let order = supersession_order(conn, &assertions)?;
    let mut historical_groups = BTreeSet::new();
    for assertion in &assertions {
        let key = group_key(assertion);
        if !historical_groups.insert(key.clone()) {
            continue;
        }
        recover_group(conn, setup, assertion, now)?;
    }
    for i in order {
        let assertion = &assertions[i];
        out.keys.insert(assertion.key.0.clone());
        match persist::insert_claim(conn, assertion)? {
            persist::Inserted::Row(_) => report.assertions_inserted += 1,
            persist::Inserted::Existing(_) => report.assertions_noop += 1,
        }
        out.touched.insert(group_key(assertion));
    }
    Ok(out)
}

/// Recover uniquely determined historical authority before an incoming claim can vote.
pub(crate) fn recover_group(
    conn: &Connection,
    setup: &Setup,
    seed: &Assertion,
    now: i64,
) -> Result<()> {
    let (historical, restored) = persist::restore_group(conn, seed, now)?;
    if historical.claims.is_empty() {
        return Ok(());
    }
    let missing_edge = match &seed.subject {
        Subject::Edge { src, rel, dst } => {
            let present: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM edge WHERE src=?1 AND dst=?2 AND rel=?3)",
                    rusqlite::params![src, dst, rel],
                    |r| r.get(0),
                )
                .sql()?;
            if !present
                && accepted_record_rel(setup, rel)
                && persist::accepted_endpoint(conn, *src, *dst)?
            {
                let eligible: Vec<_> = v1::voting(&historical.claims)
                    .filter(|claim| !persist::retired(claim))
                    .collect();
                if eligible.len() > 1 && !eligible.iter().any(|claim| claim.layer == Layer::Human) {
                    // The immutable history cannot identify the lost accepted edge's
                    // original winner uniquely. Refuse rather than invent authority.
                    return Err(StoreError::EdgeRequiresAssertion);
                }
            }
            !present
        }
        _ => false,
    };
    if restored || missing_edge {
        decide_groups(conn, setup, &BTreeSet::from([group_key(seed)]), now, None)?;
    }
    Ok(())
}

/// Where a claim stands in the walk of [`supersession_order`].
#[derive(Clone, Copy, PartialEq, Eq)]
enum Walk {
    Unvisited,
    OnPath,
    Placed,
}

/// Validate new batch claims' `supersedes` (OWN-86 (3), EN-F8) and return the insert order:
/// the key names a claim of the same conflict group ([`kaban_core::same_group`]), stored in
/// the batch's workspace ([`persist::superseded_row`]) or earlier or later in this batch; a
/// claim of the batch is inserted after the batch claim it supersedes, so a forward reference
/// resolves inside the batch only; the batch's references never form a cycle. Nothing is
/// written here: any refusal fails the whole batch first. Already-stored keys are checked
/// for tenant membership but remain no-op replays even after their old target is purged.
fn supersession_order(conn: &Connection, claims: &[Assertion]) -> Result<Vec<usize>> {
    // the first claim of each key (a repeated key is the same claim and writes nothing)
    let mut first: HashMap<&str, usize> = HashMap::with_capacity(claims.len());
    for (i, claim) in claims.iter().enumerate() {
        first.entry(claim.key.0.as_str()).or_insert(i);
    }
    // the batch claim each claim supersedes, when the target is in the batch
    let mut target = Vec::with_capacity(claims.len());
    for claim in claims {
        if persist::existing_claim(conn, claim)?.is_some() {
            target.push(None);
            continue;
        }
        target.push(match &claim.supersedes {
            None => None,
            Some(key) => match first.get(key.0.as_str()) {
                Some(&j) if !kaban_core::same_group(&claims[j], claim) => {
                    return Err(StoreError::SupersedesInvalid(SupersedesClass::CrossGroup));
                }
                Some(&j) => Some(j),
                None => {
                    persist::superseded_row(conn, claim, key)?;
                    None
                }
            },
        });
    }
    // each claim supersedes at most one: follow each chain to a placed claim or its end,
    // then place the chain target first; meeting the walk's own path is a cycle
    let mut walk = vec![Walk::Unvisited; claims.len()];
    let mut order = Vec::with_capacity(claims.len());
    let mut path = Vec::new();
    for start in 0..claims.len() {
        let mut at = Some(start);
        while let Some(i) = at {
            match walk[i] {
                Walk::Placed => break,
                Walk::OnPath => {
                    return Err(StoreError::SupersedesInvalid(SupersedesClass::Cycle));
                }
                Walk::Unvisited => {
                    walk[i] = Walk::OnPath;
                    path.push(i);
                    at = target[i];
                }
            }
        }
        while let Some(i) = path.pop() {
            walk[i] = Walk::Placed;
            order.push(i);
        }
    }
    Ok(order)
}

/// A human edge claim (EN-F9 "a direct `link` produces one human assertion and one edge"):
/// what `link`, the inline links of `remember` and the M5 import say about a triple.
pub(crate) struct HumanEdge<'a> {
    pub(crate) src: i64,
    pub(crate) dst: i64,
    pub(crate) rel: &'a str,
    /// The caller's confidence; `None` is full confidence (today's `link` default).
    pub(crate) confidence: Option<f64>,
    /// The gated edge props the caller passed (`{}` when none); the claim's value.
    pub(crate) props: Value,
    pub(crate) method: Method,
    pub(crate) actor: &'a Actor,
}

/// Record a human edge claim and decide its group now (the human path of `link_in`).
pub(crate) fn human_edge(
    conn: &Connection,
    setup: &Setup,
    ws: &str,
    edge: HumanEdge<'_>,
    now: i64,
) -> Result<()> {
    let raw_score = match edge.confidence {
        Some(c) => Fixed::from_f64(c)?,
        None => Fixed::ONE,
    };
    // the actor's standing lineage (EN-F12b `human:<actor>`): an identical repeated `link` is
    // the same claim and writes nothing (EN-F12a N5); the rank makes it vote alone (EN-F12c)
    let lineage_id = lineage_id(
        Layer::Human,
        &LineageParts::Human {
            actor: actor_name(edge.actor),
            event_id: None,
        },
    );
    let mut assertion = Assertion {
        key: AssertionKey::default(),
        workspace_id: WorkspaceId::new(ws),
        subject: Subject::Edge {
            src: edge.src,
            rel: edge.rel.to_owned(),
            dst: edge.dst,
        },
        claim_class: ClaimClass::Edge,
        predicate: None,
        object_rid: None,
        value: CanonicalJson::new(edge.props),
        layer: Layer::Human,
        method: edge.method,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id,
        raw_score,
        rank: Rank::Preferred,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
        ts: now,
    }
    .with_key()?;
    let (mut heads, had_history) = standing_human_edge(conn, &assertion)?;
    let prior = if heads.is_empty() {
        None
    } else {
        Some(heads.remove(0))
    };
    if let Some(prior) = &prior
        && heads.is_empty()
        && prior.value == assertion.value
        && prior.raw_score == assertion.raw_score
        && prior.method == assertion.method
    {
        return decide_human_claim(conn, setup, prior, now);
    }
    if !had_history && persist::existing_claim(conn, &assertion)?.is_none() {
        // Preserve the legacy first-action identity; only an actual changed action needs
        // an event-qualified lineage. The claim value remains the caller's semantic props.
        return decide_human_claim(conn, setup, &assertion, now);
    }
    // An explicit human action consolidates legacy same-actor heads only. Keep the
    // newest head for supersession and immutable payload/history on every older row.
    // Rank changes can emit ChangeLog rows, so complete them before binding the new event.
    for head in heads {
        conn.execute(
            "UPDATE assertion SET rank='deprecated',rank_reason=?2 WHERE key=?1 AND rank<>'deprecated'",
            rusqlite::params![head.key.0, HUMAN_WINNER_REASON],
        ).sql()?;
    }
    // Every caller holds an IMMEDIATE write transaction. Nothing writes between this read
    // and insert_claim: assertion_ai records this actual action at the next ChangeLog seq.
    // Check the emitted row before deciding; a rollback keeps claim and event indivisible.
    let sequence: i64 = conn
        .query_row("SELECT COALESCE(max(seq),0)+1 FROM changes", [], |r| {
            r.get(0)
        })
        .sql()?;
    let event_id = u64::try_from(sequence)
        .map_err(|_| crate::sql_error::backend_error(rusqlite::Error::InvalidQuery))?;
    assertion.lineage_id = kaban_core::lineage_id(
        Layer::Human,
        &LineageParts::Human {
            actor: actor_name(edge.actor),
            event_id: Some(event_id),
        },
    );
    assertion.supersedes = prior.map(|claim| claim.key);
    assertion = assertion.with_key()?;
    let persist::Inserted::Row(id) = persist::insert_claim(conn, &assertion)? else {
        return Err(crate::sql_error::backend_error(
            rusqlite::Error::InvalidQuery,
        ));
    };
    let recorded: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM changes WHERE seq=?1 AND entity='assertion' AND op='insert' AND id=?2 AND workspace_id=?3)", rusqlite::params![sequence, id.to_string(), ws], |r| r.get(0)).sql()?;
    if !recorded {
        return Err(crate::sql_error::backend_error(
            rusqlite::Error::InvalidQuery,
        ));
    }
    let mut groups = BTreeSet::new();
    groups.insert(group_key(&assertion));
    decide_groups(conn, setup, &groups, now, None)
}

/// Eligible standing action of this actor alone, using the existing triple index.
fn standing_human_edge(conn: &Connection, claim: &Assertion) -> Result<(Vec<Assertion>, bool)> {
    let Subject::Edge { src, rel, dst } = &claim.subject else {
        return Ok((Vec::new(), false));
    };
    let mut statement = conn.prepare_cached("SELECT a.id,a.lineage_id,a.rank<>'deprecated' AND NOT EXISTS(SELECT 1 FROM assertion b WHERE b.supersedes=a.id) FROM assertion a WHERE a.workspace_id=?1 AND a.subject_kind='edge' AND a.subject_rid=?2 AND a.predicate=?3 AND a.object_rid=?4 AND a.claim_class='edge' AND a.layer='human' ORDER BY a.id DESC").sql()?;
    let rows = statement
        .query_map(
            rusqlite::params![claim.workspace_id.as_str(), src, rel, dst],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, bool>(2)?,
                ))
            },
        )
        .sql()?;
    let mut had_history = false;
    let mut heads = Vec::new();
    let event_prefix = format!("{}:", claim.lineage_id);
    for row in rows {
        let (id, lineage, eligible) = row.sql()?;
        let same_actor = lineage == claim.lineage_id
            || lineage.strip_prefix(&event_prefix).is_some_and(|suffix| {
                suffix.bytes().all(|byte| byte.is_ascii_digit()) && suffix.parse::<u64>().is_ok()
            });
        if same_actor {
            had_history = true;
            if eligible && let Some(head) = persist::load_claim(conn, id)? {
                heads.push(head);
            }
        }
    }
    Ok((heads, had_history))
}

/// A human verdict on a span's conflict group (engine-plan E2.5 `resolve --winner`): the
/// span twin of [`HumanEdge`]. The subject, class and predicate are the group's own (read
/// from a stored claim of the group), so the claim votes in that group and nowhere else.
pub(crate) struct HumanSpan<'a> {
    pub(crate) subject: Subject,
    pub(crate) claim_class: ClaimClass,
    pub(crate) predicate: Option<String>,
    /// The value the human names as the winner (gated by the caller).
    pub(crate) value: Value,
    pub(crate) actor: &'a Actor,
    /// The same person's standing claim this one replaces (RW4-2: a second `resolve
    /// --symbol` supersedes the first, so the group's winner is the key the store writes);
    /// `None` when there is none. A claim never supersedes itself.
    pub(crate) supersedes: Option<AssertionKey>,
}

/// The `rank_reason` of a machine `preferred` claim a human verdict retired (`resolve
/// --winner`, EN-F12c; RE4-7): a human reason, so [`persist::retired`] leaves the row a
/// non-voting supporter whose status follows the verdict, its history kept.
pub(crate) const HUMAN_WINNER_REASON: &str = "human_winner";

/// Record a human `preferred` span claim and decide its group now (EN-F12c: the human rank
/// votes alone, so the group projects the named value as its winner in this transaction).
/// A machine `preferred` row of the group would turn the vote into a `preferred_conflict`
/// (EN-F12b), so such rows are marked `deprecated` under [`HUMAN_WINNER_REASON`] first, in
/// the same transaction; no row is deleted.
pub(crate) fn human_span(
    conn: &Connection,
    setup: &Setup,
    ws: &str,
    span: HumanSpan<'_>,
    now: i64,
) -> Result<()> {
    let lineage_id = lineage_id(
        Layer::Human,
        &LineageParts::Human {
            actor: actor_name(span.actor),
            event_id: None,
        },
    );
    let assertion = Assertion {
        key: AssertionKey::default(),
        workspace_id: WorkspaceId::new(ws),
        subject: span.subject,
        claim_class: span.claim_class,
        predicate: span.predicate,
        object_rid: None,
        value: CanonicalJson::new(span.value),
        layer: Layer::Human,
        method: Method::Link,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id,
        raw_score: Fixed::ONE,
        rank: Rank::Preferred,
        rank_reason: None,
        calibration_version: 0,
        supersedes: span.supersedes,
        ts: now,
    }
    .with_key()?;
    let assertion = Assertion {
        // the same person naming the same value again is the same claim (the key omits
        // `supersedes`), and a claim never supersedes itself
        supersedes: assertion.supersedes.filter(|prior| *prior != assertion.key),
        ..assertion
    };
    conn.execute(
        "UPDATE assertion SET rank = 'deprecated', rank_reason = ?2
         WHERE layer <> 'human' AND rank = 'preferred'
           AND id IN (SELECT assertion_id FROM assertion_status WHERE group_key = ?1)",
        rusqlite::params![group_key(&assertion), HUMAN_WINNER_REASON],
    )
    .sql()?;
    decide_human_claim(conn, setup, &assertion, now)
}

/// A human acceptance as a claim about the record's kind (EN-F12c "every human `accept`
/// creates a `preferred` human assertion in the same transaction"): what `transition` to
/// `accepted` says about a governed record.
pub(crate) struct HumanRecord<'a> {
    pub(crate) rid: i64,
    /// The record's stored kind.
    pub(crate) kind: &'a str,
    /// The record's props as stored; with the kind, the claim's `{kind, props}` value.
    pub(crate) props: &'a Value,
    pub(crate) actor: &'a Actor,
    /// The `events.seq` of the acceptance (EN-F12b `human:<actor>:<event_id>`).
    pub(crate) event_id: u64,
}

/// Record a human `kind_of_record` claim for an accepted record and decide its group now
/// (the `accept` path of `transition`): a later machine value for the record's kind is left
/// `unresolved` against it.
pub(crate) fn human_record(
    conn: &Connection,
    setup: &Setup,
    ws: &str,
    record: &HumanRecord<'_>,
    now: i64,
) -> Result<()> {
    let lineage_id = lineage_id(
        Layer::Human,
        &LineageParts::Human {
            actor: actor_name(record.actor),
            event_id: Some(record.event_id),
        },
    );
    let assertion = Assertion {
        key: AssertionKey::default(),
        workspace_id: WorkspaceId::new(ws),
        subject: Subject::Record(record.rid),
        claim_class: ClaimClass::KindOfRecord,
        predicate: None,
        object_rid: None,
        value: CanonicalJson::new(json!({"kind": record.kind, "props": record.props})),
        layer: Layer::Human,
        method: Method::Accept,
        extractor_id: None,
        extractor_version: None,
        source_id: None,
        lineage_id,
        raw_score: Fixed::ONE,
        rank: Rank::Preferred,
        rank_reason: None,
        calibration_version: 0,
        supersedes: None,
        ts: now,
    }
    .with_key()?;
    decide_human_claim(conn, setup, &assertion, now)
}

/// The actor's stored spelling in a human lineage (`human:cli`, `human:agent`).
fn actor_name(actor: &Actor) -> &'static str {
    match actor {
        Actor::HumanCli => "cli",
        Actor::Agent => "agent",
    }
}

/// Insert a human claim and decide its group now: the tail every human claim shares.
fn decide_human_claim(
    conn: &Connection,
    setup: &Setup,
    assertion: &Assertion,
    now: i64,
) -> Result<()> {
    recover_group(conn, setup, assertion, now)?;
    persist::insert_claim(conn, assertion)?;
    let mut groups = BTreeSet::new();
    groups.insert(group_key(assertion));
    decide_groups(conn, setup, &groups, now, None)
}

/// Decide `groups` in ascending `group_key` order over every stored supporter of each
/// (EN-F12), project the verdicts and count them into `report` when one is given.
pub(crate) fn decide_groups(
    conn: &Connection,
    setup: &Setup,
    groups: &BTreeSet<String>,
    now: i64,
    report: Option<&mut BatchReport>,
) -> Result<()> {
    decide_groups_without(conn, setup, groups, &BTreeSet::new(), now, report)
}

/// [`decide_groups`] with the claim rows in `excluded` kept out of the vote and out of the
/// projection: the rows this transaction retires (the policy rescan's masked claims, EN-S9;
/// a forgotten node's claims). Every row the engine retired earlier (`persist::retired`:
/// `deprecated` under a machine reason, RE4-1) is folded in from the loaded rows, so its
/// status stays `rejected` across every later decide and across a replay. A group whose
/// every supporter is out is projected as `Rejected` (`policy_reject`), so a materialised
/// edge it owned is removed. A `rejected` status set by the reconciler itself (a v1
/// structural reject) is not an exclusion: such a group is decided again as before.
pub(crate) fn decide_groups_without(
    conn: &Connection,
    setup: &Setup,
    groups: &BTreeSet<String>,
    excluded: &BTreeSet<i64>,
    now: i64,
    mut report: Option<&mut BatchReport>,
) -> Result<()> {
    if groups.is_empty() {
        return Ok(());
    }
    for key in groups {
        let stored = persist::load_group(conn, key)?;
        if stored.claims.is_empty() {
            continue;
        }
        let mut group = Group::default();
        for ((id, claim), status) in stored.ids.iter().zip(&stored.claims).zip(&stored.statuses) {
            if excluded.contains(id) {
                continue;
            }
            if persist::retired(claim) {
                // out of the vote for good; `rejected` whatever a replay re-created
                persist::set_status(conn, *id, AssertionStatus::Rejected, now)?;
                continue;
            }
            group.ids.push(*id);
            group.claims.push(claim.clone());
            group.statuses.push(*status);
        }
        let protection = protection_of(conn, setup, key, &group)?;
        // a group deciding without a supporter it had until this transaction re-projects
        // its row whatever the protection says (RE4-2): bytes copied from the lost supporter
        // never outlive it. A row retired earlier is not news: the row was re-projected then.
        let lost_member = stored.ids.iter().any(|id| excluded.contains(id));
        let verdict = if group.claims.is_empty() {
            // nothing left to vote: the excluded rows are `rejected` already, so the
            // projection of the whole stored group only removes what it materialised
            group = stored;
            Verdict::Rejected {
                reason: "policy_reject".into(),
            }
        } else {
            let ctx = setup.ctx(&protection)?;
            er::identity_conflict(
                human_protected(setup.reconciler.decide(&group.claims, &ctx), &group),
                &group,
            )
        };
        let disposition = project::apply(
            conn,
            setup,
            &group,
            key,
            &verdict,
            &protection,
            lost_member,
            now,
        )?;
        if let Some(r) = report.as_deref_mut() {
            *r.verdicts.entry(disposition).or_insert(0) += 1;
        }
    }
    Ok(())
}

/// Rels whose edge to an accepted governed record is protected whatever the ontology says
/// (EN-F12c), beside every `governed_semantics: true` rel of the loaded one.
const ACCEPTED_RECORD_RELS: [&str; 3] = ["GOVERNS", "ABOUT", "EVIDENCE_FOR"];

/// The protection set of one group before its decision (EN-F12c): the human claim that wins
/// it now, and whether its materialised edge touches an accepted governed record over a rel
/// whose semantics a layer may not rewrite.
pub(crate) fn protection_of(
    conn: &Connection,
    setup: &Setup,
    key: &str,
    group: &Group,
) -> Result<ProtectionSet> {
    let human_winner = persist::human_winner(conn, key)?;
    let accepted_record_edge = match group.claims.first().map(|a| &a.subject) {
        Some(Subject::Edge { src, rel, dst }) => {
            accepted_record_rel(setup, rel)
                && persist::edge_touches_accepted_record(conn, *src, *dst, rel)?
        }
        _ => false,
    };
    Ok(ProtectionSet {
        human_winner,
        accepted_record_edge,
    })
}

/// Whether an edge over `rel` into an accepted governed record is protected (EN-F12c): the
/// fixed rels, or a `governed_semantics: true` rel of the loaded ontology.
fn accepted_record_rel(setup: &Setup, rel: &str) -> bool {
    ACCEPTED_RECORD_RELS.contains(&rel)
        || setup
            .ontology
            .rels
            .iter()
            .any(|(q, r)| r.governed_semantics && stored_name(q) == rel)
}

/// Whether a producer's bare edge op may not touch the triple `src -rel-> dst` (RE4-5, D6):
/// a human claim wins its conflict group, or its materialised edge touches an accepted
/// governed record over a protected rel. Such a triple is written by projection only.
pub(crate) fn triple_protected(
    conn: &Connection,
    setup: &Setup,
    src: i64,
    dst: i64,
    rel: &str,
) -> Result<bool> {
    let group: Option<String> = conn
        .prepare_cached(
            "SELECT s.group_key FROM assertion a JOIN assertion_status s ON s.assertion_id = a.id
             WHERE a.subject_kind = 'edge' AND a.subject_rid = ?1 AND a.object_rid = ?2 AND a.predicate = ?3
             LIMIT 1",
        )
        .sql()?
        .query_row(rusqlite::params![src, dst, rel], |r| r.get(0))
        .optional()
        .sql()?;
    if let Some(key) = group
        && persist::human_winner(conn, &key)?.is_some()
    {
        return Ok(true);
    }
    Ok(accepted_record_rel(setup, rel)
        && persist::edge_touches_accepted_record(conn, src, dst, rel)?)
}

/// Whether any stored claim names the triple `src -rel-> dst` (RE4-5): a claimed triple is
/// the projection's to write, a bare producer op on it is skipped.
pub(crate) fn triple_claimed(conn: &Connection, src: i64, dst: i64, rel: &str) -> Result<bool> {
    conn.prepare_cached(
        "SELECT EXISTS(SELECT 1 FROM assertion WHERE subject_kind = 'edge' AND subject_rid = ?1 AND object_rid = ?2 AND predicate = ?3)",
    )
    .sql()?
    .query_row(rusqlite::params![src, dst, rel], |r| r.get(0))
    .sql()
}

/// A human leader wins its review (OD-E-4 "a human link is never overwritten"; the companion
/// of EN-F12c's `ProtectionSet`, which guards an existing win): a group v1 leaves for review
/// whose leading candidate is a `human` claim is projected as that claim's win. Without it
/// every human `link` on a governed-semantics or thresholded rel (`SUPERSEDES`, EN-S10) would
/// write no edge, which EN-S10 reserves for the layers, not for the human.
pub(crate) fn human_protected(verdict: Verdict, group: &Group) -> Verdict {
    let Verdict::Candidate { ranked, .. } = &verdict else {
        return verdict;
    };
    let Some((top, confidence)) = ranked.first() else {
        return verdict;
    };
    let Some(leader) = group.claim_of(top) else {
        return verdict;
    };
    if leader.layer != Layer::Human {
        return verdict;
    }
    let margin = ranked.get(1).map_or(Fixed::ONE, |(_, second)| {
        Fixed(confidence.0.saturating_sub(second.0))
    });
    Verdict::Winner {
        assertion_key: top.clone(),
        confidence: *confidence,
        margin,
        dissent: group
            .claims
            .iter()
            .filter(|a| !v1::same_value(a, leader))
            .map(|a| a.key.clone())
            .collect(),
    }
}

/// The disposition every supporter of a group receives under a verdict, apart from the
/// winner itself (`project::apply` sets the representative to `winner`).
pub(crate) fn disposition_of(verdict: &Verdict) -> AssertionStatus {
    match verdict {
        Verdict::Winner { .. } => AssertionStatus::Dissent,
        Verdict::Candidate { .. } => AssertionStatus::Candidate,
        Verdict::Unresolved { .. } => AssertionStatus::Unresolved,
        // `Verdict` is `non_exhaustive`: an unknown verdict projects nothing
        Verdict::Rejected { .. } | _ => AssertionStatus::Rejected,
    }
}

/// The key of `actor`'s standing `symbol` re-target claim on node `rid` of `ws` (RW4-2): the
/// latest human `prop_value` claim of the actor's own lineage that is neither `deprecated` nor
/// superseded, if any. Another person's claim is never returned.
fn standing_retarget(
    conn: &Connection,
    ws: &str,
    rid: i64,
    actor: &Actor,
) -> Result<Option<AssertionKey>> {
    let lineage = lineage_id(
        Layer::Human,
        &LineageParts::Human {
            actor: actor_name(actor),
            event_id: None,
        },
    );
    let key: Option<String> = conn
        .query_row(
            "SELECT a.key FROM assertion a
             WHERE a.workspace_id = ?1 AND a.subject_kind = 'node' AND a.subject_rid = ?2
               AND a.claim_class = 'prop_value' AND a.predicate = 'symbol' AND a.layer = 'human'
               AND a.lineage_id = ?3 AND a.rank <> 'deprecated'
               AND NOT EXISTS (SELECT 1 FROM assertion b WHERE b.supersedes = a.id)
             ORDER BY a.ts DESC, a.id DESC LIMIT 1",
            rusqlite::params![ws, rid, lineage],
            |r| r.get(0),
        )
        .optional()
        .sql()?;
    Ok(key.map(AssertionKey))
}

/// `IngestStore::resolve_symbol` (engine-plan E2.5 `kaban resolve <coderef-id> --symbol
/// <key>`, SO-6): one transaction over `conn` that records the human `prop_value` claim
/// (predicate `symbol`, subject the node) through [`human_span`]'s twin shape, decides its
/// group, writes `key` into the node's `props.symbol` (`rev` bumped, `updated_at` set) and
/// brings the anchor rows to the new key (`anchors::maintain` deletes the old `symbol` row and
/// inserts the new one; EN-F10), then appends `ctx.audit`. The node must be an active
/// `CodeRef` of `ws`: anything else is `NotFound` (S6: no existence leak across workspaces).
/// The caller has judged the actor and the key's form; nothing here reads either.
pub(crate) fn resolve_symbol(
    conn: &mut Connection,
    setup: &Setup,
    signer: &dyn kaban_policy::AuditSigner,
    ws: &str,
    node: &NodeId,
    key: &str,
    ctx: &kaban_core::WriteCtx,
) -> Result<()> {
    let tx = conn
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .sql()?;
    if !engine_tables(&tx)? {
        return Err(StoreError::NotFound(node.to_string()));
    }
    let row: Option<(i64, Option<String>, String, String)> = tx
        .query_row(
            "SELECT rid, repo, props, path_scope FROM node
             WHERE id = ?1 AND workspace_id = ?2 AND kind = 'CodeRef' AND lifecycle = 'active'",
            rusqlite::params![node.as_str(), ws],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()
        .sql()?;
    let Some((rid, repo, props, path_scope)) = row else {
        return Err(StoreError::NotFound(node.to_string()));
    };
    let mut props: Value = serde_json::from_str(&props)?;
    let path_scope: Vec<String> = serde_json::from_str(&path_scope)?;
    // a props column that does not hold a JSON object is a corrupted row (RB-5): refuse to
    // guess, before anything is written
    let Value::Object(map) = &mut props else {
        return Err(StoreError::NotFound(node.to_string()));
    };
    map.insert("symbol".to_owned(), Value::String(key.to_owned()));
    let now = persist::now_unix();
    // RW4-2: the person's standing re-target of this node is superseded by the new claim in
    // this transaction, so the group's winner is the key written below; a claim of another
    // human authority is left to vote (a conflict between people stays one)
    let prior = standing_retarget(&tx, ws, rid, &ctx.actor)?;
    human_span(
        &tx,
        setup,
        ws,
        HumanSpan {
            subject: Subject::Node(rid),
            claim_class: ClaimClass::PropValue,
            predicate: Some("symbol".to_owned()),
            value: Value::String(key.to_owned()),
            actor: &ctx.actor,
            supersedes: prior,
        },
        now,
    )?;
    tx.execute(
        "UPDATE node SET props = ?1, updated_at = ?2, rev = rev + 1 WHERE rid = ?3",
        rusqlite::params![serde_json::to_string(&props)?, kaban_core::now_iso(), rid],
    )
    .sql()?;
    crate::anchors::maintain(
        &tx,
        &crate::anchors::NodeFacts {
            rid,
            workspace_id: ws,
            kind: "CodeRef",
            repo: repo.as_deref(),
            props: &props,
            path_scope: &path_scope,
        },
        &ctx.governed,
    )?;
    crate::audit_table::append(&tx, signer, &ctx.audit)?;
    crate::changelog::record_terminal(&tx, ctx)?;
    tx.commit().sql()?;
    Ok(())
}
