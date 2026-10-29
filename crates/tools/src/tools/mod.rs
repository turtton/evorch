//! 標準ツール群の定義。
//!
//! 各ツールは [`crate::Tool`] を実装するスタブであり、実行ボディは wave 2 で
//! 実装する。スキーマと権限の宣言は最終契約である。

pub mod apply_patch;
pub mod browser;
pub mod edit;
pub mod file_access;
mod file_diff;
pub mod git_diff;
pub mod grep;
pub mod read;
pub mod run_js;
pub mod shell;
pub mod shell_escalation;
pub mod web_fetch;
pub mod web_search;
pub mod write;

pub use self::apply_patch::ApplyPatch;
pub use browser::{
    BROWSER_TOOL_NAMES, BrowserTool, BrowserToolOptions, GuardPolicy, browser_tools,
};
pub use edit::Edit;
pub use git_diff::GitDiff;
pub use grep::Grep;
pub use read::Read;
pub use run_js::RunJs;
pub use shell::Shell;
pub use web_fetch::WebFetch;
pub use web_search::WebSearch;
pub use write::Write;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandbox::DirectSandbox;

    use super::{ApplyPatch, Edit, GitDiff, Grep, Read, Shell, WebFetch, WebSearch, Write};
    use crate::tool::Tool;

    // Given: 標準ツールの静的スキーマ / When: jsonschema::validator_for でコンパイル / Then: すべて成功する
    #[test]
    fn all_standard_tool_schemas_compile() {
        let shell = Shell::new(Arc::new(DirectSandbox::new_unchecked()));
        let git_diff = GitDiff::new(Arc::new(DirectSandbox::new_unchecked()));
        let apply_patch = ApplyPatch::new();
        let schemas = [
            (Read.name(), Read.schema()),
            (Edit.name(), Edit.schema()),
            (Write.name(), Write.schema()),
            (apply_patch.name(), apply_patch.schema()),
            (Grep.name(), Grep.schema()),
            (
                shell.name(),
                Shell::new(Arc::new(DirectSandbox::new_unchecked())).schema(),
            ),
            (
                git_diff.name(),
                GitDiff::new(Arc::new(DirectSandbox::new_unchecked())).schema(),
            ),
        ];

        for (name, schema) in schemas {
            jsonschema::validator_for(&schema)
                .unwrap_or_else(|error| panic!("{name} のスキーマのコンパイルに失敗: {error}"));
        }
    }

    // Given: web_search / web_fetch の静的スキーマ / When: jsonschema::validator_for でコンパイル / Then: すべて成功する
    #[test]
    fn web_tool_schemas_compile() {
        let web_search = WebSearch::keyless_default()
            .unwrap_or_else(|error| panic!("WebSearch の生成に失敗: {error}"));
        let web_fetch =
            WebFetch::new().unwrap_or_else(|error| panic!("WebFetch の生成に失敗: {error}"));
        let schemas = [
            (web_search.name(), web_search.schema()),
            (web_fetch.name(), web_fetch.schema()),
        ];

        for (name, schema) in schemas {
            jsonschema::validator_for(&schema)
                .unwrap_or_else(|error| panic!("{name} のスキーマのコンパイルに失敗: {error}"));
        }
    }

    // Given: the four direct file tools / When: reading their schemas and validating args
    // Then: each accepts isolated / unsandboxed with a justification, and nothing else
    #[test]
    fn file_tool_schemas_accept_sandbox_access_and_justification() {
        let schemas = [
            (Read.name(), Read.schema()),
            (Edit.name(), Edit.schema()),
            (Write.name(), Write.schema()),
            (Grep.name(), Grep.schema()),
        ];
        for (name, schema) in schemas {
            let access = &schema["properties"]["sandbox_access"];
            assert_eq!(
                access["enum"],
                serde_json::json!(["isolated", "unsandboxed"]),
                "{name}"
            );
            assert_eq!(access["default"], "isolated", "{name}");
            let description = access["description"].as_str().unwrap();
            assert!(description.contains("not an isolated workspace"), "{name}");
            assert!(schema["properties"]["justification"].is_object(), "{name}");
            let validator = jsonschema::validator_for(&schema).unwrap();
            let mut args = serde_json::json!({
                "path": "/x", "content": "c", "old_string": "a", "new_string": "b",
                "pattern": "p", "sandbox_access": "unsandboxed", "justification": "why",
            });
            let properties = schema["properties"].as_object().unwrap();
            args.as_object_mut()
                .unwrap()
                .retain(|key, _| properties.contains_key(key));
            assert!(validator.is_valid(&args), "{name}");
            args["sandbox_access"] = "network".into();
            assert!(!validator.is_valid(&args), "{name} has no network mode");
        }
    }
}
