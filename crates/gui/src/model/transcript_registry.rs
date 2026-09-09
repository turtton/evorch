//! Transcript 配送の責務を集約し、run ID ごとにイベントを分離する。
//!
//! `MessageDelta` / `ReasoningDelta` は payload の `run_id` が `Some` なら thread と
//! 該当 run の両 transcript へ決定的に配送する。`run_id` が `None` の delta は
//! 警告して完全に破棄し、Running の run 数にかかわらず配送先を推測しない。

use std::collections::BTreeMap;

use event_bus::{AgentMessageEvent, Event, EventKind, MessageEvent, ToolEvent};

use super::transcript::{MessageDirection, TranscriptEntry, TranscriptModel};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptKey {
    Thread,
    Run(String),
}

#[derive(Debug, Clone)]
pub struct TranscriptRegistry {
    thread: TranscriptModel,
    runs: BTreeMap<String, TranscriptModel>,
    call_index: BTreeMap<String, String>,
}

impl Default for TranscriptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptRegistry {
    pub fn new() -> Self {
        Self {
            thread: TranscriptModel::new(),
            runs: BTreeMap::new(),
            call_index: BTreeMap::new(),
        }
    }

    pub fn route(&self, event: &Event) -> Vec<TranscriptKey> {
        match &event.kind {
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: event_bus::AgentRunPhase::Error,
                ..
            }) => vec![TranscriptKey::Thread, TranscriptKey::Run(run_id.clone())],
            EventKind::Lifecycle(event_bus::LifecycleEvent::Failed { .. })
            | EventKind::Fault(_) => {
                vec![TranscriptKey::Thread]
            }
            EventKind::Provider(event_bus::ProviderEvent::RequestFailed { run_id, .. }) => {
                match run_id {
                    Some(run_id) => vec![TranscriptKey::Thread, TranscriptKey::Run(run_id.clone())],
                    None => vec![TranscriptKey::Thread],
                }
            }
            EventKind::Message(MessageEvent::MessageDelta { run_id, .. })
            | EventKind::Message(MessageEvent::ReasoningDelta { run_id, .. }) => match run_id {
                Some(run_id) => vec![TranscriptKey::Thread, TranscriptKey::Run(run_id.clone())],
                None => Vec::new(),
            },
            EventKind::Tool(ToolEvent::ToolStarted { run_id, .. })
            | EventKind::Tool(ToolEvent::ToolCompleted { run_id, .. }) => {
                run_id.as_ref().map_or_else(
                    || vec![TranscriptKey::Thread],
                    |run_id| vec![TranscriptKey::Run(run_id.clone())],
                )
            }
            EventKind::Tool(ToolEvent::ApprovalRequested { call_id, .. })
            | EventKind::Tool(ToolEvent::ApprovalResolved { call_id, .. })
            | EventKind::Tool(ToolEvent::ExecutionDenied { call_id, .. }) => {
                self.call_index.get(call_id).map_or_else(
                    || vec![TranscriptKey::Thread],
                    |run_id| vec![TranscriptKey::Run(run_id.clone())],
                )
            }
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => vec![
                TranscriptKey::Run(message.sender_run_id.clone()),
                TranscriptKey::Run(message.recipient_run_id.clone()),
            ],
            EventKind::Lifecycle(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => Vec::new(),
        }
    }

    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            EventKind::Message(
                MessageEvent::MessageDelta { run_id: None, .. }
                | MessageEvent::ReasoningDelta { run_id: None, .. },
            ) => {
                tracing::warn!("dropped run-less stream delta");
                return;
            }
            EventKind::Message(
                MessageEvent::MessageDelta {
                    run_id: Some(_), ..
                }
                | MessageEvent::ReasoningDelta {
                    run_id: Some(_), ..
                },
            )
            | EventKind::Lifecycle(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => {}
        }

        if let EventKind::Tool(ToolEvent::ToolStarted {
            call_id,
            run_id: Some(run_id),
            ..
        }) = &event.kind
        {
            self.call_index.insert(call_id.clone(), run_id.clone());
        }

        if let EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) = &event.kind {
            self.runs
                .entry(message.sender_run_id.clone())
                .or_default()
                .push(TranscriptEntry::AgentMessage {
                    direction: MessageDirection::Outgoing,
                    peer_run_id: message.recipient_run_id.clone(),
                    kind: message.kind.clone(),
                    content: message.content.clone(),
                });
            self.runs
                .entry(message.recipient_run_id.clone())
                .or_default()
                .push(TranscriptEntry::AgentMessage {
                    direction: MessageDirection::Incoming,
                    peer_run_id: message.sender_run_id.clone(),
                    kind: message.kind.clone(),
                    content: message.content.clone(),
                });
            return;
        }

        for key in self.route(event) {
            match key {
                TranscriptKey::Thread => self.thread.apply(event),
                TranscriptKey::Run(run_id) => {
                    self.runs.entry(run_id).or_default().apply(event);
                }
            }
        }
    }

    pub fn get(&self, key: &TranscriptKey) -> Option<&TranscriptModel> {
        match key {
            TranscriptKey::Thread => Some(&self.thread),
            TranscriptKey::Run(run_id) => self.runs.get(run_id),
        }
    }

    pub fn thread(&self) -> &TranscriptModel {
        &self.thread
    }

    pub fn push_thread(&mut self, entry: TranscriptEntry) {
        self.thread.push(entry);
    }

    pub fn run(&self, run_id: &str) -> Option<&TranscriptModel> {
        self.runs.get(run_id)
    }

    pub fn run_ids(&self) -> impl Iterator<Item = &String> {
        self.runs.keys()
    }
}

// allow: SIZE_OK - issue #85 keeps routing regressions beside the existing registry tests.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transcript::{MessageDirection, ToolStatus, TranscriptEntry};
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event,
        MessageEvent, ToolEvent,
    };
    use event_bus::{AgentRunPhase, LifecycleEvent};

    #[test]
    fn run_error_routes_to_owning_thread() {
        // Given: an attributed run failure.
        let mut registry = TranscriptRegistry::new();
        let event = Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-error".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("profile=x http error 401".into()),
        });
        // When: routing and applying the event.
        let destinations = registry.route(&event);
        registry.apply(&event);
        // Then: the owning thread and run both receive the failure.
        assert_eq!(
            destinations,
            vec![
                TranscriptKey::Thread,
                TranscriptKey::Run("run-error".into())
            ]
        );
        assert_eq!(registry.thread().entries().len(), 1);
        assert_eq!(
            registry.run("run-error").expect("run transcript").entries(),
            registry.thread().entries()
        );
    }

    fn delivered(sender: &str, recipient: &str) -> Event {
        Event::new(AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "message-1".into(),
                sender_run_id: sender.into(),
                recipient_run_id: recipient.into(),
                kind: AgentMessageKind::Send,
                content: "handoff".into(),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Aside,
        })
    }

    #[test]
    fn push_thread_appends_to_thread_only() {
        // Given: a registry with no run transcripts.
        let mut registry = TranscriptRegistry::new();
        // When: a notice is appended directly to the thread.
        registry.push_thread(TranscriptEntry::Notice { text: "n".into() });
        // Then: only the thread contains the notice; no run is created.
        assert_eq!(
            registry.thread().entries(),
            &[TranscriptEntry::Notice { text: "n".into() },]
        );
        assert!(registry.runs.is_empty());
    }

    #[test]
    fn route_runless_delta_to_nowhere() {
        // Given: both stream variants have no run attribution.
        let registry = TranscriptRegistry::new();
        let events = [
            MessageEvent::MessageDelta {
                delta: "hello".into(),
                run_id: None,
            },
            MessageEvent::ReasoningDelta {
                delta: "thinking".into(),
                run_id: None,
            },
        ];

        // When: both deltas are routed.
        let routes = events.map(|event| registry.route(&Event::new(event)));

        // Then: neither has a transcript destination.
        assert_eq!(routes, [vec![], vec![]]);
    }

    #[test]
    fn route_attributed_delta_to_thread_and_run() {
        // Given: both stream variants carry an explicit run attribution.
        let registry = TranscriptRegistry::new();
        let events = [
            MessageEvent::MessageDelta {
                delta: "hello".into(),
                run_id: Some("run-2".into()),
            },
            MessageEvent::ReasoningDelta {
                delta: "thinking".into(),
                run_id: Some("run-2".into()),
            },
        ];

        // When: both deltas are routed.
        let routes = events.map(|event| registry.route(&Event::new(event)));

        // Then: each reaches the thread and its own run, in that order.
        let expected = vec![TranscriptKey::Thread, TranscriptKey::Run("run-2".into())];
        assert_eq!(routes, [expected.clone(), expected]);
    }

    #[test]
    fn route_tool_events_by_run_id_and_index_call_id() {
        let mut registry = TranscriptRegistry::new();
        let started = Event::new(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            run_id: Some("run-1".into()),
        });
        registry.apply(&started);
        let completed = Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            detail: None,
            run_id: Some("run-1".into()),
        });

        assert_eq!(
            registry.route(&completed),
            vec![TranscriptKey::Run("run-1".into())]
        );
        registry.apply(&completed);
        assert_eq!(
            registry.run("run-1").expect("run transcript").entries(),
            &[TranscriptEntry::Tool {
                tool_name: "read".into(),
                call_id: "call-1".into(),
                status: ToolStatus::Succeeded,
            }]
        );
    }

    #[test]
    fn approval_events_follow_call_index_else_thread() {
        let mut registry = TranscriptRegistry::new();
        registry.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "write".into(),
            call_id: "known".into(),
            run_id: Some("run-2".into()),
        }));
        let known = Event::new(ToolEvent::ApprovalRequested {
            tool_name: "write".into(),
            call_id: "known".into(),
        });
        let unknown = Event::new(ToolEvent::ApprovalResolved {
            call_id: "unknown".into(),
            approved: false,
        });

        assert_eq!(
            registry.route(&known),
            vec![TranscriptKey::Run("run-2".into())]
        );
        assert_eq!(registry.route(&unknown), vec![TranscriptKey::Thread]);
    }

    #[test]
    fn agent_message_appears_in_sender_and_recipient_only() {
        let mut registry = TranscriptRegistry::new();
        registry.apply(&delivered("run-1", "run-2"));

        assert_eq!(
            registry.run("run-1").expect("sender").entries(),
            &[TranscriptEntry::AgentMessage {
                direction: MessageDirection::Outgoing,
                peer_run_id: "run-2".into(),
                kind: AgentMessageKind::Send,
                content: "handoff".into(),
            }]
        );
        assert_eq!(
            registry.run("run-2").expect("recipient").entries(),
            &[TranscriptEntry::AgentMessage {
                direction: MessageDirection::Incoming,
                peer_run_id: "run-1".into(),
                kind: AgentMessageKind::Send,
                content: "handoff".into(),
            }]
        );
        assert!(registry.thread().entries().is_empty());
    }

    #[test]
    fn three_runs_never_cross_contaminate() {
        let mut registry = TranscriptRegistry::new();
        for run_id in ["run-1", "run-2", "run-3"] {
            registry.apply(&Event::new(ToolEvent::ToolStarted {
                tool_name: format!("tool-{run_id}"),
                call_id: format!("call-{run_id}"),
                run_id: Some(run_id.into()),
            }));
        }
        registry.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "thread-only".into(),
            run_id: None,
        }));

        for run_id in ["run-1", "run-2", "run-3"] {
            assert_eq!(
                registry.run(run_id).expect("run transcript").entries(),
                &[TranscriptEntry::Tool {
                    tool_name: format!("tool-{run_id}"),
                    call_id: format!("call-{run_id}"),
                    status: ToolStatus::Running,
                }]
            );
        }
        assert!(registry.thread().entries().is_empty());
    }
}
