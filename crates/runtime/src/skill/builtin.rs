//! バイナリ同梱 skill の静的定義とレジストリ entry への変換。
//!
//! 本番テーブルは空とし、fs の探索後に最低優先度でマージする。
//! 本文・リソースは静的文字列を参照し、ディスクアクセスを必要としない。

use super::registry::{SkillEntry, SkillScope, SkillSource};

/// 同梱 builtin skill の静的定義。今回は基盤のみで空。
/// 追加手順: crates/runtime/skills/builtin/<name>/ に SKILL.md と resources を配置し、
/// 下テーブルに (name, description, body, resources) を include_str! で登録する。
/// `body` は frontmatter を除いた本文のみとする。
struct EmbeddedSkillDef {
    name: &'static str,
    description: &'static str,
    body: &'static str,
    resources: &'static [(&'static str, &'static str)],
}

impl EmbeddedSkillDef {
    fn to_entry(&self) -> SkillEntry {
        SkillEntry {
            name: self.name.to_owned(),
            description: self.description.to_owned(),
            source: SkillSource::Embedded {
                body: self.body,
                resources: self.resources,
            },
            scope: SkillScope::Builtin,
        }
    }
}

const BUILTIN_SKILLS: &[EmbeddedSkillDef] = &[];

/// 同梱 skill の entry 一覧を定義順に返す。
pub(crate) fn builtin_skill_entries() -> Vec<SkillEntry> {
    BUILTIN_SKILLS
        .iter()
        .map(EmbeddedSkillDef::to_entry)
        .collect()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;

    use event_bus::SkillDiagnosticKind;
    use tempfile::tempdir;

    use super::*;
    use crate::skill::{SkillRegistry, SkillResourceError, discover_skills, split_frontmatter};

    const FIXTURE_BODY: &str = "BUILTIN BODY SENTINEL\n---\n本文中の区切りは保持する。\n";
    const FIXTURE_RESOURCES: &[(&str, &str)] = &[
        ("NOTES.md", "BUILTIN ROOT RESOURCE\n"),
        ("references/note.md", "BUILTIN REFERENCE SENTINEL\n"),
        // 不正なキーが存在しても、検索前に形状規約で拒否する。
        ("references/deep/note.md", "UNREACHABLE RESOURCE SENTINEL"),
    ];
    const FIXTURE: EmbeddedSkillDef = EmbeddedSkillDef {
        name: "demo-builtin",
        description: "Builtin fixture",
        body: FIXTURE_BODY,
        resources: FIXTURE_RESOURCES,
    };

    fn fixture_registry() -> SkillRegistry {
        let mut registry = SkillRegistry::new(BTreeMap::new(), Vec::new());
        registry.merge_shadowing(vec![FIXTURE.to_entry()]);
        registry
    }

    #[test]
    fn production_builtin_table_is_empty() {
        assert!(builtin_skill_entries().is_empty());
    }

    #[test]
    fn embedded_fixture_preserves_metadata_and_returns_body_verbatim() {
        let registry = fixture_registry();
        let entry = registry.get("demo-builtin").unwrap();
        assert_eq!(entry.name, "demo-builtin");
        assert_eq!(entry.description, "Builtin fixture");
        assert_eq!(entry.scope, SkillScope::Builtin);
        assert_eq!(
            entry.source,
            SkillSource::Embedded {
                body: FIXTURE_BODY,
                resources: FIXTURE_RESOURCES,
            }
        );
        assert_eq!(registry.load_body("demo-builtin").unwrap(), FIXTURE_BODY);
        assert_eq!(
            registry.available_skills()[0].description,
            "Builtin fixture"
        );
        assert!(registry.diagnostics.is_empty());
    }

    #[test]
    fn frontmatter_fixture_uses_only_the_split_body() {
        const SKILL_MD: &str =
            "---\nname: demo-builtin\ndescription: Builtin fixture\n---\n本文。\n";
        let (_, body) = split_frontmatter(SKILL_MD).unwrap();
        let fixture = EmbeddedSkillDef { body, ..FIXTURE };
        let mut registry = SkillRegistry::new(BTreeMap::new(), Vec::new());
        registry.merge_shadowing(vec![fixture.to_entry()]);

        assert_eq!(registry.load_body("demo-builtin").unwrap(), "本文。\n");
    }

    #[test]
    fn embedded_resources_resolve_root_and_one_level_references() {
        let entry = FIXTURE.to_entry();
        assert_eq!(
            entry.read_resource("NOTES.md").unwrap(),
            "BUILTIN ROOT RESOURCE\n"
        );
        assert_eq!(
            entry.read_resource("references/note.md").unwrap(),
            "BUILTIN REFERENCE SENTINEL\n"
        );
    }

    #[test]
    fn embedded_resource_missing_yields_not_found_without_content() {
        let error = FIXTURE.to_entry().read_resource("missing.md").unwrap_err();
        assert!(
            matches!(&error, SkillResourceError::NotFound(reference) if reference == "missing.md")
        );
        let message = error.to_string();
        assert!(message.contains("missing.md"));
        assert!(!message.contains("SENTINEL"));
    }

    #[test]
    fn embedded_resource_shape_is_validated_before_lookup() {
        let entry = FIXTURE.to_entry();
        for reference in [
            "references/deep/note.md",
            "../note.md",
            "./NOTES.md",
            "/private/full/path.md",
            "references\\note.md",
            "references//note.md",
            "references/",
            "",
        ] {
            let error = entry.read_resource(reference).unwrap_err();
            assert!(
                matches!(&error, SkillResourceError::InvalidReference(_)),
                "{reference}"
            );
            let message = error.to_string();
            assert!(!message.contains("/private/full/path.md"));
            assert!(!message.contains("SENTINEL"));
        }
    }

    #[test]
    fn filesystem_scopes_shadow_builtin_and_merge_appends_diagnostics_in_name_order() {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo-builtin");
        fs::create_dir_all(&skill_dir).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo-builtin\ndescription: Filesystem winner\n---\nFS BODY\n",
        )
        .unwrap();

        for scope in [SkillScope::Repo, SkillScope::RepoAgents, SkillScope::User] {
            let mut registry =
                discover_skills(&[(scope, skills.clone()), (SkillScope::User, skills.clone())]);
            let existing_diagnostics = registry.diagnostics.clone();
            assert_eq!(existing_diagnostics.len(), 1);
            let alpha = EmbeddedSkillDef {
                name: "alpha-builtin",
                ..FIXTURE
            };
            let zulu = EmbeddedSkillDef {
                name: "zulu-builtin",
                ..FIXTURE
            };

            registry.merge_shadowing(vec![zulu.to_entry(), FIXTURE.to_entry(), alpha.to_entry()]);

            let winner = registry.get("demo-builtin").unwrap();
            assert_eq!(winner.scope, scope);
            assert_eq!(winner.description, "Filesystem winner");
            assert!(matches!(&winner.source, SkillSource::Filesystem { .. }));
            assert_eq!(registry.load_body("demo-builtin").unwrap(), "FS BODY\n");
            assert_eq!(
                registry.get("alpha-builtin").unwrap().scope,
                SkillScope::Builtin
            );
            assert_eq!(registry.load_body("alpha-builtin").unwrap(), FIXTURE_BODY);
            assert_eq!(registry.load_body("zulu-builtin").unwrap(), FIXTURE_BODY);
            let available = registry.available_skills();
            assert_eq!(
                available
                    .iter()
                    .map(|skill| skill.name.as_str())
                    .collect::<Vec<_>>(),
                ["alpha-builtin", "demo-builtin", "zulu-builtin"]
            );
            assert_eq!(registry.diagnostics.len(), existing_diagnostics.len() + 1);
            assert_eq!(
                &registry.diagnostics[..existing_diagnostics.len()],
                &existing_diagnostics
            );
            let diagnostic = registry.diagnostics.last().unwrap();
            assert_eq!(diagnostic.kind, SkillDiagnosticKind::Shadowed);
            assert_eq!(diagnostic.skill, "demo-builtin");
            assert_eq!(diagnostic.scope, SkillScope::Builtin);
            assert_eq!(
                diagnostic.detail,
                format!(
                    "skill 'demo-builtin': {} scope shadows builtin scope",
                    scope.as_str()
                )
            );
            assert!(!diagnostic.detail.contains(root.path().to_str().unwrap()));
            assert!(!diagnostic.detail.contains("SENTINEL"));
        }
    }
}
