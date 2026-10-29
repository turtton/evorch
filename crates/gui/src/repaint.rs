//! Repaint requests for work that finishes off the UI thread.
//!
//! egui paints a second time after every `request_repaint()` (an "echo"
//! frame), so a source that fires per event paints twice per event while it
//! streams. `request_repaint_after(d)` with a non-zero `d` schedules a single
//! repaint and keeps only the earliest pending deadline, so any number of
//! requests inside one window collapse into one frame.

use std::time::Duration;

/// How long the display may lag behind a background update. Requests inside
/// this window share one frame, which caps streaming at about 20 frames/s.
pub const COALESCE: Duration = Duration::from_millis(50);

/// Wakes the UI for a background update without an immediate frame.
///
/// Use it where nothing the user just did is waiting on the result. Keep
/// `request_repaint()` for updates that answer user input, such as terminal
/// echo, so typing does not lag by [`COALESCE`].
pub fn coalesced(ctx: &egui::Context) {
    ctx.request_repaint_after(COALESCE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn recording_context() -> (egui::Context, Arc<Mutex<Vec<Duration>>>) {
        let ctx = egui::Context::default();
        let delays = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&delays);
        ctx.set_request_repaint_callback(move |info| sink.lock().unwrap().push(info.delay));
        // A new context still owes its first frames; run them so the tests
        // start from idle.
        while pass(&ctx) != Duration::MAX {}
        delays.lock().unwrap().clear();
        (ctx, delays)
    }

    fn pass(ctx: &egui::Context) -> Duration {
        let mut output = ctx.run_ui(egui::RawInput::default(), |_ui| {});
        output.textures_delta.clear();
        output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .expect("root viewport output")
            .repaint_delay
    }

    #[test]
    fn a_burst_of_requests_becomes_one_deferred_repaint() {
        let (ctx, delays) = recording_context();
        for _ in 0..100 {
            coalesced(&ctx);
        }

        let delays = delays.lock().unwrap();
        assert_eq!(delays.len(), 1, "only the earliest deadline is scheduled");
        assert!(!delays[0].is_zero(), "a zero delay is not coalesced");
        assert!(delays[0] <= COALESCE);
    }

    #[test]
    fn no_echo_frame_follows_the_scheduled_repaint() {
        let (ctx, _delays) = recording_context();
        coalesced(&ctx);
        // The frame the request scheduled asks for nothing more.
        assert_eq!(pass(&ctx), Duration::MAX);
    }
}
