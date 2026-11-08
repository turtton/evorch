use super::*;

#[test]
fn project_directory_preserves_sidebar_ids_without_writing_sidebar_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let sidebar = serde_json::json!({
        "projects": [{"id":"desktop-project", "repo_root":root, "name":"Project"}],
        "threads": [{"draft":"must not change"}]
    })
    .to_string();
    std::fs::write(root.join("sidebar.json"), &sidebar).unwrap();
    let options = BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    };
    let projects = known_projects(&options).unwrap().entries();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].0.as_str(), "desktop-project");
    assert_eq!(projects[0].1, root);
    assert_eq!(
        std::fs::read_to_string(options.config_dir.join("sidebar.json")).unwrap(),
        sidebar
    );
}

#[tokio::test]
async fn bootstrap_drains_before_releasing_its_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    let _permit = boot.owner.start("owned").unwrap();
    assert!(!boot.busy());
    boot.shutdown().await.unwrap();
    assert_eq!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}
