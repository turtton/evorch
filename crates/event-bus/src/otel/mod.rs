// allow: SIZE_OK - 属性 mapping 表 (module doc) と写像 core、そして網羅的
// in-module テストを 1 つの対応表として保持するため分割不可能
// (event.rs の先例に準拠)。
//! OpenTelemetry semantic conventions 写像層 (issue #55)。
//!
//! イベントバス上の [`Event`] を GenAI metrics semantic conventions
//! v1.37.0 ([`SEMCONV_PIN`]) の metric measurements へ写像する。
//!
//! Pin 先 (canonical URL): <https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/>
//! (採用リリース v1.37.0)。v1.37.0 以降このページは
//! <https://github.com/open-telemetry/semantic-conventions-genai> へ移動したため、
//! 本実装の内容は `open-telemetry/semantic-conventions` の `v1.37.0` タグ
//! (<https://raw.githubusercontent.com/open-telemetry/semantic-conventions/v1.37.0/docs/gen-ai/gen-ai-metrics.md>)
//! で確認した。
//!
//! # 属性 mapping 表
//!
//! | source event | metric name | instrument / unit | attributes (順序固定) |
//! |---|---|---|---|
//! | `UsageEvent::Usage` | `gen_ai.client.token.usage` | u64 histogram / `{token}` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `gen_ai.token.type` |
//! | `ProviderEvent::RequestCompleted` | `gen_ai.client.operation.duration` | f64 histogram / `s` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, (`evorch.profile.name`) |
//! | `ProviderEvent::RequestFailed` | `gen_ai.client.operation.duration` | f64 histogram / `s` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, `error.type`, (`evorch.profile.name`) |
//! | `ProviderEvent::FirstTokenObserved` | `evorch.client.time_to_first_token` | f64 histogram / `s` | `gen_ai.operation.name`, `gen_ai.provider.name`, `gen_ai.request.model`, (`evorch.profile.name`) |
//!
//! 括弧付き属性は条件付き: `profile` が `Some` かつ shape ポリシー
//! ([`is_profile_name_valid`]) に適合するとき、map_event は属性を付与する
//! (不適合値は measurement を保持したまま属性のみ省略)。`gen_ai.request.model`
//! は metrics の必須次元であるため属性は**常に付与**される: shape ポリシー
//! ([`is_model_name_valid`]) 適合時は元値、不適合時は固定値 `other` に
//! 畳み込む ([`normalize_model_shape`])。さらに exporter 層
//! (otel-exporter feature) は、初期化時に渡された registry の非 member で
//! ある属性を emit 時に正規化する: profile は属性を除外し、model は値を
//! `other` へ書き換える (属性は残す)。つまり OTLP label への profile 属性
//! の emit 条件は「map 層 shape 適合 ∧ emitter registry member」、model
//! 属性は無条件で存在し、その値は registry member 値または `other` に
//! 有界化される。attribute value は [`validate_metric_attributes`] の
//! domain 検査 (閉集合 / shape ポリシー) と exporter registry による
//! 数的有界化で保護される。
//!
//! # 非写像 event と理由
//!
//! | event | 理由 |
//! |---|---|
//! | `ProviderEvent::RequestStarted` | duration は `RequestCompleted` / `RequestFailed` で捕捉される |
//! | `UsageEvent::CacheStats` | cache metrics 単独項目は roadmap v0.3 以降 |
//! | `ProviderEvent::FallbackTriggered` / `ProviderEvent::ProviderFallback` | whitelist 最終決定後、slice ②以降 |
//! | `Lifecycle` / `Message` / `Tool` / `Fault` / `AgentMessage` | 本 slice 対象外 |
//!
//! # 意図的省略
//!
//! - `gen_ai.system`: v1.37.0 で deprecated のため `gen_ai.provider.name` を採用する。
//! - `server.address` / `server.port`: provider event に存在しないため。
//! - `finish_reason`: semconv metric 属性に存在しないため
//!   (`RequestCompleted` の `finish_reason` フィールドは写像しない)。
//! - `evorch.delegation.depth` / `evorch.delegation.role`:
//!   [`ATTRIBUTE_WHITELIST`] 定義のみで、供給 event 未実装のため本 slice
//!   では emit しない。値 domain は固定済み (depth: `0`..=`99` の decimal
//!   文字列で leading zero なし / role: repo 既定の agent role 語彙
//!   `orchestrator` / `explorer` / `worker` / `reviewer`) であり、供給
//!   event 実装時に [`validate_metric_attributes`] が即適用する。
//! - `evorch.client.time_to_first_token`: semconv v1.37.0 には server 側の
//!   `gen_ai.server.time_to_first_token` しか無く、client 観測は evorch.*
//!   拡張で表現する。
//!
//! # `gen_ai.request.model` の管理方針
//!
//! model 名は metrics の必須次元であり観測から落とせないため、whitelist に
//! 含める。カーディナリティの責務者は provider profile config 宣言集合
//! (emitter 初期化時の `known_models` registry、上限
//! [`super::MAX_MODEL_NAMES`]) であり、非 member の model は exporter が
//! 固定値 `other` へ正規化する (属性自体は残す)。本層 / validator は
//! 正規化防壁としてのみ動作する: shape ポリシー
//! ([`is_model_name_valid`]: 非空・128 文字以下・printable ASCII) 不適合の
//! model 文字列は map 層で [`normalize_model_shape`] により `other` に
//! 畳み込まれ、validator は DTO 直構築への防御として shape gate を行う。
//!
//! # 非ゴール
//!
//! 本 slice では runtime への subscribe 配線を行わない (ADR 0014 の
//! 有効化経路は後続 slice)。

use serde::{Deserialize, Serialize};

use crate::event::{Event, EventKind, ProviderEvent, ProviderFailureKind, UsageEvent};

#[cfg(feature = "otel-exporter")]
pub mod exporter;
pub mod span;

pub use span::{
    FiniteF64, SpanAction, SpanAttribute, SpanAttributeValue, SpanDrop, SpanDropKind, SpanKey,
    SpanKind, SpanMapper, SpanStatus,
};

/// 写像先の GenAI metrics semantic conventions の pin バージョン。
pub const SEMCONV_PIN: &str = "1.37.0";

/// `gen_ai.client.token.usage` (u64 histogram, `{token}`) の metric 名。
pub const TOKEN_USAGE_METRIC: &str = "gen_ai.client.token.usage";
/// `gen_ai.client.operation.duration` (f64 histogram, `s`) の metric 名。
pub const OPERATION_DURATION_METRIC: &str = "gen_ai.client.operation.duration";
/// evorch 拡張の client TTFT metric 名 (f64 histogram, `s`)。
pub const TIME_TO_FIRST_TOKEN_METRIC: &str = "evorch.client.time_to_first_token";
/// token histogram の unit。
pub const TOKEN_UNIT: &str = "{token}";
/// 時間 histogram の unit (秒)。
pub const SECONDS_UNIT: &str = "s";

/// metric attribute キーの whitelist。
///
/// このリスト外のキーは [`validate_metric_attributes`] が拒否する。ID 形状
/// キー (`.id` 終端・`*_id` 系) は whitelist に含めない方針であり、その
/// 不変条件は統合テスト (tests/otel_cardinality.rs) で検査する。
pub const ATTRIBUTE_WHITELIST: [&str; 8] = [
    "gen_ai.operation.name",
    "gen_ai.provider.name",
    "gen_ai.request.model",
    "gen_ai.token.type",
    "error.type",
    "evorch.profile.name",
    "evorch.delegation.depth",
    "evorch.delegation.role",
];

/// `gen_ai.operation.name` の閉集合 domain。
const OPERATION_NAME_DOMAIN: [&str; 1] = ["chat"];
/// `gen_ai.token.type` の閉集合 domain。
const TOKEN_TYPE_DOMAIN: [&str; 4] = ["input", "output", "cache_read", "cache_write"];
/// `gen_ai.provider.name` の閉集合 domain (正規化後)。
const PROVIDER_NAME_DOMAIN: [&str; 4] = ["anthropic", "openai", "openai-compatible", "other"];
/// `error.type` の閉集合 domain ([`ProviderFailureKind`] 全 variant の正規化値)。
const ERROR_TYPE_DOMAIN: [&str; 9] = [
    "rate_limited",
    "http",
    "timeout",
    "invalid_response",
    "transport",
    "server",
    "quota",
    "auth",
    "other",
];
/// `evorch.delegation.role` の閉集合 domain (repo 既定の agent role 語彙)。
const DELEGATION_ROLE_DOMAIN: [&str; 4] = ["orchestrator", "explorer", "worker", "reviewer"];

const ATTR_OPERATION_NAME: &str = "gen_ai.operation.name";
const ATTR_PROVIDER_NAME: &str = "gen_ai.provider.name";
const ATTR_TOKEN_TYPE: &str = "gen_ai.token.type";
const ATTR_ERROR_TYPE: &str = "error.type";
const ATTR_PROFILE_NAME: &str = "evorch.profile.name";
const ATTR_DELEGATION_DEPTH: &str = "evorch.delegation.depth";
const ATTR_DELEGATION_ROLE: &str = "evorch.delegation.role";

/// `gen_ai.request.model` 属性キー (whitelist 8 キー目)。
///
/// 値の数的有界性は otel-exporter feature の emitter 初期化時
/// `known_models` registry (上限 [`super::MAX_MODEL_NAMES`]) が担い、非
/// member は `other` へ正規化される。本層 / validator は shape 防壁のみ。
pub const ATTR_REQUEST_MODEL: &str = "gen_ai.request.model";

/// metric attribute の key-value 対。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricAttribute {
    /// semconv または evorch 拡張の属性キー。
    pub key: String,
    /// 属性値 (低カーディナリティ文字列)。
    pub value: String,
}

impl MetricAttribute {
    fn new(key: &'static str, value: impl Into<String>) -> Self {
        Self {
            key: key.to_owned(),
            value: value.into(),
        }
    }
}

/// histogram 記録値の閉型。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum MetricValue {
    /// u64 histogram の記録値。
    U64(u64),
    /// f64 histogram の記録値。
    F64(f64),
}

/// 1 measurement 分の DTO。
///
/// `attrs` は意図的に [`Vec`] (固定順) であり、golden テストの順序安定性を
/// 保つ (`HashMap` は使用しない)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricMeasurement {
    /// metric 名 (例: `gen_ai.client.token.usage`)。
    pub name: String,
    /// UCUM unit (例: `{token}`)。
    pub unit: String,
    /// 記録値。
    pub value: MetricValue,
    /// 順序固定の属性リスト。
    pub attrs: Vec<MetricAttribute>,
}

impl MetricMeasurement {
    fn new(
        name: &'static str,
        unit: &'static str,
        value: MetricValue,
        attrs: Vec<MetricAttribute>,
    ) -> Self {
        Self {
            name: name.to_owned(),
            unit: unit.to_owned(),
            value,
            attrs,
        }
    }
}

/// 属性検査違反。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CardinalityViolation {
    /// [`ATTRIBUTE_WHITELIST`] 外の属性キー。
    UnknownAttributeKey {
        /// 違反したキー。
        key: String,
    },
    /// 閉集合 domain 外の属性値。
    InvalidAttributeValue {
        /// 対象のキー。
        key: String,
        /// domain 外の値。
        value: String,
    },
    /// ID 形状の高カーディナリティ属性キー (whitelist 内外を問わず常に違反)。
    IdentifierLikeAttribute {
        /// 違反したキー。
        key: String,
    },
}

impl std::fmt::Display for CardinalityViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownAttributeKey { key } => {
                write!(f, "attribute key `{key}` is not whitelisted")
            }
            Self::InvalidAttributeValue { key, value } => write!(
                f,
                "attribute value `{value}` is out of the closed domain of `{key}`"
            ),
            Self::IdentifierLikeAttribute { key } => write!(
                f,
                "attribute key `{key}` looks like a high-cardinality identifier"
            ),
        }
    }
}

impl std::error::Error for CardinalityViolation {}

/// イベントを metric measurements へ写像する。
///
/// 写像対象外の variant は空 [`Vec`] を返す (module doc の非写像表を参照)。
/// 新しい variant が追加された場合、この match はコンパイルエラーになり、
/// 写像要否の再検討を強制する。
pub fn map_event(event: &Event) -> Vec<MetricMeasurement> {
    match &event.kind {
        EventKind::Usage(usage) => match usage {
            UsageEvent::Usage {
                provider,
                model,
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                ..
            } => {
                let provider = normalize_provider(provider);
                let mut token_kinds: Vec<(&str, u64)> =
                    vec![("input", *input_tokens), ("output", *output_tokens)];
                if *cache_read_tokens > 0 {
                    token_kinds.push(("cache_read", *cache_read_tokens));
                }
                if *cache_write_tokens > 0 {
                    token_kinds.push(("cache_write", *cache_write_tokens));
                }
                token_kinds
                    .into_iter()
                    .map(|(token_type, value)| {
                        let mut attrs = vec![
                            MetricAttribute::new(ATTR_OPERATION_NAME, "chat"),
                            MetricAttribute::new(ATTR_PROVIDER_NAME, provider),
                            MetricAttribute::new(ATTR_REQUEST_MODEL, normalize_model_shape(model)),
                        ];
                        attrs.push(MetricAttribute::new(ATTR_TOKEN_TYPE, token_type));
                        MetricMeasurement::new(
                            TOKEN_USAGE_METRIC,
                            TOKEN_UNIT,
                            MetricValue::U64(value),
                            attrs,
                        )
                    })
                    .collect()
            }
            UsageEvent::CacheStats { .. } => Vec::new(),
        },
        EventKind::Provider(provider) => match provider {
            ProviderEvent::RequestCompleted {
                provider,
                model,
                profile,
                duration_ms,
                ..
            } => vec![MetricMeasurement::new(
                OPERATION_DURATION_METRIC,
                SECONDS_UNIT,
                MetricValue::F64(*duration_ms as f64 / 1000.0),
                operation_attrs(provider, model, profile.as_deref(), None),
            )],
            ProviderEvent::RequestFailed {
                provider,
                model,
                profile,
                duration_ms,
                failure,
                ..
            } => vec![MetricMeasurement::new(
                OPERATION_DURATION_METRIC,
                SECONDS_UNIT,
                MetricValue::F64(*duration_ms as f64 / 1000.0),
                operation_attrs(
                    provider,
                    model,
                    profile.as_deref(),
                    Some(map_failure(failure)),
                ),
            )],
            ProviderEvent::FirstTokenObserved {
                provider,
                model,
                profile,
                ttft_ms,
                ..
            } => vec![MetricMeasurement::new(
                TIME_TO_FIRST_TOKEN_METRIC,
                SECONDS_UNIT,
                MetricValue::F64(*ttft_ms as f64 / 1000.0),
                operation_attrs(provider, model, profile.as_deref(), None),
            )],
            ProviderEvent::RequestStarted { .. }
            | ProviderEvent::ProviderFallback { .. }
            | ProviderEvent::FallbackTriggered { .. } => Vec::new(),
        },
        EventKind::Lifecycle(_)
        | EventKind::Message(_)
        | EventKind::Tool(_)
        | EventKind::Fault(_)
        | EventKind::AgentMessage(_) => Vec::new(),
    }
}

/// measurement の属性を whitelist と値 domain で検査する。
///
/// ID 形状キーの防御深層検査を whitelist 判定より先に行い、将来
/// [`ATTRIBUTE_WHITELIST`] に ID 形状キーが誤って混入しても常に拒否する。
/// whitelisted キーの値は [`value_domain`] の domain (閉集合 / shape
/// ポリシー) に適合しなければならない。
///
/// # Errors
/// whitelist 外キー・domain 外値・ID 形状キーのいずれかを検出した場合、
/// [`CardinalityViolation`] を返す。
pub fn validate_metric_attributes(
    measurement: &MetricMeasurement,
) -> Result<(), CardinalityViolation> {
    for attr in &measurement.attrs {
        if is_identifier_like_key(&attr.key) {
            return Err(CardinalityViolation::IdentifierLikeAttribute {
                key: attr.key.clone(),
            });
        }
        if !ATTRIBUTE_WHITELIST.contains(&attr.key.as_str()) {
            return Err(CardinalityViolation::UnknownAttributeKey {
                key: attr.key.clone(),
            });
        }
        let violation = match value_domain(&attr.key) {
            Some(AttributeDomain::Closed(domain)) => {
                (!domain.contains(&attr.value.as_str())).then(|| attr.value.clone())
            }
            Some(AttributeDomain::ProfileName) => {
                (!is_profile_name_valid(&attr.value)).then(|| attr.value.clone())
            }
            Some(AttributeDomain::DelegationDepth) => {
                (!is_delegation_depth_valid(&attr.value)).then(|| attr.value.clone())
            }
            Some(AttributeDomain::RequestModel) => {
                (!is_model_name_valid(&attr.value)).then(|| attr.value.clone())
            }
            None => None,
        };
        if let Some(value) = violation {
            return Err(CardinalityViolation::InvalidAttributeValue {
                key: attr.key.clone(),
                value,
            });
        }
    }
    Ok(())
}

/// duration / TTFT 系 measurement 共通の属性列を作る。
///
/// 順序は semconv 定義キー (`gen_ai.operation.name`, `gen_ai.provider.name`,
/// `gen_ai.request.model`, `error.type`) の後に evorch 拡張
/// (`evorch.profile.name`) を置く。model は必須次元のため無条件で付与し、
/// shape 不適合値は [`normalize_model_shape`] が固定値 `other` に畳み込む。
/// profile は [`is_profile_name_valid`] の shape ポリシーに適合するときのみ
/// 付与する。
fn operation_attrs(
    provider: &str,
    model: &str,
    profile: Option<&str>,
    error_type: Option<&'static str>,
) -> Vec<MetricAttribute> {
    let mut attrs = vec![
        MetricAttribute::new(ATTR_OPERATION_NAME, "chat"),
        MetricAttribute::new(ATTR_PROVIDER_NAME, normalize_provider(provider)),
        MetricAttribute::new(ATTR_REQUEST_MODEL, normalize_model_shape(model)),
    ];
    if let Some(error_type) = error_type {
        attrs.push(MetricAttribute::new(ATTR_ERROR_TYPE, error_type));
    }
    if let Some(profile) = profile.filter(|profile| is_profile_name_valid(profile)) {
        attrs.push(MetricAttribute::new(ATTR_PROFILE_NAME, profile.to_owned()));
    }
    attrs
}

/// provider 識別子を閉集合 domain へ正規化する。
///
/// pass-through は禁止し、未知値はすべて `other` へ bucket する。
fn normalize_provider(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "openai-compatible" => "openai-compatible",
        _ => "other",
    }
}

/// [`ProviderFailureKind`] を低カーディナリティな `error.type` 値へ正規化する。
///
/// `Http` の status はカーディナリティ保護のため捨てる。
fn map_failure(failure: &ProviderFailureKind) -> &'static str {
    match failure {
        ProviderFailureKind::RateLimited => "rate_limited",
        ProviderFailureKind::Http { .. } => "http",
        ProviderFailureKind::Timeout => "timeout",
        ProviderFailureKind::InvalidResponse => "invalid_response",
        ProviderFailureKind::Transport => "transport",
        ProviderFailureKind::Server => "server",
        ProviderFailureKind::Quota => "quota",
        ProviderFailureKind::Auth => "auth",
        ProviderFailureKind::Other => "other",
    }
}

/// attribute キーごとの値検査方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttributeDomain {
    /// 閉集合 domain に含まれる値のみ許容。
    Closed(&'static [&'static str]),
    /// profile 名の shape ポリシー ([`is_profile_name_valid`]) 適合のみ許容。
    ProfileName,
    /// `0`..=`99` の decimal 文字列 (leading zero なし) のみ許容。
    DelegationDepth,
    /// model 名の shape ポリシー ([`is_model_name_valid`]) 適合のみ許容。
    ///
    /// `"other"` (emitter が非 member model を正規化した結果の値) も shape
    /// 上常に通過する。membership は emitter 側責務 (profile と同型)。
    RequestModel,
}

/// 属性キーに応じた値 domain を返す。
///
/// whitelisted キーはすべて何らかの domain を持つ
/// (`gen_ai.operation.name` / `gen_ai.token.type` / `gen_ai.provider.name` /
/// `error.type` / `evorch.delegation.role` は閉集合、
/// `evorch.profile.name` / `evorch.delegation.depth` /
/// `gen_ai.request.model` は shape ポリシー)。whitelist 外キーは `None`
/// (whitelist 検査が先に拒否する)。
fn value_domain(key: &str) -> Option<AttributeDomain> {
    match key {
        ATTR_OPERATION_NAME => Some(AttributeDomain::Closed(&OPERATION_NAME_DOMAIN)),
        ATTR_TOKEN_TYPE => Some(AttributeDomain::Closed(&TOKEN_TYPE_DOMAIN)),
        ATTR_PROVIDER_NAME => Some(AttributeDomain::Closed(&PROVIDER_NAME_DOMAIN)),
        ATTR_ERROR_TYPE => Some(AttributeDomain::Closed(&ERROR_TYPE_DOMAIN)),
        ATTR_DELEGATION_ROLE => Some(AttributeDomain::Closed(&DELEGATION_ROLE_DOMAIN)),
        ATTR_PROFILE_NAME => Some(AttributeDomain::ProfileName),
        ATTR_DELEGATION_DEPTH => Some(AttributeDomain::DelegationDepth),
        ATTR_REQUEST_MODEL => Some(AttributeDomain::RequestModel),
        _ => None,
    }
}

/// `evorch.profile.name` 値の shape ポリシー。
///
/// 非空・長さ 64 文字以下・全文字が小文字 ASCII alnum または `-_.`・
/// 先頭は alnum。このポリシーで保証できるのは「任意文字列性の排除
/// (自由文字列・大文字混じり・非 ASCII・空値の拒否) と 1 値あたりの
/// 最大長」であり、値の種類数の有界性は含まない。数的有界性は
/// otel-exporter feature の emitter 初期化時 profile registry
/// (上限 [`super::MAX_PROFILE_NAMES`]) が担う (責任分界)。
fn is_profile_name_valid(profile: &str) -> bool {
    !profile.is_empty()
        && profile.len() <= 64
        && profile
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        && profile.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// `evorch.delegation.depth` 値の shape ポリシー。
///
/// `0`..=`99` の decimal 文字列で leading zero を禁じる (`"0"` のみ例外)。
fn is_delegation_depth_valid(depth: &str) -> bool {
    let bytes = depth.as_bytes();
    depth == "0"
        || (!bytes.is_empty()
            && bytes.len() <= 2
            && bytes[0].is_ascii_digit()
            && bytes[0] != b'0'
            && bytes.iter().all(u8::is_ascii_digit))
}

/// `gen_ai.request.model` 値の shape ポリシー。
///
/// 非空・128 文字以下・全文字が printable ASCII (`0x21..=0x7E`、空白なし)。
/// profile より意図的に緩い: model id は digest や区切り文字 (`/`、`:` など)
/// を含み得るため。予約値 `"other"` もこの shape で常に通過する (emitter が
/// 非 member model を正規化した結果の値として、domain 上明示的に許可)。
/// このポリシーで保証できるのは任意文字列性の排除と 1 値あたりの最大長
/// であり、値の種類数の有界性は emitter 初期化時の `known_models`
/// registry が担う (profile と同型の責務境界)。
fn is_model_name_valid(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.chars().all(|c| c.is_ascii_graphic())
}

/// model 値を shape ポリシーで畳み込む。
///
/// 適合値はそのまま、不適合値は固定値 `other` へ畳み込む。`gen_ai.request.model`
/// は metrics の必須次元であるため属性省略は許容されず、全 4 写像でこの
/// 関数を経由して常に属性が付与される (exporter 側の membership 正規化と
/// 合流点が `other` で統一される)。
fn normalize_model_shape(model: &str) -> &str {
    if is_model_name_valid(model) {
        model
    } else {
        "other"
    }
}

/// ID 形状キー (`.id` 終端・`*_id` 系) の判定。
///
/// これらのキーは高カーディナリティ識別子であるため、値の内容にかかわらず
/// 常に違反とみなす。
fn is_identifier_like_key(key: &str) -> bool {
    key.ends_with(".id")
        || matches!(
            key,
            "request_id" | "session_id" | "task_id" | "run_id" | "call_id" | "message_id"
        )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, FaultEvent,
        LifecycleEvent, MessageEvent,
    };

    fn usage_event(
        provider: &str,
        input: u64,
        output: u64,
        cache_read: u64,
        cache_write: u64,
    ) -> Event {
        Event::new(UsageEvent::Usage {
            provider: provider.to_owned(),
            model: "kimi-k3".to_owned(),
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
        })
    }

    fn completed_event(provider: &str, profile: Option<&str>, duration_ms: u64) -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "req-1".to_owned(),
            provider: provider.to_owned(),
            profile: profile.map(str::to_owned),
            protocol: "openai-chat-completions".to_owned(),
            model: "kimi-k3".to_owned(),
            streaming: false,
            duration_ms,
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".to_owned(),
            run_id: None,
        })
    }

    fn failed_event(
        provider: &str,
        profile: Option<&str>,
        duration_ms: u64,
        failure: ProviderFailureKind,
    ) -> Event {
        Event::new(ProviderEvent::RequestFailed {
            request_id: "req-1".to_owned(),
            provider: provider.to_owned(),
            profile: profile.map(str::to_owned),
            protocol: "anthropic-messages".to_owned(),
            model: "kimi-k3".to_owned(),
            streaming: true,
            duration_ms,
            failure,
            run_id: None,
        })
    }

    fn ttft_event(provider: &str, profile: Option<&str>, ttft_ms: u64) -> Event {
        Event::new(ProviderEvent::FirstTokenObserved {
            request_id: "req-1".to_owned(),
            provider: provider.to_owned(),
            profile: profile.map(str::to_owned),
            protocol: "openai-chat-completions".to_owned(),
            model: "kimi-k3".to_owned(),
            ttft_ms,
            run_id: None,
        })
    }

    fn measurement_with_key(key: &str) -> MetricMeasurement {
        MetricMeasurement {
            name: TOKEN_USAGE_METRIC.to_owned(),
            unit: TOKEN_UNIT.to_owned(),
            value: MetricValue::U64(1),
            attrs: vec![MetricAttribute {
                key: key.to_owned(),
                value: "v".to_owned(),
            }],
        }
    }

    fn token_summary(m: &MetricMeasurement) -> (&str, u64) {
        assert_eq!(m.name, TOKEN_USAGE_METRIC);
        assert_eq!(m.unit, TOKEN_UNIT);
        assert_eq!(m.attrs.len(), 4);
        assert_eq!(m.attrs[0].key, "gen_ai.operation.name");
        assert_eq!(m.attrs[0].value, "chat");
        assert_eq!(m.attrs[2].key, "gen_ai.request.model");
        assert_eq!(m.attrs[2].value, "kimi-k3");
        match m.value {
            MetricValue::U64(value) => (m.attrs[3].value.as_str(), value),
            MetricValue::F64(_) => panic!("u64 histogram expected"),
        }
    }

    // Given: ProviderFailureKind の全 9 variant。
    // When: map_failure で error.type 値へ正規化する。
    // Then: variant ごとに固定の snake_case 文字列へ変換される (Http は status を捨てる)。
    #[test]
    fn map_failure_covers_every_provider_failure_variant() {
        let cases = [
            (ProviderFailureKind::RateLimited, "rate_limited"),
            (ProviderFailureKind::Http { status: 503 }, "http"),
            (ProviderFailureKind::Timeout, "timeout"),
            (ProviderFailureKind::InvalidResponse, "invalid_response"),
            (ProviderFailureKind::Transport, "transport"),
            (ProviderFailureKind::Server, "server"),
            (ProviderFailureKind::Quota, "quota"),
            (ProviderFailureKind::Auth, "auth"),
            (ProviderFailureKind::Other, "other"),
        ];
        for (failure, expected) in cases {
            assert_eq!(map_failure(&failure), expected, "failure={failure:?}");
        }
    }

    // Given: 既知 3 provider と未知の provider 識別子。
    // When: normalize_provider で正規化する。
    // Then: 既知値はそのまま、未知値 (my-proxy / 空) はすべて `other` へ bucket される。
    #[test]
    fn normalize_provider_passes_known_names_and_buckles_unknown() {
        assert_eq!(normalize_provider("anthropic"), "anthropic");
        assert_eq!(normalize_provider("openai"), "openai");
        assert_eq!(normalize_provider("openai-compatible"), "openai-compatible");
        assert_eq!(normalize_provider("my-proxy"), "other");
        assert_eq!(normalize_provider(""), "other");
    }

    // Given: 全 token 数が正の Usage イベント。
    // When: map_event で写像する。
    // Then: input/output/cache_read/cache_write の 4 measurement がこの順で得られ、
    //       各 measurement は u64 値と 4 属性 (operation/provider/request.model/token.type) を持つ。
    #[test]
    fn usage_maps_four_token_types_when_all_counts_are_positive() {
        let measurements = map_event(&usage_event("anthropic", 10, 20, 30, 40));

        let summaries: Vec<(&str, u64)> = measurements.iter().map(token_summary).collect();
        assert_eq!(
            summaries,
            [
                ("input", 10),
                ("output", 20),
                ("cache_read", 30),
                ("cache_write", 40)
            ]
        );
        for measurement in &measurements {
            assert_eq!(measurement.attrs[1].key, "gen_ai.provider.name");
            assert_eq!(measurement.attrs[1].value, "anthropic");
        }
    }

    // Given: input/output/cache がすべて 0 の Usage イベント。
    // When: map_event で写像する。
    // Then: 0 でも input/output は record され、cache 系 measurement は生成されない。
    #[test]
    fn usage_records_zero_input_and_output_and_skips_zero_cache() {
        let measurements = map_event(&usage_event("openai", 0, 0, 0, 0));

        let summaries: Vec<(&str, u64)> = measurements.iter().map(token_summary).collect();
        assert_eq!(summaries, [("input", 0), ("output", 0)]);
    }

    // Given: cache_read のみ正で cache_write が 0 の Usage イベント。
    // When: map_event で写像する。
    // Then: cache_write だけが欠けた 3 measurement になる (cache 系は値が 0 より大きいときのみ)。
    #[test]
    fn usage_skips_only_zero_cache_fields() {
        let measurements = map_event(&usage_event("openai", 1, 2, 5, 0));

        let summaries: Vec<(&str, u64)> = measurements.iter().map(token_summary).collect();
        assert_eq!(summaries, [("input", 1), ("output", 2), ("cache_read", 5)]);
    }

    // Given: profile Some と None の RequestCompleted。
    // When: map_event で写像する。
    // Then: value は duration_ms を秒へ換算した f64。profile Some は
    //       evorch.profile.name を含み、None は含まない。
    #[test]
    fn request_completed_maps_duration_seconds_with_optional_profile() {
        let with_profile = map_event(&completed_event("openai", Some("primary"), 500));

        assert_eq!(with_profile.len(), 1);
        let measurement = &with_profile[0];
        assert_eq!(measurement.name, OPERATION_DURATION_METRIC);
        assert_eq!(measurement.unit, SECONDS_UNIT);
        assert_eq!(measurement.value, MetricValue::F64(0.5));
        assert_eq!(
            measurement.attrs,
            vec![
                MetricAttribute {
                    key: "gen_ai.operation.name".to_owned(),
                    value: "chat".to_owned()
                },
                MetricAttribute {
                    key: "gen_ai.provider.name".to_owned(),
                    value: "openai".to_owned()
                },
                MetricAttribute {
                    key: "gen_ai.request.model".to_owned(),
                    value: "kimi-k3".to_owned()
                },
                MetricAttribute {
                    key: "evorch.profile.name".to_owned(),
                    value: "primary".to_owned()
                },
            ]
        );

        let without_profile = map_event(&completed_event("openai", None, 1250));

        assert_eq!(without_profile.len(), 1);
        let measurement = &without_profile[0];
        assert_eq!(measurement.value, MetricValue::F64(1.25));
        assert_eq!(measurement.attrs.len(), 3);
    }

    // Given: Http{503} 失敗 (profile Some) と RateLimited 失敗 (profile None)。
    // When: map_event で写像する。
    // Then: 同一 duration instrument へ error.type=map_failure(failure) が付き、
    //       Http の status は捨てられる。
    #[test]
    fn request_failed_maps_error_type_from_failure_kind() {
        let http = map_event(&failed_event(
            "openai-compatible",
            Some("secondary"),
            3000,
            ProviderFailureKind::Http { status: 503 },
        ));

        assert_eq!(http.len(), 1);
        let measurement = &http[0];
        assert_eq!(measurement.name, OPERATION_DURATION_METRIC);
        assert_eq!(measurement.unit, SECONDS_UNIT);
        assert_eq!(measurement.value, MetricValue::F64(3.0));
        assert_eq!(
            measurement.attrs,
            vec![
                MetricAttribute {
                    key: "gen_ai.operation.name".to_owned(),
                    value: "chat".to_owned()
                },
                MetricAttribute {
                    key: "gen_ai.provider.name".to_owned(),
                    value: "openai-compatible".to_owned()
                },
                MetricAttribute {
                    key: "gen_ai.request.model".to_owned(),
                    value: "kimi-k3".to_owned()
                },
                MetricAttribute {
                    key: "error.type".to_owned(),
                    value: "http".to_owned()
                },
                MetricAttribute {
                    key: "evorch.profile.name".to_owned(),
                    value: "secondary".to_owned()
                },
            ]
        );

        let rate_limited = map_event(&failed_event(
            "anthropic",
            None,
            10,
            ProviderFailureKind::RateLimited,
        ));

        assert_eq!(rate_limited.len(), 1);
        let measurement = &rate_limited[0];
        assert_eq!(measurement.attrs.len(), 4);
        assert_eq!(measurement.attrs[3].key, "error.type");
        assert_eq!(measurement.attrs[3].value, "rate_limited");
    }

    // Given: TTFT 1500ms の FirstTokenObserved。
    // When: map_event で写像する。
    // Then: evorch.client.time_to_first_token に 1.5 秒が f64 で記録され、
    //       request.model は provider の直後に配置される。
    #[test]
    fn first_token_observed_maps_ttft_seconds() {
        let measurements = map_event(&ttft_event("anthropic", Some("primary"), 1500));

        assert_eq!(measurements.len(), 1);
        let measurement = &measurements[0];
        assert_eq!(measurement.name, TIME_TO_FIRST_TOKEN_METRIC);
        assert_eq!(measurement.unit, SECONDS_UNIT);
        assert_eq!(measurement.value, MetricValue::F64(1.5));
        assert_eq!(measurement.attrs.len(), 4);
        assert_eq!(measurement.attrs[2].key, "gen_ai.request.model");
        assert_eq!(measurement.attrs[2].value, "kimi-k3");
        assert_eq!(measurement.attrs[3].key, "evorch.profile.name");
        assert_eq!(measurement.attrs[3].value, "primary");
    }

    // Given: 写像対象外の代表 variant (RequestStarted / CacheStats /
    //        FallbackTriggered / ProviderFallback / Lifecycle / Message /
    //        Fault / AgentMessage)。
    // When: map_event で写像する。
    // Then: いずれも空 Vec になる。
    #[test]
    fn non_mapped_variants_map_to_no_measurements() {
        let cases = vec![
            Event::new(ProviderEvent::RequestStarted {
                request_id: "req-1".to_owned(),
                provider: "anthropic".to_owned(),
                profile: None,
                protocol: "anthropic-messages".to_owned(),
                model: "kimi-k3".to_owned(),
                streaming: false,
                run_id: None,
            }),
            Event::new(UsageEvent::CacheStats {
                provider: "anthropic".to_owned(),
                model: "kimi-k3".to_owned(),
                cache_hits: 1,
                cache_misses: 2,
            }),
            Event::new(ProviderEvent::FallbackTriggered {
                from_provider: "primary".to_owned(),
                from_model: None,
                to_provider: "secondary".to_owned(),
                to_model: "model-b".to_owned(),
                logical_model: "summary".to_owned(),
                session_id: "session-1".to_owned(),
                failure: ProviderFailureKind::Timeout,
                request_id: None,
            }),
            Event::new(ProviderEvent::ProviderFallback {
                from_provider: "primary".to_owned(),
                to_provider: "secondary".to_owned(),
                reason: "timeout".to_owned(),
            }),
            Event::new(LifecycleEvent::Started {
                session_id: "session-1".to_owned(),
            }),
            Event::new(MessageEvent::MessageDelta {
                delta: "he".to_owned(),
            }),
            Event::new(FaultEvent::SubscriberLagged {
                subscriber_id: 1,
                skipped: 2,
            }),
            Event::new(AgentMessageEvent::Delivered {
                message: AgentMessage {
                    message_id: "msg-1".to_owned(),
                    sender_run_id: "run-1".to_owned(),
                    recipient_run_id: "run-2".to_owned(),
                    kind: AgentMessageKind::Send,
                    content: "ping".to_owned(),
                    reply_to: None,
                },
                disposition: DeliveryDisposition::Wake,
            }),
        ];

        for event in cases {
            assert!(
                map_event(&event).is_empty(),
                "kind={:?} must not be mapped",
                event.kind
            );
        }
    }

    // Given: whitelist 外の属性キー (gen_ai.response.model) を含む measurement。
    // When: validate_metric_attributes で検査する。
    // Then: UnknownAttributeKey で拒否される。
    #[test]
    fn validate_rejects_keys_outside_the_whitelist() {
        let measurement = measurement_with_key("gen_ai.response.model");

        assert_eq!(
            validate_metric_attributes(&measurement),
            Err(CardinalityViolation::UnknownAttributeKey {
                key: "gen_ai.response.model".to_owned()
            })
        );
    }

    // Given: ID 形状キー (request_id / foo.id など 6 種の *_id と .id 終端) を
    //        含む measurement。
    // When: validate_metric_attributes で検査する。
    // Then: whitelist 内外を問わず IdentifierLikeAttribute で拒否される。
    #[test]
    fn validate_rejects_identifier_like_keys() {
        for key in [
            "request_id",
            "session_id",
            "task_id",
            "run_id",
            "call_id",
            "message_id",
            "foo.id",
            "trace.id",
        ] {
            let measurement = measurement_with_key(key);

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::IdentifierLikeAttribute { .. })
                ),
                "key={key}"
            );
        }
    }

    // Given: 閉集合 domain 外の値 (token.type="reasoning" 等 4 種)。
    // When: validate_metric_attributes で検査する。
    // Then: InvalidAttributeValue で拒否される。
    #[test]
    fn validate_rejects_values_outside_closed_domains() {
        let cases = [
            ("gen_ai.token.type", "reasoning"),
            ("gen_ai.operation.name", "embeddings"),
            ("gen_ai.provider.name", "custom-proxy"),
            ("error.type", "rate-limit"),
        ];
        for (key, value) in cases {
            let mut measurement = measurement_with_key(key);
            measurement.attrs[0].value = value.to_owned();

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::InvalidAttributeValue { .. })
                ),
                "key={key} value={value}"
            );
        }
    }

    // Given: profile 値の shape ポリシーに適合する値 ("primary" / "gpu-pool.1")
    //        と不適合な値 ("GPU Pool" / 65 文字 / "プール") を持つ
    //        RequestCompleted。
    // When: map_event で写像する。
    // Then: 適合値のときのみ evorch.profile.name が付与され、不適合値では
    //       measurement 自体は保持しつつ profile 属性のみ省略される。
    #[test]
    fn profile_attribute_follows_shape_policy() {
        for valid in ["primary", "gpu-pool.1"] {
            let measurements = map_event(&completed_event("openai", Some(valid), 500));
            assert_eq!(measurements.len(), 1, "profile={valid:?}");
            assert!(
                measurements[0]
                    .attrs
                    .iter()
                    .any(|attr| attr.key == "evorch.profile.name" && attr.value == valid),
                "valid profile={valid:?} must be kept"
            );
        }

        let long_profile = "x".repeat(65);
        for (label, profile) in [
            ("大文字と空白", "GPU Pool"),
            ("65 文字", long_profile.as_str()),
            ("非 ASCII", "プール"),
        ] {
            let measurements = map_event(&completed_event("openai", Some(profile), 500));
            assert_eq!(measurements.len(), 1, "{label}: measurement must be kept");
            assert!(
                !measurements[0]
                    .attrs
                    .iter()
                    .any(|attr| attr.key == "evorch.profile.name"),
                "{label}: profile attribute must be omitted"
            );
            assert!(
                measurements[0]
                    .attrs
                    .iter()
                    .any(|attr| attr.key == "gen_ai.provider.name"),
                "{label}: other attributes must be kept"
            );
        }
    }

    // Given: shape ポリシー不適合の profile 値 (空 / "GPU Pool" / 65 文字 /
    //        "プール" / 65 文字境界超過の "a" 繰り返し) と適合値 ("primary" /
    //        "gpu-pool.1" / ちょうど 64 文字) を注入した measurement。
    // When: validate_metric_attributes で検査する。
    // Then: 不適合値は InvalidAttributeValue で拒否され、適合値は通過する。
    #[test]
    fn validate_rejects_invalid_profile_names() {
        let long_profile = "x".repeat(65);
        for profile in [
            "",
            "GPU Pool",
            long_profile.as_str(),
            "プール",
            "-leading-hyphen",
        ] {
            let mut measurement = measurement_with_key("evorch.profile.name");
            measurement.attrs[0].value = profile.to_owned();

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::InvalidAttributeValue { .. })
                ),
                "profile={profile:?}"
            );
        }

        let exact_64 = "a".repeat(64);
        for profile in ["primary", "gpu-pool.1", exact_64.as_str()] {
            let mut measurement = measurement_with_key("evorch.profile.name");
            measurement.attrs[0].value = profile.to_owned();

            assert_eq!(
                validate_metric_attributes(&measurement),
                Ok(()),
                "profile={profile:?}"
            );
        }
    }

    // Given: depth の domain 外値 ("100" / "01" / "abc" / 空) と domain 内値
    //        ("0" / "9" / "99") を注入した measurement。
    // When: validate_metric_attributes で検査する。
    // Then: domain 外は InvalidAttributeValue で拒否され、domain 内は通過する。
    #[test]
    fn validate_rejects_invalid_delegation_depth() {
        for depth in ["100", "01", "abc", ""] {
            let mut measurement = measurement_with_key("evorch.delegation.depth");
            measurement.attrs[0].value = depth.to_owned();

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::InvalidAttributeValue { .. })
                ),
                "depth={depth:?}"
            );
        }

        for depth in ["0", "9", "99"] {
            let mut measurement = measurement_with_key("evorch.delegation.depth");
            measurement.attrs[0].value = depth.to_owned();

            assert_eq!(
                validate_metric_attributes(&measurement),
                Ok(()),
                "depth={depth:?}"
            );
        }
    }

    // Given: role の domain 外値 ("super-admin" / 空) と domain 内 4 値
    //        (repo 既定の agent role 語彙) を注入した measurement。
    // When: validate_metric_attributes で検査する。
    // Then: domain 外は InvalidAttributeValue で拒否され、domain 内は通過する。
    #[test]
    fn validate_rejects_invalid_delegation_role() {
        for role in ["super-admin", ""] {
            let mut measurement = measurement_with_key("evorch.delegation.role");
            measurement.attrs[0].value = role.to_owned();

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::InvalidAttributeValue { .. })
                ),
                "role={role:?}"
            );
        }

        for role in ["orchestrator", "explorer", "worker", "reviewer"] {
            let mut measurement = measurement_with_key("evorch.delegation.role");
            measurement.attrs[0].value = role.to_owned();

            assert_eq!(
                validate_metric_attributes(&measurement),
                Ok(()),
                "role={role:?}"
            );
        }
    }

    fn completed_event_with_model(model: &str) -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "req-1".to_owned(),
            provider: "openai".to_owned(),
            profile: Some("primary".to_owned()),
            protocol: "openai-chat-completions".to_owned(),
            model: model.to_owned(),
            streaming: false,
            duration_ms: 500,
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".to_owned(),
            run_id: None,
        })
    }

    // Given: shape-valid な model ("kimi-k3") と shape-invalid な model
    //        ("has space") を持つ RequestCompleted。
    // When: map_event で写像する。
    // Then: gen_ai.request.model は必須次元として常に付与される —
    //       shape-valid な model は元値、shape-invalid な model は固定値
    //       "other" に畳み込まれる (measurement 数はいずれも 1 のまま)。
    #[test]
    fn model_attribute_requirements() {
        let valid = map_event(&completed_event_with_model("kimi-k3"));
        assert_eq!(valid.len(), 1);
        assert!(
            valid[0]
                .attrs
                .iter()
                .any(|attr| attr.key == "gen_ai.request.model" && attr.value == "kimi-k3"),
            "shape-valid model must be emitted as is"
        );

        let invalid = map_event(&completed_event_with_model("has space"));
        assert_eq!(invalid.len(), 1, "measurement must be kept");
        assert!(
            invalid[0]
                .attrs
                .iter()
                .any(|attr| attr.key == "gen_ai.request.model" && attr.value == "other"),
            "shape-invalid model must be folded to `other` (required dimension)"
        );
        assert!(
            invalid[0]
                .attrs
                .iter()
                .any(|attr| attr.key == "gen_ai.provider.name"),
            "other attributes must be kept"
        );
    }

    // Given: domain 内の model 値 ("other" / digest と区切り文字を含む値 /
    //        ちょうど 128 文字) を注入した measurement。
    // When: validate_metric_attributes で検査する。
    // Then: いずれも通過する (validator は shape gate のみで membership は
    //       emitter 側責務)。
    #[test]
    fn validate_accepts_other_and_valid_models() {
        let exact_128 = "m".repeat(128);
        for model in ["other", "gpt-4o:2024-08-06", exact_128.as_str()] {
            let mut measurement = measurement_with_key("gen_ai.request.model");
            measurement.attrs[0].value = model.to_owned();

            assert_eq!(
                validate_metric_attributes(&measurement),
                Ok(()),
                "model={model:?}"
            );
        }
    }

    // Given: shape ポリシー不適合の model 値 ("with space" / 129 文字 /
    //        非 ASCII / 空) を注入した measurement。
    // When: validate_metric_attributes で検査する。
    // Then: InvalidAttributeValue で拒否される。map 層出力は既に "other" へ
    //       畳み込み済みのため通常この経路は通らないが、DTO 直構築への
    //       防御として shape gate を維持する。
    #[test]
    fn validate_rejects_shape_invalid_models() {
        let too_long = "m".repeat(129);
        for model in ["with space", too_long.as_str(), "モデル", ""] {
            let mut measurement = measurement_with_key("gen_ai.request.model");
            measurement.attrs[0].value = model.to_owned();

            assert!(
                matches!(
                    validate_metric_attributes(&measurement),
                    Err(CardinalityViolation::InvalidAttributeValue { .. })
                ),
                "model={model:?}"
            );
        }
    }

    // Given: 全ての写像対象 event (未知 provider・自由文字列 profile を含む)。
    // When: map_event → validate_metric_attributes。
    // Then: 生成された全 measurement が検査を通過する。
    #[test]
    fn mapped_measurements_pass_validation() {
        let events = vec![
            usage_event("anthropic", 10, 20, 30, 40),
            usage_event("my-proxy", 1, 2, 0, 0),
            completed_event("openai-compatible", Some("gpu-pool"), 700),
            completed_event("openai", None, 1250),
            failed_event("anthropic", None, 5, ProviderFailureKind::Auth),
            ttft_event("openai", Some("primary"), 120),
        ];

        for event in events {
            for measurement in map_event(&event) {
                assert_eq!(
                    validate_metric_attributes(&measurement),
                    Ok(()),
                    "measurement={measurement:?}"
                );
            }
        }
    }
}
