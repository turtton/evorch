use std::panic::{AssertUnwindSafe, catch_unwind};

use storage::thread_retention::{
    BlockReason, ConfirmedFamily, FamilyState, FinalizeBudget, FinalizeOutcome, RetentionStatus,
};

use super::super::startup::{
    Settled, Stage, StartupProgress, finalize_with_doubling, run_exclusive_phase_with_progress,
};
use super::super::{BINDING_MISMATCH_BLOCK, BindingStatus, StartupBudget, ThreadRetention};
use super::{NOW_NS, World, policy, ran};

const NOW_MS: u64 = (NOW_NS / 1_000_000) as u64;
const DAY_MS: u64 = 86_400_000;

/// A world with archived thread `T` (run `rt`), a Released-style idle owner row and a
/// scratch directory: everything a confirmation has to clean up outside the database.
fn world_with_thread() -> World {
    let world = World::new();
    world.archived_thread("T", "rt");
    world.owner_row("T", false, 1);
    world.scratch_dir("T");
    world
}

fn assert_outside_state_untouched(world: &World) {
    assert!(world.tombstones().is_empty(), "no tombstone");
    assert_eq!(world.owner_rows(), ["T"], "owners.db unchanged");
    assert!(world.scratch_exists("T"), "scratch unchanged");
}

// Given: a long-archived thread with an idle owner row and scratch files
// When: the exclusive phase runs
// Then: the family is confirmed, both sides of the binding exist, and the owner row and
// scratch directory of the deleted thread are gone; a second phase finds nothing to do.
#[test]
fn a_due_family_is_confirmed_and_its_outside_state_is_removed() {
    let world = world_with_thread();
    let input = world.input(&["T"], &[]);

    let report = world.phase(&policy(), &input);
    let view = ran(&report);

    assert_eq!(view.confirmed.len(), 1, "{:?}", view.families);
    assert_eq!(view.owners_removed, 1);
    assert_eq!(view.scratch.removed, 1);
    assert!(view.errors.is_empty(), "{:?}", view.errors);
    assert!(report.tombstones.contains_thread("T"));
    assert!(report.tombstones.contains_run("rt"));
    assert!(world.owner_rows().is_empty());
    assert!(!world.scratch_exists("T"));
    assert!(world.root_binding_file().is_file());
    assert!(world.database().retention_binding().unwrap().is_some());
    assert_eq!(
        report.binding,
        Some(BindingStatus::Unbound),
        "state before this start"
    );

    let again = world.phase(&policy(), &input);
    let view = ran(&again);
    assert!(view.confirmed.is_empty() && view.errors.is_empty());
    assert_eq!(again.binding, Some(BindingStatus::Bound));
}

// Given: each condition that must keep a family from being confirmed
// When: the phase runs
// Then: no tombstone appears and owners.db and scratch are unchanged.
#[test]
fn nothing_outside_the_database_changes_without_a_confirmation() {
    // days = 0
    let world = world_with_thread();
    let disabled = storage::thread_retention::ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };
    let report = world.phase(&disabled, &world.input(&["T"], &[]));
    assert!(ran(&report).confirmed.is_empty());
    assert_outside_state_untouched(&world);

    // Unfinished backfill: an unconverted event and no row budget.
    let world = world_with_thread();
    world
        .conn()
        .execute(
            "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (NULL, 1, 1, 1, 'Lifecycle', 'x')",
            [],
        )
        .unwrap();
    let budget = StartupBudget {
        backfill_max_rows: 0,
        ..StartupBudget::default()
    };
    let report = world.phase_with(&policy(), &world.input(&["T"], &[]), &budget);
    let view = ran(&report);
    assert!(view.remaining_backfill > 0 && view.confirmed.is_empty());
    assert_outside_state_untouched(&world);

    // Search not finished: no edge budget.
    let world = world_with_thread();
    let budget = StartupBudget {
        explore_edges: 0,
        ..StartupBudget::default()
    };
    let report = world.phase_with(&policy(), &world.input(&["T"], &[]), &budget);
    let view = ran(&report);
    assert!(view.confirmed.is_empty());
    assert!(
        view.families
            .iter()
            .all(|(_, state)| !matches!(state, FamilyState::Explored { .. })),
        "{:?}",
        view.families
    );
    assert_outside_state_untouched(&world);

    // Pinned.
    let world = world_with_thread();
    let mut input = world.input(&["T"], &[]);
    input.threads[0].pinned = true;
    let report = world.phase(&policy(), &input);
    let view = ran(&report);
    assert!(view.confirmed.is_empty());
    assert_outside_state_untouched(&world);

    // The family grew between the search and the confirmation.
    let world = world_with_thread();
    let input = world.input(&["T"], &[]);
    let leases = world.exclusive();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &world.paths(),
        &policy(),
        &input,
        &StartupBudget::default(),
        NOW_NS,
        &mut |event| {
            if *event == StartupProgress::StageFinished(Stage::Explore) {
                world.archived_thread("T2", "rt2");
                world
                    .conn()
                    .execute(
                        "UPDATE threads SET parent_thread_id = 'T', lineage_kind = 'fork' \
                         WHERE thread_id = 'T2'",
                        [],
                    )
                    .unwrap();
            }
        },
    )
    .unwrap();
    let view = ran(&report);
    assert!(
        view.confirmed.is_empty(),
        "{:?} {:?}",
        view.stale,
        view.blocked
    );
    assert_outside_state_untouched(&world);
}

// Given: a crash (panic) right after each stage
// When: the phase runs again
// Then: a confirmation survives a crash at or after its commit and the outside state is
// reclaimed by the next start; a crash before it leaves everything unchanged.
#[test]
fn a_crash_after_any_stage_is_resumed_by_the_next_start() {
    let before_confirmation = [Stage::Backfill, Stage::Explore];
    let after_confirmation = [
        Stage::Finalize,
        Stage::Owners,
        Stage::Scratch,
        Stage::Oversized,
        Stage::Vacuum,
    ];
    for crash_at in before_confirmation.into_iter().chain(after_confirmation) {
        let world = world_with_thread();
        let input = world.input(&["T"], &[]);
        {
            let leases = world.exclusive();
            let crashed = catch_unwind(AssertUnwindSafe(|| {
                run_exclusive_phase_with_progress(
                    &leases,
                    &world.paths(),
                    &policy(),
                    &input,
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
        if before_confirmation.contains(&crash_at) {
            assert_outside_state_untouched(&world);
        } else {
            assert!(world.tombstones().contains_thread("T"), "{crash_at:?}");
        }

        let report = world.phase(&policy(), &input);

        assert!(ran(&report).errors.is_empty(), "{crash_at:?}");
        assert!(report.tombstones.contains_thread("T"), "{crash_at:?}");
        assert!(world.owner_rows().is_empty(), "{crash_at:?}");
        assert!(!world.scratch_exists("T"), "{crash_at:?}");
    }
}

// Given: owner rows of each kind / When: the phase runs / Then: released and idle rows do
// not hold a thread; a row claiming a turn holds it until its lease expired for the whole
// retention period, and confirms (and is removed) after that.
#[test]
fn owner_remnants_block_only_inside_the_grace_period() {
    let cases = [
        // (active, expires_at_ms, confirmed)
        (false, NOW_MS, true),
        (true, NOW_MS - 29 * DAY_MS, false),
        (true, NOW_MS - 31 * DAY_MS, true),
        (true, NOW_MS + DAY_MS, false),
    ];
    for (active, expires_at, confirmed) in cases {
        let world = World::new();
        world.archived_thread("T", "rt");
        world.owner_row("T", active, expires_at);
        world.scratch_dir("T");
        let report = world.phase(&policy(), &world.input(&["T"], &[]));
        let view = ran(&report);
        assert_eq!(
            view.confirmed.len(),
            usize::from(confirmed),
            "{active} {expires_at}"
        );
        if confirmed {
            assert!(world.owner_rows().is_empty());
            assert!(!world.scratch_exists("T"));
        } else {
            assert!(
                matches!(view.blocked, [(_, BlockReason::Owner(_))]),
                "{:?}",
                view.blocked
            );
            assert!(
                !world.root_binding_file().exists(),
                "a blocked family binds nothing"
            );
            assert_eq!(world.owner_rows(), ["T"]);
            assert!(world.scratch_exists("T"));
            assert!(world.tombstones().is_empty());
        }
    }
}

// Given: an owner row that cannot be parsed / When: the phase runs
// Then: nothing is confirmed (who holds the thread is unknown) and the reason is reported.
#[test]
fn an_unreadable_owner_registry_confirms_nothing() {
    let world = world_with_thread();
    rusqlite::Connection::open(world.root.join("owners.db"))
        .unwrap()
        .execute(
            "UPDATE thread_owners SET state = 'not json' WHERE thread_id = 'T'",
            [],
        )
        .unwrap();

    let report = world.phase(&policy(), &world.input(&["T"], &[]));
    let view = ran(&report);

    assert!(view.confirmed.is_empty());
    assert!(view.owner_registry_error.is_some());
    assert!(world.tombstones().is_empty());
    assert!(world.scratch_exists("T"));
}

// Given: more scratch directories than the budget / When: the phase runs twice
// Then: each start removes at most the budget and the next one continues.
#[test]
fn scratch_removal_is_bounded_per_start_and_continues() {
    let world = World::new();
    for (thread, run) in [("A", "ra"), ("B", "rb"), ("C", "rc")] {
        world.archived_thread(thread, run);
        world.scratch_dir(thread);
    }
    let budget = StartupBudget {
        scratch_dirs: 2,
        ..StartupBudget::default()
    };
    let input = world.input(&["A", "B", "C"], &[]);

    let first = world.phase_with(&policy(), &input, &budget);
    assert_eq!(ran(&first).scratch.removed, 2);
    let left = ["A", "B", "C"]
        .into_iter()
        .filter(|thread| world.scratch_exists(thread))
        .count();
    assert_eq!(left, 1);

    let second = world.phase_with(&policy(), &input, &budget);
    assert_eq!(ran(&second).scratch.removed, 1);
    assert!(!world.scratch_exists("A") && !world.scratch_exists("B") && !world.scratch_exists("C"));
}

fn confirmed(key: &str) -> ConfirmedFamily {
    ConfirmedFamily {
        family_key: key.into(),
        threads: vec![],
        runs: vec![],
        tasks: vec![],
        estimate_bytes: 0,
    }
}

// Given: a family whose check exceeds the budget / When: retried with doubling
// Then: it settles as soon as a round fits, and after the last round it waits for the
// next start (a family is never split, and every round uses twice the previous budget).
#[test]
fn finalize_doubles_the_budget_and_gives_up_after_the_last_round() {
    let over = || FinalizeOutcome::OverBudget {
        members: 1,
        edges_read: 1,
    };
    let mut seen = Vec::new();
    let settled = finalize_with_doubling(FinalizeBudget { max_edges: 2_000 }, 4, |budget| {
        seen.push(budget.max_edges);
        Ok(if seen.len() < 3 {
            over()
        } else {
            FinalizeOutcome::Confirmed(confirmed("F"))
        })
    })
    .unwrap();
    assert_eq!(settled, Settled::Confirmed(confirmed("F")));
    assert_eq!(seen, [2_000, 4_000, 8_000]);

    let mut tries = 0;
    let settled = finalize_with_doubling(FinalizeBudget { max_edges: 1_000 }, 4, |_| {
        tries += 1;
        Ok(over())
    })
    .unwrap();
    assert_eq!(settled, Settled::OverBudget);
    assert_eq!(tries, 4);

    let settled =
        finalize_with_doubling(FinalizeBudget { max_edges: 1 }, 0, |_| Ok(over())).unwrap();
    assert_eq!(
        settled,
        Settled::OverBudget,
        "at least one attempt, never an endless loop"
    );
    assert_eq!(
        finalize_with_doubling(FinalizeBudget { max_edges: 1 }, 4, |_| Ok(
            FinalizeOutcome::Stale { new_members: 2 }
        ))
        .unwrap(),
        Settled::Stale
    );
}

// Given: archived, recent, blocked and ordinary threads / When: displaying their status
// Then: the host gets days left, next-startup, owner blocks and the binding-mismatch reason.
#[test]
fn schedule_reports_what_the_host_shows() {
    let world = World::new();
    world.archived_thread("OLD", "r1");
    world.archived_thread("NEW", "r2");
    world.archived_thread("HELD", "r3");
    world.owner_row("HELD", true, NOW_MS + DAY_MS);
    let mut input = world.input(&["OLD", "NEW", "HELD"], &["LIVE"]);
    let new = input
        .threads
        .iter_mut()
        .find(|thread| thread.thread_id == "NEW")
        .unwrap();
    new.archived_at_s = Some(NOW_NS / 1_000_000_000 - 10 * 86_400);
    let database = world.database();
    let registry = world.registry();
    let status = |binding: &BindingStatus,
                  policy: &storage::thread_retention::ThreadRetentionPolicy|
     -> std::collections::BTreeMap<String, RetentionStatus> {
        ThreadRetention::schedule(&database, &input, policy, binding, Some(&registry))
            .unwrap()
            .into_iter()
            .collect()
    };

    let normal = policy();
    let shown = status(&BindingStatus::Bound, &normal);
    assert_eq!(shown["OLD"], RetentionStatus::DueNextStartup);
    assert_eq!(shown["NEW"], RetentionStatus::DueIn { days: 20 });
    assert!(matches!(
        shown["HELD"],
        RetentionStatus::Blocked(BlockReason::Owner(_))
    ));
    assert_eq!(shown["LIVE"], RetentionStatus::NotArchived);

    let mismatch = status(&BindingStatus::Mismatch("x".into()), &normal);
    let blocked = RetentionStatus::Blocked(BlockReason::Owner(BINDING_MISMATCH_BLOCK.into()));
    assert_eq!(mismatch["OLD"], blocked);
    assert_eq!(mismatch["NEW"], blocked);
    assert_eq!(mismatch["LIVE"], RetentionStatus::NotArchived);

    let off = storage::thread_retention::ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };
    assert_eq!(
        status(&BindingStatus::Bound, &off)["OLD"],
        RetentionStatus::Disabled
    );
}

// Given: a sidebar read for a different path than the startup paths
// When: the phase is asked to run / Then: it refuses before touching anything.
#[test]
fn an_input_for_other_paths_is_refused() {
    let world = world_with_thread();
    let mut input = world.input(&["T"], &[]);
    input.sidebar_path = "/somewhere/else.json".into();
    let leases = world.exclusive();
    let error = super::super::run_exclusive_phase(
        &leases,
        &world.paths(),
        &policy(),
        &input,
        &StartupBudget::default(),
        NOW_NS,
    )
    .unwrap_err();
    assert!(matches!(error, super::super::RetentionError::Input(_)));
    assert_outside_state_untouched(&world);
}

fn register_oversized_event(world: &World) -> i64 {
    let conn = world.conn();
    conn.execute(
        "INSERT INTO events(session_id, schema_version, monotonic_ns, wall_clock_ns, kind, \
         payload, subkind, run_id) VALUES (NULL, 1, 1, 1, 'Lifecycle', 'big', '', NULL)",
        [],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    conn.execute(
        "INSERT INTO reclaim_oversized(family_key, tbl, key, stored_bytes) \
         VALUES ('archive-index', 'events', ?1, 3)",
        [id.to_string()],
    )
    .unwrap();
    id
}

fn oversized_left(world: &World, id: i64) -> (i64, i64) {
    (
        world.count(&format!("SELECT COUNT(*) FROM events WHERE id = {id}")),
        world.count("SELECT COUNT(*) FROM reclaim_oversized"),
    )
}

// Given: a registered oversized row (as another change registers them) and a retention
// period of 0 / When: the phase runs / Then: nothing new is confirmed but the row is
// reclaimed; and the same holds when the binding does not match, where the owner rows
// and scratch directories are left alone.
#[test]
fn database_maintenance_does_not_depend_on_the_retention_period_or_the_binding() {
    let world = world_with_thread();
    let id = register_oversized_event(&world);
    let off = storage::thread_retention::ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };
    let report = world.phase(&off, &world.input(&["T"], &[]));
    assert!(ran(&report).confirmed.is_empty());
    assert_eq!(oversized_left(&world, id), (0, 0), "reclaimed at days = 0");
    assert_outside_state_untouched(&world);

    let world = world_with_thread();
    let id = register_oversized_event(&world);
    std::fs::write(
        world.root_binding_file(),
        r#"{"db_id":"someone-else","db_path":"/x","bound_at_ns":1}"#,
    )
    .unwrap();
    let report = world.phase(&policy(), &world.input(&["T"], &[]));
    match &report.outcome {
        super::super::StartupOutcome::Skipped {
            reason: super::super::SkipReason::BindingMismatch,
            maintenance: Some(maintenance),
        } => assert_eq!(maintenance.oversized.rows, 1, "{maintenance:?}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        oversized_left(&world, id),
        (0, 0),
        "reclaimed despite the mismatch"
    );
    assert_outside_state_untouched(&world);
}

// Given: a host observing a start / When: the phase runs / Then: it is told, in order and
// before the phase returns, every stage and the confirmed family (public API only).
#[test]
fn progress_is_delivered_to_an_external_observer_in_order() {
    let world = world_with_thread();
    let input = world.input(&["T"], &[]);
    let leases = world.exclusive();
    let mut events = Vec::new();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &world.paths(),
        &policy(),
        &input,
        &StartupBudget::default(),
        NOW_NS,
        &mut |event| events.push(event.clone()),
    )
    .unwrap();

    use StartupProgress::{FamilyConfirmed, StageFinished, StageStarted};
    let both = |stage| [StageStarted(stage), StageFinished(stage)];
    let mut expected = Vec::new();
    expected.extend(both(Stage::Backfill));
    expected.extend(both(Stage::Explore));
    expected.push(StageStarted(Stage::Finalize));
    expected.push(StartupProgress::RootBound);
    expected.push(FamilyConfirmed {
        family_key: ran(&report).confirmed[0].to_owned(),
    });
    expected.push(StageFinished(Stage::Finalize));
    for stage in [
        Stage::Owners,
        Stage::Scratch,
        Stage::Oversized,
        Stage::Vacuum,
    ] {
        expected.extend(both(stage));
    }
    assert_eq!(events, expected);
}

// Given: two due families and a database failure after the first is confirmed
// When: the phase runs / Then: it still returns a report that names the confirmed
// family and the failure (and reclaims the confirmed one's owner row and scratch), instead
// of an error that hides the committed deletion.
#[test]
fn a_failure_after_a_confirmation_keeps_the_report() {
    let world = World::new();
    for (thread, run) in [("T", "rt"), ("U", "ru")] {
        world.archived_thread(thread, run);
        world.owner_row(thread, false, 1);
        world.scratch_dir(thread);
    }
    let input = world.input(&["T", "U"], &[]);
    let leases = world.exclusive();
    let report = run_exclusive_phase_with_progress(
        &leases,
        &world.paths(),
        &policy(),
        &input,
        &StartupBudget::default(),
        NOW_NS,
        &mut |event| {
            if matches!(event, StartupProgress::FamilyConfirmed { .. }) {
                world
                    .conn()
                    .execute_batch("DROP TABLE retention_staging")
                    .unwrap();
            }
        },
    )
    .unwrap();

    let view = ran(&report);
    assert_eq!(view.confirmed.len(), 1, "{view:?}", view = view.errors);
    assert!(
        view.errors
            .iter()
            .any(|error| error.starts_with("finalize")),
        "{:?}",
        view.errors
    );
    let confirmed = view.confirmed[0].to_owned();
    assert_eq!(report.tombstones.threads.len(), 1);
    assert!(report.tombstones.contains_thread(&confirmed));
    assert!(!world.scratch_exists(&confirmed));
    let other = if confirmed == "T" { "U" } else { "T" };
    assert!(world.scratch_exists(other));
}

fn scratch_dir_with_files(world: &World, thread: &str, files: usize) -> std::path::PathBuf {
    let dir = world.scratch_dir(thread);
    for index in 0..files {
        std::fs::write(dir.join(format!("file-{index}")), "x").unwrap();
    }
    dir
}

// Given: a scratch tree larger than the entry budget / When: starts follow each other
// Then: a start that cannot finish the tree reports it as incomplete (never as removed),
// removes no more than its budget, and the start that unlinks the directory is the one
// that reports it removed.
#[test]
fn a_scratch_tree_is_reported_removed_only_when_it_is_gone() {
    let world = World::new();
    world.archived_thread("T", "rt");
    let dir = scratch_dir_with_files(&world, "T", 30);
    let budget = StartupBudget {
        scratch_entries: 10,
        ..StartupBudget::default()
    };
    let input = world.input(&["T"], &[]);

    let mut starts = 0;
    let mut entries = 0;
    loop {
        let report = world.phase_with(&policy(), &input, &budget);
        let view = ran(&report);
        starts += 1;
        entries += view.scratch.entries_removed;
        assert!(view.scratch.entries_removed <= 10);
        assert_eq!(
            view.scratch.removed == 1,
            !dir.exists(),
            "removed means gone"
        );
        if view.scratch.removed == 1 {
            break;
        }
        assert_eq!(view.scratch.incomplete, ["T"]);
        assert!(dir.exists());
        assert!(starts < 20);
    }
    assert!(
        starts >= 3 && entries >= 31,
        "{starts} starts, {entries} entries"
    );
}

// Given: a scratch tree that cannot be emptied / When: starts run
// Then: the failure is reported with the path, the directory is not counted removed and
// the tombstone stays; once the obstacle is gone the next start finishes.
#[test]
fn a_scratch_failure_is_reported_and_retried() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    world.archived_thread("T", "rt");
    let dir = scratch_dir_with_files(&world, "T", 2);
    let locked = dir.join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("file"), "x").unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
    if std::fs::write(locked.join("probe"), "x").is_ok() {
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let input = world.input(&["T"], &[]);

    let report = world.phase(&policy(), &input);
    let view = ran(&report);
    assert_eq!(view.scratch.removed, 0);
    assert_eq!(view.scratch.failed.len(), 1, "{:?}", view.scratch);
    assert!(view.scratch.failed[0].1.contains("locked"));
    assert!(dir.exists());
    assert!(report.tombstones.contains_thread("T"));

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    let again = world.phase(&policy(), &input);
    assert_eq!(ran(&again).scratch.removed, 1);
    assert!(!dir.exists());
}

/// Entries below `path` plus `path` itself, 0 when it is gone.
fn entries_on_disk(path: &std::path::Path) -> usize {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return 1;
    }
    1 + std::fs::read_dir(path)
        .unwrap()
        .flatten()
        .map(|entry| entries_on_disk(&entry.path()))
        .sum::<usize>()
}

// Given: thread A whose scratch tree fails part-way (after some entries were unlinked)
// and thread B with plenty left, and a small entry budget for the whole start
// When: the phase runs / Then: the entries A lost before failing are charged to the
// budget: the real total removed on disk never exceeds the budget, and the report counts
// exactly what disappeared.
#[test]
fn entries_removed_before_a_failure_count_against_the_start_budget() {
    use std::os::unix::fs::PermissionsExt;
    let world = World::new();
    for (thread, run) in [("A", "ra"), ("B", "rb")] {
        world.archived_thread(thread, run);
    }
    // A holds only `sub`, full of files. Its own mode stops `sub` from being unlinked, so
    // the failure comes last, after all of `sub`'s files were removed.
    let a = world.scratch_dir("A");
    for entry in std::fs::read_dir(&a).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).unwrap();
        } else {
            std::fs::remove_file(&path).unwrap();
        }
    }
    let sub = a.join("sub");
    std::fs::create_dir(&sub).unwrap();
    for index in 0..8 {
        std::fs::write(sub.join(format!("file-{index}")), "x").unwrap();
    }
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o500)).unwrap();
    let b = scratch_dir_with_files(&world, "B", 30);
    if std::fs::write(a.join("probe"), "x").is_ok() {
        std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o700)).unwrap();
        return;
    }
    let budget = StartupBudget {
        scratch_entries: 12,
        ..StartupBudget::default()
    };
    let before = entries_on_disk(&a) + entries_on_disk(&b);

    let report = world.phase_with(&policy(), &world.input(&["A", "B"], &[]), &budget);

    let view = ran(&report);
    let removed_on_disk = before - (entries_on_disk(&a) + entries_on_disk(&b));
    assert!(
        removed_on_disk <= 12,
        "{removed_on_disk} entries removed, budget 12"
    );
    assert_eq!(
        view.scratch.entries_removed, removed_on_disk,
        "{:?}",
        view.scratch
    );
    assert_eq!(view.scratch.failed.len(), 1, "{:?}", view.scratch);
    assert_eq!(view.scratch.failed[0].0, "A");
    assert!(a.exists(), "A could not be emptied");
    assert_eq!(
        view.scratch.entries_removed, 12,
        "8 from A, the 4 left from B"
    );
    std::fs::set_permissions(&a, std::fs::Permissions::from_mode(0o700)).unwrap();
}

// Given: an owners.db that cannot even be examined (a symlink loop)
// When: the phase runs / Then: that is not "no registry": nothing is confirmed, no
// tombstone is written, nothing outside the database changes and the reason is reported.
#[test]
fn an_unexaminable_owner_registry_confirms_nothing() {
    let world = World::new();
    world.archived_thread("T", "rt");
    world.scratch_dir("T");
    let owners = world.root.join("owners.db");
    std::os::unix::fs::symlink("owners.db", &owners).unwrap();

    let report = world.phase(&policy(), &world.input(&["T"], &[]));

    let view = ran(&report);
    assert!(view.confirmed.is_empty());
    assert!(
        view.owner_registry_error.is_some(),
        "{view:?}",
        view = view.errors
    );
    assert!(world.tombstones().is_empty());
    assert!(world.scratch_exists("T"));
    assert!(!world.root_binding_file().exists());
}

// Given: a scratch parent that cannot be examined (a symlink loop) and a confirmed thread
// When: the phase runs / Then: the failure to look is reported for the thread (not read as
// "no directory"), and the next start with a readable parent finishes the removal.
#[test]
fn a_scratch_directory_that_cannot_be_examined_is_reported_not_skipped() {
    let world = World::new();
    world.archived_thread("T", "rt");
    let parent = world.scratch().parent().to_path_buf();
    let name = parent.file_name().unwrap().to_owned();
    assert!(!parent.exists());
    std::os::unix::fs::symlink(&name, &parent).unwrap();

    let report = world.phase(&policy(), &world.input(&["T"], &[]));

    let view = ran(&report);
    assert_eq!(view.confirmed.len(), 1, "the deletion itself stands");
    assert_eq!(view.scratch.removed, 0);
    assert_eq!(view.scratch.failed.len(), 1, "{:?}", view.scratch);
    assert_eq!(view.scratch.failed[0].0, "T");

    std::fs::remove_file(&parent).unwrap();
    let dir = world.scratch_dir("T");
    let again = world.phase(&policy(), &world.input(&["T"], &[]));
    assert_eq!(ran(&again).scratch.removed, 1);
    assert!(!dir.exists());
}

// Given: a sidebar path below a symlink loop / When: building the phase input
// Then: it fails instead of silently comparing an unresolved path.
#[test]
fn a_path_that_cannot_be_resolved_is_an_error() {
    let world = World::new();
    let looped = world.dir.path().join("loop");
    std::os::unix::fs::symlink("loop", &looped).unwrap();
    let mut paths = world.paths();
    paths.sidebar_path = looped.join("sidebar.json");
    assert!(super::super::startup_input(&paths, vec![], NOW_NS).is_err());
    let database = world.database();
    let status = super::super::check_binding(
        &database,
        super::super::BindingPaths {
            ownership_root: &world.root,
            sidebar_path: &paths.sidebar_path,
        },
    );
    // Nothing is bound yet, so the check never needs the sidebar; once bound it does.
    assert_eq!(status.unwrap(), BindingStatus::Unbound);
    world.archived_thread("T", "rt");
    world.phase(&policy(), &world.input(&["T"], &[]));
    let bound = super::super::check_binding(
        &world.database(),
        super::super::BindingPaths {
            ownership_root: &world.root,
            sidebar_path: &paths.sidebar_path,
        },
    )
    .unwrap();
    assert!(matches!(bound, BindingStatus::Mismatch(reason) if reason.contains("cannot resolve")));
}

/// An archived thread whose run is `evicted(archived)` and holds one block larger than a
/// purge step, as the index leaves it after an archive. The writer's maintenance tick
/// (not this test) then hands the block to the exclusive phase.
fn archived_run_with_oversized_block(world: &World) {
    let conn = world.conn();
    conn.execute(
        "INSERT INTO thread_index_runs(run_id, state, evicted_reason, source_revision, \
         snapshot_revision, source_context_sha256, extractor_version, accounting_version, \
         source_updated_at_ns, retention_at_ns, work_generation, charged_bytes) \
         VALUES ('rt', 'evicted', 'archived', 1, 1, '', 1, 1, 1, 1, 1, 3000000)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO conversation_blocks(run_id, source_kind, source_key, ordinal, block_index, \
         role, kind, truncated, hash, text) VALUES ('rt', 'message', '0:0', 0, 0, 'user', \
         'text', 0, 'h', hex(zeroblob(1500000)))",
        [],
    )
    .unwrap();
    drop(conn);
    let storage = storage::Storage::open(world.storage_config()).unwrap();
    storage.handle().checkpoint_now().unwrap();
    storage.close();
}

fn index_blocks(world: &World) -> (i64, i64) {
    (
        world.count("SELECT COUNT(*) FROM conversation_blocks WHERE run_id = 'rt'"),
        world.count(
            "SELECT COUNT(*) FROM reclaim_oversized WHERE family_key = 'archive-index' \
             AND tbl = 'conversation_blocks' AND key = 'rt'",
        ),
    )
}

// Given: the archive purge handed an oversized index block of an archived thread to the
// exclusive phase / When: the phase runs with a retention period of 0, or with a binding
// that does not match / Then: the block is deleted and the registration dropped, while the
// owner rows and scratch directory stay; and when the thread was unarchived in the
// meantime, nothing is deleted and only the registration is dropped.
#[test]
fn an_archived_threads_oversized_index_block_is_reclaimed_whatever_the_retention_period() {
    let off = storage::thread_retention::ThreadRetentionPolicy {
        archived_delete_after_days: 0,
        ..policy()
    };

    let world = world_with_thread();
    archived_run_with_oversized_block(&world);
    assert_eq!(
        index_blocks(&world),
        (1, 1),
        "the tick registered it, deleted nothing"
    );
    let report = world.phase(&off, &world.input(&["T"], &[]));
    assert!(
        ran(&report).confirmed.is_empty(),
        "days = 0 confirms nothing"
    );
    assert_eq!(index_blocks(&world), (0, 0), "reclaimed at days = 0");
    assert_eq!(
        world.count("SELECT charged_bytes FROM thread_index_runs WHERE run_id = 'rt'"),
        0
    );
    assert_outside_state_untouched(&world);

    let world = world_with_thread();
    archived_run_with_oversized_block(&world);
    std::fs::write(
        world.root_binding_file(),
        r#"{"db_id":"someone-else","db_path":"/x","bound_at_ns":1}"#,
    )
    .unwrap();
    let report = world.phase(&policy(), &world.input(&["T"], &[]));
    assert!(matches!(
        report.outcome,
        super::super::StartupOutcome::Skipped {
            reason: super::super::SkipReason::BindingMismatch,
            ..
        }
    ));
    assert_eq!(
        index_blocks(&world),
        (0, 0),
        "reclaimed despite the mismatch"
    );
    assert_outside_state_untouched(&world);

    let world = world_with_thread();
    archived_run_with_oversized_block(&world);
    world
        .conn()
        .execute("UPDATE threads SET archived = 0 WHERE thread_id = 'T'", [])
        .unwrap();
    world.phase(&off, &world.input(&[], &["T"]));
    assert_eq!(
        index_blocks(&world),
        (1, 0),
        "an unarchived thread keeps its block"
    );
}
