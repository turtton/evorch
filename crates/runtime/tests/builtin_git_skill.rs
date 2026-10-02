//! 実際の同梱 Git skill を production composition とメタ操作から利用する。

mod support;

use std::sync::Arc;
use std::time::Duration;

use event_bus::{AgentRunPhase, EventBus, SkillDiagnosticKind};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole, ToolResultContent};
use routing::MapEnv;
use runtime::skill::{SkillSource, discover_with_builtin};
use runtime::{
    AgentRuntime, ModelSource, Role, RunConfig, RuntimeComposition, SkillScope, WorkspaceSeam,
    compose_runtime,
};
use sandbox::credential::FileCredentialStore;
use serde_json::json;
use support::{ScriptedModel, init_git_repo, recording_factory, text_response, tool_response};
use tools::ToolExecutor;

const NAME: &str = "git-best-practices";
const SKILL_MD: &str = include_str!("../skills/builtin/git-best-practices/SKILL.md");

fn compose(model: Arc<ScriptedModel>) -> (AgentRuntime, tempfile::TempDir) {
    let (directory, repo) = init_git_repo();
    let bus = Arc::new(EventBus::new(128));
    let (factory, _) = recording_factory();
    let composed = compose_runtime(RuntimeComposition {
        config: &config::Config::default(),
        executor: Arc::new(ToolExecutor::new(bus.clone())),
        bus,
        credential_store: Arc::new(
            FileCredentialStore::open(directory.path().join("credentials")).unwrap(),
        ),
        env: Arc::new(MapEnv::default()),
        model_source: ModelSource::Fixed(model),
        workspace: Some(WorkspaceSeam::with_factory(repo, factory).unwrap()),
    })
    .unwrap();
    (composed.runtime, directory)
}

fn system(messages: &[Message]) -> &str {
    assert_eq!(messages[0].role, MessageRole::System);
    match &messages[0].content[0] {
        ContentBlock::Text { text } => text,
        other => panic!("unexpected system content: {other:?}"),
    }
}

async fn run(runtime: &AgentRuntime, role: Role, config: RunConfig) {
    let id = runtime.delegate_background(role, "plan Git work".into(), config);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), runtime.wait(id))
            .await
            .unwrap()
            .unwrap(),
        AgentRunPhase::Done
    );
}

#[tokio::test]
async fn production_composition_exposes_git_trigger_and_loads_body_without_rewriting_prefix() {
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response(
            "git-skill",
            "skill_load",
            json!({"name": NAME}),
        )),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let (runtime, _directory) = compose(model.clone());
    run(&runtime, Role::Orchestrator, RunConfig::default()).await;
    let observed = model.observed().await;
    assert_eq!(observed.len(), 2);
    let initial_system = system(&observed[0]);
    assert!(initial_system.contains("- git-best-practices: Git操作"));
    assert!(initial_system.contains("前に参照"));
    assert!(!initial_system.contains("# Git best practices"));
    assert_eq!(&observed[1][..observed[0].len()], observed[0].as_slice());
    let content = observed[1]
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } if tool_call_id == "git-skill" => {
                assert!(!is_error);
                match &content[0] {
                    ToolResultContent::Text { text } => Some(text),
                }
            }
            _ => None,
        })
        .expect("skill_load must return the builtin body");
    let (_, expected) = runtime::skill::split_frontmatter(SKILL_MD).unwrap();
    assert_eq!(content, expected);
    assert!(content.contains("明示されたコミットメッセージ・ブランチ命名規則"));
    assert!(content.contains("git diff --cached"));
    assert!(!content.starts_with("---\nname:"));
}

#[tokio::test]
async fn worker_can_receive_git_skill_in_initial_system() {
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let (runtime, _directory) = compose(model.clone());
    run(
        &runtime,
        Role::Worker,
        RunConfig {
            load_skills: vec![NAME.into()],
            ..Default::default()
        },
    )
    .await;
    let observed = model.observed().await;
    let prompt = system(&observed[0]);
    assert!(prompt.contains("<!-- skill:git-best-practices BEGIN -->"));
    let (_, expected) = runtime::skill::split_frontmatter(SKILL_MD).unwrap();
    assert!(prompt.contains(expected));
    assert!(!prompt.contains("name: git-best-practices"));
}

#[test]
fn git_guidance_is_independent_of_repository_and_agent_runtime() {
    for specific in [
        "evorch",
        "intent-cli",
        "Worker",
        "Orchestrator",
        "load_skills",
        "skill_load",
        "feat(runtime)",
        ".evorch/",
    ] {
        assert!(
            !SKILL_MD.contains(specific),
            "generic Git guidance must not depend on {specific}"
        );
    }
    assert!(SKILL_MD.contains("明示されたコミットメッセージ・ブランチ命名規則"));
    assert!(SKILL_MD.contains("Conventional Commitsが要求されている場合だけ"));
    assert!(SKILL_MD.contains("git diff --cached"));
}

#[test]
fn all_filesystem_scopes_can_override_the_actual_builtin() {
    let directory = tempfile::tempdir().unwrap();
    let skill = directory.path().join(NAME);
    std::fs::create_dir(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {NAME}\ndescription: Repository Git policy\n---\nLOCAL GIT POLICY\n"),
    )
    .unwrap();
    for scope in [SkillScope::Repo, SkillScope::RepoAgents, SkillScope::User] {
        let registry = discover_with_builtin(&[(scope, directory.path().to_path_buf())]);
        let entry = registry.get(NAME).unwrap();
        assert_eq!(entry.scope, scope);
        assert!(matches!(entry.source, SkillSource::Filesystem { .. }));
        assert_eq!(registry.load_body(NAME).unwrap(), "LOCAL GIT POLICY\n");
        assert!(registry.diagnostics.iter().any(|diagnostic| diagnostic.kind
            == SkillDiagnosticKind::Shadowed
            && diagnostic.skill == NAME
            && diagnostic.scope == SkillScope::Builtin));
    }
}
