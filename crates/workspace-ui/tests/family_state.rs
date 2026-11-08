//! The archive and pin state of a conversation family: one state per family, owned
//! by neither a version nor a child; merged by revision; converted once from the
//! older per-thread form.

use tempfile::TempDir;
use workspace_ui::{
    FamilyUnresolved, LineageKind, ProjectId, SidebarState, ThreadId, ThreadLineage, ThreadRecord,
    sidebar_from_json_report,
};

fn id(name: &str) -> ThreadId {
    ThreadId::new(name)
}

/// `root` with a fork `fork`, a rewound version `v2` and an escalation `esc` of `fork`.
fn family(dir: &TempDir) -> SidebarState {
    let project = ProjectId::new("p");
    let mut sidebar = SidebarState::default();
    sidebar
        .add_project(project.clone(), "P", dir.path())
        .unwrap();
    for name in ["root", "other"] {
        sidebar
            .create_thread(id(name), project.clone(), name)
            .unwrap();
    }
    for (name, parent, kind) in [
        ("fork", "root", Some(LineageKind::Fork)),
        ("v2", "root", Some(LineageKind::Rewind)),
        ("esc", "fork", None),
    ] {
        sidebar
            .create_thread(id(name), project.clone(), name)
            .unwrap();
        let thread = sidebar.threads.last_mut().unwrap();
        thread.parent_thread_id = Some(id(parent));
        thread.lineage = kind.map(|kind| ThreadLineage { kind, point: None });
    }
    workspace_ui::show_version(&mut sidebar.threads, &id("v2"));
    sidebar.refresh_derived();
    sidebar
}

fn state<'a>(
    states: &'a std::collections::BTreeMap<ThreadId, workspace_ui::ResolvedState>,
    name: &str,
) -> &'a workspace_ui::ResolvedState {
    &states[&id(name)]
}

#[test]
fn every_descendant_resolves_to_the_same_stable_family_key() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);

    let keys: Vec<_> = ["root", "fork", "v2", "esc"]
        .map(|name| sidebar.family_key(&id(name)).unwrap())
        .to_vec();
    // The key is the top ancestor, whichever version is shown.
    workspace_ui::show_version(&mut sidebar.threads, &id("v2"));
    let shown: Vec<_> = ["root", "fork", "v2", "esc"]
        .map(|name| sidebar.family_key(&id(name)).unwrap())
        .to_vec();

    assert!(keys.iter().all(|key| key == &id("root")), "{keys:?}");
    assert_eq!(keys, shown);
    assert_eq!(sidebar.family_key(&id("other")).unwrap(), id("other"));
    assert_eq!(
        sidebar.family_members(&id("root")),
        [id("root"), id("fork"), id("v2"), id("esc")]
    );
}

#[test]
fn archiving_and_pinning_act_on_the_whole_family_and_number_their_own_series() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);

    // Archiving from any member reaches the family; the pin series is not touched.
    assert!(sidebar.set_family_archived(&id("esc"), true, 500).unwrap());
    let states = sidebar.resolve_states();
    for name in ["root", "fork", "v2", "esc"] {
        let state = state(&states, name);
        assert_eq!(
            (state.archived, state.archived_at, state.archive_revision),
            (true, Some(500), 1),
            "{name}"
        );
        assert_eq!((state.pinned, state.pin_revision), (false, 0), "{name}");
    }
    assert!(!state(&states, "other").archived);
    assert!(sidebar.threads.iter().filter(|t| t.archived).count() == 4);

    // The same edit again changes nothing and numbers nothing.
    assert!(!sidebar.set_family_archived(&id("root"), true, 900).unwrap());
    assert_eq!(sidebar.families[0].archive_revision, 1);

    // Pinning acts on the family too (past versions, forks and escalations), and does
    // not touch the archive series.
    assert!(sidebar.set_family_pinned(&id("v2"), true).unwrap());
    let states = sidebar.resolve_states();
    for name in ["root", "fork", "v2", "esc"] {
        assert_eq!(
            (
                state(&states, name).pinned,
                state(&states, name).pin_revision
            ),
            (true, 1),
            "{name}"
        );
        assert_eq!(state(&states, name).archive_revision, 1, "{name}");
    }
    // Restoring clears the time and numbers on.
    sidebar.set_family_archived(&id("fork"), false, 0).unwrap();
    let restored = sidebar.resolve_states();
    assert_eq!(
        (
            state(&restored, "root").archived,
            state(&restored, "root").archived_at,
            state(&restored, "root").archive_revision
        ),
        (false, None, 2)
    );
}

#[test]
fn adding_a_member_or_showing_a_version_changes_no_state() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);
    sidebar.set_family_archived(&id("root"), true, 500).unwrap();
    sidebar.set_family_pinned(&id("root"), true).unwrap();
    let before = sidebar.families.clone();

    // A new fork joins the family; another version is shown.
    sidebar
        .create_thread(id("late"), ProjectId::new("p"), "late")
        .unwrap();
    sidebar.threads.last_mut().unwrap().parent_thread_id = Some(id("v2"));
    sidebar.refresh_derived();
    workspace_ui::show_version(&mut sidebar.threads, &id("v2"));
    sidebar.refresh_derived();

    // The state is the same one, and the new member reads it.
    assert_eq!(sidebar.families, before);
    let late = &sidebar.threads.iter().find(|t| t.id == id("late")).unwrap();
    assert!(late.archived && late.pinned);
    assert_eq!(late.archived_at, Some(500));
}

#[test]
fn derived_fields_are_not_saved_and_families_are_saved_once() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);
    sidebar.set_family_archived(&id("root"), true, 500).unwrap();

    let json = serde_json::to_value(&sidebar).unwrap();

    assert!(json["threads"][0].get("archived").is_none(), "{json}");
    assert!(json["threads"][0].get("pinned").is_none());
    assert_eq!(json["families"].as_array().unwrap().len(), 1);
    assert_eq!(json["families"][0]["key"], "root");
    assert_eq!(json["version"], 2);
    // And a roundtrip through the loader gives the same sidebar.
    let (loaded, report) =
        sidebar_from_json_report(&serde_json::to_string(&json).unwrap()).unwrap();
    assert!(report.is_none());
    assert_eq!(loaded, sidebar);
}

#[test]
fn an_unresolvable_family_has_no_state_and_cannot_be_archived() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);
    let project = ProjectId::new("p");
    for (name, parent) in [
        ("orphan", "gone"),
        ("loop-a", "loop-b"),
        ("loop-b", "loop-a"),
    ] {
        sidebar
            .create_thread(id(name), project.clone(), name)
            .unwrap();
        sidebar.threads.last_mut().unwrap().parent_thread_id = Some(id(parent));
    }
    sidebar.refresh_derived();

    let states = sidebar.resolve_states();
    assert_eq!(
        state(&states, "orphan").family,
        Err(FamilyUnresolved::MissingParent {
            thread: id("orphan"),
            parent: id("gone")
        })
    );
    assert!(matches!(
        state(&states, "loop-a").family,
        Err(FamilyUnresolved::Cycle { .. })
    ));
    // Not archived, not pinned, nothing to project, nothing to edit.
    for name in ["orphan", "loop-a", "loop-b"] {
        let unresolved = state(&states, name);
        assert!(!unresolved.archived && !unresolved.pinned && !unresolved.is_resolved());
        assert!(sidebar.set_family_archived(&id(name), true, 1).is_err());
        assert!(sidebar.set_family_pinned(&id(name), true).is_err());
    }
    assert_eq!(sidebar.unresolved_threads().len(), 3);
    // The resolved families are not affected.
    assert!(state(&states, "root").is_resolved());
}

#[test]
fn two_windows_creating_the_state_of_one_family_end_with_one_state_and_no_rewind() {
    let dir = TempDir::new().unwrap();
    let base = family(&dir);
    // A archives; the stale B pins. Neither had a state before.
    let mut a = base.clone();
    a.set_family_archived(&id("root"), true, 100).unwrap();
    let mut b = base.clone();
    b.set_family_pinned(&id("root"), true).unwrap();

    let merged = SidebarState::merge_three_way(&base, &a, &b);
    let swapped = SidebarState::merge_three_way(&base, &b, &a);

    for merged in [&merged, &swapped] {
        assert_eq!(merged.families.len(), 1);
        let state = &merged.families[0];
        assert_eq!(
            (
                state.archived,
                state.archive_revision,
                state.pinned,
                state.pin_revision
            ),
            (true, 1, true, 1)
        );
        assert!(merged.threads.iter().all(|t| t.archived == t.pinned));
    }
}

#[test]
fn a_higher_revision_wins_and_a_replayed_member_never_rewinds_it() {
    let dir = TempDir::new().unwrap();
    let mut base = family(&dir);
    // A: archive (1), restore (2), archive (3). The stale B holds revision 1 and
    // re-adds the same members (a replay) without touching the state.
    let stale = {
        base.set_family_archived(&id("root"), true, 100).unwrap();
        base.clone()
    };
    let mut a = base.clone();
    a.set_family_archived(&id("root"), false, 0).unwrap();
    a.set_family_archived(&id("root"), true, 300).unwrap();
    let mut b = stale.clone();
    b.threads.push({
        let mut replay = ThreadRecord::new(id("esc"), ProjectId::new("p"), "esc");
        replay.parent_thread_id = Some(id("fork"));
        replay
    });
    b.threads.pop();

    for merged in [
        SidebarState::merge_three_way(&stale, &a, &b),
        SidebarState::merge_three_way(&stale, &b, &a),
    ] {
        let state = &merged.families[0];
        assert_eq!(
            (state.archived, state.archived_at, state.archive_revision),
            (true, Some(300), 3)
        );
    }
}

#[test]
fn equal_revisions_with_different_content_keep_the_protective_side() {
    let dir = TempDir::new().unwrap();
    let base = family(&dir);
    let mut a = base.clone();
    a.families.push(workspace_ui::FamilyState {
        membership_epoch: 0,
        key: id("root"),
        archived: true,
        archived_at: Some(40),
        archive_revision: 3,
        pinned: true,
        pin_revision: 3,
    });
    let mut b = base.clone();
    b.families.push(workspace_ui::FamilyState {
        membership_epoch: 0,
        key: id("root"),
        archived: false,
        archived_at: None,
        archive_revision: 3,
        pinned: false,
        pin_revision: 3,
    });

    for merged in [
        SidebarState::merge_three_way(&base, &a, &b),
        SidebarState::merge_three_way(&base, &b, &a),
    ] {
        let state = &merged.families[0];
        assert_eq!(
            (
                state.archived,
                state.archived_at,
                state.pinned,
                state.archive_revision
            ),
            (true, Some(40), true, 3)
        );
    }
}

/// The older form: every thread keeps an archive and a pin of its own.
fn legacy(dir: &TempDir, edit: impl FnOnce(&mut serde_json::Value)) -> String {
    let sidebar = family(dir);
    let mut json = serde_json::to_value(&sidebar).unwrap();
    json["version"] = 1.into();
    json.as_object_mut().unwrap().remove("families");
    edit(&mut json);
    serde_json::to_string(&json).unwrap()
}

fn set(json: &mut serde_json::Value, name: &str, key: &str, value: impl Into<serde_json::Value>) {
    let thread = json["threads"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|thread| thread["id"] == name)
        .unwrap();
    thread[key] = value.into();
}

#[test]
fn an_older_sidebar_is_converted_once_a_mixed_family_is_not_archived_and_no_time_is_made() {
    let dir = TempDir::new().unwrap();
    let json = legacy(&dir, |json| {
        // The root and one fork were archived (with a time); the rest of the family was
        // not (a version made later). "other" is archived without a time (legacy).
        for name in ["root", "fork"] {
            set(json, name, "archived", true);
            set(json, name, "archived_at", 777);
            set(json, name, "archive_revision", 4);
        }
        set(json, "other", "archived", true);
    });

    let (sidebar, report) = sidebar_from_json_report(&json).unwrap();

    let report = report.expect("an older file is converted");
    assert_eq!(
        (
            report.families,
            report.archived,
            report.mixed,
            report.unresolved
        ),
        (2, 1, 1, 0)
    );
    let states = sidebar.resolve_states();
    // The mixed family is not archived, and has no time.
    for name in ["root", "fork", "v2", "esc"] {
        assert!(!state(&states, name).archived, "{name}");
        assert_eq!(state(&states, name).archived_at, None, "{name}");
        // Its revision is above everything its members carried.
        assert!(state(&states, name).archive_revision > 4, "{name}");
    }
    // The legacy archive without a time stays an archive without a time.
    let other = state(&states, "other");
    assert!(other.archived);
    assert_eq!(other.archived_at, None);
    assert_eq!(sidebar.version, 2);
    // Converting again does nothing: the file is the new form.
    let saved = serde_json::to_string(&sidebar).unwrap();
    assert!(sidebar_from_json_report(&saved).unwrap().1.is_none());
}

#[test]
fn a_family_archived_in_every_member_keeps_the_latest_recorded_time_and_a_pin_of_any_member_pins_it()
 {
    let dir = TempDir::new().unwrap();
    let json = legacy(&dir, |json| {
        for (name, at) in [("root", 100), ("fork", 300), ("v2", 200)] {
            set(json, name, "archived", true);
            set(json, name, "archived_at", at);
        }
        set(json, "esc", "archived", true);
        set(json, "fork", "pinned", true);
        set(json, "fork", "pin_revision", 9);
    });

    let (sidebar, report) = sidebar_from_json_report(&json).unwrap();

    assert_eq!(report.unwrap().mixed, 0);
    let states = sidebar.resolve_states();
    for name in ["root", "fork", "v2", "esc"] {
        let state = state(&states, name);
        assert!(state.archived && state.pinned, "{name}");
        // Dated by the latest time any member recorded; a member without a time does
        // not make the family undated.
        assert_eq!(state.archived_at, Some(300), "{name}");
        assert!(state.pin_revision > 9, "{name}");
    }
}

#[test]
fn raising_revisions_above_the_projection_gives_the_sidebars_state_the_last_word() {
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);
    sidebar.set_family_archived(&id("root"), true, 10).unwrap();
    // The database projection of "fork" is at revision 7 (archive) and 5 (pin); nothing
    // is known about "other".
    let floor = |thread: &ThreadId| match thread.to_string().as_str() {
        "fork" => (7, 5),
        _ => (0, 0),
    };

    let raised = sidebar.raise_family_revisions(&floor, false);

    assert_eq!(raised, 1);
    let state = &sidebar.families[0];
    assert_eq!(
        (state.archived, state.archive_revision, state.pin_revision),
        (true, 8, 6),
        "the value is kept; the numbers move above the projection's"
    );
    // A second pass has nothing to raise.
    assert_eq!(sidebar.raise_family_revisions(&floor, false), 0);
}

#[test]
fn a_converted_state_ends_strictly_above_an_equal_projection_but_a_steady_one_is_left_alone() {
    // Given: a family at archive revision 1 / pin revision 1, and a projection holding
    // the same numbers (with another value: the protective side would win there).
    let dir = TempDir::new().unwrap();
    let mut sidebar = family(&dir);
    sidebar.set_family_archived(&id("root"), true, 10).unwrap();
    sidebar.set_family_pinned(&id("root"), true).unwrap();
    let equal = |thread: &ThreadId| {
        if thread == &id("other") {
            (0, 0)
        } else {
            (1, 1)
        }
    };

    // When: a steady state (not just converted) meets an equal projection.
    // Then: nothing changes, so a restart does not renumber.
    assert_eq!(sidebar.raise_family_revisions(&equal, false), 0);
    assert_eq!(sidebar.families[0].archive_revision, 1);

    // When: the same numbers meet a state that was just converted.
    assert_eq!(sidebar.raise_family_revisions(&equal, true), 1);

    // Then: it moves strictly above the projection, and once only.
    let state = &sidebar.families[0];
    assert_eq!((state.archive_revision, state.pin_revision), (2, 2));
    assert_eq!(sidebar.raise_family_revisions(&equal, true), 0);
    // A projection that holds nothing (revision 0 / never synchronised) is no floor.
    assert_eq!(sidebar.raise_family_revisions(&|_| (0, 0), true), 0);
}

#[test]
fn equal_revisions_with_two_archive_times_keep_the_later_one_on_either_side() {
    let dir = TempDir::new().unwrap();
    let base = family(&dir);
    let side = |at: i64| {
        let mut sidebar = base.clone();
        sidebar.families.push(workspace_ui::FamilyState {
            membership_epoch: 0,
            key: id("root"),
            archived: true,
            archived_at: Some(at),
            archive_revision: 4,
            pinned: false,
            pin_revision: 0,
        });
        sidebar.refresh_derived();
        sidebar
    };
    let (early, late) = (side(10), side(20));

    for merged in [
        SidebarState::merge_three_way(&base, &early, &late),
        SidebarState::merge_three_way(&base, &late, &early),
    ] {
        assert_eq!(merged.families[0].archived_at, Some(20));
    }
}

#[test]
fn two_sides_that_gave_one_thread_different_parents_set_it_and_its_descendants_aside() {
    // Given: both sides created the same ID, under different parents.
    let dir = TempDir::new().unwrap();
    let base = family(&dir);
    let project = ProjectId::new("p");
    let side = |parent: &str| {
        let mut sidebar = base.clone();
        sidebar
            .create_thread(id("twin"), project.clone(), "twin")
            .unwrap();
        sidebar.threads.last_mut().unwrap().parent_thread_id = Some(id(parent));
        sidebar
            .create_thread(id("twin-child"), project.clone(), "c")
            .unwrap();
        sidebar.threads.last_mut().unwrap().parent_thread_id = Some(id("twin"));
        sidebar.refresh_derived();
        sidebar
    };
    let (a, b) = (side("root"), side("other"));

    // When: they are merged, either way round, and the result is saved and read back.
    for merged in [
        SidebarState::merge_three_way(&base, &a, &b),
        SidebarState::merge_three_way(&base, &b, &a),
    ] {
        // Then: the thread and its child have no family (nothing is archived, pinned
        // or deleted on a guess), the others are unaffected, and the conflict is kept.
        assert_eq!(merged.parent_conflicts, [id("twin")]);
        let states = merged.resolve_states();
        for name in ["twin", "twin-child"] {
            assert!(matches!(
                state(&states, name).family,
                Err(FamilyUnresolved::ParentConflict { .. })
            ));
            assert!(
                merged
                    .clone()
                    .set_family_archived(&id(name), true, 1)
                    .is_err()
            );
        }
        assert!(state(&states, "root").is_resolved());
        let (loaded, _) =
            sidebar_from_json_report(&serde_json::to_string(&merged).unwrap()).unwrap();
        assert_eq!(loaded.parent_conflicts, [id("twin")]);
    }

    // And: a merge in which only one side changed a parent is not a conflict.
    let mut moved = base.clone();
    moved.threads[1].parent_thread_id = Some(id("root"));
    let merged = SidebarState::merge_three_way(&base, &moved, &base);
    assert!(merged.parent_conflicts.is_empty());
}

#[test]
fn merging_family_states_is_commutative_idempotent_and_never_lowers_a_revision() {
    // Every combination of a small domain, on both sides, against an empty base.
    let dir = TempDir::new().unwrap();
    let base = family(&dir);
    let states: Vec<_> = [0_u64, 1, 2]
        .into_iter()
        .flat_map(|revision| {
            [(false, None), (true, None), (true, Some(7))]
                .into_iter()
                .map(move |(archived, at)| (revision, archived, at))
        })
        .collect();
    let with = |revision: u64, archived: bool, at: Option<i64>, pinned: bool| {
        let mut sidebar = base.clone();
        sidebar.families.push(workspace_ui::FamilyState {
            membership_epoch: 0,
            key: id("root"),
            archived,
            archived_at: at.filter(|_| archived),
            archive_revision: revision,
            pinned,
            pin_revision: revision,
        });
        sidebar.refresh_derived();
        sidebar
    };

    for &(left_revision, left_archived, left_at) in &states {
        for &(right_revision, right_archived, right_at) in &states {
            for pinned in [false, true] {
                let left = with(left_revision, left_archived, left_at, pinned);
                let right = with(right_revision, right_archived, right_at, !pinned);
                let one = SidebarState::merge_three_way(&base, &left, &right);
                let two = SidebarState::merge_three_way(&base, &right, &left);
                assert_eq!(one.families, two.families, "commutative");
                let again = SidebarState::merge_three_way(&base, &one, &left);
                assert_eq!(again.families, one.families, "idempotent");
                let merged = &one.families[0];
                assert!(merged.archive_revision >= left_revision.max(right_revision));
                assert!(merged.pin_revision >= left_revision.max(right_revision));
            }
        }
    }
}
