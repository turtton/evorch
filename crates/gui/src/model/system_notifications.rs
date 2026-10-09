//! Best-effort OS notifications, isolated from the GUI and runtime event bus.

/// User-facing notification text. Bodies contain a question or thread title only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemNotification {
    pub title: String,
    pub body: String,
}

/// Implementations must return promptly; native delivery runs on a dedicated worker.
pub trait SystemNotificationSink: Send + Sync {
    fn send(&self, notification: SystemNotification);
}

/// Native desktop delivery with a bounded queue. Errors never interrupt agent work.
#[cfg(not(target_arch = "wasm32"))]
pub struct NativeSystemNotificationSink {
    sender: std::sync::mpsc::SyncSender<SystemNotification>,
}

#[cfg(not(target_arch = "wasm32"))]
impl NativeSystemNotificationSink {
    pub fn new() -> Result<Self, std::io::Error> {
        Self::spawn(deliver_native)
    }

    fn spawn(
        deliver: impl FnMut(SystemNotification) + Send + 'static,
    ) -> Result<Self, std::io::Error> {
        let (sender, receiver) = std::sync::mpsc::sync_channel::<SystemNotification>(32);
        std::thread::Builder::new()
            .name("evorch-system-notifications".into())
            .spawn(move || {
                let mut deliver = deliver;
                for notification in receiver {
                    deliver(notification);
                }
            })?;
        Ok(Self { sender })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn deliver_native(notification: SystemNotification) {
    let mut native = notify_rust::Notification::new();
    native.appname("evorch").summary(&notification.title);
    // freedesktop notification bodies may interpret markup. The other
    // platforms display plain text, so escape only for that backend.
    #[cfg(all(unix, not(target_os = "macos")))]
    let body = notification
        .body
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    let body = notification.body;
    if let Err(error) = native.body(&body).show() {
        tracing::warn!(%error, "system notification delivery failed");
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl SystemNotificationSink for NativeSystemNotificationSink {
    fn send(&self, notification: SystemNotification) {
        if self.sender.try_send(notification).is_err() {
            tracing::warn!("system notification queue unavailable; notification dropped");
        }
    }
}

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;

    #[test]
    fn blocked_delivery_keeps_enqueue_nonblocking_and_backlog_bounded() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (delivered_tx, delivered_rx) = std::sync::mpsc::channel();
        let mut first = true;
        let sink = NativeSystemNotificationSink::spawn(move |notification| {
            if first {
                first = false;
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            }
            delivered_tx.send(notification).unwrap();
        })
        .unwrap();
        let notification = SystemNotification {
            title: "question".into(),
            body: "scope".into(),
        };
        sink.send(notification.clone());
        entered_rx.recv().unwrap();
        // Delivery is deliberately blocked. Saturating the queue must still
        // return so this thread can release the worker (no timing assertion).
        for _ in 0..100 {
            sink.send(notification.clone());
        }
        release_tx.send(()).unwrap();
        drop(sink);
        let delivered: Vec<_> = delivered_rx.iter().collect();
        assert_eq!(delivered.len(), 33);
        assert!(delivered.iter().all(|item| item == &notification));
    }
}
