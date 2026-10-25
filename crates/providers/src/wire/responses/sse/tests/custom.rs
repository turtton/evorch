use super::*;
use crate::stream::StreamEvent;
use serde_json::json;

fn tool_event(index: usize, metadata: bool, arguments: &str) -> StreamEvent {
    StreamEvent::ToolCallDelta {
        index,
        id: metadata.then(|| format!("call_{index}")),
        name: metadata.then(|| "apply_patch".to_owned()),
        arguments_delta: arguments.to_owned(),
    }
}

#[test]
fn custom_tool_events_follow_the_state_table_without_duplicate_closing_bytes() {
    let mut interpreter = ResponsesStreamInterpreter::new();
    for (frame, expected) in [
        (
            item("response.output_item.added", "a\n", 0),
            vec![tool_event(0, true, "{\"input\":\"a\\n")],
        ),
        (delta("\"b", 0), vec![tool_event(0, false, "\\\"b")]),
        (done("a\n\"bc", 0), vec![tool_event(0, false, "c\"}")]),
        (done("a\n\"bc", 0), vec![]),
        (
            item("response.output_item.done", "a\n\"bc", 0),
            vec![tool_event(0, true, "")],
        ),
        (delta("x", 1), vec![tool_event(1, false, "{\"input\":\"x")]),
        (
            item("response.output_item.done", "xy", 1),
            vec![tool_event(1, true, "y\"}")],
        ),
        (
            done("z", 2),
            vec![tool_event(2, false, "{\"input\":\"z\"}")],
        ),
        (
            item("response.output_item.done", "z", 2),
            vec![tool_event(2, true, "")],
        ),
        (
            item("response.output_item.done", "", 3),
            vec![tool_event(3, true, "{\"input\":\"\"}")],
        ),
    ] {
        assert_eq!(interpreter.interpret(frame).unwrap().events, expected);
    }
}

fn item(kind: &str, input: &str, index: usize) -> SseFrame {
    frame(
        kind,
        json!({"output_index": index, "item": {
            "type": "custom_tool_call", "id": format!("custom_{index}"),
            "call_id": format!("call_{index}"), "name": "apply_patch", "input": input
        }}),
    )
}

fn delta(input: &str, index: usize) -> SseFrame {
    frame(
        "response.custom_tool_call_input.delta",
        json!({
            "item_id": format!("custom_{index}"), "output_index": index, "delta": input
        }),
    )
}

fn done(input: &str, index: usize) -> SseFrame {
    frame(
        "response.custom_tool_call_input.done",
        json!({
            "item_id": format!("custom_{index}"), "output_index": index, "input": input
        }),
    )
}

fn completed() -> SseFrame {
    frame(
        "response.completed",
        json!({"response": {"usage": {"input_tokens": 1, "output_tokens": 1}}}),
    )
}

fn assert_input(mut stream: Vec<SseFrame>, expected: serde_json::Value) {
    stream.push(completed());
    let response = interpret_all(stream).unwrap().unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolUse);
    assert_eq!(
        response.message.content,
        vec![ContentBlock::ToolUse {
            id: "call_0".into(),
            name: "apply_patch".into(),
            input: expected
        }]
    );
}

#[test]
fn custom_tool_state_transitions_preserve_input_and_close_exactly_once() {
    let added = "response.output_item.added";
    let ended = "response.output_item.done";
    // Unopened/added, Open/delta, Open/done, Closed/done and Closed/item.done.
    assert_input(
        vec![
            item(added, "a", 0),
            delta("b", 0),
            done("abc", 0),
            done("abc", 0),
            item(ended, "abc", 0),
        ],
        json!({"input": "abc"}),
    );
    // Unopened/delta and metadata arriving only with item.done.
    assert_input(
        vec![
            delta("a", 0),
            delta("b", 0),
            done("abc", 0),
            item(ended, "abc", 0),
        ],
        json!({"input": "abc"}),
    );
    // Unopened/done, then Closed/item.done.
    assert_input(
        vec![done("abc", 0), item(ended, "abc", 0)],
        json!({"input": "abc"}),
    );
    // Unopened/item.done (Codex may send no added or input events).
    assert_input(vec![item(ended, "abc", 0)], json!({"input": "abc"}));
    // Open/item.done supplies the unreceived suffix.
    assert_input(
        vec![item(added, "a", 0), item(ended, "abc", 0)],
        json!({"input": "abc"}),
    );
    assert_input(vec![item(ended, "", 0)], json!({"input": ""}));
    assert_input(
        vec![delta("a", 0), item(added, "", 0), item(ended, "abc", 0)],
        json!({"input": "abc"}),
    );
    assert_input(
        vec![frame(
            ended,
            json!({"output_index": 0, "item": {
                "type": "custom_tool_call", "id": "custom_0", "call_id": "call_0", "name": "apply_patch"
            }}),
        )],
        json!({"input": ""}),
    );
}

#[test]
fn custom_tool_rejects_late_deltas_and_inconsistent_final_input() {
    for stream in [
        vec![done("abc", 0), delta("", 0)],
        vec![delta("abc", 0), done("ab", 0)],
        vec![done("abc", 0), done("abcd", 0)],
        vec![done("abc", 0), item("response.output_item.done", "ab", 0)],
        vec![
            delta("abc", 0),
            item("response.output_item.added", "xyz", 0),
        ],
    ] {
        assert!(matches!(
            interpret_all(stream),
            Err(ProviderError::InvalidSse { .. })
        ));
    }
}

#[test]
fn custom_tool_unclosed_input_stays_null_and_missing_call_id_is_removed() {
    assert_input(
        vec![
            item("response.output_item.added", "abc", 0),
            delta("def", 0),
        ],
        serde_json::Value::Null,
    );
    for mut stream in [vec![delta("abc", 0)], vec![done("abc", 0)]] {
        stream.push(completed());
        assert!(
            interpret_all(stream)
                .unwrap()
                .unwrap()
                .message
                .content
                .is_empty()
        );
    }
}

#[test]
fn custom_and_function_calls_share_indices_when_interleaved() {
    let function_item = |kind: &str, index, arguments| {
        frame(
            kind,
            json!({
                "output_index": index,
                "item": {"type": "function_call", "id": format!("fc_{index}"),
                    "call_id": format!("function_{index}"), "name": "read", "arguments": arguments}
            }),
        )
    };
    let response = interpret_all(vec![
        item("response.output_item.added", "", 1),
        function_item("response.output_item.added", 0, ""),
        delta("a", 1),
        frame(
            "response.function_call_arguments.delta",
            json!({
                "item_id": "fc_0", "output_index": 0, "delta": "{\"path\":"
            }),
        ),
        item("response.output_item.done", "def", 3),
        function_item("response.output_item.done", 2, "{}"),
        frame(
            "response.function_call_arguments.done",
            json!({
                "item_id": "fc_0", "output_index": 0, "arguments": "{\"path\":\"a\"}"
            }),
        ),
        item("response.output_item.done", "abc", 1),
        function_item("response.output_item.done", 0, "{\"path\":\"a\"}"),
        completed(),
    ])
    .unwrap()
    .unwrap();
    assert_eq!(
        response.message.content,
        vec![
            ContentBlock::ToolUse {
                id: "function_0".into(),
                name: "read".into(),
                input: json!({"path": "a"})
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "apply_patch".into(),
                input: json!({"input": "abc"})
            },
            ContentBlock::ToolUse {
                id: "function_2".into(),
                name: "read".into(),
                input: json!({})
            },
            ContentBlock::ToolUse {
                id: "call_3".into(),
                name: "apply_patch".into(),
                input: json!({"input": "def"})
            },
        ]
    );
}

#[test]
fn custom_tool_escaping_is_compositional_at_every_unicode_boundary() {
    let input = "\"\\\n\r\t\0\u{001f}日本語*** End Patch";
    let boundaries: Vec<_> = input
        .char_indices()
        .map(|(i, _)| i)
        .chain([input.len()])
        .collect();
    for split in boundaries {
        let (a, b) = input.split_at(split);
        assert_input(
            vec![
                item("response.output_item.added", "", 0),
                delta(a, 0),
                delta(b, 0),
                done(input, 0),
                item("response.output_item.done", input, 0),
            ],
            json!({"input": input}),
        );
    }
}

#[test]
fn custom_tool_unknown_incomplete_is_discarded_but_known_reason_is_preserved() {
    for (reason, keep) in [
        ("future_reason", false),
        ("max_output_tokens", true),
        ("content_filter", true),
    ] {
        let response = interpret_all(vec![
            item("response.output_item.done", "patch", 0),
            incomplete(json!({"reason": reason})),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(!response.message.content.is_empty(), keep);
    }
    let response = interpret_all(vec![
        item("response.output_item.done", "patch", 0),
        incomplete(json!({})),
    ])
    .unwrap()
    .unwrap();
    assert!(response.message.content.is_empty());
}
