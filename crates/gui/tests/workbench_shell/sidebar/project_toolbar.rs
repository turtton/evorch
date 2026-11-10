use super::{MockSource, state};
use gui::app::WorkbenchState;
use gui::headless::HeadlessWorkbench;
use gui::theme::tokens::ROW_DENSE;
use workspace_ui::{ProjectId, ProjectSort, SidebarState, ThreadId, UiSettings};

const ADD: &str = "Add project";
const SORT: &str = "Sort projects";
const COLLAPSE_ALL: &str = "Collapse all projects";
const EXPAND_ALL: &str = "Expand all projects";

/// Projects `(id, name)` registered in the given order, each in its own directory.
fn sidebar_of(root: &std::path::Path, projects: &[(&str, &str)]) -> SidebarState {
    let mut sidebar = SidebarState::default();
    for (id, name) in projects {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).expect("project dir");
        sidebar
            .add_project(ProjectId::new(*id), *name, &dir)
            .expect("project");
    }
    sidebar
}

fn add_thread(sidebar: &mut SidebarState, project: &str, title: &str, at: i64, archived: bool) {
    sidebar
        .create_thread(ThreadId::new(title), ProjectId::new(project), title)
        .expect("thread");
    let record = sidebar.threads.last_mut().expect("created");
    record.created_at = at;
    record.updated_at = at;
    if archived {
        sidebar
            .set_family_archived(&ThreadId::new(title), true, at)
            .expect("archive");
    }
}

fn harness(sidebar: SidebarState, size: [f32; 2]) -> HeadlessWorkbench<MockSource> {
    let mut harness = HeadlessWorkbench::new(state(MockSource::default(), sidebar), size);
    harness.run();
    harness
}

fn choose_sort(harness: &mut HeadlessWorkbench<MockSource>, choice: &str) {
    harness.click_label(SORT);
    harness.run();
    harness.click_label(choice);
    harness.run();
}

/// Project names top to bottom, as the sidebar draws them.
fn shown_order(harness: &HeadlessWorkbench<MockSource>, names: &[&str]) -> Vec<String> {
    let mut found: Vec<(f32, &str)> = names
        .iter()
        .filter_map(|name| {
            harness
                .label_rects(name)
                .first()
                .map(|rect| (rect.top(), *name))
        })
        .collect();
    found.sort_by(|a, b| a.0.total_cmp(&b.0));
    found.into_iter().map(|(_, name)| name.to_owned()).collect()
}

fn registered_ids(harness: &HeadlessWorkbench<MockSource>) -> Vec<String> {
    harness
        .state()
        .sidebar()
        .projects
        .iter()
        .map(|project| project.id.to_string())
        .collect()
}

#[test]
fn toolbar_stays_fixed_above_the_project_list_while_it_scrolls() {
    // Given: more projects, each with a thread, than fit on screen.
    let temp = tempfile::tempdir().expect("temp dir");
    let ids: Vec<String> = (0..30).map(|index| format!("p{index:02}")).collect();
    let pairs: Vec<(&str, &str)> = ids.iter().map(|id| (id.as_str(), id.as_str())).collect();
    let mut sidebar = sidebar_of(temp.path(), &pairs);
    for id in &ids {
        add_thread(&mut sidebar, id, &format!("thread-{id}"), 1, false);
    }
    let mut harness = harness(sidebar, [1280.0, 600.0]);
    let controls = [ADD, SORT, COLLAPSE_ALL];
    let before: Vec<_> = controls
        .iter()
        .map(|label| harness.label_rects(label)[0])
        .collect();
    let first_project = harness.label_rects("p00")[0];

    // Then: the controls fit one dense row that sits above the first project.
    for rect in &before {
        assert!(rect.height() <= ROW_DENSE, "{rect:?} exceeds one dense row");
        assert!(rect.bottom() <= first_project.top(), "{rect:?} overlaps");
    }

    // When: the list is scrolled to its last project.
    harness.scroll_label_into_view("p29");
    harness.run();

    // Then: the list moved but the toolbar did not.
    assert_ne!(harness.label_rects("p00").first(), Some(&first_project));
    let after: Vec<_> = controls
        .iter()
        .map(|label| harness.label_rects(label)[0])
        .collect();
    assert_eq!(before, after);
}

#[test]
fn toolbar_controls_stay_apart_and_clickable_across_the_label_threshold() {
    // Given: windows from narrow to wide, so the sidebar crosses the width at which the
    // Add project text no longer fits beside the two right-hand buttons.
    let temp = tempfile::tempdir().expect("temp dir");
    let make = || sidebar_of(temp.path(), &[("demo", "demo")]);
    let (mut labelled, mut icon_only) = (0, 0);
    for width in (400..=800).step_by(25) {
        let size = [width as f32, 700.0];
        let mut bar = harness(make(), size);
        let add = bar.label_rects(ADD)[0];
        let sort = bar.label_rects(SORT)[0];
        let fold = bar.label_rects(COLLAPSE_ALL)[0];

        // Then: the controls never overlap, in whichever form the Add button takes.
        assert!(
            add.right() <= sort.left() + 0.5,
            "{width}: {add:?} {sort:?}"
        );
        assert!(
            sort.right() <= fold.left() + 0.5,
            "{width}: {sort:?} {fold:?}"
        );
        for rect in [add, sort, fold] {
            assert!(rect.height() <= ROW_DENSE, "{width}: {rect:?}");
        }
        if add.width() > 60.0 {
            labelled += 1;
        } else {
            icon_only += 1;
        }

        // When: Add project is pressed.
        bar.click_label(ADD);
        bar.run();
        // Then: it opens the dialog.
        assert!(bar.state().project_dialog().is_open(), "{width}");

        // When: the other two controls are pressed on a fresh window.
        let mut other = harness(make(), size);
        other.click_label(COLLAPSE_ALL);
        other.run();
        other.click_label(SORT);
        other.run();
        // Then: folding applied and the sort menu opened.
        assert!(other.has_label(EXPAND_ALL), "{width}");
        assert!(other.has_label("Name (A–Z)"), "{width}");
    }

    // Then: the sweep covered both sides of the threshold.
    assert!(labelled > 0 && icon_only > 0, "{labelled} / {icon_only}");
}

#[test]
fn project_sorts_break_ties_by_registration_and_keep_idle_projects_last() {
    // Given: equal names in different case, a project without threads, and one with
    // only archived threads; "zulu" has the newest thread, "echo" and "delta" tie.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut sidebar = sidebar_of(
        temp.path(),
        &[
            ("a", "echo"),
            ("b", "Alpha"),
            ("c", "alpha"),
            ("d", "delta"),
            ("e", "empty"),
            ("f", "archived"),
            ("g", "zulu"),
        ],
    );
    add_thread(&mut sidebar, "a", "t-a", 50, false);
    add_thread(&mut sidebar, "b", "t-b", 10, false);
    add_thread(&mut sidebar, "c", "t-c", 10, false);
    add_thread(&mut sidebar, "d", "t-d", 50, false);
    add_thread(&mut sidebar, "f", "t-f", 999, true);
    add_thread(&mut sidebar, "g", "t-g", 100, false);
    let names = [
        "echo", "Alpha", "alpha", "delta", "empty", "archived", "zulu",
    ];
    let mut harness = harness(sidebar, [1280.0, 900.0]);
    let canonical = registered_ids(&harness);

    // Then: the default is the registration order.
    assert_eq!(shown_order(&harness, &names), names);

    // When: sorted by name.
    choose_sort(&mut harness, "Name (A–Z)");
    // Then: case is folded and equal names keep their registration order.
    assert_eq!(
        shown_order(&harness, &names),
        [
            "Alpha", "alpha", "archived", "delta", "echo", "empty", "zulu"
        ]
    );

    // When: sorted by recent activity.
    choose_sort(&mut harness, "Recent activity");
    // Then: newest first, ties by registration, and projects with no live thread
    // (empty, archived-only) last in registration order.
    assert_eq!(
        shown_order(&harness, &names),
        [
            "zulu", "echo", "delta", "Alpha", "alpha", "empty", "archived"
        ]
    );

    // When: the registration order is chosen again.
    choose_sort(&mut harness, "Registration order");
    // Then: it is restored, and sorting never touched the stored order.
    assert_eq!(shown_order(&harness, &names), names);
    assert_eq!(registered_ids(&harness), canonical);
    assert_eq!(
        harness.state().sidebar_view().project_sort,
        ProjectSort::Registration
    );
}

#[test]
fn project_search_from_a_sorted_row_is_scoped_to_that_project() {
    // Given: two projects whose name order is the reverse of their registration order.
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_of(temp.path(), &[("first", "zeta"), ("second", "alpha")]);
    let mut harness = harness(sidebar, [1280.0, 700.0]);
    choose_sort(&mut harness, "Name (A–Z)");
    assert_eq!(shown_order(&harness, &["zeta", "alpha"]), ["alpha", "zeta"]);

    // When: searching in the project that is now on top.
    harness.click_label("Search threads in alpha");
    harness.run();
    // Then: the search is scoped to its id, not to its position.
    assert_eq!(
        harness.state().thread_search().project(),
        Some(&ProjectId::new("second"))
    );
}

#[test]
fn project_settings_from_a_sorted_row_open_that_project() {
    // Given: a name sort that shows the project registered last on top.
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_of(temp.path(), &[("first", "zeta"), ("second", "alpha")]);
    let first_root = sidebar.projects[0].repo_root.display().to_string();
    let second_root = sidebar.projects[1].repo_root.display().to_string();
    let mut harness = harness(sidebar, [1280.0, 700.0]);
    choose_sort(&mut harness, "Name (A–Z)");

    // When: opening the settings of the second displayed row ("zeta").
    harness.click_nth_label("Project settings", 1);
    harness.run();

    // Then: it is the settings of the project registered first.
    assert!(harness.has_label(&first_root));
    assert!(!harness.has_label(&second_root));
}

#[test]
fn new_thread_from_a_sorted_row_belongs_to_that_project() {
    // Given: a name sort that puts the last-registered project first.
    let temp = tempfile::tempdir().expect("temp dir");
    let sidebar = sidebar_of(temp.path(), &[("first", "zeta"), ("second", "alpha")]);
    let mut harness = harness(sidebar, [1280.0, 700.0]);
    choose_sort(&mut harness, "Name (A–Z)");

    // When: a thread is started from the top row.
    harness.click_label("New thread in alpha");
    harness.run();

    // Then: it is stored under that project and the stored order is unchanged.
    let sidebar = harness.state().sidebar();
    assert_eq!(
        sidebar
            .threads
            .last()
            .map(|thread| thread.project_id.clone()),
        Some(ProjectId::new("second"))
    );
    assert_eq!(registered_ids(&harness), ["first", "second"]);
}

#[test]
fn bulk_fold_follows_and_drives_the_individual_folds() {
    // Given: two projects with a thread each, all open.
    let temp = tempfile::tempdir().expect("temp dir");
    let mut sidebar = sidebar_of(temp.path(), &[("a", "one"), ("b", "two")]);
    add_thread(&mut sidebar, "a", "thread-one", 1, false);
    add_thread(&mut sidebar, "b", "thread-two", 1, false);
    let mut harness = harness(sidebar, [1280.0, 700.0]);
    assert!(harness.has_label("thread-one") && harness.has_label("thread-two"));

    // When: one project is folded by hand.
    harness.click_label("Collapse threads of one");
    harness.run();
    // Then: another is still open, so the bulk control still offers to fold.
    assert!(!harness.has_label("thread-one") && harness.has_label("thread-two"));
    assert!(harness.has_label(COLLAPSE_ALL));

    // When: everything is folded in one go.
    harness.click_label(COLLAPSE_ALL);
    harness.run();
    // Then: no thread shows, each row offers to expand, and the bulk control flips.
    assert!(!harness.has_label("thread-one") && !harness.has_label("thread-two"));
    assert!(
        harness.has_label("Expand threads of one") && harness.has_label("Expand threads of two")
    );
    assert!(harness.has_label(EXPAND_ALL) && !harness.has_label(COLLAPSE_ALL));

    // When: a single project is opened again.
    harness.click_label("Expand threads of two");
    harness.run();
    // Then: the bulk control is back to folding.
    assert!(harness.has_label("thread-two") && harness.has_label(COLLAPSE_ALL));

    // When: everything is unfolded in one go.
    harness.click_label("Collapse threads of two");
    harness.run();
    harness.click_label(EXPAND_ALL);
    harness.run();
    // Then: every project is open again.
    assert!(harness.has_label("thread-one") && harness.has_label("thread-two"));
    assert!(harness.state().sidebar_view().is_default());
}

#[test]
fn bulk_fold_is_disabled_without_projects() {
    // Given/When: an empty sidebar.
    let harness = harness(SidebarState::default(), [1280.0, 700.0]);
    // Then: the description stays, the one add control stays, and the bulk control is inert.
    assert!(harness.has_label("No projects yet"));
    assert_eq!(harness.count_labels(ADD), 1);
    assert!(harness.state().sidebar_view().is_default());
    assert_eq!(harness.label_enabled(EXPAND_ALL), Some(false));
}

#[test]
fn sort_and_folds_survive_a_restart_through_the_ui_settings_file() {
    // Given: a workbench with a UI settings path, saved earlier with a project that
    // has since been removed.
    let temp = tempfile::tempdir().expect("temp dir");
    let settings_path = temp.path().join("ui.toml");
    let make_sidebar = || {
        let mut sidebar = sidebar_of(
            temp.path(),
            &[("a", "charlie"), ("b", "alpha"), ("c", "bravo")],
        );
        add_thread(&mut sidebar, "a", "thread-charlie", 1, false);
        add_thread(&mut sidebar, "b", "thread-alpha", 1, false);
        sidebar
    };
    let mut stale = UiSettings::default();
    stale
        .sidebar_view
        .collapsed_projects
        .insert(ProjectId::new("gone"));
    workspace_ui::save_settings(&stale, &settings_path).expect("seed settings");
    let load = || workspace_ui::load_settings(&settings_path).expect("settings");
    let first = WorkbenchState::new(MockSource::default(), &load())
        .expect("state")
        .with_ui_settings_path(&settings_path)
        .with_sidebar(make_sidebar());
    let mut session = HeadlessWorkbench::new(first, [1280.0, 900.0]);
    session.run();

    // When: the operator sorts by name and folds one project.
    choose_sort(&mut session, "Name (A–Z)");
    session.click_label("Collapse threads of alpha");
    session.run();

    // Then: the settings file records both, and the removed project is not carried along.
    let saved = load().sidebar_view;
    assert_eq!(saved.project_sort, ProjectSort::Name);
    assert_eq!(
        saved.collapsed_projects.iter().collect::<Vec<_>>(),
        [&ProjectId::new("b")]
    );

    // When: a new process starts from that file.
    let second = WorkbenchState::new(MockSource::default(), &load())
        .expect("state")
        .with_sidebar(make_sidebar());
    let mut restarted = HeadlessWorkbench::new(second, [1280.0, 900.0]);
    restarted.run();

    // Then: the order and folds are as they were left, and the stored order is not.
    let names = ["charlie", "alpha", "bravo"];
    assert_eq!(
        shown_order(&restarted, &names),
        ["alpha", "bravo", "charlie"]
    );
    assert!(restarted.has_label("Expand threads of alpha"));
    assert!(!restarted.has_label("thread-alpha"));
    assert!(restarted.has_label("thread-charlie"));
    assert_eq!(registered_ids(&restarted), ["a", "b", "c"]);

    // When: the registration order is chosen again after the restart.
    choose_sort(&mut restarted, "Registration order");
    // Then: it can always be returned to.
    assert_eq!(shown_order(&restarted, &names), names);
}
