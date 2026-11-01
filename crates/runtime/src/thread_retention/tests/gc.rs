//! Stage 7 (artifact blob collection): which blobs survive, and that no error is ever
//! read as "unreferenced". Files live in a temporary directory; nothing waits on time.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;

use super::super::executor::{ExecutorConfig, ReclaimWorker};
use super::super::startup::{
    SkipReason, Stage, StartupBudget, StartupOutcome, StartupPaths, StartupProgress,
    run_exclusive_phase_with_progress,
};
use super::{NOW_NS, World, policy, ran};
use crate::artifacts::{
    ArtifactStore, CaptureRequest, GcBudget, GcReport, GcStop, collect_blobs, store_with_refs,
};

fn sha(n: u8) -> String {
    format!("{n:02x}").repeat(32)
}

fn id(n: u32) -> String {
    format!("artifact-{n:032x}")
}

/// A world with an artifact store directory next to it.
struct Store {
    world: World,
    dir: PathBuf,
}

impl Store {
    fn new() -> Self {
        let world = World::new();
        let dir = world.dir.path().join("artifacts");
        std::fs::create_dir_all(dir.join("meta")).unwrap();
        std::fs::create_dir_all(dir.join("blobs")).unwrap();
        Self { world, dir }
    }

    fn blob(&self, sha: &str) -> PathBuf {
        self.dir.join("blobs").join(format!("{sha}.png"))
    }

    fn meta(&self, n: u32) -> PathBuf {
        self.dir.join("meta").join(format!("{}.json", id(n)))
    }

    fn add_blob(&self, sha: &str) {
        std::fs::write(self.blob(sha), b"blob").unwrap();
    }

    fn meta_json(n: u32, sha: &str, thread: &str) -> String {
        json!({
            "artifact_id": id(n),
            "sha256": sha,
            "media_type": "image/png",
            "extension": "png",
            "title": "t",
            "source_path": "/x",
            "byte_len": 4,
            "run_id": "r",
            "root_run_id": "r",
            "thread_id": thread,
            // A field a future version may add.
            "future_field": [1, 2, 3],
        })
        .to_string()
    }

    /// The meta file of artifact `n`, as a capture writes it.
    fn add_meta(&self, n: u32, sha: &str, thread: &str) {
        std::fs::write(self.meta(n), Self::meta_json(n, sha, thread)).unwrap();
    }

    fn collect_with(&self, budget: &GcBudget) -> GcReport {
        collect_blobs(&self.world.database(), &self.dir, budget, NOW_NS)
    }

    fn collect(&self) -> GcReport {
        self.collect_with(&GcBudget::default())
    }

    fn refs(&self) -> Vec<String> {
        self.world
            .database()
            .artifact_refs_after(None, 10_000)
            .unwrap()
            .into_iter()
            .map(|reference| reference.artifact_id)
            .collect()
    }

    fn state(&self) -> storage::artifact_refs::GcState {
        self.world.database().gc_state().unwrap()
    }

    fn paths(&self) -> StartupPaths {
        StartupPaths {
            artifacts_dir: Some(self.dir.clone()),
            ..self.world.paths()
        }
    }

    /// One startup phase with the artifacts directory configured.
    fn phase(&self, budget: &StartupBudget) -> crate::thread_retention::StartupReport {
        let leases = self.world.exclusive();
        run_exclusive_phase_with_progress(
            &leases,
            &self.paths(),
            &policy(),
            &self.world.input(&[], &[]),
            budget,
            NOW_NS,
            &mut |_| {},
        )
        .unwrap()
    }
}

fn assert_nothing_unlinked(report: &GcReport) {
    assert_eq!(report.blobs_unlinked, 0, "{report:?}");
}

// Given: referenced, shared and unreferenced blobs / When: the collection runs twice
// Then: the first run gives the metas their references and unlinks only the blob nothing
// names (tolerating unknown meta fields); the second run changes nothing.
#[test]
fn only_unreferenced_blobs_are_unlinked_and_a_second_run_changes_nothing() {
    let store = Store::new();
    store.add_blob(&sha(1));
    store.add_blob(&sha(2));
    store.add_blob(&sha(9));
    store.add_meta(1, &sha(1), "T1");
    store.add_meta(2, &sha(2), "T2");

    let first = store.collect();

    assert!(first.refs_complete && first.stops.is_empty(), "{first:?}");
    assert_eq!((first.refs_added, first.meta_scanned), (2, 2));
    assert_eq!((first.blobs_unlinked, first.bytes_freed), (1, 4));
    assert!(store.blob(&sha(1)).is_file() && store.blob(&sha(2)).is_file());
    assert!(!store.blob(&sha(9)).exists());
    assert_eq!(store.refs(), [id(1), id(2)]);
    assert!(store.state().refs_complete);

    let second = store.collect();

    assert!(second.refs_complete, "{second:?}");
    assert_eq!((second.refs_added, second.blobs_unlinked), (0, 0));
    assert_eq!(second.meta_scanned, 0, "known metas are not read again");
    assert!(store.blob(&sha(1)).is_file() && store.blob(&sha(2)).is_file());
}

// Given: two metas of different threads naming one blob / When: one thread's meta and
// reference are removed, then the other's / Then: the blob outlives the first removal
// and goes after the second.
#[test]
fn a_shared_blob_is_kept_until_its_last_reference_is_gone() {
    let store = Store::new();
    store.add_blob(&sha(1));
    store.add_meta(1, &sha(1), "T1");
    store.add_meta(2, &sha(1), "T2");
    assert!(store.collect().refs_complete);

    // The first thread is deleted: the file goes, and its reference with it.
    std::fs::remove_file(store.meta(1)).unwrap();
    let storage = storage::Storage::open(store.world.storage_config()).unwrap();
    storage.handle().remove_artifact_refs(&[id(1)]).unwrap();
    drop(storage);
    let after_first = store.collect();
    assert_nothing_unlinked(&after_first);
    assert!(store.blob(&sha(1)).is_file());
    assert_eq!(store.refs(), [id(2)]);

    std::fs::remove_file(store.meta(2)).unwrap();
    let after_second = store.collect();
    assert!(after_second.refs_complete, "{after_second:?}");
    assert_eq!(after_second.refs_removed_orphan, 1);
    assert_eq!(after_second.blobs_unlinked, 1);
    assert!(!store.blob(&sha(1)).exists());
    assert!(store.refs().is_empty());
}

// Given: the reference of a deleted meta was never removed (a stop between the two) and
// the executor did not run / When: the collection runs / Then: the orphan check drops the
// row first, so the blob is collected in the same start.
#[test]
fn a_reference_whose_meta_is_gone_is_dropped_and_frees_its_blob() {
    let store = Store::new();
    store.add_blob(&sha(1));
    store.add_meta(1, &sha(1), "T");
    assert!(store.collect().refs_complete);
    std::fs::remove_file(store.meta(1)).unwrap();

    let report = store.collect();

    assert_eq!((report.refs_removed_orphan, report.blobs_unlinked), (1, 1));
    assert!(!store.blob(&sha(1)).exists());
}

// Given: a capture stopped after saving the blob and recording the reference but before
// the meta, and another that stopped right after the blob / When: the collection runs
// Then: the reference is an orphan and both blobs are reclaimed; a meta whose reference
// was never recorded gets one (its blob stays).
#[test]
fn a_capture_that_stopped_halfway_leaves_nothing_behind() {
    let store = Store::new();
    store.add_blob(&sha(1));
    store.add_blob(&sha(2));
    store.add_blob(&sha(3));
    store
        .world
        .database()
        .gc_insert_refs(&[storage::artifact_refs::ArtifactRef {
            artifact_id: id(1),
            sha256: sha(1),
            thread_id: None,
            run_id: None,
            recorded_at_ns: 1,
        }])
        .unwrap();
    store.add_meta(3, &sha(3), "T");

    let report = store.collect();

    assert!(report.refs_complete, "{report:?}");
    assert_eq!((report.refs_removed_orphan, report.blobs_unlinked), (1, 2));
    assert!(!store.blob(&sha(1)).exists() && !store.blob(&sha(2)).exists());
    assert!(store.blob(&sha(3)).is_file());
    assert_eq!(store.refs(), [id(3)]);
}

// Given: meta files that cannot be turned into a reference, each of a different kind
// When: the collection runs, then the file is repaired or deleted
// Then: while it is unreadable nothing is unlinked and the references are not complete;
// afterwards it resolves and the unreferenced blob goes.
#[test]
fn an_unreadable_meta_stops_the_sweep_until_it_is_repaired() {
    type Break = fn(&Store, &Path);
    let cases: [(&str, Break); 6] = [
        ("broken json", |_, path| {
            std::fs::write(path, b"{ \"artifact_id\": ").unwrap();
        }),
        ("larger than the limit", |_, path| {
            let mut content = b"{\"pad\":\"".to_vec();
            content.resize(2 * 1024 * 1024, b'x');
            std::fs::write(path, content).unwrap();
        }),
        ("a directory instead of a file", |_, path| {
            std::fs::create_dir(path).unwrap();
        }),
        ("a dangling link", |_, path| {
            std::os::unix::fs::symlink("/nonexistent/target", path).unwrap();
        }),
        ("wrong identity", |_, path| {
            std::fs::write(path, Store::meta_json(99, &sha(1), "T")).unwrap();
        }),
        ("invalid sha256", |_, path| {
            std::fs::write(path, Store::meta_json(7, "not-a-sha", "T")).unwrap();
        }),
    ];
    for (label, break_it) in cases {
        let store = Store::new();
        store.add_blob(&sha(9));
        store.add_blob(&sha(1));
        store.add_meta(1, &sha(1), "T");
        break_it(&store, &store.meta(7));

        let broken = store.collect();

        assert!(!broken.refs_complete, "{label}: {broken:?}");
        assert_nothing_unlinked(&broken);
        assert_eq!(broken.unknown_remaining, 1, "{label}");
        assert!(store.blob(&sha(9)).is_file(), "{label}");
        assert_eq!(
            store.refs(),
            [id(1)],
            "{label}: the readable meta is marked"
        );

        // Still broken at the next start: still no sweep.
        let again = store.collect();
        assert!(!again.refs_complete, "{label}");
        assert_nothing_unlinked(&again);
        assert!(store.blob(&sha(9)).is_file(), "{label}");

        // Repaired: it names a blob, which is now referenced.
        let path = store.meta(7);
        if path.is_dir() {
            std::fs::remove_dir(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
        store.add_meta(7, &sha(2), "T");
        store.add_blob(&sha(2));
        let repaired = store.collect();
        assert!(repaired.refs_complete, "{label}: {repaired:?}");
        assert_eq!(repaired.unknown_remaining, 0, "{label}");
        assert_eq!(repaired.unknown_resolved, 1, "{label}");
        assert!(!store.blob(&sha(9)).exists(), "{label}");
        assert!(store.blob(&sha(1)).is_file() && store.blob(&sha(2)).is_file());
    }
}

// Given: a meta file that the process may not read / When: the collection runs
// Then: it is unknown (not "absent") and blocks the sweep; with permission restored the
// next start resolves it.
#[test]
fn a_meta_without_read_permission_blocks_the_sweep() {
    let store = Store::new();
    store.add_blob(&sha(9));
    store.add_meta(1, &sha(1), "T");
    std::fs::set_permissions(store.meta(1), std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::File::open(store.meta(1)).is_ok() {
        // Running as a user that ignores file modes (root): the injection is impossible.
        return;
    }

    let blocked = store.collect();

    assert!(!blocked.refs_complete, "{blocked:?}");
    assert_eq!(blocked.unknown_remaining, 1);
    assert!(
        blocked.unknown_sample[0]
            .1
            .to_lowercase()
            .contains("permission"),
        "{:?}",
        blocked.unknown_sample
    );
    assert_nothing_unlinked(&blocked);
    assert!(store.blob(&sha(9)).is_file());

    std::fs::set_permissions(store.meta(1), std::fs::Permissions::from_mode(0o644)).unwrap();
    let repaired = store.collect();
    assert!(repaired.refs_complete, "{repaired:?}");
    assert_eq!(repaired.blobs_unlinked, 1);
}

// Given: a meta directory that cannot be listed or does not exist / When: the collection
// runs / Then: no reference is concluded from it: references stay, nothing is unlinked.
#[test]
fn a_meta_directory_that_cannot_be_listed_sweeps_nothing() {
    let store = Store::new();
    store.add_blob(&sha(9));
    store
        .world
        .database()
        .gc_insert_refs(&[storage::artifact_refs::ArtifactRef {
            artifact_id: id(1),
            sha256: sha(8),
            thread_id: None,
            run_id: None,
            recorded_at_ns: 1,
        }])
        .unwrap();

    std::fs::remove_dir(store.dir.join("meta")).unwrap();
    let missing = store.collect();
    assert!(!missing.refs_complete);
    assert!(
        missing.stops.contains(&GcStop::MetaDirMissing),
        "{missing:?}"
    );
    assert_nothing_unlinked(&missing);
    assert_eq!(
        store.refs(),
        [id(1)],
        "references are not judged without a listing"
    );

    std::fs::create_dir(store.dir.join("meta")).unwrap();
    std::fs::set_permissions(
        store.dir.join("meta"),
        std::fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    if std::fs::read_dir(store.dir.join("meta")).is_ok() {
        std::fs::set_permissions(
            store.dir.join("meta"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        return;
    }
    let unreadable = store.collect();
    std::fs::set_permissions(
        store.dir.join("meta"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    assert!(!unreadable.refs_complete);
    assert!(
        unreadable
            .stops
            .iter()
            .any(|stop| matches!(stop, GcStop::MetaListing(_))),
        "{unreadable:?}"
    );
    assert_nothing_unlinked(&unreadable);
    assert!(store.blob(&sha(9)).is_file());
}

// Given: a database that fails (its reference table is gone) / When: the collection runs
// Then: the failure is reported as a stop and no blob is unlinked.
#[test]
fn a_database_error_is_never_read_as_unreferenced() {
    let store = Store::new();
    store.add_blob(&sha(9));
    store.add_meta(1, &sha(1), "T");
    store
        .world
        .conn()
        .execute_batch("DROP TABLE artifact_refs;")
        .unwrap();

    let report = store.collect();

    assert!(!report.refs_complete);
    assert!(
        report
            .stops
            .iter()
            .any(|stop| matches!(stop, GcStop::Database(_))),
        "{report:?}"
    );
    assert_nothing_unlinked(&report);
    assert!(store.blob(&sha(9)).is_file());
}

// Given: names in blobs/ that are not `<sha256>.<ext>` files, and a directory with a blob's
// name / When: the collection runs / Then: only real unreferenced blob files are removed.
#[test]
fn only_blob_files_are_touched() {
    let store = Store::new();
    let blobs = store.dir.join("blobs");
    std::fs::write(blobs.join(".partial-abc.tmp"), b"x").unwrap();
    std::fs::write(blobs.join("notes.txt"), b"x").unwrap();
    std::fs::write(
        blobs.join(format!("{}.png", sha(0xab).to_uppercase())),
        b"x",
    )
    .unwrap();
    std::fs::write(blobs.join(format!("{}.tar.gz", sha(2))), b"x").unwrap();
    std::fs::create_dir(blobs.join(format!("{}.png", sha(3)))).unwrap();
    std::os::unix::fs::symlink("/etc/hostname", blobs.join(format!("{}.png", sha(4)))).unwrap();
    store.add_blob(&sha(9));

    let report = store.collect();

    assert!(report.refs_complete);
    assert_eq!(report.blobs_unlinked, 1, "{report:?}");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(std::fs::read_dir(&blobs).unwrap().count(), 6);
    assert!(Path::new("/etc/hostname").exists());
}

// Given: more metas than one start may open / When: starts repeat / Then: marking
// continues over the starts, references stay incomplete (nothing swept) until the last
// meta has one, and only then is the unreferenced blob unlinked.
#[test]
fn marking_continues_across_starts_and_the_sweep_waits_for_it() {
    let store = Store::new();
    for n in 1..=6_u32 {
        let blob = sha(u8::try_from(n).unwrap());
        store.add_blob(&blob);
        store.add_meta(n, &blob, "T");
    }
    store.add_blob(&sha(99));
    let budget = GcBudget {
        meta_files: 2,
        ..GcBudget::default()
    };

    for (start, added) in [(1, 2), (2, 2)] {
        let report = store.collect_with(&budget);
        assert_eq!(report.refs_added, added, "start {start}");
        assert!(!report.refs_complete, "start {start}");
        assert!(report.stops.contains(&GcStop::MetaOpenBudget));
        assert_nothing_unlinked(&report);
        assert!(store.blob(&sha(99)).is_file());
    }
    let last = store.collect_with(&budget);
    assert_eq!(last.refs_added, 2);
    assert!(last.refs_complete, "{last:?}");
    assert_eq!(last.blobs_unlinked, 1);
    assert_eq!(store.refs().len(), 6);
    assert!(!store.blob(&sha(99)).exists());
}

// Given: a meta written between two starts by a host that records no references, after
// an earlier complete start / When: the collection runs / Then: the full listing finds it
// and its blob is kept.
#[test]
fn a_meta_added_between_starts_without_a_reference_is_still_protected() {
    let store = Store::new();
    store.add_blob(&sha(1));
    store.add_meta(1, &sha(1), "T");
    assert!(store.collect().refs_complete);

    store.add_blob(&sha(2));
    store.add_meta(2, &sha(2), "T");
    let report = store.collect();

    assert!(report.refs_complete);
    assert_nothing_unlinked(&report);
    assert!(store.blob(&sha(2)).is_file());
}

/// `count` metas, each with its own blob, optionally already registered.
fn many_metas(store: &Store, count: u32, registered: bool) {
    for n in 1..=count {
        let blob = sha(u8::try_from(n).unwrap());
        store.add_blob(&blob);
        store.add_meta(n, &blob, "T");
    }
    if registered {
        let references = (1..=count)
            .map(|n| storage::artifact_refs::ArtifactRef {
                artifact_id: id(n),
                sha256: sha(u8::try_from(n).unwrap()),
                thread_id: None,
                run_id: None,
                recorded_at_ns: 1,
            })
            .collect::<Vec<_>>();
        store.world.database().gc_insert_refs(&references).unwrap();
    }
}

// Given: more metas than one start may look up, all with references already
// When: starts repeat with the same budget / Then: the pass continues from its position,
// the references are complete only at the start that gets past the last name, and only
// then is the unreferenced blob unlinked.
#[test]
fn a_pass_beyond_the_lookup_budget_finishes_over_several_starts_when_all_are_registered() {
    let store = Store::new();
    many_metas(&store, 8, true);
    store.add_blob(&sha(99));
    let budget = GcBudget {
        meta_lookups: 3,
        ..GcBudget::default()
    };

    for start in 1..=2 {
        let report = store.collect_with(&budget);
        assert!(!report.refs_complete, "start {start}: {report:?}");
        assert!(report.stops.contains(&GcStop::MetaLookupBudget));
        assert!(!report.pass_restarted);
        assert_nothing_unlinked(&report);
        assert!(store.blob(&sha(99)).is_file());
    }
    assert!(store.state().meta_cursor.is_some(), "a pass is under way");

    let last = store.collect_with(&budget);
    assert!(last.refs_complete, "{last:?}");
    assert_eq!((last.refs_added, last.blobs_unlinked), (0, 1));
    assert!(store.state().meta_cursor.is_none());

    // The next start begins a new pass (and takes three starts again).
    assert!(!store.collect_with(&budget).refs_complete);
}

// Given: more unregistered metas than one start may look up / When: starts repeat
// Then: each start registers its share and the pass completes over the same number of
// starts as the lookups need.
#[test]
fn a_pass_beyond_the_lookup_budget_finishes_over_several_starts_when_none_is_registered() {
    let store = Store::new();
    many_metas(&store, 8, false);
    store.add_blob(&sha(99));
    let budget = GcBudget {
        meta_lookups: 3,
        ..GcBudget::default()
    };

    let added = (0..3)
        .map(|_| {
            let report = store.collect_with(&budget);
            (report.refs_added, report.refs_complete)
        })
        .collect::<Vec<_>>();

    assert_eq!(added, [(3, false), (3, false), (2, true)]);
    assert!(!store.blob(&sha(99)).exists());
    assert_eq!(store.refs().len(), 8);
}

// Given: a pass under way / When: a meta without a reference appears behind its position
// (a host that records none), a meta appears ahead of it, or a meta behind it is removed
// Then: only the changes behind the position restart the pass; the new meta's blob is never
// swept and the pass still finishes in a bounded number of starts.
#[test]
fn a_meta_added_behind_the_position_restarts_the_pass_and_keeps_its_blob() {
    let store = Store::new();
    many_metas(&store, 8, true);
    let budget = GcBudget {
        meta_lookups: 3,
        ..GcBudget::default()
    };
    store.collect_with(&budget);

    // Ahead of the position: no restart.
    store.add_blob(&sha(50));
    store.add_meta(50, &sha(50), "T");
    let ahead = store.collect_with(&budget);
    assert!(!ahead.pass_restarted, "{ahead:?}");

    // Behind the position, no reference: restart, and nothing is swept meanwhile.
    store.add_blob(&sha(60));
    store.add_meta(0, &sha(60), "T");
    let mut complete_at = None;
    for start in 1..=6 {
        let report = store.collect_with(&budget);
        assert_eq!(
            report.pass_restarted,
            start == 1,
            "start {start}: {report:?}"
        );
        assert!(store.blob(&sha(60)).is_file(), "start {start}");
        assert_nothing_unlinked(&report);
        if report.refs_complete {
            complete_at = Some(start);
            break;
        }
    }
    assert!(complete_at.is_some(), "the restarted pass must finish");
    assert!(store.refs().contains(&id(0)), "the new meta was registered");
    assert!(store.blob(&sha(60)).is_file());

    // Removal behind the position also restarts (the names no longer match).
    store.collect_with(&budget);
    std::fs::remove_file(store.meta(1)).unwrap();
    let removed = store.collect_with(&budget);
    assert!(removed.pass_restarted, "{removed:?}");
}

// Given: a byte budget smaller than the metas / When: the collection runs
// Then: reading stops with the budget, the rest waits for the next start, and nothing is swept
// until every meta is registered.
#[test]
fn the_byte_budget_bounds_reading_and_blocks_the_sweep() {
    let store = Store::new();
    store.add_blob(&sha(99));
    for n in 1..=3_u32 {
        store.add_meta(n, &sha(u8::try_from(n).unwrap()), "T");
    }
    let report = store.collect_with(&GcBudget {
        meta_bytes: 1,
        ..GcBudget::default()
    });

    assert_eq!(report.meta_scanned, 1, "{report:?}");
    assert!(!report.refs_complete);
    assert!(report.stops.contains(&GcStop::MetaBytesBudget));
    assert_nothing_unlinked(&report);
}

// Given: metas whose reads fail after consuming bytes (larger than the limit) and a byte
// budget of one and a half of them / When: the collection runs
// Then: the bytes of the failed reads count, so reading stops after two files and the rest
// waits for the next start; they are all unknown, so nothing is swept.
#[test]
fn bytes_consumed_by_failed_reads_count_against_the_budget() {
    let store = Store::new();
    store.add_blob(&sha(99));
    for n in 1..=5_u32 {
        let mut content = b"{\"pad\":\"".to_vec();
        content.resize(2 * 1024 * 1024, b'x');
        std::fs::write(store.meta(n), content).unwrap();
    }

    let report = store.collect_with(&GcBudget {
        meta_bytes: 3 * 1024 * 1024 / 2,
        ..GcBudget::default()
    });

    assert_eq!(report.meta_scanned, 2, "{report:?}");
    assert_eq!(report.unknown_remaining, 2);
    assert!(report.stops.contains(&GcStop::MetaBytesBudget));
    assert!(!report.refs_complete);
    assert_nothing_unlinked(&report);
    assert!(store.blob(&sha(99)).is_file());
}

// Given: more garbage than the unlink budget, and more entries than the entry budget
// When: starts repeat / Then: each start unlinks at most its budget, the cursor carries on
// and wraps, and everything unreferenced is eventually gone while referenced blobs stay.
#[test]
fn the_sweep_is_bounded_per_start_and_resumes_from_its_cursor() {
    let store = Store::new();
    for n in 10..20_u8 {
        store.add_blob(&sha(n));
    }
    store.add_blob(&sha(1));
    store.add_meta(1, &sha(1), "T");
    let budget = GcBudget {
        unlinks: 3,
        ..GcBudget::default()
    };

    let mut unlinked = Vec::new();
    for _ in 0..4 {
        let report = store.collect_with(&budget);
        assert!(report.blobs_unlinked <= 3);
        unlinked.push(report.blobs_unlinked);
    }
    assert_eq!(unlinked, [3, 3, 3, 1]);
    assert!(store.state().blob_cursor.is_none(), "wrapped around");
    assert!(store.blob(&sha(1)).is_file());

    let store = Store::new();
    for n in 10..20_u8 {
        store.add_blob(&sha(n));
    }
    let budget = GcBudget {
        blob_entries: 4,
        ..GcBudget::default()
    };
    let first = store.collect_with(&budget);
    assert_eq!(first.blobs_unlinked, 4);
    assert!(first.stops.contains(&GcStop::BlobEntryBudget));
    assert!(store.state().blob_cursor.is_some());
    store.collect_with(&budget);
    store.collect_with(&budget);
    assert_eq!(
        std::fs::read_dir(store.dir.join("blobs")).unwrap().count(),
        0
    );
}

// Given: more references than the orphan-check budget, all orphans / When: starts repeat
// Then: each start checks at most its budget and the cursor carries on to the end.
#[test]
fn the_orphan_check_is_bounded_per_start_and_resumes() {
    let store = Store::new();
    let references = (1..=5_u32)
        .map(|n| storage::artifact_refs::ArtifactRef {
            artifact_id: id(n),
            sha256: sha(u8::try_from(n).unwrap()),
            thread_id: None,
            run_id: None,
            recorded_at_ns: 1,
        })
        .collect::<Vec<_>>();
    store.world.database().gc_insert_refs(&references).unwrap();
    let budget = GcBudget {
        ref_checks: 2,
        ..GcBudget::default()
    };

    let removed = (0..3)
        .map(|_| store.collect_with(&budget).refs_removed_orphan)
        .collect::<Vec<_>>();

    assert_eq!(removed, [2, 2, 1]);
    assert!(store.refs().is_empty());
}

// Given: a crash between the two writes that resolve an unreadable meta (the reference is
// in, the record is still there) / When: the collection runs / Then: the stale record
// blocks nothing.
#[test]
fn a_stale_unknown_record_with_a_reference_does_not_block() {
    let store = Store::new();
    store.add_blob(&sha(9));
    store.add_blob(&sha(1));
    store.add_meta(1, &sha(1), "T");
    let database = store.world.database();
    database
        .gc_unknown_meta_upsert(&format!("{}.json", id(1)), "was unreadable", 1)
        .unwrap();
    database
        .gc_insert_refs(&[storage::artifact_refs::ArtifactRef {
            artifact_id: id(1),
            sha256: sha(1),
            thread_id: None,
            run_id: None,
            recorded_at_ns: 1,
        }])
        .unwrap();
    drop(database);

    let report = store.collect();

    assert!(report.refs_complete, "{report:?}");
    assert_eq!(report.blobs_unlinked, 1);
    assert!(store.blob(&sha(1)).is_file());
}

// Given: a crash right after each stage of the phase, including stage 7 / When: the phase
// runs again / Then: the leftover blob is collected exactly once and the referenced blob
// stays, whatever stage the crash hit.
#[test]
fn a_crash_at_any_stage_is_finished_by_the_next_start() {
    let stages = [Stage::Backfill, Stage::Vacuum, Stage::ArtifactGc];
    for crash_at in stages {
        let store = Store::new();
        store.add_blob(&sha(1));
        store.add_blob(&sha(9));
        store.add_meta(1, &sha(1), "T");
        {
            let leases = store.world.exclusive();
            let crashed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_exclusive_phase_with_progress(
                    &leases,
                    &store.paths(),
                    &policy(),
                    &store.world.input(&[], &[]),
                    &StartupBudget::default(),
                    NOW_NS,
                    &mut |event| {
                        if *event == StartupProgress::StageFinished(crash_at) {
                            panic!("injected crash after {crash_at:?}");
                        }
                    },
                )
            }));
            assert!(crashed.is_err());
        }
        // After stage 7 the work is already done; before it, nothing was collected.
        assert_eq!(
            store.blob(&sha(9)).exists(),
            crash_at != Stage::ArtifactGc,
            "{crash_at:?}"
        );

        let report = store.phase(&StartupBudget::default());
        let gc = ran(&report).gc.expect("stage 7 ran");

        assert!(gc.refs_complete, "{crash_at:?}: {gc:?}");
        assert_eq!(
            gc.blobs_unlinked,
            usize::from(crash_at != Stage::ArtifactGc),
            "{crash_at:?}"
        );
        assert!(!store.blob(&sha(9)).exists() && store.blob(&sha(1)).is_file());
    }
}

// Given: a sweep cut short by its budget (a crash after a partial sweep) / When: the next
// start runs with the full budget / Then: the rest is collected and nothing is unlinked
// twice.
#[test]
fn a_partial_sweep_is_completed_without_repeating_work() {
    let store = Store::new();
    for n in 10..16_u8 {
        store.add_blob(&sha(n));
    }
    let partial = store.collect_with(&GcBudget {
        unlinks: 2,
        ..GcBudget::default()
    });
    assert_eq!(partial.blobs_unlinked, 2);

    let rest = store.collect();
    assert_eq!(rest.blobs_unlinked, 4, "{rest:?}");
    assert_eq!(store.collect().blobs_unlinked, 0);
}

// Given: the phase with and without the preconditions of stage 7 / When: it runs
// Then: it runs after the vacuum stage only with an existing artifacts directory and a
// matching binding; otherwise the report has no GC and no file is touched.
#[test]
fn stage_seven_runs_only_with_an_artifacts_directory_and_a_matching_binding() {
    // No directory configured.
    let store = Store::new();
    store.add_blob(&sha(9));
    let report = store.world.phase(&policy(), &store.world.input(&[], &[]));
    assert!(ran(&report).gc.is_none());
    assert!(store.blob(&sha(9)).is_file());

    // A configured directory that does not exist.
    let missing = StartupPaths {
        artifacts_dir: Some(store.world.dir.path().join("no-such-dir")),
        ..store.world.paths()
    };
    let leases = store.world.exclusive();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &missing,
        &policy(),
        &store.world.input(&[], &[]),
        &StartupBudget::default(),
        NOW_NS,
        &mut |_| {},
    )
    .unwrap();
    assert!(ran(&report).gc.is_none() && ran(&report).errors.is_empty());
    drop(leases);

    // A configured path that is a file cannot be examined as a store: reported.
    let file = store.world.dir.path().join("a-file");
    std::fs::write(&file, b"x").unwrap();
    let not_a_directory = StartupPaths {
        artifacts_dir: Some(file),
        ..store.world.paths()
    };
    let leases = store.world.exclusive();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &not_a_directory,
        &policy(),
        &store.world.input(&[], &[]),
        &StartupBudget::default(),
        NOW_NS,
        &mut |_| {},
    )
    .unwrap();
    assert!(ran(&report).gc.is_none());
    assert!(
        ran(&report)
            .errors
            .iter()
            .any(|error| error.contains("artifact gc")),
        "{:?}",
        ran(&report).errors
    );
    drop(leases);

    // Directory present: stage 7 runs last and collects.
    let mut events = Vec::new();
    let leases = store.world.exclusive();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &store.paths(),
        &policy(),
        &store.world.input(&[], &[]),
        &StartupBudget::default(),
        NOW_NS,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();
    drop(leases);
    assert_eq!(ran(&report).gc.expect("ran").blobs_unlinked, 1);
    assert!(!store.blob(&sha(9)).exists());
    let tail = &events[events.len() - 4..];
    assert_eq!(
        tail,
        [
            StartupProgress::StageStarted(Stage::Vacuum),
            StartupProgress::StageFinished(Stage::Vacuum),
            StartupProgress::StageStarted(Stage::ArtifactGc),
            StartupProgress::StageFinished(Stage::ArtifactGc),
        ]
    );

    // Binding mismatch: only the database maintenance runs; files are untouched.
    let store = Store::new();
    store.add_blob(&sha(9));
    std::fs::write(
        store.world.root_binding_file(),
        r#"{"db_id":"someone-else","db_path":"/x","bound_at_ns":1}"#,
    )
    .unwrap();
    let report = store.phase(&StartupBudget::default());
    assert!(matches!(
        report.outcome,
        StartupOutcome::Skipped {
            reason: SkipReason::BindingMismatch,
            ..
        }
    ));
    assert!(store.blob(&sha(9)).is_file());
}

// Given: two threads that captured the same image and a third artifact, deleted one thread
// at a time / When: the executor reclaims each family and the next start runs
// Then: the shared blob survives the first deletion and is collected after the second; the
// third artifact's blob is never touched.
#[test]
fn a_blob_shared_by_two_deleted_threads_goes_with_the_last_of_them() {
    let world = World::new();
    world.archived_thread("T1", "r-T1");
    world.archived_thread("T2", "r-T2");
    let dir = world.dir.path().join("artifacts");
    let workspace = world.dir.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("a.png"),
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-shared",
    )
    .unwrap();
    std::fs::write(
        workspace.join("b.png"),
        b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-other",
    )
    .unwrap();
    let (shared, second, other, blob) = {
        // The writer holds the database open; it is closed before a startup phase.
        let storage = storage::Storage::open(world.storage_config()).unwrap();
        let store = store_with_refs(&dir, storage.handle()).unwrap();
        let capture = |file: &str, run: &str, thread: &str| {
            store
                .capture(
                    &workspace,
                    None,
                    Path::new(file),
                    CaptureRequest {
                        title: "t",
                        caption: None,
                        run_id: run.into(),
                        root_run_id: run.into(),
                        thread_id: Some(thread.into()),
                    },
                )
                .unwrap()
        };
        let shared = capture("a.png", "r-T1", "T1");
        let second = capture("a.png", "r-T2", "T2");
        let other = capture("b.png", "r-S", "S");
        let blob = store.blob_path(&shared);
        (shared, second, other, blob)
    };
    assert_eq!(shared.sha256, second.sha256);
    assert_ne!(shared.sha256, other.sha256);
    let store = Arc::new(ArtifactStore::new(&dir).unwrap());

    let startup = |archived: &[&str], active: &[&str]| {
        let leases = world.exclusive();
        let paths = StartupPaths {
            artifacts_dir: Some(dir.clone()),
            ..world.paths()
        };
        let report = run_exclusive_phase_with_progress(
            &leases,
            &paths,
            &policy(),
            &world.input(archived, active),
            &StartupBudget::default(),
            NOW_NS,
            &mut |_| {},
        )
        .unwrap();
        drop(leases);
        report
    };
    let reclaim = || {
        let storage = storage::Storage::open(world.storage_config()).unwrap();
        let mut config = ExecutorConfig::new(
            storage.handle(),
            world.storage_config(),
            Some(Arc::clone(&store)),
        );
        config.lease_retry_cycles = 1;
        let mut worker = ReclaimWorker::new(config);
        let mut cycles = 0;
        while world.database().reclaim_summary().unwrap().jobs_done == 0
            || world.database().reclaim_summary().unwrap().jobs_db_pending > 0
        {
            let cycle = worker.run_once();
            assert!(cycle.errors.is_empty(), "{cycle:?}");
            cycles += 1;
            assert!(cycles < 100);
        }
    };

    // T1 is deleted; T2 and S live on.
    assert_eq!(ran(&startup(&["T1"], &["T2", "S"])).confirmed.len(), 1);
    reclaim();
    let after_first = startup(&["T1"], &["T2", "S"]);
    let gc = ran(&after_first).gc.expect("stage 7 ran");
    assert!(gc.refs_complete && gc.blobs_unlinked == 0, "{gc:?}");
    assert!(blob.is_file(), "T2 still needs the shared blob");

    // T2 is deleted as well.
    assert_eq!(ran(&startup(&["T1", "T2"], &["S"])).confirmed.len(), 1);
    reclaim();
    let after_second = startup(&["T1", "T2"], &["S"]);
    let gc = ran(&after_second).gc.expect("stage 7 ran");
    assert!(gc.refs_complete && gc.blobs_unlinked == 1, "{gc:?}");
    assert!(!blob.exists());
    assert!(store.blob_path(&other).is_file());
    assert!(store.load(&other.artifact_id).is_ok());

    // A further start has nothing left to do.
    let last = startup(&["T1", "T2"], &["S"]);
    assert_eq!(ran(&last).gc.expect("ran").blobs_unlinked, 0);
}
