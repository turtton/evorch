//! A finished database read as an immutable snapshot, with a bound on the rows
//! read and failures that tell the caller what kind they were.

use storage::usage::{UsageRequestRecord, UsageStatus};
use storage::{Database, FailureClass, Storage, StorageConfig};
use tempfile::TempDir;

fn request(id: &str) -> UsageRequestRecord {
    UsageRequestRecord {
        usage_reported: true,
        request_id: id.into(),
        at_ns: 1,
        provider: "openai".into(),
        profile: Some("main".into()),
        model: "gpt-test".into(),
        run_id: Some("run-1".into()),
        parent_run_id: None,
        role: Some("worker".into()),
        purpose: Some("agent".into()),
        status: UsageStatus::Ok,
        failure: None,
        finish_reason: Some("stop".into()),
        input_tokens: 10,
        output_tokens: 1,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        reasoning_tokens: None,
        ttft_ms: None,
        duration_ms: 1,
        cost_usd: None,
        service_tier: None,
    }
}

fn finished_database(dir: &TempDir, requests: usize) -> std::path::PathBuf {
    let path = dir.path().join("run.db");
    let storage = Storage::open(StorageConfig {
        db_path: path.clone(),
        ..StorageConfig::default()
    })
    .unwrap();
    storage
        .handle()
        .record_usage_requests((0..requests).map(|n| request(&format!("r{n}"))).collect())
        .unwrap();
    drop(storage);
    path
}

#[test]
fn an_immutable_snapshot_reads_a_bounded_number_of_rows_and_leaves_no_files() {
    // Given: a closed database with three usage rows.
    let dir = TempDir::new().unwrap();
    let path = finished_database(&dir, 3);
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();

    // When: it is opened as an immutable snapshot and read with bounds.
    let database = Database::open_read_only_immutable(&path).unwrap();
    let (all, all_more) = database
        .usage_requests_between_limited(i64::MIN, i64::MAX, 3)
        .unwrap();
    let (some, more) = database
        .usage_requests_between_limited(i64::MIN, i64::MAX, 2)
        .unwrap();
    drop(database);

    // Then: the bound is exact, says when rows were left, and nothing was created.
    assert_eq!((all.len(), all_more), (3, false));
    assert_eq!((some.len(), more), (2, true));
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after);
}

#[test]
fn open_failures_are_classified() {
    // Given: a database, a file that is not one, and one claiming a newer or older schema.
    let dir = TempDir::new().unwrap();
    let path = finished_database(&dir, 1);
    let good = std::fs::read(&path).unwrap();
    let class = |bytes: &[u8]| {
        let file = dir.path().join("probe.db");
        std::fs::write(&file, bytes).unwrap();
        Database::open_read_only_immutable(&file)
            .err()
            .map(|e| e.class())
    };
    let versioned = |version: u32| {
        let mut bytes = good.clone();
        bytes[60..64].copy_from_slice(&version.to_be_bytes());
        bytes
    };

    // Then: each is told apart.
    assert_eq!(class(&good), None);
    assert_eq!(class(&[b'x'; 200]), Some(FailureClass::Corrupt));
    assert_eq!(
        class(&versioned(9_999)),
        Some(FailureClass::SchemaUnsupported)
    );
    assert_eq!(class(&versioned(1)), Some(FailureClass::SchemaUnsupported));
}
