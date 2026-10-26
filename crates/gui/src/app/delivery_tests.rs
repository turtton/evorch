//! Save-before-effect boundaries for durable resend drafts.
use super::WorkbenchState;
use super::history::{DeliveryStatus, Resolution, UserMessage};
use crate::fixture::DemoSource;
use crate::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use crate::model::composer::ProviderStatus;
use std::sync::{Arc, Mutex};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

mod review;

#[derive(Default)]
struct Receipts(Arc<Mutex<Vec<storage::HostInputReceipt>>>);
impl CommandSink for Receipts {
    fn host_input_receipts(
        &self,
        _: &[String],
    ) -> Option<Result<Vec<storage::HostInputReceipt>, storage::StorageError>> {
        Some(Ok(self.0.lock().unwrap().clone()))
    }
    fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
        Vec::new()
    }
}

fn state(path: &std::path::Path) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("test");
    sidebar
        .add_project(project.clone(), "test", path.parent().unwrap())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("thread"), project, "thread")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("thread")).unwrap();
    let mut state = WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.into())
        .with_provider_status(ProviderStatus::Configured);
    state.history.push(UserMessage {
        id: "old".into(),
        thread_id: "thread".into(),
        text: "retry this".into(),
        prompt: "retry this".into(),
        at: std::time::SystemTime::now(),
        image_count: 2,
        resent_from: None,
        status: DeliveryStatus::Dropped,
        resolution: None,
    });
    state.rebuild_delivery_rows();
    state
}

#[test]
fn edit_and_dismiss_roll_back_when_atomic_save_fails() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.sidebar_path = Some(dir.path().join("missing/sidebar.json"));
    state.restore_queued_follow_up(0);
    assert!(state.history[0].resolution.is_none());
    assert!(state.composer_drafts.is_empty());
    assert!(state.composer.input.is_empty());
    assert_eq!(state.composer.queued.len(), 1);
    state.dismiss_queued_follow_up(0);
    assert!(state.history[0].resolution.is_none());
    assert_eq!(state.composer.queued.len(), 1);
}

#[test]
fn edit_is_durable_once_and_resend_saves_new_identity_before_dispatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let mut state = state(&path);
    state.restore_queued_follow_up(0);
    state.restore_queued_follow_up(0); // double click cannot create a second draft
    assert_eq!(state.composer.input, "retry this");
    assert_eq!(state.composer_drafts.len(), 1);
    assert_eq!(state.composer.images_needed, 2);
    assert!(state.composer.queued.is_empty());
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["composer_drafts"][0]["images_needed"], 2);
    let mut restored = state_for_reload(&path, state.sidebar.clone());
    let config = storage::StorageConfig {
        db_path: dir.path().join("test.db"),
        ..Default::default()
    };
    restored
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(restored.composer.images_needed, 2);
    assert_eq!(restored.composer.input, "retry this");

    state.submit_composer();
    assert_eq!(state.issued.len(), 1);
    let new = &state.history[1];
    assert_ne!(new.id, "old");
    assert_eq!(new.resent_from.as_deref(), Some("old"));
    assert!(
        matches!(&state.history[0].resolution, Some(Resolution::Resent { new_id }) if new_id == &new.id)
    );
    assert!(state.composer_drafts.is_empty());
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["user_messages"][1]["id"], new.id);
    assert_eq!(saved["composer_drafts"].as_array().unwrap().len(), 0);
}

fn state_for_reload(path: &std::path::Path, sidebar: SidebarState) -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_sidebar_path(path.into())
}

#[test]
fn resend_save_failure_never_dispatches_and_keeps_original_draft() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.restore_queued_follow_up(0);
    state.sidebar_path = Some(dir.path().join("missing/sidebar.json"));
    state.submit_composer();
    assert!(state.issued.is_empty());
    assert_eq!(state.history.len(), 1);
    assert_eq!(state.composer_drafts.len(), 1);
    assert_eq!(state.composer.input, "retry this");
    assert!(matches!(
        state.history[0].resolution,
        Some(Resolution::MovedToComposer { .. })
    ));
}

#[test]
fn receipts_override_counts_and_resolved_originals_ignore_late_delivery() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    let receipt = storage::HostInputReceipt {
        prompt_id: "old".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "retry this".into(),
        image_count: 2,
        status: storage::HostInputStatus::Unknown,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
    };
    let receipts = Arc::new(Mutex::new(vec![receipt]));
    state.sink = Box::new(Receipts(receipts.clone()));
    state.history[0].status = DeliveryStatus::Queued;
    state.reconcile_follow_ups();
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    assert!(
        state.composer.queued[0]
            .rejection
            .as_ref()
            .unwrap()
            .contains("重複")
    );
    state.restore_queued_follow_up(0);
    receipts.lock().unwrap()[0].status = storage::HostInputStatus::Delivered;
    state.reconcile_follow_ups();
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
    assert!(state.composer.queued.is_empty());
    assert!(
        !state
            .transcripts
            .thread()
            .entries()
            .iter()
            .any(|entry| matches!(
                entry,
                crate::model::transcript::TranscriptEntry::UserMessage { .. }
            ))
    );
}

#[test]
fn startup_missing_receipt_is_rejected_and_terminal_accepted_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    let mut state = state(&dir.path().join("sidebar.json"));
    state.history[0].status = DeliveryStatus::Queued;
    state.apply_receipts(&[], true);
    assert!(
        matches!(&state.history[0].status, DeliveryStatus::Rejected { reason } if reason == "送信前に中断")
    );
    state
        .phases
        .insert("run-1".into(), workspace_ui::ThreadRunPhase::Stopped);
    let receipt = storage::HostInputReceipt {
        prompt_id: "old".into(),
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        text: "retry this".into(),
        image_count: 0,
        status: storage::HostInputStatus::Accepted,
        reason: None,
        accepted_at_ns: 1,
        status_at_ns: 1,
    };
    state.apply_receipts(&[receipt], false);
    assert_eq!(state.history[0].status, DeliveryStatus::Unknown);
}

#[test]
fn rejected_resend_creates_a_new_durable_draft_without_reviving_the_original() {
    struct Refuse;
    impl CommandSink for Refuse {
        fn submit(&mut self, _: WorkbenchCommand) -> Vec<LoopEvent> {
            vec![LoopEvent::ChatRejected {
                thread_id: "thread".into(),
                reason: "sealed".into(),
            }]
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sidebar.json");
    let mut state = state(&path);
    state.sink = Box::new(Refuse);
    state.restore_queued_follow_up(0);
    state.submit_composer();
    assert!(matches!(
        state.history[0].resolution,
        Some(Resolution::Resent { .. })
    ));
    assert!(matches!(
        state.history[1].status,
        DeliveryStatus::Rejected { .. }
    ));
    assert!(matches!(
        state.history[1].resolution,
        Some(Resolution::MovedToComposer { .. })
    ));
    assert_eq!(state.composer_drafts.len(), 1);
    assert_eq!(state.composer_drafts[0].origin, state.history[1].id);
    assert_eq!(state.composer.input, "retry this");
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(saved["composer_drafts"][0]["origin"], state.history[1].id);
}
