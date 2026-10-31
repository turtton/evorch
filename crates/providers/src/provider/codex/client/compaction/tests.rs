use super::*;
use serde_json::{Value, json};

fn frame(value: Value) -> SseFrame {
    SseFrame {
        event: None,
        data: value.to_string(),
    }
}

fn item(blob: Value) -> SseFrame {
    frame(
        json!({"type":"response.output_item.done", "item":{"type":"compaction", "encrypted_content":blob}}),
    )
}

fn completed() -> SseFrame {
    frame(
        json!({"type":"response.completed", "response":{"usage":{"input_tokens":42,"output_tokens":7,"input_tokens_details":{"cached_tokens":30}}}}),
    )
}

#[test]
fn captures_blob_usage_and_ignores_other_output() {
    let mut stream = CompactionStream::default();
    for kind in ["message", "reasoning", "function_call"] {
        assert_eq!(
            stream
                .interpret(frame(
                    json!({"type":"response.output_item.done", "item":{"type":kind}})
                ))
                .unwrap(),
            None
        );
    }
    assert_eq!(stream.interpret(item(json!("opaque+/="))).unwrap(), None);
    let result = stream.interpret(completed()).unwrap().unwrap();
    assert!(result.usage_reported);
    assert_eq!(result.encrypted_content, "opaque+/=");
    assert_eq!(
        result.usage,
        Usage {
            context_tokens: None,
            input_tokens: 42,
            output_tokens: 7,
            cache_read_tokens: 30,
            cache_write_tokens: 0,
            reasoning_tokens: None,
        }
    );
}

#[test]
fn cached_tokens_are_optional() {
    let mut stream = CompactionStream::default();
    stream.interpret(item(json!("opaque"))).unwrap();
    let result = stream.interpret(frame(json!({"type":"response.completed", "response":{"usage":{"input_tokens":2,"output_tokens":1}}}))).unwrap().unwrap();
    assert_eq!(result.usage.cache_read_tokens, 0);
}

// Given: a compaction whose response.completed has no usage (null or absent key)
// When: interpreted / Then: the blob is still returned, but usage is marked unreported
#[test]
fn missing_usage_still_completes_but_is_marked_unreported() {
    for response in [
        json!({"id":"resp_1","usage":null}),
        json!({"id":"resp_1"}),
        json!({}),
    ] {
        let mut stream = CompactionStream::default();
        stream.interpret(item(json!("opaque"))).unwrap();
        let completed = stream
            .interpret(frame(
                json!({"type":"response.completed","response":response}),
            ))
            .unwrap()
            .expect("completion");
        assert!(!completed.usage_reported, "{response}");
        assert_eq!(completed.encrypted_content, "opaque");
        assert_eq!(completed.usage, Usage::default());
    }
}

#[test]
fn rejects_missing_empty_duplicate_blob_and_invalid_usage() {
    assert!(CompactionStream::default().interpret(completed()).is_err());
    for blob in [json!(""), Value::Null, json!(42)] {
        assert!(CompactionStream::default().interpret(item(blob)).is_err());
    }
    let mut stream = CompactionStream::default();
    stream.interpret(item(json!("opaque"))).unwrap();
    assert!(stream.interpret(item(json!("second"))).is_err());
    // usage 自体は任意だが、あれば形が正しくなければならない。
    assert!(
        stream
            .interpret(frame(
                json!({"type":"response.completed", "response":{"usage":{"input_tokens":"x"}}})
            ))
            .is_err()
    );
}

#[test]
fn rejects_failed_incomplete_done_and_malformed_frames_without_echoing_payload() {
    for kind in ["response.failed", "response.incomplete", "error"] {
        let error = CompactionStream::default()
            .interpret(frame(json!({"type":kind,"message":"secret"})))
            .unwrap_err();
        assert!(!error.to_string().contains("secret"));
    }
    for data in ["[DONE]", "secret", "{}"] {
        assert!(
            CompactionStream::default()
                .interpret(SseFrame {
                    event: None,
                    data: data.into()
                })
                .is_err()
        );
    }
}
