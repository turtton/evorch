//! Opt-in frame diagnostics. With `RUST_LOG=info,gui::frame_trace=debug`, the GUI
//! logs a summary every few seconds: how often it painted, how long its passes took,
//! and the call sites whose immediate repaint requests made it paint again. Only call
//! sites and timings are recorded.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(5);
const TOP_CAUSES: usize = 8;
const TOP_PANES: usize = 5;
const PANE_MEASUREMENTS_ID: &str = "gui::frame_trace::pane_measurements";

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct PaneMeasurement {
    total: Duration,
    samples: u32,
}

type PaneMeasurements = BTreeMap<String, PaneMeasurement>;

/// Adds a tab-viewer duration to the measurements for the current egui pass.
pub(super) fn record_pane_time(ctx: &egui::Context, pane: String, elapsed: Duration) {
    if !enabled() {
        return;
    }
    ctx.data_mut(|data| {
        let id = egui::Id::new(PANE_MEASUREMENTS_ID);
        let measurement = data
            .get_temp_mut_or_default::<PaneMeasurements>(id)
            .entry(pane)
            .or_default();
        measurement.total += elapsed;
        measurement.samples += 1;
    });
}

fn take_pane_measurements(ctx: &egui::Context) -> PaneMeasurements {
    ctx.data_mut(|data| data.remove_temp(egui::Id::new(PANE_MEASUREMENTS_ID)))
        .unwrap_or_default()
}

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
    /// Per-pane render totals and sample counts collected during this pass.
    pub panes: BTreeMap<String, (Duration, u32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FrameSummary {
    pub seconds: f64,
    pub frames: u32,
    pub discarded_reruns: u32,
    pub immediate_frames: u32,
    pub echo_frames: u32,
    pub mean_pass: Duration,
    pub longest_pass: Duration,
    /// Mean per pass.
    pub sections: PassSections,
    pub other: Duration,
    /// Call sites of immediate repaints, most frequent first, counted once per frame.
    pub causes: Vec<(String, u32)>,
    /// Call sites also observed on non-immediate frames in this window.
    pub delayed_causes: BTreeSet<String>,
    /// Mean render duration per pane, slowest first.
    pub panes: Vec<(String, Duration)>,
}

impl FrameSummary {
    pub fn causes_line(&self) -> String {
        self.causes
            .iter()
            .map(|(cause, count)| {
                let marker = if self.delayed_causes.contains(cause) {
                    ""
                } else {
                    "*"
                };
                format!("{count}x {cause}{marker}")
            })
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub fn panes_line(&self) -> String {
        self.panes
            .iter()
            .map(|(pane, average)| format!("{pane}={:.2}ms", average.as_secs_f64() * 1000.0))
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
            echo_frames = self.echo_frames,
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
            panes = %self.panes_line(),
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
    immediate_frame_causes: Vec<BTreeSet<String>>,
    elapsed: Duration,
    longest: Duration,
    sections: PassSections,
    causes: BTreeMap<String, u32>,
    delayed_causes: BTreeSet<String>,
    pane_measurements: PaneMeasurements,
}

impl FrameTrace {
    pub fn new(now: Instant) -> Self {
        Self {
            started: now,
            frames: 0,
            passes: 0,
            discarded_reruns: 0,
            immediate_frames: 0,
            immediate_frame_causes: Vec::new(),
            elapsed: Duration::ZERO,
            longest: Duration::ZERO,
            sections: PassSections::default(),
            causes: BTreeMap::new(),
            delayed_causes: BTreeSet::new(),
            pane_measurements: BTreeMap::new(),
        }
    }

    /// Adds one pass and returns the summary once a window has elapsed.
    pub fn record(&mut self, now: Instant, pass: PassRecord) -> Option<FrameSummary> {
        self.passes += 1;
        self.elapsed += pass.elapsed;
        self.longest = self.longest.max(pass.elapsed);
        self.sections.add(pass.sections);
        for (pane, (duration, samples)) in pass.panes {
            let total = self.pane_measurements.entry(pane).or_default();
            total.total += duration;
            total.samples += samples;
        }
        if pass.discarded_rerun {
            self.discarded_reruns += 1;
        } else {
            self.frames += 1;
            let causes: BTreeSet<_> = pass.causes.into_iter().collect();
            if pass.immediate {
                self.immediate_frames += 1;
                self.immediate_frame_causes.push(causes.clone());
                for cause in causes {
                    *self.causes.entry(cause).or_default() += 1;
                }
            } else {
                self.delayed_causes.extend(causes);
            }
        }
        let window = now.saturating_duration_since(self.started);
        if window < WINDOW {
            return None;
        }
        let mut causes: Vec<_> = std::mem::take(&mut self.causes).into_iter().collect();
        causes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
        causes.truncate(TOP_CAUSES);
        let echo_frames = echo_frame_count(&self.immediate_frame_causes, &self.delayed_causes);
        let panes = summarize_panes(&self.pane_measurements);
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
            echo_frames,
            mean_pass: mean(self.elapsed),
            longest_pass: self.longest,
            sections,
            other: mean(self.elapsed.saturating_sub(self.sections.measured())),
            causes,
            delayed_causes: std::mem::take(&mut self.delayed_causes),
            panes,
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
        let causes = ctx
            .repaint_causes()
            .iter()
            .map(ToString::to_string)
            .collect();
        let pass = PassRecord {
            elapsed,
            sections,
            discarded_rerun: ctx.current_pass_index() > 0,
            immediate,
            causes,
            panes: take_pane_measurements(ctx)
                .into_iter()
                .map(|(pane, measurement)| (pane, (measurement.total, measurement.samples)))
                .collect(),
        };
        if let Some(summary) = self.record(Instant::now(), pass) {
            summary.log();
        }
    }
}

fn echo_frame_count(
    immediate_frame_causes: &[BTreeSet<String>],
    delayed_causes: &BTreeSet<String>,
) -> u32 {
    immediate_frame_causes
        .iter()
        .filter(|causes| causes.iter().all(|cause| delayed_causes.contains(cause)))
        .count() as u32
}

fn summarize_panes(measurements: &PaneMeasurements) -> Vec<(String, Duration)> {
    let mut panes: Vec<_> = measurements
        .iter()
        .filter(|(_, measurement)| measurement.samples > 0)
        .map(|(pane, measurement)| (pane.clone(), measurement.total / measurement.samples))
        .collect();
    panes.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.0.cmp(&right.0)));
    panes.truncate(TOP_PANES);
    panes
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
            panes: BTreeMap::new(),
        }
    }

    fn pass_with_panes(
        ms: u64,
        immediate: bool,
        causes: &[&str],
        panes: &[(&str, Duration, u32)],
    ) -> PassRecord {
        let mut pass = pass(ms, immediate, causes);
        pass.panes = panes
            .iter()
            .map(|(pane, duration, samples)| ((*pane).to_owned(), (*duration, *samples)))
            .collect();
        pass
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
        assert_eq!(summary.causes_line(), "2x a.rs:1 spin*; 1x b.rs:2*");
    }

    #[test]
    fn delayed_echo_and_pane_measurements_reset_after_each_window() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(
            trace
                .record(
                    start,
                    pass_with_panes(
                        10,
                        true,
                        &["echo.rs:1"],
                        &[("Same title (panel-1)", Duration::from_millis(9), 3)],
                    ),
                )
                .is_none()
        );
        let first = trace
            .record(
                start + WINDOW,
                pass_with_panes(
                    10,
                    false,
                    &["echo.rs:1"],
                    &[("Same title (panel-1)", Duration::from_millis(3), 1)],
                ),
            )
            .expect("first window");
        assert_eq!(
            first.delayed_causes,
            BTreeSet::from(["echo.rs:1".to_owned()])
        );
        assert_eq!(first.echo_frames, 1);
        assert_eq!(
            first.panes,
            vec![("Same title (panel-1)".to_owned(), Duration::from_millis(3))]
        );
        let next = trace
            .record(
                start + WINDOW * 2,
                pass_with_panes(
                    10,
                    false,
                    &["later.rs:2"],
                    &[("Fresh title (panel-2)", Duration::from_millis(8), 2)],
                ),
            )
            .expect("second window");
        assert_eq!(next.frames, 1);
        assert_eq!(next.immediate_frames, 0);
        assert_eq!(next.echo_frames, 0);
        assert!(next.causes.is_empty());
        assert_eq!(
            next.delayed_causes,
            BTreeSet::from(["later.rs:2".to_owned()])
        );
        assert_eq!(
            next.panes,
            vec![("Fresh title (panel-2)".to_owned(), Duration::from_millis(4))]
        );
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

    #[test]
    fn marks_immediate_only_causes_and_counts_echo_frames_for_the_window() {
        let delayed = BTreeSet::from(["delayed.rs:1".to_owned()]);
        let immediate = vec![
            BTreeSet::from(["delayed.rs:1".to_owned()]),
            BTreeSet::from(["immediate.rs:2".to_owned()]),
            BTreeSet::new(),
        ];
        assert_eq!(echo_frame_count(&immediate, &delayed), 2);

        let summary = FrameSummary {
            seconds: 5.0,
            frames: 3,
            discarded_reruns: 0,
            immediate_frames: 3,
            echo_frames: echo_frame_count(&immediate, &delayed),
            mean_pass: Duration::ZERO,
            longest_pass: Duration::ZERO,
            sections: PassSections::default(),
            other: Duration::ZERO,
            causes: vec![
                ("delayed.rs:1".to_owned(), 1),
                ("immediate.rs:2".to_owned(), 1),
            ],
            delayed_causes: delayed,
            panes: Vec::new(),
        };
        assert_eq!(summary.causes_line(), "1x delayed.rs:1; 1x immediate.rs:2*");
    }

    #[test]
    fn pane_summary_averages_ranks_and_formats_the_top_five() {
        let measurements: PaneMeasurements = (0_u32..7)
            .map(|index| {
                (
                    format!("pane-{index}"),
                    PaneMeasurement {
                        total: Duration::from_millis(u64::from(index + 1) * 2),
                        samples: 2,
                    },
                )
            })
            .collect();
        let panes = summarize_panes(&measurements);
        assert_eq!(
            panes,
            (2_u32..=6)
                .rev()
                .map(|index| {
                    (
                        format!("pane-{index}"),
                        Duration::from_millis(u64::from(index + 1)),
                    )
                })
                .collect::<Vec<_>>()
        );
        let summary = FrameSummary {
            seconds: 5.0,
            frames: 1,
            discarded_reruns: 0,
            immediate_frames: 0,
            echo_frames: 0,
            mean_pass: Duration::ZERO,
            longest_pass: Duration::ZERO,
            sections: PassSections::default(),
            other: Duration::ZERO,
            causes: Vec::new(),
            delayed_causes: BTreeSet::new(),
            panes,
        };
        assert_eq!(
            summary.panes_line(),
            "pane-6=7.00ms; pane-5=6.00ms; pane-4=5.00ms; pane-3=4.00ms; pane-2=3.00ms"
        );
    }

    #[test]
    fn trace_counts_echo_frames_when_their_causes_are_also_seen_as_delayed() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(
            trace
                .record(start, pass(10, true, &["same.rs:1"]))
                .is_none()
        );
        assert!(
            trace
                .record(
                    start + Duration::from_millis(1),
                    pass(10, false, &["same.rs:1"])
                )
                .is_none()
        );
        let summary = trace
            .record(start + WINDOW, pass(10, false, &[]))
            .expect("window elapsed");
        assert_eq!(summary.echo_frames, 1);
        assert_eq!(summary.causes_line(), "1x same.rs:1");
    }

    #[test]
    fn an_immediate_frame_with_no_causes_is_counted_as_an_echo() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(trace.record(start, pass(10, true, &[])).is_none());
        let summary = trace
            .record(start + WINDOW, pass(10, false, &[]))
            .expect("window elapsed");
        assert_eq!(summary.immediate_frames, 1);
        assert_eq!(summary.echo_frames, 1);
    }

    #[test]
    fn pane_samples_are_weighted_and_identical_titles_keep_distinct_panel_ids() {
        let start = Instant::now();
        let mut trace = FrameTrace::new(start);
        assert!(
            trace
                .record(
                    start,
                    pass_with_panes(
                        10,
                        false,
                        &[],
                        &[
                            ("Shared title (panel-1)", Duration::from_millis(9), 3),
                            ("Shared title (panel-2)", Duration::from_millis(10), 1),
                        ],
                    ),
                )
                .is_none()
        );
        let summary = trace
            .record(
                start + WINDOW,
                pass_with_panes(
                    10,
                    false,
                    &[],
                    &[("Shared title (panel-1)", Duration::from_millis(3), 1)],
                ),
            )
            .expect("window elapsed");
        assert_eq!(
            summary.panes,
            vec![
                (
                    "Shared title (panel-2)".to_owned(),
                    Duration::from_millis(10)
                ),
                (
                    "Shared title (panel-1)".to_owned(),
                    Duration::from_millis(3)
                ),
            ]
        );
    }

    #[test]
    fn taking_pane_measurements_removes_them_from_the_egui_context() {
        let ctx = egui::Context::default();
        ctx.data_mut(|data| {
            data.get_temp_mut_or_default::<PaneMeasurements>(egui::Id::new(PANE_MEASUREMENTS_ID))
                .insert(
                    "Pane (panel-1)".to_owned(),
                    PaneMeasurement {
                        total: Duration::from_millis(12),
                        samples: 3,
                    },
                );
        });
        assert_eq!(
            take_pane_measurements(&ctx).get("Pane (panel-1)"),
            Some(&PaneMeasurement {
                total: Duration::from_millis(12),
                samples: 3,
            })
        );
        assert!(take_pane_measurements(&ctx).is_empty());
    }
}
