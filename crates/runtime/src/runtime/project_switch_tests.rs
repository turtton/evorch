//! Switching the active project rebinds every project-scoped runtime input.
use super::comment_checker_support::{
    self as support, ScriptedModel, text_response, tool_response,
};
use super::*;
use crate::rules::{ProjectTrust, RulesSettings};
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
    AgentRuntime::with_sandbox_factory(Arc::clone(bus), Arc::new(executor), model, factory)
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
async fn project_switch_rebinds_shared_workspace_rules_and_skills() {
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
    let runtime = runtime_on(&bus, Arc::clone(&model), &startup, factory).with_skill_source(
        Arc::new(SkillCatalogSource::new(
            config::Config::default(),
            None,
            Vec::new(),
            crate::skill::repo_skill_dirs(&startup).into(),
            Arc::clone(&bus),
        )),
    );

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
}

// The runtime no longer needs a git repository: a plain directory is a
// project root like any other (ADR 0031).
#[tokio::test]
async fn project_switch_to_a_non_git_directory_runs_there() {
    let (_startup_temp, startup) = canonical_repo();
    let plain = tempfile::tempdir().unwrap();
    let plain = plain.path().canonicalize().unwrap();
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "write",
            "write",
            serde_json::json!({"path": "plain.rs", "content": "fn main() {}"}),
        )),
        Ok(text_response("done", providers::FinishReason::Stop)),
    ]));
    let (factory, mounts) = support::recording_factory();
    let runtime = runtime_on(&bus, model, &startup, factory);

    runtime.set_project_root(plain.clone()).unwrap();
    let run = runtime.delegate_background(Role::Worker, "edit".into(), RunConfig::default());

    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(mounts.lock().unwrap()[0].workspace_root, plain);
    assert!(plain.join("plain.rs").exists());
    assert!(!plain.join(".evorch").exists());
    assert!(!startup.join("plain.rs").exists());
    assert_eq!(
        runtime
            .inspect_agent(run)
            .unwrap()
            .workspace
            .unwrap()
            .active_root,
        Some(plain),
    );
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

// A project's own instructions reach its runs only once the host trusts it, and
// re-declaring trust applies to runs started afterwards.
#[tokio::test]
async fn declared_trust_gates_project_rules_and_repository_skills() {
    let (_trusted_temp, trusted) = canonical_repo();
    let (_untrusted_temp, untrusted) = canonical_repo();
    write_skill(&trusted, "trusted-skill");
    write_skill(&untrusted, "untrusted-skill");
    let bus = Arc::new(EventBus::new(64));
    let (factory, _mounts) = support::recording_factory();
    let runtime = runtime_on(&bus, Arc::new(ScriptedModel::new([])), &trusted, factory)
        .with_skill_source(Arc::new(SkillCatalogSource::new(
            config::Config::default(),
            None,
            Vec::new(),
            Vec::new(),
            Arc::clone(&bus),
        )));
    runtime.set_project_trust(trusted.clone(), ProjectTrust::Approved);
    runtime.set_project_trust(untrusted.clone(), ProjectTrust::Unapproved);
    let inputs = |root: &std::path::Path| {
        let project = runtime.shared.project(root);
        let shared = loop_shared(&Arc::downgrade(&runtime.shared), Some(&project)).unwrap();
        let skills: Vec<_> = shared
            .skills
            .unwrap()
            .available_skills()
            .into_iter()
            .map(|skill| skill.name)
            .collect();
        (shared.rules.unwrap().trust(), skills)
    };

    let (trust, skills) = inputs(&trusted);
    assert_eq!(trust, ProjectTrust::Approved);
    assert!(skills.contains(&"trusted-skill".to_owned()), "{skills:?}");
    let (trust, skills) = inputs(&untrusted);
    assert_eq!(trust, ProjectTrust::Unapproved);
    assert!(
        !skills.contains(&"untrusted-skill".to_owned()),
        "{skills:?}"
    );

    runtime.set_project_trust(untrusted.clone(), ProjectTrust::Approved);
    let (trust, skills) = inputs(&untrusted);
    assert_eq!(trust, ProjectTrust::Approved);
    assert!(skills.contains(&"untrusted-skill".to_owned()), "{skills:?}");
}

fn linked_repo() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, repo) = canonical_repo();
    let link = temp.path().join("link");
    std::os::unix::fs::symlink(&repo, &link).unwrap();
    (temp, repo, link)
}

/// A project with a rule and a repository skill, both named after `marker`.
fn write_instructions(root: &std::path::Path, marker: &str) {
    std::fs::write(root.join("AGENTS.md"), format!("{marker}-rule\n")).unwrap();
    write_skill(root, &format!("{marker}-skill"));
}

/// Undeclared projects get the shared rules' trust, here Approved, so only a
/// declaration that reaches the run can withhold its instructions.
fn approving_runtime(
    bus: &Arc<EventBus>,
    model: Arc<ScriptedModel>,
    root: &std::path::Path,
) -> (AgentRuntime, Arc<std::sync::Mutex<Vec<WorkspaceMounts>>>) {
    let (factory, mounts) = support::recording_factory();
    let executor = ToolExecutor::with_standard_tools(
        Arc::clone(bus),
        Arc::new(DirectSandbox::new_unchecked()),
    );
    let runtime =
        AgentRuntime::with_sandbox_factory(Arc::clone(bus), Arc::new(executor), model, factory)
            .with_project_rules(Arc::new(RulesSource::new(
                ProjectTrust::Approved,
                RulesSettings::from(&config::RulesConfig::default()),
                None,
                Some(root.to_path_buf()),
                None,
            )))
            .with_sandbox_root(root.to_path_buf())
            .with_skill_source(Arc::new(SkillCatalogSource::new(
                config::Config::default(),
                None,
                Vec::new(),
                Vec::new(),
                Arc::clone(bus),
            )));
    (runtime, mounts)
}

/// Runs a worker in `root` and returns its system prompt text and repository skills.
async fn run_inputs(
    runtime: &AgentRuntime,
    model: &ScriptedModel,
    root: &std::path::Path,
) -> (String, Vec<String>) {
    let run = runtime.delegate_background(
        Role::Worker,
        "go".into(),
        RunConfig {
            project_root: Some(root.to_path_buf()),
            ..RunConfig::default()
        },
    );
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let request = model.observed().await.pop().unwrap();
    assert_eq!(request[0].role, providers::Role::System);
    let system = request[0]
        .content
        .iter()
        .filter_map(|block| match block {
            providers::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let project = runtime.shared.project_of(run).unwrap();
    let skills = loop_shared(&Arc::downgrade(&runtime.shared), Some(&project))
        .unwrap()
        .skills
        .unwrap()
        .available_skills()
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    (system, skills)
}

fn done_script(runs: usize) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new((0..runs).map(|_| {
        Ok(text_response("done", providers::FinishReason::Stop))
    })))
}

// Trust declared under one spelling of a project root (symlink, `..`) gates runs
// started under another, and those runs mount the canonical root.
#[tokio::test]
async fn declared_trust_applies_across_root_spellings() {
    let (_linked_temp, linked, link) = linked_repo();
    let (_dotted_temp, dotted) = canonical_repo();
    write_instructions(&linked, "linked");
    write_instructions(&dotted, "dotted");
    let bus = Arc::new(EventBus::new(256));
    let model = done_script(2);
    let (runtime, mounts) = approving_runtime(&bus, Arc::clone(&model), &linked);
    runtime.set_project_trust(link, ProjectTrust::Unapproved);
    runtime.set_project_trust(dotted.clone(), ProjectTrust::Unapproved);

    for (root, marker) in [
        (linked.clone(), "linked"),
        (dotted.join(".evorch/.."), "dotted"),
    ] {
        let (system, skills) = run_inputs(&runtime, &model, &root).await;
        assert!(!system.contains(&format!("{marker}-rule")), "{system}");
        assert!(!skills.contains(&format!("{marker}-skill")), "{skills:?}");
    }
    let mounted: Vec<_> = mounts
        .lock()
        .unwrap()
        .iter()
        .map(|mounts| mounts.workspace_root.clone())
        .collect();
    assert_eq!(mounted, [linked, dotted]);
}

// Re-declaring trust under another spelling drops the cached project context, so
// the next run sees the new trust.
#[tokio::test]
async fn redeclared_trust_under_another_spelling_reaches_new_runs() {
    let (_temp, repo, link) = linked_repo();
    write_instructions(&repo, "project");
    let bus = Arc::new(EventBus::new(256));
    let model = done_script(2);
    let (runtime, _mounts) = approving_runtime(&bus, Arc::clone(&model), &repo);
    runtime.set_project_trust(repo.clone(), ProjectTrust::Unapproved);

    let (system, skills) = run_inputs(&runtime, &model, &repo).await;
    assert!(!system.contains("project-rule"), "{system}");
    assert!(!skills.contains(&"project-skill".to_owned()), "{skills:?}");

    runtime.set_project_trust(link, ProjectTrust::Approved);
    let (system, skills) = run_inputs(&runtime, &model, &repo).await;
    assert!(system.contains("project-rule"), "{system}");
    assert!(skills.contains(&"project-skill".to_owned()), "{skills:?}");
}

const WITHHELD_BODY: &str = "WITHHELD-SKILL-BODY";
const WITHHELD_DESCRIPTION: &str = "WITHHELD-SKILL-DESCRIPTION";

/// A repository skill under `<root>/<directory>` whose description and body are sentinels.
fn write_withheld_skill(root: &std::path::Path, directory: &str, name: &str) {
    let directory = root.join(directory).join(name);
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(
        directory.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {WITHHELD_DESCRIPTION}\n---\n{WITHHELD_BODY}\n"),
    )
    .unwrap();
}

/// The error flag and text of the tool result answering `id`.
fn tool_result(request: &[providers::Message], id: &str) -> (bool, String) {
    request
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            providers::ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == id => Some((
                *is_error,
                content
                    .iter()
                    .filter_map(|part| match part {
                        providers::ToolResultContent::Text { text } => Some(text.as_str()),
                        providers::ToolResultContent::Image { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            )),
            _ => None,
        })
        .unwrap_or_else(|| panic!("tool result {id} is in the request"))
}

fn assert_explains_trust_gate(text: &str, directory: &str) {
    assert!(
        text.contains(&format!("exists in the project's {directory}")),
        "{text}"
    );
    assert!(text.contains("not trusted"), "{text}");
    assert!(text.contains("Trust project instructions"), "{text}");
    assert!(!text.contains("unknown skill"), "{text}");
    assert!(!text.contains(WITHHELD_BODY), "{text}");
    assert!(!text.contains(WITHHELD_DESCRIPTION), "{text}");
}

fn bound_to(root: &std::path::Path) -> RunConfig {
    RunConfig {
        project_root: Some(root.to_path_buf()),
        ..RunConfig::default()
    }
}

// Regression: an untrusted project's repository skills were reported as
// `unknown skill`, so users and agents mistook the trust gate for a bug.
// skill_load now names the gate without returning the skill's text, a name
// the project lacks stays unknown, and trusting the project loads the skill.
#[tokio::test]
async fn untrusted_project_skill_load_explains_the_trust_gate() {
    let (_temp, repo) = canonical_repo();
    write_withheld_skill(&repo, ".evorch/skills", "evorch-skill");
    write_withheld_skill(&repo, ".agents/skills", "agents-skill");
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([
        Ok(support::tool_responses([
            (
                "evorch",
                "skill_load",
                serde_json::json!({"name": "evorch-skill"}),
            ),
            (
                "agents",
                "skill_load",
                serde_json::json!({"name": "agents-skill"}),
            ),
            (
                "missing",
                "skill_load",
                serde_json::json!({"name": "missing-skill"}),
            ),
        ])),
        Ok(text_response("done", providers::FinishReason::Stop)),
        Ok(tool_response(
            "trusted",
            "skill_load",
            serde_json::json!({"name": "agents-skill"}),
        )),
        Ok(text_response("done", providers::FinishReason::Stop)),
    ]));
    let (runtime, _mounts) = approving_runtime(&bus, Arc::clone(&model), &repo);
    runtime.set_project_trust(repo.clone(), ProjectTrust::Unapproved);

    let run = runtime.delegate_background(Role::Worker, "go".into(), bound_to(&repo));

    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let request = model.observed().await[1].clone();
    for (id, directory) in [("evorch", ".evorch/skills"), ("agents", ".agents/skills")] {
        let (is_error, text) = tool_result(&request, id);
        assert!(is_error, "{text}");
        assert_explains_trust_gate(&text, directory);
    }
    let (is_error, text) = tool_result(&request, "missing");
    assert!(is_error, "{text}");
    assert!(text.contains("unknown skill: missing-skill"), "{text}");
    assert!(!text.contains("not trusted"), "{text}");
    let sent = format!("{request:?}");
    assert!(!sent.contains(WITHHELD_BODY), "{sent}");
    assert!(!sent.contains(WITHHELD_DESCRIPTION), "{sent}");

    runtime.set_project_trust(repo.clone(), ProjectTrust::Approved);
    let run = runtime.delegate_background(Role::Worker, "go".into(), bound_to(&repo));

    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let request = model.observed().await[3].clone();
    let (is_error, text) = tool_result(&request, "trusted");
    assert!(!is_error, "{text}");
    assert!(text.contains(WITHHELD_BODY), "{text}");
}

// The same gate explains a delegate whose load_skills names a withheld skill,
// before any child run is registered.
#[tokio::test]
async fn untrusted_project_delegate_load_skills_explains_the_trust_gate() {
    let (_temp, repo) = canonical_repo();
    write_withheld_skill(&repo, ".agents/skills", "agents-skill");
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "delegate",
            "delegate",
            serde_json::json!({
                "target": {"role": "worker"},
                "prompt": "CHILD",
                "load_skills": ["agents-skill"],
            }),
        )),
        Ok(text_response("parent done", providers::FinishReason::Stop)),
    ]));
    let (runtime, _mounts) = approving_runtime(&bus, Arc::clone(&model), &repo);
    runtime.set_project_trust(repo.clone(), ProjectTrust::Unapproved);

    let parent = runtime.delegate_background(Role::Orchestrator, "ORCH".into(), bound_to(&repo));

    assert_eq!(runtime.wait(parent).await.unwrap(), AgentRunPhase::Done);
    let request = model.observed().await.pop().unwrap();
    let (is_error, text) = tool_result(&request, "delegate");
    assert!(is_error, "{text}");
    assert_explains_trust_gate(&text, ".agents/skills");
    assert_eq!(runtime.list_agents().len(), 1, "no child run is registered");
}

/// `link` becomes a symlink to `target`, creating the directories above it.
fn symlink(target: &std::path::Path, link: &std::path::Path) {
    std::fs::create_dir_all(link.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(target, link).unwrap();
}

// The trust hint must not report skills outside the project: a symlinked
// skills directory or one above it, a symlinked skill directory and a
// symlinked SKILL.md all leave the name unknown, in both repository scopes.
#[tokio::test]
async fn untrusted_project_skill_hint_does_not_follow_symlinks_out_of_the_project() {
    let outside_temp = tempfile::tempdir().unwrap();
    let outside = outside_temp.path().canonicalize().unwrap();
    write_withheld_skill(&outside, "evorch-root/skills", "evorch-ancestor");
    write_withheld_skill(&outside, "agents-skills", "agents-skills-root");
    write_withheld_skill(&outside, "dirs", "evorch-dir");
    write_withheld_skill(&outside, "dirs", "agents-dir");
    write_withheld_skill(&outside, "files", "target");
    let skill_md = outside.join("files/target/SKILL.md");
    let (_roots_temp, roots) = canonical_repo();
    symlink(&outside.join("evorch-root"), &roots.join(".evorch"));
    symlink(
        &outside.join("agents-skills"),
        &roots.join(".agents/skills"),
    );
    let (_entries_temp, entries) = canonical_repo();
    symlink(
        &outside.join("dirs/evorch-dir"),
        &entries.join(".evorch/skills/evorch-dir"),
    );
    symlink(
        &outside.join("dirs/agents-dir"),
        &entries.join(".agents/skills/agents-dir"),
    );
    symlink(
        &skill_md,
        &entries.join(".evorch/skills/evorch-md/SKILL.md"),
    );
    symlink(
        &skill_md,
        &entries.join(".agents/skills/agents-md/SKILL.md"),
    );
    let cases: [(&std::path::Path, &[&'static str]); 2] = [
        (&roots, &["evorch-ancestor", "agents-skills-root"]),
        (
            &entries,
            &["evorch-dir", "agents-dir", "evorch-md", "agents-md"],
        ),
    ];
    let bus = Arc::new(EventBus::new(256));
    let model = Arc::new(ScriptedModel::new(cases.iter().flat_map(|(_, names)| {
        [
            Ok(support::tool_responses(names.iter().map(|name| {
                (*name, "skill_load", serde_json::json!({"name": name}))
            }))),
            Ok(text_response("done", providers::FinishReason::Stop)),
        ]
    })));
    let (runtime, _mounts) = approving_runtime(&bus, Arc::clone(&model), &roots);

    for (index, (root, names)) in cases.iter().enumerate() {
        runtime.set_project_trust(root.to_path_buf(), ProjectTrust::Unapproved);
        let run = runtime.delegate_background(Role::Worker, "go".into(), bound_to(root));

        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        let request = model.observed().await[index * 2 + 1].clone();
        for name in *names {
            let directory = if name.starts_with("evorch") {
                ".evorch/skills"
            } else {
                ".agents/skills"
            };
            let followed = root.join(directory).join(name).join("SKILL.md");
            assert!(
                followed.is_file(),
                "{} resolves outside",
                followed.display()
            );
            let (is_error, text) = tool_result(&request, name);
            assert!(is_error, "{text}");
            assert!(text.contains(&format!("unknown skill: {name}")), "{text}");
            assert!(!text.contains("not trusted"), "{text}");
        }
    }
}
