use egui::{Pos2, Rect, pos2};
use gui::app::WorkbenchState;
use gui::fixture::{self, DemoSource};
use gui::headless::HeadlessWorkbench;
use workspace_ui::{ProjectId, ThreadId, UiSettings};

fn state() -> WorkbenchState<DemoSource> {
    WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default()).expect("workbench builds")
}

#[test]
fn logical_screen_size_is_preserved_when_pixels_per_point_is_two() {
    // Given: an 800 by 600 logical-point viewport.
    let state = state();
    // When: constructing a high-DPI harness.
    let harness = HeadlessWorkbench::with_pixels_per_point(state, [800.0, 600.0], 2.0);
    // Then: DPI changes without scaling the logical screen rectangle.
    assert_eq!(harness.pixels_per_point(), 2.0);
    assert_eq!(
        harness.screen_rect(),
        Rect::from_min_max(Pos2::ZERO, pos2(800.0, 600.0))
    );
}

#[test]
fn pixels_per_point_is_one_when_using_existing_constructor() {
    // Given: the existing state and size constructor inputs.
    let state = state();
    // When: using the unchanged constructor signature.
    let harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
    // Then: existing callers retain the default DPI.
    assert_eq!(harness.pixels_per_point(), 1.0);
}

#[test]
fn late_thread_is_reachable_when_scrolled_into_view() {
    // Given: a populated sidebar overflowing the logical screen.
    let root = tempfile::tempdir().expect("demo root");
    let mut sidebar = fixture::demo_sidebar(root.path()).expect("demo sidebar");
    for index in 0..40 {
        sidebar
            .create_thread(
                ThreadId::new(format!("overflow-{index:02}")),
                ProjectId::new("evorch"),
                format!("Overflow thread {index:02}"),
            )
            .expect("overflow thread");
    }
    let state = fixture::populate(state(), sidebar);
    let mut harness = HeadlessWorkbench::new(state, [800.0, 600.0]);
    harness.run();
    let label = "Overflow thread 39";
    let before = harness.label_rects(label)[0];
    assert!(!harness.screen_rect().intersects(before), "{before:?}");
    // When: requesting accesskit scrolling and running its frames.
    harness.scroll_label_into_view(label);
    harness.run();
    // Then: the complete target rectangle is inside the screen.
    let after = harness.label_rects(label)[0];
    assert!(harness.screen_rect().contains_rect(after), "{after:?}");
}
