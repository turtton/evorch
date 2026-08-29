//! 標準ツール群の定義。
//!
//! 各ツールは [`crate::Tool`] を実装するスタブであり、実行ボディは wave 2 で
//! 実装する。スキーマと権限の宣言は最終契約である。

pub mod edit;
pub mod git_diff;
pub mod grep;
pub mod read;
pub mod shell;

pub use edit::Edit;
pub use git_diff::GitDiff;
pub use grep::Grep;
pub use read::Read;
pub use shell::Shell;

#[cfg(test)]
mod tests {
    use super::{Edit, GitDiff, Grep, Read, Shell};
    use crate::tool::Tool;

    // Given: 5 つの標準ツールの静的スキーマ / When: jsonschema::validator_for でコンパイル / Then: すべて成功する
    #[test]
    fn all_standard_tool_schemas_compile() {
        let schemas = [
            (Read.name(), Read.schema()),
            (Edit.name(), Edit.schema()),
            (Grep.name(), Grep.schema()),
            (Shell.name(), Shell.schema()),
            (GitDiff.name(), GitDiff.schema()),
        ];

        assert_eq!(schemas.len(), 5);
        for (name, schema) in schemas {
            jsonschema::validator_for(&schema)
                .unwrap_or_else(|error| panic!("{name} のスキーマのコンパイルに失敗: {error}"));
        }
    }
}
