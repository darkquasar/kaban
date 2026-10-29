//! The default build has no PDF extractor (`pdf` is opt-in for 3.0.0, OWN-70, DI2.1b): a `.pdf`
//! document is still walked, and `prepare.rs` records it `failed` with the fixed reason
//! `no extractor for this format`; nothing panics and the batch continues (D8). With
//! `--features pdf` the whole binary is compiled out (`golden_pdf` covers that build).

#![cfg(not(feature = "pdf"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test code: failures are the assertion

mod common;

use kaban_ingest::{Format, extractor_for, index_status};

const NO_EXTRACTOR: &str = "no extractor for this format";

#[test]
fn without_the_pdf_feature_a_pdf_is_recorded_failed_with_the_fixed_reason() {
    assert!(extractor_for(Format::Pdf).is_none());

    let rig = common::real::rig("kaban-di21b", "pdf-unavailable");
    std::fs::create_dir_all(rig.root.join("docs")).unwrap();
    let born1 =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pdf/born1.pdf");
    std::fs::copy(born1, rig.root.join("docs/born1.pdf")).unwrap();
    std::fs::write(rig.root.join("docs/note.md"), "# Note\n\nA paragraph.\n").unwrap();

    let report = rig.run();
    assert_eq!(report.documents, 1, "{report:?}"); // the Markdown neighbour is committed
    assert_eq!(report.failed, 1, "{report:?}");
    assert_eq!(report.partial, 0, "{report:?}");
    assert_eq!(report.blocked, 0, "{report:?}");
    assert!(!report.cancelled);

    let doc = rig.document("docs/born1.pdf").unwrap();
    assert_eq!(doc.props["status"].as_str().unwrap(), "failed");
    assert_eq!(doc.props["error"].as_str().unwrap(), NO_EXTRACTOR);
    // a failed document carries no chunk or section ids (nothing was extracted)
    assert!(
        doc.props
            .get("chunk_ids")
            .is_none_or(|v| v == &serde_json::json!([])),
        "{:?}",
        doc.props
    );

    let status = index_status(rig.store.as_ref(), &kaban_core::WorkspaceId::default()).unwrap();
    assert!(
        !status
            .extractors
            .iter()
            .any(|e| e.starts_with("pdf_oxide@")),
        "{:?}",
        status.extractors
    );
    assert_eq!(status.documents_by_status["failed"], 1);
    assert_eq!(status.failed.len(), 1);
    assert_eq!(status.failed[0].path, "docs/born1.pdf");
    assert_eq!(status.failed[0].error, NO_EXTRACTOR);
}
