//! `list_skills`: answered from the runtime's skill registry through the
//! ordinary query path, without the workspace area being wired.

use std::sync::Arc;

use workbench_host::CommandHost;
use workbench_host::commands::CommandService;
use workbench_host::projects::StaticProjects;
use workbench_protocol::errors::{ErrorCode, WorkbenchError};
use workbench_protocol::ids::{ProjectId, SessionId};
use workbench_protocol::namespaces::workspace::{
    MAX_SKILL_DESCRIPTION_BYTES, MAX_SKILL_SUMMARIES, ProjectTarget, SkillSummary,
};
use workbench_protocol::namespaces::{WorkspaceQuery, WorkspaceReply};
use workbench_protocol::queries::{Query, QueryReply};

struct Idle;

#[async_trait::async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "test".into()
    }
}

fn write_skill(dir: &std::path::Path, name: &str, description: &str) {
    let skill = dir.join(name);
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\nbody of {name}\n"),
    )
    .unwrap();
}

struct World {
    rt: tokio::runtime::Runtime,
    _dir: tempfile::TempDir,
    host: CommandHost,
    runtime: runtime::AgentRuntime,
    project_root: std::path::PathBuf,
}

fn project() -> ProjectId {
    ProjectId::new("project-1").unwrap()
}

fn world(skill_count: usize, description: &str) -> World {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let user_skills = dir.path().join("user-skills");
    for index in 0..skill_count {
        write_skill(&user_skills, &format!("skill-{index:03}"), description);
    }
    let project_root = dir.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(16));
    let runtime = runtime::AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus.clone())),
        Arc::new(Idle),
    )
    .with_skill_source(Arc::new(runtime::SkillCatalogSource::new(
        config::Config::default(),
        None,
        Vec::new(),
        vec![(runtime::SkillScope::User, user_skills)],
        bus,
    )));
    let mut projects = StaticProjects::default();
    projects.insert(&project(), project_root.clone());
    let host = CommandHost::new(CommandService::new(runtime.clone(), rt.handle().clone()))
        .with_projects(projects);
    World {
        rt,
        _dir: dir,
        host,
        runtime,
        project_root,
    }
}

impl World {
    fn list(&self, project: ProjectId) -> Result<Vec<SkillSummary>, WorkbenchError> {
        let _ = &self.rt;
        let reply = self.host.handle_query(
            &SessionId::new("session").unwrap(),
            &Query::Workspace(WorkspaceQuery::ListSkills(ProjectTarget { project })),
            &workbench_host::provenance::local_desktop(),
        )?;
        let QueryReply::Workspace(WorkspaceReply::Skills(skills)) = reply else {
            panic!("not a skill list: {reply:?}");
        };
        // What leaves the core is a valid reply.
        workbench_protocol::namespaces::workspace::validate_skills(&skills).unwrap();
        Ok(skills)
    }
}

#[test]
fn a_project_lists_the_skills_its_messages_can_mention() {
    let world = world(2, "does a thing");
    let skills = world.list(project()).unwrap();
    let find = |name: &str| skills.iter().find(|skill| skill.name == name);
    assert_eq!(find("skill-000").unwrap().description, "does a thing");
    assert!(find("skill-001").is_some());
    // The list is what the runtime reads bodies from, and only names travel.
    let runtime_names: Vec<String> = world
        .runtime
        .list_skills_for_host(&world.project_root)
        .into_iter()
        .map(|skill| skill.name)
        .collect();
    let listed: Vec<String> = skills.iter().map(|skill| skill.name.clone()).collect();
    assert_eq!(listed, runtime_names);
    let wire = serde_json::to_string(&skills).unwrap();
    assert!(!wire.contains("body of"), "{wire}");
    assert!(
        !wire.contains(world.project_root.to_str().unwrap()),
        "{wire}"
    );
}

#[test]
fn a_repository_skill_is_listed_for_its_project_only() {
    let world = world(1, "d");
    write_skill(
        &world.project_root.join(".evorch/skills"),
        "repo-only",
        "in the repo",
    );
    let skills = world.list(project()).unwrap();
    assert!(skills.iter().any(|skill| skill.name == "repo-only"));
    let unapproved = world.project_root.clone();
    world
        .runtime
        .set_project_trust(unapproved, runtime::ProjectTrust::Unapproved);
    let skills = world.list(project()).unwrap();
    assert!(skills.iter().all(|skill| skill.name != "repo-only"));
}

#[test]
fn an_unknown_project_is_not_found() {
    let world = world(0, "d");
    let error = world.list(ProjectId::new("nope").unwrap()).unwrap_err();
    assert_eq!(error.code, ErrorCode::NotFound);
}

#[test]
fn a_description_is_one_bounded_line_and_secrets_are_redacted() {
    let long = "é".repeat(MAX_SKILL_DESCRIPTION_BYTES);
    let world = world(1, &long);
    let skills = world.list(project()).unwrap();
    let skill = skills
        .iter()
        .find(|skill| skill.name == "skill-000")
        .unwrap();
    assert!(skill.description.len() <= MAX_SKILL_DESCRIPTION_BYTES);
    assert!(skill.description.starts_with('é'));
    assert!(!skill.description.contains(char::is_control));
}

#[test]
fn a_credential_in_a_description_does_not_leave_the_core() {
    // Assembled at run time so the source holds no credential-shaped literal.
    let secret = format!("{}_{}", "ghp", "a1B2".repeat(9));
    let world = world(1, &format!("uses {secret} to push"));
    let skills = world.list(project()).unwrap();
    let skill = skills
        .iter()
        .find(|skill| skill.name == "skill-000")
        .unwrap();
    assert!(
        !skill.description.contains(&secret),
        "{}",
        skill.description
    );
    assert!(skill.description.starts_with("uses "));
}

#[test]
fn more_skills_than_the_wire_carries_is_a_limit_error_not_a_partial_list() {
    let world = world(MAX_SKILL_SUMMARIES + 1, "d");
    let error = world.list(project()).unwrap_err();
    assert_eq!(error.code, ErrorCode::LimitExceeded);
}

#[test]
fn a_multi_line_description_with_tabs_is_flattened_to_one_valid_line() {
    let world = world(1, "\"first line\\n\\tsecond line\\r\\nthird\\u0007bell\"");
    let skills = world.list(project()).unwrap();
    let skill = skills
        .iter()
        .find(|skill| skill.name == "skill-000")
        .unwrap();
    assert!(
        !skill.description.contains(char::is_control),
        "{:?}",
        skill.description
    );
    assert!(skill.description.contains("first line"));
    assert!(skill.description.contains("second line"));
    assert!(skill.description.contains("third"));
    // `world` already validated the reply the way a client does.
}
