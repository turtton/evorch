//! GUI event pump (event-bus subscription to a bounded frame batch).

use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use event_bus::{Event, EventReceiver, MutationBatchError, MutationValidator, RecvError};

const MAX_FRAME_EVENTS: usize = 256;
// A paused/minimized GUI must not retain a SQLite read transaction indefinitely.
const MAX_GUARD_HOLD: Duration = Duration::from_millis(100);
const CONTENTION_RETRY_DELAY: Duration = Duration::from_millis(10);
const SLOW_VALIDATION: Duration = Duration::from_millis(200);
const WARNING_INTERVAL: Duration = Duration::from_secs(30);
type Repaint = Option<Arc<dyn Fn() + Send + Sync>>;

#[derive(Default)]
enum Delivery {
    #[default]
    Empty,
    Ready {
        events: Vec<Event>,
        remaining: Vec<Event>,
        revision: usize,
    },
    Retry(Vec<Event>),
}

#[derive(Default)]
struct Handoff {
    delivery: Mutex<Delivery>,
    consumed: Condvar,
    closed: AtomicBool,
}

/// tokio のイベント購読を GUI フレーム用の標準チャネルへ橋渡しする。
/// SQLite mutation guards are acquired and released by a blocking worker.
pub struct EventPump {
    validator: MutationValidator,
    rx: mpsc::Receiver<Event>,
    requests: mpsc::Sender<Vec<Event>>,
    handoff: Arc<Handoff>,
    in_flight: bool,
    repaint: Repaint,
    task: tokio::task::JoinHandle<()>,
}

impl EventPump {
    /// イベント購読タスクを起動し、フレーム側のポンプを生成する。
    pub fn spawn(
        handle: &tokio::runtime::Handle,
        mut receiver: EventReceiver,
        repaint: Repaint,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let validator = receiver.mutation_validator();
        let (requests, request_rx) = mpsc::channel();
        let handoff = Arc::new(Handoff::default());
        let worker_handoff = Arc::clone(&handoff);
        let worker_validator = validator.clone();
        let worker_repaint = repaint.clone();
        // A long-lived blocking task would make Runtime::drop wait for the GUI
        // to drop its pump first. A dedicated thread owns this independent queue.
        std::thread::Builder::new()
            .name("evorch-event-validation".into())
            .spawn(move || {
                validate_batches(request_rx, worker_validator, worker_handoff, worker_repaint);
            })
            .expect("event validation worker");
        let subscription_repaint = repaint.clone();
        let task = handle.spawn(async move {
            loop {
                let event = match receiver.recv().await {
                    Ok(event) => event,
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                };
                if tx.send(event).is_err() {
                    break;
                }
                request_repaint(&subscription_repaint);
            }
        });
        Self {
            rx,
            requests,
            handoff,
            in_flight: false,
            repaint,
            task,
            validator,
        }
    }

    /// Take at most one validated batch without waiting for ownership I/O.
    /// A pending batch is retained for a later repaint; ordering is preserved.
    pub fn drain(&mut self) -> Vec<Event> {
        let mut output = Vec::new();
        let mut inspected = 0;
        if self.in_flight {
            let Ok(mut delivery) = self.handoff.delivery.try_lock() else {
                request_repaint(&self.repaint);
                return output;
            };
            let retry = match std::mem::take(&mut *delivery) {
                Delivery::Empty => return output,
                Delivery::Ready {
                    events,
                    remaining,
                    revision,
                } => {
                    inspected = events.len();
                    // The worker retains all database guards until this mutex is
                    // released. Only legacy check-only callbacks run here.
                    match self.validator.recheck_batch(events, revision) {
                        Ok(events) => {
                            output = events;
                            (!remaining.is_empty()).then_some(remaining)
                        }
                        Err(mut events) => {
                            events.extend(remaining);
                            Some(events)
                        }
                    }
                }
                Delivery::Retry(events) => Some(events),
            };
            self.handoff.consumed.notify_one();
            drop(delivery);
            self.in_flight = false;
            if let Some(events) = retry {
                self.in_flight = self.requests.send(events).is_ok();
                return output;
            }
        }
        let remaining = MAX_FRAME_EVENTS - inspected;
        let events: Vec<_> = self.rx.try_iter().take(remaining).collect();
        let hit_limit = events.len() == remaining;
        if !events.is_empty() {
            match self.validator.try_check_batch(events) {
                Ok(events) => output.extend(events),
                Err(events) => self.in_flight = self.requests.send(events).is_ok(),
            }
        }
        if hit_limit {
            request_repaint(&self.repaint);
        }
        output
    }
}

fn request_repaint(repaint: &Repaint) {
    if let Some(repaint) = repaint {
        repaint();
    }
}

fn validate_batches(
    requests: mpsc::Receiver<Vec<Event>>,
    validator: MutationValidator,
    handoff: Arc<Handoff>,
    repaint: Repaint,
) {
    let mut last_warning: Option<Instant> = None;
    for mut events in requests {
        if handoff.closed.load(Ordering::Acquire) {
            break;
        }
        let started = Instant::now();
        let event_count = events.len();
        let guards = validator.acquire_batch(&events);
        let elapsed = started.elapsed();
        if elapsed >= SLOW_VALIDATION
            && last_warning.is_none_or(|last| last.elapsed() >= WARNING_INTERVAL)
        {
            tracing::warn!(
                event_count,
                duration_ms = elapsed.as_millis() as u64,
                "GUI event mutation validation was slow"
            );
            last_warning = Some(Instant::now());
        }
        let (events, remaining, revision) =
            match &guards {
                Ok(guards) => {
                    let remaining = events.split_off(guards.accepted().len());
                    let events = events
                        .into_iter()
                        .zip(guards.accepted())
                        .filter_map(|(event, accepted)| accepted.then_some(event))
                        .collect();
                    (events, remaining, guards.revision())
                }
                Err(MutationBatchError::Busy) => {
                    // acquire_batch has released every partial read transaction.
                    // Preserve the whole batch and back off without holding guards,
                    // so an SQLite writer can commit before the next GUI attempt.
                    let Ok(mut delivery) = handoff.delivery.lock() else {
                        break;
                    };
                    *delivery = Delivery::Retry(events);
                    let Ok((delivery, _)) = handoff.consumed.wait_timeout_while(
                        delivery,
                        CONTENTION_RETRY_DELAY,
                        |_| !handoff.closed.load(Ordering::Acquire),
                    ) else {
                        break;
                    };
                    drop(delivery);
                    request_repaint(&repaint);
                    continue;
                }
                // A poisoned fence registry never authorizes events.
                Err(MutationBatchError::Poisoned) => (Vec::new(), Vec::new(), 0),
            };
        let Ok(mut delivery) = handoff.delivery.lock() else {
            break;
        };
        if handoff.closed.load(Ordering::Acquire) {
            break;
        }
        *delivery = Delivery::Ready {
            events,
            remaining,
            revision,
        };
        drop(delivery);
        request_repaint(&repaint);
        let Ok(delivery) = handoff.delivery.lock() else {
            break;
        };
        let Ok((mut delivery, _)) =
            handoff
                .consumed
                .wait_timeout_while(delivery, MAX_GUARD_HOLD, |delivery| {
                    matches!(delivery, Delivery::Ready { .. })
                        && !handoff.closed.load(Ordering::Acquire)
                })
        else {
            break;
        };
        let expired = expire_delivery(&mut delivery);
        drop(delivery);
        // Release the SQLite transaction on this worker, including expiration,
        // shutdown, and rejected batches. The GUI never owns/drops a guard.
        drop(guards);
        if expired {
            request_repaint(&repaint);
        }
    }
}

fn expire_delivery(delivery: &mut Delivery) -> bool {
    if let Delivery::Ready {
        events, remaining, ..
    } = delivery
    {
        events.append(remaining);
        *delivery = Delivery::Retry(std::mem::take(events));
        true
    } else {
        false
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        self.task.abort();
        self.handoff.closed.store(true, Ordering::Release);
        self.handoff.consumed.notify_one();
    }
}

#[cfg(test)]
mod tests;
