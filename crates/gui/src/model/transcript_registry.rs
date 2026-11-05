//! Transcript 配送の責務を集約し、run ID ごとにイベントを分離する。
//!
//! `MessageDelta` / `ReasoningDelta` は payload の `run_id` が `Some` なら該当 run へ、
//! conversation root の場合だけ thread にも配送する。`run_id` が `None` の delta は
//! 警告して完全に破棄し、Running の run 数にかかわらず配送先を推測しない。
//!
//! Subscriber lag is diagnostic-only: the event bus reports each episode.
//! Transcripts never retain lag residue or insert lag summaries into conversations.

use std::collections::{BTreeMap, BTreeSet};

use event_bus::{AgentMessageEvent, Event, EventKind, MessageEvent, ToolEvent};

use super::scoped_call::parse_scoped_call_id;
use super::transcript::{Fill, MessageDirection, TranscriptEntry, TranscriptModel};
use crate::body::{BodyRef, BodyText, FailureKind, HistoryRow};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptKey {
    Thread,
    Run(String),
}

#[derive(Debug, Clone)]
pub struct TranscriptRegistry {
    thread: TranscriptModel,
    threads: BTreeMap<String, TranscriptModel>,
    active_thread: Option<String>,
    run_threads: BTreeMap<String, String>,
    thread_roots: BTreeMap<String, String>,
    runs: BTreeMap<String, TranscriptModel>,
    call_index: BTreeMap<String, String>,
    ambiguous_calls: BTreeSet<String>,
    compaction_checkpoints: BTreeSet<(String, String)>,
    /// Events of other processes are being shown: nothing may be delivered to
    /// the open thread or the unowned fallback on their behalf.
    foreign_scope: bool,
    /// The stored event being applied, for the entries it makes.
    source: Option<(i64, i64)>,
}

impl Default for TranscriptRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TranscriptRegistry {
    pub(crate) fn log_restore_census(&self) {
        if !tracing::enabled!(target: "gui::restore", tracing::Level::INFO) {
            return;
        }
        let mut entries = 0usize;
        let mut tool_input_bytes = 0usize;
        let mut tool_detail_bytes = 0usize;
        let mut tool_output_bytes = 0usize;
        let mut message_bytes = 0usize;
        let mut reasoning_bytes = 0usize;
        for model in std::iter::once(&self.thread)
            .chain(self.threads.values())
            .chain(self.runs.values())
        {
            for entry in model.entries() {
                entries += 1;
                match entry {
                    TranscriptEntry::Tool {
                        input,
                        detail,
                        output,
                        ..
                    } => {
                        tool_input_bytes += input.as_ref().map_or(0, json_bytes);
                        tool_detail_bytes += detail.as_ref().map_or(0, json_bytes);
                        tool_output_bytes += output.as_ref().map_or(0, BodyText::len);
                    }
                    TranscriptEntry::Message { text, .. } => message_bytes += text.len(),
                    TranscriptEntry::Reasoning { text, .. } => reasoning_bytes += text.len(),
                    _ => {}
                }
            }
        }
        let (vm_rss_kib, vm_hwm_kib) = crate::restore_metrics::process_memory();
        tracing::info!(
            target: "gui::restore",
            thread_models = self.threads.len(),
            fallback_models = 1,
            run_models = self.runs.len(),
            entries,
            tool_input_bytes,
            tool_detail_bytes,
            tool_output_bytes,
            message_bytes,
            reasoning_bytes,
            vm_rss_kib,
            vm_hwm_kib,
            "restore census (logical payload bytes, including duplicate deliveries)"
        );
    }

    pub(crate) fn finish_history(&mut self) {
        self.thread.finish_history();
        for model in self.threads.values_mut().chain(self.runs.values_mut()) {
            model.finish_history();
        }
    }

    /// Interrupts `run_id`'s running calls in every transcript that shows them,
    /// for runs whose terminal phase was reconciled without an event.
    pub(crate) fn interrupt_run_tools(&mut self, run_id: &str) {
        self.thread.interrupt_compactions(Some(run_id));
        self.thread.interrupt_running_tools(run_id);
        for model in self.threads.values_mut().chain(self.runs.values_mut()) {
            model.interrupt_compactions(Some(run_id));
            model.interrupt_running_tools(run_id);
        }
    }

    pub(crate) fn running_tool_runs(&self) -> BTreeSet<String> {
        std::iter::once(&self.thread)
            .chain(self.threads.values())
            .chain(self.runs.values())
            .flat_map(TranscriptModel::running_tool_runs)
            .map(str::to_owned)
            .collect()
    }

    pub fn new() -> Self {
        Self {
            thread: TranscriptModel::new(),
            threads: BTreeMap::new(),
            active_thread: None,
            run_threads: BTreeMap::new(),
            thread_roots: BTreeMap::new(),
            runs: BTreeMap::new(),
            call_index: BTreeMap::new(),
            ambiguous_calls: BTreeSet::new(),
            compaction_checkpoints: BTreeSet::new(),
            foreign_scope: false,
            source: None,
        }
    }

    /// Says which stored event the events applied from now on come from (first
    /// and last id, more than one for a response joined from slices). `None`
    /// for events that are not in the log.
    pub fn set_source(&mut self, source: Option<(i64, i64)>) {
        self.source = source;
    }

    pub fn run_for_call<'a>(&'a self, call_id: &'a str) -> Option<&'a str> {
        if let Some((run_id, _, _)) = parse_scoped_call_id(call_id) {
            return call_id.get(..run_id.len());
        }
        if self.ambiguous_calls.contains(call_id) {
            return None;
        }
        self.call_index.get(call_id).map(String::as_str)
    }

    pub fn route(&self, event: &Event) -> Vec<TranscriptKey> {
        match &event.kind {
            EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
                if question.run_id == question.root_run_id {
                    self.route_run(&question.run_id)
                } else {
                    let mut route = Vec::new();
                    if self.run_threads.contains_key(&question.run_id)
                        || self.run_threads.contains_key(&question.root_run_id)
                    {
                        route.push(TranscriptKey::Thread);
                    }
                    route.push(TranscriptKey::Run(question.run_id.clone()));
                    route
                }
            }
            EventKind::Lifecycle(event_bus::LifecycleEvent::RunProgress { run_id, .. })
            | EventKind::Compaction(
                event_bus::CompactionEvent::Compacted { run_id, .. }
                | event_bus::CompactionEvent::Progress { run_id, .. }
                | event_bus::CompactionEvent::Aborted { run_id, .. },
            ) => self.route_run(run_id),
            EventKind::Tool(ToolEvent::ArtifactsPresented { run_id, .. }) => self.route_run(run_id),
            EventKind::Ownership(_) => vec![TranscriptKey::Thread],
            // A pull request change is not a transcript entry: no transcript receives it.
            EventKind::PullRequest(_) => Vec::new(),
            EventKind::Ledger(event_bus::LedgerEvent::RunLedgerAppended { run_id, .. }) => {
                vec![TranscriptKey::Run(run_id.clone())]
            }
            EventKind::Diagnostic(event) => event.run_id.as_ref().map_or_else(
                || vec![TranscriptKey::Thread],
                |run_id| self.route_run(run_id),
            ),
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStarted {
                run_id,
                parent_run_id,
                ..
            }) => {
                let mut route = Vec::new();
                if parent_run_id.is_some() {
                    route.push(TranscriptKey::Thread);
                }
                route.push(TranscriptKey::Run(run_id.clone()));
                route
            }
            EventKind::Lifecycle(event_bus::LifecycleEvent::TaskPromptPublished {
                run_id, ..
            }) => vec![TranscriptKey::Run(run_id.clone())],
            EventKind::Lifecycle(event_bus::LifecycleEvent::TurnCompleted { run_id, .. }) => {
                self.route_run(run_id)
            }
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to:
                    event_bus::AgentRunPhase::Stopped
                    | event_bus::AgentRunPhase::Done
                    | event_bus::AgentRunPhase::Error,
                ..
            }) => vec![TranscriptKey::Thread, TranscriptKey::Run(run_id.clone())],
            // An idle run discards drafts of calls its finished response never started.
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                run_id,
                to: event_bus::AgentRunPhase::Waiting,
                ..
            }) => self.route_run(run_id),
            EventKind::Lifecycle(event_bus::LifecycleEvent::Failed { .. })
            | EventKind::Fault(_) => {
                vec![TranscriptKey::Thread]
            }
            EventKind::Provider(event_bus::ProviderEvent::FallbackTriggered {
                session_id, ..
            }) => self.route_run(session_id),
            EventKind::Provider(
                event_bus::ProviderEvent::RequestFailed { run_id, .. }
                | event_bus::ProviderEvent::CooldownStarted { run_id, .. },
            ) => match run_id {
                Some(run_id) => self.route_run(run_id),
                None => vec![TranscriptKey::Thread],
            },
            EventKind::Message(
                MessageEvent::FinalResultPublished { run_id, .. }
                | MessageEvent::MessageCompleted { run_id, .. }
                | MessageEvent::MessagePart { run_id, .. },
            ) => self.route_run(run_id),
            EventKind::Message(MessageEvent::MessageDelta { run_id, .. })
            | EventKind::Message(MessageEvent::ReasoningDelta { run_id, .. })
            | EventKind::Message(MessageEvent::ToolCallDelta { run_id, .. })
            | EventKind::Message(MessageEvent::StreamRetried { run_id }) => match run_id {
                Some(run_id) => self.route_run(run_id),
                None => Vec::new(),
            },
            EventKind::Tool(ToolEvent::ToolStarted { run_id, .. })
            | EventKind::Tool(ToolEvent::ToolCompleted { run_id, .. })
            | EventKind::Tool(ToolEvent::ShellJobOutput { run_id, .. }) => {
                run_id.as_ref().map_or_else(
                    || vec![TranscriptKey::Thread],
                    |run_id| self.route_run(run_id),
                )
            }
            EventKind::Tool(ToolEvent::ApprovalRequested {
                call_id,
                correlation,
                ..
            })
            | EventKind::Tool(ToolEvent::ApprovalResolved {
                call_id,
                correlation,
                ..
            }) => correlation
                .as_ref()
                .map(|c| c.run_id.as_str())
                .or_else(|| self.run_for_call(call_id))
                .or_else(|| self.call_index.get(call_id).map(String::as_str))
                .map_or_else(
                    || vec![TranscriptKey::Thread],
                    |run_id| self.route_run(run_id),
                ),
            EventKind::Tool(ToolEvent::ExecutionDenied { call_id, .. }) => self
                .run_for_call(call_id)
                .or_else(|| self.call_index.get(call_id).map(String::as_str))
                .map_or_else(
                    || vec![TranscriptKey::Thread],
                    |run_id| self.route_run(run_id),
                ),
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => vec![
                TranscriptKey::Run(message.sender_run_id.clone()),
                TranscriptKey::Run(message.recipient_run_id.clone()),
            ],
            // Lane ledger events describe the sidebar and lane list, not a transcript.
            EventKind::Ledger(_)
            | EventKind::Lifecycle(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Orchestrator(_) => Vec::new(),
        }
    }

    pub fn apply(&mut self, event: &Event) {
        self.apply_with_body(event, None);
    }

    /// [`Self::apply`] for an event whose display body is still in the store.
    pub fn apply_with_body(&mut self, event: &Event, body: Option<&BodyText>) {
        // A committed event may be rejected on receipt after authority changes.
        // Existing ownership signals therefore also settle live-only cards,
        // including saved compactions whose run phase was already terminal.
        if let EventKind::Ownership(ownership) = &event.kind
            && matches!(
                ownership.action,
                event_bus::OwnershipAction::Claimed
                    | event_bus::OwnershipAction::Released
                    | event_bus::OwnershipAction::Handoff
            )
        {
            if let Some(model) = self.threads.get_mut(&ownership.thread_id) {
                model.interrupt_compactions(None);
            }
            for (run, thread) in &self.run_threads {
                if thread == &ownership.thread_id
                    && let Some(model) = self.runs.get_mut(run)
                {
                    model.interrupt_compactions(Some(run));
                }
            }
        }
        if let EventKind::Compaction(event_bus::CompactionEvent::Compacted {
            run_id,
            checkpoint_id,
            ..
        }) = &event.kind
            && !self
                .compaction_checkpoints
                .insert((run_id.clone(), checkpoint_id.clone()))
        {
            return;
        }
        match &event.kind {
            EventKind::Message(
                MessageEvent::MessageDelta { run_id: None, .. }
                | MessageEvent::ReasoningDelta { run_id: None, .. }
                | MessageEvent::ToolCallDelta { run_id: None, .. }
                | MessageEvent::StreamRetried { run_id: None },
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
                }
                | MessageEvent::ToolCallDelta {
                    run_id: Some(_), ..
                }
                | MessageEvent::StreamRetried { run_id: Some(_) }
                | MessageEvent::FinalResultPublished { .. }
                | MessageEvent::MessageCompleted { .. }
                | MessageEvent::MessagePart { .. },
            )
            | EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::Orchestrator(_)
            | EventKind::Diagnostic(_)
            | EventKind::Ownership(_)
            | EventKind::PullRequest(_) => {}
        }

        if let EventKind::Tool(ToolEvent::ToolStarted {
            call_id,
            run_id: Some(run_id),
            ..
        }) = &event.kind
            && self
                .call_index
                .insert(call_id.clone(), run_id.clone())
                .is_some_and(|previous_run| previous_run != *run_id)
        {
            self.ambiguous_calls.insert(call_id.clone());
        }

        if let EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) = &event.kind {
            let source = self.source;
            let sender = self.runs.entry(message.sender_run_id.clone()).or_default();
            sender.set_source(source);
            sender.push(TranscriptEntry::AgentMessage {
                direction: MessageDirection::Outgoing,
                peer_run_id: message.recipient_run_id.clone(),
                kind: message.kind.clone(),
                content: message.content.clone(),
            });
            sender.set_source(None);
            let recipient = self
                .runs
                .entry(message.recipient_run_id.clone())
                .or_default();
            recipient.set_source(source);
            recipient.push(TranscriptEntry::AgentMessage {
                direction: MessageDirection::Incoming,
                peer_run_id: message.sender_run_id.clone(),
                kind: message.kind.clone(),
                content: message.content.clone(),
            });
            recipient.set_source(None);
            return;
        }

        let route = self.route(event);
        let explicit_thread = match &event.kind {
            EventKind::Diagnostic(event) => event.thread_id.clone(),
            EventKind::Ownership(event) => Some(event.thread_id.clone()),
            EventKind::Lifecycle(_)
            | EventKind::Ledger(_)
            | EventKind::Message(_)
            | EventKind::Tool(_)
            | EventKind::Usage(_)
            | EventKind::Provider(_)
            | EventKind::Fault(_)
            | EventKind::AgentMessage(_)
            | EventKind::Compaction(_)
            | EventKind::PullRequest(_)
            | EventKind::Orchestrator(_) => None,
        };
        let owner = explicit_thread
            .or_else(|| {
                route.iter().find_map(|key| match key {
                    TranscriptKey::Run(id) => self.run_threads.get(id).cloned(),
                    TranscriptKey::Thread => None,
                })
            })
            .or_else(|| match &event.kind {
                EventKind::Tool(ToolEvent::UserQuestionUpdated { question }) => {
                    self.run_threads.get(&question.root_run_id).cloned()
                }
                _ => None,
            });
        let attributed = route.iter().any(|key| matches!(key, TranscriptKey::Run(_)));
        let child_terminal = matches!(&event.kind,
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged { run_id, to: event_bus::AgentRunPhase::Stopped | event_bus::AgentRunPhase::Done | event_bus::AgentRunPhase::Error, .. })
                if !self.is_thread_root(run_id));
        let source = self.source;
        for key in route {
            match key {
                TranscriptKey::Thread => {
                    if self.foreign_scope && owner.is_none() {
                        continue;
                    }
                    let model = match owner.as_ref().or({
                        if attributed {
                            None
                        } else {
                            self.active_thread.as_ref()
                        }
                    }) {
                        Some(id) => self.threads.entry(id.clone()).or_default(),
                        None => &mut self.thread,
                    };
                    model.set_deferred_body(body.cloned());
                    model.set_source(source);
                    model.apply_thread(event, child_terminal);
                    model.set_source(None);
                    model.set_deferred_body(None);
                }
                TranscriptKey::Run(run_id) => {
                    let model = self.runs.entry(run_id).or_default();
                    model.set_deferred_body(body.cloned());
                    model.set_source(source);
                    model.apply(event);
                    model.set_source(None);
                    model.set_deferred_body(None);
                }
            }
        }
    }

    /// Marks, in every transcript that would have shown the event, the place
    /// of a stored event that could not be read. Returns false when no
    /// transcript owns it (no run recorded): nothing is guessed, the caller
    /// reports it elsewhere.
    pub fn push_unreadable(
        &mut self,
        event_id: i64,
        run_id: Option<&str>,
        kind: FailureKind,
        reason: &str,
        reference: Option<BodyRef>,
    ) -> bool {
        let Some(run) = run_id else {
            return false;
        };
        for key in self.route_run(run) {
            match key {
                TranscriptKey::Thread => {
                    // Only a bound run reaches a thread; the open thread is never a guess.
                    if let Some(thread) = self.run_threads.get(run).cloned() {
                        self.threads.entry(thread).or_default().push_unreadable(
                            event_id,
                            run_id,
                            kind.clone(),
                            reason,
                            reference,
                        );
                    }
                }
                TranscriptKey::Run(run) => {
                    self.runs.entry(run).or_default().push_unreadable(
                        event_id,
                        run_id,
                        kind.clone(),
                        reason,
                        reference,
                    );
                }
            }
        }
        true
    }

    /// Replaces every body in every transcript (tests only).
    #[cfg(test)]
    pub(crate) fn map_bodies(&mut self, f: &mut dyn FnMut(&BodyText) -> BodyText) {
        for model in self.models_mut() {
            model.map_bodies(f);
        }
    }

    #[cfg(test)]
    pub(crate) fn has_deferred_bodies(&self) -> bool {
        std::iter::once(&self.thread)
            .chain(self.threads.values())
            .chain(self.runs.values())
            .any(TranscriptModel::has_deferred_bodies)
    }

    fn models_mut(&mut self) -> impl Iterator<Item = &mut TranscriptModel> {
        std::iter::once(&mut self.thread)
            .chain(self.threads.values_mut())
            .chain(self.runs.values_mut())
    }

    /// Records why reading the event again failed, in every transcript that
    /// shows it as unreadable.
    pub fn update_unreadable(
        &mut self,
        event_id: i64,
        kind: Option<&FailureKind>,
        reason: &str,
        reference: Option<BodyRef>,
    ) {
        for model in self.models_mut() {
            model.update_unreadable(event_id, kind.cloned(), reason, reference);
        }
    }

    /// Fills the gap of `event_id` in the transcripts of `run` from `rows`,
    /// the rows of the run read again (their slices already joined). The run
    /// is replayed into scratch transcripts the way a restart would show it,
    /// and each shown transcript takes from that replay only what its gap
    /// held, so nothing shown moves or changes identity. No notification, no
    /// telemetry: only the transcripts are touched. Returns whether any
    /// transcript showed the gap.
    pub fn fill_unreadable(
        &mut self,
        run: &str,
        event_id: i64,
        rows: &[HistoryRow],
        through: i64,
    ) -> Fill {
        let mut scratch = Self::new();
        match self.run_threads.get(run) {
            Some(thread) if self.is_thread_root(run) => scratch.bind_thread_root(thread, run),
            Some(thread) => scratch.bind_run(run, thread),
            None => {}
        }
        for row in rows {
            match row {
                HistoryRow::Event(row) => {
                    scratch.set_source(Some((row.first_id.unwrap_or(row.id), row.id)));
                    scratch.apply_with_body(&row.event, row.body.as_ref());
                    scratch.set_source(None);
                }
                HistoryRow::Failure(failure) if failure.run_id.as_deref() == Some(run) => {
                    scratch.push_unreadable(
                        failure.id,
                        Some(run),
                        failure.kind.clone(),
                        &failure.reason,
                        failure.reference,
                    );
                }
                HistoryRow::Failure(_) => {}
            }
        }
        // Asked of every transcript first: a replacement is made in all of them
        // or in none.
        let mut outcome = Fill::Absent;
        for apply in [false, true] {
            for key in self.route_run(run) {
                let result = match key {
                    TranscriptKey::Thread => {
                        let Some(thread) = self.run_threads.get(run).cloned() else {
                            continue;
                        };
                        match (self.threads.get_mut(&thread), scratch.threads.get(&thread)) {
                            (Some(model), Some(replayed)) => {
                                model.fill_unreadable_from(event_id, replayed, through, apply)
                            }
                            _ => Fill::Absent,
                        }
                    }
                    TranscriptKey::Run(run) => {
                        match (self.runs.get_mut(&run), scratch.runs.get(&run)) {
                            (Some(model), Some(replayed)) => {
                                model.fill_unreadable_from(event_id, replayed, through, apply)
                            }
                            _ => Fill::Absent,
                        }
                    }
                };
                match result {
                    Fill::Stale => return Fill::Stale,
                    Fill::Filled => outcome = Fill::Filled,
                    Fill::Absent => {}
                }
            }
        }
        outcome
    }

    /// Drops the transcript of a deleted thread (and of its runs).
    pub fn drop_thread(&mut self, thread: &str) -> Vec<String> {
        let runs: Vec<String> = self
            .run_threads
            .iter()
            .filter(|(_, owner)| owner.as_str() == thread)
            .map(|(run, _)| run.clone())
            .collect();
        self.threads.remove(thread);
        self.thread_roots.remove(thread);
        for run in &runs {
            self.runs.remove(run);
            self.run_threads.remove(run);
        }
        runs
    }

    pub fn get(&self, key: &TranscriptKey) -> Option<&TranscriptModel> {
        match key {
            TranscriptKey::Thread => Some(self.thread()),
            TranscriptKey::Run(run_id) => self.runs.get(run_id),
        }
    }

    pub fn thread(&self) -> &TranscriptModel {
        self.active_thread
            .as_ref()
            .and_then(|id| self.threads.get(id))
            .unwrap_or(&self.thread)
    }

    pub fn push_thread(&mut self, entry: TranscriptEntry) {
        match &self.active_thread {
            Some(id) => self.threads.entry(id.clone()).or_default().push(entry),
            None => self.thread.push(entry),
        }
    }

    pub fn select_thread(&mut self, id: Option<String>) {
        if let Some(id) = &id {
            self.threads.entry(id.clone()).or_default();
        }
        self.active_thread = id;
    }

    pub(crate) fn thread_model(&self, thread: &str) -> Option<&TranscriptModel> {
        self.threads.get(thread)
    }

    /// Install a branched conversation's inherited history.
    pub(crate) fn insert_thread(&mut self, thread: &str, model: TranscriptModel) {
        self.threads.insert(thread.into(), model);
    }

    pub(crate) fn push_to_thread(&mut self, thread: &str, entry: TranscriptEntry) {
        self.threads.entry(thread.into()).or_default().push(entry);
    }

    pub(crate) fn push_host_input(
        &mut self,
        thread: &str,
        id: &str,
        text: String,
        skills: Vec<String>,
    ) {
        self.threads
            .entry(thread.into())
            .or_default()
            .push_host_input(id, text, skills);
    }

    pub(crate) fn update_host_input(
        &mut self,
        thread: &str,
        id: &str,
        text: &str,
        skills: &[String],
    ) {
        if let Some(model) = self.threads.get_mut(thread) {
            model.update_host_input(id, text, skills);
        }
    }

    pub(crate) fn remove_host_input(&mut self, thread: &str, id: &str) {
        if let Some(model) = self.threads.get_mut(thread) {
            model.remove_host_input(id);
        }
    }

    /// Adopt output that may arrive before the escalation event names its owner.
    pub(crate) fn adopt_thread_root(&mut self, thread: &str, run: &str) {
        if !self.thread_roots.contains_key(thread) {
            let transcript = self.runs.get(run).cloned().unwrap_or_default();
            self.threads.insert(thread.into(), transcript);
        }
        self.bind_thread_root(thread, run);
    }

    /// While set, events that no conversation owns are not shown anywhere.
    /// Returns the previous setting, to restore when the scope ends.
    pub(crate) fn set_foreign_scope(&mut self, on: bool) -> bool {
        std::mem::replace(&mut self.foreign_scope, on)
    }

    /// The conversation a run is bound to, if any.
    pub(crate) fn run_thread(&self, run: &str) -> Option<&str> {
        self.run_threads.get(run).map(String::as_str)
    }

    /// Drops what was shown for `run` while no conversation owned it, ahead of
    /// replaying its events under the conversation that does.
    pub(crate) fn forget_run(&mut self, run: &str) {
        self.runs.remove(run);
        self.compaction_checkpoints
            .retain(|(checkpoint_run, _)| checkpoint_run != run);
    }

    pub fn bind_run(&mut self, run: &str, thread: &str) {
        self.thread_roots
            .retain(|owner, root| owner == thread || root != run);
        self.run_threads.insert(run.into(), thread.into());
    }

    pub fn bind_thread_root(&mut self, thread: &str, run: &str) {
        self.bind_run(run, thread);
        self.thread_roots.insert(thread.into(), run.into());
    }

    pub(crate) fn is_thread_root(&self, run: &str) -> bool {
        self.run_threads
            .get(run)
            .and_then(|thread| self.thread_roots.get(thread))
            .is_some_and(|root| root == run)
    }

    fn route_run(&self, run: &str) -> Vec<TranscriptKey> {
        let mut route = Vec::new();
        if self.is_thread_root(run) {
            route.push(TranscriptKey::Thread);
        }
        route.push(TranscriptKey::Run(run.into()));
        route
    }

    pub fn run(&self, run_id: &str) -> Option<&TranscriptModel> {
        self.runs.get(run_id)
    }

    pub fn run_ids(&self) -> impl Iterator<Item = &String> {
        self.runs.keys()
    }
}

// Estimate retained JSON payload without allocating a second serialized copy.
fn json_bytes(value: &serde_json::Value) -> usize {
    match value {
        serde_json::Value::Null => 4,
        serde_json::Value::Bool(_) => 5,
        serde_json::Value::Number(_) => 8,
        serde_json::Value::String(text) => text.len(),
        serde_json::Value::Array(values) => values.iter().map(json_bytes).sum(),
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len() + json_bytes(value))
            .sum(),
    }
}

// allow: SIZE_OK - issue #85 keeps routing regressions beside the existing registry tests.
#[cfg(test)]
mod root_tests;

#[cfg(test)]
mod approval_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::transcript::{MessageDirection, ToolStatus, TranscriptEntry};
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, Event,
        MessageEvent, ToolEvent,
    };
    use event_bus::{AgentRunPhase, LifecycleEvent};

    // Given: a pull request change for a thread whose transcript is selected / When: it is
    // routed and applied / Then: no transcript receives it, so it can never reach a model
    #[test]
    fn a_pull_request_change_enters_no_transcript() {
        let mut registry = TranscriptRegistry::new();
        registry.bind_thread_root("thread", "run-1");
        registry.select_thread(Some("thread".into()));
        let event = Event::new(event_bus::PullRequestEvent::Changed {
            thread_id: "thread".into(),
            link_revision: 1,
            transition_seq: 1,
            pull_request: event_bus::PullRequestRef {
                provider: "github".into(),
                host: "github.com".into(),
                repository: "turtton/evorch".into(),
                number: 7,
            },
            fields: vec![event_bus::PullRequestChangedField::Checks],
            notifications: vec!["checks_failed".into()],
        });

        let destinations = registry.route(&event);
        registry.apply(&event);

        assert!(destinations.is_empty());
        assert!(registry.thread().entries().is_empty());
    }

    #[test]
    fn run_error_routes_to_owning_thread() {
        // Given: an attributed run failure.
        let mut registry = TranscriptRegistry::new();
        registry.bind_thread_root("thread", "run-error");
        registry.select_thread(Some("thread".into()));
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
    fn root_run_delta_reaches_thread_and_run() {
        // Given: both stream variants carry an explicit run attribution.
        let mut registry = TranscriptRegistry::new();
        registry.bind_thread_root("thread", "run-2");
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
        registry.bind_thread_root("thread", "run-1");
        let started = Event::new(ToolEvent::ToolStarted {
            input: None,
            tool_name: "read".into(),
            call_id: "call-1".into(),
            run_id: Some("run-1".into()),
            parent_call_id: None,
        });
        registry.apply(&started);
        let completed = Event::new(ToolEvent::ToolCompleted {
            output: None,
            tool_name: "read".into(),
            call_id: "call-1".into(),
            is_error: false,
            detail: None,
            run_id: Some("run-1".into()),
            parent_call_id: None,
        });

        assert_eq!(
            registry.route(&completed),
            vec![TranscriptKey::Thread, TranscriptKey::Run("run-1".into())]
        );
        registry.apply(&completed);
        assert_eq!(
            registry.run("run-1").expect("run transcript").entries(),
            &[TranscriptEntry::Tool {
                run_id: Some("run-1".into()),
                tool_name: "read".into(),
                call_id: "call-1".into(),
                input: None,
                output: None,
                detail: None,
                is_error: false,
                status: ToolStatus::Succeeded,
                reviews: Vec::new(),
                parent_call_id: None,
            }]
        );
    }

    #[test]
    fn run_for_call_returns_indexed_run_or_none() {
        // Given: one indexed call and one call without run attribution.
        let mut registry = TranscriptRegistry::new();
        for (call_id, run_id) in [("known", Some("run-2")), ("runless", None)] {
            registry.apply(&Event::new(ToolEvent::ToolStarted {
                input: None,
                tool_name: "write".into(),
                call_id: call_id.into(),
                run_id: run_id.map(str::to_owned),
                parent_call_id: None,
            }));
        }
        // When: looking up known, unknown, and runless calls.
        let targets = ["known", "unknown", "runless"].map(|id| registry.run_for_call(id));
        // Then: only the explicitly attributed call has a target.
        assert_eq!(targets, [Some("run-2"), None, None]);
    }

    #[test]
    fn approval_events_follow_call_index_else_thread() {
        let mut registry = TranscriptRegistry::new();
        registry.bind_thread_root("thread", "run-2");
        registry.apply(&Event::new(ToolEvent::ToolStarted {
            input: None,
            tool_name: "write".into(),
            call_id: "known".into(),
            run_id: Some("run-2".into()),
            parent_call_id: None,
        }));
        let known = Event::new(ToolEvent::ApprovalRequested {
            correlation: None,
            input: None,
            tool_name: "write".into(),
            call_id: "known".into(),
        });
        let unknown = Event::new(ToolEvent::ApprovalResolved {
            correlation: None,
            call_id: "unknown".into(),
            approved: false,
        });

        assert_eq!(
            registry.route(&known),
            vec![TranscriptKey::Thread, TranscriptKey::Run("run-2".into())]
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
                input: None,
                tool_name: format!("tool-{run_id}"),
                call_id: format!("call-{run_id}"),
                run_id: Some(run_id.into()),
                parent_call_id: None,
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
                    run_id: Some(run_id.into()),
                    tool_name: format!("tool-{run_id}"),
                    call_id: format!("call-{run_id}"),
                    input: None,
                    output: None,
                    detail: None,
                    is_error: false,
                    status: ToolStatus::Running,
                    reviews: Vec::new(),
                    parent_call_id: None,
                }]
            );
        }
        assert_eq!(registry.thread().entries(), &[]);
    }
}
