//! Pointer gestures through the production Workbench, runtime and SQLite ledger.
use super::{detach, repair, setup};
use crate::{
    app::{WorkbenchState, lane_management::LaneManagementHost},
    fixture::DemoSource,
    headless::HeadlessWorkbench,
    runtime_sink::GuiLaneManagementHost,
};
use ::runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunStore, RuntimeError};
use async_trait::async_trait;
use egui::{Event, Modifiers, PointerButton, Pos2};
use event_bus::EventBus;
use providers::{ChatResponse, Message, ToolSpec};
use std::{path::Path, sync::Arc};
use storage::{Storage, StorageConfig, lane_management::*, lanes::LanePolicy};
use workspace_ui::{ProjectId, ThreadChatRole, ThreadId, lane_management::LaneDropError};

type Window = HeadlessWorkbench<DemoSource>;

pub(super) struct NoExecution;
#[async_trait]
impl AgentModel for NoExecution {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        panic!("Human lane management must not execute a model")
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "no-execution".into()
    }
}

fn actual_host(dir: &Path) -> (WorkbenchState<DemoSource>, Storage, AgentRuntime) {
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
    (
        ui.with_lane_management_host(Arc::new(
            GuiLaneManagementHost::new(runtime.clone()).with_ownership(owner),
        )),
        storage,
        runtime,
    )
}

fn button(ui: &mut Window, pos: Pos2, button: PointerButton, pressed: bool) {
    ui.input_mut().events.push(Event::PointerButton {
        pos,
        button,
        pressed,
        modifiers: Modifiers::NONE,
    });
    ui.step();
}
fn move_to(ui: &mut Window, pos: Pos2) {
    ui.input_mut().events.push(Event::PointerMoved(pos));
    ui.step();
}
pub(super) fn context_action(ui: &mut Window, label: &str, action: &str) {
    let pos = ui.label_rects(label)[0].center();
    move_to(ui, pos);
    button(ui, pos, PointerButton::Secondary, true);
    button(ui, pos, PointerButton::Secondary, false);
    ui.run();
    assert!(ui.has_label(action), "missing context action {action}");
    ui.click_label(action);
    ui.run();
}
fn drag(ui: &mut Window, from: &str, to: &str) {
    let start = ui.label_rects(from)[0].center();
    let end = ui.label_rects(to)[0].center();
    move_to(ui, start);
    button(ui, start, PointerButton::Primary, true);
    move_to(ui, start + egui::vec2(12.0, 0.0));
    move_to(ui, end);
    button(ui, end, PointerButton::Primary, false);
    ui.run();
}
fn lane_id() -> ThreadId {
    ThreadId::new("lane-thread")
}
fn policy(ui: &Window) -> workspace_ui::LanePolicy {
    ui.state()
        .sidebar
        .threads
        .iter()
        .find(|t| t.id == lane_id())
        .unwrap()
        .lane
        .as_ref()
        .unwrap()
        .policy
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pointer_menu_and_drop_commit_through_runtime_and_ack_latest_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let (ui, storage, runtime) = actual_host(dir.path());
    let mut ui = Window::new(ui, [1280.0, 900.0]);
    ui.run();
    context_action(&mut ui, "↳ lane", "Report completion only (summary)");
    assert_eq!(
        storage.handle().lane("lane").unwrap().unwrap().policy,
        LanePolicy::Summary
    );
    context_action(&mut ui, "↳ lane", "Report milestones (attached)");
    assert_eq!(
        storage.handle().lane("lane").unwrap().unwrap().policy,
        LanePolicy::Attached
    );
    context_action(&mut ui, "↳ lane", "Detach lane");
    assert_eq!(policy(&ui), workspace_ui::LanePolicy::Detached);
    assert_eq!(
        ui.state().sidebar.family_key(&lane_id()).unwrap(),
        lane_id()
    );
    let intent = workspace_ui::lane_management::LaneDropIntent {
        source: ui.state().sidebar.lane_drag_intent(&lane_id()).unwrap(),
        target_thread_id: ThreadId::new("thread"),
        expected_lead_root_run_id: Some("run-1".into()),
    };
    drag(&mut ui, "lane", "thread");
    assert_eq!(policy(&ui), workspace_ui::LanePolicy::Summary);
    assert!(ui.state_mut().reattach_lane_drop(intent).is_err());
    repair(ui.state_mut());
    let lane = storage.handle().lane("lane").unwrap().unwrap();
    assert_eq!(lane.management_revision, 5, "one drop, never two commits");
    assert_eq!(lane.lead_thread_id.as_deref(), Some("thread"));
    assert!(
        storage
            .handle()
            .pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        ui.state().sidebar.family_key(&lane_id()).unwrap(),
        ThreadId::new("thread")
    );
    assert!(runtime.list_agents().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pointer_drop_displays_each_rejection_without_changing_the_ledger() {
    for reason in [
        LaneDropError::Child,
        LaneDropError::Worker,
        LaneDropError::LaneRoot,
        LaneDropError::Pinned,
        LaneDropError::Archived,
        LaneDropError::DifferentProject,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (mut state, storage, runtime) = actual_host(dir.path());
        state.manage_lane(detach(1), None).unwrap();
        match reason {
            LaneDropError::Child => {
                state.sidebar.threads[0].parent_thread_id = Some(lane_id());
            }
            LaneDropError::Worker => {
                state.sidebar.threads[0].chat_role = Some(ThreadChatRole::Worker);
            }
            LaneDropError::Pinned => {
                state.sidebar.set_family_pinned(&lane_id(), true).unwrap();
            }
            LaneDropError::Archived => {
                state
                    .sidebar
                    .set_family_archived(&lane_id(), true, 100)
                    .unwrap();
            }
            LaneDropError::DifferentProject => {
                let other = ProjectId::new("other");
                std::fs::create_dir(dir.path().join("other")).unwrap();
                state
                    .sidebar
                    .add_project(other.clone(), "Other", &dir.path().join("other"))
                    .unwrap();
                state.sidebar.threads[0].project_id = other;
            }
            LaneDropError::LaneRoot => {}
            _ => unreachable!(),
        }
        state.save_sidebar();
        state.force_thread_mirror();
        let mut ui = Window::new(state, [1280.0, 900.0]);
        ui.run();
        if reason == LaneDropError::Archived {
            ui.click_label("Archived (1)");
            ui.run();
        }
        let target = if reason == LaneDropError::LaneRoot {
            "lane"
        } else if reason == LaneDropError::Child {
            "↳ thread"
        } else {
            "thread"
        };
        drag(&mut ui, "lane", target);
        assert!(
            ui.has_label(&reason.to_string()),
            "{reason:?}: rejection must be visible"
        );
        let lane = storage.handle().lane("lane").unwrap().unwrap();
        assert_eq!(lane.management_revision, 2, "{reason:?}");
        assert_eq!(lane.policy, LanePolicy::Detached, "{reason:?}");
        assert_eq!(policy(&ui), workspace_ui::LanePolicy::Detached);
        assert!(runtime.list_agents().is_empty());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn runtime_replay_projects_current_lane_and_keeps_same_epoch_edits() {
    let dir = tempfile::tempdir().unwrap();
    let (mut ui, storage, runtime) = actual_host(dir.path());
    let request = LaneManagementRequest {
        operation_id: "policy-before-detach".into(),
        lane_id: "lane".into(),
        expected_management_revision: 1,
        action: LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    };
    ui.manage_lane(request.clone(), None).unwrap();
    ui.manage_lane(detach(2), None).unwrap();
    ui.sidebar.set_family_pinned(&lane_id(), true).unwrap();
    ui.save_sidebar();
    ui.manage_lane(request, None).unwrap();
    repair(&mut ui);
    let lane = storage.handle().lane("lane").unwrap().unwrap();
    let current = ui
        .sidebar
        .threads
        .iter()
        .find(|t| t.id == lane_id())
        .unwrap();
    assert!(current.pinned);
    assert_eq!(current.lane.as_ref().unwrap().management_revision, 3);
    assert_eq!(
        current.lane.as_ref().unwrap().policy,
        workspace_ui::LanePolicy::Detached
    );
    assert_eq!(lane.detached_since_management_revision, Some(3));
    assert!(
        storage
            .handle()
            .pending_lane_management_projections(10)
            .unwrap()
            .is_empty()
    );
    let host = GuiLaneManagementHost::new(runtime.clone());
    assert!(
        host.apply(
            LaneManagementRequest {
                operation_id: "no-guard".into(),
                lane_id: "lane".into(),
                expected_management_revision: 3,
                action: LaneManagementAction::Reattach {
                    lead_thread_id: "thread".into(),
                    expected_lead_root_run_id: "run-1".into(),
                },
            },
            None
        )
        .is_err()
    );
    assert_eq!(
        storage
            .handle()
            .lane("lane")
            .unwrap()
            .unwrap()
            .management_revision,
        3
    );
    assert!(runtime.list_agents().is_empty());
}
