//! run 境界の再発見・診断・fail-soft と復元履歴の結合テスト。

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use config::Config;
use event_bus::{AgentRunPhase, EventBus, EventKind, FaultEvent, SkillDiagnosticKind};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::{
    AgentRuntime, CatalogBuildInput, Role, RunConfig, SkillCatalogSource, SkillScope,
    build_catalog, discover_skills,
};
use support::{ScriptedModel, drain_events, text_response};
use tools::ToolExecutor;

const V1: &str = "SKILL-BODY-SENTINEL-V1";
const V2: &str = "SKILL-BODY-SENTINEL-V2";

fn write_skill(root: &Path, name: &str, description: &str, body: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n"),
    )
    .unwrap();
}

fn source(bus: &Arc<EventBus>, root: &Path) -> Arc<SkillCatalogSource> {
    Arc::new(SkillCatalogSource::new(
        Config::default(),
        None,
        Vec::new(),
        vec![(SkillScope::Repo, root.to_path_buf())],
        bus.clone(),
    ))
}

fn model(replies: usize) -> Arc<ScriptedModel> {
    Arc::new(ScriptedModel::new(
        (0..replies).map(|_| Ok(text_response("done", FinishReason::Stop))),
    ))
}

fn runtime(bus: Arc<EventBus>, model: Arc<ScriptedModel>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::new(bus.clone()));
    AgentRuntime::new(bus, executor, model)
}

fn load_demo() -> RunConfig {
    RunConfig {
        load_skills: vec!["demo".into()],
        ..Default::default()
    }
}

async fn run(runtime: &AgentRuntime, config: RunConfig) {
    let id = runtime.delegate_background(Role::Orchestrator, "observe skills".into(), config);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(10), runtime.wait(id))
            .await
            .unwrap()
            .unwrap(),
        AgentRunPhase::Done
    );
}

fn system(messages: &[Message]) -> &str {
    assert_eq!(messages[0].role, MessageRole::System);
    match &messages[0].content[0] {
        ContentBlock::Text { text } => text,
        other => panic!("unexpected system block: {other:?}"),
    }
}

#[tokio::test]
async fn discovers_added_skill_metadata_at_next_run_without_loading_body() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("skills");
    let bus = Arc::new(EventBus::new(128));
    let model = model(2);
    let runtime = runtime(bus.clone(), model.clone()).with_skill_source(source(&bus, &root));
    run(&runtime, RunConfig::default()).await;
    write_skill(&root, "demo", "Demo skill", V1);
    run(&runtime, RunConfig::default()).await;
    let observed = model.observed().await;
    assert!(!system(&observed[0]).contains("- demo: Demo skill"));
    assert!(system(&observed[1]).contains("- demo: Demo skill"));
    assert!(!system(&observed[1]).contains(V1));
}

#[tokio::test]
async fn next_run_loads_edited_body_into_skills_section() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "Demo skill", V1);
    let bus = Arc::new(EventBus::new(128));
    let model = model(2);
    let runtime = runtime(bus.clone(), model.clone()).with_skill_source(source(&bus, dir.path()));
    run(&runtime, load_demo()).await;
    write_skill(dir.path(), "demo", "Demo skill", V2);
    run(&runtime, load_demo()).await;
    let observed = model.observed().await;
    assert!(system(&observed[0]).contains(V1));
    let updated = system(&observed[1]);
    assert!(updated.contains("Skills"));
    assert!(updated.contains(V2));
    assert!(!updated.contains(V1));
}

#[tokio::test]
async fn repo_wins_over_repo_agents_and_unchanged_diagnostics_are_not_repeated() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join(".evorch/skills");
    let agents = dir.path().join(".agents/skills");
    write_skill(&repo, "demo", "Repo winner", V1);
    write_skill(&agents, "demo", "Agents loser", V2);
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let source = Arc::new(SkillCatalogSource::new(
        Config::default(),
        None,
        Vec::new(),
        vec![
            (SkillScope::Repo, repo),
            (SkillScope::RepoAgents, agents.clone()),
        ],
        bus.clone(),
    ));
    let initial = drain_events(&mut receiver).await;
    assert_eq!(initial.len(), 1);
    assert!(matches!(&initial[0].kind,
        EventKind::Fault(FaultEvent::SkillDiagnostic { kind: SkillDiagnosticKind::Shadowed, skill, scope, .. })
        if skill == "demo" && scope == "repo-agents"));
    let snapshot = source.snapshot();
    assert_eq!(
        snapshot.registry.get("demo").unwrap().scope,
        SkillScope::Repo
    );
    source.snapshot();
    assert!(drain_events(&mut receiver).await.is_empty());
    let model = model(1);
    let runtime = runtime(bus, model.clone()).with_skill_source(source.clone());
    run(&runtime, load_demo()).await;
    let observed = model.observed().await;
    assert!(system(&observed[0]).contains("- demo: Repo winner"));
    assert!(system(&observed[0]).contains(V1));
    assert!(!system(&observed[0]).contains(V2));
    assert!(
        !drain_events(&mut receiver)
            .await
            .iter()
            .any(|e| matches!(e.kind, EventKind::Fault(_)))
    );

    // 診断が一度解消してから再発した場合は同じ診断でも再通知する。
    std::fs::remove_dir_all(agents.join("demo")).unwrap();
    source.snapshot();
    assert!(drain_events(&mut receiver).await.is_empty());
    write_skill(&agents, "demo", "Agents loser", V2);
    source.snapshot();
    assert_eq!(drain_events(&mut receiver).await.len(), 1);
}

#[tokio::test]
async fn changed_diagnostic_set_is_emitted_in_full() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "bad", "Bad skill", V1);
    std::fs::write(dir.path().join("bad/SKILL.md"), "not-frontmatter").unwrap();
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let source = source(&bus, dir.path());
    assert_eq!(drain_events(&mut receiver).await.len(), 1);
    write_skill(dir.path(), "other", "Other", V1);
    std::fs::write(dir.path().join("other/SKILL.md"), "not-frontmatter").unwrap();
    source.snapshot();
    assert_eq!(drain_events(&mut receiver).await.len(), 2);
    source.snapshot();
    assert!(drain_events(&mut receiver).await.is_empty());
}

#[tokio::test]
async fn missing_preset_is_fail_soft_and_recovers_without_replacing_source() {
    let dir = tempfile::tempdir().unwrap();
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some("reload-test-appendix".into());
    let source = Arc::new(SkillCatalogSource::new(
        config,
        Some(dir.path().to_path_buf()),
        Vec::new(),
        Vec::new(),
        bus.clone(),
    ));
    let initial = drain_events(&mut receiver).await;
    assert_eq!(initial.len(), 1);
    assert!(matches!(&initial[0].kind,
        EventKind::Fault(FaultEvent::SkillDiagnostic { kind: SkillDiagnosticKind::DiscoveryError, skill, scope, detail })
        if skill == "<system-prompts>" && scope == "runtime" && detail == "preset-resolution-failed"));
    assert!(source.snapshot().catalog.is_none());
    let model = model(2);
    let runtime = runtime(bus, model.clone()).with_skill_source(source.clone());
    run(&runtime, RunConfig::default()).await;
    assert_eq!(model.observed().await[0][0].role, MessageRole::User);
    std::fs::create_dir(dir.path().join("presets")).unwrap();
    std::fs::write(
        dir.path().join("presets/reload-test-appendix.md"),
        "RECOVERED-PRESET",
    )
    .unwrap();
    drain_events(&mut receiver).await;
    run(&runtime, RunConfig::default()).await;
    assert!(system(&model.observed().await[1]).contains("RECOVERED-PRESET"));
    assert!(
        !drain_events(&mut receiver)
            .await
            .iter()
            .any(|e| matches!(e.kind, EventKind::Fault(_)))
    );
}

#[tokio::test]
async fn catalog_failure_keeps_last_good_catalog_but_updates_registry() {
    let dir = tempfile::tempdir().unwrap();
    let presets = dir.path().join("presets");
    std::fs::create_dir(&presets).unwrap();
    let preset = presets.join("reload-test-appendix.md");
    std::fs::write(&preset, "LAST-GOOD-PRESET").unwrap();
    let mut config = Config::default();
    config.agents.orchestrator.preset = Some("reload-test-appendix".into());
    let bus = Arc::new(EventBus::new(128));
    let mut receiver = bus.subscribe();
    let skills = dir.path().join("skills");
    let source = Arc::new(SkillCatalogSource::new(
        config,
        Some(dir.path().to_path_buf()),
        Vec::new(),
        vec![(SkillScope::Repo, skills.clone())],
        bus.clone(),
    ));
    let good = source.snapshot();
    std::fs::remove_file(&preset).unwrap();
    write_skill(&skills, "demo", "Added during failure", V2);
    let failed = source.snapshot();
    assert!(Arc::ptr_eq(
        good.catalog.as_ref().unwrap(),
        failed.catalog.as_ref().unwrap()
    ));
    assert!(failed.registry.get("demo").is_some());
    assert!(failed.registry.get("git-best-practices").is_some());
    let model = model(2);
    let runtime = runtime(bus, model.clone()).with_skill_source(source);
    run(&runtime, load_demo()).await;
    let observed = model.observed().await;
    assert!(system(&observed[0]).contains("LAST-GOOD-PRESET"));
    assert!(system(&observed[0]).contains(V2));
    assert!(!system(&observed[0]).contains("- demo: Added during failure"));
    assert_eq!(
        drain_events(&mut receiver)
            .await
            .iter()
            .filter(|e| matches!(e.kind, EventKind::Fault(_)))
            .count(),
        2
    );
    std::fs::write(&preset, "NEW-GOOD-PRESET").unwrap();
    run(&runtime, load_demo()).await;
    let observed = model.observed().await;
    assert!(system(&observed[1]).contains("NEW-GOOD-PRESET"));
    assert!(system(&observed[1]).contains("- demo: Added during failure"));
}

#[tokio::test]
async fn skill_source_is_first_wins_and_overrides_static_builders() {
    let dir = tempfile::tempdir().unwrap();
    write_skill(dir.path(), "demo", "Source winner", V1);
    let bus = Arc::new(EventBus::new(128));
    let first = source(&bus, dir.path());
    let second = source(&bus, &dir.path().join("missing"));
    let model = model(1);
    let config = Config::default();
    let catalog = build_catalog(&CatalogBuildInput {
        config: &config,
        user_presets_dir: None,
        available_agents: &[],
        available_skills: &[],
    })
    .unwrap();
    let runtime = runtime(bus, model.clone())
        .with_skill_source(first)
        .with_skills(Arc::new(discover_skills(&[])))
        .with_system_prompts(Arc::new(catalog))
        .with_skill_source(second);
    run(&runtime, load_demo()).await;
    let observed = model.observed().await;
    assert!(system(&observed[0]).contains("- demo: Source winner"));
    assert!(system(&observed[0]).contains(V1));
}

#[tokio::test]
async fn stopped_run_continues_with_original_system_despite_new_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let skills = dir.path().join("skills");
    write_skill(&skills, "demo", "Original metadata", V1);
    let bus = Arc::new(EventBus::new(128));
    let gate = Arc::new(tokio::sync::Notify::new());
    let model = Arc::new(ScriptedModel::gated([], gate));
    let source = source(&bus, &skills);
    let storage_config = storage::StorageConfig {
        db_path: dir.path().join("history.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(storage_config.clone()).unwrap();
    let runtime = runtime(bus, model.clone())
        .with_skill_source(source.clone())
        .with_run_store(runtime::RunStore::open(&storage_config, storage.handle()).unwrap());
    let id = runtime.delegate_background(Role::Orchestrator, "keep prefix".into(), load_demo());
    wait_observed(&model, 1).await;
    runtime.cancel(id).unwrap();
    assert_eq!(runtime.wait(id).await.unwrap(), AgentRunPhase::Error);
    write_skill(&skills, "demo", "Changed metadata", V2);
    write_skill(&skills, "second", "New skill", "SECOND-BODY");
    let snapshot = source.snapshot();
    assert!(snapshot.registry.get("demo").is_some());
    assert!(snapshot.registry.get("second").is_some());
    assert!(snapshot.registry.get("git-best-practices").is_some());
    runtime
        .continue_goal(id, "continue".into(), load_demo())
        .unwrap();
    wait_observed(&model, 2).await;
    runtime.cancel(id).unwrap();
    runtime.wait(id).await.unwrap();
    let observed = model.observed().await;
    assert_eq!(observed[0][0], observed[1][0]);
    assert!(system(&observed[1]).contains(V1));
    assert!(!system(&observed[1]).contains(V2));
    assert!(!system(&observed[1]).contains("- second: New skill"));
}

async fn wait_observed(model: &ScriptedModel, count: usize) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while model.observed().await.len() < count {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
