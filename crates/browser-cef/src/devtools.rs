//! In-process Chrome DevTools Protocol session; no remote-debugging port is opened.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use cef::*;
use serde_json::{Value, json};

use crate::browser::OsrBrowser;
use crate::runtime::{SendCell, on_ui, post_ui};

pub type Reply = Result<Value, String>;

type Callback = Box<dyn FnOnce(Reply) + Send>;

#[derive(Default)]
struct Pending {
    next_id: AtomicI32,
    waiting: Mutex<HashMap<i32, Callback>>,
}

impl Pending {
    fn take(&self, id: i32) -> Option<Callback> {
        lock(&self.waiting).remove(&id)
    }
}

struct Session {
    host: SendCell<BrowserHost>,
    _registration: SendCell<Registration>,
}

pub struct DevTools {
    session: Mutex<Option<Session>>,
    pending: Arc<Pending>,
}

impl DevTools {
    pub fn attach(browser: &OsrBrowser) -> Result<Self, String> {
        let host = browser.host().ok_or("the browser is closed")?;
        let pending = Arc::new(Pending {
            next_id: AtomicI32::new(1),
            ..Default::default()
        });
        let observer_pending = Arc::clone(&pending);
        let observer_host = SendCell(host.clone());
        let registration = on_ui(move || {
            let mut observer = Observer::new(observer_pending);
            observer_host
                .0
                .add_dev_tools_message_observer(Some(&mut observer))
                .map(SendCell)
        })
        .flatten()
        .ok_or("CEF refused the DevTools observer")?;
        Ok(Self {
            session: Mutex::new(Some(Session {
                host: SendCell(host),
                _registration: registration,
            })),
            pending,
        })
    }

    /// Sends one CDP command and waits up to `timeout` for its result.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        let (sender, receiver) = channel();
        let id = self.send(
            method,
            params,
            Box::new(move |reply| {
                let _ = sender.send(reply);
            }),
        )?;
        let reply = receiver.recv_timeout(timeout);
        self.pending.take(id);
        reply.map_err(|_| format!("{method}: no reply"))?
    }

    /// [`DevTools::call`] for async callers; it never blocks the calling thread.
    pub async fn call_async(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let id = self.send(
            method,
            params,
            Box::new(move |reply| {
                let _ = sender.send(reply);
            }),
        )?;
        let reply = tokio::time::timeout(timeout, receiver).await;
        self.pending.take(id);
        match reply {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) => Err(format!("{method}: the page closed")),
            Err(_) => Err(format!("{method}: no reply")),
        }
    }

    /// Releases the browser and fails every pending command; later calls fail.
    pub fn detach(&self) {
        drop(lock(&self.session).take());
        let waiting: Vec<Callback> = lock(&self.pending.waiting)
            .drain()
            .map(|(_, callback)| callback)
            .collect();
        for callback in waiting {
            callback(Err("the page closed".into()));
        }
    }

    fn send(&self, method: &str, params: Value, reply: Callback) -> Result<i32, String> {
        let host = lock(&self.session)
            .as_ref()
            .map(|session| session.host.clone())
            .ok_or_else(|| format!("{method}: the page closed"))?;
        let id = self.pending.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.pending.waiting).insert(id, reply);
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        let pending = Arc::clone(&self.pending);
        let rejected = format!("{method}: CEF rejected the DevTools message");
        let posted = post_ui(move || {
            if host.0.send_dev_tools_message(Some(message.as_bytes())) != 1
                && let Some(callback) = pending.take(id)
            {
                callback(Err(rejected));
            }
        });
        if posted {
            Ok(id)
        } else {
            self.pending.take(id);
            Err(format!("{method}: CEF is not running"))
        }
    }
}

impl Drop for DevTools {
    fn drop(&mut self) {
        self.detach();
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

wrap_dev_tools_message_observer! {
    struct Observer {
        pending: Arc<Pending>,
    }

    impl DevToolsMessageObserver {
        fn on_dev_tools_message(&self, _browser: Option<&mut Browser>, message: Option<&[u8]>) -> i32 {
            let Some(message) = message.and_then(|message| serde_json::from_slice::<Value>(message).ok()) else {
                return 0;
            };
            let Some(id) = message.get("id").and_then(Value::as_i64) else {
                return 0;
            };
            let reply = match message.get("error") {
                Some(error) => Err(error.to_string()),
                None => Ok(message.get("result").cloned().unwrap_or(Value::Null)),
            };
            if let Some(callback) = self.pending.take(id as i32) {
                callback(reply);
            }
            1
        }
    }
}
