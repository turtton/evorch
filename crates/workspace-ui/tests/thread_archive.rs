use std::collections::BTreeMap;

use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord, ThreadRunPhase};

#[test]
fn legacy_v1_defaults_when_archive_fields_are_absent() {
    // Given: a sidebar written before archive metadata existed.
    let json = r#"{"version":1,"projects":[],"selected_project":null,
        "active_thread":null,"threads":[{"id":"old","project_id":"p",
        "title":"Legacy","pinned":false,"run_ids":[],
        "branch":null,"worktree_path":null}]}"#;
    // When: reading and reserializing the legacy v1 shape.
    let sidebar: SidebarState = serde_json::from_str(json).unwrap();
    let saved = serde_json::to_value(&sidebar).unwrap();
    // Then: metadata defaults, without a schema bump, survive a round trip.
    assert_eq!(sidebar.version, 1);
    assert!(!sidebar.threads[0].archived);
    assert_eq!(sidebar.threads[0].created_at, 0);
    assert_eq!(sidebar.threads[0].active_root, None);
    assert_eq!(saved["threads"][0]["active_root"], serde_json::Value::Null);
    assert_eq!(saved["threads"][0]["archived"], false);
    assert_eq!(saved["threads"][0]["created_at"], 0);
    assert_eq!(
        serde_json::from_value::<SidebarState>(saved).unwrap(),
        sidebar
    );
}

#[test]
fn active_root_survives_thread_json_round_trip() {
    let mut thread = ThreadRecord::new(ThreadId::new("shared"), ProjectId::new("p"), "Shared");
    assert_eq!(thread.active_root, None);
    thread.active_root = Some("/workspace/active".into());
    let saved = serde_json::to_value(&thread).unwrap();
    assert_eq!(saved["active_root"], "/workspace/active");
    assert_eq!(
        serde_json::from_value::<ThreadRecord>(saved).unwrap(),
        thread
    );
}

#[test]
fn main_list_orders_pinned_then_newest_then_title_when_dates_tie() {
    // Given: interleaved groups and legacy rows with no known creation date.
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = [
        ("old", false, 10),
        ("pinned-old", true, 1),
        ("Zulu", false, 0),
        ("new", false, 20),
        ("Alpha", false, 0),
        ("pinned-new", true, 2),
    ]
    .into_iter()
    .map(|(title, pinned, created_at)| {
        let mut thread = ThreadRecord::new(ThreadId::new(title), project.clone(), title);
        thread.pinned = pinned;
        thread.created_at = created_at;
        thread
    })
    .collect();
    let stored = threads.clone();
    // When: building the presentation lists without modifying stored order.
    let (main, archived) =
        ThreadRecord::partition_for_project(&threads, &project, &BTreeMap::new());
    // Then: pins take precedence over age, and unknown dates sort by title.
    assert_eq!(
        main.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["pinned-new", "pinned-old", "new", "old", "Alpha", "Zulu"]
    );
    assert!(archived.is_empty());
    assert_eq!(threads, stored);
    threads.reverse();
    assert_eq!(
        ThreadRecord::partition_for_project(&threads, &project, &BTreeMap::new())
            .0
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["pinned-new", "pinned-old", "new", "old", "Alpha", "Zulu"]
    );
}

#[test]
fn archive_partition_is_newest_first_when_pins_and_other_projects_exist() {
    // Given: archived pins, newer archives, a visible thread and another project.
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = ["old", "new", "visible", "other"]
        .into_iter()
        .map(|title| ThreadRecord::new(ThreadId::new(title), project.clone(), title))
        .collect();
    threads[0].archived = true;
    threads[0].pinned = true;
    threads[0].created_at = 1;
    threads[1].archived = true;
    threads[1].created_at = 2;
    threads[3].project_id = ProjectId::new("other");
    // When: partitioning the selected project's threads.
    let (main, archived) =
        ThreadRecord::partition_for_project(&threads, &project, &BTreeMap::new());
    // Then: archived rows never leak into main, and pins do not reorder archives.
    assert_eq!(
        main.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["visible"]
    );
    assert_eq!(
        archived
            .iter()
            .map(|t| t.title.as_str())
            .collect::<Vec<_>>(),
        ["new", "old"]
    );
}

#[test]
fn new_thread_has_current_unix_seconds_when_constructed() {
    // Given: bounds from the system clock.
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    // When: constructing a new thread.
    let thread = ThreadRecord::new(ThreadId::new("new"), ProjectId::new("p"), "New");
    // Then: its creation time is now, not the legacy sentinel.
    let after = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    assert!((before..=after).contains(&u64::try_from(thread.created_at).unwrap()));
    assert!(!thread.archived);
}

#[test]
fn main_list_orders_by_latest_activity_when_older_threads_are_updated() {
    // Given: an older thread that had activity after a newer one was created.
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = ["older", "newer"]
        .into_iter()
        .map(|title| ThreadRecord::new(ThreadId::new(title), project.clone(), title))
        .collect();
    threads[0].created_at = 10;
    threads[1].created_at = 20;
    let at = |seconds| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
    // When: the older thread is touched, including by a stale replayed event.
    assert!(threads[0].touch(at(30)));
    assert!(!threads[0].touch(at(25)));
    // Then: activity, not creation, decides the order and survives a round trip.
    let titles = |threads: &[ThreadRecord]| {
        ThreadRecord::partition_for_project(threads, &project, &BTreeMap::new())
            .0
            .iter()
            .map(|t| t.title.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(titles(&threads), ["older", "newer"]);
    let saved: Vec<ThreadRecord> =
        serde_json::from_value(serde_json::to_value(&threads).unwrap()).unwrap();
    assert_eq!(saved[0].last_activity(), 30);
    assert_eq!(titles(&saved), ["older", "newer"]);
}

#[test]
fn main_list_lifts_family_when_a_child_thread_has_newer_activity() {
    // Given: an older root with two escalation children and a newer root.
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = ["older", "child-a", "child-b", "grandchild", "newer"]
        .into_iter()
        .map(|title| ThreadRecord::new(ThreadId::new(title), project.clone(), title))
        .collect();
    for (thread, created_at) in threads.iter_mut().zip([10, 11, 12, 13, 20]) {
        thread.created_at = created_at;
    }
    for (child, parent) in [(1, "older"), (2, "older"), (3, "child-a")] {
        threads[child].parent_thread_id = Some(ThreadId::new(parent));
        threads[child].escalation_source_run_id = Some("run-worker".into());
    }
    let order = |threads: &[ThreadRecord], members: &[&str]| {
        ThreadRecord::partition_for_project(threads, &project, &BTreeMap::new())
            .0
            .iter()
            .map(|t| t.title.as_str())
            .filter(|title| members.contains(title))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(order(&threads, &["older", "newer"]), ["newer", "older"]);
    assert_eq!(
        order(&threads, &["child-a", "child-b"]),
        ["child-a", "child-b"]
    );
    // When: the user replies in the grandchild after the newer root was created.
    let at = |seconds| std::time::UNIX_EPOCH + std::time::Duration::from_secs(seconds);
    assert!(threads[3].touch(at(30)));
    // Then: each of its ancestors rises above its own peers.
    assert_eq!(order(&threads, &["older", "newer"]), ["older", "newer"]);
    assert_eq!(
        order(&threads, &["child-a", "child-b"]),
        ["child-a", "child-b"]
    );
    // When: the other child gets a later reply.
    assert!(threads[2].touch(at(40)));
    // Then: it overtakes its sibling while its family stays first.
    assert_eq!(
        order(&threads, &["child-a", "child-b"]),
        ["child-b", "child-a"]
    );
    assert_eq!(order(&threads, &["older", "newer"]), ["older", "newer"]);
}

/// Family of `parent` (a root with one escalation child) plus lone threads.
fn running_fixture() -> (ProjectId, Vec<ThreadRecord>) {
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = [
        "pinned",
        "parent",
        "child",
        "idle-new",
        "idle-old",
        "stale-pinned-child",
    ]
    .into_iter()
    .map(|title| ThreadRecord::new(ThreadId::new(title), project.clone(), title))
    .collect();
    for (thread, created_at) in threads.iter_mut().zip([1, 2, 3, 30, 20, 4]) {
        thread.created_at = created_at;
    }
    threads[0].pinned = true;
    for (child, parent) in [(2, "parent"), (5, "pinned")] {
        threads[child].parent_thread_id = Some(ThreadId::new(parent));
        threads[child].escalation_source_run_id = Some("run-worker".into());
    }
    // A pin set on a child before pinning was restricted to roots.
    threads[5].pinned = true;
    threads[1].run_ids = vec!["run-parent".into()];
    threads[2].run_ids = vec!["run-child".into()];
    (project, threads)
}

fn root_titles(
    threads: &[ThreadRecord],
    project: &ProjectId,
    phases: &BTreeMap<String, ThreadRunPhase>,
) -> Vec<String> {
    ThreadRecord::partition_for_project(threads, project, phases)
        .0
        .iter()
        .filter(|thread| workspace_ui::display_parent(threads, &thread.id).is_none())
        .map(|thread| thread.title.clone())
        .collect()
}

#[test]
fn main_list_places_family_with_running_child_directly_below_pinned_when_run_is_active() {
    // Given: an older family whose root is idle, and newer idle threads.
    let (project, threads) = running_fixture();
    let mut phases = BTreeMap::from([
        ("run-parent".to_owned(), ThreadRunPhase::Done),
        ("run-child".to_owned(), ThreadRunPhase::Done),
    ]);
    assert_eq!(
        root_titles(&threads, &project, &phases),
        ["pinned", "idle-new", "idle-old", "parent"]
    );
    // When: only the child's run is running.
    phases.insert("run-child".into(), ThreadRunPhase::Running);
    // Then: its family follows the pinned thread, ahead of newer idle ones.
    assert_eq!(
        root_titles(&threads, &project, &phases),
        ["pinned", "parent", "idle-new", "idle-old"]
    );
}

#[test]
fn main_list_ignores_pin_on_child_thread_when_ordering_siblings() {
    // Given: a stale pin on a child of the pinned root, and an unpinned sibling.
    let (project, mut threads) = running_fixture();
    threads[3].parent_thread_id = Some(ThreadId::new("pinned"));
    threads[3].escalation_source_run_id = Some("run-worker".into());
    // When: ordering with no running run, "idle-new" being the newer sibling.
    let titles: Vec<_> = ThreadRecord::partition_for_project(&threads, &project, &BTreeMap::new())
        .0
        .iter()
        .map(|thread| thread.title.clone())
        .collect();
    // Then: the child's pin gives it no precedence over its newer sibling.
    let position = |title: &str| titles.iter().position(|t| t == title).unwrap();
    assert!(position("idle-new") < position("stale-pinned-child"));
}

#[test]
fn main_list_lifts_family_for_a_run_in_a_hidden_rewind_version_and_restores_on_finish() {
    // Given: an older family whose visible version is idle, a hidden rewind
    // version of its child with a run, and newer idle threads.
    let project = ProjectId::new("p");
    let mut threads: Vec<_> = ["family", "child", "child-rewind", "newer-a", "newer-b"]
        .into_iter()
        .map(|title| ThreadRecord::new(ThreadId::new(title), project.clone(), title))
        .collect();
    for (thread, created_at) in threads.iter_mut().zip([1, 2, 3, 20, 30]) {
        thread.created_at = created_at;
    }
    threads[1].parent_thread_id = Some(ThreadId::new("family"));
    threads[1].escalation_source_run_id = Some("run-worker".into());
    // A rewind replaces its source in place: the source is hidden, the rewind shows.
    threads[2].parent_thread_id = Some(ThreadId::new("child"));
    threads[2].lineage = Some(workspace_ui::ThreadLineage {
        kind: workspace_ui::LineageKind::Rewind,
        point: None,
    });
    threads[1].superseded = true;
    threads[1].run_ids = vec!["run-hidden".into()];
    let project_id = project.clone();
    let order = |phase: Option<ThreadRunPhase>| {
        let phases: BTreeMap<_, _> = phase
            .into_iter()
            .map(|phase| ("run-hidden".to_owned(), phase))
            .collect();
        root_titles(&threads, &project_id, &phases)
    };
    assert_eq!(order(None), ["newer-b", "newer-a", "family"]);
    // When the hidden version's run is running, Then its family rises to the top.
    assert_eq!(
        order(Some(ThreadRunPhase::Running)),
        ["family", "newer-b", "newer-a"]
    );
    // When it finishes, Then the family returns to its activity position.
    assert_eq!(
        order(Some(ThreadRunPhase::Done)),
        ["newer-b", "newer-a", "family"]
    );
}
