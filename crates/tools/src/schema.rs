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
        .map(|error| {
            let path = error.instance_path().as_str();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect::<Vec<_>>()
        .join("; ");
    if detail.is_empty() {
        return Ok(());
    }
    tracing::debug!(detail = %detail, "tool args validation failed");
    Err(ToolError::InvalidArgs { detail })
}

/// スキーマが決して受け付けない、任意の欄の `null` を取り除く。
///
/// 使わない任意の欄を省略せず `null` で埋めるモデルがある。型が `null` を含まない
/// 欄の `null` は検証で必ず失敗するため、省略と同じに扱う。必須の欄、型の指定が
/// ない欄、`null` を許す欄、スキーマにない欄はそのまま残し、検証に判断を任せる。
/// `properties` を持つ入れ子の object と、その形の `items` を持つ配列の要素にも
/// 同じ規則を当てる。
pub(crate) fn strip_null_optionals(schema: &serde_json::Value, args: &mut serde_json::Value) {
    let Some(object) = args.as_object_mut() else {
        return;
    };
    let Some(properties) = schema
        .get("properties")
        .and_then(serde_json::Value::as_object)
    else {
        return;
    };
    let required = schema.get("required").and_then(serde_json::Value::as_array);
    object.retain(|key, value| {
        !value.is_null()
            || required.is_some_and(|required| required.iter().any(|name| name == key.as_str()))
            || !properties.get(key).is_some_and(rejects_null)
    });
    for (key, value) in object.iter_mut() {
        let Some(property) = properties.get(key) else {
            continue;
        };
        match value {
            serde_json::Value::Object(_) => strip_null_optionals(property, value),
            serde_json::Value::Array(items) => {
                if let Some(item) = property.get("items") {
                    for value in items {
                        strip_null_optionals(item, value);
                    }
                }
            }
            _ => {}
        }
    }
}

/// `type` が `null` を含まず、`null` を必ず拒否するスキーマか。
fn rejects_null(schema: &serde_json::Value) -> bool {
    match schema.get("type") {
        Some(serde_json::Value::String(kind)) => kind != "null",
        Some(serde_json::Value::Array(kinds)) => !kinds.iter().any(|kind| kind == "null"),
        _ => false,
    }
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

    #[test]
    fn schema_length_error_identifies_the_argument_path() {
        let validator = compile(
            "test",
            &json!({
                "type": "object",
                "properties": {"escalation_reason": {"type": "string", "minLength": 1}}
            }),
        )
        .unwrap();
        let ToolError::InvalidArgs { detail } =
            validate_args(&validator, &json!({"escalation_reason": ""})).unwrap_err()
        else {
            panic!("expected invalid arguments");
        };
        assert!(detail.starts_with("/escalation_reason:"), "{detail}");
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
