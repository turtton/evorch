//! Aggregated refusals: one row per 60 second window and bounded key.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::store::{Limits, cutoff, expired, inconsistent, serialize};
use super::{
    AuditError, AuditPrincipal, AuditReply, DENIAL_MAX_ROWS, DenialAggregate, DenialKey, validate,
};
use crate::StorageError;

/// Room kept beyond the key text for the numeric fields; the budget of a row is fixed so
/// that merging more counts into it never changes the total.
const ROW_RESERVE_BYTES: usize = 384;

fn normalized(key: &DenialKey) -> DenialKey {
    match key {
        DenialKey::Caller {
            principal: None,
            surface,
            operation,
            reason,
        } => DenialKey::Caller {
            principal: Some(AuditPrincipal::anonymous()),
            surface: *surface,
            operation: *operation,
            reason: *reason,
        },
        other => other.clone(),
    }
}

fn key_text(key: &DenialKey) -> Result<String, StorageError> {
    serialize(key)
}

pub(crate) fn totals(conn: &Connection) -> Result<(u64, u64), StorageError> {
    Ok(conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(budget_bytes), 0) FROM admin_denials",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?)
}

fn clamp(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

fn sql_i64(value: u64) -> i64 {
    // Validated beforehand to fit.
    i64::try_from(value).unwrap_or(i64::MAX)
}

pub(crate) fn write(
    tx: &Transaction<'_>,
    rows: Vec<DenialAggregate>,
    now_ms: u64,
    limits: &Limits,
) -> Result<AuditReply, StorageError> {
    if rows.len() > limits.batch {
        return Err(AuditError::Invalid("too many denial rows in one batch").into());
    }
    validate::db_u64(now_ms, "now_ms is beyond the storable range")?;
    for row in &rows {
        validate::denial(row)?;
    }
    let (mut row_count, mut byte_count) = totals(tx)?;
    let mut evicted = 0;
    let (mut stored, mut dropped) = (0, 0);
    let overflow_text = key_text(&DenialKey::Overflow)?;
    for mut row in rows {
        if expired(row.window_start_ms, limits.denial_retention_ms, now_ms) {
            dropped += 1;
            continue;
        }
        row.key = normalized(&row.key);
        let mut text = key_text(&row.key)?;
        let mut existing = find(tx, row.window_start_ms, &text)?;
        if existing.is_none() && row.key != DenialKey::Overflow {
            let callers: usize = tx.query_row(
                "SELECT COUNT(*) FROM admin_denials WHERE window_start_ms = ?1 AND key_text != ?2",
                params![sql_i64(row.window_start_ms), overflow_text],
                |r| r.get(0),
            )?;
            if callers >= limits.denial_keys_per_window {
                row.key = DenialKey::Overflow;
                text.clone_from(&overflow_text);
                existing = find(tx, row.window_start_ms, &text)?;
            }
        }
        if let Some((id, current)) = existing {
            let merged = DenialAggregate {
                key: row.key,
                window_start_ms: row.window_start_ms,
                first_ms: current.first_ms.min(row.first_ms),
                last_ms: current.last_ms.max(row.last_ms),
                count: current.count.saturating_add(row.count),
            };
            let json = serialize(&merged)?;
            tx.execute(
                "UPDATE admin_denials SET first_ms = ?2, last_ms = ?3, count = ?4, record_json = ?5 \
                 WHERE id = ?1",
                params![
                    id,
                    sql_i64(merged.first_ms),
                    sql_i64(merged.last_ms),
                    clamp(merged.count),
                    json
                ],
            )?;
            stored += 1;
            continue;
        }
        let json = serialize(&row)?;
        let budget = (text.len() + ROW_RESERVE_BYTES) as u64;
        if json.len() as u64 > budget || budget > super::AUDIT_MAX_RECORD_BYTES {
            return Err(AuditError::Invalid("the denial record exceeds the record limit").into());
        }
        while row_count + 1 > limits.denial_rows || byte_count + budget > limits.denial_bytes {
            if evicted >= limits.evictions {
                break;
            }
            let oldest: Option<(i64, u64)> = tx
                .query_row(
                    "SELECT id, budget_bytes FROM admin_denials ORDER BY window_start_ms, id LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let Some((id, freed)) = oldest else { break };
            tx.execute("DELETE FROM admin_denials WHERE id = ?1", [id])?;
            row_count -= 1;
            byte_count -= freed;
            evicted += 1;
        }
        if row_count + 1 > limits.denial_rows || byte_count + budget > limits.denial_bytes {
            dropped += 1;
            continue;
        }
        tx.execute(
            "INSERT INTO admin_denials(window_start_ms, key_text, first_ms, last_ms, count, \
             record_json, budget_bytes) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                sql_i64(row.window_start_ms),
                text,
                sql_i64(row.first_ms),
                sql_i64(row.last_ms),
                clamp(row.count),
                json,
                budget
            ],
        )?;
        row_count += 1;
        byte_count += budget;
        stored += 1;
    }
    Ok(AuditReply::Denials { stored, dropped })
}

fn find(
    conn: &Connection,
    window_start_ms: u64,
    text: &str,
) -> Result<Option<(i64, DenialAggregate)>, StorageError> {
    conn.query_row(
        "SELECT id, record_json FROM admin_denials WHERE window_start_ms = ?1 AND key_text = ?2",
        params![sql_i64(window_start_ms), text],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )
    .optional()?
    .map(|(id, json)| Ok((id, decode(&json, text)?)))
    .transpose()
}

fn decode(json: &str, text: &str) -> Result<DenialAggregate, StorageError> {
    let aggregate: DenialAggregate = serde_json::from_str(json).map_err(|_| inconsistent())?;
    validate::denial(&aggregate).map_err(|_| inconsistent())?;
    if key_text(&aggregate.key)? != text {
        return Err(inconsistent());
    }
    Ok(aggregate)
}

pub(crate) fn list(conn: &Connection, limit: usize) -> Result<Vec<DenialAggregate>, StorageError> {
    let limit = limit.min(DENIAL_MAX_ROWS as usize);
    let mut statement = conn.prepare(
        "SELECT record_json, key_text FROM admin_denials ORDER BY window_start_ms, id LIMIT ?1",
    )?;
    let rows = statement
        .query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.iter().map(|(json, text)| decode(json, text)).collect()
}

/// Removes up to one batch of expired aggregates; the flag says whether more are left.
pub(crate) fn cleanup(
    tx: &Transaction<'_>,
    now_ms: u64,
    limits: &Limits,
) -> Result<(u64, bool), StorageError> {
    let Some(cutoff) = cutoff(limits.denial_retention_ms, now_ms) else {
        return Ok((0, false));
    };
    let removed = tx.execute(
        "DELETE FROM admin_denials WHERE id IN (SELECT id FROM admin_denials \
         WHERE window_start_ms <= ?1 ORDER BY window_start_ms, id LIMIT ?2)",
        params![cutoff, i64::try_from(limits.batch).unwrap_or(i64::MAX)],
    )? as u64;
    let more = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM admin_denials WHERE window_start_ms <= ?1)",
        [cutoff],
        |row| row.get::<_, bool>(0),
    )?;
    Ok((removed, more))
}
