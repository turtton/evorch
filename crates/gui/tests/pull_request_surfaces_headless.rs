//! ADR 0043 Phase 1: the thread-row badge and "Link PR…" menu, the candidate chip, the "Link
//! PR…" dialog and the Notifications rows, drawn offscreen.

use std::collections::{BTreeMap, BTreeSet};

use egui_kittest::{Harness, kittest::NodeT, kittest::Queryable};
use gui::app::pull_requests::LinkDialog;
use gui::model::notifications::{NotificationKind, NotificationsModel};
use gui::model::pull_requests::{
    CandidateChip, ChipState, PrGlyph, PullRequestAction, PullRequestBadge,
};
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use gui::panes::notifications::{NotificationsAction, notifications_pane};
use gui::panes::pull_request_chip::{ChipsByCall, PullRequestChips, show as show_chips};
use gui::panes::pull_request_dialog::{LinkDialogAction, link_dialog};
use gui::panes::sidebar::{PullRequestsView, SidebarAction, sidebar_pane_with_pull_requests};
use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRunPhase};

fn labels(harness: &Harness<'_, impl Sized>) -> Vec<String> {
    harness
        .root()
        .children_recursive()
        .flat_map(|node| {
            let node = node.accesskit_node();
            [node.label(), node.value()]
        })
        .flatten()
        .map(|text| text.to_owned())
        .collect()
}

fn sidebar() -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "project", &std::env::temp_dir())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("t1"), project.clone(), "Linked thread")
        .unwrap();
    sidebar
        .create_thread(ThreadId::new("t2"), project, "Plain thread")
        .unwrap();
    sidebar
}

fn sidebar_harness(
    badges: BTreeMap<ThreadId, PullRequestBadge>,
    enabled: bool,
) -> Harness<'static, Option<SidebarAction>> {
    let sidebar = sidebar();
    let view = workspace_ui::SidebarView::default();
    Harness::builder()
        .with_size(egui::vec2(420.0, 360.0))
        .build_ui_state(
            move |ui, state: &mut Option<SidebarAction>| {
                gui::theme::install(ui.ctx());
                let none = BTreeSet::new();
                let phases: BTreeMap<String, ThreadRunPhase> = BTreeMap::new();
                if let Some(action) = sidebar_pane_with_pull_requests(
                    ui,
                    &sidebar,
                    (&phases, &none, &none, &none),
                    (&none, &BTreeMap::new()),
                    &view,
                    PullRequestsView {
                        enabled,
                        badges: Some(&badges),
                    },
                ) {
                    *state = Some(action);
                }
            },
            None,
        )
}

// Given: a linked and a plain thread / Then: only the linked row has a badge, with its glyph
// class in words for assistive technology
#[test]
fn a_linked_thread_row_has_a_badge_and_a_plain_one_has_none() {
    let badges = BTreeMap::from([(
        ThreadId::new("t1"),
        PullRequestBadge {
            number: 12,
            glyph: PrGlyph::Failing,
        },
    )]);
    let mut h = sidebar_harness(badges, true);
    h.run_steps(3);
    h.get_by_label("Pull request #12: failing");
    let all = labels(&h);
    assert_eq!(
        all.iter()
            .filter(|label| label.starts_with("Pull request #"))
            .count(),
        1,
        "{all:?}"
    );
}

// Given: a merged pull request / Then: the row reads "merged" outright
#[test]
fn a_merged_badge_reads_merged() {
    let badges = BTreeMap::from([(
        ThreadId::new("t1"),
        PullRequestBadge {
            number: 12,
            glyph: PrGlyph::Merged,
        },
    )]);
    let mut h = sidebar_harness(badges, true);
    h.run_steps(3);
    assert!(
        labels(&h).iter().any(|label| label == "#12 merged"),
        "{:?}",
        labels(&h)
    );
}

// Given: the thread name's right-click menu / Then: "Link PR…" sits with the copy items and asks
// for the dialog of that thread; it is absent while the feature is off
#[test]
fn the_thread_menu_offers_link_pr_when_the_feature_is_on() {
    let mut h = sidebar_harness(BTreeMap::new(), true);
    h.run_steps(3);
    h.get_by_label("Plain thread").click_secondary();
    h.run_steps(2);
    h.get_by_label("Copy thread ID");
    h.get_by_label("Link PR…").click();
    h.run_steps(2);
    assert_eq!(
        h.state(),
        &Some(SidebarAction::LinkPullRequest(ThreadId::new("t2")))
    );

    let mut off = sidebar_harness(BTreeMap::new(), false);
    off.run_steps(3);
    off.get_by_label("Plain thread").click_secondary();
    off.run_steps(2);
    off.get_by_label("Copy thread ID");
    assert!(off.query_by_label("Link PR…").is_none());
}

fn tool_entry() -> TranscriptEntry {
    TranscriptEntry::Tool {
        run_id: Some("run-1".into()),
        tool_name: "shell".into(),
        call_id: "call-1".into(),
        input: None,
        output: None,
        detail: None,
        is_error: false,
        status: ToolStatus::Succeeded,
        reviews: Vec::new(),
        parent_call_id: None,
    }
}

fn chip_harness(state: ChipState) -> Harness<'static, Option<PullRequestAction>> {
    let by_call: ChipsByCall = BTreeMap::from([(
        ("run-1".to_owned(), "call-1".to_owned()),
        vec![CandidateChip {
            candidate_id: 7,
            label: "turtton/evorch#21".into(),
            state,
        }],
    )])
    .into_iter()
    .collect();
    Harness::builder()
        .with_size(egui::vec2(500.0, 120.0))
        .build_ui_state(
            move |ui, action: &mut Option<PullRequestAction>| {
                gui::theme::install(ui.ctx());
                let mut chips = PullRequestChips {
                    thread_id: "t1",
                    by_call: &by_call,
                    action,
                };
                show_chips(ui, &mut chips, &[tool_entry()]);
            },
            None,
        )
}

// Given: an offered candidate / Then: the label is the parsed pull request, with Link PR and
// Ignore; Link PR confirms that candidate
#[test]
fn an_offered_chip_names_the_parsed_pull_request_and_links_on_one_click() {
    let mut h = chip_harness(ChipState::Offered);
    h.run_steps(3);
    assert!(labels(&h).iter().any(|label| label == "turtton/evorch#21"));
    h.get_by_label("Ignore");
    h.get_by_label("Link PR turtton/evorch#21").click();
    h.run_steps(2);
    assert_eq!(
        h.state(),
        &Some(PullRequestAction::Confirm {
            thread_id: "t1".into(),
            candidate_id: 7
        })
    );
}

#[test]
fn ignore_dismisses_the_candidate() {
    let mut h = chip_harness(ChipState::Offered);
    h.run_steps(3);
    h.get_by_label("Ignore").click();
    h.run_steps(2);
    assert_eq!(
        h.state(),
        &Some(PullRequestAction::Dismiss {
            thread_id: "t1".into(),
            candidate_id: 7
        })
    );
}

// Given: a linked candidate / Then: the chip is one line, with no buttons left
#[test]
fn a_linked_chip_is_one_line_without_buttons() {
    let mut h = chip_harness(ChipState::Linked);
    h.run_steps(3);
    assert!(
        labels(&h)
            .iter()
            .any(|label| label == "Linked pull request turtton/evorch#21")
    );
    assert!(h.query_by_label("Ignore").is_none());
    assert!(h.query_by_label_contains("Link PR").is_none());
}

fn dialog_harness(
    dialog: LinkDialog,
    loaded: bool,
) -> Harness<'static, (LinkDialog, Option<LinkDialogAction>)> {
    Harness::builder()
        .with_size(egui::vec2(700.0, 400.0))
        .build_ui_state(
            move |ui, (dialog, answer): &mut (LinkDialog, Option<LinkDialogAction>)| {
                let ctx = ui.ctx().clone();
                gui::theme::install(&ctx);
                if let Some(action) = link_dialog(&ctx, dialog, loaded) {
                    *answer = Some(action);
                }
            },
            (dialog, None),
        )
}

fn dialog(input: &str, slug: Option<&str>) -> LinkDialog {
    LinkDialog {
        thread_id: "t1".into(),
        input: input.into(),
        project_slug: slug.map(str::to_owned),
    }
}

// Given: a pull request of another repository / Then: a warning, and Link still works
#[test]
fn the_dialog_warns_about_a_mismatch_and_still_links() {
    let mut h = dialog_harness(dialog("other/repo#7", Some("turtton/evorch")), true);
    h.run_steps(3);
    assert!(
        labels(&h)
            .iter()
            .any(|label| label.contains("This pull request is in other/repo")),
        "{:?}",
        labels(&h)
    );
    h.get_by_label("Link").click();
    h.run_steps(2);
    assert_eq!(
        h.state().1,
        Some(LinkDialogAction::Link("other/repo#7".into()))
    );
}

// Given: text that is not a pull request, or a thread not read yet / Then: Link cannot be pressed
#[test]
fn the_dialog_refuses_nonsense_and_waits_for_the_thread() {
    for (input, loaded) in [("nonsense", true), ("a/b#1", false)] {
        let mut h = dialog_harness(dialog(input, None), loaded);
        h.run_steps(3);
        h.get_by_label("Link").click();
        h.run_steps(2);
        assert_eq!(h.state().1, None, "{input}");
    }
}

// Given: CI failed and merged notifications / Then: they read in the existing row form and open
// their thread
#[test]
fn pull_request_notifications_use_the_existing_rows() {
    let mut model = NotificationsModel::default();
    model.push_pull_request(
        NotificationKind::PullRequestChecksFailed {
            thread_id: ThreadId::new("t1"),
        },
        "CI failed on turtton/evorch#12 · Linked thread".into(),
    );
    model.push_pull_request(
        NotificationKind::PullRequestMerged {
            thread_id: ThreadId::new("t2"),
        },
        "turtton/evorch#13 merged · Plain thread".into(),
    );
    let mut h = Harness::builder()
        .with_size(egui::vec2(700.0, 300.0))
        .build_ui_state(
            move |ui, state: &mut (NotificationsModel, Option<NotificationsAction>)| {
                gui::theme::install(ui.ctx());
                if let Some(action) = notifications_pane(ui, &mut state.0, None) {
                    state.1 = Some(action);
                }
            },
            (model, None),
        );
    h.run_steps(3);
    h.get_by_label("CI failed");
    h.get_by_label("Merged");
    h.get_by_label("CI failed on turtton/evorch#12 · Linked thread")
        .click();
    h.run_steps(2);
    assert_eq!(
        h.state().1,
        Some(NotificationsAction::OpenThread(ThreadId::new("t1")))
    );
}
