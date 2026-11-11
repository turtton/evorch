//! A disposable projection. Drafts and pending commands intentionally live
//! outside this cache so replacing an epoch cannot erase unsent human input.
use std::collections::BTreeMap;
use workbench_protocol::namespaces::{
    ApprovalsEvent, ConversationEvent, ExecutionEvent, LifecycleEvent, ProjectionEvent,
    QuestionsEvent, WorkspaceEvent,
};

use workbench_protocol::{
    events::{CursorDecision, CursorTracker, EventCursor, EventEnvelope, SnapshotItem, UiEvent},
    ids::PageCursor,
    views::*,
};

#[derive(Default, Clone)]
pub struct Cache {
    pub projects: BTreeMap<String, ProjectSummary>,
    pub threads: BTreeMap<String, ThreadSummary>,
    pub runs: BTreeMap<String, RunSummary>,
    pub questions: BTreeMap<String, QuestionView>,
    pub approvals: BTreeMap<String, ApprovalView>,
    pub receipts: BTreeMap<String, InputReceipt>,
    pub history: BTreeMap<String, Vec<CommittedMessage>>,
    pub older: BTreeMap<String, Option<PageCursor>>,
    pub bodies: BTreeMap<String, String>,
    pub approval_inputs: BTreeMap<String, String>,
    cursor: Option<CursorTracker>,
}

impl Cache {
    pub fn snapshot(&mut self, item: SnapshotItem) {
        match item {
            SnapshotItem::CoreStatus(_) => (),
            SnapshotItem::Project(p) => {
                self.projects.insert(p.id.to_string(), p);
            }
            SnapshotItem::Thread(t) => {
                self.threads.insert(t.id.to_string(), t);
            }
            SnapshotItem::Run(r) => {
                self.runs.insert(r.id.to_string(), r);
            }
            SnapshotItem::Question(q) => {
                self.questions.insert(q.id.to_string(), q);
            }
            SnapshotItem::Approval(a) => {
                self.approvals.insert(a.id.to_string(), a);
            }
        }
    }

    pub fn at(&mut self, cursor: EventCursor) {
        self.cursor = Some(CursorTracker::new(cursor));
    }

    pub fn cursor(&self) -> Option<EventCursor> {
        self.cursor.as_ref().map(CursorTracker::last)
    }

    /// False means this projection cannot be continued and needs a fresh cut.
    pub fn event(&mut self, envelope: EventEnvelope) -> bool {
        if envelope.event.ends_subscription() {
            return false;
        }
        let Some(cursor) = &mut self.cursor else {
            return false;
        };
        match cursor.observe(envelope.cursor) {
            CursorDecision::Duplicate => return true,
            CursorDecision::Deliver => (),
            _ => return false,
        }
        match envelope.event {
            UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged { .. }) => (),
            UiEvent::Workspace(WorkspaceEvent::ThreadChanged { thread }) => {
                self.threads.insert(thread.id.to_string(), thread);
            }
            UiEvent::Execution(ExecutionEvent::RunChanged { run }) => {
                self.runs.insert(run.id.to_string(), run);
            }
            UiEvent::Questions(QuestionsEvent::QuestionChanged { question }) => {
                self.questions.insert(question.id.to_string(), question);
            }
            UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval }) => {
                self.approval_inputs.remove(approval.id.as_str());
                self.approvals.insert(approval.id.to_string(), approval);
            }
            UiEvent::Conversation(ConversationEvent::InputReceiptChanged { receipt }) => {
                self.receipts
                    .insert(receipt.submission_id.to_string(), receipt);
            }
            UiEvent::Conversation(ConversationEvent::MessageCommitted { message }) => {
                let messages = self.history.entry(message.thread.to_string()).or_default();
                if let Some(existing) = messages
                    .iter_mut()
                    .find(|m| m.run == message.run && m.index == message.index)
                {
                    *existing = message;
                } else {
                    messages.push(message);
                }
            }
            UiEvent::Projection(ProjectionEvent::ResyncRequired { .. })
            | UiEvent::Projection(ProjectionEvent::SubscriptionClosed { .. }) => unreachable!(),
        }
        true
    }

    pub fn history_page(&mut self, thread: &str, page: Page<CommittedMessage>, older: bool) {
        let existing = self.history.entry(thread.into()).or_default();
        let mut messages = page.items;
        let remaining = existing
            .iter()
            .filter(|_| older)
            .filter(|old| !messages_key_in_page(old, &messages))
            .cloned()
            .collect::<Vec<_>>();
        messages.extend(remaining);
        *existing = messages;
        self.older.insert(thread.into(), page.next_cursor);
    }
}

fn messages_key_in_page(message: &CommittedMessage, page: &[CommittedMessage]) -> bool {
    page.iter()
        .any(|m| m.run == message.run && m.index == message.index)
}
