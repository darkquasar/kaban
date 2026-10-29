//! Compile-time API availability, deliberately not semantic fixture qualification.

#[test]
fn candidate_exposes_required_loading_and_semantic_entry_points() {
    let _load = ra_ap_load_cargo::load_workspace;
    let _project = ra_ap_project_model::ProjectWorkspace::load;
    let _build = ra_ap_project_model::ProjectWorkspace::run_build_scripts;
    let _build_error = ra_ap_project_model::WorkspaceBuildScripts::error;
    let _impls = ra_ap_hir::Impl::all_in_crate;
    let _negative = ra_ap_hir::Impl::is_negative;
    let _trait = ra_ap_hir::Impl::trait_;
    let _self_type = ra_ap_hir::Impl::self_ty;
    let _members = ra_ap_hir::Impl::items;
    let _declarations = ra_ap_hir::Module::declarations;
    let _static_index = ra_ap_ide::StaticIndex::compute;

    let config = ra_ap_load_cargo::LoadCargoConfig {
        load_out_dirs_from_check: true,
        with_proc_macro_server: ra_ap_load_cargo::ProcMacroServerChoice::None,
        prefill_caches: false,
        num_worker_threads: 4,
        proc_macro_processes: 1,
    };
    assert_eq!(config.num_worker_threads, 4);
    assert!(config.load_out_dirs_from_check);
}
