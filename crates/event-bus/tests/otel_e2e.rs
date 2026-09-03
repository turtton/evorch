//! otel-exporter feature の E2E テスト。
//!
//! 同一 [`SdkMeterProvider`] に InMemory reader と OTLP HTTP reader を接続し、
//! `force_flush` 1 回で (a) InMemory への記録と (b) loopback HTTP receiver
//! への OTLP/protobuf POST が到達することを検証する。loopback receiver は
//! std のみで構成し (wiremock 等は使用しない)、素の `#[test]` で実行する
//! (tokio runtime 内での blocking reqwest 構築は panic し得るため禁止)。
#![cfg(feature = "otel-exporter")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use event_bus::otel::exporter::{OtelMetricsEmitter, build_otlp_metric_reader};
use event_bus::otel::{
    OPERATION_DURATION_METRIC, SECONDS_UNIT, TIME_TO_FIRST_TOKEN_METRIC, TOKEN_UNIT,
    TOKEN_USAGE_METRIC,
};
use event_bus::{Event, ProviderEvent, UsageEvent};
use opentelemetry_sdk::metrics::data::{AggregatedMetrics, MetricData, ResourceMetrics};
use opentelemetry_sdk::metrics::{InMemoryMetricExporter, PeriodicReader, SdkMeterProvider};

/// loopback receiver が捕捉した 1 リクエスト分の要約。
struct CapturedRequest {
    method: String,
    path: String,
    content_type: String,
    body_len: usize,
}

fn spawn_loopback_receiver() -> (
    std::net::SocketAddr,
    mpsc::Receiver<CapturedRequest>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback listener");
    let addr = listener.local_addr().expect("local addr");
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        // テスト終了まで accept を続ける (provider shutdown 時の 2 回目 POST
        // も許容)。receiver が drop されたら終了する。
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { break };
            let Some(request) = read_request(&mut stream) else {
                continue;
            };
            let response = b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
            let _ = stream.write_all(response);
            let _ = stream.flush();
            if sender.send(request).is_err() {
                break;
            }
        }
    });
    (addr, receiver, handle)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

fn read_request(stream: &mut TcpStream) -> Option<CapturedRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            break position;
        }
        if buffer.len() > 1_048_576 {
            return None;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let start_line = lines.next()?;
    let mut parts = start_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let path = parts.next()?.to_owned();
    let mut content_type = String::new();
    let mut content_length = 0usize;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "content-type" => content_type = value.trim().to_owned(),
            "content-length" => content_length = value.trim().parse().unwrap_or(0),
            _ => {}
        }
    }
    let body_start = header_end + 4;
    while buffer.len() < body_start + content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
    let body_len = buffer.len().saturating_sub(body_start);
    Some(CapturedRequest {
        method,
        path,
        content_type,
        body_len,
    })
}

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

// Given: InMemory reader と loopback OTLP HTTP receiver を同一 provider に
//        接続した構成 (自動 export 抑止のため interval は 3600 秒)。
// When: Usage / RequestCompleted / FirstTokenObserved を emit し
//       force_flush する。
// Then: (a) InMemory 側に 3 instrument が正しい unit・値・属性で記録され、
//       (b) loopback receiver に method=POST / path=/v1/metrics /
//       content-type=application/x-protobuf / body 非空 のリクエストが届く。
#[test]
fn otel_exporter_flushes_to_in_memory_and_otlp_http_receiver() {
    let (addr, receiver, _server) = spawn_loopback_receiver();
    let endpoint = format!("http://{addr}");

    let in_memory = InMemoryMetricExporter::default();
    // base URL のみ渡し、signal path /v1/metrics の付加を build_otlp_metric_reader
    // 契約に委ねる。
    let otlp_reader = build_otlp_metric_reader(&endpoint, Duration::from_secs(3600))
        .expect("build OTLP metric reader");
    let provider = SdkMeterProvider::builder()
        .with_reader(PeriodicReader::builder(in_memory.clone()).build())
        .with_reader(otlp_reader)
        .build();
    let emitter = OtelMetricsEmitter::new(&provider);

    emitter.emit(&usage_event());
    emitter.emit(&completed_event());
    emitter.emit(&ttft_event());
    provider
        .force_flush()
        .expect("force_flush exports to both readers");

    let finished = in_memory.get_finished_metrics().expect("finished metrics");
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
    // data point の並び順は実装詳細のため、token.type 属性で lookup する。
    let token_points: Vec<_> = histogram.data_points().collect();
    assert_eq!(token_points.len(), 2, "input/output data points");
    let sum_for = |token_type: &str| {
        token_points
            .iter()
            .find(|point| {
                point.attributes().any(|kv| {
                    kv.key.as_str() == "gen_ai.token.type" && kv.value.to_string() == token_type
                })
            })
            .unwrap_or_else(|| panic!("{token_type} data point missing"))
            .sum()
    };
    assert_eq!(sum_for("input"), 10);
    assert_eq!(sum_for("output"), 20);

    let duration = metrics
        .iter()
        .find(|metric| metric.name() == OPERATION_DURATION_METRIC)
        .expect("operation duration metric");
    assert_eq!(duration.unit(), SECONDS_UNIT);
    let AggregatedMetrics::F64(MetricData::Histogram(histogram)) = duration.data() else {
        panic!("f64 histogram expected: {:?}", duration.data());
    };
    let duration_points: Vec<_> = histogram.data_points().collect();
    assert_eq!(duration_points.len(), 1);
    assert_eq!(duration_points[0].sum(), 0.5);
    assert!(duration_points[0].attributes().any(|kv| {
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
    let ttft_points: Vec<_> = histogram.data_points().collect();
    assert_eq!(ttft_points.len(), 1);
    assert_eq!(ttft_points[0].sum(), 1.5);

    let request = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("OTLP export arrives at loopback receiver");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/metrics");
    assert_eq!(request.content_type, "application/x-protobuf");
    assert!(request.body_len > 0, "OTLP body must be non-empty");
}
