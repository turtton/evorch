use gui::{app::WorkbenchState, fixture::DemoSource, headless::HeadlessWorkbench};
use storage::improvement::{
    ImprovementSeverity, ImprovementSource, ImprovementWritePolicy, NewImprovementCandidate,
};
use storage::{Storage, StorageConfig};
use workspace_ui::{PanelId, ProjectId, SidebarState, UiSettings};

fn repo(root: &std::path::Path, slug: &str) {
    std::fs::create_dir_all(root).unwrap();
    for args in [vec!["init"], vec!["remote", "add", "origin", slug]] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn companion_tab_uses_selected_repository_identity_and_updates_without_clicks() {
    let dir = tempfile::tempdir().unwrap();
    let first_root = dir.path().join("first");
    let second_root = dir.path().join("second");
    repo(&first_root, "https://github.com/example/repo.git");
    repo(&second_root, "git@github.com:example/other.git");
    let config = StorageConfig {
        db_path: dir.path().join("store.db"),
        ..Default::default()
    };
    let store = Storage::open(config.clone()).unwrap();
    let record = |root: &std::path::Path, id: &str| {
        // Match the runtime collector's storage key, not the display ID.
        store
            .handle()
            .record_improvement_candidate(
                &gui::runtime_sink::derive_repo_slug(root),
                NewImprovementCandidate {
                    id: id.into(),
                    source: ImprovementSource::Diagnostic,
                    severity: ImprovementSeverity::Warning,
                    code: "NoProgress".into(),
                    title: format!("Candidate {id}"),
                    evidence: "Repeated tools".into(),
                    dedup_key: id.into(),
                    run_id: None,
                },
                ImprovementWritePolicy {
                    duplicate_cooldown: std::time::Duration::ZERO,
                    daily_limit: 20,
                    max_candidates: 200,
                },
            )
            .unwrap();
    };
    let mut sidebar = SidebarState::default();
    let first = ProjectId::new("display-id");
    let second = ProjectId::new("another-display-id");
    sidebar
        .add_project(first.clone(), "First project", &first_root)
        .unwrap();
    sidebar
        .add_project(second.clone(), "Second project", &second_root)
        .unwrap();
    sidebar.select_project(&first).unwrap();
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_memory_storage(config)
        .with_self_improvement(store.handle(), true, None);
    let path = state
        .dock()
        .find_tab(&PanelId::new("self-improvement-main"))
        .unwrap();
    state.dock_mut().set_active_tab(path).unwrap();
    let mut harness = HeadlessWorkbench::new(state, [1200.0, 900.0]);
    harness.run();
    assert!(harness.has_label("No improvement candidates yet."));
    record(&first_root, "first");
    harness.input_mut().time = Some(10.0);
    harness.run();
    assert!(harness.has_label("Candidate first"));
    assert!(!harness.has_label("Refresh"));
    record(&second_root, "second");
    harness.state_mut().select_project(second).unwrap();
    harness.step();
    assert!(harness.has_label("Candidate second"));
    assert!(!harness.has_label("Candidate first"));
}
