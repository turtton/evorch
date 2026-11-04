use event_bus::CompactionEvent;

use super::TranscriptEntry;

fn redactor() -> &'static secret_guard::SecretRedactor {
    static REDACTOR: std::sync::OnceLock<secret_guard::SecretRedactor> = std::sync::OnceLock::new();
    REDACTOR.get_or_init(secret_guard::SecretRedactor::from_env)
}

fn entry(event: &CompactionEvent) -> TranscriptEntry {
    match event {
        CompactionEvent::Compacted {
            reason,
            threshold,
            context_window_tokens,
            estimated_tokens_before,
            estimated_tokens_after,
            compacted_range_start,
            compacted_range_end,
            checkpoint_id,
            summary,
            ..
        } => TranscriptEntry::Compaction {
            reason: *reason,
            threshold: *threshold,
            context_window_tokens: *context_window_tokens,
            estimated_tokens_before: *estimated_tokens_before,
            estimated_tokens_after: *estimated_tokens_after,
            compacted_range_start: *compacted_range_start,
            compacted_range_end: *compacted_range_end,
            checkpoint_id: checkpoint_id.clone(),
            summary: redactor().redact(summary).text,
        },
        _ => unreachable!("only durable compaction has a completed entry"),
    }
}

/// Live-only, bounded observation; the raw accumulated text never reaches a renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionProgressModel {
    pub run_id: String,
    pub operation_id: String,
    pub mode: Option<event_bus::CompactionMode>,
    pub attempt: u64,
    pub revision: u64,
    pub text: String,
    pub active: bool,
    pub interrupted: bool,
    raw: String,
}
impl super::TranscriptModel {
    pub(crate) fn interrupt_compactions(&mut self, run_id: Option<&str>) {
        for i in 0..self.entries.len() {
            if matches!(&self.entries[i], TranscriptEntry::CompactionProgress { progress } if progress.active && run_id.is_none_or(|run| progress.run_id == run))
                && let Some(TranscriptEntry::CompactionProgress { progress }) = self.entry_mut(i)
            {
                progress.active = false;
                progress.interrupted = true;
                progress.raw.clear();
            }
        }
    }
    pub(super) fn apply_compaction(&mut self, event: &event_bus::Event) -> bool {
        use event_bus::{CompactionMode, EventKind, LifecycleEvent, RunActivity};
        if let EventKind::Lifecycle(LifecycleEvent::RunProgress {
            run_id,
            activity: RunActivity::Compaction,
            compaction_operation_id: Some(operation_id),
            ..
        }) = &event.kind
        {
            if self
                .closed_compactions
                .contains(&(run_id.clone(), operation_id.clone()))
            {
                return true;
            }
            // Duplicate starts cannot revive terminal operations.
            if self.entries.iter().any(|entry| matches!(entry, TranscriptEntry::CompactionProgress { progress } if progress.run_id == *run_id && progress.operation_id == *operation_id)) { return true; }
            self.push(TranscriptEntry::CompactionProgress {
                progress: CompactionProgressModel {
                    run_id: run_id.clone(),
                    operation_id: operation_id.clone(),
                    mode: None,
                    attempt: 0,
                    revision: 0,
                    text: String::new(),
                    active: true,
                    interrupted: false,
                    raw: String::new(),
                },
            });
            return true;
        }
        let (run, operation) = match &event.kind {
            EventKind::Compaction(
                CompactionEvent::Compacted {
                    run_id,
                    operation_id,
                    ..
                }
                | CompactionEvent::Progress {
                    run_id,
                    operation_id,
                    ..
                }
                | CompactionEvent::Aborted {
                    run_id,
                    operation_id,
                },
            ) => (run_id, operation_id),
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                run_id,
                to:
                    event_bus::AgentRunPhase::Done
                    | event_bus::AgentRunPhase::Error
                    | event_bus::AgentRunPhase::Stopped,
                ..
            }) => {
                self.interrupt_compactions(Some(run_id));
                return false;
            }
            _ => return false,
        };
        if matches!(
            event.kind,
            EventKind::Compaction(
                CompactionEvent::Compacted { .. } | CompactionEvent::Aborted { .. }
            )
        ) {
            self.closed_compactions
                .push_back((run.clone(), operation.clone()));
            if self.closed_compactions.len() > self.capacity {
                self.closed_compactions.pop_front();
            }
        }
        let position = self.entries.iter().position(|entry| matches!(entry, TranscriptEntry::CompactionProgress { progress } if progress.run_id == *run && progress.operation_id == *operation));
        match &event.kind {
            EventKind::Compaction(event @ CompactionEvent::Compacted { .. }) => {
                if let Some(position) = position {
                    *self.entry_mut(position).unwrap() = entry(event);
                } else {
                    self.push(entry(event));
                }
            }
            EventKind::Compaction(CompactionEvent::Aborted { .. }) => {
                if let Some(position) = position
                    && let Some(TranscriptEntry::CompactionProgress { progress }) =
                        self.entry_mut(position)
                {
                    progress.active = false;
                    progress.interrupted = true;
                    progress.raw.clear();
                }
            }
            EventKind::Compaction(CompactionEvent::Progress {
                mode,
                attempt,
                revision,
                delta,
                ..
            }) => {
                if let Some(position) = position
                    && let Some(TranscriptEntry::CompactionProgress { progress }) =
                        self.entry_mut(position)
                {
                    if !progress.active
                        || *attempt < progress.attempt
                        || (*attempt == progress.attempt && *revision <= progress.revision)
                    {
                        return true;
                    }
                    if *attempt != progress.attempt {
                        progress.raw.clear();
                        progress.text.clear();
                        progress.revision = 0;
                    }
                    // Never append after a lost delta: a partial secret could otherwise leak.
                    if *revision != progress.revision + 1 {
                        progress.active = false;
                        progress.interrupted = true;
                        progress.raw.clear();
                        return true;
                    }
                    progress.mode = Some(*mode);
                    progress.attempt = *attempt;
                    progress.revision = *revision;
                    if *mode == CompactionMode::ModelSummary
                        && delta.len() <= event_bus::COMPACTION_DELTA_MAX_BYTES
                        && progress.raw.len() + delta.len() <= 64 * 1024
                    {
                        progress.raw.push_str(delta);
                        progress.text = redactor().redact_streaming(&progress.raw).text;
                    }
                }
            }
            _ => {}
        }
        true
    }
}
