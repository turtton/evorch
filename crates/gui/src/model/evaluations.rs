//! The Evaluations panel's model: which evaluation groups exist, which one is
//! open, and what its members, trials, report and replay show.
//!
//! Everything on disk is read by [`EvaluationReader`] on a worker thread. The
//! UI thread only starts a read, drains finished ones through a channel and
//! draws what it holds, so a slow or stuck read never blocks a frame. A result
//! carries the generation of the selection it was started for, and one whose
//! selection has changed since is dropped.
//!
//! Time is an argument ([`EvaluationsModel::poll_once`] takes `now`), so the
//! polling schedule is the same code in the GUI and in tests.

use std::collections::VecDeque;
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use eval::inspection::{
    Artifact, BoundedText, EvaluationGroupSnapshot, EvaluationInspector, FileState, GroupListing,
    GroupPhase, ReportStatus, TrialState,
};

mod markdown;
pub mod present;
mod replay;

pub use replay::{
    Limits as ReplayLimits, MAX_EVENTS as REPLAY_MAX_EVENTS,
    MAX_SCANNED_ROWS as REPLAY_MAX_SCANNED_ROWS, ReplayData, ReplayLoad, ReplayRun, ReplayTarget,
    load_with as load_replay_with,
};

/// Reads started and not yet drained, at most (stale ones included).
pub const MAX_IN_FLIGHT: usize = 3;
/// How often the open group is read again while its evaluation runs.
pub const RUNNING_GROUP_POLL: Duration = Duration::from_secs(1);
/// How often the group list is read again while an evaluation runs.
pub const RUNNING_LIST_POLL: Duration = Duration::from_secs(3);
/// How often everything is read again when nothing is known to run.
pub const IDLE_POLL: Duration = Duration::from_secs(15);
/// Groups listed.
pub const LIST_LIMIT: usize = 50;
/// Bytes of `report.md` read for display.
pub const MARKDOWN_BYTES: u64 = 256 * 1024;
/// Finished replays kept for going back and forth.
pub const REPLAY_CACHE: usize = 2;

/// What the panel reads. The GUI uses [`InspectorReader`]; tests give their own
/// to control when a read ends.
pub trait EvaluationReader: Send + Sync + 'static {
    fn list(&self, limit: usize) -> GroupListing;
    fn group(&self, id: &str) -> Artifact<EvaluationGroupSnapshot>;
    fn report_markdown(&self, group: &str, member: usize, max_bytes: u64) -> Artifact<BoundedText>;
    fn replay(&self, target: &ReplayTarget) -> ReplayLoad;
}

/// Reads the evaluations under an evaluation root through the shared inspector.
pub struct InspectorReader(pub EvaluationInspector);

impl EvaluationReader for InspectorReader {
    fn list(&self, limit: usize) -> GroupListing {
        self.0.list_groups(limit)
    }

    fn group(&self, id: &str) -> Artifact<EvaluationGroupSnapshot> {
        self.0.group(id)
    }

    fn report_markdown(&self, group: &str, member: usize, max_bytes: u64) -> Artifact<BoundedText> {
        self.0.member_report_markdown(group, member, max_bytes)
    }

    fn replay(&self, target: &ReplayTarget) -> ReplayLoad {
        replay::load(target)
    }
}

type Job = Box<dyn FnOnce() + Send>;
/// Runs a job off the UI thread.
pub type Spawner = Arc<dyn Fn(Job) + Send + Sync>;
/// Asks the UI for a frame after a read finished.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    List,
    Group,
    Markdown,
    Replay,
}

/// A value that is being read, was read, or cannot be shown.
#[derive(Debug, Clone, PartialEq)]
pub enum Loaded<T> {
    Loading,
    Ready(T),
    Missing,
    Problem(String),
}

impl<T> Loaded<T> {
    fn of(artifact: Artifact<T>) -> Self {
        match artifact {
            Artifact::Present(value) => Self::Ready(value),
            Artifact::Missing => Self::Missing,
            Artifact::Problem(problem) => Self::Problem(problem.to_string()),
        }
    }
}

/// `report.md`, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportText {
    /// Links and images are already plain text.
    pub text: String,
    pub total_bytes: u64,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownKey {
    pub group: String,
    pub member: usize,
    /// When the report was generated, so a regenerated report is read again.
    pub generated_at: Option<String>,
}

/// One trial: a group's member and its `baseline-1` style name.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ReplayKey {
    pub group: String,
    pub member: usize,
    pub member_dir: String,
    pub trial: String,
}

/// What the replay area shows.
#[derive(Debug, Clone)]
pub enum ReplayView {
    Loading,
    Ready(Arc<ReplayData>),
    /// The database was unusable; the conversation file stands in.
    Fallback {
        reason: String,
        conversation: Artifact<Vec<eval::conversation::Entry>>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    pub group: Option<String>,
    pub member: Option<usize>,
    pub trial: Option<String>,
}

struct GroupSlot {
    id: String,
    snapshot: Artifact<EvaluationGroupSnapshot>,
}

struct ReplaySlot {
    key: ReplayKey,
    target: ReplayTarget,
    view: ReplayView,
    run: Option<String>,
}

enum Payload {
    List(GroupListing),
    Group {
        id: String,
        group: Artifact<EvaluationGroupSnapshot>,
    },
    Markdown {
        key: MarkdownKey,
        text: Artifact<ReportTextParts>,
    },
    Replay {
        key: ReplayKey,
        load: ReplayLoad,
    },
    Panicked,
}

struct ReportTextParts {
    text: String,
    total_bytes: u64,
    truncated: bool,
}

struct Completed {
    kind: Kind,
    generation: u64,
    payload: Payload,
}

/// What one [`EvaluationsModel::poll_once`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Poll {
    /// Something shown changed; draw a frame.
    pub changed: bool,
    /// When to poll again if nothing else wakes the UI before.
    pub repaint_after: Option<Duration>,
}

pub struct EvaluationsModel {
    reader: Option<Arc<dyn EvaluationReader>>,
    spawner: Spawner,
    waker: Waker,
    sender: Sender<Completed>,
    receiver: Receiver<Completed>,
    in_flight: Vec<(Kind, u64)>,
    group_generation: u64,
    markdown_generation: u64,
    replay_generation: u64,
    listing: Option<GroupListing>,
    selection: Selection,
    group: Option<GroupSlot>,
    markdown: Option<(MarkdownKey, Loaded<ReportText>)>,
    replay: Option<ReplaySlot>,
    replay_cache: VecDeque<(ReplayKey, Arc<ReplayData>)>,
    list_stale: bool,
    group_stale: bool,
    markdown_stale: bool,
    list_due: Option<Instant>,
    group_due: Option<Instant>,
    failure: Option<String>,
    /// The panel was drawn since the last poll.
    rendered: bool,
    /// The panel was drawn before the last poll, so it is on screen.
    shown: bool,
}

impl Default for EvaluationsModel {
    fn default() -> Self {
        Self::new(None, thread_spawner(), Arc::new(|| {}))
    }
}

/// A fresh thread per read: reads are rare, short and bounded by
/// [`MAX_IN_FLIGHT`].
pub fn thread_spawner() -> Spawner {
    Arc::new(|job| {
        let started = std::thread::Builder::new()
            .name("evaluations-reader".into())
            .spawn(job);
        if let Err(error) = started {
            tracing::warn!(%error, "could not start an evaluation reader thread");
        }
    })
}

impl EvaluationsModel {
    pub fn new(reader: Option<Arc<dyn EvaluationReader>>, spawner: Spawner, waker: Waker) -> Self {
        let (sender, receiver) = channel();
        Self {
            reader,
            spawner,
            waker,
            sender,
            receiver,
            in_flight: Vec::new(),
            group_generation: 0,
            markdown_generation: 0,
            replay_generation: 0,
            listing: None,
            selection: Selection::default(),
            group: None,
            markdown: None,
            replay: None,
            replay_cache: VecDeque::new(),
            list_stale: true,
            group_stale: false,
            markdown_stale: false,
            list_due: None,
            group_due: None,
            failure: None,
            rendered: false,
            shown: false,
        }
    }

    pub fn with_reader(mut self, reader: Arc<dyn EvaluationReader>) -> Self {
        self.reader = Some(reader);
        self
    }

    pub fn with_spawner(mut self, spawner: Spawner) -> Self {
        self.spawner = spawner;
        self
    }

    pub fn with_waker(mut self, waker: Waker) -> Self {
        self.waker = waker;
        self
    }

    pub fn set_waker(&mut self, waker: Waker) {
        self.waker = waker;
    }

    /// The panel is being drawn. Returns whether it was not on screen before,
    /// when the caller should ask for a frame so the poll notices it.
    pub fn note_rendered(&mut self) -> bool {
        self.rendered = true;
        !self.shown
    }

    /// Whether the panel was drawn since the last call; the answer to give
    /// [`Self::poll_once`] as `visible`.
    pub fn take_rendered(&mut self) -> bool {
        self.shown = std::mem::take(&mut self.rendered);
        self.shown
    }

    /// Whether evaluations can be read at all (a user config directory exists).
    pub fn is_available(&self) -> bool {
        self.reader.is_some()
    }

    pub fn listing(&self) -> Option<&GroupListing> {
        self.listing.as_ref()
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    pub fn failure(&self) -> Option<&str> {
        self.failure.as_deref()
    }

    /// The open group, once read.
    pub fn group(&self) -> Option<&Artifact<EvaluationGroupSnapshot>> {
        self.group
            .as_ref()
            .filter(|slot| Some(&slot.id) == self.selection.group.as_ref())
            .map(|slot| &slot.snapshot)
    }

    pub fn markdown(&self) -> Option<&Loaded<ReportText>> {
        self.markdown.as_ref().map(|(_, loaded)| loaded)
    }

    pub fn replay(&self) -> Option<(&ReplayKey, &ReplayView)> {
        self.replay.as_ref().map(|slot| (&slot.key, &slot.view))
    }

    /// The run of the replay being read.
    pub fn replay_run(&self) -> Option<&str> {
        self.replay.as_ref().and_then(|slot| slot.run.as_deref())
    }

    /// Reads started and not yet drained.
    pub fn in_flight(&self) -> usize {
        self.in_flight.len()
    }

    /// Whether some group is known to run, which is when polling is fast.
    pub fn any_running(&self) -> bool {
        self.listing.as_ref().is_some_and(|listing| {
            listing.entries.iter().any(|entry| {
                matches!(
                    entry.group.present().map(|group| group.phase),
                    Some(GroupPhase::Running)
                )
            })
        }) || self.group_running()
    }

    fn group_running(&self) -> bool {
        matches!(
            self.group().and_then(Artifact::present).map(|g| g.phase),
            Some(GroupPhase::Running)
        )
    }

    /// Read everything again as soon as the panel is shown (a notification
    /// about a finished evaluation, or the user's Refresh).
    pub fn request_refresh(&mut self) {
        self.list_stale = true;
        self.group_stale = self.selection.group.is_some();
        self.markdown_stale = true;
    }

    /// Open `id`; nothing changes when it is already open.
    pub fn select_group(&mut self, id: &str) -> bool {
        if self.selection.group.as_deref() == Some(id) {
            return false;
        }
        self.selection = Selection {
            group: Some(id.to_owned()),
            member: None,
            trial: None,
        };
        self.group = None;
        self.group_due = None;
        self.group_stale = true;
        self.clear_member_views();
        self.group_generation += 1;
        true
    }

    pub fn clear_group(&mut self) {
        if self.selection.group.take().is_some() {
            self.selection = Selection::default();
            self.group = None;
            self.group_due = None;
            self.group_stale = false;
            self.clear_member_views();
            self.group_generation += 1;
        }
    }

    /// Back from a member to its group.
    pub fn clear_member(&mut self) {
        if self.selection.member.take().is_some() {
            self.clear_member_views();
        }
    }

    pub fn select_member(&mut self, member: usize) -> bool {
        if self.selection.group.is_none() || self.selection.member == Some(member) {
            return false;
        }
        self.selection.member = Some(member);
        self.selection.trial = None;
        self.clear_member_views();
        true
    }

    /// Open the replay of a finished trial of the open member. Returns whether
    /// it was opened; a trial that has not ended has no replay.
    pub fn select_trial(&mut self, trial: &str) -> bool {
        let (Some(group), Some(member)) = (self.selection.group.clone(), self.selection.member)
        else {
            return false;
        };
        let Some(Artifact::Present(snapshot)) = self.group() else {
            return false;
        };
        let Some(snapshot_member) = snapshot.members.get(member) else {
            return false;
        };
        let Some(found) = snapshot_member
            .trials
            .iter()
            .find(|candidate| candidate.name() == trial)
        else {
            return false;
        };
        if !matches!(found.state, TrialState::Finished | TrialState::Failed) {
            return false;
        }
        let present = |file: &eval::inspection::FileRef| {
            matches!(file.state, FileState::Present { .. }).then(|| file.path.clone())
        };
        let target = match &found.artifacts {
            Some(Artifact::Present(artifacts)) => ReplayTarget {
                database: present(&artifacts.database),
                conversation: present(&artifacts.conversation),
            },
            _ => ReplayTarget {
                database: None,
                conversation: None,
            },
        };
        let key = ReplayKey {
            group,
            member,
            member_dir: snapshot_member.dir_name.clone(),
            trial: trial.to_owned(),
        };
        self.selection.trial = Some(trial.to_owned());
        self.replay_generation += 1;
        let cached = self
            .replay_cache
            .iter()
            .position(|(cached, _)| cached == &key)
            .and_then(|index| self.replay_cache.remove(index));
        let view = match cached {
            Some((cached_key, data)) => {
                self.replay_cache.push_back((cached_key, Arc::clone(&data)));
                ReplayView::Ready(data)
            }
            None => ReplayView::Loading,
        };
        let run = match &view {
            ReplayView::Ready(data) => data.default_run().map(str::to_owned),
            _ => None,
        };
        self.replay = Some(ReplaySlot {
            key,
            target,
            view,
            run,
        });
        true
    }

    pub fn close_replay(&mut self) {
        if self.selection.trial.take().is_some() {
            self.replay = None;
            self.replay_generation += 1;
        }
    }

    /// Show another run of the open replay.
    pub fn select_replay_run(&mut self, run_id: &str) -> bool {
        match &mut self.replay {
            Some(slot) if matches!(slot.view, ReplayView::Ready(_)) => {
                if slot.run.as_deref() == Some(run_id) {
                    return false;
                }
                slot.run = Some(run_id.to_owned());
                true
            }
            _ => false,
        }
    }

    fn clear_member_views(&mut self) {
        self.markdown = None;
        self.markdown_stale = false;
        self.markdown_generation += 1;
        self.replay = None;
        self.selection.trial = None;
        self.replay_generation += 1;
    }

    /// Drain finished reads, start the ones that are due and say when to come
    /// back. Never blocks. While `visible` is false nothing is started: a
    /// hidden panel does not read.
    pub fn poll_once(&mut self, now: Instant, visible: bool) -> Poll {
        let changed = self.drain(now);
        if !visible || self.reader.is_none() {
            return Poll {
                changed,
                repaint_after: None,
            };
        }
        self.start_due(now);
        Poll {
            changed,
            repaint_after: self.next_due(now),
        }
    }

    /// Block until at least one read finished. Only for tests, which have no
    /// frame loop to come back to; the GUI never calls it.
    #[doc(hidden)]
    pub fn wait_for_result(&mut self, now: Instant) -> bool {
        if self.in_flight.is_empty() {
            return false;
        }
        let Ok(first) = self.receiver.recv() else {
            return false;
        };
        let mut changed = self.accept(first, now);
        changed |= self.drain(now);
        changed
    }

    fn drain(&mut self, now: Instant) -> bool {
        let mut changed = false;
        while let Ok(done) = self.receiver.try_recv() {
            changed |= self.accept(done, now);
        }
        changed
    }

    fn accept(&mut self, done: Completed, now: Instant) -> bool {
        if let Some(index) = self
            .in_flight
            .iter()
            .position(|(kind, generation)| *kind == done.kind && *generation == done.generation)
        {
            self.in_flight.remove(index);
        }
        match done.payload {
            Payload::Panicked => {
                self.failure = Some("an evaluation read stopped unexpectedly".to_owned());
                true
            }
            Payload::List(listing) => {
                self.list_due = Some(now + self.list_interval(&listing));
                self.failure = None;
                if self.listing.as_ref() == Some(&listing) {
                    return false;
                }
                self.listing = Some(listing);
                true
            }
            Payload::Group { id, group } => {
                if done.generation != self.group_generation
                    || self.selection.group.as_deref() != Some(id.as_str())
                {
                    return false;
                }
                self.group_due = match group.present().map(|snapshot| snapshot.phase) {
                    Some(GroupPhase::Running) => Some(now + RUNNING_GROUP_POLL),
                    Some(GroupPhase::Finished | GroupPhase::Incomplete) => None,
                    Some(GroupPhase::Unverified | GroupPhase::Partial) | None => {
                        Some(now + IDLE_POLL)
                    }
                };
                if self
                    .group
                    .as_ref()
                    .is_some_and(|slot| slot.id == id && slot.snapshot == group)
                {
                    return false;
                }
                self.group = Some(GroupSlot {
                    id,
                    snapshot: group,
                });
                true
            }
            Payload::Markdown { key, text } => {
                if done.generation != self.markdown_generation
                    || self.markdown.as_ref().map(|(wanted, _)| wanted) != Some(&key)
                {
                    return false;
                }
                let loaded = Loaded::of(text.map(|parts| ReportText {
                    text: parts.text,
                    total_bytes: parts.total_bytes,
                    truncated: parts.truncated,
                }));
                self.markdown = Some((key, loaded));
                true
            }
            Payload::Replay { key, load } => {
                if done.generation != self.replay_generation {
                    return false;
                }
                let Some(slot) = self.replay.as_mut().filter(|slot| slot.key == key) else {
                    return false;
                };
                slot.view = match load {
                    ReplayLoad::Ready(data) => {
                        slot.run = data.default_run().map(str::to_owned);
                        self.replay_cache.retain(|(cached, _)| cached != &key);
                        self.replay_cache.push_back((key, Arc::clone(&data)));
                        while self.replay_cache.len() > REPLAY_CACHE {
                            self.replay_cache.pop_front();
                        }
                        ReplayView::Ready(data)
                    }
                    ReplayLoad::Fallback {
                        reason,
                        conversation,
                    } => ReplayView::Fallback {
                        reason,
                        conversation,
                    },
                };
                true
            }
        }
    }

    fn list_interval(&self, listing: &GroupListing) -> Duration {
        let running = listing.entries.iter().any(|entry| {
            matches!(
                entry.group.present().map(|group| group.phase),
                Some(GroupPhase::Running)
            )
        });
        if running {
            RUNNING_LIST_POLL
        } else {
            IDLE_POLL
        }
    }

    fn busy(&self, kind: Kind, generation: u64) -> bool {
        self.in_flight
            .iter()
            .any(|(running, current)| *running == kind && *current == generation)
    }

    fn can_start(&self, kind: Kind, generation: u64) -> bool {
        self.in_flight.len() < MAX_IN_FLIGHT && !self.busy(kind, generation)
    }

    fn start_due(&mut self, now: Instant) {
        self.start_replay();
        self.start_markdown();
        self.start_group(now);
        self.start_list(now);
    }

    fn start_replay(&mut self) {
        let generation = self.replay_generation;
        let Some(slot) = &self.replay else { return };
        if !matches!(slot.view, ReplayView::Loading) || !self.can_start(Kind::Replay, generation) {
            return;
        }
        let (key, target) = (slot.key.clone(), slot.target.clone());
        self.dispatch(Kind::Replay, generation, move |reader| Payload::Replay {
            load: reader.replay(&target),
            key,
        });
    }

    fn wanted_markdown(&self) -> Option<MarkdownKey> {
        let member = self.selection.member?;
        let group = self.selection.group.as_ref()?;
        let snapshot = self.group()?.present()?;
        let found = snapshot.members.get(member)?;
        let file = found.report_markdown.as_ref()?;
        if !matches!(file.state, FileState::Present { .. }) {
            return None;
        }
        Some(MarkdownKey {
            group: group.clone(),
            member,
            generated_at: match &found.report {
                ReportStatus::Available(summary) => Some(summary.generated_at.clone()),
                _ => None,
            },
        })
    }

    fn start_markdown(&mut self) {
        let generation = self.markdown_generation;
        let Some(key) = self.wanted_markdown() else {
            if self.markdown.take().is_some() {
                self.markdown_generation += 1;
            }
            return;
        };
        let current = self.markdown.as_ref().map(|(held, _)| held);
        if current == Some(&key) && !self.markdown_stale {
            return;
        }
        if !self.can_start(Kind::Markdown, generation) {
            return;
        }
        self.markdown_stale = false;
        if current != Some(&key) {
            self.markdown = Some((key.clone(), Loaded::Loading));
        }
        let (group, member) = (key.group.clone(), key.member);
        self.dispatch(Kind::Markdown, generation, move |reader| {
            let text = match reader.report_markdown(&group, member, MARKDOWN_BYTES) {
                Artifact::Present(text) => Artifact::Present(ReportTextParts {
                    text: markdown::neutralize(&text.text),
                    total_bytes: text.total_bytes,
                    truncated: text.truncated,
                }),
                Artifact::Missing => Artifact::Missing,
                Artifact::Problem(problem) => Artifact::Problem(problem),
            };
            Payload::Markdown { key, text }
        });
    }

    fn start_group(&mut self, now: Instant) {
        let generation = self.group_generation;
        let Some(id) = self.selection.group.clone() else {
            return;
        };
        let due = self.group_stale
            || self.group().is_none()
            || self.group_due.is_some_and(|due| now >= due);
        if !due || !self.can_start(Kind::Group, generation) {
            return;
        }
        self.group_stale = false;
        self.group_due = None;
        self.dispatch(Kind::Group, generation, move |reader| Payload::Group {
            group: reader.group(&id),
            id,
        });
    }

    fn start_list(&mut self, now: Instant) {
        let due = self.list_stale || self.list_due.is_some_and(|due| now >= due);
        if !due || !self.can_start(Kind::List, 0) {
            return;
        }
        self.list_stale = false;
        self.list_due = None;
        self.dispatch(Kind::List, 0, |reader| {
            Payload::List(reader.list(LIST_LIMIT))
        });
    }

    fn next_due(&self, now: Instant) -> Option<Duration> {
        // With every read slot taken nothing can start, so no timer helps: a
        // finishing read wakes the UI and the next poll starts what is due. A
        // zero delay here would repaint in a loop until a slot frees.
        if self.in_flight.len() >= MAX_IN_FLIGHT {
            return None;
        }
        [
            (Kind::List, 0, self.list_due),
            (Kind::Group, self.group_generation, self.group_due),
        ]
        .into_iter()
        .filter(|(kind, generation, _)| !self.busy(*kind, *generation))
        .filter_map(|(_, _, due)| due)
        .map(|due| due.saturating_duration_since(now))
        .min()
    }

    fn dispatch(
        &mut self,
        kind: Kind,
        generation: u64,
        read: impl FnOnce(&dyn EvaluationReader) -> Payload + Send + 'static,
    ) {
        let Some(reader) = self.reader.clone() else {
            return;
        };
        self.in_flight.push((kind, generation));
        let (sender, waker) = (self.sender.clone(), Arc::clone(&self.waker));
        (self.spawner)(Box::new(move || {
            let payload = std::panic::catch_unwind(AssertUnwindSafe(|| read(reader.as_ref())))
                .unwrap_or(Payload::Panicked);
            let _ = sender.send(Completed {
                kind,
                generation,
                payload,
            });
            waker();
        }));
    }
}

/// Test helper: a spawner that queues jobs and runs them when told, so a test
/// decides when a read finishes without sleeping.
#[doc(hidden)]
#[derive(Default, Clone)]
pub struct ManualSpawner {
    jobs: Arc<Mutex<VecDeque<Job>>>,
}

impl ManualSpawner {
    pub fn spawner(&self) -> Spawner {
        let jobs = Arc::clone(&self.jobs);
        Arc::new(move |job| {
            jobs.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push_back(job);
        })
    }

    pub fn pending(&self) -> usize {
        self.jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Run the oldest queued read on this thread.
    pub fn run_next(&self) -> bool {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        job.map(|job| job()).is_some()
    }

    /// Run the most recently queued read, which lets a test finish a newer read
    /// before an older one.
    pub fn run_newest(&self) -> bool {
        let job = self
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_back();
        job.map(|job| job()).is_some()
    }

    pub fn run_all(&self) {
        while self.run_next() {}
    }
}
