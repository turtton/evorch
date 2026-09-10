use event_bus::AgentRunPhase;
use gui::{app::WorkbenchState, headless::HeadlessWorkbench, model::tasks::AgentRunSource};
use runtime::{AgentSummary, RunId};
use workspace_ui::UiSettings;

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
                name: format!("agent-{id}"),
                role_name: (*role).into(),
                phase: AgentRunPhase::Running,
                model: "vision-model".into(),
            })
            .collect(),
    );
    let state = WorkbenchState::new(source, &UiSettings::default()).expect("workbench");
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    for (_, role) in roles {
        assert!(harness.has_label(role), "missing role {role}");
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
        harness.click_label(&run_id);
        harness.step();
        harness.run();
        assert!(harness.has_label(&format!("{run_id} / agent-{id} / {role}")));
        assert!(harness.has_label(&format!(" Running read (image-{id})")));
        harness.click_label("← Thread");
        harness.run();
    }
}
