//! terminal response の usage は任意 (Codex CLI 0.166.0 と同じ)。欠落は黙ってゼロと
//! 報告せず、インタプリタが「未報告」を呼び出し側へ伝える。

use serde_json::json;

use super::{frame, function_call_done, text_delta};
use crate::ResponsesDialect::{self, Codex, OpenAi};
use crate::http::stream::WireStreamInterpreter;
use crate::message::{ContentBlock, FinishReason, Usage};
use crate::sse::SseFrame;
use crate::wire::responses::ResponsesStreamInterpreter;

fn terminal(event: &str, response: serde_json::Value) -> SseFrame {
    frame(event, json!({"type": event, "response": response}))
}

/// 完了時の (usage, finish_reason, usage_reported, tool_call が残ったか) を返す。
fn complete(dialect: ResponsesDialect, frames: Vec<SseFrame>) -> (Usage, FinishReason, bool, bool) {
    let mut interpreter = match dialect {
        OpenAi => ResponsesStreamInterpreter::new(),
        Codex => ResponsesStreamInterpreter::with_origin(crate::ResolvedResponsesScope::resolve(
            &serde_json::from_value(json!({"model":"m","messages":[]})).unwrap(),
            Codex,
            Some("test"),
            "https://example.test/v1/responses",
            crate::ResponsesAccount::Unavailable,
        )),
    };
    let mut accumulator = crate::stream::StreamAccumulator::default();
    for frame in frames {
        let interpreted = interpreter.interpret(frame).unwrap();
        for event in &interpreted.events {
            accumulator.feed(event);
        }
        if let Some((usage, reason)) = interpreted.completion {
            let mut response = accumulator.finish(usage, reason);
            interpreter.finalize_message(&mut response.message);
            let has_tool = response
                .message
                .content
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. }));
            return (
                response.usage,
                response.finish_reason,
                interpreter.usage_reported(),
                has_tool,
            );
        }
    }
    panic!("no completion");
}

// Given: response.completed の usage が null / キーなし / response が空
// When: 解釈 / Then: Stop で完了し、usage は代替のゼロで「未報告」になる (両 dialect)
#[test]
fn completed_without_usage_completes_as_unreported() {
    for dialect in [OpenAi, Codex] {
        for response in [
            json!({"id": "resp_1", "usage": null}),
            json!({"id": "resp_1"}),
            json!({}),
        ] {
            let (usage, reason, reported, _) = complete(
                dialect,
                vec![
                    text_delta("hi"),
                    terminal("response.completed", response.clone()),
                ],
            );
            assert_eq!(usage, Usage::default(), "{dialect:?} {response}");
            assert_eq!(reason, FinishReason::Stop, "{dialect:?} {response}");
            assert!(!reported, "{dialect:?} {response}");
        }
    }
}

// Given: usage を持つ response.completed / When: 解釈 / Then: 従来どおり値を写し「報告済み」
#[test]
fn completed_with_usage_is_reported_and_unchanged() {
    for dialect in [OpenAi, Codex] {
        let (usage, reason, reported, _) = complete(
            dialect,
            vec![terminal(
                "response.completed",
                json!({"id": "resp_1", "usage": {"input_tokens": 9, "output_tokens": 4,
                    "input_tokens_details": {"cached_tokens": 2},
                    "output_tokens_details": {"reasoning_tokens": 1}}}),
            )],
        );
        assert_eq!(
            usage,
            Usage {
                context_tokens: None,
                input_tokens: 9,
                output_tokens: 4,
                cache_read_tokens: 2,
                cache_write_tokens: 0,
                reasoning_tokens: Some(1),
            }
        );
        assert_eq!(reason, FinishReason::Stop);
        assert!(reported);
    }
}

// Given: tool call 後の usage なし completed / When: 解釈 / Then: ToolUse のまま未報告
#[test]
fn completed_without_usage_keeps_tool_use() {
    let (_, reason, reported, has_tool) = complete(
        OpenAi,
        vec![
            function_call_done(),
            terminal("response.completed", json!({"id": "resp_1"})),
        ],
    );
    assert_eq!(reason, FinishReason::ToolUse);
    assert!(has_tool);
    assert!(!reported);
}

// Given: 各 reason の response.incomplete で usage なし (null / キーなし)
// When: 解釈 / Then: 写像と tool call の扱いは usage ありと同じで、usage だけ未報告
//   max_output_tokens → Length、content_filter → ContentFilter (tool call 維持)、
//   interrupted / 未知 / 欠落 → Other("incomplete[: reason]") で tool call を破棄
#[test]
fn incomplete_without_usage_keeps_its_mapping_and_is_unreported() {
    for (details, expected, keeps_tool) in [
        (
            json!({"reason": "max_output_tokens"}),
            FinishReason::Length,
            true,
        ),
        (
            json!({"reason": "content_filter"}),
            FinishReason::ContentFilter,
            true,
        ),
        (
            json!({"reason": "interrupted"}),
            FinishReason::Other("incomplete: interrupted".into()),
            false,
        ),
        (
            json!({"reason": "future_reason"}),
            FinishReason::Other("incomplete: future_reason".into()),
            false,
        ),
        (json!({}), FinishReason::Other("incomplete".into()), false),
    ] {
        for usage in [json!(null), json!("absent")] {
            let mut response = json!({
                "id": "resp_1", "status": "incomplete", "incomplete_details": details,
            });
            if !usage.is_string() {
                response["usage"] = usage;
            }
            let (usage, reason, reported, has_tool) = complete(
                OpenAi,
                vec![
                    text_delta("partial"),
                    function_call_done(),
                    terminal("response.incomplete", response),
                ],
            );
            assert_eq!(reason, expected, "{details}");
            assert_eq!(has_tool, keeps_tool, "{details}");
            assert_eq!(usage, Usage::default(), "{details}");
            assert!(!reported, "{details}");
        }
    }
}
