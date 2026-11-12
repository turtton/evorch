//! Bounded subscription delivery. Snapshot/high-water/registration and publication
//! share the projection mutex; a queue reserves a terminal slot even when full.
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::Notify;
use workbench_protocol::namespaces::ProjectionEvent;
use workbench_protocol::{events::*, ids::SubscriptionId};

#[derive(Debug, Clone, Copy)]
pub struct SubscriptionLimits {
    pub replay_events: usize,
    pub replay_bytes: usize,
    pub queue_events: usize,
    pub queue_bytes: usize,
}
impl Default for SubscriptionLimits {
    fn default() -> Self {
        Self {
            replay_events: 4096,
            replay_bytes: 8 * 1024 * 1024,
            queue_events: 256,
            queue_bytes: 1024 * 1024,
        }
    }
}
#[derive(Default)]
struct Pending {
    events: VecDeque<(EventEnvelope, usize)>,
    bytes: usize,
    ended: bool,
}
pub(crate) struct Queue {
    pending: Mutex<Pending>,
    changed: Notify,
}
impl Queue {
    fn new() -> Self {
        Self {
            pending: Mutex::default(),
            changed: Notify::new(),
        }
    }
    pub(crate) fn push(&self, mut event: EventEnvelope, limits: SubscriptionLimits) {
        let mut p = self.pending.lock().unwrap();
        if p.ended {
            return;
        }
        let bytes = serde_json::to_vec(&event).expect("wire event").len();
        if !event.event.ends_subscription()
            && (p.events.len() >= limits.queue_events
                || p.bytes.saturating_add(bytes) > limits.queue_bytes)
        {
            p.events.clear();
            p.bytes = 0;
            event.event = UiEvent::Projection(ProjectionEvent::ResyncRequired {
                reason: ResyncReason::SlowSubscriber,
            });
        }
        if event.event.ends_subscription() {
            p.events.clear();
            p.bytes = 0;
        }
        p.ended = event.event.ends_subscription();
        let bytes = serde_json::to_vec(&event).expect("wire event").len();
        p.bytes += bytes;
        p.events.push_back((event, bytes));
        drop(p);
        self.changed.notify_one();
    }
}
/// One subscription. Dropping it releases its queue; the hub holds only a Weak.
pub struct Subscription {
    pub started: SubscriptionStarted,
    pub(crate) queue: Arc<Queue>,
}
impl Subscription {
    pub async fn recv(&mut self) -> Option<EventEnvelope> {
        loop {
            let notified = self.queue.changed.notified();
            {
                let mut p = self.queue.pending.lock().unwrap();
                if let Some((e, bytes)) = p.events.pop_front() {
                    p.bytes -= bytes;
                    return Some(e);
                }
                if p.ended {
                    return None;
                }
            }
            notified.await;
        }
    }
}
pub(crate) struct Subscriber {
    pub id: SubscriptionId,
    pub scope: SubscriptionScope,
    pub queue: Weak<Queue>,
}
pub(crate) fn attach(
    started: SubscriptionStarted,
    scope: SubscriptionScope,
) -> (Subscription, Subscriber) {
    let queue = Arc::new(Queue::new());
    let subscriber = Subscriber {
        id: started.subscription_id.clone(),
        scope,
        queue: Arc::downgrade(&queue),
    };
    (Subscription { started, queue }, subscriber)
}
