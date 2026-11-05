//! Pull requests in the Workbench (ADR 0043 Phase 1, PR5): what the bar, the badge and the chips
//! show, and the operations behind them.
//!
//! The database is the source of what is shown. A change reaches the display as an event (from
//! this process's runtime, or from another window through the shared feed); the event only says
//! *which* thread and pull request to read again, and the read runs off the UI thread. A window
//! that does not track a thread shows what the owner stored and does nothing else: it fetches
//! nothing, republishes no event and raises no notification. Only an event this process's own
//! runtime published can raise one.
//!
//! Link, Unlink, Refresh and Confirm take the thread's write mode the way chat does
//! ([`WorkbenchState::chat_permit`]): they start or claim the lease when nobody holds it, which
//! is how a restarted process resumes polling, and refuse when another window holds it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use event_bus::{Event, EventKind, PullRequestEvent, PullRequestRef};
use storage::Database;
use storage::pull_requests::PullRequestKey;

use super::WorkbenchState;
use super::lanes::{Spawner, thread_spawner};
use crate::model::commands::WorkbenchCommand;
use crate::model::pull_requests::{PullRequestAction, PullRequestsModel, ThreadPullRequest};
use crate::model::tasks::AgentRunSource;

/// Reads reading the database at once.
const MAX_IN_FLIGHT: usize = 4;
/// Notification keys remembered to keep one transition from notifying twice.
const NOTIFIED_KEPT: usize = 256;

/// Reads the pull request tables. Blocking: it always runs on a worker thread.
pub trait PullRequestReader: Send + Sync + 'static {
    /// One thread's lifecycle, link, snapshot and candidates. `tracked_here` is left false for
    /// the caller to fill in.
    ///
    /// # Errors
    /// A message when the database cannot be read.
    fn load(&self, thread_id: &str) -> Result<ThreadPullRequest, String>;

    /// The threads that have an active link now.
    ///
    /// # Errors
    /// A message when the database cannot be read.
    fn linked_threads(&self) -> Result<Vec<String>, String>;
}

/// Reads through a read-only connection to the shared database.
pub struct StoragePullRequestReader {
    db_path: PathBuf,
}

impl StoragePullRequestReader {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Database, String> {
        Database::open_read_only(&self.db_path).map_err(|error| error.to_string())
    }
}

impl PullRequestReader for StoragePullRequestReader {
    fn load(&self, thread_id: &str) -> Result<ThreadPullRequest, String> {
        let fail = |error: storage::StorageError| error.to_string();
        let db = self.open()?;
        let state = db.pull_request_thread_state(thread_id).map_err(fail)?;
        let (facts, fetched_at_ns, failure) = match &state.active_link {
            Some(link) => match db.pull_request_snapshot(&link.key).map_err(fail)? {
                Some(snapshot) => (
                    snapshot.facts,
                    snapshot.fetched_at_ns,
                    snapshot.last_attempt.and_then(|attempt| attempt.failure),
                ),
                None => (None, None, None),
            },
            None => (None, None, None),
        };
        let candidates = db.pull_request_candidates(thread_id, None).map_err(fail)?;
        Ok(ThreadPullRequest {
            thread_id: thread_id.to_owned(),
            lifecycle_revision: state.lifecycle_revision,
            transition_seq: state.transition_seq,
            link: state.active_link,
            local_head: state.local_head,
            facts,
            fetched_at_ns,
            failure,
            candidates,
            tracked_here: false,
        })
    }

    fn linked_threads(&self) -> Result<Vec<String>, String> {
        let db = self.open()?;
        let mut threads = BTreeSet::new();
        let mut after = 0;
        loop {
            let page = db
                .active_pull_request_links(after, 200)
                .map_err(|error| error.to_string())?;
            let Some(last) = page.last() else { break };
            after = last.link_id;
            let full = page.len() >= 200;
            threads.extend(page.into_iter().map(|link| link.thread_id));
            if !full {
                break;
            }
        }
        Ok(threads.into_iter().collect())
    }
}

/// Whether this process holds the lease of a thread (it then polls its pull request).
pub type OwnerProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

type Waker = Arc<dyn Fn() + Send + Sync>;

enum Loaded {
    Thread(String, Result<Box<ThreadPullRequest>, String>),
    Linked(Result<Vec<String>, String>),
    Slug(String, Option<String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanPhase {
    Pending,
    InFlight,
    Done,
}

/// The "Link PR…" dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkDialog {
    pub thread_id: String,
    pub input: String,
    /// `owner/name` of the thread's project, once read; `None` while unknown.
    pub project_slug: Option<String>,
}

/// The pull request side of the Workbench.
pub(super) struct PullRequestState {
    reader: Option<Arc<dyn PullRequestReader>>,
    probe: Option<OwnerProbe>,
    spawner: Spawner,
    waker: Waker,
    waker_installed: bool,
    pub(super) model: PullRequestsModel,
    queued: BTreeSet<String>,
    in_flight: BTreeSet<String>,
    /// Asked for again while a read was running: that read may predate what the request is
    /// about.
    dirty: BTreeSet<String>,
    scan: ScanPhase,
    results: Receiver<Loaded>,
    sender: Sender<Loaded>,
    shown: Option<String>,
    /// Link revisions that ended in this window; a late event for one never notifies.
    ended: BTreeSet<(String, u64)>,
    notified: std::collections::VecDeque<(String, u64, u64, String)>,
    pub(super) dialog: Option<LinkDialog>,
}

impl Default for PullRequestState {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            reader: None,
            probe: None,
            spawner: thread_spawner(),
            waker: Arc::new(|| {}),
            waker_installed: false,
            model: PullRequestsModel::default(),
            queued: BTreeSet::new(),
            in_flight: BTreeSet::new(),
            dirty: BTreeSet::new(),
            scan: ScanPhase::Pending,
            results,
            sender,
            shown: None,
            ended: BTreeSet::new(),
            notified: std::collections::VecDeque::new(),
            dialog: None,
        }
    }
}

impl PullRequestState {
    fn request(&mut self, thread: &str) {
        if self.in_flight.contains(thread) {
            self.dirty.insert(thread.to_owned());
        } else {
            self.queued.insert(thread.to_owned());
        }
    }

    fn matches(key: &PullRequestKey, reference: &PullRequestRef) -> bool {
        key.provider.as_str() == reference.provider
            && key.host == reference.host
            && key.repository == reference.repository
            && key.number == reference.number
    }

    /// Reads again what an event is about: its thread, and every thread showing the same
    /// pull request (the owner of another thread may be the one that published).
    fn invalidate(&mut self, event: &PullRequestEvent) {
        self.request(event.thread_id());
        let reference = event.pull_request();
        let others: Vec<String> = self
            .model
            .linked_views_all()
            .filter(|view| view.key().is_some_and(|key| Self::matches(key, reference)))
            .map(|view| view.thread_id.clone())
            .collect();
        for thread in others {
            self.request(&thread);
        }
    }

    fn remember_notified(&mut self, key: (String, u64, u64, String)) -> bool {
        if self.notified.contains(&key) {
            return false;
        }
        self.notified.push_back(key);
        while self.notified.len() > NOTIFIED_KEPT {
            self.notified.pop_front();
        }
        true
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Show pull requests from `reader` (the shared database) and enable their operations.
    /// `probe` tells whether this process holds a thread; without one every thread reads as
    /// tracked.
    #[must_use]
    pub fn with_pull_requests(
        mut self,
        reader: Arc<dyn PullRequestReader>,
        probe: Option<OwnerProbe>,
    ) -> Self {
        self.pull_requests.reader = Some(reader);
        self.pull_requests.probe = probe;
        self
    }

    /// Run reads through `spawner` instead of a thread each (tests run them inline).
    #[must_use]
    pub fn with_pull_request_spawner(mut self, spawner: Spawner) -> Self {
        self.pull_requests.spawner = spawner;
        self
    }

    /// What the application knows about pull requests.
    #[must_use]
    pub fn pull_request_model(&self) -> &PullRequestsModel {
        &self.pull_requests.model
    }

    /// The open "Link PR…" dialog, if any.
    #[must_use]
    pub fn pull_request_dialog(&self) -> Option<&LinkDialog> {
        self.pull_requests.dialog.as_ref()
    }

    /// Drains finished reads and starts the due ones, once per frame.
    pub(super) fn poll_pull_requests(&mut self, ctx: &egui::Context) {
        if !self.pull_requests.waker_installed {
            let ctx = ctx.clone();
            self.pull_requests.waker = Arc::new(move || ctx.request_repaint());
            self.pull_requests.waker_installed = true;
        }
        self.pump_pull_requests();
    }

    /// [`Self::poll_pull_requests`] without an egui context: what a headless host calls once
    /// per tick.
    pub fn pump_pull_requests(&mut self) {
        if self.pull_requests.reader.is_none() {
            return;
        }
        let enabled = self.sink.pull_requests_enabled();
        if enabled != self.pull_requests.model.enabled {
            self.pull_requests.model.enabled = enabled;
            self.pull_requests.model.clear();
            self.pull_requests.scan = ScanPhase::Pending;
        }
        while let Ok(loaded) = self.pull_requests.results.try_recv() {
            self.apply_loaded_pull_request(loaded);
        }
        if !enabled {
            self.pull_requests.queued.clear();
            return;
        }
        self.refresh_shown_pull_request();
        self.start_pull_request_reads();
    }

    fn refresh_shown_pull_request(&mut self) {
        let Some(active) = self.sidebar.active_thread.as_ref().map(ToString::to_string) else {
            return;
        };
        if self.pull_requests.shown.as_ref() == Some(&active) {
            return;
        }
        self.pull_requests.shown = Some(active.clone());
        self.pull_requests.request(&active);
    }

    fn start_pull_request_reads(&mut self) {
        let Some(reader) = self.pull_requests.reader.clone() else {
            return;
        };
        if self.pull_requests.scan == ScanPhase::Pending {
            self.pull_requests.scan = ScanPhase::InFlight;
            let sender = self.pull_requests.sender.clone();
            let waker = Arc::clone(&self.pull_requests.waker);
            let reader = Arc::clone(&reader);
            (self.pull_requests.spawner)(Box::new(move || {
                let _ = sender.send(Loaded::Linked(reader.linked_threads()));
                waker();
            }));
        }
        while self.pull_requests.in_flight.len() < MAX_IN_FLIGHT
            && let Some(thread) = self.pull_requests.queued.pop_first()
        {
            self.pull_requests.in_flight.insert(thread.clone());
            let sender = self.pull_requests.sender.clone();
            let waker = Arc::clone(&self.pull_requests.waker);
            let reader = Arc::clone(&reader);
            let probe = self.pull_requests.probe.clone();
            (self.pull_requests.spawner)(Box::new(move || {
                let result = reader.load(&thread).map(|mut view| {
                    view.tracked_here = probe.as_ref().is_none_or(|probe| probe(&thread));
                    Box::new(view)
                });
                let _ = sender.send(Loaded::Thread(thread, result));
                waker();
            }));
        }
    }

    fn apply_loaded_pull_request(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Thread(thread, result) => {
                self.pull_requests.in_flight.remove(&thread);
                if self.pull_requests.dirty.remove(&thread) {
                    self.pull_requests.request(&thread);
                }
                match result {
                    Ok(view) if self.pull_requests.model.enabled => {
                        self.pull_requests.model.set(*view);
                    }
                    Ok(_) => {}
                    Err(error) => {
                        tracing::warn!(%thread, %error, "pull request read failed");
                    }
                }
            }
            Loaded::Linked(result) => {
                self.pull_requests.scan = ScanPhase::Done;
                match result {
                    Ok(threads) => {
                        for thread in threads {
                            self.pull_requests.request(&thread);
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "pull request scan failed");
                    }
                }
            }
            Loaded::Slug(thread, slug) => {
                if let Some(dialog) = &mut self.pull_requests.dialog
                    && dialog.thread_id == thread
                {
                    dialog.project_slug = slug;
                }
            }
        }
    }

    /// Folds a pull request event this process's own runtime published: reads again, and raises
    /// the notifications the runtime claimed for it.
    pub(super) fn apply_local_pull_request_event(&mut self, event: &Event) {
        let EventKind::PullRequest(change) = &event.kind else {
            return;
        };
        if let PullRequestEvent::Unlinked {
            thread_id,
            link_revision,
            ..
        } = change
        {
            self.pull_requests
                .ended
                .insert((thread_id.clone(), *link_revision));
        }
        self.pull_requests.invalidate(change);
        if let PullRequestEvent::Changed {
            thread_id,
            link_revision,
            transition_seq,
            pull_request,
            notifications,
            ..
        } = change
        {
            // The link this change was about may have ended while the event waited.
            if self
                .pull_requests
                .ended
                .contains(&(thread_id.clone(), *link_revision))
            {
                return;
            }
            for kind in notifications {
                let key = (
                    thread_id.clone(),
                    *link_revision,
                    *transition_seq,
                    kind.clone(),
                );
                if self.pull_requests.remember_notified(key) {
                    self.raise_pull_request_notification(thread_id, pull_request, kind);
                }
            }
        }
    }

    /// Folds a pull request event another process published: only the display is read again.
    pub(super) fn apply_shared_pull_request_event(&mut self, event: &Event) {
        if let EventKind::PullRequest(change) = &event.kind {
            self.pull_requests.invalidate(change);
        }
    }

    /// An ownership change decides who polls a thread, and so what its bar says about
    /// tracking.
    pub(super) fn invalidate_pull_request_tracking(&mut self, thread: &str) {
        if self.pull_requests.model.linked(thread).is_some() {
            self.pull_requests.request(thread);
        }
    }

    fn raise_pull_request_notification(
        &mut self,
        thread_id: &str,
        reference: &PullRequestRef,
        kind: &str,
    ) {
        use runtime::pull_requests::diff::notification;
        let title = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == thread_id)
            .map(|thread| thread.title.clone());
        let label = format!("{}#{}", reference.repository, reference.number);
        let thread = workspace_ui::ThreadId::new(thread_id);
        let (kind, summary) = match kind {
            notification::CHECKS_FAILED => (
                crate::model::notifications::NotificationKind::PullRequestChecksFailed {
                    thread_id: thread,
                },
                format!("CI failed on {label}"),
            ),
            notification::MERGED => (
                crate::model::notifications::NotificationKind::PullRequestMerged {
                    thread_id: thread,
                },
                format!("{label} merged"),
            ),
            _ => return,
        };
        let summary = match title {
            Some(title) => format!("{summary} · {title}"),
            None => summary,
        };
        self.notifications.push_pull_request(kind, summary);
    }

    /// Takes the thread's write mode for a pull request operation, as chat does. A thread this
    /// window attached read-only is not touched.
    fn pull_request_permit(&mut self, thread: &str) -> bool {
        if self.readonly_threads.contains(thread) {
            self.push_notice("Read-only attach: explicitly Start or Claim before mutating.");
            return false;
        }
        match self.chat_permit(thread) {
            Ok(_) => true,
            Err(notice) => {
                self.push_notice(notice);
                false
            }
        }
    }

    fn submit_pull_request_command(&mut self, command: WorkbenchCommand) {
        self.issued.push(command.clone());
        for event in self.sink.submit(command) {
            self.apply_loop_event(event);
        }
    }

    pub(super) fn show_pull_request_rejection(&mut self, thread_id: &str, reason: &str) {
        tracing::warn!(thread = thread_id, %reason, "pull request operation refused");
        self.push_notice(format!("Pull request: {reason}"));
    }

    /// Runs what the user asked of a pull request surface (a click on the bar, a chip, the
    /// sidebar menu or the dialog). Also the entry for a headless host.
    pub fn apply_pull_request_action(&mut self, action: PullRequestAction) {
        match action {
            PullRequestAction::OpenLinkDialog { thread_id } => self.open_link_dialog(thread_id),
            PullRequestAction::ToggleExpanded { thread_id } => {
                let bar = self.pull_requests.model.bar_mut(&thread_id);
                bar.expanded = !bar.expanded;
            }
            PullRequestAction::Refresh { thread_id } => {
                if self.pull_request_permit(&thread_id) {
                    self.submit_pull_request_command(WorkbenchCommand::RefreshPullRequest {
                        thread_id,
                    });
                }
            }
            PullRequestAction::Unlink { thread_id } => {
                let Some(view) = self.pull_requests.model.linked(&thread_id) else {
                    return;
                };
                let expected_revision = view.lifecycle_revision;
                let ended = view.link.as_ref().map(|link| link.link_revision);
                if self.pull_request_permit(&thread_id) {
                    if let Some(revision) = ended {
                        self.pull_requests
                            .ended
                            .insert((thread_id.clone(), revision));
                    }
                    self.submit_pull_request_command(WorkbenchCommand::UnlinkPullRequest {
                        thread_id,
                        expected_revision,
                    });
                }
            }
            PullRequestAction::Confirm {
                thread_id,
                candidate_id,
            } => {
                let expected_revision = self
                    .pull_requests
                    .model
                    .view(&thread_id)
                    .map_or(0, |view| view.lifecycle_revision);
                if self.pull_request_permit(&thread_id) {
                    self.submit_pull_request_command(
                        WorkbenchCommand::ConfirmPullRequestCandidate {
                            thread_id,
                            candidate_id,
                            expected_revision,
                        },
                    );
                }
            }
            PullRequestAction::Dismiss {
                thread_id,
                candidate_id,
            } => self.submit_pull_request_command(WorkbenchCommand::DismissPullRequestCandidate {
                thread_id,
                candidate_id,
            }),
            PullRequestAction::Link { thread_id, locator } => {
                let Some(view) = self.pull_requests.model.view(&thread_id) else {
                    // The revision to name is not read yet; it is now.
                    self.pull_requests.request(&thread_id);
                    self.push_notice("Pull request: still reading the thread's link; try again");
                    return;
                };
                let expected_revision = view.lifecycle_revision;
                if self.pull_request_permit(&thread_id) {
                    self.pull_requests.dialog = None;
                    self.submit_pull_request_command(WorkbenchCommand::LinkPullRequest {
                        thread_id,
                        locator,
                        expected_revision,
                    });
                }
            }
        }
    }

    /// Opens the "Link PR…" dialog for a thread. The thread is read first (its revision is
    /// what Link must name) and its project's repository is looked up off the UI thread.
    pub fn open_link_dialog(&mut self, thread_id: String) {
        self.pull_requests.request(&thread_id);
        let root = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == thread_id)
            .and_then(|thread| {
                self.sidebar
                    .projects
                    .iter()
                    .find(|project| project.id == thread.project_id)
            })
            .map(|project| project.repo_root.clone());
        self.pull_requests.dialog = Some(LinkDialog {
            thread_id: thread_id.clone(),
            input: String::new(),
            project_slug: None,
        });
        if let Some(root) = root {
            let sender = self.pull_requests.sender.clone();
            let waker = Arc::clone(&self.pull_requests.waker);
            (self.pull_requests.spawner)(Box::new(move || {
                let slug = crate::runtime_sink::derive_repo_slug(&root);
                let _ = sender.send(Loaded::Slug(thread_id, Some(slug)));
                waker();
            }));
        }
    }

    /// Draws the dialog and runs its answer.
    pub(super) fn render_link_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.pull_requests.dialog else {
            return;
        };
        let loaded = self.pull_requests.model.is_loaded(&dialog.thread_id);
        match crate::panes::pull_request_dialog::link_dialog(ctx, dialog, loaded) {
            Some(crate::panes::pull_request_dialog::LinkDialogAction::Link(locator)) => {
                let thread_id = dialog.thread_id.clone();
                self.apply_pull_request_action(PullRequestAction::Link { thread_id, locator });
            }
            Some(crate::panes::pull_request_dialog::LinkDialogAction::Close) => {
                self.pull_requests.dialog = None;
            }
            None => {}
        }
    }

    /// Derives a thread from another: the child may take the parent's pull request link. Sent
    /// where the child is created, before it has any run.
    pub(super) fn inherit_pull_request_link(
        &mut self,
        parent: &workspace_ui::ThreadId,
        child: &workspace_ui::ThreadId,
        kind: workspace_ui::LineageKind,
    ) {
        let kind = match kind {
            workspace_ui::LineageKind::Fork => crate::model::commands::PullRequestDerivation::Fork,
            workspace_ui::LineageKind::Rewind => {
                crate::model::commands::PullRequestDerivation::Rewind
            }
        };
        self.submit_pull_request_command(WorkbenchCommand::InheritPullRequest {
            parent_thread_id: parent.to_string(),
            child_thread_id: child.to_string(),
            kind,
        });
    }

    /// Threads' badges for the sidebar.
    pub(super) fn pull_request_badges(
        &self,
    ) -> BTreeMap<workspace_ui::ThreadId, crate::model::pull_requests::PullRequestBadge> {
        self.pull_requests
            .model
            .linked_views()
            .filter_map(|view| {
                Some((
                    workspace_ui::ThreadId::new(view.thread_id.clone()),
                    crate::model::pull_requests::badge(view)?,
                ))
            })
            .collect()
    }
}
