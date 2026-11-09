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

pub(super) enum SharedFeed {
    Worker(FeedWorker),
    Manual(Box<FeedPoller>),
}

#[derive(Default)]
pub(super) struct SharedFeedState {
    pub harness: super::harness_recovery::HarnessReplay,
    /// Raw log progress, including local rows excluded from the foreign event list.
    pub scanned_through: i64,
    pub feed: Option<SharedFeed>,
    /// Response slices may span feed batches or a restart's replay boundary.
    pub parts: event_bus::MessagePartAssembler,
    /// Events not folded yet, oldest first.
    pub pending: std::collections::VecDeque<FedEvent>,
    /// Rows of other processes that cannot be shown, by log id: each is put in
    /// the transcripts where it stands among the events once they are folded.
    pub failures: std::collections::BTreeMap<i64, crate::body::EventFailure>,
    pub blocked_since: Option<Instant>,
    /// How the runs of threads other processes released last ended, by thread.
    pub released_runs: std::collections::BTreeMap<String, Vec<ReleasedRun>>,
    /// The database the feed follows.
    pub db_path: Option<std::path::PathBuf>,
    /// Conversations whose earlier history is being read on the resolver's
    /// worker. Events of other processes wait until it has been applied, so
    /// the replay and the feed never both show the same event.
    pub rehydrating: std::collections::BTreeSet<String>,
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
        let meter = self.feed_meter();
        match FeedPoller::open(config).and_then(|poller| {
            let poller = poller.with_meter(meter);
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
        let poller = FeedPoller::open(config)?.with_meter(self.feed_meter());
        self.shared_feed.feed = Some(SharedFeed::Manual(Box::new(poller)));
        Ok(self)
    }

    /// Where the feed's reads at once are counted: the body resolver's meter.
    fn feed_meter(&self) -> crate::body::ImmediateMeter {
        self.body_resolver
            .as_ref()
            .map(crate::body::BodyResolver::meter)
            .unwrap_or_default()
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
        if let Some(through) = batch.scanned_through {
            self.shared_feed.scanned_through = self.shared_feed.scanned_through.max(through);
        }
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
        for failure in batch.failures {
            self.shared_feed.failures.insert(failure.id, failure);
        }
        for thread in batch.released {
            self.shared_feed
                .released_runs
                .insert(thread.thread_id, thread.runs);
        }
        self.fold_pending_shared();
    }

    /// Whether events of other processes are held back, waiting for the sidebar.
    pub(super) fn shared_events_held(&self) -> bool {
        !self.shared_feed.pending.is_empty() || !self.shared_feed.failures.is_empty()
    }

    /// Folds the queued events of other processes in order, stopping at one that
    /// waits for the sidebar. Run once per frame while events are queued.
    pub fn retry_pending_shared_events(&mut self) {
        if !self.shared_feed.pending.is_empty() || !self.shared_feed.failures.is_empty() {
            self.fold_pending_shared();
        }
    }

    fn fold_pending_shared(&mut self) {
        self.apply_rows_results();
        // Events no conversation owns are dropped, never shown in the open one.
        let outer = self.transcripts.set_foreign_scope(true);
        let mut imported = false;
        while let Some(fed) = self.shared_feed.pending.front() {
            if !self.shared_feed.rehydrating.is_empty() {
                break;
            }
            let next_id = fed.id;
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
            self.fold_shared_failures(Some(next_id));
            if let Some(fed) = self.shared_feed.pending.pop_front() {
                self.shared_feed
                    .harness
                    .show_through(&mut self.transcripts, fed.event.meta.wall_clock);
                self.transcripts.set_source(Some((fed.id, fed.id)));
                self.fold_shared_event(&fed.event);
                self.transcripts.set_source(None);
                self.shown_through_event = self.shown_through_event.max(fed.id);
            }
        }
        if self.shared_feed.pending.is_empty() && self.shared_feed.rehydrating.is_empty() {
            self.fold_shared_failures(None);
            self.shared_feed.harness.show_tail_through(
                &mut self.transcripts,
                self.shared_feed
                    .scanned_through
                    .max(self.shown_through_event),
            );
        }
        self.transcripts.set_foreign_scope(outer);
        self.publish_shown_through();
        if self.shared_feed.pending.is_empty() {
            self.shared_feed.blocked_since = None;
            self.shared_feed.released_runs.clear();
        }
    }

    /// Shows the rows of other processes that could not be read and that stand
    /// before event `before` (all of them with `None`), each as a gap in the
    /// transcripts of its run, where the events around it put it.
    fn fold_shared_failures(&mut self, before: Option<i64>) {
        while let Some(entry) = self.shared_feed.failures.first_entry() {
            if before.is_some_and(|limit| *entry.key() > limit) {
                break;
            }
            let failure = entry.remove();
            if let Some(at) = failure.wall_clock {
                self.shared_feed
                    .harness
                    .show_through(&mut self.transcripts, at);
            }
            // What the run was streaming cannot continue across the gap, as in
            // a restart's replay: the next slice would be the rest of nothing.
            if let Some(run) = &failure.run_id {
                let boundary = Event::new(event_bus::MessageEvent::StreamRetried {
                    run_id: Some(run.clone()),
                });
                let _ = self.shared_feed.parts.accept(&boundary);
            }
            let mut shown = std::collections::BTreeMap::new();
            self.show_unreadable_event(&failure, &mut shown);
            for (id, event) in shown {
                self.unreadable_history_events.entry(id).or_insert(event);
            }
            if failure.run_id.is_none() {
                self.notifications.push_history_gap(&[failure.id]);
            }
            self.shown_through_event = self.shown_through_event.max(failure.id);
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
        let Some(assembled) = self.shared_feed.parts.accept(event) else {
            return;
        };
        let event: &Event = &assembled;
        if let EventKind::Lifecycle(
            LifecycleEvent::AgentRunStarted { run_id, .. }
            | LifecycleEvent::AgentRunStateChanged { run_id, .. },
        ) = &event.kind
        {
            self.shared_feed.foreign_runs.insert(run_id.clone());
        }
        self.fold_user_question(event);
        self.apply_conversation_event(event);
        self.apply_run_display_event(event);
        self.bind_goal_event(event);
        match &event.kind {
            EventKind::Lifecycle(LifecycleEvent::TurnCompleted {
                run_id,
                context_len,
            }) => self.seed_branches(Some(&workspace_ui::ForkPoint {
                run_id: run_id.clone(),
                context_len: *context_len,
            })),
            EventKind::Ownership(ownership) => {
                self.ownership_status.invalidate();
                self.external_work.invalidate();
                self.invalidate_pull_request_tracking(&ownership.thread_id);
                if matches!(
                    ownership.action,
                    event_bus::OwnershipAction::Released | event_bus::OwnershipAction::Handoff
                ) {
                    self.settle_released_thread(&ownership.thread_id);
                }
            }
            // Display only: read the stored snapshot again. Nothing is republished, fetched or
            // notified for another process's change.
            EventKind::PullRequest(_) => self.apply_shared_pull_request_event(event),
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
        for run in &active {
            let settled = ended
                .and_then(|runs| runs.iter().find(|ended| ended.run_id == run.as_str()))
                .and_then(|ended| ended.terminal_phase.as_deref());
            let settled = match settled {
                Some("Done") => ThreadRunPhase::Done,
                Some("Error") => ThreadRunPhase::Error,
                _ => ThreadRunPhase::Stopped,
            };
            self.phases.insert(run.clone(), settled);
            self.transcripts.interrupt_run_tools(run);
        }
        for run in active {
            self.park_completed_subagent(&run);
        }
    }

    /// Conversations that just became known and whose runs were already shown
    /// without one: replay those runs' events from the database so the
    /// conversation's transcript has what happened before it was known.
    /// Only events already reflected (up to `shown_through_event`) are replayed;
    /// the later ones are still to be folded by the feed, exactly once.
    pub(super) fn rehydrate_threads(&mut self, threads: &[(String, Vec<String>)]) {
        let Some(resolver) = self.body_resolver.clone() else {
            tracing::warn!(
                "earlier events of newly known conversations cannot be loaded: no body reader"
            );
            return;
        };
        let through = self.shown_through_event;
        for (thread, runs) in threads {
            // The worker reads them; the result replaces what is shown for the
            // runs in one step, when it arrives, and the feed waits until then.
            self.shared_feed.rehydrating.insert(thread.clone());
            resolver.request_rows(
                crate::body::RowsPurpose::Rehydrate {
                    thread: thread.clone(),
                },
                runs.clone(),
                0,
                through,
            );
        }
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

    /// Threads with a run that other processes started or changed and that is
    /// still Running or Pending: the shared feed's evidence of work elsewhere,
    /// before ownership is taken into account.
    fn foreign_active_threads(&self) -> std::collections::BTreeSet<String> {
        if self.shared_feed.foreign_runs.is_empty() {
            return std::collections::BTreeSet::new();
        }
        self.sidebar
            .threads
            .iter()
            .filter(|thread| {
                thread.run_ids.iter().any(|run| {
                    self.shared_feed.foreign_runs.contains(run)
                        && matches!(
                            self.phases.get(run),
                            Some(ThreadRunPhase::Running | ThreadRunPhase::Pending)
                        )
                })
            })
            .map(|thread| thread.id.to_string())
            .collect()
    }

    /// Frame hook: lets the background probe of who holds those threads pick up
    /// its answer and, when due, look again. Never reads the registry itself.
    pub(super) fn refresh_external_work(&mut self, repaint: Option<&egui::Context>) {
        let candidates = self.foreign_active_threads();
        self.external_work.refresh(&candidates, repaint);
    }

    /// Runs one probe round now and waits for it, for headless runs that draw
    /// no frames.
    pub fn observe_external_work_now(&mut self) {
        self.refresh_external_work(None);
        self.external_work.wait_current();
    }

    /// Threads another process is working on, shown from what the shared feed
    /// already folded: a run of the thread that other processes started or
    /// changed and that is still Running or Pending, in a thread whose record
    /// does not name this process as holder. A run merely missing from this
    /// process's runtime, one waiting for an answer, and a thread whose holder
    /// could not be read are not enough. Draws on the last background probe;
    /// nothing is claimed, renewed or written, and no frame reads the registry.
    pub fn externally_working_threads(&self) -> std::collections::BTreeSet<workspace_ui::ThreadId> {
        let candidates = self.foreign_active_threads();
        if candidates.is_empty() {
            return std::collections::BTreeSet::new();
        }
        self.sidebar
            .threads
            .iter()
            .filter(|thread| {
                let id = thread.id.to_string();
                candidates.contains(&id) && self.external_work.not_owned_here(&id)
            })
            .map(|thread| thread.id.clone())
            .collect()
    }

    fn thread_owned_here(&self, thread_id: &str) -> bool {
        self.ownership
            .as_ref()
            .is_some_and(|host| host.owned_permit(thread_id).is_ok())
    }

    /// Where the replay of a restart stopped, so following starts after it.
    pub(super) fn note_replayed_event(&mut self, id: i64) {
        self.shown_through_event = self.shown_through_event.max(id);
        self.publish_shown_through();
    }

    /// Tells the body resolver how far the history reaches, for the reads of
    /// unreadable events it starts later.
    fn publish_shown_through(&self) {
        if let Some(resolver) = &self.body_resolver {
            resolver.note_shown_through(self.shown_through_event);
        }
    }
}
