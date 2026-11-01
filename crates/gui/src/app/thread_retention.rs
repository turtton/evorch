//! The GUI side of deleting archived threads: what the sidebar drops once a
//! deletion is confirmed, the per-thread status shown on hover, and the notices
//! about the start-up maintenance. Deciding and deleting stay in `runtime` and
//! `storage`; this module only drives and displays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use runtime::ownership::Registry;
use runtime::thread_retention::{
    BindingPaths, BindingStatus, SkipReason, StartupOutcome, StartupPaths, StartupReport,
    ThreadRetention, check_binding, startup_input,
};
use storage::thread_retention::{
    RetentionStatus, SidebarThread, ThreadRetentionPolicy, TombstoneSet,
};
use storage::{Database, StorageConfig};
use workspace_ui::{ThreadId, ThreadRecord};

use super::WorkbenchState;
use super::history::HistorySidebar;
use crate::model::notifications::NotificationKind;
use crate::model::tasks::AgentRunSource;

/// How often the statuses are recomputed while nothing changes them.
const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

const BYTES_PER_MB: u64 = 1_000_000;

pub(super) fn now_unix_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_secs()).ok())
        .unwrap_or(0)
}

/// Removes every thread a confirmed deletion tombstoned, with what hangs on it
/// (messages, drafts, the selection). A tombstone wins over the "keep the other
/// side's edit" rule of the sidebar merge, so a deleted thread never returns from a
/// file another window saved or from an old copy. Returns how many threads left.
pub(super) fn prune_tombstoned(history: &mut HistorySidebar, tombstones: &TombstoneSet) -> usize {
    if tombstones.threads.is_empty() {
        return 0;
    }
    let before = history.sidebar.threads.len();
    let removed_project = history
        .sidebar
        .active_thread
        .as_ref()
        .filter(|active| tombstones.contains_thread(&active.to_string()))
        .and_then(|active| {
            history
                .sidebar
                .threads
                .iter()
                .find(|thread| &thread.id == active)
                .map(|thread| thread.project_id.clone())
        });
    history
        .sidebar
        .threads
        .retain(|thread| !tombstones.contains_thread(&thread.id.to_string()));
    history
        .user_messages
        .retain(|message| !tombstones.contains_thread(&message.thread_id));
    history
        .composer_drafts
        .retain(|draft| !tombstones.contains_thread(&draft.thread_id));
    if history
        .sidebar
        .active_thread
        .as_ref()
        .is_some_and(|active| tombstones.contains_thread(&active.to_string()))
    {
        history.sidebar.active_thread = removed_project.and_then(|project| {
            ThreadRecord::partition_for_project(
                &history.sidebar.threads,
                &project,
                &BTreeMap::new(),
            )
            .0
            .first()
            .map(|thread| thread.id.clone())
        });
    }
    before - history.sidebar.threads.len()
}

/// [`prune_tombstoned`] for a sidebar read on its own (start-up, before the
/// workbench exists). Returns how many threads left.
pub fn prune_deleted(sidebar: &mut workspace_ui::SidebarState, tombstones: &TombstoneSet) -> usize {
    let mut history = HistorySidebar {
        sidebar: std::mem::take(sidebar),
        user_messages: Vec::new(),
        composer_drafts: Vec::new(),
    };
    let removed = prune_tombstoned(&mut history, tombstones);
    *sidebar = history.sidebar;
    removed
}

/// The statuses of the sidebar's threads, computed off the render thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionSnapshot {
    pub statuses: BTreeMap<String, RetentionStatus>,
    pub binding: BindingStatus,
}

/// Where the statuses come from. Production reads the database and the owner
/// registry; a test supplies its own.
pub trait RetentionScheduler: Send + Sync + 'static {
    /// # Errors
    /// A message for the log when the statuses could not be computed.
    fn schedule(
        &self,
        threads: Vec<SidebarThread>,
        now_ns: i64,
    ) -> Result<RetentionSnapshot, String>;
}

/// The production scheduler.
pub struct StorageRetentionScheduler {
    pub storage_config: StorageConfig,
    pub ownership_root: PathBuf,
    pub sidebar_path: PathBuf,
    /// Shared with the config reload, which replaces it.
    pub policy: Arc<Mutex<ThreadRetentionPolicy>>,
}

impl RetentionScheduler for StorageRetentionScheduler {
    fn schedule(
        &self,
        threads: Vec<SidebarThread>,
        now_ns: i64,
    ) -> Result<RetentionSnapshot, String> {
        let error = |error: &dyn std::fmt::Display| error.to_string();
        let database = Database::open(&self.storage_config).map_err(|e| error(&e))?;
        let paths = StartupPaths {
            db_path: self.storage_config.db_path.clone(),
            ownership_root: self.ownership_root.clone(),
            sidebar_path: self.sidebar_path.clone(),
            artifacts_dir: None,
        };
        let input = startup_input(&paths, threads, now_ns).map_err(|e| error(&e))?;
        let binding = check_binding(
            &database,
            BindingPaths {
                ownership_root: &self.ownership_root,
                sidebar_path: &self.sidebar_path,
            },
        )
        .map_err(|e| error(&e))?;
        // No registry yet means no owner holds anything.
        let registry = Registry::open_readonly(&self.ownership_root.join("owners.db")).ok();
        let policy = *self
            .policy
            .lock()
            .map_err(|_| "retention policy lock poisoned".to_owned())?;
        let statuses =
            ThreadRetention::schedule(&database, &input, &policy, &binding, registry.as_ref())
                .map_err(|e| error(&e))?;
        Ok(RetentionSnapshot {
            statuses: statuses.into_iter().collect(),
            binding,
        })
    }
}

/// What the sidebar shows about retention, and the job that refreshes it.
pub(super) struct RetentionState {
    scheduler: Option<Arc<dyn RetentionScheduler>>,
    sender: Sender<Result<RetentionSnapshot, String>>,
    receiver: Receiver<Result<RetentionSnapshot, String>>,
    in_flight: bool,
    dirty: bool,
    next_refresh: Option<Instant>,
    pub(super) statuses: BTreeMap<ThreadId, RetentionStatus>,
    binding: Option<BindingStatus>,
}

impl Default for RetentionState {
    fn default() -> Self {
        let (sender, receiver) = std::sync::mpsc::channel();
        Self {
            scheduler: None,
            sender,
            receiver,
            in_flight: false,
            dirty: false,
            next_refresh: None,
            statuses: BTreeMap::new(),
            binding: None,
        }
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Computes the retention statuses with `scheduler` from now on.
    #[must_use]
    pub fn with_retention_scheduler(mut self, scheduler: Arc<dyn RetentionScheduler>) -> Self {
        self.retention.scheduler = Some(scheduler);
        self.retention.dirty = true;
        self
    }

    /// The deleted identities this process runs with (read after the start-up
    /// locks became shared). The sidebar never holds them again.
    #[must_use]
    pub fn with_tombstones(mut self, tombstones: TombstoneSet) -> Self {
        self.tombstones = tombstones;
        self
    }

    /// Drops the threads already deleted from the sidebar, its messages and drafts,
    /// then saves. A failed save is harmless: the next load removes them again.
    pub fn prune_deleted_threads(&mut self) -> usize {
        let mut history = HistorySidebar {
            sidebar: self.sidebar.clone(),
            user_messages: self.history.clone(),
            composer_drafts: self.composer_drafts.clone(),
        };
        let removed = prune_tombstoned(&mut history, &self.tombstones);
        if removed > 0 {
            self.sidebar = history.sidebar;
            self.history = history.user_messages;
            self.composer_drafts = history.composer_drafts;
            self.save_sidebar();
        }
        removed
    }

    /// Posts the notice about the start-up maintenance, if there is something to say.
    pub fn announce_startup_retention(
        &mut self,
        report: &StartupReport,
        binding: Option<&BindingStatus>,
    ) {
        if let Some((kind, summary)) = startup_notice(report, binding) {
            self.notifications.push_retention(kind, summary);
        }
    }

    /// The statuses of archived threads, by thread.
    #[must_use]
    pub const fn retention_statuses(&self) -> &BTreeMap<ThreadId, RetentionStatus> {
        &self.retention.statuses
    }

    #[must_use]
    pub const fn retention_binding(&self) -> Option<&BindingStatus> {
        self.retention.binding.as_ref()
    }

    /// The sidebar's archive or pin state changed: recompute the statuses soon.
    pub(super) fn refresh_retention(&mut self) {
        self.retention.dirty = true;
        self.poll_retention(Instant::now());
    }

    /// Takes a finished refresh and starts the next one when it is due.
    pub(super) fn poll_retention(&mut self, now: Instant) {
        while let Ok(result) = self.retention.receiver.try_recv() {
            self.retention.in_flight = false;
            match result {
                Ok(snapshot) => {
                    self.retention.statuses = snapshot
                        .statuses
                        .into_iter()
                        .map(|(thread, status)| (ThreadId::new(thread), status))
                        .collect();
                    self.retention.binding = Some(snapshot.binding);
                }
                Err(error) => tracing::warn!(%error, "could not compute the retention status"),
            }
        }
        let Some(scheduler) = self.retention.scheduler.clone() else {
            return;
        };
        let due = self.retention.next_refresh.is_none_or(|next| now >= next);
        if self.retention.in_flight || !(self.retention.dirty || due) {
            return;
        }
        self.retention.dirty = false;
        self.retention.in_flight = true;
        self.retention.next_refresh = Some(now + REFRESH_INTERVAL);
        let threads = crate::startup::sidebar_threads(&self.sidebar);
        let now_ns = now_unix_seconds().saturating_mul(1_000_000_000);
        let sender = self.retention.sender.clone();
        let spawned = std::thread::Builder::new()
            .name("retention-schedule".into())
            .spawn(move || {
                let _ = sender.send(scheduler.schedule(threads, now_ns));
            });
        if let Err(error) = spawned {
            self.retention.in_flight = false;
            tracing::warn!(%error, "could not start the retention status job");
        }
    }

    /// Starts a due refresh and waits until none is in flight (tests only).
    #[cfg(test)]
    pub fn finish_retention_refresh(&mut self) {
        self.poll_retention(Instant::now());
        while self.retention.in_flight {
            let result = self
                .retention
                .receiver
                .recv()
                .expect("the refresh job always answers");
            self.retention.sender.send(result).unwrap();
            self.poll_retention(Instant::now());
        }
    }
}

/// The notice for a start, if it did or could not do something worth telling.
/// `NoExclusive` is silent (another window did the maintenance), and an unchanged
/// start is silent too.
#[must_use]
pub fn startup_notice(
    report: &StartupReport,
    binding: Option<&BindingStatus>,
) -> Option<(NotificationKind, String)> {
    let mismatch = match (&report.outcome, binding.or(report.binding.as_ref())) {
        (
            StartupOutcome::Skipped {
                reason: SkipReason::NoExclusive,
                ..
            },
            _,
        ) => return None,
        (_, Some(BindingStatus::Mismatch(reason))) => Some(reason.clone()),
        (
            StartupOutcome::Skipped {
                reason: SkipReason::BindingMismatch,
                ..
            },
            _,
        ) => Some("不明".to_owned()),
        _ => None,
    };
    if let Some(reason) = mismatch {
        return Some((
            NotificationKind::RetentionDisabled {
                reason: reason.clone(),
            },
            format!("削除機能は無効です（構成不一致: {reason}）"),
        ));
    }
    let StartupOutcome::Ran {
        confirmed,
        blocked,
        retried_over_budget,
        owner_registry_error,
        errors,
        ..
    } = &report.outcome
    else {
        return None;
    };
    let threads: usize = confirmed.iter().map(|family| family.threads.len()).sum();
    let runs: usize = confirmed.iter().map(|family| family.runs.len()).sum();
    let bytes: u64 = confirmed.iter().map(|family| family.estimate_bytes).sum();
    let mut skipped = Vec::new();
    if !blocked.is_empty() {
        skipped.push(format!("{} 系統が保留", blocked.len()));
    }
    if !retried_over_budget.is_empty() {
        skipped.push(format!(
            "{} 系統は次回起動で再試行",
            retried_over_budget.len()
        ));
    }
    if let Some(error) = owner_registry_error {
        skipped.push(format!("所有情報を読めません（{error}）"));
    }
    skipped.extend(errors.iter().cloned());
    if confirmed.is_empty() && owner_registry_error.is_none() && errors.is_empty() {
        return None;
    }
    let mut summary = format!(
        "起動時の保守: {threads} スレッド（{runs} run）を削除確定、約 {} MB を回収予定。",
        bytes.div_ceil(BYTES_PER_MB)
    );
    if !skipped.is_empty() {
        summary.push_str(&format!("スキップ理由: {}", skipped.join("、")));
    }
    Some((NotificationKind::RetentionStartup, summary))
}

/// The ids of the threads a start confirmed for deletion.
#[must_use]
pub fn confirmed_threads(report: &StartupReport) -> BTreeSet<String> {
    match &report.outcome {
        StartupOutcome::Ran { confirmed, .. } => confirmed
            .iter()
            .flat_map(|family| family.threads.iter().cloned())
            .collect(),
        StartupOutcome::Skipped { .. } => BTreeSet::new(),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use storage::thread_retention::{ConfirmedFamily, OversizedProgress};
    use workspace_ui::{ProjectId, SidebarState};

    use super::*;
    use crate::app::delivery_tests::state;
    use crate::app::history::{ComposerDraft, UserMessage};

    fn tombstones(threads: &[&str]) -> TombstoneSet {
        TombstoneSet {
            threads: threads.iter().map(|id| (*id).to_owned()).collect(),
            ..TombstoneSet::default()
        }
    }

    fn history(ids: &[&str]) -> HistorySidebar {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("p");
        sidebar
            .add_project(project.clone(), "P", std::path::Path::new("/tmp"))
            .unwrap();
        sidebar.select_project(&project).unwrap();
        for id in ids {
            sidebar
                .create_thread(ThreadId::new(*id), project.clone(), *id)
                .unwrap();
        }
        HistorySidebar {
            sidebar,
            user_messages: Vec::new(),
            composer_drafts: Vec::new(),
        }
    }

    fn message(thread: &str) -> UserMessage {
        let directory = tempfile::tempdir().unwrap();
        let mut state = state(&directory.path().join("sidebar.json"));
        let mut message = state.history.remove(0);
        message.thread_id = thread.to_owned();
        message
    }

    fn draft(thread: &str) -> ComposerDraft {
        ComposerDraft {
            draft_id: format!("draft-{thread}"),
            thread_id: thread.to_owned(),
            text: "text".into(),
            origin: "o".into(),
            images_needed: 0,
        }
    }

    #[test]
    fn pruning_removes_the_thread_its_messages_and_drafts_and_moves_the_selection() {
        // Given: the open thread is one a deletion removed.
        let mut saved = history(&["dead", "keep"]);
        saved.sidebar.switch_thread(&ThreadId::new("dead")).unwrap();
        saved.user_messages = vec![message("dead"), message("keep")];
        saved.composer_drafts = vec![draft("dead"), draft("keep")];

        // When: pruned.
        let removed = prune_tombstoned(&mut saved, &tombstones(&["dead"]));

        // Then: only the other thread's data is left and it is selected.
        assert_eq!(removed, 1);
        assert_eq!(saved.sidebar.threads.len(), 1);
        assert_eq!(saved.sidebar.active_thread, Some(ThreadId::new("keep")));
        assert_eq!(saved.user_messages.len(), 1);
        assert_eq!(saved.user_messages[0].thread_id, "keep");
        assert_eq!(saved.composer_drafts.len(), 1);
        assert_eq!(saved.composer_drafts[0].thread_id, "keep");
    }

    #[test]
    fn pruning_the_only_thread_leaves_no_selection() {
        let mut saved = history(&["dead"]);
        saved.sidebar.switch_thread(&ThreadId::new("dead")).unwrap();

        prune_tombstoned(&mut saved, &tombstones(&["dead"]));

        assert!(saved.sidebar.threads.is_empty());
        assert_eq!(saved.sidebar.active_thread, None);
    }

    #[test]
    fn a_deleted_thread_does_not_return_from_a_file_saved_before_its_deletion() {
        // Given: this process knows "dead" is deleted; the file, written earlier by
        // another window, still holds it with a message, and this window never had it.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("sidebar.json");
        let mut state = state(&path).with_tombstones(tombstones(&["dead"]));
        let mut stale = HistorySidebar {
            sidebar: state.sidebar.clone(),
            user_messages: vec![message("dead")],
            composer_drafts: vec![draft("dead")],
        };
        let project = stale.sidebar.projects[0].id.clone();
        stale
            .sidebar
            .create_thread(ThreadId::new("dead"), project, "dead")
            .unwrap();
        std::fs::write(&path, serde_json::to_vec(&stale_json(&stale)).unwrap()).unwrap();

        // When: this window saves (merging the file), and later imports it.
        state.history.clear();
        state.save_sidebar();
        let saved: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();

        // Then: the saved file and this window have no trace of the deleted thread.
        assert!(!saved.to_string().contains("dead"), "{saved}");
        assert!(
            state
                .sidebar
                .threads
                .iter()
                .all(|t| t.id.to_string() != "dead")
        );

        // And: the same holds when another window rewrites the old copy and this
        // window imports it.
        std::fs::write(&path, serde_json::to_vec(&stale_json(&stale)).unwrap()).unwrap();
        state.import_shared_sidebar();
        assert!(
            state
                .sidebar
                .threads
                .iter()
                .all(|t| t.id.to_string() != "dead")
        );
        assert!(state.history.iter().all(|m| m.thread_id != "dead"));
        assert!(state.composer_drafts.iter().all(|d| d.thread_id != "dead"));
    }

    fn stale_json(history: &HistorySidebar) -> serde_json::Value {
        let mut value = serde_json::to_value(&history.sidebar).unwrap();
        value["user_messages"] = serde_json::to_value(&history.user_messages).unwrap();
        value["composer_drafts"] = serde_json::to_value(&history.composer_drafts).unwrap();
        value
    }

    struct CountingScheduler(Arc<AtomicUsize>);

    impl RetentionScheduler for CountingScheduler {
        fn schedule(
            &self,
            threads: Vec<SidebarThread>,
            _now_ns: i64,
        ) -> Result<RetentionSnapshot, String> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(RetentionSnapshot {
                statuses: threads
                    .into_iter()
                    .map(|thread| {
                        let status = if thread.archived {
                            RetentionStatus::DueIn { days: 30 }
                        } else {
                            RetentionStatus::NotArchived
                        };
                        (thread.thread_id, status)
                    })
                    .collect(),
                binding: BindingStatus::Bound,
            })
        }
    }

    #[test]
    fn archiving_and_unarchiving_refresh_the_retention_statuses() {
        // Given: a window with a scheduler.
        let directory = tempfile::tempdir().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut state = state(&directory.path().join("sidebar.json"))
            .with_retention_scheduler(Arc::new(CountingScheduler(Arc::clone(&runs))));
        let thread = ThreadId::new("thread");
        state.finish_retention_refresh();
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(
            state.retention_statuses()[&thread],
            RetentionStatus::NotArchived
        );

        // When: the thread is archived.
        state.toggle_archive(thread.clone()).unwrap();
        state.finish_retention_refresh();

        // Then: the statuses were recomputed and show the deadline.
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        assert_eq!(
            state.retention_statuses()[&thread],
            RetentionStatus::DueIn { days: 30 }
        );
        assert_eq!(state.retention_binding(), Some(&BindingStatus::Bound));

        // When: it is restored.
        state.toggle_archive(thread.clone()).unwrap();
        state.finish_retention_refresh();

        // Then: the deadline is gone again.
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert_eq!(
            state.retention_statuses()[&thread],
            RetentionStatus::NotArchived
        );
    }

    fn ran(confirmed: Vec<ConfirmedFamily>, errors: Vec<String>) -> StartupReport {
        StartupReport {
            outcome: StartupOutcome::Ran {
                backfill: storage::BackfillProgress::default(),
                families: Vec::new(),
                confirmed,
                blocked: Vec::new(),
                retried_over_budget: Vec::new(),
                stale: Vec::new(),
                oversized: OversizedProgress::default(),
                vacuum_pages: 0,
                vacuum_skipped: false,
                scratch: runtime::thread_retention::ScratchReport::default(),
                owners_removed: 0,
                owner_registry_error: None,
                errors,
            },
            binding: Some(BindingStatus::Bound),
            tombstones: TombstoneSet::default(),
            elapsed: Duration::ZERO,
        }
    }

    fn family(threads: usize, runs: usize, bytes: u64) -> ConfirmedFamily {
        ConfirmedFamily {
            family_key: "f".into(),
            threads: (0..threads).map(|n| format!("t{n}")).collect(),
            runs: (0..runs).map(|n| format!("r{n}")).collect(),
            tasks: Vec::new(),
            estimate_bytes: bytes,
        }
    }

    #[test]
    fn the_startup_notice_counts_what_was_confirmed_and_names_what_was_skipped() {
        let report = ran(
            vec![family(2, 5, 1_500_000), family(1, 1, 10)],
            vec!["scratch failed".into()],
        );

        let (kind, summary) = startup_notice(&report, Some(&BindingStatus::Bound)).unwrap();

        assert_eq!(kind, NotificationKind::RetentionStartup);
        assert_eq!(
            summary,
            "起動時の保守: 3 スレッド（6 run）を削除確定、約 2 MB を回収予定。スキップ理由: scratch failed"
        );
    }

    #[test]
    fn the_startup_notice_is_silent_when_nothing_happened_or_another_window_ran_it() {
        assert!(startup_notice(&ran(Vec::new(), Vec::new()), None).is_none());
        assert!(startup_notice(&StartupReport::no_exclusive(), None).is_none());
    }

    #[test]
    fn a_binding_mismatch_disables_the_feature_with_its_reason() {
        let mismatch = BindingStatus::Mismatch("sidebar differs".into());
        let skipped = StartupReport {
            outcome: StartupOutcome::Skipped {
                reason: SkipReason::BindingMismatch,
                maintenance: None,
            },
            binding: Some(mismatch.clone()),
            tombstones: TombstoneSet::default(),
            elapsed: Duration::ZERO,
        };

        let from_report = startup_notice(&skipped, None).unwrap();
        // The binding read again after the locks were shared wins over the phase's.
        let from_reread =
            startup_notice(&ran(vec![family(1, 1, 1)], Vec::new()), Some(&mismatch)).unwrap();

        for (kind, summary) in [from_report, from_reread] {
            assert_eq!(
                kind,
                NotificationKind::RetentionDisabled {
                    reason: "sidebar differs".into()
                }
            );
            assert_eq!(summary, "削除機能は無効です（構成不一致: sidebar differs）");
        }
    }

    #[test]
    fn announcing_posts_one_notification_of_the_right_kind() {
        let directory = tempfile::tempdir().unwrap();
        let mut state = state(&directory.path().join("sidebar.json"));

        state.announce_startup_retention(&ran(vec![family(1, 2, 3)], Vec::new()), None);
        state.announce_startup_retention(&StartupReport::no_exclusive(), None);

        let kinds: Vec<_> = state
            .notifications()
            .items()
            .map(|n| n.kind.clone())
            .collect();
        assert_eq!(kinds, [NotificationKind::RetentionStartup]);
    }
}
