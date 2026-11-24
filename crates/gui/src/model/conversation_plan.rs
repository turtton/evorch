//! What the GUI shows of a conversation's plan: the progress band above the
//! composer and the ConversationPlan panel (tree, selected task, change history).
//!
//! The plan is only read. [`PlanState`] decides which typed `execution` queries
//! to send and folds their answers; the in-process Workbench sends them through
//! [`ConversationPlanModel`] on a worker thread, the core client through its
//! feature tags. A change notification only names a revision: anything at or
//! below what is already known is ignored, and an answer older than what is
//! shown is dropped, so a late notification or answer never moves the screen
//! back to an older plan. Progress figures are the server's
//! [`PlanProgress`]; nothing here recomputes them.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};

use workbench_protocol::commands::ThreadTarget;
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::ids::ThreadId;
use workbench_protocol::namespaces::execution::plan::{
    PlanHistoryEntry, PlanHistoryPage, PlanHistoryQuery, PlanNodeView, PlanProgress, PlanSummary,
    PlanTaskQuery, PlanTaskRead, PlanTreePage, PlanTreeQuery,
};
use workbench_protocol::namespaces::{ExecutionQuery, ExecutionReply};
use workbench_protocol::wire::Revision;

#[cfg(test)]
mod tests;

/// History entries on one page of the panel.
pub const HISTORY_PAGE: u32 = 10;
/// Nodes asked for per tree page (the protocol's maximum).
pub const TREE_PAGE: u32 = 100;

/// The four reads, one in flight at most each.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadKind {
    Summary,
    Tree,
    Task,
    History,
}

const KINDS: [ReadKind; 4] = [
    ReadKind::Summary,
    ReadKind::Tree,
    ReadKind::Task,
    ReadKind::History,
];

impl ReadKind {
    const fn index(self) -> usize {
        match self {
            Self::Summary => 0,
            Self::Tree => 1,
            Self::Task => 2,
            Self::History => 3,
        }
    }

    /// Whether the read is needed only while the panel is on screen.
    const fn detail(self) -> bool {
        !matches!(self, Self::Summary)
    }
}

/// A query to send; its answer goes back to [`PlanState::apply`] with the same
/// `kind` and `generation`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRequest {
    pub kind: ReadKind,
    pub generation: u64,
    pub query: ExecutionQuery,
}

/// Why a read gave no plan view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadFailure {
    /// The plan changed under a tree cursor: read the tree again from the start.
    Conflict,
    /// The task (or the conversation) is gone.
    NotFound,
    Other(String),
}

impl From<&WorkbenchError> for ReadFailure {
    fn from(error: &WorkbenchError) -> Self {
        match error.code {
            ErrorCode::Conflict => Self::Conflict,
            ErrorCode::NotFound => Self::NotFound,
            _ => Self::Other(error.message.clone()),
        }
    }
}

/// The whole tree at one revision, in the server's (depth-first) order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTree {
    pub revision: Revision,
    pub progress: PlanProgress,
    pub nodes: Vec<PlanNodeView>,
}

/// One page of the change history, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanHistory {
    /// The plan revision the page was read at.
    pub revision: Revision,
    /// 0 is the newest page.
    pub page: u32,
    pub pages: u32,
    pub entries: Vec<PlanHistoryEntry>,
}

#[derive(Debug, Default)]
struct TreeRead {
    revision: Option<Revision>,
    cursor: Option<String>,
    nodes: Vec<PlanNodeView>,
}

/// The plan of one conversation as last read, and which reads are due.
#[derive(Debug, Default)]
pub struct PlanState {
    thread: Option<ThreadId>,
    /// Bumped on every thread switch; answers for an older one are dropped.
    generation: u64,
    /// The highest revision known from notifications and answers.
    latest: u64,
    summary: Option<PlanSummary>,
    tree: Option<PlanTree>,
    tree_read: Option<TreeRead>,
    selected: Option<String>,
    task: Option<PlanTaskRead>,
    /// The task the read in flight asks for, so a failure of an old read
    /// never deselects a task chosen since.
    task_read: Option<String>,
    history: Option<PlanHistory>,
    history_page: u32,
    /// The `(page, newest revision)` the history read in flight covers.
    history_read: Option<(u32, u64)>,
    in_flight: [bool; 4],
    stale: [bool; 4],
    error: Option<String>,
}

impl PlanState {
    /// Follows the shown conversation. Switching drops everything read for
    /// the previous one; answers still in flight for it are ignored.
    pub fn set_thread(&mut self, thread: Option<ThreadId>) {
        if self.thread == thread {
            return;
        }
        let generation = self.generation + 1;
        *self = Self {
            thread,
            generation,
            stale: [true; 4],
            ..Self::default()
        };
    }

    pub fn thread(&self) -> Option<&ThreadId> {
        self.thread.as_ref()
    }

    /// The last summary read for the shown conversation.
    pub fn summary(&self) -> Option<&PlanSummary> {
        self.summary.as_ref()
    }

    pub fn tree(&self) -> Option<&PlanTree> {
        self.tree.as_ref()
    }

    pub fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The selected task's detail, once read.
    pub fn task(&self) -> Option<&PlanTaskRead> {
        self.task
            .as_ref()
            .filter(|task| self.selected.as_deref() == Some(task.node.task_id.as_str()))
    }

    pub fn history(&self) -> Option<&PlanHistory> {
        self.history.as_ref()
    }

    pub const fn history_page(&self) -> u32 {
        self.history_page
    }

    /// Pages of history the plan has at the newest known revision (at least 1).
    pub fn history_pages(&self) -> u32 {
        pages_at(self.latest)
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// Whether the plan was written to and is empty now: every task deleted
    /// (revision above 0, no node). A plan never written to has revision 0 and
    /// is not cleared, so it has no history to open.
    pub fn is_cleared(&self) -> bool {
        let progress = self
            .tree
            .as_ref()
            .map(|tree| &tree.progress)
            .or_else(|| self.summary.as_ref().map(|summary| &summary.progress));
        progress.is_some_and(|progress| !is_planned(progress)) && self.latest > 0
    }

    /// A change notification. Only a revision newer than any known one makes
    /// the views stale; an older or repeated one changes nothing.
    pub fn invalidate(&mut self, thread: &ThreadId, revision: Revision) {
        if self.thread.as_ref() == Some(thread) {
            self.bump(revision.0);
        }
    }

    /// Marks everything again to be read, e.g. after a reconnect.
    pub fn refresh(&mut self) {
        self.stale = [true; 4];
        if self
            .tree_read
            .as_ref()
            .is_some_and(|read| read.revision.is_some())
        {
            self.tree_read = None;
        }
    }

    /// The reads in flight will never be answered (a new connection): their
    /// answers are dropped and every view is read again, while what was read
    /// stays on screen.
    pub fn restart_reads(&mut self) {
        self.generation += 1;
        self.in_flight = [false; 4];
        self.stale = [true; 4];
        self.tree_read = None;
        self.task_read = None;
        self.history_read = None;
    }

    pub fn select(&mut self, task_id: &str) {
        if self.selected.as_deref() != Some(task_id) {
            self.selected = Some(task_id.to_owned());
            self.task = None;
            self.stale[ReadKind::Task.index()] = true;
        }
    }

    pub fn set_history_page(&mut self, page: u32) {
        let page = page.min(self.history_pages() - 1);
        if page != self.history_page {
            self.history_page = page;
            self.stale[ReadKind::History.index()] = true;
        }
    }

    fn bump(&mut self, revision: u64) {
        if revision <= self.latest {
            return;
        }
        self.latest = revision;
        let behind = |shown: Option<Revision>| shown.is_none_or(|shown| shown.0 < revision);
        self.stale[0] |= behind(self.summary.as_ref().map(|s| s.revision));
        self.stale[1] |= behind(self.tree.as_ref().map(|t| t.revision));
        self.stale[2] |= self.selected.is_some() && behind(self.task.as_ref().map(|t| t.revision));
        self.stale[3] |= behind(self.history.as_ref().map(|h| h.revision));
    }

    /// The reads due now. Panel reads are only made while `detail_visible`.
    /// Each returned request is in flight until its answer is applied.
    pub fn requests(&mut self, detail_visible: bool) -> Vec<PlanRequest> {
        let Some(thread) = self.thread.clone() else {
            return Vec::new();
        };
        let mut requests = Vec::new();
        for kind in KINDS {
            let i = kind.index();
            if self.in_flight[i] || !self.stale[i] || kind.detail() && !detail_visible {
                continue;
            }
            let query = match kind {
                ReadKind::Summary => ExecutionQuery::GetConversationPlan(ThreadTarget {
                    thread: thread.clone(),
                }),
                ReadKind::Tree => {
                    let read = self.tree_read.get_or_insert_with(TreeRead::default);
                    ExecutionQuery::ReadConversationPlanTree(PlanTreeQuery {
                        thread: thread.clone(),
                        cursor: read.cursor.clone(),
                        limit: Some(TREE_PAGE),
                    })
                }
                ReadKind::Task => {
                    let Some(task_id) = self.selected.clone() else {
                        self.stale[i] = false;
                        continue;
                    };
                    self.task_read = Some(task_id.clone());
                    ExecutionQuery::GetConversationPlanTask(PlanTaskQuery {
                        thread: thread.clone(),
                        task_id,
                    })
                }
                ReadKind::History => {
                    let newest = self.latest;
                    let page = self.history_page.min(pages_at(newest) - 1);
                    self.history_read = Some((page, newest));
                    ExecutionQuery::ReadConversationPlanHistory(PlanHistoryQuery {
                        thread: thread.clone(),
                        after_revision: Some(Revision(
                            newest.saturating_sub(u64::from(page + 1) * u64::from(HISTORY_PAGE)),
                        )),
                        limit: Some(HISTORY_PAGE),
                    })
                }
            };
            self.stale[i] = false;
            self.in_flight[i] = true;
            requests.push(PlanRequest {
                kind,
                generation: self.generation,
                query,
            });
        }
        requests
    }

    /// Folds the answer to a request from [`Self::requests`].
    pub fn apply(
        &mut self,
        kind: ReadKind,
        generation: u64,
        answer: Result<ExecutionReply, ReadFailure>,
    ) {
        if generation != self.generation {
            return;
        }
        let i = kind.index();
        self.in_flight[i] = false;
        let failure = match (kind, answer) {
            (ReadKind::Summary, Ok(ExecutionReply::ConversationPlan(summary))) => {
                self.apply_summary(*summary);
                return;
            }
            (ReadKind::Tree, Ok(ExecutionReply::ConversationPlanTree(page))) => {
                self.apply_tree(*page);
                return;
            }
            (ReadKind::Task, Ok(ExecutionReply::ConversationPlanTask(task))) => {
                self.apply_task(*task);
                return;
            }
            (ReadKind::History, Ok(ExecutionReply::ConversationPlanHistory(page))) => {
                self.apply_history(*page);
                return;
            }
            (_, Ok(_)) => ReadFailure::Other("unexpected reply".into()),
            (_, Err(failure)) => failure,
        };
        match (kind, failure) {
            (ReadKind::Tree, ReadFailure::Conflict) => {
                // The plan moved between pages: start over at its new revision.
                self.tree_read = None;
                self.stale[i] = true;
            }
            (ReadKind::Task, ReadFailure::NotFound) => {
                if self.task_read.take() == self.selected {
                    self.selected = None;
                    self.task = None;
                } else {
                    // The task asked for is no longer the selected one: the
                    // current selection has its own read due.
                    self.stale[i] = self.selected.is_some();
                }
            }
            (ReadKind::Tree, failure) => {
                self.tree_read = None;
                self.error = Some(describe(&failure));
            }
            (_, failure) => self.error = Some(describe(&failure)),
        }
    }

    // Each answer is stored before its revision is noted, so noting it only
    // marks the *other* views that are behind as stale. A view read while a
    // newer notification arrived stays stale and is read once more.

    fn apply_summary(&mut self, summary: PlanSummary) {
        let revision = summary.revision.0;
        if self
            .summary
            .as_ref()
            .is_none_or(|s| summary.revision >= s.revision)
        {
            self.error = None;
            self.summary = Some(summary);
        }
        self.bump(revision);
    }

    fn apply_tree(&mut self, page: PlanTreePage) {
        let Some(read) = self.tree_read.as_mut() else {
            return;
        };
        if read
            .revision
            .is_some_and(|revision| revision != page.revision)
        {
            // The plan moved between pages: start over at its new revision.
            self.tree_read = None;
            self.stale[ReadKind::Tree.index()] = true;
            return;
        }
        read.revision = Some(page.revision);
        read.nodes.extend(page.nodes);
        if let Some(cursor) = page.next_cursor {
            read.cursor = Some(cursor);
            self.stale[ReadKind::Tree.index()] = true;
            return;
        }
        let read = self.tree_read.take().unwrap_or_default();
        if self
            .tree
            .as_ref()
            .is_none_or(|tree| page.revision >= tree.revision)
        {
            self.error = None;
            // A selected task the new tree no longer has is dropped.
            if self
                .selected
                .as_ref()
                .is_some_and(|selected| !read.nodes.iter().any(|n| &n.task_id == selected))
            {
                self.selected = None;
                self.task = None;
            }
            self.tree = Some(PlanTree {
                revision: page.revision,
                progress: page.progress,
                nodes: read.nodes,
            });
        }
        self.bump(page.revision.0);
    }

    fn apply_task(&mut self, task: PlanTaskRead) {
        if self.selected.as_deref() != Some(task.node.task_id.as_str()) {
            // Another task was selected meanwhile; read that one.
            self.stale[ReadKind::Task.index()] = self.selected.is_some();
            return;
        }
        let revision = task.revision.0;
        if self
            .task
            .as_ref()
            .is_none_or(|t| task.revision >= t.revision)
        {
            self.task = Some(task);
        }
        self.bump(revision);
    }

    fn apply_history(&mut self, page: PlanHistoryPage) {
        let Some((requested, newest)) = self.history_read.take() else {
            return;
        };
        if requested != self.history_page {
            // Paged meanwhile: that read is already due.
            return;
        }
        let upper = newest.saturating_sub(u64::from(requested) * u64::from(HISTORY_PAGE));
        let mut entries: Vec<_> = page
            .entries
            .into_iter()
            .filter(|entry| entry.revision.0 <= upper)
            .collect();
        entries.reverse();
        let revision = page.revision.0;
        self.history = Some(PlanHistory {
            revision: page.revision,
            page: requested,
            pages: pages_at(newest.max(revision)),
            entries,
        });
        self.bump(revision);
        // Pages are counted back from the newest revision known when asking:
        // a newer one shifts every page, so the page is read again.
        if newest < self.latest {
            self.stale[ReadKind::History.index()] = true;
        }
    }
}

fn pages_at(revision: u64) -> u32 {
    let pages = revision.div_ceil(u64::from(HISTORY_PAGE)).max(1);
    u32::try_from(pages).unwrap_or(u32::MAX)
}

fn describe(failure: &ReadFailure) -> String {
    match failure {
        ReadFailure::Conflict => "The plan changed while it was read".into(),
        ReadFailure::NotFound => "Not found".into(),
        ReadFailure::Other(message) => message.clone(),
    }
}

/// Whether the plan has any task. An empty plan (never written or cleared) is
/// "not planned", never 100%.
pub fn is_planned(progress: &PlanProgress) -> bool {
    progress.node_count > 0
}

/// Whether the summary is of a plan that was written to and then emptied: it has
/// no task but a revision, so its history is still worth opening.
pub fn summary_is_cleared(summary: &PlanSummary) -> bool {
    !is_planned(&summary.progress) && summary.revision.0 > 0
}

/// Completion in whole percent: the completed share of the plan's weight.
pub fn completed_percent(progress: &PlanProgress) -> u32 {
    progress.weighted_ppm.completed.min(1_000_000) / 10_000
}

/// Sends plan queries for the in-process Workbench.
pub trait PlanReader: Send + Sync + 'static {
    fn read(&self, query: &ExecutionQuery) -> Result<ExecutionReply, ReadFailure>;
}

/// Answers from the workbench host's `execution` area over the GUI's runtime,
/// the same handler the core serves remote clients with.
pub struct HostPlanReader(workbench_host::areas::execution::ExecutionArea);

impl HostPlanReader {
    pub fn new(runtime: runtime::AgentRuntime) -> Self {
        Self(workbench_host::areas::execution::ExecutionArea::with_runtime(runtime))
    }
}

impl PlanReader for HostPlanReader {
    fn read(&self, query: &ExecutionQuery) -> Result<ExecutionReply, ReadFailure> {
        let whole = workbench_protocol::queries::Query::Execution(query.clone());
        match self.0.query(&whole, query) {
            Ok(workbench_protocol::queries::QueryReply::Execution(reply)) => Ok(reply),
            Ok(_) => Err(ReadFailure::Other("unexpected reply".into())),
            Err(error) => Err(ReadFailure::from(&error)),
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;
/// Runs a read off the UI thread.
pub type Spawner = Arc<dyn Fn(Job) + Send + Sync>;
/// Asks the UI for a frame after a read finished.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

type Answer = (ReadKind, u64, Result<ExecutionReply, ReadFailure>);

/// [`PlanState`] with the reads made on worker threads.
pub struct ConversationPlanModel {
    state: PlanState,
    reader: Option<Arc<dyn PlanReader>>,
    spawner: Spawner,
    waker: Arc<Mutex<Option<Waker>>>,
    sender: Sender<Answer>,
    answers: Receiver<Answer>,
}

impl Default for ConversationPlanModel {
    fn default() -> Self {
        let (sender, answers) = channel();
        Self {
            state: PlanState::default(),
            reader: None,
            spawner: thread_spawner(),
            waker: Arc::new(Mutex::new(None)),
            sender,
            answers,
        }
    }
}

/// One thread per read; reads are rare (a notification, a click).
pub fn thread_spawner() -> Spawner {
    Arc::new(|job| {
        let started = std::thread::Builder::new()
            .name("conversation-plan-reader".into())
            .spawn(job);
        if let Err(error) = started {
            tracing::warn!(%error, "could not start a conversation plan reader thread");
        }
    })
}

/// Runs a read where it is started: fixtures and tests.
pub fn inline_spawner() -> Spawner {
    Arc::new(|job| job())
}

impl ConversationPlanModel {
    pub fn with_reader(mut self, reader: Arc<dyn PlanReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    pub fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
    }

    /// Without a reader nothing is read and the band stays hidden.
    pub fn is_wired(&self) -> bool {
        self.reader.is_some()
    }

    pub fn set_waker(&self, waker: Waker) {
        *self.waker.lock().unwrap_or_else(PoisonError::into_inner) = Some(waker);
    }

    pub fn state(&self) -> &PlanState {
        &self.state
    }

    pub fn state_mut(&mut self) -> &mut PlanState {
        &mut self.state
    }

    /// Folds finished reads, then starts the due ones. Returns whether
    /// something was folded.
    pub fn poll(&mut self, detail_visible: bool) -> bool {
        let mut changed = false;
        loop {
            while let Ok((kind, generation, answer)) = self.answers.try_recv() {
                self.state.apply(kind, generation, answer);
                changed = true;
            }
            let Some(reader) = self.reader.clone() else {
                return changed;
            };
            let requests = self.state.requests(detail_visible);
            if requests.is_empty() {
                return changed;
            }
            for request in requests {
                let reader = Arc::clone(&reader);
                let sender = self.sender.clone();
                let waker = Arc::clone(&self.waker);
                (self.spawner)(Box::new(move || {
                    let answer = reader.read(&request.query);
                    let _ = sender.send((request.kind, request.generation, answer));
                    if let Some(wake) = waker
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                    {
                        wake();
                    }
                }));
            }
        }
    }
}
