//! Usage ledger persistence: request rows, run attribution and daily rollups.
use rusqlite::{Connection, Row, params};

use crate::StorageError;
use crate::usage::{
    RunAttribution, UsageDailyRow, UsageRequestRecord, UsageRequestRow, UsageStatus,
};

/// Rows rolled up per maintenance tick, so retention never blocks the writer.
const ROLLUP_BATCH: i64 = 5_000;

/// Insert records in one transaction. A request ID already present is kept.
pub fn insert_requests(
    conn: &Connection,
    records: &[UsageRequestRecord],
) -> Result<(), StorageError> {
    if records.is_empty() {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT OR IGNORE INTO usage_requests (request_id, at_ns, provider, profile, model, \
             run_id, parent_run_id, role, purpose, status, failure, finish_reason, input_tokens, \
             output_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens, ttft_ms, \
             duration_ms, cost_usd) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, \
             ?18, ?19, ?20)",
        )?;
        for record in records {
            statement.execute(params![
                record.request_id,
                record.at_ns,
                record.provider,
                record.profile,
                record.model,
                record.run_id,
                record.parent_run_id,
                record.role,
                record.purpose,
                record.status.as_str(),
                record.failure,
                record.finish_reason,
                to_i64(record.input_tokens, "input tokens")?,
                to_i64(record.output_tokens, "output tokens")?,
                to_i64(record.cache_read_tokens, "cache read tokens")?,
                to_i64(record.cache_write_tokens, "cache write tokens")?,
                record
                    .reasoning_tokens
                    .map(|value| to_i64(value, "reasoning tokens"))
                    .transpose()?,
                record
                    .ttft_ms
                    .map(|value| to_i64(value, "ttft"))
                    .transpose()?,
                to_i64(record.duration_ms, "duration")?,
                record.cost_usd.filter(|cost| cost.is_finite()),
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Replace the owner of each run; the last binding wins.
pub fn attribute_runs(
    conn: &Connection,
    attributions: &[RunAttribution],
) -> Result<(), StorageError> {
    if attributions.is_empty() {
        return Ok(());
    }
    let transaction = conn.unchecked_transaction()?;
    {
        let mut statement = transaction.prepare_cached(
            "INSERT INTO usage_run_threads (run_id, thread_id, project_id) VALUES (?1, ?2, ?3) \
             ON CONFLICT(run_id) DO UPDATE SET thread_id = excluded.thread_id, \
             project_id = excluded.project_id",
        )?;
        for attribution in attributions {
            statement.execute(params![
                attribution.run_id,
                attribution.thread_id,
                attribution.project_id
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

/// Fold one bounded batch of rows older than `cutoff_ns` into `usage_daily`
/// (local calendar days) and delete them. Returns the number of rows moved.
pub fn roll_up_before(conn: &Connection, cutoff_ns: i64) -> Result<usize, StorageError> {
    // Both statements select the same deterministic batch inside one transaction.
    const BATCH: &str = "SELECT rowid FROM usage_requests WHERE at_ns < ?1 \
                         ORDER BY at_ns, rowid LIMIT ?2";
    let transaction = conn.unchecked_transaction()?;
    transaction.execute(
        &format!(
            "INSERT INTO usage_daily (day, provider, profile, model, project_id, role, purpose, \
             request_count, failed_count, input_tokens, output_tokens, cache_read_tokens, \
             cache_write_tokens, reasoning_tokens, cost_usd, unpriced_input_tokens, \
             unpriced_output_tokens, unpriced_cache_read_tokens, unpriced_cache_write_tokens, \
             ttft_sum_ms, ttft_count, duration_sum_ms) \
             SELECT date(r.at_ns / 1000000000, 'unixepoch', 'localtime'), r.provider, \
             COALESCE(r.profile, ''), r.model, COALESCE(t.project_id, ''), COALESCE(r.role, ''), \
             COALESCE(r.purpose, ''), COUNT(*), SUM(r.status = 'failed'), SUM(r.input_tokens), \
             SUM(r.output_tokens), SUM(r.cache_read_tokens), SUM(r.cache_write_tokens), \
             COALESCE(SUM(r.reasoning_tokens), 0), COALESCE(SUM(r.cost_usd), 0), \
             SUM(CASE WHEN r.cost_usd IS NULL THEN r.input_tokens ELSE 0 END), \
             SUM(CASE WHEN r.cost_usd IS NULL THEN r.output_tokens ELSE 0 END), \
             SUM(CASE WHEN r.cost_usd IS NULL THEN r.cache_read_tokens ELSE 0 END), \
             SUM(CASE WHEN r.cost_usd IS NULL THEN r.cache_write_tokens ELSE 0 END), \
             COALESCE(SUM(r.ttft_ms), 0), COUNT(r.ttft_ms), SUM(r.duration_ms) \
             FROM usage_requests r LEFT JOIN usage_run_threads t ON t.run_id = r.run_id \
             WHERE r.rowid IN ({BATCH}) \
             GROUP BY 1, 2, 3, 4, 5, 6, 7 \
             ON CONFLICT(day, provider, profile, model, project_id, role, purpose) DO UPDATE SET \
             request_count = request_count + excluded.request_count, \
             failed_count = failed_count + excluded.failed_count, \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
             cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
             reasoning_tokens = reasoning_tokens + excluded.reasoning_tokens, \
             cost_usd = cost_usd + excluded.cost_usd, \
             unpriced_input_tokens = unpriced_input_tokens + excluded.unpriced_input_tokens, \
             unpriced_output_tokens = unpriced_output_tokens + excluded.unpriced_output_tokens, \
             unpriced_cache_read_tokens = \
             unpriced_cache_read_tokens + excluded.unpriced_cache_read_tokens, \
             unpriced_cache_write_tokens = \
             unpriced_cache_write_tokens + excluded.unpriced_cache_write_tokens, \
             ttft_sum_ms = ttft_sum_ms + excluded.ttft_sum_ms, \
             ttft_count = ttft_count + excluded.ttft_count, \
             duration_sum_ms = duration_sum_ms + excluded.duration_sum_ms"
        ),
        params![cutoff_ns, ROLLUP_BATCH],
    )?;
    let moved = transaction.execute(
        &format!("DELETE FROM usage_requests WHERE rowid IN ({BATCH})"),
        params![cutoff_ns, ROLLUP_BATCH],
    )?;
    transaction.commit()?;
    Ok(moved)
}

const REQUEST_COLUMNS: &str = "SELECT r.request_id, r.at_ns, r.provider, r.profile, r.model, \
     r.run_id, r.parent_run_id, r.role, r.purpose, r.status, r.failure, r.finish_reason, \
     r.input_tokens, r.output_tokens, r.cache_read_tokens, r.cache_write_tokens, \
     r.reasoning_tokens, r.ttft_ms, r.duration_ms, r.cost_usd, t.thread_id, t.project_id, \
     date(r.at_ns / 1000000000, 'unixepoch', 'localtime'), \
     time(r.at_ns / 1000000000, 'unixepoch', 'localtime') \
     FROM usage_requests r LEFT JOIN usage_run_threads t ON t.run_id = r.run_id";

/// Requests in `[from_ns, to_ns)` ordered by time, joined with their owner.
pub fn list_requests(
    conn: &Connection,
    from_ns: i64,
    to_ns: i64,
) -> Result<Vec<UsageRequestRow>, StorageError> {
    query_requests(
        conn,
        &format!(
            "{REQUEST_COLUMNS} WHERE r.at_ns >= ?1 AND r.at_ns < ?2 ORDER BY r.at_ns, r.rowid"
        ),
        params![from_ns, to_ns],
    )
}

/// Requests on the inclusive local days `[from_day, to_day]` (`YYYY-MM-DD`).
pub fn list_requests_in_days(
    conn: &Connection,
    from_day: &str,
    to_day: &str,
) -> Result<Vec<UsageRequestRow>, StorageError> {
    // unixepoch(day, 'utc') reads the day as local midnight and converts it to UTC.
    query_requests(
        conn,
        &format!(
            "{REQUEST_COLUMNS} WHERE r.at_ns >= unixepoch(?1, 'utc') * 1000000000 \
             AND r.at_ns < unixepoch(?2, '+1 day', 'utc') * 1000000000 \
             ORDER BY r.at_ns, r.rowid"
        ),
        params![from_day, to_day],
    )
}

/// Today's local day and its midnight bounds, on the same clock as the ledger days.
pub fn local_clock(conn: &Connection) -> Result<crate::usage::LocalClock, StorageError> {
    Ok(conn.query_row(
        "SELECT date('now', 'localtime'), \
         unixepoch(date('now', 'localtime'), 'utc') * 1000000000, \
         unixepoch(date('now', 'localtime'), '+1 day', 'utc') * 1000000000",
        [],
        |row| {
            Ok(crate::usage::LocalClock {
                today: row.get(0)?,
                day_start_ns: row.get(1)?,
                day_end_ns: row.get(2)?,
            })
        },
    )?)
}

/// Today's local calendar day, `YYYY-MM-DD`, on the same clock as the ledger days.
pub fn local_today(conn: &Connection) -> Result<String, StorageError> {
    Ok(conn.query_row("SELECT date('now', 'localtime')", [], |row| row.get(0))?)
}

fn query_requests(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<Vec<UsageRequestRow>, StorageError> {
    let mut statement = conn.prepare(sql)?;
    let mut rows = statement.query(params)?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        result.push(UsageRequestRow {
            record: request_record(row)?,
            day: row.get(22)?,
            time: row.get(23)?,
            thread_id: row.get(20)?,
            project_id: row.get(21)?,
        });
    }
    Ok(result)
}

/// Rolled-up days in the inclusive `[from_day, to_day]` range (`YYYY-MM-DD`).
pub fn list_daily(
    conn: &Connection,
    from_day: &str,
    to_day: &str,
) -> Result<Vec<UsageDailyRow>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT day, provider, profile, model, project_id, role, purpose, request_count, \
         failed_count, input_tokens, output_tokens, cache_read_tokens, cache_write_tokens, \
         reasoning_tokens, cost_usd, unpriced_input_tokens, unpriced_output_tokens, \
         unpriced_cache_read_tokens, unpriced_cache_write_tokens, ttft_sum_ms, ttft_count, \
         duration_sum_ms FROM usage_daily WHERE day >= ?1 AND day <= ?2 \
         ORDER BY day, provider, profile, model, project_id, role, purpose",
    )?;
    let mut rows = statement.query(params![from_day, to_day])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let count = |index: usize, name: &'static str| -> Result<u64, StorageError> {
            from_i64(row.get(index)?, name)
        };
        result.push(UsageDailyRow {
            day: row.get(0)?,
            provider: row.get(1)?,
            profile: non_empty(row.get(2)?),
            model: row.get(3)?,
            project_id: non_empty(row.get(4)?),
            role: non_empty(row.get(5)?),
            purpose: non_empty(row.get(6)?),
            request_count: count(7, "request count")?,
            failed_count: count(8, "failed count")?,
            input_tokens: count(9, "input tokens")?,
            output_tokens: count(10, "output tokens")?,
            cache_read_tokens: count(11, "cache read tokens")?,
            cache_write_tokens: count(12, "cache write tokens")?,
            reasoning_tokens: count(13, "reasoning tokens")?,
            cost_usd: row.get(14)?,
            unpriced_input_tokens: count(15, "unpriced input tokens")?,
            unpriced_output_tokens: count(16, "unpriced output tokens")?,
            unpriced_cache_read_tokens: count(17, "unpriced cache read tokens")?,
            unpriced_cache_write_tokens: count(18, "unpriced cache write tokens")?,
            ttft_sum_ms: count(19, "ttft sum")?,
            ttft_count: count(20, "ttft count")?,
            duration_sum_ms: count(21, "duration sum")?,
        });
    }
    Ok(result)
}

fn request_record(row: &Row<'_>) -> Result<UsageRequestRecord, StorageError> {
    let optional = |index: usize, name: &'static str| -> Result<Option<u64>, StorageError> {
        row.get::<_, Option<i64>>(index)?
            .map(|value| from_i64(value, name))
            .transpose()
    };
    Ok(UsageRequestRecord {
        request_id: row.get(0)?,
        at_ns: row.get(1)?,
        provider: row.get(2)?,
        profile: row.get(3)?,
        model: row.get(4)?,
        run_id: row.get(5)?,
        parent_run_id: row.get(6)?,
        role: row.get(7)?,
        purpose: row.get(8)?,
        status: UsageStatus::parse(&row.get::<_, String>(9)?),
        failure: row.get(10)?,
        finish_reason: row.get(11)?,
        input_tokens: from_i64(row.get(12)?, "input tokens")?,
        output_tokens: from_i64(row.get(13)?, "output tokens")?,
        cache_read_tokens: from_i64(row.get(14)?, "cache read tokens")?,
        cache_write_tokens: from_i64(row.get(15)?, "cache write tokens")?,
        reasoning_tokens: optional(16, "reasoning tokens")?,
        ttft_ms: optional(17, "ttft")?,
        duration_ms: from_i64(row.get(18)?, "duration")?,
        cost_usd: row.get(19)?,
    })
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

fn to_i64(value: u64, name: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::OutOfRange(name))
}

fn from_i64(value: i64, name: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::OutOfRange(name))
}
