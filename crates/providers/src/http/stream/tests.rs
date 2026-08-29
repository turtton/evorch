use super::*;
use crate::message::{ChatResponse, ContentBlock, Message, Role};
use event_bus::{EventBus, EventKind, UsageEvent};
use std::sync::Arc;
use std::time::Duration;

/// `data: {"text": "..."}` を [`StreamEvent::TextDelta`] へ、`[DONE]` を
/// 完了シグナルへ解釈する偽インタプリタ。
struct FakeInterpreter {
    /// この data を持つフレームで解釈エラーを起こす。
    fail_on: Option<String>,
    /// 完了シグナルを解釈時ではなく終端処理で返す。
    complete_on_tail: bool,
}

impl FakeInterpreter {
    fn new() -> Self {
        Self {
            fail_on: None,
            complete_on_tail: false,
        }
    }

    fn failing_on(data: &str) -> Self {
        Self {
            fail_on: Some(data.to_string()),
            complete_on_tail: false,
        }
    }

    fn completing_on_tail() -> Self {
        Self {
            fail_on: None,
            complete_on_tail: true,
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
        Ok(FrameInterpretation {
            events: vec![StreamEvent::TextDelta {
                text: value["text"].as_str().unwrap_or_default().to_string(),
            }],
            completion: None,
        })
    }

    fn finish(&mut self) -> Result<FrameInterpretation, ProviderError> {
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
    );

    let items = collect(stream).await;

    assert_eq!(items.len(), 1);
    assert!(matches!(&items[0], Ok(StreamEvent::Completed { .. })));
}
