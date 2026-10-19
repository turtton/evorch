#![cfg(feature = "browser")]

use egui_kittest::kittest::Queryable;
use gui::browser::{BrowserAction, BrowserPane, FakeFrameSource, FrameSource};

#[test]
fn click_displays_action_log_and_dom_diff() {
    let source = FakeFrameSource {
        reports: [gui::browser::BrowserReport {
            action: "click".into(),
            error: None,
            removed: "before".into(),
            inserted: "after".into(),
        }]
        .into(),
        ..Default::default()
    };
    let mut harness = egui_kittest::Harness::builder().build_ui_state(
        |ui, pane: &mut BrowserPane<FakeFrameSource>| pane.render(ui),
        BrowserPane::new(source),
    );
    harness.get_by_label("Click element").click();
    harness.run();
    assert!(harness.query_by_label("click: completed").is_some());
    assert!(harness.query_by_label("- before").is_some());
    assert!(harness.query_by_label("+ after").is_some());
}

#[test]
fn fake_delivers_each_frame_once() {
    // Given: a scripted source with one decoded frame.
    let mut source = FakeFrameSource::default();
    source
        .frames
        .push_back(egui::ColorImage::filled([2, 2], egui::Color32::RED));
    // When: the frame is polled.
    let frame = source.poll_frame().expect("frame");
    // Then: it is delivered once with unchanged dimensions.
    assert_eq!(frame.size, [2, 2]);
    assert!(source.poll_frame().is_none());
}

#[test]
fn pane_uploads_fake_frame_without_launching_browser() {
    // Given: an injected fake, not Chromium.
    let mut source = FakeFrameSource::default();
    source
        .frames
        .push_back(egui::ColorImage::filled([2, 3], egui::Color32::RED));
    let mut pane = BrowserPane::new(source);
    let ctx = egui::Context::default();
    // When: a real egui frame renders the pane.
    let mut output = ctx.run_ui(egui::RawInput::default(), |ui| pane.render(ui));
    // Then: the JPEG-decoded pixel seam reaches an egui texture.
    assert_eq!(pane.texture_size(), Some([2, 3]));
    assert!(!output.textures_delta.set.is_empty());
    output.textures_delta.clear();
}

#[test]
fn fake_records_action_without_external_side_effects() {
    // Given: an idle fake.
    let mut source = FakeFrameSource::default();
    // When: a navigation is submitted.
    source
        .submit(BrowserAction::Navigate(
            "https://example.org/".parse().expect("URL"),
        ))
        .expect("submit");
    // Then: the action is observable without any browser process.
    assert_eq!(source.actions.len(), 1);
}

#[test]
fn file_navigation_is_rejected_before_submission() {
    let mut source = FakeFrameSource::default();
    let result = source.submit(BrowserAction::Navigate(
        "file:///etc/passwd".parse().expect("URL"),
    ));
    assert!(matches!(result, Err(gui::browser::BrowserError::UrlScheme)));
    assert!(source.actions.is_empty());
}

#[test]
fn failed_action_keeps_dom_evidence_visible_across_frames() {
    let source = FakeFrameSource {
        reports: [gui::browser::BrowserReport {
            action: "click #submit".into(),
            error: Some("element detached".into()),
            removed: "<button id=submit>Send</button>".into(),
            inserted: "<p>retry</p>".into(),
        }]
        .into(),
        ..Default::default()
    };
    let mut harness = egui_kittest::Harness::builder().build_ui_state(
        |ui, pane: &mut BrowserPane<FakeFrameSource>| pane.render(ui),
        BrowserPane::new(source),
    );
    harness.get_by_label("Click element").click();
    harness.run();
    harness.run_steps(3);
    assert!(
        harness
            .query_by_label("click #submit: element detached")
            .is_some()
    );
    assert!(
        harness
            .query_by_label("- <button id=submit>Send</button>")
            .is_some()
    );
    assert!(harness.query_by_label("+ <p>retry</p>").is_some());
}

fn agent_pane(
    view: gui::browser::FakeAgentView,
) -> egui_kittest::Harness<'static, gui::browser::AgentBrowserPane<gui::browser::FakeAgentView>> {
    egui_kittest::Harness::builder()
        .with_size(egui::vec2(900.0, 700.0))
        .build_ui_state(
            |ui, pane: &mut gui::browser::AgentBrowserPane<gui::browser::FakeAgentView>| {
                pane.render(ui)
            },
            gui::browser::AgentBrowserPane::new(view),
        )
}

#[test]
fn agent_pane_screencasts_the_first_run_with_its_activity_overlaid() {
    // Given: two runs with open pages, a frame and recent activity.
    use gui::browser::Activity;
    let view = gui::browser::FakeAgentView {
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

    // When: the pane renders.
    let mut harness = agent_pane(view);
    harness.run();

    // Then: only the first run is observed, and its frame and activity show.
    assert_eq!(harness.state().view().observed, [Some("run-a".to_owned())]);
    assert_eq!(harness.state().texture_size(), Some([320, 200]));
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
fn agent_pane_switches_runs_and_stops_when_the_page_closes() {
    // Given: a pane observing the first of two runs.
    let view = gui::browser::FakeAgentView {
        pages: vec!["run-a".into(), "run-b".into()],
        ..Default::default()
    };
    let mut harness = agent_pane(view);
    harness.run();

    // When: the user picks the other run, and then its page closes.
    harness.get_by_label("Run").click();
    harness.run();
    harness.get_by_label("run-b").click();
    harness.run();
    let switched = harness.state().selected().map(str::to_owned);
    harness.state_mut().view_mut().pages.clear();
    harness.run();

    // Then: observation moved to run-b and stopped once nothing is open.
    assert_eq!(switched.as_deref(), Some("run-b"));
    assert_eq!(
        harness.state().view().observed,
        [Some("run-a".to_owned()), Some("run-b".to_owned()), None]
    );
    assert!(
        harness
            .query_by_label_contains("No agent page is open")
            .is_some()
    );
}
