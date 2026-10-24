//! Opt-in frame diagnostics. With `RUST_LOG=info,gui::frame_trace=debug`, the GUI
//! logs a summary every few seconds: how often it painted, how long its passes took,
//! and the call sites whose immediate repaint requests made it paint again. Only call
//! sites and timings are recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(5);
const TOP_CAUSES: usize = 8;

/// Uses the log filter instead of an `EVORCH_*` variable, which the config
/// loader would read as a config field.
pub(super) fn enabled() -> bool {
    tracing::enabled!(target: "gui::frame_trace", tracing::Level::DEBUG)
}

/// Time spent in the parts of one pass that `WorkbenchState::ui` measures.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct PassSections {
    pub drain: Duration,
    pub fold: Duration,
    pub sink: Duration,
    pub ownership: Duration,
    pub render: Duration,
    pub draft: Duration,
}

impl PassSections {
    fn add(&mut self, other: Self) {
        self.drain += other.drain;
        self.fold += other.fold;
        self.sink += other.sink;
        self.ownership += other.ownership;
        self.render += other.render;
        self.draft += other.draft;
    }

    fn measured(&self) -> Duration {
        self.drain + self.fold + self.sink + self.ownership + self.render + self.draft
    }
}

/// One `WorkbenchState::ui` call as egui reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct PassRecord {
    pub elapsed: Duration,
    pub sections: PassSections,
    /// egui runs the pass again in the same frame after `request_discard`.
    pub discarded_rerun: bool,
    /// The previous pass asked for an immediate repaint, which caused this frame.
    pub immediate: bool,
    /// `file:line reason` of each repaint request made during the previous pass.
    pub causes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameSummary {
    pub seconds: f64,
    pub frames: u32,
    pub discarded_reruns: u32,
    pub immediate_frames: u32,
    pub mean_pass: Duration,
    pub longest_pass: Duration,
    /// Mean per pass.
    pub sections: PassSections,
    pub other: Duration,
    /// Call sites of immediate repaints, most frequent first, counted once per frame.
    pub causes: Vec<(String, u32)>,
}

impl FrameSummary {
    pub fn causes_line(&self) -> String {
        self.causes
            .iter()
            .map(|(cause, count)| format!("{count}x {cause}"))
            .collect::<Vec<_>>()
            .join("; ")
    }

    fn log(&self) {
        let ms = |value: Duration| value.as_secs_f64() * 1000.0;
        tracing::debug!(
            target: "gui::frame_trace",
            seconds = self.seconds,
            frames = self.frames,
            fps = f64::from(self.frames) / self.seconds,
            immediate_frames = self.immediate_frames,
            discarded_reruns = self.discarded_reruns,
            mean_pass_ms = ms(self.mean_pass),
            longest_pass_ms = ms(self.longest_pass),
            drain_ms = ms(self.sections.drain),
            fold_ms = ms(self.sections.fold),
            sink_ms = ms(self.sections.sink),
            ownership_ms = ms(self.sections.ownership),
            render_ms = ms(self.sections.render),
            draft_ms = ms(self.sections.draft),
            other_ms = ms(self.other),
            causes = %self.causes_line(),
            "GUI frame trace"
        );
    }
}

#[derive(Debug)]
pub struct FrameTrace {
    started: Instant,
    frames: u32,
    passes: u32,
    discarded_reruns: u32,
    immediate_frames: u32,
    elapsed: Duration,
    longest: Duration,
    sections: PassSections,
    causes: BTreeMap<String, u32>,
}

impl FrameTrace {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            frames: 0,
            passes: 0,
            discarded_reruns: 0,
            immediate_frames: 0,
            elapsed: Duration::ZERO,
            longest: Duration::ZERO,
            sections: PassSections::default(),
            causes: BTreeMap::new(),
        }
    }

    /// Adds one pass and returns the summary once a window has elapsed.
    pub fn record(&mut self, now: Instant, pass: PassRecord) -> Option<FrameSummary> {
        self.passes += 1;
        self.elapsed += pass.elapsed;
        self.longest = self.longest.max(pass.elapsed);
        self.sections.add(pass.sections);
        if pass.discarded_rerun {
            self.discarded_reruns += 1;
        } else {
            self.frames += 1;
            if pass.immediate {
                self.immediate_frames += 1;
                for cause in pass.causes.into_iter().collect::<BTreeSet<_>>() {
                    *self.causes.entry(cause).or_default() += 1;
                }
            }
        }
        let window = now.saturating_duration_since(self.started);
        if window < WINDOW {
            return None;
        }
        let mut causes: Vec<_> = std::mem::take(&mut self.causes).into_iter().collect();
        causes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        causes.truncate(TOP_CAUSES);
        let passes = self.passes.max(1);
        let mean = |total: Duration| total / passes;
        let sections = PassSections {
            drain: mean(self.sections.drain),
            fold: mean(self.sections.fold),
            sink: mean(self.sections.sink),
            ownership: mean(self.sections.ownership),
            render: mean(self.sections.render),
            draft: mean(self.sections.draft),
        };
        let summary = FrameSummary {
            seconds: window.as_secs_f64(),
            frames: self.frames,
            discarded_reruns: self.discarded_reruns,
            immediate_frames: self.immediate_frames,
            mean_pass: mean(self.elapsed),
            longest_pass: self.longest,
            sections,
            other: mean(self.elapsed.saturating_sub(self.sections.measured())),
            causes,
        };
        *self = Self::new(now);
        Some(summary)
    }

    /// Records the pass `ctx` is running and logs a summary when one is due.
    pub(super) fn record_pass(
        &mut self,
        ctx: &egui::Context,
        elapsed: Duration,
        sections: PassSections,
    ) {
        let immediate = ctx.requested_repaint_last_pass();
        let causes = if immediate {
            ctx.repaint_causes()
                .iter()
                .map(ToString::to_string)
                .collect()
        } else {
            Vec::new()
        };
        let pass = PassRecord {
            elapsed,
            sections,
            discarded_rerun: ctx.current_pass_index() > 0,
            immediate,
            causes,
        };
        if let Some(summary) = self.record(Instant::now(), pass) {
            summary.log();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(ms: u64, immediate: bool, causes: &[&str]) -> PassRecord {
        PassRecord {
            elapsed: Duration::from_millis(ms),
            sections: PassSections {
                render: Duration::from_millis(ms / 2),
                ..PassSections::default()
            },
            discarded_rerun: false,
            immediate,
            causes: causes.iter().map(|cause| (*cause).to_owned()).collect(),
        }
    }

    #[test]
    fn summary_counts_immediate_causes_once_per_frame_after_the_window() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(
            trace
                .record(
                    start,
                    pass(10, true, &["a.rs:1 spin", "a.rs:1 spin", "b.rs:2"])
                )
                .is_none()
        );
        assert!(trace.record(start, pass(20, false, &["c.rs:3"])).is_none());
        let mut rerun = pass(30, true, &["d.rs:4"]);
        rerun.discarded_rerun = true;
        assert!(trace.record(start, rerun).is_none());
        let summary = trace
            .record(start + WINDOW, pass(40, true, &["a.rs:1 spin"]))
            .expect("window elapsed");
        assert_eq!(summary.frames, 3);
        assert_eq!(summary.discarded_reruns, 1);
        assert_eq!(summary.immediate_frames, 2);
        assert_eq!(summary.mean_pass, Duration::from_millis(25));
        assert_eq!(summary.longest_pass, Duration::from_millis(40));
        assert_eq!(
            summary.sections.render,
            Duration::from_millis(12) + Duration::from_micros(500)
        );
        assert_eq!(
            summary.other,
            Duration::from_millis(12) + Duration::from_micros(500)
        );
        assert_eq!(
            summary.causes,
            vec![("a.rs:1 spin".to_owned(), 2), ("b.rs:2".to_owned(), 1)]
        );
        assert_eq!(summary.causes_line(), "2x a.rs:1 spin; 1x b.rs:2");
    }

    #[test]
    fn a_new_window_starts_empty_after_each_summary() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(
            trace
                .record(start + WINDOW, pass(10, true, &["a.rs:1"]))
                .is_some()
        );
        let next = trace
            .record(start + WINDOW * 2, pass(10, false, &[]))
            .expect("second window");
        assert_eq!(next.frames, 1);
        assert_eq!(next.immediate_frames, 0);
        assert!(next.causes.is_empty());
    }

    #[test]
    fn records_the_live_pass_from_an_egui_context() {
        let ctx = egui::Context::default();
        let mut trace = FrameTrace::new(Instant::now());
        let mut first = ctx.run_ui(egui::RawInput::default(), |ui| {
            ui.ctx().request_repaint();
        });
        first.textures_delta.clear();
        let mut second = ctx.run_ui(egui::RawInput::default(), |ui| {
            trace.record_pass(ui.ctx(), Duration::from_millis(1), PassSections::default());
        });
        second.textures_delta.clear();
        assert_eq!(trace.frames, 1);
        assert_eq!(trace.immediate_frames, 1);
        assert!(
            trace
                .causes
                .keys()
                .any(|cause| cause.contains("frame_trace.rs")),
            "{:?}",
            trace.causes
        );
    }
}
