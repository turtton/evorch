//! Opt-in transcript work counters, for the headless `transcript_perf`
//! benchmark. They count work (entries enumerated, rows drawn or skipped, ...)
//! and, only while enabled, time its phases. Timing is report-only: nothing
//! may use it as a pass criterion.
//!
//! Disabled, a pass costs one relaxed atomic load: counters are plain integers
//! in a per-pass struct that is published once, at the end of the pass, into a
//! thread-local total. Nothing is written to `egui::Context` data per row.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Work done by transcript passes since the last [`take_transcript_perf`].
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TranscriptPerf {
    /// `run_detail_body` passes (a frame can run more than one).
    pub passes: u64,
    /// Entries visited by full scans of the entry list (height-cache pruning,
    /// shell-job map, run_js nesting, display projection, row loop).
    pub entries_enumerated: u64,
    /// Rows the pass walked (one `VirtualList::row` call each).
    pub rows_listed: u64,
    /// Rows whose content was drawn, which is also when they are measured.
    pub rows_drawn: u64,
    /// Rows replaced by a spacer of their known height.
    pub rows_skipped: u64,
    /// Drawn rows that lay outside the viewport widened by one screenful each
    /// way: they were drawn only to be measured.
    pub rows_drawn_far: u64,
    /// Drawn rows whose height was not yet known to be stable.
    pub rows_unsettled: u64,
    /// Row revisions computed.
    pub revisions: u64,
    /// Display projections built (run_js nesting, shell-job map, grouping).
    pub projection_rebuilds: u64,
    /// Times a width change dropped the whole height cache.
    pub height_cache_clears: u64,
    /// Rows hidden inside closed tool groups (neither drawn nor measured).
    pub rows_hidden_by_groups: u64,
    /// Closed or open group header rows in the projection.
    pub group_headers: u64,
    /// Nanoseconds before the row loop (cache load, hashes, projection).
    pub prep_ns: u64,
    /// Nanoseconds computing row revisions.
    pub revision_ns: u64,
    /// Nanoseconds in row contents that were drawn.
    pub draw_ns: u64,
    /// Nanoseconds of the whole row loop (includes revisions and draws).
    pub loop_ns: u64,
}

impl TranscriptPerf {
    fn add(&mut self, other: &Self) {
        self.passes += other.passes;
        self.entries_enumerated += other.entries_enumerated;
        self.rows_listed += other.rows_listed;
        self.rows_drawn += other.rows_drawn;
        self.rows_skipped += other.rows_skipped;
        self.rows_drawn_far += other.rows_drawn_far;
        self.rows_unsettled += other.rows_unsettled;
        self.revisions += other.revisions;
        self.projection_rebuilds += other.projection_rebuilds;
        self.height_cache_clears += other.height_cache_clears;
        self.rows_hidden_by_groups += other.rows_hidden_by_groups;
        self.group_headers += other.group_headers;
        self.prep_ns += other.prep_ns;
        self.revision_ns += other.revision_ns;
        self.draw_ns += other.draw_ns;
        self.loop_ns += other.loop_ns;
    }
}

thread_local! {
    static TOTAL: RefCell<TranscriptPerf> = RefCell::new(TranscriptPerf::default());
}

/// Turns the counters on or off for every thread. Off by default.
#[doc(hidden)]
pub fn set_transcript_perf_enabled(enabled: bool) {
    ENABLED.store(enabled, Ordering::Relaxed);
}

/// Returns what this thread's passes counted since the last call.
#[doc(hidden)]
pub fn take_transcript_perf() -> TranscriptPerf {
    TOTAL.with(|total| std::mem::take(&mut *total.borrow_mut()))
}

pub(super) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub(super) fn publish(pass: &TranscriptPerf) {
    TOTAL.with(|total| total.borrow_mut().add(pass));
}

/// A clock that only runs while the counters are enabled.
#[derive(Clone, Copy)]
pub(super) struct Clock(Option<Instant>);

impl Clock {
    pub(super) fn start(on: bool) -> Self {
        Self(on.then(Instant::now))
    }

    pub(super) fn elapsed_ns(self) -> u64 {
        self.0.map_or(0, |start| {
            u64::try_from(start.elapsed().as_nanos()).unwrap_or(u64::MAX)
        })
    }
}
