use event_bus::CompactionEvent;

use super::TranscriptEntry;

pub(super) fn entry(event: &CompactionEvent) -> TranscriptEntry {
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
            summary: summary.clone(),
        },
    }
}
