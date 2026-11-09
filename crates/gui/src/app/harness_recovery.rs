//! Display-only replay of inputs committed before their announcement could be emitted.
use crate::model::{harness_card::HarnessCard, transcript_registry::TranscriptRegistry};
use event_bus::{Event, LifecycleEvent};
use std::{collections::VecDeque, time::SystemTime};

pub(super) struct HarnessReplay(VecDeque<storage::AdoptedHarnessNotice>);

impl HarnessReplay {
    pub(super) fn load(db: &storage::Database) -> Result<Self, storage::StorageError> {
        Ok(Self::from_notices(db.adopted_harness_notices(None)?))
    }

    pub(super) fn from_notices(notices: Vec<storage::AdoptedHarnessNotice>) -> Self {
        Self(notices.into())
    }

    /// Interleave at the same durable host-input clock used by composer history. Within
    /// one adoption transaction the verified message ordinal, not the notice ID, orders
    /// cards. No event is republished/persisted, no model context or receipt is changed.
    pub(super) fn show_through(
        &mut self,
        transcripts: &mut TranscriptRegistry,
        at: Option<SystemTime>,
    ) {
        while self.0.front().is_some_and(|delivery| {
            at.is_none_or(|at| storage::ns_to_system_time(delivery.adopted_at_ns) <= at)
        }) {
            let delivery = self.0.pop_front().expect("front exists");
            let Some(card) = HarnessCard::from_notice(&delivery.notice) else {
                continue;
            };
            let mut event = Event::new(LifecycleEvent::HarnessNoticeDelivered {
                run_id: delivery.run_id,
                notification_id: card.notice_id.clone(),
            });
            event.meta.wall_clock = storage::ns_to_system_time(delivery.adopted_at_ns);
            transcripts.set_source(None);
            transcripts.apply(&event);
            transcripts.fill_harness_card(&card);
        }
    }
}
