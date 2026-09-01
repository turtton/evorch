//! ツール実行結果の型を定義します。

use crate::origin::ContentOrigin;

/// ツール実行の結果。
///
/// `content` は LLM へ返される本文、`is_error` は異常終了を示す。`origin` は
/// 出力の由来で、ToolExecutor が権限宣言から機械導出する (ADR 0008 / AC5)。
/// 将来のフィールド追加に備えて `#[non_exhaustive]` としている。
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct ToolResult {
    /// LLM へ返す本文。
    pub content: String,
    /// ツール実行が異常終了した場合に `true`。
    pub is_error: bool,
    /// ツールが添えたメタデータ。規定では `None`。
    pub detail: Option<serde_json::Value>,
    /// 出力の由来。コンストラクタでは fail-closed の [`ContentOrigin::WebUntrusted`]。
    pub origin: ContentOrigin,
}

impl ToolResult {
    /// 正常終了の結果を生成する。
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            detail: None,
            origin: ContentOrigin::WebUntrusted,
        }
    }

    /// 異常終了の結果を生成する。
    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            detail: None,
            origin: ContentOrigin::WebUntrusted,
        }
    }

    /// メタデータを添えた結果へ変換する。
    pub fn with_detail(mut self, detail: serde_json::Value) -> Self {
        self.detail = Some(detail);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 本文となる文字列と String / When: success と error で生成 / Then: content と is_error が対応し detail は None・origin は fail-closed の WebUntrusted
    #[test]
    fn result_success_and_error_constructors() {
        let ok = ToolResult::success("file content");
        assert_eq!(
            ok,
            ToolResult {
                content: "file content".to_string(),
                is_error: false,
                detail: None,
                origin: ContentOrigin::WebUntrusted,
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
                detail: None,
                origin: ContentOrigin::WebUntrusted,
            }
        );
    }

    // Given: 正常終了の結果 / When: with_detail でメタデータを添える / Then: detail に値が入り他のフィールドは不変
    #[test]
    fn with_detail_attaches_metadata() {
        let result = ToolResult::success("本文").with_detail(serde_json::json!({ "k": "v" }));

        assert_eq!(result.content, "本文");
        assert!(!result.is_error);
        assert_eq!(result.detail, Some(serde_json::json!({ "k": "v" })));
    }
}
