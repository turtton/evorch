//! A live event stream and the rules for ending it.

use crate::client::{Inner, Shared};
use crate::error::ClientError;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::sync::mpsc;
use workbench_protocol::envelope::{Control, RequestBody};
use workbench_protocol::events::{
    EventCursor, EventEnvelope, ResyncReason, SubscriptionStarted, UiEvent,
};
use workbench_protocol::ids::SubscriptionId;

/// Why a subscription stopped delivering events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionEnd {
    /// The caller was not reading fast enough. A [`UiEvent::ResyncRequired`]
    /// was delivered last; take a new snapshot.
    SlowSubscriber,
    /// The core restarted under the stream. A [`UiEvent::ResyncRequired`] was
    /// delivered last; take a new snapshot.
    EpochChanged,
    /// The core ended it with `ResyncRequired` or `SubscriptionClosed`, which was delivered last.
    EndedByCore,
    /// The caller unsubscribed or dropped the subscription.
    Unsubscribed,
    /// The connection closed.
    Disconnected,
}

pub(crate) struct Queued {
    pub(crate) envelope: EventEnvelope,
    pub(crate) bytes: usize,
}

/// Client-side half of a subscription, owned by the reader task.
pub(crate) struct SubscriptionEntry {
    /// Which registration of the id this is; see `SubscriptionGuard`.
    pub(crate) generation: u64,
    pub(crate) tx: mpsc::Sender<Queued>,
    pub(crate) tracker: workbench_protocol::events::CursorTracker,
    pub(crate) queued_bytes: Arc<AtomicUsize>,
    pub(crate) end: Arc<Mutex<Option<SubscriptionEnd>>>,
}

impl SubscriptionEntry {
    /// Record why the stream ended, then close the queue. Events already
    /// queued are still delivered.
    pub(crate) fn finish(self, end: SubscriptionEnd) {
        let mut slot = self.end.lock().unwrap_or_else(PoisonError::into_inner);
        slot.get_or_insert(end);
    }
}

/// Releases a registered subscription unless it was explicitly handed over.
///
/// The reader registers a subscription the moment the core's reply arrives,
/// before any caller has the `Subscription`. This guard travels with the
/// reply, so wherever the reply ends up (a live `Subscription`, a reply nobody
/// is waiting for any more, or a future dropped before the reply came), dropping
/// it unregisters the subscription and tells the core to stop sending.
pub(crate) struct SubscriptionGuard {
    shared: Arc<Shared>,
    id: SubscriptionId,
    /// The registration this guard owns. A subscription that ended earlier
    /// may be followed by a new one reusing the id; only the matching
    /// generation may release it.
    generation: u64,
    armed: bool,
}

impl SubscriptionGuard {
    pub(crate) fn new(shared: Arc<Shared>, id: SubscriptionId, generation: u64) -> Self {
        Self {
            shared,
            id,
            generation,
            armed: true,
        }
    }

    /// The owner is releasing the subscription itself.
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if self.armed {
            self.shared.release_subscription(&self.id, self.generation);
        }
    }
}

/// Everything the reader hands to the caller when a subscription starts.
pub(crate) struct SubscriptionParts {
    pub(crate) guard: SubscriptionGuard,
    pub(crate) started: SubscriptionStarted,
    pub(crate) rx: mpsc::Receiver<Queued>,
    pub(crate) queued_bytes: Arc<AtomicUsize>,
    pub(crate) end: Arc<Mutex<Option<SubscriptionEnd>>>,
}

/// Events for one subscription, in cursor order, without duplicates.
///
/// Events at or before the snapshot boundary (`started().high_water`) are
/// dropped because the snapshot already contains them. When the stream cannot
/// continue, a final [`UiEvent::ResyncRequired`] is delivered and then
/// [`Subscription::next`] returns `None`; [`Subscription::end`] says why.
pub struct Subscription {
    // Declared before `client` so the release is queued while the connection is still alive.
    guard: SubscriptionGuard,
    client: Arc<Inner>,
    started: SubscriptionStarted,
    rx: mpsc::Receiver<Queued>,
    queued_bytes: Arc<AtomicUsize>,
    end: Arc<Mutex<Option<SubscriptionEnd>>>,
    last_cursor: EventCursor,
    synthesized: bool,
}

impl Subscription {
    pub(crate) fn new(client: Arc<Inner>, parts: SubscriptionParts) -> Self {
        Self {
            guard: parts.guard,
            client,
            last_cursor: parts.started.high_water,
            started: parts.started,
            rx: parts.rx,
            queued_bytes: parts.queued_bytes,
            end: parts.end,
            synthesized: false,
        }
    }

    /// The core's answer to the subscribe request, including the snapshot.
    pub fn started(&self) -> &SubscriptionStarted {
        &self.started
    }

    pub fn id(&self) -> &SubscriptionId {
        &self.started.subscription_id
    }

    /// The next event, or `None` once the subscription is over.
    pub async fn next(&mut self) -> Option<EventEnvelope> {
        if let Some(queued) = self.rx.recv().await {
            self.queued_bytes.fetch_sub(queued.bytes, Ordering::Relaxed);
            self.last_cursor = queued.envelope.cursor;
            return Some(queued.envelope);
        }
        if self.synthesized {
            return None;
        }
        self.synthesized = true;
        let reason = match self.end() {
            Some(SubscriptionEnd::SlowSubscriber) => ResyncReason::SlowSubscriber,
            Some(SubscriptionEnd::EpochChanged) => ResyncReason::EpochChanged,
            _ => return None,
        };
        Some(EventEnvelope {
            subscription_id: self.started.subscription_id.clone(),
            cursor: self.last_cursor,
            caused_by_request_id: None,
            event: UiEvent::ResyncRequired { reason },
        })
    }

    /// Why the subscription ended; `None` while it is live.
    pub fn end(&self) -> Option<SubscriptionEnd> {
        *self.end.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Stop the subscription and wait for the core to confirm.
    pub async fn unsubscribe(mut self) -> Result<(), ClientError> {
        self.guard.disarm();
        let current = self
            .client
            .shared
            .drop_subscription(self.id(), self.guard.generation);
        if !current {
            // Already over (ended by the core, a resync or a close). The id may
            // belong to a newer subscription now, which must not be told to stop.
            return Ok(());
        }
        let body = RequestBody::Control(Control::Unsubscribe {
            subscription_id: self.id().clone(),
        });
        self.client
            .call(body, crate::client::PendingKind::Control)
            .await?;
        Ok(())
    }
}
