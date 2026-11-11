use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_tungstenite::tungstenite::Message;
use async_tungstenite::tungstenite::protocol::WebSocketConfig;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};

use super::AgentError;

#[derive(Debug)]
pub(super) struct Event {
    pub method: String,
    pub params: Value,
}

type Reply = oneshot::Sender<Result<Value, String>>;

struct Pending {
    reply: Reply,
    session: Option<mpsc::UnboundedSender<SessionMessage>>,
}

/// Events and command replies share a queue, so a reply cannot overtake
/// navigation events that were received before it on the CDP connection.
pub(super) enum SessionMessage {
    Event(Event),
    Reply {
        reply: Reply,
        outcome: Result<Value, String>,
    },
}

impl SessionMessage {
    pub fn into_event(self) -> Option<Event> {
        match self {
            Self::Event(event) => Some(event),
            Self::Reply { reply, outcome } => {
                let _ = reply.send(outcome);
                None
            }
        }
    }
}

/// A CDP client of its own on the browser endpoint. chromiumoxide resumes
/// every target it attaches as soon as it sees it, so a new tab would load
/// before anyone could enable Fetch on it; this connection auto-attaches
/// targets paused and resumes them itself.
pub(super) struct Connection {
    outgoing: mpsc::UnboundedSender<String>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, Pending>>,
    sessions: Mutex<HashMap<String, mpsc::UnboundedSender<SessionMessage>>>,
}

impl Connection {
    /// Connects to `url`. Browser-level events go to `on_event` in the order
    /// they arrive, before any later message is read, so it can route a new
    /// session before that session's first event; `None` follows the last.
    pub async fn open(
        url: &str,
        mut on_event: impl FnMut(&Arc<Self>, Option<Event>) + Send + 'static,
    ) -> Result<(Arc<Self>, tokio::task::JoinHandle<()>), AgentError> {
        let config = WebSocketConfig::default()
            .max_message_size(None)
            .max_frame_size(None);
        let (mut socket, _) =
            async_tungstenite::tokio::connect_async_with_config(url, Some(config))
                .await
                .map_err(|error| AgentError::Launch(format!("CDP connection: {error}")))?;
        let (outgoing, mut queued) = mpsc::unbounded_channel();
        let connection = Arc::new(Self {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            sessions: Mutex::default(),
        });
        let reader = Arc::clone(&connection);
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    message = socket.next() => match message {
                        Some(Ok(Message::Text(text))) => reader.dispatch(text.as_str(), &mut on_event),
                        Some(Ok(Message::Close(_)) | Err(_)) | None => break,
                        Some(Ok(_)) => {}
                    },
                    Some(text) = queued.recv() => {
                        if socket.send(Message::text(text)).await.is_err() {
                            break;
                        }
                    }
                }
            }
            lock(&reader.pending).clear();
            lock(&reader.sessions).clear();
            on_event(&reader, None);
        });
        Ok((connection, task))
    }

    fn dispatch(
        self: &Arc<Self>,
        text: &str,
        on_event: &mut impl FnMut(&Arc<Self>, Option<Event>),
    ) {
        let Ok(mut message) = serde_json::from_str::<Value>(text) else {
            tracing::debug!("agent browser CDP message was not JSON");
            return;
        };
        if let Some(id) = message.get("id").and_then(Value::as_u64) {
            if let Some(reply) = lock(&self.pending).remove(&id) {
                let outcome = match message.get_mut("error") {
                    Some(error) => Err(error
                        .get("message")
                        .and_then(Value::as_str)
                        .map_or_else(|| error.to_string(), str::to_owned)),
                    None => Ok(message
                        .get_mut("result")
                        .map(Value::take)
                        .unwrap_or_default()),
                };
                match reply.session {
                    Some(session) => {
                        let _ = session.send(SessionMessage::Reply {
                            reply: reply.reply,
                            outcome,
                        });
                    }
                    None => {
                        let _ = reply.reply.send(outcome);
                    }
                }
            }
            return;
        }
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            return;
        };
        let event = Event {
            method: method.to_owned(),
            params: message
                .get_mut("params")
                .map(Value::take)
                .unwrap_or_default(),
        };
        match message.get("sessionId").and_then(Value::as_str) {
            Some(session) => {
                if let Some(events) = lock(&self.sessions).get(session) {
                    let _ = events.send(SessionMessage::Event(event));
                }
            }
            None => on_event(self, Some(event)),
        }
    }

    /// Sends `method` to `session`, or to the browser with `None`, when
    /// called rather than when first polled, so commands keep call order. A
    /// session reply resolves only after its preceding events are processed.
    pub fn call(
        &self,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> impl std::future::Future<Output = Result<Value, AgentError>> + Send + 'static {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut message = json!({"id": id, "method": method, "params": params});
        if let Some(session) = session {
            message["sessionId"] = json!(session);
        }
        let (reply, response) = oneshot::channel();
        let events = session.and_then(|session| lock(&self.sessions).get(session).cloned());
        lock(&self.pending).insert(
            id,
            Pending {
                reply,
                session: events,
            },
        );
        let sent = self.outgoing.send(message.to_string()).is_ok();
        if !sent {
            lock(&self.pending).remove(&id);
        }
        let method = method.to_owned();
        async move {
            let failed = |detail: String| AgentError::Cdp {
                method: method.clone(),
                detail,
            };
            if !sent {
                return Err(failed("the browser connection closed".into()));
            }
            response
                .await
                .map_err(|_| failed("the browser connection closed".into()))?
                .map_err(failed)
        }
    }

    pub fn subscribe(&self, session: &str) -> mpsc::UnboundedReceiver<SessionMessage> {
        let (events, receiver) = mpsc::unbounded_channel();
        lock(&self.sessions).insert(session.to_owned(), events);
        receiver
    }

    pub fn forget(&self, session: &str) {
        lock(&self.sessions).remove(session);
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> (Arc<Connection>, mpsc::UnboundedReceiver<String>) {
        let (outgoing, commands) = mpsc::unbounded_channel();
        let connection = Arc::new(Connection {
            outgoing,
            next_id: AtomicU64::new(1),
            pending: Mutex::default(),
            sessions: Mutex::default(),
        });
        (connection, commands)
    }

    #[tokio::test]
    async fn session_reply_waits_for_preceding_navigation_events() {
        let (connection, mut commands) = connection();
        let mut events = connection.subscribe("page");
        let reply = connection.call(Some("page"), "Page.navigateToHistoryEntry", json!({}));
        tokio::pin!(reply);
        let command: Value = serde_json::from_str(&commands.recv().await.unwrap()).unwrap();
        connection.dispatch(
            &json!({
                "sessionId": "page", "method": "Page.navigatedWithinDocument",
                "params": {"frameId": "frame", "url": "https://example.test/#details"}
            })
            .to_string(),
            &mut |_, _| unreachable!(),
        );
        connection.dispatch(
            &json!({"id": command["id"], "sessionId": "page", "result": {}}).to_string(),
            &mut |_, _| unreachable!(),
        );

        assert!(
            futures_util::poll!(reply.as_mut()).is_pending(),
            "the command must not complete while its navigation event is still queued"
        );
        assert_eq!(
            events.recv().await.unwrap().into_event().unwrap().method,
            "Page.navigatedWithinDocument"
        );
        assert!(futures_util::poll!(reply.as_mut()).is_pending());
        assert!(events.recv().await.unwrap().into_event().is_none());
        assert_eq!(reply.await.unwrap(), json!({}));
    }

    #[tokio::test]
    async fn session_errors_are_delivered_and_a_closed_session_cancels_its_reply() {
        let (connection, mut commands) = connection();
        let mut events = connection.subscribe("page");
        let reply = connection.call(Some("page"), "Page.navigate", json!({}));
        let command: Value = serde_json::from_str(&commands.recv().await.unwrap()).unwrap();
        connection.dispatch(
            &json!({"id": command["id"], "error": {"message": "navigation refused"}}).to_string(),
            &mut |_, _| unreachable!(),
        );
        assert!(events.recv().await.unwrap().into_event().is_none());
        assert!(matches!(
            reply.await,
            Err(AgentError::Cdp { detail, .. }) if detail == "navigation refused"
        ));

        let reply = connection.call(Some("page"), "Page.navigate", json!({}));
        let command: Value = serde_json::from_str(&commands.recv().await.unwrap()).unwrap();
        drop(events);
        connection.dispatch(
            &json!({"id": command["id"], "result": {}}).to_string(),
            &mut |_, _| unreachable!(),
        );
        assert!(matches!(
            reply.await,
            Err(AgentError::Cdp { detail, .. }) if detail == "the browser connection closed"
        ));
    }

    #[tokio::test]
    async fn browser_reply_needs_no_session_event_pump() {
        let (connection, mut commands) = connection();
        let reply = connection.call(None, "Target.closeTarget", json!({}));
        let command: Value = serde_json::from_str(&commands.recv().await.unwrap()).unwrap();
        connection.dispatch(
            &json!({"id": command["id"], "result": {"success": true}}).to_string(),
            &mut |_, _| unreachable!(),
        );
        assert_eq!(reply.await.unwrap(), json!({"success": true}));
    }
}
