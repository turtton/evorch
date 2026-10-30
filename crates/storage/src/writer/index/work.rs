//! The index state machine driven by one owner: settings, admission, commit,
//! logical-budget eviction, physical reclaim, FTS merge steps and the shared
//! step used by both the GUI's background job and the CLI's foreground loop.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use secret_guard::SecretRedactor;

use super::capacity::{
    self, CapacityPolicy, CheckpointOutcome, MEASURED_POLICY, MERGE_GROWTH_PAGES, MERGE_STEP_PAGES,
    Measurement,
};
use super::wait::{self, WAL_WAIT_LIMIT_NS, WalWait};
use super::{lease, requests};
use crate::db::file_sizes;
use crate::repo::thread_index::{LegacyDependencies, SqliteSnapshotStore, SqliteThreadIndex};
#[cfg(test)]
use crate::thread_index::IndexStepResult;
use crate::thread_index::{
    AccountingPolicy, CapacityState, CommitRejection, DEFAULT_ACCOUNTING_POLICY, EXTRACTOR_VERSION,
    ExtractedIndexInput, ExtractionError, IndexAccounting, IndexAdjustmentPhase,
    IndexBlockedReason, IndexCapacityStatus, IndexCommitGuard, IndexCommitResult,
    IndexEvictionReason, IndexFailure, IndexLease, IndexOperation, IndexPauseReason,
    IndexSettingsReceipt, IndexState, IndexTarget, PlannedGrowth, RequestGapReason, RequestGuard,
    SnapshotProvenance, SnapshotRepository as _, ThreadIndexRepository as _, ThreadIndexSettings,
    account_index_input, extract_snapshot_bundle, reaccount_index,
};
use crate::{HardLimits, LimitKind, StorageError};

const DISCOVERY_BATCH: usize = 256;

/// One unit of index work, frozen when it was selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Work {
    pub lease_epoch: u64,
    pub target: IndexTarget,
    pub work_generation: u64,
    pub retention_at_ns: i64,
    pub request: Option<RequestGuard>,
}

/// Why an extraction produced no committable input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExtractFailure {
    /// Deterministic: the same revision will fail again (R2 blocked).
    Extraction(ExtractionError),
    /// The bundle could not be read now (retry with backoff).
    Transient,
    /// The source moved on after selection; the run is pending again.
    Stale,
    /// Reader-derived evidence must first cross the guarded writer boundary.
    Legacy(Box<(SnapshotProvenance, LegacyDependencies)>),
}

pub(crate) type Extracted = (Work, Result<ExtractedIndexInput, ExtractFailure>);

/// Writer-owned index state that outlives single steps.
#[derive(Debug)]
pub(crate) struct IndexRuntime {
    pub settings: ThreadIndexSettings,
    pub settings_generation: u64,
    pub policy: CapacityPolicy,
    pub accounting: AccountingPolicy,
    /// Last refused admission; `None` while admitting.
    pub pause: Option<IndexCapacityStatus>,
    pub wal_wait: Option<WalWait>,
    /// At most one admission checkpoint per step, including a wait probe.
    pub last_checkpoint: Option<CheckpointOutcome>,
    /// Retry the refused operation's actual bound (a deletion can be larger
    /// than a new-index step). This prevents false resume/pause cycles.
    pub paused_growth: Option<PlannedGrowth>,
    /// A result-specific bound is valid only for this revision/generation.
    pub paused_work: Option<Work>,
    /// `P` when a sticky pause (reclaim exhausted / WAL pinned) was recorded.
    pub sticky_total_bytes: u64,
    pub discovery_complete: bool,
    pub merge_pending: bool,
    pub ready: Option<Extracted>,
    pub counters: ThreadIndexCounters,
    #[cfg(test)]
    pub commit_timings_us: Vec<(&'static str, u128)>,
    #[cfg(test)]
    pub admission_clock: Option<std::sync::Arc<std::sync::atomic::AtomicI64>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ThreadIndexCounters {
    pub steps: u64,
    pub commits: u64,
    pub discarded: u64,
    pub failures: u64,
    pub evictions: u64,
    pub merges: u64,
    pub pauses: u64,
    pub wal_waits: u64,
    pub wal_wait_probes: u64,
    pub wal_wait_expiries: u64,
    pub reclaim_cycles: u64,
}

impl IndexRuntime {
    pub(crate) fn new(settings: ThreadIndexSettings) -> Self {
        Self {
            settings,
            settings_generation: 0,
            policy: MEASURED_POLICY,
            accounting: DEFAULT_ACCOUNTING_POLICY,
            pause: None,
            wal_wait: None,
            last_checkpoint: None,
            paused_growth: None,
            paused_work: None,
            sticky_total_bytes: 0,
            discovery_complete: false,
            merge_pending: true,
            ready: None,
            counters: ThreadIndexCounters::default(),
            #[cfg(test)]
            commit_timings_us: Vec::new(),
            #[cfg(test)]
            admission_clock: None,
        }
    }

    /// The admission clock is sampled only after acquiring IMMEDIATE, so lock
    /// waits and checkpoint time cannot extend a lease. Tests can advance this
    /// clock without changing the persisted owner, epoch or expiration.
    fn admission_now_ns(&self, step_now_ns: i64) -> i64 {
        #[cfg(test)]
        {
            self.admission_clock.as_ref().map_or(step_now_ns, |clock| {
                clock.load(std::sync::atomic::Ordering::SeqCst)
            })
        }
        #[cfg(not(test))]
        {
            let _ = step_now_ns;
            super::now_ns()
        }
    }

    pub(crate) fn sticky(&self) -> bool {
        matches!(
            self.pause.map(|status| status.state),
            Some(CapacityState::IndexPaused(
                IndexPauseReason::ReclaimExhausted | IndexPauseReason::WalReaderPinned
            ))
        )
    }
}

/// The writer connection plus the limits every decision is measured against.
#[derive(Clone, Copy)]
pub(crate) struct Env<'a> {
    pub conn: &'a Connection,
    pub db_path: &'a Path,
    pub limits: &'a HardLimits,
    pub source_suspended: bool,
}

impl Env<'_> {
    fn measure(&self) -> Result<Measurement, StorageError> {
        Ok(Measurement {
            sizes: file_sizes(self.db_path)?,
            source_suspended: self.source_suspended,
        })
    }
}

fn immediate<T>(
    conn: &Connection,
    body: impl FnOnce(&Connection) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let transaction = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let value = body(&transaction)?;
    transaction.commit()?;
    Ok(value)
}

/// Retry only a WAL-only admission refusal, once, before publishing a pause or
/// terminal request gap. Every attempt reopens IMMEDIATE and reruns the whole
/// guarded decision (lease, revision/generation, growth and measured capacity).
/// The refused transaction has performed no index writes. TRUNCATE never waits
/// for readers and is always outside that transaction, even on its busy path.
fn admitted_transaction<T>(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    operation: IndexOperation,
    step_now_ns: i64,
    mut body: impl FnMut(
        &Connection,
        &mut IndexRuntime,
        i64,
    ) -> Result<Result<T, IndexCapacityStatus>, StorageError>,
) -> Result<(Result<T, IndexCapacityStatus>, i64), StorageError> {
    for attempt in 0..2 {
        #[cfg(test)]
        let started = std::time::Instant::now();
        let transaction =
            rusqlite::Transaction::new_unchecked(env.conn, TransactionBehavior::Immediate)?;
        #[cfg(test)]
        let begin_us = started.elapsed().as_micros();
        #[cfg(test)]
        let body_started = std::time::Instant::now();
        let now_ns = rt.admission_now_ns(step_now_ns);
        let result = body(&transaction, rt, now_ns)?;
        #[cfg(test)]
        let body_us = body_started.elapsed().as_micros();
        #[cfg(test)]
        let finish_started = std::time::Instant::now();
        transaction.commit()?;
        #[cfg(test)]
        if operation == IndexOperation::Commit
            && std::env::var_os("EVORCH_INDEX_GATE_WORK").is_some()
        {
            rt.commit_timings_us.extend([
                ("begin", begin_us),
                ("body", body_us),
                ("finish", finish_started.elapsed().as_micros()),
            ]);
        }
        if attempt == 0
            && rt.last_checkpoint.is_none()
            && matches!(&result, Err(status)
            if status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit))
        {
            let before = env.measure()?.sizes;
            let outcome = capacity::checkpoint_truncate(env.conn)?;
            rt.last_checkpoint = Some(outcome);
            let after = env.measure()?.sizes;
            tracing::debug!(
                checkpointed = outcome.complete(),
                before_db_bytes = before.db,
                before_wal_bytes = before.wal,
                before_shm_bytes = before.shm,
                after_db_bytes = after.db,
                after_wal_bytes = after.wal,
                after_shm_bytes = after.shm,
                operation = ?operation,
                "thread index WAL admission checkpoint"
            );
            continue;
        }
        if result.is_ok() && rt.pause.is_none() {
            rt.paused_growth = None;
            rt.paused_work = None;
        }
        return Ok((result, now_ns));
    }
    unreachable!("at most one checkpoint retry")
}

/// Moves FTS merges out of commit transactions (ADR 0041 順序 5 gate): the
/// default automerge rewrote segments inside commits without a usable bound.
pub(crate) fn configure_fts(conn: &Connection) -> Result<(), StorageError> {
    for (key, value) in [("automerge", 0_i64), ("crisismerge", 64)] {
        let current: Option<i64> = conn
            .query_row(
                "SELECT v FROM conversation_fts_config WHERE k = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?;
        if current != Some(value) {
            conn.execute(
                "INSERT INTO conversation_fts(conversation_fts, rank) VALUES(?1, ?2)",
                params![key, value],
            )?;
        }
    }
    Ok(())
}

fn applied_versions(conn: &Connection) -> Result<(u32, u32), StorageError> {
    Ok(conn.query_row(
        "SELECT extractor_version, accounting_version FROM thread_index_state WHERE id = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn charged_total(conn: &Connection) -> Result<u64, StorageError> {
    SqliteThreadIndex::new(conn).charged_bytes_total()
}

/// Writer-ordered settings ingestion: the new limit is effective from here.
/// The receipt acknowledges the settings generation, NOT completed maintenance.
/// Per-run versions and charges are the durable adjustment cursor: `prepare`
/// reconciles them before allowing any index growth, including after restart.
/// No blocks, FTS entries or charges are changed by this command.
pub(crate) fn apply_settings(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    settings: ThreadIndexSettings,
    now_ns: i64,
) -> Result<IndexSettingsReceipt, StorageError> {
    immediate(env.conn, |conn| {
        if !settings.enabled {
            // Disabled owners do not get scheduled. Finish control work here,
            // atomically with clearing the durable episode, not in prepare.
            requests::gap_pending(conn, RequestGapReason::IndexDisabled)?;
            requests::finalize(conn, now_ns)?;
            wait::clear_since(conn)?;
        }
        if applied_versions(conn)? == (EXTRACTOR_VERSION, rt.accounting.version) {
            return Ok(());
        }
        let page_size = capacity::page_size(conn)?;
        let growth = rt.policy.planned_growth(
            0,
            page_size,
            capacity::freelist_bytes(conn, page_size)?,
            false,
        );
        // Publish the versions used by NEW source headers/requests without
        // touching any existing run. If refused, the first admitted adjustment
        // step publishes them instead. Per-run mismatches remain resumable.
        if capacity::evaluate(
            &rt.policy,
            env.limits,
            settings.enabled,
            env.measure()?,
            growth,
            false,
        )
        .state
            == CapacityState::Normal
        {
            conn.execute(
                "UPDATE thread_index_state SET extractor_version = ?1, accounting_version = ?2 WHERE id = 1",
                params![EXTRACTOR_VERSION, rt.accounting.version],
            )?;
        }
        Ok(())
    })?;
    rt.settings = settings;
    rt.settings_generation += 1;
    rt.pause = None;
    rt.paused_growth = None;
    rt.paused_work = None;
    if !settings.enabled {
        rt.wal_wait = None;
        rt.ready = None;
    }
    if let Some(wait) = rt.wal_wait.as_mut() {
        // Settings affect admission immediately, but never extend the episode.
        wait.advance_to(i64::MIN);
    }
    Ok(IndexSettingsReceipt {
        applied_generation: rt.settings_generation,
        settings,
    })
}

/// A request accepted by another writer is also an explicit new WAL episode.
/// Only WalLimit pauses are released; ordinary capacity pauses retain +J.
pub(crate) fn resume_pending_wal_request(
    conn: &Connection,
    rt: &mut IndexRuntime,
) -> Result<bool, StorageError> {
    if rt.settings.enabled
        && rt.pause.is_some_and(|status| {
            status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit)
        })
        && requests::latest_pending_created_at(conn)?.is_some()
    {
        rt.pause = None;
        rt.paused_growth = None;
        rt.paused_work = None;
        return Ok(true);
    }
    Ok(false)
}

fn adjustment_pending(conn: &Connection, rt: &IndexRuntime) -> Result<bool, StorageError> {
    Ok(adjustment_phase(conn, rt)?.is_some())
}

fn adjustment_phase(
    conn: &Connection,
    rt: &IndexRuntime,
) -> Result<Option<IndexAdjustmentPhase>, StorageError> {
    let versions = applied_versions(conn)?;
    if versions != (EXTRACTOR_VERSION, rt.accounting.version)
        || conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM thread_index_runs WHERE state <> 'deleted' AND \
             (extractor_version <> ?1 OR accounting_version <> ?2 OR \
              (charged_bytes > 0 AND indexed_accounting_version IS NOT ?2)))",
            params![EXTRACTOR_VERSION, rt.accounting.version],
            |row| row.get::<_, bool>(0),
        )?
    {
        return Ok(Some(IndexAdjustmentPhase::Reaccounting));
    }
    Ok((charged_total(conn)? > rt.settings.max_bytes).then_some(IndexAdjustmentPhase::Evicting))
}

/// Existing blocks remain on pending/retry/blocked rows. Use their stored
/// totals, not the new extraction or a newly reduced logical charge. The
/// measured physical coefficients remain fixed even when accounting changes.
fn stored_accounting(conn: &Connection, run_id: &str) -> Result<IndexAccounting, StorageError> {
    let (text, attributes, blocks): (u64, u64, u64) = conn.query_row(
        "SELECT text_bytes, attribute_bytes, block_count FROM thread_index_runs WHERE run_id = ?1",
        [run_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    reaccount_index(text, attributes, blocks, DEFAULT_ACCOUNTING_POLICY)
        .map_err(|error| StorageError::Serialization(format!("{error:?}")))
}

fn deletion_growth(
    conn: &Connection,
    rt: &IndexRuntime,
    run_id: &str,
) -> Result<PlannedGrowth, StorageError> {
    let stored = stored_accounting(conn, run_id)?;
    if !rt.policy.within_step(&stored) {
        // Atomic run eviction cannot safely split FTS/block/state changes.
        // Keep the old index and report growth_unbounded, never delete anyway.
        return Ok(PlannedGrowth::Unbounded);
    }
    let page_size = capacity::page_size(conn)?;
    let freelist = capacity::freelist_bytes(conn, page_size)?;
    Ok(rt
        .policy
        .planned_growth(stored.charged_bytes, page_size, freelist, true))
}

fn add_growth(left: PlannedGrowth, right: PlannedGrowth) -> PlannedGrowth {
    match (left, right) {
        (
            PlannedGrowth::Bounded {
                db: a,
                wal: b,
                shm: c,
            },
            PlannedGrowth::Bounded {
                db: x,
                wal: y,
                shm: z,
            },
        ) => match (a.checked_add(x), b.checked_add(y), c.checked_add(z)) {
            (Some(db), Some(wal), Some(shm)) => PlannedGrowth::Bounded { db, wal, shm },
            _ => PlannedGrowth::Unbounded,
        },
        _ => PlannedGrowth::Unbounded,
    }
}

/// At most ONE run per step, inside a write transaction. No extraction,
/// backfill, merge or ready-result commit may overtake this reconciliation.
/// Reaccount every run first, then evict using ONLY the settled new total.
/// Each charge update and each later eviction commits atomically per run.
fn adjustment_step(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    now_ns: i64,
) -> Result<Option<Action>, StorageError> {
    if !adjustment_pending(env.conn, rt)? {
        return Ok(None);
    }
    let (outcome, now_ns) = admitted_transaction(
        env,
        rt,
        IndexOperation::Adjustment,
        now_ns,
        |conn, rt, now_ns| {
            if !lease::is_valid(conn, lease, now_ns)? {
                return Ok(Ok(None));
            }
            let env = Env { conn, ..env };
            let stale: Option<(String, u32, u64, u64, u64, u64)> = conn.query_row(
            "SELECT run_id, extractor_version, text_bytes, attribute_bytes, block_count, charged_bytes \
             FROM thread_index_runs WHERE state <> 'deleted' AND \
             (extractor_version <> ?1 OR accounting_version <> ?2 OR \
              (charged_bytes > 0 AND indexed_accounting_version IS NOT ?2)) \
             ORDER BY retention_at_ns ASC, run_id ASC LIMIT 1",
            params![EXTRACTOR_VERSION, rt.accounting.version],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        ).optional()?;
            let total = charged_total(conn)?;
            let new_charge = stale
                .as_ref()
                .map(|(_, _, text, attributes, blocks, old)| {
                    if *old == 0 && *blocks == 0 {
                        Ok(0)
                    } else {
                        reaccount_index(*text, *attributes, *blocks, rt.accounting)
                            .map(|a| a.charged_bytes)
                    }
                })
                .transpose()
                .map_err(|error| StorageError::Serialization(format!("{error:?}")))?;
            if stale.is_none()
                && applied_versions(conn)? == (EXTRACTOR_VERSION, rt.accounting.version)
                && total > rt.settings.max_bytes
                && let Some((run_id, _)) = SqliteThreadIndex::new(conn)
                    .eviction_candidates(1)?
                    .into_iter()
                    .next()
            {
                let growth = deletion_growth(conn, rt, &run_id)?;
                let status = admit(env, rt, growth, IndexOperation::Adjustment)?;
                if status.state != CapacityState::Normal {
                    return Ok(Err(status));
                }
                SqliteThreadIndex::new(conn)
                    .evict_index(&run_id, IndexEvictionReason::LogicalBudget)?;
                conn.execute(
                    "UPDATE thread_index_runs SET accounting_version = ?2 WHERE run_id = ?1",
                    params![run_id, rt.accounting.version],
                )?;
                rt.counters.evictions += 1;
                rt.merge_pending = true;
                requests::sync(conn)?;
                requests::finalize(conn, now_ns)?;
                return Ok(Ok(Some(())));
            }
            let page_size = capacity::page_size(conn)?;
            let growth = rt.policy.planned_growth(
                0,
                page_size,
                capacity::freelist_bytes(conn, page_size)?,
                false,
            );
            let status = admit(env, rt, growth, IndexOperation::Adjustment)?;
            if status.state != CapacityState::Normal {
                return Ok(Err(status));
            }
            if let Some((run_id, extractor, _, _, _, _)) = stale {
                if extractor != EXTRACTOR_VERSION {
                    conn.execute(
                    "UPDATE thread_index_runs SET state = 'pending', attempt_count = 0, \
                     next_attempt_at_ns = NULL, blocked_reason = NULL, last_error_code = NULL, \
                     work_generation = work_generation + 1 \
                     WHERE run_id = ?1 AND state IN ('pending', 'indexed', 'retry_wait', 'blocked')",
                    [&run_id],
                )?;
                }
                conn.execute(
                    "UPDATE thread_index_runs SET extractor_version = ?2, accounting_version = ?3, \
                 charged_bytes = ?4, indexed_accounting_version = \
                 CASE WHEN indexed_revision IS NOT NULL THEN ?3 ELSE NULL END WHERE run_id = ?1",
                    params![
                        run_id,
                        EXTRACTOR_VERSION,
                        rt.accounting.version,
                        new_charge.unwrap_or(0)
                    ],
                )?;
                requests::sync(conn)?;
                requests::finalize(conn, now_ns)?;
            } else {
                conn.execute(
                "UPDATE thread_index_state SET extractor_version = ?1, accounting_version = ?2 WHERE id = 1",
                params![EXTRACTOR_VERSION, rt.accounting.version],
            )?;
            }
            Ok(Ok(Some(())))
        },
    )?;
    match outcome {
        Ok(None) => Ok(Some(Action::LeaseLost)),
        Ok(Some(())) => {
            tracing::debug!(
                generation = rt.settings_generation,
                "thread index settings adjustment step"
            );
            Ok(Some(Action::Progress))
        }
        Err(status) => refuse(env, rt, status, now_ns).map(|refusal| Some(refusal.action())),
    }
}

fn growth_for_step_max(rt: &IndexRuntime, page_size: u64, freelist: u64) -> PlannedGrowth {
    let worst = crate::thread_index::charge_index_bytes(
        rt.policy.step_max_text_bytes,
        0,
        rt.policy.step_max_blocks,
        rt.accounting,
    )
    .unwrap_or(u64::MAX);
    rt.policy.planned_growth(worst, page_size, freelist, false)
}

/// `capacity_status`: the shared decision for a planned growth.
pub(crate) fn capacity_status(
    env: Env<'_>,
    rt: &IndexRuntime,
    growth: PlannedGrowth,
) -> Result<IndexCapacityStatus, StorageError> {
    let mut status = physical_capacity_status(env, rt, growth)?;
    if status.state == CapacityState::Normal && adjustment_pending(env.conn, rt)? {
        status.state = CapacityState::IndexPaused(IndexPauseReason::SettingsAdjustment);
    }
    Ok(status)
}

fn physical_capacity_status(
    env: Env<'_>,
    rt: &IndexRuntime,
    growth: PlannedGrowth,
) -> Result<IndexCapacityStatus, StorageError> {
    let measured = env.measure()?;
    let adjustment = adjustment_phase(env.conn, rt)?;
    if rt.sticky() {
        let mut status = rt.pause.expect("sticky pause recorded");
        status.db_bytes = measured.sizes.db;
        status.wal_bytes = measured.sizes.wal;
        status.shm_bytes = measured.sizes.shm;
        status.adjustment = adjustment;
        status.max_db_bytes = env.limits.max_db_bytes;
        status.max_wal_bytes = env.limits.max_wal_bytes;
        return Ok(status);
    }
    let mut status = capacity::evaluate(
        &rt.policy,
        env.limits,
        rt.settings.enabled,
        measured,
        growth,
        rt.pause.is_some(),
    );
    status.adjustment = adjustment;
    Ok(status)
}

fn admit(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    growth: PlannedGrowth,
    operation: IndexOperation,
) -> Result<IndexCapacityStatus, StorageError> {
    let mut status = physical_capacity_status(env, rt, growth)?;
    status.operation = Some(operation);
    if status.state != CapacityState::Normal {
        rt.paused_growth = Some(growth);
    } else {
        // `env.conn` is the very same IMMEDIATE that admitted the write.
        wait::clear_since(env.conn)?;
        rt.wal_wait = None;
    }
    Ok(status)
}

enum Refusal {
    Paused(IndexCapacityStatus),
    Waiting { next_wake_at_ns: i64 },
}

impl Refusal {
    fn action(self) -> Action {
        match self {
            Self::Paused(status) => Action::Paused(status),
            Self::Waiting { next_wake_at_ns } => Action::Idle {
                next_wake_at_ns: Some(next_wake_at_ns),
            },
        }
    }
}

fn wait_deadline(conn: &Connection, since_ns: i64) -> Result<i64, StorageError> {
    Ok(requests::latest_pending_created_at(conn)?
        .unwrap_or(since_ns)
        .max(since_ns)
        .saturating_add(WAL_WAIT_LIMIT_NS))
}

fn merge_growth(page_size: u64) -> PlannedGrowth {
    PlannedGrowth::Bounded {
        db: MERGE_GROWTH_PAGES * page_size,
        wal: MERGE_GROWTH_PAGES * (page_size + 24) + 32,
        shm: 32 * 1024 * (MERGE_GROWTH_PAGES / 4096 + 1),
    }
}

fn refuse(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    status: IndexCapacityStatus,
    now_ns: i64,
) -> Result<Refusal, StorageError> {
    if status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit)
        && rt
            .last_checkpoint
            .is_some_and(|outcome| !outcome.complete())
    {
        if rt.wal_wait.is_none() {
            let since_ns = immediate(env.conn, |conn| wait::set_since(conn, now_ns))?;
            let deadline = wait_deadline(env.conn, since_ns)?;
            rt.wal_wait = Some(WalWait::begin(
                since_ns,
                status,
                now_ns,
                Some(deadline),
                rt.last_checkpoint,
            ));
            rt.counters.wal_waits += 1;
            let growth_wal = match status.growth {
                PlannedGrowth::Bounded { wal, .. } => wal,
                PlannedGrowth::Unbounded => unreachable!("WAL-only refusal is bounded"),
            };
            tracing::info!(
                checkpoint = ?rt.last_checkpoint,
                wal_bytes = status.wal_bytes,
                growth_wal_bytes = growth_wal,
                max_wal_bytes = status.max_wal_bytes,
                operation = ?status.operation,
                deadline_ns = deadline,
                "thread index WAL wait started"
            );
        }
        return Ok(Refusal::Waiting {
            next_wake_at_ns: rt.wal_wait.expect("wait started").next_retry_at_ns,
        });
    }
    let reason = if status.state == CapacityState::IndexPaused(IndexPauseReason::Disabled) {
        RequestGapReason::IndexDisabled
    } else {
        RequestGapReason::CapacityPaused
    };
    pause_with(env, rt, status, now_ns, reason).map(Refusal::Paused)
}

fn pause_with(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    status: IndexCapacityStatus,
    now_ns: i64,
    reason: RequestGapReason,
) -> Result<IndexCapacityStatus, StorageError> {
    let refused_operation = status.operation;
    let status = match status.state {
        CapacityState::IndexPaused(IndexPauseReason::ReserveInsufficient)
        | CapacityState::SourceSuspended(LimitKind::DbSize)
            if rt.settings.enabled && env.limits.max_db_bytes > rt.policy.reserve_bytes() =>
        {
            reclaim(env, rt)?
        }
        _ => status,
    };
    let status = IndexCapacityStatus {
        operation: refused_operation,
        ..status
    };
    if status.state == CapacityState::Normal {
        immediate(env.conn, wait::clear_since)?;
        rt.wal_wait = None;
        rt.pause = None;
        rt.paused_growth = None;
        rt.paused_work = None;
        return Ok(status);
    }
    if rt.pause.is_none() {
        rt.counters.pauses += 1;
        tracing::warn!(
            state = ?status.state,
            db_bytes = status.db_bytes,
            wal_bytes = status.wal_bytes,
            reserve_bytes = status.reserve_bytes,
            "thread index paused"
        );
    }
    rt.pause = Some(status);
    immediate(env.conn, |conn| {
        wait::clear_since(conn)?;
        requests::gap_pending(conn, reason)?;
        requests::finalize(conn, now_ns)
    })?;
    rt.wal_wait = None;
    Ok(status)
}

/// A parked WAL wait does no write work before its scheduled probe. The start
/// is durable; all other fields are reconstructed on owner handoff/restart.
fn wal_wait_step(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    now_ns: i64,
) -> Result<Option<Action>, StorageError> {
    if rt.wal_wait.is_none()
        && let Some(since_ns) = wait::get_since(env.conn)?
    {
        let page_size = capacity::page_size(env.conn)?;
        // The old owner's frozen result is gone. Use the operation this owner
        // will attempt next, so expiry retains its actual +J resume boundary.
        let phase = adjustment_phase(env.conn, rt)?;
        let (growth, operation) = match phase {
            Some(IndexAdjustmentPhase::Evicting) => {
                let candidate = SqliteThreadIndex::new(env.conn).eviction_candidates(1)?;
                let growth = if let Some((run_id, _)) = candidate.first() {
                    deletion_growth(env.conn, rt, run_id)?
                } else {
                    PlannedGrowth::Unbounded
                };
                (growth, IndexOperation::Adjustment)
            }
            Some(IndexAdjustmentPhase::Reaccounting) => (
                rt.policy.planned_growth(
                    0,
                    page_size,
                    capacity::freelist_bytes(env.conn, page_size)?,
                    false,
                ),
                IndexOperation::Adjustment,
            ),
            None => (merge_growth(page_size), IndexOperation::Merge),
        };
        let mut refused = physical_capacity_status(env, rt, growth)?;
        refused.state = CapacityState::IndexPaused(IndexPauseReason::WalLimit);
        refused.operation = Some(operation);
        rt.paused_growth = Some(growth);
        let mut inherited = WalWait::begin(since_ns, refused, now_ns, None, None);
        inherited.advance_to(now_ns);
        rt.wal_wait = Some(inherited);
    }
    let Some(waiting) = rt.wal_wait else {
        return Ok(None);
    };
    if !waiting.probe_due(now_ns) {
        return Ok(Some(Action::Idle {
            next_wake_at_ns: Some(waiting.next_retry_at_ns),
        }));
    }
    let outcome = capacity::checkpoint_truncate(env.conn)?;
    rt.last_checkpoint = Some(outcome);
    rt.counters.wal_wait_probes += 1;
    tracing::debug!(
        checkpoint = ?outcome,
        probes = waiting.probes.saturating_add(1),
        "thread index WAL wait probe"
    );
    if !lease::is_valid(env.conn, lease, rt.admission_now_ns(now_ns))? {
        rt.ready = None;
        return Ok(Some(Action::LeaseLost));
    }
    if outcome.complete() {
        rt.wal_wait.as_mut().expect("waiting").probes = waiting.probes.saturating_add(1);
        tracing::info!(
            waited_ms = now_ns.saturating_sub(waiting.since_ns) / 1_000_000,
            probes = waiting.probes.saturating_add(1),
            "thread index WAL wait resumed"
        );
        return Ok(None);
    }
    // A failed probe permits waiting only while WAL is still the sole refusal.
    // In particular a restart must not restore an old WAL reason over current
    // source suspension or total/reserve pressure.
    let mut status =
        physical_capacity_status(env, rt, rt.paused_growth.unwrap_or(waiting.refused.growth))?;
    status.operation = waiting.refused.operation;
    if !matches!(
        status.state,
        CapacityState::Normal | CapacityState::IndexPaused(IndexPauseReason::WalLimit)
    ) {
        return refuse(env, rt, status, now_ns).map(|refusal| Some(refusal.action()));
    }
    // The deadline, current pending set, individual gaps and episode end share
    // one IMMEDIATE. A concurrent submission either participates in this
    // decision (and keeps the job alive) or starts after the completed episode.
    let (deadline, expired, ended) = immediate(env.conn, |conn| {
        let deadline = wait_deadline(conn, waiting.since_ns)?;
        let expired =
            if requests::earliest_wait_deadline(conn, waiting.since_ns, WAL_WAIT_LIMIT_NS)?
                .is_some_and(|deadline| now_ns >= deadline)
            {
                let expired = requests::gap_expired(
                    conn,
                    waiting.since_ns,
                    WAL_WAIT_LIMIT_NS,
                    now_ns,
                    RequestGapReason::CapacityWaitExpired,
                )?;
                requests::finalize(conn, now_ns)?;
                expired
            } else {
                0
            };
        let ended = now_ns >= deadline;
        if ended {
            wait::clear_since(conn)?;
        }
        Ok((deadline, expired, ended))
    })?;
    rt.counters.wal_wait_expiries += expired as u64;
    if expired > 0 {
        tracing::warn!(expired, "thread index WAL request wait expired");
    }
    if ended {
        tracing::warn!(
            since_ns = waiting.since_ns,
            deadline_ns = deadline,
            "thread index WAL job wait expired"
        );
        // All expired targets were settled in the transaction above. Never
        // gap_pending here: a different writer may already have submitted.
        status.state = CapacityState::IndexPaused(IndexPauseReason::WalLimit);
        rt.pause = Some(status);
        rt.counters.pauses += 1;
        rt.wal_wait = None;
        return Ok(Some(Action::Paused(status)));
    }
    let waiting = rt.wal_wait.as_mut().expect("waiting");
    if status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit) {
        waiting.refused = status;
    }
    waiting.probed(outcome, now_ns, Some(deadline));
    Ok(Some(Action::Idle {
        next_wake_at_ns: Some(waiting.next_retry_at_ns),
    }))
}

/// Physical reclaim for DbSize pressure only: at most two cycles of budgeted
/// index deletion, TRUNCATE checkpoint and (if available) incremental vacuum,
/// each re-measured. Deletion growth is itself admitted without consuming the
/// foreground reserve. A reader pinning the WAL or two failed cycles become
/// sticky reasons that are re-judged only after a measured size decrease or a
/// settings change, never on a short timer. Full VACUUM never runs.
fn reclaim(env: Env<'_>, rt: &mut IndexRuntime) -> Result<IndexCapacityStatus, StorageError> {
    let conn = env.conn;
    let page_size = capacity::page_size(conn)?;
    let before = env.measure()?;
    let resume_growth = rt
        .paused_growth
        .unwrap_or_else(|| growth_for_step_max(rt, page_size, 0));
    let mut deleted = false;
    let mut vacuumed = false;
    for cycle in 0..2 {
        rt.counters.reclaim_cycles += 1;
        let measured = env.measure()?;
        let PlannedGrowth::Bounded { db, wal, shm } = resume_growth else {
            break;
        };
        let needed = u128::from(measured.sizes.total())
            + u128::from(db + wal + shm)
            + u128::from(rt.policy.reserve_bytes())
            + u128::from(rt.policy.hysteresis_bytes);
        let deficit = needed.saturating_sub(u128::from(env.limits.max_db_bytes));
        if deficit > 0
            && !deleted
            && adjustment_phase(conn, rt)? != Some(IndexAdjustmentPhase::Reaccounting)
        {
            deleted = immediate(conn, |conn| {
                let mut index = SqliteThreadIndex::new(conn);
                let Some((run_id, _)) = index.eviction_candidates(1)?.into_iter().next() else {
                    return Ok(false);
                };
                let growth = deletion_growth(conn, rt, &run_id)?;
                let status = physical_capacity_status(Env { conn, ..env }, rt, growth)?;
                if status.state != CapacityState::Normal {
                    return Ok(false);
                }
                index.evict_index(&run_id, IndexEvictionReason::PhysicalReclaim)?;
                rt.counters.evictions += 1;
                rt.merge_pending = true;
                Ok(true)
            })?;
        }
        if !capacity::checkpoint_truncate(conn)?.complete() {
            let status = sticky(env, rt, IndexPauseReason::WalReaderPinned)?;
            log_reclaim(cycle, before, env, &status)?;
            return Ok(status);
        }
        if !deleted && !vacuumed && capacity::incremental_vacuum_enabled(conn)? {
            let free_pages = capacity::freelist_bytes(conn, page_size)? / page_size;
            let room = env
                .limits
                .max_wal_bytes
                .saturating_sub(env.measure()?.sizes.wal)
                .saturating_sub(rt.policy.hysteresis_bytes)
                / (page_size + 24);
            let pages = free_pages.min(room).min(MERGE_STEP_PAGES as u64);
            if pages > 0 {
                immediate(conn, |conn| {
                    let growth = PlannedGrowth::Bounded {
                        db: pages * page_size,
                        wal: pages * (page_size + 24) + 32,
                        shm: 32 * 1024,
                    };
                    if physical_capacity_status(Env { conn, ..env }, rt, growth)?.state
                        == CapacityState::Normal
                    {
                        conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages})"))?;
                        vacuumed = true;
                    }
                    Ok(())
                })?;
                if !capacity::checkpoint_truncate(conn)?.complete() {
                    let status = sticky(env, rt, IndexPauseReason::WalReaderPinned)?;
                    log_reclaim(cycle, before, env, &status)?;
                    return Ok(status);
                }
            }
        }
        // Judge the reclaim by sizes alone; a suspended source resumes on the
        // writer's own size check and is reported as such, not as exhausted.
        let measured = env.measure()?;
        let sized = capacity::evaluate(
            &rt.policy,
            env.limits,
            rt.settings.enabled,
            Measurement {
                source_suspended: false,
                ..measured
            },
            resume_growth,
            true,
        );
        if sized.state == CapacityState::Normal {
            let status = capacity::evaluate(
                &rt.policy,
                env.limits,
                rt.settings.enabled,
                measured,
                resume_growth,
                true,
            );
            log_reclaim(cycle, before, env, &status)?;
            return Ok(status);
        }
    }
    let status = sticky(env, rt, IndexPauseReason::ReclaimExhausted)?;
    log_reclaim(1, before, env, &status)?;
    Ok(status)
}

fn sticky(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    reason: IndexPauseReason,
) -> Result<IndexCapacityStatus, StorageError> {
    let measured = env.measure()?;
    rt.sticky_total_bytes = measured.sizes.total();
    Ok(IndexCapacityStatus {
        state: CapacityState::IndexPaused(reason),
        adjustment: adjustment_phase(env.conn, rt)?,
        operation: rt.pause.and_then(|status| status.operation),
        growth: rt.paused_growth.unwrap_or(PlannedGrowth::Unbounded),
        max_db_bytes: env.limits.max_db_bytes,
        max_wal_bytes: env.limits.max_wal_bytes,
        db_bytes: measured.sizes.db,
        wal_bytes: measured.sizes.wal,
        shm_bytes: measured.sizes.shm,
        reserve_bytes: rt.policy.reserve_bytes(),
        resume_hysteresis_bytes: rt.policy.hysteresis_bytes,
    })
}

fn log_reclaim(
    cycle: u32,
    before: Measurement,
    env: Env<'_>,
    status: &IndexCapacityStatus,
) -> Result<(), StorageError> {
    let after = env.measure()?;
    tracing::warn!(
        cycles = cycle + 1,
        before_total_bytes = before.sizes.total(),
        after_total_bytes = after.sizes.total(),
        reserve_bytes = status.reserve_bytes,
        max_db_bytes = env.limits.max_db_bytes,
        state = ?status.state,
        "thread index physical reclaim"
    );
    Ok(())
}

/// Re-judges a pause after a capacity-change notification (maintenance
/// checkpoint, cleanup, settings). Sticky reasons need a measured decrease.
pub(crate) fn recheck_pause(env: Env<'_>, rt: &mut IndexRuntime) -> Result<bool, StorageError> {
    if discard_stale_pause(env, rt)? {
        return Ok(true);
    }
    let Some(status) = rt.pause else {
        return Ok(true);
    };
    let measured = env.measure()?;
    if rt.sticky() {
        if measured.sizes.total() >= rt.sticky_total_bytes {
            return Ok(false);
        }
        rt.pause = Some(IndexCapacityStatus {
            state: CapacityState::IndexPaused(IndexPauseReason::ReserveInsufficient),
            ..status
        });
    }
    if status.state == CapacityState::IndexPaused(IndexPauseReason::WalLimit) {
        // A fully checkpointed WAL keeps its high-water size until truncated.
        rt.last_checkpoint = Some(capacity::checkpoint_truncate(env.conn)?);
    }
    let page_size = capacity::page_size(env.conn)?;
    let freelist = capacity::freelist_bytes(env.conn, page_size)?;
    let growth = rt
        .paused_growth
        .unwrap_or_else(|| growth_for_step_max(rt, page_size, freelist));
    let mut status = capacity::evaluate(
        &rt.policy,
        env.limits,
        rt.settings.enabled,
        env.measure()?,
        growth,
        true,
    );
    status.adjustment = adjustment_phase(env.conn, rt)?;
    status.operation = rt.pause.and_then(|status| status.operation);
    if status.state == CapacityState::Normal {
        tracing::info!(db_bytes = status.db_bytes, "thread index resumed");
        rt.pause = None;
        rt.paused_growth = None;
        rt.paused_work = None;
        return Ok(true);
    }
    rt.pause = Some(status);
    Ok(false)
}

/// Invalidates only a result-specific pause. Physical pauses without a frozen
/// work item (e.g. an old index larger than the envelope) keep their stop rule.
/// Called after source commands too, so a parked owner need not await a timer.
pub(crate) fn discard_stale_pause(
    env: Env<'_>,
    rt: &mut IndexRuntime,
) -> Result<bool, StorageError> {
    let Some(work) = rt.paused_work.as_ref() else {
        return Ok(false);
    };
    let live = lease::current(env.conn)?;
    let stale = match live {
        // Expiry itself is checked at prepare/commit. Here only determine
        // whether the frozen result still describes this source and request.
        Some(lease) if lease.epoch == work.lease_epoch => SqliteThreadIndex::new(env.conn)
            .check_guard(&guard(&lease, work), lease.expires_at_ns.saturating_sub(1))?
            .is_some(),
        _ => true,
    };
    if !stale {
        return Ok(false);
    }
    if rt.ready.as_ref().is_some_and(|(ready, _)| ready == work) {
        rt.ready = None;
        rt.counters.discarded += 1;
    }
    rt.paused_work = None;
    rt.paused_growth = None;
    rt.pause = None;
    Ok(true)
}

fn guard(lease: &IndexLease, work: &Work) -> IndexCommitGuard {
    IndexCommitGuard {
        lease: lease.clone(),
        target: work.target.clone(),
        work_generation: work.work_generation,
        expected_state: IndexState::Pending,
        source_present: true,
        request: work.request.clone(),
    }
}

/// Request targets first (so a foreground CLI finishes its own request),
/// then ordinary work newest first.
pub(crate) fn next_work(conn: &Connection) -> Result<Option<Work>, StorageError> {
    let requested = conn
        .query_row(
            "SELECT r.run_id, r.source_revision, r.extractor_version, r.accounting_version, \
             r.work_generation, r.retention_at_ns, t.request_id \
             FROM thread_index_request_targets t \
             JOIN thread_index_requests q ON q.request_id = t.request_id AND q.state = 'pending' \
             JOIN thread_index_runs r ON r.run_id = t.run_id AND r.state = 'pending' \
             AND r.source_revision = t.source_revision \
             AND r.extractor_version = t.extractor_version \
             AND r.accounting_version = t.accounting_version \
             AND r.work_generation = t.work_generation \
             WHERE t.state = 'pending' \
             ORDER BY r.retention_at_ns DESC, r.run_id DESC LIMIT 1",
            [],
            work_row,
        )
        .optional()?;
    if requested.is_some() {
        return Ok(requested);
    }
    Ok(conn
        .query_row(
            "SELECT run_id, source_revision, extractor_version, accounting_version, \
             work_generation, retention_at_ns, NULL FROM thread_index_runs \
             WHERE state = 'pending' \
             ORDER BY priority DESC, retention_at_ns DESC, run_id DESC LIMIT 1",
            [],
            work_row,
        )
        .optional()?)
}

fn work_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Work> {
    let target = IndexTarget {
        run_id: row.get(0)?,
        source_revision: row.get(1)?,
        extractor_version: row.get(2)?,
        accounting_version: row.get(3)?,
    };
    let work_generation = row.get(4)?;
    let request = row
        .get::<_, Option<String>>(6)?
        .map(|request_id| RequestGuard {
            request_id,
            target: target.clone(),
            work_generation,
        });
    Ok(Work {
        lease_epoch: 0,
        target,
        work_generation,
        retention_at_ns: row.get(5)?,
        request,
    })
}

/// Reads one bundle in a single read transaction and extracts it. Runs on the
/// single extractor thread for both GUI and CLI owners.
pub(crate) fn extract(
    conn: &Connection,
    work: &Work,
    redactor: &SecretRedactor,
) -> Result<ExtractedIndexInput, ExtractFailure> {
    let (bundle, missing_sidecar, dependencies) = (|| {
        let transaction = conn
            .unchecked_transaction()
            .map_err(|_| ExtractFailure::Transient)?;
        let missing_sidecar: bool = transaction
            .query_row(
                "SELECT NOT EXISTS(SELECT 1 FROM run_context_provenance WHERE run_id = ?1)",
                [&work.target.run_id],
                |row| row.get(0),
            )
            .map_err(|_| ExtractFailure::Transient)?;
        let bundle = SqliteSnapshotStore::new(&transaction)
            .load_snapshot_bundle(&work.target.run_id)
            .map_err(|_| ExtractFailure::Transient)?;
        let dependencies = if missing_sidecar {
            LegacyDependencies::capture(&transaction, &work.target.run_id)
                .map_err(|_| ExtractFailure::Transient)?
        } else {
            None
        };
        drop(transaction);
        Ok((
            bundle.ok_or(ExtractFailure::Transient)?,
            missing_sidecar,
            dependencies,
        ))
    })()?;
    if bundle.source_revision != work.target.source_revision {
        return Err(ExtractFailure::Stale);
    }
    if missing_sidecar && dependencies.is_none() {
        return Err(ExtractFailure::Extraction(
            ExtractionError::ProvenanceUnknown(
                crate::thread_index::ProvenanceUnknownReason::LegacyUnproven,
            ),
        ));
    }
    if let Some(dependencies) = dependencies
        && bundle.snapshot_revision == 0
        && let Some(sidecar) = bundle.provenance
    {
        return Err(ExtractFailure::Legacy(Box::new((sidecar, dependencies))));
    }
    let input = extract_snapshot_bundle(&bundle, redactor).map_err(ExtractFailure::Extraction)?;
    if input.extractor_version != work.target.extractor_version
        || input.source_revision != work.target.source_revision
        || input.run_id != work.target.run_id
    {
        return Err(ExtractFailure::Stale);
    }
    Ok(input)
}

/// What `prepare` decided the owner should do next.
#[derive(Debug)]
pub(crate) enum Action {
    LeaseLost,
    Paused(IndexCapacityStatus),
    Idle { next_wake_at_ns: Option<i64> },
    Progress,
    Extract(Work),
}

/// Bookkeeping plus at most one bounded write. Extraction is returned to the
/// caller so the background job can run it outside the writer.
pub(crate) fn prepare(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    now_ns: i64,
) -> Result<Action, StorageError> {
    rt.last_checkpoint = None;
    if !lease::is_valid(env.conn, lease, now_ns)? {
        rt.ready = None;
        return Ok(Action::LeaseLost);
    }
    rt.counters.steps += 1;
    discard_stale_pause(env, rt)?;
    if !rt.settings.enabled {
        let status = capacity_status(
            env,
            rt,
            PlannedGrowth::Bounded {
                db: 0,
                wal: 0,
                shm: 0,
            },
        )?;
        return refuse(env, rt, status, now_ns).map(Refusal::action);
    }
    if rt.pause.is_some() && (rt.sticky() || !recheck_pause(env, rt)?) {
        let status = rt.pause.expect("still paused");
        immediate(env.conn, |conn| {
            requests::gap_pending(conn, RequestGapReason::CapacityPaused)?;
            requests::finalize(conn, now_ns)
        })?;
        return Ok(Action::Paused(status));
    }
    if let Some(action) = wal_wait_step(env, rt, lease, now_ns)? {
        return Ok(action);
    }
    if let Some(action) = adjustment_step(env, rt, lease, now_ns)? {
        return Ok(action);
    }
    if let Some(extracted) = rt.ready.take() {
        return handle_extracted(env, rt, lease, extracted, now_ns);
    }
    immediate(env.conn, |conn| {
        // Promotion bumps the generation; `sync` rebinds request targets to it.
        SqliteThreadIndex::new(conn).promote_due_retries(now_ns)?;
        requests::sync(conn)?;
        requests::finalize(conn, now_ns)?;
        Ok(())
    })?;
    if rt.merge_pending {
        return merge_step(env, rt, lease, now_ns);
    }
    if !rt.discovery_complete {
        let batch = SqliteThreadIndex::new(env.conn).discover_batch(DISCOVERY_BATCH, now_ns)?;
        rt.discovery_complete = batch.complete;
        return Ok(Action::Progress);
    }
    match next_work(env.conn)? {
        Some(mut work) => {
            work.lease_epoch = lease.epoch;
            Ok(Action::Extract(work))
        }
        None => Ok(Action::Idle {
            next_wake_at_ns: SqliteThreadIndex::new(env.conn).next_retry_at_ns()?,
        }),
    }
}

fn merge_step(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    now_ns: i64,
) -> Result<Action, StorageError> {
    let (more, now_ns) = admitted_transaction(
        env,
        rt,
        IndexOperation::Merge,
        now_ns,
        |conn, rt, now_ns| {
            if !lease::is_valid(conn, lease, now_ns)? {
                return Ok(Ok(None));
            }
            let page_size = capacity::page_size(conn)?;
            let growth = merge_growth(page_size);
            let status = admit(Env { conn, ..env }, rt, growth, IndexOperation::Merge)?;
            if status.state != CapacityState::Normal {
                return Ok(Err(status));
            }
            let changes = |conn: &Connection| -> Result<i64, StorageError> {
                Ok(conn.query_row("SELECT total_changes()", [], |row| row.get(0))?)
            };
            let before = changes(conn)?;
            conn.execute(
                "INSERT INTO conversation_fts(conversation_fts, rank) VALUES('merge', ?1)",
                [MERGE_STEP_PAGES],
            )?;
            Ok(Ok(Some(changes(conn)? - before >= 2)))
        },
    )?;
    match more {
        Ok(None) => Ok(Action::LeaseLost),
        Ok(Some(more)) => {
            rt.counters.merges += 1;
            rt.merge_pending = more;
            Ok(Action::Progress)
        }
        Err(status) => refuse(env, rt, status, now_ns).map(Refusal::action),
    }
}

fn evict_self(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: &Work,
    reason: IndexEvictionReason,
    code: Option<&str>,
    now_ns: i64,
) -> Result<Budget, StorageError> {
    let (result, now_ns) = admitted_transaction(
        env,
        rt,
        IndexOperation::Eviction,
        now_ns,
        |conn, rt, now_ns| {
            if let Some(rejection) =
                SqliteThreadIndex::new(conn).check_guard(&guard(lease, work), now_ns)?
            {
                rt.counters.discarded += 1;
                return Ok(Ok(if rejection == CommitRejection::LeaseLost {
                    Budget::LeaseLost
                } else {
                    Budget::EvictedSelf
                }));
            }
            let growth = deletion_growth(conn, rt, &work.target.run_id)?;
            let status = admit(Env { conn, ..env }, rt, growth, IndexOperation::Eviction)?;
            if status.state != CapacityState::Normal {
                rt.paused_work = Some(work.clone());
                return Ok(Err(status));
            }
            SqliteThreadIndex::new(conn).evict_index(&work.target.run_id, reason)?;
            if let Some(code) = code {
                conn.execute(
                    "UPDATE thread_index_runs SET last_error_code = ?2 WHERE run_id = ?1",
                    params![work.target.run_id, code],
                )?;
            }
            rt.counters.evictions += 1;
            rt.merge_pending = true;
            requests::sync(conn)?;
            requests::finalize(conn, now_ns)?;
            Ok(Ok(Budget::EvictedSelf))
        },
    )?;
    Ok(result.unwrap_or_else(|status| Budget::Paused(status, now_ns)))
}

fn oversized(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: Work,
    input: ExtractedIndexInput,
    code: &str,
    now_ns: i64,
) -> Result<Action, StorageError> {
    match evict_self(
        env,
        rt,
        lease,
        &work,
        IndexEvictionReason::Oversized,
        Some(code),
        now_ns,
    )? {
        Budget::LeaseLost => Ok(Action::LeaseLost),
        Budget::Paused(status, now_ns) => {
            rt.ready = Some((work, Ok(input)));
            refuse(env, rt, status, now_ns).map(Refusal::action)
        }
        _ => Ok(Action::Progress),
    }
}

fn record_failure(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: &Work,
    failure: IndexFailure,
    now_ns: i64,
) -> Result<(), StorageError> {
    immediate(env.conn, |conn| {
        let result = SqliteThreadIndex::new(conn).record_index_failure(
            &guard(lease, work),
            failure,
            now_ns,
        )?;
        match result {
            IndexCommitResult::Committed => rt.counters.failures += 1,
            IndexCommitResult::Discarded(_) => rt.counters.discarded += 1,
        }
        requests::sync(conn)?;
        requests::finalize(conn, now_ns)?;
        Ok(())
    })
}

fn handle_extracted(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    (work, extracted): Extracted,
    now_ns: i64,
) -> Result<Action, StorageError> {
    if work.lease_epoch != lease.epoch {
        rt.counters.discarded += 1;
        return Ok(Action::Progress);
    }
    if SqliteThreadIndex::new(env.conn)
        .check_guard(&guard(lease, &work), now_ns)?
        .is_some()
    {
        rt.counters.discarded += 1;
        return Ok(Action::Progress);
    }
    let input = match extracted {
        Ok(input) => input,
        Err(ExtractFailure::Legacy(sidecar)) => {
            let (sidecar, dependencies) = *sidecar;
            immediate(env.conn, |conn| {
                let result = SqliteThreadIndex::new(conn).commit_legacy_provenance(
                    &guard(lease, &work),
                    &sidecar,
                    &dependencies,
                    now_ns,
                )?;
                if matches!(result, IndexCommitResult::Discarded(_)) {
                    rt.counters.discarded += 1;
                }
                requests::sync(conn)?;
                requests::finalize(conn, now_ns)
            })?;
            return Ok(Action::Progress);
        }
        Err(ExtractFailure::Stale) => {
            rt.counters.discarded += 1;
            return Ok(Action::Progress);
        }
        Err(ExtractFailure::Transient) => {
            record_failure(env, rt, lease, &work, IndexFailure::Transient, now_ns)?;
            return Ok(Action::Progress);
        }
        Err(ExtractFailure::Extraction(error)) => {
            let failure = IndexFailure::Blocked(IndexBlockedReason::Extraction(error));
            record_failure(env, rt, lease, &work, failure, now_ns)?;
            return Ok(Action::Progress);
        }
    };
    let accounting = match account_index_input(&input, rt.accounting) {
        Ok(accounting) if accounting.accounting_version == work.target.accounting_version => {
            accounting
        }
        Ok(_) => {
            rt.counters.discarded += 1;
            return Ok(Action::Progress);
        }
        Err(_) => {
            return oversized(
                env,
                rt,
                lease,
                work,
                input,
                requests::OVERSIZED_STEP_CODE,
                now_ns,
            );
        }
    };
    if !rt.policy.within_step(&accounting) {
        return oversized(
            env,
            rt,
            lease,
            work,
            input,
            requests::OVERSIZED_STEP_CODE,
            now_ns,
        );
    }
    if accounting.charged_bytes > rt.settings.max_bytes {
        return oversized(
            env,
            rt,
            lease,
            work,
            input,
            requests::OVERSIZED_BUDGET_CODE,
            now_ns,
        );
    }
    match budget_step(env, rt, lease, &work, &accounting, now_ns)? {
        Budget::LeaseLost => Ok(Action::LeaseLost),
        Budget::Fits => commit(env, rt, lease, work, input, accounting, now_ns),
        Budget::EvictedOlder => {
            rt.ready = Some((work, Ok(input)));
            Ok(Action::Progress)
        }
        Budget::EvictedSelf => Ok(Action::Progress),
        Budget::Paused(status, now_ns) => {
            rt.ready = Some((work, Ok(input)));
            refuse(env, rt, status, now_ns).map(Refusal::action)
        }
    }
}

enum Budget {
    LeaseLost,
    Fits,
    EvictedOlder,
    EvictedSelf,
    Paused(IndexCapacityStatus, i64),
}

/// Logical budget (B-4): evict ONE run strictly older than this one per step,
/// or this run itself when nothing older holds bytes. Newer indexes are never
/// evicted to backfill an older run.
fn budget_step(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: &Work,
    accounting: &IndexAccounting,
    now_ns: i64,
) -> Result<Budget, StorageError> {
    let (result, now_ns) = admitted_transaction(
        env,
        rt,
        IndexOperation::Eviction,
        now_ns,
        |conn, rt, now_ns| {
            if let Some(rejection) =
                SqliteThreadIndex::new(conn).check_guard(&guard(lease, work), now_ns)?
            {
                return Ok(Ok(Some(if rejection == CommitRejection::LeaseLost {
                    Budget::LeaseLost
                } else {
                    Budget::Fits
                })));
            }
            let own: u64 = conn
                .query_row(
                    "SELECT charged_bytes FROM thread_index_runs WHERE run_id = ?1",
                    [&work.target.run_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let needed = charged_total(conn)?.saturating_sub(own) + accounting.charged_bytes;
            if needed <= rt.settings.max_bytes {
                return Ok(Ok(Some(Budget::Fits)));
            }
            let older: Option<(String, u64)> = conn
                .query_row(
                    "SELECT run_id, charged_bytes FROM thread_index_runs WHERE charged_bytes > 0 \
             AND run_id <> ?1 AND (retention_at_ns < ?2 OR (retention_at_ns = ?2 AND run_id < ?1)) \
             ORDER BY retention_at_ns ASC, run_id ASC LIMIT 1",
                    params![work.target.run_id, work.retention_at_ns],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((run_id, _)) = older else {
                return Ok(Ok(None));
            };
            let growth = deletion_growth(conn, rt, &run_id)?;
            let status = admit(Env { conn, ..env }, rt, growth, IndexOperation::Eviction)?;
            if status.state != CapacityState::Normal {
                rt.paused_work = Some(work.clone());
                return Ok(Err(status));
            }
            SqliteThreadIndex::new(conn)
                .evict_index(&run_id, IndexEvictionReason::LogicalBudget)?;
            rt.counters.evictions += 1;
            rt.merge_pending = true;
            Ok(Ok(Some(Budget::EvictedOlder)))
        },
    )?;
    match result {
        Ok(None) => evict_self(
            env,
            rt,
            lease,
            work,
            IndexEvictionReason::LogicalBudget,
            None,
            now_ns,
        ),
        Ok(Some(budget)) => Ok(budget),
        Err(status) => Ok(Budget::Paused(status, now_ns)),
    }
}

fn commit(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: Work,
    input: ExtractedIndexInput,
    accounting: IndexAccounting,
    now_ns: i64,
) -> Result<Action, StorageError> {
    let (outcome, now_ns) = admitted_transaction(
        env,
        rt,
        IndexOperation::Commit,
        now_ns,
        |conn, rt, now_ns| {
            if let Some(rejection) =
                SqliteThreadIndex::new(conn).check_guard(&guard(lease, &work), now_ns)?
            {
                return Ok(Ok(IndexCommitResult::Discarded(rejection)));
            }
            let page_size = capacity::page_size(conn)?;
            let freelist = capacity::freelist_bytes(conn, page_size)?;
            let measured_charge = reaccount_index(
                accounting.text_bytes,
                accounting.attribute_bytes,
                accounting.block_count,
                DEFAULT_ACCOUNTING_POLICY,
            )
            .map_err(|error| StorageError::Serialization(format!("{error:?}")))?;
            let growth =
                rt.policy
                    .planned_growth(measured_charge.charged_bytes, page_size, freelist, false);
            let old = stored_accounting(conn, &work.target.run_id)?;
            let growth = if old.text_bytes.saturating_add(accounting.text_bytes)
                > rt.policy.step_max_text_bytes
                || old.block_count.saturating_add(accounting.block_count)
                    > rt.policy.step_max_blocks
            {
                PlannedGrowth::Unbounded
            } else if old.block_count > 0 {
                add_growth(growth, deletion_growth(conn, rt, &work.target.run_id)?)
            } else {
                growth
            };
            let status = admit(Env { conn, ..env }, rt, growth, IndexOperation::Commit)?;
            if status.state != CapacityState::Normal {
                rt.paused_work = Some(work.clone());
                return Ok(Err(status));
            }
            let result = SqliteThreadIndex::new(conn).commit_index_input(
                &guard(lease, &work),
                &input,
                accounting,
                now_ns,
            )?;
            if result == IndexCommitResult::Committed {
                requests::sync(conn)?;
                requests::finalize(conn, now_ns)?;
            }
            Ok(Ok(result))
        },
    )?;
    match outcome {
        Ok(IndexCommitResult::Committed) => {
            rt.counters.commits += 1;
            rt.merge_pending = true;
            Ok(Action::Progress)
        }
        Ok(IndexCommitResult::Discarded(rejection)) => {
            rt.counters.discarded += 1;
            if rejection == CommitRejection::LeaseLost {
                return Ok(Action::LeaseLost);
            }
            Ok(Action::Progress)
        }
        Err(status) => {
            rt.ready = Some((work, Ok(input)));
            refuse(env, rt, status, now_ns).map(Refusal::action)
        }
    }
}

/// Deterministic fixture driver. Production steps dispatch the single extractor.
#[cfg(test)]
pub(crate) fn foreground_step(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    now_ns: i64,
    redactor: &SecretRedactor,
) -> Result<IndexStepResult, StorageError> {
    match prepare(env, rt, lease, now_ns)? {
        Action::LeaseLost => Ok(IndexStepResult::LeaseLost),
        Action::Paused(status) => Ok(IndexStepResult::Paused(status)),
        Action::Idle { next_wake_at_ns } => Ok(IndexStepResult::Idle { next_wake_at_ns }),
        Action::Progress => Ok(IndexStepResult::Progress),
        Action::Extract(work) => {
            let extracted = extract(env.conn, &work, redactor);
            rt.ready = Some((work, extracted));
            Ok(IndexStepResult::Progress)
        }
    }
}

/// Discovery to completion (request submission needs every source header).
pub(crate) fn discover_all(
    conn: &Connection,
    rt: &mut IndexRuntime,
    now_ns: i64,
) -> Result<(), StorageError> {
    let mut index = SqliteThreadIndex::new(conn);
    while !index.discover_batch(DISCOVERY_BATCH, now_ns)?.complete {}
    rt.discovery_complete = true;
    Ok(())
}
