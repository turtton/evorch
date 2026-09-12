use egui::vec2;
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::AgentRunPhase;
use gui::model::tasks::{AgentRunSource, TasksModel};
use runtime::{AgentSummary, RunId};

struct FixtureSource(Vec<AgentSummary>);

impl AgentRunSource for FixtureSource {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

fn run(id: u64, name: &'static str, phase: AgentRunPhase) -> AgentSummary {
    AgentSummary {
        run_id: RunId::new(id),
        name: name.into(),
        role_name: "worker".into(),
        phase,
        model: "test-model".into(),
    }
}

#[test]
fn tasks_pane_lists_multiple_concurrent_run_states() {
    // Given: concurrent runs covering every agent lifecycle phase.
    let source = FixtureSource(vec![
        run(1, "pending-run", AgentRunPhase::Pending),
        run(2, "running-run", AgentRunPhase::Running),
        run(3, "waiting-run", AgentRunPhase::Waiting),
        run(4, "done-run", AgentRunPhase::Done),
        run(5, "error-run", AgentRunPhase::Error),
    ]);
    let mut tasks = TasksModel::new(source);
    tasks.refresh();
    let mut harness = Harness::builder()
        .with_size(vec2(800.0, 480.0))
        .build_ui_state(
            |ui, tasks: &mut TasksModel<FixtureSource>| gui::panes::tasks::tasks_pane(ui, tasks),
            tasks,
        );
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);

    // When: the tasks pane is rendered.
    harness.run();

    // Then: every run name and its distinct phase label is visible.
    for (name, phase) in [
        ("pending-run", "Pending"),
        ("running-run", "Running"),
        ("waiting-run", "Waiting"),
        ("done-run", "Done"),
        ("error-run", "Error"),
    ] {
        assert!(harness.query_by_label(name).is_some(), "missing run {name}");
        assert!(
            harness.query_by_label(phase).is_some(),
            "missing phase {phase}"
        );
    }
}
