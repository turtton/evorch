//! ツール実行結果の型を定義します。

/// ツール実行の結果。
///
/// `content` は LLM へ返される本文、`is_error` は異常終了を示す。v0.2 で出力の
/// 由来を表す `ContentOrigin` フィールドを追加する予定のため（ADR 0008）、
/// `#[non_exhaustive]` としている。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ToolResult {
    /// LLM へ返す本文。
    pub content: String,
    /// ツール実行が異常終了した場合に `true`。
    pub is_error: bool,
}

impl ToolResult {
    /// 正常終了の結果を生成する。
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    /// 異常終了の結果を生成する。
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 本文となる文字列と String / When: success と error で生成 / Then: content と is_error が対応する
    #[test]
    fn result_success_and_error_constructors() {
        let ok = ToolResult::success("file content");
        assert_eq!(
            ok,
            ToolResult {
                content: "file content".to_string(),
                is_error: false,
            }
        );

        let owned = String::from("owned content");
        let ok_owned = ToolResult::success(owned);
        assert_eq!(ok_owned.content, "owned content");
        assert!(!ok_owned.is_error);

        let failure = ToolResult::error("command failed");
        assert_eq!(
            failure,
            ToolResult {
                content: "command failed".to_string(),
                is_error: true,
            }
        );
    }
}
