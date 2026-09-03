//! otel cardinality guard の統合テスト。
//!
//! ID 形状キー・whitelist 外キー・domain 外値が
//! [`event_bus::validate_metric_attributes`] で拒否されることを pin し、
//! [`event_bus::ATTRIBUTE_WHITELIST`] 自体に ID 形状キーが混入していない
//! 不変条件を検査する。

use event_bus::{
    ATTRIBUTE_WHITELIST, CardinalityViolation, MetricAttribute, MetricMeasurement, MetricValue,
    TOKEN_UNIT, TOKEN_USAGE_METRIC, validate_metric_attributes,
};

fn measurement_with(key: &str, value: &str) -> MetricMeasurement {
    MetricMeasurement {
        name: TOKEN_USAGE_METRIC.to_owned(),
        unit: TOKEN_UNIT.to_owned(),
        value: MetricValue::U64(1),
        attrs: vec![MetricAttribute {
            key: key.to_owned(),
            value: value.to_owned(),
        }],
    }
}

// Given: ID 形状キー 7 種と whitelist 外キー (gen_ai.response.model) を注入
//        した measurement。
// When: validate_metric_attributes で検査する。
// Then: ID 形状キーは IdentifierLikeAttribute、whitelist 外キーは
//       UnknownAttributeKey で拒否される。gen_ai.request.model は
//       whitelist 8 キー目として許可されているため拒否リストに含めない。
#[test]
fn identifier_like_and_unknown_keys_are_rejected() {
    let cases = [
        ("request_id", "IdentifierLike"),
        ("session_id", "IdentifierLike"),
        ("task_id", "IdentifierLike"),
        ("run_id", "IdentifierLike"),
        ("call_id", "IdentifierLike"),
        ("message_id", "IdentifierLike"),
        ("foo.id", "IdentifierLike"),
        ("gen_ai.response.model", "Unknown"),
    ];

    for (key, expected) in cases {
        let measurement = measurement_with(key, "v");
        let result = validate_metric_attributes(&measurement);
        match expected {
            "IdentifierLike" => assert!(
                matches!(
                    result,
                    Err(CardinalityViolation::IdentifierLikeAttribute { .. })
                ),
                "key={key} result={result:?}"
            ),
            _ => assert!(
                matches!(
                    result,
                    Err(CardinalityViolation::UnknownAttributeKey { .. })
                ),
                "key={key} result={result:?}"
            ),
        }
    }
}

// Given: whitelist 8 キーとそれぞれの domain 内値 (model の "other" は
//        正規化済み値として常に許可される)。
// When: validate_metric_attributes で検査する。
// Then: いずれも通過する。
#[test]
fn allowed_whitelist_keys_are_accepted() {
    let cases = [
        ("gen_ai.operation.name", "chat"),
        ("gen_ai.provider.name", "anthropic"),
        // "other" は emitter が非 member model を正規化した結果の値であり、
        // shape ポリシーで常に許可される。
        ("gen_ai.request.model", "other"),
        ("gen_ai.token.type", "input"),
        ("error.type", "http"),
        ("evorch.profile.name", "primary"),
        ("evorch.delegation.depth", "1"),
        ("evorch.delegation.role", "worker"),
    ];

    for (key, value) in cases {
        let measurement = measurement_with(key, value);
        assert_eq!(
            validate_metric_attributes(&measurement),
            Ok(()),
            "key={key} value={value}"
        );
    }
}

// Given: 閉集合 domain 外の値 (token.type="reasoning" /
//        operation.name="embeddings" / provider.name="custom-proxy")、
//        shape ポリシー不適合の evorch 拡張値 (profile / depth / role)、
//        および shape ポリシー不適合の model 値。
// When: validate_metric_attributes で検査する。
// Then: InvalidAttributeValue で拒否される。
#[test]
fn out_of_domain_values_are_rejected() {
    let too_long_model = "m".repeat(129);
    let cases = [
        ("gen_ai.token.type", "reasoning"),
        ("gen_ai.operation.name", "embeddings"),
        ("gen_ai.provider.name", "custom-proxy"),
        ("error.type", "rate-limit"),
        ("evorch.profile.name", "GPU Pool"),
        ("evorch.profile.name", "プール"),
        ("evorch.delegation.depth", "100"),
        ("evorch.delegation.depth", "01"),
        ("evorch.delegation.depth", "abc"),
        ("evorch.delegation.role", "super-admin"),
        ("gen_ai.request.model", "has space"),
        ("gen_ai.request.model", too_long_model.as_str()),
        ("gen_ai.request.model", "モデル"),
    ];

    for (key, value) in cases {
        let measurement = measurement_with(key, value);
        let result = validate_metric_attributes(&measurement);
        assert!(
            matches!(
                result,
                Err(CardinalityViolation::InvalidAttributeValue { .. })
            ),
            "key={key} value={value} result={result:?}"
        );
    }
}

// Given: ATTRIBUTE_WHITELIST の全キー。
// When: ID 形状判定 (`.id` 終端・`*_id` 系 6 種の完全一致) を適用する。
// Then: いずれのキーも ID 形状ではない。
#[test]
fn whitelist_contains_no_identifier_like_keys() {
    for key in ATTRIBUTE_WHITELIST {
        let identifier_like = key.ends_with(".id")
            || matches!(
                key,
                "request_id" | "session_id" | "task_id" | "run_id" | "call_id" | "message_id"
            );
        assert!(!identifier_like, "whitelist key `{key}` is identifier-like");
    }
}
