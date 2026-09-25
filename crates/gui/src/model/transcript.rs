use event_bus::{AgentMessageKind, CompactionReason, Event};

mod compaction;
mod diagnostics;
mod lifecycle;
mod thinking;

#[cfg(test)]
mod tool_tests;

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
    Compaction {
        reason: CompactionReason,
        threshold: f64,
        context_window_tokens: u64,
        estimated_tokens_before: u64,
        estimated_tokens_after: u64,
        compacted_range_start: usize,
        compacted_range_end: usize,
        checkpoint_id: String,
        summary: String,
    },
    Message {
        text: String,
        run_id: Option<String>,
    },
    Reasoning {
        text: String,
        run_id: Option<String>,
    },
    Tool {
        tool_name: String,
        call_id: String,
        input: Option<serde_json::Value>,
        output: Option<String>,
        detail: Option<serde_json::Value>,
        is_error: bool,
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
    first_entry_id: usize,
    thinking: std::collections::BTreeMap<Option<String>, usize>,
    streaming_messages: std::collections::BTreeMap<Option<String>, Vec<usize>>,
    completed_messages: std::collections::BTreeMap<String, event_bus::EventMeta>,
    agent_names: std::collections::BTreeMap<String, String>,
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
            first_entry_id: 0,
            thinking: std::collections::BTreeMap::new(),
            streaming_messages: std::collections::BTreeMap::new(),
            completed_messages: std::collections::BTreeMap::new(),
            agent_names: std::collections::BTreeMap::new(),
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
        self.push(TranscriptEntry::Message {
            text: text.into(),
            run_id: None,
        });
    }

    pub fn push_user_message(&mut self, text: impl Into<String>) {
        self.streaming_messages.clear();
        self.push(TranscriptEntry::UserMessage { text: text.into() });
    }

    pub fn push_notice(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::Notice { text: text.into() });
    }

    pub fn push_reasoning(&mut self, text: impl Into<String>) {
        self.push(TranscriptEntry::Reasoning {
            text: text.into(),
            run_id: None,
        });
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
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status,
        });
    }

    pub fn apply(&mut self, event: &Event) {
        self.finish_thinking(event);
        if let Some(entry) = diagnostics::entry(&event.kind) {
            self.push(entry);
            return;
        }
        match &event.kind {
            event_bus::EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) => {
                self.push(TranscriptEntry::Notice {text:match &question.answer {
                    Some(answer)=>format!("Answer [{}]: {}",question.id,answer),
                    None=>format!("Question [{}]: {}",question.id,question.title),
                }});
            }
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStarted {
                run_id, parent_run_id: Some(_), agent_name, ..
            }) => {
                self.agent_names.insert(run_id.clone(), agent_name.clone());
                self.push_notice(format!("subagent {agent_name} ({run_id}) started"));
            }
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TaskPromptPublished {
                run_id, agent_name, prompt, ..
            }) => {
                let first_instruction = self.entries.iter().find_map(|entry| match entry {
                    TranscriptEntry::UserMessage { text } => Some(text),
                    _ => None,
                });
                if first_instruction != Some(prompt) {
                    let only_start_notice = matches!(self.entries.as_slice(),
                        [TranscriptEntry::Notice { text }]
                            if text == &format!("subagent {agent_name} ({run_id}) started"));
                    self.push_user_message(prompt.clone());
                    // The spawn notice arrives first, but the run's instruction leads its transcript.
                    if only_start_notice && self.entries.len() == 2 {
                        self.entries.swap(0, 1);
                    }
                }
            }
            event_bus::EventKind::Compaction(event) => self.push(compaction::entry(event)),
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(reason),
                    ..
                },
            ) if reason == "cancelled" => self.push(TranscriptEntry::Notice {
                text: "Run cancelled".into(),
            }),
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(reason),
                    ..
                } | event_bus::LifecycleEvent::Failed { reason, .. },
            ) => self.push(TranscriptEntry::Error {
                text: format!("Run failed: {reason}"),
            }),
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id, to: event_bus::AgentRunPhase::Done | event_bus::AgentRunPhase::Error, ..
            }) if self.agent_names.contains_key(run_id) => {
                if let Some(entry) = self.terminal_notice(event) {
                    self.push(entry);
                }
            }
            event_bus::EventKind::Message(event_bus::MessageEvent::MessageCompleted { run_id, text }) => {
                self.complete_message(run_id, text, &event.meta);
            }
            event_bus::EventKind::Message(message) => self.append_text(message),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted {
                tool_name,
                call_id,
                input,
                run_id,
            }) => {
                // Older ledgers have no completion events. Tool execution still
                // closes the preceding response before the next answer streams.
                self.streaming_messages.remove(run_id);
                self.push(TranscriptEntry::Tool {
                    tool_name: tool_name.clone(),
                    call_id: call_id.clone(),
                    input: input.clone(),
                    output: None,
                    detail: None,
                    is_error: false,
                    status: ToolStatus::Running,
                });
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error,
                output,
                detail,
                ..
            }) => {
                self.update_tool(
                call_id,
                tool_name,
                if *is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Succeeded
                },
                );
                if let Some(TranscriptEntry::Tool {
                    output: current_output,
                    detail: current_detail,
                    is_error: current_is_error,
                    ..
                }) = self.find_tool_mut(call_id) {
                    current_output.clone_from(output);
                    current_detail.clone_from(detail);
                    *current_is_error = *is_error;
                }
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
                ..
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
            | event_bus::EventKind::Ledger(_)
            | event_bus::EventKind::Usage(_)
            | event_bus::EventKind::Provider(_)
            | event_bus::EventKind::Fault(_)
            // エージェント間メッセージは transcript 表示の対象外（明示 no-op）。
            | event_bus::EventKind::AgentMessage(_)
            // オーケストレーション状態は goal pane 表示の対象外（明示 no-op）。
            | event_bus::EventKind::Orchestrator(_)
            | event_bus::EventKind::Diagnostic(_)
            | event_bus::EventKind::Ownership(_)
            | event_bus::EventKind::Snapshot(_) => {}
        }
    }

    fn append_text(&mut self, message: &event_bus::MessageEvent) {
        use event_bus::MessageEvent;
        let (delta, run_id) = match message {
            MessageEvent::MessageDelta { delta, run_id }
            | MessageEvent::ReasoningDelta { delta, run_id } => (delta, run_id),
            MessageEvent::MessageCompleted { .. } => return,
            MessageEvent::FinalResultPublished { text, run_id } => {
                self.streaming_messages.remove(&Some(run_id.clone()));
                self.thinking.remove(&Some(run_id.clone()));
                let last_message = self.entries.iter().rev().find_map(|entry| match entry {
                    TranscriptEntry::Message {
                        text,
                        run_id: Some(id),
                    } if id == run_id => Some(text),
                    _ => None,
                });
                if !text.is_empty() && last_message != Some(text) {
                    // A finish result is a complete message, not another stream chunk.
                    self.push(TranscriptEntry::Message {
                        text: text.clone(),
                        run_id: Some(run_id.clone()),
                    });
                }
                return;
            }
        };
        if delta.is_empty() {
            return;
        }
        if matches!(message, MessageEvent::MessageDelta { .. }) {
            self.thinking.remove(run_id);
        }
        let matching = self.entries.last().is_some_and(|entry| {
            let open_message = !matches!(message, MessageEvent::MessageDelta { .. })
                || self.streaming_messages.get(run_id).is_some_and(|ids| {
                    ids.last() == Some(&(self.first_entry_id + self.entries.len() - 1))
                });
            open_message && matches!(
                (message, entry),
                (MessageEvent::ReasoningDelta { .. }, TranscriptEntry::Reasoning { run_id: previous, .. })
                    | (MessageEvent::MessageDelta { .. }, TranscriptEntry::Message { run_id: previous, .. })
                    if previous == run_id
            )
        });
        if matching {
            if let Some(entry) = self.entries.last_mut() {
                match entry {
                    TranscriptEntry::Message { text, .. }
                    | TranscriptEntry::Reasoning { text, .. } => text.push_str(delta),
                    TranscriptEntry::UserMessage { .. }
                    | TranscriptEntry::Error { .. }
                    | TranscriptEntry::Notice { .. }
                    | TranscriptEntry::Compaction { .. }
                    | TranscriptEntry::Tool { .. }
                    | TranscriptEntry::AgentMessage { .. } => {}
                }
            }
        } else {
            self.push(match message {
                MessageEvent::MessageDelta { .. }
                | MessageEvent::FinalResultPublished { .. }
                | MessageEvent::MessageCompleted { .. } => TranscriptEntry::Message {
                    text: delta.clone(),
                    run_id: run_id.clone(),
                },
                MessageEvent::ReasoningDelta { .. } => TranscriptEntry::Reasoning {
                    text: delta.clone(),
                    run_id: run_id.clone(),
                },
            });
        }
        if matches!(message, MessageEvent::MessageDelta { .. }) && !self.entries.is_empty() {
            let id = self.first_entry_id + self.entries.len() - 1;
            let ids = self.streaming_messages.entry(run_id.clone()).or_default();
            if ids.last() != Some(&id) {
                ids.push(id);
            }
        }
        if matches!(message, MessageEvent::ReasoningDelta { .. }) && !self.entries.is_empty() {
            self.thinking
                .insert(run_id.clone(), self.first_entry_id + self.entries.len() - 1);
        }
    }

    fn complete_message(&mut self, run_id: &str, text: &str, meta: &event_bus::EventMeta) {
        if self.completed_messages.get(run_id) == Some(meta) {
            return;
        }
        self.completed_messages.insert(run_id.into(), meta.clone());
        let key = Some(run_id.to_owned());
        self.thinking.remove(&key);
        let ids = self.streaming_messages.remove(&key).unwrap_or_default();
        let mut indices = ids.into_iter().filter_map(|id| {
            let index = id.checked_sub(self.first_entry_id)?;
            matches!(self.entries.get(index), Some(TranscriptEntry::Message { run_id, .. }) if run_id == &key)
                .then_some(index)
        }).collect::<Vec<_>>();
        if !text.is_empty() {
            if let Some(&index) = indices.first() {
                if let TranscriptEntry::Message { text: current, .. } = &mut self.entries[index] {
                    text.clone_into(current);
                }
                indices.remove(0);
            } else {
                self.push(TranscriptEntry::Message {
                    text: text.into(),
                    run_id: key,
                });
            }
        }
        // Reasoning or diagnostics may split one response into several display
        // entries. Keep one canonical answer, retaining the intervening entries.
        for index in indices.into_iter().rev() {
            self.remove_entry(index);
        }
    }

    fn remove_entry(&mut self, index: usize) {
        self.entries.remove(index);
        let removed_id = self.first_entry_id + index;
        self.thinking.retain(|_, id| *id != removed_id);
        for id in self.thinking.values_mut() {
            if *id > removed_id {
                *id -= 1;
            }
        }
        for ids in self.streaming_messages.values_mut() {
            ids.retain(|id| *id != removed_id);
            for id in ids {
                if *id > removed_id {
                    *id -= 1;
                }
            }
        }
        if self.view_start > index {
            self.view_start -= 1;
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
            self.first_entry_id += 1;
            self.thinking.retain(|_, id| *id >= self.first_entry_id);
            self.streaming_messages.retain(|_, ids| {
                ids.retain(|id| *id >= self.first_entry_id);
                !ids.is_empty()
            });
        }
        self.view_start = self.view_start.min(self.entries.len());
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn cancelled_reason_becomes_notice_not_error() {
        let mut model = super::TranscriptModel::default();
        model.apply(&event_bus::Event::new(
            event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id: "chat-1".into(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Error,
                reason: Some("cancelled".into()),
            },
        ));
        assert!(
            matches!(model.entries(), [super::TranscriptEntry::Notice { text }] if text == "Run cancelled")
        );
    }
    use super::*;
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, CompactionEvent, CompactionReason,
        DeliveryDisposition, EventKind, MessageEvent, ToolEvent,
    };
    use event_bus::{AgentRunPhase, LifecycleEvent};

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
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Error {
                text: "Run failed: profile=x http error 401".into(),
            }]
        );
    }

    #[test]
    fn append_text_does_not_merge_into_error_entry() {
        // Given: an error follows earlier assistant text.
        let mut model = TranscriptModel::new();
        model.push_message("before");
        model.push(TranscriptEntry::Error {
            text: "Run failed: timeout".into(),
        });
        // When: the assistant streams another delta.
        model.apply(&Event::new(MessageEvent::MessageDelta {
            delta: "after".into(),
            run_id: Some("run-error".into()),
        }));
        // Then: the error separates the two messages.
        assert_eq!(
            model.entries(),
            &[
                TranscriptEntry::Message {
                    text: "before".into(),
                    run_id: None,
                },
                TranscriptEntry::Error {
                    text: "Run failed: timeout".into()
                },
                TranscriptEntry::Message {
                    text: "after".into(),
                    run_id: Some("run-error".into()),
                },
            ]
        );
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
                TranscriptEntry::Message {
                    text: "yo".into(),
                    run_id: Some("r1".into())
                },
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
                TranscriptEntry::Message {
                    text: "yo".into(),
                    run_id: Some("r1".into())
                },
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
                text: "hello".into(),
                run_id: None,
            }]
        );
    }

    #[test]
    fn tool_lifecycle_updates_status_by_call_id() {
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(ToolEvent::ToolStarted {
            input: None,
            tool_name: "read".into(),
            call_id: "c1".into(),
            run_id: None,
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            output: None,
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
                input: None,
                output: None,
                detail: None,
                is_error: false,
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
                text: "before".into(),
                run_id: None,
            }]
        );
    }

    #[test]
    fn compaction_event_preserves_display_fields() {
        let mut model = TranscriptModel::new();
        model.push_message("before");
        let event = Event::new(CompactionEvent::Compacted {
            run_id: "run-1".into(),
            reason: CompactionReason::Automatic,
            threshold: 0.8,
            context_window_tokens: 200_000,
            window_source: event_bus::WindowSource::Default,
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
            &[
                TranscriptEntry::Message {
                    text: "before".into(),
                    run_id: None,
                },
                TranscriptEntry::Compaction {
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 200_000,
                    estimated_tokens_before: 180_000,
                    estimated_tokens_after: 60_000,
                    compacted_range_start: 0,
                    compacted_range_end: 42,
                    checkpoint_id: "checkpoint-1".into(),
                    summary: "圧縮要約".into(),
                },
            ]
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
                TranscriptEntry::Message {
                    text: "two".into(),
                    run_id: None
                },
                TranscriptEntry::Message {
                    text: "three".into(),
                    run_id: None,
                }
            ]
        );
    }
}
