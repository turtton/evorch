//! Synthetic WAL measurements only. Elapsed time is a report, never a CI assertion.
use super::*;
use crate::body_ref::BodyToken;
use crate::{Database, StorageConfig};
use std::time::Instant;

#[test]
#[ignore = "manual GC budget measurement on synthetic file-backed databases"]
fn tool_body_retention_measurements() {
    for limit in [64, 256] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("measurement.db");
        let db = Database::open(&StorageConfig {
            db_path: path.clone(),
            ..Default::default()
        })
        .unwrap();
        let conn = &db.conn;
        conn.execute_batch("PRAGMA wal_autocheckpoint=0;").unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        // Mixed dedicated, shared/re-referenced and stale candidates, with enough
        // stored bytes to exercise deletion as well as reference probes.
        for n in 0..1024 {
            let token = BodyToken::of(&format!("measurement-{n}"));
            tx.execute("INSERT INTO content_blobs(hash, byte_len, codec, data) VALUES (?1, 16384, 0, zeroblob(16384))", [&token.hash[..]]).unwrap();
            let id = tx.last_insert_rowid();
            tx.execute(
                "INSERT INTO content_blob_gc_queue(blob_id) VALUES (?1)",
                [id],
            )
            .unwrap();
        }
        tx.execute_batch("CREATE TABLE content_blob_refs_measurement(owner_id TEXT, blob_id INTEGER REFERENCES content_blobs(id)); CREATE INDEX measurement_blob ON content_blob_refs_measurement(blob_id); INSERT INTO content_blob_refs_measurement SELECT 'shared', id FROM content_blobs WHERE id % 4 = 0; INSERT INTO content_blob_gc_queue VALUES (0),(1000000);").unwrap();
        tx.commit().unwrap();
        conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .unwrap();
        let wal = path.with_extension("db-wal");
        let before = std::fs::metadata(&wal).unwrap().len();
        let start = Instant::now();
        let progress = gc_step_with_limit(conn, limit).unwrap();
        let elapsed = start.elapsed();
        let growth = std::fs::metadata(&wal)
            .unwrap()
            .len()
            .saturating_sub(before);
        println!(
            "GC candidates={limit} writer_ms={:.3} wal_growth_bytes={growth} progress={progress:?}",
            elapsed.as_secs_f64() * 1000.0
        );
        assert_eq!(progress.candidates, limit);
        assert_eq!(
            conn.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                .unwrap(),
            "ok"
        );
    }
}
