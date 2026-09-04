use std::panic::{AssertUnwindSafe, catch_unwind};

use gui::app::WorkbenchState;
use gui::headless::{CapturedFrame, HeadlessWorkbench, OffscreenError};
use runtime::AgentSummary;
use workspace_ui::UiSettings;

#[derive(Clone)]
struct EmptySource;

impl gui::model::tasks::AgentRunSource for EmptySource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }
}

fn workbench() -> HeadlessWorkbench<EmptySource> {
    let state = WorkbenchState::new(EmptySource, &UiSettings::default())
        .expect("default workbench state must build");
    HeadlessWorkbench::new(state, [640.0, 360.0])
}

#[test]
fn run_executes_workbench_logic_and_exposes_state() {
    // Given: a headless workbench backed by build_ui_state
    let mut workbench = workbench();

    // When: one UI frame is run
    workbench.run();

    // Then: the rendered workbench state remains available for assertions
    assert_eq!(workbench.state().dock().iter_all_tabs().count(), 7);
}

#[test]
fn capture_returns_adapter_unavailable_without_panicking() {
    // Given: a headless workbench on a machine without a wgpu adapter
    let mut workbench = workbench();
    workbench.run();

    // When: capture is called through the panic-safe public boundary
    let outcome = catch_unwind(AssertUnwindSafe(|| workbench.capture()));

    // Then: adapter absence is typed, while adapter-enabled CI returns a valid frame
    let result = outcome.expect("capture must not unwind");
    match result {
        Err(OffscreenError::AdapterUnavailable(message)) => {
            assert!(message.to_ascii_lowercase().contains("no adapter found"));
        }
        Ok(frame) => {
            assert_eq!((frame.width, frame.height), (640, 360));
            assert_eq!(frame.rgba.len(), 640 * 360 * 4);
        }
        Err(error) => panic!("unexpected capture error: {error}"),
    }
}

#[test]
fn captured_frame_saves_a_decodable_png() {
    // Given: a synthetic two-pixel RGBA frame
    let directory = tempfile::tempdir().expect("temporary directory must be created");
    let path = directory.path().join("frame.png");
    let frame = CapturedFrame {
        width: 2,
        height: 1,
        rgba: vec![255, 0, 0, 255, 0, 255, 0, 255],
    };

    // When: the frame is saved with the PNG-only image backend
    frame.save_png(&path).expect("PNG must be saved");

    // Then: the resulting PNG can be decoded with the expected dimensions
    let decoded = image::open(&path).expect("saved PNG must decode");
    assert_eq!((decoded.width(), decoded.height()), (2, 1));
}

#[test]
#[ignore = "requires a working wgpu adapter; image generation is covered by Wave 5 CI"]
fn capture_produces_a_frame_when_an_adapter_is_available() {
    // Given: a headless workbench on a machine with a working wgpu adapter
    let mut workbench = workbench();
    workbench.run();

    // When: a frame is captured
    let frame = workbench.capture().expect("wgpu adapter must render");

    // Then: the requested dimensions and RGBA byte count are preserved
    assert_eq!((frame.width, frame.height), (640, 360));
    assert_eq!(frame.rgba.len(), 640 * 360 * 4);
}
