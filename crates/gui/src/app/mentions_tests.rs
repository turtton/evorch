use std::path::Path;

use workspace_ui::{ProjectId, SidebarState, UiSettings};

use super::*;
use crate::fixture::DemoSource;
use crate::model::composer::{CompletionItem, mention_at};

fn folder(parent: &Path, name: &str, file: &str) -> PathBuf {
    let folder = parent.join(name);
    std::fs::create_dir_all(&folder).expect("folder");
    std::fs::write(folder.join(file), "").expect("file");
    folder.canonicalize().expect("canonical folder")
}

/// The `/`-separated absolute form completions insert for `path`.
fn slash_form(path: &Path) -> String {
    path.to_str()
        .expect("utf-8 temp path")
        .replace(std::path::MAIN_SEPARATOR, "/")
}

/// What `index` offers for each of `queries`, in order.
fn offered(index: &MentionIndex, queries: &[String]) -> Vec<Vec<CompletionItem>> {
    queries
        .iter()
        .map(|query| index.complete(&mention_at(query, query.len()).expect("mention")))
        .collect()
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

#[test]
fn a_revoked_directory_makes_room_for_the_rest_without_closing_the_mention() {
    // Given: an open mention whose index spent a small path budget on `a`,
    // leaving none for `b`'s file.
    let temp = tempfile::tempdir().expect("temp");
    let root = folder(temp.path(), "project", "lib.rs");
    let a = folder(temp.path(), "a", "a.md");
    let b = folder(temp.path(), "b", "b.md");
    let (mut state, project) = mention_state(&root, &[&a, &b]);
    let ctx = egui::Context::default();
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();
    let scope = state.mention_scope().expect("scope");
    let skills = runtime::skill::build_standard_registry(Some(&root));
    state.composer.mentions = MentionIndex::build_within(&scope.root, &scope.allowed, &skills, 2);
    assert!(has_entry(&state, "/a/a.md"));
    assert!(!has_entry(&state, "/b/b.md"));
    // An index built for the current scope is kept, budget-limited or not.
    state.refresh_mention_index(&ctx);
    assert!(state.mention_job.is_none());

    // When: `a` is revoked while the mention stays open.
    state
        .set_allowed_trust(&project, &a, TrustState::Unapproved)
        .expect("revoke");
    state.refresh_mention_index(&ctx);

    // Then: `a` stops completing on that frame, and a rebuild starts...
    assert!(!has_entry(&state, "/a/a.md"));
    assert!(state.mention_job.is_some());
    // ...which indexes `b`'s file for the still open mention.
    state.wait_mention_index();
    assert!(has_entry(&state, "/b/b.md"));
    assert!(
        state
            .composer
            .mentions
            .is_built_for(&root, std::slice::from_ref(&b))
    );
    // Then: later frames start no further rebuild.
    for _ in 0..3 {
        state.refresh_mention_index(&ctx);
        assert!(state.mention_job.is_none());
    }
}

#[test]
fn revoking_a_parent_offers_what_a_fresh_index_of_the_nested_directory_does() {
    // Given: an open mention over an allowed directory and its parent, added after it.
    let temp = tempfile::tempdir().expect("temp");
    let root = folder(temp.path(), "project", "lib.rs");
    let outer = folder(temp.path(), "outer", "top.md");
    let inner = folder(&outer, "inner", "deep.md");
    let (mut state, project) = mention_state(&root, &[&inner, &outer]);
    let ctx = egui::Context::default();
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();
    let skills = runtime::skill::build_standard_registry(Some(&root));
    let fresh = MentionIndex::build(&root, std::slice::from_ref(&inner), &skills);
    let queries: Vec<String> = ["@", "@deep", "@inner", "@outer", "@top", "@md"]
        .into_iter()
        .map(str::to_owned)
        .chain([
            format!("@{}/", slash_form(&outer)),
            format!("@{}/", slash_form(&inner)),
        ])
        .collect();
    assert_ne!(
        offered(&state.composer.mentions, &queries),
        offered(&fresh, &queries)
    );

    // When: the parent is revoked while the mention stays open.
    state
        .set_allowed_trust(&project, &outer, TrustState::Unapproved)
        .expect("revoke");
    state.refresh_mention_index(&ctx);

    // Then: that frame already offers what an index of the nested directory
    // alone does, in the same order, its root included in an empty query.
    let now = offered(&state.composer.mentions, &queries);
    assert_eq!(now, offered(&fresh, &queries));
    let root_label = format!("@{}/", slash_form(&inner));
    assert!(now[0].iter().any(|item| item.label == root_label));
    // Then: the rebuild it starts lands on exactly that fresh index.
    assert!(state.mention_job.is_some());
    state.wait_mention_index();
    assert_eq!(state.composer.mentions, fresh);
}

#[test]
fn approving_a_parent_that_ignores_a_nested_directory_keeps_it_completing() {
    // Given: an approved directory that its parent's repository ignores, with
    // a mention for its file open.
    let temp = tempfile::tempdir().expect("temp");
    let root = folder(temp.path(), "project", "lib.rs");
    let outer = folder(temp.path(), "outer", "top.md");
    let inner = folder(&outer, "inner", "deep.md");
    let mut init = std::process::Command::new("git");
    for (variable, _) in std::env::vars_os() {
        if variable.to_string_lossy().starts_with("GIT_") {
            init.env_remove(variable);
        }
    }
    let status = init
        .args(["init", "--quiet"])
        .current_dir(&outer)
        .status()
        .expect("git init");
    assert!(status.success());
    std::fs::write(outer.join(".gitignore"), "inner/\n").expect("ignore");
    let (mut state, project) = mention_state(&root, &[&inner]);
    state.composer.input = "@deep".into();
    let ctx = egui::Context::default();
    let deep = format!("@{}/deep.md ", slash_form(&inner));
    let first = |state: &WorkbenchState<DemoSource>| {
        state
            .composer
            .completion_items()
            .first()
            .map(|item| item.replacement.clone())
    };

    // When: it is the only allowed directory.
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();
    // Then: its file is found by name and inserted as an absolute path.
    assert_eq!(first(&state).as_ref(), Some(&deep));

    // When: the parent is approved too while the mention stays open.
    state
        .add_allowed_directory(&project, &outer)
        .expect("approve parent");
    state.refresh_mention_index(&ctx);
    state.wait_mention_index();

    // Then: the rebuilt index covers both, and the file still completes the same way.
    assert!(
        state
            .composer
            .mentions
            .is_built_for(&root, &[inner.clone(), outer.clone()])
    );
    assert_eq!(first(&state).as_ref(), Some(&deep));
    assert!(has_entry(&state, "/outer/top.md"));
}
