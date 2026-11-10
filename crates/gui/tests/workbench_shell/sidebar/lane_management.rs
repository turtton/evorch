//! Pointer gestures emit management intents; the tree waits for host commit.
use egui_kittest::{Harness, kittest::Queryable};
use gui::panes::sidebar::{SidebarAction, sidebar_pane};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Mutex},
};
use workspace_ui::{LaneLink, LanePolicy, ProjectId, SidebarState, ThreadChatRole, ThreadId};

struct View {
    harness: Harness<'static>,
    actions: Arc<Mutex<Vec<SidebarAction>>>,
}

fn sidebar(policy: LanePolicy) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(
            project.clone(),
            "Project",
            &std::env::current_dir().unwrap(),
        )
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for (id, title) in [("lead", "Destination"), ("lane-thread", "Independent")] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), title)
            .unwrap();
        let thread = sidebar.threads.last_mut().unwrap();
        thread.chat_role = Some(ThreadChatRole::Orchestrator);
        thread.root_run_id = Some(format!("{id}-root"));
    }
    sidebar
        .apply_lane_link(
            &ThreadId::new("lane-thread"),
            LaneLink {
                lane_id: "lane".into(),
                origin_thread_id: ThreadId::new("lead"),
                lead_thread_id: policy.is_managed().then(|| ThreadId::new("lead")),
                policy,
                management_revision: 7,
                detached_since_management_revision: (policy == LanePolicy::Detached).then_some(7),
            },
        )
        .unwrap();
    sidebar
}

fn render(sidebar: SidebarState) -> View {
    let actions = Arc::new(Mutex::new(Vec::new()));
    let emitted = actions.clone();
    let mut harness = Harness::builder()
        .with_size(egui::vec2(420.0, 460.0))
        .build_ui(move |ui| {
            gui::theme::install(ui.ctx());
            if let Some(action) = sidebar_pane(
                ui,
                &sidebar,
                &BTreeMap::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
                &BTreeSet::new(),
            ) {
                emitted.lock().unwrap().push(action);
            }
        });
    harness.run();
    View { harness, actions }
}

#[test]
fn pointer_drag_emits_typed_drop_without_optimistic_reparent() {
    let mut view = render(sidebar(LanePolicy::Detached));
    let start = view.harness.get_by_label("Independent").rect().center();
    let target = view.harness.get_by_label("Destination").rect().center();
    view.harness.hover_at(start);
    view.harness.run_steps(1);
    view.harness.drag_at(start);
    view.harness.run_steps(1);
    view.harness.hover_at(target);
    view.harness.run_steps(2);
    view.harness.event(egui::Event::PointerButton {
        pos: target,
        button: egui::PointerButton::Primary,
        pressed: false,
        modifiers: egui::Modifiers::NONE,
    });
    view.harness.run_steps(2);
    let actions = view.actions.lock().unwrap();
    let drops: Vec<_> = actions
        .iter()
        .filter_map(|action| match action {
            SidebarAction::ReattachLane(intent) => Some(intent),
            _ => None,
        })
        .collect();
    assert_eq!(drops.len(), 1);
    let intent = drops[0];
    assert_eq!(intent.source.lane_id, "lane");
    assert_eq!(intent.source.expected_management_revision, 7);
    assert_eq!(intent.source.expected_family_revision.membership_epoch, 7);
    assert_eq!(intent.target_thread_id, ThreadId::new("lead"));
    assert_eq!(
        intent.expected_lead_root_run_id.as_deref(),
        Some("lead-root")
    );
    // No host has committed: the independent row remains a top-level row.
    assert!(view.harness.query_by_label("Independent").is_some());
    assert!(view.harness.query_by_label("↳ Independent").is_none());
}

#[test]
fn lane_context_menu_emits_policy_and_detach_cas_requests() {
    use storage::lane_management::{LaneManagementAction, ManagedLanePolicy};
    for (label, expected) in [
        (
            "Report milestones (attached)",
            LaneManagementAction::SetPolicy {
                policy: ManagedLanePolicy::Attached,
            },
        ),
        (
            "Report completion only (summary)",
            LaneManagementAction::SetPolicy {
                policy: ManagedLanePolicy::Summary,
            },
        ),
        ("Detach lane", LaneManagementAction::Detach),
    ] {
        let mut view = render(sidebar(LanePolicy::Attached));
        view.harness.get_by_label("↳ Independent").click_secondary();
        view.harness.run();
        view.harness.get_by_label(label).click();
        view.harness.run();
        let actions = view.actions.lock().unwrap();
        let Some(SidebarAction::ManageLane(request)) = actions.last() else {
            panic!("management intent missing");
        };
        assert_eq!(request.lane_id, "lane");
        assert_eq!(request.expected_management_revision, 7);
        assert_eq!(request.action, expected);
        assert!(!request.operation_id.is_empty());
    }
}
