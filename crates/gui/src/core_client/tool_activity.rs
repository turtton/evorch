//! The tool calls the core says are running, as the new GUI keeps them for its transcripts.
//!
//! A call is running from its `ToolStarted` (or a snapshot that holds it) to its
//! `ToolFinished`. Once finished it stays here, closed and with the core's duration, until its
//! committed row arrives and takes its place: the row is the record, this is only the gap
//! before it. A call whose committed row is known is never kept (a start that arrives after
//! its row was committed must not bring the card back).
//!
//! Time comes from a [`Clock`] so that tests move it by hand: a running call's elapsed time is
//! the one the core reported plus the time since this client received it.
use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use workbench_protocol::namespaces::execution::{ToolActivityStatus, ToolActivityView};

/// The tools whose time is shown, running and after: the ones that take a while. The call's
/// own time, so a poll of a shell job shows the poll's, not the job's.
pub const TIMED_TOOLS: &[&str] = &["shell", "wait", "run_js", "delegate"];

pub fn is_timed(tool_name: &str) -> bool {
    TIMED_TOOLS.contains(&tool_name)
}

/// Finished calls kept while waiting for their committed rows. A row that never comes (it
/// could not be saved) must not keep one forever.
const MAX_FINISHED: usize = 32;

#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> Instant + Send + Sync>);

impl Default for Clock {
    fn default() -> Self {
        Self(Arc::new(Instant::now))
    }
}

impl Clock {
    #[cfg(test)]
    pub fn from_fn(f: impl Fn() -> Instant + Send + Sync + 'static) -> Self {
        Self(Arc::new(f))
    }

    pub fn now(&self) -> Instant {
        (self.0)()
    }
}

/// How a call stands: running, or finished the way the core said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Running,
    Finished(ToolActivityStatus),
}

#[derive(Clone)]
pub struct LiveTool {
    pub tool: ToolActivityView,
    seq: u64,
    received: Instant,
    end: Option<End>,
}

#[derive(Clone, Copy)]
struct End {
    status: ToolActivityStatus,
    duration_ms: u64,
}

impl LiveTool {
    pub fn outcome(&self) -> Outcome {
        self.end
            .map_or(Outcome::Running, |end| Outcome::Finished(end.status))
    }

    pub fn running(&self) -> bool {
        self.end.is_none()
    }

    /// The call's time: while it runs, what the core reported plus the time since it was
    /// received; after, the core's duration. `None` for a tool whose time is not shown.
    pub fn time(&self, now: Instant) -> Option<Duration> {
        is_timed(&self.tool.tool_name).then(|| match self.end {
            Some(end) => Duration::from_millis(end.duration_ms),
            None => self.elapsed(now),
        })
    }

    fn elapsed(&self, now: Instant) -> Duration {
        Duration::from_millis(self.tool.elapsed_ms) + now.saturating_duration_since(self.received)
    }
}

#[derive(Clone, Default)]
pub struct LiveTools {
    clock: Clock,
    calls: BTreeMap<(String, String), LiveTool>,
    next_seq: u64,
}

impl LiveTools {
    #[cfg(test)]
    pub fn with_clock(clock: Clock) -> Self {
        Self {
            clock,
            ..Self::default()
        }
    }

    pub fn now(&self) -> Instant {
        self.clock.now()
    }

    /// A call the core says is running. One already kept is left as it is: the first word
    /// about a call stands.
    pub fn start(&mut self, tool: ToolActivityView) {
        let key = (tool.run.to_string(), tool.call_id.clone());
        if self.calls.contains_key(&key) {
            return;
        }
        self.next_seq += 1;
        let live = LiveTool {
            tool,
            seq: self.next_seq,
            received: self.clock.now(),
            end: None,
        };
        self.calls.insert(key, live);
    }

    pub fn finish(
        &mut self,
        run: &str,
        call_id: &str,
        status: ToolActivityStatus,
        duration_ms: Option<u64>,
    ) {
        let now = self.clock.now();
        let Some(live) = self.calls.get_mut(&(run.to_owned(), call_id.to_owned())) else {
            return;
        };
        if live.end.is_none() {
            let duration_ms = duration_ms.unwrap_or_else(|| {
                u64::try_from(live.elapsed(now).as_millis()).unwrap_or(u64::MAX)
            });
            live.end = Some(End {
                status,
                duration_ms,
            });
        }
        self.trim();
    }

    /// The committed row of this call is known: it is the record now.
    pub fn committed(&mut self, run: &str, call_id: &str) {
        self.calls.remove(&(run.to_owned(), call_id.to_owned()));
    }

    fn trim(&mut self) {
        let mut finished: Vec<_> = self
            .calls
            .iter()
            .filter(|(_, live)| !live.running())
            .map(|(key, live)| (live.seq, key.clone()))
            .collect();
        let excess = finished.len().saturating_sub(MAX_FINISHED);
        finished.sort();
        for (_, key) in finished.into_iter().take(excess) {
            self.calls.remove(&key);
        }
    }

    /// The calls to draw, oldest first, among those `keep` accepts.
    pub fn shown(&self, keep: impl Fn(&LiveTool) -> bool) -> Vec<&LiveTool> {
        let mut shown: Vec<_> = self.calls.values().filter(|live| keep(live)).collect();
        shown.sort_by_key(|live| live.seq);
        shown
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.calls.is_empty()
    }
}

/// A time as the new GUI writes it: `0.4s`, `12.4s`, `1m 05s`, `1h 02m`. It truncates, so
/// a running call's time never reads ahead of the clock (59.99s is `59.9s`, not `60.0s`).
pub fn format_time(time: Duration) -> String {
    let secs = time.as_secs();
    if secs < 60 {
        format!("{}.{}s", secs, time.subsec_millis() / 100)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(call: &str, name: &str, elapsed_ms: u64) -> ToolActivityView {
        serde_json::from_value(serde_json::json!({
            "thread": "thr", "run": "r", "call_id": call, "tool_name": name,
            "elapsed_ms": elapsed_ms,
        }))
        .unwrap()
    }

    // Given: times from a tenth of a second to hours, and ones just under a boundary
    // Then: each reads in the form the person asked for, truncated
    #[test]
    fn times_are_written_in_the_requested_forms() {
        let ms = Duration::from_millis;
        assert_eq!(format_time(ms(0)), "0.0s");
        assert_eq!(format_time(ms(400)), "0.4s");
        assert_eq!(format_time(ms(12_400)), "12.4s");
        assert_eq!(format_time(ms(59_999)), "59.9s");
        assert_eq!(format_time(ms(60_000)), "1m 00s");
        assert_eq!(format_time(ms(65_000)), "1m 05s");
        assert_eq!(format_time(ms(3_599_999)), "59m 59s");
        assert_eq!(format_time(ms(3_720_000)), "1h 02m");
        assert_eq!(format_time(ms(90_000_000)), "25h 00m");
    }

    // Given: a running call and a clock moved by hand
    // Then: its time is the core's elapsed time plus what the clock moved since receipt, and
    //       a finished call keeps the core's duration whatever the clock does
    #[test]
    fn a_running_calls_time_follows_the_clock_and_a_finished_calls_does_not() {
        let base = Instant::now();
        let offset = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let clock = Clock::from_fn({
            let offset = offset.clone();
            move || base + Duration::from_millis(offset.load(std::sync::atomic::Ordering::SeqCst))
        });
        let mut live = LiveTools::with_clock(clock);
        live.start(tool("c", "run_js", 1_000));
        let move_clock = |ms| offset.store(ms, std::sync::atomic::Ordering::SeqCst);
        let time = |live: &LiveTools| live.shown(|_| true)[0].time(live.now());
        assert_eq!(time(&live), Some(Duration::from_millis(1_000)));
        move_clock(2_500);
        assert_eq!(time(&live), Some(Duration::from_millis(3_500)));
        live.finish("r", "c", ToolActivityStatus::Succeeded, Some(3_400));
        move_clock(9_000);
        assert_eq!(time(&live), Some(Duration::from_millis(3_400)));
    }

    // Given: a tool outside the list
    // Then: it has no time, running or finished
    #[test]
    fn a_tool_outside_the_list_has_no_time() {
        let mut live = LiveTools::default();
        live.start(tool("c", "read", 5_000));
        let now = live.now();
        assert_eq!(live.shown(|_| true)[0].time(now), None);
        live.finish("r", "c", ToolActivityStatus::Succeeded, Some(5_000));
        assert_eq!(live.shown(|_| true)[0].time(now), None);
        assert!(is_timed("shell") && is_timed("wait") && is_timed("delegate"));
    }

    // Given: a finished call whose row has not come, and more of them than are kept
    // When: the row of one comes
    // Then: it is gone, and the oldest finished ones are dropped past the limit
    #[test]
    fn finished_calls_wait_for_their_rows_within_a_limit() {
        let mut live = LiveTools::default();
        for index in 0..(MAX_FINISHED + 3) {
            let call = format!("c{index}");
            live.start(tool(&call, "shell", 0));
            live.finish("r", &call, ToolActivityStatus::Failed, None);
        }
        let calls = |live: &LiveTools| {
            live.shown(|_| true)
                .iter()
                .map(|l| l.tool.call_id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(calls(&live).len(), MAX_FINISHED);
        assert_eq!(calls(&live)[0], "c3");
        live.committed("r", "c3");
        assert_eq!(calls(&live)[0], "c4");
        live.start(tool("busy", "shell", 0));
        assert!(live.shown(|l| l.running()).len() == 1);
    }
}
