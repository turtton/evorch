//! otel-exporter feature で有効化される metrics exporter 層。
//!
//! 写像層 ([`super::map_event`]) の出力を OTel histogram instruments へ
//! 記録する [`OtelMetricsEmitter`] と、OTLP HTTP / InMemory の meter
//! provider 構築関数を提供する。histogram の bucket boundaries は semconv
//! v1.37.0 (`super::SEMCONV_PIN`) の advisory ExplicitBucketBoundaries に
//! 従う。

use std::time::Duration;

use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, MeterProvider};
use opentelemetry_otlp::{ExporterBuildError, MetricExporter, Protocol, WithExportConfig};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

use crate::event::Event;

use super::{
    MetricMeasurement, MetricValue, OPERATION_DURATION_METRIC, SECONDS_UNIT,
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

/// イベントを OTel histogram へ記録する emitter。
///
/// [`OtelMetricsEmitter::emit`] は panic しない。cardinality guard 違反や
/// instrument と値型の不一致は `tracing::warn!` で記録し、該当 measurement
/// のみ skip する。
pub struct OtelMetricsEmitter {
    token_usage: Histogram<u64>,
    operation_duration: Histogram<f64>,
    time_to_first_token: Histogram<f64>,
}

impl OtelMetricsEmitter {
    /// [`SdkMeterProvider`] から meter を取得し、3 histogram を構築する。
    pub fn new(provider: &SdkMeterProvider) -> Self {
        let meter = provider.meter("evorch.event-bus");
        Self {
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
        }
    }

    /// イベントを写像し、有効な measurement を histogram へ記録する。
    pub fn emit(&self, event: &Event) {
        for measurement in map_event(event) {
            self.record(measurement);
        }
    }

    fn record(&self, measurement: MetricMeasurement) {
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

/// OTLP HTTP (protobuf) exporter + [`PeriodicReader`] で meter provider を
/// 構築する。
///
/// `endpoint` は OTLP HTTP の base URL (例: `http://127.0.0.1:4318`) で、
/// exporter が signal path `/v1/metrics` を自動付加する。`interval` は
/// PeriodicReader の収集間隔。
///
/// # Errors
/// exporter の構築に失敗した場合 [`ExporterBuildError`] を返す。0.32 では
/// `OTelSdkResult` が非ジェネリクス (`Result<(), OTelSdkError>`) のため、
/// 唯一の失敗点である exporter 構築の error 型をそのまま返す。
pub fn build_otlp_meter_provider(
    endpoint: &str,
    interval: Duration,
) -> Result<SdkMeterProvider, ExporterBuildError> {
    let exporter = MetricExporter::builder()
        .with_http()
        .with_protocol(Protocol::HttpBinary)
        .with_endpoint(endpoint)
        .build()?;
    let reader = PeriodicReader::builder(exporter)
        .with_interval(interval)
        .build();
    Ok(SdkMeterProvider::builder().with_reader(reader).build())
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

    fn completed_event() -> Event {
        Event::new(ProviderEvent::RequestCompleted {
            request_id: "req-1".to_owned(),
            provider: "openai".to_owned(),
            profile: Some("primary".to_owned()),
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
            profile: None,
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

    // Given: in-memory provider と emitter。
    // When: 3 種のイベントを emit し force_flush する。
    // Then: 3 instrument が正しい unit・合計値・属性で記録される。
    #[test]
    fn in_memory_smoke_records_three_instruments() {
        let (provider, exporter) = build_in_memory_meter_provider();
        let emitter = OtelMetricsEmitter::new(&provider);

        emitter.emit(&usage_event());
        emitter.emit(&completed_event());
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
    }

    // Given: advisory boundaries を設定した 3 instrument。
    // When: in-memory provider で emit し force_flush する。
    // Then: 記録された histogram の bounds が semconv v1.37.0 の推奨値と一致する。
    #[test]
    fn histograms_use_semconv_advisory_boundaries() {
        let (provider, exporter) = build_in_memory_meter_provider();
        let emitter = OtelMetricsEmitter::new(&provider);

        emitter.emit(&usage_event());
        emitter.emit(&completed_event());
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
}
