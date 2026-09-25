use event_bus::AgentRunPhase;
use gui::{app::WorkbenchState, headless::HeadlessWorkbench, model::tasks::AgentRunSource};
use runtime::{AgentSummary, RunId};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

#[derive(Clone)]
struct Source(Vec<AgentSummary>);

impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

#[test]
fn agents_display_additional_role_names() {
    let roles = [(1, "Planner"), (2, "Oracle"), (3, "MultimodalLooker")];
    let source = Source(
        roles
            .iter()
            .map(|(id, role)| AgentSummary {
                run_id: RunId::new(*id),
                parent_run_id: Some(RunId::new(0)),
                name: format!("agent-{id}"),
                role_name: (*role).into(),
                phase: AgentRunPhase::Running,
                model: "vision-model".into(),
            })
            .collect(),
    );
    let root = tempfile::tempdir().expect("project root");
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("demo");
    sidebar
        .add_project(project.clone(), "demo", root.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    let thread_id = ThreadId::new("role-thread");
    sidebar
        .create_thread(thread_id.clone(), project, "Roles")
        .unwrap();
    sidebar.switch_thread(&thread_id).unwrap();
    sidebar.threads[0].run_ids = roles.iter().map(|(id, _)| format!("run-{id}")).collect();
    let state = WorkbenchState::new(source, &UiSettings::default())
        .expect("workbench")
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    for (_, role) in roles {
        assert!(
            harness.has_label(&format!("{role} · Running")),
            "missing role {role}"
        );
    }
    for (id, role) in roles {
        let run_id = format!("run-{id}");
        harness.state_mut().apply_events([event_bus::Event::new(
            event_bus::ToolEvent::ToolStarted {
                input: None,
                tool_name: "read".into(),
                call_id: format!("image-{id}"),
                run_id: Some(run_id.clone()),
            },
        )]);
        harness.run();
        harness.state_mut().drill_down(&run_id);
        harness.step();
        harness.run();
        assert!(harness.has_label(&format!("{run_id} / agent-{id} / {role}")));
        assert!(
            harness.count_labels("read") >= 1,
            "tool read should be visible for {run_id}"
        );
        harness.click_label("← Thread");
        harness.run();
    }
}
