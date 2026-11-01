use std::time::Duration;

use super::super::executor::{ExecutorConfig, ReclaimWorker};
use super::super::{BindingPaths, BindingStatus, SkipReason, StartupOutcome, check_binding};
use super::{World, policy, ran};

fn paths(world: &World) -> BindingPaths<'_> {
    BindingPaths {
        ownership_root: &world.root,
        sidebar_path: &world.sidebar,
    }
}

fn status(world: &World) -> BindingStatus {
    check_binding(&world.database(), paths(world)).unwrap()
}

/// A world whose first confirmation bound the database and the root.
fn bound_world() -> World {
    let world = World::new();
    world.archived_thread("T", "rt");
    let report = world.phase(&policy(), &world.input(&["T"], &[]));
    assert_eq!(ran(&report).confirmed.len(), 1);
    world
}

// Given: a fresh pair / When: confirming / Then: the pairing goes Unbound -> Bound.
#[test]
fn the_first_confirmation_binds_both_sides() {
    let world = World::new();
    assert_eq!(status(&world), BindingStatus::Unbound);
    world.archived_thread("T", "rt");
    world.phase(&policy(), &world.input(&["T"], &[]));
    assert_eq!(status(&world), BindingStatus::Bound);
}

// Given: a bound pair with one side damaged or replaced / When: checking
// Then: each case is a mismatch with its own reason, and an interrupted first attempt
// (root file for this database, no database row yet) is not.
#[test]
fn a_missing_changed_or_foreign_side_is_a_mismatch() {
    // Root file removed while the database remembers the pairing.
    let world = bound_world();
    std::fs::remove_file(world.root_binding_file()).unwrap();
    assert!(
        matches!(status(&world), BindingStatus::Mismatch(reason) if reason.contains("missing"))
    );

    // Root file written for another database.
    let world = bound_world();
    std::fs::write(
        world.root_binding_file(),
        r#"{"db_id":"someone-else","db_path":"/x","bound_at_ns":1}"#,
    )
    .unwrap();
    assert!(
        matches!(status(&world), BindingStatus::Mismatch(reason) if reason.contains("another database"))
    );

    // Root file unreadable.
    let world = bound_world();
    std::fs::write(world.root_binding_file(), "garbage").unwrap();
    assert!(
        matches!(status(&world), BindingStatus::Mismatch(reason) if reason.contains("unreadable"))
    );

    // A different sidebar file (a start with `--state`).
    let world = bound_world();
    let other = world.dir.path().join("other-sidebar.json");
    std::fs::write(&other, "{}").unwrap();
    let paths = BindingPaths {
        ownership_root: &world.root,
        sidebar_path: &other,
    };
    assert!(matches!(
        check_binding(&world.database(), paths).unwrap(),
        BindingStatus::Mismatch(reason) if reason.contains("sidebar")
    ));

    // A different ownership root, which has no binding file of its own.
    let world = bound_world();
    let elsewhere = world.dir.path().join("other-root");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let paths = BindingPaths {
        ownership_root: &elsewhere,
        sidebar_path: &world.sidebar,
    };
    assert!(matches!(
        check_binding(&world.database(), paths).unwrap(),
        BindingStatus::Mismatch(reason) if reason.contains("missing")
    ));

    // The root file of this database without a database row: an interrupted first attempt.
    let world = World::new();
    let id = world.database().retention_identity().unwrap();
    super::super::write_root_binding(&world.root, &id, &world.db_path, 1).unwrap();
    assert_eq!(status(&world), BindingStatus::Unbound);
    world.archived_thread("T", "rt");
    let report = world.phase(&policy(), &world.input(&["T"], &[]));
    assert_eq!(
        ran(&report).confirmed.len(),
        1,
        "the attempt resumes and confirms"
    );
    assert_eq!(status(&world), BindingStatus::Bound);
}

// Given: a root file naming another database / When: writing this database's binding
// Then: it is refused and the file is not replaced.
#[test]
fn a_root_binding_is_never_overwritten() {
    let world = World::new();
    let foreign = r#"{"db_id":"someone-else","db_path":"/x","bound_at_ns":1}"#;
    std::fs::write(world.root_binding_file(), foreign).unwrap();
    let id = world.database().retention_identity().unwrap();
    assert!(super::super::write_root_binding(&world.root, &id, &world.db_path, 1).is_err());
    assert_eq!(
        std::fs::read_to_string(world.root_binding_file()).unwrap(),
        foreign
    );
}

// Two databases (A and B) share one ownership root, as two profiles can.
//
// (a) while B runs, A cannot take the root exclusively;
// (b) A confirmed T and went on; B starts later and owns T; A's reclaim executor runs
//     but T's scratch directory and owner row stay (the executor never touches either);
// (c) while B runs, a restart of A cannot take the root exclusively either, so A's
//     startup phase never removes T's scratch;
// (d) once B is the only starter, B sees the root bound to A's database and neither
//     confirms nor removes anything.
#[test]
fn two_databases_sharing_a_root_never_delete_each_others_state() {
    let a = World::new();
    a.archived_thread("T", "rt");
    let b = a.sibling_sharing_root("b");
    b.archived_thread("U", "ru");

    // (a) B is running: it holds the root shared.
    let b_running = storage::shared_with_wait(&b.db_path, &b.root, Duration::ZERO).unwrap();
    assert!(
        storage::try_exclusive(&a.db_path, &a.root)
            .unwrap()
            .is_none()
    );
    drop(b_running);

    // A confirms T while B is not running, then runs on (shared leases, storage, executor).
    let report = a.phase(&policy(), &a.input(&["T"], &[]));
    assert_eq!(ran(&report).confirmed.len(), 1);
    let a_leases = storage::shared_with_wait(&a.db_path, &a.root, Duration::ZERO).unwrap();
    let a_storage =
        storage::Storage::open_with_lease(a.storage_config(), a_leases.db_lease().unwrap())
            .unwrap();

    // (b) B starts later (it can share the root) and owns a thread called T of its own.
    let b_running = storage::shared_with_wait(&b.db_path, &b.root, Duration::ZERO).unwrap();
    b.owner_row("T", true, u64::MAX / 2);
    let b_scratch = b.scratch_dir("T");
    let mut worker = ReclaimWorker::new(ExecutorConfig::new(
        a_storage.handle(),
        a.storage_config(),
        None,
    ));
    for _ in 0..3 {
        let cycle = worker.run_once();
        assert!(cycle.holds_lease && cycle.errors.is_empty(), "{cycle:?}");
    }
    assert!(b_scratch.is_dir(), "the executor must never remove scratch");
    assert_eq!(b.owner_rows(), ["T"], "nor owner rows");

    // (c) A restarts while B runs: no exclusive locks, hence no phase at all.
    drop(worker);
    a_storage.close();
    drop(a_leases);
    assert!(
        storage::try_exclusive(&a.db_path, &a.root)
            .unwrap()
            .is_none()
    );
    assert!(b_scratch.is_dir());
    drop(b_running);

    // (d) B alone: the root belongs to A's database.
    assert!(
        matches!(status(&b), BindingStatus::Mismatch(reason) if reason.contains("another database"))
    );
    let report = b.phase(&policy(), &b.input(&["U"], &[]));
    assert!(matches!(
        report.outcome,
        StartupOutcome::Skipped {
            reason: SkipReason::BindingMismatch,
            ..
        }
    ));
    assert!(b.tombstones().is_empty());
    assert!(b_scratch.is_dir());
    assert_eq!(b.owner_rows(), ["T"]);
}

// Given: a crash at each boundary of the first confirmation (after the root side of the
// binding, and after the commit that wrote tombstones and the database side)
// When: the next start runs / Then: both resume to a bound pair, and until then the
// half-written state reads as this database's own interrupted attempt for this database
// but as a mismatch for any other database.
#[test]
fn a_crash_at_each_binding_boundary_resumes_to_a_bound_pair() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::super::{StartupProgress, run_exclusive_phase_with_progress};

    for crash_at in [
        StartupProgress::RootBound,
        StartupProgress::FamilyConfirmed {
            family_key: "T".into(),
        },
    ] {
        let world = World::new();
        world.archived_thread("T", "rt");
        let stranger = world.sibling_sharing_root("stranger");
        let input = world.input(&["T"], &[]);
        {
            let leases = world.exclusive();
            let crashed = catch_unwind(AssertUnwindSafe(|| {
                run_exclusive_phase_with_progress(
                    &leases,
                    &world.paths(),
                    &policy(),
                    &input,
                    &super::super::StartupBudget::default(),
                    super::NOW_NS,
                    &mut |event| {
                        if *event == crash_at {
                            panic!("injected crash at {event:?}");
                        }
                    },
                )
            }));
            assert!(crashed.is_err());
        }
        assert!(world.root_binding_file().is_file());
        let committed = crash_at != StartupProgress::RootBound;
        assert_eq!(world.tombstones().contains_thread("T"), committed);
        assert_eq!(
            status(&world),
            if committed {
                BindingStatus::Bound
            } else {
                BindingStatus::Unbound
            }
        );
        assert!(
            matches!(status(&stranger), BindingStatus::Mismatch(_)),
            "another database never adopts a root that is half-bound to this one"
        );

        let report = world.phase(&policy(), &input);
        assert!(ran(&report).errors.is_empty());
        assert!(report.tombstones.contains_thread("T"));
        assert_eq!(status(&world), BindingStatus::Bound);
    }
}
