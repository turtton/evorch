//! run ID ごとに transcript を分離し、相関可能なイベントだけを決定的に配送する。
//!
//! `MessageDelta` / `ReasoningDelta` は event-bus 上に `run_id` を持たないため、通常の
//! route では thread transcript のみに残る。呼び出し側で Running の run が厳密に 1 件
//! の場合だけ、明示的に run transcript へ同じ delta を適用できる。Running が 0 件または
//! 複数なら thread のみに留め、推測による run 間の混線を防ぐ。

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
            EventKind::Message(MessageEvent::MessageDelta { .. })
            | EventKind::Message(MessageEvent::ReasoningDelta { .. }) => {
                vec![TranscriptKey::Thread]
            }
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
            | EventKind::Fault(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => Vec::new(),
        }
    }

    pub fn apply(&mut self, event: &Event) {
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

    /// `run_id` を持たない stream delta を、既知の対象 run に明示的に適用する。
    pub fn apply_stream_delta(&mut self, run_id: &str, event: &Event) {
        match &event.kind {
            EventKind::Message(MessageEvent::MessageDelta { .. })
            | EventKind::Message(MessageEvent::ReasoningDelta { .. }) => {
                self.runs.entry(run_id.to_owned()).or_default().apply(event);
            }
            EventKind::Lifecycle(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_) => {}
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

    pub fn run(&self, run_id: &str) -> Option<&TranscriptModel> {
        self.runs.get(run_id)
    }

    pub fn run_ids(&self) -> impl Iterator<Item = &String> {
        self.runs.keys()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transcript::{MessageDirection, ToolStatus, TranscriptEntry};
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event,
        MessageEvent, ToolEvent,
    };

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
    fn route_message_delta_to_thread_only() {
        let registry = TranscriptRegistry::new();
        let event = Event::new(MessageEvent::MessageDelta {
            delta: "hello".into(),
        });

        assert_eq!(registry.route(&event), vec![TranscriptKey::Thread]);
    }

    #[test]
    fn apply_stream_delta_creates_and_updates_target_run() {
        // Given: no transcript model exists for the target run.
        let mut registry = TranscriptRegistry::new();
        let event = Event::new(MessageEvent::ReasoningDelta {
            delta: "considering".into(),
        });

        // When: the stream delta is explicitly applied to that run.
        registry.apply_stream_delta("run-1", &event);

        // Then: the target model is created with the reasoning entry only.
        assert_eq!(
            registry.run("run-1").expect("run transcript").entries(),
            &[TranscriptEntry::Reasoning {
                text: "considering".into(),
            }]
        );
        assert!(registry.thread().entries().is_empty());
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
        assert_eq!(
            registry.thread().entries(),
            &[TranscriptEntry::Message {
                text: "thread-only".into(),
            }]
        );
    }
}
