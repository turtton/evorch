use std::path::Path;
use std::sync::Arc;

use super::super::executor::{ExecutorConfig, FILES_RETRY_LIMIT, ReclaimExecutor, ReclaimWorker};
use super::{World, policy, ran};
use crate::artifacts::{ArtifactRecord, ArtifactStore, CaptureRequest};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-test-image";

struct Running {
    world: World,
    storage: storage::Storage,
    artifacts: Arc<ArtifactStore>,
    deleted: ArtifactRecord,
    survivor: ArtifactRecord,
}

impl Running {
    fn config(&self) -> ExecutorConfig {
        let mut config = ExecutorConfig::new(
            self.storage.handle(),
            self.world.storage_config(),
            Some(Arc::clone(&self.artifacts)),
        );
        config.lease_retry_cycles = 1;
        config
    }

    fn summary(&self) -> storage::thread_retention::ReclaimSummary {
        self.world.database().reclaim_summary().unwrap()
    }
}

fn capture(store: &ArtifactStore, workspace: &Path, run: &str, thread: &str) -> ArtifactRecord {
    store
        .capture(
            workspace,
            None,
            Path::new("mock.png"),
            CaptureRequest {
                title: "t",
                caption: None,
                run_id: run.into(),
                root_run_id: run.into(),
                thread_id: Some(thread.into()),
            },
        )
        .unwrap()
}

/// `T` confirmed by a startup phase (its rows still to reclaim), one artifact of `T` and
/// one of a surviving thread, and a storage writer running on the database.
fn running() -> Running {
    let world = World::new();
    world.archived_thread("T", "rt");
    let report = world.phase(&policy(), &world.input(&["T"], &["S"]));
    assert_eq!(ran(&report).confirmed.len(), 1);
    let workspace = world.dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("mock.png"), PNG).unwrap();
    let artifacts = Arc::new(ArtifactStore::new(world.dir.path().join("artifacts")).unwrap());
    let deleted = capture(&artifacts, &workspace, "rt", "T");
    let survivor = capture(&artifacts, &workspace, "rs", "S");
    let storage = storage::Storage::open(world.storage_config()).unwrap();
    Running {
        world,
        storage,
        artifacts,
        deleted,
        survivor,
    }
}

// Given: another process holds the reclaim lease / When: a cycle runs
// Then: this process reclaims nothing and leaves the job and files alone; once the
// holder is gone the next cycle takes over.
#[test]
fn only_the_reclaim_lease_holder_reclaims() {
    let run = running();
    let other = storage::reclaim_lease(&run.world.db_path).unwrap().unwrap();
    let mut worker = ReclaimWorker::new(run.config());

    let idle = worker.run_once();

    assert!(!idle.holds_lease);
    assert_eq!(idle.rows_reclaimed, 0);
    assert_eq!(run.summary().jobs_db_pending, 1);
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_ok());

    drop(other);
    assert!(worker.run_once().holds_lease);
}

// Given: a confirmed family with artifacts, next to a survivor's / When: cycles run
// Then: its rows are reclaimed, its meta is removed, the job is done, and the survivor's
// meta and every blob remain (blobs are collected by a later change).
#[test]
fn rows_then_meta_are_reclaimed_and_blobs_stay() {
    let run = running();
    let mut worker = ReclaimWorker::new(run.config());
    let mut cycles = 0;
    while run.summary().jobs_done == 0 {
        let cycle = worker.run_once();
        assert!(cycle.errors.is_empty(), "{cycle:?}");
        cycles += 1;
        assert!(cycles < 100, "reclaim must converge: {:?}", run.summary());
    }

    assert!(run.artifacts.load(&run.deleted.artifact_id).is_err());
    assert!(run.artifacts.load(&run.survivor.artifact_id).is_ok());
    assert!(run.artifacts.blob_path(&run.deleted).is_file());
    assert_eq!(
        run.world
            .count("SELECT COUNT(*) FROM run_contexts WHERE run_id = 'rt'"),
        0
    );
}

// Given: a meta file that cannot be read / When: cycles run
// Then: the family's files fail and are retried each cycle until the limit, after which
// the family is reported as exhausted and stays failed; the blob is untouched. A failure
// that goes away earlier is finished by the retry.
#[test]
fn unreadable_meta_is_retried_then_reported_at_the_limit() {
    let run = running();
    let corrupt = run
        .world
        .dir
        .path()
        .join("artifacts/meta/artifact-00000000000000000000000000000000.json");
    std::fs::write(&corrupt, b"{ broken").unwrap();
    let mut worker = ReclaimWorker::new(run.config());

    let mut exhausted = Vec::new();
    for _ in 0..(FILES_RETRY_LIMIT + 40) {
        exhausted = worker.run_once().files_exhausted;
        if !exhausted.is_empty() {
            break;
        }
    }

    assert_eq!(exhausted.len(), 1, "{:?}", run.summary());
    assert_eq!(run.summary().jobs_files_failed, 1);
    assert_eq!(run.summary().jobs_done, 0);
    assert!(run.artifacts.blob_path(&run.deleted).is_file());

    // A failure that is gone by the retry is finished by it.
    let run = running();
    let corrupt = run
        .world
        .dir
        .path()
        .join("artifacts/meta/artifact-00000000000000000000000000000000.json");
    std::fs::write(&corrupt, b"{ broken").unwrap();
    let mut worker = ReclaimWorker::new(run.config());
    while run.summary().jobs_files_failed == 0 {
        worker.run_once();
    }
    std::fs::remove_file(&corrupt).unwrap();
    while run.summary().jobs_done == 0 {
        worker.run_once();
    }
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_err());
}

// Given: nothing to reclaim / When: cycles run / Then: the database is not written
// (SQLite's data version, which changes on every commit of another connection, stays).
#[test]
fn an_idle_cycle_writes_nothing() {
    let world = World::new();
    let storage = storage::Storage::open(world.storage_config()).unwrap();
    let mut worker = ReclaimWorker::new(ExecutorConfig::new(
        storage.handle(),
        world.storage_config(),
        None,
    ));
    // The first cycle may finish the attribution backfill step.
    assert!(worker.run_once().holds_lease);
    let probe = world.conn();
    let version = |conn: &rusqlite::Connection| -> i64 {
        conn.query_row("PRAGMA data_version", [], |row| row.get(0))
            .unwrap()
    };
    let before = version(&probe);

    for _ in 0..3 {
        let cycle = worker.run_once();
        assert!(cycle.errors.is_empty());
    }

    assert_eq!(version(&probe), before);
}

// Given: a running executor thread / When: shut down
// Then: the first cycle ran, the thread is joined (no timing involved: shutdown wakes it)
// and the reclaim lease is free again.
#[test]
fn the_thread_runs_a_cycle_and_releases_the_lease_on_shutdown() {
    let run = running();
    let mut config = run.config();
    config.interval = std::time::Duration::from_secs(3_600);
    let executor = ReclaimExecutor::spawn(config).unwrap();

    let report = executor.wait_for_cycles(1);
    assert!(report.holds_lease && report.cycles >= 1);

    executor.shutdown();
    assert!(
        storage::reclaim_lease(&run.world.db_path)
            .unwrap()
            .is_some()
    );
}
