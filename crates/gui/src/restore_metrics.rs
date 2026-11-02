//! Opt-in startup measurements (`RUST_LOG=info,gui::restore=info`).
//! Memory is process-wide, best-effort, and absent on non-Linux platforms.

use std::time::Instant;

pub struct RestoreTimer {
    stage: &'static str,
    started: Option<Instant>,
}

impl RestoreTimer {
    pub fn new(stage: &'static str) -> Self {
        Self {
            stage,
            started: tracing::enabled!(target: "gui::restore", tracing::Level::INFO)
                .then(Instant::now),
        }
    }

    pub fn finish(self) {
        if let Some(started) = self.started {
            let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
            let (vm_rss_kib, vm_hwm_kib) = process_memory();
            tracing::info!(
                target: "gui::restore",
                stage = self.stage,
                elapsed_ms,
                vm_rss_kib,
                vm_hwm_kib,
                "restore stage complete"
            );
        }
    }
}

pub(crate) fn process_memory() -> (Option<u64>, Option<u64>) {
    #[cfg(target_os = "linux")]
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        return memory_from_status(&status);
    }
    (None, None)
}

#[cfg(any(target_os = "linux", test))]
fn memory_from_status(status: &str) -> (Option<u64>, Option<u64>) {
    let field = |name| {
        status.lines().find_map(|line| {
            let value = line.strip_prefix(name)?;
            let mut words = value.split_whitespace();
            let number = words.next()?.parse().ok()?;
            (words.next()? == "kB").then_some(number)
        })
    };
    (field("VmRSS:"), field("VmHWM:"))
}

/// What the first pass of the history restore read, as the numbers behind
/// "how much of the history did it open".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstPassReport {
    pub rows_scanned: u64,
    /// Events decoded only to find a waiting state; the pass reads it from the
    /// task decode, so any value above 0 is a regression.
    pub waiting_event_decodes: u64,
    /// Lifecycle events decoded in full for the delegated-task restore.
    pub task_event_decodes: u64,
    /// Stored-response slices expanded for their metadata (text is never kept).
    pub message_part_rows: u64,
    pub message_part_stored_bytes: u64,
    pub message_part_expanded_bytes: u64,
    /// Session markers, unclassifiable rows and thread probes.
    pub other_decodes: u64,
    pub isolated_events: u64,
    /// `(events decoded, payload bytes read)` when the `subkind` column was not
    /// backfilled and the pass had to decode the whole history.
    pub fallback: Option<(u64, u64)>,
}

impl FirstPassReport {
    /// One info line for the measurements, and a single warning when the
    /// slow path was taken.
    pub fn log(&self) {
        tracing::info!(
            target: "gui::restore",
            rows_scanned = self.rows_scanned,
            waiting_event_decodes = self.waiting_event_decodes,
            task_event_decodes = self.task_event_decodes,
            message_part_rows = self.message_part_rows,
            message_part_stored_bytes = self.message_part_stored_bytes,
            message_part_expanded_bytes = self.message_part_expanded_bytes,
            other_decodes = self.other_decodes,
            isolated_events = self.isolated_events,
            "history first pass"
        );
        if let Some((events, bytes)) = self.fallback {
            tracing::warn!(
                first_pass_fallback_events_decoded = events,
                first_pass_fallback_payload_bytes = bytes,
                "history restore decoded every event: the event subkind column is not backfilled yet"
            );
        }
    }
}

/// Reports the stored events the restore skipped because they could not be
/// read, once per restore: the same event fails in both passes, so they are
/// keyed by event ID. `(run, reason)` is what the `run_id` column and the
/// decoder said.
pub fn report_unreadable_events(
    events: &std::collections::BTreeMap<i64, (Option<String>, String)>,
) {
    if events.is_empty() {
        return;
    }
    const SHOWN: usize = 10;
    let sample: Vec<String> = events
        .iter()
        .take(SHOWN)
        .map(|(id, (run, reason))| format!("#{id} run={} {reason}", run.as_deref().unwrap_or("-")))
        .collect();
    tracing::warn!(
        count = events.len(),
        shown = sample.len(),
        events = ?sample,
        "history restore skipped stored events that could not be read"
    );
}

#[cfg(test)]
mod tests {
    use super::memory_from_status;

    #[test]
    fn proc_memory_is_optional_and_uses_kib() {
        assert_eq!(
            memory_from_status("Name:\ttest\nVmHWM:\t123 kB\nVmRSS:\t42 kB\n"),
            (Some(42), Some(123))
        );
        assert_eq!(
            memory_from_status("VmRSS: bad kB\nVmHWM: 4 MB\n"),
            (None, None)
        );
        assert_eq!(memory_from_status(""), (None, None));
    }
}
