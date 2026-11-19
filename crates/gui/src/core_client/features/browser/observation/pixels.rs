//! Pixels and text of an observed page, prepared for the screen.
use egui::ColorImage;
use workbench_client::renderer::observation::AgentFrame;

/// An RGBA8 frame as an image. The core promises straight (unpremultiplied)
/// RGBA in `width * 4` byte rows; anything else is refused, not repaired.
pub(super) fn rgba_to_image(frame: &AgentFrame) -> Result<ColorImage, String> {
    let start = &frame.start;
    start
        .validate()
        .map_err(|_| "image size is outside the supported range".to_owned())?;
    if frame.rgba.len() as u64 != start.total_bytes {
        return Err("frame is shorter than its size".to_owned());
    }
    Ok(ColorImage::from_rgba_unmultiplied(
        [start.width as usize, start.height as usize],
        &frame.rgba,
    ))
}

/// A page's own text as one line: control characters (newlines included) become
/// spaces so a title or URL cannot reshape the layout. The byte bound of the
/// protocol is a resource limit, not a redaction; nothing is hidden here.
pub(super) fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}
