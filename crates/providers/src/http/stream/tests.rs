// allow: SIZE_OK — SSE pump の既存契約と指定された観測失敗経路表を同一テストモジュールに集約する。
use super::*;
use crate::message::{ChatResponse, ContentBlock, Message, Role};
use crate::observe::AttemptObserver;
use event_bus::{EventBus, EventKind, ProviderEvent, ProviderFailureKind, UsageEvent};
use std::sync::Arc;
use std::time::Duration;

/// `data: {"text": "..."}` を [`StreamEvent::TextDelta`] へ、`[DONE]` を
/// 完了シグナルへ解釈する偽インタプリタ。
struct FakeInterpreter {
    /// この data を持つフレームで解釈エラーを起こす。
    fail_on: Option<String>,
    /// 完了シグナルを解釈時ではなく終端処理で返す。
    complete_on_tail: bool,
    /// 終端処理で解釈エラーを返す。
    fail_on_tail: bool,
}

impl FakeInterpreter {
    fn new() -> Self {
        Self {
            fail_on: None,
            complete_on_tail: false,
            fail_on_tail: false,
        }
    }

    fn failing_on(data: &str) -> Self {
        Self {
            fail_on: Some(data.to_string()),
            complete_on_tail: false,
            fail_on_tail: false,
        }
    }

    fn completing_on_tail() -> Self {
        Self {
            fail_on: None,
            complete_on_tail: true,
            fail_on_tail: false,
        }
    }

    fn failing_on_tail() -> Self {
        Self {
            fail_on: None,
            complete_on_tail: false,
            fail_on_tail: true,
        }
    }

    fn completion_stop() -> FrameInterpretation {
        FrameInterpretation {
            events: Vec::new(),
            completion: Some((
                Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                FinishReason::Stop,
            )),
        }
    }
}

impl WireStreamInterpreter for FakeInterpreter {
    fn interpret(&mut self, frame: SseFrame) -> Result<FrameInterpretation, ProviderError> {
        if Some(&frame.data) == self.fail_on.as_ref() {
            return Err(ProviderError::InvalidJson {
                detail: "壊れた JSON".to_string(),
            });
        }
        if frame.data == "[DONE]" {
            if self.complete_on_tail {
                return Ok(FrameInterpretation::default());
            }
            return Ok(Self::completion_stop());
        }
        let value: serde_json::Value =
            serde_json::from_str(&frame.data).map_err(|err| ProviderError::InvalidJson {
                detail: err.to_string(),
            })?;
        let event = if let Some(text) = value["reasoning"].as_str() {
            StreamEvent::ReasoningDelta {
                text: text.to_string(),
            }
        } else if value["tool"].as_bool() == Some(true) {
            StreamEvent::ToolCallDelta {
                index: 0,
                id: Some("call-1".to_string()),
                name: Some("tool".to_string()),
                arguments_delta: String::new(),
            }
        } else {
            StreamEvent::TextDelta {
                text: value["text"].as_str().unwrap_or_default().to_string(),
            }
        };
        Ok(FrameInterpretation {
            events: vec![event],
            completion: None,
        })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
        if self.fail_on_tail {
            return Err(ProviderError::InvalidJson {
                detail: "終端状態が不正".to_string(),
            });
        }
        if self.complete_on_tail {
            return Ok(FrameInterpretation {
                events: Vec::new(),
                completion: Some((Usage::default(), FinishReason::Length)),
            });
        }
        Ok(FrameInterpretation::default())
    }
}

fn usage_emitter_with_bus() -> (UsageEmitter, Arc<EventBus>) {
    let bus = Arc::new(EventBus::new(8));
    (UsageEmitter::new(Some(bus.clone()), "test"), bus)
}

fn observer(bus: Option<Arc<EventBus>>) -> AttemptObserver {
    AttemptObserver::new(bus, "test", None, "test-protocol", "model-a", true, None)
}

async fn next_provider_event(rx: &mut event_bus::EventReceiver) -> ProviderEvent {
    let event = rx.recv().await.expect("provider イベントを受信できる");
    match event.kind {
        EventKind::Provider(event) => event,
        other => panic!("Provider 以外のイベントを受信しました: {other:?}"),
    }
}

fn text_delta(text: &str) -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::TextDelta {
        text: text.to_string(),
    })
}

fn completed_response(text: &str, usage: Usage) -> Result<StreamEvent, ProviderError> {
    Ok(StreamEvent::Completed {
        response: ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: text.to_string(),
                }],
            },
            usage,
            finish_reason: FinishReason::Stop,
        },
    })
}

async fn collect(mut stream: DeltaStream) -> Vec<Result<StreamEvent, ProviderError>> {
    let mut items = Vec::new();
    while let Some(item) = stream.next().await {
        items.push(item);
    }
    items
}

// Given: 複数チャンクに分割した SSE バイト列 / When: アダプタを収集 / Then: 差分イベントが順に流れ Completed で結合応答が届く
#[tokio::test]
async fn sse_chunks_flow_as_ordered_events_then_completed() {
    let (emitter, bus) = usage_emitter_with_bus();
    let mut rx = bus.subscribe();
    let chunks = vec![
        Ok(Bytes::copy_from_slice(
            "data: {\"text\":\"こ\"}\n\n".as_bytes(),
        )),
        Ok(Bytes::copy_from_slice(
            "data: {\"text\":\"んに\"}\n".as_bytes(),
        )),
        Ok(Bytes::copy_from_slice(b"\ndata: [DONE]\n\n")),
    ];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        emitter,
        "model-a".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    let usage = Usage {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    };
    assert_eq!(
        items,
        vec![
            text_delta("こ"),
            text_delta("んに"),
            completed_response("こんに", usage),
        ]
    );
    let event = rx.recv().await.expect("usage イベントを受信できる");
    match event.kind {
        EventKind::Usage(UsageEvent::Usage {
            model,
            input_tokens,
            output_tokens,
            ..
        }) => {
            assert_eq!(model, "model-a");
            assert_eq!((input_tokens, output_tokens), (10, 5));
        }
        other => panic!("Usage 以外のイベントを受信しました: {other:?}"),
    }
    let second = tokio::time::timeout(Duration::from_millis(50), rx.recv()).await;
    assert!(second.is_err(), "usage は完了時に 1 回だけ発行される");
}

// Given: 完結行に不正 UTF-8 を含むチャンク / When: アダプタを収集 / Then: InvalidSse エラーが流れストリームが終わる
#[tokio::test]
async fn malformed_utf8_yields_invalid_sse_error_and_ends() {
    let chunks = vec![Ok(Bytes::copy_from_slice(b"data: \xff\xfe\n\n"))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items.len(), 1);
    assert!(matches!(&items[0], Err(ProviderError::InvalidSse { .. })));
}

// Given: 特定フレームで失敗するインタプリタ / When: アダプタを収集 / Then: 直前のイベントの後に解釈エラーが流れる
#[tokio::test]
async fn interpreter_error_is_surfaced_and_stream_ends() {
    let chunks = vec![
        Ok(Bytes::copy_from_slice(b"data: {\"text\":\"a\"}\n\n")),
        Ok(Bytes::copy_from_slice(b"data: BAD\n\n")),
    ];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::failing_on("BAD"),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items.len(), 2);
    assert!(matches!(&items[0], Ok(StreamEvent::TextDelta { .. })));
    assert!(matches!(&items[1], Err(ProviderError::InvalidJson { .. })));
}

// Given: 完了シグナル無しで入力が終了 / When: アダプタを収集 / Then: Completed は流れずイベントだけで終わる
#[tokio::test]
async fn stream_end_without_completion_signal_yields_no_completed() {
    let chunks = vec![Ok(Bytes::copy_from_slice(b"data: {\"text\":\"a\"}\n\n"))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items, vec![text_delta("a")]);
}

// Given: 末尾が空行で終端していないフレーム / When: アダプタを収集 / Then: パーサーの finish で配送されイベントになる
#[tokio::test]
async fn tail_frame_without_blank_line_is_flushed() {
    let chunks = vec![Ok(Bytes::copy_from_slice(
        "data: {\"text\":\"a\"}\n\ndata: {\"text\":\"b\"}\n".as_bytes(),
    ))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items, vec![text_delta("a"), text_delta("b")]);
}

// Given: 完了シグナルを終端処理でのみ返すインタプリタ / When: アダプタを収集 / Then: 終端処理の完了シグナルで Completed が流れる
#[tokio::test]
async fn interpreter_finish_completion_produces_completed() {
    let chunks = vec![Ok(Bytes::copy_from_slice(b"data: [DONE]\n"))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::completing_on_tail(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items.len(), 1);
    assert!(matches!(
        &items[0],
        Ok(StreamEvent::Completed { response })
            if response.finish_reason == FinishReason::Length
    ));
}

// Given: 完了フレームと同じチャンクに後続フレームが含まれる / When: アダプタを収集 / Then: 完了後のフレームは破棄され Completed のみ流れる
#[tokio::test]
async fn frames_after_completion_are_dropped() {
    let chunks = vec![Ok(Bytes::copy_from_slice(
        b"data: [DONE]\n\ndata: {\"text\":\"x\"}\n\n",
    ))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );

    let items = collect(stream).await;

    assert_eq!(items.len(), 1);
    assert!(matches!(&items[0], Ok(StreamEvent::Completed { .. })));
}

// Given: paused time 上で reasoning-only frame の後に text frame が届く / When: pump が順に解釈 / Then: reasoning では発火せず text 到達時の累積時間を TTFT にする
#[tokio::test(start_paused = true)]
async fn reasoning_then_text_records_exact_ttft() {
    let bus = Arc::new(EventBus::new(8));
    let mut rx = bus.subscribe();
    let mut observer = observer(Some(bus));
    observer.emit_started();
    let _ = next_provider_event(&mut rx).await;
    let mut pump = SsePump::new(
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "model-a".to_string(),
        observer,
    );
    tokio::time::advance(Duration::from_millis(4)).await;

    pump.push_chunk("data: {\"reasoning\":\"考察\"}\n\n".as_bytes());

    assert!(
        tokio::time::timeout(Duration::from_millis(1), rx.recv())
            .await
            .is_err()
    );
    tokio::time::advance(Duration::from_millis(6)).await;
    pump.push_chunk("data: {\"text\":\"答え\"}\n\n".as_bytes());
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::FirstTokenObserved { ttft_ms: 11, .. }
    ));
}

// Given: 空 text・tool call・後続 text を含む frame 列 / When: pump が解釈 / Then: tool call でのみ TTFT を一度発行する
#[tokio::test]
async fn empty_text_does_not_trigger_but_tool_delta_triggers_once() {
    let bus = Arc::new(EventBus::new(8));
    let mut rx = bus.subscribe();
    let mut observer = observer(Some(bus));
    observer.emit_started();
    let _ = next_provider_event(&mut rx).await;
    let mut pump = SsePump::new(
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "model-a".to_string(),
        observer,
    );

    pump.push_chunk(b"data: {\"text\":\"\"}\n\n");
    assert!(
        tokio::time::timeout(Duration::from_millis(10), rx.recv())
            .await
            .is_err()
    );
    pump.push_chunk(b"data: {\"tool\":true}\n\n");
    pump.push_chunk(b"data: {\"text\":\"later\"}\n\n");

    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::FirstTokenObserved { .. }
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), rx.recv())
            .await
            .is_err()
    );
}

// Given: pump の各失敗入口 / When: エラーを発生させる / Then: 対応する failure の RequestFailed をちょうど1回発行する
#[tokio::test]
async fn every_failure_path_emits_one_terminal_failure() {
    enum FailurePath {
        InvalidSse,
        Transport,
        Interpret,
        FinishSse,
        FinishInterpret,
        Eof,
    }

    for (path, expected) in [
        (
            FailurePath::InvalidSse,
            ProviderFailureKind::InvalidResponse,
        ),
        (FailurePath::Transport, ProviderFailureKind::Transport),
        (FailurePath::Interpret, ProviderFailureKind::InvalidResponse),
        (FailurePath::FinishSse, ProviderFailureKind::InvalidResponse),
        (
            FailurePath::FinishInterpret,
            ProviderFailureKind::InvalidResponse,
        ),
        (FailurePath::Eof, ProviderFailureKind::Transport),
    ] {
        let bus = Arc::new(EventBus::new(8));
        let mut rx = bus.subscribe();
        let mut observer = observer(Some(bus));
        observer.emit_started();
        let started = next_provider_event(&mut rx).await;
        let interpreter = if matches!(path, FailurePath::Interpret) {
            FakeInterpreter::failing_on("BAD")
        } else if matches!(path, FailurePath::FinishInterpret) {
            FakeInterpreter::failing_on_tail()
        } else {
            FakeInterpreter::new()
        };
        let mut pump = SsePump::new(
            interpreter,
            UsageEmitter::new(None, "test"),
            "model-a".to_string(),
            observer,
        );

        match path {
            FailurePath::InvalidSse => pump.push_chunk(b"data: \xff\xfe\n\n"),
            FailurePath::Transport => {
                pump.push_transport_error(ProviderError::Request("reset".to_string()))
            }
            FailurePath::Interpret => pump.push_chunk(b"data: BAD\n\n"),
            FailurePath::FinishSse => {
                pump.push_chunk(b"data: \xff");
                pump.finish_tail();
            }
            FailurePath::FinishInterpret | FailurePath::Eof => pump.finish_tail(),
        }
        drop(pump);

        let ProviderEvent::RequestStarted { request_id, .. } = started else {
            panic!("started を期待")
        };
        assert!(matches!(
            next_provider_event(&mut rx).await,
            ProviderEvent::RequestFailed { request_id: failed_id, failure, .. }
                if failed_id == request_id && failure == expected
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
    }
}

// Given: Started 済みの streaming adapter / When: consumer が最初の delta 後に stream を破棄 / Then: Other の RequestFailed を1件発行する
#[tokio::test]
async fn consumer_drop_emits_other_failure() {
    let bus = Arc::new(EventBus::new(8));
    let mut rx = bus.subscribe();
    let mut observer = observer(Some(bus));
    observer.emit_started();
    let started = next_provider_event(&mut rx).await;
    let chunks = vec![Ok(Bytes::copy_from_slice(b"data: {\"text\":\"a\"}\n\n"))];
    let mut stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "model-a".to_string(),
        observer,
    );

    let _ = stream.next().await;
    let _ = next_provider_event(&mut rx).await;
    drop(stream);

    let ProviderEvent::RequestStarted { request_id, .. } = started else {
        panic!("started を期待")
    };
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::RequestFailed { request_id: failed_id, failure: ProviderFailureKind::Other, .. }
            if failed_id == request_id
    ));
}

// Given: 完了シグナルを返さない interpreter と EventBus 接続済みの adapter。
// When: 入力終端まで canonical stream を収集する。
// Then: canonical イベント列は delta のみで静かに終わり、観測 bus 上には
//       Transport の RequestFailed がちょうど 1 件発行される (観測のみの追加)。
#[tokio::test]
async fn stream_end_without_completion_reports_transport_failure_on_bus_only() {
    let bus = Arc::new(EventBus::new(8));
    let mut rx = bus.subscribe();
    let mut observer = observer(Some(bus));
    observer.emit_started();
    let started = next_provider_event(&mut rx).await;
    let chunks = vec![Ok(Bytes::copy_from_slice(b"data: {\"text\":\"a\"}\n\n"))];
    let stream = adapt_sse_stream(
        futures_util::stream::iter(chunks),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "model-a".to_string(),
        observer,
    );

    let items: Vec<Result<StreamEvent, ProviderError>> = stream.collect().await;

    assert_eq!(
        items,
        vec![text_delta("a")],
        "canonical 列は delta のみで終わる"
    );
    let ProviderEvent::RequestStarted { request_id, .. } = started else {
        panic!("started を期待")
    };
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::FirstTokenObserved { .. }
    ));
    assert!(matches!(
        next_provider_event(&mut rx).await,
        ProviderEvent::RequestFailed { request_id: failed_id, failure: ProviderFailureKind::Transport, .. }
            if failed_id == request_id
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(10), rx.recv())
            .await
            .is_err(),
        "Transport 失敗は 1 件だけ発行される"
    );
}
