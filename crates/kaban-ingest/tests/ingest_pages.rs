//! Serialized delivery bounds; full job revalidation is exercised by the runtime tests.
#![allow(clippy::unwrap_used)] // test assertions

use kaban_api::ingest_jobs::ChunkView;
use kaban_core::JobId;
use kaban_ingest::jobs::next::{PageBudget, PageError, slice_chunk};
mod common;

fn persisted_fixture(name: &str) -> (common::real::Rig, kaban_api::KabanApi, JobId) {
    persisted_fixture_body(
        name,
        "# Delivery\n\nA harmless source paragraph for incremental delivery.\n",
    )
}

fn persisted_fixture_body(
    name: &str,
    body: &str,
) -> (common::real::Rig, kaban_api::KabanApi, JobId) {
    persisted_fixture_with(name, body, false)
}

fn persisted_fixture_with(
    name: &str,
    body: &str,
    on_disk: bool,
) -> (common::real::Rig, kaban_api::KabanApi, JobId) {
    use kaban_core::{IngestJob, IngestStore, JobStatus, MemoryStore, NodeId};
    use kaban_ingest::jobs::types::{JobChunk, JobEnvelope, PreparationOptions};
    let rig = if on_disk {
        common::real::rig_on_disk("kaban-e38-next", name, kaban_policy::Policy::empty())
    } else {
        common::real::rig("kaban-e38-next", name)
    };
    std::fs::write(rig.root.join("a.md"), body).unwrap();
    assert_eq!(rig.run().documents, 1);
    let api = kaban_api::KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
    );
    let document = rig.document("a.md").unwrap();
    let props: kaban_ingest::DocumentProps =
        serde_json::from_value(document.props.clone()).unwrap();
    let chunks = rig
        .chunk_ids("a.md")
        .iter()
        .enumerate()
        .map(|(index, id)| {
            let id = NodeId::parse(id).unwrap();
            let mut node = rig.store.get(&id, false).unwrap().unwrap();
            node.props["text"] = serde_json::Value::String(node.text);
            let props: kaban_ingest::ChunkProps = serde_json::from_value(node.props).unwrap();
            JobChunk {
                n: u32::try_from(index).unwrap(),
                id,
                byte_start: usize::try_from(props.byte_start).unwrap(),
                byte_end: usize::try_from(props.byte_end).unwrap(),
                token_estimate: usize::try_from(props.token_estimate).unwrap(),
                kind: props.kind,
                oversize: props.oversize,
                slices: u32::from(props.slices),
            }
        })
        .collect();
    let ontology_hash = blake3::hash(&serde_json::to_vec(&api.registry().list()).unwrap())
        .to_hex()
        .to_string();
    let envelope = JobEnvelope {
        v: 1,
        lease: None,
        config_hash: String::new(),
        document_id: NodeId::parse(&document.id).unwrap(),
        ontology_hash,
        policy_fingerprint: api.policy_version(),
        source_bytes_hmac: kaban_ingest::keyed_id(
            rig.store.signer().as_ref(),
            "bytes",
            body.as_bytes(),
        ),
        target_schema: serde_json::json!({}),
        allowed_rels: vec![],
        chunks,
        options: PreparationOptions {
            link: "none".into(),
            symbols: false,
        },
        submit_state: serde_json::json!({}),
        finished_report_json: None,
    };
    let job = JobId::from_digest(&[8; 32]);
    rig.store
        .job_open(&IngestJob {
            job: job.clone(),
            workspace: api.workspace().clone(),
            source_id: props.source_id,
            relative_path: "a.md".into(),
            content_hash: props.content_hash.unwrap(),
            pipeline_version: "1".into(),
            chunk_config_hash: "default".into(),
            mode: "structured".into(),
            dialect: "none".into(),
            doc_type: "generic".into(),
            kinds: vec!["Fact".into()],
            plan_json: envelope.encode().unwrap(),
            named_edges: vec![],
            submits: 0,
            created: 0,
            updated: 0,
            unchanged: 0,
            redactions: 0,
            status: JobStatus::Open,
            created_at: 0,
            expires_at: i64::MAX,
            finished_at: None,
        })
        .unwrap();
    (rig, api, job)
}

fn chunk(text: String) -> ChunkView {
    ChunkView {
        n: 0,
        id: "01a17a03-51a8-7000-8000-000000000001".into(),
        kind: kaban_api::ingest_jobs::IngestChunkKind::Code,
        section_path: vec!["A heading".into()],
        context_prefix: "a safe prefix".into(),
        byte_start: 11,
        byte_end: 11 + u64::try_from(text.len()).unwrap(),
        text,
        slice: 0,
        slices: 1,
    }
}

#[test]
fn default_two_hundred_kib_chunk_has_seven_exact_slices() {
    let job = JobId::from_digest(&[1; 32]);
    let source = chunk("x".repeat(200 * 1024));
    let pages = slice_chunk(&job, &source, None, PageBudget::default()).unwrap();
    assert_eq!(pages.len(), 7);
    assert_eq!(
        pages
            .iter()
            .map(|page| page.chunk.text.as_str())
            .collect::<String>(),
        source.text
    );
    for (index, page) in pages.iter().enumerate() {
        assert!(serde_json::to_vec(page).unwrap().len() <= 64 * 1024);
        assert_eq!(page.chunk.slice, u32::try_from(index).unwrap());
        assert_eq!(page.chunk.slices, 7);
    }
    assert!(pages.last().unwrap().next.is_none());
}

#[test]
fn escaping_and_utf8_shrink_slices_against_complete_wire_bytes() {
    let job = JobId::from_digest(&[2; 32]);
    let source = chunk("\u{0}\"\\🦀".repeat(16_000));
    let budget = PageBudget {
        max_result_bytes: 16 * 1024,
        metadata_allowance: 4096,
    };
    let pages = slice_chunk(&job, &source, Some(1), budget).unwrap();
    assert_eq!(
        pages
            .iter()
            .map(|page| page.chunk.text.as_str())
            .collect::<String>(),
        source.text
    );
    for page in &pages {
        assert!(serde_json::to_vec(page).unwrap().len() <= budget.max_result_bytes);
        assert_eq!(page.chunk.byte_start, source.byte_start);
        assert_eq!(page.chunk.byte_end, source.byte_end);
    }
    assert_eq!(pages.last().unwrap().next.unwrap().n, 1);
    assert_eq!(pages, slice_chunk(&job, &source, Some(1), budget).unwrap());
}

#[test]
fn impossible_metadata_and_invalid_offsets_refuse_without_partial_pages() {
    let job = JobId::from_digest(&[3; 32]);
    let mut source = chunk("🦀".into());
    let budget = PageBudget {
        max_result_bytes: 10,
        metadata_allowance: 0,
    };
    assert_eq!(
        slice_chunk(&job, &source, None, budget),
        Err(PageError::Budget)
    );
    source.byte_end = source.byte_start - 1;
    assert_eq!(
        slice_chunk(&job, &source, None, PageBudget::default()),
        Err(PageError::Contract)
    );
}

#[tokio::test]
async fn next_reopens_every_call_and_source_edits_only_mark_registry_stale() {
    use kaban_core::{IngestStore, JobStatus};
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::JobError;
    // Fixture setup uses a separate runtime through the existing real-pipeline rig.
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("reopen"))
        .await
        .unwrap();
    let before = rig.outbox_len();
    let first = next(&api, &rig.root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap();
    assert!(first.chunk.text.contains("harmless"));
    assert_eq!(
        first,
        next(&api, &rig.root, &job, 0, 0, PageBudget::default())
            .await
            .unwrap()
    );
    assert_eq!(
        rig.outbox_len(),
        before,
        "next republished the document graph"
    );
    std::fs::write(rig.root.join("a.md"), "# Edited\n\nA changed source.\n").unwrap();
    assert!(matches!(
        next(&api, &rig.root, &job, 0, 0, PageBudget::default()).await,
        Err(JobError::Stale)
    ));
    assert_eq!(
        rig.store
            .job_get(&job, api.workspace())
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Stale
    );
    assert_eq!(
        rig.outbox_len(),
        before,
        "stale next changed document/chunk rows"
    );
}

#[tokio::test]
async fn next_foreign_unknown_and_invalid_selectors_never_deliver_source() {
    use kaban_api::{ApiConfig, KabanApi};
    use kaban_core::WorkspaceId;
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::JobError;
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("scope"))
        .await
        .unwrap();
    let foreign = KabanApi::with_config(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
        ApiConfig {
            workspace: WorkspaceId::new("foreign"),
            ..ApiConfig::default()
        },
    );
    let unknown = JobId::from_digest(&[9; 32]);
    let a = next(&foreign, &rig.root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap_err();
    let b = next(&foreign, &rig.root, &unknown, 0, 0, PageBudget::default())
        .await
        .unwrap_err();
    assert_eq!(a.to_string(), b.to_string());
    assert_eq!(a.exit_code(), 3);
    assert!(matches!(
        next(&api, &rig.root, &job, 50, 0, PageBudget::default()).await,
        Err(JobError::Selector)
    ));
    assert!(matches!(
        next(&api, &rig.root, &job, 0, 50, PageBudget::default()).await,
        Err(JobError::Selector)
    ));
}

#[tokio::test]
async fn next_applies_changed_policy_without_republishing_source() {
    use kaban_ingest::jobs::next::next;
    use kaban_policy::Policy;
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("policy"))
        .await
        .unwrap();
    let before = rig.outbox_len();
    let file = Policy::parse(b"words:\n  - { term: harmless, action: redact, id: current-mask }\n")
        .unwrap();
    *api.policy_handle().write().unwrap() = Policy::from_file(file, "changed".into()).unwrap();
    let page = next(&api, &rig.root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap();
    let bytes = serde_json::to_vec(&page).unwrap();
    assert!(!String::from_utf8(bytes).unwrap().contains("harmless"));
    assert!(page.chunk.text.contains("REDACTED"));
    assert_eq!(rig.outbox_len(), before);
}

#[tokio::test]
async fn next_distinguishes_present_expiry_and_detects_ontology_changes() {
    use kaban_core::{IngestStore, JobStatus};
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::{JobEnvelope, JobError};
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("expiry"))
        .await
        .unwrap();
    let original = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    let mut expired = original.clone();
    expired.job = JobId::from_digest(&[10; 32]);
    expired.expires_at = 1;
    rig.store.job_open(&expired).unwrap();
    assert!(matches!(
        next(&api, &rig.root, &expired.job, 0, 0, PageBudget::default()).await,
        Err(JobError::Expired)
    ));
    let mut changed = original;
    changed.job = JobId::from_digest(&[11; 32]);
    let mut envelope = JobEnvelope::decode(&changed.plan_json).unwrap();
    envelope.ontology_hash = "an obsolete ontology snapshot".into();
    changed.plan_json = envelope.encode().unwrap();
    rig.store.job_open(&changed).unwrap();
    let before = rig.outbox_len();
    assert!(matches!(
        next(&api, &rig.root, &changed.job, 0, 0, PageBudget::default()).await,
        Err(JobError::Stale)
    ));
    assert_eq!(
        rig.store
            .job_get(&changed.job, api.workspace())
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Stale
    );
    assert_eq!(rig.outbox_len(), before);
}

#[tokio::test]
async fn real_oversized_fence_delivers_seven_exact_slices() {
    use kaban_core::MemoryStore;
    use kaban_ingest::jobs::next::next;
    let body = format!("# Fence\n\n```text\n{}\n```\n", "x".repeat(200 * 1024));
    let (rig, api, job) =
        tokio::task::spawn_blocking(move || persisted_fixture_body("fence", &body))
            .await
            .unwrap();
    let ids = rig.chunk_ids("a.md");
    assert_eq!(ids.len(), 1);
    let expected = rig
        .store
        .get(&kaban_core::NodeId::parse(&ids[0]).unwrap(), false)
        .unwrap()
        .unwrap()
        .text;
    let mut reconstructed = String::new();
    for slice in 0..7 {
        let page = next(&api, &rig.root, &job, 0, slice, PageBudget::default())
            .await
            .unwrap();
        assert_eq!(page.chunk.slices, 7);
        assert!(serde_json::to_vec(&page).unwrap().len() <= 64 * 1024);
        reconstructed.push_str(&page.chunk.text);
    }
    assert_eq!(reconstructed, expected);
}

#[tokio::test]
async fn real_range_stops_at_serialized_byte_budget_and_resumes_without_gaps() {
    use kaban_ingest::jobs::next::next_range;
    use std::fmt::Write;
    let body = (0..40).fold(String::new(), |mut body, i| {
        write!(
            body,
            "## Section {i}\n\n{}\n\n",
            "unexceptional prose ".repeat(120)
        )
        .unwrap();
        body
    });
    let (rig, api, job) =
        tokio::task::spawn_blocking(move || persisted_fixture_body("range", &body))
            .await
            .unwrap();
    let count = u32::try_from(rig.chunk_ids("a.md").len()).unwrap();
    let budget = PageBudget {
        max_result_bytes: 16 * 1024,
        metadata_allowance: 4096,
    };
    let mut n = 0;
    let mut delivered = Vec::new();
    loop {
        let page = next_range(&api, &rig.root, &job, n, count, budget)
            .await
            .unwrap();
        assert!(serde_json::to_vec(&page).unwrap().len() <= budget.max_result_bytes);
        assert_ne!(page.chunks, [] as [ChunkView; 0]);
        delivered.extend(page.chunks.iter().map(|chunk| chunk.n));
        if let Some(next) = page.next {
            assert!(next.n > n && next.n < count);
            n = next.n;
        } else {
            break;
        }
    }
    assert_eq!(delivered, (0..count).collect::<Vec<_>>());
}

#[tokio::test]
async fn source_edit_is_stale_even_when_new_content_is_policy_rejected() {
    use kaban_core::{IngestStore, JobStatus};
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::JobError;
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("edit-reject"))
        .await
        .unwrap();
    let before = rig.outbox_len();
    let file = kaban_policy::Policy::parse(
        b"words:\n  - { term: forbidden, action: reject, id: new-reject }\n",
    )
    .unwrap();
    *api.policy_handle().write().unwrap() =
        kaban_policy::Policy::from_file(file, "changed".into()).unwrap();
    std::fs::write(rig.root.join("a.md"), "# Changed\n\nforbidden new text\n").unwrap();
    assert!(matches!(
        next(&api, &rig.root, &job, 0, 0, PageBudget::default()).await,
        Err(JobError::Stale)
    ));
    assert_eq!(
        rig.store
            .job_get(&job, api.workspace())
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Stale
    );
    assert_eq!(rig.outbox_len(), before);
}

#[tokio::test]
async fn current_policy_rejection_does_not_mark_unchanged_source_stale() {
    use kaban_core::{IngestStore, JobStatus};
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::JobError;
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("policy-reject"))
        .await
        .unwrap();
    let before = rig.outbox_len();
    let file = kaban_policy::Policy::parse(
        b"words:\n  - { term: harmless, action: reject, id: new-reject }\n",
    )
    .unwrap();
    *api.policy_handle().write().unwrap() =
        kaban_policy::Policy::from_file(file, "changed".into()).unwrap();
    assert!(matches!(
        next(&api, &rig.root, &job, 0, 0, PageBudget::default()).await,
        Err(JobError::Source)
    ));
    assert_eq!(
        rig.store
            .job_get(&job, api.workspace())
            .unwrap()
            .unwrap()
            .status,
        JobStatus::Open
    );
    assert_eq!(rig.outbox_len(), before);
}

#[tokio::test]
async fn next_survives_store_close_and_reopen_without_a_process_cache() {
    use kaban_api::KabanApi;
    use kaban_ingest::jobs::next::next;
    use kaban_store_sqlite::SqliteStore;
    let (rig, api, job) = tokio::task::spawn_blocking(|| {
        persisted_fixture_with(
            "restart",
            "# Restart\n\nDurable delivery from the source after restart.\n",
            true,
        )
    })
    .await
    .unwrap();
    let first = next(&api, &rig.root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap();
    let common::real::Rig {
        root,
        store,
        ingestor,
        cfg: _,
        db,
    } = rig;
    drop(api);
    drop(ingestor);
    drop(store);
    let db = db.unwrap();
    let store = std::sync::Arc::new(SqliteStore::open(&db, &root.join("store/keys")).unwrap());
    let api = KabanApi::new(
        store.clone(),
        kaban_policy::Policy::empty(),
        store.signer(),
        None,
    );
    let restarted = next(&api, &root, &job, 0, 0, PageBudget::default())
        .await
        .unwrap();
    assert_eq!(restarted, first);
}

#[tokio::test]
async fn next_refuses_a_source_replaced_by_an_escaping_symlink() {
    use kaban_ingest::jobs::next::next;
    use kaban_ingest::jobs::types::JobError;
    let (rig, api, job) = tokio::task::spawn_blocking(|| persisted_fixture("symlink"))
        .await
        .unwrap();
    let outside = common::Scratch::new("kaban-e38-outside", "next");
    std::fs::rename(rig.root.join("a.md"), outside.join("a.md")).unwrap();
    std::os::unix::fs::symlink(outside.join("a.md"), rig.root.join("a.md")).unwrap();
    let before = rig.outbox_len();
    assert!(matches!(
        next(&api, &rig.root, &job, 0, 0, PageBudget::default()).await,
        Err(JobError::Source)
    ));
    assert_eq!(rig.outbox_len(), before);
}
