//! イベントバスを介した利用者承認を扱います。

use std::{sync::Arc, time::Duration};

use event_bus::{Event, EventBus, EventKind, RecvError, ToolEvent};

/// 承認要求の結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalOutcome {
    Approved,
    Denied,
    TimedOut,
}

/// 承認要求と応答待機をまとめるゲート。
pub struct ApprovalGate {
    event_bus: Arc<EventBus>,
    timeout: Duration,
}

impl ApprovalGate {
    pub const fn new(event_bus: Arc<EventBus>, timeout: Duration) -> Self {
        Self { event_bus, timeout }
    }

    pub async fn request(&self, tool_name: &str, call_id: &str) -> ApprovalOutcome {
        let mut receiver = self.event_bus.subscribe();
        self.event_bus
            .emit(Event::new(ToolEvent::ApprovalRequested {
                tool_name: tool_name.to_owned(),
                call_id: call_id.to_owned(),
            }));

        let response = async {
            loop {
                match receiver.recv().await {
                    Ok(event) => match event.kind {
                        EventKind::Tool(ToolEvent::ApprovalResolved {
                            call_id: resolved_id,
                            approved,
                        }) if resolved_id == call_id => {
                            return if approved {
                                ApprovalOutcome::Approved
                            } else {
                                ApprovalOutcome::Denied
                            };
                        }
                        EventKind::Lifecycle(_)
                        | EventKind::Message(_)
                        | EventKind::Tool(_)
                        | EventKind::Usage(_)
                        | EventKind::Provider(_)
                        | EventKind::Fault(_)
                        | EventKind::AgentMessage(_) => {}
                    },
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return ApprovalOutcome::TimedOut,
                }
            }
        };

        tokio::time::timeout(self.timeout, response)
            .await
            .unwrap_or(ApprovalOutcome::TimedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{EventKind, EventReceiver};

    async fn respond(event_bus: Arc<EventBus>, mut receiver: EventReceiver, approved: bool) {
        let event = receiver.recv().await.expect("承認要求を受信できるはずです");
        let EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. }) = event.kind else {
            panic!("承認要求イベントであるはずです");
        };
        event_bus.emit(Event::new(ToolEvent::ApprovalResolved {
            call_id,
            approved,
        }));
    }

    // Given: 承認する応答者 / When: 承認を要求 / Then: 承認済みになる
    #[tokio::test]
    async fn request_returns_approved() {
        let event_bus = Arc::new(EventBus::new(16));
        let receiver = event_bus.subscribe();
        let responder = tokio::spawn(respond(Arc::clone(&event_bus), receiver, true));
        let gate = ApprovalGate::new(event_bus, Duration::from_secs(1));

        assert_eq!(
            gate.request("shell", "call-1").await,
            ApprovalOutcome::Approved
        );
        responder.await.expect("応答タスクが完了するはずです");
    }

    // Given: 拒否する応答者 / When: 承認を要求 / Then: 拒否になる
    #[tokio::test]
    async fn request_returns_denied() {
        let event_bus = Arc::new(EventBus::new(16));
        let receiver = event_bus.subscribe();
        let responder = tokio::spawn(respond(Arc::clone(&event_bus), receiver, false));
        let gate = ApprovalGate::new(event_bus, Duration::from_secs(1));

        assert_eq!(
            gate.request("shell", "call-2").await,
            ApprovalOutcome::Denied
        );
        responder.await.expect("応答タスクが完了するはずです");
    }

    // Given: 応答者なし / When: 短い期限で承認を要求 / Then: 時間切れになる
    #[tokio::test]
    async fn request_times_out_without_responder() {
        let gate = ApprovalGate::new(Arc::new(EventBus::new(16)), Duration::from_millis(50));

        assert_eq!(
            gate.request("shell", "call-3").await,
            ApprovalOutcome::TimedOut
        );
    }

    // Given: 異なる ID の応答後に正しい応答を返す応答者 / When: 承認を要求 / Then: 正しい応答だけ採用される
    #[tokio::test]
    async fn request_ignores_foreign_call_id() {
        let event_bus = Arc::new(EventBus::new(16));
        let responder_bus = Arc::clone(&event_bus);
        let mut receiver = event_bus.subscribe();
        let responder = tokio::spawn(async move {
            let _request = receiver.recv().await.expect("承認要求を受信できるはずです");
            responder_bus.emit(Event::new(ToolEvent::ApprovalResolved {
                call_id: "foreign".to_owned(),
                approved: false,
            }));
            responder_bus.emit(Event::new(ToolEvent::ApprovalResolved {
                call_id: "call-4".to_owned(),
                approved: true,
            }));
        });
        let gate = ApprovalGate::new(event_bus, Duration::from_secs(1));

        assert_eq!(
            gate.request("shell", "call-4").await,
            ApprovalOutcome::Approved
        );
        responder.await.expect("応答タスクが完了するはずです");
    }
}
