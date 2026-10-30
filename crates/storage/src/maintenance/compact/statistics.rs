use std::{collections::BTreeMap, path::Path};

use rusqlite::{Connection, params, types::ValueRef};

use super::{CompactOptions, checkpoint};
use crate::{StorageError, codec, repo::event::encode_payload};

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct EventCount {
    pub rows: u64,
    pub payload_bytes: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Snapshot {
    pub file_bytes: crate::db::FileSizes,
    pub events: BTreeMap<String, EventCount>,
    pub run_context_values: u64,
    pub run_context_stored_bytes: u64,
    pub unbackfilled_rows: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Stage {
    pub input_rows: u64,
    pub output_rows: u64,
    pub payload_bytes_before: u64,
    pub payload_bytes_after: u64,
    pub removed_by_kind: BTreeMap<String, EventCount>,
}

impl Stage {
    pub(super) fn deleted(&mut self, kind: &str, subkind: &str, bytes: u64) {
        self.input_rows += 1;
        self.payload_bytes_before += bytes;
        let count = self.removed_by_kind.entry(key(kind, subkind)).or_default();
        count.rows += 1;
        count.payload_bytes += bytes;
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct CompressionSample {
    pub eligible_values: u64,
    pub eligible_bytes: u64,
    pub sampled_values: u64,
    pub sampled_plain_bytes: u64,
    pub sampled_stored_bytes: u64,
    pub sampled_compressed_values: u64,
    pub estimated_compressed_values: u64,
    pub estimated_saved_bytes: u64,
}

impl CompressionSample {
    fn add(&mut self, plain_bytes: usize, stored_bytes: Option<usize>) {
        self.eligible_values += 1;
        self.eligible_bytes += plain_bytes as u64;
        if let Some(stored) = stored_bytes {
            self.sampled_values += 1;
            self.sampled_plain_bytes += plain_bytes as u64;
            self.sampled_stored_bytes += stored as u64;
            self.sampled_compressed_values += u64::from(stored < plain_bytes);
        }
    }

    fn finish(&mut self) {
        if self.sampled_values > 0 {
            self.estimated_compressed_values =
                (u128::from(self.eligible_values) * u128::from(self.sampled_compressed_values)
                    / u128::from(self.sampled_values)) as u64;
        }
        if self.sampled_plain_bytes > 0 {
            let saved = self
                .sampled_plain_bytes
                .saturating_sub(self.sampled_stored_bytes);
            self.estimated_saved_bytes = (u128::from(self.eligible_bytes) * u128::from(saved)
                / u128::from(self.sampled_plain_bytes))
                as u64;
        }
    }
}

pub(super) fn subkind(column: Option<String>, payload: ValueRef<'_>) -> String {
    column.unwrap_or_else(|| {
        codec::decode(payload)
            .ok()
            .and_then(|text| serde_json::from_str::<event_bus::EventKind>(&text).ok())
            .map_or_else(String::new, |kind| kind.subkind_name().to_owned())
    })
}

pub(super) fn key(kind: &str, subkind: &str) -> String {
    format!("{kind}/{subkind}")
}

pub(super) fn snapshot(conn: &Connection, path: &Path) -> Result<Snapshot, StorageError> {
    let mut counts = BTreeMap::<String, EventCount>::new();
    // Only legacy NULL rows need decoding; compression does not affect counts.
    let mut stmt = conn.prepare("SELECT kind, subkind, COUNT(*), SUM(OCTET_LENGTH(payload)) FROM events WHERE subkind IS NOT NULL GROUP BY kind, subkind")?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        counts.insert(
            key(&row.get::<_, String>(0)?, &row.get::<_, String>(1)?),
            EventCount {
                rows: row.get(2)?,
                payload_bytes: row.get(3)?,
            },
        );
    }
    let mut stmt = conn
        .prepare("SELECT kind, payload, OCTET_LENGTH(payload) FROM events WHERE subkind IS NULL")?;
    let mut rows = stmt.query([])?;
    let mut unbackfilled_rows = 0;
    while let Some(row) = rows.next()? {
        unbackfilled_rows += 1;
        let kind: String = row.get(0)?;
        let payload = row.get_ref(1)?;
        let count = counts
            .entry(key(&kind, &subkind(None, payload)))
            .or_default();
        count.rows += 1;
        count.payload_bytes += row.get::<_, u64>(2)?;
    }
    let (run_context_values, run_context_stored_bytes) = conn.query_row(
        "SELECT COUNT(*)*2, COALESCE(SUM(OCTET_LENGTH(messages_json)+OCTET_LENGTH(checkpoints_json)),0) FROM run_contexts",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(Snapshot {
        file_bytes: crate::db::file_sizes(path)?,
        events: counts,
        run_context_values,
        run_context_stored_bytes,
        unbackfilled_rows,
    })
}

pub(super) fn subtract_deleted(counts: &mut BTreeMap<String, EventCount>, stage: &Stage) {
    for (key, removed) in &stage.removed_by_kind {
        if let Some(count) = counts.get_mut(key) {
            count.rows -= removed.rows;
            count.payload_bytes = count.payload_bytes.saturating_sub(removed.payload_bytes);
        }
    }
    counts.retain(|_, count| count.rows != 0);
}

pub(super) fn compress_events(
    conn: &Connection,
    options: &CompactOptions,
) -> Result<(CompressionSample, u64), StorageError> {
    let mut sample = CompressionSample::default();
    let mut converted = 0;
    let mut after = i64::MIN;
    loop {
        let batch = {
            let mut stmt = conn.prepare(
                "SELECT id,kind,subkind,payload FROM events WHERE id>?1
                 AND kind IN ('Tool','Message','compaction') AND typeof(payload)='text'
                 AND OCTET_LENGTH(payload)>=2048 ORDER BY id LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![after, options.batch_rows as i64])?;
            let mut batch = Vec::new();
            let mut batch_bytes = 0;
            while let Some(row) = rows.next()? {
                let payload = row.get::<_, String>(3)?;
                batch_bytes += payload.len();
                batch.push((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    subkind(row.get(2)?, row.get_ref(3)?),
                    payload,
                ));
                if batch_bytes >= 16 * 1024 * 1024 {
                    break;
                }
            }
            batch
        };
        if batch.is_empty() {
            break;
        }
        let tx = options
            .apply
            .then(|| conn.unchecked_transaction())
            .transpose()?;
        for (id, kind, subkind, payload) in batch {
            after = id;
            // Use the exact writer policy, not a parallel allowlist.
            let should_sample = sample.sampled_values < 1_000;
            let encoded =
                (options.apply || should_sample).then(|| encode_payload(&kind, &subkind, &payload));
            let eligible = crate::repo::event::compressible_payload(&kind, &subkind);
            if !eligible {
                continue;
            }
            sample.add(
                payload.len(),
                should_sample.then(|| encoded.as_ref().unwrap().stored_len()),
            );
            if let (Some(tx), Some(codec::Encoded::Compressed(_))) = (&tx, &encoded) {
                tx.execute(
                    "UPDATE events SET payload=?2 WHERE id=?1",
                    params![id, encoded.as_ref().unwrap()],
                )?;
                converted += 1;
            }
        }
        if let Some(tx) = tx {
            tx.commit()?;
            checkpoint(conn)?;
        }
    }
    sample.finish();
    Ok((sample, converted))
}

pub(super) fn compress_contexts(
    conn: &Connection,
    options: &CompactOptions,
) -> Result<(CompressionSample, u64), StorageError> {
    let mut sample = CompressionSample::default();
    let mut converted = 0;
    // TEXT keys also keep the cursor stable for WITHOUT ROWID stores.
    let mut after: Option<String> = None;
    loop {
        let batch = {
            let mut stmt = conn.prepare(
                "SELECT run_id,messages_json,checkpoints_json FROM run_contexts
                 WHERE (?1 IS NULL OR run_id>?1) ORDER BY run_id LIMIT ?2",
            )?;
            let mut rows = stmt.query(params![after, options.batch_rows as i64])?;
            let mut batch = Vec::new();
            let mut batch_bytes = 0;
            while let Some(row) = rows.next()? {
                let plain = |index| -> rusqlite::Result<Option<String>> {
                    match row.get_ref(index)? {
                        ValueRef::Text(text) if text.len() >= codec::MIN_COMPRESSED_BYTES => {
                            row.get(index).map(Some)
                        }
                        _ => Ok(None),
                    }
                };
                let messages = plain(1)?;
                let checkpoints = plain(2)?;
                batch_bytes += messages.as_ref().map_or(0, String::len)
                    + checkpoints.as_ref().map_or(0, String::len);
                batch.push((row.get::<_, String>(0)?, messages, checkpoints));
                if batch_bytes >= 16 * 1024 * 1024 {
                    break;
                }
            }
            batch
        };
        if batch.is_empty() {
            break;
        }
        let tx = options
            .apply
            .then(|| conn.unchecked_transaction())
            .transpose()?;
        for (run, messages, checkpoints) in batch {
            after = Some(run.clone());
            for (column, length, plain) in [
                ("messages_json", "messages_len", messages),
                ("checkpoints_json", "checkpoints_len", checkpoints),
            ] {
                let Some(plain) = plain else { continue };
                let should_sample = sample.sampled_values < 1_000;
                let encoded = (options.apply || should_sample).then(|| codec::encode(&plain));
                sample.add(
                    plain.len(),
                    should_sample.then(|| encoded.as_ref().unwrap().stored_len()),
                );
                if let (Some(tx), Some(codec::Encoded::Compressed(_))) = (&tx, &encoded) {
                    tx.execute(
                        &format!("UPDATE run_contexts SET {column}=?2,{length}=?3 WHERE run_id=?1"),
                        params![run, encoded.as_ref().unwrap(), plain.len() as i64],
                    )?;
                    converted += 1;
                }
            }
        }
        if let Some(tx) = tx {
            tx.commit()?;
            checkpoint(conn)?;
        }
    }
    sample.finish();
    Ok((sample, converted))
}
