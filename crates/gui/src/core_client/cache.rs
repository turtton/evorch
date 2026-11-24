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
    /// The committed messages of each run a run tab shows, keyed by run. An entry
    /// exists only while a tab wants that run ([`Self::want_run`]).
    pub run_history: BTreeMap<String, Vec<CommittedMessage>>,
    pub run_older: BTreeMap<String, Option<PageCursor>>,
    /// The wanted runs whose first page could not be read ([`Self::run_failed`]).
    run_failed: std::collections::BTreeSet<String>,
    /// The older page of each thread read for the thread view ([`Self::read_older_for_roots`]).
    older_for_roots: BTreeMap<String, PageCursor>,
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
        self.event_delivering(envelope, |_| {})
    }

    /// Like [`Self::event`], and hands a delivered (not duplicate) event to
    /// `deliver` too, so the features (`features/`) see each event exactly
    /// once and in the order the cache does.
    pub fn event_delivering(
        &mut self,
        envelope: EventEnvelope,
        deliver: impl FnOnce(&UiEvent),
    ) -> bool {
        if envelope.event.ends_subscription() {
            return false;
        }
        let Some(cursor) = &mut self.cursor else {
            return false;
        };
        match cursor.observe(envelope.cursor) {
            CursorDecision::Duplicate => return true,
            CursorDecision::Deliver => deliver(&envelope.event),
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
                if let Some(messages) = self.run_history.get_mut(message.run.as_str()) {
                    upsert(messages, message.clone());
                }
                upsert(
                    self.history.entry(message.thread.to_string()).or_default(),
                    message,
                );
            }
            UiEvent::Projection(ProjectionEvent::ResyncRequired { .. })
            | UiEvent::Projection(ProjectionEvent::SubscriptionClosed { .. }) => unreachable!(),
            // Events of a namespace a feature owns (and later events of the
            // namespaces above) are applied by the feature, not by this cache.
            #[allow(unreachable_patterns)]
            _ => {}
        }
        true
    }

    /// Keep the history of `run` from now on. True when it was not kept yet, so
    /// its first page has to be asked for.
    pub fn want_run(&mut self, run: &str) -> bool {
        if self.run_history.contains_key(run) {
            return false;
        }
        self.run_history.insert(run.into(), Vec::new());
        self.run_older.insert(run.into(), None);
        true
    }

    pub fn forget_run(&mut self, run: &str) {
        self.run_history.remove(run);
        self.run_older.remove(run);
        self.run_failed.remove(run);
    }

    /// The first page of a wanted `run` could not be read. Its tab offers to read it
    /// again rather than [`Self::want_run`] asking every frame for what keeps failing.
    pub fn run_failed(&mut self, run: &str) {
        if self.run_history.contains_key(run) {
            self.run_failed.insert(run.into());
        }
    }

    pub fn run_read_failed(&self, run: &str) -> bool {
        self.run_failed.contains(run)
    }

    /// A message the thread view shows: a root run's, or one of a run the cache does not
    /// know. A subagent's messages show only in its own run tab.
    pub fn shows_in_thread(&self, message: &CommittedMessage) -> bool {
        self.runs
            .get(message.run.as_str())
            .is_none_or(|run| run.parent_run.is_none())
    }

    /// The thread's history is read in pages of all its runs, so a page can hold only
    /// subagent messages, which the thread view does not show. True when the next older
    /// page is to be asked for so the view shows a root's message: no kept message
    /// shows, an older page exists, and it was not asked for yet.
    pub fn read_older_for_roots(&mut self, thread: &str) -> bool {
        let Some(Some(cursor)) = self.older.get(thread) else {
            return false;
        };
        let shown = self
            .history
            .get(thread)
            .into_iter()
            .flatten()
            .any(|message| self.shows_in_thread(message));
        if shown || self.older_for_roots.get(thread) == Some(cursor) {
            return false;
        }
        self.older_for_roots.insert(thread.into(), cursor.clone());
        true
    }

    pub fn history_page(&mut self, thread: &str, page: Page<CommittedMessage>, older: bool) {
        if !older {
            self.older_for_roots.remove(thread);
        }
        merge(
            self.history.entry(thread.into()).or_default(),
            page.items,
            older,
        );
        self.older.insert(thread.into(), page.next_cursor);
    }

    /// A page of one run's history, dropped once no tab wants that run any more.
    pub fn run_history_page(&mut self, run: &str, page: Page<CommittedMessage>, older: bool) {
        let Some(existing) = self.run_history.get_mut(run) else {
            return;
        };
        merge(existing, page.items, older);
        self.run_older.insert(run.into(), page.next_cursor);
        if !older {
            self.run_failed.remove(run);
        }
    }
}

fn upsert(messages: &mut Vec<CommittedMessage>, message: CommittedMessage) {
    if let Some(existing) = messages
        .iter_mut()
        .find(|m| m.run == message.run && m.index == message.index)
    {
        *existing = message;
    } else {
        messages.push(message);
    }
}

/// A newest page replaces what is kept; an older page goes in front of it.
fn merge(existing: &mut Vec<CommittedMessage>, page: Vec<CommittedMessage>, older: bool) {
    let mut messages = page;
    let remaining = existing
        .iter()
        .filter(|_| older)
        .filter(|old| !messages_key_in_page(old, &messages))
        .cloned()
        .collect::<Vec<_>>();
    messages.extend(remaining);
    *existing = messages;
}

fn messages_key_in_page(message: &CommittedMessage, page: &[CommittedMessage]) -> bool {
    page.iter()
        .any(|m| m.run == message.run && m.index == message.index)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::{
        namespaces::ConversationEvent,
        wire::{InstanceEpoch, Revision, Sequence},
    };

    fn message(run: &str, index: u64) -> CommittedMessage {
        serde_json::from_value(serde_json::json!({
            "thread": "thr", "run": run, "index": index.to_string(), "role": "assistant",
            "content": {"type": "inline", "payload": {"text": format!("{run}#{index}")}},
        }))
        .unwrap()
    }

    fn page(items: Vec<CommittedMessage>, older: Option<&str>) -> Page<CommittedMessage> {
        Page {
            items,
            next_cursor: older.map(|cursor| PageCursor::new(cursor).unwrap()),
            snapshot_revision: Revision(1),
        }
    }

    fn committed(cache: &mut Cache, sequence: u64, message: CommittedMessage) {
        let delivered = cache.event(EventEnvelope {
            subscription_id: workbench_protocol::ids::SubscriptionId::new("sub").unwrap(),
            caused_by_request_id: None,
            cursor: EventCursor {
                core_epoch: InstanceEpoch(1),
                sequence: Sequence(sequence),
            },
            event: UiEvent::Conversation(ConversationEvent::MessageCommitted { message }),
        });
        assert!(delivered);
    }

    fn keys(messages: &[CommittedMessage]) -> Vec<(String, u64)> {
        messages
            .iter()
            .map(|m| (m.run.to_string(), m.index.get()))
            .collect()
    }

    // Given: a thread's history, and no tab that wants run-b
    // When: a page of run-b arrives, then run-b is wanted and its pages arrive
    // Then: only a wanted run keeps a page, an older page goes in front with its
    //       own cursor, and the thread's history is left alone
    #[test]
    fn a_run_page_goes_only_to_a_wanted_run_and_keeps_its_own_cursor() {
        let mut cache = Cache::default();
        cache.history_page("thr", page(vec![message("run-a", 0)], None), false);
        cache.run_history_page("run-b", page(vec![message("run-b", 5)], None), false);
        assert!(!cache.run_history.contains_key("run-b"));

        assert!(cache.want_run("run-b"));
        assert!(!cache.want_run("run-b"), "an open run is not read again");
        let newest = page(vec![message("run-b", 5)], Some("c1"));
        cache.run_history_page("run-b", newest, false);
        assert_eq!(cache.run_older["run-b"].as_ref().unwrap().as_str(), "c1");
        let older = page(vec![message("run-b", 4)], None);
        cache.run_history_page("run-b", older, true);
        assert_eq!(
            keys(&cache.run_history["run-b"]),
            [("run-b".into(), 4), ("run-b".into(), 5)]
        );
        assert_eq!(cache.run_older["run-b"], None);
        assert_eq!(keys(&cache.history["thr"]), [("run-a".into(), 0)]);
        assert_eq!(cache.older["thr"], None);
    }

    // Given: a tab wants run-b but not run-c
    // When: a message of each is committed
    // Then: both reach the thread, and only run-b's reaches a run history
    #[test]
    fn a_committed_message_reaches_the_thread_and_an_open_run_history() {
        let mut cache = Cache::default();
        cache.at(EventCursor {
            core_epoch: InstanceEpoch(1),
            sequence: Sequence(0),
        });
        cache.want_run("run-b");
        committed(&mut cache, 1, message("run-b", 0));
        committed(&mut cache, 2, message("run-c", 0));
        assert_eq!(
            keys(&cache.history["thr"]),
            [("run-b".into(), 0), ("run-c".into(), 0)]
        );
        assert_eq!(keys(&cache.run_history["run-b"]), [("run-b".into(), 0)]);
        assert!(!cache.run_history.contains_key("run-c"));
    }

    #[test]
    fn forget_run_drops_history_and_cursor() {
        let mut cache = Cache::default();
        cache.want_run("run-b");
        cache.run_history_page("run-b", page(vec![message("run-b", 0)], Some("c")), false);
        cache.forget_run("run-b");
        assert!(!cache.run_history.contains_key("run-b"));
        assert!(!cache.run_older.contains_key("run-b"));
        assert!(cache.want_run("run-b"), "a run opened again is read again");
    }

    // Given: a wanted run-b and a run-c no tab wants
    // When: the first page of each fails, then run-b's newest page arrives
    // Then: only run-b is marked failed, and its newest page clears the mark
    #[test]
    fn a_failed_first_page_is_marked_until_a_newest_page_arrives() {
        let mut cache = Cache::default();
        cache.want_run("run-b");
        cache.run_failed("run-b");
        cache.run_failed("run-c");
        assert!(cache.run_read_failed("run-b"));
        assert!(!cache.run_read_failed("run-c"), "no tab wants run-c");
        assert!(
            !cache.want_run("run-b"),
            "a failed run is not asked for every frame"
        );
        cache.run_history_page("run-b", page(vec![message("run-b", 0)], None), false);
        assert!(!cache.run_read_failed("run-b"));
    }

    // Given: the newest page of a thread holds only a subagent's messages
    // When: the thread view asks whether to read further back
    // Then: the next older page is asked for once per cursor, and no more once a
    //       root's message is kept or the history begins
    #[test]
    fn a_page_of_subagent_messages_reads_older_pages_until_a_root_message() {
        let mut cache = Cache::default();
        let child: RunSummary = serde_json::from_value(serde_json::json!({
            "id": "run-c", "thread": "thr", "role": "worker", "phase": "running",
            "revision": "1", "parent_run": "run-r",
        }))
        .unwrap();
        cache.runs.insert("run-c".into(), child);
        assert!(!cache.read_older_for_roots("thr"), "nothing kept yet");
        cache.history_page("thr", page(vec![message("run-c", 9)], Some("c1")), false);
        assert!(cache.read_older_for_roots("thr"));
        assert!(!cache.read_older_for_roots("thr"), "asked once per cursor");
        cache.history_page("thr", page(vec![message("run-c", 8)], Some("c2")), true);
        assert!(cache.read_older_for_roots("thr"));
        cache.history_page("thr", page(vec![message("run-r", 0)], Some("c3")), true);
        assert!(!cache.read_older_for_roots("thr"), "a root's message shows");

        // A refreshed newest page may name a cursor asked for before.
        cache.history_page("thr", page(vec![message("run-c", 9)], Some("c1")), false);
        assert!(cache.read_older_for_roots("thr"));
        cache.history_page("thr", page(vec![message("run-c", 9)], None), false);
        assert!(!cache.read_older_for_roots("thr"), "the history begins");
    }
}
