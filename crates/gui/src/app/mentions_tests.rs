use std::path::Path;

use workspace_ui::{ProjectId, SidebarState, UiSettings};

use super::*;
use crate::fixture::DemoSource;

fn folder(parent: &Path, name: &str, file: &str) -> PathBuf {
    let folder = parent.join(name);
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join(file), "").expect("file");
    folder.canonicalize().expect("canonical folder")
}

fn has_entry(state: &WorkbenchState<DemoSource>, suffix: &str) -> bool {
    state
        .composer
        .mentions
        .entries
        .iter()
        .any(|entry| entry.value.ends_with(suffix))
}

/// A workbench on `root` with `allowed` approved and an `@` mention open.
fn mention_state(root: &Path, allowed: &[&Path]) -> (WorkbenchState<DemoSource>, ProjectId) {
    let project = ProjectId::new("project");
    let mut sidebar = SidebarState::default();
    sidebar
        .add_project(project.clone(), "project", root)
        .expect("project");
    for directory in allowed {
        sidebar
            .add_allowed_directory(&project, directory, TrustState::Approved)
            .expect("allowed directory");
    }
    sidebar.select_project(&project).expect("select");
    let mut state = WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
        .expect("state")
        .with_sidebar(sidebar);
    state.composer.input = "@".into();
    (state, project)
}

#[test]
fn an_index_built_for_a_previous_scope_is_discarded() {
    // Given: a project with one approved allowed directory, indexed while a mention is open.
    let temp = tempfile::tempdir().expect("temp");
    let root = folder(temp.path(), "project", "lib.rs");
    let kept = folder(temp.path(), "kept", "keep.md");
    let removed = folder(temp.path(), "removed", "drop.md");
    let (mut state, project) = mention_state(&root, &[&kept]);
    let ctx = egui::Context::default();
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();
    assert!(has_entry(&state, "/kept/keep.md"));

    for wait in [false, true] {
        let before = state.composer.mentions.clone();
        // Given: a directory is approved and a rebuild covering it starts,
        // listing a project file that appeared meanwhile...
        state
            .add_allowed_directory(&project, &removed)
            .expect("approve");
        let scope = state.mention_scope().expect("scope");
        assert_eq!(scope.allowed, [kept.clone(), removed.clone()]);
        std::fs::write(root.join(format!("new-{wait}.rs")), "").expect("new file");
        let (send, result) = mpsc::sync_channel(1);
        let skills = runtime::skill::discover_skills(&[]);
        send.send(MentionIndex::build(&scope.root, &scope.allowed, &skills))
            .expect("send index");
        state.mention_job = Some(Job { scope, result });
        // ...but the directory is removed before that rebuild lands.
        state
            .remove_allowed_directory(&project, &removed)
            .expect("remove");

        // When: the rebuild lands on the next frame, or in a blocking wait.
        if wait {
            state.wait_mention_index();
        } else {
            state.refresh_mention_index(&ctx);
        }

        // Then: it is dropped whole, and the current index already matches the scope.
        assert!(state.mention_job.is_none(), "wait: {wait}");
        assert!(!has_entry(&state, "/removed/drop.md"), "wait: {wait}");
        assert!(
            !has_entry(&state, &format!("new-{wait}.rs")),
            "wait: {wait}"
        );
        assert_eq!(state.composer.mentions, before, "wait: {wait}");
        assert_eq!(state.composer.mentions.allowed, std::slice::from_ref(&kept));
    }
}

#[test]
fn a_missing_approved_directory_is_skipped_without_rebuilding_every_frame() {
    // Given: an approved allowed directory that was deleted after approval.
    let temp = tempfile::tempdir().expect("temp");
    let root = folder(temp.path(), "project", "lib.rs");
    let gone = folder(temp.path(), "gone", "old.md");
    let (mut state, _) = mention_state(&root, &[&gone]);
    std::fs::remove_dir_all(&gone).expect("delete");
    let ctx = egui::Context::default();

    // When: the mention's index is built.
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();

    // Then: the directory adds nothing, yet the index matches the scope, so
    // later frames start no further rebuild.
    assert!(!has_entry(&state, "/gone"));
    assert_eq!(state.composer.mentions.allowed, [gone]);
    for _ in 0..3 {
        state.refresh_mention_index(&ctx);
        assert!(state.mention_job.is_none());
    }
}
