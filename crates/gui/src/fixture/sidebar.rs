//! demo 用 sidebar 構築。

use std::path::{Path, PathBuf};

use workspace_ui::{ProjectError, ProjectId, SidebarState, ThreadError, ThreadId, TrustState};

/// demo sidebar 構築の失敗。
#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    /// `root` が実在ディレクトリではない。
    #[error("demo root directory does not exist: {0}")]
    MissingRoot(PathBuf),
    #[error("demo filesystem setup failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("demo project setup failed: {0}")]
    Project(#[from] ProjectError),
    #[error("demo thread setup failed: {0}")]
    Thread(#[from] ThreadError),
}

/// 2 projects / 4 threads の demo sidebar を構築する。
///
/// `root` 配下に `evorch` と `intent-cli` の実ディレクトリを作成する。
/// 許可 directory は project root 外である必要がある (`validate_allowed_directory`
/// は root 内 nested を `NestedInProjectRoot` で拒否する) ため、`shared` (approved)
/// と `scratch` (unapproved) を root 直下に作る。
///
/// # Errors
/// `root` が存在しない場合や directory 作成・sidebar 操作に失敗した場合は
/// [`FixtureError`] を返す。
pub fn demo_sidebar(root: &Path) -> Result<SidebarState, FixtureError> {
    if !root.is_dir() {
        return Err(FixtureError::MissingRoot(root.to_path_buf()));
    }
    let evorch = root.join("evorch");
    let shared = root.join("shared");
    let scratch = root.join("scratch");
    let intent_cli = root.join("intent-cli");
    std::fs::create_dir_all(&evorch)?;
    std::fs::create_dir_all(&shared)?;
    std::fs::create_dir_all(&scratch)?;
    std::fs::create_dir_all(&intent_cli)?;

    let mut sidebar = SidebarState::default();
    let evorch_id = ProjectId::new("evorch");
    sidebar.add_project(evorch_id.clone(), "evorch", &evorch)?;
    sidebar.select_project(&evorch_id)?;
    sidebar.add_allowed_directory(&evorch_id, &shared, TrustState::Approved)?;
    sidebar.add_allowed_directory(&evorch_id, &scratch, TrustState::Unapproved)?;
    let intent_cli_id = ProjectId::new("intent-cli");
    sidebar.add_project(intent_cli_id.clone(), "intent-cli", &intent_cli)?;

    let thread_1 = ThreadId::new("thread-1");
    sidebar.create_thread(
        thread_1.clone(),
        evorch_id.clone(),
        "Refine GUI design system",
    )?;
    let thread_2 = ThreadId::new("thread-2");
    sidebar.create_thread(
        thread_2.clone(),
        evorch_id.clone(),
        "Provider composition root",
    )?;
    let thread_3 = ThreadId::new("thread-3");
    sidebar.create_thread(thread_3.clone(), evorch_id, "Fix flaky offscreen test")?;
    sidebar.create_thread(ThreadId::new("thread-4"), intent_cli_id, "Queue seed CLI")?;
    sidebar.set_pinned(&thread_2, true)?;
    sidebar.set_paused(&thread_3, true)?;
    sidebar.switch_thread(&thread_1)?;
    Ok(sidebar)
}
