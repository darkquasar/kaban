//! The two decisions behind the adapter capability `const` (spec 05 SS-23), tested at the
//! seam their consumers call through: a store without physical cleanup never reports a purge
//! as physically cleaned, whatever its cleanup step claims (SS-03's report stays honest); a
//! store that cannot share a database between processes refuses the shared open (mode B,
//! SS-21) of a database another process owns, and admits the open of a free one as its sole
//! owner; a sharing store never takes ownership. The adapter's exclusion itself (an `flock`
//! held for the store's lifetime) is exercised in `kaban-store-sqlite`.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

use kaban_core::{StoreCapabilities, StoreError};

const RESIDUE: &str = "free-page residue until VACUUM";

#[test]
fn a_store_without_physical_cleanup_never_reports_a_purge_as_physically_cleaned() {
    let unable = StoreCapabilities {
        physical_cleanup: false,
        shared_multiprocess: true,
    };
    // the step claims it reclaimed the pages: the report is still incomplete and says why
    let (cleaned, detail) = unable.physical_cleanup_report(true, RESIDUE.to_owned());
    assert!(!cleaned);
    assert_eq!(
        detail.as_deref(),
        Some("physical cleanup is not supported by this store; free-page residue until VACUUM")
    );
    assert!(!unable.physical_cleanup_report(false, RESIDUE.to_owned()).0);

    // the same claims under a store that can clean: the step's word is the report
    let able = StoreCapabilities {
        physical_cleanup: true,
        shared_multiprocess: true,
    };
    assert_eq!(
        able.physical_cleanup_report(true, RESIDUE.to_owned()),
        (true, None)
    );
    assert_eq!(
        able.physical_cleanup_report(false, RESIDUE.to_owned()),
        (false, Some(RESIDUE.to_owned()))
    );
}

#[test]
fn a_store_without_shared_multiprocess_refuses_the_shared_open_of_a_held_database() {
    let exclusive = StoreCapabilities {
        physical_cleanup: true,
        shared_multiprocess: false,
    };
    // another process owns the file: the adapter's exclusion could not be taken
    let refused = exclusive.admit_shared_open(|| Ok(None::<()>)).unwrap_err();
    assert!(
        matches!(refused, StoreError::SharedOpenUnsupported),
        "{refused:?}"
    );
    assert!(
        refused
            .to_string()
            .contains("close the other kaban process"),
        "the refusal says what to do: {refused}"
    );
    // nobody else owns it: admitted, and the ownership lives on with the store
    assert_eq!(
        exclusive.admit_shared_open(|| Ok(Some("owner"))).unwrap(),
        Some("owner")
    );

    // a sharing store is admitted without ever taking ownership
    let sharing = StoreCapabilities {
        physical_cleanup: true,
        shared_multiprocess: true,
    };
    let admitted = sharing
        .admit_shared_open(|| -> kaban_core::Result<Option<()>> {
            panic!("a sharing store takes no ownership")
        })
        .unwrap();
    assert_eq!(admitted, None);
}
