//! otel cardinality guard の統合テスト。
//!
//! ID 形状キー・whitelist 外キー・domain 外値が
//! [`event_bus::validate_metric_attributes`] で拒否されることを pin し、
//! [`event_bus::ATTRIBUTE_WHITELIST`] 自体に ID 形状キーが混入していない
//! 不変条件を検査する。

use event_bus::otel::{
    ATTRIBUTE_WHITELIST, SPAN_ATTRIBUTE_WHITELIST, SpanAttribute, SpanAttributeValue,
    SpanAttributeViolation, validate_metric_attributes, validate_span_attributes,
};
use event_bus::{
    CardinalityViolation, MetricAttribute, MetricMeasurement, MetricValue, TOKEN_UNIT,
    TOKEN_USAGE_METRIC,
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

// Given: metrics whitelist と span whitelist の全キー。
// When: span 側から ID 形状キー (`.id` 終端) を動的に抽出する。
// Then: metrics whitelist との交差は空であり、span ID キーが metrics 層へ混入しない。
#[test]
fn metric_whitelist_is_disjoint_from_span_identifier_keys() {
    let span_identifier_keys: Vec<&str> = SPAN_ATTRIBUTE_WHITELIST
        .into_iter()
        .filter(|key| key.ends_with(".id"))
        .collect();

    assert!(
        ATTRIBUTE_WHITELIST
            .iter()
            .all(|key| !span_identifier_keys.contains(key)),
        "metrics whitelist contains span identifier key: {span_identifier_keys:?}"
    );
}

// Given: SPAN_ATTRIBUTE_WHITELIST。
// When: 隣接要素を比較して閉集合の不変条件を検査する。
// Then: 辞書順で、重複がない。
#[test]
fn span_whitelist_is_sorted_and_unique() {
    for pair in SPAN_ATTRIBUTE_WHITELIST.windows(2) {
        assert!(
            pair[0] < pair[1],
            "span whitelist is not sorted and unique: {pair:?}"
        );
    }
}

// Given: raw-content / credential 形状を示す denylist の部分文字列。
// When: span whitelist の各キーへ適用する。
// Then: 生内容や credential を表すキーは一切含まれない。
#[test]
fn span_whitelist_contains_no_raw_content_or_credential_keys() {
    let denied_parts = [
        "gen_ai.prompt",
        "gen_ai.completion",
        "content",
        "message",
        "body",
        "sse",
        "credential",
        "token",
        "api_key",
    ];

    for key in SPAN_ATTRIBUTE_WHITELIST {
        assert!(
            denied_parts.iter().all(|part| {
                !key.contains(part) || (part == &"token" && key.starts_with("gen_ai.usage."))
            }),
            "span whitelist contains denied key shape: {key}"
        );
    }
}

// Given: span whitelist の evorch.* キー。
// When: AC3 で許可された ID と低カーディナリティ構造軸の集合と比較する。
// Then: evorch.* キーが期待集合に exact 一致する。
#[test]
fn span_whitelist_evorch_keys_match_ac3_expected_set() {
    let expected = [
        "evorch.agent.name",
        "evorch.agent_run.id",
        "evorch.delegation.depth",
        "evorch.delegation.role",
        "evorch.parent_agent_run.id",
        "evorch.request.id",
        "evorch.session.id",
        "evorch.task.id",
    ];
    let actual: Vec<&str> = SPAN_ATTRIBUTE_WHITELIST
        .into_iter()
        .filter(|key| key.starts_with("evorch."))
        .collect();

    assert_eq!(actual, expected);
}

// Given: span 属性列に ID 形状キーと raw-content 形状キーを含める。
// When: validate_span_attributes で検査する。
// Then: いずれも対応する violation として拒否される。
#[test]
fn span_validator_rejects_identifier_and_raw_content_keys() {
    let cases = [
        (
            SpanAttribute {
                key: "evorch.unknown.id".to_owned(),
                value: SpanAttributeValue::Str("session-1".to_owned()),
            },
            SpanAttributeViolation::UnknownKey {
                key: "evorch.unknown.id".to_owned(),
            },
        ),
        (
            SpanAttribute {
                key: "gen_ai.prompt.content".to_owned(),
                value: SpanAttributeValue::Str("secret".to_owned()),
            },
            SpanAttributeViolation::RawContentKey {
                key: "gen_ai.prompt.content".to_owned(),
            },
        ),
    ];

    for (attribute, expected) in cases {
        assert_eq!(validate_span_attributes(&[attribute]), Err(expected));
    }
}
