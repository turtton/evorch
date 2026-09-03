// allow: SIZE_OK - タスク要件により loopback receiver 先例 (otel_e2e.rs から
// 流用) ・決定的 event fixture (データテーブル) ・wire/InMemory dual assert
// を 1 test 1 file に収めるため。tests/ 配下への追加 file 作成は本タスクで
// 禁止されており、fixture テーブルが過半を占める。
//! otel-exporter feature の OTLP span E2E テスト。
//!
//! 同一 event 列を (a) OTLP HTTP tracer provider 経由の loopback receiver と
//! (b) InMemory tracer provider 経由の 2 系統へ流し、wire 上への
//! OTLP/protobuf POST 到達と span tree の意味内容を双方で検証する。protobuf
//! の decode は行わず wire assert は transport level (method / path /
//! content-type / 非空 body) に留め、意味内容は InMemory 側が担う。loopback
//! receiver は std のみで構成し (wiremock 等は使用しない)、素の `#[test]` で
//! 実行する (tokio runtime 内での blocking reqwest 構築は panic し得るため
//! 禁止)。
#![cfg(feature = "otel-exporter")]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

use event_bus::otel::SpanMapper;
use event_bus::otel::span_exporter::{
    OtelSpanEmitter, build_in_memory_tracer_provider, build_otlp_tracer_provider,
};
use event_bus::{
    AgentRunPhase, Event, EventKind, EventMeta, LifecycleEvent, ProviderEvent, ToolEvent,
};
use opentelemetry::trace::{SpanId, SpanKind as OtelSpanKind, Status};
use opentelemetry_sdk::trace::SpanData;

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
        // テスト終了まで accept を続ける (SimpleSpanProcessor の span End ごと
        // の複数 POST も許容)。receiver が drop されたら終了する。
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

/// event 列の基準時刻 (UNIX_EPOCH からの秒)。
const BASE_UNIX_SECONDS: u64 = 1_700_000_000;

fn wall(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(BASE_UNIX_SECONDS + seconds)
}

fn meta(seconds: u64) -> EventMeta {
    EventMeta {
        schema_version: 1,
        monotonic: Duration::from_secs(seconds),
        wall_clock: wall(seconds),
    }
}

/// 固定時刻の [`Event`] を生成する (`Event::new` は now() を刻むため直構築)。
fn event(seconds: u64, kind: impl Into<EventKind>) -> Event {
    Event {
        meta: meta(seconds),
        kind: kind.into(),
    }
}

/// session 開始 → run 開始 → request 開始/完了 → tool 開始/完了 → run 終端 →
/// session 終端の 8 event 列 (成功経路のフルシーケンス)。
fn event_sequence() -> Vec<Event> {
    vec![
        event(
            0,
            LifecycleEvent::Started {
                session_id: "session-1".to_owned(),
            },
        ),
        event(
            1,
            LifecycleEvent::AgentRunStarted {
                run_id: "run-1".to_owned(),
                parent_run_id: None,
                agent_name: "orchestrator".to_owned(),
                role: "orchestrator".to_owned(),
            },
        ),
        event(
            2,
            ProviderEvent::RequestStarted {
                request_id: "req-1".to_owned(),
                provider: "anthropic".to_owned(),
                profile: Some("primary".to_owned()),
                protocol: "anthropic-messages".to_owned(),
                model: "kimi-k3".to_owned(),
                streaming: true,
                run_id: Some("run-1".to_owned()),
            },
        ),
        event(
            3,
            ProviderEvent::RequestCompleted {
                request_id: "req-1".to_owned(),
                provider: "anthropic".to_owned(),
                profile: Some("primary".to_owned()),
                protocol: "anthropic-messages".to_owned(),
                model: "kimi-k3".to_owned(),
                streaming: true,
                duration_ms: 500,
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
                finish_reason: "stop".to_owned(),
                run_id: Some("run-1".to_owned()),
            },
        ),
        event(
            4,
            ToolEvent::ToolStarted {
                tool_name: "read".to_owned(),
                call_id: "call-1".to_owned(),
                run_id: Some("run-1".to_owned()),
            },
        ),
        event(
            5,
            ToolEvent::ToolCompleted {
                tool_name: "read".to_owned(),
                call_id: "call-1".to_owned(),
                is_error: false,
                detail: None,
                run_id: Some("run-1".to_owned()),
            },
        ),
        event(
            6,
            LifecycleEvent::AgentRunStateChanged {
                run_id: "run-1".to_owned(),
                from: AgentRunPhase::Running,
                to: AgentRunPhase::Done,
                reason: None,
            },
        ),
        event(
            7,
            LifecycleEvent::Completed {
                session_id: "session-1".to_owned(),
            },
        ),
    ]
}

/// event 列を mapper へ流し、出力 action を emitter で記録する。
///
/// mapper が 1 件も drop しないことを検査する (budget / sampling 既定値下で
/// 正常経路は drop なし)。
fn ingest_and_apply(mapper: &mut SpanMapper, emitter: &mut OtelSpanEmitter) {
    for event in event_sequence() {
        for action in mapper.ingest(&event) {
            emitter.apply(action);
        }
    }
    let drops = mapper.drain_drops();
    assert!(drops.is_empty(), "unexpected span drops: {drops:?}");
}

fn find_span<'a>(spans: &'a [SpanData], name: &str) -> &'a SpanData {
    spans
        .iter()
        .find(|span| span.name.as_ref() == name)
        .unwrap_or_else(|| panic!("span {name} not found: spans={spans:?}"))
}

// Given: loopback OTLP HTTP receiver (provider 構築前に listen 済み) と
//        InMemory tracer provider の 2 系統。
// When: 同一 event 列を各系統の SpanMapper → OtelSpanEmitter で記録し
//       force_flush する。
// Then: (a) wire 経路に method=POST / path=/v1/traces /
//       content-type=application/x-protobuf / 非空 body の POST が届き、
//       (b) InMemory 側に 5 span (session / run / agent / request / tool) が
//       親子連鎖・同一定 trace_id (run tree)・names/kinds・status Unset で
//       記録される。
#[test]
fn otel_span_exporter_flushes_to_in_memory_and_otlp_http_receiver() {
    // --- (a) wire 経路: receiver を listen 済みにしてから provider を構築 ---
    let (addr, receiver, _server) = spawn_loopback_receiver();
    let endpoint = format!("http://{addr}");
    // base URL のみ渡し、signal path /v1/traces の付加を
    // build_otlp_tracer_provider 契約に委ねる。
    let otlp_provider = build_otlp_tracer_provider(&endpoint).expect("build OTLP tracer provider");
    let mut mapper = SpanMapper::new();
    let mut emitter = OtelSpanEmitter::new(&otlp_provider);
    ingest_and_apply(&mut mapper, &mut emitter);
    // SimpleSpanProcessor は End 時に同期 export するが、確実性のため
    // force_flush で残存がないことを保証する。
    otlp_provider
        .force_flush()
        .expect("force_flush exports spans");

    let request = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("OTLP span export arrives at loopback receiver");
    assert_eq!(request.method, "POST");
    assert_eq!(request.path, "/v1/traces");
    assert!(
        request.content_type.contains("application/x-protobuf"),
        "content-type must carry protobuf media type: {}",
        request.content_type
    );
    assert!(request.body_len > 0, "OTLP body must be non-empty");

    // --- (b) InMemory 経路: 同一 event 列を再生成して流す ---
    let (in_memory_provider, exporter) = build_in_memory_tracer_provider();
    let mut mapper = SpanMapper::new();
    let mut emitter = OtelSpanEmitter::new(&in_memory_provider);
    ingest_and_apply(&mut mapper, &mut emitter);
    in_memory_provider
        .force_flush()
        .expect("force_flush succeeds");

    let spans = exporter.get_finished_spans().expect("finished spans");
    assert_eq!(spans.len(), 5, "spans={spans:?}");

    let session = find_span(&spans, "evorch.session");
    let run = find_span(&spans, "evorch.run orchestrator");
    let agent = find_span(&spans, "invoke_agent orchestrator");
    let request = find_span(&spans, "chat kimi-k3");
    let tool = find_span(&spans, "execute_tool read");

    assert_eq!(session.span_kind, OtelSpanKind::Internal);
    assert_eq!(run.span_kind, OtelSpanKind::Internal);
    assert_eq!(agent.span_kind, OtelSpanKind::Client);
    assert_eq!(request.span_kind, OtelSpanKind::Client);
    assert_eq!(tool.span_kind, OtelSpanKind::Internal);

    // 親子連鎖: session と run は独立 root (session↔run リンクは存在しない)、
    // agent は run の子、request / tool は agent の子。
    assert_eq!(session.parent_span_id, SpanId::INVALID);
    assert_eq!(run.parent_span_id, SpanId::INVALID);
    assert_eq!(agent.parent_span_id, run.span_context.span_id());
    assert_eq!(request.parent_span_id, agent.span_context.span_id());
    assert_eq!(tool.parent_span_id, agent.span_context.span_id());

    // run tree 4 span は同一定 trace_id、session は独立 root として別 trace。
    let run_trace = run.span_context.trace_id();
    assert_eq!(agent.span_context.trace_id(), run_trace);
    assert_eq!(request.span_context.trace_id(), run_trace);
    assert_eq!(tool.span_context.trace_id(), run_trace);
    assert_ne!(session.span_context.trace_id(), run_trace);

    // 成功終端のため全 span の status は Unset。
    for span in [&session, &run, &agent, &request, &tool] {
        assert_eq!(span.status, Status::Unset, "span={}", span.name.as_ref());
    }

    // 時刻は元イベントの wall_clock をそのまま写す (決定性の証拠)。
    assert_eq!(session.start_time, wall(0));
    assert_eq!(session.end_time, wall(7));
    assert_eq!(run.start_time, wall(1));
    assert_eq!(run.end_time, wall(6));
    assert_eq!(agent.start_time, wall(1));
    assert_eq!(agent.end_time, wall(6));
    assert_eq!(request.start_time, wall(2));
    assert_eq!(request.end_time, wall(3));
    assert_eq!(tool.start_time, wall(4));
    assert_eq!(tool.end_time, wall(5));
}
