//! Public admitted-snapshot callback: lifetime, safe absence, and consumer errors.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "common/cg_fixture.rs"]
mod cg_fixture;
mod common;

use cg_fixture::{AFERO_COMMIT, OTHER_COMMIT, Variant};
use common::Scratch;
use kaban_api::{ApiConfig, KabanApi};
use kaban_codegraph::testing::FixedSnapshot;
use kaban_core::{MemoryStore, StoreError, SymbolPage};
use kaban_policy::Policy;
use kaban_store_sqlite::SqliteStore;
use std::process::Command;
use std::sync::Arc;

#[test]
fn admitted_callback_pages_symbols_and_absence_or_too_new_detaches_old_snapshot() {
    let scratch = Scratch::new("snapshot-consumer", "states");
    let root = scratch.0.join("repo");
    std::fs::create_dir_all(&root).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=t",
            "-c",
            "user.email=t@example.invalid",
            "commit",
            "--allow-empty",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        assert!(
            Command::new("git")
                .current_dir(&root)
                .args(args)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .status()
                .unwrap()
                .success()
        );
    }
    let store = Arc::new(SqliteStore::open(&scratch.db(), &scratch.keys()).unwrap());
    let base = KabanApi::with_config(
        store.clone(),
        Policy::empty(),
        store.signer(),
        None,
        ApiConfig {
            repo: Some("fixture".into()),
            repo_root: Some(root),
            ..Default::default()
        },
    );
    let admitted = cg_fixture::build(&scratch.0, Variant::Afero, AFERO_COMMIT);
    let api = base.clone().with_snapshot_provider(
        admitted.dir.clone(),
        Arc::new(FixedSnapshot::new(&admitted.path)),
    );
    let count = api
        .with_current_snapshot(|commit| {
            assert_eq!(commit, Some(AFERO_COMMIT));
            let (symbols, _) = store.snapshot_symbols(
                commit.unwrap(),
                SymbolPage {
                    after_id: 0,
                    limit: 500,
                },
            )?;
            Ok::<_, StoreError>(symbols.len())
        })
        .unwrap();
    assert!(count > 0);
    let error = api
        .with_current_snapshot(|_| Err::<(), _>(StoreError::NotFound("consumer fixture".into())));
    assert!(matches!(error, Err(StoreError::NotFound(_))));
    base.with_current_snapshot(|commit| {
        assert_eq!(commit, None);
        assert!(
            store
                .snapshot_symbols(
                    AFERO_COMMIT,
                    SymbolPage {
                        after_id: 0,
                        limit: 500
                    }
                )
                .is_err()
        );
        Ok::<_, StoreError>(())
    })
    .unwrap();
    // Reattach first, so TooNew must actively remove a prior admitted read.
    api.with_current_snapshot(|_| Ok::<_, StoreError>(()))
        .unwrap();
    let newer = cg_fixture::build(&scratch.0, Variant::AferoTooNew, OTHER_COMMIT);
    let newer_api =
        base.with_snapshot_provider(newer.dir, Arc::new(FixedSnapshot::new(&newer.path)));
    newer_api
        .with_current_snapshot(|commit| {
            assert_eq!(commit, None);
            assert!(
                store
                    .snapshot_symbols(
                        AFERO_COMMIT,
                        SymbolPage {
                            after_id: 0,
                            limit: 500
                        }
                    )
                    .is_err()
            );
            Ok::<_, StoreError>(())
        })
        .unwrap();
}
