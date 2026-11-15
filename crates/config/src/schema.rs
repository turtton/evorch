//! 設定型から JSON Schema を生成します。

use crate::types::Config;

/// 現行の [`Config`] に対応する整形済み JSON Schema を返す。
///
/// # Panics
///
/// `schemars` が生成するスキーマは常に JSON 直列化可能であるため、直列化失敗は
/// 到達不能として扱う。
pub fn json_schema() -> String {
    serde_json::to_string_pretty(&schemars::schema_for!(Config))
        .expect("schemars が生成した Config schema は JSON として直列化可能")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: Config から生成した JSON Schema / When: serde_json でパースする
    // Then: 有効な JSON として読み取れる
    #[test]
    fn json_schema_parses_as_valid_json() {
        let schema = json_schema();

        let parsed: serde_json::Value =
            serde_json::from_str(&schema).expect("生成した schema は有効な JSON");

        assert!(parsed.is_object());
    }

    // Given: Config から生成した JSON Schema / When: ProviderType の列挙値を調べる
    // Then: anthropic と openai-compatible を含む
    #[test]
    fn json_schema_contains_provider_type_enum_values() {
        let schema = json_schema();

        assert!(schema.contains("\"anthropic\""));
        assert!(schema.contains("\"openai-compatible\""));
    }

    #[test]
    fn learning_drain_timeout_schema_carries_the_validated_range() {
        let schema: serde_json::Value = serde_json::from_str(&json_schema()).unwrap();
        let definition = ["$defs", "definitions"]
            .iter()
            .find_map(|key| schema.get(key)?.get("CoreRestartConfig"))
            .expect("CoreRestartConfig is a schema definition");
        let property = &definition["properties"]["learning_drain_timeout_secs"];
        let range = crate::types::LEARNING_DRAIN_TIMEOUT_SECS;
        assert_eq!(property["minimum"], *range.start());
        assert_eq!(property["maximum"], *range.end());
    }
}
