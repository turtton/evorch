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
