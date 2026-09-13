use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::model::tasks::AgentRunSource;
use runtime::AgentSummary;
use workspace_ui::{PanelId, UiSettings};

#[derive(Clone)]
struct Source(Vec<AgentSummary>);

impl AgentRunSource for Source {
    fn list(&self) -> Vec<AgentSummary> {
        self.0.clone()
    }
}

fn terminal_harness() -> HeadlessWorkbench<Source> {
    let state = WorkbenchState::new(Source(Vec::new()), &UiSettings::default())
        .expect("default state builds");
    let mut harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
    let path = harness
        .state()
        .dock()
        .find_tab(&PanelId::new("terminal-main"))
        .expect("terminal tab exists");
    harness
        .state_mut()
        .dock_mut()
        .set_active_tab(path)
        .expect("terminal tab can be activated");
    harness.run();
    harness
}

// Given: terminal with fed content / When: terminal tab active / Then: lines render as labels.
#[test]
fn terminal_pane_renders_buffer_lines() {
    let mut harness = terminal_harness();
    harness
        .state_mut()
        .feed_terminal(b"$ cargo test\nrunning 42 tests\nall tests passed");
    harness.run();

    assert!(harness.has_label("$ cargo test"));
    assert!(harness.has_label("running 42 tests"));
    assert!(harness.has_label("all tests passed"));
}

// Given: terminal with CJK content / When: terminal tab active / Then: CJK lines render.
#[test]
fn terminal_pane_renders_cjk_lines() {
    let mut harness = terminal_harness();
    harness
        .state_mut()
        .feed_terminal("コンパイル成功\nテスト結果: 全緑".as_bytes());
    harness.run();

    assert!(harness.has_label("コンパイル成功"));
    assert!(harness.has_label("テスト結果: 全緑"));
}

// Given: empty terminal / When: terminal tab active / Then: pane renders without error.
#[test]
fn terminal_pane_renders_empty_buffer() {
    let harness = terminal_harness();

    assert!(harness.has_label("Projects"));
}

// Given: terminal with content / When: geometry check / Then: content labels are within viewport.
#[test]
fn terminal_content_labels_within_viewport() {
    let mut harness = terminal_harness();
    harness
        .state_mut()
        .feed_terminal(b"line one\nline two\nline three");
    harness.run();

    let screen = harness.screen_rect();
    for label in ["line one", "line two", "line three"] {
        let rects = harness.label_rects(label);
        assert!(
            !rects.is_empty(),
            "label {label:?} should have at least one rect"
        );
        for rect in &rects {
            assert!(
                screen.contains_rect(*rect),
                "label {label:?} rect {rect:?} is outside viewport {screen:?}"
            );
        }
    }
}
