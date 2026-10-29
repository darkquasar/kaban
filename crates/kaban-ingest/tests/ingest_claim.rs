//! Current policy and unfenced compatibility remain independent of lease authority.
#![allow(clippy::unwrap_used, clippy::expect_used)]
mod common;
use kaban_api::KabanApi;
use kaban_core::{IngestStore, JobId, LeaseStore};
use kaban_ingest::jobs::{
    brief::brief,
    claim::claim,
    next::{PageBudget, next, next_fenced},
    types::{IngestOpts, JobError},
};

#[tokio::test]
async fn plain_brief_stays_unfenced_and_claimed_source_is_rechecked_under_current_policy() {
    let rig = common::real::rig("kaban-e310b-runtime", "policy");
    std::fs::write(rig.root.join("a.md"), "# Source\n\nHarmless body.\n").unwrap();
    let api = KabanApi::new(
        rig.store.clone(),
        kaban_policy::Policy::empty(),
        rig.store.signer(),
        None,
    );
    let prepared = brief(&api, &rig.root, "a.md", &IngestOpts::default())
        .await
        .unwrap();
    let plain = JobId::parse(&prepared.job).unwrap();
    rig.store
        .lease_acquire(
            &format!(
                "ingest:{}:none:{}@{}",
                api.workspace(),
                prepared.document.id,
                prepared.document.content_hash
            ),
            "unrelated",
            300,
        )
        .unwrap();
    assert!(
        next(&api, &rig.root, &plain, 0, 0, PageBudget::default())
            .await
            .is_ok(),
        "plain jobs do not inherit unrelated leases"
    );
    let request =
        serde_json::from_value(serde_json::json!({"with":"gliner","worker":"fixture:1"})).unwrap();
    let claimed = claim(&api, &rig.root, &request).await.unwrap();
    let job = JobId::parse(claimed.job.as_deref().unwrap()).unwrap();
    let fence = claimed.lease.unwrap().fence;
    let before = rig.store.job_get(&job, api.workspace()).unwrap().unwrap();
    let file = kaban_policy::Policy::parse(
        b"words:\n  - { term: Harmless, action: reject, id: new-reject }\n",
    )
    .unwrap();
    *api.policy_handle().write().unwrap() =
        kaban_policy::Policy::from_file(file, "changed".into()).unwrap();
    assert!(matches!(
        next_fenced(
            &api,
            &rig.root,
            &job,
            0,
            0,
            PageBudget::default(),
            Some(fence)
        )
        .await,
        Err(JobError::Source)
    ));
    assert_eq!(
        rig.store.job_get(&job, api.workspace()).unwrap().unwrap(),
        before
    );
    let bad = serde_json::from_value(
        serde_json::json!({"with":"gliner","path_prefix":"AKIAQWERTYUIOPASDFGH"}),
    )
    .unwrap();
    assert!(claim(&api, &rig.root, &bad).await.is_err());
}
