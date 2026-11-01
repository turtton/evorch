//! The exclusive startup phase.
//!
//! The caller holds [`storage::ExclusiveLeases`]: no other evorch process has the
//! database open or shares the ownership root. This is the only place a deletion is
//! confirmed, and the only place state outside the database (owner rows, scratch
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
use crate::ownership::Registry;
use crate::scratch::ThreadScratch;

/// Where the phase finds its inputs.
#[derive(Debug, Clone)]
pub struct StartupPaths {
    pub db_path: PathBuf,
    pub ownership_root: PathBuf,
    pub sidebar_path: PathBuf,
    /// Reserved for the blob collector; artifact meta is removed by the reclaim executor.
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
    /// Scratch directories removed.
    pub scratch_dirs: usize,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScratchReport {
    pub removed: usize,
    /// Directories left for the next start (budget).
    pub remaining: usize,
    pub failed: Vec<(String, String)>,
}

/// One report per start, built once: the size difference between the variants costs nothing.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum StartupOutcome {
    Skipped {
        reason: SkipReason,
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
        /// Failures that stopped a reclaim stage short (the next start repeats it).
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
            },
            binding: None,
            tombstones: TombstoneSet::default(),
            elapsed: Duration::ZERO,
        }
    }
}

/// A phase boundary, for fault injection in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Backfill,
    Explore,
    Finalize,
    Owners,
    Scratch,
    Oversized,
    Vacuum,
}

/// The input the phase needs, with the canonical paths it will compare against the
/// binding. Build it after taking the exclusive locks.
#[must_use]
pub fn startup_input(
    paths: &StartupPaths,
    threads: Vec<SidebarThread>,
    now_ns: i64,
) -> RetentionInput {
    RetentionInput {
        threads,
        sidebar_path: canonical_string(&paths.sidebar_path),
        ownership_root: canonical_string(&paths.ownership_root),
        now_ns,
    }
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
    run_exclusive_phase_with(leases, paths, policy, input, budget, now_ns, &mut |_| {})
}

pub(crate) fn run_exclusive_phase_with(
    _leases: &ExclusiveLeases,
    paths: &StartupPaths,
    policy: &ThreadRetentionPolicy,
    input: &RetentionInput,
    budget: &StartupBudget,
    now_ns: i64,
    after_stage: &mut dyn FnMut(Stage),
) -> Result<StartupReport, RetentionError> {
    let started = Instant::now();
    if input.ownership_root != canonical_string(&paths.ownership_root)
        || input.sidebar_path != canonical_string(&paths.sidebar_path)
    {
        return Err(RetentionError::Input(
            "input paths differ from the startup paths; build it with startup_input".into(),
        ));
    }
    let database = open_database(&paths.db_path)?;
    // Stage 0.
    let binding_paths = BindingPaths {
        ownership_root: &paths.ownership_root,
        sidebar_path: &paths.sidebar_path,
    };
    let status = check_binding(&database, binding_paths)?;
    if matches!(status, BindingStatus::Mismatch(_)) {
        return Ok(StartupReport {
            outcome: StartupOutcome::Skipped {
                reason: SkipReason::BindingMismatch,
            },
            binding: Some(status),
            tombstones: database.tombstones()?,
            elapsed: started.elapsed(),
        });
    }
    let enabled = policy.archived_delete_after_days > 0;
    // Stage 1.
    let mut ran = Ran {
        backfill: database
            .retention_backfill_startup(budget.backfill_rows, budget.backfill_max_rows)?,
        ..Ran::default()
    };
    after_stage(Stage::Backfill);
    let backfilled =
        ran.backfill.remaining_unconverted == 0 && ran.backfill.remaining_unattributed == 0;

    if enabled && backfilled {
        // Stage 2.
        ran.families = database.retention_explore(
            input,
            policy,
            ExploreBudget {
                max_edges: budget.explore_edges,
            },
        )?;
        after_stage(Stage::Explore);
        // Stage 3.
        confirm_families(
            &database, paths, policy, input, budget, now_ns, &status, &mut ran,
        )?;
        after_stage(Stage::Finalize);
    }

    // Stages 4-7: reclaim what is confirmed. A failure here is reported, not raised.
    let tombstones = database.tombstones()?;
    let mut threads = tombstones.threads.iter().cloned().collect::<Vec<_>>();
    threads.sort_unstable();
    remove_owners(paths, &threads, &mut ran);
    after_stage(Stage::Owners);
    remove_scratch(paths, &threads, budget, &mut ran);
    after_stage(Stage::Scratch);
    reclaim_oversized(&database, budget, &mut ran);
    after_stage(Stage::Oversized);
    match database.retention_vacuum_step(budget.vacuum_pages) {
        Ok(Some(pages)) => ran.vacuum_pages = pages,
        Ok(None) => ran.vacuum_skipped = true,
        Err(error) => ran.errors.push(format!("incremental vacuum: {error}")),
    }
    after_stage(Stage::Vacuum);

    Ok(StartupReport {
        outcome: ran.into_outcome(),
        binding: Some(status),
        tombstones: database.tombstones()?,
        elapsed: started.elapsed(),
    })
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
            errors: self.errors,
        }
    }
}

fn owners_db(ownership_root: &Path) -> PathBuf {
    ownership_root.join("owners.db")
}

/// The owner registry, or `None` when this root has none yet.
fn open_registry(ownership_root: &Path) -> Result<Option<Registry>, RetentionError> {
    let path = owners_db(ownership_root);
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(Registry::open_existing(&path)?))
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
    let now_ms = u64::try_from(now_ns / 1_000_000).unwrap_or(0);
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
        }
        let settled = finalize_with_doubling(first, budget.finalize_rounds, |attempt| {
            database.retention_finalize(&key, input, policy, &family_blocks, &binding, attempt)
        })?;
        match settled {
            Settled::Confirmed(family) => ran.confirmed.push(family),
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
    for thread in threads {
        if !scratch.thread_dir_exists(thread) {
            continue;
        }
        if ran.scratch.removed + ran.scratch.failed.len() >= budget.scratch_dirs {
            ran.scratch.remaining += 1;
            continue;
        }
        match scratch.remove_thread(thread) {
            Ok(_) => ran.scratch.removed += 1,
            Err(error) => ran.scratch.failed.push((thread.clone(), error.to_string())),
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
