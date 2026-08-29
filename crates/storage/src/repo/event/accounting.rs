//! イベント容量の遅延シードと上限判定を担当します。

use std::time::SystemTime;

use rusqlite::Connection;

use crate::db::system_time_to_ns;
use crate::{LimitKind, StorageError};

use super::NANOS_PER_DAY;

/// セッションに属するイベント payload の累積バイト数を返します。
pub(crate) fn session_event_bytes(
    conn: &Connection,
    session_id: &str,
) -> Result<u64, StorageError> {
    sum_payload_bytes(
        conn,
        "SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE session_id = ?1",
        rusqlite::params![session_id],
    )
}

/// 指定 UTC 日の開始以降に記録された payload の累積バイト数を返します。
pub(crate) fn day_event_bytes(conn: &Connection, day_start_ns: i64) -> Result<u64, StorageError> {
    sum_payload_bytes(
        conn,
        "SELECT COALESCE(SUM(OCTET_LENGTH(payload)), 0) FROM events WHERE wall_clock_ns >= ?1",
        rusqlite::params![day_start_ns],
    )
}

/// 壁時計を UTC 深夜の開始ナノ秒へ切り捨てます。
pub(crate) fn day_start_ns(time: SystemTime) -> Result<i64, StorageError> {
    let nanos = system_time_to_ns(time)?;
    Ok(nanos / NANOS_PER_DAY * NANOS_PER_DAY)
}

/// 容量上限を検査します。
pub(crate) fn enforce_limit(limit: LimitKind, actual: u64, max: u64) -> Result<(), StorageError> {
    if actual > max {
        return Err(StorageError::LimitExceeded { limit, actual, max });
    }
    Ok(())
}

fn sum_payload_bytes<P: rusqlite::Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<u64, StorageError> {
    let total: i64 = conn.query_row(sql, params, |row| row.get(0))?;
    u64::try_from(total).map_err(|_| StorageError::OutOfRange("event payload byte total"))
}
