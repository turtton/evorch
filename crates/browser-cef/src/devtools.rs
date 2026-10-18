//! In-process Chrome DevTools Protocol session; no remote-debugging port is opened.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use cef::*;
use serde_json::{Value, json};

use crate::browser::OsrBrowser;
use crate::runtime::{SendCell, on_ui};

pub type Reply = Result<Value, String>;

#[derive(Default)]
struct Pending {
    next_id: AtomicI32,
    waiting: Mutex<HashMap<i32, Sender<Reply>>>,
}

pub struct DevTools {
    host: SendCell<BrowserHost>,
    pending: Arc<Pending>,
    _registration: SendCell<Registration>,
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
            host: SendCell(host),
            pending,
            _registration: registration,
        })
    }

    /// Sends one CDP command and waits up to `timeout` for its result.
    pub fn call(&self, method: &str, params: Value, timeout: Duration) -> Reply {
        let (sender, receiver) = channel();
        let id = self.send(method, params, sender)?;
        let reply = receiver.recv_timeout(timeout);
        self.lock().remove(&id);
        reply.map_err(|_| format!("{method}: no reply"))?
    }

    fn send(&self, method: &str, params: Value, reply: Sender<Reply>) -> Result<i32, String> {
        let id = self.pending.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(id, reply);
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        let host = self.host.clone();
        let sent = on_ui(move || host.0.send_dev_tools_message(Some(message.as_bytes())) == 1)
            .unwrap_or(false);
        if sent {
            Ok(id)
        } else {
            self.lock().remove(&id);
            Err(format!("{method}: CEF rejected the DevTools message"))
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i32, Sender<Reply>>> {
        self.pending
            .waiting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
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
            let sender = self
                .pending
                .waiting
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&(id as i32));
            if let Some(sender) = sender {
                let _ = sender.send(reply);
            }
            1
        }
    }
}
