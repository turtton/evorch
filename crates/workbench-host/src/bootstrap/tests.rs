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
    let projects = known_projects(&options).unwrap().directory.entries();
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
    let owner = boot.owner.clone();
    let (stopped, completed) = tokio::sync::oneshot::channel();
    boot.services.start("test-service", |stop| async move {
        stop.await?;
        assert_eq!(
            owner.attach("owned")?.state,
            runtime::ownership::OwnerState::Quiescing
        );
        stopped.send(()).unwrap();
        Ok(())
    });
    assert!(!boot.busy());
    boot.shutdown().await.unwrap();
    completed.await.unwrap();
    assert_eq!(
        boot.owner.attach("owned").unwrap().state,
        runtime::ownership::OwnerState::Released
    );
}

#[tokio::test]
async fn saved_access_only_declares_approved_existing_directories_without_rewriting_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let allowed = root.join("allowed");
    let rejected = root.join("unapproved");
    std::fs::create_dir(&allowed).unwrap();
    std::fs::create_dir(&rejected).unwrap();
    let sidebar = serde_json::json!({
        "projects": [{"id":"desktop", "repo_root":root, "trust":"approved",
            "allowed_directories": [
                {"path": allowed, "trust":"approved"},
                {"path": allowed, "trust":"approved"},
                {"path": rejected, "trust":"unapproved"},
                {"path": root.join("missing"), "trust":"approved"}
            ]}],
        "threads": [{"draft":"untouched"}]
    })
    .to_string();
    std::fs::write(root.join("sidebar.json"), &sidebar).unwrap();
    let mut boot = BootstrappedHost::open(BootstrapOptions {
        config_dir: root.clone(),
        project_root: root.clone(),
        ownership_root: root.join("owners"),
    })
    .await
    .unwrap();
    assert_eq!(
        boot.runtime
            .project_allowed_directories(&root)
            .get()
            .as_ref(),
        &[allowed]
    );
    boot.shutdown().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("sidebar.json")).unwrap(),
        sidebar
    );
}

#[test]
fn absent_access_is_unapproved_and_unknown_trust_does_not_grant_access() {
    let access: components::ProjectAccess = serde_json::from_str("{}").unwrap();
    assert!(matches!(access.trust, components::Trust::Unapproved));
    assert!(access.allowed_directories.is_empty());
    assert!(serde_json::from_str::<components::ProjectAccess>(r#"{"trust":"trusted"}"#).is_err());
}
