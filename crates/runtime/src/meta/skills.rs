//! skill 本文・バンドルリソースを読み出す `skill_load` メタ操作ハンドラ (issue #53)。

use std::sync::Arc;

use serde::Deserialize;

use super::{DispatchResult, error, parse, success};
use crate::agent_loop::LoopState;
use crate::skill::{SkillRegistry, read_skill_resource};

#[derive(Deserialize)]
struct SkillLoadArgs {
    name: String,
    #[serde(default)]
    resource: Option<String>,
}

pub(super) fn skill_load(state: &LoopState, input: serde_json::Value) -> DispatchResult {
    skill_load_with_registry(state.skills(), input)
}

/// レジストリを引数に取る内部実装。LoopState は agent_loop モジュール外から
/// 構築できないため、単体テストはレジストリを直接渡すこの関数を検査する。
fn skill_load_with_registry(
    skills: Option<&Arc<SkillRegistry>>,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<SkillLoadArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let Some(registry) = skills else {
        return error("skill registry is not configured");
    };
    let Some(entry) = registry.get(&args.name) else {
        return error(format!("unknown skill: {}", args.name));
    };
    let loaded = match &args.resource {
        Some(reference) => {
            read_skill_resource(&entry.dir, reference).map_err(|load_error| load_error.to_string())
        }
        None => registry
            .load_body(&args.name)
            .map_err(|load_error| load_error.to_string()),
    };
    match loaded {
        Ok(content) => success(content),
        Err(load_error) => error(load_error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use serde_json::json;
    use tempfile::tempdir;

    use super::skill_load_with_registry;
    use crate::meta::Terminal;
    use crate::skill::{SkillRegistry, SkillScope, discover_skills};

    /// 本文と references/note.md を持つ `demo` skill 1 件のレジストリを組み立てる。
    fn demo_registry() -> (Arc<SkillRegistry>, tempfile::TempDir) {
        let root = tempdir().unwrap();
        let skills = root.path().join("skills");
        let skill_dir = skills.join("demo");
        fs::create_dir_all(skill_dir.join("references")).unwrap();
        fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: demo\ndescription: Demo skill\n---\nDEMO BODY SENTINEL\n",
        )
        .unwrap();
        fs::write(
            skill_dir.join("references").join("note.md"),
            "DEMO REFERENCE SENTINEL\n",
        )
        .unwrap();
        let registry = discover_skills(&[(SkillScope::Repo, skills)]);
        (Arc::new(registry), root)
    }

    // Given: demo skill を持つレジストリ
    // When:  {"name":"demo"} で skill_load を呼ぶ (stage 2)
    // Then:  frontmatter を含まない本文が success で返り run は継続指示になる
    #[test]
    fn skill_load_returns_body_without_frontmatter() {
        let (registry, _root) = demo_registry();

        let dispatch = skill_load_with_registry(Some(&registry), json!({ "name": "demo" }));

        assert!(!dispatch.result.is_error);
        assert_eq!(dispatch.result.content, "DEMO BODY SENTINEL\n");
        assert!(matches!(dispatch.terminal, Terminal::Continue));
    }

    // Given: references/note.md を持つ demo skill
    // When:  resource 付きで skill_load を呼ぶ (stage 3)
    // Then:  リソースファイルの内容が success で返る
    #[test]
    fn skill_load_resource_returns_resource_content() {
        let (registry, _root) = demo_registry();

        let dispatch = skill_load_with_registry(
            Some(&registry),
            json!({ "name": "demo", "resource": "references/note.md" }),
        );

        assert!(!dispatch.result.is_error);
        assert_eq!(dispatch.result.content, "DEMO REFERENCE SENTINEL\n");
    }

    // Given: demo skill と 2 階層目のリファレンス
    // When:  references/a/b.md を要求する
    // Then:  形状規約違反の型付きエラーで拒否される
    #[test]
    fn skill_load_rejects_reference_beyond_one_directory_level() {
        let (registry, _root) = demo_registry();

        let dispatch = skill_load_with_registry(
            Some(&registry),
            json!({ "name": "demo", "resource": "references/a/b.md" }),
        );

        assert!(dispatch.result.is_error);
        assert!(
            dispatch.result.content.contains("リファレンスが不正です"),
            "形状規約違反の識別子が欠落: {}",
            dispatch.result.content
        );
    }

    // Given: demo のみ登録されたレジストリ
    // When:  未登録名 nope を要求する
    // Then:  "unknown skill" 識別子付きの error になる
    #[test]
    fn skill_load_unknown_name_yields_unknown_skill_error() {
        let (registry, _root) = demo_registry();

        let dispatch = skill_load_with_registry(Some(&registry), json!({ "name": "nope" }));

        assert!(dispatch.result.is_error);
        assert!(
            dispatch.result.content.contains("unknown skill: nope"),
            "unknown skill 識別子が欠落: {}",
            dispatch.result.content
        );
    }

    // Given: レジストリ未接続の state
    // When:  skill_load を呼ぶ
    // Then:  "not configured" 識別子付きの error になる (fail-closed)
    #[test]
    fn skill_load_without_registry_fails_closed() {
        let dispatch = skill_load_with_registry(None, json!({ "name": "demo" }));

        assert!(dispatch.result.is_error);
        assert!(
            dispatch.result.content.contains("not configured"),
            "fail-closed 識別子が欠落: {}",
            dispatch.result.content
        );
    }
}
