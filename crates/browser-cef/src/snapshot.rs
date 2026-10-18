use std::time::Duration;

use egui::ColorImage;

use crate::artifact::ArtifactContext;
use crate::browser::OsrBrowser;
use crate::offline::{ArtifactBundle, INDEX};

/// Desktop-width viewport in DIP, so mocks do not fall into mobile layouts.
pub const SNAPSHOT_SIZE: [i32; 2] = [1024, 768];

/// Renders `html` offline in a throwaway browser and returns its settled frame.
pub fn render_snapshot(html: Vec<u8>, timeout: Duration) -> Result<ColorImage, String> {
    let context = ArtifactContext::create(ArtifactBundle::html(html))?;
    let browser = OsrBrowser::create(context, &ArtifactContext::url(INDEX), SNAPSHOT_SIZE, 1.0)?;
    browser.capture_settled(timeout)
}
