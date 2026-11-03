//! Live-only observation. This guard spans preparation AND durable installation.
#[cfg(test)]
#[path = "progress_tests.rs"]
mod tests;
use event_bus::{COMPACTION_DELTA_MAX_BYTES, CompactionEvent, CompactionMode, Event, EventBus};
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicU64, Ordering},
};

pub(crate) struct Operation {
    pub(crate) id: String,
    bus: Arc<EventBus>,
    run: String,
    started: bool,
    committed: bool,
    attempt: Arc<AtomicU64>,
}
impl Operation {
    pub(crate) fn new(bus: Arc<EventBus>, run: String) -> Self {
        // Reuse runtime IDs so replayed operations cannot collide after a
        // process restart / PID reuse on the same saved run.
        static IDS: OnceLock<crate::run_ids::RunIds> = OnceLock::new();
        let id = format!("compaction-{}", IDS.get_or_init(Default::default).next());
        Self {
            id,
            bus,
            run,
            started: false,
            committed: false,
            attempt: Arc::new(AtomicU64::new(0)),
        }
    }
    pub(crate) fn start(&mut self, state: &crate::agent_loop::LoopState) {
        if !self.started {
            self.started = true;
            state.compaction_activity(self.id.clone());
        }
    }
    pub(crate) fn path(&mut self, mode: CompactionMode) -> SummaryProgress {
        let attempt = self.attempt.fetch_add(1, Ordering::Relaxed) + 1;
        let mut stream = SummaryProgress {
            bus: self.bus.clone(),
            run: self.run.clone(),
            operation: self.id.clone(),
            mode,
            attempt,
            attempts: self.attempt.clone(),
            revision: 0,
            pending: String::new(),
            shown: 0,
        };
        stream.flush();
        stream
    }
    pub(crate) fn committed(&mut self) {
        self.committed = true;
    }
}
impl Drop for Operation {
    fn drop(&mut self) {
        if self.started && !self.committed {
            self.bus.emit(Event::new(CompactionEvent::Aborted {
                run_id: self.run.clone(),
                operation_id: self.id.clone(),
            }));
        }
    }
}

/// Coalesce tiny provider fragments by bytes. Cap the preview at 64 KiB per
/// attempt (16 full deltas); at most 256 revisions including timed flushes.
/// This bounds main-bus traffic even for huge or single-character streams.
/// The full accepted summary still lives in the existing checkpoint only.
pub(crate) struct SummaryProgress {
    bus: Arc<EventBus>,
    run: String,
    operation: String,
    mode: CompactionMode,
    attempt: u64,
    attempts: Arc<AtomicU64>,
    revision: u64,
    pending: String,
    shown: usize,
}
impl SummaryProgress {
    pub(crate) fn text(&mut self, text: &str) {
        if self.mode != CompactionMode::ModelSummary || self.revision >= 256 {
            return;
        }
        let remaining = (64 * 1024_usize).saturating_sub(self.shown + self.pending.len());
        let mut end = text.len().min(remaining);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        self.pending.push_str(&text[..end]);
        while self.pending.len() >= COMPACTION_DELTA_MAX_BYTES && self.revision < 256 {
            self.flush();
        }
    }
    pub(crate) fn retry(&mut self) {
        self.attempt = self.attempts.fetch_add(1, Ordering::Relaxed) + 1;
        self.revision = 0;
        self.pending.clear();
        self.shown = 0;
        self.flush();
    }
    pub(crate) fn flush_pending(&mut self) {
        if !self.pending.is_empty() && self.revision < 256 {
            self.flush();
        }
    }
    pub(crate) fn event(&mut self, event: &event_bus::Event) {
        match &event.kind {
            event_bus::EventKind::Message(event_bus::MessageEvent::MessageDelta {
                delta, ..
            }) => self.text(delta),
            event_bus::EventKind::Provider(event_bus::ProviderEvent::FallbackTriggered {
                ..
            })
            | event_bus::EventKind::Message(event_bus::MessageEvent::StreamRetried { .. }) => {
                self.retry()
            }
            _ => {}
        }
    }
    pub(crate) fn flush(&mut self) {
        let mut end = self.pending.len().min(COMPACTION_DELTA_MAX_BYTES);
        while !self.pending.is_char_boundary(end) {
            end -= 1;
        }
        let delta: String = self.pending.drain(..end).collect();
        self.shown += delta.len();
        self.revision += 1;
        self.bus.emit(Event::new(CompactionEvent::Progress {
            run_id: self.run.clone(),
            operation_id: self.operation.clone(),
            mode: self.mode,
            attempt: self.attempt,
            revision: self.revision,
            delta,
        }));
    }
}
