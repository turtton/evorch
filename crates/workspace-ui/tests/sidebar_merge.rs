//! Merging sidebar states saved by processes that share one file, and the
//! collision-free thread ids those processes create.

use std::collections::BTreeSet;

use tempfile::TempDir;
use workspace_ui::{ProjectId, SidebarState, ThreadId};

fn base_state(dir: &TempDir) -> (SidebarState, ProjectId) {
    let project = ProjectId::new("project");
    let mut sidebar = SidebarState::default();
    sidebar
        .add_project(project.clone(), "Project", dir.path())
        .expect("project must be added");
    sidebar.selected_project = Some(project.clone());
    for id in ["alpha", "beta"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .expect("thread must be created");
    }
    (sidebar, project)
}

fn thread<'a>(sidebar: &'a SidebarState, id: &str) -> &'a workspace_ui::ThreadRecord {
    sidebar
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == id)
        .unwrap_or_else(|| panic!("thread {id} must exist"))
}

#[test]
fn generated_ids_are_short_crockford_base32_and_avoid_taken_ids() {
    // Given: a generator asked to avoid ids it has already produced
    let mut seen = BTreeSet::new();
    for _ in 0..200 {
        let taken = seen.clone();
        // When: another id is generated
        let id = ThreadId::generate(|candidate| taken.contains(&candidate.to_string()));

        // Then: it is `thread-` plus 8 lowercase Crockford characters and is new
        let text = id.to_string();
        let suffix = text.strip_prefix("thread-").expect("thread- prefix");
        assert_eq!(suffix.len(), 8, "{text}");
        assert!(
            suffix
                .chars()
                .all(|c| "0123456789abcdefghjkmnpqrstvwxyz".contains(c)),
            "{text}"
        );
        assert!(seen.insert(text));
    }
}

#[test]
fn generate_retries_until_the_candidate_is_free() {
    // Given: a predicate that rejects the first three candidates
    let rejected = std::cell::Cell::new(0);

    // When: an id is generated
    let id = ThreadId::generate(|_| {
        rejected.set(rejected.get() + 1);
        rejected.get() <= 3
    });

    // Then: the fourth candidate is returned
    assert_eq!(rejected.get(), 4);
    assert!(id.to_string().starts_with("thread-"));
}

#[test]
fn derived_ids_extend_the_base_with_a_random_suffix() {
    // Given/When: two derivations from the same base
    let base = ThreadId::new("thread-abcd1234");
    let first = ThreadId::derived(&base, "fork", |_| false);
    let second = ThreadId::derived(&base, "fork", |_| false);

    // Then: both keep the base and tag, and differ from each other
    for id in [&first, &second] {
        let text = id.to_string();
        let suffix = text.strip_prefix("thread-abcd1234-fork-").expect("prefix");
        assert_eq!(suffix.len(), 8, "{text}");
    }
    assert_ne!(first, second);
}

#[test]
fn threads_created_by_two_processes_both_survive() {
    // Given: two processes that each add a thread to the shared base
    let dir = TempDir::new().unwrap();
    let (base, project) = base_state(&dir);
    let mut local = base.clone();
    local
        .create_thread(ThreadId::new("from-local"), project.clone(), "local")
        .unwrap();
    let mut disk = base.clone();
    disk.create_thread(ThreadId::new("from-disk"), project, "disk")
        .unwrap();

    // When: the local process merges the file's content
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: nothing is lost and the result is valid
    let ids: Vec<String> = merged.threads.iter().map(|t| t.id.to_string()).collect();
    assert_eq!(ids, ["alpha", "beta", "from-local", "from-disk"]);
    merged.validate().expect("merged sidebar must be valid");
}

#[test]
fn edits_to_different_fields_of_one_thread_combine() {
    // Given: local renames a thread while another process adds a run to it
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut local = base.clone();
    local.threads[0].title = "Renamed here".into();
    let mut disk = base.clone();
    disk.threads[0].run_ids.push("run-1".into());
    disk.threads[0].root_run_id = Some("run-1".into());
    disk.threads[0].updated_at = 50;
    local.threads[0].updated_at = 20;

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: both edits are present and activity takes the later time
    let alpha = thread(&merged, "alpha");
    assert_eq!(alpha.title, "Renamed here");
    assert_eq!(alpha.run_ids, ["run-1"]);
    assert_eq!(alpha.root_run_id.as_deref(), Some("run-1"));
    assert_eq!(alpha.updated_at, 50);
}

#[test]
fn a_field_edited_on_both_sides_keeps_the_local_value() {
    // Given: both processes rename the same thread
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut local = base.clone();
    local.threads[1].title = "Local title".into();
    let mut disk = base.clone();
    disk.threads[1].title = "Disk title".into();
    disk.threads[1].set_pin(true);

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: the local title wins while the other side's pin still applies
    let beta = thread(&merged, "beta");
    assert_eq!(beta.title, "Local title");
    assert!(beta.pinned);
}

#[test]
fn run_ids_keep_local_order_then_append_foreign_ones() {
    // Given: each side appended a different run to the same thread
    let dir = TempDir::new().unwrap();
    let (mut base, _) = base_state(&dir);
    base.threads[0].run_ids = vec!["run-0".into()];
    let mut local = base.clone();
    local.threads[0].run_ids.push("run-local".into());
    let mut disk = base.clone();
    disk.threads[0].run_ids.push("run-disk".into());

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: union, local order first
    assert_eq!(
        thread(&merged, "alpha").run_ids,
        ["run-0", "run-local", "run-disk"]
    );
}

#[test]
fn an_unchanged_side_follows_a_deletion_but_an_edit_survives_it() {
    // Given: disk deleted alpha and beta; local only edited beta
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut local = base.clone();
    local.threads[1].title = "Edited".into();
    let mut disk = base.clone();
    disk.threads.clear();

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: the untouched thread is gone and the edited one remains
    let ids: Vec<String> = merged.threads.iter().map(|t| t.id.to_string()).collect();
    assert_eq!(ids, ["beta"]);
}

#[test]
fn deleting_locally_removes_a_thread_the_other_side_left_alone() {
    // Given: local deleted alpha; disk kept it and added a thread
    let dir = TempDir::new().unwrap();
    let (base, project) = base_state(&dir);
    let mut local = base.clone();
    local.threads.remove(0);
    let mut disk = base.clone();
    disk.create_thread(ThreadId::new("gamma"), project, "gamma")
        .unwrap();

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: alpha stays deleted, gamma arrives
    let ids: Vec<String> = merged.threads.iter().map(|t| t.id.to_string()).collect();
    assert_eq!(ids, ["beta", "gamma"]);
}

#[test]
fn selection_stays_local_and_dangling_selection_is_cleared() {
    // Given: local views alpha; disk views beta; alpha is deleted by neither
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut local = base.clone();
    local.active_thread = Some(ThreadId::new("alpha"));
    let mut disk = base.clone();
    disk.active_thread = Some(ThreadId::new("beta"));

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: the local selection is kept
    assert_eq!(merged.active_thread, Some(ThreadId::new("alpha")));
    assert_eq!(merged.selected_project, base.selected_project);

    // And: a local selection of a thread the merge removed is dropped
    let mut disk = base.clone();
    disk.threads
        .retain(|thread| thread.id.to_string() != "alpha");
    let merged = SidebarState::merge_three_way(&base, &local, &disk);
    assert_eq!(merged.active_thread, None);
    merged.validate().expect("merged sidebar must be valid");
}

#[test]
fn a_new_thread_keeps_its_project_when_the_other_side_removed_it() {
    // Given: disk removed the project while local added a thread under it
    let dir = TempDir::new().unwrap();
    let (base, project) = base_state(&dir);
    let mut local = base.clone();
    local
        .create_thread(ThreadId::new("fresh"), project, "fresh")
        .unwrap();
    let disk = SidebarState::default();

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: the project is restored so the new thread stays valid
    assert!(merged.threads.iter().any(|t| t.id.to_string() == "fresh"));
    merged.validate().expect("merged sidebar must be valid");
}

#[test]
fn the_archive_series_with_the_higher_revision_wins_even_over_a_later_local_edit() {
    // Given: this process archives a thread, while a stale process only pins and
    // unpins another thread (it never saw the archive).
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut disk = base.clone();
    disk.threads[0].set_archived(true, 100);
    let mut local = base.clone();
    local.threads[0].set_pin(true);
    local.threads[0].set_pin(false);

    // When: merged
    let merged = SidebarState::merge_three_way(&base, &local, &disk);

    // Then: the archive survives and the pin series is the local one
    let alpha = thread(&merged, "alpha");
    assert!(alpha.archived);
    assert_eq!((alpha.archived_at, alpha.archive_revision), (Some(100), 1));
    assert!(!alpha.pinned);
    assert_eq!(alpha.pin_revision, 2);
}

#[test]
fn a_newer_unarchive_beats_an_older_archive_and_clears_the_time() {
    // Given: the disk archived (revision 1), this process unarchived it after (revision 2)
    let dir = TempDir::new().unwrap();
    let (mut base, _) = base_state(&dir);
    base.threads[0].set_archived(true, 100);
    let mut local = base.clone();
    local.threads[0].set_archived(false, 0);
    let disk = base.clone();

    // When: merged either way round
    let merged = SidebarState::merge_three_way(&base, &local, &disk);
    let swapped = SidebarState::merge_three_way(&base, &disk, &local);

    // Then: the higher revision wins as one unit
    for sidebar in [&merged, &swapped] {
        let alpha = thread(sidebar, "alpha");
        assert_eq!(
            (alpha.archived, alpha.archived_at, alpha.archive_revision),
            (false, None, 2)
        );
    }
}

#[test]
fn equal_revisions_with_different_values_keep_the_protective_side() {
    // Given: both sides reached the same revision with different values (records
    // written by hand, as no numbered edit produces this).
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut local = base.clone();
    local.threads[0].archive_revision = 3;
    local.threads[0].pin_revision = 3;
    local.threads[0].pinned = true;
    let mut disk = base.clone();
    disk.threads[0].archived = true;
    disk.threads[0].archived_at = Some(40);
    disk.threads[0].archive_revision = 3;
    disk.threads[0].pin_revision = 3;

    // When: merged either way round
    let merged = SidebarState::merge_three_way(&base, &local, &disk);
    let swapped = SidebarState::merge_three_way(&base, &disk, &local);

    // Then: archived and pinned win, with the time of the archived side
    for sidebar in [&merged, &swapped] {
        let alpha = thread(sidebar, "alpha");
        assert_eq!(
            (alpha.archived, alpha.archived_at, alpha.pinned),
            (true, Some(40), true)
        );
        assert_eq!((alpha.archive_revision, alpha.pin_revision), (3, 3));
    }
}

#[test]
fn a_record_without_revisions_loads_as_revision_zero_and_numbering_is_not_written_for_it() {
    // Given: a record saved before the archive series was numbered
    let dir = TempDir::new().unwrap();
    let (base, _) = base_state(&dir);
    let mut value = serde_json::to_value(&base).unwrap();
    let thread = &mut value["threads"][0];
    assert!(thread.get("archive_revision").is_none());
    assert!(thread.get("archived_at").is_none());
    thread["archived"] = true.into();

    // When: loaded
    let loaded: SidebarState = serde_json::from_value(value).unwrap();

    // Then: the series is revision 0 with no time
    let alpha = &loaded.threads[0];
    assert_eq!(
        (alpha.archived, alpha.archived_at, alpha.archive_revision),
        (true, None, 0)
    );
}
