use std::sync::Arc;

use storage::thread_retention::TombstoneSet;

use super::super::{ThreadDeleted, TombstoneGuard};
use crate::ownership::{OwnerHost, RegistryError};

pub(crate) fn tombstones(threads: &[&str], runs: &[&str]) -> TombstoneGuard {
    TombstoneGuard::new(TombstoneSet {
        threads: threads.iter().map(|id| (*id).to_owned()).collect(),
        runs: runs.iter().map(|id| (*id).to_owned()).collect(),
        tasks: Default::default(),
    })
}

// Given: a guard with one deleted thread and run / When: asked about them and others
// Then: only the deleted identities are refused, each with its own error.
#[test]
fn the_guard_refuses_exactly_the_deleted_identities() {
    let guard = tombstones(&["gone"], &["r-gone"]);
    assert_eq!(
        guard.check_thread("gone"),
        Err(ThreadDeleted::Thread("gone".into()))
    );
    assert_eq!(
        guard.check_run("r-gone"),
        Err(ThreadDeleted::Run("r-gone".into()))
    );
    assert!(guard.check_thread("kept").is_ok());
    assert!(guard.check_run("r-kept").is_ok());
    assert!(
        guard.check_thread("r-gone").is_ok(),
        "threads and runs are separate"
    );
}

// Given: a host with a guard for thread "gone" / When: starting, attaching or claiming it
// Then: each is refused with `ThreadDeleted` and leaves no owner row; other threads work.
#[test]
fn an_owner_host_refuses_start_claim_and_attach_for_a_deleted_thread() {
    let root = test_socket_dir::SocketDir::new();
    let bus = Arc::new(event_bus::EventBus::new(32));
    let host = OwnerHost::open(root.path(), config::OwnershipConfig::default(), bus).unwrap();
    // A row that exists from before the deletion: claim must not even look at it.
    let permit = host.start("gone").unwrap();
    let existing = host.attach("gone").unwrap();
    drop(permit);
    assert!(host.set_tombstone_guard(tombstones(&["gone"], &[])).is_ok());
    assert!(
        host.set_tombstone_guard(tombstones(&[], &[])).is_err(),
        "the guard is installed once"
    );

    assert!(matches!(
        host.start("gone"),
        Err(RegistryError::ThreadDeleted(_))
    ));
    assert!(matches!(
        host.attach("gone"),
        Err(RegistryError::ThreadDeleted(_))
    ));
    assert!(matches!(
        host.claim(&existing),
        Err(RegistryError::ThreadDeleted(_))
    ));
    assert!(matches!(
        host.owned_permit("gone"),
        Err(RegistryError::ThreadDeleted(_))
    ));

    assert!(host.start("kept").is_ok());
    assert_eq!(host.attach("kept").unwrap().thread_id, "kept");
    // The refusals changed nothing: still exactly the two rows that existed.
    let registry =
        crate::ownership::Registry::open_readonly(&root.path().join("owners.db")).unwrap();
    let mut rows = registry
        .list()
        .unwrap()
        .into_iter()
        .map(|row| row.thread_id)
        .collect::<Vec<_>>();
    rows.sort();
    assert_eq!(rows, ["gone", "kept"]);
}
