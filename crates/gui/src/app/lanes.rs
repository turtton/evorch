//! Lanes in the Workbench (ADR 0044 Phase 1b,担当 D): projecting the lane ledger into
//! the sidebar and keeping what the lane view shows.
//!
//! The ledger is the source of whether a thread is a lane, who manages it and under which
//! policy. The sidebar keeps only a projection ([`workspace_ui::LaneLink`]) so it can draw
//! and save without the ledger; three things feed it, all through one function
//! ([`WorkbenchState::apply_lane_snapshot`] or [`SidebarState::apply_lane_link`]) that never
//! lets an older management revision replace a newer one:
//!
//! - **events** (`LaneCreated`, `LaneManagementChanged`, `LaneReportAppended`,
//!   `LaneBriefRevised`) folded by [`WorkbenchState::apply_lane_event`]. They carry the
//!   management data, so the thread appears and moves without any I/O, and they request a
//!   ledger read for the rest;
//! - **ledger reads** of one lane ([`LaneLedgerReader::snapshot`]), run off the UI thread,
//!   for the brief and the latest Report;
//! - **a scan** at startup ([`LaneLedgerReader::scan`]) that repairs a lane whose event was
//!   lost or whose sidebar was not saved before a crash. A lane is keyed by its thread ID, so
//!   a repair never shows it twice.
//!
//! Reading, redrawing and polling never change a lane's place: its order comes from real
//! activity (conversation, Report) only.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, SystemTime};

use event_bus::lane::{LanePolicy as LedgerPolicy, LaneReport};
use event_bus::{Event, EventKind, LedgerEvent};
use storage::Database;
use storage::lanes::LaneRecord;
use storage::threads::ThreadLineageKind;
use workspace_ui::{LaneLink, LanePolicy, ProjectId, ThreadChatRole, ThreadId, ThreadRecord};

use super::WorkbenchState;
use crate::model::lanes::{LaneDeliveryView, LaneDetail};
use crate::model::tasks::AgentRunSource;

const NANOS_PER_SECOND: i64 = 1_000_000_000;
/// Ledger reads in flight at once.
const MAX_IN_FLIGHT: usize = 4;
/// Thread rows read per page by the startup scan.
const SCAN_PAGE: usize = 200;
/// Title shown until the ledger names the lane.
const PENDING_TITLE: &str = "Lane";
/// Longest objective excerpt used as a lane's title, in characters.
const TITLE_CHARS: usize = 60;

/// A lane as the ledger holds it, with what the sidebar needs to place its thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneSnapshot {
    pub detail: LaneDetail,
    /// The GUI project the lane's thread row was written with, if known.
    pub project_id: Option<String>,
    pub title: Option<String>,
    pub created_at_ns: i64,
}

/// Reads the lane ledger. Blocking: it always runs on a worker thread.
pub trait LaneLedgerReader: Send + Sync + 'static {
    /// The lane whose root lives in `thread_id`; `None` when it is not a lane.
    ///
    /// # Errors
    /// A message when the ledger cannot be read.
    fn snapshot(&self, thread_id: &str) -> Result<Option<LaneSnapshot>, String>;

    /// Every lane whose thread belongs to one of `project_roots`.
    ///
    /// # Errors
    /// A message when the ledger cannot be read.
    fn scan(&self, project_roots: &[PathBuf]) -> Result<Vec<LaneSnapshot>, String>;
}

/// Reads the lanes through a read-only connection to the shared database.
pub struct StorageLaneReader {
    db_path: PathBuf,
}

impl StorageLaneReader {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self { db_path }
    }

    fn open(&self) -> Result<Database, String> {
        Database::open_read_only(&self.db_path).map_err(|error| error.to_string())
    }
}

fn snapshot_of(db: &Database, thread_id: &str) -> Result<Option<LaneSnapshot>, String> {
    let fail = |error: storage::StorageError| error.to_string();
    let Some(record) = db.lane_by_thread(thread_id).map_err(fail)? else {
        return Ok(None);
    };
    let latest_report = if record.latest_report_seq == 0 {
        None
    } else {
        db.lane_reports(&record.lane_id, record.latest_report_seq - 1, 1)
            .map_err(fail)?
            .into_iter()
            .next()
    };
    let row = db.thread(thread_id).map_err(fail)?;
    let delivery = LaneDeliveryView {
        capacity_blocked: db
            .lane_question_capacity_diagnostic(&record.lane_id)
            .map_err(fail)?,
        guard: db.lane_wake_guard(&record.lane_id).map_err(fail)?,
        lead_stopped: match record.lead_thread_id.as_deref() {
            Some(lead) => db
                .lane_delivery_destination(lead)
                .map_err(fail)?
                .is_some_and(|destination| destination.stopped),
            None => false,
        },
    };
    Ok(Some(LaneSnapshot {
        project_id: row.as_ref().and_then(|row| row.project_id.clone()),
        title: row.as_ref().and_then(|row| row.title.clone()),
        created_at_ns: row.as_ref().map_or(record.created_at_ns, |row| {
            if row.created_at_ns > 0 {
                row.created_at_ns
            } else {
                record.created_at_ns
            }
        }),
        detail: LaneDetail {
            record,
            latest_report,
            delivery,
        },
    }))
}

impl LaneLedgerReader for StorageLaneReader {
    fn snapshot(&self, thread_id: &str) -> Result<Option<LaneSnapshot>, String> {
        snapshot_of(&self.open()?, thread_id)
    }

    fn scan(&self, project_roots: &[PathBuf]) -> Result<Vec<LaneSnapshot>, String> {
        let db = self.open()?;
        let mut found = Vec::new();
        let mut seen = BTreeSet::new();
        for root in project_roots {
            let Ok(canonical) = std::fs::canonicalize(root) else {
                continue;
            };
            let root = canonical.to_string_lossy().into_owned();
            let mut cursor = None;
            loop {
                let page = db
                    .threads_in_project(&root, cursor.as_ref(), SCAN_PAGE)
                    .map_err(|error| error.to_string())?;
                for row in page.threads {
                    if row.lineage_kind == Some(ThreadLineageKind::Lane)
                        && seen.insert(row.thread_id.clone())
                        && let Some(snapshot) = snapshot_of(&db, &row.thread_id)?
                    {
                        found.push(snapshot);
                    }
                }
                match page.next_cursor {
                    Some(next) => cursor = Some(next),
                    None => break,
                }
            }
        }
        Ok(found)
    }
}

type Job = Box<dyn FnOnce() + Send>;
/// Runs a ledger read off the UI thread.
pub type Spawner = Arc<dyn Fn(Job) + Send + Sync>;
type Waker = Arc<dyn Fn() + Send + Sync>;

/// A spawner that runs each job on a thread of its own.
#[must_use]
pub fn thread_spawner() -> Spawner {
    Arc::new(|job| {
        if let Err(error) = std::thread::Builder::new()
            .name("lane-ledger-read".into())
            .spawn(job)
        {
            tracing::warn!(%error, "lane ledger read could not start");
        }
    })
}

enum Loaded {
    Snapshot(String, Result<Option<Box<LaneSnapshot>>, String>),
    Scan(Result<Vec<LaneSnapshot>, String>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScanPhase {
    Pending,
    InFlight,
    Done,
}

/// The lane side of the Workbench: what the ledger last said and the reads under way.
pub(super) struct LaneState {
    reader: Option<Arc<dyn LaneLedgerReader>>,
    spawner: Spawner,
    waker: Waker,
    waker_installed: bool,
    pub(super) details: HashMap<String, LaneDetail>,
    /// Threads waiting for a ledger read.
    queued: BTreeSet<String>,
    in_flight: BTreeSet<String>,
    /// Threads whose last read failed: read again only when something asks for it.
    failed: BTreeSet<String>,
    /// Threads asked for again while a read of them was in flight: that read may predate
    /// what the request is about, so one more read follows it.
    dirty: BTreeSet<String>,
    scan: ScanPhase,
    results: Receiver<Loaded>,
    sender: Sender<Loaded>,
    /// Chat roots whose thread the sidebar did not have yet: the runtime starts a lane root
    /// before the creation event places its thread.
    early_roots: VecDeque<(String, String)>,
    /// The thread whose delivery state was last asked for, so showing a thread reads it once.
    shown: Option<String>,
}

impl Default for LaneState {
    fn default() -> Self {
        let (sender, results) = channel();
        Self {
            reader: None,
            spawner: thread_spawner(),
            waker: Arc::new(|| {}),
            waker_installed: false,
            details: HashMap::new(),
            queued: BTreeSet::new(),
            in_flight: BTreeSet::new(),
            failed: BTreeSet::new(),
            dirty: BTreeSet::new(),
            scan: ScanPhase::Pending,
            results,
            sender,
            early_roots: VecDeque::new(),
            shown: None,
        }
    }
}

impl LaneState {
    /// Remembers the newest root of a thread the sidebar lacks, the oldest threads going
    /// first once too many are waiting. A lane that lost its root this way gets it from
    /// the ledger read.
    pub(super) fn note_early_root(&mut self, thread_id: &str, run_id: &str) {
        const KEPT: usize = 64;
        self.early_roots.retain(|(thread, _)| thread != thread_id);
        self.early_roots
            .push_back((thread_id.to_owned(), run_id.to_owned()));
        while self.early_roots.len() > KEPT {
            self.early_roots.pop_front();
        }
    }

    fn take_early_root(&mut self, thread_id: &str) -> Option<String> {
        let at = self
            .early_roots
            .iter()
            .position(|(thread, _)| thread == thread_id)?;
        self.early_roots.remove(at).map(|(_, run)| run)
    }

    fn request(&mut self, thread_id: &str) {
        self.failed.remove(thread_id);
        if self.in_flight.contains(thread_id) {
            self.dirty.insert(thread_id.to_owned());
        } else {
            self.queued.insert(thread_id.to_owned());
        }
    }
}

const fn policy_of(policy: LedgerPolicy) -> LanePolicy {
    match policy {
        LedgerPolicy::Attached => LanePolicy::Attached,
        LedgerPolicy::Summary => LanePolicy::Summary,
        LedgerPolicy::Detached => LanePolicy::Detached,
    }
}

fn link_of(record: &LaneRecord) -> LaneLink {
    LaneLink {
        lane_id: record.lane_id.clone(),
        origin_thread_id: ThreadId::new(record.origin_thread_id.clone()),
        lead_thread_id: record.lead_thread_id.clone().map(ThreadId::new),
        policy: policy_of(record.policy),
        management_revision: record.management_revision,
    }
}

fn seconds(nanos: i64) -> i64 {
    nanos.max(0) / NANOS_PER_SECOND
}

/// A lane's title: the ledger's own, else the first line of the objective.
fn lane_title(snapshot: &LaneSnapshot) -> String {
    let titled = snapshot
        .title
        .as_deref()
        .map(str::trim)
        .filter(|title| !title.is_empty());
    if let Some(title) = titled {
        return title.to_owned();
    }
    let objective = snapshot.detail.record.brief.objective.lines().next();
    let excerpt: String = objective
        .unwrap_or_default()
        .trim()
        .chars()
        .take(TITLE_CHARS)
        .collect();
    if excerpt.is_empty() {
        PENDING_TITLE.to_owned()
    } else {
        format!("Lane · {excerpt}")
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Show lanes from `reader`'s ledger and repair the sidebar from it.
    #[must_use]
    pub fn with_lane_ledger(mut self, reader: Arc<dyn LaneLedgerReader>) -> Self {
        self.lanes.reader = Some(reader);
        self
    }

    /// Run ledger reads through `spawner` instead of a thread each (tests run them inline).
    #[must_use]
    pub fn with_lane_spawner(mut self, spawner: Spawner) -> Self {
        self.lanes.spawner = spawner;
        self
    }

    /// What the ledger last said about the lane in `thread`.
    #[must_use]
    pub fn lane_detail(&self, thread: &ThreadId) -> Option<&LaneDetail> {
        self.lanes.details.get(&thread.to_string())
    }

    /// Drains finished ledger reads and starts the due ones, once per frame. Returns whether
    /// the sidebar changed (the caller saves it).
    pub(super) fn poll_lanes(&mut self, ctx: &egui::Context) -> bool {
        if !self.lanes.waker_installed {
            let ctx = ctx.clone();
            self.lanes.waker = Arc::new(move || ctx.request_repaint());
            self.lanes.waker_installed = true;
        }
        self.pump_lanes()
    }

    /// [`Self::poll_lanes`] without an egui context: what a headless host calls once per tick.
    pub fn pump_lanes(&mut self) -> bool {
        if self.lanes.reader.is_none() {
            return false;
        }
        let mut changed = false;
        while let Ok(loaded) = self.lanes.results.try_recv() {
            changed |= self.apply_loaded_lanes(loaded);
        }
        self.refresh_shown_lanes();
        self.queue_stale_lanes();
        self.start_lane_reads();
        if changed {
            self.sidebar.refresh_derived();
        }
        changed
    }

    fn apply_loaded_lanes(&mut self, loaded: Loaded) -> bool {
        match loaded {
            Loaded::Snapshot(thread, result) => {
                self.lanes.in_flight.remove(&thread);
                let again = self.lanes.dirty.remove(&thread);
                let changed = match result {
                    Ok(Some(snapshot)) => {
                        let changed = self.apply_lane_snapshot(*snapshot);
                        // A ledger behind the sidebar is not asked again until an event says
                        // it moved: polling must not turn into a read loop.
                        if self.lane_details_lag(&thread) {
                            self.lanes.failed.insert(thread.clone());
                        }
                        changed
                    }
                    // Not a lane (any more): do not ask again until something says so.
                    Ok(None) => {
                        self.lanes.failed.insert(thread.clone());
                        false
                    }
                    Err(error) => {
                        tracing::warn!(%thread, %error, "lane ledger read failed");
                        self.lanes.failed.insert(thread.clone());
                        false
                    }
                };
                if again {
                    self.lanes.request(&thread);
                }
                changed
            }
            Loaded::Scan(result) => {
                self.lanes.scan = ScanPhase::Done;
                match result {
                    Ok(snapshots) => snapshots.into_iter().fold(false, |changed, snapshot| {
                        self.apply_lane_snapshot(snapshot) | changed
                    }),
                    Err(error) => {
                        tracing::warn!(%error, "lane ledger scan failed");
                        false
                    }
                }
            }
        }
    }

    /// Delivery state (capacity, paused wake, Stop) has no event of its own, so it is read when
    /// a lane or a lead is shown, when the runtime reports a delivery problem and after the
    /// user acts on it: never on a timer.
    fn refresh_shown_lanes(&mut self) {
        let Some(active) = self.sidebar.active_thread.as_ref().map(ToString::to_string) else {
            return;
        };
        if self.lanes.shown.as_ref() == Some(&active) {
            return;
        }
        self.lanes.shown = Some(active.clone());
        self.request_delivery_reads(&active);
    }

    /// Asks for a read of `thread` when it is a lane, and of every lane it leads.
    fn request_delivery_reads(&mut self, thread: &str) {
        let wanted: Vec<String> = self
            .sidebar
            .threads
            .iter()
            .filter(|candidate| {
                candidate.lane.as_ref().is_some_and(|lane| {
                    candidate.id.to_string() == thread
                        || lane
                            .lead_thread_id
                            .as_ref()
                            .is_some_and(|lead| lead.to_string() == thread)
                })
            })
            .map(|candidate| candidate.id.to_string())
            .collect();
        for id in wanted {
            self.lanes.request(&id);
        }
    }

    /// Runs what a delivery notice offered. The runtime decides what each operation does.
    pub(super) fn apply_lane_action(&mut self, action: crate::panes::agent::LaneAction) {
        use crate::model::commands::WorkbenchCommand;
        use crate::panes::agent::LaneAction;
        let lead = match &action {
            LaneAction::ResumeDelivery { lead } | LaneAction::ContinueLead { lead } => lead.clone(),
        };
        let command = match action {
            LaneAction::ResumeDelivery { .. } => WorkbenchCommand::ResumeLaneDeliveries {
                thread_id: lead.clone(),
            },
            LaneAction::ContinueLead { .. } => WorkbenchCommand::ContinueChat(
                self.chat_continuation(&ThreadId::new(lead.clone()), false),
            ),
        };
        self.submit_command(command);
        self.request_delivery_reads(&lead);
    }

    /// A lane thread the ledger has not been read for, or whose details lag the sidebar's
    /// management revision (another window moved it), is read again.
    fn queue_stale_lanes(&mut self) {
        let stale: Vec<String> = self
            .sidebar
            .threads
            .iter()
            .filter_map(|thread| {
                thread.lane.as_ref()?;
                let id = thread.id.to_string();
                (self.lane_details_lag(&id) && !self.lanes.failed.contains(&id)).then_some(id)
            })
            .collect();
        for id in stale {
            self.lanes.request(&id);
        }
    }

    /// Whether the details held for `thread` are older than the sidebar's projection.
    fn lane_details_lag(&self, thread: &str) -> bool {
        let shown = self
            .sidebar
            .threads
            .iter()
            .find(|candidate| candidate.id.to_string() == thread)
            .and_then(|candidate| candidate.lane.as_ref());
        shown.is_some_and(|lane| {
            self.lanes
                .details
                .get(thread)
                .is_none_or(|detail| detail.record.management_revision < lane.management_revision)
        })
    }

    fn start_lane_reads(&mut self) {
        let Some(reader) = self.lanes.reader.clone() else {
            return;
        };
        if self.lanes.scan == ScanPhase::Pending {
            self.lanes.scan = ScanPhase::InFlight;
            let roots: Vec<PathBuf> = self
                .sidebar
                .projects
                .iter()
                .map(|project| project.repo_root.clone())
                .collect();
            let sender = self.lanes.sender.clone();
            let waker = Arc::clone(&self.lanes.waker);
            let reader = Arc::clone(&reader);
            (self.lanes.spawner)(Box::new(move || {
                let _ = sender.send(Loaded::Scan(reader.scan(&roots)));
                waker();
            }));
        }
        while self.lanes.in_flight.len() < MAX_IN_FLIGHT
            && let Some(thread) = self.lanes.queued.pop_first()
        {
            self.lanes.in_flight.insert(thread.clone());
            let sender = self.lanes.sender.clone();
            let waker = Arc::clone(&self.lanes.waker);
            let reader = Arc::clone(&reader);
            (self.lanes.spawner)(Box::new(move || {
                let result = reader.snapshot(&thread).map(|found| found.map(Box::new));
                let _ = sender.send(Loaded::Snapshot(thread, result));
                waker();
            }));
        }
    }

    /// Folds a lane event into the sidebar: no I/O. The event carries the management data, so
    /// the thread appears or moves at once; the rest is read from the ledger. Returns
    /// whether the sidebar changed.
    pub(super) fn apply_lane_event(&mut self, event: &Event) -> bool {
        let changed = self.fold_lane_event(event);
        if changed {
            self.sidebar.refresh_derived();
        }
        changed
    }

    fn fold_lane_event(&mut self, event: &Event) -> bool {
        if let EventKind::Diagnostic(diagnostic) = &event.kind
            && diagnostic.source == "lane_delivery"
        {
            // The runtime names the thread whose delivery state changed (a lane or a lead);
            // a delivery problem that names none refreshes what is shown.
            let thread = diagnostic
                .thread_id
                .clone()
                .or_else(|| self.sidebar.active_thread.as_ref().map(ToString::to_string));
            if let Some(thread) = thread {
                self.request_delivery_reads(&thread);
            }
            return false;
        }
        let EventKind::Ledger(ledger) = &event.kind else {
            return false;
        };
        match ledger {
            LedgerEvent::LaneCreated {
                lane_id,
                thread_id,
                origin_thread_id,
                lead_thread_id,
                policy,
                management_revision,
                ..
            } => {
                self.lanes.request(thread_id);
                let link = LaneLink {
                    lane_id: lane_id.clone(),
                    origin_thread_id: ThreadId::new(origin_thread_id.clone()),
                    lead_thread_id: lead_thread_id.clone().map(ThreadId::new),
                    policy: policy_of(*policy),
                    management_revision: *management_revision,
                };
                let project = self.project_of_thread(&link.origin_thread_id);
                let created = project.is_some_and(|project| {
                    self.ensure_lane_thread(
                        thread_id,
                        project,
                        PENDING_TITLE,
                        event.meta.wall_clock,
                    )
                });
                let linked = self.link_lane_thread(thread_id, link);
                let rootless = self.sidebar.threads.iter().any(|thread| {
                    thread.id.to_string() == *thread_id && thread.root_run_id.is_none()
                });
                let adopted = rootless
                    && self
                        .lanes
                        .take_early_root(thread_id)
                        .is_some_and(|run| self.bind_conversation_run(thread_id, &run, true));
                self.bind_lane_root(thread_id);
                linked | created | adopted
            }
            LedgerEvent::LaneManagementChanged {
                lane_id,
                thread_id,
                lead_thread_id,
                policy,
                management_revision,
                ..
            } => {
                self.lanes.request(thread_id);
                let origin = self
                    .sidebar
                    .threads
                    .iter()
                    .find(|thread| thread.id.to_string() == *thread_id)
                    .and_then(|thread| thread.lane.as_ref())
                    .map(|lane| lane.origin_thread_id.clone());
                // Without the projection the origin is unknown here: the ledger read places it.
                origin.is_some_and(|origin_thread_id| {
                    self.link_lane_thread(
                        thread_id,
                        LaneLink {
                            lane_id: lane_id.clone(),
                            origin_thread_id,
                            lead_thread_id: lead_thread_id.clone().map(ThreadId::new),
                            policy: policy_of(*policy),
                            management_revision: *management_revision,
                        },
                    )
                })
            }
            LedgerEvent::LaneReportAppended { thread_id, .. } => {
                self.lanes.request(thread_id);
                self.touch_thread(thread_id, event.meta.wall_clock)
            }
            LedgerEvent::LaneBriefRevised { thread_id, .. } => {
                self.lanes.request(thread_id);
                false
            }
            _ => false,
        }
    }

    /// Gives the command sink the lane's current root. The runtime starts a lane root
    /// itself, so no submission from this window ever bound it; without the binding a
    /// Stop on the lane would find no run. Events own which root is current.
    pub(super) fn bind_lane_root(&mut self, thread_id: &str) {
        let Some(root) = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == thread_id && thread.lane.is_some())
            .and_then(|thread| thread.root_run_id.clone())
        else {
            return;
        };
        self.sink.bind_lane_root(thread_id, &root);
    }

    fn project_of_thread(&self, id: &ThreadId) -> Option<ProjectId> {
        self.sidebar
            .threads
            .iter()
            .find(|thread| &thread.id == id)
            .map(|thread| thread.project_id.clone())
    }

    fn touch_thread(&mut self, thread_id: &str, at: SystemTime) -> bool {
        self.sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
            .is_some_and(|thread| thread.touch(at))
    }

    /// Adds the lane's thread unless the sidebar has it (or a confirmed deletion removed it).
    fn ensure_lane_thread(
        &mut self,
        thread_id: &str,
        project: ProjectId,
        title: &str,
        at: SystemTime,
    ) -> bool {
        let id = ThreadId::new(thread_id);
        if thread_id.is_empty()
            || self.tombstones.contains_thread(thread_id)
            || self.sidebar.threads.iter().any(|thread| thread.id == id)
        {
            return false;
        }
        let mut thread = ThreadRecord::new_at(id, project, title, at);
        thread.title_is_placeholder = title == PENDING_TITLE;
        thread.chat_role = Some(ThreadChatRole::Orchestrator);
        self.sidebar.threads.push(thread);
        true
    }

    fn link_lane_thread(&mut self, thread_id: &str, link: LaneLink) -> bool {
        match self.sidebar.set_lane_link(&ThreadId::new(thread_id), link) {
            Ok(changed) => changed,
            Err(error) => {
                tracing::debug!(%thread_id, %error, "lane projection not applied");
                false
            }
        }
    }

    /// Applies one ledger read: places the lane's thread (adding it when the sidebar lacks it),
    /// moves its activity forward and keeps the details for the lane view.
    pub(super) fn apply_lane_snapshot(&mut self, snapshot: LaneSnapshot) -> bool {
        let thread_id = snapshot.detail.record.thread_id.clone();
        if self.tombstones.contains_thread(&thread_id) {
            return false;
        }
        let at = SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(seconds(snapshot.created_at_ns)).unwrap_or(0));
        let project = self
            .project_of_thread(&ThreadId::new(
                snapshot.detail.record.origin_thread_id.clone(),
            ))
            .or_else(|| {
                let wanted = snapshot.project_id.as_deref()?;
                self.sidebar
                    .projects
                    .iter()
                    .find(|project| project.id.to_string() == wanted)
                    .map(|project| project.id.clone())
            });
        let title = lane_title(&snapshot);
        let mut changed =
            project.is_some_and(|project| self.ensure_lane_thread(&thread_id, project, &title, at));
        let record = &snapshot.detail.record;
        changed |= self.link_lane_thread(&thread_id, link_of(record));
        let activity = seconds(record.last_activity_at_ns);
        let root = record.root_run_id.clone();
        let ledger_root = root.clone();
        if let Some(thread) = self
            .sidebar
            .threads
            .iter_mut()
            .find(|thread| thread.id.to_string() == thread_id)
        {
            if thread.title_is_placeholder && title != PENDING_TITLE {
                thread.title.clone_from(&title);
                thread.title_is_placeholder = false;
                changed = true;
            }
            if activity > thread.updated_at {
                thread.updated_at = activity;
                changed = true;
            }
        }
        // Events own a conversation's current root; the ledger only fills a missing one.
        if let Some(root) = root
            && self
                .sidebar
                .threads
                .iter()
                .any(|thread| thread.id.to_string() == thread_id && thread.root_run_id.is_none())
        {
            changed |= self.bind_conversation_run(&thread_id, &root, true);
        }
        // The sink follows the ledger's root, and only where the events agree with it.
        if ledger_root.as_deref().is_some_and(|ledger| {
            self.sidebar.threads.iter().any(|thread| {
                thread.id.to_string() == thread_id && thread.root_run_id.as_deref() == Some(ledger)
            })
        }) {
            self.bind_lane_root(&thread_id);
        }
        let current = self.lanes.details.get(&thread_id);
        let newer =
            current.is_none_or(|current| detail_key(&snapshot.detail) >= detail_key(current));
        if newer {
            self.lanes.details.insert(thread_id, snapshot.detail);
        }
        changed
    }
}

/// Orders two reads of one lane: later management, brief and Report win.
fn detail_key(detail: &LaneDetail) -> (u64, u64, u64) {
    let record = &detail.record;
    (
        record.management_revision,
        record.brief_revision,
        detail
            .latest_report
            .as_ref()
            .map_or(0, |report: &LaneReport| report.report_seq),
    )
}
