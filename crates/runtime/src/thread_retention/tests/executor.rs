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
    running_with(&["T"])
}

/// Like [`running`] with one family (and one artifact) per name in `threads`; `deleted`
/// is the first one's artifact.
fn running_with(threads: &[&str]) -> Running {
    running_configured(threads, false)
}

/// [`running`] whose captures record their references in the database, the way a host
/// that collects blobs wires its store.
fn running_with_refs() -> Running {
    running_configured(&["T"], true)
}

fn running_configured(threads: &[&str], record_refs: bool) -> Running {
    let world = World::new();
    for thread in threads {
        world.archived_thread(thread, &format!("r-{thread}"));
    }
    let report = world.phase(&policy(), &world.input(threads, &["S"]));
    assert_eq!(ran(&report).confirmed.len(), threads.len());
    let workspace = world.dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("mock.png"), PNG).unwrap();
    let storage = storage::Storage::open(world.storage_config()).unwrap();
    let root = world.dir.path().join("artifacts");
    let store = if record_refs {
        crate::artifacts::store_with_refs(root, storage.handle()).unwrap()
    } else {
        ArtifactStore::new(root).unwrap()
    };
    let artifacts = Arc::new(store);
    let mut records = threads
        .iter()
        .map(|thread| capture(&artifacts, &workspace, &format!("r-{thread}"), thread))
        .collect::<Vec<_>>();
    let deleted = records.remove(0);
    let survivor = capture(&artifacts, &workspace, "rs", "S");
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
// meta and every blob remain (blobs are collected by the next startup's stage 7).
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

fn reference_rows(run: &Running) -> Vec<String> {
    run.world
        .database()
        .artifact_refs_after(None, 100)
        .unwrap()
        .into_iter()
        .map(|reference| reference.artifact_id)
        .collect()
}

// Given: captures that recorded their references / When: the family's meta is reclaimed
// Then: the reference of the deleted artifact goes with its file and the survivor keeps
// its own.
#[test]
fn reclaiming_meta_removes_its_references() {
    let run = running_with_refs();
    assert_eq!(reference_rows(&run).len(), 2);
    let mut worker = ReclaimWorker::new(run.config());
    let mut cycles = 0;
    while run.summary().jobs_done == 0 {
        let cycle = worker.run_once();
        assert!(cycle.errors.is_empty(), "{cycle:?}");
        cycles += 1;
        assert!(cycles < 100, "reclaim must converge: {:?}", run.summary());
    }

    assert_eq!(
        reference_rows(&run),
        std::slice::from_ref(&run.survivor.artifact_id)
    );
}

// Given: deleting references fails / When: the family's meta is reclaimed
// Then: the failure is reported but the family still finishes (a stale row only keeps a
// blob), and the next startup's orphan check drops the row.
#[test]
fn a_reference_that_cannot_be_removed_does_not_change_the_files_outcome() {
    let run = running_with_refs();
    run.world
        .conn()
        .execute_batch(
            "CREATE TRIGGER refuse_ref_delete BEFORE DELETE ON artifact_refs \
             BEGIN SELECT RAISE(ABORT, 'refused'); END;",
        )
        .unwrap();
    let mut worker = ReclaimWorker::new(run.config());
    let mut errors = Vec::new();
    let mut cycles = 0;
    while run.summary().jobs_done == 0 {
        errors.extend(worker.run_once().errors);
        cycles += 1;
        assert!(cycles < 100, "reclaim must converge: {:?}", run.summary());
    }

    assert!(
        errors
            .iter()
            .any(|error| error.contains("artifact references")),
        "{errors:?}"
    );
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_err());
    assert_eq!(reference_rows(&run).len(), 2, "the stale row remains");
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

fn corrupt_meta(run: &Running) -> std::path::PathBuf {
    let path = run
        .world
        .dir
        .path()
        .join("artifacts/meta/artifact-00000000000000000000000000000000.json");
    std::fs::write(&path, b"{ broken").unwrap();
    path
}

const FAMILIES: [&str; 5] = ["T0", "T1", "T2", "T3", "T4"];

// Given: five failing families, a page of two and a retry limit of one / When: cycles run
// Then: every family gets its attempt and is reported exhausted: families at the head of
// the listing that are exhausted never keep later ones from being reached.
#[test]
fn exhausted_families_do_not_starve_the_ones_behind_them() {
    let run = running_with(&FAMILIES);
    corrupt_meta(&run);
    let mut config = run.config();
    config.jobs_per_page = 2;
    config.files_retry_limit = 1;
    let mut worker = ReclaimWorker::new(config);

    let mut exhausted = 0;
    for _ in 0..40 {
        exhausted = worker.run_once().files_exhausted_total;
        if exhausted == FAMILIES.len() {
            break;
        }
    }

    assert_eq!(exhausted, FAMILIES.len(), "{:?}", run.summary());
    let summary = run.summary();
    assert_eq!((summary.jobs_files_failed, summary.jobs_done), (5, 0));
    assert_eq!(summary.jobs_db_done, 0, "none is left waiting unattempted");
}

// Given: five families failing for a while, a page of two and a generous retry limit
// When: the obstacle goes away / Then: all five finish, whatever page they are on.
#[test]
fn failures_are_retried_across_pages_until_every_family_is_done() {
    let run = running_with(&FAMILIES);
    let corrupt = corrupt_meta(&run);
    let mut config = run.config();
    config.jobs_per_page = 2;
    config.files_retry_limit = 50;
    let mut worker = ReclaimWorker::new(config);
    for _ in 0..100 {
        worker.run_once();
        if run.summary().jobs_files_failed == FAMILIES.len() as u64 {
            break;
        }
    }
    assert_eq!(run.summary().jobs_files_failed, 5);

    std::fs::remove_file(corrupt).unwrap();
    for _ in 0..100 {
        worker.run_once();
        if run.summary().jobs_done == FAMILIES.len() as u64 {
            break;
        }
    }
    assert_eq!(run.summary().jobs_done, 5, "{:?}", run.summary());
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_err());
    assert!(run.artifacts.load(&run.survivor.artifact_id).is_ok());
}

// Given: a meta directory that cannot be listed / When: cycles run
// Then: the family fails (never pending forever, never done), the error is reported, and
// when the directory is back the next round finishes it.
#[test]
fn a_listing_failure_is_a_failed_round_and_recovers() {
    let run = running();
    let meta = run.world.dir.path().join("artifacts/meta");
    let saved = run.world.dir.path().join("artifacts/meta-saved");
    std::fs::rename(&meta, &saved).unwrap();
    std::fs::write(&meta, b"not a directory").unwrap();
    let mut worker = ReclaimWorker::new(run.config());

    let mut errors = Vec::new();
    for _ in 0..100 {
        errors = worker.run_once().errors;
        if run.summary().jobs_files_failed == 1 {
            break;
        }
    }
    assert_eq!(run.summary().jobs_files_failed, 1, "{:?}", run.summary());
    assert_eq!(run.summary().jobs_files_pending, 0, "not pending forever");
    assert!(
        errors.iter().any(|error| error.contains("failure")),
        "{errors:?}"
    );

    std::fs::remove_file(&meta).unwrap();
    std::fs::rename(&saved, &meta).unwrap();
    for _ in 0..100 {
        worker.run_once();
        if run.summary().jobs_done == 1 {
            break;
        }
    }
    assert_eq!(run.summary().jobs_done, 1);
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_err());
}

// Given: an executor without an artifact store / When: cycles run
// Then: the rows are reclaimed but the files are left unfinished (not marked done).
#[test]
fn without_an_artifact_store_the_files_are_never_marked_done() {
    let run = running();
    let mut config = run.config();
    config.artifacts = None;
    let mut worker = ReclaimWorker::new(config);
    for _ in 0..50 {
        worker.run_once();
        if run.summary().jobs_db_done == 1 {
            break;
        }
    }
    for _ in 0..5 {
        worker.run_once();
    }
    let summary = run.summary();
    assert_eq!((summary.jobs_db_done, summary.jobs_done), (1, 0));
    assert!(run.artifacts.load(&run.deleted.artifact_id).is_ok());
}
