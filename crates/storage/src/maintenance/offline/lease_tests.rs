//! Startup lease tests: acquisition order, partial failure, downgrade, bounded waits.

use std::time::Duration;

use super::*;
use crate::{Storage, StorageConfig, reclaim_lease, shared_with_wait, try_exclusive};

struct Place {
    _dir: tempfile::TempDir,
    db: PathBuf,
    root: PathBuf,
}

fn place() -> Place {
    let dir = tempfile::tempdir().unwrap();
    Place {
        db: dir.path().join("events.db"),
        root: dir.path().join("ownership"),
        _dir: dir,
    }
}

/// Whether the database lease can be taken exclusively right now, i.e. nobody holds it.
fn db_is_free(place: &Place) -> bool {
    lease_file(&place.db).unwrap().try_lock().is_ok()
}

// Given: another process holds the database lease shared / When: taking both
// exclusively / Then: refused without waiting, and succeeds once the holder is gone.
#[test]
fn exclusive_is_refused_while_a_writer_holds_the_database_lease() {
    let place = place();
    let writer = writer_lease(&place.db).unwrap();
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_none());
    drop(writer);
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());
}

// Given: another process holds the root lease shared / When: taking both exclusively
// Then: refused, and the database lock acquired first is released again (a partial
// acquisition must not block the next start).
#[test]
fn a_refused_root_lease_releases_the_database_lease() {
    let place = place();
    let other = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
    // The other process keeps only the root: drop its database half.
    let root = other.root_lease().unwrap();
    drop(other);
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_none());
    assert!(db_is_free(&place), "the database lock must not leak");
    drop(root);
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());
}

// Given: exclusive leases / When: a shared acquisition waits at most zero
// Then: it fails and holds nothing; after `downgrade` a second exclusive attempt is
// refused until the shared leases drop.
#[test]
fn downgrade_keeps_both_locks_shared_and_bounded_waits_fail() {
    let place = place();
    let exclusive = try_exclusive(&place.db, &place.root).unwrap().unwrap();
    assert!(shared_with_wait(&place.db, &place.root, Duration::ZERO).is_err());
    let shared = exclusive.downgrade(Duration::ZERO).unwrap();
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_none());
    let second = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
    drop((shared, second));
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());
}

// Given: a writer opened with a duplicate of the shared database lease / When: the outer
// shared leases drop first / Then: the database stays locked until the writer closes
// (the writer thread is the last holder).
#[test]
fn a_writer_opened_with_a_lease_keeps_the_database_locked_after_the_outer_value_drops() {
    let place = place();
    let shared = shared_with_wait(&place.db, &place.root, Duration::ZERO).unwrap();
    let config = StorageConfig {
        db_path: place.db.clone(),
        ..Default::default()
    };
    let storage = Storage::open_with_lease(config, shared.db_lease().unwrap()).unwrap();
    drop(shared);
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_none());
    storage.close();
    assert!(try_exclusive(&place.db, &place.root).unwrap().is_some());
}

// Given: one holder of the reclaim lease / When: another asks / Then: it gets none until
// the holder drops.
#[test]
fn only_one_process_holds_the_reclaim_lease() {
    let place = place();
    let first = reclaim_lease(&place.db).unwrap().unwrap();
    assert!(reclaim_lease(&place.db).unwrap().is_none());
    drop(first);
    assert!(reclaim_lease(&place.db).unwrap().is_some());
}
