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

const AGENT_DETACHED: &str = "the DevTools agent detached";

#[derive(Default)]
struct Pending {
    next_id: AtomicI32,
    waiting: Mutex<HashMap<i32, Callback>>,
}

impl Pending {
    fn insert(&self, callback: Callback) -> i32 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.waiting).insert(id, callback);
        id
    }

    fn take(&self, id: i32) -> Option<Callback> {
        lock(&self.waiting).remove(&id)
    }

    fn fail_all(&self, reason: &str) {
        let waiting: Vec<Callback> = lock(&self.waiting)
            .drain()
            .map(|(_, callback)| callback)
            .collect();
        for callback in waiting {
            callback(Err(reason.to_owned()));
        }
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
        let session = lock(&self.session).take();
        self.pending.fail_all("the page closed");
        drop(session);
    }

    fn send(&self, method: &str, params: Value, reply: Callback) -> Result<i32, String> {
        let session = lock(&self.session);
        let host = session
            .as_ref()
            .map(|session| session.host.clone())
            .ok_or_else(|| format!("{method}: the page closed"))?;
        // Explicit detach cannot drain the queue between finding the session
        // and registering this callback, leaving its receiver stranded.
        let id = self.pending.insert(reply);
        drop(session);
        let message = json!({ "id": id, "method": method, "params": params }).to_string();
        let pending = Arc::clone(&self.pending);
        let rejected = format!("{method}: CEF rejected the DevTools message");
        let posted = post_ui(move || {
            // A detach (or caller deadline) may have canceled a queued command
            // before its UI task runs. Do not send it to a newly attached agent.
            if !lock(&pending.waiting).contains_key(&id) {
                return;
            }
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
        fn on_dev_tools_agent_detached(&self, _browser: Option<&mut Browser>) {
            // CEF discards every pending result at this boundary, including
            // screenshot results. Later commands may attach a fresh agent.
            self.pending.fail_all(AGENT_DETACHED);
        }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_detach_settles_pending_results_in_either_callback_order() {
        for reply_first in [false, true] {
            let pending = Arc::new(Pending::default());
            let observer = Observer::new(Arc::clone(&pending));
            let (sender, receiver) = channel();
            let id = pending.insert(Box::new(move |reply| {
                let _ = sender.send(reply);
            }));
            let (async_sender, async_receiver) = tokio::sync::oneshot::channel();
            pending.insert(Box::new(move |reply| {
                let _ = async_sender.send(reply);
            }));
            let response = json!({ "id": id, "result": { "data": "snapshot" } }).to_string();

            if reply_first {
                observer.on_dev_tools_message(None, Some(response.as_bytes()));
            }
            observer.on_dev_tools_agent_detached(None);
            // A late result cannot replace the terminal error or complete twice.
            observer.on_dev_tools_message(None, Some(response.as_bytes()));

            let reply = receiver.recv().expect("request completion");
            if reply_first {
                assert_eq!(reply.unwrap(), json!({ "data": "snapshot" }));
            } else {
                assert_eq!(reply, Err(AGENT_DETACHED.to_owned()));
            }
            assert_eq!(
                async_receiver.blocking_recv().expect("async completion"),
                Err(AGENT_DETACHED.to_owned())
            );
            assert!(lock(&pending.waiting).is_empty());

            // CEF permits a later command to attach a fresh agent.
            let (sender, receiver) = channel();
            let next = pending.insert(Box::new(move |reply| {
                let _ = sender.send(reply);
            }));
            let response = json!({ "id": next, "result": {} }).to_string();
            observer.on_dev_tools_message(None, Some(response.as_bytes()));
            assert_eq!(receiver.recv().expect("reattached result"), Ok(json!({})));
        }
    }

    #[test]
    #[ignore = "needs the CEF runtime and helper; see docs/browser.md"]
    fn terminating_after_sending_a_screenshot_completes_its_waiter() {
        use crate::offline::ArtifactBundle;
        use crate::{ArtifactContext, CefRuntime, Dialogs, RuntimeConfig};

        let helper = std::env::current_exe()
            .expect("test executable")
            .parent()
            .and_then(|deps| deps.parent())
            .expect("target profile directory")
            .join(crate::runtime::HELPER_NAME);
        let runtime = CefRuntime::initialize(RuntimeConfig {
            helper_path: helper,
            ..RuntimeConfig::offline()
        })
        .expect("CEF runtime");

        for close_browser in [true, false] {
            let context = ArtifactContext::create(ArtifactBundle::html(
                "<!doctype html><body style=\"background:blue\">",
            ))
            .expect("artifact context");
            let browser = OsrBrowser::create(
                context,
                &ArtifactContext::url("index.html"),
                [320, 240],
                1.0,
                Dialogs::Suppress,
                30,
            )
            .expect("browser");
            assert!(browser.wait_loaded(None));
            let devtools = Arc::new(DevTools::attach(&browser).expect("DevTools"));
            devtools
                .call(
                    "Runtime.evaluate",
                    json!({ "expression": "1" }),
                    Duration::MAX,
                )
                .expect("agent attached");

            let (sender, receiver) = channel();
            let ui_devtools = Arc::clone(&devtools);
            let host = SendCell(browser.host().expect("browser host"));
            // Sending and terminating in one UI task prevents an asynchronous
            // renderer snapshot reply from arriving between these operations.
            on_ui(move || {
                ui_devtools
                    .send(
                        "Page.captureScreenshot",
                        json!({ "format": "png", "fromSurface": true }),
                        Box::new(move |reply| {
                            let _ = sender.send(reply);
                        }),
                    )
                    .expect("screenshot sent");
                if close_browser {
                    host.0.close_browser(1);
                } else {
                    ui_devtools.detach();
                }
            })
            .expect("UI task");

            assert_eq!(
                receiver.recv().expect("screenshot completion"),
                Err(if close_browser {
                    AGENT_DETACHED
                } else {
                    "the page closed"
                }
                .to_owned())
            );
            if close_browser {
                browser.wait_for(|status| status.closed);
            } else {
                assert!(
                    devtools
                        .call("Runtime.evaluate", json!({}), Duration::MAX)
                        .is_err()
                );
            }
            drop(devtools);
            drop(browser);
        }
        drop(runtime);
    }
}
