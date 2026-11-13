//! A lane's place in the sidebar follows its current management in the ledger, never
//! its origin or a stale saved copy (ADR 0044 §10, §15).

use std::collections::BTreeMap;

use tempfile::TempDir;
use workspace_state::{
    LaneLink, LanePolicy, ProjectId, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase,
};

fn id(name: &str) -> ThreadId {
    ThreadId::new(name)
}

fn link(lead: Option<&str>, policy: LanePolicy, revision: u64) -> LaneLink {
    LaneLink {
        detached_since_management_revision: (policy == LanePolicy::Detached).then_some(revision),
        lane_id: "lane-1".into(),
        origin_thread_id: id("lead"),
        lead_thread_id: lead.map(id),
        policy,
        management_revision: revision,
    }
}

/// A lead, one ordinary thread and the thread of a lane opened by the lead.
fn sidebar(dir: &TempDir) -> SidebarState {
    let project = ProjectId::new("project");
    let mut sidebar = SidebarState::default();
    sidebar
        .add_project(project.clone(), "Project", dir.path())
        .expect("project must be added");
    for name in ["lead", "other", "lane"] {
        sidebar
            .create_thread(id(name), project.clone(), name)
            .expect("thread must be created");
    }
    sidebar
}

fn lane_of<'a>(sidebar: &'a SidebarState, name: &str) -> &'a ThreadRecord {
    sidebar
        .threads
        .iter()
        .find(|thread| thread.id == id(name))
        .expect("thread must exist")
}

#[test]
fn a_managed_lane_joins_the_family_of_its_lead_and_follows_its_pin_and_archive() {
    // Given: an attached lane of the lead
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Attached, 1))
        .unwrap();

    // When: the lead's family is pinned and archived
    sidebar.set_family_pinned(&id("lead"), true).unwrap();
    sidebar.set_family_archived(&id("lead"), true, 100).unwrap();

    // Then: the lane belongs to that family and reads its state
    assert_eq!(sidebar.family_key(&id("lane")).unwrap(), id("lead"));
    let lane = lane_of(&sidebar, "lane");
    assert!(lane.pinned && lane.archived);
    assert!(!lane_of(&sidebar, "other").archived);
}

#[test]
fn a_detached_lane_is_its_own_family_and_a_lead_archive_does_not_reach_it() {
    // Given: a lane that starts detached, whose origin is the lead
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 1))
        .unwrap();

    // When: the lead's family is archived
    sidebar.set_family_archived(&id("lead"), true, 100).unwrap();

    // Then: the lane is a top-level family of its own and stays active
    assert_eq!(sidebar.family_key(&id("lane")).unwrap(), id("lane"));
    assert!(!lane_of(&sidebar, "lane").archived);
    assert_eq!(sidebar.family_members(&id("lane")), vec![id("lane")]);

    // And: it can be pinned and archived alone without touching the lead
    sidebar.set_family_pinned(&id("lane"), true).unwrap();
    assert!(lane_of(&sidebar, "lane").pinned);
    assert!(!lane_of(&sidebar, "lead").pinned);
}

#[test]
fn summary_lanes_nest_like_attached_ones() {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Summary, 1))
        .unwrap();

    let index = workspace_state::LineageIndex::new(&sidebar.threads);
    assert_eq!(index.display_parent(&id("lane")), Some(&id("lead")));
    assert_eq!(
        index
            .children(&id("lead"))
            .map(|t| t.id.clone())
            .collect::<Vec<_>>(),
        vec![id("lane")]
    );
}

#[test]
fn the_origin_is_not_a_family_relation() {
    // Given: a lane detached from the lead that opened it
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 1))
        .unwrap();

    // Then: it keeps the origin for reference but hangs under nothing
    let lane = lane_of(&sidebar, "lane");
    assert_eq!(lane.lane.as_ref().unwrap().origin_thread_id, id("lead"));
    assert_eq!(lane.tree_parent(), None);
    assert_eq!(lane.parent_thread_id, None);
}

#[test]
fn an_older_ledger_revision_never_restores_a_ended_management() {
    // Given: a lane detached at revision 2
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 2))
        .unwrap();

    // When: a late event or an old scan brings revision 1 (attached)
    let changed = sidebar
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Attached, 1))
        .unwrap();

    // Then: nothing changes
    assert!(!changed);
    assert_eq!(sidebar.family_key(&id("lane")).unwrap(), id("lane"));
}

#[test]
fn a_thread_cannot_become_a_second_lane() {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Attached, 1))
        .unwrap();
    let mut other = link(Some("lead"), LanePolicy::Attached, 5);
    other.lane_id = "lane-2".into();

    assert!(sidebar.apply_lane_link(&id("lane"), other).is_err());
    assert!(
        sidebar
            .apply_lane_link(&id("missing"), link(None, LanePolicy::Detached, 1))
            .is_err()
    );
}

#[test]
fn a_stale_saved_copy_does_not_bring_back_a_ended_management() {
    // Given: the base both sides started from has the lane attached (revision 1)
    let dir = tempfile::tempdir().unwrap();
    let mut base = sidebar(&dir);
    base.apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Attached, 1))
        .unwrap();

    // And: this process saw it detached (revision 2) while the file still says attached
    let mut local = base.clone();
    local
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 2))
        .unwrap();
    let disk = base.clone();

    // When: the two are merged in either role
    for merged in [
        SidebarState::merge_three_way(&base, &local, &disk),
        SidebarState::merge_three_way(&disk, &disk, &local),
        SidebarState::merge_three_way(&local, &local, &disk),
    ] {
        // Then: the higher revision wins and the lane stays independent
        let lane = lane_of(&merged, "lane");
        assert_eq!(lane.lane.as_ref().unwrap().management_revision, 2);
        assert_eq!(lane.tree_parent(), None);
        assert_eq!(merged.family_key(&id("lane")).unwrap(), id("lane"));
    }
}

#[test]
fn a_lane_added_on_one_side_survives_a_side_that_never_heard_of_it() {
    let dir = tempfile::tempdir().unwrap();
    let base = sidebar(&dir);
    let mut local = base.clone();
    local
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Attached, 1))
        .unwrap();

    let merged = SidebarState::merge_three_way(&base, &local, &base);

    assert_eq!(merged.family_key(&id("lane")).unwrap(), id("lead"));
}

#[test]
fn a_managed_lane_sorts_by_its_activity_not_by_a_running_run() {
    // Given: two attached lanes of one lead; the older one has a running run
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    let project = ProjectId::new("project");
    for name in ["lane-old", "lane-new"] {
        sidebar
            .create_thread(id(name), project.clone(), name)
            .unwrap();
    }
    for (name, at, lane_id) in [("lane-old", 10, "a"), ("lane-new", 20, "b")] {
        let mut lane = link(Some("lead"), LanePolicy::Attached, 1);
        lane.lane_id = lane_id.into();
        sidebar.apply_lane_link(&id(name), lane).unwrap();
        let thread = sidebar
            .threads
            .iter_mut()
            .find(|t| t.id == id(name))
            .unwrap();
        thread.updated_at = at;
        thread.created_at = at;
        thread.run_ids = vec![format!("run-{name}")];
    }
    let phases = BTreeMap::from([("run-lane-old".to_owned(), ThreadRunPhase::Running)]);

    // When: the project's threads are ordered
    let (main, _) =
        ThreadRecord::partition_for_project(&sidebar.threads, &ProjectId::new("project"), &phases);

    // Then: lanes of the lead are ordered by latest activity (newest first)
    let order: Vec<_> = main
        .iter()
        .filter(|t| t.is_managed_lane())
        .map(|t| t.id.to_string())
        .collect();
    assert_eq!(order, ["lane-new", "lane-old"]);
}

#[test]
fn the_lane_projection_survives_saving_and_loading_the_sidebar() {
    let dir = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar(&dir);
    sidebar
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Summary, 3))
        .unwrap();

    let json = workspace_state::sidebar_to_json(&sidebar).unwrap();
    let loaded = workspace_state::sidebar_from_json(&json).unwrap();

    let lane = lane_of(&loaded, "lane").lane.clone().unwrap();
    assert_eq!(lane, link(Some("lead"), LanePolicy::Summary, 3));
    assert_eq!(loaded.family_key(&id("lane")).unwrap(), id("lead"));
}

#[test]
fn each_detach_resets_state_but_repair_and_merge_keep_edits_in_the_same_epoch() {
    let dir = tempfile::tempdir().unwrap();
    let mut current = sidebar(&dir);
    current
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 2))
        .unwrap();
    current.set_family_pinned(&id("lane"), true).unwrap();
    current.set_family_archived(&id("lane"), true, 100).unwrap();
    let old_independent = current.clone();
    current
        .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Summary, 3))
        .unwrap();
    assert!(!lane_of(&current, "lane").pinned);
    assert!(!lane_of(&current, "lane").archived);
    current.set_family_pinned(&id("lead"), true).unwrap();
    current.set_family_archived(&id("lead"), true, 200).unwrap();
    current
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 4))
        .unwrap();
    assert!(!lane_of(&current, "lane").pinned);
    assert!(!lane_of(&current, "lane").archived);
    let initial = current.clone();
    // Even much higher edit revisions in an older independent period lose.
    let mut stale = old_independent.clone();
    stale.families.iter_mut().for_each(|state| {
        state.pin_revision = 999;
        state.archive_revision = 999;
    });
    current = SidebarState::merge_three_way(&old_independent, &stale, &current);
    assert!(!lane_of(&current, "lane").pinned);
    assert!(!lane_of(&current, "lane").archived);
    current.set_family_pinned(&id("lane"), true).unwrap();
    current.set_family_archived(&id("lane"), true, 300).unwrap();
    current
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 4))
        .unwrap();
    let reloaded: SidebarState =
        serde_json::from_slice(&serde_json::to_vec(&current).unwrap()).unwrap();
    for (local, disk) in [(&initial, &reloaded), (&reloaded, &initial)] {
        let merged = SidebarState::merge_three_way(&initial, local, disk);
        assert!(lane_of(&merged, "lane").pinned);
        assert!(lane_of(&merged, "lane").archived);
        assert_eq!(lane_of(&merged, "lane").archived_at, Some(300));
    }
    current
        .apply_lane_link(&id("lane"), link(Some("other"), LanePolicy::Summary, 5))
        .unwrap();
    let merged = SidebarState::merge_three_way(&old_independent, &stale, &current);
    assert_eq!(merged.family_key(&id("lane")).unwrap(), id("other"));
    assert!(!lane_of(&merged, "lane").pinned);
    assert!(!lane_of(&merged, "lane").archived);
}

#[test]
fn drops_are_typed_and_rejected_for_each_invalid_destination_and_family_state() {
    use workspace_state::{
        ThreadChatRole,
        lane_management::{LaneDropError, LaneDropIntent},
    };
    let dir = tempfile::tempdir().unwrap();
    let mut state = sidebar(&dir);
    state
        .apply_lane_link(&id("lane"), link(None, LanePolicy::Detached, 2))
        .unwrap();
    let lead = state
        .threads
        .iter_mut()
        .find(|t| t.id == id("lead"))
        .unwrap();
    lead.chat_role = Some(ThreadChatRole::Orchestrator);
    lead.root_run_id = Some("lead-root".into());
    let intent = LaneDropIntent {
        source: state.lane_drag_intent(&id("lane")).unwrap(),
        target_thread_id: id("lead"),
        expected_lead_root_run_id: Some("lead-root".into()),
    };
    assert_eq!(state.validate_lane_drop(&intent), Ok(()));
    for expected in [
        LaneDropError::Child,
        LaneDropError::Worker,
        LaneDropError::LaneRoot,
        LaneDropError::DifferentProject,
        LaneDropError::ReplacedRoot,
        LaneDropError::Pinned,
        LaneDropError::Archived,
        LaneDropError::StaleFamily,
        LaneDropError::StaleManagement,
    ] {
        let mut changed = state.clone();
        let lead = changed
            .threads
            .iter_mut()
            .find(|t| t.id == id("lead"))
            .unwrap();
        match expected {
            LaneDropError::Child => lead.parent_thread_id = Some(id("other")),
            LaneDropError::Worker => lead.chat_role = Some(ThreadChatRole::Worker),
            LaneDropError::LaneRoot => {
                lead.lane = Some(LaneLink {
                    lane_id: "another".into(),
                    ..link(None, LanePolicy::Detached, 1)
                })
            }
            LaneDropError::DifferentProject => lead.project_id = ProjectId::new("another-project"),
            LaneDropError::ReplacedRoot => lead.root_run_id = Some("replacement".into()),
            LaneDropError::Pinned => {
                changed.set_family_pinned(&id("lane"), true).unwrap();
            }
            LaneDropError::Archived => {
                changed.set_family_archived(&id("lane"), true, 100).unwrap();
            }
            LaneDropError::StaleFamily => {
                changed.set_family_pinned(&id("lane"), true).unwrap();
                changed.set_family_pinned(&id("lane"), false).unwrap();
            }
            LaneDropError::StaleManagement => {
                changed
                    .apply_lane_link(&id("lane"), link(Some("lead"), LanePolicy::Summary, 3))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(changed.validate_lane_drop(&intent), Err(expected));
        assert!(!expected.to_string().is_empty());
    }
}
