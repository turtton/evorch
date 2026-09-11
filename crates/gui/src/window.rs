//! Native window policy for the dense sidebar and conversation layout.

/// Layout floor in logical points, keeping the sidebar and conversation usable.
pub const MIN_INNER_SIZE: [f32; 2] = [960.0, 600.0];

/// Comfortable initial workbench size in logical points.
pub const DEFAULT_INNER_SIZE: [f32; 2] = [1280.0, 720.0];

/// Build window options with an instance-specific title and the layout floor.
pub fn native_options(title: &str) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(title)
            .with_inner_size(DEFAULT_INNER_SIZE)
            .with_min_inner_size(MIN_INNER_SIZE),
        ..Default::default()
    }
}
