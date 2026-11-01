use std::panic::{AssertUnwindSafe, catch_unwind};

use storage::thread_retention::{
    BlockReason, ConfirmedFamily, FamilyState, FinalizeBudget, FinalizeOutcome, RetentionStatus,
};

use super::super::startup::{Settled, Stage, finalize_with_doubling, run_exclusive_phase_with};
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
    assert!(world.scratch().thread_dir_exists("T"), "scratch unchanged");
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
    assert_eq!(view.scratch_removed, 1);
    assert!(view.errors.is_empty(), "{:?}", view.errors);
    assert!(report.tombstones.contains_thread("T"));
    assert!(report.tombstones.contains_run("rt"));
    assert!(world.owner_rows().is_empty());
    assert!(!world.scratch().thread_dir_exists("T"));
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
    let report = run_exclusive_phase_with(
        &leases,
        &world.paths(),
        &policy(),
        &input,
        &StartupBudget::default(),
        NOW_NS,
        &mut |stage| {
            if stage == Stage::Explore {
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
                run_exclusive_phase_with(
                    &leases,
                    &world.paths(),
                    &policy(),
                    &input,
                    &StartupBudget::default(),
                    NOW_NS,
                    &mut |stage| {
                        if stage == crash_at {
                            panic!("injected crash after {stage:?}");
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
        assert!(!world.scratch().thread_dir_exists("T"), "{crash_at:?}");
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
            assert!(!world.scratch().thread_dir_exists("T"));
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
            assert!(world.scratch().thread_dir_exists("T"));
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
    assert!(world.scratch().thread_dir_exists("T"));
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
    assert_eq!(ran(&first).scratch_removed, 2);
    let left = ["A", "B", "C"]
        .into_iter()
        .filter(|thread| world.scratch().thread_dir_exists(thread))
        .count();
    assert_eq!(left, 1);

    let second = world.phase_with(&policy(), &input, &budget);
    assert_eq!(ran(&second).scratch_removed, 1);
    assert!(
        !world.scratch().thread_dir_exists("A")
            && !world.scratch().thread_dir_exists("B")
            && !world.scratch().thread_dir_exists("C")
    );
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
