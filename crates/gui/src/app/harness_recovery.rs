//! Display-only replay of inputs committed before their announcement could be emitted.
use crate::model::{harness_card::HarnessCard, transcript_registry::TranscriptRegistry};
use event_bus::{Event, LifecycleEvent};
use std::{collections::VecDeque, time::SystemTime};

struct PendingNotice {
    delivery: storage::AdoptedHarnessNotice,
    through_event: i64,
}

#[derive(Default)]
pub(super) struct HarnessReplay(VecDeque<PendingNotice>);

impl HarnessReplay {
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn load(db: &storage::Database) -> Result<Self, storage::StorageError> {
        Ok(Self::from_snapshot(
            db.adopted_harness_notice_snapshot(None)?,
        ))
    }

    pub(super) fn from_snapshot(snapshot: storage::HarnessNoticeSnapshot) -> Self {
        Self(
            snapshot
                .notices
                .into_iter()
                .map(|delivery| PendingNotice {
                    delivery,
                    through_event: snapshot.through_event,
                })
                .collect(),
        )
    }

    pub(super) fn extend(&mut self, other: Self) {
        let mut pending: Vec<_> = self.0.drain(..).chain(other.0).collect();
        pending.sort_by(|a, b| {
            let (a, b) = (&a.delivery, &b.delivery);
            (a.adopted_at_ns, &a.run_id, a.ordinal).cmp(&(b.adopted_at_ns, &b.run_id, b.ordinal))
        });
        pending.dedup_by(|a, b| {
            if a.delivery.notice.notice_id != b.delivery.notice.notice_id {
                return false;
            }
            b.through_event = b.through_event.max(a.through_event);
            true
        });
        self.0 = pending.into();
    }

    /// Known events admit preceding inputs at the same durable host-input clock
    /// as composer history. Within one adoption the verified message ordinal orders
    /// cards. No event is republished/persisted, no model context or receipt changes.
    pub(super) fn show_through(&mut self, transcripts: &mut TranscriptRegistry, at: SystemTime) {
        self.show_matching(transcripts, |pending| {
            storage::ns_to_system_time(pending.delivery.adopted_at_ns) <= at
        });
    }

    /// Composer messages beyond a bounded replay's last event cannot advance its
    /// notice tail until that replay covers the evidence's observed log horizon.
    pub(super) fn show_message_through(
        &mut self,
        transcripts: &mut TranscriptRegistry,
        at: SystemTime,
        through: i64,
    ) {
        self.show_matching(transcripts, |pending| {
            pending.through_event <= through
                && storage::ns_to_system_time(pending.delivery.adopted_at_ns) <= at
        });
    }

    /// A commit-only tail has no later event to trigger it. Flush it only once the
    /// event horizon observed with its adoption evidence has actually been folded.
    pub(super) fn show_tail_through(&mut self, transcripts: &mut TranscriptRegistry, through: i64) {
        self.show_matching(transcripts, |pending| pending.through_event <= through);
    }

    fn show_matching(
        &mut self,
        transcripts: &mut TranscriptRegistry,
        ready: impl Fn(&PendingNotice) -> bool,
    ) {
        while self.0.front().is_some_and(&ready) {
            let delivery = self.0.pop_front().expect("front exists").delivery;
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
