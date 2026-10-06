//! Switching the active project rebinds every project-scoped runtime input.
use super::comment_checker_support::{
    self as support, ScriptedModel, text_response, tool_response,
};
use super::*;
use crate::rules::{ProjectTrust, RulesSettings};
use crate::workspace::Project;
use sandbox::DirectSandbox;

fn canonical_repo() -> (tempfile::TempDir, PathBuf) {
    let (temp, repo) = support::init_git_repo();
    let repo = repo.canonicalize().unwrap();
    (temp, repo)
}

fn write_skill(root: &std::path::Path, name: &str) {
    let directory = root.join(".evorch/skills").join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\nbody"),
    )
    .unwrap();
}

fn runtime_on(
    bus: &Arc<EventBus>,
    model: Arc<ScriptedModel>,
    root: &std::path::Path,
    factory: Arc<dyn SandboxFactory>,
) -> AgentRuntime {
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    AgentRuntime::with_workspace_context(
        Arc::clone(bus),
        Arc::new(executor),
        model,
        WorktreeManager::new(Project::new(root.to_path_buf()).unwrap()),
        factory,
    )
    .with_project_rules(Arc::new(RulesSource::new(
        ProjectTrust::Unapproved,
        RulesSettings::from(&config::RulesConfig::default()),
        None,
        Some(root.to_path_buf()),
        None,
    )))
    .with_sandbox_root(root.to_path_buf())
}

// Regression: the startup project leaked into the workspace note, so the model
// passed a `cwd` the switched sandbox never mounted (bwrap: "Can't chdir").
#[tokio::test]
async fn project_switch_rebinds_shared_workspace_rules_skills_and_snapshots() {
    let (_startup_temp, startup) = canonical_repo();
    let (_switched_temp, switched) = canonical_repo();
    write_skill(&startup, "startup-skill");
    write_skill(&switched, "switched-skill");
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "write",
            "write",
            serde_json::json!({"path": "source.rs", "content": "fn main() {}"}),
        )),
        Ok(text_response("done", providers::FinishReason::Stop)),
    ]));
    let (factory, mounts) = support::recording_factory();
    let snapshot_store = tempfile::tempdir().unwrap();
    let runtime = runtime_on(&bus, Arc::clone(&model), &startup, factory)
        .with_skill_source(Arc::new(SkillCatalogSource::new(
            config::Config::default(),
            None,
            Vec::new(),
            crate::skill::repo_skill_dirs(&startup).into(),
            Arc::clone(&bus),
        )))
        .with_snapshots(Arc::new(
            crate::snapshot::SnapshotService::new(&startup, snapshot_store.path()).unwrap(),
        ));

    runtime.set_project_root(switched.clone()).unwrap();
    let run = runtime.delegate_background(Role::Worker, "edit".into(), RunConfig::default());

    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let mounted: Vec<_> = mounts
        .lock()
        .unwrap()
        .iter()
        .map(|mounts| mounts.workspace_root.clone())
        .collect();
    assert_eq!(mounted, std::slice::from_ref(&switched));
    let note = crate::base_context::workspace_note(&switched);
    let system = &model.observed().await[0][0];
    assert_eq!(system.role, providers::Role::System);
    assert!(
        system
            .content
            .iter()
            .any(|block| matches!(block, providers::ContentBlock::Text { text } if *text == note))
    );
    assert_eq!(
        runtime
            .inspect_agent(run)
            .unwrap()
            .workspace
            .unwrap()
            .active_root,
        Some(switched.clone()),
    );
    assert!(switched.join("source.rs").exists());
    assert!(!startup.join("source.rs").exists());
    assert_eq!(
        runtime.shared.rules().unwrap().project_root(),
        Some(switched.as_path()),
    );
    let skills: Vec<_> = runtime
        .shared
        .skill_source
        .get()
        .unwrap()
        .snapshot()
        .registry
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    assert!(skills.contains(&"switched-skill".to_owned()), "{skills:?}");
    assert!(!skills.contains(&"startup-skill".to_owned()), "{skills:?}");

    // The pre-write checkpoint belongs to the switched project, so undo removes
    // the file there instead of restoring the startup project.
    assert!(
        runtime
            .restore_snapshot(run, false)
            .await
            .unwrap()
            .is_some()
    );
    assert!(!switched.join("source.rs").exists());
}

#[tokio::test]
async fn project_switch_moves_isolated_worktrees_and_fails_closed_outside_git() {
    let (_startup_temp, startup) = canonical_repo();
    let (_switched_temp, switched) = canonical_repo();
    let plain = tempfile::tempdir().unwrap();
    let plain = plain.path().canonicalize().unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        providers::FinishReason::Stop,
    ))]));
    let (factory, mounts) = support::recording_factory();
    let runtime = runtime_on(&bus, model, &startup, factory);
    let isolated = || RunConfig {
        workspace_mode: crate::WorkspaceMode::Isolated,
        ..RunConfig::default()
    };

    runtime.set_project_root(switched.clone()).unwrap();
    let run = runtime.delegate_background(Role::Worker, "isolated".into(), isolated());
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let worktree = mounts.lock().unwrap()[0].workspace_root.clone();
    assert!(
        worktree.starts_with(switched.join(".evorch/worktrees")),
        "{}",
        worktree.display()
    );

    // A non-repository project has no worktree manager; isolated runs must not
    // fall back to the previous repository.
    runtime.set_project_root(plain.clone()).unwrap();
    let run = runtime.delegate_background(Role::Worker, "isolated".into(), isolated());
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Error);
    assert_eq!(mounts.lock().unwrap().len(), 1);
    assert!(!plain.join(".evorch").exists());
    assert!(!startup.join(".evorch").exists());
}

fn delegate_script() -> [Result<providers::ChatResponse, RuntimeError>; 2] {
    [
        Ok(tool_response(
            "delegate",
            "delegate",
            serde_json::json!({"target": {"role": "worker"}, "prompt": "CHILD write"}),
        )),
        Ok(text_response("parent done", providers::FinishReason::Stop)),
    ]
}

fn child_script() -> [Result<providers::ChatResponse, RuntimeError>; 2] {
    [
        Ok(tool_response(
            "write",
            "write",
            serde_json::json!({"path": "child.rs", "content": "fn child() {}"}),
        )),
        Ok(text_response("child done", providers::FinishReason::Stop)),
    ]
}

fn child_of(runtime: &AgentRuntime, parent: RunId) -> RunId {
    runtime
        .list_agents()
        .into_iter()
        .find(|agent| agent.parent_run_id == Some(parent))
        .expect("delegated child run")
        .run_id
}

// A run bound to another project uses that project's sandbox, model and files,
// and its delegated children inherit the binding instead of the active project.
#[tokio::test]
async fn explicit_project_binds_root_and_children_without_switching_the_active_project() {
    let (_active_temp, active) = canonical_repo();
    let (_bound_temp, bound) = canonical_repo();
    let bus = Arc::new(EventBus::new(256));
    let active_model = Arc::new(ScriptedModel::new([]));
    let bound_model = Arc::new(ScriptedModel::new([]));
    bound_model.add_keyed("ORCH", delegate_script()).await;
    bound_model.add_keyed("CHILD", child_script()).await;
    let (factory, mounts) = support::recording_factory();
    let resolved = Arc::clone(&bound_model);
    let bound_root = bound.clone();
    let runtime = runtime_on(&bus, Arc::clone(&active_model), &active, factory)
        .with_project_models(Arc::new(move |root| {
            (root == bound_root).then(|| Arc::clone(&resolved) as Arc<dyn AgentModel>)
        }));
    runtime.set_project_root(active.clone()).unwrap();

    let parent = runtime.delegate_background(
        Role::Orchestrator,
        "ORCH".into(),
        RunConfig {
            project_root: Some(bound.clone()),
            ..RunConfig::default()
        },
    );

    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    let child = child_of(&runtime, parent);
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    let mounted: Vec<_> = mounts
        .lock()
        .unwrap()
        .iter()
        .map(|mounts| mounts.workspace_root.clone())
        .collect();
    assert_eq!(mounted, [bound.clone(), bound.clone()]);
    assert!(bound.join("child.rs").exists());
    assert!(!active.join("child.rs").exists());
    assert!(active_model.observed().await.is_empty());
    assert_eq!(
        runtime
            .inspect_agent(child)
            .unwrap()
            .workspace
            .unwrap()
            .active_root,
        Some(bound),
    );
}

// Switching the active project while a conversation runs must not move the
// children it delegates afterwards.
#[tokio::test]
async fn root_binds_the_active_project_at_registration_for_later_children() {
    let (_first_temp, first) = canonical_repo();
    let (_second_temp, second) = canonical_repo();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ORCH", delegate_script()).await;
    model.add_keyed("CHILD", child_script()).await;
    let gate = Arc::new(tokio::sync::Notify::new());
    model.gate_key("ORCH", Arc::clone(&gate)).await;
    let (factory, _mounts) = support::recording_factory();
    let runtime = runtime_on(&bus, Arc::clone(&model), &first, factory);
    runtime.set_project_root(first.clone()).unwrap();

    let parent =
        runtime.delegate_background(Role::Orchestrator, "ORCH".into(), RunConfig::default());
    model.wait_for_request(0).await;
    runtime.set_project_root(second.clone()).unwrap();
    gate.notify_one();
    // The awaited child finishes before the parent's next (gated) request.
    model.wait_for_request(2).await;
    gate.notify_one();

    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    assert!(first.join("child.rs").exists());
    assert!(!second.join("child.rs").exists());
}
