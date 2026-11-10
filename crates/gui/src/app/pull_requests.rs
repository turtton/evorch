//! Pull requests in the Workbench (ADR 0043 Phase 1, PR5): what the bar, the badge and the chips
//! show, and the operations behind them.
//!
//! The database is the source of what is shown. A change reaches the display as an event (from
//! this process's runtime, or from another window through the shared feed); the event only says
//! *which* thread and pull request to read again, and the read runs off the UI thread. A window
//! that does not track a thread shows what the owner stored and does nothing else: it fetches
//! nothing, republishes no event and raises no notification. Only the window that holds the
//! thread raises one: for a CI transition the runtime claimed, and for an auto fix stop at the
//! limit, the one the database shows (see `claim_limit_stop`).
//!
//! Link, Unlink, Refresh and Confirm take the thread's write mode the way chat does
//! ([`WorkbenchState::chat_permit`]): they start or claim the lease when nobody holds it, which
//! is how a restarted process resumes polling, and refuse when another window holds it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};

use event_bus::{Event, EventKind, PullRequestAutoFixChange, PullRequestEvent, PullRequestRef};
use runtime::pull_requests::HeadAttribution;
use storage::Database;
use storage::pull_request_auto_fix::PrLimitClaim;
use storage::pull_requests::{PullRequestDisplay, PullRequestKey};

use super::WorkbenchState;
use super::lanes::{Spawner, thread_spawner};
use crate::model::commands::WorkbenchCommand;
use crate::model::harness_card::HarnessCard;
use crate::model::pull_requests::{
    AutoFixView, HeadNotice, PullRequestAction, PullRequestsModel, ThreadPullRequest,
};
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

    /// The card of an adopted harness notice, by its durable ID. `None` while the notice is
    /// not (or no longer) in the database as an adopted one.
    ///
    /// # Errors
    /// A message when the database cannot be read.
    fn harness_card(&self, notice_id: &str) -> Result<Option<HarnessCard>, String>;

    /// Takes the one notification of a limit stop, durably: for the first claim of any window or
    /// process only, and only while `still_held` says this window holds the thread (asked
    /// before the claim and again after it, so a window that lost the thread meanwhile gives the
    /// claim back instead of spending it).
    ///
    /// # Errors
    /// A message when the claim cannot be written.
    fn claim_limit_notification(
        &self,
        claim: &PrLimitClaim,
        still_held: &dyn Fn() -> bool,
    ) -> Result<LimitClaimOutcome, String>;
}

/// What a claim of a limit notification came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitClaimOutcome {
    /// This window took it and tells the person.
    Claimed,
    /// Another window took it, or the stop no longer stands for this thread and link.
    Declined,
    /// This window does not hold the thread (any more); nothing is kept from the holder.
    NotHeld,
}

/// Reads through a read-only connection to the shared database.
pub struct StoragePullRequestReader {
    db_path: PathBuf,
    probe: Option<AutoFixProbe>,
    claims: Option<storage::StorageHandle>,
}

/// What only the process that owns a thread's pull request knows, read for display: where the
/// shown head came from and whether automatic sends are switched on globally.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoFixFacts {
    pub head: HeadAttribution,
    pub global_enabled: bool,
}

/// Asks the runtime for [`AutoFixFacts`] of a thread. Blocking: it runs on the reader's worker.
pub type AutoFixProbe = Arc<dyn Fn(&str) -> AutoFixFacts + Send + Sync>;

impl StoragePullRequestReader {
    #[must_use]
    pub const fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            probe: None,
            claims: None,
        }
    }

    /// Writes the claims of limit notifications through `handle`. Without one no limit
    /// notification is raised, since nothing could keep it from repeating.
    #[must_use]
    pub fn with_limit_claims(mut self, handle: storage::StorageHandle) -> Self {
        self.claims = Some(handle);
        self
    }

    /// Reads the coordinator's classification of the head and the global switch through
    /// `probe`. Without one a head reads as unknown, which only ever withholds an automatic
    /// send.
    #[must_use]
    pub fn with_auto_fix_probe(mut self, probe: AutoFixProbe) -> Self {
        self.probe = Some(probe);
        self
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
        let (facts, display, fetched_at_ns, failure) = match &state.active_link {
            Some(link) => match db.pull_request_snapshot(&link.key).map_err(fail)? {
                Some(snapshot) => (
                    snapshot.facts,
                    snapshot.display,
                    snapshot.fetched_at_ns,
                    snapshot.last_attempt.and_then(|attempt| attempt.failure),
                ),
                None => (None, PullRequestDisplay::default(), None, None),
            },
            None => (None, PullRequestDisplay::default(), None, None),
        };
        let candidates = db.pull_request_candidates(thread_id, None).map_err(fail)?;
        let auto_fix = match &state.active_link {
            Some(link) => {
                let known = self.probe.as_ref().map(|probe| probe(thread_id));
                Some(read_auto_fix(&db, thread_id, &link.key, &facts, known).map_err(fail)?)
            }
            None => None,
        };
        Ok(ThreadPullRequest {
            thread_id: thread_id.to_owned(),
            lifecycle_revision: state.lifecycle_revision,
            transition_seq: state.transition_seq,
            link: state.active_link,
            local_head: state.local_head,
            facts,
            display,
            fetched_at_ns,
            failure,
            candidates,
            tracked_here: false,
            auto_fix,
        })
    }

    fn harness_card(&self, notice_id: &str) -> Result<Option<HarnessCard>, String> {
        let notice = self
            .open()?
            .pr_notice(notice_id)
            .map_err(|error| error.to_string())?;
        Ok(notice.as_ref().and_then(HarnessCard::from_notice))
    }

    fn claim_limit_notification(
        &self,
        claim: &PrLimitClaim,
        still_held: &dyn Fn() -> bool,
    ) -> Result<LimitClaimOutcome, String> {
        let handle = self
            .claims
            .as_ref()
            .ok_or("no writer to claim the limit notification")?;
        if !still_held() {
            return Ok(LimitClaimOutcome::NotHeld);
        }
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |elapsed| {
                i64::try_from(elapsed.as_nanos()).unwrap_or(i64::MAX)
            });
        let taken = handle
            .claim_pr_limit_notification(claim.clone(), now_ns)
            .map_err(|error| error.to_string())?;
        if !taken {
            return Ok(LimitClaimOutcome::Declined);
        }
        // The thread's registry is another store, so the claim cannot carry the lease: asking
        // again once it is taken is what keeps a window that lost the thread meanwhile from
        // spending the claim the new holder needs.
        if !still_held() {
            handle
                .release_pr_limit_notification(claim.clone(), now_ns)
                .map_err(|error| error.to_string())?;
            return Ok(LimitClaimOutcome::NotHeld);
        }
        Ok(LimitClaimOutcome::Claimed)
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

/// The PR-wide auto fix, the notice of the shown head and the thread's Stop, in the read that
/// also took the snapshot.
fn read_auto_fix(
    db: &Database,
    thread_id: &str,
    key: &PullRequestKey,
    facts: &Option<storage::pull_requests::PullRequestFacts>,
    known: Option<AutoFixFacts>,
) -> Result<AutoFixView, storage::StorageError> {
    let state = db.pr_auto_fix_state(key)?;
    let head_notice = match facts {
        Some(facts) => db
            .pr_notice_for_head(key, &facts.head_sha)?
            .map(|notice| HeadNotice {
                notice_id: notice.notice_id,
                phase: notice.phase,
                mode: notice.mode,
            }),
        None => None,
    };
    Ok(AutoFixView {
        state,
        head_notice,
        this_stopped: db
            .lane_delivery_destination(thread_id)?
            .is_some_and(|destination| destination.stopped),
        suppressed: db.pr_wake_guard(key)?.suppressed,
        head_origin: known.map_or(HeadAttribution::Unknown, |known| known.head),
        global_enabled: known.is_none_or(|known| known.global_enabled),
    })
}

/// Whether this process holds the lease of a thread (it then polls its pull request).
pub type OwnerProbe = Arc<dyn Fn(&str) -> bool + Send + Sync>;

type Waker = Arc<dyn Fn() + Send + Sync>;

enum Loaded {
    /// A thread's read, with the event epoch it started at.
    Thread(String, u64, Result<Box<ThreadPullRequest>, String>),
    Linked(Result<Vec<String>, String>),
    Slug(String, Option<String>),
    /// The card of a harness notice the transcript shows without it.
    Card(String, Result<Option<Box<HarnessCard>>, String>),
    /// The claim of a limit notification: the thread and pull request it is about, the budget
    /// epoch, and what the claim came to.
    LimitClaim(
        String,
        PullRequestKey,
        u64,
        Result<LimitClaimOutcome, String>,
    ),
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

/// The confirmation shown before auto fix moves to another thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DestinationDialog {
    /// The thread that would receive the CI failures from now on.
    pub thread_id: String,
    /// The thread that has them now, as the person last saw it.
    pub previous_thread_id: String,
    /// What the dialog names that thread (its title, or its ID when unknown).
    pub previous_label: String,
    pub link_revision: u64,
    pub policy_revision: u64,
    /// What the person goes on to do once the move is confirmed.
    pub then: DestinationChange,
}

/// The operation a [`DestinationDialog`] confirms the move for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DestinationChange {
    /// Turn auto fix on here.
    TurnOn,
    /// Start the automatic count again here, which also turns auto fix on.
    ResetBudget,
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
    /// Counts the events of this process's runtime; a read that started at or after an event
    /// has seen the database as that event left it.
    epoch: u64,
    /// Notifications the runtime claimed, held until a read made after their event confirms that
    /// the thread is still linked to the same link and this process still owns it.
    pending_notices: Vec<PendingNotice>,
    notified: std::collections::VecDeque<(String, u64, u64, String)>,
    pub(super) dialog: Option<LinkDialog>,
    pub(super) destination_dialog: Option<DestinationDialog>,
    /// Operations issued and not yet answered by an event or a refusal, so a second click on
    /// the same state does not issue a second operation.
    issued: BTreeSet<(String, IssuedOp)>,
    /// Threads whose bar folds once a read made after an automatic adoption was shown: the
    /// read may itself expand the bar (a move into failure), and the fold must come after.
    pending_collapses: Vec<(String, u64)>,
    /// The window began to close: no limit claim starts, and one already running gives way.
    closing: Arc<std::sync::atomic::AtomicBool>,
    /// `(pull request, budget epoch)` whose claim is running here or was taken here, so a read
    /// does not claim again. The claim in the database is what keeps other windows and restarts
    /// from telling it twice.
    limit_notified: std::collections::VecDeque<(String, u64)>,
    /// A transcript may show a notice whose card is not read yet.
    cards_dirty: bool,
    cards_in_flight: BTreeSet<String>,
}

/// What an operation is about, for [`PullRequestState::issued`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum IssuedOp {
    Policy(u64, u64, &'static str),
    Send(u64, String),
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
            epoch: 0,
            pending_notices: Vec::new(),
            notified: std::collections::VecDeque::new(),
            dialog: None,
            destination_dialog: None,
            issued: BTreeSet::new(),
            pending_collapses: Vec::new(),
            limit_notified: std::collections::VecDeque::new(),
            closing: Arc::default(),
            cards_dirty: false,
            cards_in_flight: BTreeSet::new(),
        }
    }
}

/// A notification waiting for the database to confirm it is still wanted.
struct PendingNotice {
    thread_id: String,
    link_revision: u64,
    transition_seq: u64,
    kind: String,
    reference: PullRequestRef,
    /// The epoch of the event that brought it.
    arrived: u64,
}

/// Notifications held at most; the oldest go first if reads never come.
const PENDING_NOTICES_KEPT: usize = 64;

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

    fn forget_limit_notified(&mut self, key: &PullRequestKey, epoch: u64) {
        let seen = (
            format!("{}/{}#{}", key.host, key.repository, key.number),
            epoch,
        );
        self.limit_notified.retain(|entry| *entry != seen);
    }

    fn remember_limit_notified(&mut self, key: (String, u64)) -> bool {
        if self.limit_notified.contains(&key) {
            return false;
        }
        self.limit_notified.push_back(key);
        while self.limit_notified.len() > NOTIFIED_KEPT {
            self.limit_notified.pop_front();
        }
        true
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
        self.pull_requests
            .closing
            .store(self.shutdown_requested, std::sync::atomic::Ordering::SeqCst);
        while let Ok(loaded) = self.pull_requests.results.try_recv() {
            self.apply_loaded_pull_request(loaded);
        }
        self.start_harness_card_reads();
        if !enabled {
            self.pull_requests.queued.clear();
            return;
        }
        self.refresh_shown_pull_request();
        self.start_pull_request_reads();
    }

    /// Asks for the notices a transcript shows without their card, once per request: the
    /// reads run off the UI thread, and a notice not stored yet stays a bare tag.
    fn start_harness_card_reads(&mut self) {
        if !self.pull_requests.cards_dirty {
            return;
        }
        let Some(reader) = self.pull_requests.reader.clone() else {
            return;
        };
        self.pull_requests.cards_dirty = false;
        for notice_id in self.transcripts.unfilled_harness_notices() {
            if !self.pull_requests.cards_in_flight.insert(notice_id.clone()) {
                continue;
            }
            let sender = self.pull_requests.sender.clone();
            let waker = Arc::clone(&self.pull_requests.waker);
            let reader = Arc::clone(&reader);
            (self.pull_requests.spawner)(Box::new(move || {
                let result = reader
                    .harness_card(&notice_id)
                    .map(|card| card.map(Box::new));
                let _ = sender.send(Loaded::Card(notice_id, result));
                waker();
            }));
        }
    }

    /// A transcript may now show a notice whose card is not read yet (a delivery event, or a
    /// history that was just restored).
    pub(super) fn note_harness_notices(&mut self) {
        self.pull_requests.cards_dirty = true;
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
            let epoch = self.pull_requests.epoch;
            (self.pull_requests.spawner)(Box::new(move || {
                let result = reader.load(&thread).map(|mut view| {
                    view.tracked_here = probe.as_ref().is_none_or(|probe| probe(&thread));
                    Box::new(view)
                });
                let _ = sender.send(Loaded::Thread(thread, epoch, result));
                waker();
            }));
        }
    }

    fn apply_loaded_pull_request(&mut self, loaded: Loaded) {
        match loaded {
            Loaded::Thread(thread, epoch, result) => {
                self.pull_requests.in_flight.remove(&thread);
                // Something about the thread (an event, an ownership change) arrived while this
                // read ran: what it found may already be out of date, and the read that follows
                // will say. It is shown meanwhile, but decides no notification.
                let overtaken = self.pull_requests.dirty.remove(&thread);
                if overtaken {
                    self.pull_requests.request(&thread);
                }
                match result {
                    Ok(view) if self.pull_requests.model.enabled => {
                        if !overtaken {
                            self.settle_pending_notices(&view, epoch);
                        }
                        self.pull_requests.model.set(*view);
                        if !overtaken {
                            self.apply_pending_collapses(&thread, epoch);
                        }
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
            Loaded::Card(notice_id, result) => {
                self.pull_requests.cards_in_flight.remove(&notice_id);
                match result {
                    Ok(Some(card)) => {
                        self.transcripts.fill_harness_card(&card);
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%notice_id, %error, "harness card read failed");
                    }
                }
            }
            // Raised in the frame that drains the result, with no read or wait in between: the
            // claim is spent by the time it arrives, so it is shown now or never.
            Loaded::LimitClaim(thread, key, epoch, claimed) => match claimed {
                Ok(LimitClaimOutcome::Claimed) => {
                    self.raise_auto_fix_limit_notification(&thread, &key);
                }
                // Only a claim this window took is remembered. Another window's claim may be
                // given back (its taker lost the thread), so a declined one is tried again by
                // the next read.
                Ok(LimitClaimOutcome::Declined | LimitClaimOutcome::NotHeld) => {
                    self.pull_requests.forget_limit_notified(&key, epoch);
                }
                Err(error) => {
                    // Not told, not claimed: a later read tries again.
                    self.pull_requests.forget_limit_notified(&key, epoch);
                    tracing::warn!(%thread, %error, "limit notification claim failed");
                }
            },
            Loaded::Slug(thread, slug) => {
                if let Some(dialog) = &mut self.pull_requests.dialog
                    && dialog.thread_id == thread
                {
                    dialog.project_slug = slug;
                }
            }
        }
    }

    /// Folds a pull request event this process's own runtime published: reads again, and holds
    /// the notifications the runtime claimed for it until that read confirms them (see
    /// [`Self::settle_pending_notices`]).
    pub(super) fn apply_local_pull_request_event(&mut self, event: &Event) {
        let EventKind::PullRequest(change) = &event.kind else {
            return;
        };
        self.pull_requests.epoch += 1;
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
        if let PullRequestEvent::AutoFixChanged {
            thread_id, reason, ..
        } = change
        {
            // The answer to what this window issued has arrived (the read it started follows). A
            // change of the global switch answers nothing this window issued.
            if *reason != PullRequestAutoFixChange::Settings {
                self.pull_requests
                    .issued
                    .retain(|(thread, _)| thread != thread_id);
            }
            if let PullRequestAutoFixChange::Adopted { automatic } = reason
                && *automatic
            {
                // The notice is working on the failure now: the bar gets out of the way,
                // once the read that follows is shown (see `pending_collapses`).
                self.pull_requests
                    .pending_collapses
                    .push((thread_id.clone(), self.pull_requests.epoch));
            }
        }
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
                self.pull_requests.pending_notices.push(PendingNotice {
                    thread_id: thread_id.clone(),
                    link_revision: *link_revision,
                    transition_seq: *transition_seq,
                    kind: kind.clone(),
                    reference: pull_request.clone(),
                    arrived: self.pull_requests.epoch,
                });
            }
            let excess = self
                .pull_requests
                .pending_notices
                .len()
                .saturating_sub(PENDING_NOTICES_KEPT);
            self.pull_requests.pending_notices.drain(..excess);
        }
    }

    /// Decides the notifications that waited for `view`, a read that started at `started`.
    /// A notification is raised only when a read made after its event shows the thread still
    /// linked to the link it was about and this process still holding the thread: an Unlink in
    /// any window, a relink, or a lost lease while it waited drops it. One transition notifies
    /// once.
    fn settle_pending_notices(&mut self, view: &ThreadPullRequest, started: u64) {
        self.claim_limit_stop(view);
        let (due, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.pull_requests.pending_notices)
                .into_iter()
                .partition(|notice| {
                    notice.thread_id == view.thread_id && notice.arrived <= started
                });
        self.pull_requests.pending_notices = waiting;
        let linked = view.link.as_ref().map(|link| link.link_revision);
        for notice in due {
            let still_wanted = view.tracked_here
                && linked == Some(notice.link_revision)
                && !self
                    .pull_requests
                    .ended
                    .contains(&(notice.thread_id.clone(), notice.link_revision));
            let key = (
                notice.thread_id.clone(),
                notice.link_revision,
                notice.transition_seq,
                notice.kind.clone(),
            );
            if still_wanted && self.pull_requests.remember_notified(key) {
                self.raise_pull_request_notification(
                    &notice.thread_id,
                    &notice.reference,
                    &notice.kind,
                );
            }
        }
    }

    /// Folds the bar of `thread` for each automatic adoption a read that started at `started`
    /// has seen. Runs after the view was shown, so a failure transition in that same read
    /// (which expands the bar) cannot undo the fold.
    fn apply_pending_collapses(&mut self, thread: &str, started: u64) {
        let before = self.pull_requests.pending_collapses.len();
        self.pull_requests
            .pending_collapses
            .retain(|(waiting, arrived)| waiting != thread || *arrived > started);
        if self.pull_requests.pending_collapses.len() != before {
            self.pull_requests.model.bar_mut(thread).collapse();
        }
    }

    /// Starts the claim of the notification for a stop at the limit that `view` shows, once per
    /// pull request and budget epoch. Whether the runtime's event for the adoption ever came is
    /// no condition: the database shows the stop, and the claim in it tells whether any window
    /// already told. Only the window that holds the destination thread claims, and it re-asks
    /// whether it still does when the claim runs (see [`PullRequestReader::claim_limit_notification`]).
    ///
    /// The claim is spent before the person is told, so a crash after it and before the result
    /// is drawn loses the notification rather than repeating it (at most once). A window that
    /// begins to close starts no claim.
    fn claim_limit_stop(&mut self, view: &ThreadPullRequest) {
        let (Some(link), Some(auto)) = (&view.link, &view.auto_fix) else {
            return;
        };
        let state = &auto.state;
        let stopped = state.stop_reason.as_deref() == Some("limit_reached")
            && state.limit_reached_epoch == Some(state.budget_epoch)
            && state.destination_thread_id.as_deref() == Some(view.thread_id.as_str())
            && state.link_revision == Some(link.link_revision);
        if !view.tracked_here
            || self.shutdown_requested
            || !stopped
            || self
                .pull_requests
                .ended
                .contains(&(view.thread_id.clone(), link.link_revision))
        {
            return;
        }
        let Some(reader) = self.pull_requests.reader.clone() else {
            return;
        };
        let epoch = state.budget_epoch;
        let key = link.key.clone();
        let seen = (
            format!("{}/{}#{}", key.host, key.repository, key.number),
            epoch,
        );
        if !self.pull_requests.remember_limit_notified(seen) {
            return;
        }
        let thread = view.thread_id.clone();
        let claim = PrLimitClaim {
            key: key.clone(),
            thread_id: thread.clone(),
            link_revision: link.link_revision,
            budget_epoch: epoch,
        };
        let probe = self.pull_requests.probe.clone();
        let closing = Arc::clone(&self.pull_requests.closing);
        let sender = self.pull_requests.sender.clone();
        let waker = Arc::clone(&self.pull_requests.waker);
        (self.pull_requests.spawner)(Box::new(move || {
            let still_held = || {
                !closing.load(std::sync::atomic::Ordering::SeqCst)
                    && probe.as_ref().is_none_or(|probe| probe(&claim.thread_id))
            };
            let claimed = reader.claim_limit_notification(&claim, &still_held);
            let _ = sender.send(Loaded::LimitClaim(thread, key, epoch, claimed));
            waker();
        }));
    }

    fn raise_auto_fix_limit_notification(&mut self, thread_id: &str, key: &PullRequestKey) {
        let title = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == thread_id)
            .map(|thread| thread.title.clone());
        let summary = format!(
            "Auto fix stopped on {} after {} sends",
            crate::model::pull_requests::key_label(key),
            storage::pull_request_auto_fix::PR_AUTO_FIX_LIMIT
        );
        let summary = match title {
            Some(title) => format!("{summary} · {title}"),
            None => summary,
        };
        self.notifications.push_pull_request(
            crate::model::notifications::NotificationKind::PullRequestAutoFixLimit {
                thread_id: workspace_ui::ThreadId::new(thread_id),
            },
            summary,
        );
    }

    /// Folds a pull request event another process published: only the display is read again.
    pub(super) fn apply_shared_pull_request_event(&mut self, event: &Event) {
        if let EventKind::PullRequest(change) = &event.kind {
            // A link another window ended is ended here too: nothing waiting for it notifies.
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
        }
    }

    /// An ownership change decides who polls a thread, and so what its bar says about
    /// tracking.
    pub(super) fn invalidate_pull_request_tracking(&mut self, thread: &str) {
        let notices_wait = self
            .pull_requests
            .pending_notices
            .iter()
            .any(|notice| notice.thread_id == thread);
        if notices_wait || self.pull_requests.model.linked(thread).is_some() {
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
        // What was refused may be what the screen no longer shows: read it again, and let the
        // person try again from the new state.
        self.pull_requests
            .issued
            .retain(|(thread, _)| thread != thread_id);
        self.pull_requests.request(thread_id);
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
            PullRequestAction::ToggleAutoFix { thread_id } => self.toggle_auto_fix(thread_id),
            PullRequestAction::ConfirmAutoFixDestination { thread_id } => {
                let Some(dialog) = self.pull_requests.destination_dialog.take() else {
                    return;
                };
                if dialog.thread_id != thread_id {
                    return;
                }
                let (link_revision, policy_revision) =
                    (dialog.link_revision, dialog.policy_revision);
                let confirmed_previous = Some(dialog.previous_thread_id);
                let (label, command) = match dialog.then {
                    DestinationChange::TurnOn => (
                        "move",
                        WorkbenchCommand::ChangePullRequestAutoFixDestination {
                            thread_id: thread_id.clone(),
                            link_revision,
                            policy_revision,
                            confirmed_previous,
                        },
                    ),
                    DestinationChange::ResetBudget => (
                        "reset",
                        WorkbenchCommand::ResetPullRequestAutoFixBudget {
                            thread_id: thread_id.clone(),
                            link_revision,
                            policy_revision,
                            confirmed_previous,
                        },
                    ),
                };
                let op = IssuedOp::Policy(link_revision, policy_revision, label);
                self.issue_pull_request_command(&thread_id, op, command);
            }
            PullRequestAction::CancelAutoFixDestination => {
                self.pull_requests.destination_dialog = None;
            }
            PullRequestAction::ResetAutoFixBudget { thread_id } => {
                let Some(controls) = self.auto_fix_controls(&thread_id) else {
                    return;
                };
                if !controls.reset.as_ref().is_some_and(|reset| reset.enabled) {
                    return;
                }
                if let Some(previous) = controls.moves_from.clone() {
                    self.ask_destination(
                        thread_id,
                        previous,
                        &controls,
                        DestinationChange::ResetBudget,
                    );
                    return;
                }
                let op =
                    IssuedOp::Policy(controls.link_revision, controls.policy_revision, "reset");
                self.issue_pull_request_command(
                    &thread_id,
                    op,
                    WorkbenchCommand::ResetPullRequestAutoFixBudget {
                        thread_id: thread_id.clone(),
                        link_revision: controls.link_revision,
                        policy_revision: controls.policy_revision,
                        confirmed_previous: None,
                    },
                );
            }
            PullRequestAction::SendToAgent { thread_id } => {
                let Some(controls) = self.auto_fix_controls(&thread_id) else {
                    return;
                };
                let Some(head) = controls.head_sha.clone().filter(|_| controls.send.enabled) else {
                    return;
                };
                let op = IssuedOp::Send(controls.link_revision, head.clone());
                self.issue_pull_request_command(
                    &thread_id,
                    op,
                    WorkbenchCommand::SendPullRequestNotice {
                        thread_id: thread_id.clone(),
                        link_revision: controls.link_revision,
                        expected_head: head,
                    },
                );
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
                // Nothing is assumed ended here: a refused Unlink leaves the link, and a granted
                // one shows in the next read and in the Unlinked event.
                if self.pull_request_permit(&thread_id) {
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
            } => {
                // Dismissing writes the thread's candidate state, so it takes the same write
                // mode (and the same refusals) as every other operation.
                if self.pull_request_permit(&thread_id) {
                    self.submit_pull_request_command(
                        WorkbenchCommand::DismissPullRequestCandidate {
                            thread_id,
                            candidate_id,
                        },
                    );
                }
            }
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

    /// The auto fix controls of a thread's bar, as the last read of the database says.
    #[must_use]
    pub fn auto_fix_controls(
        &self,
        thread: &str,
    ) -> Option<crate::model::pull_requests::AutoFixControls> {
        crate::model::pull_requests::auto_fix_controls(
            self.pull_requests.model.linked(thread)?,
            crate::model::pull_requests::now_ns(),
        )
    }

    /// The confirmation of moving auto fix to another thread, while it is open.
    #[must_use]
    pub fn destination_dialog(&self) -> Option<&DestinationDialog> {
        self.pull_requests.destination_dialog.as_ref()
    }

    /// Issues an operation once: a second click on the state that issued it does nothing until
    /// the runtime answered (an event, or a refusal).
    fn issue_pull_request_command(
        &mut self,
        thread_id: &str,
        op: IssuedOp,
        command: WorkbenchCommand,
    ) {
        if self
            .pull_requests
            .issued
            .contains(&(thread_id.to_owned(), op.clone()))
            || !self.pull_request_permit(thread_id)
        {
            return;
        }
        self.pull_requests.issued.insert((thread_id.to_owned(), op));
        self.submit_pull_request_command(command);
    }

    fn toggle_auto_fix(&mut self, thread_id: String) {
        let Some(controls) = self.auto_fix_controls(&thread_id) else {
            return;
        };
        if !controls.toggle.enabled {
            return;
        }
        let (link_revision, policy_revision) = (controls.link_revision, controls.policy_revision);
        if let Some(previous) = controls.moves_from.clone().filter(|_| !controls.on_here) {
            self.ask_destination(thread_id, previous, &controls, DestinationChange::TurnOn);
            return;
        }
        let enabled = !controls.on_here;
        let op = IssuedOp::Policy(
            link_revision,
            policy_revision,
            if enabled { "on" } else { "off" },
        );
        self.issue_pull_request_command(
            &thread_id.clone(),
            op,
            WorkbenchCommand::SetPullRequestAutoFix {
                thread_id,
                link_revision,
                policy_revision,
                enabled,
            },
        );
    }

    /// Opens the confirmation before another thread loses its auto fix to `thread_id`.
    fn ask_destination(
        &mut self,
        thread_id: String,
        previous: String,
        controls: &crate::model::pull_requests::AutoFixControls,
        then: DestinationChange,
    ) {
        let previous_label = self
            .sidebar
            .threads
            .iter()
            .find(|thread| thread.id.to_string() == previous)
            .map_or_else(|| previous.clone(), |thread| thread.title.clone());
        self.pull_requests.destination_dialog = Some(DestinationDialog {
            thread_id,
            previous_thread_id: previous,
            previous_label,
            link_revision: controls.link_revision,
            policy_revision: controls.policy_revision,
            then,
        });
    }

    /// Draws the confirmation of moving auto fix and runs its answer.
    pub(super) fn render_destination_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &self.pull_requests.destination_dialog else {
            return;
        };
        let thread_id = dialog.thread_id.clone();
        match crate::panes::pull_request_dialog::destination_dialog(ctx, dialog) {
            Some(crate::panes::pull_request_dialog::DestinationDialogAction::Move) => {
                self.apply_pull_request_action(PullRequestAction::ConfirmAutoFixDestination {
                    thread_id,
                });
            }
            Some(crate::panes::pull_request_dialog::DestinationDialogAction::Cancel) => {
                self.apply_pull_request_action(PullRequestAction::CancelAutoFixDestination);
            }
            None => {}
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
