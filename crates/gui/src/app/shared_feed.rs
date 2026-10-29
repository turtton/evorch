//! Showing what other GUI processes do in the shared database.
//!
//! Events another process persisted are folded here for display only: they
//! build transcripts, run phases and counters like a restart's replay does,
//! but nothing is sent to the runtime, no approval or wake-up is acted on, no
//! OS notification is raised, and nothing is emitted on the event bus (which
//! would persist it a second time).

use std::sync::Arc;
use std::time::{Duration, Instant};

use event_bus::{Event, EventKind, LifecycleEvent};
use workspace_ui::ThreadRunPhase;

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::shared_state::{FedEvent, FeedBatch, FeedConfig, FeedPoller, FeedWorker, ReleasedRun};

/// How long events of a conversation unknown to the sidebar wait for the
/// sidebar that introduces it before being folded without it.
const UNKNOWN_THREAD_GRACE: Duration = Duration::from_secs(10);

/// Events replayed per conversation when its earlier history is recovered.
const REHYDRATE_EVENT_LIMIT: usize = 100_000;

pub(super) enum SharedFeed {
    Worker(FeedWorker),
    Manual(Box<FeedPoller>),
}

#[derive(Default)]
pub(super) struct SharedFeedState {
    pub feed: Option<SharedFeed>,
    /// Events not folded yet, oldest first.
    pub pending: std::collections::VecDeque<FedEvent>,
    pub blocked_since: Option<Instant>,
    /// How the runs of threads other processes released last ended, by thread.
    pub released_runs: std::collections::BTreeMap<String, Vec<ReleasedRun>>,
    /// The database the feed follows.
    pub db_path: Option<std::path::PathBuf>,
    /// Runs whose events came from other processes.
    pub foreign_runs: std::collections::BTreeSet<String>,
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Follows the shared database on a background thread; `own_stream` is the
    /// stream this process persists under, whose rows are already shown live.
    /// Call after [`Self::restore_history`], which fixes where following starts.
    pub fn with_shared_feed(
        mut self,
        db_path: std::path::PathBuf,
        own_stream: String,
        repaint: Option<Arc<dyn Fn() + Send + Sync>>,
    ) -> Self {
        self.shared_feed.db_path = Some(db_path.clone());
        let config = self.feed_config(db_path, own_stream);
        match FeedPoller::open(config).and_then(|poller| {
            FeedWorker::spawn(poller, repaint)
                .map_err(|error| storage::StorageError::Io(error.to_string()))
        }) {
            Ok(worker) => self.shared_feed.feed = Some(SharedFeed::Worker(worker)),
            Err(error) => {
                tracing::warn!(%error, "following other windows is unavailable");
            }
        }
        self
    }

    /// Like [`Self::with_shared_feed`], but nothing runs in the background:
    /// [`Self::poll_shared_feed`] polls once. For headless tests.
    pub fn with_manual_shared_feed(
        mut self,
        db_path: std::path::PathBuf,
        own_stream: String,
    ) -> Result<Self, storage::StorageError> {
        self.shared_feed.db_path = Some(db_path.clone());
        let config = self.feed_config(db_path, own_stream);
        self.shared_feed.feed = Some(SharedFeed::Manual(Box::new(FeedPoller::open(config)?)));
        Ok(self)
    }

    fn feed_config(&self, db_path: std::path::PathBuf, own_stream: String) -> FeedConfig {
        FeedConfig {
            db_path,
            own_stream,
            cursor: self.shown_through_event,
            sidebar_path: self.sidebar_path.clone(),
        }
    }

    /// Polls a manual feed once (or takes what a background one has queued)
    /// and shows the result. Returns the number of events that arrived.
    pub fn poll_shared_feed(&mut self) -> usize {
        let mut arrived = 0;
        let batches = match &mut self.shared_feed.feed {
            Some(SharedFeed::Manual(poller)) => match poller.poll_once() {
                Ok(batch) => vec![batch],
                Err(error) => {
                    tracing::warn!(%error, "shared event feed poll failed");
                    Vec::new()
                }
            },
            Some(SharedFeed::Worker(worker)) => {
                std::iter::from_fn(|| worker.try_next()).take(8).collect()
            }
            None => Vec::new(),
        };
        for batch in batches {
            arrived += batch.events.len();
            self.apply_shared_batch(batch);
        }
        arrived
    }

    /// Frame hook: shows queued batches and keeps the worker's cadence informed.
    pub(super) fn drain_shared_feed(&mut self) {
        let active = self.foreign_work_active();
        if let Some(SharedFeed::Worker(worker)) = &self.shared_feed.feed {
            worker.set_foreign_active(active);
        } else {
            return;
        }
        self.poll_shared_feed();
        self.retry_pending_shared_events();
    }

    /// Whether a run another process executes is still going, which keeps
    /// following on its fast cadence.
    pub fn foreign_work_active(&self) -> bool {
        self.shared_feed.foreign_runs.iter().any(|run| {
            matches!(
                self.phases.get(run),
                Some(ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting)
            )
        })
    }

    /// Sidebar first, so a thread is known before its events arrive; then the events.
    fn apply_shared_batch(&mut self, batch: FeedBatch) {
        let (stamp_changed, pending) = {
            let shared = self.shared_sidebar.borrow();
            (
                batch
                    .sidebar_stamp
                    .as_ref()
                    .is_some_and(|stamp| Some(stamp) != shared.stamp()),
                shared.import_pending(),
            )
        };
        if stamp_changed || pending {
            self.import_shared_sidebar();
        }
        self.shared_feed.pending.extend(batch.events);
        for thread in batch.released {
            self.shared_feed
                .released_runs
                .insert(thread.thread_id, thread.runs);
        }
        self.fold_pending_shared();
    }

    /// Folds the queued events of other processes in order, stopping at one that
    /// waits for the sidebar. Run once per frame while events are queued.
    pub fn retry_pending_shared_events(&mut self) {
        if !self.shared_feed.pending.is_empty() {
            self.fold_pending_shared();
        }
    }

    fn fold_pending_shared(&mut self) {
        // Events no conversation owns are dropped, never shown in the open one.
        let outer = self.transcripts.set_foreign_scope(true);
        let mut imported = false;
        while let Some(fed) = self.shared_feed.pending.front() {
            if self.waits_for_sidebar(&fed.event) {
                if !imported {
                    imported = true;
                    self.import_shared_sidebar();
                    continue;
                }
                let since = *self
                    .shared_feed
                    .blocked_since
                    .get_or_insert_with(Instant::now);
                if since.elapsed() < UNKNOWN_THREAD_GRACE {
                    break;
                }
            }
            self.shared_feed.blocked_since = None;
            if let Some(fed) = self.shared_feed.pending.pop_front() {
                self.fold_shared_event(&fed.event);
                self.shown_through_event = self.shown_through_event.max(fed.id);
            }
        }
        self.transcripts.set_foreign_scope(outer);
        if self.shared_feed.pending.is_empty() {
            self.shared_feed.blocked_since = None;
            self.shared_feed.released_runs.clear();
        }
    }

    /// A chat run of a conversation the sidebar does not hold yet cannot be
    /// attributed; it waits for the sidebar that introduces the conversation.
    fn waits_for_sidebar(&self, event: &Event) -> bool {
        let EventKind::Lifecycle(LifecycleEvent::AgentRunStarted {
            agent_name,
            parent_run_id: None,
            ..
        }) = &event.kind
        else {
            return false;
        };
        let Some(chat) = agent_name.strip_prefix("chat:") else {
            return false;
        };
        let thread = chat.split_once(':').map_or(chat, |(_, thread)| thread);
        !self
            .sidebar
            .threads
            .iter()
            .any(|known| known.id.to_string() == thread)
    }

    /// The display half of `fold_event`: the projections a restart's replay
    /// also builds.
    fn fold_shared_event(&mut self, event: &Event) {
        if let EventKind::Lifecycle(
            LifecycleEvent::AgentRunStarted { run_id, .. }
            | LifecycleEvent::AgentRunStateChanged { run_id, .. },
        ) = &event.kind
        {
            self.shared_feed.foreign_runs.insert(run_id.clone());
        }
        self.fold_user_question(event);
        self.apply_conversation_event(event);
        self.bind_goal_event(event);
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged { run_id, to, .. }) => {
                self.phases.insert(run_id.clone(), super::frame::phase(*to));
            }
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) => self.seed_branches(Some(&workspace_ui::ForkPoint {
                run_id: run_id.clone(),
                context_len: *context_len,
            })),
            EventKind::Ownership(ownership) => {
                self.ownership_status.invalidate();
                if matches!(
                    ownership.action,
                    event_bus::OwnershipAction::Released | event_bus::OwnershipAction::Handoff
                ) {
                    self.settle_released_thread(&ownership.thread_id);
                }
            }
            _ => {}
        }
        self.telemetry.apply_event(event);
        self.ledger.apply(event);
        self.durable_tasks.apply_event(event);
        self.seed_system_notification(event);
        self.receipt_reconciliation.dirty = true;
    }

    /// An owner that released a thread executes none of its runs any more.
    /// Called where the release sits in the event order, so only the runs
    /// shown as active at that point end: as their last snapshot says, else
    /// Stopped. Runs a later owner starts are folded afterwards.
    fn settle_released_thread(&mut self, thread_id: &str) {
        if self.thread_owned_here(thread_id) {
            return;
        }
        let ended = self.shared_feed.released_runs.get(thread_id);
        let active: Vec<String> = self
            .phases
            .iter()
            .filter(|(run, phase)| {
                matches!(
                    phase,
                    ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting
                ) && self.thread_for_run(run).as_deref() == Some(thread_id)
            })
            .map(|(run, _)| run.clone())
            .collect();
        for run in active {
            let settled = ended
                .and_then(|runs| runs.iter().find(|ended| ended.run_id == run))
                .and_then(|ended| ended.terminal_phase.as_deref());
            let settled = match settled {
                Some("Done") => ThreadRunPhase::Done,
                Some("Error") => ThreadRunPhase::Error,
                _ => ThreadRunPhase::Stopped,
            };
            self.phases.insert(run.clone(), settled);
            self.transcripts.interrupt_run_tools(&run);
        }
    }

    /// Conversations that just became known and whose runs were already shown
    /// without one: replay those runs' events from the database so the
    /// conversation's transcript has what happened before it was known.
    /// Only events already reflected (up to `shown_through_event`) are replayed;
    /// the later ones are still to be folded by the feed, exactly once.
    pub(super) fn rehydrate_threads(&mut self, threads: &[(String, Vec<String>)]) {
        let path = self.shared_feed.db_path.clone().or_else(|| {
            self.memory
                .config
                .as_ref()
                .map(|config| config.db_path.clone())
        });
        let Some(path) = path else {
            tracing::warn!(
                "earlier events of newly known conversations cannot be loaded: no database"
            );
            return;
        };
        let db = match storage::Database::open_read_only(&path) {
            Ok(db) => db,
            Err(error) => {
                tracing::warn!(%error, "earlier events of newly known conversations cannot be loaded");
                return;
            }
        };
        let outer = self.transcripts.set_foreign_scope(true);
        for (thread, runs) in threads {
            let (events, truncated) = match db.events_for_runs(runs, 0, REHYDRATE_EVENT_LIMIT) {
                Ok(found) => found,
                Err(error) => {
                    tracing::warn!(%error, thread, "earlier events of a conversation cannot be loaded");
                    continue;
                }
            };
            if truncated {
                tracing::warn!(thread, "earlier events of a conversation were cut off");
            }
            for run in runs {
                self.transcripts.forget_run(run);
            }
            let mut messages: Vec<_> = self
                .history
                .iter()
                .filter(|message| &message.thread_id == thread && message.shown_as_sent())
                .cloned()
                .collect();
            messages.sort_by_key(|message| message.at);
            let mut messages = messages.into_iter().peekable();
            let shown_through = self.shown_through_event;
            for stored in events
                .into_iter()
                .filter(|stored| stored.id <= shown_through)
            {
                while messages
                    .peek()
                    .is_some_and(|message| message.at <= stored.event.meta.wall_clock)
                {
                    if let Some(message) = messages.next() {
                        self.restore_user_message(message);
                    }
                }
                self.apply_conversation_event(&stored.event);
            }
            for message in messages {
                self.restore_user_message(message);
            }
        }
        self.transcripts.set_foreign_scope(outer);
    }

    /// After a restart's replay, runs still shown as active belong to other
    /// processes: nothing of this one runs yet.
    pub(super) fn note_restored_active_runs(&mut self) {
        let active = self.phases.iter().filter(|(_, phase)| {
            matches!(
                phase,
                ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting
            )
        });
        self.shared_feed
            .foreign_runs
            .extend(active.map(|(run, _)| run.clone()));
    }

    fn thread_owned_here(&self, thread_id: &str) -> bool {
        self.ownership
            .as_ref()
            .is_some_and(|host| host.owned_permit(thread_id).is_ok())
    }

    /// Where the replay of a restart stopped, so following starts after it.
    pub(super) fn note_replayed_event(&mut self, id: i64) {
        self.shown_through_event = self.shown_through_event.max(id);
    }
}
