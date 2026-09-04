//! 標準ツール群の定義。
//!
//! 各ツールは [`crate::Tool`] を実装するスタブであり、実行ボディは wave 2 で
//! 実装する。スキーマと権限の宣言は最終契約である。

pub mod edit;
pub mod git_diff;
pub mod grep;
pub mod read;
pub mod shell;
pub mod shell_contract;
pub mod web_fetch;
pub mod web_search;

pub use edit::Edit;
pub use git_diff::GitDiff;
pub use grep::Grep;
pub use read::Read;
pub use shell::Shell;
pub use shell_contract::{CommandVerdict, ShellCommandContract};
pub use web_fetch::WebFetch;
pub use web_search::WebSearch;

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use sandbox::DirectSandbox;

    use super::{Edit, GitDiff, Grep, Read, Shell, WebFetch, WebSearch};
    use crate::tool::Tool;

    // Given: 5 つの標準ツールの静的スキーマ / When: jsonschema::validator_for でコンパイル / Then: すべて成功する
    #[test]
    fn all_standard_tool_schemas_compile() {
        let schemas = [
            (Read.name(), Read.schema()),
            (Edit.name(), Edit.schema()),
            (Grep.name(), Grep.schema()),
            (
                Shell::new(Arc::new(DirectSandbox::new_unchecked())).name(),
                Shell::new(Arc::new(DirectSandbox::new_unchecked())).schema(),
            ),
            (
                GitDiff::new(Arc::new(DirectSandbox::new_unchecked())).name(),
                GitDiff::new(Arc::new(DirectSandbox::new_unchecked())).schema(),
            ),
        ];

        assert_eq!(schemas.len(), 5);
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

        assert_eq!(schemas.len(), 2);
        for (name, schema) in schemas {
            jsonschema::validator_for(&schema)
                .unwrap_or_else(|error| panic!("{name} のスキーマのコンパイルに失敗: {error}"));
        }
    }
}
