//! Base context preview: the inspector's sections must equal what runs actually send.

mod support;

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use event_bus::{AgentRunPhase, EventBus};
use providers::{ContentBlock, FinishReason, Message, Role as MessageRole};
use runtime::base_context::{BaseContextRequest, ContextSectionKind, ContextSource};
use runtime::{
    AgentRuntime, ProjectTrust, RulesSettings, RulesSource, RunConfig, RunStore, SkillCatalogSource,
};
use sandbox::DirectSandbox;
use storage::{Storage, StorageConfig};
use tools::ToolExecutor;

use support::{ScriptedModel, text_response};

struct Fixture {
    user_dir: tempfile::TempDir,
    project: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self {
            user_dir: tempfile::tempdir().unwrap(),
            project: tempfile::tempdir().unwrap(),
        };
        write(
            &fixture.user_dir.path().join("presets/role-worker.md"),
            "USER-WORKER-BASELINE",
        );
        write(
            &fixture.user_dir.path().join("AGENTS.md"),
            "USER-AGENTS-RULE",
        );
        write(
            &fixture.project.path().join("AGENTS.md"),
            "PROJECT-AGENTS-RULE",
        );
        fixture
    }

    fn runtime(&self, model: Arc<ScriptedModel>, trust: ProjectTrust) -> AgentRuntime {
        let bus = Arc::new(EventBus::new(64));
        let executor = Arc::new(
            ToolExecutor::with_standard_tools(
                Arc::clone(&bus),
                Arc::new(DirectSandbox::new_unchecked()),
            )
            .with_web_tools()
            .unwrap(),
        );
        let source = SkillCatalogSource::new(
            config::Config::default(),
            Some(self.user_dir.path().to_path_buf()),
            Vec::new(),
            Vec::new(),
            Arc::clone(&bus),
        );
        AgentRuntime::new(bus, executor, model)
            .with_skill_source(Arc::new(source))
            .with_project_rules(Arc::new(RulesSource::new(
                trust,
                RulesSettings::from(&config::RulesConfig::default()),
                None,
                Some(self.project.path().to_path_buf()),
                Some(self.user_dir.path().join("AGENTS.md")),
            )))
    }
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

fn system_texts(messages: &[Message]) -> Vec<&str> {
    messages
        .iter()
        .find(|message| message.role == MessageRole::System)
        .map(|message| {
            message
                .content
                .iter()
                .filter_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn joined(report: &runtime::base_context::BaseContextReport, workspace: bool) -> String {
    report
        .sections
        .iter()
        .filter(|section| (section.kind == ContextSectionKind::Workspace) == workspace)
        .filter(|section| section.kind != ContextSectionKind::Memory)
        .map(|section| section.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

// Given: a runtime with presets, user AGENTS.md and a trusted project
// When: previewing the orchestrator and then running it
// Then: the preview sections join into the exact system message the model receives
#[tokio::test]
async fn preview_matches_the_system_message_a_run_sends() {
    let fixture = Fixture::new();
    for role in [Role::Orchestrator, Role::Worker] {
        let model = Arc::new(ScriptedModel::new([Ok(text_response(
            "done",
            FinishReason::Stop,
        ))]));
        let runtime = fixture.runtime(model.clone(), ProjectTrust::Approved);
        let report = runtime
            .preview_base_context(BaseContextRequest::new(role))
            .await
            .unwrap();

        let run = runtime.delegate_background(role, "TASK".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

        let observed = model.observed().await;
        let texts = system_texts(&observed[0]);
        assert_eq!(texts, [joined(&report, false), joined(&report, true)]);
        assert!(texts[0].contains("PROJECT-AGENTS-RULE"));
    }
}

// Given: a user override for the worker baseline only
// When: previewing a worker
// Then: each preset section names the layer it was read from
#[tokio::test]
async fn preview_reports_where_each_preset_comes_from() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(Arc::new(ScriptedModel::new([])), ProjectTrust::Approved);
    let report = runtime
        .preview_base_context(BaseContextRequest {
            category: Some("deep".into()),
            ..BaseContextRequest::new(Role::Worker)
        })
        .await
        .unwrap();

    let source_of = |kind| {
        report
            .sections
            .iter()
            .find(|section| section.kind == kind)
            .map(|section| section.source.clone())
            .unwrap()
    };
    assert_eq!(
        source_of(ContextSectionKind::RoleBaseline),
        ContextSource::UserPreset {
            name: "role-worker".into(),
            path: fixture.user_dir.path().join("presets/role-worker.md"),
        }
    );
    assert_eq!(
        source_of(ContextSectionKind::ModelFamily),
        ContextSource::BundledPreset {
            name: "family-generic".into()
        }
    );
    assert_eq!(
        source_of(ContextSectionKind::CategoryOverlay),
        ContextSource::BundledPreset {
            name: "category-deep".into()
        }
    );
    assert!(
        report
            .sections
            .iter()
            .all(|section| section.kind != ContextSectionKind::IntentGate)
    );
    assert_eq!(
        report.total_tokens(),
        report.section_tokens() + report.tool_tokens()
    );
}

// Given: an untrusted project with an AGENTS.md
// When: previewing a delegated worker
// Then: the project rule and role-gated tools are listed as excluded with reasons
#[tokio::test]
async fn preview_lists_what_is_left_out_and_why() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(Arc::new(ScriptedModel::new([])), ProjectTrust::Unapproved);
    let report = runtime
        .preview_base_context(BaseContextRequest {
            child: true,
            ..BaseContextRequest::new(Role::Worker)
        })
        .await
        .unwrap();

    let rules = report
        .sections
        .iter()
        .find(|section| section.kind == ContextSectionKind::Rules)
        .unwrap();
    assert!(rules.text.contains("USER-AGENTS-RULE"));
    assert!(!rules.text.contains("PROJECT-AGENTS-RULE"));
    let project_rule = fixture.project.path().join("AGENTS.md");
    assert!(
        report
            .exclusions
            .iter()
            .any(|item| item.item == project_rule.display().to_string())
    );

    let names: Vec<_> = report.tools.iter().map(|tool| tool.name.as_str()).collect();
    assert!(names.contains(&"shell") && names.contains(&"edit"));
    let reason = |name: &str| {
        report
            .excluded_tools
            .iter()
            .find(|item| item.item == name)
            .map(|item| item.reason.as_str())
    };
    assert_eq!(reason("delegate"), Some("Not allowed for this role"));
    assert_eq!(reason("escalate"), Some("Only root runs can escalate"));
}

// Given: a finished run with a run store
// When: reading its context view
// Then: the persisted history, model and visible tools are returned
#[tokio::test]
async fn run_view_returns_the_persisted_context() {
    let directory = tempfile::tempdir().unwrap();
    let config = StorageConfig {
        db_path: directory.path().join("events.db"),
        ..StorageConfig::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let fixture = Fixture::new();
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        "done",
        FinishReason::Stop,
    ))]));
    let runtime = fixture
        .runtime(model, ProjectTrust::Approved)
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());

    let run = runtime.delegate_background(Role::Worker, "TASK".into(), RunConfig::default());
    assert_eq!(runtime.wait(run).await, Ok(AgentRunPhase::Done));

    let view = runtime.run_context_view(run).unwrap().unwrap();
    assert_eq!(view.role_name, "Worker");
    assert_eq!(view.checkpoint_phase, "Done");
    assert_eq!(view.selected_model.as_deref(), Some("scripted-worker"));
    assert!(view.tool_names.iter().any(|name| name == "shell"));
    assert_eq!(view.messages[0].role, MessageRole::System);
    assert!(
        view.messages
            .iter()
            .any(|message| message.role == MessageRole::User)
    );
}
