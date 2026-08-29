//! ダウンサンプリング済みメトリクスを永続化します。
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "single-writer integration lands in the next storage task"
    )
)]

use event_bus::{BucketKey, UsageBucket};
use rusqlite::{Connection, params};

use crate::StorageError;

/// バケット群を一つのトランザクションで加算保存します。
pub fn upsert_buckets(conn: &Connection, buckets: &[UsageBucket]) -> Result<(), StorageError> {
    let transaction = conn.unchecked_transaction()?;
    for bucket in buckets {
        let values = bucket_values(bucket)?;
        transaction.execute(
            "INSERT INTO downsampled_metrics \
             (window_start, provider, model, input_tokens, output_tokens, cache_read_tokens, \
              cache_write_tokens, cache_hits, cache_misses, request_count) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
             ON CONFLICT(window_start, provider, model) DO UPDATE SET \
             input_tokens = input_tokens + excluded.input_tokens, \
             output_tokens = output_tokens + excluded.output_tokens, \
             cache_read_tokens = cache_read_tokens + excluded.cache_read_tokens, \
             cache_write_tokens = cache_write_tokens + excluded.cache_write_tokens, \
             cache_hits = cache_hits + excluded.cache_hits, \
             cache_misses = cache_misses + excluded.cache_misses, \
             request_count = request_count + excluded.request_count",
            params![
                values[0],
                &bucket.key.provider,
                &bucket.key.model,
                values[1],
                values[2],
                values[3],
                values[4],
                values[5],
                values[6],
                values[7],
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// inclusive な window_start 範囲をキー順で返します。
pub fn list_range(
    conn: &Connection,
    from_window_start: u64,
    to_window_start: u64,
) -> Result<Vec<UsageBucket>, StorageError> {
    let from = to_i64(from_window_start, "metrics range start")?;
    let to = to_i64(to_window_start, "metrics range end")?;
    let mut statement = conn.prepare(
        "SELECT window_start, provider, model, input_tokens, output_tokens, cache_read_tokens, \
         cache_write_tokens, cache_hits, cache_misses, request_count \
         FROM downsampled_metrics WHERE window_start >= ?1 AND window_start <= ?2 \
         ORDER BY window_start ASC, provider ASC, model ASC",
    )?;
    let mut rows = statement.query(params![from, to])?;
    let mut buckets = Vec::new();
    while let Some(row) = rows.next()? {
        buckets.push(UsageBucket {
            key: BucketKey {
                window_start: from_i64(row.get(0)?, "metrics window start")?,
                provider: row.get(1)?,
                model: row.get(2)?,
            },
            input_tokens: from_i64(row.get(3)?, "input tokens")?,
            output_tokens: from_i64(row.get(4)?, "output tokens")?,
            cache_read_tokens: from_i64(row.get(5)?, "cache read tokens")?,
            cache_write_tokens: from_i64(row.get(6)?, "cache write tokens")?,
            cache_hits: from_i64(row.get(7)?, "cache hits")?,
            cache_misses: from_i64(row.get(8)?, "cache misses")?,
            request_count: from_i64(row.get(9)?, "request count")?,
        });
    }
    Ok(buckets)
}

fn bucket_values(bucket: &UsageBucket) -> Result<[i64; 8], StorageError> {
    Ok([
        to_i64(bucket.key.window_start, "metrics window start")?,
        to_i64(bucket.input_tokens, "input tokens")?,
        to_i64(bucket.output_tokens, "output tokens")?,
        to_i64(bucket.cache_read_tokens, "cache read tokens")?,
        to_i64(bucket.cache_write_tokens, "cache write tokens")?,
        to_i64(bucket.cache_hits, "cache hits")?,
        to_i64(bucket.cache_misses, "cache misses")?,
        to_i64(bucket.request_count, "request count")?,
    ])
}

fn to_i64(value: u64, name: &'static str) -> Result<i64, StorageError> {
    i64::try_from(value).map_err(|_| StorageError::OutOfRange(name))
}

fn from_i64(value: i64, name: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::OutOfRange(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    fn bucket(window_start: u64, provider: &str, model: &str, value: u64) -> UsageBucket {
        UsageBucket {
            key: BucketKey {
                window_start,
                provider: provider.into(),
                model: model.into(),
            },
            input_tokens: value,
            output_tokens: value,
            cache_read_tokens: value,
            cache_write_tokens: value,
            cache_hits: value,
            cache_misses: value,
            request_count: value,
        }
    }

    #[test]
    fn upsert_new_bucket_then_list_returns_it() {
        // Given: 空の DB と一つの usage バケット
        let database = Database::open_in_memory().unwrap();
        let expected = bucket(60, "p", "m", 1);

        // When: バケットを保存して同範囲を取得する
        upsert_buckets(&database.conn, std::slice::from_ref(&expected)).unwrap();
        let actual = list_range(&database.conn, 60, 60).unwrap();

        // Then: 全フィールドが一致する
        assert_eq!(actual, [expected]);
    }

    #[test]
    fn upsert_same_key_adds_every_counter() {
        // Given: 同一キーを持つ二つのバケット
        let database = Database::open_in_memory().unwrap();
        let first = bucket(60, "p", "m", 2);
        let second = bucket(60, "p", "m", 3);

        // When: 二回に分けて upsert する
        upsert_buckets(&database.conn, &[first]).unwrap();
        upsert_buckets(&database.conn, &[second]).unwrap();

        // Then: すべてのカウンターが加算される
        assert_eq!(
            list_range(&database.conn, 60, 60).unwrap(),
            [bucket(60, "p", "m", 5)]
        );
    }

    #[test]
    fn list_range_is_inclusive_and_key_ordered() {
        // Given: 範囲外を含む順不同のバケット
        let database = Database::open_in_memory().unwrap();
        upsert_buckets(
            &database.conn,
            &[
                bucket(120, "z", "m", 1),
                bucket(60, "b", "m", 1),
                bucket(60, "a", "m", 1),
                bucket(180, "a", "m", 1),
            ],
        )
        .unwrap();

        // When: 60 から 120 を inclusive に取得する
        let actual = list_range(&database.conn, 60, 120).unwrap();

        // Then: 範囲内だけが window/provider/model 順になる
        assert_eq!(
            actual,
            [
                bucket(60, "a", "m", 1),
                bucket(60, "b", "m", 1),
                bucket(120, "z", "m", 1)
            ]
        );
    }
}
