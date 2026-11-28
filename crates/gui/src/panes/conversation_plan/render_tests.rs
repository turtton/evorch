//! The band and the panel drawn headless from the fixture plans: they show the
//! server's figures, open on request, select and page, and edit nothing.

use super::*;

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

mod panel {
    use egui_kittest::{
        Harness,
        kittest::{NodeT, Queryable},
    };
    use workbench_protocol::ids::ThreadId;
    use workbench_protocol::namespaces::ExecutionReply;
    use workbench_protocol::namespaces::execution::plan::{
        PlanChange, PlanChangeCause, PlanHistoryChange, PlanHistoryEntry, PlanHistoryPage,
        PlanNodeView, PlanProgress, PlanStatusValues, PlanSummary, PlanTaskStatus, PlanTreePage,
    };
    use workbench_protocol::wire::{Revision, Sequence};

    use super::super::*;
    use crate::model::conversation_plan::{PlanState, ReadKind};

    /// The theme takes effect from the next pass; the panel needs its text styles.
    fn themed(ui: &egui::Ui) -> bool {
        crate::theme::install(ui.ctx());
        ui.style()
            .text_styles
            .contains_key(&crate::theme::tokens::text_style_badge())
    }

    fn progress(nodes: u32) -> PlanProgress {
        PlanProgress {
            node_count: nodes,
            leaf_count: nodes,
            leaf_counts: PlanStatusValues {
                pending: nodes,
                ..PlanStatusValues::default()
            },
            weighted_ppm: PlanStatusValues {
                pending: if nodes == 0 { 0 } else { 1_000_000 },
                ..PlanStatusValues::default()
            },
        }
    }

    fn node(task_id: &str) -> PlanNodeView {
        PlanNodeView {
            task_id: task_id.into(),
            parent_id: None,
            position: 0,
            depth: 0,
            title: format!("Task {task_id}"),
            weight: 1,
            status: Some(PlanTaskStatus::Pending),
            child_count: 0,
            progress_ppm: PlanStatusValues::default(),
        }
    }

    fn history_page(revision: u64, change_at_top: Option<PlanChange>) -> PlanHistoryPage {
        let top = revision;
        PlanHistoryPage {
            plan_id: Some("plan-1".into()),
            revision: Revision(revision),
            entries: (top.saturating_sub(9).max(1)..=top)
                .map(|at| PlanHistoryEntry {
                    revision: Revision(at),
                    event_id: Sequence(at),
                    change: (at == top)
                        .then(|| change_at_top.clone())
                        .flatten()
                        .map(|change| PlanHistoryChange {
                            cause: PlanChangeCause::ModelUpdate,
                            source_run_id: None,
                            changes: vec![change],
                            progress: progress(0),
                        }),
                    updated_at_unix_ms: None,
                })
                .collect(),
            next_after_revision: None,
        }
    }

    /// The plan of thread `t1` as the core would have answered every read (history oldest first,
    /// as the protocol sends it).
    fn plan(revision: u64, tasks: &[&str], newest_change: Option<PlanChange>) -> PlanState {
        let thread = ThreadId::new("t1").expect("thread id");
        let mut state = PlanState::default();
        state.set_thread(Some(thread.clone()));
        // The history is asked at the revision known when it leaves, so a first round that learns
        // the revision leaves it due again.
        for _ in 0..3 {
            for request in state.requests(true) {
                let answer = match request.kind {
                    ReadKind::Summary => ExecutionReply::ConversationPlan(Box::new(PlanSummary {
                        thread: thread.clone(),
                        plan_id: (revision > 0).then(|| "plan-1".into()),
                        revision: Revision(revision),
                        progress: progress(u32::try_from(tasks.len()).unwrap()),
                        updated_at_unix_ms: None,
                    })),
                    ReadKind::Tree => {
                        ExecutionReply::ConversationPlanTree(Box::new(PlanTreePage {
                            plan_id: (revision > 0).then(|| "plan-1".into()),
                            revision: Revision(revision),
                            progress: progress(u32::try_from(tasks.len()).unwrap()),
                            nodes: tasks.iter().map(|id| node(id)).collect(),
                            next_cursor: None,
                        }))
                    }
                    ReadKind::History => ExecutionReply::ConversationPlanHistory(Box::new(
                        history_page(revision, newest_change.clone()),
                    )),
                    ReadKind::Task => continue,
                };
                state.apply(request.kind, request.generation, Ok(answer));
            }
        }
        state
    }

    fn cleared() -> PlanState {
        plan(15, &[], Some(PlanChange::Cleared { removed: 10 }))
    }

    fn band(state: PlanState) -> Harness<'static, (PlanState, bool)> {
        let mut harness = Harness::new_ui_state(
            |ui, (state, opened): &mut (PlanState, bool)| {
                if !themed(ui) {
                    return;
                }
                *opened |= band_row(
                    ui,
                    PlanBand {
                        summary: state.summary(),
                    },
                );
            },
            (state, false),
        );
        harness.run_steps(3);
        harness
    }

    fn panel(state: PlanState) -> Harness<'static, (PlanState, Vec<PlanPanelAction>)> {
        let mut harness = Harness::builder()
            .with_size(egui::vec2(640.0, 900.0))
            .build_ui_state(
                |ui, (state, actions): &mut (PlanState, Vec<PlanPanelAction>)| {
                    if !themed(ui) {
                        return;
                    }
                    if let Some(action) = plan_panel(ui, state) {
                        actions.push(action);
                    }
                },
                (state, Vec::new()),
            );
        harness.run_steps(3);
        harness
    }

    // Given: a plan that had tasks and has none now (revision above 0)
    // When: the band is drawn and its open button used
    // Then: the band says the plan was cleared and asks for the tab, unlike a plan never
    // written to, which has no band row at all
    #[test]
    fn a_cleared_plan_still_opens_from_the_band() {
        let mut harness = band(cleared());
        assert!(harness.query_all_by_label(CLEARED).count() >= 1);
        let open = harness.get_by_label(OPEN_LABEL);
        assert!(!open.accesskit_node().is_disabled());
        open.click();
        harness.run_steps(2);
        assert!(harness.state().1, "the open button asks for the tab");

        let never_planned = band(plan(0, &[], None));
        assert!(
            never_planned
                .query_all_by_label(NOT_PLANNED)
                .next()
                .is_none()
        );
        assert!(
            never_planned
                .query_all_by_label(OPEN_LABEL)
                .next()
                .is_none()
        );
    }

    // Given: the panel of a plan whose every task was deleted
    // Then: it says so and keeps the history, newest change first, and the older page can be
    // asked for
    #[test]
    fn a_cleared_plan_shows_its_history_which_ends_with_the_clearing() {
        let mut harness = panel(cleared());
        assert!(harness.query_all_by_label(NOT_PLANNED).next().is_none());
        assert!(harness.query_all_by_label(CLEARED).next().is_some());
        harness.get_by_label("History");
        let newest = harness.get_by_label_contains("Revision 15, Cleared");
        let text = {
            let node = newest.accesskit_node();
            node.label().or_else(|| node.value()).unwrap_or_default()
        };
        assert!(text.ends_with(", 10 tasks"), "{text:?}");
        assert_eq!(harness.state().0.history_pages(), 2);
        let older = harness.get_by_label("Older changes");
        assert!(!older.accesskit_node().is_disabled());
        older.click();
        harness.run_steps(2);
        assert_eq!(harness.state().1, vec![PlanPanelAction::HistoryPage(1)]);
    }

    // Given: a conversation that never had a plan
    // Then: the panel shows the empty state and nothing to press, and no history
    #[test]
    fn a_plan_never_written_to_shows_the_empty_state_and_no_history() {
        let harness = panel(plan(0, &[], None));
        assert!(harness.query_all_by_label(NOT_PLANNED).count() >= 2);
        assert!(harness.query_all_by_label(CLEARED).next().is_none());
        assert!(harness.query_all_by_label("History").next().is_none());
        assert!(
            harness
                .query_all_by_role(egui::accesskit::Role::Button)
                .next()
                .is_none()
        );
    }

    // Given: a plan with tasks and more than one page of history
    // Then: the newest page has nothing newer to go to, the older page and a task can be asked
    // for, and no other control exists (the panel is read-only for people)
    #[test]
    fn the_history_pages_older_and_the_panel_edits_nothing() {
        let mut harness = panel(plan(15, &["a", "b"], None));
        assert!(harness.state().0.history_pages() > 1);
        let newer = harness.get_by_label("Newer changes");
        assert!(newer.accesskit_node().is_disabled());
        harness.get_by_label("Older changes").click();
        harness.run_steps(2);
        assert_eq!(harness.state().1, vec![PlanPanelAction::HistoryPage(1)]);

        assert!(
            harness
                .query_all_by_role(egui::accesskit::Role::TextInput)
                .next()
                .is_none()
        );
        let rows: Vec<String> = harness
            .state()
            .0
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

        let mut harness = panel(plan(15, &["a", "b"], None));
        let second = harness.state().0.tree().unwrap().nodes[1].clone();
        harness.get_by_label(&node_label(&second)).click();
        harness.run_steps(2);
        assert_eq!(
            harness.state().1,
            vec![PlanPanelAction::Select(second.task_id)]
        );
    }
}
