use egui::accesskit::Role;
use egui::vec2;
use egui_kittest::{
    Harness,
    kittest::{By, Queryable},
};
use workspace_ui::{PanelId, UiSettings};

#[derive(Clone)]
struct MockSource(Vec<runtime::AgentSummary>);

impl gui::model::tasks::AgentRunSource for MockSource {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        self.0.clone()
    }
}

fn build_harness(
    state: gui::app::WorkbenchState<MockSource>,
) -> Harness<'static, gui::app::WorkbenchState<MockSource>> {
    Harness::builder()
        .with_size(vec2(800.0, 600.0))
        .build_ui_state(
            |ui, state: &mut gui::app::WorkbenchState<MockSource>| {
                state.ui(ui, &mut eframe::Frame::_new_kittest());
            },
            state,
        )
}

fn activate_tab(harness: &mut Harness<'static, gui::app::WorkbenchState<MockSource>>, id: &str) {
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new(id))
        .expect("panel tab exists");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("activate tab");
}

#[test]
fn right_panes_expose_landmarks_without_headings() {
    // Given: a default workbench state
    let state = gui::app::WorkbenchState::new(MockSource(Vec::new()), &UiSettings::default())
        .expect("default state builds");
    let mut harness = build_harness(state);
    harness.run();

    // Then: each right pane title resolves to exactly one Pane landmark when active.
    for (tab_id, title) in [
        ("agents-main", "Agents"),
        ("goal-main", "Goal"),
        ("merge-main", "Merge"),
    ] {
        activate_tab(&mut harness, tab_id);
        harness.run();
        harness.get_by_label(title);
        assert_eq!(harness.query_all_by_label(title).count(), 1);
        assert!(
            harness
                .query(By::new().role(Role::Pane).label(title))
                .is_some()
        );
    }
}

#[test]
fn merge_pane_without_pr_shows_single_placeholder() {
    // Given: a default workbench state with the Merge tab active
    let state = gui::app::WorkbenchState::new(MockSource(Vec::new()), &UiSettings::default())
        .expect("default state builds");
    let mut harness = build_harness(state);
    harness.run();
    activate_tab(&mut harness, "merge-main");
    harness.run();

    // Then: the "no pull request" placeholder appears exactly once.
    assert_eq!(harness.query_all_by_label("no pull request").count(), 1);
}
