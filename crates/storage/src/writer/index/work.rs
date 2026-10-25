//! The index state machine driven by one owner: settings, admission, commit,
//! logical-budget eviction, physical reclaim, FTS merge steps and the shared
//! step used by both the GUI's background job and the CLI's foreground loop.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use secret_guard::SecretRedactor;

use super::capacity::{
    self, CapacityPolicy, MEASURED_POLICY, MERGE_GROWTH_PAGES, MERGE_STEP_PAGES, Measurement,
};
use super::{lease, requests};
use crate::db::file_sizes;
use crate::repo::thread_index::{LegacyDependencies, SqliteSnapshotStore, SqliteThreadIndex};
#[cfg(test)]
use crate::thread_index::IndexStepResult;
use crate::thread_index::{
    AccountingPolicy, CapacityState, CommitRejection, DEFAULT_ACCOUNTING_POLICY, EXTRACTOR_VERSION,
    ExtractedIndexInput, ExtractionError, IndexAccounting, IndexBlockedReason, IndexCapacityStatus,
    IndexCommitGuard, IndexCommitResult, IndexEvictionReason, IndexFailure, IndexLease,
    IndexPauseReason, IndexSettingsReceipt, IndexState, IndexTarget, PlannedGrowth,
    RequestGapReason, RequestGuard, SnapshotProvenance, SnapshotRepository as _,
    ThreadIndexRepository as _, ThreadIndexSettings, account_index_input, extract_snapshot_bundle,
    reaccount_index,
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
    /// `P` when a sticky pause (reclaim exhausted / WAL pinned) was recorded.
    pub sticky_total_bytes: u64,
    pub discovery_complete: bool,
    pub merge_pending: bool,
    pub ready: Option<Extracted>,
    pub counters: ThreadIndexCounters,
    #[cfg(test)]
    pub commit_timings_us: Vec<(&'static str, u128)>,
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
            sticky_total_bytes: 0,
            discovery_complete: false,
            merge_pending: true,
            ready: None,
            counters: ThreadIndexCounters::default(),
            #[cfg(test)]
            commit_timings_us: Vec::new(),
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

/// Evicts oldest-first (`retention_at_ns ASC, run_id ASC`) until the logical
/// total fits `max_bytes`. Used when the budget shrinks or charges change.
fn enforce_budget(conn: &Connection, max_bytes: u64) -> Result<u64, StorageError> {
    let mut evicted = 0;
    let mut index = SqliteThreadIndex::new(conn);
    let mut total = charged_total(conn)?;
    while total > max_bytes {
        let Some((run_id, charged)) = index.eviction_candidates(1)?.into_iter().next() else {
            break;
        };
        index.evict_index(&run_id, IndexEvictionReason::LogicalBudget)?;
        total = total.saturating_sub(charged);
        evicted += 1;
    }
    Ok(evicted)
}

/// Writer-ordered settings ingestion: the new limit is effective from here.
/// Version changes follow R2 in the same transaction (extractor: re-extract
/// live rows; accounting: re-account without re-extraction, then the needed
/// logical eviction). Raising the limit never revives evicted rows.
pub(crate) fn apply_settings(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    settings: ThreadIndexSettings,
) -> Result<IndexSettingsReceipt, StorageError> {
    let accounting = rt.accounting;
    immediate(env.conn, |conn| {
        let (extractor, accounting_version) = applied_versions(conn)?;
        if extractor != EXTRACTOR_VERSION {
            conn.execute(
                "UPDATE thread_index_runs SET state = 'pending', attempt_count = 0, \
                 next_attempt_at_ns = NULL, blocked_reason = NULL, last_error_code = NULL, \
                 extractor_version = ?1, work_generation = work_generation + 1 \
                 WHERE state IN ('pending', 'indexed', 'retry_wait', 'blocked')",
                [EXTRACTOR_VERSION],
            )?;
            conn.execute(
                "UPDATE thread_index_runs SET extractor_version = ?1 WHERE state = 'evicted'",
                [EXTRACTOR_VERSION],
            )?;
            conn.execute(
                "UPDATE thread_index_state SET extractor_version = ?1 WHERE id = 1",
                [EXTRACTOR_VERSION],
            )?;
        }
        if accounting_version != accounting.version {
            let mut statement = conn.prepare(
                "SELECT run_id, text_bytes, attribute_bytes, block_count FROM thread_index_runs \
                 WHERE state = 'indexed'",
            )?;
            let indexed = statement
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, u64>(1)?,
                        row.get::<_, u64>(2)?,
                        row.get::<_, u64>(3)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            for (run_id, text, attributes, blocks) in indexed {
                let charged = reaccount_index(text, attributes, blocks, accounting)
                    .map_err(|error| StorageError::Serialization(format!("{error:?}")))?
                    .charged_bytes;
                conn.execute(
                    "UPDATE thread_index_runs SET charged_bytes = ?2, accounting_version = ?3, \
                     indexed_accounting_version = ?3 WHERE run_id = ?1",
                    params![run_id, charged, accounting.version],
                )?;
            }
            conn.execute(
                "UPDATE thread_index_runs SET accounting_version = ?1 \
                 WHERE state IN ('pending', 'retry_wait', 'blocked', 'evicted')",
                [accounting.version],
            )?;
            conn.execute(
                "UPDATE thread_index_state SET accounting_version = ?1 WHERE id = 1",
                [accounting.version],
            )?;
        }
        rt.counters.evictions += enforce_budget(conn, settings.max_bytes)?;
        Ok(())
    })?;
    rt.settings = settings;
    rt.settings_generation += 1;
    rt.pause = None;
    Ok(IndexSettingsReceipt {
        applied_generation: rt.settings_generation,
        settings,
    })
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
    let measured = env.measure()?;
    if rt.sticky() {
        let mut status = rt.pause.expect("sticky pause recorded");
        status.db_bytes = measured.sizes.db;
        status.wal_bytes = measured.sizes.wal;
        status.shm_bytes = measured.sizes.shm;
        return Ok(status);
    }
    Ok(capacity::evaluate(
        &rt.policy,
        env.limits,
        rt.settings.enabled,
        measured,
        growth,
        rt.pause.is_some(),
    ))
}

fn pause(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    status: IndexCapacityStatus,
    now_ns: i64,
) -> Result<IndexCapacityStatus, StorageError> {
    let status = match status.state {
        CapacityState::IndexPaused(IndexPauseReason::ReserveInsufficient)
        | CapacityState::SourceSuspended(LimitKind::DbSize)
            if rt.settings.enabled && env.limits.max_db_bytes > rt.policy.reserve_bytes() =>
        {
            reclaim(env, rt)?
        }
        _ => status,
    };
    if status.state == CapacityState::Normal {
        rt.pause = None;
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
    let reason = if status.state == CapacityState::IndexPaused(IndexPauseReason::Disabled) {
        RequestGapReason::IndexDisabled
    } else {
        RequestGapReason::CapacityPaused
    };
    immediate(env.conn, |conn| {
        requests::gap_pending(conn, reason)?;
        requests::finalize(conn, now_ns)
    })?;
    Ok(status)
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
    let resume_growth = growth_for_step_max(rt, page_size, 0);
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
        let mut deficit = needed.saturating_sub(u128::from(env.limits.max_db_bytes));
        let mut index = SqliteThreadIndex::new(conn);
        for (run_id, charged) in index.eviction_candidates(rt.policy.reclaim_max_runs_per_cycle)? {
            if deficit == 0 {
                break;
            }
            let now = env.measure()?;
            let freelist = capacity::freelist_bytes(conn, page_size)?;
            let PlannedGrowth::Bounded { db, wal, shm } =
                rt.policy.planned_growth(charged, page_size, freelist, true)
            else {
                break;
            };
            let after = u128::from(now.sizes.total()) + u128::from(db + wal + shm);
            let keep = u128::from(rt.policy.foreground_bound_bytes);
            if after + keep >= u128::from(env.limits.max_db_bytes)
                || u128::from(now.sizes.wal) + u128::from(wal)
                    >= u128::from(env.limits.max_wal_bytes)
            {
                break;
            }
            index.evict_index(&run_id, IndexEvictionReason::PhysicalReclaim)?;
            rt.counters.evictions += 1;
            rt.merge_pending = true;
            deficit = deficit.saturating_sub(u128::from(charged));
        }
        if !capacity::checkpoint_truncate(conn)? {
            let status = sticky(env, rt, IndexPauseReason::WalReaderPinned)?;
            log_reclaim(cycle, before, env, &status)?;
            return Ok(status);
        }
        if capacity::incremental_vacuum_enabled(conn)? {
            let free_pages = capacity::freelist_bytes(conn, page_size)? / page_size;
            let room = env
                .limits
                .max_wal_bytes
                .saturating_sub(env.measure()?.sizes.wal)
                .saturating_sub(rt.policy.hysteresis_bytes)
                / (page_size + 24);
            let pages = free_pages.min(room);
            if pages > 0 {
                conn.execute_batch(&format!("PRAGMA incremental_vacuum({pages})"))?;
                if !capacity::checkpoint_truncate(conn)? {
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
        let _ = capacity::checkpoint_truncate(env.conn)?;
    }
    let page_size = capacity::page_size(env.conn)?;
    let freelist = capacity::freelist_bytes(env.conn, page_size)?;
    let growth = growth_for_step_max(rt, page_size, freelist);
    let status = capacity::evaluate(
        &rt.policy,
        env.limits,
        rt.settings.enabled,
        env.measure()?,
        growth,
        true,
    );
    if status.state == CapacityState::Normal {
        tracing::info!(db_bytes = status.db_bytes, "thread index resumed");
        rt.pause = None;
        return Ok(true);
    }
    rt.pause = Some(status);
    Ok(false)
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
    if !lease::is_valid(env.conn, lease, now_ns)? {
        rt.ready = None;
        return Ok(Action::LeaseLost);
    }
    rt.counters.steps += 1;
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
        return pause(env, rt, status, now_ns).map(Action::Paused);
    }
    if rt.pause.is_some() && (rt.sticky() || !recheck_pause(env, rt)?) {
        let status = rt.pause.expect("still paused");
        immediate(env.conn, |conn| {
            requests::gap_pending(conn, RequestGapReason::CapacityPaused)?;
            requests::finalize(conn, now_ns)
        })?;
        return Ok(Action::Paused(status));
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
        return merge_step(env, rt, now_ns);
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

fn merge_step(env: Env<'_>, rt: &mut IndexRuntime, now_ns: i64) -> Result<Action, StorageError> {
    let page_size = capacity::page_size(env.conn)?;
    let growth = PlannedGrowth::Bounded {
        db: MERGE_GROWTH_PAGES * page_size,
        wal: MERGE_GROWTH_PAGES * (page_size + 24) + 32,
        shm: 32 * 1024 * (MERGE_GROWTH_PAGES / 4096 + 1),
    };
    let more = immediate(env.conn, |conn| {
        let status = capacity_status(env, rt, growth)?;
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
        Ok(Ok(changes(conn)? - before >= 2))
    })?;
    match more {
        Ok(more) => {
            rt.counters.merges += 1;
            rt.merge_pending = more;
            Ok(Action::Progress)
        }
        Err(status) => pause(env, rt, status, now_ns).map(Action::Paused),
    }
}

fn run_matches(conn: &Connection, work: &Work) -> Result<bool, StorageError> {
    let row: Option<(String, u64, u64)> = conn
        .query_row(
            "SELECT state, source_revision, work_generation FROM thread_index_runs WHERE run_id = ?1",
            [&work.target.run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    Ok(row.is_some_and(|(state, revision, generation)| {
        state == "pending"
            && revision == work.target.source_revision
            && generation == work.work_generation
    }))
}

fn evict_self(
    env: Env<'_>,
    rt: &mut IndexRuntime,
    lease: &IndexLease,
    work: &Work,
    reason: IndexEvictionReason,
    code: Option<&str>,
    now_ns: i64,
) -> Result<(), StorageError> {
    immediate(env.conn, |conn| {
        if !lease::is_valid(conn, lease, now_ns)? || !run_matches(conn, work)? {
            rt.counters.discarded += 1;
            return Ok(());
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
        Ok(())
    })
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
            evict_self(
                env,
                rt,
                lease,
                &work,
                IndexEvictionReason::Oversized,
                Some(requests::OVERSIZED_STEP_CODE),
                now_ns,
            )?;
            return Ok(Action::Progress);
        }
    };
    if !rt.policy.within_step(&accounting) {
        evict_self(
            env,
            rt,
            lease,
            &work,
            IndexEvictionReason::Oversized,
            Some(requests::OVERSIZED_STEP_CODE),
            now_ns,
        )?;
        return Ok(Action::Progress);
    }
    if accounting.charged_bytes > rt.settings.max_bytes {
        evict_self(
            env,
            rt,
            lease,
            &work,
            IndexEvictionReason::Oversized,
            Some(requests::OVERSIZED_BUDGET_CODE),
            now_ns,
        )?;
        return Ok(Action::Progress);
    }
    match budget_step(env, rt, lease, &work, &accounting, now_ns)? {
        Budget::Fits => commit(env, rt, lease, work, input, accounting, now_ns),
        Budget::EvictedOlder => {
            rt.ready = Some((work, Ok(input)));
            Ok(Action::Progress)
        }
        Budget::EvictedSelf => Ok(Action::Progress),
        Budget::Paused(status) => {
            rt.ready = Some((work, Ok(input)));
            pause(env, rt, status, now_ns).map(Action::Paused)
        }
    }
}

enum Budget {
    Fits,
    EvictedOlder,
    EvictedSelf,
    Paused(IndexCapacityStatus),
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
    let conn = env.conn;
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
        return Ok(Budget::Fits);
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
    let Some((run_id, charged)) = older else {
        evict_self(
            env,
            rt,
            lease,
            work,
            IndexEvictionReason::LogicalBudget,
            None,
            now_ns,
        )?;
        return Ok(Budget::EvictedSelf);
    };
    let page_size = capacity::page_size(conn)?;
    let freelist = capacity::freelist_bytes(conn, page_size)?;
    let growth = rt.policy.planned_growth(charged, page_size, freelist, true);
    immediate(conn, |conn| {
        let status = capacity_status(Env { conn, ..env }, rt, growth)?;
        if status.state != CapacityState::Normal {
            return Ok(Budget::Paused(status));
        }
        if SqliteThreadIndex::new(conn)
            .check_guard(&guard(lease, work), now_ns)?
            .is_some()
        {
            return Ok(Budget::Fits);
        }
        SqliteThreadIndex::new(conn).evict_index(&run_id, IndexEvictionReason::LogicalBudget)?;
        rt.counters.evictions += 1;
        rt.merge_pending = true;
        Ok(Budget::EvictedOlder)
    })
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
    let conn = env.conn;
    let page_size = capacity::page_size(conn)?;
    let freelist = capacity::freelist_bytes(conn, page_size)?;
    let growth = rt
        .policy
        .planned_growth(accounting.charged_bytes, page_size, freelist, false);
    #[cfg(test)]
    let started = std::time::Instant::now();
    let transaction = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    #[cfg(test)]
    let begin_us = started.elapsed().as_micros();
    #[cfg(test)]
    let body_started = std::time::Instant::now();
    let outcome = (|conn: &Connection| -> Result<Result<IndexCommitResult, IndexCapacityStatus>, StorageError> {
        let status = capacity_status(Env { conn, ..env }, rt, growth)?;
        if status.state != CapacityState::Normal {
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
    })(&transaction)?;
    #[cfg(test)]
    let body_us = body_started.elapsed().as_micros();
    #[cfg(test)]
    let finish_started = std::time::Instant::now();
    transaction.commit()?;
    #[cfg(test)]
    if std::env::var_os("EVORCH_INDEX_GATE_WORK").is_some() {
        rt.commit_timings_us.extend([
            ("begin", begin_us),
            ("body", body_us),
            ("finish", finish_started.elapsed().as_micros()),
        ]);
    }
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
            pause(env, rt, status, now_ns).map(Action::Paused)
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
