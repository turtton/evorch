//! ツール引数スキーマ検証の内部ヘルパ。
//!
//! [`jsonschema::validator_for`] でコンパイルした [`jsonschema::Validator`] を
//! 呼び出し側がキャッシュして再利用する想定の薄いラッパ。コンパイル失敗は
//! [`ToolError::InvalidSchema`] へ、引数検証の失敗は [`ToolError::InvalidArgs`]
//! へ対応付ける。

use crate::error::ToolError;

/// JSON スキーマをコンパイルして検証器を返す。
///
/// # Errors
///
/// スキーマが不正な場合は `tool_name` を報告先に持つ
/// [`ToolError::InvalidSchema`] を返す。
pub(crate) fn compile(
    tool_name: &str,
    schema: &serde_json::Value,
) -> Result<jsonschema::Validator, ToolError> {
    jsonschema::validator_for(schema).map_err(|error| {
        let detail = error.to_string();
        tracing::debug!(tool = %tool_name, detail = %detail, "tool schema compilation failed");
        ToolError::InvalidSchema {
            tool_name: tool_name.to_string(),
            detail,
        }
    })
}

/// コンパイル済み検証器でツール引数を検証する。
///
/// # Errors
///
/// 検証違反がある場合は、各違反を `"; "` で連結した detail を持つ
/// [`ToolError::InvalidArgs`] を返す。
pub(crate) fn validate_args(
    validator: &jsonschema::Validator,
    args: &serde_json::Value,
) -> Result<(), ToolError> {
    let detail = validator
        .iter_errors(args)
        .map(|error| error.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    if detail.is_empty() {
        return Ok(());
    }
    tracing::debug!(detail = %detail, "tool args validation failed");
    Err(ToolError::InvalidArgs { detail })
}

#[cfg(test)]
mod tests {
    use super::{compile, validate_args};
    use crate::error::ToolError;
    use serde_json::json;

    /// テスト用の object スキーマ（required を 2 項目持つ）。
    fn test_schema() -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "mode": { "type": "string" }
            },
            "required": ["path", "mode"],
            "additionalProperties": false
        })
    }

    // Given: 正しいスキーマと適合する引数 / When: compile して validate_args / Then: Ok になる
    #[test]
    fn schema_valid_args_pass() {
        let validator = compile("test", &test_schema()).expect("スキーマはコンパイルできる");

        assert_eq!(
            validate_args(&validator, &json!({"path": "a.txt", "mode": "r"})),
            Ok(())
        );
    }

    // Given: required を欠く引数 / When: validate_args / Then: InvalidArgs になり detail に違反が "; " 連結される
    #[test]
    fn schema_missing_required_is_invalid_args() {
        let validator = compile("test", &test_schema()).expect("スキーマはコンパイルできる");

        let error = validate_args(&validator, &json!({})).expect_err("引数は不正");
        let ToolError::InvalidArgs { detail } = error else {
            panic!("InvalidArgs を期待しましたが {error:?} でした");
        };
        assert!(
            detail.contains("; "),
            "複数違反が '; ' で連結される: {detail}"
        );
        assert!(
            detail.contains("path"),
            "欠落プロパティ名が含まれる: {detail}"
        );
        assert!(
            detail.contains("mode"),
            "欠落プロパティ名が含まれる: {detail}"
        );
    }

    // Given: プロパティの型が違う引数 / When: validate_args / Then: InvalidArgs になる
    #[test]
    fn schema_wrong_type_is_invalid_args() {
        let validator = compile("test", &test_schema()).expect("スキーマはコンパイルできる");

        let error =
            validate_args(&validator, &json!({"path": 1, "mode": "r"})).expect_err("引数は不正");
        let ToolError::InvalidArgs { detail } = error else {
            panic!("InvalidArgs を期待しましたが {error:?} でした");
        };
        assert!(!detail.is_empty());
    }

    // Given: 未定義プロパティを含む引数 / When: validate_args / Then: InvalidArgs になる
    #[test]
    fn schema_additional_properties_rejected() {
        let validator = compile("test", &test_schema()).expect("スキーマはコンパイルできる");

        let error = validate_args(
            &validator,
            &json!({"path": "a.txt", "mode": "r", "extra": true}),
        )
        .expect_err("引数は不正");
        let ToolError::InvalidArgs { detail } = error else {
            panic!("InvalidArgs を期待しましたが {error:?} でした");
        };
        assert!(!detail.is_empty());
    }

    // Given: コンパイルできないスキーマ / When: compile / Then: tool_name を報告する InvalidSchema になる
    #[test]
    fn schema_uncompilable_schema_is_invalid_schema() {
        let error = compile("read", &json!({"$ref": "#/definitions/missing"}))
            .expect_err("スキーマはコンパイルできない");

        let ToolError::InvalidSchema { tool_name, detail } = error else {
            panic!("InvalidSchema を期待しましたが {error:?} でした");
        };
        assert_eq!(tool_name, "read");
        assert!(!detail.is_empty());
    }
}
