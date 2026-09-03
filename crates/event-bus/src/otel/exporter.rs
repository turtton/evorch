// allow: SIZE_OK - in-module smoke テストの配置がタスク要件であり、emitter
// と provider 構築は同一 feature 生命周期の 1 単位。生産コード単体では
// 約150純LOC (event.rs の先例に準拠)。
//! otel-exporter feature で有効化される metrics exporter 層。
//!
//! 写像層 ([`super::map_event`]) の出力を OTel histogram instruments へ
//! 記録する [`OtelMetricsEmitter`] と、OTLP HTTP / InMemory の meter
//! provider 構築関数を提供する。histogram の bucket boundaries は semconv
//! v1.37.0 (`super::SEMCONV_PIN`) の advisory ExplicitBucketBoundaries に
//! 従う。
//!
//! # profile cardinality 責任分界
//!
//! `evorch.profile.name` の値は次の 2 層で有界化される:
//! - 写像層 / validator: shape ポリシー (小文字 ASCII alnum と `-_.`、
//!   最大長 64) を強制し、任意文字列性を排除する。ただし shape 制約だけでは
//!   値の種類数は有界化できない。
//! - emitter 初期化時の profile registry: `config` 由来の profile 集合を
//!   [`MAX_PROFILE_NAMES`] (64) 件まで受け取り、emit 時に registry 非
//!   member の `evorch.profile.name` 属性のみ除外する (measurement 自体は
//!   保持)。数的有界性はこの registry が単独で担う。ADR 0014 配線時に
//!   runtime から config profile 集合が注入される想定。

use std::collections::HashSet;
use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, MeterProvider};
use opentelemetry_otlp::{ExporterBuildError, MetricExporter, Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::event::Event;

use super::{
    ATTR_PROFILE_NAME, MetricMeasurement, MetricValue, OPERATION_DURATION_METRIC, SECONDS_UNIT,
    TIME_TO_FIRST_TOKEN_METRIC, TOKEN_UNIT, TOKEN_USAGE_METRIC, map_event,
    validate_metric_attributes,
};

/// `gen_ai.client.token.usage` の advisory bucket boundaries。
///
/// semconv v1.37.0 が ExplicitBucketBoundaries として推奨する値
/// (<https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/>)。
const TOKEN_USAGE_BOUNDARIES: [f64; 14] = [
    1.0, 4.0, 16.0, 64.0, 256.0, 1024.0, 4096.0, 16384.0, 65536.0, 262144.0, 1048576.0, 4194304.0,
    16777216.0, 67108864.0,
];

/// 時間系 histogram の advisory bucket boundaries。
///
/// `gen_ai.client.operation.duration` は semconv v1.37.0 の推奨値。
/// `evorch.client.time_to_first_token` は semconv 未定義の evorch 拡張の
/// ため、同じ秒単位の duration 系推奨値を流用する。
const DURATION_BOUNDARIES: [f64; 14] = [
    0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];

/// profile registry に許容される最大個数。
///
/// emitter 初期化時のみ消費され、実行中に変化しない (初期化後不変)。
pub const MAX_PROFILE_NAMES: usize = 64;

/// emitter 初期化時の profile registry 構築エラー。
///
/// [`OtelMetricsEmitter::new`] に [`MAX_PROFILE_NAMES`] を超える数の
/// profile が渡された場合に返る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProfileRegistryError {
    /// registry に要求された profile の個数 (重複除去後)。
    pub requested: usize,
    /// 許容される最大個数 ([`MAX_PROFILE_NAMES`])。
    pub max: usize,
}

impl std::fmt::Display for ProfileRegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "profile registry limit exceeded: {} profiles requested (max {})",
            self.requested, self.max
        )
    }
}

impl std::error::Error for ProfileRegistryError {}

/// イベントを OTel histogram へ記録する emitter。
///
/// [`OtelMetricsEmitter::emit`] は panic しない。cardinality guard 違反や
/// instrument と値型の不一致は `tracing::warn!` で記録し、該当 measurement
/// のみ skip する。`known_profiles` (profile registry) は初期化時にのみ
/// 構築され、以後不変である。
pub struct OtelMetricsEmitter {
    token_usage: Histogram<u64>,
    operation_duration: Histogram<f64>,
    time_to_first_token: Histogram<f64>,
    known_profiles: HashSet<String>,
}

impl OtelMetricsEmitter {
    /// [`SdkMeterProvider`] から meter を取得し、3 histogram と profile
    /// registry を構築する。
    ///
    /// `known_profiles` は emit 時に `evorch.profile.name` 属性を維持してよい
    /// profile の集合 (ADR 0014 配線時は config の profile 集合を注入する想定)。
    ///
    /// # Errors
    /// 重複除去後の `known_profiles` が [`MAX_PROFILE_NAMES`] を超える場合、
    /// [`ProfileRegistryError`] を返す。
    pub fn new(
        provider: &SdkMeterProvider,
        known_profiles: impl IntoIterator<Item = String>,
    ) -> Result<Self, ProfileRegistryError> {
        let known_profiles: HashSet<String> = known_profiles.into_iter().collect();
        if known_profiles.len() > MAX_PROFILE_NAMES {
            return Err(ProfileRegistryError {
                requested: known_profiles.len(),
                max: MAX_PROFILE_NAMES,
            });
        }
        let meter = provider.meter("evorch.event-bus");
        Ok(Self {
            token_usage: meter
                .u64_histogram(TOKEN_USAGE_METRIC)
                .with_unit(TOKEN_UNIT)
                .with_boundaries(TOKEN_USAGE_BOUNDARIES.to_vec())
                .build(),
            operation_duration: meter
                .f64_histogram(OPERATION_DURATION_METRIC)
                .with_unit(SECONDS_UNIT)
                .with_boundaries(DURATION_BOUNDARIES.to_vec())
                .build(),
            time_to_first_token: meter
                .f64_histogram(TIME_TO_FIRST_TOKEN_METRIC)
                .with_unit(SECONDS_UNIT)
                .with_boundaries(DURATION_BOUNDARIES.to_vec())
                .build(),
            known_profiles,
        })
    }

    /// イベントを写像し、有効な measurement を histogram へ記録する。
    ///
    /// `evorch.profile.name` 属性は map 層の shape ポリシー適合に加え、
    /// registry member であるときのみ維持される。非 member は profile 属性
    /// のみ除外して measurement を記録する (metric 全欠落を避ける)。
    pub fn emit(&self, event: &Event) {
        for measurement in map_event(event) {
            self.record(measurement);
        }
    }

    fn record(&self, mut measurement: MetricMeasurement) {
        if let Some(position) = measurement.attrs.iter().position(|attr| {
            attr.key == ATTR_PROFILE_NAME && !self.known_profiles.contains(&attr.value)
        }) {
            measurement.attrs.remove(position);
        }
        if let Err(violation) = validate_metric_attributes(&measurement) {
            tracing::warn!(
                metric = %measurement.name,
                violation = %violation,
                "cardinality guard rejected the measurement; skipping"
            );
            return;
        }
        let attributes: Vec<KeyValue> = measurement
            .attrs
            .iter()
            .map(|attr| KeyValue::new(attr.key.clone(), attr.value.clone()))
            .collect();
        match measurement.name.as_str() {
            TOKEN_USAGE_METRIC => match measurement.value {
                MetricValue::U64(value) => self.token_usage.record(value, &attributes),
                MetricValue::F64(_) => {
                    tracing::warn!(metric = TOKEN_USAGE_METRIC, "value type mismatch; skipping")
                }
            },
            OPERATION_DURATION_METRIC => match measurement.value {
                MetricValue::F64(value) => self.operation_duration.record(value, &attributes),
                MetricValue::U64(_) => tracing::warn!(
                    metric = OPERATION_DURATION_METRIC,
                    "value type mismatch; skipping"
                ),
            },
            TIME_TO_FIRST_TOKEN_METRIC => match measurement.value {
                MetricValue::F64(value) => self.time_to_first_token.record(value, &attributes),
                MetricValue::U64(_) => tracing::warn!(
                    metric = TIME_TO_FIRST_TOKEN_METRIC,
                    "value type mismatch; skipping"
                ),
            },
            unknown => tracing::warn!(metric = unknown, "unknown metric name; skipping"),
        }
    }
}

/// OTLP metrics signal path。
///
/// opentelemetry-otlp 0.32 の `with_endpoint` は URL を as-is で扱い
/// signal path の自動付加は行わない (env var 経由の場合のみ付加) ため、
/// base URL 契約を本 crate 側で正規化する。
const METRICS_SIGNAL_PATH: &str = "/v1/metrics";

/// endpoint が metrics signal path を持たない場合に付加する。
fn with_metrics_signal_path(endpoint: &str) -> String {
    if endpoint.ends_with(METRICS_SIGNAL_PATH) {
        endpoint.to_owned()
    } else {
        // 末尾の連続 `/` を除去してから path を繋ぐ (二重スラッシュ防止)。
        format!("{}{METRICS_SIGNAL_PATH}", endpoint.trim_end_matches('/'))
    }
}

/// OTLP HTTP (protobuf) exporter + [`PeriodicReader`] で meter provider を
/// 構築する。
///
/// `endpoint` は OTLP HTTP の base URL (例: `http://127.0.0.1:4318`) で、
/// metrics signal path `/v1/metrics` が無い場合は本関数が付加する
/// ([`METRICS_SIGNAL_PATH`])。`interval` は PeriodicReader の収集間隔。
///
/// # Errors
/// exporter の構築に失敗した場合 [`ExporterBuildError`] を返す。0.32 では
/// `OTelSdkResult` が非ジェネリクス (`Result<(), OTelSdkError>`) のため、
/// 唯一の失敗点である exporter 構築の error 型をそのまま返す。
pub fn build_otlp_meter_provider(
    endpoint: &str,
    interval: Duration,
) -> Result<SdkMeterProvider, ExporterBuildError> {
    let reader = build_otlp_metric_reader(endpoint, interval)?;
    Ok(SdkMeterProvider::builder().with_reader(reader).build())
}

/// OTLP HTTP (protobuf) exporter の [`PeriodicReader`] を構築する。
///
/// `endpoint` への signal path 付加は [`build_otlp_meter_provider`] と共通
/// ([`METRICS_SIGNAL_PATH`])。複数 reader を 1 provider へ接続する構成
/// (E2E テスト等) から再利用する。
///
/// # Errors
/// exporter の構築に失敗した場合 [`ExporterBuildError`] を返す。
pub fn build_otlp_metric_reader(
    endpoint: &str,
    interval: Duration,
) -> Result<PeriodicReader<MetricExporter>, ExporterBuildError> {
    let exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(with_metrics_signal_path(endpoint))
        .build()?;
    Ok(PeriodicReader::builder(exporter)
        .with_interval(interval)
        .build())
}

/// InMemory exporter 付きの meter provider を構築する。
///
/// debug / E2E 用途。PeriodicReader の既定間隔 (60 秒) で収集され、exporter
/// がプロセス内メモリに滞留するため production 向けではない (本番は
/// [`build_otlp_meter_provider`] を使う)。
pub fn build_in_memory_meter_provider() -> (SdkMeterProvider, InMemoryMetricExporter) {
    let exporter = InMemoryMetricExporter::default();
    let reader = PeriodicReader::builder(exporter.clone()).build();
    let provider = SdkMeterProvider::builder().with_reader(reader).build();
    (provider, exporter)
}

#[cfg(test)]
mod tests {
    use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};

    use super::*;
    use crate::event::{ProviderEvent, UsageEvent};

    fn usage_event() -> Event {
        Event::new(UsageEvent::Usage {
            provider: "anthropic".to_owned(),
            model: "kimi-k3".to_owned(),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        })
    }

    fn completed_event(profile: &str) -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "req-1".to_owned(),
            provider: "openai".to_owned(),
            profile: Some(profile.to_owned()),
            protocol: "openai-chat-completions".to_owned(),
            model: "kimi-k3".to_owned(),
            streaming: false,
            duration_ms: 500,
            input_tokens: 1,
            output_tokens: 2,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            finish_reason: "stop".to_owned(),
        })
    }

    fn ttft_event() -> Event {
        Event::new(ProviderEvent::FirstTokenObserved {
            request_id: "req-1".to_owned(),
            provider: "anthropic".to_owned(),
            profile: Some("primary".to_owned()),
            protocol: "anthropic-messages".to_owned(),
            model: "kimi-k3".to_owned(),
            ttft_ms: 1500,
        })
    }

    fn flattened_metrics(
        finished: &[ResourceMetrics],
    ) -> Vec<&opentelemetry_sdk::metrics::data::Metric> {
        finished
            .iter()
            .flat_map(|resource_metrics| resource_metrics.scope_metrics())
            .flat_map(|scope_metrics| scope_metrics.metrics())
            .collect()
    }

    // Given: in-memory provider と registry={"primary"} の emitter。
    // When: 3 種のイベント (profile="primary" を含む) を emit し force_flush する。
    // Then: 3 instrument が正しい unit・合計値・属性で記録され、registry
    //       member の profile 属性は duration / TTFT の両方に記録される。
    #[test]
    fn in_memory_smoke_records_three_instruments() {
        let (provider, exporter) = build_in_memory_meter_provider();
        let emitter = OtelMetricsEmitter::new(&provider, vec!["primary".to_owned()])
            .expect("registry within limit");

        emitter.emit(&usage_event());
        emitter.emit(&completed_event("primary"));
        emitter.emit(&ttft_event());
        provider.force_flush().expect("force_flush succeeds");

        let finished = exporter.get_finished_metrics().expect("finished metrics");
        let metrics = flattened_metrics(&finished);
        assert_eq!(metrics.len(), 3, "metrics={metrics:?}");

        let token_usage = metrics
            .iter()
            .find(|metric| metric.name() == TOKEN_USAGE_METRIC)
            .expect("token usage metric");
        assert_eq!(token_usage.unit(), TOKEN_UNIT);
        let AggregatedMetrics::U64(MetricData::Histogram(histogram)) = token_usage.data() else {
            panic!("u64 histogram expected: {:?}", token_usage.data());
        };
        let points: Vec<_> = histogram.data_points().collect();
        assert_eq!(points.len(), 2, "input/output data points");
        for point in &points {
            assert_eq!(point.count(), 1);
            let attributes: Vec<_> = point
                .attributes()
                .map(|kv| (kv.key.as_str().to_owned(), kv.value.to_string()))
                .collect();
            assert!(attributes.contains(&("gen_ai.operation.name".to_owned(), "chat".to_owned())));
            assert!(
                attributes.contains(&("gen_ai.provider.name".to_owned(), "anthropic".to_owned()))
            );
        }
        let input_point = points
            .iter()
            .find(|point| {
                point.attributes().any(|kv| {
                    kv.key.as_str() == "gen_ai.token.type" && kv.value.to_string() == "input"
                })
            })
            .expect("input data point");
        assert_eq!(input_point.sum(), 10);
        let output_point = points
            .iter()
            .find(|point| {
                point.attributes().any(|kv| {
                    kv.key.as_str() == "gen_ai.token.type" && kv.value.to_string() == "output"
                })
            })
            .expect("output data point");
        assert_eq!(output_point.sum(), 20);

        let duration = metrics
            .iter()
            .find(|metric| metric.name() == OPERATION_DURATION_METRIC)
            .expect("operation duration metric");
        assert_eq!(duration.unit(), SECONDS_UNIT);
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = duration.data() else {
            panic!("f64 histogram expected: {:?}", duration.data());
        };
        let points: Vec<_> = histogram.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].sum(), 0.5);
        assert!(points[0].attributes().any(|kv| {
            kv.key.as_str() == "evorch.profile.name" && kv.value.to_string() == "primary"
        }));

        let ttft = metrics
            .iter()
            .find(|metric| metric.name() == TIME_TO_FIRST_TOKEN_METRIC)
            .expect("ttft metric");
        assert_eq!(ttft.unit(), SECONDS_UNIT);
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = ttft.data() else {
            panic!("f64 histogram expected: {:?}", ttft.data());
        };
        let points: Vec<_> = histogram.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].sum(), 1.5);
        assert!(points[0].attributes().any(|kv| {
            kv.key.as_str() == "evorch.profile.name" && kv.value.to_string() == "primary"
        }));
    }

    // Given: advisory boundaries を設定した 3 instrument。
    // When: in-memory provider で emit し force_flush する。
    // Then: 記録された histogram の bounds が semconv v1.37.0 の推奨値と一致する。
    #[test]
    fn histograms_use_semconv_advisory_boundaries() {
        let (provider, exporter) = build_in_memory_meter_provider();
        let emitter = OtelMetricsEmitter::new(&provider, vec!["primary".to_owned()])
            .expect("registry within limit");

        emitter.emit(&usage_event());
        emitter.emit(&completed_event("primary"));
        emitter.emit(&ttft_event());
        provider.force_flush().expect("force_flush succeeds");

        let finished = exporter.get_finished_metrics().expect("finished metrics");
        let metrics = flattened_metrics(&finished);
        for (name, expected) in [
            (TOKEN_USAGE_METRIC, &TOKEN_USAGE_BOUNDARIES[..]),
            (OPERATION_DURATION_METRIC, &DURATION_BOUNDARIES[..]),
            (TIME_TO_FIRST_TOKEN_METRIC, &DURATION_BOUNDARIES[..]),
        ] {
            let metric = metrics
                .iter()
                .find(|metric| metric.name() == name)
                .unwrap_or_else(|| panic!("{name} metric"));
            let bounds: Vec<f64> = match metric.data() {
                AggregatedMetrics::U64(MetricData::Histogram(histogram)) => histogram
                    .data_points()
                    .next()
                    .expect("data point")
                    .bounds()
                    .collect(),
                AggregatedMetrics::F64(MetricData::Histogram(histogram)) => histogram
                    .data_points()
                    .next()
                    .expect("data point")
                    .bounds()
                    .collect(),
                other => panic!("histogram expected: {other:?}"),
            };
            assert_eq!(bounds, expected, "bounds of {name}");
        }
    }

    // Given: registry={"primary"} の emitter と、shape-valid だが registry 外
    //        の profile ("tenant-000001") を持つ RequestCompleted。
    // When: emit し force_flush する。
    // Then: measurement は 1 件記録され、evorch.profile.name 属性のみ除外され、
    //       他の属性 (operation / provider) は維持される。
    #[test]
    fn strips_profile_attribute_when_not_in_registry() {
        let (provider, exporter) = build_in_memory_meter_provider();
        let emitter = OtelMetricsEmitter::new(&provider, vec!["primary".to_owned()])
            .expect("registry within limit");

        emitter.emit(&completed_event("tenant-000001"));
        provider.force_flush().expect("force_flush succeeds");

        let finished = exporter.get_finished_metrics().expect("finished metrics");
        let metrics = flattened_metrics(&finished);
        assert_eq!(metrics.len(), 1, "measurement must still be recorded");
        let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = metrics[0].data() else {
            panic!("f64 histogram expected: {:?}", metrics[0].data());
        };
        let points: Vec<_> = histogram.data_points().collect();
        assert_eq!(points.len(), 1);
        assert_eq!(points[0].sum(), 0.5);
        assert!(
            points[0]
                .attributes()
                .all(|kv| kv.key.as_str() != "evorch.profile.name"),
            "non-member profile attribute must be stripped"
        );
        assert!(
            points[0]
                .attributes()
                .any(|kv| kv.key.as_str() == "gen_ai.provider.name")
        );
    }

    // Given: 上限 ([`MAX_PROFILE_NAMES`] = 64) を超える 65 個の profile 名。
    // When: OtelMetricsEmitter::new で初期化する。
    // Then: ProfileRegistryError を返す。
    #[test]
    fn rejects_registry_larger_than_max_profile_names() {
        let (provider, _exporter) = build_in_memory_meter_provider();
        let profiles: Vec<String> = (0..65).map(|index| format!("profile-{index}")).collect();

        let result = OtelMetricsEmitter::new(&provider, profiles);

        match result {
            Ok(_) => panic!("registry larger than {MAX_PROFILE_NAMES} must be rejected"),
            Err(error) => assert_eq!(
                error,
                ProfileRegistryError {
                    requested: 65,
                    max: MAX_PROFILE_NAMES
                }
            ),
        }
    }
}
