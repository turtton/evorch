//! Physical admission and reclaim for the conversation index (ADR 0041 R3).
//!
//! `H = hard_limits.max_db_bytes`, `P = file_sizes().total()` (DB+WAL+SHM),
//! `F` = conservative bound of one foreground canonical transaction,
//! `R = max(R_floor, ceil(safety × F))`. Index writes stop at `P+G+R >= H` or
//! `WAL+G_wal >= max_wal` (equality stops) and resume only when BOTH hold with
//! the hysteresis `J` strictly below the limits and source writes run. These
//! are admission limits for the index only; canonical writes keep the hard
//! limit decision in `state.rs` and may use the reserve while the index pauses.

use rusqlite::Connection;

use crate::db::FileSizes;
use crate::thread_index::{
    CapacityState, IndexAccounting, IndexCapacityStatus, IndexPauseReason, PlannedGrowth,
};
use crate::{HardLimits, LimitKind, StorageError};

/// WAL frame header bytes added to every page written to the WAL.
const WAL_FRAME_HEADER: u64 = 24;
/// WAL file header bytes.
const WAL_HEADER: u64 = 32;
/// The wal-index (SHM) grows by 32 KiB per 4096 frames.
const SHM_CHUNK_BYTES: u64 = 32 * 1024;
const SHM_FRAMES_PER_CHUNK: u64 = 4096;

/// Values fixed by the ADR 0041 順序 5 start-gate measurement. They are an
/// admission policy derived from measured maxima, not universal upper bounds:
/// inputs outside the measured step envelope are never admitted (oversized).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CapacityPolicy {
    /// `F`: largest measured foreground growth (snapshot / note / event).
    pub foreground_bound_bytes: u64,
    pub reserve_floor_bytes: u64,
    pub safety_numerator: u64,
    pub safety_denominator: u64,
    /// `J`.
    pub hysteresis_bytes: u64,
    /// Written pages per charged byte (per mille of `charged / page_size`).
    pub growth_pages_per_mille: u64,
    /// Constant pages covering FTS segment merges and B-tree splits.
    pub growth_constant_pages: u64,
    /// Single-step write envelope; larger runs become `evicted(oversized)`.
    pub step_max_text_bytes: u64,
    pub step_max_blocks: u64,
    /// Eviction write growth per charged byte freed (per mille, in pages).
    pub delete_pages_per_mille: u64,
    pub reclaim_max_runs_per_cycle: usize,
}

/// Fixed by the gate measurement on the real store copy (see ADR 0041
/// 「Phase B 順序 5 開始ゲートの計測」). With FTS `automerge=0` and bounded
/// explicit merge steps, every measured commit used at most 88.5% of the WAL
/// bound and 75.4% of the total bound below.
pub(crate) const MEASURED_POLICY: CapacityPolicy = CapacityPolicy {
    // Re-saving the largest snapshot (9.68 MB JSON): WAL 9,986,912 + main
    // file 9,846,784 + SHM 32,768.
    foreground_bound_bytes: 19_866_464,
    reserve_floor_bytes: 64 << 20,
    safety_numerator: 2,
    safety_denominator: 1,
    hysteresis_bytes: 16 << 20,
    growth_pages_per_mille: 1_500,
    growth_constant_pages: 256,
    // No measured run exceeds either bound (max 990,976 B / 1,169 blocks).
    step_max_text_bytes: 1 << 20,
    step_max_blocks: 2_048,
    delete_pages_per_mille: 7_000,
    reclaim_max_runs_per_cycle: 64,
};

/// Explicit FTS merge work per step (pages) and the growth bound admitted for
/// one such step (measured max 1,115 frames).
pub(crate) const MERGE_STEP_PAGES: i64 = 64;
pub(crate) const MERGE_GROWTH_PAGES: u64 = 2_048;

impl CapacityPolicy {
    /// `R = max(R_floor, ceil(safety × F))`.
    pub(crate) fn reserve_bytes(&self) -> u64 {
        let scaled = (u128::from(self.foreground_bound_bytes) * u128::from(self.safety_numerator))
            .div_ceil(u128::from(self.safety_denominator.max(1)));
        u64::try_from(scaled)
            .unwrap_or(u64::MAX)
            .max(self.reserve_floor_bytes)
    }

    /// Whether one run fits the measured single-step write envelope.
    pub(crate) fn within_step(&self, accounting: &IndexAccounting) -> bool {
        accounting.text_bytes <= self.step_max_text_bytes
            && accounting.block_count <= self.step_max_blocks
    }

    fn pages(&self, charged: u64, page_size: u64, per_mille: u64) -> Option<u64> {
        let scaled = u128::from(charged)
            .checked_mul(u128::from(per_mille))?
            .div_ceil(u128::from(page_size.checked_mul(1000)?));
        u64::try_from(scaled)
            .ok()?
            .checked_add(self.growth_constant_pages)
    }

    /// Physical upper bound of committing `accounting` (or deleting that many
    /// charged bytes when `deleting`). Existing freelist pages are measured
    /// free space the main file reuses before it grows; planned deletions are
    /// never credited. Unknown or overflowing bounds are `Unbounded`.
    pub(crate) fn planned_growth(
        &self,
        charged: u64,
        page_size: u64,
        freelist_bytes: u64,
        deleting: bool,
    ) -> PlannedGrowth {
        if page_size == 0 {
            return PlannedGrowth::Unbounded;
        }
        let per_mille = if deleting {
            self.delete_pages_per_mille
        } else {
            self.growth_pages_per_mille
        };
        let Some(pages) = self.pages(charged, page_size, per_mille) else {
            return PlannedGrowth::Unbounded;
        };
        let wal = pages
            .checked_mul(page_size + WAL_FRAME_HEADER)
            .and_then(|bytes| bytes.checked_add(WAL_HEADER));
        let db = pages
            .checked_mul(page_size)
            .map(|bytes| bytes.saturating_sub(freelist_bytes));
        let shm = (pages / SHM_FRAMES_PER_CHUNK + 1).checked_mul(SHM_CHUNK_BYTES);
        match (db, wal, shm) {
            (Some(db), Some(wal), Some(shm)) => PlannedGrowth::Bounded { db, wal, shm },
            _ => PlannedGrowth::Unbounded,
        }
    }
}

/// Inputs of one admission decision, all measured inside the writer.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Measurement {
    pub sizes: FileSizes,
    pub source_suspended: bool,
}

/// The shared `capacity_status` decision. `paused` selects the resume rule
/// (`+J`, strict) over the stop rule (equality stops).
pub(crate) fn evaluate(
    policy: &CapacityPolicy,
    limits: &HardLimits,
    enabled: bool,
    measured: Measurement,
    growth: PlannedGrowth,
    paused: bool,
) -> IndexCapacityStatus {
    let reserve = policy.reserve_bytes();
    let status = |state| IndexCapacityStatus {
        state,
        db_bytes: measured.sizes.db,
        wal_bytes: measured.sizes.wal,
        shm_bytes: measured.sizes.shm,
        reserve_bytes: reserve,
        resume_hysteresis_bytes: policy.hysteresis_bytes,
    };
    let paused_by = |reason| status(CapacityState::IndexPaused(reason));
    if !enabled {
        return paused_by(IndexPauseReason::Disabled);
    }
    if measured.source_suspended {
        return status(CapacityState::SourceSuspended(LimitKind::DbSize));
    }
    if limits.max_db_bytes <= reserve {
        return paused_by(IndexPauseReason::ReserveInsufficient);
    }
    let PlannedGrowth::Bounded { db, wal, shm } = growth else {
        return paused_by(IndexPauseReason::GrowthUnbounded);
    };
    let margin = if paused { policy.hysteresis_bytes } else { 0 };
    let total = u128::from(measured.sizes.total())
        + u128::from(db)
        + u128::from(wal)
        + u128::from(shm)
        + u128::from(reserve)
        + u128::from(margin);
    if total >= u128::from(limits.max_db_bytes) {
        return paused_by(IndexPauseReason::ReserveInsufficient);
    }
    let wal_total = u128::from(measured.sizes.wal) + u128::from(wal) + u128::from(margin);
    if wal_total >= u128::from(limits.max_wal_bytes) {
        return paused_by(IndexPauseReason::WalLimit);
    }
    status(CapacityState::Normal)
}

/// `PRAGMA wal_checkpoint(TRUNCATE)` without waiting on readers; `false` when
/// a reader keeps frames that could not be checkpointed. The busy handler is
/// suspended for the call so a pinned WAL never stalls the writer thread.
pub(crate) fn checkpoint_truncate(conn: &Connection) -> Result<bool, StorageError> {
    conn.busy_timeout(std::time::Duration::ZERO)?;
    let result = conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    });
    // Same value `db.rs` installs on every connection.
    conn.busy_timeout(std::time::Duration::from_millis(5_000))?;
    let (busy, log, checkpointed) = match result {
        Ok(row) => row,
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DatabaseBusy =>
        {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    Ok(busy == 0 && log == checkpointed)
}

pub(crate) fn page_size(conn: &Connection) -> Result<u64, StorageError> {
    let size: i64 = conn.pragma_query_value(None, "page_size", |row| row.get(0))?;
    u64::try_from(size).map_err(|_| StorageError::OutOfRange("page_size"))
}

pub(crate) fn freelist_bytes(conn: &Connection, page_size: u64) -> Result<u64, StorageError> {
    let pages: i64 = conn.pragma_query_value(None, "freelist_count", |row| row.get(0))?;
    let pages = u64::try_from(pages).map_err(|_| StorageError::OutOfRange("freelist_count"))?;
    Ok(pages.saturating_mul(page_size))
}

pub(crate) fn incremental_vacuum_enabled(conn: &Connection) -> Result<bool, StorageError> {
    const AUTO_VACUUM_INCREMENTAL: i64 = 2;
    let mode: i64 = conn.pragma_query_value(None, "auto_vacuum", |row| row.get(0))?;
    Ok(mode == AUTO_VACUUM_INCREMENTAL)
}
