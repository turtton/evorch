//! `GET /api/v1/events`: Server-Sent Events over a bounded queue.
//!
//! Wire format of each SSE message:
//!
//! - `event: subscription_started`, `id: <epoch>:<sequence>` of the
//!   snapshot's `high_water`, `data:` the `SubscriptionStarted` JSON. Always
//!   the first message.
//! - `event: ui_event`, `id: <epoch>:<sequence>` of the event's cursor,
//!   `data:` the `EventEnvelope` JSON.
//! - `: keepalive` comment lines. They have no `id`, so they never move the
//!   `Last-Event-ID` a browser will send.
//!
//! A `ResyncRequired` or `SubscriptionClosed` event is the last message; the
//! stream ends after it. The adapter itself synthesizes a `ResyncRequired`
//! (without an `id`) when the browser cannot keep up or the epoch changed.

use crate::backend::{BackendSubscription, WorkbenchBackend};
use crate::http_error::{error_response, reject};
use crate::state::AppState;
use axum::extract::rejection::QueryRejection;
use axum::extract::{Query, State};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use futures_core::Stream;
use http::{HeaderMap, StatusCode};
use serde::Deserialize;
use std::convert::Infallible;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};
use tokio::sync::{OwnedSemaphorePermit, mpsc};
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::events::{
    CursorDecision, CursorTracker, EventCursor, EventEnvelope, ResyncReason, Subscribe,
    SubscriptionScope, UiEvent,
};
use workbench_protocol::ids::{ClientId, ProjectId, SubscriptionId, ThreadId};
use workbench_protocol::namespaces::ProjectionEvent;
use workbench_protocol::wire::{InstanceEpoch, Sequence, parse_u64};

#[derive(Debug, Deserialize)]
pub(crate) struct EventsQuery {
    scope: Option<String>,
    after: Option<String>,
}

/// `all`, `project:<id>` or `thread:<id>`.
pub fn parse_scope(text: &str) -> Result<SubscriptionScope, WorkbenchError> {
    let bad = || WorkbenchError::invalid_request("scope must be all, project:<id> or thread:<id>");
    if text == "all" {
        return Ok(SubscriptionScope::All);
    }
    let (kind, id) = text.split_once(':').ok_or_else(bad)?;
    match kind {
        "project" => Ok(SubscriptionScope::Project {
            project: ProjectId::new(id).map_err(|_| bad())?,
        }),
        "thread" => Ok(SubscriptionScope::Thread {
            thread: ThreadId::new(id).map_err(|_| bad())?,
        }),
        _ => Err(bad()),
    }
}

/// The string form of a cursor used for SSE `id`, `Last-Event-ID` and `after`:
/// `<core_epoch>:<sequence>`, both canonical decimals.
pub fn format_cursor(cursor: EventCursor) -> String {
    format!("{}:{}", cursor.core_epoch, cursor.sequence)
}

pub fn parse_cursor(text: &str) -> Result<EventCursor, WorkbenchError> {
    let bad = || WorkbenchError::invalid_request("cursor must be <core_epoch>:<sequence>");
    let (epoch, sequence) = text.split_once(':').ok_or_else(bad)?;
    Ok(EventCursor {
        core_epoch: InstanceEpoch(parse_u64(epoch).ok_or_else(bad)?),
        sequence: Sequence(parse_u64(sequence).ok_or_else(bad)?),
    })
}

/// One serialized SSE message waiting in the queue.
struct Item {
    id: Option<String>,
    name: &'static str,
    data: String,
}

struct Queue {
    tx: mpsc::Sender<Item>,
    bytes: Arc<AtomicUsize>,
    max_bytes: usize,
}

impl Queue {
    /// Queue an item unless that would exceed a bound. The last slot is kept
    /// for the terminal message, which is why it needs more than one free.
    fn push(&self, item: Item) -> bool {
        let len = item.data.len();
        if self.tx.capacity() <= 1 || self.bytes.load(Ordering::Acquire) + len > self.max_bytes {
            return false;
        }
        self.bytes.fetch_add(len, Ordering::AcqRel);
        self.tx.try_send(item).is_ok()
    }

    /// Queue the final message into the reserved slot.
    fn push_terminal(&self, item: Item) {
        self.bytes.fetch_add(item.data.len(), Ordering::AcqRel);
        let _ = self.tx.try_send(item);
    }
}

fn ui_item<B: WorkbenchBackend>(
    backend: &B,
    envelope: &EventEnvelope,
    with_id: bool,
) -> Option<Item> {
    Some(Item {
        id: with_id.then(|| format_cursor(envelope.cursor)),
        name: "ui_event",
        data: crate::output::encode(backend, envelope).ok()?,
    })
}

fn resync_item<B: WorkbenchBackend>(
    backend: &B,
    subscription_id: &SubscriptionId,
    last: EventCursor,
    reason: ResyncReason,
) -> Option<Item> {
    ui_item(
        backend,
        &EventEnvelope {
            subscription_id: subscription_id.clone(),
            cursor: last,
            caused_by_request_id: None,
            event: UiEvent::Projection(ProjectionEvent::ResyncRequired { reason }),
        },
        false,
    )
}

/// Moves events from the backend into the bounded queue and never waits for
/// the browser: a full queue ends the stream with `ResyncRequired`.
async fn pump<B: WorkbenchBackend>(
    backend: Arc<B>,
    mut events: mpsc::Receiver<EventEnvelope>,
    queue: Queue,
    mut tracker: CursorTracker,
    subscription_id: SubscriptionId,
    shutdown: crate::shutdown::ShutdownSignal,
) {
    loop {
        let envelope = tokio::select! {
            biased;
            () = shutdown.triggered() => return,
            () = queue.tx.closed() => return,
            next = events.recv() => match next {
                Some(envelope) => envelope,
                None => return,
            },
        };
        match tracker.observe(envelope.cursor) {
            CursorDecision::Duplicate => continue,
            CursorDecision::EpochMismatch => {
                if let Some(item) = resync_item(
                    backend.as_ref(),
                    &subscription_id,
                    tracker.last(),
                    ResyncReason::EpochChanged,
                ) {
                    queue.push_terminal(item);
                }
                return;
            }
            CursorDecision::Deliver => {}
        }
        let terminal = envelope.event.ends_subscription();
        let Some(item) = ui_item(backend.as_ref(), &envelope, true) else {
            return;
        };
        if !queue.push(item) {
            if let Some(item) = resync_item(
                backend.as_ref(),
                &subscription_id,
                tracker.last(),
                ResyncReason::SlowSubscriber,
            ) {
                queue.push_terminal(item);
            }
            return;
        }
        if terminal {
            return;
        }
    }
}

/// The body of the response: queued items, then end of stream.
struct SseStream {
    rx: mpsc::Receiver<Item>,
    bytes: Arc<AtomicUsize>,
    /// Released with the stream, which frees the SSE slot.
    _slot: OwnedSemaphorePermit,
}

impl Stream for SseStream {
    type Item = Result<Event, Infallible>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(item)) => {
                let _ = self
                    .bytes
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                        Some(n.saturating_sub(item.data.len()))
                    });
                let mut event = Event::default().event(item.name).data(item.data);
                if let Some(id) = item.id {
                    event = event.id(id);
                }
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

fn last_event_id(headers: &HeaderMap) -> Result<Option<&str>, WorkbenchError> {
    let mut values = headers.get_all("last-event-id").iter();
    let Some(first) = values.next() else {
        return Ok(None);
    };
    let bad = || WorkbenchError::invalid_request("Last-Event-ID is not a cursor");
    if values.next().is_some() {
        return Err(bad());
    }
    first.to_str().map(Some).map_err(|_| bad())
}

pub(crate) async fn events<B: WorkbenchBackend>(
    State(state): State<Arc<AppState<B>>>,
    query: Result<Query<EventsQuery>, QueryRejection>,
    headers: HeaderMap,
) -> Response {
    let Ok(Query(query)) = query else {
        return error_response(WorkbenchError::invalid_request("malformed query"), None);
    };
    let scope = match query.scope.as_deref().map(parse_scope).transpose() {
        Ok(scope) => scope.unwrap_or(SubscriptionScope::All),
        Err(error) => return error_response(error, None),
    };
    // A reconnecting browser sends the id of the last message it saw, which is
    // newer than the `after` the page was opened with, so the header wins.
    let after = match last_event_id(&headers) {
        Ok(Some(header)) => Some(parse_cursor(header)),
        Ok(None) => query.after.as_deref().map(parse_cursor),
        Err(error) => Some(Err(error)),
    };
    let after = match after.transpose() {
        Ok(after) => after,
        Err(error) => return error_response(error, None),
    };

    if state.shared.shutdown.is_triggered() {
        return reject(
            StatusCode::SERVICE_UNAVAILABLE,
            ErrorCode::Draining,
            "server is shutting down",
        );
    }
    let Ok(slot) = Arc::clone(&state.shared.sse_slots).try_acquire_owned() else {
        return reject(
            StatusCode::TOO_MANY_REQUESTS,
            ErrorCode::Busy,
            "too many event streams",
        );
    };

    let client = match ClientId::new(format!("web-sse-{}", state.next_client_number())) {
        Ok(client) => client,
        Err(error) => return error_response(error.into(), None),
    };
    let subscription = match state
        .backend
        .subscribe(client, Subscribe { scope, after })
        .await
    {
        Ok(subscription) => subscription,
        Err(error) => return error_response(error, None),
    };
    let BackendSubscription { started, events } = subscription;

    let limits = state.wire_limits;
    let capacity = limits.subscription_queue_events as usize + 2;
    let (tx, rx) = mpsc::channel(capacity);
    let bytes = Arc::new(AtomicUsize::new(0));
    let queue = Queue {
        tx,
        bytes: Arc::clone(&bytes),
        max_bytes: limits.subscription_queue_bytes as usize,
    };
    let Ok(data) = crate::output::encode(state.backend.as_ref(), &started) else {
        return error_response(
            WorkbenchError::new(
                ErrorCode::Internal,
                "internal error",
                workbench_protocol::errors::RetryAdvice::Never,
            ),
            None,
        );
    };
    // The first message is not subject to the bounds: the queue is empty.
    queue.push_terminal(Item {
        id: Some(format_cursor(started.high_water)),
        name: "subscription_started",
        data,
    });
    tokio::spawn(pump(
        Arc::clone(&state.backend),
        events,
        queue,
        CursorTracker::new(started.high_water),
        started.subscription_id.clone(),
        state.shared.shutdown.clone(),
    ));

    let stream = SseStream {
        rx,
        bytes,
        _slot: slot,
    };
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(state.sse_keepalive)
                .text("keepalive"),
        )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursors_round_trip_and_reject_noise() {
        let cursor = EventCursor {
            core_epoch: InstanceEpoch(7),
            sequence: Sequence(u64::MAX),
        };
        assert_eq!(parse_cursor(&format_cursor(cursor)).unwrap(), cursor);
        for bad in [
            "", "7", "7:", ":1", "07:1", "7:01", "-1:2", "7:1:2", "a:b", " 7:1",
        ] {
            assert!(parse_cursor(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn scopes_parse() {
        assert_eq!(parse_scope("all").unwrap(), SubscriptionScope::All);
        assert!(matches!(
            parse_scope("thread:t1").unwrap(),
            SubscriptionScope::Thread { .. }
        ));
        for bad in ["", "thread", "thread:", "other:x", "ALL"] {
            assert!(parse_scope(bad).is_err(), "{bad:?}");
        }
    }
}
