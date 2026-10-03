use super::*;
use std::path::Path;
use std::sync::Mutex;
use workspace_ui::{ThreadId, ThreadRecord};

#[derive(Clone, Default)]
struct LiveSource(Arc<Mutex<HashMap<RunId, AgentInspection>>>);

impl AgentRunSource for LiveSource {
    fn list(&self) -> Vec<AgentSummary> {
        Vec::new()
    }

    fn inspect(&self, run_id: RunId) -> Option<AgentInspection> {
        self.0.lock().unwrap().get(&run_id).cloned()
    }
}

impl LiveSource {
    fn set(&self, run: AgentInspection) {
        self.0.lock().unwrap().insert(run.run_id, run);
    }
}

fn shared(run_id: u64, root: &Path) -> AgentInspection {
    AgentInspection {
        run_id: RunId::new(run_id),
        role_name: "Worker".into(),
        phase: AgentRunPhase::Running,
        message_count: 0,
        workspace: Some(WorkspaceInspection {
            mode: WorkspaceMode::Shared,
            branch: None,
            worktree_path: None,
            active_root: Some(root.into()),
            merge_mode: MergeMode::Branch,
        }),
    }
}

fn thread(sidebar: &mut SidebarState, id: &str, runs: &[&str]) {
    let mut thread = ThreadRecord::new(ThreadId::new(id), ProjectId::new("demo"), id);
    thread.run_ids = runs.iter().map(|run| (*run).into()).collect();
    sidebar.threads.push(thread);
}

fn changed(run_id: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from,
        to,
        reason: None,
    })
}

#[test]
fn first_reconciliation_covers_inactive_threads_and_prefers_isolated_in_any_order() {
    let temp = tempfile::tempdir().unwrap();
    let mut sidebar = sidebar_with_project(temp.path());
    thread(&mut sidebar, "active", &["run-1"]);
    thread(
        &mut sidebar,
        "inactive",
        &["run-1", "bad-id", "run-99", "run-2", "run-3"],
    );
    thread(&mut sidebar, "reverse", &["run-3", "run-2", "run-1"]);
    sidebar.switch_thread(&ThreadId::new("active")).unwrap();
    let source = LiveSource::default();
    let shared_root = temp.path().join("shared");
    let worktree = temp.path().join("worktree");
    source.set(shared(1, &shared_root));
    let mut detached = inspection(2, "detached", temp.path().join("detached"));
    detached.workspace.as_mut().unwrap().worktree_path = None;
    // Even stale isolated active_root is never treated as shared.
    source.set(detached);
    source.set(inspection(3, "isolated", worktree.clone()));
    let state = WorkbenchState::new(source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    // No lifecycle event or frame is needed for the initial reconciliation.
    assert_eq!(state.sidebar().threads[0].active_root, Some(shared_root));
    for thread in &state.sidebar().threads[1..] {
        assert_eq!(thread.branch.as_deref(), Some("isolated"));
        assert_eq!(thread.worktree_path.as_ref(), Some(&worktree));
        assert_eq!(thread.active_root.as_ref(), Some(&worktree));
    }
}

#[test]
fn shared_label_is_visible_only_when_root_differs_from_project() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let other = root.join("other");
    let source = LiveSource::default();
    source.set(shared(1, &root));
    source.set(shared(2, &other));
    let mut sidebar = sidebar_with_project(&root);
    thread(&mut sidebar, "same-root", &["run-1"]);
    thread(&mut sidebar, "different-root", &["run-2"]);
    sidebar.switch_thread(&ThreadId::new("same-root")).unwrap();
    let state = WorkbenchState::new(source, &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    let mut harness = HeadlessWorkbench::new(state, [1000.0, 800.0]);
    harness.run();
    assert!(!harness.has_label(&format!("Shared workspace: {}", root.display())));
    assert!(harness.has_label(&format!("Shared workspace: {}", other.display())));
}

#[test]
fn cleanup_handoff_and_same_id_restart_refresh_and_persist_all_fields() {
    let temp = tempfile::tempdir().unwrap();
    let source = LiveSource::default();
    let worktree = temp.path().join("worktree");
    source.set(inspection(1, "branch", worktree.clone()));
    let mut sidebar = sidebar_with_project(temp.path());
    thread(&mut sidebar, "thread-1", &["run-1"]);
    let path = temp.path().join("sidebar.json");
    let mut state = WorkbenchState::new(source.clone(), &UiSettings::default())
        .unwrap()
        .with_sidebar_path(path.clone())
        .with_sidebar(sidebar);
    assert_eq!(
        workspace_ui::load_sidebar(&path).unwrap().threads[0].active_root,
        Some(worktree.clone())
    );

    // Stopped retains the assignment, and an identical RunId can later be inspected again.
    state.apply_events([changed(
        "run-1",
        AgentRunPhase::Running,
        AgentRunPhase::Stopped,
    )]);
    assert_eq!(
        state.sidebar().threads[0].active_root,
        Some(worktree.clone())
    );
    let next = temp.path().join("reattached");
    source.set(inspection(1, "resumed", next.clone()));
    state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-1".into(),
        parent_run_id: None,
        agent_name: "chat:Worker:thread-1".into(),
        role: "worker".into(),
    })]);
    assert_eq!(state.sidebar().threads[0].active_root, Some(next));

    // A new adopter wins even though the earlier source has a stale active_root.
    let mut detached = inspection(1, "resumed", worktree.clone());
    detached.workspace.as_mut().unwrap().worktree_path = None;
    source.set(detached);
    source.set(inspection(2, "adopted", worktree.clone()));
    state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "run-2".into(),
        parent_run_id: None,
        agent_name: "chat:Worker:thread-1".into(),
        role: "worker".into(),
    })]);
    assert_eq!(
        state.sidebar().threads[0].branch.as_deref(),
        Some("adopted")
    );
    let mut cleaned = inspection(2, "adopted", worktree);
    let workspace = cleaned.workspace.as_mut().unwrap();
    workspace.worktree_path = None;
    workspace.active_root = None;
    source.set(cleaned);
    state.apply_events([changed(
        "run-2",
        AgentRunPhase::Running,
        AgentRunPhase::Done,
    )]);
    for sidebar in [
        state.sidebar().clone(),
        workspace_ui::load_sidebar(&path).unwrap(),
    ] {
        let thread = &sidebar.threads[0];
        assert_eq!(thread.branch, None);
        assert_eq!(thread.worktree_path, None);
        assert_eq!(thread.active_root, None);
    }
}

#[test]
fn restore_history_reconciles_and_fork_does_not_inherit_root() {
    let temp = tempfile::tempdir().unwrap();
    let source = LiveSource::default();
    let mut sidebar = sidebar_with_project(temp.path());
    thread(&mut sidebar, "thread-1", &["run-1"]);
    sidebar.switch_thread(&ThreadId::new("thread-1")).unwrap();
    let mut state = WorkbenchState::new(source.clone(), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar);
    let root = temp.path().join("shared");
    source.set(shared(1, &root));
    let config = storage::StorageConfig {
        db_path: temp.path().join("history.db"),
        ..Default::default()
    };
    state
        .restore_history(&storage::Database::open(&config).unwrap())
        .unwrap();
    assert_eq!(state.sidebar().threads[0].active_root, Some(root));
    let fork = state.fork_thread(ThreadId::new("thread-1")).unwrap();
    let fork = state
        .sidebar()
        .threads
        .iter()
        .find(|thread| thread.id == fork)
        .unwrap();
    assert_eq!(fork.active_root, None);
    assert_eq!(fork.worktree_path, None);
    assert_eq!(fork.branch, None);
}
