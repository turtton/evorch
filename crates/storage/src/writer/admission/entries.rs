//! Closed storage admission inventory, independent of deletion LIVE_ONLY exemptions.
use std::sync::{Arc, Mutex, mpsc};

#[derive(Default)]
struct Probe {
    pending: usize,
    failed: usize,
}
struct AdmissionProbe {
    state: Arc<Mutex<Probe>>,
    retain: bool,
}
impl crate::MutationAdmission for AdmissionProbe {
    fn persistence_failed(&mut self) {
        self.retain = true;
        self.state.lock().unwrap().failed += 1;
    }
}
impl Drop for AdmissionProbe {
    fn drop(&mut self) {
        if !self.retain {
            self.state.lock().unwrap().pending -= 1;
        }
    }
}

#[test]
fn actual_writer_ack_releases_admission_but_failed_persistence_retains_it() {
    let dir = tempfile::tempdir().unwrap();
    let storage = crate::Storage::open(crate::StorageConfig {
        db_path: dir.path().join("ack.db"),
        ..Default::default()
    })
    .unwrap();
    let handle = storage.handle();
    let state = Arc::new(Mutex::new(Probe::default()));
    let captured = state.clone();
    let (admitted, registered) = mpsc::channel();
    assert!(handle.install_request_admission(Arc::new(move |scope| {
        captured.lock().unwrap().pending += 1;
        admitted.send(scope).unwrap();
        Box::new(AdmissionProbe {
            state: captured.clone(),
            retain: false,
        })
    })));
    let (entered, paused) = mpsc::channel();
    let (release, proceed) = mpsc::channel();
    handle.pause_writer_for_test(entered, proceed).unwrap();
    paused.recv().unwrap();
    let writer = handle.clone();
    let event = event_bus::Event::new(event_bus::MessageEvent::MessageCompleted {
        run_id: "run-1".into(),
        text: "fixture".into(),
    });
    let sent = event.clone();
    let append = std::thread::spawn(move || writer.append_event(None, &sent));
    assert_eq!(registered.recv().unwrap(), Some("r:run-1".into()));
    assert_eq!(state.lock().unwrap().pending, 1);
    release.send(()).unwrap();
    append.join().unwrap().unwrap();
    assert_eq!(state.lock().unwrap().pending, 0);

    handle
        .activity_job(true, |conn| {
            conn.execute_batch(
                "CREATE TEMP TRIGGER reject_ack_fixture BEFORE INSERT ON events \
                 BEGIN SELECT RAISE(ABORT,'fixture persistence failure'); END",
            )?;
            Ok(())
        })
        .unwrap();
    assert!(matches!(
        handle.append_event(None, &event),
        Err(crate::StorageError::Sqlite(_))
    ));
    let held = state.lock().unwrap();
    assert_eq!(held.pending, 1);
    assert_eq!(held.failed, 1);
}

#[test]
fn uncommitted_context_binding_is_global_without_reading_or_decoding_context() {
    let (reply, _receive) = mpsc::channel();
    let record = crate::RunContextRecord {
        run_id: "run-1".into(),
        role: "worker".into(),
        name: "chat:project:not-yet-bound".into(),
        parent_run_id: None,
        config_json: "not decoded during admission".into(),
        messages_json: "not decoded during admission".into(),
        checkpoints_json: "not decoded during admission".into(),
        terminal_phase: "waiting".into(),
        restorable: true,
        updated_at_ns: 0,
        host_inputs_json: "not decoded during admission".into(),
    };
    assert_eq!(
        super::scope(&super::Command::UpsertRunContext(record, reply)),
        Some(None)
    );
}

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
fn every_public_writer_entry_has_an_archive_admission_route_or_reason() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("src");
    fn visit(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Vec<String>>,
    ) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let p = entry.unwrap().path();
            if p.is_dir() {
                visit(root, &p, out)
            } else if p.extension().is_some_and(|e| e == "rs") {
                let m = methods(&std::fs::read_to_string(&p).unwrap());
                if !m.is_empty() {
                    out.insert(
                        p.strip_prefix(root).unwrap().to_string_lossy().into_owned(),
                        m,
                    );
                }
            }
        }
    }
    let mut actual = std::collections::BTreeMap::new();
    visit(&root, &root.join("writer"), &mut actual);
    actual.insert(
        "writer.rs".into(),
        methods(&std::fs::read_to_string(root.join("writer.rs")).unwrap()),
    );
    check(
        &[
            (
                "writer/lanes.rs",
                "append_lane_report block_lane_start create_lane lane lane_by_creation_key lane_by_thread lane_collection_view lane_list lane_reports managed_lanes register_lane_delivery_key resume_lane_start revise_lane_brief start_lane_run touch_lane_activity unstarted_lanes unstarted_lanes_excluding",
                "G2/G4/G6/G7 delegated: mutations use lane_write/lane_job(true) through writer ACK; lane_read is read-only.",
            ),
            (
                "writer/lane_instructions.rs",
                "apply_lane_instruction lane_instruction lane_instruction_delivery lane_instructions",
                "G2/G4/G6/G7 delegated: mutations use lane_write/lane_job(true) through writer ACK; lane_read is read-only.",
            ),
            (
                "writer/pull_request_auto_fix.rs",
                "accept_pr_delivery ack_pr_delivery cancel_pr_notice cancel_pr_notice_if_current claim_pr_delivery claim_pr_limit_notification nack_pr_delivery pending_pr_delivery_destinations pr_auto_fix_limit_events pr_auto_fix_state pr_delivery_receipt_ids pr_notice pr_notices pr_wake_guard ready_pr_notice ready_pr_notice_if_current release_pr_limit_notification reserve_pr_notice update_pr_auto_fix_policy update_pr_wake_guard",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/thread_goal_usage.rs",
                "thread_goal_usage upsert_thread_goal_usage",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/pull_requests.rs",
                "active_pull_request_links begin_pull_request_fetch claim_pull_request_notification clear_pull_request_local_head commit_pull_request_fetch commit_pull_request_fetch_with_display dismiss_pull_request_candidate fail_pull_request_fetch link_pull_request next_pull_request_transition pending_pull_request_inheritances pin_pull_request_inheritance present_pull_request_candidate pull_request_candidates pull_request_inheritance pull_request_link_history pull_request_notification_claimed pull_request_snapshot pull_request_thread_state record_pull_request_inheritance_attempt record_pull_request_local_head resolve_pull_request_inheritance schedule_pull_request_inheritance skip_pull_request_inheritance unlink_pull_request",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/index/registration.rs",
                "request_id",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/learning_jobs.rs",
                "admit_learning_job adopt_learning_job write_learning_attempt",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/conversation_plan.rs",
                "conversation_plan_history conversation_plan_summary conversation_plan_task conversation_plan_tree install_plan_write_gate rebind_conversation_plan reconcile_conversation_plan update_conversation_plan",
                "G3/G4 delegated: source writes use request or snapshot_request through writer ACK; queries/flush/test seams admit no work.",
            ),
            (
                "writer/cleanup.rs",
                "cleanup_diagnostics preview_diagnostic_cleanup",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/recovery.rs",
                "activate_recovery_claim claim_recovery_component claim_recovery_manifest commit_recovery_manifest finish_recovery_graph_control inject_restart_write_fault invalidate_restart mutate_restart pause_next_graph_write prepare_recovery_manifest quarantine_cold_restart release_prepared_claim_if_unexecuted release_recovery_claim stop_quarantined_recovery_intents validate_prepared_claim validate_recovery_claim validate_recovery_startup write_recovery_graph_control write_recovery_graph_progress",
                "G3/G4 delegated: source writes use request or snapshot_request through writer ACK; queries/flush/test seams admit no work.",
            ),
            (
                "writer/lane_delivery.rs",
                "accept_lane_delivery accept_lane_user_input ack_lane_delivery active_lane_recovery append_lane_report_with_delivery bind_lane_delivery_destination claim_lane_deliveries create_lane_question lane_delivery_destination lane_delivery_received_ids lane_inbox lane_question_capacity_diagnostic lane_question_routing lane_questions lane_recovery_episode lane_wake_guard latest_lane_recovery mutate_lane_question nack_lane_delivery pending_lane_delivery_destinations prepare_lane_destination_recovery prepare_lane_recovery record_lane_destination_recovery_failure record_lane_recovery_failure resolve_lane_recovery update_lane_wake_guard validate_lane_delivery_claim",
                "G2/G4/G6/G7 delegated: mutations use lane_write/lane_job(true) through writer ACK; lane_read is read-only.",
            ),
            (
                "writer/admin_audit.rs",
                "admin_audit install_admin_audit_fault try_submit_admin_audit",
                "Exempt: queries, index/usage/notes/audit/diagnostic/learning/pull-request/artifact metadata are not execution, membership or canonical archive protection; actual runtime execution is G1/G3 guarded.",
            ),
            (
                "writer/resume_intent.rs",
                "flush_resume_intents retire_resume_intent save_recovery_continue save_resume_intent save_snapshot_bundle_with_fenced_resume_intent save_snapshot_bundle_with_resume_intent stop_unregistered_resume_intent",
                "G3/G4 delegated: source writes use request or snapshot_request through writer ACK; queries/flush/test seams admit no work.",
            ),
            (
                "writer/lane_control.rs",
                "begin_lane_verification close_lane complete_lane_verification current_lane_close forward_lane_question incomplete_lane_verification lane_attention lane_close_context lane_pr_evidence lane_progress_anchor lane_stall_episode lane_verification lane_verification_target observe_lane_stall reclassify_lane_stall record_lane_nudge_outcome request_lane_nudge submit_lane_verification",
                "G2/G4/G6/G7 delegated: mutations use lane_write/lane_job(true) through writer ACK; lane_read is read-only.",
            ),
            (
                "writer/lane_management.rs",
                "acknowledge_lane_management_projection append_lane_monitor_failure apply_lane_management_transition lane_family_topology_revision pending_lane_management_projections",
                "G2/G4/G6/G7 delegated: mutations use lane_write/lane_job(true) through writer ACK; lane_read is read-only.",
            ),
            (
                "writer.rs",
                "answer_user_question answer_user_question_with_evidence answer_user_question_with_evidence_and_mode answer_user_question_with_mode append_event append_fenced_event append_host_input_accepted append_review_evidence_with_reference_consumption append_stream_event bind_user_questions compare_and_swap_run_context_restore_state create_user_question delete_eval_sample invalidate_run_context mark_host_inputs_unknown reconcile record_host_chat_submission record_review_evidence record_review_evidence_with_edges record_review_lineage_edge save_eval_sample save_snapshot_bundle save_snapshot_bundle_with_evidence_and_host_inputs save_snapshot_bundle_with_evidence_and_reference_consumption save_snapshot_bundle_with_host_inputs transition_host_inputs upsert_run_context upsert_run_context_receipt upsert_run_context_with_evidence upsert_run_context_with_evidence_and_host_inputs upsert_run_context_with_evidence_and_reference_consumption upsert_run_context_with_receipts upsert_thread_runtime upsert_threads_host",
                "G1/G2/G3/G4/G5/G6/G8 guarded/delegated: exhaustive admission::scope, request and snapshot_request retain admission through writer ACK.",
            ),
            (
                "writer.rs",
                "abort_thread_index_registration acquire_thread_index_lease advance_thread_index_registration append_finding append_indexed_note append_lesson append_lessons append_run_ledger append_team_snapshot apply_thread_index_settings assistant_reference attach_improvement_draft barrier begin_thread_index_registration cancel_thread_index_request checkpoint_now claim_learning close flush_usage_now handle heartbeat_thread_index_lease host_chat_submissions host_input_receipts host_input_receipts_for_run install_request_admission latest_run_ledger max_event_bytes observe_event_commits observe_pending_reindex observe_thread_index_for_test open open_with_lease pause_writer_for_test promote_lesson reclaim_mark_files reclaim_step record_artifact_ref record_catalog_update record_improvement_candidate record_improvement_observation record_usage_requests reject_lesson release_thread_index_lease remove_artifact_refs retention_backfill_step save_assistant_reference save_thread_start_state set_improvement_status set_lesson_favorite set_thread_index_clock_for_test start_thread_index_owner statistics step_thread_index stop_thread_index_owner submit_thread_index_request thread_index_capacity thread_index_job_status thread_index_request_status tick_thread_index_for_test tombstoned_runs try_attribute_usage_runs try_upsert_threads update_improvement update_memory validate_auto_archive_authority validate_lesson without_blocking_requests_for_test",
                "Exempt: storage lifecycle, queries, notes, usage, index/reclaim/diagnostics and artifact refs do not admit runs or canonical protections; async try_upsert_threads mirrors sidebar metadata only (not thread_runs), whose canonical source is protected by host mutex.",
            ),
        ],
        actual,
        &root,
    );
}
