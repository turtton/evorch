#![cfg(feature = "browser")]

use gui::browser::{BrowserAction, BrowserPane, FakeFrameSource, FrameSource};

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
