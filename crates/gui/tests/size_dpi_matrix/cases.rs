use gui::headless::HeadlessWorkbench;

use super::{geometry::Geometry, states::State};

pub const SIZES: [[u16; 2]; 4] = [[1280, 720], [1024, 768], [1920, 720], [960, 600]];
const DPIS: [f32; 3] = [1.0, 1.5, 2.0];

pub fn verify(state: State) {
    let mut failures = Vec::new();
    for [width, height] in SIZES {
        for dpi in DPIS {
            let root = tempfile::tempdir().expect("isolated fixture directory");
            let mut workbench = HeadlessWorkbench::with_pixels_per_point(
                state.build(root.path()),
                [f32::from(width), f32::from(height)],
                dpi,
            );
            workbench.run();
            let mut geometry = Geometry {
                workbench: &mut workbench,
                context: format!("size={width}x{height} dpi={dpi:.1} state={}", state.name()),
                failures: &mut failures,
            };
            if geometry.workbench.pixels_per_point() != dpi {
                geometry.failure(
                    "DPI",
                    format!("rect=n/a actual={}", geometry.workbench.pixels_per_point()),
                );
            }
            geometry.composer();
            match state {
                State::Empty => {
                    for label in ["No project selected", "Go to Projects", "Projects"] {
                        geometry.reachable(label);
                    }
                }
                State::Demo | State::ErrorThread => {
                    for label in [
                        "Projects",
                        "Threads",
                        "evorch",
                        "Refine GUI design system",
                        "Provider composition root",
                        "Fix flaky offscreen test",
                        "New thread",
                    ] {
                        geometry.sidebar(label);
                    }
                    let settings = if geometry.workbench.has_label("Settings") {
                        "Settings"
                    } else {
                        "Open Settings"
                    };
                    geometry.reachable(settings);
                    match state {
                        State::ErrorThread => {
                            geometry.reachable("Error");
                        }
                        State::Demo => {}
                        State::Empty | State::EditProfile => unreachable!(),
                    }
                    geometry.sidebar("intent-cli");
                    if geometry.workbench.count_labels("intent-cli") == 1 {
                        geometry.workbench.click_label("intent-cli");
                        geometry.workbench.run();
                    }
                    geometry.sidebar("Queue seed CLI");
                }
                State::EditProfile => geometry.modal(),
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}
