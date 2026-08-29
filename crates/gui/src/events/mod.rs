//! GUI event pump (event-bus subscription to std channel bridge).

use std::sync::{Arc, mpsc};

use event_bus::{Event, EventReceiver, RecvError};

/// tokio のイベント購読を GUI フレーム用の標準チャネルへ橋渡しする。
pub struct EventPump {
    rx: mpsc::Receiver<Event>,
    task: tokio::task::JoinHandle<()>,
}

impl EventPump {
    /// イベント購読タスクを起動し、フレーム側のポンプを生成する。
    pub fn spawn(
        handle: &tokio::runtime::Handle,
        mut receiver: EventReceiver,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
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
                if let Some(repaint) = &repaint {
                    repaint();
                }
            }
        });
        Self { rx, task }
    }

    /// 現在キューにあるイベントを非ブロッキングで全て取り出す。
    pub fn drain(&mut self) -> Vec<Event> {
        self.rx.try_iter().collect()
    }
}

impl Drop for EventPump {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    };
    use std::time::Duration;

    use event_bus::{Event, EventBus, LifecycleEvent};

    use super::EventPump;

    #[test]
    fn pump_forwards_bus_events_to_frame_queue() {
        // Given: a subscribed event bus and a running tokio runtime
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let bus = EventBus::new(8);
        let (repaint_sender, repaint_receiver) = mpsc::channel();
        let mut pump = EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_sender.send(());
            })),
        );
        let event = Event::new(LifecycleEvent::Started {
            session_id: String::from("session-1"),
        });

        // When: an event is emitted and the forwarding task is allowed to receive it
        bus.emit(event.clone());
        assert!(
            repaint_receiver
                .recv_timeout(Duration::from_secs(1))
                .is_ok()
        );

        // Then: drain returns the same event
        assert_eq!(pump.drain(), vec![event]);
    }

    #[test]
    fn pump_invokes_repaint_hook_on_event() {
        // Given: a repaint hook recording invocations
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let bus = EventBus::new(8);
        let repaint_count = Arc::new(AtomicUsize::new(0));
        let repaint_count_for_hook = Arc::clone(&repaint_count);
        let (repaint_sender, repaint_receiver) = mpsc::channel();
        let mut pump = EventPump::spawn(
            runtime.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                repaint_count_for_hook.fetch_add(1, Ordering::Relaxed);
                let _ = repaint_sender.send(());
            })),
        );

        // When: an event is emitted
        bus.emit(Event::new(LifecycleEvent::Started {
            session_id: String::from("session-1"),
        }));
        assert!(
            repaint_receiver
                .recv_timeout(Duration::from_secs(1))
                .is_ok()
        );

        // Then: the hook runs once and the event is queued
        assert_eq!(repaint_count.load(Ordering::Relaxed), 1);
        assert_eq!(pump.drain().len(), 1);
    }

    #[test]
    fn drain_returns_empty_when_idle() {
        // Given: a pump with no events emitted
        let runtime = tokio::runtime::Runtime::new().expect("test runtime");
        let bus = EventBus::new(8);
        let mut pump = EventPump::spawn(runtime.handle(), bus.subscribe(), None);

        // When: the frame queue is drained
        let events = pump.drain();

        // Then: no event is available
        assert!(events.is_empty());
    }
}
