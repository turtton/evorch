use workspace_ui::{ProjectId, SidebarState, ThreadId, ThreadRecord};

#[test]
fn legacy_v1_defaults_when_archive_fields_are_absent() {
    // Given: a sidebar written before archive metadata existed.
    let json = r#"{"version":1,"projects":[],"selected_project":null,
        "active_thread":null,"threads":[{"id":"old","project_id":"p",
        "title":"Legacy","pinned":false,"paused":false,"run_ids":[],
        "branch":null,"worktree_path":null}]}"#;
    // When: reading and reserializing the legacy v1 shape.
    let sidebar: SidebarState = serde_json::from_str(json).unwrap();
    let saved = serde_json::to_value(&sidebar).unwrap();
    // Then: metadata defaults, without a schema bump, survive a round trip.
    assert_eq!(sidebar.version, 1);
    assert!(!sidebar.threads[0].archived);
    assert_eq!(sidebar.threads[0].created_at, 0);
    assert_eq!(saved["threads"][0]["archived"], false);
    assert_eq!(saved["threads"][0]["created_at"], 0);
    assert_eq!(
        serde_json::from_value::<SidebarState>(saved).unwrap(),
        sidebar
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
    let (main, archived) = ThreadRecord::partition_for_project(&threads, &project);
    // Then: pins take precedence over age, and unknown dates sort by title.
    assert_eq!(
        main.iter().map(|t| t.title.as_str()).collect::<Vec<_>>(),
        ["pinned-new", "pinned-old", "new", "old", "Alpha", "Zulu"]
    );
    assert!(archived.is_empty());
    assert_eq!(threads, stored);
    threads.reverse();
    assert_eq!(
        ThreadRecord::partition_for_project(&threads, &project)
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
    let (main, archived) = ThreadRecord::partition_for_project(&threads, &project);
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
