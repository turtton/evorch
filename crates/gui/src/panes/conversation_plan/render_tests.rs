//! The band and the panel drawn headless from the fixture plans: they show the
//! server's figures, open on request, select and page, and edit nothing.
use std::sync::Arc;

use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::ids::ThreadId;

use super::*;
use crate::fixture::{FixturePlanReader, PlanScenario};
use crate::model::conversation_plan::{ConversationPlanModel, inline_spawner};

/// A model that has read the scenario's plan, as the open tab would.
fn model(scenario: PlanScenario) -> ConversationPlanModel {
    let mut model = ConversationPlanModel::default()
        .with_reader(Arc::new(FixturePlanReader::new(scenario)))
        .with_spawner(inline_spawner());
    model
        .state_mut()
        .set_thread(Some(ThreadId::new("t1").expect("id")));
    model.poll(true);
    if let Some(task) = scenario.selected_task() {
        model.state_mut().select(task);
        model.poll(true);
    }
    model
}

/// The theme takes effect from the next pass; the panel needs its text styles.
fn themed(ui: &egui::Ui) -> bool {
    crate::theme::install(ui.ctx());
    ui.style()
        .text_styles
        .contains_key(&crate::theme::tokens::text_style_badge())
}

fn band_harness(scenario: PlanScenario) -> Harness<'static, (ConversationPlanModel, bool)> {
    Harness::new_ui_state(
        |ui, (model, opened): &mut (ConversationPlanModel, bool)| {
            if !themed(ui) {
                return;
            }
            let summary = model.state().summary();
            *opened |= band_row(ui, PlanBand { summary });
        },
        (model(scenario), false),
    )
}

#[test]
fn the_band_shows_the_servers_progress_and_opens_on_request() {
    let mut harness = band_harness(PlanScenario::Mixed);
    harness.run();
    let summary = harness.state().0.state().summary().cloned().unwrap();
    let label = band_label(Some(&summary));
    harness.get_by_label(&label);
    harness.get_by_label(&format!(
        "Completed tasks: {}/{}",
        summary.progress.leaf_counts.completed, summary.progress.leaf_count
    ));
    assert!(!harness.state().1);

    harness.get_by_label(OPEN_LABEL).click();
    harness.run();
    assert!(harness.state().1, "the open button asks for the tab");
}

#[test]
fn without_a_plan_the_band_says_so_and_cannot_open() {
    let mut harness = band_harness(PlanScenario::Unplanned);
    harness.run();
    assert!(harness.query_all_by_label(NOT_PLANNED).next().is_some());
    let open = harness.get_by_label(OPEN_LABEL);
    assert!(open.accesskit_node().is_disabled());
    open.click();
    harness.run();
    assert!(!harness.state().1);
}

fn panel_harness(
    scenario: PlanScenario,
) -> Harness<'static, (ConversationPlanModel, Vec<PlanPanelAction>)> {
    let mut harness = Harness::builder()
        .with_size(egui::vec2(640.0, 900.0))
        .build_ui_state(
            |ui, (model, actions): &mut (ConversationPlanModel, Vec<PlanPanelAction>)| {
                if !themed(ui) {
                    return;
                }
                if let Some(action) = plan_panel(ui, model.state()) {
                    actions.push(action);
                }
            },
            (model(scenario), Vec::new()),
        );
    harness.run();
    harness
}

#[test]
fn a_tree_row_click_asks_to_select_that_task() {
    let mut harness = panel_harness(PlanScenario::Mixed);
    let node = harness.state().0.state().tree().unwrap().nodes[1].clone();
    harness.get_by_label(&node_label(&node)).click();
    harness.run();
    assert_eq!(
        harness.state().1,
        vec![PlanPanelAction::Select(node.task_id)]
    );
}

#[test]
fn the_history_pages_older_and_the_panel_edits_nothing() {
    let mut harness = panel_harness(PlanScenario::Detailed);
    assert!(harness.state().0.state().history_pages() > 1);
    // The newest page is first: there is nothing newer to go to.
    let newer = harness.get_by_label("Newer changes");
    assert!(newer.accesskit_node().is_disabled());
    harness.get_by_label("Older changes").click();
    harness.run();
    assert_eq!(harness.state().1, vec![PlanPanelAction::HistoryPage(1)]);

    // Read-only for people: no text field and no control other than
    // selecting a task and paging the history.
    assert!(
        harness
            .query_all_by_role(egui::accesskit::Role::TextInput)
            .next()
            .is_none()
    );
    let rows: Vec<String> = harness
        .state()
        .0
        .state()
        .tree()
        .unwrap()
        .nodes
        .iter()
        .map(node_label)
        .collect();
    let controls: Vec<_> = harness
        .query_all_by_role(egui::accesskit::Role::Button)
        .filter_map(|node| node.accesskit_node().label())
        .collect();
    assert!(
        controls.iter().all(|label| label == "Older changes"
            || label == "Newer changes"
            || rows.contains(label)),
        "{controls:?}"
    );
}
