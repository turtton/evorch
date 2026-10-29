use event_bus::{AgentMessageKind, CompactionReason, Event};

use super::scoped_call::parse_scoped_call_id;

mod branch;
mod compaction;
mod diagnostics;
mod drafts;
mod lifecycle;
mod reviews;
pub mod shell_jobs;
mod thinking;

pub use reviews::ToolReview;

#[cfg(test)]
mod draft_tests;
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
    Denied {
        reason: String,
    },
    /// The run ended before the call reported a result, for example because
    /// the process exited mid-call. A late completion still replaces it.
    Interrupted,
    /// The model is still composing the call; `arguments` is the raw text
    /// streamed so far. `ToolStarted` turns the card into a running call.
    Drafting {
        /// Position of the call within the streamed response.
        index: usize,
        arguments: String,
        /// The response finished, so the call is waiting for its start event.
        sealed: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageDirection {
    Incoming,
    Outgoing,
}

/// A transcript line `prefix` + `reason`. Runtime reasons are Japanese, so the
/// line shows the English form and the original reason moves to the hover;
/// a reason that needs no translation stays a plain notice or error.
pub(crate) fn reason_entry(prefix: &str, reason: &str, error: bool) -> TranscriptEntry {
    let shown = crate::model::failure_text::english(reason);
    let text = format!("{prefix}{shown}");
    if shown == reason {
        return if error {
            TranscriptEntry::Error { text }
        } else {
            TranscriptEntry::Notice { text }
        };
    }
    TranscriptEntry::Diagnostic {
        severity: if error {
            event_bus::DiagnosticSeverity::Error
        } else {
            event_bus::DiagnosticSeverity::Warning
        },
        text,
        hover: reason.to_owned(),
    }
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
    /// A runtime diagnostic: `text` is the short user-facing line, `hover`
    /// carries severity, source, code and the raw detail.
    Diagnostic {
        severity: event_bus::DiagnosticSeverity,
        text: String,
        hover: String,
    },
    SandboxReview {
        text: String,
        run_id: Option<String>,
        call_id: Option<String>,
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
        run_id: Option<String>,
        tool_name: String,
        call_id: String,
        input: Option<serde_json::Value>,
        output: Option<String>,
        detail: Option<serde_json::Value>,
        is_error: bool,
        status: ToolStatus,
        reviews: Vec<ToolReview>,
        /// The `run_js` call that made this call from its code (ADR 0039).
        /// Such a child renders inside its parent's card when that is shown.
        parent_call_id: Option<String>,
    },
    AgentMessage {
        direction: MessageDirection,
        peer_run_id: String,
        kind: AgentMessageKind,
        content: String,
    },
    /// Artifacts the conversation owner showed to the user.
    Artifacts {
        presentation: event_bus::ArtifactPresentation,
    },
    /// A completed turn of `run_id`; a fork or rewind boundary, not visible text.
    TurnEnd {
        run_id: String,
        context_len: u64,
    },
    /// Where a forked or rewound conversation leaves its inherited history.
    Branch {
        kind: workspace_ui::LineageKind,
        source_thread_id: String,
    },
}

#[derive(Debug, Clone)]
pub struct TranscriptModel {
    instance_id: usize,
    entries: Vec<TranscriptEntry>,
    entry_meta: Vec<EntryMeta>,
    next_entry_id: usize,
    history_evicted: bool,
    capacity: usize,
    view_start: usize,
    view_len: usize,
    host_inputs: std::collections::BTreeMap<String, usize>,
    thinking: std::collections::BTreeMap<Option<String>, usize>,
    streaming_messages: std::collections::BTreeMap<Option<String>, Vec<usize>>,
    completed_messages: std::collections::BTreeMap<String, event_bus::EventMeta>,
    agent_names: std::collections::BTreeMap<String, String>,
    shell_jobs: shell_jobs::ShellJobs,
    pending_reviews: std::collections::BTreeMap<(Option<String>, String), Vec<ToolReview>>,
    /// Approval IDs that interrupted an already running call.
    approvals_while_running: std::collections::BTreeSet<String>,
    drafts: drafts::DraftIndex,
}

#[derive(Debug, Clone)]
struct EntryMeta {
    id: usize,
    revision: u64,
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
        static NEXT_INSTANCE: std::sync::atomic::AtomicUsize =
            std::sync::atomic::AtomicUsize::new(0);
        Self {
            instance_id: NEXT_INSTANCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            entries: Vec::new(),
            entry_meta: Vec::new(),
            next_entry_id: 0,
            history_evicted: false,
            capacity,
            view_start: 0,
            view_len: capacity,
            host_inputs: Default::default(),
            thinking: std::collections::BTreeMap::new(),
            streaming_messages: std::collections::BTreeMap::new(),
            completed_messages: std::collections::BTreeMap::new(),
            agent_names: std::collections::BTreeMap::new(),
            shell_jobs: shell_jobs::ShellJobs::default(),
            pending_reviews: std::collections::BTreeMap::new(),
            approvals_while_running: std::collections::BTreeSet::new(),
            drafts: drafts::DraftIndex::default(),
        }
    }

    pub fn shell_jobs(&self) -> &shell_jobs::ShellJobs {
        &self.shell_jobs
    }

    pub(crate) fn cache_identity(&self) -> usize {
        self.instance_id
    }

    pub fn entries(&self) -> &[TranscriptEntry] {
        &self.entries
    }

    pub fn visible_entries(&self) -> &[TranscriptEntry] {
        let start = self.view_start.min(self.entries.len());
        let end = start.saturating_add(self.view_len).min(self.entries.len());
        &self.entries[start..end]
    }

    /// IDs are allocated once and never reused after removal or eviction.
    pub fn visible_entry_ids(&self) -> impl Iterator<Item = usize> + '_ {
        let start = self.view_start.min(self.entries.len());
        self.entry_meta[start..]
            .iter()
            .take(self.view_len)
            .map(|meta| meta.id)
    }

    /// Changes whenever the entry's displayed content is mutated.
    pub fn entry_revision(&self, entry_id: usize) -> Option<u64> {
        Some(self.entry_meta[self.entry_index(entry_id)?].revision)
    }

    fn entry_index(&self, entry_id: usize) -> Option<usize> {
        self.entry_meta
            .binary_search_by_key(&entry_id, |meta| meta.id)
            .ok()
    }

    fn entry_mut(&mut self, index: usize) -> Option<&mut TranscriptEntry> {
        self.entry_meta.get_mut(index)?.revision += 1;
        self.entries.get_mut(index)
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
            run_id: None,
            tool_name: tool_name.into(),
            call_id: call_id.into(),
            input: None,
            output: None,
            detail: None,
            is_error: false,
            status,
            reviews: Vec::new(),
            parent_call_id: None,
        });
    }

    pub fn apply(&mut self, event: &Event) {
        self.finish_thinking(event);
        self.finish_drafts(event);
        if let event_bus::EventKind::Diagnostic(diagnostic) = &event.kind
            && self.apply_review(diagnostic)
        {
            return;
        }
        if let Some(entry) = diagnostics::entry(&event.kind) {
            self.push(entry);
            return;
        }
        match &event.kind {
            event_bus::EventKind::Tool(event_bus::ToolEvent::UserQuestionUpdated { question }) => {
                self.push(TranscriptEntry::Notice {text:match &question.answer {
                    Some(answer)=>match &question.answer_origin {
                        Some(event_bus::AnswerOrigin::Host)=>format!("Answer from you: {answer}"),
                        Some(event_bus::AnswerOrigin::ParentAgent{..})=>format!("Answer from parent agent: {answer}"),
                        None=>format!("Answer: {answer}"),
                    },
                    None=>format!("Question: {}",question.title),
                }});
            }
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStarted {
                run_id, parent_run_id: Some(_), agent_name, ..
            }) => {
                self.agent_names.insert(run_id.clone(), agent_name.clone());
                // The delegate card that spawned it already announces the subagent.
                if !self.delegate_in_flight() {
                    self.push_notice(format!("subagent {agent_name} started"));
                }
            }
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TaskPromptPublished {
                agent_name, prompt, ..
            }) => {
                let first_instruction = self.entries.iter().find_map(|entry| match entry {
                    TranscriptEntry::UserMessage { text } => Some(text),
                    _ => None,
                });
                if first_instruction != Some(prompt) {
                    let only_start_notice = matches!(self.entries.as_slice(),
                        [TranscriptEntry::Notice { text }]
                            if text == &format!("subagent {agent_name} started"));
                    self.push_user_message(prompt.clone());
                    // The spawn notice arrives first, but the run's instruction leads its transcript.
                    if only_start_notice && self.entries.len() == 2 {
                        self.entries.swap(0, 1);
                    }
                }
            }
            event_bus::EventKind::Compaction(event) => self.push(compaction::entry(event)),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ArtifactsPresented {
                presentation, ..
            }) => self.push(TranscriptEntry::Artifacts {
                presentation: presentation.clone(),
            }),
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted {
                run_id, context_len,
            }) => self.push(TranscriptEntry::TurnEnd {
                run_id: run_id.clone(),
                context_len: *context_len,
            }),
            // A stop is visible in the phase indicator and the Stop controls; no notice.
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: event_bus::AgentRunPhase::Stopped, ..
            }) => {}
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(reason),
                    ..
                },
            ) if reason == event_bus::CANCELLED_REASON => self.push(TranscriptEntry::Notice {
                text: "Run cancelled".into(),
            }),
            event_bus::EventKind::Lifecycle(
                event_bus::LifecycleEvent::AgentRunStateChanged {
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(reason),
                    ..
                } | event_bus::LifecycleEvent::Failed { reason, .. },
            ) => self.push(reason_entry("Run failed: ", reason, true)),
            event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id, to: event_bus::AgentRunPhase::Done | event_bus::AgentRunPhase::Error, ..
            }) if self.agent_names.contains_key(run_id) => {
                if let Some(entry) = self.terminal_notice(event) {
                    self.push(entry);
                }
            }
            event_bus::EventKind::Message(event_bus::MessageEvent::MessageCompleted { run_id, text }) => {
                self.complete_message(run_id, text, &event.meta);
                self.response_completed(run_id);
            }
            event_bus::EventKind::Message(event_bus::MessageEvent::ToolCallDelta {
                run_id,
                index,
                call_id,
                tool_name,
                arguments_delta,
            }) => self.apply_tool_call_delta(&drafts::ToolCallFragment {
                run_id: run_id.as_deref(),
                index: *index,
                call_id: call_id.as_deref(),
                tool_name: tool_name.as_deref(),
                arguments_delta,
            }),
            event_bus::EventKind::Message(event_bus::MessageEvent::StreamRetried { run_id }) => {
                self.discard_drafts(run_id.as_deref());
            }
            event_bus::EventKind::Message(message) => {
                if let event_bus::MessageEvent::MessageDelta { run_id, .. }
                | event_bus::MessageEvent::ReasoningDelta { run_id, .. } = message
                {
                    // A new response began, so earlier calls it did not start are stale.
                    self.discard_sealed_drafts(run_id.as_deref());
                }
                self.append_text(message);
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted {
                tool_name,
                call_id,
                input,
                run_id,
                parent_call_id,
            }) => {
                // Older ledgers have no completion events. Tool execution still
                // closes the preceding response before the next answer streams.
                self.streaming_messages.remove(run_id);
                self.start_tool(
                    run_id.as_deref(),
                    tool_name,
                    call_id,
                    input.as_ref(),
                    parent_call_id.as_deref(),
                );
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ToolCompleted {
                tool_name,
                call_id,
                is_error,
                output,
                detail,
                run_id,
                parent_call_id,
            }) => {
                self.update_scoped_tool(
                call_id,
                run_id.as_deref(),
                tool_name,
                if *is_error {
                    ToolStatus::Failed
                } else {
                    ToolStatus::Succeeded
                },
                );
                let mut input = None;
                if let Some(TranscriptEntry::Tool {
                    run_id: current_run,
                    input: current_input,
                    output: current_output,
                    detail: current_detail,
                    is_error: current_is_error,
                    parent_call_id: current_parent,
                    ..
                }) = self.find_scoped_tool_mut(call_id, run_id.as_deref()) {
                    current_run.clone_from(run_id);
                    current_output.clone_from(output);
                    current_detail.clone_from(detail);
                    *current_is_error = *is_error;
                    if parent_call_id.is_some() {
                        current_parent.clone_from(parent_call_id);
                    }
                    input = current_input.clone();
                }
                if matches!(tool_name.as_str(), "shell" | "bash") {
                    // Legacy details lack ownership; the event envelope still has it.
                    let mut scoped_detail = detail.clone();
                    if let Some(state) = scoped_detail.as_mut().and_then(|detail| detail.get_mut("shell_job")) {
                        state["run_id"] = serde_json::json!(run_id);
                    }
                    self.shell_jobs
                        .apply_result(input.as_ref(), output.as_deref(), scoped_detail.as_ref());
                }
            }
            event_bus::EventKind::Tool(event_bus::ToolEvent::ShellJobOutput {
                job_id,
                job_uid,
                call_id,
                run_id,
                offset,
                chunk,
                status,
                exit_code,
            }) => self.shell_jobs.apply_live(
                shell_jobs::ShellJobKey::new(run_id.as_deref(), job_id, job_uid.as_deref()),
                call_id.as_deref(),
                *offset,
                chunk,
                status,
                *exit_code,
            ),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ApprovalRequested {
                tool_name,
                call_id,
                input,
            }) => self.update_tool(call_id, tool_name, input.as_ref(), ToolStatus::AwaitingApproval),
            event_bus::EventKind::Tool(event_bus::ToolEvent::ApprovalResolved {
                call_id,
                approved,
            }) => self.update_tool(
                call_id,
                "",
                None,
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
                None,
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
            | event_bus::EventKind::Ownership(_) => {}
        }
    }

    fn append_text(&mut self, message: &event_bus::MessageEvent) {
        use event_bus::MessageEvent;
        let (delta, run_id) = match message {
            MessageEvent::MessageDelta { delta, run_id }
            | MessageEvent::ReasoningDelta { delta, run_id } => (delta, run_id),
            MessageEvent::MessageCompleted { .. }
            | MessageEvent::ToolCallDelta { .. }
            | MessageEvent::StreamRetried { .. } => return,
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
                    ids.last() == self.entry_meta.last().map(|meta| &meta.id)
                });
            open_message && matches!(
                (message, entry),
                (MessageEvent::ReasoningDelta { .. }, TranscriptEntry::Reasoning { run_id: previous, .. })
                    | (MessageEvent::MessageDelta { .. }, TranscriptEntry::Message { run_id: previous, .. })
                    if previous == run_id
            )
        });
        if matching {
            if let Some(entry) = self.entry_mut(self.entries.len() - 1) {
                match entry {
                    TranscriptEntry::Message { text, .. }
                    | TranscriptEntry::Reasoning { text, .. } => text.push_str(delta),
                    TranscriptEntry::UserMessage { .. }
                    | TranscriptEntry::Error { .. }
                    | TranscriptEntry::Notice { .. }
                    | TranscriptEntry::Diagnostic { .. }
                    | TranscriptEntry::SandboxReview { .. }
                    | TranscriptEntry::Compaction { .. }
                    | TranscriptEntry::Tool { .. }
                    | TranscriptEntry::AgentMessage { .. }
                    | TranscriptEntry::TurnEnd { .. }
                    | TranscriptEntry::Artifacts { .. }
                    | TranscriptEntry::Branch { .. } => {}
                }
            }
        } else {
            self.push(match message {
                MessageEvent::MessageDelta { .. }
                | MessageEvent::FinalResultPublished { .. }
                | MessageEvent::MessageCompleted { .. }
                | MessageEvent::ToolCallDelta { .. }
                | MessageEvent::StreamRetried { .. } => TranscriptEntry::Message {
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
            let id = self.entry_meta.last().expect("entry exists").id;
            let ids = self.streaming_messages.entry(run_id.clone()).or_default();
            if ids.last() != Some(&id) {
                ids.push(id);
            }
        }
        if matches!(message, MessageEvent::ReasoningDelta { .. }) && !self.entries.is_empty() {
            self.thinking.insert(
                run_id.clone(),
                self.entry_meta.last().expect("entry exists").id,
            );
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
            let index = self.entry_index(id)?;
            matches!(self.entries.get(index), Some(TranscriptEntry::Message { run_id, .. }) if run_id == &key)
                .then_some(index)
        }).collect::<Vec<_>>();
        if !text.is_empty() {
            if let Some(&index) = indices.first() {
                if let Some(TranscriptEntry::Message { text: current, .. }) = self.entry_mut(index)
                {
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
        let removed_id = self.entry_meta.remove(index).id;
        self.host_inputs.retain(|_, id| *id != removed_id);
        self.drafts.entry_removed(removed_id);
        self.thinking.retain(|_, id| *id != removed_id);
        for ids in self.streaming_messages.values_mut() {
            ids.retain(|id| *id != removed_id);
        }
        if self.view_start > index {
            self.view_start -= 1;
        }
    }

    /// Starts a call, continuing the card an earlier approval request opened.
    fn start_tool(
        &mut self,
        run_id: Option<&str>,
        tool_name: &str,
        call_id: &str,
        input: Option<&serde_json::Value>,
        parent_call_id: Option<&str>,
    ) {
        // A run_js child is never streamed by the model, so it has no draft.
        if parent_call_id.is_none() && self.claim_draft(run_id, tool_name, call_id, input) {
            return;
        }
        let pending = self.take_pending_reviews(run_id, call_id);
        if let Some(TranscriptEntry::Tool {
            tool_name: name,
            input: current_input,
            output: None,
            status: status @ (ToolStatus::AwaitingApproval | ToolStatus::Approved),
            reviews,
            parent_call_id: current_parent,
            ..
        }) = self.find_scoped_tool_mut(call_id, run_id)
        {
            tool_name.clone_into(name);
            if input.is_some() {
                *current_input = input.cloned();
            }
            if parent_call_id.is_some() {
                *current_parent = parent_call_id.map(str::to_owned);
            }
            *status = ToolStatus::Running;
            reviews.extend(pending);
            return;
        }
        self.push(TranscriptEntry::Tool {
            run_id: run_id.map(str::to_owned),
            tool_name: tool_name.to_owned(),
            call_id: call_id.to_owned(),
            input: input.cloned(),
            output: None,
            detail: None,
            is_error: false,
            status: ToolStatus::Running,
            reviews: pending,
            parent_call_id: parent_call_id.map(str::to_owned),
        });
    }

    /// Applies an approval status. A run-scoped approval ID (`run:call[:attempt]`)
    /// resolves to that run's original call, so one call keeps one card.
    fn update_tool(
        &mut self,
        approval_id: &str,
        tool_name: &str,
        input: Option<&serde_json::Value>,
        status: ToolStatus,
    ) {
        let (run_id, call_id) = match parse_scoped_call_id(approval_id) {
            Some((run, call, _)) => (Some(run), call),
            None => (None, approval_id.to_owned()),
        };
        let scoped = run_id
            .as_deref()
            .and_then(|run| self.scoped_tool_index(&call_id, Some(run)));
        let index = scoped.or_else(|| {
            self.entries.iter().position(
                |entry| matches!(entry, TranscriptEntry::Tool { call_id: id, .. } if id == approval_id),
            )
        });
        let Some(index) = index else {
            let reviews = self.take_pending_reviews(run_id.as_deref(), &call_id);
            self.push(TranscriptEntry::Tool {
                run_id,
                tool_name: tool_name.to_owned(),
                call_id,
                input: input.cloned(),
                output: None,
                detail: None,
                is_error: false,
                status,
                reviews,
                parent_call_id: None,
            });
            return;
        };
        self.entry_meta[index].revision += 1;
        let TranscriptEntry::Tool {
            status: current, ..
        } = &mut self.entries[index]
        else {
            unreachable!("index points at a tool entry");
        };
        *current = match status {
            ToolStatus::AwaitingApproval => {
                if *current == ToolStatus::Running {
                    self.approvals_while_running.insert(approval_id.to_owned());
                }
                status
            }
            // An approval during execution lets the running call continue.
            ToolStatus::Approved if self.approvals_while_running.remove(approval_id) => {
                ToolStatus::Running
            }
            status => {
                self.approvals_while_running.remove(approval_id);
                status
            }
        };
    }

    fn update_scoped_tool(
        &mut self,
        call_id: &str,
        run_id: Option<&str>,
        tool_name: &str,
        status: ToolStatus,
    ) {
        if let Some(TranscriptEntry::Tool {
            status: current, ..
        }) = self.find_scoped_tool_mut(call_id, run_id)
        {
            *current = status;
        } else {
            self.push(TranscriptEntry::Tool {
                run_id: run_id.map(str::to_owned),
                tool_name: tool_name.to_owned(),
                call_id: call_id.to_owned(),
                input: None,
                output: None,
                detail: None,
                is_error: false,
                status,
                reviews: Vec::new(),
                parent_call_id: None,
            });
        }
    }

    fn scoped_tool_index(&self, call_id: &str, run: Option<&str>) -> Option<usize> {
        self.entries.iter().rposition(|entry| matches!(entry,
            TranscriptEntry::Tool { call_id: id, run_id, .. } if id == call_id && run_id.as_deref() == run
        ))
    }

    fn find_scoped_tool_mut(
        &mut self,
        call_id: &str,
        run: Option<&str>,
    ) -> Option<&mut TranscriptEntry> {
        let index = self.scoped_tool_index(call_id, run)?;
        self.entry_mut(index)
    }

    pub(crate) fn push_host_input(&mut self, id: &str, text: String) {
        if self.capacity == 0 || (!id.is_empty() && self.host_inputs.contains_key(id)) {
            return;
        }
        let entry_id = self.next_entry_id;
        self.push(TranscriptEntry::UserMessage { text });
        if !id.is_empty() {
            self.host_inputs.insert(id.into(), entry_id);
        }
    }

    pub(crate) fn remove_host_input(&mut self, id: &str) {
        if let Some(entry) = self.host_inputs.get(id).copied()
            && let Some(index) = self.entry_index(entry)
            && matches!(
                self.entries.get(index),
                Some(TranscriptEntry::UserMessage { .. })
            )
        {
            self.remove_entry(index);
        }
    }

    pub(crate) fn push(&mut self, entry: TranscriptEntry) {
        if self.capacity == 0 {
            return;
        }
        self.entries.push(entry);
        self.entry_meta.push(EntryMeta {
            id: self.next_entry_id,
            revision: 0,
        });
        self.next_entry_id += 1;
        if self.entries.len() > self.capacity {
            self.remove_entry(0);
            self.history_evicted = true;
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

    // Given: a run that fails with the Japanese runtime Display (the same text the model reads)
    // When: the transcript shows it / Then: English line, original reason in the hover
    #[test]
    fn japanese_failure_reason_shows_in_english_with_the_original_in_the_hover() {
        let reason = "モデル呼び出しに失敗しました: HTTP 500";
        let mut model = TranscriptModel::new();
        model.apply(&Event::new(LifecycleEvent::Failed {
            session_id: "run-error".into(),
            reason: reason.into(),
        }));
        assert_eq!(
            model.entries(),
            &[TranscriptEntry::Diagnostic {
                severity: event_bus::DiagnosticSeverity::Error,
                text: "Run failed: Model call failed: HTTP 500".into(),
                hover: reason.into(),
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
            parent_call_id: None,
        }));
        model.apply(&Event::new(ToolEvent::ToolCompleted {
            output: None,
            tool_name: "read".into(),
            call_id: "c1".into(),
            is_error: false,
            detail: None,
            run_id: None,
            parent_call_id: None,
        }));
        assert_eq!(
            model.entries()[0],
            TranscriptEntry::Tool {
                run_id: None,
                tool_name: "read".into(),
                call_id: "c1".into(),
                input: None,
                output: None,
                detail: None,
                is_error: false,
                status: ToolStatus::Succeeded,
                reviews: Vec::new(),
                parent_call_id: None,
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
