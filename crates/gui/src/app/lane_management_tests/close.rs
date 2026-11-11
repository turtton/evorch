//! Ending a lane by hand through the production window, runtime and SQLite ledger: what the
//! confirmation reads, what a screen older than the ledger cannot do, and how the lane reads
//! afterwards.
use super::{runtime::NoExecution, setup};
use crate::{
    app::{
        WorkbenchState,
        lane_attention::StorageLaneAttentionReader,
        lane_close::{ClosePhase, StorageLaneClosePreviewReader},
        lanes::{Spawner, StorageLaneReader},
    },
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    model::{
        lane_attention::{AttentionAction, AttentionOp},
        lane_close::CloseChoice,
    },
    runtime_sink::GuiLaneCloseHost,
};
use ::runtime::{AgentRuntime, RunStore};
use event_bus::{
    EventBus,
    lane::{LaneCloseActor, LaneCloseKind, LaneReportInput, LaneReportProducer, LaneStatus},
};
use std::{path::Path, sync::Arc};
use storage::{Storage, StorageConfig};
use workbench_protocol::lane_attention::LaneAttentionKind;
use workspace_ui::ThreadId;

fn closing_window(dir: &Path) -> (WorkbenchState<DemoSource>, Storage, AgentRuntime) {
    let (ui, storage, _) = setup(dir);
    let bus = Arc::new(EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(NoExecution),
    )
    .with_run_store(
        RunStore::open(
            &StorageConfig {
                db_path: dir.join("events.db"),
                ..Default::default()
            },
            storage.handle(),
        )
        .unwrap(),
    );
    let owner = Arc::new(
        ::runtime::ownership::OwnerHost::open(
            &dir.join("owners"),
            Default::default(),
            runtime.event_bus(),
        )
        .unwrap(),
    );
    owner.start("thread").unwrap();
    owner.start("lane-thread").unwrap();
    let db = dir.join("events.db");
    let inline: Spawner = Arc::new(|job| job());
    let ui = ui
        .with_lane_spawner(inline)
        .with_lane_attention(Arc::new(StorageLaneAttentionReader::new(db.clone())))
        .with_lane_ledger(Arc::new(StorageLaneReader::new(db.clone())))
        .with_lane_close(
            Arc::new(StorageLaneClosePreviewReader::new(db)),
            Arc::new(GuiLaneCloseHost::new(runtime.clone()).with_ownership(owner)),
        );
    (ui, storage, runtime)
}

fn report_ready(storage: &Storage, summary: &str, at_ns: i64) {
    storage
        .handle()
        .append_lane_report(
            "lane",
            1,
            LaneReportInput {
                status: LaneStatus::Ready,
                summary: summary.into(),
                artifact_refs: vec![],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: LaneReportProducer::LaneAgent,
                artifact_revision: None,
                failure_episode: None,
            },
            at_ns,
        )
        .unwrap();
}

fn settle(ui: &mut WorkbenchState<DemoSource>) {
    for _ in 0..3 {
        ui.pump_lanes();
    }
}

/// The Ready item of the list as the screen draws it: its id and revision.
fn listed_ready(ui: &WorkbenchState<DemoSource>) -> (String, u64) {
    ui.lane_attention()
        .items()
        .into_iter()
        .find(|item| matches!(item.kind, LaneAttentionKind::ReadyUnclosed { .. }))
        .map(|item| (item.id.to_string(), item.revision.get()))
        .expect("a Ready item")
}

fn choose(ui: &mut WorkbenchState<DemoSource>, item: (String, u64), choice: CloseChoice) {
    ui.apply_attention_action(
        &egui::Context::default(),
        AttentionAction {
            lead: "thread".into(),
            item: Some(item),
            op: AttentionOp::BeginClose(choice),
        },
    );
    settle(ui);
}

fn type_reason(ui: &mut WorkbenchState<DemoSource>, reason: &str) {
    ui.lane_close.dialog.as_mut().unwrap().note = reason.into();
}

fn standing(storage: &Storage) -> Option<event_bus::lane::LaneCloseResult> {
    storage.handle().current_lane_close("lane".into()).unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_closes_a_ready_lane_and_the_lane_then_reads_as_unverified_and_not_as_the_leads() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, runtime) = closing_window(dir.path());
    report_ready(&storage, "all done", 5);
    settle(&mut ui);
    let item = listed_ready(&ui);

    choose(&mut ui, item, CloseChoice::Complete);
    let dialog = ui.lane_close_dialog().expect("the confirmation is open");
    let ClosePhase::Loaded(preview) = &dialog.phase else {
        panic!("the lane was read: {:?}", dialog.phase);
    };
    // What the person sees: the brief and the latest Ready, from the ledger.
    assert_eq!(preview.lane.brief.objective, "implement");
    assert_eq!(preview.ready.as_ref().unwrap().summary, "all done");
    assert!(dialog.blocker().is_none());
    // A reason is required; reading and typing changed nothing.
    assert!(!dialog.can_confirm());
    assert!(standing(&storage).is_none());

    type_reason(&mut ui, "  I reviewed the patch myself  ");
    ui.confirm_lane_close();
    assert!(ui.lane_close_dialog().is_none());

    let stored = standing(&storage).expect("the close stands");
    assert_eq!(stored.kind, LaneCloseKind::HumanClosed);
    assert_eq!(stored.note, "I reviewed the patch myself");
    // The ledger names the surface the desktop host authenticated, not anything typed.
    let Some(LaneCloseActor::Human { surface, client_id }) = stored.closed_by else {
        panic!("a person decided");
    };
    assert_eq!(surface, "desktop");
    assert!(client_id.unwrap().starts_with("desktop-"));

    settle(&mut ui);
    assert!(
        ui.lane_attention().items().is_empty(),
        "an ended lane asks for nothing more"
    );
    let detail = ui.lane_detail(&ThreadId::new("lane-thread")).unwrap();
    assert_eq!(
        crate::model::lane_close::close_label(detail.close.as_ref().unwrap()),
        "completed by human decision (unverified)"
    );
    // Only the ledger changed: the lane is still attached and nothing was stopped.
    assert_eq!(
        storage.handle().lane("lane").unwrap().unwrap().policy,
        storage::lanes::LanePolicy::Attached
    );
    assert!(runtime.list_agents().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_person_can_discard_a_lane_and_it_is_not_recorded_as_done() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _runtime) = closing_window(dir.path());
    report_ready(&storage, "half done", 5);
    settle(&mut ui);
    let item = listed_ready(&ui);

    choose(&mut ui, item, CloseChoice::Discard);
    type_reason(&mut ui, "superseded by another lane");
    ui.confirm_lane_close();

    assert_eq!(standing(&storage).unwrap().kind, LaneCloseKind::Discarded);
    settle(&mut ui);
    let detail = ui.lane_detail(&ThreadId::new("lane-thread")).unwrap();
    assert_eq!(
        crate::model::lane_close::close_label(detail.close.as_ref().unwrap()),
        "discarded"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_screen_older_than_the_ledger_is_refused_by_the_revisions_and_records_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _runtime) = closing_window(dir.path());
    report_ready(&storage, "first ready", 5);
    settle(&mut ui);
    let item = listed_ready(&ui);
    choose(&mut ui, item, CloseChoice::Complete);
    type_reason(&mut ui, "looks fine");
    assert!(ui.lane_close_dialog().unwrap().can_confirm());

    // The lane reports again after the screen was read: the ledger's revisions have moved.
    report_ready(&storage, "second ready", 9);
    ui.confirm_lane_close();

    let dialog = ui.lane_close_dialog().expect("the dialog stays open");
    assert!(dialog.stale);
    assert!(
        dialog
            .notice
            .as_deref()
            .unwrap()
            .contains("nothing was recorded")
    );
    assert!(!dialog.can_confirm(), "reload before deciding again");
    assert!(standing(&storage).is_none());

    // Reading again shows a lane that is not the one the list drew: still not confirmable
    // until the list is read again and the person chooses on the current item.
    ui.reload_lane_close();
    settle(&mut ui);
    let dialog = ui.lane_close_dialog().unwrap();
    assert!(dialog.blocker().unwrap().contains("reported again"));
    ui.confirm_lane_close();
    assert!(standing(&storage).is_none());

    ui.cancel_lane_close();
    ui.refresh_lane_attention();
    settle(&mut ui);
    let current = listed_ready(&ui);
    choose(&mut ui, current, CloseChoice::Complete);
    type_reason(&mut ui, "second one looks fine too");
    ui.confirm_lane_close();
    assert_eq!(standing(&storage).unwrap().ready_seq, Some(2));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_list_item_the_ledger_no_longer_holds_opens_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, _runtime) = closing_window(dir.path());
    report_ready(&storage, "ready", 5);
    settle(&mut ui);
    let (id, revision) = listed_ready(&ui);

    choose(&mut ui, (id, revision + 100), CloseChoice::Complete);
    assert!(ui.lane_close_dialog().is_none());
    choose(&mut ui, ("gone".into(), revision), CloseChoice::Discard);
    assert!(ui.lane_close_dialog().is_none());
    assert!(standing(&storage).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_confirmation_is_drawn_with_its_meaning_and_confirm_waits_for_a_reason() {
    let dir = tempfile::tempdir().unwrap();
    let (ui, storage, _runtime) = closing_window(dir.path());
    let mut window = HeadlessWorkbench::new(ui, [1280.0, 900.0]);
    report_ready(&storage, "all done", 5);
    window.state_mut().refresh_lane_attention();
    window.run();
    settle(window.state_mut());
    let item = listed_ready(window.state());
    choose(window.state_mut(), item, CloseChoice::Complete);
    window.run();

    for label in [
        "Close lane (unverified)",
        crate::model::lane_close::MEANING,
        "implement",
        "• tests",
        "Latest Ready (#1)",
        "all done",
        "Reason (required)",
        "Recorded as: completed by human decision (unverified)",
    ] {
        assert!(window.has_label(label), "{label}");
    }
    assert_eq!(window.label_enabled("Close lane"), Some(false));

    type_reason(window.state_mut(), "checked by hand");
    window.run();
    assert_eq!(window.label_enabled("Close lane"), Some(true));
    window.click_label("Close lane");
    window.run();
    assert_eq!(standing(&storage).unwrap().kind, LaneCloseKind::HumanClosed);
    assert!(!window.has_label("Close lane (unverified)"));
}
