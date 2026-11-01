//! The exclusive startup phase.
//!
//! The caller holds [`storage::ExclusiveLeases`]: no other evorch process has the
//! database open or shares the ownership root. This is the only place a deletion
//! is confirmed, and the only place state outside the database (owner rows, scratch
//! directories) is removed.
//!
//! Order matters. Stages 0-3 prepare (backfill, closure search) and end with one
//! confirming commit per family; nothing outside the database changes before it.
//! Stages 4-7 reclaim what is confirmed; a failure or crash there never undoes a
//! deletion, and the next start repeats the stage.
//!
//! | stage | work |
//! |---|---|
//! | 0 | identity, binding check |
//! | 1 | event backfill (bounded) |
//! | 2 | closure search (bounded) |
//! | 3 | owner classification, root binding, finalize per family |
//! | 4 | owner rows of deleted threads, scratch directories |
//! | 5 | oversized rows |
//! | 6 | incremental vacuum |
//! | 7 | artifact blob collection (mark and sweep) |
//!
//! Stages 1, 4, 5 and 6 do not depend on the retention period: with
//! `archived_delete_after_days = 0` nothing new is confirmed but what earlier starts
//! confirmed (and every oversized row registered by any change) is still reclaimed.
//! Stages 5 and 6 touch only the database, so they also run when the binding does
//! not match; owner rows and scratch directories never do. Stage 7 needs the binding
//! too (it deletes files), and the artifacts directory; it does not depend on the
//! retention period either, since earlier starts' deletions leave the blobs behind.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use storage::thread_retention::{
    BindingInput, BlockReason, ConfirmedFamily, ExploreBudget, FamilyState, FinalizeBudget,
    FinalizeOutcome, OversizedProgress, RetentionInput, SidebarThread, ThreadRetentionPolicy,
    TombstoneSet,
};
use storage::{BackfillProgress, Database, ExclusiveLeases, StorageConfig};

use super::binding::{BindingPaths, BindingStatus, canonical_string};
use super::{RetentionError, check_binding, owner_blocks, write_root_binding};
use crate::artifacts::{GcBudget, GcReport};
use crate::ownership::Registry;
use crate::scratch::ThreadScratch;

/// Where the phase finds its inputs.
#[derive(Debug, Clone)]
pub struct StartupPaths {
    pub db_path: PathBuf,
    pub ownership_root: PathBuf,
    pub sidebar_path: PathBuf,
    /// The artifact store (`meta/`, `blobs/`). Meta files of deleted threads are removed
    /// by the reclaim executor; the blobs by stage 7.
    pub artifacts_dir: Option<PathBuf>,
}

/// What one startup may do, in quantities (never in elapsed time). Whatever does
/// not fit continues at the next start.
#[derive(Debug, Clone)]
pub struct StartupBudget {
    /// Events converted per transaction.
    pub backfill_rows: usize,
    /// Events converted in total.
    pub backfill_max_rows: u64,
    /// New closure edges explored in total.
    pub explore_edges: u64,
    /// Finalize attempts per family: the first budget, then doubled after each `OverBudget`.
    pub finalize_rounds: u32,
    /// Stored bytes of oversized rows deleted.
    pub oversized_bytes: u64,
    /// Free pages returned to the file.
    pub vacuum_pages: u64,
    /// Scratch directories worked on.
    pub scratch_dirs: usize,
    /// Scratch entries (files, links, directories) deleted in total.
    pub scratch_entries: usize,
    /// Artifact meta files opened by stage 7, and the bytes read from them.
    pub gc_meta_files: usize,
    pub gc_meta_bytes: u64,
    /// Artifact meta file names checked against the reference table per start. More meta
    /// files are marked over several starts (no sweep until a pass is complete).
    pub gc_meta_lookups: usize,
    /// Unreadable-meta records read again.
    pub gc_unknown_retries: usize,
    /// Reference rows checked for a missing meta file.
    pub gc_ref_checks: usize,
    /// Blob directory entries examined and blob files unlinked.
    pub gc_blob_entries: usize,
    pub gc_unlinks: usize,
}

impl StartupBudget {
    fn gc(&self) -> GcBudget {
        GcBudget {
            meta_files: self.gc_meta_files,
            meta_bytes: self.gc_meta_bytes,
            meta_lookups: self.gc_meta_lookups,
            unknown_retries: self.gc_unknown_retries,
            ref_checks: self.gc_ref_checks,
            blob_entries: self.gc_blob_entries,
            unlinks: self.gc_unlinks,
        }
    }
}

impl Default for StartupBudget {
    fn default() -> Self {
        Self {
            backfill_rows: storage::thread_retention::DEFAULT_RETENTION_BACKFILL_ROWS,
            backfill_max_rows: 20_000,
            explore_edges: 50_000,
            finalize_rounds: 4,
            oversized_bytes: 64 * 1024 * 1024,
            vacuum_pages: 50_000,
            scratch_dirs: 50,
            scratch_entries: 20_000,
            gc_meta_files: 20_000,
            gc_meta_bytes: 32 * 1024 * 1024,
            gc_meta_lookups: 100_000,
            gc_unknown_retries: 1_000,
            gc_ref_checks: 20_000,
            gc_blob_entries: 50_000,
            gc_unlinks: 5_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Another evorch process holds one of the locks.
    NoExclusive,
    /// The database and ownership root do not belong together (see [`BindingStatus`]).
    BindingMismatch,
}

/// What happened to the scratch directories of deleted threads. A directory counts
/// as `removed` only once it is gone from disk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchReport {
    /// Directories deleted completely.
    pub removed: usize,
    /// Entries deleted, across complete and incomplete directories.
    pub entries_removed: usize,
    /// Threads whose directory still exists because the entry budget ran out
    /// (the next start continues).
    pub incomplete: Vec<String>,
    /// Existing directories not touched because a budget was spent.
    pub remaining: usize,
    /// Directories that could not be deleted, with the error. They stay and are retried.
    pub failed: Vec<(String, String)>,
}

/// The database-only maintenance of a start (no ownership root involved).
#[derive(Debug, Clone, Default)]
pub struct DbMaintenance {
    pub oversized: OversizedProgress,
    pub vacuum_pages: u64,
    /// The database is not `auto_vacuum = INCREMENTAL`, so no space was returned.
    pub vacuum_skipped: bool,
    pub errors: Vec<String>,
}

/// One report per start, built once: the size difference between the variants costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StartupOutcome {
    Skipped {
        reason: SkipReason,
        /// What still ran: after a binding mismatch the database-only maintenance;
        /// `None` when the exclusive locks were not held.
        maintenance: Option<DbMaintenance>,
    },
    Ran {
        backfill: BackfillProgress,
        families: Vec<(String, FamilyState)>,
        confirmed: Vec<ConfirmedFamily>,
        /// Families that cannot be confirmed now, and why.
        blocked: Vec<(String, BlockReason)>,
        /// Families whose fixed-point check still exceeded the budget after every round.
        retried_over_budget: Vec<String>,
        /// Families that grew since the search; their new members were staged.
        stale: Vec<String>,
        oversized: OversizedProgress,
        vacuum_pages: u64,
        /// The database is not `auto_vacuum = INCREMENTAL`, so no space was returned.
        vacuum_skipped: bool,
        scratch: ScratchReport,
        owners_removed: usize,
        /// Why the owner registry could not be read; nothing was confirmed then.
        owner_registry_error: Option<String>,
        /// The artifact blob collection; `None` when it did not run (no artifacts
        /// directory, or it could not be examined: see `errors`).
        gc: Option<GcReport>,
        /// Failures after which a stage stopped short (the next start repeats it). A
        /// failure after a confirmation is reported here, never as `Err`.
        errors: Vec<String>,
    },
}

#[derive(Debug)]
pub struct StartupReport {
    pub outcome: StartupOutcome,
    /// `None` when the binding was not examined (the phase was skipped before it).
    pub binding: Option<BindingStatus>,
    pub tombstones: TombstoneSet,
    pub elapsed: Duration,
}

impl StartupReport {
    /// The report of a start that could not take the exclusive locks.
    #[must_use]
    pub fn no_exclusive() -> Self {
        Self {
            outcome: StartupOutcome::Skipped {
                reason: SkipReason::NoExclusive,
                maintenance: None,
            },
            binding: None,
            tombstones: TombstoneSet::default(),
            elapsed: Duration::ZERO,
        }
    }
}

/// A phase boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Backfill,
    Explore,
    Finalize,
    Owners,
    Scratch,
    Oversized,
    Vacuum,
    ArtifactGc,
}

impl Stage {
    const fn label(self) -> &'static str {
        match self {
            Self::Backfill => "backfill",
            Self::Explore => "explore",
            Self::Finalize => "finalize",
            Self::Owners => "owner rows",
            Self::Scratch => "scratch",
            Self::Oversized => "oversized rows",
            Self::Vacuum => "vacuum",
            Self::ArtifactGc => "artifact gc",
        }
    }
}

/// What the phase tells an observer (a splash or a log) while it runs. Events are
/// delivered on the calling thread, in order, between units of work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupProgress {
    StageStarted(Stage),
    /// The root side of the binding was written (before the first confirmation commits).
    RootBound,
    /// One family's deletion was just committed.
    FamilyConfirmed {
        family_key: String,
    },
    StageFinished(Stage),
}

/// The input the phase needs, with the canonical paths it will compare against the
/// binding. Build it after taking the exclusive locks.
///
/// # Errors
/// Fails when a path cannot be resolved (see [`canonical_string`]).
pub fn startup_input(
    paths: &StartupPaths,
    threads: Vec<SidebarThread>,
    now_ns: i64,
) -> Result<RetentionInput, RetentionError> {
    Ok(RetentionInput {
        threads,
        sidebar_path: canonical_string(&paths.sidebar_path)?,
        ownership_root: canonical_string(&paths.ownership_root)?,
        now_ns,
    })
}

fn open_database(db_path: &Path) -> Result<Database, RetentionError> {
    Ok(Database::open(&StorageConfig {
        db_path: db_path.to_path_buf(),
        ..StorageConfig::default()
    })?)
}

/// Run the phase. `_leases` proves the exclusivity this function relies on.
///
/// # Errors
/// Fails on a database error before anything was confirmed, on an input whose
/// paths differ from `paths`, or when the root binding cannot be written (nothing
/// is confirmed then). Failures after a confirmation are reported in
/// [`StartupOutcome::Ran::errors`], never as `Err`.
pub fn run_exclusive_phase(
    leases: &ExclusiveLeases,
    paths: &StartupPaths,
    policy: &ThreadRetentionPolicy,
    input: &RetentionInput,
    budget: &StartupBudget,
    now_ns: i64,
) -> Result<StartupReport, RetentionError> {
    run_exclusive_phase_with_progress(leases, paths, policy, input, budget, now_ns, &mut |_| {})
}

/// [`run_exclusive_phase`] that reports each stage and each confirmed family to
/// `progress` as it happens, for a host that shows a long start. Each event is also
/// logged at `info` under the `thread_retention` target.
///
/// # Errors
/// As [`run_exclusive_phase`].
pub fn run_exclusive_phase_with_progress(
    _leases: &ExclusiveLeases,
    paths: &StartupPaths,
    policy: &ThreadRetentionPolicy,
    input: &RetentionInput,
    budget: &StartupBudget,
    now_ns: i64,
    progress: &mut dyn FnMut(&StartupProgress),
) -> Result<StartupReport, RetentionError> {
    let started = Instant::now();
    let mut notify = |event: StartupProgress| {
        match &event {
            StartupProgress::StageStarted(stage) => {
                tracing::info!(target: "thread_retention", stage = stage.label(), "started");
            }
            StartupProgress::RootBound => {
                tracing::info!(target: "thread_retention", "ownership root bound");
            }
            StartupProgress::FamilyConfirmed { family_key } => {
                tracing::info!(target: "thread_retention", family = %family_key, "family deleted");
            }
            StartupProgress::StageFinished(stage) => {
                tracing::info!(target: "thread_retention", stage = stage.label(), "finished");
            }
        }
        progress(&event);
    };
    if input.ownership_root != canonical_string(&paths.ownership_root)?
        || input.sidebar_path != canonical_string(&paths.sidebar_path)?
    {
        return Err(RetentionError::Input(
            "input paths differ from the startup paths; build it with startup_input".into(),
        ));
    }
    if input.now_ns < 0 {
        return Err(RetentionError::Input("now_ns is negative".into()));
    }
    let database = open_database(&paths.db_path)?;
    // Stage 0.
    let binding_paths = BindingPaths {
        ownership_root: &paths.ownership_root,
        sidebar_path: &paths.sidebar_path,
    };
    let status = check_binding(&database, binding_paths)?;
    if matches!(status, BindingStatus::Mismatch(_)) {
        // The pairing is wrong, so nothing outside the database may change; the
        // database's own maintenance does not depend on it.
        let mut ran = Ran::default();
        stage(&mut notify, Stage::Oversized, || {
            reclaim_oversized(&database, budget, &mut ran);
        });
        stage(&mut notify, Stage::Vacuum, || {
            vacuum(&database, budget, &mut ran);
        });
        return Ok(StartupReport {
            outcome: StartupOutcome::Skipped {
                reason: SkipReason::BindingMismatch,
                maintenance: Some(DbMaintenance {
                    oversized: ran.oversized,
                    vacuum_pages: ran.vacuum_pages,
                    vacuum_skipped: ran.vacuum_skipped,
                    errors: ran.errors,
                }),
            },
            binding: Some(status),
            tombstones: database.tombstones()?,
            elapsed: started.elapsed(),
        });
    }
    let enabled = policy.archived_delete_after_days > 0;

    // Stage 1.
    notify(StartupProgress::StageStarted(Stage::Backfill));
    let mut ran = Ran {
        backfill: database
            .retention_backfill_startup(budget.backfill_rows, budget.backfill_max_rows)?,
        ..Ran::default()
    };
    notify(StartupProgress::StageFinished(Stage::Backfill));
    let backfilled =
        ran.backfill.remaining_unconverted == 0 && ran.backfill.remaining_unattributed == 0;

    if enabled && backfilled {
        // Stage 2.
        notify(StartupProgress::StageStarted(Stage::Explore));
        ran.families = database.retention_explore(
            input,
            policy,
            ExploreBudget {
                max_edges: budget.explore_edges,
            },
        )?;
        notify(StartupProgress::StageFinished(Stage::Explore));
        // Stage 3. Once a family is confirmed, a later failure is reported, not raised.
        notify(StartupProgress::StageStarted(Stage::Finalize));
        let outcome = confirm_families(
            &database,
            paths,
            policy,
            input,
            budget,
            now_ns,
            &status,
            &mut ran,
            &mut notify,
        );
        match outcome {
            Ok(()) => {}
            Err(error) if ran.confirmed.is_empty() => return Err(error),
            Err(error) => ran.errors.push(format!("finalize: {error}")),
        }
        notify(StartupProgress::StageFinished(Stage::Finalize));
    }

    // Stages 4-7: reclaim what is confirmed. A failure here is reported, not raised.
    let tombstones = read_tombstones(&database, &mut ran)?;
    let mut threads = tombstones.threads.iter().cloned().collect::<Vec<_>>();
    threads.sort_unstable();
    stage(&mut notify, Stage::Owners, || {
        remove_owners(paths, &threads, &mut ran);
    });
    stage(&mut notify, Stage::Scratch, || {
        remove_scratch(paths, &threads, budget, &mut ran);
    });
    stage(&mut notify, Stage::Oversized, || {
        reclaim_oversized(&database, budget, &mut ran);
    });
    stage(&mut notify, Stage::Vacuum, || {
        vacuum(&database, budget, &mut ran);
    });
    // Stage 7: only where an artifacts directory is known to exist.
    match artifacts_dir_state(paths) {
        Ok(true) => stage(&mut notify, Stage::ArtifactGc, || {
            collect_blobs(&database, paths, budget, now_ns, &mut ran);
        }),
        Ok(false) => {}
        Err(error) => ran.errors.push(format!("artifact gc: {error}")),
    }

    let tombstones = read_tombstones(&database, &mut ran)?;
    Ok(StartupReport {
        outcome: ran.into_outcome(),
        binding: Some(status),
        tombstones,
        elapsed: started.elapsed(),
    })
}

fn stage(notify: &mut impl FnMut(StartupProgress), stage: Stage, work: impl FnOnce()) {
    notify(StartupProgress::StageStarted(stage));
    work();
    notify(StartupProgress::StageFinished(stage));
}

/// The tombstones as stored. After a confirmation a failed read must not discard the
/// report, so the confirmed families stand in for the stored set.
fn read_tombstones(database: &Database, ran: &mut Ran) -> Result<TombstoneSet, RetentionError> {
    match database.tombstones() {
        Ok(tombstones) => Ok(tombstones),
        Err(error) if ran.confirmed.is_empty() => Err(error.into()),
        Err(error) => {
            ran.errors.push(format!("tombstones: {error}"));
            let mut set = TombstoneSet::default();
            for family in &ran.confirmed {
                set.threads.extend(family.threads.iter().cloned());
                set.runs.extend(family.runs.iter().cloned());
                set.tasks.extend(family.tasks.iter().cloned());
            }
            Ok(set)
        }
    }
}

#[derive(Default)]
struct Ran {
    backfill: BackfillProgress,
    families: Vec<(String, FamilyState)>,
    confirmed: Vec<ConfirmedFamily>,
    blocked: Vec<(String, BlockReason)>,
    retried_over_budget: Vec<String>,
    stale: Vec<String>,
    oversized: OversizedProgress,
    vacuum_pages: u64,
    vacuum_skipped: bool,
    scratch: ScratchReport,
    owners_removed: usize,
    owner_registry_error: Option<String>,
    gc: Option<GcReport>,
    errors: Vec<String>,
}

impl Ran {
    fn into_outcome(self) -> StartupOutcome {
        StartupOutcome::Ran {
            backfill: self.backfill,
            families: self.families,
            confirmed: self.confirmed,
            blocked: self.blocked,
            retried_over_budget: self.retried_over_budget,
            stale: self.stale,
            oversized: self.oversized,
            vacuum_pages: self.vacuum_pages,
            vacuum_skipped: self.vacuum_skipped,
            scratch: self.scratch,
            owners_removed: self.owners_removed,
            owner_registry_error: self.owner_registry_error,
            gc: self.gc,
            errors: self.errors,
        }
    }
}

fn owners_db(ownership_root: &Path) -> PathBuf {
    ownership_root.join("owners.db")
}

/// The owner registry, or `None` when this root has none yet.
///
/// Only an explicit "not found" means there is none. A path that cannot be examined (a
/// loop, a permission error) is an error: who holds the threads is then unknown.
fn open_registry(ownership_root: &Path) -> Result<Option<Registry>, RetentionError> {
    let path = owners_db(ownership_root);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => Ok(Some(Registry::open_existing(&path)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(RetentionError::Io(error)),
    }
}

#[allow(clippy::too_many_arguments)]
fn confirm_families(
    database: &Database,
    paths: &StartupPaths,
    policy: &ThreadRetentionPolicy,
    input: &RetentionInput,
    budget: &StartupBudget,
    now_ns: i64,
    status: &BindingStatus,
    ran: &mut Ran,
    notify: &mut impl FnMut(StartupProgress),
) -> Result<(), RetentionError> {
    let mut explored = Vec::new();
    for (key, state) in &ran.families {
        match state {
            FamilyState::Explored { threads, .. } => explored.push((key.clone(), threads.clone())),
            FamilyState::Blocked(reason) => ran.blocked.push((key.clone(), reason.clone())),
            FamilyState::Exploring { .. } => {}
        }
    }
    if explored.is_empty() {
        return Ok(());
    }
    // Owner rows. Unreadable means nothing is known about who holds what.
    let all_threads = explored
        .iter()
        .flat_map(|(_, threads)| threads.iter().cloned())
        .collect::<Vec<_>>();
    let verdicts = open_registry(&paths.ownership_root)
        .map_err(|error| error.to_string())
        .and_then(|registry| match registry {
            Some(registry) => registry
                .classify_for_retention(&all_threads)
                .map_err(|error| error.to_string()),
            None => Ok(std::collections::BTreeMap::new()),
        });
    let verdicts = match verdicts {
        Ok(verdicts) => verdicts,
        Err(error) => {
            for (key, _) in &explored {
                ran.blocked.push((
                    key.clone(),
                    BlockReason::Owner(format!("owner registry unreadable: {error}")),
                ));
            }
            ran.owner_registry_error = Some(error);
            return Ok(());
        }
    };
    let now_ms = u64::try_from(now_ns / 1_000_000)
        .map_err(|_| RetentionError::Input("now_ns is negative".into()))?;
    let blocks = owner_blocks(&verdicts, now_ms, policy.archived_delete_after_days);
    let binding = BindingInput {
        ownership_root: input.ownership_root.clone(),
        sidebar_path: input.sidebar_path.clone(),
    };
    let mut root_bound = matches!(status, BindingStatus::Bound);
    for (key, threads) in explored {
        let Some(first) = database.retention_finalize_budget(&key)? else {
            continue;
        };
        let family_blocks: std::collections::BTreeMap<String, String> = blocks
            .iter()
            .filter(|(thread, _)| threads.contains(thread))
            .map(|(thread, reason)| (thread.clone(), reason.clone()))
            .collect();
        // An owner holds a member: no confirmation can follow, so do not even bind the root.
        if let Some(reason) = family_blocks.values().next() {
            ran.blocked.push((key, BlockReason::Owner(reason.clone())));
            continue;
        }
        // The root side of the binding is on disk before the first confirmation
        // can commit; a failure here confirms nothing.
        if !root_bound {
            let db_id = database.retention_identity()?;
            write_root_binding(&paths.ownership_root, &db_id, &paths.db_path, now_ns)?;
            root_bound = true;
            notify(StartupProgress::RootBound);
        }
        let settled = finalize_with_doubling(first, budget.finalize_rounds, |attempt| {
            database.retention_finalize(&key, input, policy, &family_blocks, &binding, attempt)
        })?;
        match settled {
            Settled::Confirmed(family) => {
                notify(StartupProgress::FamilyConfirmed {
                    family_key: family.family_key.clone(),
                });
                ran.confirmed.push(family);
            }
            Settled::OverBudget => ran.retried_over_budget.push(key),
            Settled::Stale => ran.stale.push(key),
            Settled::Blocked(reason) => ran.blocked.push((key, reason)),
            Settled::Disabled => {}
        }
    }
    Ok(())
}

/// How one family's finalize attempts ended.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Settled {
    Confirmed(ConfirmedFamily),
    /// Every round exceeded its budget; the family stays unconfirmed until the next start.
    OverBudget,
    Stale,
    Blocked(BlockReason),
    Disabled,
}

/// Try `finalize` with `first`, doubling the budget after each `OverBudget`, at most
/// `rounds` times (at least once). The family is never split: a family that does not
/// fit in the last round waits for the next start.
pub(crate) fn finalize_with_doubling(
    first: FinalizeBudget,
    rounds: u32,
    mut finalize: impl FnMut(FinalizeBudget) -> Result<FinalizeOutcome, storage::StorageError>,
) -> Result<Settled, storage::StorageError> {
    let mut attempt = first;
    let mut tried = 0;
    loop {
        tried += 1;
        match finalize(attempt)? {
            FinalizeOutcome::Confirmed(family) => return Ok(Settled::Confirmed(family)),
            FinalizeOutcome::OverBudget { .. } if tried >= rounds.max(1) => {
                return Ok(Settled::OverBudget);
            }
            FinalizeOutcome::OverBudget { .. } => attempt = attempt.doubled(),
            FinalizeOutcome::Stale { .. } => return Ok(Settled::Stale),
            FinalizeOutcome::Blocked(reason) => return Ok(Settled::Blocked(reason)),
            FinalizeOutcome::Disabled => return Ok(Settled::Disabled),
        }
    }
}

fn remove_owners(paths: &StartupPaths, threads: &[String], ran: &mut Ran) {
    if threads.is_empty() {
        return;
    }
    match open_registry(&paths.ownership_root) {
        Ok(Some(mut registry)) => match registry.remove_threads(threads) {
            Ok(removed) => ran.owners_removed = removed,
            Err(error) => ran.errors.push(format!("owner rows: {error}")),
        },
        Ok(None) => {}
        Err(error) => ran.errors.push(format!("owner rows: {error}")),
    }
}

/// Delete the scratch directories of deleted threads, entry by entry, within the
/// directory and entry budgets. A directory counts as removed only after the call
/// that emptied and unlinked it reported completion; anything else stays listed
/// (incomplete, failed or remaining) and the next start continues from what is left.
fn remove_scratch(paths: &StartupPaths, threads: &[String], budget: &StartupBudget, ran: &mut Ran) {
    if threads.is_empty() {
        return;
    }
    let scratch = match ThreadScratch::for_ownership_root(&paths.ownership_root) {
        Ok(scratch) => scratch,
        Err(error) => {
            ran.errors.push(format!("scratch: {error}"));
            return;
        }
    };
    let mut dirs = 0;
    let mut entries_left = budget.scratch_entries;
    for thread in threads {
        match scratch.thread_dir_exists(thread) {
            Ok(true) => {}
            Ok(false) => continue,
            // Not "absent": nobody could look, so it is reported and retried.
            Err(error) => {
                ran.scratch.failed.push((thread.clone(), error.to_string()));
                continue;
            }
        }
        if dirs >= budget.scratch_dirs || entries_left == 0 {
            ran.scratch.remaining += 1;
            continue;
        }
        dirs += 1;
        // Entries unlinked count against the budget whether or not the call went on to
        // fail: the next directory gets only what is left.
        match scratch.remove_thread(thread, entries_left) {
            Ok(done) => {
                entries_left = entries_left.saturating_sub(done.removed_entries);
                ran.scratch.entries_removed += done.removed_entries;
                if done.complete {
                    ran.scratch.removed += 1;
                } else {
                    ran.scratch.incomplete.push(thread.clone());
                }
            }
            Err(failed) => {
                entries_left = entries_left.saturating_sub(failed.removed_entries);
                ran.scratch.entries_removed += failed.removed_entries;
                ran.scratch
                    .failed
                    .push((thread.clone(), failed.error.to_string()));
            }
        }
    }
}

/// Upper bound on `reclaim_oversized_step` calls: each looks at 256 registrations
/// and `remaining` saturates at 1,000, so this is far above what one start needs.
const MAX_OVERSIZED_CALLS: u32 = 1_024;

fn reclaim_oversized(database: &Database, budget: &StartupBudget, ran: &mut Ran) {
    let mut spent = 0_u64;
    for _ in 0..MAX_OVERSIZED_CALLS {
        let left = budget.oversized_bytes.saturating_sub(spent);
        if left == 0 {
            break;
        }
        let step = match database.reclaim_oversized_step(left) {
            Ok(step) => step,
            Err(error) => {
                ran.errors.push(format!("oversized rows: {error}"));
                break;
            }
        };
        spent = spent.saturating_add(step.stored_bytes);
        ran.oversized.rows += step.rows;
        ran.oversized.stored_bytes += step.stored_bytes;
        ran.oversized.remaining = step.remaining;
        let failed = step.last_error.is_some();
        if failed {
            ran.oversized.last_error = step.last_error;
        }
        // `rows == 0` does not mean done: only `remaining == 0` does.
        if failed || step.remaining == 0 {
            break;
        }
    }
}

fn vacuum(database: &Database, budget: &StartupBudget, ran: &mut Ran) {
    match database.retention_vacuum_step(budget.vacuum_pages) {
        Ok(Some(pages)) => ran.vacuum_pages = pages,
        Ok(None) => ran.vacuum_skipped = true,
        Err(error) => ran.errors.push(format!("incremental vacuum: {error}")),
    }
}

/// Whether stage 7 has a directory to work on. No configured directory, or one that is
/// explicitly not there, means no; a path that cannot be examined or is not a
/// directory is an error (nothing is swept, and it is reported).
fn artifacts_dir_state(paths: &StartupPaths) -> Result<bool, String> {
    let Some(directory) = &paths.artifacts_dir else {
        return Ok(false);
    };
    match std::fs::metadata(directory) {
        Ok(metadata) if metadata.is_dir() => Ok(true),
        Ok(_) => Err(format!("{} is not a directory", directory.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!("{}: {error}", directory.display())),
    }
}

fn collect_blobs(
    database: &Database,
    paths: &StartupPaths,
    budget: &StartupBudget,
    now_ns: i64,
    ran: &mut Ran,
) {
    let Some(directory) = &paths.artifacts_dir else {
        return;
    };
    let report = crate::artifacts::collect_blobs(database, directory, &budget.gc(), now_ns);
    tracing::info!(
        target: "thread_retention",
        refs_complete = report.refs_complete,
        refs_added = report.refs_added,
        unknown_remaining = report.unknown_remaining,
        blobs_unlinked = report.blobs_unlinked,
        bytes_freed = report.bytes_freed,
        "artifact gc"
    );
    for stop in &report.stops {
        if let crate::artifacts::GcStop::Database(error) = stop {
            ran.errors.push(format!("artifact gc: {error}"));
        }
    }
    ran.gc = Some(report);
}
