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
use crate::shared_state::{
    FeedBatch, FeedConfig, FeedPoller, FeedWorker, ReleasedRun, ReleasedThread,
};

/// How long events of a conversation unknown to the sidebar wait for the
/// sidebar that introduces it before being folded without it.
const UNKNOWN_THREAD_GRACE: Duration = Duration::from_secs(10);

pub(super) enum SharedFeed {
    Worker(FeedWorker),
    Manual(Box<FeedPoller>),
}

#[derive(Default)]
pub(super) struct SharedFeedState {
    pub feed: Option<SharedFeed>,
    /// Events not folded yet, oldest first.
    pub pending: std::collections::VecDeque<Event>,
    pub blocked_since: Option<Instant>,
    pub released: Vec<ReleasedThread>,
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
        let config = self.feed_config(db_path, own_stream);
        self.shared_feed.feed = Some(SharedFeed::Manual(Box::new(FeedPoller::open(config)?)));
        Ok(self)
    }

    fn feed_config(&self, db_path: std::path::PathBuf, own_stream: String) -> FeedConfig {
        FeedConfig {
            db_path,
            own_stream,
            cursor: self.restored_event_cursor,
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
        if !self.shared_feed.pending.is_empty() {
            self.fold_pending_shared();
        }
    }

    /// Whether a run another process executes is still going.
    fn foreign_work_active(&self) -> bool {
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
        self.shared_feed.released.extend(batch.released);
        self.fold_pending_shared();
    }

    fn fold_pending_shared(&mut self) {
        let active = self.sidebar.active_thread.as_ref().map(ToString::to_string);
        // Events no run binding attributes would otherwise land in the open
        // thread's transcript.
        self.transcripts.select_thread(None);
        let mut imported = false;
        while let Some(event) = self.shared_feed.pending.front() {
            if self.waits_for_sidebar(event) {
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
            if let Some(event) = self.shared_feed.pending.pop_front() {
                self.fold_shared_event(&event);
            }
        }
        self.transcripts.select_thread(active);
        if self.shared_feed.pending.is_empty() {
            self.shared_feed.blocked_since = None;
            let released = std::mem::take(&mut self.shared_feed.released);
            self.settle_released_threads(released);
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
            EventKind::Ownership(_) => self.ownership_status.invalidate(),
            _ => {}
        }
        self.telemetry.apply_event(event);
        self.ledger.apply(event);
        self.durable_tasks.apply_event(event);
        self.seed_system_notification(event);
        self.receipt_reconciliation.dirty = true;
    }

    /// An owner that released a thread executes none of its runs any more:
    /// runs still shown as active end as their last snapshot says, else Stopped.
    fn settle_released_threads(&mut self, released: Vec<ReleasedThread>) {
        for thread in released {
            if self.thread_owned_here(&thread.thread_id) {
                continue;
            }
            let mut runs = thread.runs;
            if let Some(record) = self
                .sidebar
                .threads
                .iter()
                .find(|record| record.id.to_string() == thread.thread_id)
            {
                let extra: Vec<ReleasedRun> = record
                    .run_ids
                    .iter()
                    .filter(|run_id| !runs.iter().any(|run| &run.run_id == *run_id))
                    .map(|run_id| ReleasedRun {
                        run_id: run_id.clone(),
                        terminal_phase: None,
                    })
                    .collect();
                runs.extend(extra);
            }
            for run in runs {
                let Some(phase) = self.phases.get_mut(&run.run_id) else {
                    continue;
                };
                if !matches!(
                    phase,
                    ThreadRunPhase::Running | ThreadRunPhase::Pending | ThreadRunPhase::Waiting
                ) {
                    continue;
                }
                *phase = match run.terminal_phase.as_deref() {
                    Some("Done") => ThreadRunPhase::Done,
                    Some("Error") => ThreadRunPhase::Error,
                    _ => ThreadRunPhase::Stopped,
                };
                self.transcripts.interrupt_run_tools(&run.run_id);
            }
        }
    }

    fn thread_owned_here(&self, thread_id: &str) -> bool {
        self.ownership
            .as_ref()
            .is_some_and(|host| host.owned_permit(thread_id).is_ok())
    }

    /// Where the replay of a restart stopped, so following starts after it.
    pub(super) fn note_replayed_event(&mut self, id: i64) {
        self.restored_event_cursor = self.restored_event_cursor.max(id);
    }
}
