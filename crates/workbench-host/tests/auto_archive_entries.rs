//! Closed G4/G8/G9/G10 workspace/lifecycle inventory. No runtime LIVE_ONLY exemption.
fn methods(text: &str) -> Vec<String> {
    let mut names = text
        .lines()
        .filter_map(|line| {
            let public = line.trim_start().strip_prefix("pub ")?;
            let (_, tail) = public.split_once("fn ")?;
            Some(
                tail.chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    names.sort();
    names
}
fn check(
    rows: &[(&str, &str, &str)],
    actual: std::collections::BTreeMap<String, Vec<String>>,
    root: &std::path::Path,
) {
    let mut expected = std::collections::BTreeMap::<String, Vec<String>>::new();
    for (file, names, route) in rows {
        assert!(
            !route.trim().is_empty(),
            "{file}: classify guarded/delegated/exempt with reason"
        );
        assert!(root.join(file).is_file(), "removed entry source: {file}");
        expected
            .entry((*file).into())
            .or_default()
            .extend(names.split_whitespace().map(str::to_owned));
    }
    for names in expected.values_mut() {
        names.sort();
    }
    assert_eq!(
        actual, expected,
        "classify every new/removed public entry; do not reuse deletion LIVE_ONLY exemptions"
    );
}

#[test]
fn workspace_attachment_and_shutdown_entries_have_canonical_lock_or_admission_routes() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
    let rows = &[
        (
            "areas/workspace.rs",
            "command declare_saved project_entries query register_thread retention_entries sidebar_threads with_mirror with_scratch_hint with_store",
            "G4/G8 delegated/exempt: core entries run under CommandHost mutex and reload canonical membership/pin/archive under sidebar lock. Archive publication holds the same host mutex via try_lock; binding establishment and writer ACK run outside it. This exemption is for reversible archive only, not PR2b deletion. Constructors/queries introduce no execution admission.",
        ),
        (
            "attachments.rs",
            "append append_base64 append_base64_checked append_checked begin begin_checked check commit_locked discard discard finish finish_checked immutable_metadata message_with_paths new recover_dead_uploads references root upload upload_bytes upload_bytes_checked",
            "G9 delegated/exempt: core upload/update admission is serialized by CommandHost mutex; attachment -> sidebar locks revalidate archive. Legacy GUI upload is excluded by exclusive authority. Staging/read-only metadata is not activity.",
        ),
        (
            "ingress.rs",
            "admin answer_provenance attachment_writer command_receipt connections full_approval_input handle_command handle_query handle_request install_agent_observation_for_test management_enabled new new_with_epoch projection receipts reconcile_attachments reject_error renderer_data_plane renderer_owner renderer_reconnect_grace renderer_reconnect_grace request_provenance reserve_new_chat_thread running_invocations service service_mut status_probe submission_provenance submit_local submit_local_authorized submit_local_chat_with_permit with_admin with_areas with_projects with_receipts",
            "G2/G4/G8/G9 delegated: synchronous CommandHost dispatch holds host mutex; conversation inputs go to guarded runtime/storage admissions, sidebar and attachments use canonical lock/re-read. Observation/provenance/queries admit no work.",
        ),
        (
            "bootstrap/mod.rs",
            "activate_after_ready area_work_notify graph_collector open open_claimed_for_core open_for_core open_for_core_with_test_seams open_with_mcp_test_seams open_with_service_observer open_with_service_seams open_with_test_seams planned_probe_stats probe_after_ready quiesce_for_restart restart_preparing_services restart_producers shutdown storage_config storage_writer",
            "G10 delegated: production owners borrow dedicated DB/root writer authority; restart_producers pauses/drains retention and shutdown joins before storage barrier. Probes/test seams cannot forge authority; missing root stays disabled until restart.",
        ),
        (
            "bootstrap/planned_exit.rs",
            "cleanup completed first_five restart_holds take_planned_exit validate_planned_exit writer",
            "G10 delegated: retained producers stop/join at WorkersJoined before StorageDrained; getters/cleanup do not admit new work.",
        ),
    ];
    let actual = rows
        .iter()
        .map(|(p, _, _)| {
            (
                (*p).to_owned(),
                methods(&std::fs::read_to_string(root.join(p)).unwrap()),
            )
        })
        .collect();
    check(rows, actual, &root);
}
