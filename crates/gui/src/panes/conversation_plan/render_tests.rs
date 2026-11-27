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
fn without_a_plan_the_band_is_hidden() {
    let mut harness = band_harness(PlanScenario::Unplanned);
    harness.run();
    assert!(harness.query_all_by_label(NOT_PLANNED).next().is_none());
    assert!(harness.query_all_by_label(OPEN_LABEL).next().is_none());
    assert!(!harness.state().1);
}

fn panel_harness(
    scenario: PlanScenario,
) -> Harness<'static, (ConversationPlanModel, Vec<PlanPanelAction>)> {
    sized_panel_harness(scenario, egui::vec2(640.0, 900.0))
}

fn sized_panel_harness(
    scenario: PlanScenario,
    size: egui::Vec2,
) -> Harness<'static, (ConversationPlanModel, Vec<PlanPanelAction>)> {
    let mut harness = Harness::builder().with_size(size).build_ui_state(
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
fn the_detail_shows_the_blocker_before_the_description() {
    let harness = panel_harness(PlanScenario::Detailed);
    let blocker = harness.get_by_label("Blocker").rect();
    let description = harness.get_by_label("Description").rect();
    assert!(
        blocker.bottom() <= description.top(),
        "{blocker:?} {description:?}"
    );
}

/// The selected row's rectangle and the detail's blocker.
fn row_and_blocker(size: egui::Vec2) -> (egui::Rect, egui::Rect) {
    let harness = sized_panel_harness(PlanScenario::Detailed, size);
    let state = harness.state().0.state();
    let selected = state.selected().unwrap();
    let node = state
        .tree()
        .unwrap()
        .nodes
        .iter()
        .find(|node| node.task_id == selected)
        .unwrap()
        .clone();
    let row = harness.get_by_label(&node_label(&node)).rect();
    (row, harness.get_by_label("Blocker").rect())
}

#[test]
fn the_detail_sits_beside_the_tree_when_wide_and_below_it_when_narrow() {
    let (row, blocker) = row_and_blocker(egui::vec2(900.0, 600.0));
    assert!(row.right() <= blocker.left(), "{row:?} {blocker:?}");

    let (row, blocker) = row_and_blocker(egui::vec2(420.0, 900.0));
    assert!(row.bottom() <= blocker.top(), "{row:?} {blocker:?}");
}

#[test]
fn the_legend_gives_each_states_share_of_the_servers_weight() {
    let harness = panel_harness(PlanScenario::Mixed);
    let progress = harness.state().0.state().tree().unwrap().progress;
    for status in LEGEND_ORDER {
        let name = status_style(status).1;
        let share = percent(value(&progress.weighted_ppm, status));
        harness.get_by_label(&format!("{name} weight: {share}%"));
    }
}

#[test]
fn without_a_plan_the_panel_shows_the_empty_state() {
    let harness = panel_harness(PlanScenario::Unplanned);
    let labels: Vec<_> = harness.query_all_by_label(NOT_PLANNED).collect();
    assert!(labels.len() >= 2, "icon and text say the plan is empty");
    assert!(
        harness
            .query_all_by_role(egui::accesskit::Role::Button)
            .next()
            .is_none()
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

/// What a screen reader reads for a node: its label, or its value.
fn row_text(node: &egui_kittest::Node<'_>) -> String {
    let node = node.accesskit_node();
    node.label().or_else(|| node.value()).unwrap_or_default()
}

#[test]
fn a_cleared_plan_still_opens_from_the_band() {
    // Given: a plan that had tasks and has none now (revision above 0).
    let mut harness = band_harness(PlanScenario::Cleared);
    harness.run();
    assert!(harness.query_all_by_label(CLEARED).next().is_some());
    let open = harness.get_by_label(OPEN_LABEL);
    assert!(!open.accesskit_node().is_disabled());

    // When: the open button is used.
    open.click();
    harness.run();

    // Then: the tab is asked for, unlike a plan never written to.
    assert!(harness.state().1);
}

#[test]
fn a_cleared_plan_shows_its_history_which_ends_with_the_clearing() {
    // Given / When: the panel of a plan whose every task was deleted.
    let mut harness = panel_harness(PlanScenario::Cleared);

    // Then: it says so and keeps the history, newest change first.
    assert!(harness.query_all_by_label(NOT_PLANNED).next().is_none());
    assert!(harness.query_all_by_label(CLEARED).next().is_some());
    harness.get_by_label("History");
    let row = harness.get_by_label_contains("Revision 15, Cleared");
    assert!(row_text(&row).ends_with(", 10 tasks"));
    assert_eq!(harness.state().0.state().history_pages(), 2);
    let older = harness.get_by_label("Older changes");
    assert!(!older.accesskit_node().is_disabled());

    // And the older page can be asked for.
    older.click();
    harness.run();
    assert_eq!(harness.state().1, vec![PlanPanelAction::HistoryPage(1)]);
}

#[test]
fn a_plan_never_written_to_has_no_history_to_show() {
    let harness = panel_harness(PlanScenario::Unplanned);
    assert!(harness.query_all_by_label(CLEARED).next().is_none());
    assert!(harness.query_all_by_label("History").next().is_none());
}

#[test]
fn a_removed_task_is_named_in_the_history_by_its_id_when_no_title_is_saved() {
    // Given: the newest page, where task band-draft is deleted and neither the
    // tree nor the page's changes know its title.
    let harness = panel_harness(PlanScenario::Detailed);

    // Then: its id identifies the row, visibly and to a screen reader.
    let row = harness.get_by_label_contains("Revision 14, Removed");
    assert!(row_text(&row).ends_with(", band-draft"));
    assert!(harness.query_all_by_label("band-draft").next().is_some());
}

#[test]
fn a_removed_task_is_named_by_the_title_its_history_saved() {
    let entry = |revision, change| PlanHistoryEntry {
        revision: workbench_protocol::wire::Revision(revision),
        event_id: workbench_protocol::wire::Sequence(revision),
        change: Some(
            workbench_protocol::namespaces::execution::plan::PlanHistoryChange {
                cause: PlanChangeCause::ModelUpdate,
                source_run_id: None,
                changes: vec![change],
                progress: PlanProgress::default(),
            },
        ),
        updated_at_unix_ms: None,
    };
    let renamed = |title: &str| PlanChange::Updated {
        task_id: "gone".into(),
        title: Some(title.into()),
        description: None,
        result: None,
        blocker: None,
        weight: None,
        status: None,
    };
    let added = PlanChange::Added {
        task_id: "gone".into(),
        parent_id: None,
        position: 0,
        title: "First name".into(),
        description: String::new(),
        weight: 1,
        status: PlanTaskStatus::Pending,
    };
    // Newest first, as a page is.
    let entries = vec![
        entry(
            3,
            PlanChange::SubtreeRemoved {
                task_id: "gone".into(),
                task_ids: vec!["gone".into()],
            },
        ),
        entry(2, renamed("Second name")),
        entry(1, added),
    ];

    assert_eq!(task_title(&entries, &[], "gone"), Some("Second name"));
    assert_eq!(task_title(&entries[..1], &[], "gone"), None);
    assert_eq!(task_title(&entries, &[], "other"), None);
}
