#![cfg(feature = "browser")]

use egui_kittest::kittest::Queryable;
use gui::browser::{Activity, AgentBrowserPane, FakeAgentView};
use gui::panes::agent_browser::{AgentBrowserSurface, BrowserRun, BrowserTabView};

/// What the workbench hands the pane each frame.
#[derive(Default)]
struct Tab {
    runs: Vec<BrowserRun>,
    selected: Option<String>,
    picked: Vec<String>,
}

fn run(id: &str, label: &str) -> BrowserRun {
    BrowserRun {
        run_id: id.into(),
        label: label.into(),
    }
}

type Harness = egui_kittest::Harness<'static, (AgentBrowserPane<FakeAgentView>, Tab)>;

fn agent_pane(view: FakeAgentView, tab: Tab) -> Harness {
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_ui_state(
            |ui, (pane, tab): &mut (AgentBrowserPane<FakeAgentView>, Tab)| {
                let view = BrowserTabView {
                    runs: &tab.runs,
                    selected: tab.selected.as_deref(),
                };
                if let Some(picked) = pane.render(ui, view) {
                    tab.picked.push(picked);
                }
            },
            (AgentBrowserPane::new(view), tab),
        )
}

#[test]
fn agent_pane_screencasts_the_selected_run_with_its_activity_overlaid() {
    // Given: two runs with open pages, a frame and recent activity.
    let view = FakeAgentView {
        pages: vec!["run-a".into(), "run-b".into()],
        frames: [egui::ColorImage::filled([320, 200], egui::Color32::GRAY)].into(),
        activity: vec![
            Activity::Action {
                label: "click e5".into(),
                error: None,
            },
            Activity::Action {
                label: "press Enter".into(),
                error: Some("boom".into()),
            },
            Activity::Blocked("https://tracker.test/ (refused)".into()),
        ],
        tab: Some("t2".into()),
        ..Default::default()
    };
    let tab = Tab {
        runs: vec![run("run-a", "Conversation"), run("run-b", "WebResearcher")],
        selected: Some("run-b".into()),
        ..Default::default()
    };

    // When: the pane renders.
    let mut harness = agent_pane(view, tab);
    harness.run();

    // Then: only the selected run is observed, and its frame and activity show.
    let (pane, _) = harness.state();
    assert_eq!(pane.view().observed, [Some("run-b".to_owned())]);
    assert_eq!(pane.texture_size("run-b"), Some([320, 200]));
    assert!(harness.query_by_label("click e5").is_some());
    assert!(harness.query_by_label("press Enter failed: boom").is_some());
    assert!(
        harness
            .query_by_label("blocked https://tracker.test/ (refused)")
            .is_some()
    );
    assert!(
        harness
            .query_by_label("Showing the active tab t2")
            .is_some()
    );
}

#[test]
fn agent_pane_reports_the_run_the_user_picks() {
    // Given: a pane showing the first of two runs, labelled by agent.
    let view = FakeAgentView {
        pages: vec!["run-a".into(), "run-b".into()],
        ..Default::default()
    };
    let tab = Tab {
        runs: vec![run("run-a", "Conversation"), run("run-b", "WebResearcher")],
        selected: Some("run-a".into()),
        ..Default::default()
    };
    let mut harness = agent_pane(view, tab);
    harness.run();

    // When: the user picks the other run.
    harness.get_by_label("Run").click();
    harness.run();
    harness.get_by_label("WebResearcher").click();
    harness.run();

    // Then: the pick goes to the workbench, which owns the selection.
    assert_eq!(harness.state().1.picked, ["run-b".to_owned()]);
}

#[test]
fn a_closed_page_keeps_its_last_frame_and_says_so() {
    // Given: a pane that received a frame of run-a's page.
    let view = FakeAgentView {
        pages: vec!["run-a".into()],
        frames: [egui::ColorImage::filled([4, 3], egui::Color32::RED)].into(),
        activity: vec![Activity::Action {
            label: "navigate https://example.test/".into(),
            error: None,
        }],
        ..Default::default()
    };
    let tab = Tab {
        runs: vec![run("run-a", "WebResearcher")],
        selected: Some("run-a".into()),
        ..Default::default()
    };
    let mut harness = agent_pane(view, tab);
    harness.run();

    // When: the page closes.
    harness.state_mut().0.view_mut().pages.clear();
    harness.run();

    // Then: the last frame and activity stay, with a closed notice and no
    // further observation requests.
    let (pane, _) = harness.state();
    assert_eq!(pane.texture_size("run-a"), Some([4, 3]));
    assert_eq!(pane.view().observed, [Some("run-a".to_owned())]);
    assert!(harness.query_by_label("The page was closed.").is_some());
    assert!(
        harness
            .query_by_label("navigate https://example.test/")
            .is_some()
    );
}

#[test]
fn stopping_ends_the_screencast_and_showing_again_resumes_it() {
    // Given: a pane screencasting run-a.
    let view = FakeAgentView {
        pages: vec!["run-a".into()],
        ..Default::default()
    };
    let tab = Tab {
        runs: vec![run("run-a", "WebResearcher")],
        selected: Some("run-a".into()),
        ..Default::default()
    };
    let mut harness = agent_pane(view, tab);
    harness.run();

    // When: the tab goes out of sight, and later draws again.
    let ctx = harness.ctx.clone();
    harness.state_mut().0.stop(&ctx);
    let stopped = harness.state().0.observing().map(str::to_owned);
    harness.run();

    // Then: observation stopped, then resumed for the same run.
    assert_eq!(stopped, None);
    assert_eq!(
        harness.state().0.view().observed,
        [Some("run-a".to_owned()), None, Some("run-a".to_owned())]
    );
}

#[test]
fn a_conversation_without_pages_shows_the_empty_state() {
    let mut harness = agent_pane(FakeAgentView::default(), Tab::default());
    harness.run();
    assert!(
        harness
            .query_by_label_contains("No agent page is open in this conversation")
            .is_some()
    );
    assert!(harness.state().0.view().observed.is_empty());
}
