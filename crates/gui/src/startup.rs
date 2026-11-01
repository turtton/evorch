//! What the GUI process does before the workbench exists: take the exclusive
//! locks, read `sidebar.json`, let the runtime confirm the deletion of archived
//! threads (the only place a deletion is confirmed), then hold the locks shared for
//! the rest of the process.
//!
//! The order is fixed and observable through [`StartupEvent`]:
//!
//! 1. `try_exclusive` (database lease, then ownership-root lease). Another evorch
//!    process holding either one means this start skips the phase and only waits for
//!    shared locks.
//! 2. The sidebar is read while exclusive, so the phase sees the archive state of
//!    the only process that can change it.
//! 3. The phase runs, reporting each stage.
//! 4. `downgrade`: the locks become shared. A flock conversion is not atomic, so the
//!    tombstones and the binding are read **again** afterwards; those values, not the
//!    phase's, are what the process runs with.
//!
//! [`spawn_startup`] runs all of it on a background thread so the window can show
//! the progress.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use runtime::thread_retention::{
    BindingPaths, BindingStatus, RetentionError, StartupBudget, StartupPaths, StartupProgress,
    StartupReport, check_binding, run_exclusive_phase_with_progress, startup_input,
};
use storage::thread_retention::{SidebarThread, ThreadRetentionPolicy, TombstoneSet};
use storage::{Database, SharedLeases, StorageConfig, StorageError};
use workspace_ui::SidebarState;

/// How long the shared locks are waited for: another process may be in its own
/// exclusive phase (or offline maintenance) for a while.
pub const SHARED_LOCK_WAIT: Duration = Duration::from_secs(30);

/// Where the start-up finds its inputs.
#[derive(Debug, Clone)]
pub struct StartupPlan {
    pub db_path: PathBuf,
    pub ownership_root: PathBuf,
    /// The sidebar file; `None` runs with an empty sidebar.
    pub sidebar_path: Option<PathBuf>,
    pub artifacts_dir: Option<PathBuf>,
    /// `false` takes the shared locks only (the demo): nothing is ever deleted.
    pub run_phase: bool,
}

/// One step of the start-up, in the order it happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupEvent {
    /// Both locks are held exclusively.
    ExclusiveTaken,
    /// Another process holds a lock: the phase is skipped.
    ExclusiveUnavailable,
    /// The sidebar was read.
    SidebarLoaded { threads: usize },
    /// The phase reported progress.
    Phase(StartupProgress),
    /// The locks are held shared.
    Shared,
}

#[derive(Debug, thiserror::Error)]
pub enum StartupError {
    #[error("storage start-up failed: {0}")]
    Storage(#[from] StorageError),
    #[error("thread retention start-up failed: {0}")]
    Retention(#[from] RetentionError),
    #[error("sidebar state failed: {0}")]
    Sidebar(#[from] workspace_ui::SidebarError),
    #[error("start-up thread panicked: {0}")]
    Panicked(String),
}

/// What the start-up hands to the workbench.
pub struct StartupPrelude {
    /// The sidebar as read while exclusive; `None` for a plan without a sidebar file.
    pub sidebar: Option<SidebarState>,
    pub report: StartupReport,
    /// Held shared for the life of the process. Drop it last.
    pub leases: SharedLeases,
    /// Read again after the locks were shared; what the process runs with.
    pub tombstones: TombstoneSet,
    /// Read again after the locks were shared; `None` when it could not be examined.
    pub binding: Option<BindingStatus>,
}

/// The sidebar's threads as the retention phase reads them.
#[must_use]
pub fn sidebar_threads(sidebar: &SidebarState) -> Vec<SidebarThread> {
    sidebar
        .threads
        .iter()
        .map(|thread| SidebarThread {
            thread_id: thread.id.to_string(),
            archived: thread.archived,
            archived_at_s: thread.archived_at,
            pinned: thread.pinned,
            updated_at_s: thread.last_activity(),
            created_at_s: thread.created_at,
            archive_revision: Some(thread.archive_revision),
            pin_revision: Some(thread.pin_revision),
        })
        .collect()
}

fn load_sidebar(path: &std::path::Path) -> Result<SidebarState, StartupError> {
    if path.exists() {
        Ok(workspace_ui::load_sidebar(path)?)
    } else {
        Ok(SidebarState::default())
    }
}

fn now_ns() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0)
}

fn reread(plan: &StartupPlan) -> Result<(TombstoneSet, Option<BindingStatus>), StartupError> {
    let database = Database::open(&StorageConfig {
        db_path: plan.db_path.clone(),
        ..StorageConfig::default()
    })?;
    let tombstones = database.tombstones()?;
    let binding = plan.sidebar_path.as_deref().and_then(|sidebar_path| {
        check_binding(
            &database,
            BindingPaths {
                ownership_root: &plan.ownership_root,
                sidebar_path,
            },
        )
        .map_err(|error| tracing::warn!(%error, "could not examine the retention binding"))
        .ok()
    });
    Ok((tombstones, binding))
}

/// Runs the start-up on the calling thread. `policy_for` derives the retention
/// policy from the sidebar (the project it names decides which configuration
/// applies); it runs only when the phase does.
///
/// # Errors
/// Fails when a lock cannot be taken for another reason than being held, when the
/// sidebar cannot be read, or when the phase fails before confirming anything.
pub fn run_startup(
    plan: &StartupPlan,
    policy_for: impl FnOnce(&SidebarState) -> ThreadRetentionPolicy,
    emit: &mut dyn FnMut(StartupEvent),
) -> Result<StartupPrelude, StartupError> {
    let exclusive = if plan.run_phase {
        storage::try_exclusive(&plan.db_path, &plan.ownership_root)?
    } else {
        None
    };
    let Some(exclusive) = exclusive else {
        if plan.run_phase {
            emit(StartupEvent::ExclusiveUnavailable);
        }
        let leases =
            storage::shared_with_wait(&plan.db_path, &plan.ownership_root, SHARED_LOCK_WAIT)?;
        emit(StartupEvent::Shared);
        let sidebar = match (&plan.sidebar_path, plan.run_phase) {
            (Some(path), true) => Some(load_sidebar(path)?),
            _ => None,
        };
        let (tombstones, binding) = reread(plan)?;
        return Ok(StartupPrelude {
            sidebar,
            report: StartupReport::no_exclusive(),
            leases,
            tombstones,
            binding,
        });
    };
    emit(StartupEvent::ExclusiveTaken);
    let sidebar = match &plan.sidebar_path {
        Some(path) => load_sidebar(path)?,
        None => SidebarState::default(),
    };
    emit(StartupEvent::SidebarLoaded {
        threads: sidebar.threads.len(),
    });
    let report = match &plan.sidebar_path {
        Some(sidebar_path) => {
            let paths = StartupPaths {
                db_path: plan.db_path.clone(),
                ownership_root: plan.ownership_root.clone(),
                sidebar_path: sidebar_path.clone(),
                artifacts_dir: plan.artifacts_dir.clone(),
            };
            let policy = policy_for(&sidebar);
            let now = now_ns();
            let input = startup_input(&paths, sidebar_threads(&sidebar), now)?;
            run_exclusive_phase_with_progress(
                &exclusive,
                &paths,
                &policy,
                &input,
                &StartupBudget::default(),
                now,
                &mut |progress| emit(StartupEvent::Phase(progress.clone())),
            )?
        }
        // Without a sidebar file nothing says what is archived: nothing is deleted.
        None => StartupReport::no_exclusive(),
    };
    let leases = exclusive.downgrade(SHARED_LOCK_WAIT)?;
    emit(StartupEvent::Shared);
    let (tombstones, binding) = reread(plan)?;
    Ok(StartupPrelude {
        sidebar: Some(sidebar),
        report,
        leases,
        tombstones,
        binding,
    })
}

/// A message from the start-up thread to the window.
pub enum StartupMessage {
    Event(StartupEvent),
    Finished(Box<Result<StartupPrelude, StartupError>>),
}

/// Runs [`run_startup`] on a background thread, sending each event (and finally the
/// result) to the returned receiver and calling `wake` after each so the window
/// repaints. A panic becomes [`StartupError::Panicked`].
pub fn spawn_startup(
    plan: StartupPlan,
    policy_for: impl FnOnce(&SidebarState) -> ThreadRetentionPolicy + Send + 'static,
    wake: Arc<dyn Fn() + Send + Sync>,
) -> Receiver<StartupMessage> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("startup-phase".into())
        .spawn({
            let sender = sender.clone();
            let wake = Arc::clone(&wake);
            move || {
                let send = |message: StartupMessage, sender: &Sender<StartupMessage>| {
                    let _ = sender.send(message);
                    wake();
                };
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_startup(&plan, policy_for, &mut |event| {
                        send(StartupMessage::Event(event), &sender);
                    })
                }));
                let result = result.unwrap_or_else(|panic| {
                    let message = panic
                        .downcast_ref::<&str>()
                        .map(|text| (*text).to_owned())
                        .or_else(|| panic.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_owned());
                    Err(StartupError::Panicked(message))
                });
                send(StartupMessage::Finished(Box::new(result)), &sender);
            }
        });
    if let Err(error) = spawned {
        let _ = sender.send(StartupMessage::Finished(Box::new(Err(
            StartupError::Panicked(format!("could not start the start-up thread: {error}")),
        ))));
        wake();
    }
    receiver
}
