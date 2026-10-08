//! evorch専用の作成ガイドをrepo skillとして検証し、汎用builtinと分離する。

use std::path::PathBuf;
use std::sync::Arc;

use event_bus::EventBus;
use runtime::skill::{
    SkillScope, SkillSource, discover_skills, discover_with_builtin, parse_and_validate,
    split_frontmatter,
};
use runtime::{Role, SkillCatalogSource};

const NAME: &str = "evorch-builtin-skill-authoring";
const GUIDE: &str = include_str!("../../../.evorch/skills/evorch-builtin-skill-authoring/SKILL.md");

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn project_guide_has_valid_frontmatter_and_loads_from_repo_scope() {
    let frontmatter = parse_and_validate(GUIDE, NAME).unwrap();
    assert_eq!(frontmatter.name, NAME);
    assert!(frontmatter.description.contains("evorch開発専用"));
    let registry = discover_skills(&[(SkillScope::Repo, repo_root().join(".evorch/skills"))]);
    assert!(registry.diagnostics.is_empty());
    let entry = registry
        .get(NAME)
        .expect("project guide must be discovered");
    assert_eq!(entry.scope, SkillScope::Repo);
    assert!(matches!(entry.source, SkillSource::Filesystem { .. }));
    let (_, body) = split_frontmatter(GUIDE).unwrap();
    assert_eq!(registry.load_body(NAME).unwrap(), body);
    for path in [
        "crates/runtime/src/skill/builtin.rs",
        "crates/runtime/src/skill/frontmatter.rs",
        "crates/runtime/src/skill/discovery.rs",
        "crates/runtime/src/skill/registry.rs",
        "crates/runtime/src/skill_source.rs",
        "crates/runtime/skills/builtin/git-best-practices/SKILL.md",
        "crates/runtime/tests/builtin_git_skill.rs",
        "crates/runtime/tests/repo_skill_authoring.rs",
        "scripts/check-cache-contracts.sh",
    ] {
        assert!(GUIDE.contains(path), "guide must refer to {path}");
        assert!(
            repo_root().join(path).is_file(),
            "reference must exist: {path}"
        );
    }
}

#[test]
fn project_guide_is_not_distributed_as_a_builtin() {
    let builtins = discover_with_builtin(&[]);
    assert!(builtins.get(NAME).is_none());
    assert_eq!(
        builtins.get("git-best-practices").unwrap().scope,
        SkillScope::Builtin
    );
}

#[test]
fn source_publishes_project_metadata_without_loading_guide_body() {
    let source = SkillCatalogSource::new(
        config::Config::default(),
        None,
        Vec::new(),
        vec![(SkillScope::Repo, repo_root().join(".evorch/skills"))],
        Arc::new(EventBus::new(64)),
    );
    let snapshot = source.snapshot();
    assert_eq!(snapshot.registry.get(NAME).unwrap().scope, SkillScope::Repo);
    let prompt = snapshot
        .catalog
        .unwrap()
        .system_prompt_for(Role::Orchestrator, None, "mock-model")
        .unwrap();
    assert!(prompt.contains("- evorch-builtin-skill-authoring:"));
    assert!(!prompt.contains("# evorch: 組み込みskillの作成"));
    let (_, body) = split_frontmatter(GUIDE).unwrap();
    assert_eq!(snapshot.registry.load_body(NAME).unwrap(), body);
}

#[test]
fn test_policy_loads_from_standard_repo_agents_scope() {
    // Given: the policy installed in the repository's standard .agents scope.
    let root = repo_root();
    let name = "test-policy";
    let text = std::fs::read_to_string(root.join(".agents/skills/test-policy/SKILL.md")).unwrap();
    let metadata = parse_and_validate(&text, name).unwrap();
    let dirs = runtime::skill::default_skill_dirs(Some(&root))
        .into_iter()
        .filter(|(scope, _)| matches!(scope, SkillScope::Repo | SkillScope::RepoAgents))
        .collect();
    // When: production catalog construction discovers the repository scopes.
    let source = SkillCatalogSource::new(
        config::Config::default(),
        None,
        Vec::new(),
        dirs,
        Arc::new(EventBus::new(64)),
    );
    let snapshot = source.snapshot();
    // Then: metadata is advertised, while the body is loaded only on request.
    assert_eq!(
        snapshot.registry.get(name).unwrap().scope,
        SkillScope::RepoAgents
    );
    let prompt = snapshot
        .catalog
        .unwrap()
        .system_prompt_for(Role::Orchestrator, None, "mock-model")
        .unwrap();
    assert!(prompt.contains(&format!("- {name}: {}", metadata.description)));
    let (_, body) = split_frontmatter(&text).unwrap();
    assert!(!prompt.contains(body));
    assert_eq!(snapshot.registry.load_body(name).unwrap(), body);
    assert!(discover_with_builtin(&[]).get(name).is_none());
}

#[test]
fn every_repo_agents_skill_has_valid_frontmatter() {
    // Given: each skill directory under the repository's .agents scope.
    let skills = repo_root().join(".agents/skills");
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&skills).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        // When: its SKILL.md is parsed with the loader's validation.
        let text = std::fs::read_to_string(entry.path().join("SKILL.md")).unwrap();
        // Then: the frontmatter is valid and its name matches the directory.
        parse_and_validate(&text, &name).unwrap_or_else(|error| panic!("{name}: {error}"));
        names.push(name);
    }
    names.sort();
    assert!(names.contains(&"harness-diagnosis".to_owned()), "{names:?}");
}
