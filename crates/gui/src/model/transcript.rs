use event_bus::{AgentMessageKind, Event};

const DEFAULT_CAPACITY: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    AwaitingApproval,
    Approved,
    Denied { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDirection {
    Incoming,
    Outgoing,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptEntry {
    Error {
        text: String,
    },
    UserMessage {
        text: String,
    },
    Notice {
        text: String,
    },
    Message {
        text: String,
    },
    Reasoning {
        text: String,
    },
    Tool {
        tool_name: String,
        call_id: String,
        status: ToolStatus,
    },
    AgentMessage {
        direction: MessageDirection,
        peer_run_id: String,
        kind: AgentMessageKind,
        content: String,
    },
}

#[derive(Debug, Clone)]
pub struct TranscriptModel {
    entries: Vec<TranscriptEntry>,
    capacity: usize,
    view_start: usize,
    view_len: usize,
}

impl Default for TranscriptModel {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Vec::new(),
            capacity,
            view_start: 0,
            view_len: capacity,
        }
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn visible_entries(&self) -> &[TranscriptEntry] {
        let start = self.view_start.min(self.entries.len());
        let end = start.saturating_add(self.view_len).min(self.entries.len());
        &self.entries[start..end]
    }

    pub fn set_view_window(&mut self, start: usize, len: usize) {
        self.view_start = start;
        self.view_len = len;
    }

    pub fn push_message(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::Message { text: text.into() });
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::UserMessage { text: text.into() });
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::Notice { text: text.into() });
    }

    pub fn push_reasoning(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::Reasoning { text: text.into() });
    }

    pub fn push_tool(
        &mut self,
        tool_name: impl Into<String>,
        call_id: impl Into<String>,
        status: ToolStatus,
    ) {
        self.push(TranscriptEntry::Tool {
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            status,
        });
    }

    pub fn apply(&mut self, event: &Event) {
        match &event.kind {
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(reason),
                    ..
                } | event_bus::LifecycleEvent::Failed { reason, .. },
            ) => self.push(TranscriptEntry::Error {
                text: format!("Run failed: {reason}"),
            }),
            event_bus::EventKind::Message(event_bus::MessageEvent::MessageDelta { delta, .. }) => {
                self.append_text(delta, false)
            }
            event_bus::EventKind::Message(event_bus::MessageEvent::ReasoningDelta { delta, .. }) => {
                self.append_text(delta, true)
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted {
                tool_name,
                call_id,
                ..
            }) => self.push_tool(tool_name, call_id, ToolStatus::Running),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error,
                ..
            }) => self.update_tool(
                call_id,
                tool_name,
                if *is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Succeeded
                },
            ),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
            }) => self.update_tool(call_id, tool_name, ToolStatus::AwaitingApproval),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ApprovalResolved {
                call_id,
                approved,
            }) => self.update_tool(
                call_id,
                "",
                if *approved {
                    ToolStatus::Approved
                } else {
                    ToolStatus::Denied {
                        reason: "approval denied".into(),
                    }
                },
            ),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ExecutionDenied {
                tool_name,
                call_id,
                reason,
            }) => self.update_tool(
                call_id,
                tool_name,
                ToolStatus::Denied {
                    reason: reason.clone(),
                },
            ),
            event_bus::EventKind::Lifecycle(_)
            | event_bus::EventKind::Usage(_)
            | event_bus::EventKind::Provider(_)
            | event_bus::EventKind::Fault(_)
            // エージェント間メッセージは transcript 表示の対象外（明示 no-op）。
            | event_bus::EventKind::AgentMessage(_)
            // コンテキスト圧縮は transcript 表示の対象外（明示 no-op）。
            | event_bus::EventKind::Compaction(_)
            // オーケストレーション状態は goal pane 表示の対象外（明示 no-op）。
            | event_bus::EventKind::Orchestrator(_) => {}
        }
    }

    fn append_text(&mut self, delta: &str, reasoning: bool) {
        let matching = self.entries.last().is_some_and(|entry| {
            matches!(
                (reasoning, entry),
                (true, TranscriptEntry::Reasoning { .. })
                    | (false, TranscriptEntry::Message { .. })
            )
        });
        if matching {
            if let Some(entry) = self.entries.last_mut() {
                match entry {
                    TranscriptEntry::Message { text } | TranscriptEntry::Reasoning { text } => {
                        text.push_str(delta)
                    }
                    TranscriptEntry::UserMessage { .. }
                    | TranscriptEntry::Error { .. }
                    | TranscriptEntry::Notice { .. }
                    | TranscriptEntry::Tool { .. }
                    | TranscriptEntry::AgentMessage { .. } => {}
                }
            }
        } else if reasoning {
            self.push_reasoning(delta)
        } else {
            self.push_message(delta)
        }
    }

    fn update_tool(&mut self, call_id: &str, tool_name: &str, status: ToolStatus) {
        if let Some(entry) = self.find_tool_mut(call_id) {
            if let TranscriptEntry::Tool {
                status: current, ..
            } = entry
            {
                *current = status;
            }
        } else {
            self.push_tool(tool_name, call_id, status);
        }
    }

    fn find_tool_mut(&mut self, call_id: &str) -> Option<&mut TranscriptEntry> {
        self.entries.iter_mut().find(
            |entry| matches!(entry, TranscriptEntry::Tool { call_id: id, .. } if id == call_id),
        )
    }

    pub(crate) fn push(&mut self, entry: TranscriptEntry) {
        if self.capacity == 0 {
            return;
        }
        self.entries.push(entry);
        if self.entries.len() > self.capacity {
            self.entries.remove(0);
        }
        self.view_start = self.view_start.min(self.entries.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{AgentRunPhase, LifecycleEvent};
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, CompactionEvent, CompactionReason,
        DeliveryDisposition, EventKind, MessageEvent, ToolEvent,
    };

    #[test]
    fn run_error_lifecycle_becomes_error_entry() {
        // Given: an empty transcript.
        let mut model = TranscriptModel::new();
        // When: a run fails with diagnostic detail.
        model.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "run-error".into(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Error,
            reason: Some("profile=x http error 401".into()),
        }));
        // Then: the failure becomes a distinct visible entry.
        assert_eq!(model.entries(), &[TranscriptEntry::Error {
            text: "Run failed: profile=x http error 401".into(),
        }]);
    }

    #[test]
    fn append_text_does_not_merge_into_error_entry() {
        // Given: an error follows earlier assistant text.
        let mut model = TranscriptModel::new();
        model.push_message("before");
        model.push(TranscriptEntry::Error { text: "Run failed: timeout".into() });
        // When: the assistant streams another delta.
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "after".into(), run_id: Some("run-error".into()),
        }));
        // Then: the error separates the two messages.
        assert_eq!(model.entries(), &[
            TranscriptEntry::Message { text: "before".into() },
            TranscriptEntry::Error { text: "Run failed: timeout".into() },
            TranscriptEntry::Message { text: "after".into() },
        ]);
    }

    #[test]
    fn message_delta_after_user_message_starts_new_entry() {
        // Given: a user message is the last entry.
        let mut model = TranscriptModel::new();
        model.push_user_message("hi");
        // When: the assistant streams a delta.
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "yo".into(),
            run_id: Some("r1".into()),
        }));
        // Then: the user message remains separate.
        assert_eq!(
            model.entries(),
            &[
                TranscriptEntry::UserMessage { text: "hi".into() },
                TranscriptEntry::Message { text: "yo".into() },
            ]
        );
    }

    #[test]
    fn message_delta_after_notice_starts_new_entry() {
        // Given: a notice is the last entry.
        let mut model = TranscriptModel::new();
        model.push_notice("n");
        // When: the assistant streams a delta.
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "yo".into(),
            run_id: Some("r1".into()),
        }));
        // Then: the notice remains separate.
        assert_eq!(
            model.entries(),
            &[
                TranscriptEntry::Notice { text: "n".into() },
                TranscriptEntry::Message { text: "yo".into() },
            ]
        );
    }

    #[test]
    fn message_deltas_coalesce_into_single_entry() {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "hel".into(),
            run_id: None,
        }));
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "lo".into(),
            run_id: None,
        }));
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Message {
                text: "hello".into()
            }]
        );
    }

    #[test]
    fn tool_lifecycle_updates_status_by_call_id() {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(ToolEvent::ToolStarted {
            tool_name: "read".into(),
            call_id: "c1".into(),
            run_id: None,
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            tool_name: "read".into(),
            call_id: "c1".into(),
            is_error: false,
            detail: None,
            run_id: None,
        }));
        assert_eq!(
            model.entries()[0],
            TranscriptEntry::Tool {
                tool_name: "read".into(),
                call_id: "c1".into(),
                status: ToolStatus::Succeeded
            }
        );
    }

    #[test]
    fn agent_message_event_is_no_op() {
        let mut model = TranscriptModel::new();
        model.push_message("before");
        let event = Event::new(EventKind::AgentMessage(AgentMessageEvent::Delivered {
            message: AgentMessage {
                message_id: "msg-1".into(),
                sender_run_id: "run-1".into(),
                recipient_run_id: "run-2".into(),
                kind: AgentMessageKind::Send,
                content: "hello".into(),
                reply_to: None,
            },
            disposition: DeliveryDisposition::Aside,
        }));
        model.apply(&event);
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Message {
                text: "before".into()
            }]
        );
    }

    #[test]
    fn compaction_event_is_no_op() {
        let mut model = TranscriptModel::new();
        model.push_message("before");
        let event = Event::new(CompactionEvent::Compacted {
            run_id: "run-1".into(),
            reason: CompactionReason::Automatic,
            threshold: 0.8,
            context_window_tokens: 200_000,
            estimated_tokens_before: 180_000,
            estimated_tokens_after: 60_000,
            compacted_range_start: 0,
            compacted_range_end: 42,
            checkpoint_id: "checkpoint-1".into(),
            summary: "圧縮要約".into(),
        });
        model.apply(&event);
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Message {
                text: "before".into()
            }]
        );
    }

    #[test]
    fn entry_cap_drops_oldest() {
        let mut model = TranscriptModel::with_capacity(2);
        model.push_message("one");
        model.push_message("two");
        model.push_message("three");
        assert_eq!(
            model.entries(),
            &[
                TranscriptEntry::Message { text: "two".into() },
                TranscriptEntry::Message {
                    text: "three".into()
                }
            ]
        );
    }
}
